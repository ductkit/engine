//! Stripe's list envelope.

use serde::{Deserialize, Serialize};

/// A page of objects: `{object: "list", data, has_more, url}`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct List<T> {
    pub object: ListObject,
    pub data: Vec<T>,
    pub has_more: bool,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
}

/// The `object` value of a list.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListObject {
    #[serde(rename = "list")]
    List,
}
