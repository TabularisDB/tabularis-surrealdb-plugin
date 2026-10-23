//! Tabularis driver plugin for SurrealDB.
//!
//! The binary (`src/main.rs`) only calls [`server::run`]; everything else
//! lives in this library so it can be unit tested.

pub mod error;
pub mod handlers;
pub mod models;
pub mod rpc;
pub mod server;
pub mod surreal;
pub mod utils;
