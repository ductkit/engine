//! Stripe API types (design section 6.2).
//!
//! `generated` is emitted from the vendored OpenAPI spec by `tests/codegen` and committed;
//! never edit it by hand. The rest of this module holds the hand-written types it builds on.

pub mod double_option;
mod expandable;
mod list;

// The generator emits formatted code (prettyplease); rustfmt must not reformat it, or the
// freshness test fails.
// `check_by_object` has no caller until `check_payload` is added on top of it.
#[rustfmt::skip]
#[allow(dead_code)]
mod generated;

pub use expandable::{Expandable, HasId};
pub use generated::*;
pub use list::{List, ListObject};

use serde::de::DeserializeOwned;
use serde_json::Value;

/// A conformance check for one generated type.
type Check = fn(&Value) -> Result<(), String>;

/// Deserializes `v` into `T`, unknown fields denied; the error names the failing path.
fn check<T: DeserializeOwned>(v: &Value) -> Result<(), String> {
    serde_path_to_error::deserialize::<_, T>(v)
        .map(drop)
        .map_err(|e| format!("{}: {}", e.path(), e.inner()))
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
