# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

First release: browse SurrealDB 2.x and 3.x namespaces and databases and run
native SurrealQL. Requires Tabularis 0.26.0 or later; column statistics need
0.29.0. Known limitations are listed in the
[README](https://github.com/TabularisDB/tabularis-surrealdb-plugin#limitations).

### Added

- Connect to SurrealDB 2.x and 3.x with a host name or a `ws://`, `wss://`,
  `http://` or `https://` URL, a `namespace/database`, and a root,
  namespace or database user, or a token. Every connection has its own
  session, so two connections never share a namespace or database.
- Connection errors that say what to fix: an unreachable server (with the
  URL tried), a failed sign-in (listing each level tried), missing
  permissions, and a namespace or database that does not exist. The plugin
  never creates a missing namespace or database.
- TLS from the connection's SSL tab: Required, Verify CA and Verify
  Identity, with a CA certificate file for private or self-signed CAs.
- Connection string import:
  `surrealdb://user:pass@host:8000/namespace/database`.
- Explorer: namespaces and databases, tables (SCHEMAFULL, SCHEMALESS,
  relations, table views), fields and indexes. SCHEMALESS tables also list
  the fields found in their first 100 records.
- Table grid with server-side paging. Filters and sorts typed in the grid
  run as SurrealQL.
- Native SurrealQL in the editor and notebooks, including graph
  traversals. "Run all" sends the script as one query, so `LET` variables
  and transactions work across statements, and each statement gets its own
  result tab or error. Statements that return nothing (`LET`, `DEFINE`,
  `BEGIN`, …) get no result tab, so a notebook cell shows the first real
  result.
- Writes show the records they returned as affected rows, and results show
  the server's execution time.
- Values: record ids as `table:id` text you can paste back into a query,
  objects and arrays in the JSON viewer, decimals and large integers with
  their exact digits, datetimes, durations, uuids and GeoJSON. Records with
  different fields show the union of their fields.
- Record links (`record<table>` fields, relation `in` / `out`) appear as
  foreign keys: the ER diagram shows them and the grid opens the linked
  record.
- Generate SQL produces SurrealQL SELECT, UPDATE and DELETE templates.
- Column statistics in the grid's column menu: row, NULL and distinct
  counts, min / max and top values, computed in SurrealQL with the tab's
  filter. Value statistics on columns with more than 100 000 values come
  from a sample. Needs Tabularis 0.29.0.
- Plugin setting "Query timeout (seconds)": table-grid SELECTs get a
  SurrealQL `TIMEOUT`, and any query stops waiting after that long with a
  message that says so.
- Read-only grid: grid edits and schema changes are disabled. SurrealQL
  typed in the editor runs with your user's own permissions, so it can
  still write.
- Release archives for Linux (x86-64, ARM64), macOS (Apple Silicon, Intel)
  and Windows (x86-64), and a `.tabularium` manifest for the Tabularium
  registry.
