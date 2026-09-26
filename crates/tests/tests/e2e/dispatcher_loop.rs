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

use crate::harness::{Server, Spawn, fixture_path};

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

/// No body on a HEAD response or a 204: chunks are dropped at seal, the head stands.
/// https://www.rfc-editor.org/rfc/rfc9112#section-6.3
#[test]
fn head_and_204_drop_the_body() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Dispatcher, fixture("dispatcher/verbs-worker.php")).spawn();

    let mut head_rq = req("/");
    head_rq.method = "HEAD".into();
    let (status, body) = drain(submit(srv.addr, head_rq)?);
    assert_eq!(
        (status, body.as_str()),
        (200, ""),
        "HEAD keeps the GET head but drops the body"
    );

    let (status, body) = drain(submit(srv.addr, req("/?probe=head204"))?);
    assert_eq!((status, body.as_str()), (204, ""));
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
    // The builder writes a TCP `[http]` listener, so this `[http]` table goes in as a top-level table next to the gRPC pool.
    // The spawn waits for the gRPC listener, and the master binds the http listener before it.
    let srv = Spawn::grpc(fixture_path("grpc/echo-worker.php"))
        .toml(&format!(
            "[http]\nlisten = \"unix:{}\"\nserver_port = 8080\n\
             [http.pool]\nmode = \"dispatcher\"\nprocesses = 1\nentrypoint = \"{}\"",
            sock.display(),
            fixture("dispatcher/request-worker.php").display()
        ))
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
