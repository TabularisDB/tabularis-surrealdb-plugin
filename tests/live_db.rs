//! Live integration test: drives the plugin binary over JSON-RPC stdio, as
//! Tabularis does, against a running SurrealDB.
//!
//! Skipped (passes with a notice) unless `SURREAL_URL` is set. CI starts a
//! server and sets:
//!
//!   SURREAL_URL=ws://127.0.0.1:8000 SURREAL_USER=root SURREAL_PASS=root
//!   SURREAL_NS=test SURREAL_DB=test
//!   SURREALDB_PLUGIN_BIN=target/debug/surrealdb-plugin   (optional)
//!
//! Locally:
//!
//!   surreal start memory --user root --pass root --bind 127.0.0.1:8000
//!   SURREAL_URL=ws://127.0.0.1:8000 SURREAL_USER=root SURREAL_PASS=root \
//!     SURREAL_NS=test SURREAL_DB=test cargo test --test live_db -- --test-threads=1
//!
//! The test seeds `fixture/seed.surql` into `<ns>/<db>` and creates
//! `<ns>/<db>_iso` for the isolation check.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SEED: &str = include_str!("../fixture/seed.surql");

struct Env {
    url: String,
    user: String,
    pass: String,
    ns: String,
    db: String,
}

fn env() -> Option<Env> {
    let url = std::env::var("SURREAL_URL").ok()?;
    let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    Some(Env {
        url,
        user: get("SURREAL_USER", "root"),
        pass: get("SURREAL_PASS", "root"),
        ns: get("SURREAL_NS", "test"),
        db: get("SURREAL_DB", "test"),
    })
}

/// Skip the test when no server is configured.
macro_rules! live {
    () => {
        match env() {
            Some(e) => {
                seed_once(&e);
                e
            }
            None => {
                eprintln!("SURREAL_URL not set: skipping live SurrealDB test");
                return;
            }
        }
    };
}

/// One plugin process.
struct Plugin {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// Responses read while waiting for another id.
    stash: Vec<Value>,
}

impl Plugin {
    fn start() -> Self {
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

    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": id });
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        let request = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Wait for the response to `id`.
    fn wait(&mut self, id: u64, timeout: Duration) -> Option<Value> {
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

    fn call(&mut self, method: &str, params: Value) -> Result<Value, (i64, String)> {
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

    fn ok(&mut self, method: &str, params: Value) -> Value {
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

fn connection(e: &Env, database: &str, connection_id: &str) -> Value {
    json!({
        "driver": "surrealdb",
        "host": e.url,
        "port": null,
        "database": database,
        "username": e.user,
        "password": e.pass,
        "connection_id": connection_id,
    })
}

fn main_db(e: &Env) -> String {
    format!("{}/{}", e.ns, e.db)
}

fn iso_db(e: &Env) -> String {
    format!("{}/{}_iso", e.ns, e.db)
}

fn query(p: &mut Plugin, conn: &Value, sql: &str) -> Value {
    p.ok(
        "execute_query",
        json!({ "params": conn, "query": sql, "limit": null, "page": 1 }),
    )
}

fn seed_once(e: &Env) {
    static SEEDED: OnceLock<()> = OnceLock::new();
    SEEDED.get_or_init(|| {
        let mut p = Plugin::start();
        // Root connection without a namespace: the script selects its own.
        let root = connection(e, "", "seed");
        let ns = &e.ns;
        let db = &e.db;
        let script = format!(
            "DEFINE NAMESPACE IF NOT EXISTS `{ns}`; USE NS `{ns}`;\n\
             DEFINE DATABASE IF NOT EXISTS `{db}`; DEFINE DATABASE IF NOT EXISTS `{db}_iso`;\n\
             USE DB `{db}`;\n{SEED}\n\
             CREATE marker:here SET db = 'main';\n\
             USE DB `{db}_iso`; REMOVE TABLE IF EXISTS marker; CREATE marker:here SET db = 'iso';"
        );
        query(&mut p, &root, &script);
    });
}

fn column_names(result: &Value) -> Vec<String> {
    result["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect()
}

/// Value of `column` in `row`.
fn cell<'a>(result: &'a Value, row: usize, column: &str) -> &'a Value {
    let idx = column_names(result)
        .iter()
        .position(|c| c == column)
        .unwrap_or_else(|| panic!("no column {column} in {result}"));
    &result["rows"][row][idx]
}

#[test]
fn connects_and_authenticates() {
    let e = live!();
    let mut p = Plugin::start();

    let ok = p.ok(
        "test_connection",
        json!({ "params": connection(&e, &main_db(&e), "c") }),
    );
    assert_eq!(ok["success"], true);
    assert!(ok["server_version"]
        .as_str()
        .unwrap()
        .starts_with("surrealdb-"));
    eprintln!("server: {}", ok["server_version"]);

    // Wrong password: every sign-in level is named in the error.
    let mut bad = connection(&e, &main_db(&e), "bad");
    bad["password"] = json!("definitely-wrong");
    let (_, message) = p
        .call("test_connection", json!({ "params": bad }))
        .unwrap_err();
    assert!(
        message.starts_with("Authentication failed as database user"),
        "{message}"
    );
    assert!(message.contains("root user"), "{message}");

    // A database user signs in at database level.
    let mut reader = connection(&e, &main_db(&e), "reader");
    reader["username"] = json!("reader");
    reader["password"] = json!("reader-pass");
    let ok = p.ok("test_connection", json!({ "params": reader }));
    assert_eq!(ok["signed_in_as"], "database");

    // ...can read, but not change the schema: the error says who is signed
    // in. (SurrealDB 3.2 turns a VIEWER's data writes into silent no-ops,
    // so DDL is used to provoke the permission error.)
    let rows = query(&mut p, &reader, "SELECT * FROM person:alice");
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
    let (_, message) = p
        .call(
            "execute_query",
            json!({ "params": reader, "query": "DEFINE TABLE not_allowed SCHEMALESS", "page": 1 }),
        )
        .unwrap_err();
    assert!(message.contains("Not enough permissions"), "{message}");
    assert!(
        message.contains("signed in as database user `reader`"),
        "{message}"
    );

    // ...and cannot sign in to another database.
    let mut elsewhere = reader.clone();
    elsewhere["database"] = json!(iso_db(&e));
    elsewhere["connection_id"] = json!("elsewhere");
    let (_, message) = p
        .call("test_connection", json!({ "params": elsewhere }))
        .unwrap_err();
    assert!(message.starts_with("Authentication failed"), "{message}");

    // Explicit auth level.
    let mut root = connection(&e, &main_db(&e), "root-explicit");
    root["extra"] = json!({ "auth_level": "root" });
    assert_eq!(
        p.ok("test_connection", json!({ "params": root }))["signed_in_as"],
        "root"
    );
}

/// Does `name` exist under `key` of `INFO FOR <level>`, asked on `conn`?
fn defined(p: &mut Plugin, conn: &Value, level: &str, key: &str, name: &str) -> bool {
    let r = query(
        p,
        conn,
        &format!("RETURN '{name}' IN object::keys((INFO FOR {level}).{key})"),
    );
    r["rows"][0][0]
        .as_bool()
        .unwrap_or_else(|| panic!("INFO FOR {level}: {r}"))
}

#[test]
fn missing_namespace_or_database_is_refused_and_not_created() {
    let e = live!();
    let mut p = Plugin::start();
    let main = connection(&e, &main_db(&e), "ghost-check");
    // An EDITOR on the namespace may define databases, so on 3.x a plain
    // `use` would create the missing database for it.
    query(
        &mut p,
        &main,
        "DEFINE USER IF NOT EXISTS ns_editor ON NAMESPACE PASSWORD 'ns-editor-pass' ROLES EDITOR",
    );

    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ghost_ns = format!("ghost_ns_{suffix}");
    let ghost_db = format!("ghost_db_{suffix}");
    let ns_missing = format!("The namespace '{ghost_ns}' does not exist");
    let db_missing = format!("The database '{ghost_db}' does not exist");

    let mut ns_editor = connection(&e, &format!("{}/{ghost_db}", e.ns), "ns-editor");
    ns_editor["username"] = json!("ns_editor");
    ns_editor["password"] = json!("ns-editor-pass");
    let mut root_pinned = connection(&e, &format!("{ghost_ns}/{}", e.db), "root-pinned");
    root_pinned["extra"] = json!({ "auth_level": "root" });

    let cases = [
        (
            connection(&e, &format!("{ghost_ns}/{}", e.db), "g1"),
            &ns_missing,
        ),
        (root_pinned, &ns_missing),
        (connection(&e, &ghost_ns, "g2"), &ns_missing),
        (
            connection(&e, &format!("{}/{ghost_db}", e.ns), "g3"),
            &db_missing,
        ),
        (ns_editor, &db_missing),
    ];
    for (conn, expected) in &cases {
        // "Test connection", and the cached session other requests use.
        for (method, params) in [
            ("test_connection", json!({ "params": conn })),
            ("ping", json!({ "params": conn })),
            (
                "execute_query",
                json!({ "params": conn, "query": "INFO FOR DB", "page": 1 }),
            ),
            ("get_databases", json!({ "params": conn })),
        ] {
            let (_, message) = p.call(method, params).unwrap_err();
            assert!(
                message.starts_with(expected.as_str()) && message.contains("never creates"),
                "{method} on {}: {message}",
                conn["database"]
            );
        }
    }

    // Nothing was created on the server.
    assert!(defined(&mut p, &main, "ROOT", "namespaces", &e.ns));
    assert!(!defined(&mut p, &main, "ROOT", "namespaces", &ghost_ns));
    assert!(defined(&mut p, &main, "NS", "databases", &e.db));
    assert!(!defined(&mut p, &main, "NS", "databases", &ghost_db));
}

#[test]
fn unreachable_server_is_reported() {
    let e = live!();
    let mut p = Plugin::start();
    let mut conn = connection(&e, &main_db(&e), "nowhere");
    conn["host"] = json!("127.0.0.1");
    conn["port"] = json!(1);
    let (_, message) = p
        .call("test_connection", json!({ "params": conn }))
        .unwrap_err();
    assert!(message.contains("ws://127.0.0.1:1/rpc"), "{message}");
}

#[test]
fn discovers_databases_tables_columns_and_indexes() {
    let e = live!();
    let mut p = Plugin::start();
    let conn = connection(&e, &main_db(&e), "meta");

    let dbs = p.ok("get_databases", json!({ "params": conn }));
    let dbs: Vec<&str> = dbs
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap())
        .collect();
    assert!(dbs.contains(&main_db(&e).as_str()), "{dbs:?}");
    assert!(dbs.contains(&iso_db(&e).as_str()), "{dbs:?}");

    let tables = p.ok("get_tables", json!({ "params": conn, "schema": null }));
    let find = |name: &str| {
        tables
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("no table {name} in {tables}"))
            .clone()
    };
    assert_eq!(
        find("person")["comment"],
        "People (schemafull) · SCHEMAFULL"
    );
    assert_eq!(find("knows")["comment"], "SCHEMALESS relation");
    find("odd table");

    // SCHEMAFULL: declared top-level fields, nested ones folded into their parent.
    let cols = p.ok(
        "get_columns",
        json!({ "params": conn, "table": "person", "schema": null }),
    );
    let names: Vec<&str> = cols
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["id", "address", "age", "balance", "name", "tags"]);
    let age = &cols[2];
    assert!(age["data_type"].as_str().unwrap().contains("int"));
    assert_eq!(age["is_nullable"], true);
    assert_eq!(cols[4]["comment"], "Display name");

    // SCHEMALESS: declared `kind` plus keys sampled from the records.
    let cols = p.ok(
        "get_columns",
        json!({ "params": conn, "table": "event", "schema": null }),
    );
    let by_name = |n: &str| {
        cols.as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .unwrap_or_else(|| panic!("no column {n} in {cols}"))
            .clone()
    };
    assert_eq!(by_name("kind")["data_type"], "string");
    assert_eq!(by_name("actor")["data_type"], "record");
    assert_eq!(by_name("payload")["data_type"], "object");
    assert_eq!(by_name("exact")["data_type"], "decimal");
    assert_eq!(by_name("at")["data_type"], "datetime");
    assert_eq!(by_name("nothing")["data_type"], "null");
    assert!(by_name("extra")["comment"]
        .as_str()
        .unwrap()
        .contains("inferred"));

    // Relation tables always show in/out.
    let cols = p.ok(
        "get_columns",
        json!({ "params": conn, "table": "knows", "schema": null }),
    );
    let names: Vec<&str> = cols
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        names.starts_with(&["id"])
            && names.contains(&"in")
            && names.contains(&"out")
            && names.contains(&"since")
    );

    let idx = p.ok(
        "get_indexes",
        json!({ "params": conn, "table": "person", "schema": null }),
    );
    let idx = idx.as_array().unwrap();
    assert!(idx.iter().any(|i| i["name"] == "person_name"
        && i["column_name"] == "name"
        && i["is_unique"] == true));
    assert!(idx.iter().any(|i| i["name"] == "person_city_age"
        && i["column_name"] == "age"
        && i["seq_in_index"] == 2));

    let (_, message) = p
        .call(
            "get_columns",
            json!({ "params": conn, "table": "missing", "schema": null }),
        )
        .unwrap_err();
    assert!(message.contains("does not exist"), "{message}");

    let batch = p.ok(
        "get_all_columns_batch",
        json!({ "params": conn, "schema": null }),
    );
    assert!(batch["person"].as_array().unwrap().len() >= 6);
}

#[test]
fn browses_tables_with_limit_and_offset() {
    let e = live!();
    let mut p = Plugin::start();
    let conn = connection(&e, &main_db(&e), "browse");

    // The exact query the host builds for a table tab: `SELECT * FROM <quoted>`.
    let mut seen = Vec::new();
    for (page, expected, more) in [(1, 10, true), (2, 10, true), (3, 5, false)] {
        let r = p.ok(
            "execute_query",
            json!({ "params": conn, "query": "SELECT * FROM `item`", "limit": 10, "page": page }),
        );
        assert_eq!(r["rows"].as_array().unwrap().len(), expected, "page {page}");
        assert_eq!(r["pagination"]["has_more"], more, "page {page}");
        assert_eq!(r["pagination"]["page"], page);
        for i in 0..expected {
            seen.push(cell(&r, i, "id").as_str().unwrap().to_string());
        }
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 25);

    // Host filter/sort clauses and its LIMIT wrapper.
    let r = p.ok(
        "execute_query",
        json!({ "params": conn,
            "query": "SELECT * FROM (SELECT * FROM `item` WHERE n > 20 ORDER BY n DESC LIMIT 3) AS limited_subset",
            "limit": 2, "page": 1 }),
    );
    assert_eq!(r["rows"].as_array().unwrap().len(), 2);
    assert_eq!(cell(&r, 0, "n"), 25);
    assert_eq!(r["pagination"]["has_more"], true);

    // A trailing line comment does not swallow the pagination clause.
    let r = p.ok(
        "execute_query",
        json!({ "params": conn, "query": "SELECT * FROM item -- all items", "limit": 10, "page": 1 }),
    );
    assert_eq!(r["rows"].as_array().unwrap().len(), 10);
    assert_eq!(r["pagination"]["has_more"], true);

    // A table name that needs quoting.
    let r = p.ok(
        "execute_query",
        json!({ "params": conn, "query": "SELECT * FROM `odd table`", "limit": 50, "page": 1 }),
    );
    assert_eq!(cell(&r, 0, "odd field"), "y");
    assert_eq!(cell(&r, 0, "id"), "`odd table`:x");

    // Templates from get_table_query_template are valid SurrealQL.
    for kind in ["select", "delete"] {
        let sql = p.ok(
            "get_table_query_template",
            json!({ "params": conn, "request": { "table": "odd table", "kind": kind, "columns": [], "limit": if kind == "select" { json!(5) } else { Value::Null } } }),
        );
        query(&mut p, &conn, sql.as_str().unwrap());
    }
    let r = query(&mut p, &conn, "SELECT * FROM `odd table`");
    assert_eq!(
        r["rows"].as_array().unwrap().len(),
        1,
        "the DELETE template must not match rows"
    );
}

#[test]
fn converts_values_and_keeps_record_ids_usable() {
    let e = live!();
    let mut p = Plugin::start();
    let conn = connection(&e, &main_db(&e), "values");

    let r = query(&mut p, &conn, "SELECT * FROM event:1");
    assert_eq!(cell(&r, 0, "id"), "event:1");
    assert_eq!(cell(&r, 0, "actor"), "person:alice");
    assert_eq!(cell(&r, 0, "big"), "9223372036854775807");
    assert_eq!(cell(&r, 0, "exact"), "0.1");
    assert_eq!(cell(&r, 0, "ratio"), 1.5);
    assert_eq!(cell(&r, 0, "at"), "2024-01-02T03:04:05.123456789Z");
    assert_eq!(cell(&r, 0, "uid"), "0190d6c0-0000-7000-8000-000000000001");
    assert_eq!(cell(&r, 0, "took"), "1h30m");
    assert_eq!(cell(&r, 0, "nothing"), &Value::Null);
    // Nested NULL stays, nested objects/arrays are JSON.
    assert_eq!(
        cell(&r, 0, "payload"),
        &json!({ "steps": [1, 2, { "ok": true, "note": null }], "meta": {} })
    );

    let r = query(&mut p, &conn, "SELECT balance FROM person:alice");
    assert_eq!(cell(&r, 0, "balance"), "12.345678901234567890123");

    // Every record id the plugin shows can be pasted back into a query.
    let r = query(&mut p, &conn, "SELECT id, kind FROM event");
    let ids: Vec<String> = (0..r["rows"].as_array().unwrap().len())
        .map(|i| cell(&r, i, "id").as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 4);
    for id in &ids {
        let back = query(&mut p, &conn, &format!("SELECT id FROM {id}"));
        assert_eq!(
            cell(&back, 0, "id").as_str(),
            Some(id.as_str()),
            "round trip of {id}"
        );
    }

    // Heterogeneous records: the union of keys, missing keys are null.
    let r = query(&mut p, &conn, "SELECT * FROM event ORDER BY kind");
    let cols = column_names(&r);
    for c in ["id", "kind", "extra", "payload", "big"] {
        assert!(cols.contains(&c.to_string()), "{c} missing from {cols:?}");
    }
}

#[test]
fn runs_native_surrealql() {
    let e = live!();
    let mut p = Plugin::start();
    let conn = connection(&e, &main_db(&e), "native");

    // Graph traversal over the seeded relation.
    let r = query(
        &mut p,
        &conn,
        "SELECT name, ->knows->person.name AS friends FROM person:alice",
    );
    assert_eq!(cell(&r, 0, "friends"), &json!(["Bob"]));
    let r = query(
        &mut p,
        &conn,
        "SELECT <-knows<-person<-knows<-person.name AS two_hops FROM person:carol",
    );
    assert_eq!(cell(&r, 0, "two_hops"), &json!(["Alice"]));

    // Scalars, objects, mixed arrays and multi-statement queries.
    let r = query(&mut p, &conn, "RETURN 1 + 1");
    assert_eq!(r["columns"], json!(["result"]));
    assert_eq!(r["rows"], json!([[2]]));
    let r = query(&mut p, &conn, "RETURN [1, { a: 1 }, 'x', NONE, NULL]");
    assert_eq!(r["rows"], json!([[1], [{ "a": 1 }], ["x"], [null], [null]]));
    let r = query(&mut p, &conn, "INFO FOR DB");
    assert!(column_names(&r).contains(&"tables".to_string()));
    let r = query(
        &mut p,
        &conn,
        "LET $n = 2; SELECT * FROM item WHERE n <= $n ORDER BY n; RETURN 'done';",
    );
    assert_eq!(r["columns"], json!([]), "LET returns NONE");
    let extra = r["additional_results"].as_array().unwrap();
    assert_eq!(extra[0]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(extra[1]["rows"], json!([["done"]]));

    // Errors name the failing statement.
    let (_, message) = p
        .call(
            "execute_query",
            json!({ "params": conn, "query": "RETURN 1; SELECT * FROM;", "page": 1 }),
        )
        .unwrap_err();
    assert!(!message.is_empty());
    let (_, message) = p
        .call(
            "execute_query",
            json!({ "params": conn, "query": "RETURN 1; THROW 'boom'", "page": 1 }),
        )
        .unwrap_err();
    assert!(message.starts_with("Statement 2 of 2 failed"), "{message}");
    assert!(message.contains("boom"), "{message}");

    // Writes are not supported from the grid.
    let (code, message) = p
        .call(
            "insert_record",
            json!({ "params": conn, "table": "person", "data": {} }),
        )
        .unwrap_err();
    assert_eq!(code, -32601);
    assert!(message.starts_with("Method not found: insert_record"));
}

#[test]
fn connections_do_not_share_namespace_or_database() {
    let e = live!();
    let mut p = Plugin::start();
    let a = connection(&e, &main_db(&e), "conn-a");
    let b = connection(&e, &iso_db(&e), "conn-b");

    // Interleave requests on both connections in one plugin process.
    let ids: Vec<(u64, &str)> = (0..5)
        .flat_map(|_| {
            let ia = p.send(
                "execute_query",
                json!({ "params": a, "query": "SELECT db FROM marker:here", "page": 1 }),
            );
            let ib = p.send(
                "execute_query",
                json!({ "params": b, "query": "SELECT db FROM marker:here", "page": 1 }),
            );
            [(ia, "main"), (ib, "iso")]
        })
        .collect();
    for (id, expected) in ids {
        let r = p.wait(id, Duration::from_secs(30)).expect("response");
        assert_eq!(r["result"]["rows"][0][0], expected, "{r}");
    }

    // A USE inside a query applies to that query only.
    let target = format!("{}_iso", e.db);
    let r = query(
        &mut p,
        &a,
        &format!("USE DB `{target}`; SELECT db FROM marker:here"),
    );
    assert_eq!(r["additional_results"][0]["rows"][0][0], "iso");
    let r = query(&mut p, &a, "SELECT db FROM marker:here");
    assert_eq!(r["rows"][0][0], "main");
}

#[test]
fn cancel_stops_waiting_and_keeps_the_plugin_responsive() {
    let e = live!();
    let mut p = Plugin::start();
    let conn = connection(&e, &main_db(&e), "cancel");
    p.ok("ping", json!({ "params": conn }));

    let slow = p.send(
        "execute_query",
        json!({ "params": conn, "query": "RETURN sleep(2s)", "page": 1 }),
    );
    std::thread::sleep(Duration::from_millis(200));
    p.notify("cancel", json!({ "id": slow }));
    // Unknown ids are ignored.
    p.notify("cancel", json!({ "id": 987654 }));

    let started = Instant::now();
    p.ok("ping", json!({ "params": conn }));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "ping waited for the cancelled query"
    );
    assert!(
        p.wait(slow, Duration::from_secs(4)).is_none(),
        "a cancelled request must not get a response"
    );
    // The shared session still works after the cancellation.
    let r = query(&mut p, &conn, "RETURN 'still here'");
    assert_eq!(r["rows"], json!([["still here"]]));
}
