//! The public conformance check: does a JSON payload match the pinned Stripe schema?

use std::fmt;

use serde_json::Value;

use super::{Error, SCHEMA_OBJECTS, V2Error, V2List, check, check_by_object, schema_fields};

/// Why a payload does not match the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceError {
    /// The `object` value of the payload (or `error`) that failed.
    pub object: String,
    /// Where in the payload, such as `data[0].invoice_settings.bogus`; empty for the root.
    pub path: String,
    pub message: String,
}

impl fmt::Display for ConformanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "."
        } else {
            &self.path
        };
        write!(f, "{} at {path}: {}", self.object, self.message)
    }
}

impl std::error::Error for ConformanceError {}

fn fail(object: &str, path: &str, message: impl Into<String>) -> ConformanceError {
    ConformanceError {
        object: object.to_string(),
        path: path.to_string(),
        message: message.into(),
    }
}

fn join(base: &str, rest: &str) -> String {
    match (base.is_empty(), rest.is_empty()) {
        (_, true) => base.to_string(),
        (true, false) => rest.to_string(),
        (false, false) if rest.starts_with('[') => format!("{base}{rest}"),
        (false, false) => format!("{base}.{rest}"),
    }
}

/// Turns the generated `"schema: path: message"` text into a [`ConformanceError`]. A denied
/// unknown field is reported at its parent's path, so its name is appended to the path.
fn parse(object: &str, base: &str, text: &str) -> ConformanceError {
    let mut parts = text.splitn(3, ": ");
    let (schema, path, message) = (parts.next(), parts.next(), parts.next());
    let (path, message) = match (schema, path, message) {
        (Some(_), Some(p), Some(m)) => (if p == "." { "" } else { p }, m),
        _ => ("", text),
    };
    let mut path = join(base, path);
    if let Some(rest) = message.strip_prefix("unknown field `")
        && let Some(name) = rest.split('`').next()
    {
        path = join(&path, name);
    }
    fail(object, &path, message)
}

fn check_object(object: &str, base: &str, v: &Value) -> Result<(), ConformanceError> {
    match check_by_object(object, v) {
        Some(Ok(())) => Ok(()),
        Some(Err(text)) => Err(parse(object, base, &text)),
        None => Err(fail(
            object,
            base,
            format!("unknown object `{object}`: no such schema"),
        )),
    }
}

fn check_items(v: &Value, base: &str) -> Result<(), ConformanceError> {
    let Some(items) = v.get("data").and_then(Value::as_array) else {
        return Err(fail("list", &join(base, "data"), "missing `data` array"));
    };
    for (i, item) in items.iter().enumerate() {
        check_at(item, &join(base, &format!("data[{i}]")))?;
    }
    Ok(())
}

fn check_event(v: &Value, base: &str) -> Result<(), ConformanceError> {
    check_object("event", base, v)?;
    let data_path = join(base, "data");
    let Some(obj) = v.pointer("/data/object") else {
        return Err(fail("event", &data_path, "missing `data.object`"));
    };
    check_at(obj, &join(&data_path, "object"))?;
    if let Some(prev) = v
        .pointer("/data/previous_attributes")
        .and_then(Value::as_object)
    {
        let object = obj.get("object").and_then(Value::as_str).unwrap_or("");
        let schema = SCHEMA_OBJECTS
            .iter()
            .find(|(o, s)| *o == object && !s.starts_with("deleted_"))
            .map(|(_, s)| *s);
        let fields = schema.and_then(schema_fields).unwrap_or(&[]);
        for key in prev.keys() {
            if !fields.contains(&key.as_str()) {
                return Err(fail(
                    "event",
                    &join(&join(&data_path, "previous_attributes"), key),
                    format!("`{key}` is not a field of `{object}`"),
                ));
            }
        }
    }
    Ok(())
}

/// The v1 schema's error `type` values; any other `type` (or none) marks a v2 error.
const V1_ERROR_TYPES: [&str; 4] = [
    "api_error",
    "card_error",
    "idempotency_error",
    "invalid_request_error",
];

/// An error envelope passes as the v1 `error` or the v2 `v2.error`. When both fail, the
/// error reported is the v1 one if `error.type` is a v1 type, else the v2 one.
fn check_error(v: &Value, base: &str) -> Result<(), ConformanceError> {
    let v1 = check::<Error>(v);
    if v1.is_ok() || check::<V2Error>(v).is_ok() {
        return Ok(());
    }
    let is_v1 = v
        .pointer("/error/type")
        .and_then(Value::as_str)
        .is_some_and(|t| V1_ERROR_TYPES.contains(&t));
    let (schema, text) = if is_v1 {
        ("error", v1.unwrap_err())
    } else {
        ("v2.error", check::<V2Error>(v).unwrap_err())
    };
    Err(parse(schema, base, &format!("{schema}: {text}")))
}

fn check_at(v: &Value, base: &str) -> Result<(), ConformanceError> {
    let Some(map) = v.as_object() else {
        return Err(fail("", base, "payload is not a JSON object"));
    };
    match map.get("object").and_then(Value::as_str) {
        Some("list") => return check_items(v, base),
        Some("event") => return check_event(v, base),
        Some(o) => return check_object(o, base, v),
        None => {}
    }
    if map.contains_key("next_page_url") && map.get("data").is_some_and(Value::is_array) {
        check::<V2List<Value>>(v).map_err(|text| parse("list", base, &format!("list: {text}")))?;
        return check_items(v, base);
    }
    if map.get("error").is_some_and(Value::is_object) {
        return check_error(v, base);
    }
    Err(fail(
        "",
        base,
        "payload has no `object` and is not a list or an error",
    ))
}

/// Checks `v` against the pinned schema. Lists and v2 lists check every `data` item. An
/// event checks its envelope, its `data.object`, and that every `previous_attributes` key is
/// a field of that object. A v2 list envelope must have exactly `data`, `next_page_url` and
/// `previous_page_url`. `{"error": {...}}` checks as the v1 or the v2 error envelope. Everything else
/// is checked by its `object` (a `deleted: true` payload against the `deleted_*` schema); an
/// unknown `object` is an error.
pub fn check_payload(v: &Value) -> Result<(), ConformanceError> {
    check_at(v, "")
}
