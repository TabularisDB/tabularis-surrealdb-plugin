//! Entry point: JSON-RPC over stdin/stdout. See `server.rs`.

fn main() {
    surrealdb_plugin::server::run();
}
