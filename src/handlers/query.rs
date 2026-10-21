//! Connection checks and SurrealQL execution.

use serde_json::{json, Value as Json};

use crate::error::PluginError;
use crate::handlers::results::{pagination, query_result, tabulate, tabulate_items, Table};
use crate::handlers::{req_str, session_config, Context};
use crate::models::SessionConfig;
use crate::surreal::session::{Session, StatementOutcome};
use crate::surreal::{retry_class, Retry};
use crate::utils::pagination::{offset_for, slice_page};
use crate::utils::surql::{
    can_push_down_pagination, normalize_host_browse_query, split_statements, with_pagination,
};
use ciborium::value::Value as Cbor;

/// Open a fresh session (never a cached one, so the result reflects the
/// current server and credentials), sign in, select the namespace/database
/// and check that it can be read. A namespace or database that does not
/// exist fails in [`Session::open`], before anything is selected, so the
/// test never creates it.
pub async fn test_connection(_ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = SessionConfig {
        connection_id: None,
        ..session_config(params)?
    };
    let session = Session::open(&config).await?;
    let version = session.version().await?;
    let check = match (&config.namespace, &config.database) {
        (Some(_), Some(_)) => Some("INFO FOR DB"),
        (Some(_), None) => Some("INFO FOR NS"),
        _ => None,
    };
    if let Some(sql) = check {
        session.query_all_ok(sql, vec![]).await?;
    }
    Ok(json!({
        "success": true,
        "server_version": version,
        "signed_in_as": session.signed_in_as.label(),
    }))
}

pub async fn ping(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    ctx.sessions
        .with_session(&config, |session| async move {
            session.ping().await.map_err(Retry::Reconnect)
        })
        .await?;
    Ok(Json::Null)
}

/// How the page requested by the host is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageMode {
    /// `LIMIT size+1 START offset` was appended; the extra row tells whether
    /// another page exists.
    PushDown { page: u64, size: u64 },
    /// A SELECT that already paginates (or cannot take a clause): run as is
    /// and cut the page out of the result.
    Slice { page: u64, size: u64 },
    /// Not a single SELECT: run once, return everything, no pagination.
    None,
}

/// What `execute_query` sends to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub sql: String,
    pub mode: PageMode,
    /// The query changes namespace/database with `USE`.
    pub uses_use: bool,
    pub statement_count: usize,
}

/// Decide how to run `query` for page `page` of `limit` rows.
///
/// Only single SELECT statements are paginated: the host re-runs the query
/// for every page, which must never re-apply a write.
pub fn plan(query: &str, limit: Option<u64>, page: u64) -> Result<Plan, PluginError> {
    let normalized = normalize_host_browse_query(query);
    let statements = split_statements(&normalized);
    if statements.is_empty() {
        return Err(PluginError::invalid_params("The query is empty."));
    }
    let uses_use = statements.iter().any(|s| s.first_keyword() == Some("USE"));
    let statement_count = statements.len();
    let single_select = statement_count == 1 && statements[0].first_keyword() == Some("SELECT");
    let (sql, mode) = match (single_select, limit) {
        (true, Some(size)) if can_push_down_pagination(&statements[0]) => {
            let offset = offset_for(page, size);
            (
                with_pagination(statements[0].text, size.saturating_add(1), offset),
                PageMode::PushDown { page, size },
            )
        }
        (true, Some(size)) => (
            statements[0].text.to_string(),
            PageMode::Slice { page, size },
        ),
        _ => (normalized.clone(), PageMode::None),
    };
    Ok(Plan {
        sql,
        mode,
        uses_use,
        statement_count,
    })
}

pub async fn execute_query(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    let query = req_str(params, "query")?;
    let limit = params
        .get("limit")
        .or_else(|| params.get("page_size"))
        .and_then(Json::as_u64)
        .filter(|l| *l > 0);
    let page = params.get("page").and_then(Json::as_u64).unwrap_or(1);
    let plan = plan(query, limit, page)?;

    let outcomes = if plan.uses_use || config.connection_id.is_none() {
        // `USE` would switch the shared session's namespace/database under
        // concurrent requests; give such queries a session of their own.
        let session = Session::open(&SessionConfig {
            connection_id: None,
            ..config.clone()
        })
        .await?;
        let outcomes = session.query_checked(&plan.sql, vec![]).await?;
        with_hints(&session, outcomes)
    } else {
        let sql = plan.sql.clone();
        ctx.sessions
            .with_session(&config, |session| {
                let sql = sql.clone();
                async move {
                    let outcomes = session
                        .query(&sql, vec![])
                        .await
                        .map_err(|e| retry_class(&session, e))?;
                    Ok(with_hints(&session, outcomes))
                }
            })
            .await?
    };
    build_response(outcomes, plan.mode)
}

/// Add hints (which need the signed-in level) to statement errors.
fn with_hints(session: &Session, outcomes: Vec<StatementOutcome>) -> Vec<StatementOutcome> {
    outcomes
        .into_iter()
        .map(|o| match o {
            StatementOutcome::Err(m) => StatementOutcome::Err(session.hint(&m)),
            ok => ok,
        })
        .collect()
}

/// Turn statement outcomes into the host's `QueryResult`.
pub fn build_response(
    outcomes: Vec<StatementOutcome>,
    mode: PageMode,
) -> Result<Json, PluginError> {
    let total = outcomes.len();
    if let Some((index, message)) = outcomes.iter().enumerate().find_map(|(i, o)| match o {
        StatementOutcome::Err(m) => Some((i, m.clone())),
        StatementOutcome::Ok(_) => None,
    }) {
        return Err(PluginError::internal(if total == 1 {
            message
        } else {
            format!(
                "Statement {} of {total} failed: {message} (statements outside a transaction run independently, so other statements may have been applied)",
                index + 1
            )
        }));
    }
    let mut values: Vec<Cbor> = outcomes
        .into_iter()
        .map(|o| match o {
            StatementOutcome::Ok(v) => v,
            StatementOutcome::Err(_) => unreachable!("errors returned above"),
        })
        .collect();
    if values.is_empty() {
        return Ok(query_result(
            Table {
                columns: vec![],
                rows: vec![],
            },
            None,
        ));
    }

    let first = values.remove(0);
    let primary = match (mode, first) {
        (PageMode::PushDown { page, size }, Cbor::Array(mut items)) => {
            let has_more = items.len() as u64 > size;
            items.truncate(usize::try_from(size).unwrap_or(usize::MAX));
            query_result(
                tabulate_items(&items),
                Some(pagination(page, size, has_more)),
            )
        }
        (PageMode::Slice { page, size }, Cbor::Array(items)) => {
            let (items, has_more) = slice_page(items, offset_for(page, size), size);
            query_result(
                tabulate_items(&items),
                Some(pagination(page, size, has_more)),
            )
        }
        (_, other) => query_result(tabulate(&other), None),
    };
    if values.is_empty() {
        return Ok(primary);
    }
    let mut primary = primary;
    let additional: Vec<Json> = values
        .iter()
        .map(|v| query_result(tabulate(v), None))
        .collect();
    primary["additional_results"] = Json::Array(additional);
    Ok(primary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plans_browse_queries_with_push_down() {
        let p = plan("SELECT * FROM `person`", Some(100), 3).unwrap();
        assert_eq!(p.sql, "SELECT * FROM `person`\nLIMIT 101 START 200");
        assert_eq!(p.mode, PageMode::PushDown { page: 3, size: 100 });
        assert!(!p.uses_use);

        let p = plan(
            "SELECT * FROM (SELECT * FROM `person` LIMIT 50) AS limited_subset",
            Some(20),
            1,
        )
        .unwrap();
        assert_eq!(
            p.sql,
            "SELECT * FROM (SELECT * FROM `person` LIMIT 50)\nLIMIT 21 START 0"
        );
    }

    #[test]
    fn plans_selects_that_already_paginate_as_slices() {
        let p = plan("SELECT * FROM person LIMIT 5;", Some(2), 2).unwrap();
        assert_eq!(p.sql, "SELECT * FROM person LIMIT 5");
        assert_eq!(p.mode, PageMode::Slice { page: 2, size: 2 });
    }

    #[test]
    fn never_paginates_writes_or_scripts() {
        let p = plan("UPDATE person SET seen = true", Some(100), 2).unwrap();
        assert_eq!(p.mode, PageMode::None);
        assert_eq!(p.sql, "UPDATE person SET seen = true");
        let p = plan("LET $a = 1; SELECT * FROM person", Some(100), 1).unwrap();
        assert_eq!(p.mode, PageMode::None);
        assert_eq!(p.statement_count, 2);
        let p = plan("SELECT * FROM person", None, 1).unwrap();
        assert_eq!(p.mode, PageMode::None);
        let p = plan("USE NS x DB y; SELECT * FROM t", Some(10), 1).unwrap();
        assert!(p.uses_use);
        assert!(plan("  -- nothing\n ; ", Some(10), 1).is_err());
    }

    fn records(n: i64) -> Cbor {
        Cbor::Array(
            (0..n)
                .map(|i| Cbor::Map(vec![(Cbor::Text("n".into()), Cbor::Integer(i.into()))]))
                .collect(),
        )
    }

    #[test]
    fn push_down_uses_the_extra_row_for_has_more() {
        let r = build_response(
            vec![StatementOutcome::Ok(records(3))],
            PageMode::PushDown { page: 1, size: 2 },
        )
        .unwrap();
        assert_eq!(r["rows"], json!([[0], [1]]));
        assert_eq!(r["pagination"]["has_more"], true);

        let r = build_response(
            vec![StatementOutcome::Ok(records(2))],
            PageMode::PushDown { page: 2, size: 2 },
        )
        .unwrap();
        assert_eq!(r["pagination"]["has_more"], false);
        assert_eq!(r["pagination"]["page"], 2);
    }

    #[test]
    fn slices_client_side() {
        let r = build_response(
            vec![StatementOutcome::Ok(records(5))],
            PageMode::Slice { page: 2, size: 2 },
        )
        .unwrap();
        assert_eq!(r["rows"], json!([[2], [3]]));
        assert_eq!(r["pagination"]["has_more"], true);
    }

    #[test]
    fn multi_statement_results_become_additional_results() {
        let r = build_response(
            vec![
                StatementOutcome::Ok(Cbor::Tag(6, Box::new(Cbor::Null))),
                StatementOutcome::Ok(records(1)),
                StatementOutcome::Ok(Cbor::Integer(7.into())),
            ],
            PageMode::None,
        )
        .unwrap();
        assert_eq!(r["columns"], json!([]));
        assert_eq!(r["additional_results"][0]["rows"], json!([[0]]));
        assert_eq!(r["additional_results"][1]["columns"], json!(["result"]));
        assert!(r["pagination"].is_null());
        assert_eq!(r["affected_rows"], 0);
    }

    #[test]
    fn statement_errors_are_reported_with_their_position() {
        let e = build_response(
            vec![
                StatementOutcome::Ok(records(1)),
                StatementOutcome::Err("The table 'x' does not exist".into()),
            ],
            PageMode::None,
        )
        .unwrap_err();
        assert!(e
            .message
            .starts_with("Statement 2 of 2 failed: The table 'x' does not exist"));
        let e = build_response(
            vec![StatementOutcome::Err("Parse error".into())],
            PageMode::None,
        )
        .unwrap_err();
        assert_eq!(e.message, "Parse error");
    }
}
