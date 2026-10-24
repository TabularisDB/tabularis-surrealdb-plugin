//! Connection checks and SurrealQL execution.
//!
//! Two entry points run SurrealQL:
//!
//! - `execute_query`: one query text (a single statement, or a script from
//!   a notebook cell). Returns one `QueryResult`.
//! - `execute_query_batch`: the editor's "run all", already split into
//!   statements by the host. The plugin sends them to SurrealDB as one
//!   script, so `LET` variables and `BEGIN … COMMIT` work across them, and
//!   returns one result or error per statement.

use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::error::PluginError;
use crate::handlers::results::{
    pagination, query_result, query_result_with, tabulate, tabulate_items, Table,
};
use crate::handlers::{req_str, session_config, Context};
use crate::models::SessionConfig;
use crate::surreal::session::{Session, StatementOutcome, TimedOutcome};
use crate::surreal::{retry_class, Retry};
use crate::utils::pagination::{offset_for, slice_page};
use crate::utils::surql::{
    can_push_down_pagination, is_balanced, normalize_host_browse_query, split_statements,
    with_pagination, Statement,
};
use crate::utils::values::is_none;
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

/// What the plugin knows about one statement it sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementPlan {
    /// First keyword, upper-cased (`SELECT`, `CREATE`, …).
    pub keyword: Option<String>,
    pub mode: PageMode,
}

/// What `execute_query` sends to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub sql: String,
    pub statements: Vec<StatementPlan>,
    /// The query changes namespace/database with `USE`.
    pub uses_use: bool,
}

/// Statements whose returned records are the records they changed.
const WRITE_KEYWORDS: &[&str] = &["CREATE", "INSERT", "UPDATE", "UPSERT", "DELETE", "RELATE"];

/// Statements that never return data: they change the session (`USE`,
/// `LET`, `OPTION`), the transaction (`BEGIN`, `COMMIT`, `CANCEL`) or the
/// schema (`DEFINE`, `REMOVE`, `ALTER`, `REBUILD`), or only run code
/// (`FOR`, `SLEEP`, `KILL`). In a script they produce no result set, decided
/// by keyword so the shape is the same on every server version (3.x answers
/// `USE` with the selected namespace and database, 2.x with NONE).
const NO_RESULT_KEYWORDS: &[&str] = &[
    "USE", "LET", "OPTION", "BEGIN", "COMMIT", "CANCEL", "DEFINE", "REMOVE", "ALTER", "REBUILD",
    "FOR", "SLEEP", "KILL",
];

/// Statements that SurrealDB 2.x runs without adding an entry to the `query`
/// response. 3.x answers every statement.
const UNANSWERED_ON_2X: &[&str] = &["BEGIN", "COMMIT", "CANCEL", "OPTION"];

/// Which statements of a script the server answers with a result entry, so
/// outcomes can be lined up with the statements the plugin sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Answers {
    /// One entry per statement (3.x, and the assumption when the version is
    /// unknown).
    #[default]
    Every,
    /// No entry for [`UNANSWERED_ON_2X`] statements (2.x).
    SkipsTransactionControl,
}

impl Answers {
    pub fn for_server(major: Option<u64>) -> Self {
        match major {
            Some(m) if m < 3 => Answers::SkipsTransactionControl,
            _ => Answers::Every,
        }
    }

    /// True when the server adds a result entry for a statement starting
    /// with `keyword`.
    pub fn answers(self, keyword: Option<&str>) -> bool {
        match self {
            Answers::Every => true,
            Answers::SkipsTransactionControl => {
                !keyword.is_some_and(|k| UNANSWERED_ON_2X.contains(&k))
            }
        }
    }
}

/// Decide how to run `query` for page `page` of `limit` rows.
///
/// Only single SELECT statements are paginated: the host re-runs the query
/// for every page, which must never re-apply a write. A configured query
/// timeout is added to pushed-down SELECTs as a `TIMEOUT` clause, so the
/// server fails them when it expires.
pub fn plan(
    query: &str,
    limit: Option<u64>,
    page: u64,
    timeout_secs: Option<u64>,
) -> Result<Plan, PluginError> {
    let normalized = normalize_host_browse_query(query);
    let statements = split_statements(&normalized);
    if statements.is_empty() {
        return Err(PluginError::invalid_params("The query is empty."));
    }
    let uses_use = statements.iter().any(|s| s.first_keyword() == Some("USE"));
    let keyword = |s: &Statement<'_>| s.first_keyword().map(str::to_string);
    if statements.len() == 1 {
        let (sql, mode) = single_statement(&statements[0], limit, page, timeout_secs);
        return Ok(Plan {
            sql,
            statements: vec![StatementPlan {
                keyword: keyword(&statements[0]),
                mode,
            }],
            uses_use,
        });
    }
    Ok(Plan {
        sql: normalized.clone(),
        statements: statements
            .iter()
            .map(|s| StatementPlan {
                keyword: keyword(s),
                mode: PageMode::None,
            })
            .collect(),
        uses_use,
    })
}

fn single_statement(
    statement: &Statement<'_>,
    limit: Option<u64>,
    page: u64,
    timeout_secs: Option<u64>,
) -> (String, PageMode) {
    // A piece of a block the host's splitter cut is not a SELECT of its own,
    // even when it starts with one.
    let select = statement.first_keyword() == Some("SELECT") && is_balanced(statement.text);
    match (select, limit) {
        (true, Some(size)) if can_push_down_pagination(statement) => {
            let offset = offset_for(page, size);
            (
                with_pagination(statement.text, size.saturating_add(1), offset, timeout_secs),
                PageMode::PushDown { page, size },
            )
        }
        (true, Some(size)) => (statement.text.to_string(), PageMode::Slice { page, size }),
        _ => (statement.text.to_string(), PageMode::None),
    }
}

fn limit_param(params: &Json) -> Option<u64> {
    params
        .get("limit")
        .or_else(|| params.get("page_size"))
        .and_then(Json::as_u64)
        .filter(|l| *l > 0)
}

pub async fn execute_query(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    let query = req_str(params, "query")?;
    let page = params.get("page").and_then(Json::as_u64).unwrap_or(1);
    let timeout = ctx.settings().query_timeout_secs;
    let plan = plan(query, limit_param(params), page, timeout)?;
    let (outcomes, answers) = run(ctx, &config, &plan.sql, plan.uses_use, timeout).await?;
    build_response(outcomes, &plan.statements, answers)
}

/// Send SurrealQL and return one outcome per answered statement, with hints
/// added to statement errors, and how the server answers statements (see
/// [`Answers`]). A query with `USE` runs on a session of its own, since
/// `USE` would switch the shared session's namespace/database under
/// concurrent requests. With a query timeout set, the plugin stops waiting
/// after it (plus a second, so a server-side `TIMEOUT` reports first).
async fn run(
    ctx: &Context,
    config: &SessionConfig,
    sql: &str,
    uses_use: bool,
    timeout_secs: Option<u64>,
) -> Result<(Vec<TimedOutcome>, Answers), PluginError> {
    let work = async {
        if uses_use || config.connection_id.is_none() {
            let session = Session::open(&SessionConfig {
                connection_id: None,
                ..config.clone()
            })
            .await?;
            let outcomes = session.query_checked(sql, vec![]).await?;
            Ok((
                with_hints(&session, outcomes),
                Answers::for_server(session.server_major),
            ))
        } else {
            ctx.sessions
                .with_session(config, |session| {
                    let sql = sql.to_string();
                    async move {
                        let outcomes = session
                            .query(&sql, vec![])
                            .await
                            .map_err(|e| retry_class(&session, e))?;
                        Ok((
                            with_hints(&session, outcomes),
                            Answers::for_server(session.server_major),
                        ))
                    }
                })
                .await
        }
    };
    match timeout_secs {
        None => work.await,
        Some(secs) => {
            tokio::time::timeout(Duration::from_secs(secs) + Duration::from_secs(1), work)
                .await
                .map_err(|_| local_timeout_error(secs))?
        }
    }
}

pub fn local_timeout_error(secs: u64) -> PluginError {
    PluginError::internal(format!(
        "SurrealDB did not answer within the plugin's query timeout ({secs}s). The plugin stopped waiting, but the statement may still be running on the server. Raise or disable it in the SurrealDB plugin settings, or add a TIMEOUT clause to the statement."
    ))
}

/// Add hints (which need the signed-in level) to statement errors.
fn with_hints(session: &Session, outcomes: Vec<TimedOutcome>) -> Vec<TimedOutcome> {
    outcomes
        .into_iter()
        .map(|t| match t.outcome {
            StatementOutcome::Err(m) => TimedOutcome {
                outcome: StatementOutcome::Err(session.hint(&m)),
                time_ms: t.time_ms,
            },
            ok => TimedOutcome {
                outcome: ok,
                time_ms: t.time_ms,
            },
        })
        .collect()
}

/// One statement's value as a `QueryResult`, with paging, an affected-row
/// count for writes (the records the statement returned) and the server
/// time.
fn statement_result(value: Cbor, plan: Option<&StatementPlan>, time_ms: Option<f64>) -> Json {
    let mode = plan.map(|p| p.mode).unwrap_or(PageMode::None);
    let is_write = plan
        .and_then(|p| p.keyword.as_deref())
        .is_some_and(|k| WRITE_KEYWORDS.contains(&k));
    match (mode, value) {
        (PageMode::PushDown { page, size }, Cbor::Array(mut items)) => {
            let has_more = items.len() as u64 > size;
            items.truncate(usize::try_from(size).unwrap_or(usize::MAX));
            query_result_with(
                tabulate_items(&items),
                Some(pagination(page, size, has_more)),
                0,
                time_ms,
            )
        }
        (PageMode::Slice { page, size }, Cbor::Array(items)) => {
            let (items, has_more) = slice_page(items, offset_for(page, size), size);
            query_result_with(
                tabulate_items(&items),
                Some(pagination(page, size, has_more)),
                0,
                time_ms,
            )
        }
        (_, other) => {
            let affected = match (&other, is_write) {
                (Cbor::Array(items), true) => items.len() as u64,
                (Cbor::Map(_), true) => 1,
                _ => 0,
            };
            query_result_with(tabulate(&other), None, affected, time_ms)
        }
    }
}

/// Turn the outcomes of one `execute_query` call into a `QueryResult`.
///
/// Outcomes are lined up with the statements the server answers (on 2.x,
/// not `BEGIN`/`COMMIT`/`CANCEL`/`OPTION`). When the counts still differ,
/// the plugin's split disagrees with the server's and no per-statement
/// knowledge (paging, write counts) is used.
///
/// - A failed statement fails the call, naming the statement when there are
///   several (the host has one error slot per call).
/// - With several statements, the ones that never return data
///   ([`NO_RESULT_KEYWORDS`]: `LET`, `DEFINE`, `USE`, `BEGIN`, …) produce no
///   result set, whatever the server answered. The first remaining result
///   is the primary one, which is what a notebook cell shows; the rest go to
///   `additional_results` (result tabs in the editor). Without alignment,
///   NONE results are left out instead.
pub fn build_response(
    outcomes: Vec<TimedOutcome>,
    plans: &[StatementPlan],
    answers: Answers,
) -> Result<Json, PluginError> {
    // (position among all statements, plan) of the answered statements.
    let answered: Vec<(usize, &StatementPlan)> = plans
        .iter()
        .enumerate()
        .filter(|(_, p)| answers.answers(p.keyword.as_deref()))
        .collect();
    let aligned = answered.len() == outcomes.len();
    let total = if aligned { plans.len() } else { outcomes.len() };
    if let Some((index, message)) =
        outcomes
            .iter()
            .enumerate()
            .find_map(|(i, t)| match &t.outcome {
                StatementOutcome::Err(m) => Some((i, m.clone())),
                StatementOutcome::Ok(_) => None,
            })
    {
        let position = if aligned { answered[index].0 } else { index };
        return Err(PluginError::internal(if total == 1 {
            message
        } else {
            format!(
                "Statement {} of {total} failed: {message} (statements outside a transaction run independently, so other statements may have been applied)",
                position + 1
            )
        }));
    }
    let mut results: Vec<Json> = Vec::new();
    for (i, timed) in outcomes.into_iter().enumerate() {
        let StatementOutcome::Ok(value) = timed.outcome else {
            unreachable!("errors returned above")
        };
        let plan = if aligned { Some(answered[i].1) } else { None };
        let no_result = match plan {
            Some(p) => p
                .keyword
                .as_deref()
                .is_some_and(|k| NO_RESULT_KEYWORDS.contains(&k)),
            None => is_none(&value),
        };
        if total > 1 && no_result {
            continue;
        }
        results.push(statement_result(value, plan, timed.time_ms));
    }
    if results.is_empty() {
        return Ok(query_result(
            Table {
                columns: vec![],
                rows: vec![],
            },
            None,
        ));
    }
    let mut primary = results.remove(0);
    if !results.is_empty() {
        primary["additional_results"] = Json::Array(results);
    }
    Ok(primary)
}

// ---------------------------------------------------------------------------
// execute_query_batch
// ---------------------------------------------------------------------------

/// One entry of the host's batch after planning.
struct BatchEntry {
    /// Text sent to the server, or `None` for a comment-only entry.
    sql: Option<String>,
    plans: Vec<StatementPlan>,
    /// Byte range of `sql` inside the joined script.
    range: std::ops::Range<usize>,
}

pub async fn execute_query_batch(ctx: &Context, params: &Json) -> Result<Json, PluginError> {
    let config = session_config(params)?;
    let queries: Vec<String> = params
        .get("queries")
        .and_then(Json::as_array)
        .ok_or_else(|| PluginError::invalid_params("missing array parameter `queries`"))?
        .iter()
        .map(|q| q.as_str().unwrap_or_default().to_string())
        .collect();
    let page = params.get("page").and_then(Json::as_u64).unwrap_or(1);
    let timeout = ctx.settings().query_timeout_secs;
    let (script, entries, uses_use) = plan_batch(&queries, limit_param(params), page, timeout);

    if script.trim().is_empty() {
        return Ok(Json::Array(entries.iter().map(|_| empty_entry()).collect()));
    }
    let (outcomes, answers) = match run(ctx, &config, &script, uses_use, timeout).await {
        Ok(run) => run,
        // The whole script was rejected (parse error, timeout, connection):
        // every statement reports it.
        Err(e) => return Ok(Json::Array(
            entries
                .iter()
                .map(|_| json!({ "result": null, "error": e.message, "execution_time_ms": null }))
                .collect(),
        )),
    };
    Ok(Json::Array(assign_batch_outcomes(
        &script, &entries, outcomes, answers,
    )))
}

/// Plan every entry and join them into one script.
fn plan_batch(
    queries: &[String],
    limit: Option<u64>,
    page: u64,
    timeout_secs: Option<u64>,
) -> (String, Vec<BatchEntry>, bool) {
    let mut script = String::new();
    let mut entries = Vec::with_capacity(queries.len());
    let mut uses_use = false;
    for query in queries {
        let normalized = normalize_host_browse_query(query);
        let statements = split_statements(&normalized);
        if statements.is_empty() {
            entries.push(BatchEntry {
                sql: None,
                plans: vec![],
                range: script.len()..script.len(),
            });
            continue;
        }
        uses_use |= statements.iter().any(|s| s.first_keyword() == Some("USE"));
        let (sql, plans) = if statements.len() == 1 {
            let (sql, mode) = single_statement(&statements[0], limit, page, timeout_secs);
            (
                sql,
                vec![StatementPlan {
                    keyword: statements[0].first_keyword().map(str::to_string),
                    mode,
                }],
            )
        } else {
            (
                normalized.clone(),
                statements
                    .iter()
                    .map(|s| StatementPlan {
                        keyword: s.first_keyword().map(str::to_string),
                        mode: PageMode::None,
                    })
                    .collect(),
            )
        };
        // The `;` goes on a line of its own, so a trailing `--` comment in
        // the previous entry cannot swallow it.
        if !script.is_empty() {
            script.push_str("\n;\n");
        }
        let start = script.len();
        script.push_str(&sql);
        entries.push(BatchEntry {
            sql: Some(sql),
            plans,
            range: start..script.len(),
        });
    }
    (script, entries, uses_use)
}

fn empty_entry() -> Json {
    json!({
        "result": query_result(Table { columns: vec![], rows: vec![] }, None),
        "error": null,
        "execution_time_ms": null,
    })
}

/// Give each host entry the outcomes of the statements that end inside it.
/// Only statements the server answers (see [`Answers`]) are lined up.
///
/// The host splits scripts with a generic SQL splitter, which can cut a
/// SurrealQL block (`{ …; … }`) into several entries. Joined again, the
/// block is one statement for the server; its result goes to the entry
/// where it ends and the other pieces report an empty result.
fn assign_batch_outcomes(
    script: &str,
    entries: &[BatchEntry],
    outcomes: Vec<TimedOutcome>,
    answers: Answers,
) -> Vec<Json> {
    let statements: Vec<_> = split_statements(script)
        .into_iter()
        .filter(|s| answers.answers(s.first_keyword()))
        .collect();
    let mut per_entry: Vec<Vec<TimedOutcome>> = entries.iter().map(|_| Vec::new()).collect();
    let sent: Vec<usize> = (0..entries.len())
        .filter(|i| entries[*i].sql.is_some())
        .collect();
    if statements.len() == outcomes.len() {
        for (statement, outcome) in statements.iter().zip(outcomes) {
            let end = statement.range.end.saturating_sub(1);
            let target = sent
                .iter()
                .copied()
                .find(|i| entries[*i].range.contains(&end))
                .or_else(|| sent.last().copied());
            if let Some(i) = target {
                per_entry[i].push(outcome);
            }
        }
    } else if sent.len() == outcomes.len() {
        for (i, outcome) in sent.iter().zip(outcomes) {
            per_entry[*i].push(outcome);
        }
    } else if let Some(last) = sent.last() {
        per_entry[*last] = outcomes;
    }

    entries
        .iter()
        .zip(per_entry)
        .map(|(entry, outcomes)| {
            if outcomes.is_empty() {
                return empty_entry();
            }
            let time: Option<f64> = outcomes.iter().map(|o| o.time_ms).sum::<Option<f64>>();
            match build_response(outcomes, &entry.plans, answers) {
                Ok(result) => json!({ "result": result, "error": null, "execution_time_ms": time }),
                Err(e) => json!({ "result": null, "error": e.message, "execution_time_ms": time }),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(v: Cbor) -> TimedOutcome {
        TimedOutcome::ok(v)
    }
    fn none() -> Cbor {
        Cbor::Tag(6, Box::new(Cbor::Null))
    }
    fn records(n: i64) -> Cbor {
        Cbor::Array(
            (0..n)
                .map(|i| Cbor::Map(vec![(Cbor::Text("n".into()), Cbor::Integer(i.into()))]))
                .collect(),
        )
    }
    fn plans_for(sql: &str) -> Vec<StatementPlan> {
        plan(sql, None, 1, None).unwrap().statements
    }
    /// `build_response` as a 3.x server answers (one entry per statement).
    fn respond(outcomes: Vec<TimedOutcome>, plans: &[StatementPlan]) -> Result<Json, PluginError> {
        build_response(outcomes, plans, Answers::Every)
    }
    fn assign(script: &str, entries: &[BatchEntry], outcomes: Vec<TimedOutcome>) -> Vec<Json> {
        assign_batch_outcomes(script, entries, outcomes, Answers::Every)
    }

    #[test]
    fn plans_browse_queries_with_push_down() {
        let p = plan("SELECT * FROM `person`", Some(100), 3, None).unwrap();
        assert_eq!(p.sql, "SELECT * FROM `person`\nLIMIT 101 START 200");
        assert_eq!(
            p.statements[0].mode,
            PageMode::PushDown { page: 3, size: 100 }
        );
        assert!(!p.uses_use);

        let p = plan(
            "SELECT * FROM (SELECT * FROM `person` LIMIT 50) AS limited_subset",
            Some(20),
            1,
            None,
        )
        .unwrap();
        assert_eq!(
            p.sql,
            "SELECT * FROM (SELECT * FROM `person` LIMIT 50)\nLIMIT 21 START 0"
        );
    }

    #[test]
    fn query_timeout_becomes_a_server_side_timeout_on_paged_selects() {
        let p = plan("SELECT * FROM `person`", Some(10), 1, Some(30)).unwrap();
        assert_eq!(
            p.sql,
            "SELECT * FROM `person`\nLIMIT 11 START 0 TIMEOUT 30s"
        );
        // Other statements are not rewritten.
        let p = plan("UPDATE person SET a = 1", Some(10), 1, Some(30)).unwrap();
        assert_eq!(p.sql, "UPDATE person SET a = 1");
        assert!(local_timeout_error(30).message.contains("30s"));
    }

    #[test]
    fn plans_selects_that_already_paginate_as_slices() {
        let p = plan("SELECT * FROM person LIMIT 5;", Some(2), 2, None).unwrap();
        assert_eq!(p.sql, "SELECT * FROM person LIMIT 5");
        assert_eq!(p.statements[0].mode, PageMode::Slice { page: 2, size: 2 });
    }

    #[test]
    fn never_paginates_writes_or_scripts() {
        let p = plan("UPDATE person SET seen = true", Some(100), 2, None).unwrap();
        assert_eq!(p.statements[0].mode, PageMode::None);
        let p = plan("LET $a = 1; SELECT * FROM person", Some(100), 1, None).unwrap();
        assert_eq!(p.statements.len(), 2);
        assert!(p.statements.iter().all(|s| s.mode == PageMode::None));
        let p = plan("SELECT * FROM person", None, 1, None).unwrap();
        assert_eq!(p.statements[0].mode, PageMode::None);
        let p = plan("USE NS x DB y; SELECT * FROM t", Some(10), 1, None).unwrap();
        assert!(p.uses_use);
        assert!(plan("  -- nothing\n ; ", Some(10), 1, None).is_err());
    }

    #[test]
    fn push_down_uses_the_extra_row_for_has_more() {
        let p = plan("SELECT * FROM t", Some(2), 1, None)
            .unwrap()
            .statements;
        let r = respond(vec![ok(records(3))], &p).unwrap();
        assert_eq!(r["rows"], json!([[0], [1]]));
        assert_eq!(r["pagination"]["has_more"], true);
        let p = plan("SELECT * FROM t", Some(2), 2, None)
            .unwrap()
            .statements;
        let r = respond(vec![ok(records(2))], &p).unwrap();
        assert_eq!(r["pagination"]["has_more"], false);
        assert_eq!(r["pagination"]["page"], 2);
    }

    #[test]
    fn slices_client_side() {
        let p = plan("SELECT * FROM t LIMIT 9", Some(2), 2, None)
            .unwrap()
            .statements;
        let r = respond(vec![ok(records(5))], &p).unwrap();
        assert_eq!(r["rows"], json!([[2], [3]]));
        assert_eq!(r["pagination"]["has_more"], true);
    }

    #[test]
    fn writes_report_affected_rows_and_server_time() {
        let r = respond(
            vec![TimedOutcome {
                outcome: StatementOutcome::Ok(records(3)),
                time_ms: Some(1.5),
            }],
            &plans_for("UPDATE item SET seen = true"),
        )
        .unwrap();
        assert_eq!(r["affected_rows"], 3);
        assert_eq!(r["server_time_ms"], 1.5);
        // SELECT results are rows, not affected rows.
        let r = respond(vec![ok(records(3))], &plans_for("SELECT * FROM item")).unwrap();
        assert_eq!(r["affected_rows"], 0);
        assert!(r.get("server_time_ms").is_none());
        // DELETE returns nothing by default: no count is claimed.
        let r = respond(vec![ok(Cbor::Array(vec![]))], &plans_for("DELETE item")).unwrap();
        assert_eq!(r["affected_rows"], 0);
    }

    #[test]
    fn scripts_skip_statements_that_return_nothing() {
        let sql = "LET $n = 1; SELECT * FROM t; RETURN 7; COMMIT";
        let r = respond(
            vec![
                ok(none()),
                ok(records(1)),
                ok(Cbor::Integer(7.into())),
                ok(none()),
            ],
            &plans_for(sql),
        )
        .unwrap();
        assert_eq!(r["rows"], json!([[0]]), "the SELECT is the primary result");
        assert_eq!(r["additional_results"].as_array().unwrap().len(), 1);
        assert_eq!(r["additional_results"][0]["rows"], json!([[7]]));

        let r = respond(
            vec![ok(none()), ok(none())],
            &plans_for("LET $a = 1; LET $b = 2"),
        )
        .unwrap();
        assert_eq!(r["columns"], json!([]));
        assert!(r.get("additional_results").is_none());

        // A single statement keeps its (empty) result.
        let r = respond(vec![ok(none())], &plans_for("DEFINE TABLE x")).unwrap();
        assert_eq!(r["columns"], json!([]));
    }

    #[test]
    fn statement_errors_are_reported_with_their_position() {
        let e = respond(
            vec![
                ok(records(1)),
                TimedOutcome::err("The table 'x' does not exist"),
            ],
            &plans_for("SELECT 1; SELECT * FROM x"),
        )
        .unwrap_err();
        assert!(e
            .message
            .starts_with("Statement 2 of 2 failed: The table 'x' does not exist"));
        let e = respond(vec![TimedOutcome::err("Parse error")], &[]).unwrap_err();
        assert_eq!(e.message, "Parse error");
    }

    fn batch(queries: &[&str]) -> (String, Vec<BatchEntry>, bool) {
        let q: Vec<String> = queries.iter().map(|s| s.to_string()).collect();
        plan_batch(&q, Some(10), 1, None)
    }

    #[test]
    fn batch_joins_statements_into_one_script() {
        let (script, entries, uses_use) =
            batch(&["LET $n = 3", "SELECT * FROM item WHERE n < $n", "-- note"]);
        assert_eq!(
            script,
            "LET $n = 3\n;\nSELECT * FROM item WHERE n < $n\nLIMIT 11 START 0"
        );
        assert!(!uses_use);
        assert!(entries[2].sql.is_none());
        assert_eq!(
            &script[entries[1].range.clone()],
            entries[1].sql.as_deref().unwrap()
        );
    }

    #[test]
    fn batch_entries_ending_in_a_line_comment_keep_their_separator() {
        let (script, entries, _) = batch(&["RETURN 1 -- note", "RETURN 2"]);
        assert_eq!(script, "RETURN 1 -- note\n;\nRETURN 2");
        assert_eq!(split_statements(&script).len(), 2);
        let out = assign(
            &script,
            &entries,
            vec![ok(Cbor::Integer(1.into())), ok(Cbor::Integer(2.into()))],
        );
        assert_eq!(out[0]["result"]["rows"], json!([[1]]));
        assert_eq!(out[1]["result"]["rows"], json!([[2]]));
    }

    #[test]
    fn batch_keeps_per_statement_errors() {
        let (script, entries, _) = batch(&["SELECT * FROM a", "THROW 'boom'", "RETURN 2"]);
        let out = assign(
            &script,
            &entries,
            vec![
                ok(records(2)),
                TimedOutcome::err("An error occurred: boom"),
                ok(Cbor::Integer(2.into())),
            ],
        );
        assert_eq!(out[0]["result"]["rows"], json!([[0], [1]]));
        assert!(out[0]["error"].is_null());
        assert!(out[1]["result"].is_null());
        assert_eq!(out[1]["error"], "An error occurred: boom");
        assert_eq!(out[2]["result"]["rows"], json!([[2]]));
    }

    #[test]
    fn batch_reassembles_blocks_the_host_split() {
        // The host's splitter cut a function body at its inner `;`.
        let (script, entries, _) = batch(&[
            "DEFINE FUNCTION fn::two() { LET $a = 2",
            "RETURN $a }",
            "RETURN fn::two()",
        ]);
        assert_eq!(split_statements(&script).len(), 2);
        let out = assign(
            &script,
            &entries,
            vec![ok(none()), ok(Cbor::Integer(2.into()))],
        );
        assert_eq!(out[0]["result"]["columns"], json!([]));
        assert!(out[0]["error"].is_null());
        assert_eq!(out[1]["result"]["columns"], json!([]), "NONE from DEFINE");
        assert_eq!(out[2]["result"]["rows"], json!([[2]]));
    }

    #[test]
    fn batch_never_paginates_a_piece_of_a_cut_block() {
        let (script, entries, _) = batch(&[
            "DEFINE FUNCTION OVERWRITE fn::f() { LET $m = 1",
            "SELECT * FROM item WHERE n >= $m }",
            "RETURN fn::f()",
        ]);
        assert!(!script.contains("LIMIT"), "{script}");
        assert_eq!(entries[1].plans[0].mode, PageMode::None);
        assert_eq!(split_statements(&script).len(), 2);
    }

    #[test]
    fn batch_falls_back_when_counts_disagree() {
        let (script, entries, _) = batch(&["RETURN 1", "RETURN 2"]);
        let out = assign(&script, &entries, vec![ok(Cbor::Integer(9.into()))]);
        assert_eq!(out[1]["result"]["rows"], json!([[9]]));
        assert_eq!(out[0]["result"]["columns"], json!([]));
    }

    #[test]
    fn answers_by_server_version() {
        assert_eq!(
            Answers::for_server(Some(2)),
            Answers::SkipsTransactionControl
        );
        assert_eq!(Answers::for_server(Some(3)), Answers::Every);
        assert_eq!(Answers::for_server(None), Answers::Every);
        let v2 = Answers::SkipsTransactionControl;
        for k in ["BEGIN", "COMMIT", "CANCEL", "OPTION"] {
            assert!(!v2.answers(Some(k)), "{k}");
            assert!(Answers::Every.answers(Some(k)), "{k}");
        }
        assert!(v2.answers(Some("CREATE")));
        assert!(v2.answers(None));
    }

    #[test]
    fn transactions_on_2x_line_up_with_their_statements() {
        // 2.x answers a cancelled transaction with two entries, not four.
        let (script, entries, _) = batch(&[
            "BEGIN TRANSACTION",
            "CREATE scratch:tx SET v = 1",
            "CANCEL TRANSACTION",
            "SELECT * FROM scratch:tx",
        ]);
        let out = assign_batch_outcomes(
            &script,
            &entries,
            vec![
                TimedOutcome::err("The query was not executed due to a cancelled transaction"),
                ok(Cbor::Array(vec![])),
            ],
            Answers::SkipsTransactionControl,
        );
        assert!(out[0]["error"].is_null());
        assert_eq!(out[0]["result"]["columns"], json!([]));
        assert!(out[1]["error"]
            .as_str()
            .unwrap()
            .contains("cancelled transaction"));
        assert!(out[2]["error"].is_null());
        assert_eq!(out[3]["result"]["rows"], json!([]));
        assert!(out[3]["error"].is_null());

        // One execute_query call: writes keep their count and errors their
        // position among all statements.
        let plans = plans_for("BEGIN; UPDATE t SET a = 1; COMMIT");
        let r = build_response(
            vec![ok(records(2))],
            &plans,
            Answers::SkipsTransactionControl,
        )
        .unwrap();
        assert_eq!(r["affected_rows"], 2);
        let plans = plans_for("BEGIN; RETURN 1; THROW 'x'; COMMIT");
        let e = build_response(
            vec![ok(Cbor::Integer(1.into())), TimedOutcome::err("x")],
            &plans,
            Answers::SkipsTransactionControl,
        )
        .unwrap_err();
        assert!(
            e.message.starts_with("Statement 3 of 4 failed: x"),
            "{}",
            e.message
        );
    }

    #[test]
    fn scripts_skip_statements_by_keyword_not_by_value() {
        let text = |t: &str| Cbor::Text(t.into());
        // 3.x answers USE with the selected namespace and database.
        let used = Cbor::Map(vec![
            (text("namespace"), text("app")),
            (text("database"), text("iso")),
        ]);
        let r = respond(
            vec![ok(used), ok(records(1))],
            &plans_for("USE DB iso; SELECT * FROM t"),
        )
        .unwrap();
        assert_eq!(r["rows"], json!([[0]]), "the SELECT is the primary result");
        assert!(r.get("additional_results").is_none());
        // 2.x answers it with NONE: same shape.
        let r = respond(
            vec![ok(none()), ok(records(1))],
            &plans_for("USE DB iso; SELECT * FROM t"),
        )
        .unwrap();
        assert_eq!(r["rows"], json!([[0]]));
        assert!(r.get("additional_results").is_none());
        // A statement the user asked a value of keeps its result, even NONE.
        let r = respond(
            vec![ok(none()), ok(Cbor::Integer(1.into()))],
            &plans_for("RETURN NONE; RETURN 1"),
        )
        .unwrap();
        assert_eq!(r["columns"], json!([]));
        assert_eq!(r["additional_results"][0]["rows"], json!([[1]]));
    }
}
