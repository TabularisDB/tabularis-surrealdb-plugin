//! `get_table_query_template`: SurrealQL previews for the Generate SQL
//! dialog. Never executes anything.

use serde_json::Value;

use crate::error::PluginError;
use crate::utils::identifiers::quote_identifier;

pub fn get_table_query_template(params: &Value) -> Result<Value, PluginError> {
    let request = params
        .get("request")
        .ok_or_else(|| PluginError::invalid_params("missing `request`"))?;
    let table = request
        .get("table")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| PluginError::invalid_params("missing `request.table`"))?;
    let kind = request
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::invalid_params("missing `request.kind`"))?;
    let columns: Vec<String> = request
        .get("columns")
        .and_then(Value::as_array)
        .map(|cols| {
            cols.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let limit = match request.get("limit") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| PluginError::invalid_params("`request.limit` must be a u32"))?,
        ),
    };
    template(kind, table, &columns, limit).map(Value::String)
}

/// Build the template. Table and column names are unquoted identifiers and
/// are always quoted here.
pub fn template(
    kind: &str,
    table: &str,
    columns: &[String],
    limit: Option<u32>,
) -> Result<String, PluginError> {
    let table = quote_identifier(table);
    match kind {
        "select" => {
            let fields = if columns.is_empty() {
                "*".to_string()
            } else {
                columns
                    .iter()
                    .map(|c| quote_identifier(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            Ok(match limit {
                Some(n) => format!("SELECT {fields} FROM {table} LIMIT {n};"),
                None => format!("SELECT {fields} FROM {table};"),
            })
        }
        "update" | "delete" if limit.is_some() => Err(PluginError::invalid_params(format!(
            "`limit` is not accepted for {kind} templates"
        ))),
        "update" => {
            let assignments = if columns.is_empty() {
                "`field` = :value_1".to_string()
            } else {
                columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{} = :value_{}", quote_identifier(c), i + 1))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            Ok(format!("UPDATE {table} SET {assignments} WHERE 1 = 0;"))
        }
        "delete" => Ok(format!("DELETE FROM {table} WHERE 1 = 0;")),
        other => Err(PluginError::invalid_params(format!(
            "unknown template kind `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn select_templates() {
        assert_eq!(
            template("select", "person", &[], None).unwrap(),
            "SELECT * FROM `person`;"
        );
        assert_eq!(
            template(
                "select",
                "odd table",
                &["id".into(), "full name".into()],
                Some(100)
            )
            .unwrap(),
            "SELECT `id`, `full name` FROM `odd table` LIMIT 100;"
        );
        assert_eq!(
            template("select", "t", &[], Some(0)).unwrap(),
            "SELECT * FROM `t` LIMIT 0;"
        );
    }

    #[test]
    fn write_templates_never_match_rows() {
        assert_eq!(
            template("update", "person", &["name".into(), "age".into()], None).unwrap(),
            "UPDATE `person` SET `name` = :value_1, `age` = :value_2 WHERE 1 = 0;"
        );
        assert_eq!(
            template("update", "person", &[], None).unwrap(),
            "UPDATE `person` SET `field` = :value_1 WHERE 1 = 0;"
        );
        assert_eq!(
            template("delete", "a`b", &[], None).unwrap(),
            "DELETE FROM `a\\`b` WHERE 1 = 0;"
        );
        assert!(template("delete", "t", &[], Some(1)).is_err());
        assert!(template("merge", "t", &[], None).is_err());
    }

    #[test]
    fn reads_the_rpc_request() {
        let params = json!({
            "params": {}, "request": { "table": "person", "schema": null, "kind": "select", "columns": [], "limit": 100 }
        });
        assert_eq!(
            get_table_query_template(&params).unwrap(),
            json!("SELECT * FROM `person` LIMIT 100;")
        );
        assert!(get_table_query_template(&json!({ "request": { "kind": "select" } })).is_err());
    }
}
