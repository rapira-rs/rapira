use std::fmt::Write as _;

use hyper::StatusCode;

/// `unready` holds the name of each pool that has no worker to serve a unit, in pool order.
pub(crate) fn readyz(unready: &[&str]) -> (StatusCode, String) {
    if unready.is_empty() {
        return (StatusCode::OK, "ok\n".to_owned());
    }
    let mut body = String::new();
    for pool in unready {
        let _ = writeln!(body, "pool {pool}: no ready worker");
    }
    (StatusCode::SERVICE_UNAVAILABLE, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readyz_names_each_pool_without_a_ready_worker() {
        struct Case {
            name: &'static str,
            unready: &'static [&'static str],
            want: (StatusCode, &'static str),
        }
        let cases = [
            Case {
                name: "each pool has a ready worker",
                unready: &[],
                want: (StatusCode::OK, "ok\n"),
            },
            Case {
                name: "one pool has no ready worker",
                unready: &["grpc"],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool grpc: no ready worker\n",
                ),
            },
            Case {
                name: "neither pool has a ready worker",
                unready: &["http", "grpc"],
                want: (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "pool http: no ready worker\npool grpc: no ready worker\n",
                ),
            },
        ];
        for case in cases {
            let (status, body) = readyz(case.unready);
            assert_eq!((status, body.as_str()), case.want, "{}", case.name);
        }
    }
}
