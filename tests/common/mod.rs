//! Shared helper for the live tests: one plugin process driven over
//! JSON-RPC stdio, as Tabularis does.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// One plugin process.
pub struct Plugin {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// Responses read while waiting for another id.
    stash: Vec<Value>,
}

impl Plugin {
    pub fn start() -> Self {
        let bin = std::env::var("SURREALDB_PLUGIN_BIN")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_surrealdb-plugin").to_string());
        let mut child = Command::new(&bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {bin}: {e}"));
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
            stash: Vec::new(),
        }
    }

    pub fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": id });
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let request = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Wait for the response to `id`.
    pub fn wait(&mut self, id: u64, timeout: Duration) -> Option<Value> {
        if let Some(pos) = self.stash.iter().position(|v| v["id"] == id) {
            return Some(self.stash.remove(pos));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let v: Value = serde_json::from_str(&line)
                        .unwrap_or_else(|e| panic!("plugin wrote non-JSON {line:?}: {e}"));
                    if v["id"] == id {
                        return Some(v);
                    }
                    self.stash.push(v);
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => panic!("plugin exited"),
            }
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        let id = self.send(method, params);
        let response = self
            .wait(id, Duration::from_secs(30))
            .unwrap_or_else(|| panic!("no response to {method}"));
        match response.get("error") {
            Some(e) => Err((
                e["code"].as_i64().unwrap_or_default(),
                e["message"].as_str().unwrap_or_default().to_string(),
            )),
            None => Ok(response["result"].clone()),
        }
    }

    pub fn ok(&mut self, method: &str, params: Value) -> Value {
        self.call(method, params.clone())
            .unwrap_or_else(|e| panic!("{method} {params} failed: {e:?}"))
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
