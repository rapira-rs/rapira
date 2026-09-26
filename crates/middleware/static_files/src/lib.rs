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
    use http_body_util::Full;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use tower::util::BoxCloneService;
    use tower::{Layer as _, Service as _};

    type HttpRequest = http::Request<Body>;
    type HttpResponse = http::Response<Body>;
    type Service = BoxCloneService<HttpRequest, HttpResponse, Infallible>;

    /// An extension the http plugin would insert before the chain.
    #[derive(Clone)]
    struct Marker;

    /// Marks fallthrough: the response reports whether the extensions and body survived.
    fn fallthrough() -> Service {
        Service::new(tower::service_fn(|req: HttpRequest| async move {
            let kept = req.extensions().get::<Marker>().is_some();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Ok(http::Response::builder()
                .status(200)
                .header("x-handler", "php")
                .header("x-extensions", if kept { "kept" } else { "lost" })
                .body(Full::new(body).map_err(|e| match e {}).boxed_unsync())
                .unwrap())
        }))
    }

    fn request(method: &str, path: &str, body: &str) -> HttpRequest {
        let mut req = http::Request::builder()
            .method(method)
            .uri(path)
            .body(
                Full::new(Bytes::from(body.to_owned()))
                    .map_err(|e| match e {})
                    .boxed_unsync(),
            )
            .unwrap();
        req.extensions_mut().insert(Marker);
        req
    }

    /// The layer over the fallthrough service.
    fn service(st: StaticFiles) -> Service {
        st.layer().layer(fallthrough())
    }

    async fn run(st: StaticFiles, req: HttpRequest) -> HttpResponse {
        run_shared(&service(st), req).await
    }

    /// A cache test sends more than one request to the same instance.
    /// The future holds its own clone, so a spawned task can await it.
    fn run_shared(st: &Service, req: HttpRequest) -> impl Future<Output = HttpResponse> + use<> {
        let mut st = st.clone();
        async move {
            let Ok(ready) = st.ready().await;
            let Ok(res) = ready.call(req).await;
            res
        }
    }

    /// Use whole seconds. An ext4 volume with 128-byte inodes has no nanosecond field, so it
    /// keeps only the seconds of the mtime.
    /// https://www.kernel.org/doc/html/latest/filesystems/ext4/inodes.html#inode-timestamps
    fn write_at(path: &std::path::Path, contents: &str, mtime_secs: u64) {
        std::fs::write(path, contents).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs))
            .unwrap();
    }

    /// Sleeps past the one second freshness window.
    fn past_the_ttl() {
        std::thread::sleep(Duration::from_millis(1100));
    }

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("styles.css"), "body{}").unwrap();
        std::fs::write(dir.path().join("data.bin"), "abcdefghij").unwrap();
        std::fs::write(dir.path().join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(dir.path().join("index.php"), "<?php secret();").unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "[core]").unwrap();
        std::fs::write(dir.path().join("Upper.PHP"), "<?php upper();").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("index.php"), "<?php sub();").unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets").join("a.css"), "a{}").unwrap();
        dir
    }

    fn static_files(dir: &tempfile::TempDir) -> StaticFiles {
        StaticFiles::new(dir.path().to_path_buf(), vec![".php".to_owned()])
    }

    fn header<'r>(res: &'r HttpResponse, name: &str) -> &'r str {
        res.headers()
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .to_str()
            .unwrap()
    }

    async fn body(res: HttpResponse) -> Bytes {
        res.into_body().collect().await.unwrap().to_bytes()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serves_a_file_with_type_length_and_validators() {
        let dir = root();
        let res = run(static_files(&dir), request("GET", "/styles.css", "")).await;
        assert_eq!(res.status(), 200);
        assert!(res.headers().get("x-handler").is_none());
        assert_eq!(header(&res, "content-type"), "text/css");
        assert_eq!(header(&res, "content-length"), "6");
        assert_eq!(header(&res, "accept-ranges"), "bytes");
        assert!(res.headers().contains_key("etag"));
        assert!(res.headers().contains_key("last-modified"));
        assert_eq!(body(res).await, "body{}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_miss_falls_through_with_the_request_intact() {
        let dir = root();
        let res = run(static_files(&dir), request("GET", "/nope.txt", "ping")).await;
        assert_eq!(header(&res, "x-handler"), "php");
        assert_eq!(header(&res, "x-extensions"), "kept");
        assert_eq!(body(res).await, "ping");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_get_head_methods_fall_through() {
        let dir = root();
        let res = run(static_files(&dir), request("POST", "/styles.css", "p")).await;
        assert_eq!(header(&res, "x-handler"), "php");
        assert_eq!(body(res).await, "p");
    }

    /// The encoded-slash forms decode to a trailing separator that ServeDir drops when it resolves the file.
    #[tokio::test(flavor = "current_thread")]
    async fn forbidden_extensions_fall_through_even_when_the_file_exists() {
        let dir = root();
        for path in [
            "/index.php",
            "/index%2Ephp",
            "/Upper.PHP",
            "/index.php%2F",
            "/index.php%2F%2F",
            "/sub/index.php%2F",
        ] {
            let res = run(static_files(&dir), request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }
    }

    /// The constructor lowercases the entries, so an uppercase entry still blocks the PHP source.
    #[tokio::test(flavor = "current_thread")]
    async fn uppercase_forbid_needles_are_normalized() {
        let dir = root();
        let st = StaticFiles::new(dir.path().to_path_buf(), vec![".PHP".to_owned()]);
        let res = run(st, request("GET", "/index.php", "")).await;
        assert_eq!(header(&res, "x-handler"), "php");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn directory_routes_fall_through() {
        let dir = root();
        std::fs::write(dir.path().join("sub").join("index.html"), "<h1>s</h1>").unwrap();
        let st = service(static_files(&dir));
        for path in ["/", "/assets", "/assets/", "/sub", "/sub/"] {
            let res = run_shared(&st, request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }

        for (path, expected) in [("/index.html", "<h1>hi</h1>"), ("/assets/a.css", "a{}")] {
            let res = run_shared(&st, request("GET", path, "")).await;
            assert_eq!(res.status(), 200, "{path}");
            assert_eq!(body(res).await, expected, "{path}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_empty_forbid_list_serves_php_sources() {
        let dir = root();
        let st = StaticFiles::new(dir.path().to_path_buf(), Vec::new());
        let res = run(st, request("GET", "/index.php", "")).await;
        assert_eq!(res.status(), 200);
        assert!(res.headers().get("x-handler").is_none());
        assert_eq!(body(res).await, "<?php secret();");
    }

    /// An unsatisfiable byte range answers 416 (RFC 9110 section 15.5.17, https://www.rfc-editor.org/rfc/rfc9110#section-15.5.17).
    /// The response carries unsatisfied-range `"*/" complete-length` (section 14.4, https://www.rfc-editor.org/rfc/rfc9110#section-14.4).
    /// The file exists, so the middleware answers and PHP does not see the request.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unsatisfiable_range_answers_416_without_reaching_php() {
        let dir = root();
        let mut req = request("GET", "/data.bin", "");
        req.headers_mut()
            .insert("range", "bytes=100-200".parse().unwrap());
        let res = run(static_files(&dir), req).await;
        assert_eq!(res.status(), 416);
        assert_eq!(header(&res, "content-range"), "bytes */10");
        assert!(res.headers().get("x-handler").is_none());
    }

    /// A symlink loop fails with ELOOP. That kind is not in ServeDir's miss set (NotFound,
    /// PermissionDenied, ENOTDIR) and is not a file-name error, so it is a read failure.
    #[tokio::test(flavor = "current_thread")]
    async fn a_real_read_failure_answers_500() {
        let dir = root();
        std::os::unix::fs::symlink("loop.css", dir.path().join("loop.css")).unwrap();
        let res = run(static_files(&dir), request("GET", "/loop.css", "")).await;
        assert_eq!(res.status(), 500);
        assert!(res.headers().get("x-handler").is_none());
        assert_eq!(body(res).await, "");
    }

    /// A segment over NAME_MAX surfaces from try_call as an Err with a file-name kind, not as a 404.
    #[tokio::test(flavor = "current_thread")]
    async fn an_overlong_segment_falls_through() {
        let dir = root();
        let long = format!("/{}", "a".repeat(300));
        let res = run(static_files(&dir), request("GET", &long, "")).await;
        assert_eq!(header(&res, "x-handler"), "php");
    }

    /// A NUL byte reaches the file-name arm on HEAD only. A GET already folds it into a 404.
    #[tokio::test(flavor = "current_thread")]
    async fn a_nul_byte_in_the_path_falls_through() {
        let dir = root();
        for method in ["GET", "HEAD"] {
            let res = run(static_files(&dir), request(method, "/a%00.css", "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{method}");
        }
    }

    /// The middleware cannot check an undecodable path against the dot and forbid rules, so the path is never eligible.
    #[test]
    fn an_undecodable_path_is_never_eligible() {
        let dir = root();
        assert!(!static_files(&dir).eligible("/%FF.css"));
    }

    /// Parent-dir segments land on the same guard: `..` starts with a dot.
    #[tokio::test(flavor = "current_thread")]
    async fn dotfile_segments_fall_through() {
        let dir = root();
        for path in [
            "/.env",
            "/.git/config",
            "/%2Eenv",
            "/../outside.txt",
            "/%2e%2e/outside.txt",
        ] {
            let res = run(static_files(&dir), request("GET", path, "")).await;
            assert_eq!(header(&res, "x-handler"), "php", "{path}");
        }
    }

    /// Byte positions are inclusive (RFC 9110 section 14.1.2, https://www.rfc-editor.org/rfc/rfc9110#section-14.1.2).
    /// Content-Range carries first-pos "-" last-pos "/" complete-length (section 14.4, https://www.rfc-editor.org/rfc/rfc9110#section-14.4).
    #[tokio::test(flavor = "current_thread")]
    async fn a_range_request_answers_the_named_bytes() {
        let dir = root();
        let mut req = request("GET", "/data.bin", "");
        req.headers_mut()
            .insert("range", "bytes=0-4".parse().unwrap());
        let res = run(static_files(&dir), req).await;
        assert_eq!(res.status(), 206);
        assert_eq!(header(&res, "content-range"), "bytes 0-4/10");
        assert_eq!(header(&res, "content-length"), "5");
        assert_eq!(body(res).await, "abcde");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn if_none_match_with_the_served_etag_answers_304() {
        let dir = root();
        let first = run(static_files(&dir), request("GET", "/data.bin", "")).await;
        let etag = header(&first, "etag").to_owned();

        let mut req = request("GET", "/data.bin", "");
        req.headers_mut()
            .insert("if-none-match", etag.parse().unwrap());
        let res = run(static_files(&dir), req).await;
        assert_eq!(res.status(), 304);
        assert_eq!(body(res).await, "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn head_answers_the_full_length_without_a_body() {
        let dir = root();
        let res = run(static_files(&dir), request("HEAD", "/data.bin", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(header(&res, "content-length"), "10");
        assert_eq!(body(res).await, "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn query_strings_do_not_affect_resolution() {
        let dir = root();
        let res = run(static_files(&dir), request("GET", "/styles.css?v=2", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(body(res).await, "body{}");
    }

    /// The test deletes the file after the first request. Only a cached body can then answer
    /// the second request.
    #[tokio::test(flavor = "current_thread")]
    async fn a_second_request_serves_from_memory() {
        let dir = root();
        let st = service(static_files(&dir));
        let first = run_shared(&st, request("GET", "/styles.css", "")).await;
        let etag = header(&first, "etag").to_owned();
        let modified = header(&first, "last-modified").to_owned();
        std::fs::remove_file(dir.path().join("styles.css")).unwrap();

        let res = run_shared(&st, request("GET", "/styles.css", "")).await;
        assert_eq!(res.status(), 200);
        assert!(res.headers().get("x-handler").is_none());
        assert_eq!(header(&res, "etag"), etag);
        assert_eq!(header(&res, "last-modified"), modified);
        assert_eq!(header(&res, "content-type"), "text/css");
        assert_eq!(header(&res, "content-length"), "6");
        assert_eq!(body(res).await, "body{}");
    }

    /// The range starts past the first byte, so the answer needs a seek in the cached body.
    /// Content-Range carries first-pos "-" last-pos "/" complete-length (RFC 9110 section
    /// 14.4, https://www.rfc-editor.org/rfc/rfc9110#section-14.4).
    #[tokio::test(flavor = "current_thread")]
    async fn a_cached_entry_still_serves_a_range() {
        let dir = root();
        let st = service(static_files(&dir));
        run_shared(&st, request("GET", "/data.bin", "")).await;
        std::fs::remove_file(dir.path().join("data.bin")).unwrap();

        let mut req = request("GET", "/data.bin", "");
        req.headers_mut()
            .insert("range", "bytes=6-9".parse().unwrap());
        let res = run_shared(&st, req).await;
        assert_eq!(res.status(), 206);
        assert_eq!(header(&res, "content-range"), "bytes 6-9/10");
        assert_eq!(body(res).await, "ghij");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_cached_entry_still_answers_304() {
        let dir = root();
        let st = service(static_files(&dir));
        let first = run_shared(&st, request("GET", "/data.bin", "")).await;
        let etag = header(&first, "etag").to_owned();
        std::fs::remove_file(dir.path().join("data.bin")).unwrap();

        let mut req = request("GET", "/data.bin", "");
        req.headers_mut()
            .insert("if-none-match", etag.parse().unwrap());
        let res = run_shared(&st, req).await;
        assert_eq!(res.status(), 304);
        assert_eq!(body(res).await, "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn head_serves_cached_metadata() {
        let dir = root();
        let st = service(static_files(&dir));
        run_shared(&st, request("GET", "/data.bin", "")).await;
        std::fs::remove_file(dir.path().join("data.bin")).unwrap();

        let res = run_shared(&st, request("HEAD", "/data.bin", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(header(&res, "content-length"), "10");
        assert_eq!(body(res).await, "");
    }

    /// Both bodies have six bytes. Only the mtime shows the difference.
    #[tokio::test(flavor = "current_thread")]
    async fn a_stale_entry_reloads_a_same_length_rewrite() {
        let dir = root();
        let path = dir.path().join("rewrite.css");
        write_at(&path, "aaaaaa", 1_000_000);
        let st = service(static_files(&dir));

        let first = run_shared(&st, request("GET", "/rewrite.css", "")).await;
        let etag = header(&first, "etag").to_owned();
        assert_eq!(body(first).await, "aaaaaa");

        write_at(&path, "bbbbbb", 1_000_002);
        let inside = run_shared(&st, request("GET", "/rewrite.css", "")).await;
        assert_eq!(header(&inside, "etag"), etag);
        assert_eq!(body(inside).await, "aaaaaa");

        past_the_ttl();
        let after = run_shared(&st, request("GET", "/rewrite.css", "")).await;
        assert_ne!(header(&after, "etag"), etag);
        assert_eq!(body(after).await, "bbbbbb");
    }

    /// Both writes keep the mtime. Only the length shows the difference.
    #[tokio::test(flavor = "current_thread")]
    async fn a_stale_entry_reloads_a_same_mtime_resize() {
        let dir = root();
        let path = dir.path().join("resize.css");
        write_at(&path, "aaaaaa", 1_000_000);
        let st = service(static_files(&dir));
        let first = run_shared(&st, request("GET", "/resize.css", "")).await;
        assert_eq!(body(first).await, "aaaaaa");

        write_at(&path, "aaaaaaaaa", 1_000_000);
        past_the_ttl();
        let after = run_shared(&st, request("GET", "/resize.css", "")).await;
        assert_eq!(header(&after, "content-length"), "9");
        assert_eq!(body(after).await, "aaaaaaaaa");
    }

    /// A `HEAD` revalidation reaches `Backend::metadata` and never `Backend::open`. The body
    /// must survive it, so the request after the delete still answers from memory.
    #[tokio::test(flavor = "current_thread")]
    async fn revalidation_of_an_unchanged_file_keeps_the_body() {
        let dir = root();
        let st = service(static_files(&dir));
        run_shared(&st, request("GET", "/data.bin", "")).await;

        past_the_ttl();
        let head = run_shared(&st, request("HEAD", "/data.bin", "")).await;
        assert_eq!(head.status(), 200);

        std::fs::remove_file(dir.path().join("data.bin")).unwrap();
        let res = run_shared(&st, request("GET", "/data.bin", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(body(res).await, "abcdefghij");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_deleted_file_falls_through_after_the_ttl() {
        let dir = root();
        let st = service(static_files(&dir));
        assert_eq!(
            run_shared(&st, request("GET", "/styles.css", ""))
                .await
                .status(),
            200
        );
        std::fs::remove_file(dir.path().join("styles.css")).unwrap();
        assert_eq!(
            run_shared(&st, request("GET", "/styles.css", ""))
                .await
                .status(),
            200
        );

        past_the_ttl();
        let res = run_shared(&st, request("GET", "/styles.css", "")).await;
        assert_eq!(header(&res, "x-handler"), "php");
    }

    /// An empty body must still be an entry. Without the entry, the second request would
    /// reach the filesystem.
    #[tokio::test(flavor = "current_thread")]
    async fn an_empty_file_is_cached() {
        let dir = root();
        std::fs::write(dir.path().join("empty.css"), "").unwrap();
        let st = service(static_files(&dir));
        assert_eq!(
            run_shared(&st, request("GET", "/empty.css", ""))
                .await
                .status(),
            200
        );
        std::fs::remove_file(dir.path().join("empty.css")).unwrap();

        let res = run_shared(&st, request("GET", "/empty.css", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(header(&res, "content-length"), "0");
        assert_eq!(body(res).await, "");
    }

    /// The file holds no entry, so `HEAD` and `GET` both read the filesystem and report the
    /// same answer (RFC 9110 section 9.3.2,
    /// https://www.rfc-editor.org/rfc/rfc9110#section-9.3.2).
    #[tokio::test(flavor = "current_thread")]
    async fn a_file_over_the_cap_is_served_and_never_stored() {
        let dir = root();
        std::fs::write(dir.path().join("big.bin"), vec![b'x'; 262_145]).unwrap();
        let st = service(static_files(&dir));

        let res = run_shared(&st, request("GET", "/big.bin", "")).await;
        assert_eq!(res.status(), 200);
        assert_eq!(header(&res, "content-length"), "262145");
        assert_eq!(body(res).await.len(), 262_145);

        std::fs::remove_file(dir.path().join("big.bin")).unwrap();
        let head = run_shared(&st, request("HEAD", "/big.bin", "")).await;
        let get = run_shared(&st, request("GET", "/big.bin", "")).await;
        assert_eq!(header(&head, "x-handler"), "php");
        assert_eq!(header(&get, "x-handler"), "php");
    }

    /// Eight requests arrive for one cold path at the same time. Every answer matches.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_fills_agree() {
        let dir = root();
        let st = service(static_files(&dir));
        let gate = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let st = st.clone();
            let gate = Arc::clone(&gate);
            tasks.push(tokio::spawn(async move {
                gate.wait().await;
                let res = run_shared(&st, request("GET", "/data.bin", "")).await;
                let etag = header(&res, "etag").to_owned();
                (etag, body(res).await)
            }));
        }

        let mut answers = Vec::new();
        for task in tasks {
            answers.push(task.await.unwrap());
        }
        for (etag, bytes) in &answers {
            assert_eq!(etag, &answers[0].0);
            assert_eq!(bytes, "abcdefghij");
        }
    }
}
