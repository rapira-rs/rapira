//! Push-based HTTP response compression middleware (`gzip` and `zstd`).
//!
//! Phase 1 parses `Accept-Encoding` synchronously on the IO shard.
//! Phase 2 inspects status, headers, MIME type, and known body size on the IO shard.
//! Phase 3 encodes body slices on the IO shard, or on the blocking pool for large complete bodies, before direct socket writes.

mod config;

use std::io::{self, Write};

use flate2::Compression;
use flate2::write::GzEncoder;
use http::Extensions;
use http::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, ETAG, HeaderValue, VARY,
};
use rapira_http_middleware::{
    BodyEncoder, Middleware, Phases, RepresentationMeta, RequestCtx, RequestStep, ResponseHead,
};

pub use config::{Section, Settings, resolve};

/// Negotiated response content coding stored in `RequestCtx::extensions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Gzip,
    Zstd,
}

/// Compresses eligible HTTP responses with `zstd` or `gzip`.
pub struct Compress {
    settings: Settings,
}

impl Compress {
    pub fn new(settings: Settings) -> Self {
        Self { settings }
    }
}

impl Middleware for Compress {
    fn phases(&self) -> Phases {
        Phases::REQUEST.union(Phases::RESPONSE_HEAD)
    }

    fn on_request<'a>(&'a self, req: &'a mut RequestCtx) -> RequestStep<'a> {
        if let Some(enc) = negotiate_encoding(
            req.headers
                .get_all(ACCEPT_ENCODING)
                .iter()
                .map(HeaderValue::as_bytes),
        ) {
            req.extensions.insert(enc);
        }
        RequestStep::Next
    }

    fn on_response_head(
        &self,
        is_head: bool,
        extensions: &Extensions,
        head: &mut ResponseHead,
    ) -> Option<Box<dyn BodyEncoder>> {
        let is_not_modified = head.status == 304;
        if !is_not_modified
            && (!(200..300).contains(&head.status) || matches!(head.status, 204..=206))
        {
            return None;
        }
        if head.headers.contains_key(&CONTENT_ENCODING) || head.headers.contains_key(&CONTENT_RANGE)
        {
            return None;
        }
        if has_no_transform(head.headers.get_all(&CACHE_CONTROL).iter()) {
            return None;
        }
        let rep_meta = is_not_modified
            .then(|| extensions.get::<RepresentationMeta>())
            .flatten();
        let ct = head
            .headers
            .get(&CONTENT_TYPE)
            .or_else(|| rep_meta.map(|m| &m.content_type))?;
        if !is_compressible_mime(ct.as_bytes()) {
            return None;
        }
        let known_len = head
            .known_len
            .or_else(|| rep_meta.and_then(|m| m.known_len));
        if known_len.is_some_and(|len| len == 0 || len < self.settings.min_size) {
            return None;
        }

        // RFC 9110 §12.5.5 and §15.4.5: caches key negotiated responses (including 304) on Accept-Encoding.
        // https://www.rfc-editor.org/rfc/rfc9110#section-15.4.5
        ensure_vary_accept_encoding(head);

        let enc = *extensions.get::<Encoding>()?;
        if is_not_modified {
            weaken_etag(head);
            return None;
        }
        let encoder: Option<Box<dyn BodyEncoder>> = if is_head {
            None
        } else {
            let enc = match enc {
                Encoding::Gzip => CompressEncoder::gzip(self.settings.gzip_level),
                Encoding::Zstd => CompressEncoder::zstd(self.settings.zstd_level).ok()?,
            };
            Some(Box::new(enc))
        };
        apply_encoded_headers(head, enc);
        encoder
    }
}

fn apply_encoded_headers(head: &mut ResponseHead, enc: Encoding) {
    let val = match enc {
        Encoding::Gzip => HeaderValue::from_static("gzip"),
        Encoding::Zstd => HeaderValue::from_static("zstd"),
    };
    head.headers.insert(CONTENT_ENCODING, val);
    head.headers.remove(&CONTENT_LENGTH);
    head.headers.remove(&ACCEPT_RANGES);
    head.known_len = None;
    weaken_etag(head);
}

/// RFC 9110 §8.8.3: encoding changes the representation bytes, so a strong ETag becomes weak.
/// https://www.rfc-editor.org/rfc/rfc9110#section-8.8.3
fn weaken_etag(head: &mut ResponseHead) {
    let Some(val) = head.headers.get(&ETAG) else {
        return;
    };
    let bytes = val.as_bytes();
    if bytes.starts_with(b"\"") {
        let mut weak = Vec::with_capacity(bytes.len() + 2);
        weak.extend_from_slice(b"W/");
        weak.extend_from_slice(bytes);
        if let Ok(hv) = HeaderValue::from_bytes(&weak) {
            head.headers.insert(ETAG, hv);
        }
    }
}

fn ensure_vary_accept_encoding(head: &mut ResponseHead) {
    for val in head.headers.get_all(&VARY) {
        for token in val.as_bytes().split(|&b| b == b',') {
            let trimmed = token.trim_ascii();
            if trimmed == b"*" || trimmed.eq_ignore_ascii_case(b"accept-encoding") {
                return;
            }
        }
    }
    head.headers
        .append(VARY, HeaderValue::from_static("Accept-Encoding"));
}

/// RFC 9111 §5.2.2.6: `no-transform` forbids intermediate content-coding changes.
/// https://www.rfc-editor.org/rfc/rfc9111#section-5.2.2.6
fn has_no_transform<'a>(values: impl Iterator<Item = &'a HeaderValue>) -> bool {
    for val in values {
        for directive in val.as_bytes().split(|&b| b == b',') {
            if directive.trim_ascii().eq_ignore_ascii_case(b"no-transform") {
                return true;
            }
        }
    }
    false
}

/// Parses RFC 9110 §12.5.3 `Accept-Encoding` quality weights (`0..=1000`).
/// Prefers `zstd` over `gzip` when weights are equal.
/// https://www.rfc-editor.org/rfc/rfc9110#section-12.5.3
fn negotiate_encoding<'a>(values: impl Iterator<Item = &'a [u8]>) -> Option<Encoding> {
    let mut gzip_q: Option<u16> = None;
    let mut zstd_q: Option<u16> = None;
    let mut star_q: Option<u16> = None;

    for header in values {
        for item in header.split(|&b| b == b',') {
            let item = item.trim_ascii();
            if item.is_empty() {
                continue;
            }
            let mut parts = item.split(|&b| b == b';');
            let coding = parts.next().unwrap_or_default().trim_ascii();
            if coding.is_empty() {
                continue;
            }
            let mut q: u16 = 1000;
            let mut valid = true;
            for param in parts {
                let param = param.trim_ascii();
                if let Some(idx) = param.iter().position(|&b| b == b'=')
                    && param[..idx].trim_ascii().eq_ignore_ascii_case(b"q")
                {
                    match parse_qvalue(param[idx + 1..].trim_ascii()) {
                        Some(parsed) => q = parsed,
                        None => valid = false,
                    }
                }
            }
            if !valid {
                continue;
            }
            if coding.eq_ignore_ascii_case(b"zstd") {
                zstd_q = Some(q);
            } else if coding.eq_ignore_ascii_case(b"gzip") || coding.eq_ignore_ascii_case(b"x-gzip")
            {
                gzip_q = Some(q);
            } else if coding == b"*" {
                star_q = Some(q);
            }
        }
    }

    let zstd = zstd_q.or(star_q).unwrap_or(0);
    let gzip = gzip_q.or(star_q).unwrap_or(0);

    if zstd == 0 && gzip == 0 {
        None
    } else if zstd >= gzip {
        Some(Encoding::Zstd)
    } else {
        Some(Encoding::Gzip)
    }
}

/// Parses RFC 9110 §12.4.2 `qvalue` (`0`..`1` with up to 3 decimal digits) as `0..=1000`.
/// https://www.rfc-editor.org/rfc/rfc9110#section-12.4.2
fn parse_qvalue(raw: &[u8]) -> Option<u16> {
    match raw {
        b"1" | b"1." | b"1.0" | b"1.00" | b"1.000" => Some(1000),
        b"0" | b"0." => Some(0),
        _ if raw.starts_with(b"0.") && raw.len() <= 5 => {
            let frac = &raw[2..];
            if !frac.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let mut val: u16 = 0;
            let mut scale: u16 = 100;
            for &b in frac {
                val += u16::from(b - b'0') * scale;
                scale /= 10;
            }
            Some(val)
        }
        _ => None,
    }
}

/// Returns true for text and structured media types that compress cleanly.
fn is_compressible_mime(content_type: &[u8]) -> bool {
    let media = content_type
        .split(|&b| b == b';')
        .next()
        .unwrap_or_default()
        .trim_ascii();
    if media.len() >= 5 && media[..5].eq_ignore_ascii_case(b"text/") {
        return true;
    }
    const TYPES: [&[u8]; 8] = [
        b"application/json",
        b"application/javascript",
        b"application/xml",
        b"application/xhtml+xml",
        b"application/rss+xml",
        b"application/atom+xml",
        b"application/wasm",
        b"image/svg+xml",
    ];
    if TYPES.iter().any(|t| media.eq_ignore_ascii_case(t)) {
        return true;
    }
    let len = media.len();
    (len > 5 && media[len - 5..].eq_ignore_ascii_case(b"+json"))
        || (len > 4 && media[len - 4..].eq_ignore_ascii_case(b"+xml"))
}

enum CompressEncoder {
    Gzip(GzEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
    Finished,
}

impl CompressEncoder {
    fn gzip(level: u32) -> Self {
        Self::Gzip(GzEncoder::new(
            Vec::with_capacity(4096),
            Compression::new(level),
        ))
    }

    fn zstd(level: i32) -> io::Result<Self> {
        Ok(Self::Zstd(zstd::stream::write::Encoder::new(
            Vec::with_capacity(4096),
            level,
        )?))
    }

    fn drain_into(&mut self, out: &mut Vec<u8>) {
        let buf = match self {
            Self::Gzip(enc) => enc.get_mut(),
            Self::Zstd(enc) => enc.get_mut(),
            Self::Finished => return,
        };
        if !buf.is_empty() {
            if out.is_empty() {
                std::mem::swap(out, buf);
            } else {
                out.extend_from_slice(buf);
                buf.clear();
            }
        }
    }
}

impl BodyEncoder for CompressEncoder {
    fn write(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        match self {
            Self::Gzip(enc) => enc.write_all(chunk)?,
            Self::Zstd(enc) => enc.write_all(chunk)?,
            Self::Finished => return Ok(()),
        }
        self.drain_into(out);
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        match self {
            Self::Gzip(enc) => enc.flush()?,
            Self::Zstd(enc) => enc.flush()?,
            Self::Finished => return Ok(()),
        }
        self.drain_into(out);
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let tail = match std::mem::replace(self, Self::Finished) {
            Self::Gzip(enc) => enc.finish()?,
            Self::Zstd(enc) => enc.finish()?,
            Self::Finished => return Ok(()),
        };
        if !tail.is_empty() {
            if out.is_empty() {
                *out = tail;
            } else {
                out.extend_from_slice(&tail);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_encoding_follows_rfc9110_quality_weights() {
        struct Case {
            name: &'static str,
            headers: &'static [&'static [u8]],
            want: Option<Encoding>,
        }
        let cases = [
            Case {
                name: "empty accept-encoding selects none",
                headers: &[],
                want: None,
            },
            Case {
                name: "gzip only",
                headers: &[b"gzip"],
                want: Some(Encoding::Gzip),
            },
            Case {
                name: "x-gzip alias",
                headers: &[b"x-gzip"],
                want: Some(Encoding::Gzip),
            },
            Case {
                name: "zstd preferred over gzip on equal quality",
                headers: &[b"gzip, deflate, zstd"],
                want: Some(Encoding::Zstd),
            },
            Case {
                name: "higher quality gzip wins over zstd",
                headers: &[b"zstd;q=0.5, gzip;q=0.9"],
                want: Some(Encoding::Gzip),
            },
            Case {
                name: "q=0 disables explicit coding",
                headers: &[b"zstd;q=0, gzip;q=1.0"],
                want: Some(Encoding::Gzip),
            },
            Case {
                name: "wildcard enables zstd",
                headers: &[b"*"],
                want: Some(Encoding::Zstd),
            },
            Case {
                name: "explicit zstd;q=0 overrides wildcard",
                headers: &[b"zstd;q=0, *;q=0.5"],
                want: Some(Encoding::Gzip),
            },
            Case {
                name: "all codings rejected by q=0",
                headers: &[b"gzip;q=0, zstd;q=0, *;q=0"],
                want: None,
            },
        ];
        for case in cases {
            let got = negotiate_encoding(case.headers.iter().copied());
            assert_eq!(got, case.want, "{}", case.name);
        }
    }

    #[test]
    fn compressible_mime_matches_text_and_structured_types() {
        struct Case {
            name: &'static str,
            mime: &'static [u8],
            want: bool,
        }
        let cases = [
            Case {
                name: "text plain with charset",
                mime: b"text/plain; charset=utf-8",
                want: true,
            },
            Case {
                name: "text html uppercase",
                mime: b"TEXT/HTML",
                want: true,
            },
            Case {
                name: "application json",
                mime: b"application/json",
                want: true,
            },
            Case {
                name: "structured +json suffix",
                mime: b"application/problem+json; charset=utf-8",
                want: true,
            },
            Case {
                name: "svg xml",
                mime: b"image/svg+xml",
                want: true,
            },
            Case {
                name: "png binary",
                mime: b"image/png",
                want: false,
            },
            Case {
                name: "octet stream",
                mime: b"application/octet-stream",
                want: false,
            },
        ];
        for case in cases {
            assert_eq!(is_compressible_mime(case.mime), case.want, "{}", case.name);
        }
    }

    #[test]
    fn config_rejects_out_of_range_levels() {
        struct Case {
            name: &'static str,
            section: Section,
            want_err: bool,
        }
        let cases = [
            Case {
                name: "defaults resolve",
                section: Section::default(),
                want_err: false,
            },
            Case {
                name: "gzip level 0 rejected",
                section: Section {
                    gzip_level: Some(0),
                    ..Default::default()
                },
                want_err: true,
            },
            Case {
                name: "gzip level 10 rejected",
                section: Section {
                    gzip_level: Some(10),
                    ..Default::default()
                },
                want_err: true,
            },
            Case {
                name: "zstd level 0 rejected",
                section: Section {
                    zstd_level: Some(0),
                    ..Default::default()
                },
                want_err: true,
            },
            Case {
                name: "zstd level 20 rejected",
                section: Section {
                    zstd_level: Some(20),
                    ..Default::default()
                },
                want_err: true,
            },
        ];
        for case in cases {
            assert_eq!(
                resolve(case.section).is_err(),
                case.want_err,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn on_response_head_status_and_conditional_rules() {
        struct Case {
            name: &'static str,
            status: u16,
            min_size: u64,
            known_len: Option<u64>,
            content_type: Option<&'static str>,
            etag: Option<&'static str>,
            accept: Option<Encoding>,
            want_encoder: bool,
            want_ce: Option<&'static str>,
            want_vary: bool,
            want_etag: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "205 with min_size 0 does not compress",
                status: 205,
                min_size: 0,
                known_len: Some(0),
                content_type: Some("text/plain"),
                etag: None,
                accept: Some(Encoding::Gzip),
                want_encoder: false,
                want_ce: None,
                want_vary: false,
                want_etag: None,
            },
            Case {
                name: "205 streamed without known_len does not compress",
                status: 205,
                min_size: 64,
                known_len: None,
                content_type: Some("text/plain"),
                etag: None,
                accept: Some(Encoding::Gzip),
                want_encoder: false,
                want_ce: None,
                want_vary: false,
                want_etag: None,
            },
            Case {
                name: "200 empty body with min_size 0 does not compress",
                status: 200,
                min_size: 0,
                known_len: Some(0),
                content_type: Some("text/plain"),
                etag: Some("\"empty\""),
                accept: Some(Encoding::Gzip),
                want_encoder: false,
                want_ce: None,
                want_vary: false,
                want_etag: Some("\"empty\""),
            },
            Case {
                name: "304 compressible with gzip adds vary and weakens etag",
                status: 304,
                min_size: 64,
                known_len: Some(2000),
                content_type: Some("text/plain"),
                etag: Some("\"v1\""),
                accept: Some(Encoding::Gzip),
                want_encoder: false,
                want_ce: None,
                want_vary: true,
                want_etag: Some("W/\"v1\""),
            },
            Case {
                name: "304 compressible without accept-encoding adds vary and keeps strong etag",
                status: 304,
                min_size: 64,
                known_len: Some(2000),
                content_type: Some("text/plain"),
                etag: Some("\"v1\""),
                accept: None,
                want_encoder: false,
                want_ce: None,
                want_vary: true,
                want_etag: Some("\"v1\""),
            },
            Case {
                name: "304 incompressible keeps strong etag without vary",
                status: 304,
                min_size: 64,
                known_len: Some(2000),
                content_type: Some("image/png"),
                etag: Some("\"v1\""),
                accept: Some(Encoding::Gzip),
                want_encoder: false,
                want_ce: None,
                want_vary: false,
                want_etag: Some("\"v1\""),
            },
            Case {
                name: "304 below min_size keeps strong etag without vary",
                status: 304,
                min_size: 64,
                known_len: Some(16),
                content_type: Some("text/plain"),
                etag: Some("\"v1\""),
                accept: Some(Encoding::Gzip),
                want_encoder: false,
                want_ce: None,
                want_vary: false,
                want_etag: Some("\"v1\""),
            },
        ];

        for case in cases {
            let mw = Compress::new(
                resolve(Section {
                    min_size: Some(case.min_size),
                    ..Default::default()
                })
                .unwrap(),
            );
            let mut ext = Extensions::new();
            if let Some(enc) = case.accept {
                ext.insert(enc);
            }
            let mut head = ResponseHead::new(case.status);
            head.known_len = case.known_len;
            if let Some(ct) = case.content_type {
                head.headers
                    .insert(CONTENT_TYPE, HeaderValue::from_static(ct));
            }
            if let Some(etag) = case.etag {
                head.headers.insert(ETAG, HeaderValue::from_static(etag));
            }
            let enc = mw.on_response_head(false, &ext, &mut head);
            assert_eq!(enc.is_some(), case.want_encoder, "{}", case.name);
            assert_eq!(
                head.headers
                    .get(&CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
                case.want_ce,
                "{}",
                case.name
            );
            let has_vary = head
                .headers
                .get_all(&VARY)
                .iter()
                .any(|v| v.as_bytes().eq_ignore_ascii_case(b"accept-encoding"));
            assert_eq!(has_vary, case.want_vary, "{}", case.name);
            assert_eq!(
                head.headers.get(&ETAG).and_then(|v| v.to_str().ok()),
                case.want_etag,
                "{}",
                case.name
            );
        }
    }
}
