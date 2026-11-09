//! Request headers through the unsafe field name drop to the `$_SERVER` variables of the classic and worker modes. The input is one `name:value` field per line, and its length is the content length.

#![no_main]

use http::header::COOKIE;
use rapira_fuzz::safe_name;
use rapira_http::check::drop_unsafe_field_names;
use rapira_sapi::callbacks::cgi_header_vars;

/// Fields that do not combine into a list, so the first line gives the value: https://www.rfc-editor.org/rfc/rfc9110#section-5.3
const SINGLETONS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "content-type",
    "content-length",
    "referer",
    "from",
];

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let sent = rapira_fuzz::header_map(data);
    let mut headers = sent.clone();
    drop_unsafe_field_names(&mut headers);

    let mut want = vec![("CONTENT_LENGTH".to_owned(), data.len().to_string())];
    for name in sent.keys().filter(|n| safe_name(n)) {
        let var = format!(
            "HTTP_{}",
            name.as_str().to_ascii_uppercase().replace('-', "_")
        );
        let values: Vec<&[u8]> = sent.get_all(name).iter().map(|v| v.as_bytes()).collect();
        let value = if SINGLETONS.contains(&name.as_str()) {
            values[0].to_vec()
        } else if name == COOKIE {
            // https://www.rfc-editor.org/rfc/rfc6265#section-4.2.1
            values.join(&b"; "[..])
        } else {
            values.join(&b", "[..])
        };
        want.push((var, value.escape_ascii().to_string()));
    }
    let mut got = Vec::new();
    cgi_header_vars(&headers, data.len(), |name, value| {
        got.push((
            name.to_string_lossy().into_owned(),
            value.escape_ascii().to_string(),
        ));
    });
    // The order does not matter, because the names are distinct.
    got.sort();
    want.sort();
    assert_eq!(got, want);
});
