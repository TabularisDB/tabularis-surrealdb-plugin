//! Live TLS test: `wss://` against a SurrealDB server with a certificate
//! from a throwaway private CA.
//!
//! Self-contained: it generates the CA and a server certificate (valid for
//! the name `localhost` only) with the `openssl` CLI, starts
//! `surreal start memory` with `--web-crt/--web-key` on a free port, and
//! stops it at the end. Skipped (passes with a notice) when `surreal` or
//! `openssl` is not on PATH.
//!
//!   cargo test --test live_tls

mod common;

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::Plugin;
use serde_json::{json, Value};

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn openssl(dir: &Path, args: &[&str]) {
    let status = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run openssl");
    assert!(status.success(), "openssl {args:?} failed");
}

/// CA + server certificate for `DNS:localhost` in `dir`.
fn make_certificates(dir: &Path) {
    openssl(
        dir,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-subj",
            "/CN=Tabularis SurrealDB test CA",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign",
        ],
    );
    openssl(
        dir,
        &[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
        ],
    );
    std::fs::write(
        dir.join("server.ext"),
        "subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n",
    )
    .unwrap();
    openssl(
        dir,
        &[
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "2",
            "-extfile",
            "server.ext",
            "-out",
            "server.pem",
        ],
    );
}

struct Server {
    child: Child,
    dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_tls_server() -> (Server, u16) {
    let dir =
        std::env::temp_dir().join(format!("surrealdb-plugin-live-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    make_certificates(&dir);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new("surreal")
        .args([
            "start", "memory", "--user", "root", "--pass", "root", "--log", "error",
        ])
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--web-crt")
        .arg(dir.join("server.pem"))
        .arg("--web-key")
        .arg(dir.join("server.key"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start surreal");
    let server = Server { child, dir };
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "surreal did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    (server, port)
}

fn connection(host: &str, port: u16, ssl_mode: Option<&str>, ca: Option<&Path>) -> Value {
    let mut c = json!({
        "driver": "surrealdb", "host": host, "port": port, "database": "",
        "username": "root", "password": "root",
    });
    if let Some(mode) = ssl_mode {
        c["ssl_mode"] = json!(mode);
    }
    if let Some(ca) = ca {
        c["ssl_ca"] = json!(ca.to_string_lossy());
    }
    c
}

#[test]
fn wss_with_private_ca_and_ssl_modes() {
    if !have("surreal") || !have("openssl") {
        eprintln!("`surreal` or `openssl` not on PATH: skipping live TLS test");
        return;
    }
    let (server, port) = start_tls_server();
    let ca = server.dir.join("ca.pem");
    let mut p = Plugin::start();
    let test = |p: &mut Plugin, conn: Value| p.call("test_connection", json!({ "params": conn }));

    // Verify Identity with the private CA: name and chain check out.
    let ok = test(
        &mut p,
        connection("localhost", port, Some("verify_identity"), Some(&ca)),
    )
    .unwrap();
    assert_eq!(ok["success"], true);
    // A wss:// URL defaults to full verification.
    let url = format!("wss://localhost:{port}");
    test(&mut p, connection(&url, port, None, Some(&ca))).unwrap();

    // Queries run over the encrypted connection.
    let mut conn = connection("localhost", port, Some("verify_identity"), Some(&ca));
    conn["connection_id"] = json!("tls");
    let r = p.ok(
        "execute_query",
        json!({ "params": conn, "query": "RETURN 'over tls'", "page": 1 }),
    );
    assert_eq!(r["rows"], json!([["over tls"]]));

    // Without the CA the certificate is not trusted, and the error says
    // what to do.
    let (_, message) = test(
        &mut p,
        connection("localhost", port, Some("verify_identity"), None),
    )
    .unwrap_err();
    assert!(message.contains("TLS handshake"), "{message}");
    assert!(message.contains("CA certificate file"), "{message}");

    // The certificate is for `localhost`, not `127.0.0.1`: Verify Identity
    // rejects it, Verify CA accepts it.
    let (_, message) = test(
        &mut p,
        connection("127.0.0.1", port, Some("verify_identity"), Some(&ca)),
    )
    .unwrap_err();
    assert!(message.contains("TLS handshake"), "{message}");
    test(
        &mut p,
        connection("127.0.0.1", port, Some("verify_ca"), Some(&ca)),
    )
    .unwrap();

    // The CA file is the only trust anchor: a CA that did not sign the
    // server certificate is refused, even under Verify CA.
    let other = server.dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    make_certificates(&other);
    let (_, message) = test(
        &mut p,
        connection(
            "127.0.0.1",
            port,
            Some("verify_ca"),
            Some(&other.join("ca.pem")),
        ),
    )
    .unwrap_err();
    assert!(message.contains("TLS handshake"), "{message}");

    // Required encrypts without checking the certificate.
    test(
        &mut p,
        connection("127.0.0.1", port, Some("required"), None),
    )
    .unwrap();
    // An empty SSL mode is Required, as the host's SSL tab shows it.
    test(&mut p, connection("127.0.0.1", port, None, None)).unwrap();

    // Plain ws:// to a TLS port fails cleanly.
    let (_, message) = test(
        &mut p,
        connection("localhost", port, Some("disabled"), None),
    )
    .unwrap_err();
    assert!(
        message.contains(&format!("ws://localhost:{port}/rpc")),
        "{message}"
    );

    // An unreadable CA file is reported as such.
    let (_, message) = test(
        &mut p,
        connection(
            "localhost",
            port,
            Some("verify_identity"),
            Some(Path::new("/nonexistent/ca.pem")),
        ),
    )
    .unwrap_err();
    assert!(
        message.contains("Could not read the CA certificate file"),
        "{message}"
    );
    drop(server);
}
