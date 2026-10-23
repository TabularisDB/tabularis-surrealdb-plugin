//! The stdio loop: read JSON-RPC requests line by line, run each one as its
//! own task, write responses as they complete.
//!
//! Requests run concurrently, and stdin keeps being read while they run, so
//! a `cancel` notification from the host is seen immediately. Cancelling
//! aborts the request's task: the plugin stops waiting and sends no
//! response. The statement itself keeps running on the SurrealDB server;
//! see docs/DESIGN.md, "Cancellation".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};

use crate::error::{PluginError, INVALID_REQUEST, PARSE_ERROR};
use crate::handlers::Context;
use crate::rpc;

/// Run the plugin on the process's stdin/stdout until stdin closes.
pub fn run() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to start the async runtime");
    runtime.block_on(serve(tokio::io::stdin(), tokio::io::stdout()));
}

/// Serve JSON-RPC over any line-based reader/writer pair.
pub async fn serve<R, W>(input: R, output: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let ctx = Arc::new(Context::default());
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(line) = rx.recv().await {
            if output.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = output.flush().await;
        }
    });

    let inflight: Arc<Mutex<HashMap<String, AbortHandle>>> = Arc::default();
    let mut tasks = JoinSet::new();
    let mut lines = BufReader::new(input).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        while tasks.try_join_next().is_some() {}
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let error = PluginError {
                    code: PARSE_ERROR,
                    message: format!("parse error: {e}"),
                };
                send(&tx, rpc::error_response(Value::Null, &error));
                continue;
            }
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = request.get("id").cloned().filter(|id| !id.is_null());

        let (method, id) = match (method, id) {
            (Some(method), None) => {
                // Notifications get no response.
                if method == "cancel" {
                    if let Some(target) = request.pointer("/params/id") {
                        let handle = lock(&inflight).remove(&target.to_string());
                        if let Some(handle) = handle {
                            handle.abort();
                        }
                    }
                }
                continue;
            }
            (None, id) => {
                let error = PluginError {
                    code: INVALID_REQUEST,
                    message: "invalid request: missing method".to_string(),
                };
                send(&tx, rpc::error_response(id.unwrap_or(Value::Null), &error));
                continue;
            }
            (Some(method), Some(id)) => (method, id),
        };

        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let key = id.to_string();
        let task_ctx = ctx.clone();
        let task_tx = tx.clone();
        let task_inflight = inflight.clone();
        let task_key = key.clone();
        let handle = tasks.spawn(async move {
            let response = match rpc::dispatch(&task_ctx, &method, &params).await {
                Ok(result) => rpc::ok_response(id, result),
                Err(error) => rpc::error_response(id, &error),
            };
            lock(&task_inflight).remove(&task_key);
            send(&task_tx, response);
        });
        // The task cannot run before this insert: the runtime is
        // single-threaded and this loop has not yielded since the spawn.
        lock(&inflight).insert(key, handle);
    }

    // stdin closed: let in-flight requests finish (the host bounds this with
    // its shutdown grace period), then flush the remaining responses.
    while tasks.join_next().await.is_some() {}
    drop(tx);
    let _ = writer.await;
}

fn send(tx: &mpsc::UnboundedSender<String>, response: Value) {
    let mut line = serde_json::to_string(&response).unwrap_or_else(|e| {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":-32603,\"message\":\"serialization failed: {e}\"}},\"id\":null}}"
        )
    });
    line.push('\n');
    let _ = tx.send(line);
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn roundtrip(input: &str) -> Vec<Value> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (out_reader, out_writer) = tokio::io::duplex(64 * 1024);
        client.write_all(input.as_bytes()).await.unwrap();
        drop(client);
        serve(server, out_writer).await;
        let mut lines = BufReader::new(out_reader).lines();
        let mut out = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            out.push(serde_json::from_str(&line).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn answers_requests_and_ignores_notifications() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"settings\":{}},\"id\":1}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"cancel\",\"params\":{\"id\":99}}\n",
            "\n",
            "not json\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"insert_record\",\"params\":{},\"id\":2}\n",
        );
        let mut out = roundtrip(input).await;
        out.sort_by_key(|v| v["id"].as_i64().unwrap_or(0));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["error"]["code"], -32700);
        assert_eq!(out[1], json!({"jsonrpc": "2.0", "result": null, "id": 1}));
        assert_eq!(out[2]["error"]["code"], -32601);
    }
}
