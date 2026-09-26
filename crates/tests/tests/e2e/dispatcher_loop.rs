use std::io::{Cursor, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use http::HeaderValue;
use rapira_sapi::types::Body;
use rapira_sapi::{Frame, Mode};
use tests::wire::submit;
use tests::{drain, drain_resp, fixture, req, server_log};

use crate::harness::{Server, Spawn, scratch_dir, slot_line, wait_log_contains};

/// Read budget of a raw socket exchange.
const READ: Duration = Duration::from_secs(10);

fn verbs_probe(query: &str) -> anyhow::Result<(u16, String)> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();
    Ok(drain(submit(srv.addr, req(query))?))
}

/// The contexts of the `app` records named `message` in the log of `srv`.
fn app_contexts(srv: &Server, message: &str) -> Vec<String> {
    server_log::records(&srv.log_file())
        .into_iter()
        .filter(|c| c.target == "app" && c.message == message)
        .map(|c| c.context)
        .collect()
}

/// Writes `request` on `stream` and reads the response to EOF; returns the status and the body.
/// The request must end the connection (HTTP/1.0, or `connection: close`), and the response must be length-framed.
fn raw_exchange(mut stream: impl Read + Write, request: &[u8]) -> anyhow::Result<(u16, String)> {
    stream.write_all(request)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").context("no response head")?;
    let status = head.split(' ').nth(1).context("no status")?.parse()?;
    Ok((status, body.to_owned()))
}

/// Two sequential units through the echo loop, then the graceful stop must land as `ClosedException` in the parked `receive()`.
#[test]
fn exchange_serves_sequential_requests() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/echo-loop-worker.php"))
        .json_log()
        .spawn();

    let resp = drain_resp(submit(srv.addr, req("/first"))?);
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.header("x-rapira-target").as_deref(), Some("/first"));
    assert_eq!(
        resp.body_string(),
        "method=GET body=",
        "empty request body echoes empty"
    );

    let mut rq2 = req("/second");
    rq2.body = Body::Raw(Cursor::new(b"two".to_vec()));
    let resp = drain_resp(submit(srv.addr, rq2)?);
    assert_eq!(resp.header("x-rapira-target").as_deref(), Some("/second"));
    assert_eq!(resp.body_string(), "method=GET body=two");

    srv.stop();

    assert_eq!(
        app_contexts(&srv, "drained").len(),
        1,
        "ClosedException must reach the fixture exactly once"
    );
    Ok(())
}

/// `tryReceive()`/`receive(0)`/`receive(50ms)` on an empty-but-open channel must report null/timeout, never Closed.
#[test]
fn recv_probes_on_an_empty_channel() -> anyhow::Result<()> {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture("dispatcher/recv-probes-worker.php"),
    )
    .json_log()
    .spawn();

    server_log::wait_app_record(&srv.log_file(), "recv-probes");
    srv.stop();

    let contexts = app_contexts(&srv, "recv-probes");
    assert_eq!(contexts.len(), 1, "one probe record (got {contexts:?})");
    for fragment in [
        r#""try":"null""#,
        r#""zero":"timeout""#,
        r#""short":"timeout""#,
    ] {
        assert!(
            contexts[0].contains(fragment),
            "missing {fragment} in {:?}",
            contexts[0]
        );
    }
    Ok(())
}

/// A `writeBody()` with no prior `writeHead()` commits an implicit 200.
#[test]
fn implicit_200_on_first_write_body() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/")?;
    assert_eq!((status, body.as_str()), (200, "state=false"));
    Ok(())
}

/// A second finalizing verb after the unit sealed throws `AlreadyFinalizedError`; the sealed response is untouched.
#[test]
fn double_finalize_throws_already_finalized() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php"))
        .json_log()
        .spawn();

    let (status, body) = drain(submit(srv.addr, req("/?probe=double-finalize"))?);
    assert_eq!((status, body.as_str()), (200, "first"));

    srv.stop();

    let records = app_contexts(&srv, "double-finalize");
    assert_eq!(records.len(), 1, "one throw record (got {records:?})");
    assert!(
        records[0].contains(r#""class":"Rapira\\Exception\\AlreadyFinalizedError""#),
        "wrong exception class: {:?}",
        records[0]
    );
    Ok(())
}

/// A second `writeHead()` after the final head throws `HeadAlreadyWrittenError`; the first head stands.
#[test]
fn double_head_throws_head_already_written() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=double-head")?;
    assert_eq!(status, 201, "the first head must stand");
    assert_eq!(
        body,
        "double-head:Rapira\\Http\\Exception\\HeadAlreadyWrittenError"
    );
    Ok(())
}

/// Out-of-range statuses and malformed header names, values, and shapes raise `\ValueError` before anything reaches the plugin.
#[test]
fn status_range_and_header_shape_value_errors() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=value-errors")?;
    assert_eq!(
        (status, body.as_str()),
        (200, "range:99;range:600;name;value;shape;intkey;item")
    );
    Ok(())
}

/// An empty chunk without eos must not commit the implicit 200 and lock out a later `writeHead()`.
#[test]
fn empty_non_eos_chunk_does_nothing() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=empty-chunk")?;
    assert_eq!((status, body.as_str()), (404, "body"));
    Ok(())
}

/// Verb edges: `tryReceive()` with a unit out is the single-flight `\Error`, a timeout below -1 is `\ValueError`, and `writeHead()` after eos is `HeadAlreadyWrittenError`.
#[test]
fn verb_edges_throw_their_documented_classes() -> anyhow::Result<()> {
    let mut srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php"))
        .json_log()
        .spawn();
    let (status, body) = drain(submit(srv.addr, req("/?probe=verb-edges"))?);
    assert_eq!((status, body.as_str()), (200, "try-busy;neg-timeout"));
    srv.stop();

    let records = app_contexts(&srv, "head-after-eos");
    assert_eq!(records.len(), 1, "one throw record (got {records:?})");
    assert!(
        records[0].contains(r#""class":"Rapira\\Http\\Exception\\HeadAlreadyWrittenError""#),
        "wrong exception class: {:?}",
        records[0]
    );
    Ok(())
}

/// The polling verbs' success paths: a unit comes out of `tryReceive()`, and out of `receive(1s)` after the fixture flips modes.
#[test]
fn try_and_timed_receive_serve_units() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/poll-worker.php")).spawn();

    let (status, body) = drain(submit(srv.addr, req("/one"))?);
    assert_eq!((status, body.as_str()), (200, "served-by=try target=/one"));

    let (status, body) = drain(submit(srv.addr, req("/two?mode=timed"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "served-by=try target=/two?mode=timed")
    );

    let (status, body) = drain(submit(srv.addr, req("/three"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "served-by=timed target=/three")
    );
    Ok(())
}

/// A 1xx head other than 101 leaves the unit open for the final head. The plugin does not forward the interim head.
#[test]
fn interim_head_leaves_the_final_head_open() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();
    let resp = drain_resp(submit(srv.addr, req("/?probe=interim"))?);

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "after-interim finalized=false");
    Ok(())
}

/// 101 is the carve-out in the 1xx interim rule: it commits as the final head and locks out later `writeHead()`.
/// The plugin cannot forward a final 1xx, so the client gets a 502 with no body.
#[test]
fn writehead_101_commits_as_final() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php"))
        .json_log()
        .spawn();
    let resp = drain_resp(submit(srv.addr, req("/?probe=upgrade"))?);

    assert_eq!(resp.status(), 502);
    assert!(resp.body.is_empty(), "1xx carries no body");
    // Panics when the second writeHead() does not throw HeadAlreadyWrittenError.
    server_log::wait_app_record(&srv.log_file(), "101-locked");
    Ok(())
}

/// Buffered chunks concatenate and the first commits the implicit 200: a later `writeHead()` must throw instead of restamping the status.
#[test]
fn chunked_body_buffers_and_locks_the_head() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=chunks")?;
    assert_eq!((status, body.as_str()), (200, "one-mid=false"));

    let (status, body) = verbs_probe("/?probe=head-after-chunk")?;
    assert_eq!((status, body.as_str()), (200, "partial|locked"));
    Ok(())
}

/// Multi-value lists flatten to one field line per value, and PHP references at both nesting levels are seen through.
#[test]
fn multi_value_and_reference_headers_flatten() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();

    let resp = drain_resp(submit(srv.addr, req("/?probe=multi"))?);
    let head = resp.head.as_ref().expect("head committed");
    assert_eq!(head.status, 200);
    let multi: Vec<&HeaderValue> = head.headers.get_all("x-multi").iter().collect();
    assert_eq!(multi, ["a", "b"], "one field line per list value, in order");
    assert_eq!(resp.header("x-ref").as_deref(), Some("r1"));
    assert_eq!(resp.header("x-vref").as_deref(), Some("c1"));
    Ok(())
}

/// `receive()` while a unit is unfinalized throws the single-flight `\Error` instead of deadlocking the worker on itself.
#[test]
fn receive_while_unfinalized_throws() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=busy")?;
    assert_eq!(status, 200);
    assert!(
        body.contains("busy:receive() while a Rapira\\Http\\Exchange is unfinalized"),
        "single-flight error must surface: {body:?}"
    );
    Ok(())
}

/// An Exchange dropped without finalizing fails that unit only: the plugin answers 500 and the worker serves the next unit.
#[test]
fn abandoned_exchange_fails_that_unit_only() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();

    let resp = drain_resp(submit(srv.addr, req("/?probe=abandon"))?);
    assert_eq!(
        (
            resp.status(),
            resp.body.as_slice(),
            resp.truncated,
            resp.ended
        ),
        (500, &b""[..], false, true),
        "an abandoned unit is failed by the plugin with a complete 500"
    );

    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must keep serving after an abandoned unit"
    );
    Ok(())
}

/// A unit that dies with the cycle is a worker death, not an abandonment: the reply closes with no head, so the plugin answers 502, not the 500 of an abandoned unit.
#[test]
fn bailout_with_unit_out_dies_unsent() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();

    let resp = drain_resp(submit(srv.addr, req("/?probe=bail-with-unit"))?);
    assert_eq!(
        (resp.status(), resp.ended, resp.truncated),
        (502, true, false),
        "a cycle-death loss must get no head from PHP"
    );

    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the recycled worker must keep serving"
    );
    Ok(())
}

/// An Exchange abandoned after the head reached the wire cannot become a 500: the plugin ends the stream truncated so the client detects it.
#[test]
fn abandoned_mid_stream_exchange_truncates() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();

    let resp = drain_resp(submit(srv.addr, req("/?probe=abandon-mid"))?);
    assert_eq!(resp.status(), 200, "the committed head stands");
    assert_eq!(resp.body, b"partial");
    assert!(resp.truncated, "the cut must be visible to the client");

    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!((status, body.as_str()), (200, "state=false"));
    Ok(())
}

const BOUNDARY: &str = "rapira-test-boundary";

/// A POST of a multipart/form-data body; each part is its header lines and its content.
fn multipart(uri: &str, parts: &[(&str, &str)]) -> rapira_sapi::Request {
    let mut body = String::new();
    for (headers, content) in parts {
        body += &format!("--{BOUNDARY}\r\n{headers}\r\n\r\n{content}\r\n");
    }
    body += &format!("--{BOUNDARY}--\r\n");
    let mut rq = req(uri);
    rq.method = "POST".into();
    rq.content_type = Some(format!("multipart/form-data; boundary={BOUNDARY}").into_bytes());
    rq.body = Body::Raw(Cursor::new(body.into_bytes()));
    rq
}

/// A dispatcher pool over `fixture` that spools file parts under `uploads` in the scratch dir.
fn uploads_server(fixture_name: &str) -> Server {
    Spawn::http(Mode::Dispatcher, fixture(fixture_name))
        .http_extra("[http.uploads]\ndir = \"uploads\"")
        .spawn()
}

/// The files in the worker spool dirs of `srv`. The plugin creates one spool dir per worker at start.
fn spooled_files(srv: &Server) -> Vec<PathBuf> {
    let uploads = srv.dir.join("uploads");
    let dirs: Vec<PathBuf> = std::fs::read_dir(&uploads)
        .unwrap_or_else(|e| panic!("read {}: {e}", uploads.display()))
        .map(|e| e.expect("uploads dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    assert!(!dirs.is_empty(), "no spool dir in {}", uploads.display());
    dirs.iter()
        .flat_map(|d| std::fs::read_dir(d).expect("read the spool dir"))
        .map(|e| e.expect("spool dir entry").path())
        .collect()
}

/// An abandoned unit holding a plugin-parsed multipart body must unlink its spool the moment the exchange dies.
#[test]
fn abandoned_multipart_unit_unlinks_its_spool() -> anyhow::Result<()> {
    let srv = uploads_server("dispatcher/verbs-worker.php");

    let rq = multipart(
        "/?probe=abandon",
        &[(
            "content-disposition: form-data; name=\"f\"; filename=\"a.bin\"",
            "PAYLOAD",
        )],
    );
    let (status, body) = drain(submit(srv.addr, rq)?);
    assert_eq!(
        (status, body.as_str()),
        (500, ""),
        "the plugin fails the unit"
    );
    assert_eq!(
        spooled_files(&srv),
        Vec::<PathBuf>::new(),
        "dropping the exchange must unlink the spooled file"
    );
    Ok(())
}

/// `exit()` after serving must land as `Cycle::Recycle`: the script re-runs instead of shedding as a boot failure.
#[test]
fn exit_after_serving_recycles_the_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();

    let (status, body) = drain(submit(srv.addr, req("/?probe=exit"))?);
    assert_eq!((status, body.as_str()), (200, "bye"));

    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(
        (status, body.as_str()),
        (200, "state=false"),
        "the worker must re-run the script after exit()"
    );
    Ok(())
}

/// HEAD answers with the GET status, and a 204 head stays 204 when PHP writes a body after it. HTTP framing keeps both bodies empty on the wire.
/// https://www.rfc-editor.org/rfc/rfc9112#section-6.3
#[test]
fn head_and_204_drop_the_body() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();

    let mut head_rq = req("/");
    head_rq.method = "HEAD".into();
    let (status, _) = drain(submit(srv.addr, head_rq)?);
    assert_eq!(status, 200, "HEAD keeps the GET head");

    let (status, _) = drain(submit(srv.addr, req("/?probe=head204"))?);
    assert_eq!(status, 204);
    Ok(())
}

/// The `Rapira\Http\Request` field mapping: per-line headers, the absolute-form `$target`, `$authority`, synthesized `$uri`, socket-typed addresses, null `$tls`, and the receive stamp.
#[test]
fn request_fields_reach_php() -> anyhow::Result<()> {
    const TARGET: &str = "http://example.test/path%2Fa?x=1";
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/request-worker.php")).spawn();

    // A raw socket, so the test knows the client address that PHP gets as `remote`.
    let stream = TcpStream::connect(srv.addr)?;
    stream.set_read_timeout(Some(READ))?;
    let remote = stream.local_addr()?;
    let request = format!(
        "GET {TARGET} HTTP/1.1\r\n\
         host: example.test\r\n\
         x-probe: alpha\r\n\
         x-probe: beta\r\n\
         123: numeric\r\n\
         a: solo\r\n\
         -: dash\r\n\
         -1: neg\r\n\
         content-length: 5\r\n\
         connection: close\r\n\
         \r\n\
         hello"
    );
    let (status, body) = raw_exchange(stream, request.as_bytes())?;
    assert_eq!(status, 200, "body: {body:?}");
    let target_hex = format!(
        "target-hex={}",
        TARGET
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let remote_detail = format!("remote-detail={remote}");
    let server_detail = format!("server-detail={}", srv.addr);
    for line in [
        "method=GET",
        "uri=http://example.test/path%2Fa?x=1",
        &target_hex,
        "authority='example.test'",
        "protocol=HTTP/1.1",
        "x-probe=alpha|beta",
        "h123=numeric",
        "h-single=solo",
        "h-dash=dash",
        "h-neg=neg",
        "memo-same=true",
        "body=hello",
        "remote=Rapira\\InetAddress",
        &remote_detail,
        "server=Rapira\\InetAddress",
        &server_detail,
        "tls=NULL",
        "received-at-positive=true",
        "readonly-request=Cannot modify readonly property Rapira\\Http\\Request::$method",
        "readonly-remote=Cannot modify readonly property Rapira\\InetAddress::$ip",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }

    let (status, body) = drain(submit(srv.addr, req("/again"))?);
    assert_eq!(status, 200, "body: {body:?}");
    assert!(
        body.contains("memo-same=true"),
        "fresh memo on the new unit"
    );
    Ok(())
}

/// The protocol spelling that the plugin stamps, and the no-authority `$uri` fallback to the server socket. Only an HTTP/1.0 request may omit Host.
#[test]
fn plugin_stamped_fields_pass_through() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/request-worker.php")).spawn();

    let mut rq = req("/p");
    rq.protocol = "HTTP/1.0".into();
    let (status, body) = drain(submit(srv.addr, rq)?);
    assert_eq!(status, 200);
    let uri = format!("uri=http://{}/p", srv.addr);
    for line in ["protocol=HTTP/1.0", "authority=NULL", &uri] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}

/// The UnixAddress arms: an unnamed peer and a path-carrying listener.
#[test]
fn unix_address_arms_reach_php() -> anyhow::Result<()> {
    let sock = std::env::temp_dir().join(format!("rapira-e2e-{}.sock", std::process::id()));
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/request-worker.php"))
        .http_unix(&sock)
        .http_extra("server_port = 8080")
        .spawn();

    // An unbound client socket, so the server gets no peer path. HTTP/1.0 with no Host takes the `$uri` fallback.
    let stream = UnixStream::connect(&sock)?;
    stream.set_read_timeout(Some(READ))?;
    let (status, body) = raw_exchange(stream, b"GET / HTTP/1.0\r\n\r\n")?;
    drop(srv);
    std::fs::remove_file(&sock).ok();

    assert_eq!(status, 200, "body: {body:?}");
    let server_detail = format!("server-detail='{}'", sock.display());
    for line in [
        "remote=Rapira\\UnixAddress",
        "remote-detail=NULL",
        "server=Rapira\\UnixAddress",
        &server_detail,
        "uri=http://localhost:8080/",
        "readonly-remote=Cannot modify readonly property Rapira\\UnixAddress::$path",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}

/// An asterisk-form target collapses `$uri` to the authority root.
#[test]
fn uri_synthesis_covers_asterisk_form() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/request-worker.php")).spawn();

    let mut rq = req("*");
    rq.method = "OPTIONS".into();
    rq.authority = Some(srv.addr.to_string().into_bytes());
    let (status, body) = drain(submit(srv.addr, rq)?);
    assert_eq!(status, 200);
    let uri = format!("uri=http://{}/", srv.addr);
    for line in ["method=OPTIONS", &uri, "target-hex=2a"] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    Ok(())
}

/// A plugin-parsed Multipart reaches PHP as the object graph, and seal() unlinks the spool before the response frame is sent.
#[test]
fn multipart_body_reaches_php_and_spools_die_at_seal() -> anyhow::Result<()> {
    let srv = uploads_server("dispatcher/multipart-worker.php");

    let rq = multipart(
        "/",
        &[
            ("content-disposition: form-data; name=\"note\"", "hello"),
            (
                "content-disposition: form-data; name=\"f\"; filename=\"a.bin\"\r\n\
                 content-type: application/octet-stream",
                "PAYLOAD",
            ),
        ],
    );
    let (status, body) = drain(submit(srv.addr, rq)?);
    assert_eq!(status, 200, "body: {body:?}");
    for line in [
        "class=Rapira\\Http\\Multipart",
        "counts=1/1",
        "field0=note=hello",
        "field0-cd=true",
        "file0=f:a.bin:7:PAYLOAD",
        "file0-type='application/octet-stream'",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    assert_eq!(
        spooled_files(&srv),
        Vec::<PathBuf>::new(),
        "seal() must unlink the spool before the frame goes out"
    );
    Ok(())
}

/// Two fields and two files: the graph must pair each part with its own headers, spool path, and size by index.
#[test]
fn multipart_parts_stay_index_aligned() -> anyhow::Result<()> {
    let srv = uploads_server("dispatcher/multipart-worker.php");

    let rq = multipart(
        "/",
        &[
            ("content-disposition: form-data; name=\"one\"", "1"),
            ("content-disposition: form-data; name=\"two\"", "22"),
            (
                "content-disposition: form-data; name=\"fa\"; filename=\"a.bin\"",
                "AAA",
            ),
            (
                "content-disposition: form-data; name=\"fb\"; filename=\"b.bin\"",
                "BBBBB",
            ),
        ],
    );
    let (status, body) = drain(submit(srv.addr, rq)?);
    assert_eq!(status, 200, "body: {body:?}");
    for line in [
        "counts=2/2",
        "field0=one=1",
        "field1=two=22",
        "file0=fa:a.bin:3:AAA",
        "file1=fb:b.bin:5:BBBBB",
    ] {
        assert!(body.contains(line), "missing {line:?} in {body:?}");
    }
    assert_eq!(
        spooled_files(&srv),
        Vec::<PathBuf>::new(),
        "seal unlinks both"
    );
    Ok(())
}

/// The parse outcomes that the client sees: a file part between two fields keeps document order, an empty filename makes a file part, an empty name is a 400, and a field over the limit after a file part is a 413.
/// No spool file outlives its request, also when a later part rejects the body after a file part was spooled.
#[test]
fn multipart_parse_outcomes_leave_no_spool_file() -> anyhow::Result<()> {
    struct Case<'a> {
        name: &'static str,
        parts: Vec<(&'static str, &'a str)>,
        status: u16,
        lines: &'static [&'static str],
    }
    // max_field_size_kb = 1, so a field of 1025 bytes is over the limit.
    let long = "x".repeat(1025);
    let cases = [
        Case {
            name: "file part between two fields",
            parts: vec![
                ("content-disposition: form-data; name=\"a\"", "one"),
                (
                    "content-disposition: form-data; name=\"f\"; filename=\"x.bin\"\r\n\
                     content-type: application/octet-stream",
                    "PAYLOAD",
                ),
                ("content-disposition: form-data; name=\"b\"", "two"),
            ],
            status: 200,
            lines: &[
                "counts=2/1",
                "field0=a=one",
                "field1=b=two",
                "file0=f:x.bin:7:PAYLOAD",
                "file0-type='application/octet-stream'",
                "file0-cd=true",
            ],
        },
        Case {
            name: "empty filename",
            parts: vec![(
                "content-disposition: form-data; name=\"f\"; filename=\"\"",
                "",
            )],
            status: 200,
            lines: &["counts=0/1", "file0=f::0:"],
        },
        Case {
            name: "empty name",
            parts: vec![("content-disposition: form-data; name=\"\"", "v")],
            status: 400,
            lines: &[],
        },
        Case {
            name: "field over the limit after a file part",
            parts: vec![
                (
                    "content-disposition: form-data; name=\"f\"; filename=\"a\"",
                    "DATA",
                ),
                ("content-disposition: form-data; name=\"x\"", &long),
            ],
            status: 413,
            lines: &[],
        },
    ];

    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/multipart-worker.php"))
        .http_extra("[http.uploads]\ndir = \"uploads\"\nmax_field_size_kb = 1")
        .spawn();
    for case in cases {
        let (status, body) = drain(submit(srv.addr, multipart("/", &case.parts))?);
        assert_eq!(status, case.status, "{}: {body:?}", case.name);
        for line in case.lines {
            assert!(
                body.contains(line),
                "{}: missing {line:?} in {body:?}",
                case.name
            );
        }
        assert_eq!(
            spooled_files(&srv),
            Vec::<PathBuf>::new(),
            "{}: no spool file may outlive the request",
            case.name
        );
    }
    Ok(())
}

/// At boot the master removes each spool dir under `http.uploads.dir` whose pid is gone. It keeps the dir of a live pid and every name that is not `rapira-spool-<positive pid>`.
#[test]
fn spool_sweep_removes_only_dead_pid_dirs() -> anyhow::Result<()> {
    struct Case {
        name: String,
        kept: bool,
    }
    let mut child = std::process::Command::new("true").spawn()?;
    let dead = child.id();
    child.wait()?;
    let cases = [
        Case {
            name: format!("rapira-spool-{dead}"),
            kept: false,
        },
        Case {
            name: format!("rapira-spool-{}", std::process::id()),
            kept: true,
        },
        Case {
            name: "other-dir".to_owned(),
            kept: true,
        },
        Case {
            name: "rapira-spool-".to_owned(),
            kept: true,
        },
        Case {
            name: "rapira-spool-x".to_owned(),
            kept: true,
        },
        Case {
            name: "rapira-spool-0".to_owned(),
            kept: true,
        },
        Case {
            name: "rapira-spool--5".to_owned(),
            kept: true,
        },
    ];
    let uploads = scratch_dir();
    for case in &cases {
        std::fs::create_dir(uploads.join(&case.name))?;
    }

    // The master sweeps before it binds the listener, so the sweep is done when the spawn returns.
    let _srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/multipart-worker.php"))
        .http_extra(&format!("[http.uploads]\ndir = \"{}\"", uploads.display()))
        .spawn();
    for case in &cases {
        assert_eq!(
            uploads.join(&case.name).exists(),
            case.kept,
            "{}",
            case.name
        );
    }
    Ok(())
}

/// A malformed or over-limit multipart body answers before dispatch and never reaches PHP; an accepted body reaches PHP raw. Sources: RFC 7578 (multipart/form-data needs a boundary), RFC 9110 §8.3 (content-type is a singleton field).
#[test]
fn rejected_bodies_never_reach_php() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        method: &'static str,
        content_type: &'static [&'static str],
        body: Vec<u8>,
        /// None: the request reaches PHP with `body` unparsed.
        status: Option<u16>,
    }
    const MULTIPART: &str = "multipart/form-data; boundary=B";
    const EVIL: &str = "multipart/form-data; boundary=EVIL";
    let smuggled =
        b"--EVIL\r\ncontent-disposition: form-data; name=a\r\n\r\n1\r\n--EVIL--".to_vec();
    let cases = [
        Case {
            name: "multipart without a boundary line",
            method: "POST",
            content_type: &[MULTIPART],
            body: b"no boundary here".to_vec(),
            status: Some(400),
        },
        Case {
            name: "file part over max_file_size",
            method: "POST",
            content_type: &[MULTIPART],
            body: [
                &b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\n"[..],
                &vec![b'x'; 1024 * 1024 + 1],
                b"\r\n--B--",
            ]
            .concat(),
            status: Some(413),
        },
        Case {
            name: "plain line before a repeated multipart line",
            method: "POST",
            content_type: &["text/plain", EVIL],
            body: smuggled.clone(),
            status: Some(400),
        },
        Case {
            name: "multipart line before a repeated plain line",
            method: "POST",
            content_type: &[EVIL, "text/plain"],
            body: smuggled,
            status: Some(400),
        },
        Case {
            name: "empty multipart body",
            method: "POST",
            content_type: &[MULTIPART],
            body: Vec::new(),
            status: None,
        },
        Case {
            name: "plain body that looks like multipart",
            method: "POST",
            content_type: &["text/plain"],
            body: b"--B\r\nnot really\r\n--B--".to_vec(),
            status: None,
        },
        Case {
            name: "get",
            method: "GET",
            content_type: &[],
            body: Vec::new(),
            status: None,
        },
    ];

    // The file limit is whole MiB, so the over-limit file part is one byte over 1 MiB.
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/echo-loop-worker.php"))
        .http_extra("[http.uploads]\ndir = \"uploads\"\nmax_file_size_mb = 1")
        .spawn();
    let mut accepted = 0;
    for case in cases {
        let mut rq = req("/");
        rq.method = case.method.into();
        for line in case.content_type {
            rq.headers
                .append(http::header::CONTENT_TYPE, HeaderValue::from_static(line));
        }
        rq.body = Body::Raw(Cursor::new(case.body.clone()));
        let (status, body) = drain(submit(srv.addr, rq)?);
        assert_eq!(
            status,
            case.status.unwrap_or(200),
            "{}: {body:?}",
            case.name
        );
        if case.status.is_none() {
            // echo-loop-worker.php cannot print a parsed Multipart, so a parsed body fails the status check above.
            let raw = String::from_utf8_lossy(&case.body);
            assert_eq!(
                body,
                format!("method={} body={raw}", case.method),
                "{}",
                case.name
            );
            accepted += 1;
        }
    }
    // The slot counts a unit before its response goes out, so the count is final here.
    let slot = slot_line(&srv, "");
    assert!(
        slot.contains(&format!(" handled {accepted} ")),
        "only the accepted bodies reach PHP: {slot}"
    );
    Ok(())
}

/// `getInfo()` while handling: the outstanding unit counts as active and was already decremented from `pending` at pull.
#[test]
fn get_info_counts_the_outstanding_unit() -> anyhow::Result<()> {
    let (status, body) = verbs_probe("/?probe=info")?;
    assert_eq!((status, body.as_str()), (200, "pending=0 active=1"));
    Ok(())
}

// ---- streaming (stream-worker.php): the frame protocol past the buffered one-shot

/// A dispatcher pool over stream-worker.php with a JSON log, and the frames of one request to `query`.
fn stream_probe(query: &str) -> anyhow::Result<(Server, tokio::sync::mpsc::Receiver<Frame>)> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/stream-worker.php"))
        .json_log()
        .spawn();
    let rx = submit(srv.addr, req(query))?;
    Ok((srv, rx))
}

/// `flush()` puts the head on the wire while the body is still 300ms away.
#[test]
fn flush_puts_the_head_on_the_wire_before_eos() -> anyhow::Result<()> {
    let (_srv, mut rx) = stream_probe("/?probe=flush-park")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let first = loop {
        match rx.try_recv() {
            Ok(frame) => break frame,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "flush never reached the stream"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                panic!("worker died before flushing")
            }
        }
    };
    let Frame::Head {
        head,
        content_length,
        ..
    } = first
    else {
        panic!("the first frame must be the flushed head");
    };
    assert_eq!(head.status, 200);
    assert_eq!(content_length, None, "flush costs the computed length");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "no body before the worker wakes"
    );

    let mut body = Vec::new();
    let mut ended = false;
    while let Some(frame) = rx.blocking_recv() {
        match frame {
            Frame::Chunk(b) => body.extend_from_slice(&b),
            Frame::End { truncated, .. } => {
                assert!(!truncated);
                ended = true;
                break;
            }
            _ => {}
        }
    }
    assert!(ended, "the stream must end cleanly");
    assert_eq!(body, b"after");
    Ok(())
}

/// `writeBody(eos: false)` chunks stream in order and the head carries no computed length.
#[test]
fn streamed_chunks_arrive_in_order() -> anyhow::Result<()> {
    let (_srv, rx) = stream_probe("/?probe=chunks")?;
    let resp = drain_resp(rx);

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.content_length, None);
    assert_eq!(resp.body_string(), "one,two,three");
    assert!(resp.ended && !resp.truncated);
    Ok(())
}

/// The prefix rule on an exceeded content-length: the fitting bytes are sent, the response completes per its declaration, the write throws.
#[test]
fn content_length_exceeded_sends_the_fitting_prefix() -> anyhow::Result<()> {
    let (srv, rx) = stream_probe("/?probe=cl-exceeded")?;
    let resp = drain_resp(rx);

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.content_length,
        Some(5),
        "the declared length is honoured"
    );
    assert_eq!(resp.body_string(), "01234", "the surplus is not sent");
    assert!(
        resp.ended && !resp.truncated,
        "complete per its declaration; keepalive survives"
    );
    let ctx = server_log::wait_app_record(&srv.log_file(), "cl-exceeded");
    assert!(
        ctx.contains(r#"ContentLengthExceededError"#),
        "wrong class in {ctx}"
    );
    Ok(())
}

/// Dropping the receiver mid-unit makes the next write throw WorkDiscardedException, the unit report cancelled and finalized, and the worker keep serving.
#[test]
fn dropped_client_discards_the_unit() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/stream-worker.php"))
        .json_log()
        .spawn();
    let log = srv.log_file();

    let rx = submit(srv.addr, req("/?probe=discard"))?;
    server_log::wait_app_record(&log, "discard-held");
    drop(rx);

    let ctx = server_log::wait_app_record(&log, "discard");
    assert!(
        ctx.contains("WorkDiscardedException"),
        "wrong class in {ctx}"
    );
    assert!(ctx.contains(r#""cancelled":true"#), "isCancelled in {ctx}");
    assert!(ctx.contains(r#""finalized":true"#), "isFinalized in {ctx}");

    let resp = drain_resp(submit(srv.addr, req("/?probe=chunks"))?);
    assert_eq!(resp.body_string(), "one,two,three");
    Ok(())
}

/// A declared content-length rides the head as the framing. On an under-run the plugin cuts the body, and nothing is PHP-visible.
#[test]
fn declared_content_length_rides_the_head_frame() -> anyhow::Result<()> {
    let (_srv, rx) = stream_probe("/?probe=declared-cl")?;
    let resp = drain_resp(rx);

    assert_eq!(resp.content_length, Some(10));
    assert_eq!(resp.body_string(), "abc");
    assert!(resp.truncated, "the client must see the under-run");
    Ok(())
}

// ---- sendFile (stream-worker.php): plugin-streamed files

/// A dispatcher pool over stream-worker.php whose sendfile root is the scratch dir.
fn sendfile_server() -> Server {
    Spawn::http(Mode::Dispatcher, fixture("dispatcher/stream-worker.php"))
        .http_extra("[http.sendfile]\nroot = \".\"")
        .spawn()
}

/// Writes a payload into the sendfile root of `srv`.
fn sendfile_payload(srv: &Server) -> PathBuf {
    let path = srv.dir.join("payload");
    std::fs::write(&path, b"abcdefghijklmnopqrstuvwxyz").expect("write payload");
    path
}

fn with_path_header(query: &str, path: &Path) -> rapira_sapi::Request {
    let mut rq = req(query);
    rq.headers.append(
        "x-path",
        HeaderValue::try_from(path.to_string_lossy().into_owned()).unwrap(),
    );
    rq
}

/// A one-shot sendFile: the head carries a real content-length and the file bytes ride a File frame.
#[test]
fn sendfile_one_shot_carries_the_file_length() -> anyhow::Result<()> {
    let srv = sendfile_server();
    let path = sendfile_payload(&srv);
    let resp = drain_resp(submit(
        srv.addr,
        with_path_header("/?probe=sendfile", &path),
    )?);

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.content_length, Some(26));
    assert_eq!(resp.body, b"abcdefghijklmnopqrstuvwxyz");
    assert!(resp.ended && !resp.truncated);
    Ok(())
}

/// A range response is the handler's own 206 plus content-range with the slice passed as offset/length; sendFile applies no HTTP semantics.
#[test]
fn sendfile_slice_serves_the_named_bytes() -> anyhow::Result<()> {
    let srv = sendfile_server();
    let path = sendfile_payload(&srv);
    let resp = drain_resp(submit(
        srv.addr,
        with_path_header("/?probe=sendfile-slice", &path),
    )?);

    assert_eq!(resp.status(), 206);
    assert_eq!(resp.content_length, Some(3));
    assert_eq!(resp.body, b"cde");
    Ok(())
}

/// FileNotSendableException is raised before anything is written, so the handler can still answer 404.
#[test]
fn sendfile_missing_file_still_answers_404() -> anyhow::Result<()> {
    let srv = sendfile_server();
    let resp = drain_resp(submit(srv.addr, req("/?probe=sendfile-missing"))?);

    assert_eq!(resp.status(), 404);
    assert_eq!(resp.body_string(), "nope");
    Ok(())
}

/// A path outside the configured root is not sendable, symlinks resolved.
#[test]
fn sendfile_outside_the_root_is_denied() -> anyhow::Result<()> {
    let srv = sendfile_server();
    let resp = drain_resp(submit(
        srv.addr,
        with_path_header("/?probe=sendfile-escape", Path::new("/etc/hosts")),
    )?);

    assert_eq!(resp.status(), 403);
    assert_eq!(resp.body_string(), "denied");
    Ok(())
}

/// sendFile rejects a directory, an offset or a slice past the end of the file, and a symlink out of the root before anything is written, so the handler answers 403.
/// A symlink inside the root stays sendable, and a slice longer than one 64 KiB plugin read keeps its offset across the reads.
#[test]
fn sendfile_validates_the_path_and_the_slice() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        path: PathBuf,
        query: &'static str,
        status: u16,
        body: Vec<u8>,
    }
    let srv = sendfile_server();
    let payload = sendfile_payload(&srv);
    // A prime period, so a read at a wrong offset returns other bytes.
    let pattern: Vec<u8> = (0..100 * 1024).map(|i| (i % 251) as u8).collect();
    let patterned = srv.dir.join("patterned");
    std::fs::write(&patterned, &pattern)?;
    let link_in = srv.dir.join("link-in");
    std::os::unix::fs::symlink(&payload, &link_in)?;
    let link_out = srv.dir.join("link-out");
    std::os::unix::fs::symlink("/etc/hosts", &link_out)?;
    let cases = [
        Case {
            name: "directory",
            path: srv.dir.clone(),
            query: "",
            status: 403,
            body: b"denied".to_vec(),
        },
        Case {
            name: "offset past end",
            path: payload.clone(),
            query: "&offset=27",
            status: 403,
            body: b"denied".to_vec(),
        },
        Case {
            name: "slice past end",
            path: payload.clone(),
            query: "&offset=20&length=10",
            status: 403,
            body: b"denied".to_vec(),
        },
        Case {
            name: "escaping symlink",
            path: link_out,
            query: "",
            status: 403,
            body: b"denied".to_vec(),
        },
        Case {
            name: "intra-root symlink",
            path: link_in,
            query: "",
            status: 200,
            body: b"abcdefghijklmnopqrstuvwxyz".to_vec(),
        },
        Case {
            name: "slice over two plugin reads",
            path: patterned,
            query: "&offset=1024&length=81920",
            status: 200,
            body: pattern[1024..1024 + 80 * 1024].to_vec(),
        },
    ];

    for case in cases {
        let target = format!("/?probe=sendfile-range{}", case.query);
        let resp = drain_resp(submit(srv.addr, with_path_header(&target, &case.path))?);
        assert_eq!(resp.status(), case.status, "{}", case.name);
        assert!(
            resp.body == case.body,
            "{}: wrong body of {} byte(s)",
            case.name,
            resp.body.len()
        );
        if case.status == 200 {
            assert_eq!(
                resp.content_length,
                Some(case.body.len() as u64),
                "{}",
                case.name
            );
            assert!(resp.ended && !resp.truncated, "{}", case.name);
        }
    }
    Ok(())
}

/// A file that shrinks after sendFile() committed its slice ends the body with an error: the chunked body has no last chunk, and the plugin logs the cut. The bytes before the error are a prefix of the file up to the cut. The error gives hyper one flush pass, so a client that reads slowly can get fewer bytes than the file holds.
#[test]
fn sendfile_of_a_shrunken_file_ends_short() -> anyhow::Result<()> {
    const SIZE: u64 = 64 * 1024 * 1024;
    const CUT: u64 = 32 * 1024 * 1024;
    let srv = sendfile_server();
    let path = srv.dir.join("shrinking");
    // A sparse file: the size costs no memory.
    std::fs::File::create(&path)?.set_len(SIZE)?;
    let marker = srv.dir.join("shrinking.shrunk");

    let mut stream = TcpStream::connect(srv.addr)?;
    stream.set_read_timeout(Some(READ))?;
    write!(
        stream,
        "GET /?probe=sendfile-shrink&to={CUT} HTTP/1.1\r\n\
         host: localhost\r\n\
         x-path: {}\r\n\
         connection: close\r\n\
         \r\n",
        path.display()
    )?;
    // The test reads nothing before the cut, so the socket buffers stop the plugin read far below CUT.
    let deadline = std::time::Instant::now() + READ;
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "the file was not cut");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no response head")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200 "), "head: {head:?}");
    assert!(
        head.contains("\r\ntransfer-encoding: chunked"),
        "head: {head:?}"
    );
    let (body, last_chunk) = dechunk(&raw[split + 4..])?;
    assert!(
        body.len() as u64 <= CUT,
        "the body stops at the cut: {} bytes",
        body.len()
    );
    // The sparse file holds zeros only.
    assert!(
        body.iter().all(|&b| b == 0),
        "the body carries the file bytes"
    );
    assert!(!last_chunk, "a cut body must not end cleanly");
    assert!(
        wait_log_contains(&srv, "the file shrank mid-send", READ),
        "the plugin must log the cut"
    );
    Ok(())
}

// ---- writeTrailers (stream-worker.php): the third ending

/// `writeTrailers()` after streamed chunks ends the response cleanly. The plugin does not forward response trailers.
#[test]
fn trailers_end_a_streamed_response() -> anyhow::Result<()> {
    let (_srv, rx) = stream_probe("/?probe=trailers")?;
    let resp = drain_resp(rx);

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "chunk,");
    assert!(resp.ended && !resp.truncated);
    Ok(())
}

/// A trailers-only response keeps real length framing: content-length 0, not empty chunked.
#[test]
fn trailers_only_response_keeps_length_framing() -> anyhow::Result<()> {
    let (_srv, rx) = stream_probe("/?probe=trailers-only")?;
    let resp = drain_resp(rx);

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.content_length, Some(0));
    assert!(resp.body.is_empty());
    Ok(())
}

/// Trailers before any head throw a catchable HeadNotWrittenError and the unit still serves.
#[test]
fn trailers_before_a_head_throw_head_not_written() -> anyhow::Result<()> {
    let (srv, rx) = stream_probe("/?probe=trailers-no-head")?;
    let resp = drain_resp(rx);

    assert_eq!(resp.status(), 200, "the handler recovers with a body");
    assert_eq!(resp.body_string(), "caught");
    let ctx = server_log::wait_app_record(&srv.log_file(), "trailers-no-head");
    assert!(ctx.contains("HeadNotWrittenError"), "wrong class in {ctx}");
    Ok(())
}

/// A field from the forbidden categories raises `\ValueError` regardless of protocol; the handler recovers.
#[test]
fn forbidden_trailer_field_is_rejected() -> anyhow::Result<()> {
    let (_srv, rx) = stream_probe("/?probe=trailers-forbidden")?;
    let resp = drain_resp(rx);

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_string(), "rejected");
    Ok(())
}

/// Decodes the chunked body `raw`; returns the data and whether the last chunk arrived.
/// https://www.rfc-editor.org/rfc/rfc9112#section-7.1
fn dechunk(mut raw: &[u8]) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut data = Vec::new();
    while let Some(eol) = raw.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(std::str::from_utf8(&raw[..eol])?, 16)?;
        if size == 0 {
            return Ok((data, true));
        }
        raw = &raw[eol + 2..];
        data.extend_from_slice(&raw[..size.min(raw.len())]);
        if raw.len() < size + 2 {
            break;
        }
        raw = &raw[size + 2..];
    }
    Ok((data, false))
}
