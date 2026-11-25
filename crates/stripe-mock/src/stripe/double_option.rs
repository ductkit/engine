//! Serde `with` helpers for a field that is optional and nullable, `Option<Option<T>>`:
//! `None` is absent, `Some(None)` is `null`, `Some(Some(v))` is a value.
//!
//! Use with `#[serde(default, skip_serializing_if = "Option::is_none", with = "...")]`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<T: Serialize, S: Serializer>(
    value: &Option<Option<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        // Skipped by `skip_serializing_if`; `null` when serialized without it.
        None => serializer.serialize_none(),
        Some(inner) => inner.serialize(serializer),
    }
}

pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}
