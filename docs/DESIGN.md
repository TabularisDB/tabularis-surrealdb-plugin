# Design notes

This document records the decisions behind the SurrealDB plugin, the
evidence for them, and the documentation they rely on. Scope:
[TabularisDB/tabularis#1019](https://github.com/TabularisDB/tabularis/issues/1019).

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
exist" (or database), the same text SurrealDB uses, plus a hint to check
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
`query_cancel` from 3.3.0. Using it is planned for milestone 2, behind a
server version check and with a test against a 3.3 server.

Connecting is bounded at 10 s, and sign-in, the namespace/database check
and `use` at 15 s each, so an unreachable or unresponsive server fails fast
with a message that names the step that timed out. Queries
have no plugin-side timeout; the host's call timeout applies.

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
- Foreign keys: not reported yet (record links and relations could be,
  milestone 2).
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

`get_table_query_template` (manifest `table_query_templates: true`) returns
SurrealQL for the Generate SQL dialog. The live test runs the SELECT and
DELETE templates to prove they are valid; the DELETE uses `WHERE 1 = 0` as
the protocol requires and deletes nothing.

`sql_dialect` is `mysql`, the closest dialect the host knows: backtick
identifiers and backslash escapes in strings, as in SurrealQL. Without it the
host would assume PostgreSQL quoting.

## Result mapping

| Statement result | Columns and rows |
|---|---|
| array of objects | union of keys, `id` first, then first-seen order; a missing key is `null` |
| array with any non-object element | one `result` column, one row per element |
| single object (e.g. `INFO`) | its keys, one row |
| scalar or NULL | one `result` column, one row |
| NONE (`LET`, `DEFINE`, …) | no columns, no rows |

Each statement of a multi-statement query becomes its own result set, in
order; Tabularis shows `additional_results` as separate result tabs. If a
statement fails, the error names it ("Statement 2 of 3 failed: …") and says
that statements outside a transaction run independently.

`affected_rows` is always 0 for now; SurrealDB returns the affected records
instead of a count, and DELETE returns nothing by default, so a count would
be misleading. Milestone 2 revisits result shapes.

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
`null`, because the host has a single null value. This is a representation
limit, not a conversion loss on the server.

## Unsupported operations

The manifest declares `readonly: true` and `manage_tables: false`, so the
host hides grid editing and table management. If one of these methods is
called anyway, the plugin answers with the documented "not implemented"
response: code `-32601` and a message that starts with
`Method not found: <method>` (followed by the reason). Unknown methods,
`get_schema_snapshot` and `execute_query_batch` get the same code so the
host uses its fallbacks.

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

Read on 2026-10-16.

Tabularis:

- Building plugins: <https://tabularis.dev/wiki/building-plugins>
- Plugin protocol: <https://tabularis.dev/wiki/plugin-protocol>
- Publishing plugins: <https://tabularis.dev/wiki/plugin-development>
- Registry manifest schema (driver):
  <https://registry.tabularis.dev/manifest.schema.json?kind=driver>; the
  manifest passes `npx @tabularium/cli@0.1.0 validate .tabularium --registry https://registry.tabularis.dev --kind driver`
- TabularisDB/tabularis at `bebdd9d1`: `plugins/PLUGIN_GUIDE.md`,
  `plugins/manifest.schema.json`, `src-tauri/src/plugins/{driver,rpc,manager}.rs`,
  `src/types/plugins.ts`, `src/utils/driverCapabilities.ts`,
  `src/utils/editor.ts` (table-browse SQL), `src/utils/identifiers.ts`,
  `packages/create-plugin` 0.4.1
- Sibling plugins used for conventions: tabularis-trino-plugin 0.1.6,
  tabularis-libsql-plugin 1.0.1, tabularis-elasticsearch-plugin,
  tabularis-mongodb-plugin

SurrealDB:

- RPC protocol: <https://surrealdb.com/docs/reference/rest-api/rpc-protocol>
  (lists `query_cancel` and `query_stream` from 3.3.0)
- CBOR protocol (tags): <https://surrealdb.com/docs/reference/rest-api/cbor-protocol>
- INFO statement: <https://surrealdb.com/docs/surrealql/statements/info>
- 2.x → 3.x migration:
  <https://surrealdb.com/docs/build/migrating/from-old-surrealdb-versions/2x-to-3x>
- Rust SDK: <https://surrealdb.com/docs/sdk/rust> (SDK 3.3.4, servers 2.0+,
  Rust 1.95+) and the crate's feature list (`cargo info surrealdb@3.3.4`);
  docs.rs had no successful build of 3.3.4 at the time
