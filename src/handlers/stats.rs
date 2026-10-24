//! `get_column_stats`: the data grid's column statistics popover
//! (Tabularis #1137), computed in SurrealQL.
//!
//! One script, sent as a single `query` call:
//!
//! 1. Exact counts over every row matching the filter:
//!    `SELECT count() AS rows, count((c = NONE) OR (c = NULL)) AS nulls
//!    FROM t WHERE (filter) GROUP ALL`. A missing field is NONE, so it
//!    counts as NULL, like an empty grid cell.
//! 2. When the host asks for distinct / min / max / top values: the first
//!    [`SAMPLE_SIZE`] non-NULL values in the table's natural order go into
//!    `$values`, and everything else is computed from that sample with
//!    `array::distinct`, `array::min` / `array::max`, and the groups of
//!    `GROUP BY v` over `$values`, sorted by count in an outer SELECT (2.x
//!    ignores an ORDER BY in the grouping SELECT itself). When the
//!    column has more non-NULL values than the sample, the result says so
//!    with `sample_size`.
//!
//! With a query timeout set, every SELECT carries `TIMEOUT <n>s`, so the
//! server stops the work itself; the popover never cancels.
//!
//! See docs/DESIGN.md, "Column statistics".

use std::time::Duration;

use ciborium::value::Value as Cbor;
use serde_json::{json, Value as Json};

use crate::error::PluginError;
use crate::handlers::query::run;
use crate::handlers::{require_database, session_config, Context};
use crate::surreal::session::StatementOutcome;
use crate::utils::identifiers::quote_identifier;
use crate::utils::surql::{is_single_expression, split_statements};
use crate::utils::values::{is_none, to_cell};

/// Non-NULL values the distinct count, min / max and top values are
/// computed from.
pub const SAMPLE_SIZE: u64 = 100_000;

/// Upper bound for the host's `top_values_limit`.
pub const MAX_TOP_VALUES: u32 = 100;

/// The host's request (`params.request`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsRequest {
    pub table: String,
    pub column: String,
    /// The tab's filter, a SurrealQL condition without `WHERE`.
    pub filter: Option<String>,
    pub top_values_limit: u32,
    pub distinct: bool,
    pub min_max: bool,
    pub top_values: bool,
}

impl StatsRequest {
    pub fn from_params(params: &Json) -> Result<Self, PluginError> {
        let request = params
            .get("request")
            .ok_or_else(|| PluginError::invalid_params("missing `request`"))?;
        let text = |key: &str| {
            request
                .get(key)
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let table =
            text("table").ok_or_else(|| PluginError::invalid_params("missing `request.table`"))?;
        let column = text("column")
            .ok_or_else(|| PluginError::invalid_params("missing `request.column`"))?;
        let filter = text("filter")
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        if let Some(f) = &filter {
            if !is_single_expression(f) {
                return Err(PluginError::invalid_params(
                    "The table filter must be a single SurrealQL condition, with balanced brackets and quotes, to be added to the statistics query.",
                ));
            }
        }
        let top_values_limit = request
            .get("top_values_limit")
            .and_then(Json::as_u64)
            .map(|n| n.min(u64::from(MAX_TOP_VALUES)) as u32)
            .unwrap_or(10);
        let include = request.get("include").unwrap_or(&Json::Null);
        let part = |key: &str| include.get(key).and_then(Json::as_bool).unwrap_or(false);
        Ok(Self {
            table,
            column,
            filter,
            top_values_limit,
            distinct: part("distinct"),
            min_max: part("min_max"),
            top_values: part("top_values") && top_values_limit > 0,
        })
    }

    fn wants_values(&self) -> bool {
        self.distinct || self.min_max || self.top_values
    }
}

/// The SurrealQL script for `request`, sampling at most `sample_size`
/// non-NULL values. Identifiers are always quoted. The filter goes in
/// brackets, followed by a newline so a trailing `--` comment can't swallow
/// the closing bracket. With `timeout_secs`, every SELECT ends in
/// `TIMEOUT <n>s` (the last clause of a SELECT), so the server stops it.
pub fn build_script(request: &StatsRequest, sample_size: u64, timeout_secs: Option<u64>) -> String {
    let table = quote_identifier(&request.table);
    let col = quote_identifier(&request.column);
    let filter = request.filter.as_deref().map(|f| format!("({f}\n)"));
    let timeout = timeout_secs
        .map(|t| format!(" TIMEOUT {t}s"))
        .unwrap_or_default();
    let mut script = format!(
        "SELECT count() AS rows, count(({col} = NONE) OR ({col} = NULL)) AS nulls FROM {table}{} GROUP ALL{timeout};",
        filter
            .as_deref()
            .map(|f| format!(" WHERE {f}"))
            .unwrap_or_default()
    );
    if !request.wants_values() {
        return script;
    }
    let not_null = format!("{col} != NONE AND {col} != NULL");
    let condition = match &filter {
        Some(f) => format!("{f} AND {not_null}"),
        None => not_null,
    };
    script.push_str(&format!(
        "\nLET $values = (SELECT {col} AS v FROM {table} WHERE {condition} LIMIT {sample_size}{timeout});"
    ));
    let mut fields = vec!["sampled: array::len($values)".to_string()];
    if request.distinct {
        fields.push("distinct: array::len(array::distinct($values.v))".into());
    }
    if request.min_max {
        fields.push("min: array::min($values.v)".into());
        fields.push("max: array::max($values.v)".into());
    }
    script.push_str(&format!("\nRETURN {{ {} }};", fields.join(", ")));
    if request.top_values {
        // The sort is in an outer SELECT: 2.x ignores `ORDER BY count` in the
        // SELECT that groups and returns the groups ordered by `v`, so the
        // LIMIT would keep the wrong values.
        script.push_str(&format!(
            "\nSELECT * FROM (SELECT v, count() AS count FROM $values GROUP BY v) ORDER BY count DESC, v ASC LIMIT {}{timeout};",
            request.top_values_limit
        ));
    }
    script
}

/// A field of a CBOR map.
fn field<'a>(map: &'a Cbor, key: &str) -> Option<&'a Cbor> {
    match map {
        Cbor::Map(entries) => entries
            .iter()
            .find(|(k, _)| matches!(k, Cbor::Text(t) if t == key))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn count(value: Option<&Cbor>) -> Result<u64, PluginError> {
    match value.map(to_cell) {
        // A GROUP ALL over no rows returns no row on some versions.
        None | Some(Json::Null) => Ok(0),
        Some(Json::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .ok_or_else(|| PluginError::internal(format!("unexpected count {n}"))),
        Some(Json::String(s)) => s
            .parse()
            .map_err(|_| PluginError::internal(format!("unexpected count {s}"))),
        Some(other) => Err(PluginError::internal(format!("unexpected count {other}"))),
    }
}

/// The first row of a statement that returns rows, or the value itself.
fn first_row(value: &Cbor) -> Option<&Cbor> {
    match value {
        Cbor::Array(items) => items.first(),
        other => Some(other),
    }
}

/// Turn the statement results of [`build_script`] into the host's result.
pub fn build_result(
    request: &StatsRequest,
    values: &[Cbor],
    sample_size: u64,
) -> Result<Json, PluginError> {
    let counts = values
        .first()
        .ok_or_else(|| PluginError::internal("SurrealDB returned no result"))?;
    let counts_row = first_row(counts);
    let rows = count(counts_row.and_then(|r| field(r, "rows")))?;
    let nulls = count(counts_row.and_then(|r| field(r, "nulls")))?.min(rows);

    let mut result = json!({
        "row_count": rows,
        "null_count": nulls,
        "distinct_count": null,
        "min": null,
        "max": null,
        "top_values": null,
    });
    if !request.wants_values() {
        return Ok(result);
    }
    // values[1] is the LET (NONE), values[2] the RETURN object.
    let summary = values
        .get(2)
        .ok_or_else(|| PluginError::internal("SurrealDB returned too few results"))?;
    let sampled = count(field(summary, "sampled"))?;
    if request.distinct {
        result["distinct_count"] = json!(count(field(summary, "distinct"))?);
    }
    if request.min_max {
        let value = |key| field(summary, key).map(to_cell).unwrap_or(Json::Null);
        result["min"] = value("min");
        result["max"] = value("max");
    }
    if request.top_values {
        let top = match values.get(3) {
            Some(Cbor::Array(items)) => items
                .iter()
                .filter_map(|row| {
                    let value = field(row, "v").filter(|v| !is_none(v) && !v.is_null())?;
                    Some(
                        count(field(row, "count"))
                            .map(|n| json!({ "value": to_cell(value), "count": n })),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };
        result["top_values"] = Json::Array(top);
    }
    // The sample was cut short: the value statistics don't cover every
    // non-NULL value. (`row_count` and `null_count` stay exact.)
    if sampled >= sample_size && rows - nulls > sampled {
        result["sample_size"] = json!(sampled);
    }
    Ok(result)
}

pub async fn get_column_stats(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    require_database(&config)?;
    let request = StatsRequest::from_params(params)?;
    let timeout = ctx.settings().query_timeout_secs;
    let script = build_script(&request, SAMPLE_SIZE, timeout);
    // Each statement is stopped by the server at the timeout, so the script
    // can take that long per statement. The local bound only catches a
    // server that does not answer, and leaves a second for the server's own
    // timeout error to arrive first.
    let work = run(ctx, &config, &script, false, None);
    // The script has no BEGIN/COMMIT/CANCEL/OPTION: every version answers
    // each statement.
    let (outcomes, _) = match timeout {
        None => work.await?,
        Some(secs) => {
            let statements = split_statements(&script).len() as u64;
            let bound = secs.saturating_mul(statements).saturating_add(1);
            tokio::time::timeout(Duration::from_secs(bound), work)
                .await
                .map_err(|_| stats_timeout_error(bound))??
        }
    };
    let values = outcomes
        .into_iter()
        .map(|timed| match timed.outcome {
            StatementOutcome::Ok(v) => Ok(v),
            StatementOutcome::Err(m) => Err(PluginError::internal(m)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    build_result(&request, &values, SAMPLE_SIZE)
}

fn stats_timeout_error(bound: u64) -> PluginError {
    PluginError::internal(format!(
        "SurrealDB did not answer the column statistics within {bound}s. Every statement of it carries the plugin's query timeout, so the server stops them itself; check that the server is responsive."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::values::tags;

    fn request(filter: Option<&str>) -> StatsRequest {
        StatsRequest {
            table: "item".into(),
            column: "n".into(),
            filter: filter.map(str::to_string),
            top_values_limit: 10,
            distinct: true,
            min_max: true,
            top_values: true,
        }
    }

    fn map(entries: &[(&str, Cbor)]) -> Cbor {
        Cbor::Map(
            entries
                .iter()
                .map(|(k, v)| (Cbor::Text((*k).into()), v.clone()))
                .collect(),
        )
    }

    fn int(n: i64) -> Cbor {
        Cbor::Integer(n.into())
    }

    fn none() -> Cbor {
        Cbor::Tag(tags::NONE, Box::new(Cbor::Null))
    }

    #[test]
    fn reads_the_host_request() {
        let params = json!({ "params": {}, "request": {
            "table": "item", "schema": null, "column": "n", "data_type": "int",
            "filter": "  n > 2 ", "top_values_limit": 10,
            "include": { "distinct": true, "min_max": false, "top_values": true }
        }});
        assert_eq!(
            StatsRequest::from_params(&params).unwrap(),
            StatsRequest {
                min_max: false,
                filter: Some("n > 2".into()),
                ..request(None)
            }
        );
        let blank = json!({ "request": { "table": "t", "column": "c", "filter": "  ",
            "top_values_limit": 500, "include": { "top_values": true } } });
        let r = StatsRequest::from_params(&blank).unwrap();
        assert_eq!(r.filter, None);
        assert_eq!(r.top_values_limit, MAX_TOP_VALUES);
        assert!(!r.distinct && !r.min_max && r.top_values);
    }

    #[test]
    fn rejects_missing_names_and_filters_of_the_wrong_shape() {
        for request in [
            json!({ "column": "c" }),
            json!({ "table": "t" }),
            json!({ "table": "", "column": "c" }),
        ] {
            assert!(StatsRequest::from_params(&json!({ "request": request })).is_err());
        }
        assert!(StatsRequest::from_params(&json!({})).is_err());
        for filter in ["1; DELETE item", "1) OR (1", "s = 'open"] {
            let e = StatsRequest::from_params(
                &json!({ "request": { "table": "t", "column": "c", "filter": filter } }),
            )
            .unwrap_err();
            assert!(e.message.contains("single SurrealQL condition"), "{filter}");
        }
        let regex =
            json!({ "request": { "table": "t", "column": "c", "filter": "name = /it's/" } });
        assert!(StatsRequest::from_params(&regex).is_ok());
    }

    #[test]
    fn script_with_filter_and_every_part() {
        assert_eq!(
            build_script(&request(Some("n > 2")), 1000, None),
            "SELECT count() AS rows, count((`n` = NONE) OR (`n` = NULL)) AS nulls FROM `item` WHERE (n > 2\n) GROUP ALL;\n\
             LET $values = (SELECT `n` AS v FROM `item` WHERE (n > 2\n) AND `n` != NONE AND `n` != NULL LIMIT 1000);\n\
             RETURN { sampled: array::len($values), distinct: array::len(array::distinct($values.v)), min: array::min($values.v), max: array::max($values.v) };\n\
             SELECT * FROM (SELECT v, count() AS count FROM $values GROUP BY v) ORDER BY count DESC, v ASC LIMIT 10;"
        );
    }

    #[test]
    fn every_select_carries_the_query_timeout() {
        let script = build_script(&request(None), 1000, Some(30));
        assert_eq!(script.matches(" TIMEOUT 30s").count(), 3, "{script}");
        assert!(script.contains("GROUP ALL TIMEOUT 30s;"));
        assert!(script.contains("LIMIT 1000 TIMEOUT 30s);"));
        assert!(script.contains("v ASC LIMIT 10 TIMEOUT 30s;"));
        assert!(stats_timeout_error(121).message.contains("121s"));
    }

    #[test]
    fn script_for_counts_only_and_odd_names() {
        let counts_only = StatsRequest {
            table: "odd table".into(),
            column: "a`b".into(),
            distinct: false,
            min_max: false,
            top_values: false,
            ..request(None)
        };
        assert_eq!(
            build_script(&counts_only, 1000, None),
            "SELECT count() AS rows, count((`a\\`b` = NONE) OR (`a\\`b` = NULL)) AS nulls FROM `odd table` GROUP ALL;"
        );
        let masked = StatsRequest {
            min_max: false,
            top_values: false,
            ..request(None)
        };
        let script = build_script(&masked, 1000, None);
        assert!(script.contains("RETURN { sampled: array::len($values), distinct:"));
        assert!(!script.contains("array::min") && !script.contains("GROUP BY v"));
    }

    #[test]
    fn builds_the_result_from_the_statement_values() {
        let values = [
            Cbor::Array(vec![map(&[("rows", int(5)), ("nulls", int(1))])]),
            none(),
            map(&[
                ("sampled", int(4)),
                ("distinct", int(3)),
                ("min", Cbor::Float(1.5)),
                ("max", int(9)),
            ]),
            Cbor::Array(vec![
                map(&[("v", int(5)), ("count", int(2))]),
                map(&[("v", Cbor::Float(1.5)), ("count", int(1))]),
            ]),
        ];
        assert_eq!(
            build_result(&request(None), &values, 1000).unwrap(),
            json!({
                "row_count": 5, "null_count": 1, "distinct_count": 3,
                "min": 1.5, "max": 9,
                "top_values": [{ "value": 5, "count": 2 }, { "value": 1.5, "count": 1 }]
            })
        );
    }

    #[test]
    fn reports_a_cut_sample_and_handles_empty_tables() {
        let values = [
            Cbor::Array(vec![map(&[("rows", int(10)), ("nulls", int(2))])]),
            none(),
            map(&[("sampled", int(4)), ("distinct", int(4))]),
            Cbor::Array(vec![]),
        ];
        let r = request(None);
        let result = build_result(&r, &values, 4).unwrap();
        assert_eq!(result["sample_size"], 4);
        assert_eq!(result["top_values"], json!([]));
        // Not cut: every non-NULL value was read.
        assert!(build_result(&r, &values, 8)
            .unwrap()
            .get("sample_size")
            .is_none());

        // 2.x returns no row for a GROUP ALL over no rows, 3.x a zero row.
        for counts in [
            Cbor::Array(vec![]),
            Cbor::Array(vec![map(&[("rows", int(0))])]),
        ] {
            let values = [
                counts,
                none(),
                map(&[
                    ("sampled", int(0)),
                    ("distinct", int(0)),
                    ("min", none()),
                    ("max", none()),
                ]),
                Cbor::Array(vec![]),
            ];
            let result = build_result(&r, &values, 1000).unwrap();
            assert_eq!(result["row_count"], 0);
            assert_eq!(result["null_count"], 0);
            assert_eq!(result["min"], Json::Null);
        }
    }

    #[test]
    fn counts_only_results_leave_the_value_parts_null() {
        let counts_only = StatsRequest {
            distinct: false,
            min_max: false,
            top_values: false,
            ..request(None)
        };
        let values = [Cbor::Array(vec![map(&[
            ("rows", int(3)),
            ("nulls", int(3)),
        ])])];
        assert_eq!(
            build_result(&counts_only, &values, 1000).unwrap(),
            json!({ "row_count": 3, "null_count": 3, "distinct_count": null,
                    "min": null, "max": null, "top_values": null })
        );
        assert!(build_result(&request(None), &values, 1000).is_err());
    }
}
