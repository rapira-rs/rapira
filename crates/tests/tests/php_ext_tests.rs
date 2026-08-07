use php_sys::{Mode, Rapira};
use tests::{drain, fixture, php_lock, req};

// One resident worker per extension; `?boom=1` switches its handler to the throwing
// call. 1 thread => the follow-up request rides the same interpreter, proving an
// uncaught extension throw leaves the worker serving.
fn run(name: &str, uris: &[&str]) -> anyhow::Result<Vec<(u16, String)>> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::WorkerRequest(fixture(name)))?;
    let h = r.handle()?;
    let mut out = Vec::with_capacity(uris.len());
    for uri in uris {
        out.push(drain(h.handle_blocking(req(uri, name))?));
    }
    drop(h);
    r.shutdown();
    Ok(out)
}

fn success(name: &str, token: &str) -> anyhow::Result<()> {
    let out = run(name, &["/"])?;
    // fixtures echo "skip" when their extension is missing from this libphp build
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(out[0].0, 200, "{name} must serve 200 (got: {:?})", out[0]);
    assert!(
        out[0].1.contains(token),
        "{name} must echo {token:?} (got: {:?})",
        out[0].1
    );
    Ok(())
}

fn exception(name: &str, token: &str) -> anyhow::Result<()> {
    let out = run(name, &["/?boom=1", "/"])?;
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(
        out[0].0, 500,
        "{name} uncaught throw must be a 500 (got: {:?})",
        out[0]
    );
    assert_eq!(
        out[1].0, 200,
        "{name} must keep serving after the throw (got: {:?})",
        out[1]
    );
    assert!(
        out[1].1.contains(token),
        "{name} follow-up must echo {token:?} (got: {:?})",
        out[1].1
    );
    Ok(())
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn zlib_success() -> anyhow::Result<()> {
    success("php_ext/zlib-worker.php", "zlib:rapira zlib")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn zlib_exception() -> anyhow::Result<()> {
    exception("php_ext/zlib-worker.php", "zlib:rapira zlib")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn curl_success() -> anyhow::Result<()> {
    success("php_ext/curl-worker.php", "curl:")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn curl_exception() -> anyhow::Result<()> {
    exception("php_ext/curl-worker.php", "curl:")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn ctype_success() -> anyhow::Result<()> {
    success("php_ext/ctype-worker.php", "ctype:1")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn ctype_exception() -> anyhow::Result<()> {
    exception("php_ext/ctype-worker.php", "ctype:1")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn mbstring_success() -> anyhow::Result<()> {
    success("php_ext/mbstring-worker.php", "mb:HÉLLO")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn mbstring_exception() -> anyhow::Result<()> {
    exception("php_ext/mbstring-worker.php", "mb:HÉLLO")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn iconv_success() -> anyhow::Result<()> {
    success("php_ext/iconv-worker.php", "iconv:iconv ok")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn iconv_exception() -> anyhow::Result<()> {
    exception("php_ext/iconv-worker.php", "iconv:iconv ok")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn openssl_success() -> anyhow::Result<()> {
    success("php_ext/openssl-worker.php", "openssl:64")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn openssl_exception() -> anyhow::Result<()> {
    exception("php_ext/openssl-worker.php", "openssl:64")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn fileinfo_success() -> anyhow::Result<()> {
    success("php_ext/fileinfo-worker.php", "finfo:text/plain")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn fileinfo_exception() -> anyhow::Result<()> {
    exception("php_ext/fileinfo-worker.php", "finfo:text/plain")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn tokenizer_success() -> anyhow::Result<()> {
    success("php_ext/tokenizer-worker.php", "tok:")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn tokenizer_exception() -> anyhow::Result<()> {
    exception("php_ext/tokenizer-worker.php", "tok:")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn phar_success() -> anyhow::Result<()> {
    success("php_ext/phar-worker.php", "phar:")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn phar_exception() -> anyhow::Result<()> {
    exception("php_ext/phar-worker.php", "phar:")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn dom_success() -> anyhow::Result<()> {
    success("php_ext/dom-worker.php", "dom:ok")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn dom_exception() -> anyhow::Result<()> {
    exception("php_ext/dom-worker.php", "dom:ok")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn simplexml_success() -> anyhow::Result<()> {
    success("php_ext/simplexml-worker.php", "sxml:ok")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn simplexml_exception() -> anyhow::Result<()> {
    exception("php_ext/simplexml-worker.php", "sxml:ok")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn xml_success() -> anyhow::Result<()> {
    success("php_ext/xml-worker.php", "xml:1")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn xml_exception() -> anyhow::Result<()> {
    exception("php_ext/xml-worker.php", "xml:1")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn xmlreader_success() -> anyhow::Result<()> {
    success("php_ext/xmlreader-worker.php", "xr:a")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn xmlreader_exception() -> anyhow::Result<()> {
    exception("php_ext/xmlreader-worker.php", "xr:a")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn xmlwriter_success() -> anyhow::Result<()> {
    success("php_ext/xmlwriter-worker.php", "xw:<v>ok</v>")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn xmlwriter_exception() -> anyhow::Result<()> {
    exception("php_ext/xmlwriter-worker.php", "xw:<v>ok</v>")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn pdo_sqlite_success() -> anyhow::Result<()> {
    success("php_ext/pdo_sqlite-worker.php", "pdo:ok")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn pdo_sqlite_exception() -> anyhow::Result<()> {
    exception("php_ext/pdo_sqlite-worker.php", "pdo:ok")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn sqlite3_success() -> anyhow::Result<()> {
    success("php_ext/sqlite3-worker.php", "sqlite:42")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn sqlite3_exception() -> anyhow::Result<()> {
    exception("php_ext/sqlite3-worker.php", "sqlite:42")
}

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn filter_success() -> anyhow::Result<()> {
    success("php_ext/filter-worker.php", "filter:a@b.com")
}
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn filter_exception() -> anyhow::Result<()> {
    exception("php_ext/filter-worker.php", "filter:a@b.com")
}

// No `exception` counterpart: this guards that OPcache actually started under our SAPI
// name, which PHP <= 8.4 gates on an allowlist (see build_sapi_module). Nothing to throw.
#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn opcache_success() -> anyhow::Result<()> {
    success("php_ext/opcache-worker.php", "opcache:enabled")
}
