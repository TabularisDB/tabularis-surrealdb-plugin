//! Mapping statement results onto Tabularis' `QueryResult`
//! (`columns`, `rows`, `affected_rows`, `truncated`, `pagination`,
//! `additional_results`).
//!
//! | Statement result            | Columns / rows                                  |
//! |-----------------------------|-------------------------------------------------|
//! | array of objects (records)  | union of keys, `id` first, then first-seen order; a missing key is `null` |
//! | array with any non-object   | one `result` column, one row per element        |
//! | single object               | its keys, one row                               |
//! | scalar or NULL              | one `result` column, one row                    |
//! | NONE (e.g. `LET`, `DEFINE`) | no columns, no rows                             |
//!
//! Every statement of a multi-statement query gets its own result set, in
//! order: the first one is the primary result, the rest go to
//! `additional_results`, which Tabularis shows as separate result tabs.

use ciborium::value::Value as Cbor;
use serde_json::{json, Value as Json};

use crate::utils::values::{is_none, key_to_string, to_cell};

pub const SCALAR_COLUMN: &str = "result";

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Json>>,
}

/// Turn one statement result into columns and rows.
pub fn tabulate(value: &Cbor) -> Table {
    match value {
        v if is_none(v) => Table {
            columns: vec![],
            rows: vec![],
        },
        Cbor::Array(items) => tabulate_items(items),
        Cbor::Map(entries) => tabulate_records(&[entries.as_slice()]),
        scalar => Table {
            columns: vec![SCALAR_COLUMN.to_string()],
            rows: vec![vec![to_cell(scalar)]],
        },
    }
}

/// Rows for an array result.
pub fn tabulate_items(items: &[Cbor]) -> Table {
    if items.is_empty() {
        return Table {
            columns: vec![],
            rows: vec![],
        };
    }
    let records: Option<Vec<&[(Cbor, Cbor)]>> = items
        .iter()
        .map(|item| match item {
            Cbor::Map(entries) => Some(entries.as_slice()),
            _ => None,
        })
        .collect();
    match records {
        Some(records) => tabulate_records(&records),
        None => Table {
            columns: vec![SCALAR_COLUMN.to_string()],
            rows: items.iter().map(|item| vec![to_cell(item)]).collect(),
        },
    }
}

fn tabulate_records(records: &[&[(Cbor, Cbor)]]) -> Table {
    let mut columns: Vec<String> = Vec::new();
    let mut has_id = false;
    for record in records {
        for (key, value) in record.iter() {
            if is_none(value) {
                continue;
            }
            let key = key_to_string(key);
            if key == "id" {
                has_id = true;
            } else if !columns.contains(&key) {
                columns.push(key);
            }
        }
    }
    if has_id {
        columns.insert(0, "id".to_string());
    }
    let rows = records
        .iter()
        .map(|record| {
            let mut row = vec![Json::Null; columns.len()];
            for (key, value) in record.iter() {
                if is_none(value) {
                    continue;
                }
                let key = key_to_string(key);
                if let Some(idx) = columns.iter().position(|c| *c == key) {
                    row[idx] = to_cell(value);
                }
            }
            row
        })
        .collect();
    Table { columns, rows }
}

/// Serialise into the host's `QueryResult` shape.
pub fn query_result(table: Table, pagination: Option<Json>) -> Json {
    query_result_with(table, pagination, 0, None)
}

/// [`query_result`] with an affected-row count and the server's execution
/// time for the statement.
pub fn query_result_with(
    table: Table,
    pagination: Option<Json>,
    affected_rows: u64,
    server_time_ms: Option<f64>,
) -> Json {
    let mut out = json!({
        "columns": table.columns,
        "rows": table.rows,
        "affected_rows": affected_rows,
        "truncated": false,
        "pagination": pagination,
    });
    if let Some(ms) = server_time_ms {
        out["server_time_ms"] = json!(ms);
    }
    out
}

pub fn pagination(page: u64, page_size: u64, has_more: bool) -> Json {
    json!({
        "page": page,
        "page_size": page_size,
        "total_rows": Json::Null,
        "has_more": has_more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::values::tags;
    use serde_json::json;

    fn text(s: &str) -> Cbor {
        Cbor::Text(s.into())
    }
    fn obj(entries: Vec<(&str, Cbor)>) -> Cbor {
        Cbor::Map(entries.into_iter().map(|(k, v)| (text(k), v)).collect())
    }
    fn rid(t: &str, id: &str) -> Cbor {
        Cbor::Tag(
            tags::RECORD_ID,
            Box::new(Cbor::Array(vec![text(t), text(id)])),
        )
    }
    fn none() -> Cbor {
        Cbor::Tag(tags::NONE, Box::new(Cbor::Null))
    }

    #[test]
    fn heterogeneous_records_keep_every_field() {
        let v = Cbor::Array(vec![
            obj(vec![
                ("age", Cbor::Integer(30.into())),
                ("id", rid("p", "a")),
                ("name", text("A")),
            ]),
            obj(vec![
                ("id", rid("p", "b")),
                ("nick", text("B")),
                ("name", Cbor::Null),
            ]),
            obj(vec![("id", rid("p", "c")), ("age", none())]),
        ]);
        let t = tabulate(&v);
        assert_eq!(t.columns, vec!["id", "age", "name", "nick"]);
        assert_eq!(
            t.rows,
            vec![
                vec![json!("p:a"), json!(30), json!("A"), Json::Null],
                vec![json!("p:b"), Json::Null, Json::Null, json!("B")],
                vec![json!("p:c"), Json::Null, Json::Null, Json::Null],
            ]
        );
    }

    #[test]
    fn mixed_arrays_use_a_single_result_column() {
        let v = Cbor::Array(vec![
            Cbor::Integer(1.into()),
            obj(vec![("a", text("x"))]),
            Cbor::Null,
        ]);
        let t = tabulate(&v);
        assert_eq!(t.columns, vec![SCALAR_COLUMN]);
        assert_eq!(
            t.rows,
            vec![vec![json!(1)], vec![json!({"a": "x"})], vec![Json::Null]]
        );
    }

    #[test]
    fn graph_traversal_result() {
        // SELECT ->knows->person.name AS friends FROM person:alice
        let v = Cbor::Array(vec![obj(vec![("friends", Cbor::Array(vec![text("Bob")]))])]);
        let t = tabulate(&v);
        assert_eq!(t.columns, vec!["friends"]);
        assert_eq!(t.rows, vec![vec![json!(["Bob"])]]);
    }

    #[test]
    fn objects_scalars_and_none() {
        let info = obj(vec![(
            "tables",
            obj(vec![("person", text("DEFINE TABLE person"))]),
        )]);
        let t = tabulate(&info);
        assert_eq!(t.columns, vec!["tables"]);
        assert_eq!(t.rows, vec![vec![json!({"person": "DEFINE TABLE person"})]]);

        let t = tabulate(&Cbor::Integer(1.into()));
        assert_eq!(
            (t.columns, t.rows),
            (vec![SCALAR_COLUMN.to_string()], vec![vec![json!(1)]])
        );

        let t = tabulate(&Cbor::Null);
        assert_eq!(t.rows, vec![vec![Json::Null]]);

        let t = tabulate(&none());
        assert!(t.columns.is_empty() && t.rows.is_empty());

        let t = tabulate(&Cbor::Array(vec![]));
        assert!(t.columns.is_empty() && t.rows.is_empty());
    }

    #[test]
    fn serialises_the_host_shape() {
        let r = query_result(
            Table {
                columns: vec!["result".into()],
                rows: vec![vec![json!(1)]],
            },
            Some(pagination(2, 50, true)),
        );
        assert_eq!(
            r,
            json!({
                "columns": ["result"], "rows": [[1]], "affected_rows": 0, "truncated": false,
                "pagination": {"page": 2, "page_size": 50, "total_rows": null, "has_more": true}
            })
        );
    }
}
