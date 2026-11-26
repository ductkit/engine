//! Stripe API types (design section 6.2).
//!
//! `generated` is emitted from the vendored OpenAPI spec by `tests/codegen` and committed;
//! never edit it by hand. The rest of this module holds the hand-written types it builds on.

mod conformance;
pub mod double_option;
mod expandable;
mod list;

// The generator emits formatted code (prettyplease); rustfmt must not reformat it, or the
// freshness test fails.
#[rustfmt::skip]
mod generated;

pub use conformance::{ConformanceError, check_payload};
pub use expandable::{Expandable, HasId};
pub use generated::*;
pub use list::{List, ListObject};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A conformance check for one generated type.
type Check = fn(&Value) -> Result<(), String>;

/// Deserializes `v` into `T`, unknown fields denied, then serializes it back and compares:
/// what deserializes but does not round-trip (such as `null` in a field that is optional but
/// not nullable, which serde reads as absent) is an error too. The error names the path.
fn check<T: DeserializeOwned + Serialize>(v: &Value) -> Result<(), String> {
    let typed: T =
        serde_path_to_error::deserialize(v).map_err(|e| format!("{}: {}", e.path(), e.inner()))?;
    let back = serde_json::to_value(&typed).map_err(|e| format!(".: does not serialize: {e}"))?;
    match round_trip_diff(v, &back, "") {
        None => Ok(()),
        Some((path, message)) => Err(format!(
            "{}: {message}",
            if path.is_empty() { "." } else { &path }
        )),
    }
}

fn child_path(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_string()
    } else {
        format!("{base}.{key}")
    }
}

/// The first place where the served `sent` and its round trip `back` differ. Numbers compare
/// by value, so `1` and `1.0` are equal.
fn round_trip_diff(sent: &Value, back: &Value, path: &str) -> Option<(String, String)> {
    match (sent, back) {
        (Value::Object(s), Value::Object(b)) => {
            for (k, sv) in s {
                let p = child_path(path, k);
                match b.get(k) {
                    None if sv.is_null() => {
                        return Some((p, "`null` is not allowed here (not nullable)".into()));
                    }
                    None => return Some((p, "lost on the round trip through the type".into())),
                    Some(bv) => {
                        if let Some(d) = round_trip_diff(sv, bv, &p) {
                            return Some(d);
                        }
                    }
                }
            }
            b.keys()
                .find(|k| !s.contains_key(*k))
                .map(|k| (child_path(path, k), "added by the round trip".into()))
        }
        (Value::Array(s), Value::Array(b)) if s.len() == b.len() => s
            .iter()
            .zip(b)
            .enumerate()
            .find_map(|(i, (sv, bv))| round_trip_diff(sv, bv, &format!("{path}[{i}]"))),
        (Value::Number(s), Value::Number(b)) if s.as_f64() == b.as_f64() => None,
        _ if sent == back => None,
        _ => Some((
            path.to_string(),
            format!("`{sent}` round-trips as `{back}`"),
        )),
    }
}

/// Checks `v` against the schemas that share its `object` value (`customer` and
/// `deleted_customer`): those whose `deleted_` prefix matches `v.deleted`, or all of them when
/// none does. Passes if one passes; otherwise reports the first one's error.
fn check_candidates(v: &Value, candidates: &[(&str, Check)]) -> Result<(), String> {
    let deleted = v.get("deleted") == Some(&Value::Bool(true));
    let matching: Vec<_> = candidates
        .iter()
        .filter(|(schema, _)| schema.starts_with("deleted_") == deleted)
        .collect();
    let tried = if matching.is_empty() {
        candidates.iter().collect()
    } else {
        matching
    };
    let mut first_error = None;
    for (schema, check) in tried {
        match check(v) {
            Ok(()) => return Ok(()),
            Err(e) => {
                first_error.get_or_insert_with(|| format!("{schema}: {e}"));
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "no schema to check against".to_string()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn customer() -> Value {
        json!({ "id": "cus_1", "object": "customer", "created": 1, "livemode": false })
    }

    #[test]
    fn check_by_object_accepts_a_minimal_customer() {
        assert_eq!(check_by_object("customer", &customer()), Some(Ok(())));
    }

    #[test]
    fn check_by_object_rejects_unknown_fields_with_their_path() {
        let mut v = customer();
        v["invoice_settings"] = json!({ "footer": null, "bogus": 1 });
        let err = check_by_object("customer", &v).unwrap().unwrap_err();
        assert!(err.contains("invoice_settings"), "{err}");
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn check_by_object_accepts_a_tombstone_and_rejects_a_missing_required_field() {
        let v = json!({ "id": "cus_1", "object": "customer", "deleted": true });
        assert_eq!(check_by_object("customer", &v), Some(Ok(())));
        let mut v = customer();
        v.as_object_mut().unwrap().remove("livemode");
        assert!(check_by_object("customer", &v).unwrap().is_err());
    }

    #[test]
    fn check_by_object_is_none_for_unknown_objects() {
        assert_eq!(check_by_object("no_such_object", &json!({})), None);
    }

    #[test]
    fn tables_cover_named_schemas() {
        assert!(SCHEMA_OBJECTS.contains(&("customer", "deleted_customer")));
        assert!(SCHEMA_OBJECTS.contains(&("checkout.session", "checkout.session")));
        let fields = schema_fields("customer").unwrap();
        assert!(fields.contains(&"invoice_settings"));
        assert_eq!(schema_fields("customer/invoice_settings"), None);
        assert!(expandable_fields("charge").contains(&"customer"));
        assert!(expandable_fields("no_such_schema").is_empty());
    }

    #[test]
    fn optional_fields_keep_absent_and_null_apart() {
        let c: Customer = serde_json::from_value(customer()).unwrap();
        assert_eq!(c.email, None);
        let mut v = customer();
        v["email"] = Value::Null;
        let c: Customer = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(c.email, Some(None));
        assert_eq!(serde_json::to_value(&c).unwrap(), v);
        assert_eq!(c.id(), "cus_1");
    }
}
