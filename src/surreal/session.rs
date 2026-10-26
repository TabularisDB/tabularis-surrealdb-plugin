//! One signed-in SurrealDB session: a WebSocket connection, its sign-in and
//! its selected namespace/database.

use std::time::Duration;

use ciborium::value::Value as Cbor;

use crate::error::PluginError;
use crate::models::{AuthLevel, SessionConfig};
use crate::surreal::client::{RpcClient, RpcError};
use crate::surreal::errors;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on each handshake step (sign-in, the existence check, `use`), so an
/// unresponsive server fails the connection instead of hanging it.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Result of one statement of a `query` call.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementOutcome {
    Ok(Cbor),
    Err(String),
}

/// A statement outcome with the server-side execution time it reported.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedOutcome {
    pub outcome: StatementOutcome,
    pub time_ms: Option<f64>,
}

impl TimedOutcome {
    pub fn ok(value: Cbor) -> Self {
        Self {
            outcome: StatementOutcome::Ok(value),
            time_ms: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            outcome: StatementOutcome::Err(message.into()),
            time_ms: None,
        }
    }
}

pub struct Session {
    client: RpcClient,
    pub config: SessionConfig,
    pub signed_in_as: AuthLevel,
    /// Major version of the server (`2`, `3`), read once when the session
    /// opens. `None` when the server did not say.
    pub server_major: Option<u64>,
}

impl Session {
    /// Connect, sign in, check that the namespace/database exists and
    /// select it.
    pub async fn open(config: &SessionConfig) -> Result<Self, PluginError> {
        let client = RpcClient::connect(&config.url, CONNECT_TIMEOUT, &config.tls)
            .await
            .map_err(|e| errors::connect_error(&config.url, &e))?;
        let signed_in_as = bounded("signing in", sign_in(&client, config)).await??;
        let mut session = Self {
            client,
            config: config.clone(),
            signed_in_as,
            server_major: None,
        };
        // Best effort: statement results are shaped slightly differently on
        // 2.x (see `handlers::query::Answers`), but a server that does not
        // answer `version` can still be used.
        session.server_major = bounded("reading the server version", session.version())
            .await
            .ok()
            .and_then(Result::ok)
            .as_deref()
            .and_then(major_version);
        if config.namespace.is_some() {
            let target = config.target();
            bounded(
                &format!("checking that `{target}` exists"),
                session.check_target_exists(),
            )
            .await??;
            bounded(&format!("selecting `{target}`"), session.use_target()).await??;
        }
        Ok(session)
    }

    /// Refuse a namespace or database that does not exist, without creating
    /// it.
    ///
    /// On SurrealDB 3.x, `use` creates a missing namespace or database when
    /// the signed-in user may define one: root users get both, namespace
    /// OWNERs and EDITORs get a database. (2.x creates nothing, but the
    /// connection would then look empty.) So `use` never names anything
    /// that has not been seen to exist:
    ///
    /// 1. `INFO FOR ROOT`, which needs no namespace, lists the namespaces.
    ///    Users below root may not run it; they cannot create a namespace
    ///    either, and a namespace or database user only signs in to a
    ///    namespace that exists.
    /// 2. Only then `use` selects the namespace alone, and `INFO FOR NS`
    ///    lists its databases. Database users may not run it; they cannot
    ///    create a database either, and only sign in to one that exists.
    ///
    /// When a check is not allowed it is skipped and the server's own
    /// permissions decide.
    async fn check_target_exists(&self) -> Result<(), PluginError> {
        let Some(ns) = &self.config.namespace else {
            return Ok(());
        };
        if let Some(names) = self.info_names("INFO FOR ROOT", "namespaces").await? {
            if !names.contains(ns) {
                return Err(errors::not_found(
                    "namespace",
                    ns,
                    &self.config,
                    self.signed_in_as,
                ));
            }
        }
        let Some(db) = &self.config.database else {
            return Ok(());
        };
        self.client
            .call("use", vec![Cbor::Text(ns.clone()), Cbor::Null])
            .await
            .map_err(|e| self.rpc_error(e))?;
        if let Some(names) = self.info_names("INFO FOR NS", "databases").await? {
            if !names.contains(db) {
                return Err(errors::not_found(
                    "database",
                    db,
                    &self.config,
                    self.signed_in_as,
                ));
            }
        }
        Ok(())
    }

    /// Names under `key` in the result of an `INFO FOR …` statement. `None`
    /// when the signed-in user may not run it, or the result has no such
    /// key, so the caller cannot tell either way.
    async fn info_names(&self, sql: &str, key: &str) -> Result<Option<Vec<String>>, PluginError> {
        let info = match self.query(sql, vec![]).await {
            Ok(outcomes) => match outcomes.into_iter().next().map(|timed| timed.outcome) {
                Some(StatementOutcome::Ok(info)) => info,
                Some(StatementOutcome::Err(m)) if errors::is_permission_denied(&m) => {
                    return Ok(None)
                }
                Some(StatementOutcome::Err(m)) => return Err(PluginError::internal(self.hint(&m))),
                None => return Ok(None),
            },
            // 3.x rejects the whole call for anonymous sessions.
            Err(RpcError::Server(e))
                if e.kind.as_deref() == Some("NotAllowed")
                    || errors::is_permission_denied(&e.message) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(self.rpc_error(e)),
        };
        Ok(definition_keys(&info, key))
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// (Re)select the configured namespace and database.
    pub async fn use_target(&self) -> Result<(), PluginError> {
        let ns = self
            .config
            .namespace
            .clone()
            .map(Cbor::Text)
            .unwrap_or(Cbor::Null);
        let db = self
            .config
            .database
            .clone()
            .map(Cbor::Text)
            .unwrap_or(Cbor::Null);
        self.client
            .call("use", vec![ns, db])
            .await
            .map(|_| ())
            .map_err(|e| self.rpc_error(e))
    }

    /// Run SurrealQL and return one outcome per statement. Statement errors
    /// are returned as `StatementOutcome::Err`, not as `Err(_)`; `Err(_)` is
    /// for failures of the call itself.
    pub async fn query(
        &self,
        sql: &str,
        vars: Vec<(&str, Cbor)>,
    ) -> Result<Vec<TimedOutcome>, RpcError> {
        let vars = Cbor::Map(
            vars.into_iter()
                .map(|(k, v)| (Cbor::Text(k.to_string()), v))
                .collect(),
        );
        let response = self
            .client
            .call("query", vec![Cbor::Text(sql.to_string()), vars])
            .await?;
        parse_query_response(response)
    }

    /// Like [`Self::query`], but turns call failures into user-facing errors.
    pub async fn query_checked(
        &self,
        sql: &str,
        vars: Vec<(&str, Cbor)>,
    ) -> Result<Vec<TimedOutcome>, PluginError> {
        self.query(sql, vars).await.map_err(|e| self.rpc_error(e))
    }

    /// Run SurrealQL whose statements must all succeed, returning their
    /// results.
    pub async fn query_all_ok(
        &self,
        sql: &str,
        vars: Vec<(&str, Cbor)>,
    ) -> Result<Vec<Cbor>, PluginError> {
        self.query_checked(sql, vars)
            .await?
            .into_iter()
            .map(|timed| match timed.outcome {
                StatementOutcome::Ok(v) => Ok(v),
                StatementOutcome::Err(m) => Err(PluginError::internal(self.hint(&m))),
            })
            .collect()
    }

    pub async fn ping(&self) -> Result<(), PluginError> {
        self.client
            .call("ping", vec![])
            .await
            .map(|_| ())
            .map_err(|e| self.rpc_error(e))
    }

    /// Server version text, e.g. `surrealdb-3.2.3`. 2.x answers with a
    /// string, 3.x with `{ version, build, timestamp }`.
    pub async fn version(&self) -> Result<String, PluginError> {
        let value = self
            .client
            .call("version", vec![])
            .await
            .map_err(|e| self.rpc_error(e))?;
        Ok(version_text(&value))
    }

    pub fn hint(&self, message: &str) -> String {
        errors::with_hint(message, &self.config, self.signed_in_as)
    }

    pub fn rpc_error(&self, error: RpcError) -> PluginError {
        match error {
            RpcError::Server(e) => PluginError::internal(self.hint(&e.message)),
            other => PluginError::internal(other.to_string()),
        }
    }
}

/// Run one handshake step under [`HANDSHAKE_TIMEOUT`]; `stage` names the
/// step in the timeout message ("while {stage}").
async fn bounded<T>(
    stage: &str,
    fut: impl std::future::Future<Output = Result<T, PluginError>>,
) -> Result<Result<T, PluginError>, PluginError> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .map_err(|_| timeout_error(stage))
}

fn timeout_error(stage: &str) -> PluginError {
    PluginError::internal(format!(
        "SurrealDB did not answer within {}s while {stage}.",
        HANDSHAKE_TIMEOUT.as_secs()
    ))
}

/// Keys of the definition map `key` (`namespaces`, `databases`, …) of an
/// `INFO FOR …` result; `None` when there is no such map.
fn definition_keys(info: &Cbor, key: &str) -> Option<Vec<String>> {
    let Cbor::Map(entries) = info else {
        return None;
    };
    let (_, Cbor::Map(defs)) = entries.iter().find(|(k, _)| k.as_text() == Some(key))? else {
        return None;
    };
    Some(
        defs.iter()
            .filter_map(|(name, _)| name.as_text().map(str::to_string))
            .collect(),
    )
}

async fn sign_in(client: &RpcClient, config: &SessionConfig) -> Result<AuthLevel, PluginError> {
    let candidates: Vec<AuthLevel> = match config.auth {
        AuthLevel::Anonymous => return Ok(AuthLevel::Anonymous),
        AuthLevel::Token => {
            let token = config.password.clone().unwrap_or_default();
            return match client.call("authenticate", vec![Cbor::Text(token)]).await {
                Ok(_) => Ok(AuthLevel::Token),
                Err(e) if errors::is_auth_failure(&e) => {
                    Err(errors::auth_error(config, &[(AuthLevel::Token, e)]))
                }
                Err(e) => Err(PluginError::internal(e.to_string())),
            };
        }
        AuthLevel::Auto => {
            // Least privilege first.
            let mut levels = Vec::new();
            if config.database.is_some() {
                levels.push(AuthLevel::Database);
            }
            if config.namespace.is_some() {
                levels.push(AuthLevel::Namespace);
            }
            levels.push(AuthLevel::Root);
            levels
        }
        level => vec![level],
    };

    let mut failures = Vec::new();
    for level in candidates {
        match client
            .call("signin", vec![signin_params(config, level)])
            .await
        {
            Ok(_) => return Ok(level),
            Err(e) if errors::is_auth_failure(&e) => failures.push((level, e)),
            Err(e) => return Err(PluginError::internal(e.to_string())),
        }
    }
    Err(errors::auth_error(config, &failures))
}

/// `signin` parameters for a system user at `level`.
pub fn signin_params(config: &SessionConfig, level: AuthLevel) -> Cbor {
    let text = |s: &Option<String>| Cbor::Text(s.clone().unwrap_or_default());
    let mut entries = Vec::new();
    if matches!(level, AuthLevel::Namespace | AuthLevel::Database) {
        entries.push((Cbor::Text("NS".into()), text(&config.namespace)));
    }
    if level == AuthLevel::Database {
        entries.push((Cbor::Text("DB".into()), text(&config.database)));
    }
    entries.push((Cbor::Text("user".into()), text(&config.username)));
    entries.push((Cbor::Text("pass".into()), text(&config.password)));
    Cbor::Map(entries)
}

/// Read the `query` response: an array of `{ status, time, result }`.
/// For `status: "ERR"` the result is the error message (2.x) or carries it
/// next to `kind`/`details` (3.x).
pub fn parse_query_response(response: Cbor) -> Result<Vec<TimedOutcome>, RpcError> {
    let Cbor::Array(items) = response else {
        return Err(RpcError::Protocol(
            "query did not return an array".to_string(),
        ));
    };
    Ok(items
        .into_iter()
        .map(|item| {
            let Cbor::Map(entries) = item else {
                return TimedOutcome::ok(item);
            };
            let mut status = None;
            let mut time_ms = None;
            let mut result = Cbor::Tag(crate::utils::values::tags::NONE, Box::new(Cbor::Null));
            for (k, v) in entries {
                match k.as_text() {
                    Some("status") => status = v.as_text().map(str::to_string),
                    Some("result") => result = v,
                    Some("time") => time_ms = v.as_text().and_then(parse_duration_ms),
                    _ => {}
                }
            }
            if status.as_deref() == Some("ERR") {
                let message = match result {
                    Cbor::Text(t) => t,
                    other => crate::utils::values::to_cell(&other).to_string(),
                };
                TimedOutcome {
                    outcome: StatementOutcome::Err(message),
                    time_ms,
                }
            } else {
                TimedOutcome {
                    outcome: StatementOutcome::Ok(result),
                    time_ms,
                }
            }
        })
        .collect())
}

/// Milliseconds from a statement `time` such as `3.82ms`, `130.3µs`,
/// `1.5s` or `10ns` (Rust's `Duration` debug format, used by 2.x and 3.x).
pub fn parse_duration_ms(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let factor = match unit {
        "ns" => 1e-6,
        "µs" | "us" => 1e-3,
        "ms" => 1.0,
        "s" => 1e3,
        _ => return None,
    };
    Some(value * factor)
}

/// Major version from a version text such as `surrealdb-2.7.0` or
/// `surrealdb-3.2.6+20261018.c63e805`.
pub fn major_version(text: &str) -> Option<u64> {
    let rest = text
        .trim()
        .strip_prefix("surrealdb-")
        .unwrap_or(text.trim());
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn version_text(value: &Cbor) -> String {
    match value {
        Cbor::Text(t) => t.clone(),
        Cbor::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_text() == Some("version"))
            .and_then(|(_, v)| v.as_text())
            .map(|v| format!("surrealdb-{v}"))
            .unwrap_or_else(|| crate::utils::values::to_cell(value).to_string()),
        other => crate::utils::values::to_cell(other).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: Vec<(&str, Cbor)>) -> Cbor {
        Cbor::Map(
            entries
                .into_iter()
                .map(|(k, v)| (Cbor::Text(k.into()), v))
                .collect(),
        )
    }

    fn config() -> SessionConfig {
        SessionConfig {
            url: "ws://localhost:8000/rpc".into(),
            namespace: Some("app".into()),
            database: Some("main".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            auth: AuthLevel::Auto,
            tls: Default::default(),
            connection_id: None,
        }
    }

    #[test]
    fn signin_params_per_level() {
        let keys = |v: Cbor| -> Vec<String> {
            match v {
                Cbor::Map(e) => e.into_iter().map(|(k, _)| k.into_text().unwrap()).collect(),
                _ => vec![],
            }
        };
        assert_eq!(
            keys(signin_params(&config(), AuthLevel::Root)),
            ["user", "pass"]
        );
        assert_eq!(
            keys(signin_params(&config(), AuthLevel::Namespace)),
            ["NS", "user", "pass"]
        );
        assert_eq!(
            keys(signin_params(&config(), AuthLevel::Database)),
            ["NS", "DB", "user", "pass"]
        );
    }

    #[test]
    fn parses_mixed_statement_results() {
        let response = Cbor::Array(vec![
            map(vec![
                ("status", Cbor::Text("OK".into())),
                ("result", Cbor::Integer(1.into())),
            ]),
            map(vec![
                ("status", Cbor::Text("ERR".into())),
                (
                    "result",
                    Cbor::Text("The table 'nosuch' does not exist".into()),
                ),
                ("kind", Cbor::Text("NotFound".into())),
            ]),
            map(vec![("status", Cbor::Text("OK".into()))]),
        ]);
        let out = parse_query_response(response).unwrap();
        assert_eq!(
            out[0].outcome,
            StatementOutcome::Ok(Cbor::Integer(1.into()))
        );
        assert_eq!(
            out[1].outcome,
            StatementOutcome::Err("The table 'nosuch' does not exist".into())
        );
        assert!(matches!(
            &out[2].outcome,
            StatementOutcome::Ok(Cbor::Tag(6, _))
        ));
        assert!(parse_query_response(Cbor::Null).is_err());
    }

    #[test]
    fn reads_definition_keys() {
        let info = map(vec![(
            "namespaces",
            map(vec![("app", Cbor::Text("DEFINE NAMESPACE app".into()))]),
        )]);
        assert_eq!(
            definition_keys(&info, "namespaces"),
            Some(vec!["app".into()])
        );
        assert_eq!(definition_keys(&info, "databases"), None);
        assert_eq!(definition_keys(&Cbor::Null, "namespaces"), None);
    }

    #[test]
    fn timeouts_name_the_stage() {
        assert_eq!(
            timeout_error("selecting `app/main`").message,
            "SurrealDB did not answer within 15s while selecting `app/main`."
        );
    }

    #[test]
    fn parses_statement_times() {
        let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
        assert!(close(parse_duration_ms("3.822666ms"), 3.822666));
        assert!(close(parse_duration_ms("130.333µs"), 0.130333));
        assert!(close(parse_duration_ms("1.5s"), 1500.0));
        assert!(close(parse_duration_ms("10ns"), 0.00001));
        assert_eq!(parse_duration_ms("soon"), None);
        assert_eq!(parse_duration_ms("1h"), None);
        let response = Cbor::Array(vec![map(vec![
            ("status", Cbor::Text("OK".into())),
            ("time", Cbor::Text("2ms".into())),
            ("result", Cbor::Null),
        ])]);
        assert_eq!(
            parse_query_response(response).unwrap()[0].time_ms,
            Some(2.0)
        );
    }

    #[test]
    fn version_text_from_2x_and_3x() {
        assert_eq!(
            version_text(&Cbor::Text("surrealdb-2.3.7".into())),
            "surrealdb-2.3.7"
        );
        assert_eq!(
            version_text(&map(vec![("version", Cbor::Text("3.2.3".into()))])),
            "surrealdb-3.2.3"
        );
    }

    #[test]
    fn major_version_from_version_text() {
        assert_eq!(major_version("surrealdb-2.7.0"), Some(2));
        assert_eq!(major_version("surrealdb-3.2.6+20261018.c63e805"), Some(3));
        assert_eq!(major_version("3.0.0"), Some(3));
        assert_eq!(major_version("surrealdb-beta"), None);
    }
}
