//! Sorting and paging — C#'s `_orderBy`, `_orderBy_desc`, `_start`, `_limit`
//! and `<list>PageInfo`, with the reference Mongo adapter's ordering rules.
//!
//! Adapters receive a [`ListQuery`] through [`crate::TypiQLAdapter::query`];
//! the trait's default implementation filters through `get_many` and then
//! sorts and pages in memory with [`sort_and_page`], so every adapter gets
//! identical results. Adapters that can do better (DuckDB: `ORDER BY` /
//! `LIMIT` / `OFFSET`) override it, and the conformance suite holds them to
//! the same order.

use std::cmp::Ordering;

use async_graphql::{Error, Result as GqlResult, SimpleObject};
use serde_json::Value;

use crate::adapter::{FieldFilter, FilterOp};

/// One sort key: a field's storage name, and direction. A dotted name
/// (`owner.DISPLAY_NAME`) sorts by a field of a joined relation (see
/// [`Join`]); through a has-many relation it follows Mongo — ascending by
/// each row's smallest value, descending by its largest.
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub field: String,
    pub desc: bool,
}

/// Where a join condition's value comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinValue {
    /// The parent row's own field (storage name). A missing or null value
    /// means the parent has no related rows.
    Parent(String),
    /// A literal, or an already-resolved variable (`@currentUser`, ...).
    Literal(Value),
}

/// One condition of a join: the target's `field` (storage name) `op` value.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinCondition {
    pub field: String,
    pub op: FilterOp,
    pub value: JoinValue,
}

/// A relation in join mode (a relation field with a `data_name`, as in C#):
/// its related rows are attached to each parent row under `name` — one
/// object (or null) for has-one, an array for has-many — as part of the
/// parent's own read, so the parent can be sorted by related fields. Only
/// one level deep: joined rows don't carry joins of their own.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub name: String,
    /// The relation field's GraphQL name — a join is only done when the
    /// query selects that field or sorts by it.
    pub graphql_name: String,
    pub target: String,
    /// The adapter name serving the target type; the core does the join
    /// itself when that's a different adapter from the parent's.
    pub target_adapter: &'static str,
    pub target_key: String,
    pub many: bool,
    pub conditions: Vec<JoinCondition>,
    pub sort: Vec<SortKey>,
    pub skip: usize,
    pub limit: Option<usize>,
}

impl Join {
    /// This join's conditions for one parent row, or `None` when a parent
    /// field it needs is missing (no related rows).
    pub fn filters_for(&self, parent: &Value) -> Option<Vec<FieldFilter>> {
        self.conditions
            .iter()
            .map(|c| {
                let value = match &c.value {
                    JoinValue::Literal(v) => v.clone(),
                    JoinValue::Parent(field) => match parent.get(field) {
                        None | Some(Value::Null) => return None,
                        Some(v) => v.clone(),
                    },
                };
                Some(FieldFilter { field: c.field.clone(), op: c.op, value })
            })
            .collect()
    }

    /// Attaches this join's related rows to every parent, choosing from
    /// `candidates` — every target row that may match any parent (see
    /// [`Join::candidate_groups`]).
    pub fn attach(&self, parents: &mut [Value], candidates: &[Value]) {
        for parent in parents.iter_mut() {
            let related: Vec<Value> = match self.filters_for(parent) {
                None => Vec::new(),
                Some(filters) => {
                    let matching = candidates.iter().filter(|c| matches_all(c, &filters)).cloned().collect();
                    let query = ListQuery {
                        sort: self.sort.clone(),
                        skip: self.skip,
                        limit: if self.many { self.limit } else { Some(1) },
                        key_field: self.target_key.clone(),
                        ..Default::default()
                    };
                    sort_and_page(matching, &query)
                }
            };
            let value = if self.many {
                Value::Array(related)
            } else {
                related.into_iter().next().unwrap_or(Value::Null)
            };
            if let Value::Object(map) = parent {
                map.insert(self.name.clone(), value);
            }
        }
    }

    /// One OR-group per parent, for fetching every candidate in one call.
    pub fn candidate_groups(&self, parents: &[Value]) -> Vec<Vec<FieldFilter>> {
        parents.iter().filter_map(|p| self.filters_for(p)).collect()
    }
}

/// Everything a list read needs: filters (all AND-ed), sort keys in
/// priority order, and paging. `key_field` breaks ties so pages are stable
/// on every adapter — Mongo leaves the order of equal keys unspecified, so
/// this is one valid Mongo order, chosen so that it's also the only one.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub filters: Vec<FieldFilter>,
    pub sort: Vec<SortKey>,
    pub skip: usize,
    pub limit: Option<usize>,
    pub key_field: String,
    /// Relations to join into each row — see [`Join`].
    pub joins: Vec<Join>,
}

/// C#'s `PageInfo`, returned by each list's `<list>PageInfo` query.
#[derive(SimpleObject, serde::Serialize, Debug, Clone, PartialEq)]
pub struct PageInfo {
    /// Rows matching the filters, ignoring `_start`/`_limit`.
    pub total_results: i64,
    /// 1-based page `_start` falls on, given `_limit`; 1 without a limit.
    pub page: i64,
    /// Pages of `_limit` rows needed for every result; at least 1.
    pub total_pages: i64,
}

impl PageInfo {
    /// C#'s arithmetic: `page = skip / limit + 1`, `totalPages =
    /// ceil(total / limit)` but never below 1; both 1 without a limit.
    pub fn new(total: usize, skip: usize, limit: Option<usize>) -> Self {
        let (page, pages) = match limit {
            Some(l) if l > 0 => (skip / l + 1, total.div_ceil(l).max(1)),
            _ => (1, 1),
        };
        PageInfo {
            total_results: total as i64,
            page: page as i64,
            total_pages: pages as i64,
        }
    }
}

/// Builds a [`ListQuery`] from a generated resolver's GraphQL arguments.
/// `storage_name` maps a field's GraphQL name to the name its adapter stores
/// it under (see `TypiQLType::sort_field`); an unknown name is an error, as
/// it is in C#. Ascending keys come first, then descending — the order of
/// C#'s `_orderBy` and `_orderBy_desc` arguments.
pub fn list_query(
    filters: Vec<FieldFilter>,
    order_by: Option<String>,
    order_by_desc: Option<String>,
    start: Option<i32>,
    limit: Option<i32>,
    key_field: &str,
    storage_name: impl Fn(&str) -> Option<String>,
) -> GqlResult<ListQuery> {
    let mut sort = Vec::new();
    for (list, desc) in [(order_by, false), (order_by_desc, true)] {
        for name in list.iter().flat_map(|l| l.split(',')).map(str::trim).filter(|n| !n.is_empty()) {
            let field = storage_name(name)
                .ok_or_else(|| Error::new(format!("Unknown sort field `{name}`")))?;
            sort.push(SortKey { field, desc });
        }
    }
    let non_negative = |v: Option<i32>, arg: &str| -> GqlResult<Option<usize>> {
        match v {
            Some(n) if n < 0 => Err(Error::new(format!("`{arg}` can't be negative"))),
            other => Ok(other.map(|n| n as usize)),
        }
    };
    Ok(ListQuery {
        filters,
        sort,
        skip: non_negative(start, "_start")?.unwrap_or(0),
        // `_limit: 0` means no limit, as in C#.
        limit: non_negative(limit, "_limit")?.filter(|&l| l > 0),
        key_field: key_field.to_string(),
        joins: Vec::new(),
    })
}

/// Sorts and pages rows in memory — the default for adapters that don't do
/// it themselves.
pub fn sort_and_page(mut rows: Vec<Value>, q: &ListQuery) -> Vec<Value> {
    if !q.sort.is_empty() {
        rows.sort_by(|a, b| {
            for key in &q.sort {
                let ord = compare(sort_value(a, key), sort_value(b, key));
                let ord = if key.desc { ord.reverse() } else { ord };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            compare(a.get(&q.key_field), b.get(&q.key_field))
        });
    }
    let rows = rows.into_iter().skip(q.skip);
    match q.limit {
        Some(l) => rows.take(l).collect(),
        None => rows.collect(),
    }
}

/// The value a row sorts by for `key`: the field itself, or for a dotted
/// path, the value inside the joined relation. Through an array (has-many),
/// Mongo's rule: the smallest value ascending, the largest descending; no
/// values at all sorts like a missing one.
fn sort_value<'a>(row: &'a Value, key: &SortKey) -> Option<&'a Value> {
    let mut values: Vec<&'a Value> = vec![row];
    for part in key.field.split('.') {
        values = values
            .into_iter()
            .flat_map(|v| match v.get(part) {
                Some(Value::Array(items)) => items.iter().collect(),
                Some(Value::Null) | None => Vec::new(),
                Some(other) => vec![other],
            })
            .collect();
    }
    let pick = |a: &&Value, b: &&Value| compare(Some(a), Some(b));
    if key.desc {
        values.into_iter().max_by(pick)
    } else {
        values.into_iter().min_by(pick)
    }
}

/// Mongo's sort order: missing and null first, then numbers (by value),
/// strings (by bytes — case-sensitive), objects, arrays, and booleans
/// (`false` before `true`).
pub fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    fn rank(v: Option<&Value>) -> u8 {
        match v {
            None | Some(Value::Null) => 0,
            Some(Value::Number(_)) => 1,
            Some(Value::String(_)) => 2,
            Some(Value::Object(_)) => 3,
            Some(Value::Array(_)) => 4,
            Some(Value::Bool(_)) => 5,
        }
    }
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => match (x.as_i64(), y.as_i64()) {
            // Exact for integers beyond f64's 2^53.
            (Some(x), Some(y)) => x.cmp(&y),
            _ => x
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&y.as_f64().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal),
        },
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        (Some(x @ (Value::Object(_) | Value::Array(_))), Some(y)) if rank(Some(x)) == rank(Some(y)) => {
            x.to_string().cmp(&y.to_string())
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

// ── aggregate types ───────────────────────────────────────────────────────

/// One aggregated value of an aggregate type's row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggregateFn {
    /// Rows in the group.
    Count,
    /// Rows in the group with a (non-null) value for the field.
    CountOf,
    /// Total of the field's numbers; 0 when there are none (Mongo `$sum`).
    Sum,
    /// Mean of the field's numbers; null when there are none.
    Avg,
    /// Smallest / largest value, in the reference sort order; null when
    /// there are none.
    Min,
    Max,
}

#[derive(Debug, Clone)]
pub struct Measure {
    /// The aggregate row's field (storage name).
    pub name: String,
    pub func: AggregateFn,
    /// The source field (storage name) — none for `Count`.
    pub field: Option<String>,
}

/// An aggregate type (`#[typiql_type(aggregate = "Source")]`): its rows are
/// the source collection's rows grouped by `groups` (aggregate field,
/// source field — both storage names), each with its `measures`. Like
/// Mongo's `$group`, an empty source has no groups at all.
#[derive(Debug, Clone)]
pub struct AggregateSpec {
    pub source: String,
    pub groups: Vec<(String, String)>,
    pub measures: Vec<Measure>,
}

impl AggregateSpec {
    /// Groups and aggregates source rows in memory — the reference for
    /// adapters that push this into their store.
    pub fn compute(&self, rows: Vec<Value>) -> Vec<Value> {
        let mut groups: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        for row in rows {
            let key: Vec<Value> = self
                .groups
                .iter()
                .map(|(_, source)| row.get(source).cloned().unwrap_or(Value::Null))
                .collect();
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(row),
                None => groups.push((key, vec![row])),
            }
        }
        groups
            .into_iter()
            .map(|(key, members)| {
                let mut out = serde_json::Map::new();
                for ((name, _), value) in self.groups.iter().zip(key) {
                    out.insert(name.clone(), value);
                }
                for m in &self.measures {
                    out.insert(m.name.clone(), m.compute(&members));
                }
                Value::Object(out)
            })
            .collect()
    }
}

impl Measure {
    fn compute(&self, rows: &[Value]) -> Value {
        let values: Vec<&Value> = self
            .field
            .as_ref()
            .map(|f| rows.iter().filter_map(|r| r.get(f)).filter(|v| !v.is_null()).collect())
            .unwrap_or_default();
        let numbers = || values.iter().filter_map(|v| v.as_f64());
        match self.func {
            AggregateFn::Count => Value::from(rows.len() as i64),
            AggregateFn::CountOf => Value::from(values.len() as i64),
            // Non-numbers are ignored; all-integer (or no) numbers sum to an
            // integer.
            AggregateFn::Sum => {
                if values.iter().filter(|v| v.is_number()).all(|v| v.is_i64()) {
                    Value::from(values.iter().filter_map(|v| v.as_i64()).sum::<i64>())
                } else {
                    Value::from(numbers().sum::<f64>())
                }
            }
            AggregateFn::Avg => {
                let n: Vec<f64> = numbers().collect();
                if n.is_empty() {
                    Value::Null
                } else {
                    Value::from(n.iter().sum::<f64>() / n.len() as f64)
                }
            }
            AggregateFn::Min => values.into_iter().min_by(|a, b| compare(Some(a), Some(b))).cloned().unwrap_or(Value::Null),
            AggregateFn::Max => values.into_iter().max_by(|a, b| compare(Some(a), Some(b))).cloned().unwrap_or(Value::Null),
        }
    }
}

// ── list element filters ──────────────────────────────────────────────────

/// Which element of a list an element filter looks at.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElementAt {
    /// Some element (Mongo `$elemMatch`).
    Any,
    First,
    Last,
    /// By position; negative counts from the end (Mongo `$arrayElemAt`).
    Index(i64),
}

/// An element filter's value (see [`FilterOp::ElemMatch`]): which element,
/// and conditions on that one element's fields.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ElementMatch {
    pub at: ElementAt,
    pub filters: Vec<FieldFilter>,
}

impl ElementMatch {
    /// The filter matching rows whose `field` list has such an element.
    pub fn filter(field: impl Into<String>, at: ElementAt, filters: Vec<FieldFilter>) -> FieldFilter {
        FieldFilter {
            field: field.into(),
            op: FilterOp::ElemMatch,
            value: serde_json::to_value(ElementMatch { at, filters }).unwrap_or(Value::Null),
        }
    }

    /// Whether a row's list value has a matching element. `Any` needs an
    /// element that meets every condition. A position with no element there
    /// is checked as an empty element, as Mongo's `$arrayElemAt` yields a
    /// missing value — so `last: { kind_not: "out" }` matches an empty list,
    /// as C#'s `lastNot` did.
    fn matches(&self, list: Option<&Value>) -> bool {
        let items: &[Value] = match list {
            Some(Value::Array(items)) => items,
            _ => &[],
        };
        let at = |i: i64| -> Option<&Value> {
            let idx = if i < 0 { items.len() as i64 + i } else { i };
            usize::try_from(idx).ok().and_then(|i| items.get(i))
        };
        let empty = Value::Object(Default::default());
        match self.at {
            ElementAt::Any => items.iter().any(|e| matches_all(e, &self.filters)),
            ElementAt::First => matches_all(at(0).unwrap_or(&empty), &self.filters),
            ElementAt::Last => matches_all(at(-1).unwrap_or(&empty), &self.filters),
            ElementAt::Index(i) => matches_all(at(i).unwrap_or(&empty), &self.filters),
        }
    }
}

/// Whether a filter can only be evaluated in memory (it looks inside a list)
/// — adapters storing lists as opaque text apply these after their own query.
pub fn is_element_filter(f: &FieldFilter) -> bool {
    matches!(f.op, FilterOp::AnyIn | FilterOp::NoneIn | FilterOp::ElemMatch)
}

// ── in-memory filter evaluation ───────────────────────────────────────────

/// Whether `row` satisfies every filter — the reference (Mongo) semantics,
/// for anything that filters in memory: the JSON adapter, and relation
/// loading, which matches a batch of candidates back to each parent.
pub fn matches_all(row: &Value, filters: &[FieldFilter]) -> bool {
    filters.iter().all(|f| matches_filter(row, f))
}

/// Ordinal comparison across the two JSON scalar shapes that support it —
/// numbers (compared as f64) and strings (lexicographic). Anything else
/// (bool, null, object, array, or a type mismatch) has no ordering.
fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

pub fn matches_filter(item: &Value, filter: &FieldFilter) -> bool {
    let item_val = item.get(&filter.field);
    match filter.op {
        FilterOp::Eq => item_val == Some(&filter.value),
        FilterOp::Ne => item_val != Some(&filter.value),
        FilterOp::Gt => {
            item_val.and_then(|v| compare_values(v, &filter.value))
                == Some(std::cmp::Ordering::Greater)
        }
        FilterOp::Gte => matches!(
            item_val.and_then(|v| compare_values(v, &filter.value)),
            Some(std::cmp::Ordering::Greater) | Some(std::cmp::Ordering::Equal)
        ),
        FilterOp::Lt => {
            item_val.and_then(|v| compare_values(v, &filter.value))
                == Some(std::cmp::Ordering::Less)
        }
        FilterOp::Lte => matches!(
            item_val.and_then(|v| compare_values(v, &filter.value)),
            Some(std::cmp::Ordering::Less) | Some(std::cmp::Ordering::Equal)
        ),
        FilterOp::In => filter
            .value
            .as_array()
            .map(|arr| item_val.map(|v| arr.contains(v)).unwrap_or(false))
            .unwrap_or(false),
        FilterOp::NotIn => filter
            .value
            .as_array()
            .map(|arr| item_val.map(|v| !arr.contains(v)).unwrap_or(true))
            .unwrap_or(true),
        FilterOp::StartsWith => {
            str_match(item_val, &filter.value, |s, n| s.starts_with(n))
        }
        FilterOp::EndsWith => str_match(item_val, &filter.value, |s, n| s.ends_with(n)),
        FilterOp::Contains => str_match(item_val, &filter.value, |s, n| s.contains(n)),
        FilterOp::NotStartsWith => {
            str_match(item_val, &filter.value, |s, n| !s.starts_with(n))
        }
        FilterOp::NotEndsWith => {
            str_match(item_val, &filter.value, |s, n| !s.ends_with(n))
        }
        FilterOp::NotContains => {
            str_match(item_val, &filter.value, |s, n| !s.contains(n))
        }
        // Mongo `$in` / `$nin` against a list: any element in the given
        // values / none of them (a missing list contains none).
        FilterOp::AnyIn | FilterOp::NoneIn => {
            let wanted = filter.value.as_array().map(Vec::as_slice).unwrap_or(&[]);
            let has_any = match item_val {
                Some(Value::Array(items)) => items.iter().any(|i| wanted.contains(i)),
                Some(v) => wanted.contains(v),
                None => false,
            };
            has_any == (filter.op == FilterOp::AnyIn)
        }
        FilterOp::ElemMatch => serde_json::from_value::<ElementMatch>(filter.value.clone())
            .map(|m| m.matches(item_val))
            .unwrap_or(false),
    }
}

/// Shared helper for the `LIKE`-style string operators, following the
/// reference Mongo adapter (whose versions are `/i` regexes):
/// case-insensitive, and both sides must be strings — a missing, null or
/// non-string value never matches, for the negated operators as well
/// (a regex only ever matches a string). `Ne`/`NotIn`, by contrast, do
/// match a missing value, as Mongo's `$ne`/`$nin` do.
fn str_match(item_val: Option<&Value>, needle: &Value, f: impl Fn(&str, &str) -> bool) -> bool {
    match (item_val.and_then(Value::as_str), needle.as_str()) {
        (Some(s), Some(n)) => f(&s.to_lowercase(), &n.to_lowercase()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn page_info_follows_csharp() {
        assert_eq!(PageInfo::new(0, 0, Some(10)), PageInfo { total_results: 0, page: 1, total_pages: 1 });
        assert_eq!(PageInfo::new(25, 20, Some(10)), PageInfo { total_results: 25, page: 3, total_pages: 3 });
        assert_eq!(PageInfo::new(25, 5, None), PageInfo { total_results: 25, page: 1, total_pages: 1 });
    }

    #[test]
    fn mongo_type_order() {
        let mut v = vec![json!(true), json!("b"), json!(2), json!(null), json!("B"), json!(10), json!(false)];
        v.sort_by(|a, b| compare(Some(a), Some(b)));
        assert_eq!(v, vec![json!(null), json!(2), json!(10), json!("B"), json!("b"), json!(false), json!(true)]);
    }
}
