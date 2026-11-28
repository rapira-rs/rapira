use rapira_config::Mode;
use tests::wire::submit;
use tests::{Resp, drain_resp, fixture, req};

use crate::harness::Spawn;

/// One worker serves every URI in order.
fn run(name: &str, uris: &[&str]) -> anyhow::Result<Vec<Resp>> {
    let srv = Spawn::http(Mode::Worker, fixture(name)).spawn();
    uris.iter()
        .map(|uri| Ok(drain_resp(submit(srv.addr, req(uri))?)))
        .collect()
}

/// A request that throws must answer 500, and the next plain request must answer 200 with the expected tokens, so this also covers the success path of the extension.
#[test]
fn extension_exceptions_recover_and_keep_serving() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        tokens: &'static [&'static str],
    }
    let cases = [
        Case {
            name: "zlib",
            fixture: "php_ext/zlib-worker.php",
            tokens: &["zlib:rapira zlib"],
        },
        Case {
            name: "curl",
            fixture: "php_ext/curl-worker.php",
            tokens: &["curl:"],
        },
        Case {
            name: "ctype",
            fixture: "php_ext/ctype-worker.php",
            tokens: &["ctype:1"],
        },
        Case {
            name: "mbstring",
            fixture: "php_ext/mbstring-worker.php",
            tokens: &["mb:HÉLLO"],
        },
        Case {
            name: "iconv",
            fixture: "php_ext/iconv-worker.php",
            tokens: &["iconv:iconv ok"],
        },
        Case {
            name: "openssl",
            fixture: "php_ext/openssl-worker.php",
            tokens: &["openssl:64"],
        },
        Case {
            name: "fileinfo",
            fixture: "php_ext/fileinfo-worker.php",
            tokens: &["finfo:text/plain"],
        },
        Case {
            name: "tokenizer",
            fixture: "php_ext/tokenizer-worker.php",
            tokens: &["tok:"],
        },
        Case {
            name: "phar",
            fixture: "php_ext/phar-worker.php",
            tokens: &["phar:"],
        },
        Case {
            name: "dom",
            fixture: "php_ext/dom-worker.php",
            tokens: &["dom:ok"],
        },
        Case {
            name: "simplexml",
            fixture: "php_ext/simplexml-worker.php",
            tokens: &["sxml:ok"],
        },
        Case {
            name: "xml",
            fixture: "php_ext/xml-worker.php",
            tokens: &["xml:1"],
        },
        Case {
            name: "xmlreader",
            fixture: "php_ext/xmlreader-worker.php",
            tokens: &["xr:a"],
        },
        Case {
            name: "xmlwriter",
            fixture: "php_ext/xmlwriter-worker.php",
            tokens: &["xw:<v>ok</v>"],
        },
        Case {
            name: "pdo_sqlite",
            fixture: "php_ext/pdo_sqlite-worker.php",
            tokens: &["pdo:ok"],
        },
        Case {
            name: "sqlite3",
            fixture: "php_ext/sqlite3-worker.php",
            tokens: &["sqlite:42"],
        },
        Case {
            name: "filter",
            fixture: "php_ext/filter-worker.php",
            tokens: &["filter:a@b.com"],
        },
        // The token is the unkeyed 32-byte BLAKE2b of "rapira" (RFC 7693), which is what crypto_generichash computes.
        Case {
            name: "sodium",
            fixture: "php_ext/sodium-worker.php",
            tokens: &["sodium:94dc0c93428ad3519ec5ec157a5c3c53b7b6056ba3fff7ad773a19e0b4f3fd5a"],
        },
        Case {
            name: "browscap",
            fixture: "php_ext/browscap-worker.php",
            tokens: &["browscap:false"],
        },
        Case {
            name: "imap",
            fixture: "php_ext/imap-worker.php",
            tokens: &["imap:ok"],
        },
    ];
    for case in cases {
        let out = run(case.fixture, &["/?boom=1", "/"])?;
        if out[0].body_string() == "skip" {
            continue;
        }
        assert_eq!(
            out[0].status(),
            500,
            "{}: uncaught throw must be 500 (got: {:?})",
            case.name,
            out[0]
        );
        assert_eq!(
            out[1].status(),
            200,
            "{}: must keep serving after throw (got: {:?})",
            case.name,
            out[1]
        );
        let body = out[1].body_string();
        for token in case.tokens {
            assert!(
                body.contains(token),
                "{}: follow-up must echo {token:?} (got: {body:?})",
                case.name
            );
        }
    }
    Ok(())
}

#[test]
fn extension_success_cases() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        fixture: &'static str,
        tokens: &'static [&'static str],
    }
    let cases = [
        Case {
            name: "bcmath",
            fixture: "php_ext/bcmath-worker.php",
            tokens: &["bcmath:0.30"],
        },
        Case {
            name: "intl",
            fixture: "php_ext/intl-worker.php",
            tokens: &["intl:café"],
        },
        Case {
            name: "pdo_pgsql",
            fixture: "php_ext/pdo_pgsql-worker.php",
            tokens: &["pdo_pgsql:ok"],
        },
        Case {
            name: "pgsql",
            fixture: "php_ext/pgsql-worker.php",
            tokens: &["pgsql:0001ff"],
        },
        Case {
            name: "igbinary",
            fixture: "php_ext/igbinary-worker.php",
            tokens: &["igbinary:ok"],
        },
        Case {
            name: "redis",
            fixture: "php_ext/redis-worker.php",
            tokens: &["redis:ok"],
        },
        // Checks that OPcache started under the rapira SAPI name. OPcache <= 8.4 starts only for the SAPI names on an allowlist. See build_sapi_module.
        Case {
            name: "opcache",
            fixture: "php_ext/opcache-worker.php",
            tokens: &["opcache:enabled"],
        },
        // The `browscap` ini is PHP_INI_SYSTEM and unset here, so get_browser() must warn and return false (browscap.c).
        Case {
            name: "browscap unset warns",
            fixture: "php_ext/browscap-worker.php",
            tokens: &[
                "browscap:false",
                "get_browser(): browscap ini directive not set",
            ],
        },
    ];
    for case in cases {
        let out = run(case.fixture, &["/"])?;
        if out[0].body_string() == "skip" {
            continue;
        }
        assert_eq!(
            out[0].status(),
            200,
            "{}: must serve 200 (got: {:?})",
            case.name,
            out[0]
        );
        let body = out[0].body_string();
        for token in case.tokens {
            assert!(
                body.contains(token),
                "{}: must echo {token:?} (got: {body:?})",
                case.name
            );
        }
    }
    Ok(())
}

/// ext/openssl has no RINIT or RSHUTDOWN (openssl.c), in php-fpm and in rapira alike, so an undrained error in the persistent ring stays after its request.
#[test]
fn openssl_error_ring_outlives_the_request() -> anyhow::Result<()> {
    // The first drain starts the test from an empty ring.
    let out = run(
        "php_ext/openssl-worker.php",
        &[
            "/?step=drain",
            "/?step=leak",
            "/?step=drain",
            "/?step=drain",
        ],
    )?;
    if out[0].body_string() == "skip" {
        return Ok(());
    }
    assert_eq!(
        (out[1].status(), out[1].body_string().as_str()),
        (200, "openssl:leaked"),
        "leak request must succeed (got: {:?})",
        out[1]
    );
    // The PEM reader leaves one PEM_R_NO_START_LINE entry. Only the routine and reason substrings are the same in OpenSSL 1.1.1 and 3.x.
    assert!(
        out[2].body_string().starts_with("openssl:drained:1:"),
        "the next request must drain exactly one error (got: {:?})",
        out[2].body_string()
    );
    assert!(
        out[2].body_string().contains("PEM routines")
            && out[2].body_string().contains("no start line"),
        "drained error must be the PEM no-start-line entry (got: {:?})",
        out[2].body_string()
    );
    assert_eq!(
        (out[3].status(), out[3].body_string().as_str()),
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
    if out[0].body_string() == "skip" {
        return Ok(());
    }
    assert_eq!(
        (out[1].status(), out[1].body_string().as_str()),
        (200, "openssl:leaked:20"),
        "the leak request must push 20 errors (got: {:?})",
        out[1]
    );
    assert!(
        out[2].body_string().starts_with("openssl:drained:15:"),
        "20 pushes wrap the 16-slot ring down to 15 readable entries (got: {:?})",
        out[2].body_string()
    );
    Ok(())
}
