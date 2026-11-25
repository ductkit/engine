//! Embedded lists such as `subscription.items` and `invoice.lines` (spec section 7.3).

use serde_json::{Value, json};

/// The first `page_size` children in creation order, with `has_more`, `total_count`
/// and the `url` of the full sub-list.
#[allow(dead_code)] // TEMP: used from Task 4 (invoice lines)
pub(crate) fn embedded_list(children: &[Value], page_size: usize, url: &str) -> Value {
    json!({
        "object": "list",
        "data": children.iter().take(page_size).cloned().collect::<Vec<_>>(),
        "has_more": children.len() > page_size,
        "total_count": children.len(),
        "url": url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_list_truncates() {
        let kids: Vec<Value> = (0..12).map(|i| json!({"id": i})).collect();
        let l = embedded_list(&kids, 10, "/v1/subscription_items?subscription=sub_1");
        assert_eq!(
            (
                l["data"].as_array().unwrap().len(),
                l["has_more"].clone(),
                l["total_count"].clone()
            ),
            (10, json!(true), json!(12))
        );
        assert_eq!(l["url"], "/v1/subscription_items?subscription=sub_1");
        let short = embedded_list(&kids[..3], 10, "/x");
        assert_eq!(
            (short["has_more"].clone(), short["total_count"].clone()),
            (json!(false), json!(3))
        );
    }
}
