//! List query parsing and cursor pagination (spec section 7.3).

use std::collections::BTreeMap;

use crate::api::ApiError;
use crate::engine::events::EventRecord;
use crate::engine::store::StoredObject;

/// Something a list cursor can name.
/// Decodes a raw query string into ordered pairs.
pub(crate) fn decode(query: Option<String>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

pub(crate) trait Cursorable {
    fn cursor_id(&self) -> &str;
}

impl Cursorable for StoredObject {
    fn cursor_id(&self) -> &str {
        &self.latest.object_id
    }
}

impl Cursorable for EventRecord {
    fn cursor_id(&self) -> &str {
        &self.id
    }
}

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
    /// Endpoint-specific array filters (e.g. `types[]`), keyed without brackets.
    pub extra_multi: BTreeMap<String, Vec<String>>,
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
    allowed_multi: &[&str],
) -> Result<ListParams, ApiError> {
    let mut p = ListParams {
        limit: 10,
        starting_after: None,
        ending_before: None,
        created: CreatedFilter::default(),
        extra: BTreeMap::new(),
        extra_multi: BTreeMap::new(),
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
            k if array_base(k).is_some_and(|b| allowed_multi.contains(&b)) => {
                let base = array_base(k).expect("checked").to_string();
                p.extra_multi.entry(base).or_default().push(value);
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

/// `types` for `types[]` or `types[3]`.
pub(crate) fn array_base(key: &str) -> Option<&str> {
    let (base, rest) = key.split_once('[')?;
    let inner = rest.strip_suffix(']')?;
    (inner.is_empty() || inner.bytes().all(|b| b.is_ascii_digit())).then_some(base)
}

/// Cuts one page from `ordered` (the full list order, deleted objects included,
/// so cursors on deleted objects keep their position). Returns the page and `has_more`.
pub(crate) fn paginate<'a, T: Cursorable>(
    ordered: &[&'a T],
    include: impl Fn(&T) -> bool,
    p: &ListParams,
) -> Result<(Vec<&'a T>, bool), ApiError> {
    let position = |param: &str, id: &str| {
        ordered
            .iter()
            .position(|o| o.cursor_id() == id)
            .ok_or_else(|| {
                ApiError::bad_request(
                    format!("No such object: '{id}'"),
                    Some("resource_missing"),
                    Some(param),
                )
            })
    };
    // Evaluate `include` only until the page and one more match are found.
    let take = |items: &mut dyn Iterator<Item = &&'a T>| {
        let mut page: Vec<&'a T> = Vec::new();
        for o in items {
            if include(o) {
                if page.len() == p.limit {
                    return (page, true);
                }
                page.push(*o);
            }
        }
        (page, false)
    };
    if let Some(id) = &p.ending_before {
        let end = position("ending_before", id)?;
        let (mut page, has_more) = take(&mut ordered[..end].iter().rev());
        page.reverse();
        return Ok((page, has_more));
    }
    let from = match &p.starting_after {
        Some(id) => position("starting_after", id)? + 1,
        None => 0,
    };
    Ok(take(&mut ordered[from..].iter()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Item(String, usize);

    impl Cursorable for Item {
        fn cursor_id(&self) -> &str {
            &self.0
        }
    }

    /// The pre-phase-4 algorithm: filter everything, then cut.
    fn reference<'a>(
        ordered: &[&'a Item],
        include: impl Fn(&Item) -> bool,
        p: &ListParams,
    ) -> (Vec<&'a Item>, bool) {
        let pos = |id: &str| ordered.iter().position(|o| o.0 == id).unwrap();
        let keep = |s: &[&'a Item]| -> Vec<&'a Item> {
            s.iter().copied().filter(|o| include(o)).collect()
        };
        if let Some(id) = &p.ending_before {
            let c = keep(&ordered[..pos(id)]);
            let start = c.len().saturating_sub(p.limit);
            return (c[start..].to_vec(), start > 0);
        }
        let c = match &p.starting_after {
            Some(id) => keep(&ordered[pos(id) + 1..]),
            None => keep(ordered),
        };
        let has_more = c.len() > p.limit;
        (c.into_iter().take(p.limit).collect(), has_more)
    }

    fn params(limit: usize, after: Option<String>, before: Option<String>) -> ListParams {
        ListParams {
            limit,
            starting_after: after,
            ending_before: before,
            created: CreatedFilter::default(),
            extra: BTreeMap::new(),
            extra_multi: BTreeMap::new(),
        }
    }

    #[test]
    fn paginate_matches_full_filter() {
        let items: Vec<Item> = (0..30).map(|i| Item(format!("o_{i}"), i)).collect();
        let ordered: Vec<&Item> = items.iter().collect();
        let include = |o: &Item| o.1.is_multiple_of(3);
        let names = |v: &[&Item]| v.iter().map(|o| o.0.clone()).collect::<Vec<_>>();
        for limit in 1..=12 {
            let mut cases = vec![params(limit, None, None)];
            for o in &items {
                cases.push(params(limit, Some(o.0.clone()), None));
                cases.push(params(limit, None, Some(o.0.clone())));
            }
            for p in cases {
                let (got, more) = paginate(&ordered, include, &p).unwrap();
                let (want, want_more) = reference(&ordered, include, &p);
                assert_eq!(
                    (names(&got), more),
                    (names(&want), want_more),
                    "{limit} {:?} {:?}",
                    p.starting_after,
                    p.ending_before
                );
            }
        }
    }
}
