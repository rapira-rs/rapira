mod cache;
mod config;

pub use config::{Section, Settings, resolve};

use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty};
use tower::util::BoxCloneServiceLayer;
use tower::{Service, ServiceExt as _};
use tower_http::services::ServeDir;
use tower_http::services::fs::DefaultServeDirFallback;

use cache::CachingBackend;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// The body of the http plugin's middleware chain.
type Body = UnsyncBoxBody<Bytes, BoxError>;

/// Serves files from a directory and hands every miss to the inner service.
/// A permission error or a bad file name is also a miss. Any other read failure
/// answers 500. That request does not reach the inner service.
pub struct StaticFiles {
    dir: ServeDir<DefaultServeDirFallback, CachingBackend>,
    forbid: Vec<String>,
}

impl StaticFiles {
    /// A relative `root` resolves against the process working directory.
    /// `forbid` holds file-name suffixes with a leading dot.
    /// The constructor lowercases them, so an uppercase entry still matches in `eligible`.
    pub fn new(root: PathBuf, mut forbid: Vec<String>) -> Self {
        for entry in &mut forbid {
            entry.make_ascii_lowercase();
        }
        Self {
            // The URL space belongs to PHP: a directory URL is the app's route, not an
            // implicit index.html.
            dir: ServeDir::with_backend(root, CachingBackend::default())
                .append_index_html_on_directories(false),
            forbid,
        }
    }

    /// The check runs on the decoded path because ServeDir percent-decodes before it reads
    /// the filesystem. A match on the raw path would accept `%2Ephp`.
    fn eligible(&self, path: &str) -> bool {
        let Ok(decoded) = percent_encoding::percent_decode_str(path).decode_utf8() else {
            return false;
        };
        if decoded.split('/').any(|segment| segment.starts_with('.')) {
            return false;
        }
        // The last non-empty segment is the served file; the component walk drops trailing
        // separators, so `/index.php%2F` still names index.php here.
        let file = decoded
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase();
        !self.forbid.iter().any(|ext| file.ends_with(ext.as_str()))
    }
}

/// The error kinds that mean there is no file to serve. `try_call` reports a missing path and
/// an unreadable path this way. The backend reports a directory with `IsADirectory`.
/// https://docs.rs/tower-http/0.7.1/tower_http/services/struct.ServeDir.html#method.try_call
///
/// A bad file name also reaches this check. A segment over `NAME_MAX` gives `InvalidFilename`.
/// A NUL byte gives `InvalidInput` on `HEAD`. A `GET` answers 404 for a NUL byte.
fn is_miss(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::IsADirectory
            | std::io::ErrorKind::InvalidFilename
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::NotADirectory
    )
}

impl StaticFiles {
    /// The middleware as a tower layer over the http plugin's inner service.
    pub fn layer<S>(
        self,
    ) -> BoxCloneServiceLayer<S, http::Request<Body>, http::Response<Body>, Infallible>
    where
        S: Service<http::Request<Body>, Response = http::Response<Body>, Error = Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
    {
        let files = Arc::new(self);
        BoxCloneServiceLayer::new(tower::layer::layer_fn(move |inner: S| {
            let files = Arc::clone(&files);
            tower::service_fn(move |req| {
                let files = Arc::clone(&files);
                let inner = inner.clone();
                async move { Ok(files.handle(req, inner).await) }
            })
        }))
    }

    async fn handle<S>(&self, req: http::Request<Body>, inner: S) -> http::Response<Body>
    where
        S: Service<http::Request<Body>, Response = http::Response<Body>, Error = Infallible>
            + Send
            + 'static,
        S::Future: Send + 'static,
    {
        if req.method() != Method::GET && req.method() != Method::HEAD {
            return forward(inner, req).await;
        }
        if !self.eligible(req.uri().path()) {
            return forward(inner, req).await;
        }

        // The probe carries only the head. The original request stays unchanged for the
        // miss path, so its extensions reach the inner service.
        let mut probe = http::Request::new(Empty::<Bytes>::new());
        *probe.method_mut() = req.method().clone();
        *probe.uri_mut() = req.uri().clone();
        *probe.headers_mut() = req.headers().clone();

        let mut dir = self.dir.clone();
        match dir.try_call(probe).await {
            Ok(res) if res.status() != StatusCode::NOT_FOUND => {
                res.map(|b| b.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync())
            }
            // A directory URL answers 404 without a filesystem error. It then reaches PHP.
            Ok(_) => forward(inner, req).await,
            Err(e) if is_miss(&e) => forward(inner, req).await,
            // An Err outside the miss kinds is a read failure and must not reach PHP.
            // https://docs.rs/tower-http/0.7.1/tower_http/services/struct.ServeDir.html#method.try_call
            Err(e) => {
                tracing::error!(target: "http", "static probe failed for {}: {e}", req.uri().path());
                http::Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Empty::<Bytes>::new().map_err(BoxError::from).boxed_unsync())
                    .unwrap()
            }
        }
    }
}

/// Hands a miss to the inner service.
/// The `Oneshot` is boxed as `Send` before the await: the compiler cannot prove `Send` for a held `Oneshot` over this request type.
/// https://github.com/rust-lang/rust/issues/110338
async fn forward<S>(inner: S, req: http::Request<Body>) -> http::Response<Body>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>, Error = Infallible>
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let call: Pin<Box<dyn Future<Output = Result<http::Response<Body>, Infallible>> + Send>> =
        Box::pin(inner.oneshot(req));
    let Ok(res) = call.await;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The middleware cannot check an undecodable path against the dot and forbid rules, so the path is never eligible.
    #[test]
    fn an_undecodable_path_is_never_eligible() {
        let st = StaticFiles::new(PathBuf::from("/"), vec![".php".into()]);
        assert!(!st.eligible("/%FF.css"));
    }
}
