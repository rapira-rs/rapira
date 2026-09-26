use std::net::SocketAddr;

use http::header::SET_COOKIE;
use rapira_sapi::Mode;
use tests::wire::submit_async;
use tests::{Resp, collect, fixture, req};

use crate::harness::Spawn;

/// One request on its own connection, collected to its end; a truncated body is an error.
async fn exchange(addr: SocketAddr, uri: &str) -> anyhow::Result<Resp> {
    collect(submit_async(addr, req(uri)).await?).await
}

fn check(res: &Resp, want: &str) -> anyhow::Result<()> {
    anyhow::ensure!(res.status() == 200, "expected 200, got {}", res.status());
    anyhow::ensure!(
        res.body == want.as_bytes(),
        "expected body {want:?}, got {:?}",
        String::from_utf8_lossy(&res.body)
    );
    Ok(())
}

/// Two concurrent requests; distinct bodies prove both ran.
#[tokio::test]
async fn classic_mode_serves_exchanges() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Classic, fixture("plugin_tests/driver-classic.php")).spawn();
    let (a, b) = tokio::join!(
        exchange(srv.addr, "/?from=a"),
        exchange(srv.addr, "/?from=b"),
    );
    check(&a?, "ok:a")?;
    check(&b?, "ok:b")
}

/// A buffered head-only error response must not be flagged truncated, or the plugin serves a generic 502 instead of the real 404.
async fn error_path(mode: Mode, script: &str) -> anyhow::Result<()> {
    let srv = Spawn::http(mode, fixture(script)).spawn();
    let resp = exchange(srv.addr, "/").await?;
    anyhow::ensure!(resp.status() == 404, "expected 404, got {}", resp.status());
    anyhow::ensure!(
        resp.header(SET_COOKIE.as_str()).is_some(),
        "the session Set-Cookie must survive the buffered error path"
    );
    Ok(())
}

#[tokio::test]
async fn buffered_error_response_arrives_whole_worker() -> anyhow::Result<()> {
    error_path(Mode::Worker, "shared/error-keeps-headers-worker.php").await
}

#[tokio::test]
async fn buffered_error_response_arrives_whole_classic() -> anyhow::Result<()> {
    error_path(Mode::Classic, "shared/error-keeps-headers.php").await
}

/// Output before the throw seals a truncated frame: the reply must end as truncated, not as a possibly-incomplete body.
#[tokio::test]
async fn truncated_response_ends_as_truncated_worker() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("shared/output-then-throw-worker.php")).spawn();
    let err = match exchange(srv.addr, "/").await {
        Ok(resp) => anyhow::bail!(
            "the reply must end as truncated, got {} with body {:?}",
            resp.status(),
            resp.body_string()
        ),
        Err(e) => e,
    };
    anyhow::ensure!(
        err.to_string().contains("truncated"),
        "expected the truncated-response error, got: {err:#}"
    );
    Ok(())
}
