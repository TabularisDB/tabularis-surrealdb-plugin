//! Minimal client for SurrealDB's WebSocket RPC protocol, CBOR encoded.
//!
//! One `RpcClient` is one WebSocket connection, and therefore one SurrealDB
//! session (auth, namespace, database). Requests are multiplexed by id, so
//! concurrent calls on the same client do not wait for each other.
//!
//! Protocol reference: <https://surrealdb.com/docs/reference/rest-api/rpc-protocol>
//! and <https://surrealdb.com/docs/reference/rest-api/cbor-protocol>.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ciborium::value::Value as Cbor;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// Failure of one RPC call.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    /// The connection was already closed; the request was never sent, so it
    /// is safe to retry on a new connection.
    NotConnected,
    /// The connection failed after the request was sent. The server may or
    /// may not have executed it.
    Transport(String),
    /// The server answered with an RPC error object.
    Server(ServerError),
    /// The response could not be decoded.
    Protocol(String),
}

/// An RPC-level error object (`{ code, message, kind?, details? }`). SurrealDB
/// 2.x sends `code` and `message`; 3.x adds `kind` and `details`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerError {
    pub code: i64,
    pub message: String,
    pub kind: Option<String>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::NotConnected => f.write_str("the connection to SurrealDB is closed"),
            RpcError::Transport(m) => write!(f, "connection to SurrealDB failed: {m}"),
            RpcError::Server(e) => f.write_str(&e.message),
            RpcError::Protocol(m) => write!(f, "unexpected response from SurrealDB: {m}"),
        }
    }
}

/// Why a WebSocket connection could not be opened.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectError {
    Timeout(Duration),
    /// HTTP status of a refused WebSocket upgrade.
    Http(u16),
    Io(String),
    Tls(String),
    InvalidUrl(String),
    Other(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Cbor, RpcError>>>>>;

pub struct RpcClient {
    outgoing: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl RpcClient {
    /// Open a WebSocket to `url` negotiating the `cbor` subprotocol.
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self, ConnectError> {
        let mut request = url
            .into_client_request()
            .map_err(|e| ConnectError::InvalidUrl(e.to_string()))?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("cbor"));

        let connect = tokio_tungstenite::connect_async(request);
        let (stream, _response) = match tokio::time::timeout(timeout, connect).await {
            Err(_) => return Err(ConnectError::Timeout(timeout)),
            Ok(Err(e)) => return Err(classify_connect_error(e)),
            Ok(Ok(ok)) => ok,
        };

        let (mut sink, mut source) = stream.split();
        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<Message>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let writer_closed = closed.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = outgoing_rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            writer_closed.store(true, Ordering::Release);
            let _ = sink.close().await;
        });

        let reader_pending = pending.clone();
        let reader_closed = closed.clone();
        let reader_outgoing = outgoing.clone();
        let reader = tokio::spawn(async move {
            let reason = loop {
                match source.next().await {
                    Some(Ok(Message::Binary(bytes))) => dispatch_response(&bytes, &reader_pending),
                    Some(Ok(Message::Ping(payload))) => {
                        // Make sure the pong is flushed even when no request
                        // is being written.
                        let _ = reader_outgoing.send(Message::Pong(payload));
                    }
                    Some(Ok(Message::Close(frame))) => {
                        break frame
                            .map(|f| format!("closed by server: {}", f.reason))
                            .unwrap_or_else(|| "closed by server".to_string());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break e.to_string(),
                    None => break "connection closed".to_string(),
                }
            };
            reader_closed.store(true, Ordering::Release);
            let waiting: Vec<_> = {
                let mut map = reader_pending.lock().unwrap_or_else(|e| e.into_inner());
                map.drain().map(|(_, tx)| tx).collect()
            };
            for tx in waiting {
                let _ = tx.send(Err(RpcError::Transport(reason.clone())));
            }
        });

        Ok(Self {
            outgoing,
            pending,
            next_id: AtomicU64::new(1),
            closed,
            reader,
            writer,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Send one RPC request and wait for its response. Dropping the returned
    /// future (e.g. on cancellation) forgets the request locally; the server
    /// still finishes it.
    pub async fn call(&self, method: &str, params: Vec<Cbor>) -> Result<Cbor, RpcError> {
        if self.is_closed() {
            return Err(RpcError::NotConnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Cbor::Map(vec![
            (Cbor::Text("id".into()), Cbor::Integer(id.into())),
            (Cbor::Text("method".into()), Cbor::Text(method.into())),
            (Cbor::Text("params".into()), Cbor::Array(params)),
        ]);
        let mut bytes = Vec::new();
        ciborium::into_writer(&request, &mut bytes)
            .map_err(|e| RpcError::Protocol(format!("could not encode request: {e}")))?;

        let (tx, rx) = oneshot::channel();
        self.lock_pending().insert(id, tx);
        let guard = PendingGuard {
            pending: &self.pending,
            id,
        };
        if self.outgoing.send(Message::Binary(bytes.into())).is_err() {
            return Err(RpcError::NotConnected);
        }
        let result = rx
            .await
            .unwrap_or_else(|_| Err(RpcError::Transport("connection closed".to_string())));
        drop(guard);
        result
    }

    fn lock_pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<Cbor, RpcError>>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Removes a request's slot when its caller stops waiting.
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&self.id);
    }
}

fn dispatch_response(bytes: &[u8], pending: &Pending) {
    let Ok(value) = ciborium::from_reader::<Cbor, _>(bytes) else {
        return;
    };
    let Some((id, outcome)) = parse_response(value) else {
        // Live-query notifications have no id; the plugin does not use them.
        return;
    };
    let tx = {
        let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&id)
    };
    if let Some(tx) = tx {
        let _ = tx.send(outcome);
    }
}

/// Split a response map into its id and result or error.
pub fn parse_response(value: Cbor) -> Option<(u64, Result<Cbor, RpcError>)> {
    let Cbor::Map(entries) = value else {
        return None;
    };
    let mut id = None;
    let mut result = None;
    let mut error = None;
    for (k, v) in entries {
        match k.as_text() {
            Some("id") => {
                id = match &v {
                    Cbor::Integer(i) => u64::try_from(i128::from(*i)).ok(),
                    Cbor::Text(t) => t.parse().ok(),
                    _ => None,
                }
            }
            Some("result") => result = Some(v),
            Some("error") => error = Some(v),
            _ => {}
        }
    }
    let id = id?;
    if let Some(error) = error {
        return Some((id, Err(RpcError::Server(parse_server_error(&error)))));
    }
    Some((id, Ok(result.unwrap_or(Cbor::Null))))
}

fn parse_server_error(value: &Cbor) -> ServerError {
    let mut out = ServerError {
        code: 0,
        message: String::new(),
        kind: None,
    };
    match value {
        Cbor::Map(entries) => {
            for (k, v) in entries {
                match (k.as_text(), v) {
                    (Some("code"), Cbor::Integer(i)) => {
                        out.code = i64::try_from(i128::from(*i)).unwrap_or_default()
                    }
                    (Some("message"), Cbor::Text(t)) => out.message = t.clone(),
                    (Some("kind"), Cbor::Text(t)) => out.kind = Some(t.clone()),
                    _ => {}
                }
            }
        }
        Cbor::Text(t) => out.message = t.clone(),
        _ => {}
    }
    if out.message.is_empty() {
        out.message = "unknown server error".to_string();
    }
    out
}

fn classify_connect_error(e: tokio_tungstenite::tungstenite::Error) -> ConnectError {
    use tokio_tungstenite::tungstenite::Error as WsError;
    match e {
        WsError::Http(response) => ConnectError::Http(response.status().as_u16()),
        WsError::Io(io) => ConnectError::Io(io.to_string()),
        WsError::Tls(tls) => ConnectError::Tls(tls.to_string()),
        WsError::Url(url) => ConnectError::InvalidUrl(url.to_string()),
        other => ConnectError::Other(other.to_string()),
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

    #[test]
    fn parses_success_responses() {
        let (id, out) = parse_response(map(vec![
            ("id", Cbor::Integer(7.into())),
            ("result", Cbor::Bool(true)),
        ]))
        .unwrap();
        assert_eq!(id, 7);
        assert_eq!(out.unwrap(), Cbor::Bool(true));
    }

    #[test]
    fn parses_2x_and_3x_error_objects() {
        let v2 = map(vec![
            ("id", Cbor::Text("3".into())),
            (
                "error",
                map(vec![
                    ("code", Cbor::Integer((-32000).into())),
                    (
                        "message",
                        Cbor::Text("There was a problem with authentication".into()),
                    ),
                ]),
            ),
        ]);
        let (id, out) = parse_response(v2).unwrap();
        assert_eq!(id, 3);
        assert_eq!(
            out.unwrap_err(),
            RpcError::Server(ServerError {
                code: -32000,
                message: "There was a problem with authentication".into(),
                kind: None
            })
        );

        let v3 = map(vec![
            ("id", Cbor::Integer(4.into())),
            (
                "error",
                map(vec![
                    ("code", Cbor::Integer((-32002).into())),
                    ("message", Cbor::Text("Not enough permissions".into())),
                    ("kind", Cbor::Text("NotAllowed".into())),
                ]),
            ),
        ]);
        let (_, out) = parse_response(v3).unwrap();
        match out.unwrap_err() {
            RpcError::Server(e) => assert_eq!(e.kind.as_deref(), Some("NotAllowed")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ignores_notifications_without_id() {
        assert!(parse_response(map(vec![("result", Cbor::Null)])).is_none());
        assert!(parse_response(Cbor::Null).is_none());
    }
}
