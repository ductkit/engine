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
        ObjectType::Subscription => &["customer", "latest_invoice", "items.data.price.product"],
        ObjectType::Invoice => &["customer", "parent.subscription_details.subscription"],
        ObjectType::Price => &["product"],
        ObjectType::SubscriptionItem => &["price.product"],
        ObjectType::Customer | ObjectType::Product | ObjectType::InvoiceLineItem => &[],
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

/// The object an ID names, as expansion shows it: a deleted customer is its
/// stub, other deleted objects are `null`, unknown IDs stay strings.
fn lookup(view: &View, id: &str) -> Option<Value> {
    let prefix = id.split_once('_')?.0;
    let t = ObjectType::ALL
        .into_iter()
        .find(|t| t.id_prefix() == prefix)?;
    let u = view.state(view.engine.store().get(t, id)?)?;
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
