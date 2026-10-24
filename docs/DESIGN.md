# Design notes

This document records the decisions behind the SurrealDB plugin, the
evidence for them, and the documentation they rely on. Scope:
[TabularisDB/tabularis#1019](https://github.com/TabularisDB/tabularis/issues/1019).

## Coverage of #1019 and the plugin protocol

Status: **M1** = milestone 1 (PR #1), **M2** = this milestone, **deferred**
= not implemented, with the reason. Evidence names a live test in
`tests/live_db.rs` / `tests/live_tls.rs`, or a unit test module.

### Acceptance criteria

| Criterion | Status | Evidence |
|---|---|---|
| Documented local version connects with auth and explicit namespace/database; actionable auth and permission errors | M1 | `connects_and_authenticates`, `unreachable_server_is_reported`, `surreal::errors` tests |
| Browse schemafull and schemaless tables, declared or inferred fields, indexes; open records in the grid | M1 | `discovers_databases_tables_columns_and_indexes`, `browses_tables_with_limit_and_offset` |
| Native SurrealQL in the editor and notebooks, incl. SELECT and a graph traversal | M1; M2 adds the editor's batch path and notebook-friendly scripts | `runs_native_surrealql`, `batches_run_as_one_script_with_per_statement_results`, `result_shapes_writes_and_empty_results` |
| Record ids usable; nested values in the JSON UI; heterogeneous and scalar/object results never crash or drop fields | M1; M2 adds record links as foreign keys the grid can follow | `converts_values_and_keeps_record_ids_usable`, `record_links_are_foreign_keys_and_navigable`, `handlers::results` tests |
| Browsing is valid SurrealQL, honours limit/offset, quotes identifiers; two connections do not share context | M1 | `browses_tables_with_limit_and_offset`, `connections_do_not_share_namespace_or_database` |
| Timeouts and query errors handled cleanly; cancellation semantics documented | M1 (cancel, docs); M2 (query timeout setting, per-statement errors) | `cancel_stops_waiting_and_keeps_the_plugin_responsive`, `query_timeout_is_enforced_with_actionable_errors` |
| Unsupported grid writes, DDL and other capabilities disabled or answered with the documented response | M1 | `rpc` tests `writes_and_ddl_return_the_documented_unsupported_response`, `unknown_methods_are_method_not_found` |
| Metadata and value-conversion tests; reproducible fixture with schemafull/schemaless records, nested values, record ids, relations, permission errors | M1; M2 adds record links and a write table | `utils::values`, `utils::define`, `handlers::metadata` tests; `fixture/seed.surql` |
| Valid `.tabularium`, installation instructions, release assets for the declared platforms | Partly: manifest validated in CI, release workflow in place. **Deferred to M3**: first tagged release, install docs for released assets, registry submission | CI `Validate .tabularium manifest` |

### Protocol methods and capabilities

| Method or capability | Status | Evidence or reason |
|---|---|---|
| `initialize` | M2: reads `query_timeout_seconds` | `rpc` test `methods_without_a_server_round_trip`, `handlers` settings test |
| `test_connection`, `ping` | M1 | `connects_and_authenticates`, `cancel_stops_waiting…` (ping) |
| `get_databases` | M1 (`namespace/database` pairs) | `discovers_databases…` |
| `get_schemas` | M1: `[]` (`schemas: false`) | namespaces and databases are listed together by `get_databases` |
| `get_tables`, `get_columns`, `get_indexes`, `get_all_columns_batch` | M1 | `discovers_databases…` |
| `get_foreign_keys`, `get_all_foreign_keys_batch` | M2: `record<t>` fields and relation `in`/`out` | `record_links_are_foreign_keys_and_navigable`, `handlers::metadata` tests |
| `get_schema_snapshot`, `get_ai_schema_context` | `-32601` by design: the host composes them from the methods above | `unknown_methods_are_method_not_found` |
| `execute_query` | M1; M2: statements that never return data skipped (by keyword), affected rows, server time, timeout | `runs_native_surrealql`, `result_shapes_writes_and_empty_results` |
| `execute_query_batch` | M2: one script, one result or error per statement | `batches_run_as_one_script…`, `handlers::query` batch tests |
| `session_id`, `release_session` | `session_id` accepted and ignored; `release_session` answers `-32601`, which the host tolerates. Deferred: transactions that span separate runs (SurrealDB transactions live inside one `query` call; 3.x interactive transactions need the `begin`/`commit` RPC) | — |
| `cancel` notification | M1: local only; server-side `query_cancel` (3.3+) deferred, see Cancellation | `cancel_stops_waiting…` |
| `get_table_query_template` (`table_query_templates`) | M1 | `browses_tables…` runs the templates |
| `get_column_stats` (`column_stats`) | Added for Tabularis #1137 (needs a host with that change); see Column statistics | `column_statistics_…` live tests, `handlers::stats` tests |
| `explain_query` (`explain`) | Deferred, capability off; see EXPLAIN | typed `… EXPLAIN` works as a query: `result_shapes…` |
| Views (`views`), materialized views | Deferred / not applicable. Table views (`DEFINE TABLE … AS SELECT`) are listed as tables with a "table view" comment | — |
| Routines (`routines`, `routine_management`) | Deferred: `DEFINE FUNCTION` could map, not in scope | — |
| Triggers (`triggers`) | Deferred: `DEFINE EVENT` could map, not in scope | — |
| `insert_record`, `update_record`, `delete_record` | Unsupported (`readonly: true`), `-32601`. Candidate for M3 (record id as key) | `rpc` tests |
| DDL methods, `drop_index`, `drop_foreign_key`, view DDL | Unsupported (`manage_tables: false`), `-32601` | `rpc` tests |
| `save_blob_to_file`, `fetch_blob_as_data_url` | `-32601`; bytes are shown as `0x…` hex | — |
| `get_connection_metadata` (`connection_metadata`) | Not needed: one static manifest fits every server | — |
| User management methods | Not declared, `-32601` | — |
| `supports_ssl` | M2: SSL modes and CA file | `live_tls` `wss_with_private_ca_and_ssl_modes`, `models`/`tls` tests |
| `connection_string`, `sql_dialect`, `identifier_quote`, `readonly`, `manage_tables` | M1 | manifest; `browses_tables…` |
| Plugin `settings` | M2: `query_timeout_seconds` | `query_timeout_is_enforced…` |
| Auth level in the connection form (`connection-modal.extra_fields` UI extension) | Deferred: needs a UI bundle and release-workflow changes; awaiting a decision in PR #1 | — |

## Transport: WebSocket RPC with CBOR, not the `surrealdb` crate

The plugin speaks SurrealDB's documented RPC protocol directly over a
WebSocket (`/rpc`, subprotocol `cbor`), using `tokio-tungstenite` and
`ciborium`. It does not use the official Rust SDK.

Both options were built and measured on the same machine (Apple Silicon,
Rust 1.99, clean `target/`, trivial `main`):

| | `surrealdb` 3.3.4, `default-features = false`, `protocol-ws` + `rustls` | Direct RPC (`tokio-tungstenite` 0.30 + `ciborium` 0.2) |
|---|---|---|
| Crates in the normal dependency graph | 199 | 64 |
| Clean debug build | 43 s, 807 MB `target/` | 14 s, 256 MB `target/` |
| Clean release build | 78 s, 1.3 GB `target/` | 19 s, 437 MB `target/` |
| TLS crypto | pulls `aws-lc-sys` (C, needs cmake) alongside `ring` | `ring` only |

Other factors:

- **Server versions.** The SDK 3.x documents support for servers 2.0 and
  later, so it would cover the same range. The RPC methods the plugin needs
  (`signin`, `authenticate`, `use`, `query`, `ping`, `version`) and the
  CBOR tags exist in both 2.x and 3.x; the few differences (version answer,
  error objects) are handled explicitly and unit tested.
- **Type fidelity.** CBOR carries every SurrealDB type as a tag (record id,
  decimal, datetime, duration, uuid, NONE, geometry, range), so values are
  converted without guessing. The SDK offers the same, at the cost above.
- **Sessions.** One WebSocket is one SurrealDB session (sign-in, namespace,
  database). With direct RPC that is explicit and visible in
  `src/surreal/`, which matters for the isolation requirement.
- **Cancellation.** Neither option stops a running statement on 2.x or
  3.0–3.2 servers (see below). The RPC reference lists `query_cancel`
  from 3.3.0; direct RPC can add it without waiting for an SDK release.
- **Release builds.** The release workflow cross-compiles five targets,
  one with `cross`. Fewer crates and no C crypto dependency keep those jobs
  short and less fragile.
- **Cost.** The plugin owns a small protocol client: request multiplexing,
  error decoding and CBOR conversion. These are covered by unit tests and
  by the live test.

## Supported SurrealDB versions

- **3.x**: tested. The live test passes against 3.2.3 locally and against
  the `surrealdb/surrealdb:v3` image in CI.
- **2.x**: tested. The CI job `live-db-integration` runs the same live test
  against the `surrealdb/surrealdb:v2` image, and it passes. To run it
  locally without installing another version:
  `docker run --rm -p 8000:8000 surrealdb/surrealdb:v2 start --user root --pass root memory`,
  then `just live-test`.
- **1.x**: not supported (EOL; INFO output, auth and CBOR differ).

Choices made so the same code works on both majors:

- Metadata comes from `INFO FOR DB` / `INFO FOR TABLE` in their plain form
  (maps of `name → DEFINE statement`), which both majors return. The
  `INFO … STRUCTURE` form is documented as "for internal use and subject to
  change without notice", and its keys already differ between releases
  (`what` in the docs, `table` on 3.2.3), so it is not used.
- No functions renamed in 3.0 (for example `type::thing` → `type::record`)
  are used. Table browsing uses `type::table`, present in both.
- `version` answers with a string on 2.x and an object on 3.x; both parse.
- RPC errors are `{ code, message }` on 2.x and add `kind`/`details` on 3.x;
  both parse.

Relevant 3.0 changes for users (from the 2.x → 3.x migration guide): a
SELECT on a missing table is now an error instead of an empty result, and
SCHEMAFULL tables reject unknown fields instead of dropping them. The plugin
passes such errors through unchanged.

## Connection model and sessions

| Tabularis field | SurrealDB |
|---|---|
| Host | host name, or a URL (`ws://`, `wss://`, `http://`, `https://`); `/rpc` is appended |
| Port | port for a bare host name (default 8000) |
| Database | `namespace/database`, `namespace`, or empty |
| Username / Password | a system user (root, namespace or database level) |
| Password with empty Username | a JWT, used with `authenticate` |

Sign-in (`extra.auth_level`, default `auto`): `auto` tries a database user
(when `namespace/database` is set), then a namespace user (when a namespace
is set), then a root user, least privilege first, and reports every level it
tried when all fail. `root`, `namespace`, `database`, `token` and `none`
(servers started with `--unauthenticated`) pin the level. The connection form
has no field for it yet; see "Open questions".

Existing namespaces and databases only: on SurrealDB 3.x, the `use` RPC
creates a namespace or database that does not exist when the signed-in user
may define one (root users both, namespace OWNERs and EDITORs a database).
2.x creates nothing, but the connection would look empty. So before `use`,
a session checks what the signed-in level can see, in an order that never
names a missing namespace in `use`:

1. `INFO FOR ROOT` lists the namespaces. It needs no namespace selected.
2. Only once the namespace is known to exist, `use` selects it alone (this
   creates no database) and `INFO FOR NS` lists its databases.
3. `use` selects the namespace and database.

A missing namespace or database fails with "The namespace 'x' does not
exist" (or database), plus a hint to check
the Database field. This covers "Test connection" and every other request.
A check the signed-in user may not run is skipped, and the server's own
permissions decide. Checked against 3.2:

- Root users: both checks run.
- Namespace users cannot run `INFO FOR ROOT`, but they can only sign in to
  a namespace that exists, and cannot create another one. `INFO FOR NS`
  runs, so a missing database is refused (an EDITOR or OWNER would
  otherwise create it).
- Database users can run neither check. They can only sign in to a
  database that exists (a missing one shows as a failed sign-in) and cannot
  create a namespace or database.
- Tokens and anonymous sessions get whichever checks their level allows.

TLS (`supports_ssl: true`, the connection's SSL tab). The host offers the
MySQL-style modes for this dialect; the plugin maps them as follows:

| SSL mode | Bare host name | Certificate check |
|---|---|---|
| Disabled, Preferred | `ws://` | none (no TLS) |
| Required, or empty | `wss://` | none: encrypted, any certificate accepted |
| Verify CA | `wss://` | chain to a trusted root; any host name |
| Verify Identity | `wss://` | chain and host name |

An empty SSL mode is Required because that is what the host shows for it:
`NewConnectionModal.tsx` selects `required` when `ssl_mode` is blank for a
driver that is not PostgreSQL-dialect (this manifest says `mysql`), and
sends no `ssl_mode` at all (`withoutBlankSslMode`). Tabularis's own MySQL
driver does the same (`mysql_options_default_ssl_mode_is_required` in
`pool_manager_tests.rs`). Treating empty as Disabled would show Required
while sending credentials in clear. A plain server answering a TLS client
hello gets a message that says so and points to Disabled.

A `wss://` (or `https://`) URL in Host always uses TLS, with Verify Identity
unless the SSL mode says otherwise; a `ws://` URL with a TLS mode is an
error. With an empty mode, a URL's scheme decides: `ws://` stays plain (the
scheme is written in Host, next to the tab) and `wss://` gets Verify
Identity, stricter than the Required the tab shows. Trusted roots are the PEM file in "CA certificate" when it is set,
and only that file; otherwise the bundled Mozilla roots (`webpki-roots`).
Trusting both would let anyone with a publicly valid certificate through:
for any host name under Verify CA, which does not check the name, and for
the server's own name under Verify Identity. The file replaces the public
roots in both modes, as `sslrootcert` does for PostgreSQL's `verify-ca` and
`verify-full` and `--ssl-ca` for MySQL's `VERIFY_CA` and `VERIFY_IDENTITY`:
whoever sets a private CA has said which CA signs the server. Required
checks no certificate and ignores the file. Client
certificates are rejected with an explanation: SurrealDB authenticates with
users and tokens. `tests/live_tls.rs` generates a private CA with `openssl`,
starts `surreal start --web-crt/--web-key` on a free port and checks every
mode, the host-name mismatch, the missing-CA error and plain `ws://` against
a TLS port. It needs the `surreal` CLI, so CI skips it; it passes locally
against 3.2.3.

Isolation: a session is keyed by the full connection configuration,
including the Tabularis `connection_id`. Two Tabularis connections never
share a WebSocket, even with identical settings, so their sign-in, namespace,
database and session variables cannot leak. A connection that selects
several databases in the explorer gets one session per database. "Test
connection" always uses a fresh session that is closed afterwards.

A query that contains `USE` runs on a short-lived session of its own,
because `USE` changes the namespace/database of the session it runs on and
other requests share that session. `get_databases` does the same for the
`USE NS …; INFO FOR NS` it needs.

A session that closed (server restart, network) is reopened on the next
request. A request is retried on a new session only when it never reached
the server (socket already closed) or the server rejected it as expired; a
request that may have executed is never retried, so writes are not applied
twice.

## Cancellation and timeouts

The host stops waiting after its plugin call timeout and sends a `cancel`
notification. The plugin keeps reading stdin while requests run (each
request is its own task), aborts the cancelled task, and sends no response,
as the protocol requires. The plugin stays responsive; the live test checks
this with `RETURN sleep(2s)`.

**This is local cancellation only.** The statement keeps running on the
SurrealDB server and its effects, if any, still apply. SurrealDB 2.x and
3.0–3.2 have no RPC to cancel a running query; the RPC reference lists
`query_cancel` from 3.3.0. It is deferred to M3: the local test server is
3.2.3, and the plugin does not ship server-side cancellation it cannot test.

Connecting is bounded at 10 s, and sign-in, the namespace/database check
and `use` at 15 s each, so an unreachable or unresponsive server fails fast
with a message that names the step that timed out.

Query timeout (M2). The host already has a per-plugin call timeout
(default 120 s) after which it cancels the request; that gives a generic
"timed out" error and leaves the statement running. The plugin adds an
optional setting, `query_timeout_seconds` (Settings, SurrealDB plugin; 0
or empty disables it):

- Paged SELECTs (the table grid) get `TIMEOUT <n>s` after `START`. SurrealDB
  fails such a statement when the duration passes and returns an error
  ("exceeded the timeout"); the plugin adds a hint naming the setting. In
  the live test the server answered after 1.0 s for `TIMEOUT 1s`.
- Every query call is also bounded locally at the timeout plus one second.
  The error says that the plugin stopped waiting and that the statement may
  still be running on the server. A script or editor batch is one `query`
  call, so the bound covers the whole run, not each statement (the server
  gives no per-statement progress to bound them separately); for several
  statements the error says so, and that the ones that finished were
  applied.

Set it below the host's call timeout to get these messages instead of the
host's generic one.

## Discovery

- Tables: `INFO FOR DB`. The comment shows the table's own COMMENT plus
  SCHEMAFULL/SCHEMALESS, "relation" for `TYPE RELATION` and "table view"
  for `DEFINE TABLE … AS SELECT`.
- Columns: `id` first (`record<table>`, primary key), then declared
  top-level fields in server order. Nested definitions such as
  `address.city` or `tags.*` belong to their parent column and are not
  separate columns. Relation tables always list `in` and `out`.
- SCHEMALESS tables additionally get the union of the top-level keys of
  their first 100 records (`SELECT * FROM type::table($table) LIMIT 100`,
  natural record order). A sampled column's type is the set of value types
  seen (for example `object | string`), it is nullable, and its comment says
  it was inferred. Keys that appear only after the first 100 records are not
  listed; the grid still shows them, because result columns come from the
  data.
- Indexes: `INFO FOR TABLE`, one entry per indexed field, with `UNIQUE`.
- Foreign keys (M2): a top-level field typed `record<t>` (also inside
  `option<…>` or `none | …`) is a foreign key to `t.id`; `record<a | b>` gives
  one key per table, named `field→a`. Relation tables add `in` and `out`
  from `TYPE RELATION IN … OUT …` when the server does not list them as
  fields (2.x). Arrays of records and untyped `record` are not keys.
  `on_delete`/`on_update` are not reported.
- Autocomplete (`get_all_columns_batch`) runs `INFO FOR DB; INFO FOR TABLE`
  and, for SCHEMALESS tables, the 100-record sample for every table, one
  table after the other. On a database with many SCHEMALESS tables this is
  slow. Milestone 2: run the per-table requests concurrently (bounded), or
  fetch all table definitions in one script.

## Table browsing and query templates

The host opens a table with ``SELECT * FROM `name` `` (backtick is the
manifest's `identifier_quote`), adds the filter and sort the user typed, and
passes the page size and page number to `execute_query`. For a single
SELECT without its own LIMIT, START, FETCH, TIMEOUT, PARALLEL, TEMPFILES,
EXPLAIN or ONLY, the plugin appends `LIMIT <size+1> START <offset>` on a new
line; the extra row tells whether another page exists. Other SELECTs are run
as written and the page is cut out of the result. Statements that are not a
single SELECT are never paginated, because the host re-runs the query for
every page and a write must not be applied again.

Two host constructs are not valid SurrealQL and are rewritten, only in this
exact shape:

- `SELECT * FROM (…) AS limited_subset` (the host's wrapper for a user
  LIMIT): the alias is dropped.
- A backtick inside a quoted table name, which the host doubles; SurrealQL
  escapes it with a backslash.
- The host's foreign-key lookup,
  ``SELECT * FROM `t` WHERE `id` = 't:x'``, compares a record id with a
  string, which never matches in SurrealQL. When the string starts with the
  same table, it becomes ``WHERE id = <record> 't:x'`` (the value stays a
  string literal). The same applies to that filter typed in the grid.

`get_table_query_template` (manifest `table_query_templates: true`) returns
SurrealQL for the Generate SQL dialog. The live test runs the SELECT and
DELETE templates to prove they are valid; the DELETE uses `WHERE 1 = 0` as
the protocol requires and deletes nothing.

`sql_dialect` is `mysql`, the closest dialect the host knows: backtick
identifiers and backslash escapes in strings, as in SurrealQL. Without it the
host would assume PostgreSQL quoting.

## Column statistics

The grid's column header menu has a "Column statistics" popover: row, NULL
and distinct counts, min / max and the 10 most frequent values, for the rows
matching the tab's filter. Tabularis builds it with generic SQL
(`COUNT(DISTINCT col)`, `GROUP BY col ORDER BY 2 DESC, 1 LIMIT n`) for
SQL engines. That SQL is not SurrealQL, and since `sql_dialect` is `mysql`
the host would otherwise treat the plugin as SQL-compatible. The manifest
therefore declares `column_stats: true`, and the plugin answers
`get_column_stats` (Tabularis #1137) in SurrealQL. Older hosts ignore the
capability.

The request names the table and column (unquoted), the tab's filter (a
SurrealQL condition, as typed in the grid), a top-N limit and which parts to
compute. The host never asks for min / max or top values of a masked column,
and drops them anyway if they come back. The plugin sends one script in one
`query` call, on the connection's session:

```sql
SELECT count() AS rows, count((`c` = NONE) OR (`c` = NULL)) AS nulls
  FROM `t` WHERE (<filter>) GROUP ALL;
LET $values = (SELECT `c` AS v FROM `t`
  WHERE (<filter>) AND `c` != NONE AND `c` != NULL LIMIT 100000);
RETURN { sampled: array::len($values),
         distinct: array::len(array::distinct($values.v)),
         min: array::min($values.v), max: array::max($values.v) };
SELECT * FROM (SELECT v, count() AS count FROM $values GROUP BY v)
  ORDER BY count DESC, v ASC LIMIT 10;
```

Only the parts the host asked for are added. Choices:

- **NULL.** A missing field is NONE. Both NONE and NULL count as NULL, the
  way the grid shows both as an empty cell.
- **Counts are exact.** `count()` with `GROUP ALL` reads every matching
  record, the same work as counting the filtered rows of the grid.
- **Value statistics are sampled on big tables.** Distinct count, min / max
  and top values are computed from the first 100 000 non-NULL values in the
  table's natural (record id) order, kept in `$values` on the server. When
  the column has more non-NULL values than that, the result carries
  `sample_size: 100000` and the popover says the values come from a sample.
  The first records are not a random sample; a random one
  (`ORDER BY rand()`) would have to read and sort the whole table, which is
  what the cap avoids. Grouping runs over `$values`, so the table is read
  twice at most (once for the counts, once for the sample).
- **Min / max.** `array::min` / `array::max` compare with SurrealDB's own
  ordering, so they work for numbers, datetimes, strings and durations
  alike (`math::min` only takes numbers). The host asks for them only for
  numeric and date/time columns.
- **Top values.** `GROUP BY v` over the sample, sorted by count in an
  outer SELECT, ties broken by value. The sort cannot go in the grouping
  SELECT: 2.x ignores `ORDER BY count` there and returns the groups ordered
  by `v`, and the `LIMIT` then keeps the wrong values. Values
  are converted like grid cells (record ids as `table:id`, datetimes as
  text).
- **Filter.** The filter is inserted as `(<filter>\n)`: the newline keeps a
  trailing `--` comment from swallowing the bracket. It must have the shape
  of one condition (brackets, quotes and regex literals balance, no
  top-level `;`), or it is refused with `-32602`: spliced in as is, it
  would change the shape of the script around it and fail with a confusing
  parse error, or report statistics for something else. This is a shape
  check, not a security guard. One expression can still write
  (`(DELETE t) = []`, `{ DELETE t; true }`), just as the same filter can in
  the grid, which runs it as typed, and the editor runs anything; all of it
  runs with the signed-in user's permissions.
- **Empty results.** 2.x returns no row for a `GROUP ALL` over nothing, 3.x
  a zero row; both read as 0. That row is the only result allowed to be
  missing, and min / max the only values allowed to be NONE (an empty
  sample). A missing or mistyped `sampled`, `distinct` or top-values result
  is an error naming what was expected, not a zero or an empty list, so a
  server that shapes a result differently cannot show wrong statistics.
- **Timeout.** With the query timeout setting on, every SELECT of the
  script (the counts, the sample inside `LET`, the top values) ends in
  `TIMEOUT <n>s`, so the server stops it and answers with its timeout
  error. The popover never cancels a request, so a timeout that only
  stopped the plugin from waiting would leave a full `count() … GROUP ALL`
  running on a big table. The plugin waits at most `<n>` seconds per
  statement plus one, which only matters for a server that does not answer.
  `RETURN` takes no `TIMEOUT`; it works on the sample in `$values`, at most
  100 000 values.

Verified live on 3.2.3 (`column_statistics_respect_the_filter_and_the_requested_parts`,
`column_statistics_sample_large_tables_and_keep_exact_counts` with over 100 000
rows). CI runs the same live tests on v2.

## Result mapping

| Statement result | Columns and rows |
|---|---|
| array of objects | union of keys, `id` first, then first-seen order; a missing key is `null` |
| array with any non-object element | one `result` column, one row per element |
| single object (e.g. `INFO`) | its keys, one row |
| scalar or NULL | one `result` column, one row |
| NONE (`LET`, `DEFINE`, …) | no columns, no rows |

Scripts in one `execute_query` call (notebook cells, a single editor
statement the host did not split): statements that never return data
produce no result set. They are recognised by their first keyword, not by a
NONE result, because the servers differ (3.x answers `USE` with the
selected namespace and database, 2.x with NONE) and the shape of the
response must not depend on the version:

| Kind | Keywords |
|---|---|
| session | `USE`, `LET`, `OPTION` |
| transaction | `BEGIN`, `COMMIT`, `CANCEL` |
| schema | `DEFINE`, `REMOVE`, `ALTER`, `REBUILD` |
| code without a value | `FOR`, `SLEEP`, `KILL` |

Any other statement keeps its result, even NONE (`RETURN NONE`, an `IF`
without `ELSE`): the user asked for a value. When the plugin's statement
count disagrees with the server's, NONE results are left out instead. The
first remaining result is the primary one, which is what a notebook cell shows;
the others go to `additional_results` (result tabs in the editor). A failed
statement fails the call and is named ("Statement 2 of 3 failed: …"),
because a notebook cell has a single error slot and hiding the error behind
a later result would be worse.

The editor's "run all" splits the script and calls `execute_query_batch`
(M2). The plugin joins the statements into one `query` call (with `;` on a
line of its own, so a trailing `--` comment cannot swallow it), so `LET`
variables and `BEGIN … COMMIT` work across them, and returns one result or
error per statement: an error stays with its statement and the others still
show. The host splits with a generic SQL splitter that can cut a SurrealQL
block (`{ …; … }`); joined again it is one statement, whose result goes to
the piece where it ends while the other pieces show an empty result. A
piece with unbalanced brackets is never paginated, even when it starts with
SELECT, since a `LIMIT` after it would land inside or after the block. Each
result carries the server's execution time (`execution_time_ms`, and
`server_time_ms` on the result).

Results are matched to statements by position. SurrealDB 2.x adds no
result entry for `BEGIN`, `COMMIT`, `CANCEL` and `OPTION` (3.x answers
every statement), so the plugin reads the server version when a session
opens and, on 2.x, leaves those statements out when lining results up. This
applies to both `execute_query` (affected rows, paging, the failing
statement's position) and `execute_query_batch`.

`affected_rows` (M2): for CREATE, INSERT, UPDATE, UPSERT, DELETE and RELATE,
the number of records the statement returned. The host shows it only when
there is no result grid. DELETE returns nothing by default, so it reports no
count rather than a wrong one.

## EXPLAIN

`explain` stays off, so the Visual EXPLAIN button is hidden. Reasons:

- The syntax and output differ by version: 2.x has `SELECT … EXPLAIN [FULL]`
  returning a list of `{ detail, operation }`; 3.x adds
  `EXPLAIN [ANALYZE] [FORMAT TEXT | JSON] <statement>` with an operator tree.
- The SurrealDB docs call the output "for informational purposes and
  subject to change" and advise against tooling that depends on its exact
  shape.
- SurrealDB plans have no costs, which is what the host's plan view is
  built around.

Typing `EXPLAIN` in the editor works and shows the plan as data
(`result_shapes_writes_and_empty_results`). A raw-output parser bundle
(`explain_parsers`) is the right way to add a visual plan later.

## Value conversion

| SurrealDB | Cell |
|---|---|
| record id | `table:id` in SurrealQL syntax, quoting where needed (`` event:`weird-id` ``, `event:[2024, 'q1']`, `` `odd table`:x ``); pasting it into a query selects that record |
| object, array | JSON (shown in the JSON viewer) |
| int within ±(2^53−1) | number |
| int outside that range | decimal string (JavaScript would round the number) |
| float | number; NaN and ±Infinity as strings |
| decimal | exact decimal string |
| datetime | RFC 3339 UTC string with nanoseconds |
| duration | SurrealQL duration (`1h30m`) |
| uuid | hyphenated string |
| bytes | `0x…` hex string |
| geometry | GeoJSON object |
| NONE, NULL | `null` |

NONE versus NULL: inside objects the distinction is kept (a NONE field is
absent, a NULL field is `null`). In grid cells and scalar results both are
`null`. Checked again for M2: result cells travel as `serde_json::Value`
(`QueryResult.rows` in `src-tauri/src/models.rs`), which has a single null
and no "absent", and the grid renders `null`/`undefined` with the same NULL
label (`src/utils/dataGrid.ts`). A sentinel string such as `"NONE"` would be
indistinguishable from real text, so the plugin does not invent one. This is
a host representation limit, not a conversion loss.

## Unsupported operations

The manifest declares `readonly: true` and `manage_tables: false`, so the
host hides grid editing and table management. If one of these methods is
called anyway, the plugin answers with the documented "not implemented"
response: code `-32601` and a message that starts with
`Method not found: <method>` (followed by the reason). Unknown methods,
`get_schema_snapshot` and `get_ai_schema_context` get the same code so the
host uses its fallbacks.

## Open questions

- Auth level in the connection form: pinning it needs a
  `connection-modal.extra_fields` UI extension, i.e. a `ui/` bundle that
  `release.yml` does not build yet. Until then `auto` is used.

## Where the docs disagree

- `plugins/PLUGIN_GUIDE.md` shows `execute_query` taking `page_size` and
  returning `total_count` / `execution_time_ms`, and `get_indexes` returning
  `index_name` + `columns`. The wiki's plugin protocol page and the host
  source (`src-tauri/src/plugins/driver.rs`, `src-tauri/src/models.rs`) use
  `limit` + `page`, require `affected_rows`, and expect one index entry per
  column (`name`, `column_name`, `seq_in_index`). The plugin follows the
  wiki and host source, which are newer. It also accepts `page_size`.
- The wiki and the issue scaffold with
  `pnpm create @tabularis/plugin@latest surrealdb`. With
  `@tabularis/create-plugin` 0.4.1 that fails with
  `ERR_PNPM_DLX_MULTIPLE_BINS` (the package now has two binaries). The
  scaffold was generated with
  ``pnpm --package=@tabularis/create-plugin@0.4.1 dlx tabularis-create-plugin surrealdb --db-type=network --quote='`'``.

## References

Read on 2026-10-16 (M1 and M2).

Tabularis:

- Building plugins: <https://tabularis.dev/wiki/building-plugins>
- Plugin protocol: <https://tabularis.dev/wiki/plugin-protocol>
- Publishing plugins: <https://tabularis.dev/wiki/plugin-development>
- Registry manifest schema (driver):
  <https://registry.tabularis.dev/manifest.schema.json?kind=driver>; the
  manifest passes `npx @tabularium/cli@0.1.0 validate .tabularium --registry https://registry.tabularis.dev --kind driver`
- TabularisDB/tabularis at `bebdd9d1` (M1) and `2ea3a6e2` (M2, no plugin
  protocol changes in between): `plugins/PLUGIN_GUIDE.md`,
  `plugins/manifest.schema.json`, `src-tauri/src/plugins/{driver,rpc,manager}.rs`,
  `src/types/plugins.ts`, `src/utils/driverCapabilities.ts`,
  `src/utils/editor.ts` (table-browse SQL), `src/utils/identifiers.ts`,
  `packages/create-plugin` 0.4.1; for M2 also
  `src-tauri/src/plugins/call_timeout.rs` (host call timeout, default 120 s),
  `src/pages/Editor.tsx` (batch execution), `src/components/notebook/NotebookView.tsx`
  (notebook cells use `execute_query` and show the primary result),
  `src/components/ui/ResultEntryContent.tsx` (affected rows),
  `src/hooks/useReferencedRecord.ts` and `src/utils/foreignKeys.ts`
  (foreign-key lookup SQL), `src/components/modals/NewConnectionModal.tsx`
  (SSL modes), `packages/explain/src/types.ts` (parsed plan shape)
- Sibling plugins used for conventions: tabularis-trino-plugin 0.1.6,
  tabularis-libsql-plugin 1.0.1, tabularis-elasticsearch-plugin,
  tabularis-mongodb-plugin

SurrealDB:

- RPC protocol: <https://surrealdb.com/docs/reference/rest-api/rpc-protocol>
  (lists `query_cancel` and `query_stream` from 3.3.0)
- CBOR protocol (tags): <https://surrealdb.com/docs/reference/rest-api/cbor-protocol>
- INFO statement: <https://surrealdb.com/docs/surrealql/statements/info>
- SELECT statement (clause order, `TIMEOUT`, `EXPLAIN [FULL]`):
  <https://surrealdb.com/docs/surrealql/statements/select>
- EXPLAIN statement (3.x form, "subject to change"):
  <https://surrealdb.com/docs/surrealql/statements/explain>
- Casting (`<record>` from a string):
  <https://surrealdb.com/docs/surrealql/datamodel/casting>
- 2.x → 3.x migration:
  <https://surrealdb.com/docs/build/migrating/from-old-surrealdb-versions/2x-to-3x>
- Rust SDK: <https://surrealdb.com/docs/sdk/rust> (SDK 3.3.4, servers 2.0+,
  Rust 1.95+) and the crate's feature list (`cargo info surrealdb@3.3.4`);
  docs.rs had no successful build of 3.3.4 at the time
