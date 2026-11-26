//! `expand[]` for billing relations (spec section 7.3).

use serde_json::Value;

use crate::api::ApiError;
use crate::api::resources::View;
use crate::{ChangeKind, ObjectType};

/// Deepest expansion, counting the segments other than `data`.
const MAX_DEPTH: usize = 4;

/// Paths a retrieve of `t` may expand; lists take the same paths after `data.`.
fn allowed(t: ObjectType) -> &'static [&'static str] {
    match t {
        ObjectType::Subscription => &[
            "customer",
            "default_payment_method",
            "latest_invoice",
            "items.data.price.product",
        ],
        ObjectType::Invoice => &[
            "customer",
            "default_payment_method",
            "parent.subscription_details.subscription",
        ],
        ObjectType::Customer => &["invoice_settings.default_payment_method"],
        ObjectType::Price => &["product"],
        ObjectType::SubscriptionItem => &["price.product"],
        // Each new type's `expandable_fields` whose target is modelled (not `application`
        // or `on_behalf_of`: accounts are platform-scope, and `application` is not a type).
        ObjectType::PaymentIntent => &["customer", "latest_charge", "payment_method"],
        ObjectType::Charge => &[
            "balance_transaction",
            "customer",
            "failure_balance_transaction",
            "payment_intent",
        ],
        ObjectType::Refund => &[
            "balance_transaction",
            "charge",
            "customer",
            "failure_balance_transaction",
            "payment_intent",
            "payment_method",
        ],
        ObjectType::Dispute => &["charge", "payment_intent"],
        ObjectType::BalanceTransaction => &["source"],
        ObjectType::PaymentMethod => &["customer"],
        ObjectType::SetupIntent => &["customer", "payment_method"],
        ObjectType::CheckoutSession => &[
            "customer",
            "invoice",
            "payment_intent",
            "setup_intent",
            "subscription",
        ],
        ObjectType::InvoicePayment => &["invoice"],
        ObjectType::Product | ObjectType::InvoiceLineItem | ObjectType::Account => &[],
    }
}

fn invalid(message: String) -> ApiError {
    ApiError::bad_request(message, None, Some("expand"))
}

/// Checks every path against what `t` (or a list of `t`) can expand.
fn validate(t: ObjectType, paths: &[String], list: bool) -> Result<(), ApiError> {
    for path in paths {
        if path.split('.').filter(|s| *s != "data").count() > MAX_DEPTH {
            return Err(invalid(
                "You cannot expand more than 4 levels of a property.".to_string(),
            ));
        }
        let inner = if list {
            path.strip_prefix("data.")
        } else {
            Some(path.as_str())
        };
        if !inner.is_some_and(|p| allowed(t).contains(&p)) {
            return Err(invalid(format!(
                "This property cannot be expanded ({path})."
            )));
        }
    }
    Ok(())
}

/// The type an ID names. The longest matching `prefix_` wins, so `cs_test_...` is a checkout
/// session even though prefixes could contain underscores.
fn type_of_id(id: &str) -> Option<ObjectType> {
    ObjectType::ALL
        .into_iter()
        .filter(|t| {
            id.strip_prefix(t.id_prefix())
                .is_some_and(|rest| rest.starts_with('_'))
        })
        .max_by_key(|t| t.id_prefix().len())
}

/// The object an ID names, as expansion shows it: a deleted customer is its
/// stub, other deleted objects are `null`, unknown IDs stay strings.
fn lookup(view: &View, id: &str) -> Option<Value> {
    let t = type_of_id(id)?;
    let u = view.state(view.store().get(t, id)?)?;
    if u.kind == ChangeKind::Deleted && t != ObjectType::Customer {
        return Some(Value::Null);
    }
    Some(u.state.clone())
}

fn apply(view: &View, v: &mut Value, segs: &[&str]) {
    let Some((first, rest)) = segs.split_first() else {
        return;
    };
    if *first == "data"
        && let Some(items) = v.get_mut("data").and_then(Value::as_array_mut)
    {
        for item in items {
            apply(view, item, rest);
        }
        return;
    }
    let Some(child) = v.get_mut(*first) else {
        return;
    };
    if let Some(id) = child.as_str()
        && let Some(found) = lookup(view, id)
    {
        *child = found;
    }
    apply(view, child, rest);
}

/// Expands `paths` in `obj`, an object of type `t` or a list of them.
pub(crate) fn expand(
    view: &View,
    t: ObjectType,
    obj: &mut Value,
    paths: &[String],
) -> Result<(), ApiError> {
    validate(t, paths, obj["object"] == "list")?;
    for path in paths {
        let segs: Vec<&str> = path.split('.').collect();
        apply(view, obj, &segs);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_resolve_by_longest_prefix() {
        assert_eq!(
            type_of_id("cs_test_a1B2"),
            Some(ObjectType::CheckoutSession)
        );
        assert_eq!(type_of_id("cus_1"), Some(ObjectType::Customer));
        assert_eq!(type_of_id("in_1"), Some(ObjectType::Invoice));
        assert_eq!(type_of_id("inpay_1"), Some(ObjectType::InvoicePayment));
        assert_eq!(type_of_id("pi_1"), Some(ObjectType::PaymentIntent));
        assert_eq!(type_of_id("cs_live_1"), None);
        assert_eq!(type_of_id("nope"), None);
    }
}
