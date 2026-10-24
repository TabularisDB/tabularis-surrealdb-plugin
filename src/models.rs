//! Connection parameters sent by the host, and how they map onto a
//! SurrealDB endpoint, namespace, database and sign-in.
//!
//! Mapping (see README, "Connecting"):
//!
//! | Tabularis field | SurrealDB                                            |
//! |-----------------|------------------------------------------------------|
//! | Host            | host name, or a full `ws://` / `wss://` / `http(s)://` URL |
//! | Port            | port (default 8000); ignored when Host is a URL with a port |
//! | Database        | `namespace/database`, `namespace`, or empty          |
//! | Username        | system user name (root, namespace or database user)  |
//! | Password        | that user's password, or a JWT when Username is empty |
//!
//! `extra.auth_level` (`auto`, `root`, `namespace`, `database`, `token`,
//! `none`) can pin the sign-in level; `auto` is the default.
//!
//! TLS comes from the connection's SSL tab: `ssl_mode` (`disabled`,
//! `preferred`, `required`, `verify_ca`, `verify_identity`) and `ssl_ca`.
//! See [`TlsSettings`].

use serde_json::Value;

use crate::error::PluginError;

pub const DEFAULT_PORT: u16 = 8000;

/// Raw connection fields as sent by the host in `params.params`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionParams {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub auth_level: Option<String>,
    pub ssl_mode: Option<String>,
    pub ssl_ca: Option<String>,
    pub ssl_cert: Option<String>,
    pub ssl_key: Option<String>,
    /// Stable id of the Tabularis connection, when the host has one (it does
    /// not for "Test connection" on an unsaved connection).
    pub connection_id: Option<String>,
}

impl ConnectionParams {
    pub fn from_value(value: &Value) -> Self {
        let get_str = |k: &str| {
            value
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let port = value
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok());
        // `database` is a string, or an array for multi-database connections
        // (the host normally rewrites it to the selected entry first).
        let database = match value.get("database") {
            Some(Value::String(s)) => Some(s.trim().to_string()),
            Some(Value::Array(items)) => items.iter().find_map(Value::as_str).map(str::to_string),
            _ => None,
        }
        .filter(|s| !s.is_empty());
        let auth_level = value
            .get("extra")
            .and_then(|e| e.get("auth_level"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Self {
            host: get_str("host"),
            port,
            database,
            username: get_str("username"),
            // Passwords are not trimmed: whitespace can be significant.
            password: value
                .get("password")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            auth_level,
            ssl_mode: get_str("ssl_mode"),
            ssl_ca: get_str("ssl_ca"),
            ssl_cert: get_str("ssl_cert"),
            ssl_key: get_str("ssl_key"),
            connection_id: get_str("connection_id"),
        }
    }
}

/// How the plugin signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthLevel {
    /// Try database, then namespace, then root users (least privilege first).
    Auto,
    Root,
    Namespace,
    Database,
    /// `authenticate` with a JWT (from `Password`).
    Token,
    /// No sign-in: for servers started with `--unauthenticated`.
    Anonymous,
}

impl AuthLevel {
    pub fn label(self) -> &'static str {
        match self {
            AuthLevel::Auto => "auto",
            AuthLevel::Root => "root",
            AuthLevel::Namespace => "namespace",
            AuthLevel::Database => "database",
            AuthLevel::Token => "token",
            AuthLevel::Anonymous => "none",
        }
    }
}

/// Everything needed to open one SurrealDB session. Two requests with an
/// equal `SessionConfig` (including the Tabularis connection id) share a
/// session; anything else gets its own WebSocket connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionConfig {
    pub url: String,
    pub namespace: Option<String>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub auth: AuthLevel,
    pub tls: TlsSettings,
    pub connection_id: Option<String>,
}

/// How a `wss://` connection checks the server certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TlsVerify {
    /// Chain to a trusted root and match the host name (the default).
    #[default]
    Full,
    /// Chain to a trusted root, any host name (SSL mode "Verify CA").
    CaOnly,
    /// Encrypt without checking the certificate (SSL mode "Required").
    None,
}

/// TLS options for one connection. Trusted roots are the PEM file in
/// `ca_file` when set (and nothing else), or the bundled Mozilla roots.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TlsSettings {
    pub verify: TlsVerify,
    pub ca_file: Option<String>,
}

impl SessionConfig {
    pub fn from_params(params: &ConnectionParams) -> Result<Self, PluginError> {
        let (tls_on, verify) = ssl_mode(params.ssl_mode.as_deref())?;
        if params.ssl_cert.is_some() || params.ssl_key.is_some() {
            return Err(PluginError::invalid_params(
                "Client certificates are not supported: SurrealDB signs in with users or tokens. Clear the client certificate and key in the SSL tab.",
            ));
        }
        let mut url = rpc_url(params.host.as_deref(), params.port)?;
        let host_has_scheme = params.host.as_deref().is_some_and(|h| h.contains("://"));
        if tls_on && url.starts_with("ws://") {
            if host_has_scheme {
                return Err(PluginError::invalid_params(
                    "Host is a ws:// URL but the SSL mode asks for TLS. Use a wss:// URL, or set SSL mode to Disabled.",
                ));
            }
            url = format!("wss://{}", &url["ws://".len()..]);
        }
        let (namespace, database) = split_namespace_database(params.database.as_deref());
        let auth = resolve_auth_level(params)?;
        if matches!(auth, AuthLevel::Namespace) && namespace.is_none() {
            return Err(PluginError::invalid_params(
                "Namespace sign-in needs a namespace: set Database to `namespace` or `namespace/database`.",
            ));
        }
        if matches!(auth, AuthLevel::Database) && database.is_none() {
            return Err(PluginError::invalid_params(
                "Database sign-in needs both: set Database to `namespace/database`.",
            ));
        }
        Ok(Self {
            url,
            namespace,
            database,
            username: params.username.clone(),
            password: params.password.clone(),
            auth,
            tls: TlsSettings {
                verify,
                ca_file: params.ssl_ca.clone(),
            },
            connection_id: params.connection_id.clone(),
        })
    }

    /// `namespace/database` as shown to the user.
    pub fn target(&self) -> String {
        match (&self.namespace, &self.database) {
            (Some(ns), Some(db)) => format!("{ns}/{db}"),
            (Some(ns), None) => ns.clone(),
            _ => String::new(),
        }
    }
}

/// `ns/db` → (Some(ns), Some(db)); `ns` → (Some(ns), None); empty → (None, None).
/// Only the first `/` separates, so a database name may itself contain `/`.
pub fn split_namespace_database(value: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return (None, None);
    };
    match value.split_once('/') {
        Some((ns, db)) => {
            let ns = ns.trim();
            let db = db.trim();
            (
                (!ns.is_empty()).then(|| ns.to_string()),
                (!db.is_empty()).then(|| db.to_string()),
            )
        }
        None => (Some(value.to_string()), None),
    }
}

/// SSL mode → (use TLS for a bare host name, certificate check). Accepts the
/// MySQL-style names the host offers for this driver and the PostgreSQL
/// spellings.
fn ssl_mode(mode: Option<&str>) -> Result<(bool, TlsVerify), PluginError> {
    let mode = mode.map(|m| m.trim().to_ascii_lowercase().replace('-', "_"));
    Ok(match mode.as_deref() {
        None | Some("") | Some("disabled" | "disable" | "preferred" | "prefer" | "allow") => {
            (false, TlsVerify::Full)
        }
        Some("required" | "require") => (true, TlsVerify::None),
        Some("verify_ca") => (true, TlsVerify::CaOnly),
        Some("verify_identity" | "verify_full") => (true, TlsVerify::Full),
        Some(other) => {
            return Err(PluginError::invalid_params(format!(
                "Unknown SSL mode `{other}`."
            )))
        }
    })
}

fn resolve_auth_level(params: &ConnectionParams) -> Result<AuthLevel, PluginError> {
    let explicit = params
        .auth_level
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase());
    let level = match explicit.as_deref() {
        None | Some("") | Some("auto") => {
            match (params.username.is_some(), params.password.is_some()) {
                (false, false) => AuthLevel::Anonymous,
                (false, true) => AuthLevel::Token,
                _ => AuthLevel::Auto,
            }
        }
        Some("root") => AuthLevel::Root,
        Some("namespace" | "ns") => AuthLevel::Namespace,
        Some("database" | "db") => AuthLevel::Database,
        Some("token" | "jwt") => AuthLevel::Token,
        Some("none" | "anonymous") => AuthLevel::Anonymous,
        Some(other) => {
            return Err(PluginError::invalid_params(format!(
                "Unknown auth_level `{other}`. Use auto, root, namespace, database, token or none."
            )))
        }
    };
    if level == AuthLevel::Token && params.password.is_none() {
        return Err(PluginError::invalid_params(
            "Token sign-in needs the token in the Password field.",
        ));
    }
    Ok(level)
}

/// Build the WebSocket RPC endpoint from the Host and Port fields.
///
/// - `localhost` + 8000 → `ws://localhost:8000/rpc`
/// - `wss://db.example.com` → `wss://db.example.com/rpc` (port from the URL
///   or the scheme default; the Port field is not applied)
/// - `https://x.surreal.cloud` → `wss://x.surreal.cloud/rpc`
/// - a URL that already ends in `/rpc` is used as is.
pub fn rpc_url(host: Option<&str>, port: Option<u16>) -> Result<String, PluginError> {
    let host = host
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or("localhost");
    if let Some((scheme, rest)) = host.split_once("://") {
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "ws" | "http" => "ws",
            "wss" | "https" => "wss",
            other => {
                return Err(PluginError::invalid_params(format!(
                    "Unsupported scheme `{other}://` in Host. Use a host name, or a ws://, wss://, http:// or https:// URL."
                )))
            }
        };
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            return Err(PluginError::invalid_params("Host URL has no host name."));
        }
        let path = if rest.ends_with("/rpc") { "" } else { "/rpc" };
        return Ok(format!("{scheme}://{rest}{path}"));
    }
    if host.contains('/') {
        return Err(PluginError::invalid_params(
            "Host must be a host name or a full URL such as wss://db.example.com.",
        ));
    }
    let port = port.unwrap_or(DEFAULT_PORT);
    // Bracket bare IPv6 addresses.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("ws://{host}:{port}/rpc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_host_params() {
        let p = ConnectionParams::from_value(&json!({
            "driver": "surrealdb", "host": " localhost ", "port": 8000,
            "database": "app/main", "username": "root", "password": " pw ",
            "extra": { "auth_level": "root" }, "connection_id": "c1"
        }));
        assert_eq!(p.host.as_deref(), Some("localhost"));
        assert_eq!(p.database.as_deref(), Some("app/main"));
        assert_eq!(p.password.as_deref(), Some(" pw "));
        assert_eq!(p.auth_level.as_deref(), Some("root"));
        assert_eq!(p.connection_id.as_deref(), Some("c1"));

        let multi = ConnectionParams::from_value(&json!({ "database": ["a/b", "c/d"] }));
        assert_eq!(multi.database.as_deref(), Some("a/b"));
        let empty = ConnectionParams::from_value(&json!({ "database": "", "username": "" }));
        assert_eq!(empty.database, None);
        assert_eq!(empty.username, None);
    }

    #[test]
    fn splits_namespace_and_database() {
        assert_eq!(
            split_namespace_database(Some("app/main")),
            (Some("app".into()), Some("main".into()))
        );
        assert_eq!(
            split_namespace_database(Some("app")),
            (Some("app".into()), None)
        );
        assert_eq!(
            split_namespace_database(Some("app/")),
            (Some("app".into()), None)
        );
        assert_eq!(
            split_namespace_database(Some("a/b/c")),
            (Some("a".into()), Some("b/c".into()))
        );
        assert_eq!(split_namespace_database(Some("  ")), (None, None));
        assert_eq!(split_namespace_database(None), (None, None));
    }

    #[test]
    fn builds_rpc_urls() {
        assert_eq!(
            rpc_url(Some("localhost"), None).unwrap(),
            "ws://localhost:8000/rpc"
        );
        assert_eq!(
            rpc_url(None, Some(9000)).unwrap(),
            "ws://localhost:9000/rpc"
        );
        assert_eq!(
            rpc_url(Some("::1"), Some(8000)).unwrap(),
            "ws://[::1]:8000/rpc"
        );
        assert_eq!(
            rpc_url(Some("wss://db.example.com/"), Some(8000)).unwrap(),
            "wss://db.example.com/rpc"
        );
        assert_eq!(
            rpc_url(Some("https://x.surreal.cloud"), None).unwrap(),
            "wss://x.surreal.cloud/rpc"
        );
        assert_eq!(
            rpc_url(Some("http://127.0.0.1:8001/rpc"), None).unwrap(),
            "ws://127.0.0.1:8001/rpc"
        );
        assert!(rpc_url(Some("ftp://x"), None).is_err());
        assert!(rpc_url(Some("host/path"), None).is_err());
    }

    #[test]
    fn ssl_modes_select_tls_and_verification() {
        let cfg = |host: &str, mode: Option<&str>| {
            SessionConfig::from_params(&ConnectionParams {
                host: Some(host.into()),
                ssl_mode: mode.map(Into::into),
                ssl_ca: Some("/tmp/ca.pem".into()),
                ..Default::default()
            })
        };
        let c = cfg("localhost", None).unwrap();
        assert_eq!(c.url, "ws://localhost:8000/rpc");
        let c = cfg("localhost", Some("preferred")).unwrap();
        assert!(c.url.starts_with("ws://"));
        let c = cfg("localhost", Some("required")).unwrap();
        assert_eq!(c.url, "wss://localhost:8000/rpc");
        assert_eq!(c.tls.verify, TlsVerify::None);
        assert_eq!(c.tls.ca_file.as_deref(), Some("/tmp/ca.pem"));
        assert_eq!(
            cfg("localhost", Some("verify_ca")).unwrap().tls.verify,
            TlsVerify::CaOnly
        );
        assert_eq!(
            cfg("localhost", Some("verify-full")).unwrap().tls.verify,
            TlsVerify::Full
        );
        // A wss:// URL is TLS whatever the mode says; a ws:// URL conflicts
        // with a TLS mode.
        assert!(cfg("wss://db.example.com", None)
            .unwrap()
            .url
            .starts_with("wss://"));
        assert!(cfg("ws://db.example.com", Some("required")).is_err());
        assert!(cfg("localhost", Some("bogus")).is_err());
        let client_cert = SessionConfig::from_params(&ConnectionParams {
            ssl_mode: Some("required".into()),
            ssl_cert: Some("/tmp/c.pem".into()),
            ..Default::default()
        });
        assert!(client_cert
            .unwrap_err()
            .message
            .contains("Client certificates"));
    }

    #[test]
    fn auth_level_resolution() {
        let base = |u: Option<&str>, p: Option<&str>, level: Option<&str>, db: Option<&str>| {
            SessionConfig::from_params(&ConnectionParams {
                username: u.map(Into::into),
                password: p.map(Into::into),
                auth_level: level.map(Into::into),
                database: db.map(Into::into),
                ..Default::default()
            })
        };
        assert_eq!(
            base(Some("root"), Some("x"), None, None).unwrap().auth,
            AuthLevel::Auto
        );
        assert_eq!(
            base(None, Some("ey.jwt"), None, None).unwrap().auth,
            AuthLevel::Token
        );
        assert_eq!(
            base(None, None, None, None).unwrap().auth,
            AuthLevel::Anonymous
        );
        assert_eq!(
            base(Some("u"), Some("p"), Some("Root"), None).unwrap().auth,
            AuthLevel::Root
        );
        assert!(base(Some("u"), Some("p"), Some("namespace"), None).is_err());
        assert!(base(Some("u"), Some("p"), Some("database"), Some("ns")).is_err());
        assert!(base(Some("u"), Some("p"), Some("database"), Some("ns/db")).is_ok());
        assert!(base(Some("u"), None, Some("token"), None).is_err());
        assert!(base(Some("u"), Some("p"), Some("weird"), None).is_err());
    }
}
