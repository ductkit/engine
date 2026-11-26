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

/// A page of a v2 list: `{data, next_page_url, previous_page_url}`. The pinned spec types
/// this envelope inline in each v2 list response, so it is hand-written; both page URLs are
/// required and nullable there, so both are always serialized.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct V2List<T> {
    pub data: Vec<T>,
    pub next_page_url: Option<String>,
    pub previous_page_url: Option<String>,
}
