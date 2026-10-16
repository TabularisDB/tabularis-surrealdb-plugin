//! Turning connection, sign-in and query failures into messages a user can
//! act on. The raw server message is always kept; a hint is appended when
//! the cause is recognisable.

use crate::error::PluginError;
use crate::models::{AuthLevel, SessionConfig};
use crate::surreal::client::{ConnectError, RpcError};

pub fn connect_error(url: &str, error: &ConnectError) -> PluginError {
    let message = match error {
        ConnectError::Timeout(t) => format!(
            "Timed out after {}s connecting to {url}. Check Host and Port, and that no firewall blocks the connection.",
            t.as_secs()
        ),
        ConnectError::Http(status) => format!(
            "{url} refused the WebSocket connection (HTTP {status}). Check that Host and Port point to a SurrealDB server; the plugin connects to its /rpc endpoint."
        ),
        ConnectError::Io(io) if io.to_ascii_lowercase().contains("refused") => format!(
            "Could not connect to SurrealDB at {url}: connection refused. Check that the server is running and that Host and Port are right (SurrealDB listens on 8000 by default)."
        ),
        ConnectError::Io(io)
            if io.to_ascii_lowercase().contains("lookup")
                || io.to_ascii_lowercase().contains("nodename")
                || io.to_ascii_lowercase().contains("resolve") =>
        {
            format!("Could not resolve the host in {url}: {io}")
        }
        ConnectError::Io(io) => format!("Could not connect to SurrealDB at {url}: {io}"),
        ConnectError::Tls(tls) => format!(
            "TLS handshake with {url} failed: {tls}. Use a ws:// or plain host name for servers without TLS, and wss:// for servers with TLS."
        ),
        ConnectError::InvalidUrl(e) => format!("Invalid SurrealDB address {url}: {e}"),
        ConnectError::Other(e) => format!("Could not connect to SurrealDB at {url}: {e}"),
    };
    PluginError::internal(message)
}

/// Is this a rejected sign-in (wrong user, password, level or token)?
pub fn is_auth_failure(error: &RpcError) -> bool {
    match error {
        RpcError::Server(e) => {
            let m = e.message.to_ascii_lowercase();
            m.contains("authentication")
                || m.contains("invalid credentials")
                || m.contains("token")
                || e.kind.as_deref() == Some("NotAllowed")
        }
        _ => false,
    }
}

fn level_description(level: AuthLevel, config: &SessionConfig) -> String {
    let user = config.username.as_deref().unwrap_or("");
    match level {
        AuthLevel::Root => format!("root user `{user}`"),
        AuthLevel::Namespace => format!(
            "namespace user `{user}` on namespace `{}`",
            config.namespace.as_deref().unwrap_or("")
        ),
        AuthLevel::Database => format!("database user `{user}` on `{}`", config.target()),
        AuthLevel::Token => "token".to_string(),
        AuthLevel::Anonymous | AuthLevel::Auto => "anonymous".to_string(),
    }
}

/// Every attempted sign-in level failed.
pub fn auth_error(config: &SessionConfig, attempts: &[(AuthLevel, RpcError)]) -> PluginError {
    let tried: Vec<String> = attempts
        .iter()
        .map(|(level, _)| level_description(*level, config))
        .collect();
    let server = attempts
        .last()
        .map(|(_, e)| e.to_string())
        .unwrap_or_default();
    let hint = match config.auth {
        AuthLevel::Token => {
            "Check that the token is valid, not expired, and issued for this server.".to_string()
        }
        _ => {
            let mut hint = "Check the username and password.".to_string();
            if config.namespace.is_none() {
                hint.push_str(
                    " Namespace and database users also need Database set to `namespace` or `namespace/database`.",
                );
            }
            hint
        }
    };
    PluginError::internal(format!(
        "Authentication failed as {}. {hint} (SurrealDB: {server})",
        tried.join(", then as ")
    ))
}

/// Add a hint to a query or `use` failure when the cause is recognisable.
pub fn with_hint(message: &str, config: &SessionConfig, signed_in_as: AuthLevel) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("not enough permissions")
        || lower.contains("not allowed")
        || lower.contains("iam error")
    {
        return format!(
            "{message} (signed in as {}; this needs a user with more privileges, or a role such as EDITOR or OWNER)",
            level_description(signed_in_as, config)
        );
    }
    if lower.contains("namespace") && lower.contains("does not exist") {
        return format!(
            "{message}. Check the namespace in the Database field (`namespace/database`); SurrealDB does not create it on read."
        );
    }
    if lower.contains("database") && lower.contains("does not exist") {
        return format!(
            "{message}. Check the database in the Database field (`namespace/database`); SurrealDB does not create it on read."
        );
    }
    if lower.contains("specify a namespace") || lower.contains("specify a database") {
        return format!(
            "{message}. Set the Database field to `namespace/database`, or select a database in the explorer."
        );
    }
    if lower.contains("expired") {
        return format!("{message}. Reconnect to sign in again.");
    }
    message.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surreal::client::ServerError;
    use std::time::Duration;

    fn config() -> SessionConfig {
        SessionConfig {
            url: "ws://localhost:8000/rpc".into(),
            namespace: Some("app".into()),
            database: Some("main".into()),
            username: Some("reader".into()),
            password: Some("x".into()),
            auth: AuthLevel::Auto,
            connection_id: None,
        }
    }

    fn server(message: &str) -> RpcError {
        RpcError::Server(ServerError {
            code: -32000,
            message: message.into(),
            kind: None,
        })
    }

    #[test]
    fn connect_errors_are_actionable() {
        let url = "ws://localhost:8000/rpc";
        let refused = connect_error(
            url,
            &ConnectError::Io("Connection refused (os error 61)".into()),
        );
        assert!(refused.message.contains("connection refused"));
        assert!(refused.message.contains("8000"));
        let http = connect_error(url, &ConnectError::Http(404));
        assert!(http.message.contains("HTTP 404"));
        let timeout = connect_error(url, &ConnectError::Timeout(Duration::from_secs(10)));
        assert!(timeout.message.contains("Timed out after 10s"));
        let tls = connect_error("wss://x/rpc", &ConnectError::Tls("bad cert".into()));
        assert!(tls.message.contains("wss://"));
    }

    #[test]
    fn auth_errors_list_every_level_tried() {
        let attempts = vec![
            (
                AuthLevel::Database,
                server("There was a problem with authentication"),
            ),
            (
                AuthLevel::Namespace,
                server("There was a problem with authentication"),
            ),
            (
                AuthLevel::Root,
                server("There was a problem with authentication"),
            ),
        ];
        let e = auth_error(&config(), &attempts);
        assert!(e.message.starts_with(
            "Authentication failed as database user `reader` on `app/main`, then as namespace user `reader` on namespace `app`, then as root user `reader`."
        ));
        assert!(e
            .message
            .contains("There was a problem with authentication"));
        assert!(is_auth_failure(&attempts[0].1));
        assert!(!is_auth_failure(&RpcError::NotConnected));
    }

    #[test]
    fn query_errors_get_hints() {
        let c = config();
        let perm = with_hint(
            "IAM error: Not enough permissions to perform this action",
            &c,
            AuthLevel::Database,
        );
        assert!(perm.contains("signed in as database user `reader` on `app/main`"));
        let ns = with_hint("The namespace 'nope' does not exist", &c, AuthLevel::Root);
        assert!(ns.contains("Check the namespace"));
        let plain = with_hint("Parse error: unexpected token", &c, AuthLevel::Root);
        assert_eq!(plain, "Parse error: unexpected token");
    }
}
