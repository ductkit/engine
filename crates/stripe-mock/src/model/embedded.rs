//! Embedded lists such as `subscription.items` and `invoice.lines` (spec section 7.3).

use crate::stripe::{List, ListObject};

/// The first `page_size` children in creation order, with `has_more`, `total_count`
/// and the `url` of the full sub-list.
pub(crate) fn embedded_list<T: Clone>(children: &[T], page_size: usize, url: &str) -> List<T> {
    List {
        object: ListObject::List,
        data: children.iter().take(page_size).cloned().collect(),
        has_more: children.len() > page_size,
        url: url.to_string(),
        total_count: Some(children.len() as i64),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn embedded_list_truncates() {
        let kids: Vec<Value> = (0..12).map(|i| json!({"id": i})).collect();
        let l = embedded_list(&kids, 10, "/v1/subscription_items?subscription=sub_1");
        assert_eq!(
            (l.data.len(), l.has_more, l.total_count),
            (10, true, Some(12))
        );
        assert_eq!(l.url, "/v1/subscription_items?subscription=sub_1");
        let short = embedded_list(&kids[..3], 10, "/x");
        assert_eq!((short.has_more, short.total_count), (false, Some(3)));
        let v = serde_json::to_value(&short).unwrap();
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"].as_array().unwrap().len(), 3);
    }
}
