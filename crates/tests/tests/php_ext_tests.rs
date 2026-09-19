use tests::run_worker;

fn run(name: &str, uris: &[&str]) -> anyhow::Result<Vec<(u16, String)>> {
    run_worker(name, uris, None)
}

fn success(name: &str, token: &str) -> anyhow::Result<()> {
    let out = run(name, &["/"])?;
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
fn zlib_success() -> anyhow::Result<()> {
    success("php_ext/zlib-worker.php", "zlib:rapira zlib")
}
#[test]
fn zlib_exception() -> anyhow::Result<()> {
    exception("php_ext/zlib-worker.php", "zlib:rapira zlib")
}

#[test]
fn curl_success() -> anyhow::Result<()> {
    success("php_ext/curl-worker.php", "curl:")
}
#[test]
fn curl_exception() -> anyhow::Result<()> {
    exception("php_ext/curl-worker.php", "curl:")
}

#[test]
fn ctype_success() -> anyhow::Result<()> {
    success("php_ext/ctype-worker.php", "ctype:1")
}
#[test]
fn ctype_exception() -> anyhow::Result<()> {
    exception("php_ext/ctype-worker.php", "ctype:1")
}

#[test]
fn mbstring_success() -> anyhow::Result<()> {
    success("php_ext/mbstring-worker.php", "mb:HÉLLO")
}
#[test]
fn mbstring_exception() -> anyhow::Result<()> {
    exception("php_ext/mbstring-worker.php", "mb:HÉLLO")
}

#[test]
fn iconv_success() -> anyhow::Result<()> {
    success("php_ext/iconv-worker.php", "iconv:iconv ok")
}
#[test]
fn iconv_exception() -> anyhow::Result<()> {
    exception("php_ext/iconv-worker.php", "iconv:iconv ok")
}

#[test]
fn openssl_success() -> anyhow::Result<()> {
    success("php_ext/openssl-worker.php", "openssl:64")
}
#[test]
fn openssl_exception() -> anyhow::Result<()> {
    exception("php_ext/openssl-worker.php", "openssl:64")
}

#[test]
fn fileinfo_success() -> anyhow::Result<()> {
    success("php_ext/fileinfo-worker.php", "finfo:text/plain")
}
#[test]
fn fileinfo_exception() -> anyhow::Result<()> {
    exception("php_ext/fileinfo-worker.php", "finfo:text/plain")
}

#[test]
fn tokenizer_success() -> anyhow::Result<()> {
    success("php_ext/tokenizer-worker.php", "tok:")
}
#[test]
fn tokenizer_exception() -> anyhow::Result<()> {
    exception("php_ext/tokenizer-worker.php", "tok:")
}

#[test]
fn phar_success() -> anyhow::Result<()> {
    success("php_ext/phar-worker.php", "phar:")
}
#[test]
fn phar_exception() -> anyhow::Result<()> {
    exception("php_ext/phar-worker.php", "phar:")
}

#[test]
fn dom_success() -> anyhow::Result<()> {
    success("php_ext/dom-worker.php", "dom:ok")
}
#[test]
fn dom_exception() -> anyhow::Result<()> {
    exception("php_ext/dom-worker.php", "dom:ok")
}

#[test]
fn simplexml_success() -> anyhow::Result<()> {
    success("php_ext/simplexml-worker.php", "sxml:ok")
}
#[test]
fn simplexml_exception() -> anyhow::Result<()> {
    exception("php_ext/simplexml-worker.php", "sxml:ok")
}

#[test]
fn xml_success() -> anyhow::Result<()> {
    success("php_ext/xml-worker.php", "xml:1")
}
#[test]
fn xml_exception() -> anyhow::Result<()> {
    exception("php_ext/xml-worker.php", "xml:1")
}

#[test]
fn xmlreader_success() -> anyhow::Result<()> {
    success("php_ext/xmlreader-worker.php", "xr:a")
}
#[test]
fn xmlreader_exception() -> anyhow::Result<()> {
    exception("php_ext/xmlreader-worker.php", "xr:a")
}

#[test]
fn xmlwriter_success() -> anyhow::Result<()> {
    success("php_ext/xmlwriter-worker.php", "xw:<v>ok</v>")
}
#[test]
fn xmlwriter_exception() -> anyhow::Result<()> {
    exception("php_ext/xmlwriter-worker.php", "xw:<v>ok</v>")
}

#[test]
fn pdo_sqlite_success() -> anyhow::Result<()> {
    success("php_ext/pdo_sqlite-worker.php", "pdo:ok")
}
#[test]
fn pdo_sqlite_exception() -> anyhow::Result<()> {
    exception("php_ext/pdo_sqlite-worker.php", "pdo:ok")
}

#[test]
fn sqlite3_success() -> anyhow::Result<()> {
    success("php_ext/sqlite3-worker.php", "sqlite:42")
}
#[test]
fn sqlite3_exception() -> anyhow::Result<()> {
    exception("php_ext/sqlite3-worker.php", "sqlite:42")
}

#[test]
fn filter_success() -> anyhow::Result<()> {
    success("php_ext/filter-worker.php", "filter:a@b.com")
}
#[test]
fn filter_exception() -> anyhow::Result<()> {
    exception("php_ext/filter-worker.php", "filter:a@b.com")
}

#[test]
fn bcmath_success() -> anyhow::Result<()> {
    success("php_ext/bcmath-worker.php", "bcmath:0.30")
}

#[test]
fn intl_success() -> anyhow::Result<()> {
    success("php_ext/intl-worker.php", "intl:café")
}

#[test]
fn pdo_pgsql_success() -> anyhow::Result<()> {
    success("php_ext/pdo_pgsql-worker.php", "pdo_pgsql:ok")
}

#[test]
fn pgsql_success() -> anyhow::Result<()> {
    success("php_ext/pgsql-worker.php", "pgsql:0001ff")
}

#[test]
fn igbinary_success() -> anyhow::Result<()> {
    success("php_ext/igbinary-worker.php", "igbinary:ok")
}

#[test]
fn redis_success() -> anyhow::Result<()> {
    success("php_ext/redis-worker.php", "redis:ok")
}

/// Pins that OPcache started under our SAPI name, which PHP <= 8.4 gates on an allowlist (see build_sapi_module).
#[test]
fn opcache_success() -> anyhow::Result<()> {
    success("php_ext/opcache-worker.php", "opcache:enabled")
}

/// ext/openssl has no RINIT or RSHUTDOWN (openssl.c), in php-fpm and in rapira alike, so an undrained error in the persistent ring outlives its request.
#[test]
fn openssl_error_ring_outlives_the_request() -> anyhow::Result<()> {
    // the first drain empties whatever earlier tests left on the process-global ring
    let out = run(
        "php_ext/openssl-worker.php",
        &[
            "/?step=drain",
            "/?step=leak",
            "/?step=drain",
            "/?step=drain",
        ],
    )?;
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(
        (out[1].0, out[1].1.as_str()),
        (200, "openssl:leaked"),
        "leak request must succeed (got: {:?})",
        out[1]
    );
    // the PEM reader leaves one PEM_R_NO_START_LINE entry; only the routine and reason substrings are stable across OpenSSL 1.1.1 and 3.x
    assert!(
        out[2].1.starts_with("openssl:drained:1:"),
        "the next request must drain exactly one error (got: {:?})",
        out[2].1
    );
    assert!(
        out[2].1.contains("PEM routines") && out[2].1.contains("no start line"),
        "drained error must be the PEM no-start-line entry (got: {:?})",
        out[2].1
    );
    assert_eq!(
        (out[3].0, out[3].1.as_str()),
        (200, "openssl:drained:0:"),
        "the ring is FIFO-drained, so the last request must find it empty (got: {:?})",
        out[3]
    );
    Ok(())
}

/// The 16-slot ring overwrites the oldest entry when full, so at most the newest 15 errors are readable (php_openssl_store_errors, openssl.c).
#[test]
fn openssl_error_ring_overwrites_the_oldest() -> anyhow::Result<()> {
    let out = run(
        "php_ext/openssl-worker.php",
        &["/?step=drain", "/?step=leak_many", "/?step=drain"],
    )?;
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(
        (out[1].0, out[1].1.as_str()),
        (200, "openssl:leaked:20"),
        "the leak request must push 20 errors (got: {:?})",
        out[1]
    );
    assert!(
        out[2].1.starts_with("openssl:drained:15:"),
        "20 pushes wrap the 16-slot ring down to 15 readable entries (got: {:?})",
        out[2].1
    );
    Ok(())
}

#[test]
fn browscap_unset_exception() -> anyhow::Result<()> {
    exception("php_ext/browscap-worker.php", "browscap:false")
}
/// The `browscap` ini is PHP_INI_SYSTEM and unset here: get_browser() must warn and return false (browscap.c).
#[test]
fn browscap_unset_warns() -> anyhow::Result<()> {
    let out = run("php_ext/browscap-worker.php", &["/"])?;
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(out[0].0, 200, "must serve 200 (got: {:?})", out[0]);
    assert!(
        out[0]
            .1
            .contains("get_browser(): browscap ini directive not set"),
        "the E_WARNING must be visible in the body (got: {:?})",
        out[0].1
    );
    assert!(
        out[0].1.contains("browscap:false"),
        "get_browser() must return false (got: {:?})",
        out[0].1
    );
    Ok(())
}

#[test]
fn imap_success() -> anyhow::Result<()> {
    success("php_ext/imap-worker.php", "imap:ok")
}
#[test]
fn imap_exception() -> anyhow::Result<()> {
    exception("php_ext/imap-worker.php", "imap:ok")
}
