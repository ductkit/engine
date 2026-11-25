//! List query parsing and cursor pagination (spec section 7.3).

use std::collections::BTreeMap;

use crate::api::ApiError;
use crate::engine::store::StoredObject;

/// Filter on `created`, from `created=` or `created[gt|gte|lt|lte]=`.
#[derive(Debug, Clone, Default)]
pub(crate) struct CreatedFilter {
    pub gt: Option<i64>,
    pub gte: Option<i64>,
    pub lt: Option<i64>,
    pub lte: Option<i64>,
    pub eq: Option<i64>,
}

impl CreatedFilter {
    pub(crate) fn matches(&self, created: i64) -> bool {
        self.gt.is_none_or(|v| created > v)
            && self.gte.is_none_or(|v| created >= v)
            && self.lt.is_none_or(|v| created < v)
            && self.lte.is_none_or(|v| created <= v)
            && self.eq.is_none_or(|v| created == v)
    }
}

/// Parsed list parameters.
#[derive(Debug, Clone)]
pub(crate) struct ListParams {
    pub limit: usize,
    pub starting_after: Option<String>,
    pub ending_before: Option<String>,
    pub created: CreatedFilter,
    /// Endpoint-specific filters (e.g. `email`).
    pub extra: BTreeMap<String, String>,
}

fn invalid_integer(param: &str) -> ApiError {
    ApiError::bad_request(
        format!("Invalid integer: {param}"),
        Some("parameter_invalid_integer"),
        Some(param),
    )
}

fn integer(param: &str, value: &str) -> Result<i64, ApiError> {
    value.parse().map_err(|_| invalid_integer(param))
}

/// Parses decoded query pairs. Unknown keys are rejected.
pub(crate) fn parse_list_params(
    pairs: Vec<(String, String)>,
    allowed_extra: &[&str],
) -> Result<ListParams, ApiError> {
    let mut p = ListParams {
        limit: 10,
        starting_after: None,
        ending_before: None,
        created: CreatedFilter::default(),
        extra: BTreeMap::new(),
    };
    for (key, value) in pairs {
        match key.as_str() {
            "limit" => {
                let n = integer("limit", &value)?;
                if !(1..=100).contains(&n) {
                    return Err(ApiError::bad_request(
                        "This value must be between 1 and 100.",
                        Some("parameter_invalid_integer"),
                        Some("limit"),
                    ));
                }
                p.limit = n as usize;
            }
            "starting_after" => p.starting_after = Some(value),
            "ending_before" => p.ending_before = Some(value),
            "created" => p.created.eq = Some(integer(&key, &value)?),
            "created[gt]" => p.created.gt = Some(integer(&key, &value)?),
            "created[gte]" => p.created.gte = Some(integer(&key, &value)?),
            "created[lt]" => p.created.lt = Some(integer(&key, &value)?),
            "created[lte]" => p.created.lte = Some(integer(&key, &value)?),
            k if allowed_extra.contains(&k) => {
                p.extra.insert(key, value);
            }
            _ => {
                return Err(ApiError::bad_request(
                    format!("Received unknown parameter: {key}"),
                    Some("parameter_unknown"),
                    Some(&key),
                ));
            }
        }
    }
    if p.starting_after.is_some() && p.ending_before.is_some() {
        return Err(ApiError::bad_request(
            "You may only specify one of these parameters: starting_after, ending_before.",
            None,
            None,
        ));
    }
    Ok(p)
}

/// Cuts one page from `ordered` (the full list order, deleted objects included,
/// so cursors on deleted objects keep their position). Returns the page and `has_more`.
pub(crate) fn paginate<'a>(
    ordered: &[&'a StoredObject],
    include: impl Fn(&StoredObject) -> bool,
    p: &ListParams,
) -> Result<(Vec<&'a StoredObject>, bool), ApiError> {
    let position = |param: &str, id: &str| {
        ordered
            .iter()
            .position(|o| o.latest.object_id == id)
            .ok_or_else(|| {
                ApiError::bad_request(
                    format!("No such object: '{id}'"),
                    Some("resource_missing"),
                    Some(param),
                )
            })
    };
    let keep = |slice: &[&'a StoredObject]| -> Vec<&'a StoredObject> {
        slice.iter().copied().filter(|o| include(o)).collect()
    };
    if let Some(id) = &p.ending_before {
        let candidates = keep(&ordered[..position("ending_before", id)?]);
        let start = candidates.len().saturating_sub(p.limit);
        return Ok((candidates[start..].to_vec(), start > 0));
    }
    let candidates = match &p.starting_after {
        Some(id) => keep(&ordered[position("starting_after", id)? + 1..]),
        None => keep(ordered),
    };
    let has_more = candidates.len() > p.limit;
    Ok((candidates.into_iter().take(p.limit).collect(), has_more))
}
