# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Connection to SurrealDB 2.x and 3.x over the WebSocket RPC protocol with
  CBOR encoding: host name or `ws://`/`wss://` URL, `namespace/database`,
  root, namespace or database users, or a token. One session per Tabularis
  connection and database.
- `test_connection` with actionable messages for unreachable servers, failed
  sign-in (listing every level tried) and missing permissions.
- Discovery: `namespace/database` list, tables, declared fields of
  SCHEMAFULL tables, fields sampled from the first 100 records of
  SCHEMALESS tables, and indexes.
- Table browsing with server-side `LIMIT … START …` paging, and SurrealQL
  templates for the Generate SQL dialog.
- `execute_query` for any SurrealQL; each statement of a script is its own
  result set.
- Value conversion: usable `table:id` record ids, JSON objects and arrays,
  exact decimals and large integers, datetimes, durations, uuids, GeoJSON.
- Cancellation of in-flight requests (local only; the server finishes the
  statement).
- Grid writes and schema changes answer with the documented "Method not
  found" response; the manifest declares the plugin read-only.
- Live integration test (`tests/live_db.rs`) and fixture
  (`fixture/seed.surql`).
