# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Connection errors repeat the address they tried, so a Host URL with a
  user name and password (`wss://user:pass@host`) showed the password. They
  are now left out (`wss://host/rpc`). The plugin never used them: it signs
  in with Username and Password.
- A SELECT with a write inside it, such as
  `SELECT * FROM (UPDATE person SET n += 1)` or
  `SELECT * FROM UPDATE person SET n += 1`, was paged like any SELECT.
  Tabularis runs the query again for each page, so opening the next page
  applied the write again. Such a SELECT now runs once and shows every row,
  in the editor and in "Run all", like the other writes.
- A Namespace/Database with a database but no namespace (`/typo`, also
  imported from `surrealdb://user:pass@host:8000//typo`) connected without
  checking or selecting anything, and Test Connection succeeded for a root
  user. It is now refused with a message that asks for
  `namespace/database`.
- When `INFO FOR ROOT` or `INFO FOR NS` succeeded without listing the
  namespaces or databases, the check that they exist passed, so `use` could
  create a missing one on SurrealDB 3.x. The connection now fails instead.
  2.x and 3.x list both, so connections to them are unchanged.
- An editor query naming a missing namespace or database
  (`REMOVE DATABASE ghost`) got the hint to check the connection's
  `namespace/database`. Only errors while connecting get it now; the editor
  shows the server's message.

## [0.1.2] - 2026-11-06

### Added

- Namespace and database users can connect from the connection form: the
  manifest declares `connection_fields.database`, so Tabularis newer than
  0.31.0 shows a Namespace/Database field on the Databases tab. Enter
  `namespace/database` (or `namespace`) there; All databases signs in to it,
  and a chosen database takes its place. Older Tabularis versions ignore the
  field, and the connection string keeps working. The sign-in error now names
  the field as well as the connection string.

## [0.1.1] - 2026-11-04

### Fixed

- Namespace and database users: Tabularis's connection form has a
  Databases tab and no Database field for this driver, so Host, Port,
  Username and Password alone send no namespace and only a root user can
  sign in. The sign-in error now says to import a connection string with
  the namespace and database (`surrealdb://user:pass@host:8000/ns/db`) and
  keep it selected under Choose databases, and the README's Connecting
  section describes the Databases tab and the connection string. No message
  refers to a Database field any more.
- A filtered table tab, or a record link followed to its record, showed
  only the fields the returned records had: `person` filtered to Bob, who
  has no `age` or `best_friend`, lost those two columns. Table tabs now show
  `id` and every declared field of the table, in declared order, then any
  other field of the records, whichever records come back. Queries typed in
  the editor keep the columns of their result.
- Clicking a record link among the top values in the column statistics
  popover, or filtering by a record link cell, filtered to 0 rows: the host
  writes the value as text (`` `best_friend` = 'person:bob' ``), which
  SurrealQL never equals a record. In table tabs and in the column
  statistics, text compared with a record id or record link column
  (`record<…>`, `option<record<…>>`, relation `in` / `out`) is now cast to
  a record id when it is one of the linked table's. Other text is left as
  it is.
- The cast of the query that follows a record link no longer applies to
  text that only starts like a record id (`'person:a b'`), which
  SurrealDB 3.x read as another record (`person:a`) and 2.x refused.

## [0.1.0] - 2026-11-03

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
- TLS from the connection's SSL tab: Required (also an empty mode, as the
  tab shows it), Verify CA and Verify Identity. A CA certificate file for a
  private or self-signed CA becomes the only trusted CA.
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
  result tab or error, on 2.x as on 3.x. Statements that never return data
  (`USE`, `LET`, `DEFINE`, `BEGIN`, …) get no result tab, so a notebook
  cell shows the first real result.
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
- Plugin setting "Query timeout (seconds)": table-grid and column
  statistics SELECTs get a SurrealQL `TIMEOUT`, and any query stops waiting
  after that long with a message that says so.
- Read-only grid: grid edits and schema changes are disabled. SurrealQL
  typed in the editor runs with your user's own permissions, so it can
  still write.
- Release archives for Linux (x86-64, ARM64), macOS (Apple Silicon, Intel)
  and Windows (x86-64), and a `.tabularium` manifest for the Tabularium
  registry.
