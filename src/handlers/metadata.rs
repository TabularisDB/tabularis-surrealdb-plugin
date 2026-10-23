//! Discovery: databases, tables, fields (columns) and indexes.
//!
//! Tables come from `INFO FOR DB`, fields and indexes from
//! `INFO FOR TABLE`. SCHEMAFULL tables are described by their declared
//! fields only. SCHEMALESS tables additionally get the union of the
//! top-level keys of their first [`SAMPLE_SIZE`] records (in the server's
//! natural record order), so fields that exist only in data still show up as
//! columns. Sampled fields are marked as such in the column comment.

use std::collections::BTreeMap;

use ciborium::value::Value as Cbor;
use serde_json::{json, Map, Value as Json};

use crate::error::PluginError;
use crate::handlers::{require_database, session_config, table_param, Context};
use crate::models::SessionConfig;
use crate::surreal::session::{Session, StatementOutcome};
use crate::surreal::{retry_class, Retry};
use crate::utils::define::{parse_field, parse_index, parse_table, TableDef};
use crate::utils::identifiers::quote_identifier;
use crate::utils::values::{is_none, key_to_string, kind_name};

/// Records read from a SCHEMALESS table to infer its fields.
pub const SAMPLE_SIZE: u64 = 100;

/// Run SurrealQL whose statements must all succeed.
pub async fn query_ok(
    session: &Session,
    sql: &str,
    vars: Vec<(&str, Cbor)>,
) -> Result<Vec<Cbor>, Retry<PluginError>> {
    let outcomes = session
        .query(sql, vars)
        .await
        .map_err(|e| retry_class(session, e))?;
    outcomes
        .into_iter()
        .map(|o| match o {
            StatementOutcome::Ok(v) => Ok(v),
            StatementOutcome::Err(m) => Err(Retry::Final(PluginError::internal(session.hint(&m)))),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Databases
// ---------------------------------------------------------------------------

/// `namespace/database` pairs the signed-in user can see. Root users get
/// every namespace (or only the configured one); namespace users get the
/// databases of their namespace; database users get their database.
///
/// Listing needs `USE` between namespaces, so it runs on a short-lived
/// session of its own and never changes the connection's shared session.
pub async fn get_databases(_ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    let session = Session::open(&SessionConfig {
        connection_id: None,
        ..config.clone()
    })
    .await?;

    let namespaces: Vec<String> = match &config.namespace {
        Some(ns) => vec![ns.clone()],
        None => {
            let root = session.query_all_ok("INFO FOR ROOT", vec![]).await?;
            definition_names(root.first(), "namespaces")
        }
    };

    let mut out = Vec::new();
    for ns in namespaces {
        let sql = format!("USE NS {}; INFO FOR NS;", quote_identifier(&ns));
        match session.query_all_ok(&sql, vec![]).await {
            Ok(results) => {
                for db in definition_names(results.get(1), "databases") {
                    out.push(format!("{ns}/{db}"));
                }
            }
            // A database user cannot list its namespace; offer its database.
            Err(_) if config.namespace.is_some() && config.database.is_some() => {
                out.push(config.target());
            }
            Err(e) => return Err(e),
        }
    }
    out.sort();
    out.dedup();
    Ok(json!(out))
}

/// Names (keys) of an `INFO FOR …` definition map such as `tables`.
pub fn definition_names(info: Option<&Cbor>, key: &str) -> Vec<String> {
    definitions(info, key)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// `(name, DEFINE statement)` pairs of an `INFO FOR …` definition map, in
/// server order.
pub fn definitions(info: Option<&Cbor>, key: &str) -> Vec<(String, String)> {
    let Some(Cbor::Map(entries)) = info else {
        return Vec::new();
    };
    let Some((_, Cbor::Map(defs))) = entries.iter().find(|(k, _)| k.as_text() == Some(key)) else {
        return Vec::new();
    };
    defs.iter()
        .map(|(name, def)| {
            let def = match def {
                Cbor::Text(t) => t.clone(),
                other => crate::utils::values::to_cell(other).to_string(),
            };
            (key_to_string(name), def)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

pub async fn get_tables(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    require_database(&config)?;
    let tables = ctx
        .sessions
        .with_session(&config, |session| async move {
            let info = query_ok(&session, "INFO FOR DB", vec![]).await?;
            Ok(definitions(info.first(), "tables"))
        })
        .await?;
    Ok(Json::Array(
        tables
            .into_iter()
            .map(|(name, def)| table_info(&name, &parse_table(&def)))
            .collect(),
    ))
}

fn table_info(name: &str, def: &TableDef) -> Json {
    let mut notes = Vec::new();
    if let Some(c) = &def.comment {
        notes.push(c.clone());
    }
    notes.push(
        match (def.schemafull, def.relation, def.view) {
            (_, _, true) => "table view",
            (true, true, _) => "SCHEMAFULL relation",
            (false, true, _) => "SCHEMALESS relation",
            (true, false, _) => "SCHEMAFULL",
            (false, false, _) => "SCHEMALESS",
        }
        .to_string(),
    );
    json!({ "name": name, "comment": notes.join(" · ") })
}

// ---------------------------------------------------------------------------
// Columns
// ---------------------------------------------------------------------------

/// Everything known about one table.
struct TableMeta {
    def: TableDef,
    fields: Vec<(String, String)>,
    indexes: Vec<(String, String)>,
    sample: Option<Vec<Cbor>>,
}

async fn table_meta(
    ctx: &Context,
    config: &SessionConfig,
    table: &str,
    with_sample: bool,
) -> Result<TableMeta, PluginError> {
    ctx.sessions
        .with_session(config, |session| {
            let table = table.to_string();
            async move {
                let sql = format!("INFO FOR DB; INFO FOR TABLE {};", quote_identifier(&table));
                let info = query_ok(&session, &sql, vec![]).await?;
                let Some((_, def)) = definitions(info.first(), "tables")
                    .into_iter()
                    .find(|(name, _)| *name == table)
                else {
                    return Err(Retry::Final(PluginError::invalid_params(format!(
                        "Table `{table}` does not exist in {}",
                        session.config.target()
                    ))));
                };
                let def = parse_table(&def);
                let sample = if with_sample && !def.schemafull {
                    Some(sample_records(&session, &table).await?)
                } else {
                    None
                };
                Ok(TableMeta {
                    def,
                    fields: definitions(info.get(1), "fields"),
                    indexes: definitions(info.get(1), "indexes"),
                    sample,
                })
            }
        })
        .await
}

async fn sample_records(session: &Session, table: &str) -> Result<Vec<Cbor>, Retry<PluginError>> {
    let sql = format!("SELECT * FROM type::table($table) LIMIT {SAMPLE_SIZE}");
    let result = query_ok(
        session,
        &sql,
        vec![("table", Cbor::Text(table.to_string()))],
    )
    .await?;
    Ok(match result.into_iter().next() {
        Some(Cbor::Array(items)) => items,
        _ => Vec::new(),
    })
}

pub async fn get_columns(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    require_database(&config)?;
    let table = table_param(params)?;
    let meta = table_meta(ctx, &config, table, true).await?;
    Ok(Json::Array(build_columns(
        table,
        &meta.def,
        &meta.fields,
        meta.sample.as_deref(),
    )))
}

/// Columns for all tables at once (used by the host for autocomplete). Takes
/// the same inputs as `get_columns`, per table.
pub async fn get_all_columns_batch(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    require_database(&config)?;
    let tables = ctx
        .sessions
        .with_session(&config, |session| async move {
            let info = query_ok(&session, "INFO FOR DB", vec![]).await?;
            Ok(definitions(info.first(), "tables"))
        })
        .await?;
    let mut out = Map::new();
    for (name, _) in tables {
        let meta = table_meta(ctx, &config, &name, true).await?;
        let columns = build_columns(&name, &meta.def, &meta.fields, meta.sample.as_deref());
        out.insert(name, Json::Array(columns));
    }
    Ok(Json::Object(out))
}

/// Record links are not reported as foreign keys yet; every table maps to
/// an empty list so the host's ER and autocomplete paths keep working.
pub async fn get_all_foreign_keys_batch(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    require_database(&config)?;
    let tables = ctx
        .sessions
        .with_session(&config, |session| async move {
            let info = query_ok(&session, "INFO FOR DB", vec![]).await?;
            Ok(definition_names(info.first(), "tables"))
        })
        .await?;
    let out: Map<String, Json> = tables.into_iter().map(|t| (t, json!([]))).collect();
    Ok(Json::Object(out))
}

/// Is a field path top level (`name`, not `address.city` or `tags[*]`)?
fn is_top_level(field: &str) -> bool {
    !field.contains(['.', '[', '*'])
}

/// May the field be absent or NULL, judging from its declared type?
fn nullable_type(kind: Option<&str>) -> bool {
    let Some(kind) = kind else {
        return true; // no TYPE clause: any value
    };
    let k = kind.trim().to_ascii_lowercase();
    k == "any"
        || k.starts_with("option<")
        || k.split('|')
            .any(|part| matches!(part.trim(), "none" | "null" | "any"))
}

/// Build `get_columns` entries.
///
/// Order: `id` first; then declared top-level fields in server order; then,
/// for SCHEMALESS tables, sampled keys that are not declared, in first-seen
/// order.
pub fn build_columns(
    table: &str,
    table_def: &TableDef,
    fields: &[(String, String)],
    sample: Option<&[Cbor]>,
) -> Vec<Json> {
    let declared: Vec<(String, crate::utils::define::FieldDef)> = fields
        .iter()
        .filter(|(name, _)| is_top_level(name))
        .map(|(name, def)| (name.clone(), parse_field(def)))
        .collect();

    let mut columns = Vec::new();

    let id_kind = declared
        .iter()
        .find(|(name, _)| name == "id")
        .and_then(|(_, f)| f.kind.clone());
    columns.push(column(
        "id",
        &id_kind.unwrap_or_else(|| format!("record<{table}>")),
        true,
        false,
        false,
        None,
        Some("Record id (table:id)".to_string()),
    ));

    for (name, field) in declared.iter().filter(|(name, _)| name != "id") {
        columns.push(column(
            name,
            field.kind.as_deref().unwrap_or("any"),
            false,
            nullable_type(field.kind.as_deref()),
            field.computed,
            field.default.clone(),
            field.comment.clone(),
        ));
    }

    let mut known: Vec<String> = columns
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();

    if table_def.relation {
        for edge in ["in", "out"] {
            if !known.iter().any(|k| k == edge) {
                columns.push(column(edge, "record", false, false, false, None, None));
                known.push(edge.to_string());
            }
        }
    }

    if let Some(records) = sample {
        // key → (type names in first-seen order); BTreeMap only for lookup,
        // order is kept in `order`.
        let mut order: Vec<String> = Vec::new();
        let mut kinds: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
        for record in records {
            let Cbor::Map(entries) = record else { continue };
            for (key, value) in entries {
                let key = key_to_string(key);
                if known.contains(&key) || is_none(value) {
                    continue;
                }
                let seen = kinds.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    Vec::new()
                });
                let kind = kind_name(value);
                if !seen.contains(&kind) {
                    seen.push(kind);
                }
            }
        }
        let note = format!(
            "Not declared; inferred from the first {} record(s) of this SCHEMALESS table",
            records.len()
        );
        for key in order {
            let kinds = &kinds[&key];
            let data_type = if kinds.is_empty() {
                "any".to_string()
            } else {
                kinds.join(" | ")
            };
            columns.push(column(
                &key,
                &data_type,
                false,
                true,
                false,
                None,
                Some(note.clone()),
            ));
        }
    }
    columns
}

fn column(
    name: &str,
    data_type: &str,
    is_pk: bool,
    is_nullable: bool,
    is_generated: bool,
    default_value: Option<String>,
    comment: Option<String>,
) -> Json {
    json!({
        "name": name,
        "data_type": data_type,
        "is_pk": is_pk,
        "is_nullable": is_nullable,
        "is_auto_increment": false,
        "is_generated": is_generated,
        "default_value": default_value,
        "comment": comment,
    })
}

// ---------------------------------------------------------------------------
// Indexes
// ---------------------------------------------------------------------------

pub async fn get_indexes(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    require_database(&config)?;
    let table = table_param(params)?;
    let meta = table_meta(ctx, &config, table, false).await?;
    Ok(Json::Array(build_indexes(&meta.indexes)))
}

/// One entry per indexed column, as the host expects.
pub fn build_indexes(indexes: &[(String, String)]) -> Vec<Json> {
    let mut out = Vec::new();
    for (name, def) in indexes {
        let index = parse_index(def);
        for (i, column) in index.columns.iter().enumerate() {
            out.push(json!({
                "name": name,
                "column_name": column,
                "is_unique": index.unique,
                "is_primary": false,
                "seq_in_index": i + 1,
                "is_expression": false,
            }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::values::tags;

    fn text(s: &str) -> Cbor {
        Cbor::Text(s.into())
    }
    fn obj(entries: Vec<(&str, Cbor)>) -> Cbor {
        Cbor::Map(entries.into_iter().map(|(k, v)| (text(k), v)).collect())
    }
    fn defs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }
    fn names(cols: &[Json]) -> Vec<&str> {
        cols.iter().map(|c| c["name"].as_str().unwrap()).collect()
    }

    #[test]
    fn reads_info_definition_maps() {
        let info = obj(vec![
            (
                "tables",
                obj(vec![("person", text("DEFINE TABLE person SCHEMAFULL"))]),
            ),
            ("users", obj(vec![])),
        ]);
        assert_eq!(
            definitions(Some(&info), "tables"),
            defs(&[("person", "DEFINE TABLE person SCHEMAFULL")])
        );
        assert!(definitions(Some(&info), "fields").is_empty());
        assert!(definitions(None, "tables").is_empty());
    }

    #[test]
    fn schemafull_columns_use_declared_fields_only() {
        let fields = defs(&[
            (
                "address",
                "DEFINE FIELD address ON person TYPE object PERMISSIONS FULL",
            ),
            (
                "address.city",
                "DEFINE FIELD address.city ON person TYPE string PERMISSIONS FULL",
            ),
            (
                "age",
                "DEFINE FIELD age ON person TYPE none | int PERMISSIONS FULL",
            ),
            (
                "name",
                "DEFINE FIELD name ON person TYPE string COMMENT 'Full name' PERMISSIONS FULL",
            ),
            (
                "tags",
                "DEFINE FIELD tags ON person TYPE array<string> DEFAULT [] PERMISSIONS FULL",
            ),
            (
                "tags.*",
                "DEFINE FIELD tags.* ON person TYPE string PERMISSIONS FULL",
            ),
        ]);
        let def = parse_table("DEFINE TABLE person TYPE NORMAL SCHEMAFULL PERMISSIONS NONE");
        let cols = build_columns("person", &def, &fields, None);
        assert_eq!(names(&cols), vec!["id", "address", "age", "name", "tags"]);
        assert_eq!(cols[0]["data_type"], "record<person>");
        assert_eq!(cols[0]["is_pk"], true);
        assert_eq!(cols[2]["data_type"], "none | int");
        assert_eq!(cols[2]["is_nullable"], true);
        assert_eq!(cols[3]["is_nullable"], false);
        assert_eq!(cols[3]["comment"], "Full name");
        assert_eq!(cols[4]["default_value"], "[]");
    }

    #[test]
    fn schemaless_columns_add_sampled_keys() {
        let def = parse_table("DEFINE TABLE event TYPE ANY SCHEMALESS PERMISSIONS NONE");
        let fields = defs(&[("kind", "DEFINE FIELD kind ON event TYPE string")]);
        let rid = Cbor::Tag(
            tags::RECORD_ID,
            Box::new(Cbor::Array(vec![text("event"), Cbor::Integer(1.into())])),
        );
        let sample = vec![
            obj(vec![
                ("id", rid.clone()),
                ("kind", text("x")),
                ("payload", obj(vec![])),
                ("n", Cbor::Null),
            ]),
            obj(vec![
                ("id", rid),
                ("payload", text("raw")),
                ("extra", Cbor::Integer(1.into())),
            ]),
        ];
        let cols = build_columns("event", &def, &fields, Some(&sample));
        assert_eq!(names(&cols), vec!["id", "kind", "payload", "n", "extra"]);
        assert_eq!(cols[2]["data_type"], "object | string");
        assert_eq!(cols[3]["data_type"], "null");
        assert_eq!(cols[2]["is_nullable"], true);
        assert!(cols[2]["comment"]
            .as_str()
            .unwrap()
            .contains("first 2 record(s)"));
    }

    #[test]
    fn relation_tables_always_show_in_and_out() {
        let def = parse_table("DEFINE TABLE knows TYPE RELATION IN person OUT person SCHEMALESS");
        let cols = build_columns("knows", &def, &[], Some(&[]));
        assert_eq!(names(&cols), vec!["id", "in", "out"]);
        let fields = defs(&[("in", "DEFINE FIELD in ON knows TYPE record<person>")]);
        let cols = build_columns("knows", &def, &fields, None);
        assert_eq!(names(&cols), vec!["id", "in", "out"]);
        assert_eq!(cols[1]["data_type"], "record<person>");
    }

    #[test]
    fn declared_id_type_wins() {
        let def = TableDef::default();
        let fields = defs(&[("id", "DEFINE FIELD id ON t TYPE int")]);
        let cols = build_columns("t", &def, &fields, None);
        assert_eq!(names(&cols), vec!["id"]);
        assert_eq!(cols[0]["data_type"], "int");
    }

    #[test]
    fn index_rows_per_column() {
        let idx = build_indexes(&defs(&[
            (
                "person_city_age",
                "DEFINE INDEX person_city_age ON person FIELDS address.city, age",
            ),
            (
                "person_name",
                "DEFINE INDEX person_name ON person FIELDS name UNIQUE",
            ),
        ]));
        assert_eq!(idx.len(), 3);
        assert_eq!(idx[0]["column_name"], "address.city");
        assert_eq!(idx[1]["seq_in_index"], 2);
        assert_eq!(idx[2]["is_unique"], true);
    }

    #[test]
    fn nullability_from_types() {
        assert!(nullable_type(None));
        assert!(nullable_type(Some("option<string>")));
        assert!(nullable_type(Some("none | int")));
        assert!(nullable_type(Some("string | null")));
        assert!(!nullable_type(Some("string")));
        assert!(!nullable_type(Some("record<person>")));
    }

    #[test]
    fn table_comments() {
        let t = table_info(
            "person",
            &parse_table("DEFINE TABLE person SCHEMAFULL COMMENT 'People'"),
        );
        assert_eq!(
            t,
            json!({ "name": "person", "comment": "People · SCHEMAFULL" })
        );
    }
}
