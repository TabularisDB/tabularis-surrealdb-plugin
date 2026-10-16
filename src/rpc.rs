//! JSON-RPC method routing and response helpers.

use serde_json::{json, Value};

use crate::error::PluginError;
use crate::handlers::{metadata, query, templates, Context, Settings};

const READ_ONLY: &str = "the SurrealDB plugin is read-only in this release";
const NO_DDL: &str = "schema changes are not supported by the SurrealDB plugin yet";

/// Run one method and return its result or error.
pub async fn dispatch(ctx: &Context, method: &str, params: &Value) -> Result<Value, PluginError> {
    match method {
        "initialize" => {
            let settings = Settings::from_initialize(params);
            *ctx.settings.write().unwrap_or_else(|e| e.into_inner()) = settings;
            Ok(Value::Null)
        }
        "ping" => query::ping(ctx, params).await,
        "test_connection" => query::test_connection(ctx, params).await,

        "get_databases" => metadata::get_databases(ctx, params).await,
        // `capabilities.schemas` is false: namespaces and databases are
        // listed together by `get_databases` as `namespace/database`.
        "get_schemas" => Ok(json!([])),
        "get_tables" => metadata::get_tables(ctx, params).await,
        "get_columns" => metadata::get_columns(ctx, params).await,
        "get_indexes" => metadata::get_indexes(ctx, params).await,
        "get_all_columns_batch" => metadata::get_all_columns_batch(ctx, params).await,
        "get_all_foreign_keys_batch" => metadata::get_all_foreign_keys_batch(ctx, params).await,
        // Record links and relations are not reported as foreign keys yet.
        "get_foreign_keys" => Ok(json!([])),
        // `capabilities.views` and `routines` are false.
        "get_views" | "get_routines" => Ok(json!([])),

        "execute_query" => query::execute_query(ctx, params).await,
        "execute_query_batch" => query::execute_query_batch(ctx, params).await,
        "get_table_query_template" => templates::get_table_query_template(params),

        "insert_record" | "update_record" | "delete_record" | "save_blob_to_file" => {
            Err(PluginError::unsupported(method, READ_ONLY))
        }
        "get_create_table_sql"
        | "get_add_column_sql"
        | "get_alter_column_sql"
        | "get_create_index_sql"
        | "get_create_foreign_key_sql"
        | "drop_index"
        | "drop_foreign_key"
        | "create_view"
        | "alter_view"
        | "drop_view" => Err(PluginError::unsupported(method, NO_DDL)),

        // Everything else, including get_schema_snapshot (the host composes
        // it from get_tables/get_columns/get_foreign_keys).
        other => Err(PluginError::method_not_found(other)),
    }
}

pub fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "result": result, "id": id })
}

pub fn error_response(id: Value, error: &PluginError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "error": { "code": error.code, "message": error.message },
        "id": id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::METHOD_NOT_FOUND;

    fn run(method: &str, params: Value) -> Result<Value, PluginError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ctx = Context::default();
        rt.block_on(dispatch(&ctx, method, &params))
    }

    #[test]
    fn writes_and_ddl_return_the_documented_unsupported_response() {
        for method in [
            "insert_record",
            "update_record",
            "delete_record",
            "get_create_table_sql",
            "drop_index",
            "create_view",
        ] {
            let e = run(method, json!({})).unwrap_err();
            assert_eq!(e.code, METHOD_NOT_FOUND, "{method}");
            assert!(e
                .message
                .starts_with(&format!("Method not found: {method}")));
        }
    }

    #[test]
    fn unknown_methods_are_method_not_found() {
        for method in ["get_schema_snapshot", "explain_query", "nope"] {
            let e = run(method, json!({})).unwrap_err();
            assert_eq!(e.code, METHOD_NOT_FOUND);
            assert_eq!(e.message, format!("Method not found: {method}"));
        }
    }

    #[test]
    fn methods_without_a_server_round_trip() {
        assert_eq!(
            run("initialize", json!({ "settings": {} })).unwrap(),
            Value::Null
        );
        let ctx = Context::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(dispatch(
            &ctx,
            "initialize",
            &json!({ "settings": { "query_timeout_seconds": 12 } }),
        ))
        .unwrap();
        assert_eq!(ctx.settings().query_timeout_secs, Some(12));
        assert_eq!(run("get_schemas", json!({})).unwrap(), json!([]));
        assert_eq!(
            run(
                "get_table_query_template",
                json!({ "request": { "table": "t", "kind": "delete" } })
            )
            .unwrap(),
            json!("DELETE FROM `t` WHERE 1 = 0;")
        );
    }

    #[test]
    fn metadata_without_a_database_is_an_actionable_error() {
        let e = run("get_tables", json!({ "params": { "host": "localhost" } })).unwrap_err();
        assert!(e.message.contains("namespace/database"));
    }
}
