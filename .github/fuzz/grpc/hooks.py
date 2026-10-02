"""Schemathesis hooks for the Connect JSON target.

rapira decodes the JSON request into a binary message, echo.php replies with the same bytes, and rapira encodes them as
JSON. So the reply is the request in the form that a proto3 JSON printer writes,
https://protobuf.dev/programming-guides/json/:
- A field without presence at its default value is not in the reply. A float or double -0 is not the default:
  https://protobuf.dev/programming-guides/proto3/#default
- A field with presence (a message, a oneof member, an optional field) is in the reply when the request sets it.
- A 64-bit integer is a decimal string. An enum is its name, or its number when the enum has no value with that number.

The check `connect_echo` compares the reply of a positive case with that form. A negative case breaks the spec on
purpose. For a negative case, the check accepts:
- 200 with a JSON object.
- 400 with the Connect code invalid_argument, https://connectrpc.com/docs/protocol/#error-codes
- 405 for a method other than POST, https://www.rfc-editor.org/rfc/rfc9110.html#section-15.5.6
- 415 for a body without the JSON content type, https://connectrpc.com/docs/protocol/#unary-request

An integer or enum field with a value such as 454.0 is a form that a parser can reject, so the check also accepts
invalid_argument for a positive case with one.

rapira parses some JSON numbers up to 2 units in the last place (ULP) off, at any digit count:
https://github.com/rapira-rs/rapira/issues/177
So the check accepts a float or double that is at most 2 doubles away from the sent value.
"""

import json
import math
import struct

import schemathesis

KINDS = {0: "KIND_UNSPECIFIED", 1: "KIND_ONE", 2: "KIND_TWO", -1: "KIND_NEGATIVE"}

# The JSON name and the printed type of each field: "int" is a JSON number, "int64" a decimal string. A list is a
# repeated field, and a tuple is a map value. The spec allows only the printed form of bytes, so bytes compare as a
# string.
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
    """The value as a printer writes it. A float or double keeps the sent number: `same` rounds it."""
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


def same(kind, want, got):
    """The path of the first difference between want and got, or None."""
    if isinstance(kind, (dict, tuple)):
        if not isinstance(got, dict) or got.keys() != want.keys():
            return ""
        for k, w in want.items():
            path = same(kind[k] if isinstance(kind, dict) else kind[0], w, got[k])
            if path is not None:
                return f".{k}{path}"
        return None
    if isinstance(kind, list):
        if not isinstance(got, list) or len(got) != len(want):
            return ""
        for i, (w, g) in enumerate(zip(want, got)):
            path = same(kind[0], w, g)
            if path is not None:
                return f"[{i}]{path}"
        return None
    if isinstance(want, float):
        if type(got) not in (int, float):
            return ""
        r = f32 if kind == "float" else float
        near = {math.nextafter(want, to, steps=n) for to in (-math.inf, math.inf) for n in range(3)}
        bits = {struct.pack("<d", r(x)) for x in near}
        return None if struct.pack("<d", r(got)) in bits else ""
    return None if type(got) is type(want) and got == want else ""


def short(value, n=600):
    r = repr(value)
    return r if len(r) <= n else r[:n] + "..."


@schemathesis.check
def connect_echo(ctx, response, case):
    request = response.request
    negative = case.meta.generation.mode.is_negative
    if negative and response.status_code == 405 and request.method != "POST":
        return
    if negative and response.status_code == 415 and request.headers.get("Content-Type") != "application/json":
        return
    sent = None if negative else json.loads(request.body)
    if response.status_code == 400 and (negative or float_int(SAMPLE, sent)):
        code = json.loads(response.content).get("code")
        assert code == "invalid_argument", f"400 with the code {code!r}: {short(response.content)}"
        return
    assert response.status_code == 200, f"expected 200, got {response.status_code}: {short(response.content)}"
    got = json.loads(response.content)
    if negative:
        assert isinstance(got, dict), f"not a JSON object: {short(response.content)}"
        return
    want = printed(SAMPLE, sent)
    path = same(SAMPLE, want, got)
    assert path is None, f"the reply differs at ${path}: want {short(want)}, got {short(got)}"
