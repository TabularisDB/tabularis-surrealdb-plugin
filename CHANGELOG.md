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
- A namespace or database that does not exist is reported as not found and
  never created (SurrealDB 3.x `use` would create it).
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
- `execute_query_batch`: the editor's "run all" runs as one SurrealQL
  query, so `LET` and transactions work across statements, with one result
  or error per statement and the server's execution time.
- Record links as foreign keys (`get_foreign_keys`,
  `get_all_foreign_keys_batch`), and the grid's foreign-key lookup is cast
  to a record id so it finds the linked record.
- TLS settings from the SSL tab (`supports_ssl`): Required, Verify CA and
  Verify Identity modes and a CA certificate file. Live TLS test with a
  private CA (`tests/live_tls.rs`).
- Plugin setting `query_timeout_seconds`: paged SELECTs get a SurrealQL
  `TIMEOUT`, every query is bounded locally, with actionable errors.
- Affected rows for CREATE, INSERT, UPDATE, UPSERT, DELETE and RELATE, and
  the server's statement time on results.
- `get_column_stats` (manifest `column_stats`) for the grid's column
  statistics popover: exact row and NULL counts, and distinct count,
  min / max and top values from the first 100 000 non-NULL values, all with
  the tab's filter. Needs the Tabularis host change for #1137.

### Changed

- In a multi-statement `execute_query`, statements that return NONE
  (`LET`, `DEFINE`, `BEGIN`, …) no longer produce empty result sets; the
  first real result is the primary one, which notebook cells show.
