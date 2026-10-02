"""Schemathesis hooks for the HTTP target.

The check `rapira_echo` compares the echo of echo.php with the request that Schemathesis sent (`response.request`, a
requests.PreparedRequest): the method, target, authority, URI, protocol, header fields, the raw body, and each
multipart field and file in document order. The expected values come from the PHP contract
(https://github.com/rapira-rs/contract, src/Http) and from RFC 9110 and RFC 9112.

Known exclusions:
- A backslash in a part name or filename: rapira reads it as a quoted-pair,
  https://github.com/rapira-rs/rapira/issues/165
- A control byte in a part name or filename: rapira answers 400, https://github.com/rapira-rs/rapira/issues/181
- The case of a header field name: rapira lowercases it on HTTP/1.1, https://github.com/rapira-rs/rapira/issues/180

A negative case breaks the spec on purpose. The check accepts a 400 for a multipart body with an empty part name, a
control byte or a backslash in a part name or filename, or a body outside the urllib3 shape. It accepts a 413 for more
files than the default max_files.

urllib3 encodes a multipart body: one part per item, `Content-Disposition: form-data; name="N"[; filename="F"]`, an
optional `Content-Type`, then the data. In the parameter values it percent-encodes only LF, CR and `"`.
"""

import base64
import json
import re
from urllib.parse import urlsplit

import schemathesis

CD_RE = re.compile(rb'form-data; name="([^"]*)"(?:; filename="([^"]*)")?')
# rapira answers 400 for a control byte other than HTAB in a part name or filename:
# https://github.com/rapira-rs/rapira/issues/181
CTL_RE = re.compile(rb"[\x00-\x08\x0a-\x1f\x7f]")
# The default [http.uploads] max_files
MAX_FILES = 20


def to_bytes(value):
    if isinstance(value, bytes):
        return value
    # http.client sends a str header value as latin-1
    return value.encode("latin-1")


def ows_trim(value):
    # RFC 9110 section 5.5: a field value has no leading or trailing whitespace
    return value.strip(b" \t")


def field_set(pairs):
    """A sent header section as a sorted list of (name, value)."""
    return sorted((name, ows_trim(value)) for name, value in pairs)


def echo_field_set(entries):
    d = base64.b64decode
    return sorted((d(name), d(value)) for name, values in entries for value in values)


def short(value, n=200):
    r = repr(value)
    return r if len(r) <= n else r[:n] + "..."


def media_type(ctype):
    return ows_trim(ctype.split(b";", 1)[0]).lower()


def boundary_of(ctype):
    m = re.search(rb";\s*boundary=([^;\s]+)", ctype, re.IGNORECASE)
    return m.group(1) if m else None


def sent_parts(body, boundary):
    """(name, filename, headers, data) of each part of a body in the urllib3 shape, or None for another shape."""
    if boundary is None:
        return None
    delim = b"--" + boundary
    if body == delim + b"--\r\n":
        return []
    head = delim + b"\r\n"
    tail = b"\r\n" + delim + b"--\r\n"
    if not body.startswith(head) or not body.endswith(tail):
        return None
    parts = []
    for chunk in body[len(head) : -len(tail)].split(b"\r\n" + delim + b"\r\n"):
        raw_head, sep, data = chunk.partition(b"\r\n\r\n")
        if not sep:
            return None
        headers = []
        for line in raw_head.split(b"\r\n"):
            name, sep, value = line.partition(b":")
            if not sep:
                return None
            headers.append((name, value))
        cds = [ows_trim(v) for n, v in headers if n.lower() == b"content-disposition"]
        m = CD_RE.fullmatch(cds[0]) if len(cds) == 1 else None
        if m is None:
            return None
        parts.append((m.group(1), m.group(2), headers, data))
    return parts


@schemathesis.check
def rapira_echo(ctx, response, case):
    request = response.request
    body = request.body or b""
    if isinstance(body, str):
        # urllib3 sends a str body as UTF-8
        body = body.encode("utf-8")
    ctype = to_bytes(request.headers.get("Content-Type", ""))
    # Request::$body: a non-empty multipart/form-data body arrives parsed, any other body raw
    multipart = media_type(ctype) == b"multipart/form-data" and body != b""
    parts = sent_parts(body, boundary_of(ctype)) if multipart else None
    names = [x for p in parts or [] for x in p[:2] if x is not None]
    # A body outside the urllib3 shape has no parser here to compare with. rapira reads a backslash in a part name or
    # filename as a quoted-pair: https://github.com/rapira-rs/rapira/issues/165
    unchecked = multipart and (parts is None or any(b"\\" in x for x in names))
    # A negative case breaks the spec on purpose. rapira answers 400 for an empty part name or a CTL_RE byte, and 413
    # for more files than MAX_FILES.
    malformed = any(p[0] == b"" for p in parts or []) or any(CTL_RE.search(x) for x in names)
    if response.status_code == 400 and (unchecked or malformed):
        return
    if response.status_code == 413 and sum(p[1] is not None for p in parts or []) > MAX_FILES:
        return
    assert response.status_code == 200, f"expected 200, got {response.status_code}: {short(response.content)}"

    echo = json.loads(response.content)
    d = base64.b64decode

    # Request::$method and $target: byte-for-byte as received
    assert d(echo["method"]) == to_bytes(request.method), "method differs"
    target = to_bytes(request.path_url)
    assert d(echo["target"]) == target, f"target differs: sent {short(target)}, echo {short(d(echo['target']))}"
    # Request::$authority: the Host field byte-for-byte; http.client sends the netloc of the URL
    host = urlsplit(request.url).netloc.encode()
    assert echo["authority"] is not None and d(echo["authority"]) == host, "authority differs"
    # Request::$uri: the scheme of the listener and the authority before the origin-form target (RFC 9112 section 3.3)
    uri = b"http://" + host + target
    assert d(echo["uri"]) == uri, f"uri differs: want {short(uri)}, echo {short(d(echo['uri']))}"
    assert echo["protocol"] == "HTTP/1.1", "protocol differs"

    # Request::$headers: every field as received, Host included
    sent = [(to_bytes(n), to_bytes(v)) for n, v in request.headers.items()] + [(b"Host", host)]
    # requests.adapters.HTTPAdapter.send sends a body without Content-Length chunked, and urllib3 adds the field
    if request.body is not None and "Content-Length" not in request.headers:
        sent.append((b"Transfer-Encoding", b"chunked"))
    # The contract keeps the case of an HTTP/1.1 field name, but rapira lowercases it:
    # https://github.com/rapira-rs/rapira/issues/180
    want = field_set((name.lower(), value) for name, value in sent)
    got = echo_field_set(echo["headers"])
    assert got == want, f"request headers differ: sent {short(want, 600)}, echo {short(got, 600)}"

    if not multipart:
        assert echo["kind"] == "raw", f"body kind {echo['kind']}, want raw"
        assert d(echo["body"]) == body, f"raw body differs: sent {short(body)}, echo {short(d(echo['body']))}"
        return

    assert echo["kind"] == "multipart", f"body kind {echo['kind']}, want multipart"
    if unchecked:
        return
    # Multipart: fields are the parts without a filename parameter, files the parts with one, each in document order
    fields = [p for p in parts if p[1] is None]
    files = [p for p in parts if p[1] is not None]
    assert (len(echo["fields"]), len(echo["files"])) == (len(fields), len(files)), (
        f"part counts differ: sent {len(fields)}/{len(files)}, echo {len(echo['fields'])}/{len(echo['files'])}"
    )
    for i, ((name, _, headers, data), e) in enumerate(zip(fields, echo["fields"])):
        assert d(e["name"]) == name, f"field {i} name: sent {short(name)}, echo {short(d(e['name']))}"
        assert d(e["value"]) == data, f"field {i} value: sent {short(data)}, echo {short(d(e['value']))}"
        want, got = field_set(headers), echo_field_set(e["headers"])
        assert got == want, f"field {i} headers: sent {short(want)}, echo {short(got)}"
    for i, ((name, filename, headers, data), e) in enumerate(zip(files, echo["files"])):
        assert d(e["name"]) == name, f"file {i} name: sent {short(name)}, echo {short(d(e['name']))}"
        got = d(e["filename"])
        assert got == filename, f"file {i} filename: sent {short(filename)}, echo {short(got)}"
        # UploadedFile::$clientMediaType: the Content-Type value byte-for-byte, null without the field
        ctypes = [ows_trim(v) for n, v in headers if n.lower() == b"content-type"]
        want = ctypes[0] if ctypes else None
        got = None if e["type"] is None else d(e["type"])
        assert got == want, f"file {i} type: sent {short(want)}, echo {short(got)}"
        assert e["size"] == len(data), f"file {i} size: sent {len(data)}, echo {e['size']}"
        assert d(e["content"]) == data, f"file {i} content differs ({len(data)} bytes sent)"
        want, got = field_set(headers), echo_field_set(e["headers"])
        assert got == want, f"file {i} headers: sent {short(want)}, echo {short(got)}"
