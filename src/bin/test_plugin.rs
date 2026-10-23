//! Interactive REPL that drives the real plugin binary over JSON-RPC stdio,
//! the way Tabularis does.
//!
//! Usage: `just repl` or `cargo run --bin test_plugin`
//!
//! Connection settings come from the environment (defaults in brackets):
//! `SURREAL_HOST` [localhost], `SURREAL_PORT` [8000], `SURREAL_NS` [test],
//! `SURREAL_DB` [test], `SURREAL_USER` [root], `SURREAL_PASS` [root].
//! `SURREALDB_PLUGIN_BIN` overrides the plugin binary (default: the
//! `surrealdb-plugin` next to this executable).
//!
//! Commands:
//!   test                     test_connection
//!   databases                get_databases
//!   tables                   get_tables
//!   columns <table>          get_columns
//!   indexes <table>          get_indexes
//!   page <n> <sql>           execute_query, page n of 10 rows
//!   <any other text>         execute_query with that SurrealQL
//!   raw <method> <json>      any method; <json> is merged into params
//!   exit

use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn plugin_path() -> PathBuf {
    if let Ok(p) = std::env::var("SURREALDB_PLUGIN_BIN") {
        return PathBuf::from(p);
    }
    let mut p = std::env::current_exe().expect("current exe");
    p.set_file_name(format!("surrealdb-plugin{}", std::env::consts::EXE_SUFFIX));
    p
}

fn main() {
    let connection = json!({
        "driver": "surrealdb",
        "host": env_or("SURREAL_HOST", "localhost"),
        "port": env_or("SURREAL_PORT", "8000").parse::<u16>().unwrap_or(8000),
        "database": format!("{}/{}", env_or("SURREAL_NS", "test"), env_or("SURREAL_DB", "test")),
        "username": env_or("SURREAL_USER", "root"),
        "password": env_or("SURREAL_PASS", "root"),
        "connection_id": "repl",
    });

    let path = plugin_path();
    let mut child = Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            panic!(
                "cannot start {}: {e} (run `cargo build` first)",
                path.display()
            )
        });
    let mut plugin_in = child.stdin.take().expect("stdin");
    let mut plugin_out = BufReader::new(child.stdout.take().expect("stdout"));

    println!("test_plugin: driving {}", path.display());
    println!("connection: {connection}");
    println!("type `help` for commands");

    let mut next_id = 1u64;
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let (method, mut params) = match word {
            "exit" | "quit" => break,
            "help" => {
                println!("test | databases | tables | columns <t> | indexes <t> | page <n> <sql> | raw <method> <json> | <SurrealQL>");
                continue;
            }
            "test" => ("test_connection", json!({})),
            "databases" => ("get_databases", json!({})),
            "tables" => ("get_tables", json!({ "schema": null })),
            "columns" => ("get_columns", json!({ "table": rest, "schema": null })),
            "indexes" => ("get_indexes", json!({ "table": rest, "schema": null })),
            "page" => {
                let (n, sql) = rest.split_once(' ').unwrap_or(("1", rest));
                (
                    "execute_query",
                    json!({ "query": sql, "limit": 10, "page": n.parse::<u64>().unwrap_or(1) }),
                )
            }
            "raw" => {
                let (m, extra) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let extra: Value = serde_json::from_str(extra).unwrap_or_else(|_| json!({}));
                (Box::leak(m.to_string().into_boxed_str()) as &str, extra)
            }
            _ => (
                "execute_query",
                json!({ "query": line, "limit": null, "page": 1 }),
            ),
        };
        params["params"] = connection.clone();
        let request =
            json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": next_id });
        next_id += 1;

        writeln!(plugin_in, "{request}").expect("write to plugin");
        plugin_in.flush().ok();
        let mut response = String::new();
        if plugin_out.read_line(&mut response).unwrap_or(0) == 0 {
            eprintln!("plugin exited");
            break;
        }
        match serde_json::from_str::<Value>(&response) {
            Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or(response)),
            Err(_) => print!("{response}"),
        }
    }
    drop(plugin_in);
    let _ = child.wait();
}
