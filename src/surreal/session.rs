//! One signed-in SurrealDB session: a WebSocket connection, its sign-in and
//! its selected namespace/database.

use std::time::Duration;

use ciborium::value::Value as Cbor;

use crate::error::PluginError;
use crate::models::{AuthLevel, SessionConfig};
use crate::surreal::client::{RpcClient, RpcError};
use crate::surreal::errors;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on sign-in and `use`, so an unresponsive server fails the
/// connection instead of hanging it.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Result of one statement of a `query` call.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementOutcome {
    Ok(Cbor),
    Err(String),
}

pub struct Session {
    client: RpcClient,
    pub config: SessionConfig,
    pub signed_in_as: AuthLevel,
}

impl Session {
    /// Connect, sign in and select the namespace/database.
    pub async fn open(config: &SessionConfig) -> Result<Self, PluginError> {
        let client = RpcClient::connect(&config.url, CONNECT_TIMEOUT)
            .await
            .map_err(|e| errors::connect_error(&config.url, &e))?;
        let signed_in_as = bounded(sign_in(&client, config)).await??;
        let session = Self {
            client,
            config: config.clone(),
            signed_in_as,
        };
        if config.namespace.is_some() {
            bounded(session.use_target()).await??;
        }
        Ok(session)
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
    ) -> Result<Vec<StatementOutcome>, RpcError> {
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
    ) -> Result<Vec<StatementOutcome>, PluginError> {
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
            .map(|outcome| match outcome {
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

async fn bounded<T>(
    fut: impl std::future::Future<Output = Result<T, PluginError>>,
) -> Result<Result<T, PluginError>, PluginError> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .map_err(|_| {
            PluginError::internal(format!(
                "SurrealDB did not answer within {}s while signing in.",
                HANDSHAKE_TIMEOUT.as_secs()
            ))
        })
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
pub fn parse_query_response(response: Cbor) -> Result<Vec<StatementOutcome>, RpcError> {
    let Cbor::Array(items) = response else {
        return Err(RpcError::Protocol(
            "query did not return an array".to_string(),
        ));
    };
    Ok(items
        .into_iter()
        .map(|item| {
            let Cbor::Map(entries) = item else {
                return StatementOutcome::Ok(item);
            };
            let mut status = None;
            let mut result = Cbor::Tag(crate::utils::values::tags::NONE, Box::new(Cbor::Null));
            for (k, v) in entries {
                match k.as_text() {
                    Some("status") => status = v.as_text().map(str::to_string),
                    Some("result") => result = v,
                    _ => {}
                }
            }
            if status.as_deref() == Some("ERR") {
                let message = match result {
                    Cbor::Text(t) => t,
                    other => crate::utils::values::to_cell(&other).to_string(),
                };
                StatementOutcome::Err(message)
            } else {
                StatementOutcome::Ok(result)
            }
        })
        .collect())
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
        assert_eq!(out[0], StatementOutcome::Ok(Cbor::Integer(1.into())));
        assert_eq!(
            out[1],
            StatementOutcome::Err("The table 'nosuch' does not exist".into())
        );
        assert!(matches!(&out[2], StatementOutcome::Ok(Cbor::Tag(6, _))));
        assert!(parse_query_response(Cbor::Null).is_err());
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
}
