//! JSON-RPC method implementations, grouped by area.

pub mod metadata;
pub mod query;
pub mod results;
pub mod templates;

use serde_json::Value;

use crate::error::PluginError;
use crate::models::{ConnectionParams, SessionConfig};
use crate::surreal::Registry;

/// State shared by every request of one plugin process.
#[derive(Default)]
pub struct Context {
    pub sessions: Registry,
    pub settings: std::sync::RwLock<Settings>,
}

impl Context {
    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

/// Plugin settings from the host's `initialize` call (Settings → plugin).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// `query_timeout_seconds`; `None` (or 0) means no plugin timeout.
    pub query_timeout_secs: Option<u64>,
}

impl Settings {
    /// Read `params.settings`. Unknown keys are ignored; a number may also
    /// arrive as a string.
    pub fn from_initialize(params: &Value) -> Self {
        let timeout = params
            .get("settings")
            .and_then(|s| s.get("query_timeout_seconds"))
            .and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            })
            .filter(|t| t.is_finite() && *t >= 1.0)
            .map(|t| t.round() as u64);
        Self {
            query_timeout_secs: timeout,
        }
    }
}

/// The session configuration for the connection in `params.params`.
pub fn session_config(params: &Value) -> Result<SessionConfig, PluginError> {
    let connection = params.get("params").unwrap_or(&Value::Null);
    SessionConfig::from_params(&ConnectionParams::from_value(connection))
}

/// A required string parameter.
pub fn req_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, PluginError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::invalid_params(format!("missing string parameter `{key}`")))
}

/// The table name, accepted as `table` (current hosts) or `table_name`.
pub fn table_param(params: &Value) -> Result<&str, PluginError> {
    params
        .get("table")
        .or_else(|| params.get("table_name"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| PluginError::invalid_params("missing string parameter `table`"))
}

/// Metadata needs a selected database.
pub fn require_database(config: &SessionConfig) -> Result<(), PluginError> {
    if config.namespace.is_some() && config.database.is_some() {
        Ok(())
    } else {
        Err(PluginError::invalid_params(
            "No database selected. Set the connection's Database field to `namespace/database`, or pick a database in the explorer.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_query_timeout_setting() {
        let t = |v: Value| {
            Settings::from_initialize(&json!({ "settings": { "query_timeout_seconds": v } }))
        };
        assert_eq!(t(json!(30)).query_timeout_secs, Some(30));
        assert_eq!(t(json!("45")).query_timeout_secs, Some(45));
        assert_eq!(t(json!(0)).query_timeout_secs, None);
        assert_eq!(t(json!(-5)).query_timeout_secs, None);
        assert_eq!(t(json!(null)).query_timeout_secs, None);
        assert_eq!(
            Settings::from_initialize(&json!({})).query_timeout_secs,
            None
        );
    }
}
