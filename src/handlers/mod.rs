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
