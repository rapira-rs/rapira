mod cache;
mod config;

pub use config::{Section, Settings, resolve};

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Method, StatusCode, header};
use http_body::Body as _;
use rapira_http_middleware::{
    BodyStream, Middleware, Phases, RequestCtx, RequestStep, ResponseHead, ShortResponse,
};
use tower_http::services::ServeDir;
use tower_http::services::fs::{DefaultServeDirFallback, ServeFileSystemResponseBody};

use cache::CachingBackend;

/// Serves files from a directory and passes every miss to the next middleware or PHP.
/// A permission error or a bad file name is also a miss. Any other read failure
/// answers 500. That request does not reach PHP.
pub struct StaticFiles {
    dir: ServeDir<DefaultServeDirFallback, CachingBackend>,
    forbid: Vec<String>,
}

struct ServeDirStream {
    inner: ServeFileSystemResponseBody,
    /// Remaining bytes declared by the file metadata; early EOF while positive yields `UnexpectedEof`.
    remaining: Option<u64>,
}

impl BodyStream for ServeDirStream {
    fn poll_next_chunk(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Bytes>>> {
        loop {
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    if self.remaining.is_some_and(|rem| rem > 0) {
                        self.remaining = Some(0);
                        return Poll::Ready(Some(Err(io::Error::from(
                            io::ErrorKind::UnexpectedEof,
                        ))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data()
                        && !data.is_empty()
                    {
                        if let Some(rem) = &mut self.remaining {
                            let n = data.len() as u64;
                            if n > *rem {
                                *rem = 0;
                                return Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "static file grew during read",
                                ))));
                            }
                            *rem -= n;
                        }
                        return Poll::Ready(Some(Ok(data)));
                    }
                }
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err))),
            }
        }
    }
}

impl StaticFiles {
    /// A relative `root` resolves against the process working directory.
    /// `forbid` holds ASCII case-insensitive file-name suffixes with a leading dot.
    pub fn new(root: PathBuf, forbid: Vec<String>) -> Self {
        Self {
            // The URL space belongs to PHP, so a directory URL is the app's route, not an
            // implicit index.html.
            dir: ServeDir::with_backend(root, CachingBackend::default())
                .append_index_html_on_directories(false)
                // ServeDir has no multipart/byteranges, so a multi-range request gets the
                // whole file, not 416.
                .ignore_multi_range_requests(true),
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
        // The last non-empty segment is the served file. The component walk drops trailing
        // separators, so /index.php%2F still names index.php here.
        let file = decoded
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or_default()
            .as_bytes();
        !self.forbid.iter().any(|ext| {
            let start = file.len().saturating_sub(ext.len());
            file[start..].eq_ignore_ascii_case(ext.as_bytes())
        })
    }

    async fn serve_eligible(&self, req: &mut RequestCtx) -> Option<ShortResponse> {
        let mut dir = self.dir.clone();
        let mut served = dir.try_call(probe(req, true)).await;
        // ServeDir does not check If-Range. A validator other than the current strong ETag gets
        // the whole file. https://www.rfc-editor.org/rfc/rfc9110#section-13.1.5
        if let Ok(res) = &served
            && res.headers().contains_key(header::CONTENT_RANGE)
            && let Some(validator) = req.headers.get(header::IF_RANGE)
            && res.headers().get(header::ETAG) != Some(validator)
        {
            served = dir.try_call(probe(req, false)).await;
        }
        match served {
            Ok(res) if res.status() != StatusCode::NOT_FOUND => {
                // ServeDir omits Content-Type and Content-Length on 304; fetch representation metadata for outer response-head middleware (e.g. Vary and weak ETag).
                if res.status() == StatusCode::NOT_MODIFIED {
                    let mut head_probe = http::Request::new(());
                    *head_probe.method_mut() = Method::HEAD;
                    *head_probe.uri_mut() = req.uri.clone();
                    if let Ok(meta_res) = dir.try_call(head_probe).await
                        && meta_res.status() == StatusCode::OK
                        && let Some(content_type) =
                            meta_res.headers().get(header::CONTENT_TYPE).cloned()
                    {
                        let known_len = meta_res
                            .headers()
                            .get(header::CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
                        req.extensions
                            .insert(rapira_http_middleware::RepresentationMeta {
                                content_type,
                                known_len,
                            });
                    }
                }
                Some(into_short_response(req.method == Method::HEAD, res).await)
            }
            // A directory URL answers 404 without a filesystem error. It then reaches PHP.
            Ok(_) => None,
            Err(e) if is_miss(&e) => None,
            // An Err outside the miss kinds is a read failure and must not reach PHP.
            // https://docs.rs/tower-http/0.7.1/tower_http/services/struct.ServeDir.html#method.try_call
            Err(e) => {
                tracing::error!(target: "http", "static probe failed for {}: {e}", req.uri.path());
                Some(ShortResponse::full(
                    ResponseHead::new(StatusCode::INTERNAL_SERVER_ERROR.as_u16()),
                    Bytes::new(),
                ))
            }
        }
    }
}

impl Middleware for StaticFiles {
    fn phases(&self) -> Phases {
        Phases::REQUEST
    }

    fn on_request<'a>(&'a self, req: &'a mut RequestCtx) -> RequestStep<'a> {
        if req.method != Method::GET && req.method != Method::HEAD {
            return RequestStep::Next;
        }
        if !self.eligible(req.uri.path()) {
            return RequestStep::Next;
        }
        RequestStep::Async(Box::pin(async move { self.serve_eligible(req).await }))
    }
}

async fn into_short_response(
    is_head: bool,
    res: http::Response<ServeFileSystemResponseBody>,
) -> ShortResponse {
    let (parts, body) = res.into_parts();
    let status = parts.status.as_u16();
    let content_length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let head = ResponseHead {
        status,
        headers: parts.headers,
        known_len: content_length,
    };
    if is_head || matches!(status, 204 | 304) || content_length == Some(0) {
        return ShortResponse::full(head, Bytes::new());
    }
    let mut stream = ServeDirStream {
        inner: body,
        remaining: content_length,
    };
    let first = match std::future::poll_fn(|cx| Pin::new(&mut stream).poll_next_chunk(cx)).await {
        None => return ShortResponse::full(head, Bytes::new()),
        Some(Ok(data)) => data,
        Some(Err(err)) => {
            tracing::error!(target: "http", "static file read failed: {err}");
            return ShortResponse::full(
                ResponseHead::new(StatusCode::INTERNAL_SERVER_ERROR.as_u16()),
                Bytes::new(),
            );
        }
    };
    if content_length == Some(first.len() as u64) {
        return ShortResponse::full(head, first);
    }
    ShortResponse {
        head,
        body: first,
        rest: Some(Box::new(stream)),
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

/// Builds a head-only probe request for `ServeDir` carrying only the range and precondition headers it reads.
fn probe(req: &RequestCtx, include_range: bool) -> http::Request<()> {
    const PRECONDITIONS: [header::HeaderName; 4] = [
        header::IF_MATCH,
        header::IF_UNMODIFIED_SINCE,
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
    ];
    let mut probe = http::Request::new(());
    *probe.method_mut() = req.method.clone();
    *probe.uri_mut() = req.uri.clone();
    let dst = probe.headers_mut();
    if include_range {
        for val in req.headers.get_all(header::RANGE) {
            dst.append(header::RANGE, val.clone());
        }
    }
    for name in PRECONDITIONS {
        for val in req.headers.get_all(&name) {
            dst.append(name.clone(), val.clone());
        }
    }
    probe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_match_the_end_of_the_file_name_in_ascii_case() {
        struct Case {
            name: &'static str,
            path: &'static str,
            eligible: bool,
        }
        let cases = [
            Case {
                name: "mixed case compound suffix",
                path: "/archive.TaR.Gz",
                eligible: false,
            },
            Case {
                name: "suffix before the end",
                path: "/archive.tar.gz.txt",
                eligible: true,
            },
            Case {
                name: "shorter than the suffix",
                path: "/a",
                eligible: true,
            },
            Case {
                name: "suffix window starts inside a UTF-8 character",
                path: "/ééx",
                eligible: true,
            },
        ];
        let files = StaticFiles::new(PathBuf::from("/"), vec![".TaR.gZ".into(), ".LOG".into()]);
        for case in cases {
            assert_eq!(files.eligible(case.path), case.eligible, "{}", case.name);
        }
    }

    /// The middleware cannot check an undecodable path against the dot and forbid rules, so the path is never eligible.
    #[test]
    fn an_undecodable_path_is_never_eligible() {
        let st = StaticFiles::new(PathBuf::from("/"), vec![".php".into()]);
        assert!(!st.eligible("/%FF.css"));
    }

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn modified_stream_reports_size_mismatch_errors() {
        #[derive(Clone, Copy)]
        enum Mutation {
            None,
            TruncateBeforeFirst,
            TruncateAfterFirst,
            GrowBeforeFirst,
            GrowAfterFirst,
        }
        struct Case {
            name: &'static str,
            mutation: Mutation,
            want_status: u16,
            want_stream_err: Option<std::io::ErrorKind>,
        }
        let cases = [
            Case {
                name: "file truncated to zero before first read returns 500",
                mutation: Mutation::TruncateBeforeFirst,
                want_status: 500,
                want_stream_err: None,
            },
            Case {
                name: "file truncated mid-stream returns unexpected eof error",
                mutation: Mutation::TruncateAfterFirst,
                want_status: 200,
                want_stream_err: Some(std::io::ErrorKind::UnexpectedEof),
            },
            Case {
                name: "file grown before first read returns invalid data error",
                mutation: Mutation::GrowBeforeFirst,
                want_status: 200,
                want_stream_err: Some(std::io::ErrorKind::InvalidData),
            },
            Case {
                name: "file grown mid-stream returns invalid data error",
                mutation: Mutation::GrowAfterFirst,
                want_status: 200,
                want_stream_err: Some(std::io::ErrorKind::InvalidData),
            },
            Case {
                name: "unmodified large file streams cleanly to end",
                mutation: Mutation::None,
                want_status: 200,
                want_stream_err: None,
            },
        ];

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let size = 300 * 1024;
        let payload = vec![b'a'; size];

        for (idx, case) in cases.iter().enumerate() {
            let root = std::env::temp_dir()
                .join(format!("rapira-static-trunc-{}-{idx}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let _guard = TempDir(root.clone());
            let file_path = root.join("large.txt");
            std::fs::write(&file_path, &payload).unwrap();

            rt.block_on(async {
                let mut dir = ServeDir::with_backend(root.clone(), CachingBackend::default())
                    .append_index_html_on_directories(false);
                let req = http::Request::builder()
                    .method(Method::GET)
                    .uri("/large.txt")
                    .body(())
                    .unwrap();
                let res = dir.try_call(req).await.unwrap();
                assert_eq!(res.status(), StatusCode::OK, "{}", case.name);

                match case.mutation {
                    Mutation::TruncateBeforeFirst => {
                        std::fs::File::options()
                            .write(true)
                            .open(&file_path)
                            .unwrap()
                            .set_len(0)
                            .unwrap();
                    }
                    Mutation::GrowBeforeFirst => {
                        use std::io::Write as _;
                        let mut f = std::fs::File::options()
                            .append(true)
                            .open(&file_path)
                            .unwrap();
                        f.write_all(&vec![b'b'; 128 * 1024]).unwrap();
                    }
                    _ => {}
                }

                let mut short = into_short_response(false, res).await;
                assert_eq!(short.head.status, case.want_status, "{}", case.name);

                match case.mutation {
                    Mutation::TruncateAfterFirst => {
                        std::fs::File::options()
                            .write(true)
                            .open(&file_path)
                            .unwrap()
                            .set_len(0)
                            .unwrap();
                    }
                    Mutation::GrowAfterFirst => {
                        use std::io::Write as _;
                        let mut f = std::fs::File::options()
                            .append(true)
                            .open(&file_path)
                            .unwrap();
                        f.write_all(&vec![b'b'; 128 * 1024]).unwrap();
                    }
                    _ => {}
                }

                let mut total = short.body.len();
                let mut got_err: Option<std::io::ErrorKind> = None;
                if let Some(mut rest) = short.rest.take() {
                    while let Some(item) =
                        std::future::poll_fn(|cx| Pin::new(rest.as_mut()).poll_next_chunk(cx)).await
                    {
                        match item {
                            Ok(chunk) => total += chunk.len(),
                            Err(err) => {
                                got_err = Some(err.kind());
                                break;
                            }
                        }
                    }
                }
                assert_eq!(got_err, case.want_stream_err, "{}", case.name);
                if case.want_status == 200 && case.want_stream_err.is_none() {
                    assert_eq!(total, size, "{}", case.name);
                }
            });
        }
    }
}
