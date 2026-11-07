//! Turning connection, sign-in and query failures into messages a user can
//! act on. The raw server message is always kept; a hint is appended when
//! the cause is recognisable.

use std::borrow::Cow;

use crate::error::PluginError;
use crate::models::{AuthLevel, SessionConfig};
use crate::surreal::client::{ConnectError, RpcError};

pub fn connect_error(url: &str, error: &ConnectError) -> PluginError {
    let url = without_userinfo(url);
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
        ConnectError::Tls(tls)
            if tls.to_ascii_lowercase().contains("corrupt message")
                || tls.contains("InvalidContentType") =>
        {
            format!(
                "{url} answered without TLS ({tls}): the server does not use TLS on this port. Set SSL mode to Disabled in the SSL tab (an empty SSL mode means Required, as the tab shows), or put a ws:// URL in Host."
            )
        }
        ConnectError::Tls(tls) => format!(
            "TLS handshake with {url} failed: {tls}. For a server with a private or self-signed certificate, set its CA certificate file in the SSL tab (SSL mode Verify CA or Verify Identity), or use SSL mode Required to encrypt without checking the certificate. For a server without TLS, use SSL mode Disabled or a ws:// URL."
        ),
        ConnectError::TlsConfig(e) => e.clone(),
        ConnectError::InvalidUrl(e) => format!("Invalid SurrealDB address {url}: {e}"),
        ConnectError::Other(e) => format!("Could not connect to SurrealDB at {url}: {e}"),
    };
    PluginError::internal(message)
}

/// `url` without the user name and password that a Host URL may carry
/// (`wss://user:pass@host/rpc` → `wss://host/rpc`), so that a message
/// showing the address never shows a password. The connection does not use
/// them either: the plugin signs in with Username and Password.
///
/// The last `@` ends them, since a password may contain `@` or `/`. A bare
/// `user:pass@host` in Host is bracketed like an IPv6 address
/// (`ws://[user:pass@host]:8000/rpc`), and keeps its bracket.
pub fn without_userinfo(url: &str) -> Cow<'_, str> {
    let Some(scheme_end) = url.find("://") else {
        return Cow::Borrowed(url);
    };
    let mut start = scheme_end + "://".len();
    if url[start..].starts_with('[') {
        start += 1;
    }
    match url[start..].rfind('@') {
        Some(at) => Cow::Owned(format!("{}{}", &url[..start], &url[start + at + 1..])),
        None => Cow::Borrowed(url),
    }
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
            // The namespace comes from Namespace/Database on the Databases
            // tab (Tabularis after 0.31.0) or from an imported connection
            // string; older forms have no field for it.
            if config.namespace.is_none() {
                hint.push_str(
                    " Without a namespace, only a root user can sign in. For a namespace or database user, enter `namespace/database` (or `namespace`) in Namespace/Database on the connection's Databases tab, or import a connection string that names them, `surrealdb://user:pass@host:8000/namespace/database`.",
                );
            } else if config.database.is_none() {
                hint.push_str(
                    " A database user also needs the database: connect to `namespace/database`.",
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

/// Does a server message say the signed-in user may not do this?
pub fn is_permission_denied(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("not enough permissions")
        || lower.contains("not allowed")
        || lower.contains("iam error")
}

/// The namespace or database to connect to does not exist. Worded like the
/// server's own message, so [`with_handshake_hint`] adds the same hint to
/// both.
pub fn not_found(
    kind: &str,
    name: &str,
    config: &SessionConfig,
    signed_in_as: AuthLevel,
) -> PluginError {
    PluginError::internal(with_handshake_hint(
        &format!("The {kind} '{name}' does not exist"),
        config,
        signed_in_as,
    ))
}

/// Add a hint to a failure while connecting, after sign-in: checking and
/// selecting the connection's namespace and database. A namespace or
/// database that does not exist is then the connection's own, so the hint
/// says where it is set. Other causes get the hints of [`with_hint`].
pub fn with_handshake_hint(
    message: &str,
    config: &SessionConfig,
    signed_in_as: AuthLevel,
) -> String {
    let lower = message.to_ascii_lowercase();
    if !is_permission_denied(message) && lower.contains("does not exist") {
        if lower.contains("namespace") {
            return format!(
                "{message}. Check the namespace of the connection's `namespace/database` (chosen in its Databases tab, or the path of its connection string). The plugin only connects to an existing namespace and never creates one."
            );
        }
        if lower.contains("database") {
            return format!(
                "{message}. Check the database of the connection's `namespace/database` (chosen in its Databases tab, or the path of its connection string). The plugin only connects to an existing database and never creates one."
            );
        }
    }
    with_hint(message, config, signed_in_as)
}

/// Add a hint to a failed query or request when the cause is recognisable.
/// A namespace or database that does not exist gets none: in a query typed
/// in the editor (`REMOVE DATABASE ghost`) it is not the connection's. The
/// connection's own are checked while connecting, see
/// [`with_handshake_hint`].
pub fn with_hint(message: &str, config: &SessionConfig, signed_in_as: AuthLevel) -> String {
    let lower = message.to_ascii_lowercase();
    if is_permission_denied(message) {
        return format!(
            "{message} (signed in as {}; this needs a user with more privileges, or a role such as EDITOR or OWNER)",
            level_description(signed_in_as, config)
        );
    }
    if lower.contains("specify a namespace") || lower.contains("specify a database") {
        return format!(
            "{message}. Select a database in the explorer, or choose a `namespace/database` in the connection's Databases tab."
        );
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return format!(
            "{message} (the statement ran past its TIMEOUT and the server failed it; paged SELECTs get a TIMEOUT from the plugin's query timeout setting)"
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
            tls: Default::default(),
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
        let plain = connect_error(
            "wss://localhost:8000/rpc",
            &ConnectError::Tls("received corrupt message of type InvalidContentType".into()),
        );
        assert!(
            plain.message.contains("does not use TLS"),
            "{}",
            plain.message
        );
        assert!(plain.message.contains("Disabled"));
    }

    #[test]
    fn connect_errors_never_show_a_password() {
        let url = "wss://admin:s3cret@db.example.com:8000/rpc";
        for error in [
            ConnectError::Timeout(Duration::from_secs(10)),
            ConnectError::Http(404),
            ConnectError::Io("Connection refused (os error 61)".into()),
            ConnectError::Io("failed to lookup address information".into()),
            ConnectError::Io("broken pipe".into()),
            ConnectError::Tls("received corrupt message of type InvalidContentType".into()),
            ConnectError::Tls("invalid peer certificate: UnknownIssuer".into()),
            ConnectError::InvalidUrl("invalid uri character".into()),
            ConnectError::Other("protocol error".into()),
        ] {
            let message = connect_error(url, &error).message;
            assert!(!message.contains("s3cret"), "{message}");
            assert!(!message.contains("admin"), "{message}");
            assert!(
                message.contains("wss://db.example.com:8000/rpc"),
                "{message}"
            );
        }
    }

    #[test]
    fn strips_the_user_name_and_password_from_urls() {
        for (url, shown) in [
            (
                "wss://user:pass@db.example.com/rpc",
                "wss://db.example.com/rpc",
            ),
            ("ws://reader@127.0.0.1:8000/rpc", "ws://127.0.0.1:8000/rpc"),
            // A password with `@` or `/` in it.
            ("ws://u:p@ss/w@rd@127.0.0.1:1/rpc", "ws://127.0.0.1:1/rpc"),
            // `user:pass@host` without a scheme, bracketed by `rpc_url`.
            ("ws://[user:pass@host]:8000/rpc", "ws://[host]:8000/rpc"),
            ("wss://db.example.com/rpc", "wss://db.example.com/rpc"),
            ("ws://[::1]:8000/rpc", "ws://[::1]:8000/rpc"),
            ("wss://user:pass@[::1]:8000/rpc", "wss://[::1]:8000/rpc"),
        ] {
            assert_eq!(without_userinfo(url), shown, "{url}");
        }
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
    fn auth_errors_say_how_to_send_a_namespace() {
        let failed = |levels: &[AuthLevel]| -> Vec<(AuthLevel, RpcError)> {
            levels
                .iter()
                .map(|l| (*l, server("There was a problem with authentication")))
                .collect()
        };
        // What the connection form sends without a connection string: no
        // namespace, so only the root level was tried.
        let no_namespace = SessionConfig {
            namespace: None,
            database: None,
            ..config()
        };
        let e = auth_error(&no_namespace, &failed(&[AuthLevel::Root])).message;
        assert!(
            e.starts_with("Authentication failed as root user `reader`."),
            "{e}"
        );
        assert!(e.contains("only a root user can sign in"), "{e}");
        assert!(
            e.contains("connection string that names them, `surrealdb://user:pass@host:8000/namespace/database`"),
            "{e}"
        );
        assert!(
            e.contains("in Namespace/Database on the connection's Databases tab"),
            "{e}"
        );
        // A namespace alone: a database user still needs its database.
        let namespace_only = SessionConfig {
            database: None,
            ..config()
        };
        let e = auth_error(
            &namespace_only,
            &failed(&[AuthLevel::Namespace, AuthLevel::Root]),
        )
        .message;
        assert!(e.contains("connect to `namespace/database`"), "{e}");
        // With both, the credentials are what is left to check.
        let e = auth_error(&config(), &failed(&[AuthLevel::Database])).message;
        assert!(
            e.contains("Check the username and password. (SurrealDB:"),
            "{e}"
        );
    }

    #[test]
    fn hints_do_not_name_a_database_field() {
        // Tabularis shows a Databases tab, not a Database field, for this
        // driver.
        let c = config();
        let no_namespace = SessionConfig {
            namespace: None,
            database: None,
            ..config()
        };
        let messages = [
            with_handshake_hint("The namespace 'nope' does not exist", &c, AuthLevel::Root),
            with_handshake_hint("The database 'nope' does not exist", &c, AuthLevel::Root),
            with_hint("Specify a namespace to use", &c, AuthLevel::Root),
            auth_error(
                &no_namespace,
                &[(
                    AuthLevel::Root,
                    server("There was a problem with authentication"),
                )],
            )
            .message,
        ];
        for m in &messages {
            assert!(!m.contains("Database field"), "{m}");
            assert!(!m.contains("Database set"), "{m}");
            assert!(m.contains("Databases tab"), "{m}");
        }
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
        let ns = with_handshake_hint("The namespace 'nope' does not exist", &c, AuthLevel::Root);
        assert!(ns.contains("Check the namespace"));
        assert!(ns.contains("never creates one"));
        let db = not_found("database", "ghost", &c, AuthLevel::Root);
        assert!(db
            .message
            .starts_with("The database 'ghost' does not exist. Check the database"));
        assert!(is_permission_denied(
            "Anonymous access not allowed: Not enough permissions to perform this action"
        ));
        assert!(!is_permission_denied("The namespace 'x' does not exist"));
        let plain = with_hint("Parse error: unexpected token", &c, AuthLevel::Root);
        assert_eq!(plain, "Parse error: unexpected token");
    }

    #[test]
    fn only_connecting_gets_the_not_found_hint() {
        let c = config();
        // A query typed in the editor names some other namespace or database.
        for message in [
            "The database 'ghost' does not exist",
            "The namespace 'ghost' does not exist",
        ] {
            assert_eq!(with_hint(message, &c, AuthLevel::Root), message);
            let connecting = with_handshake_hint(message, &c, AuthLevel::Root);
            assert!(
                connecting.starts_with(&format!("{message}. Check the ")),
                "{connecting}"
            );
            assert!(connecting.contains("never creates one"), "{connecting}");
        }
        // Other causes get the same hints either way.
        let perm = "IAM error: Not enough permissions to perform this action";
        assert_eq!(
            with_handshake_hint(perm, &c, AuthLevel::Database),
            with_hint(perm, &c, AuthLevel::Database)
        );
    }
}
