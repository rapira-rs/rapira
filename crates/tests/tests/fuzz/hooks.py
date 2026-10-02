"""Schemathesis checks of the fuzz session. Each check compares the reply of an echo app with the request that
Schemathesis sent. run.sh selects the check of each target with --checks.
"""

import base64
import json
import math
import re
import struct
from urllib.parse import urlsplit

import schemathesis


def short(value, n=600):
    r = repr(value)
    return r if len(r) <= n else r[:n] + "..."


# The HTTP target. http/echo.php returns the Request as JSON, every byte string in base64. The expected values come
# from the PHP contract (https://github.com/rapira-rs/contract, src/Http) and from RFC 9110 and RFC 9112.
#
# urllib3 encodes a multipart body: one part per item, `Content-Disposition: form-data; name="N"[; filename="F"]`, an
# optional `Content-Type`, then the data. In the parameter values it percent-encodes only LF, CR and `"`.

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
    """The method, target, URI, header fields, raw body, and each multipart field and file in document order."""
    request = response.request
    body = request.body or b""
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
    assert response.status_code == 200, f"expected 200, got {response.status_code}"

    echo = json.loads(response.content)
    d = base64.b64decode

    # Request::$method and $target: byte-for-byte as received
    assert d(echo["method"]) == to_bytes(request.method), "method differs"
    target = to_bytes(request.path_url)
    assert d(echo["target"]) == target, f"target differs: sent {short(target)}, echo {short(d(echo['target']))}"
    # Request::$uri: the scheme of the listener and the Host field before the origin-form target (RFC 9112 section
    # 3.3). http.client sends the netloc of the URL as the Host field.
    host = urlsplit(request.url).netloc.encode()
    uri = b"http://" + host + target
    assert d(echo["uri"]) == uri, f"uri differs: want {short(uri)}, echo {short(d(echo['uri']))}"

    # Request::$headers: every field as received, Host included. The contract keeps the case of an HTTP/1.1 field
    # name, but rapira lowercases it: https://github.com/rapira-rs/rapira/issues/180
    sent = [(to_bytes(n), to_bytes(v)) for n, v in request.headers.items()] + [(b"Host", host)]
    want = field_set((name.lower(), value) for name, value in sent)
    got = echo_field_set(echo["headers"])
    assert got == want, f"request headers differ: sent {short(want)}, echo {short(got)}"

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


# The Connect JSON target. rapira decodes the JSON request into a binary message, grpc/echo.php replies with the same
# bytes, and rapira encodes them as JSON. So the reply is the request in the form that a proto3 JSON printer writes,
# https://protobuf.dev/programming-guides/json/:
# - A field without presence at its default value is not in the reply. A float or double -0 is not the default:
#   https://protobuf.dev/programming-guides/proto3/#default
# - A field with presence (a message, a oneof member, an optional field) is in the reply when the request sets it.
# - A 64-bit integer is a decimal string. An enum is its name, or its number when the enum has no value with that
#   number.

KINDS = {0: "KIND_UNSPECIFIED", 1: "KIND_ONE", 2: "KIND_TWO", -1: "KIND_NEGATIVE"}

# The JSON name and the printed type of each field of grpc/sample.proto: "int" is a JSON number, "int64" a decimal
# string. A list is a repeated field, and a tuple is a map value. The spec allows only the printed form of bytes, so
# bytes compare as a string.
INNER = {"id": "int", "label": "string"}
SAMPLE = {
    "int32Value": "int",
    "int64Value": "int64",
    "uint32Value": "int",
    "uint64Value": "int64",
    "sint32Value": "int",
    "sint64Value": "int64",
    "fixed32Value": "int",
    "fixed64Value": "int64",
    "sfixed32Value": "int",
    "sfixed64Value": "int64",
    "floatValue": "float",
    "doubleValue": "double",
    "boolValue": "bool",
    "stringValue": "string",
    "bytesValue": "string",
    "kind": "enum",
    "inner": INNER,
    "repeatedInt32": ["int"],
    "repeatedString": ["string"],
    "repeatedKind": ["enum"],
    "repeatedInner": [INNER],
    "stringToInt64": ("int64",),
    "int32ToString": ("string",),
    "choiceText": "string",
    "choiceInner": INNER,
    "choiceNumber": "int64",
    "optionalInt32": "int",
}
# The scalar fields with presence. A message field has presence too.
PRESENCE = {"choiceText", "choiceNumber", "optionalInt32"}
# The printed default value of each scalar type.
DEFAULTS = {"int": 0, "int64": "0", "bool": False, "string": "", "enum": "KIND_UNSPECIFIED"}


def f32(x):
    return struct.unpack("<f", struct.pack("<f", x))[0]


def printed(kind, value):
    """The value as a printer writes it. A float or double keeps the sent number: `near` rounds it."""
    if isinstance(kind, dict):
        out = {}
        for name, v in value.items():
            v = printed(kind[name], v)
            if name in PRESENCE or isinstance(kind[name], dict) or not is_default(kind[name], v):
                out[name] = v
        return out
    if isinstance(kind, list):
        return [printed(kind[0], v) for v in value]
    if isinstance(kind, tuple):
        return {k: printed(kind[0], v) for k, v in value.items()}
    if kind in ("int", "int64", "enum") and isinstance(value, float):
        value = int(value)
    if kind == "int64":
        return str(value)
    if kind == "enum" and isinstance(value, int):
        return KINDS.get(value, value)
    if kind in ("float", "double") and not isinstance(value, str):
        return float(value)
    return value


def float_int(kind, value):
    """Whether an integer or enum field holds a JSON number with a fraction part or an exponent, such as 454.0."""
    if isinstance(kind, dict):
        return any(float_int(kind[k], v) for k, v in value.items())
    if isinstance(kind, list):
        return any(float_int(kind[0], v) for v in value)
    if isinstance(kind, tuple):
        return any(float_int(kind[0], v) for v in value.values())
    return kind in ("int", "int64", "enum") and isinstance(value, float)


def is_default(kind, value):
    if isinstance(kind, (list, tuple)):
        return not value
    if kind in ("float", "double"):
        if isinstance(value, str):
            return False
        value = f32(value) if kind == "float" else value
        return value == 0 and math.copysign(1, value) > 0
    return value == DEFAULTS[kind]


def near(kind, want, got):
    """Whether got is at most 2 doubles away from want. rapira parses some JSON numbers up to 2 units in the last
    place (ULP) off: https://github.com/rapira-rs/rapira/issues/177
    """
    if isinstance(want, str):
        return got == want
    if type(got) not in (int, float):
        return False
    r = f32 if kind == "float" else float
    steps = {math.nextafter(want, to, steps=n) for to in (-math.inf, math.inf) for n in range(3)}
    return struct.pack("<d", r(got)) in {struct.pack("<d", r(x)) for x in steps}


@schemathesis.check
def connect_echo(ctx, response, case):
    """A positive case gets its request back in the printed form. A negative case breaks the spec on purpose."""
    request = response.request
    negative = case.meta.generation.mode.is_negative
    # https://connectrpc.com/docs/protocol/#unary-request
    if negative and response.status_code == 405 and request.method != "POST":
        return
    if negative and response.status_code == 415 and request.headers.get("Content-Type") != "application/json":
        return
    sent = None if negative else json.loads(request.body)
    # An integer or enum field with a value such as 454.0 is a form that a parser can reject.
    if response.status_code == 400 and (negative or float_int(SAMPLE, sent)):
        code = json.loads(response.content).get("code")
        # https://connectrpc.com/docs/protocol/#error-codes
        assert code == "invalid_argument", f"400 with the code {code!r}"
        return
    assert response.status_code == 200, f"expected 200, got {response.status_code}"
    got = json.loads(response.content)
    assert isinstance(got, dict), "the reply is not a JSON object"
    if negative:
        return
    want = printed(SAMPLE, sent)
    for name, kind in (("floatValue", "float"), ("doubleValue", "double")):
        w, g = want.pop(name, None), got.pop(name, None)
        assert (w is None) == (g is None) and (w is None or near(kind, w, g)), f"{name}: want {w!r}, got {g!r}"
    assert json.dumps(got, sort_keys=True) == json.dumps(want, sort_keys=True), (
        f"the reply differs: want {short(want)}, got {short(got)}"
    )
