//! SurrealDB access: the RPC client, sessions, and the per-connection
//! session registry.

pub mod client;
pub mod errors;
pub mod session;
pub mod tls;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::error::PluginError;
use crate::models::SessionConfig;
use crate::surreal::session::Session;

type Slot = Arc<Mutex<Option<Arc<Session>>>>;

/// Keeps one live session per Tabularis connection and selected
/// namespace/database.
///
/// The key is the full [`SessionConfig`], which includes the host's
/// connection id: two Tabularis connections never share a WebSocket, even
/// when every other field is equal, so their sign-in, namespace, database
/// and session variables cannot leak into each other.
#[derive(Default)]
pub struct Registry {
    slots: std::sync::Mutex<HashMap<SessionConfig, Slot>>,
}

impl Registry {
    /// The cached session for `config`, opened on first use. Requests
    /// without a connection id (e.g. "Test connection" on an unsaved
    /// connection) always get a fresh, uncached session.
    pub async fn session(&self, config: &SessionConfig) -> Result<Arc<Session>, PluginError> {
        if config.connection_id.is_none() {
            return Ok(Arc::new(Session::open(config).await?));
        }
        let slot = self.slot(config);
        let mut guard = slot.lock().await;
        if let Some(session) = guard.as_ref() {
            if !session.is_closed() {
                return Ok(session.clone());
            }
        }
        let session = Arc::new(Session::open(config).await?);
        *guard = Some(session.clone());
        Ok(session)
    }

    /// Forget the cached session so the next request reconnects.
    pub async fn invalidate(&self, config: &SessionConfig) {
        let slot = {
            let slots = self.lock_slots();
            slots.get(config).cloned()
        };
        if let Some(slot) = slot {
            *slot.lock().await = None;
        }
    }

    /// Run `f` on the session for `config`. If the session turns out to be
    /// unusable before the request reached the server (closed socket,
    /// expired session), reconnect once and run `f` again; a request that may
    /// have executed is never retried.
    pub async fn with_session<T, F, Fut>(
        &self,
        config: &SessionConfig,
        f: F,
    ) -> Result<T, PluginError>
    where
        F: Fn(Arc<Session>) -> Fut,
        Fut: Future<Output = Result<T, Retry<PluginError>>>,
    {
        let session = self.session(config).await?;
        match f(session).await {
            Ok(v) => Ok(v),
            Err(Retry::Final(e)) => Err(e),
            Err(Retry::Reconnect(_)) => {
                self.invalidate(config).await;
                let session = self.session(config).await?;
                f(session).await.map_err(Retry::into_inner)
            }
        }
    }

    fn slot(&self, config: &SessionConfig) -> Slot {
        let mut slots = self.lock_slots();
        // A connection whose address or credentials changed leaves stale
        // sessions behind; drop them. Other namespaces/databases of the same
        // connection (multi-database selection) are kept.
        slots.retain(|key, _| {
            key.connection_id != config.connection_id
                || (key.url == config.url
                    && key.username == config.username
                    && key.password == config.password
                    && key.auth == config.auth)
        });
        slots.entry(config.clone()).or_default().clone()
    }

    fn lock_slots(&self) -> std::sync::MutexGuard<'_, HashMap<SessionConfig, Slot>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Error wrapper telling [`Registry::with_session`] whether reconnecting and
/// retrying is safe.
#[derive(Debug)]
pub enum Retry<E> {
    Reconnect(E),
    Final(E),
}

impl<E> Retry<E> {
    pub fn into_inner(self) -> E {
        match self {
            Retry::Reconnect(e) | Retry::Final(e) => e,
        }
    }
}

impl From<PluginError> for Retry<PluginError> {
    fn from(e: PluginError) -> Self {
        Retry::Final(e)
    }
}

/// Classify an RPC failure: only failures that happened before the request
/// reached the server, or a session the server rejected as expired, are
/// retried.
pub fn retry_class(session: &Session, error: client::RpcError) -> Retry<PluginError> {
    let retry = match &error {
        client::RpcError::NotConnected => true,
        client::RpcError::Server(e) => e.message.to_ascii_lowercase().contains("expired"),
        _ => false,
    };
    let mapped = session.rpc_error(error);
    if retry {
        Retry::Reconnect(mapped)
    } else {
        Retry::Final(mapped)
    }
}
