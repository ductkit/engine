//! `expand[]` for billing relations (spec section 7.3).

use serde_json::Value;

use crate::api::ApiError;
use crate::api::resources::View;
use crate::engine::Scope;
use crate::model::embedded::embedded_list;
use crate::model::invoice::payments_url;
use crate::model::payments::refunds_url;
use crate::{ChangeKind, ObjectType};

/// Deepest expansion, counting the segments other than `data`.
const MAX_DEPTH: usize = 4;

/// Paths a retrieve of `t` may expand; lists take the same paths after `data.`.
pub(crate) fn allowed(t: ObjectType) -> &'static [&'static str] {
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
            "payments",
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
            "refunds",
            "transfer",
            "application_fee",
            "source_transfer",
            "transfer_data.destination",
        ],
        ObjectType::Refund => &[
            "balance_transaction",
            "charge",
            "customer",
            "failure_balance_transaction",
            "payment_intent",
            "payment_method",
            "source_transfer_reversal",
            "transfer_reversal",
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
        // The modelled targets only: `reversals` and `refunds` are lists the object
        // carries, and `application`, `fee_source` and the sub-objects are not types.
        ObjectType::Transfer => &[
            "balance_transaction",
            "destination",
            "destination_payment",
            "source_transaction",
        ],
        ObjectType::TransferReversal => &[
            "balance_transaction",
            "destination_payment_refund",
            "source_refund",
            "transfer",
        ],
        ObjectType::ApplicationFee => &[
            "account",
            "balance_transaction",
            "charge",
            "originating_transaction",
        ],
        ObjectType::FeeRefund => &["balance_transaction", "fee"],
        ObjectType::Payout => &[
            "application_fee",
            "balance_transaction",
            "destination",
            "failure_balance_transaction",
            "original_payout",
            "reversed_by",
        ],
        // `account` is a platform-scope object (see `foreign_scope`).
        ObjectType::BankAccount | ObjectType::Card => &["account", "customer"],
        ObjectType::Product
        | ObjectType::InvoiceLineItem
        | ObjectType::Account
        | ObjectType::Person => &[],
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
    // A destination payment (`py_`) is a charge in the account, its refund (`pyr_`) a refund.
    // Neither is a type of its own.
    for (prefix, t) in [("pyr_", ObjectType::Refund), ("py_", ObjectType::Charge)] {
        if id.starts_with(prefix) {
            return Some(t);
        }
    }
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
fn lookup(view: &View, id: &str, foreign: Option<Scope>) -> Option<Value> {
    let t = type_of_id(id)?;
    // A foreign scope is read at the view's own time; an account that has disconnected is
    // out of reach, so its objects stay IDs.
    let other;
    let view = match foreign {
        Some(scope) => {
            if let Some(acct) = &scope
                && !view.engine.account_meta(acct).is_some_and(|m| m.connected)
            {
                return None;
            }
            other = View {
                scope,
                ..view.clone()
            };
            &other
        }
        None => view,
    };
    let u = view.state(view.store().get(t, id)?)?;
    if u.kind == ChangeKind::Deleted && t != ObjectType::Customer {
        return Some(Value::Null);
    }
    Some(u.state.clone())
}

/// An includable list field of `parent` (absent unless expanded, as Stripe serves
/// `charge.refunds` and `invoice.payments`), built from the store: refunds newest first,
/// invoice payments in creation order, the first `nested_list_page_size` of them.
fn includable(view: &View, parent: &Value, field: &str) -> Option<Value> {
    let id = parent["id"].as_str()?;
    let (t, key, newest_first, url) = match (parent["object"].as_str()?, field) {
        ("charge", "refunds") => (ObjectType::Refund, "charge", true, refunds_url(id)),
        ("invoice", "payments") => (
            ObjectType::InvoicePayment,
            "invoice",
            false,
            payments_url(id),
        ),
        _ => return None,
    };
    let mut items: Vec<Value> = view
        .store()
        .children(t, key, id)
        .into_iter()
        .filter_map(|o| view.listed(o))
        .map(|u| u.state.clone())
        .collect();
    if newest_first {
        items.reverse();
    }
    Some(
        serde_json::to_value(embedded_list(&items, view.page_size, &url))
            .expect("a list serializes"),
    )
}

/// The scope a path of `obj` (an object of type `t`) resolves in when it is not the view's
/// own (phase 8 ruling P2): the destination payment of a transfer lives in the destination
/// account, an application fee's charge in the account it was taken from, and a direct
/// charge's fee, source transfer and transfer reversal on the platform. `None` is the
/// view's scope; `Some(None)` is the platform.
///
/// A sibling the same request expanded first is an object by now, so its `id` is read.
pub(crate) fn foreign_scope(view: &View, t: ObjectType, obj: &Value, path: &str) -> Option<Scope> {
    let account = |field: &str| id_of(&obj[field]).map(|a| Some(a.to_string()));
    match (t, path) {
        (ObjectType::Transfer, "destination_payment") => account("destination"),
        (ObjectType::TransferReversal, "destination_payment_refund") => {
            let transfer = id_of(&obj["transfer"])?;
            let u = view.state(view.store().get(ObjectType::Transfer, transfer)?)?;
            id_of(&u.state["destination"]).map(|a| Some(a.to_string()))
        }
        (ObjectType::ApplicationFee, "charge") => account("account"),
        (ObjectType::Charge, "application_fee" | "source_transfer")
        | (ObjectType::Refund, "source_transfer_reversal")
        // An external account names its account, which lives on the platform.
        | (ObjectType::BankAccount | ObjectType::Card, "account") => Some(None),
        _ => None,
    }
}

/// The ID an expandable field holds: the string itself, or an expanded object's `id`.
fn id_of(v: &Value) -> Option<&str> {
    v.as_str().or_else(|| v["id"].as_str())
}

/// The type an object's `object` field names.
fn type_of_object(v: &Value) -> Option<ObjectType> {
    let name = v["object"].as_str()?;
    ObjectType::ALL.into_iter().find(|t| t.as_str() == name)
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
    if v.get(*first).is_none()
        && let Some(list) = includable(view, v, first)
    {
        v[*first] = list;
    }
    // `foreign_scope` reads sibling fields of the parent, so it is decided before the child
    // is borrowed.
    let foreign = match v.get(*first).and_then(Value::as_str) {
        Some(_) => type_of_object(v).and_then(|t| foreign_scope(view, t, v, first)),
        None => None,
    };
    let Some(child) = v.get_mut(*first) else {
        return;
    };
    if let Some(id) = child.as_str()
        && let Some(found) = lookup(view, id, foreign)
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
    fn py_and_pyr_ids_resolve_by_prefix() {
        assert_eq!(type_of_id("py_1Nabc"), Some(ObjectType::Charge));
        assert_eq!(type_of_id("pyr_1Nabc"), Some(ObjectType::Refund));
        assert_eq!(type_of_id("py1"), None);
    }

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
        assert_eq!(type_of_id("tr_1"), Some(ObjectType::Transfer));
        assert_eq!(type_of_id("trr_1"), Some(ObjectType::TransferReversal));
        assert_eq!(type_of_id("fee_1"), Some(ObjectType::ApplicationFee));
        assert_eq!(type_of_id("fr_1"), Some(ObjectType::FeeRefund));
        assert_eq!(type_of_id("po_1"), Some(ObjectType::Payout));
        assert_eq!(type_of_id("ba_1"), Some(ObjectType::BankAccount));
        assert_eq!(type_of_id("card_1"), Some(ObjectType::Card));
        assert_eq!(type_of_id("person_1"), Some(ObjectType::Person));
        assert_eq!(type_of_id("cs_live_1"), None);
        assert_eq!(type_of_id("nope"), None);
    }
}
