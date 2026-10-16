# Tabularis SurrealDB Plugin

SurrealDB driver plugin for [Tabularis](https://tabularis.dev): select a
namespace and database, browse tables, fields and indexes, inspect records,
and run native SurrealQL from the editor and SQL notebooks.

Work in progress, not released yet. Scope and progress:
[TabularisDB/tabularis#1019](https://github.com/TabularisDB/tabularis/issues/1019).
Design decisions and their evidence are in [docs/DESIGN.md](docs/DESIGN.md).

## Contents

- [Features](#features)
- [Supported SurrealDB versions](#supported-surrealdb-versions)
- [Connecting](#connecting)
- [Browsing and querying](#browsing-and-querying)
- [Limitations](#limitations)
- [Installation](#installation)
- [Development](#development)
- [License](#license)

## Features

- Connect with a root, namespace or database user, or a token. Every
  Tabularis connection gets its own SurrealDB session.
- List `namespace/database` pairs, tables (SCHEMAFULL, SCHEMALESS,
  relations), fields and indexes.
- SCHEMAFULL tables show their declared fields. SCHEMALESS tables also show
  fields inferred from their first 100 records.
- Open tables in the grid with paging; filter and sort text typed in the
  grid is passed through as SurrealQL.
- Run any SurrealQL, including graph traversals and multi-statement scripts.
  Each statement gets its own result tab.
- Record ids are shown as `table:id` text you can paste back into a query.
  Objects and arrays open in the JSON viewer. Decimals and large integers
  keep their exact digits.
- Read-only: grid edits and schema changes are disabled.

## Supported SurrealDB versions

| Version | Status |
|---|---|
| 3.x | Supported. Tested against 3.2.3. |
| 2.x | Supported by design. CI runs the live test against `surrealdb/surrealdb:v2` (and `:v3`); not yet verified locally. |
| 1.x | Not supported. |

The plugin connects over SurrealDB's WebSocket RPC protocol (`/rpc`) with
CBOR encoding. See [docs/DESIGN.md](docs/DESIGN.md#supported-surrealdb-versions).

## Connecting

| Field | Value |
|---|---|
| Host | Host name (`localhost`), or a URL: `ws://…`, `wss://…`, `http://…`, `https://…` |
| Port | Port for a plain host name. Default 8000. Ignored when Host is a URL. |
| Database | `namespace/database`. A bare `namespace` lets you pick databases in the explorer. Leave it empty to list every namespace you can see (root users). |
| Username | A SurrealDB system user (root, namespace or database level) |
| Password | That user's password. To sign in with a token, leave Username empty and paste the JWT here. |

Use `wss://` (or `https://`) in Host for servers behind TLS, for example
`wss://my-instance.aws-euw1.surreal.cloud`.

Sign-in level: by default the plugin tries a database user, then a
namespace user, then a root user, and reports every level it tried if all
fail. Namespace and database users need the namespace (and database) in the
Database field.

Connection strings can be imported:

```text
surrealdb://root:root@localhost:8000/namespace/database
```

For TLS, edit Host to a `wss://` URL after importing.

## Browsing and querying

- Opening a table runs `SELECT * FROM <table>` with `LIMIT … START …` for
  the current page.
- Statement results map to the grid as follows: an array of records becomes
  rows with the union of their fields (`id` first, missing fields empty); a
  single object becomes one row; scalars and mixed arrays use a single
  `result` column.
- `USE NS … DB …` inside a query applies to that query only; the
  connection keeps its namespace and database.
- Generate SQL (SELECT, UPDATE, DELETE templates) produces SurrealQL.

Values in the grid:

| SurrealDB | Shown as |
|---|---|
| record id | `person:alice`, `` event:`weird-id` ``, `event:[2024, 'q1']` |
| object, array | JSON |
| decimal, int beyond ±2^53 | exact digits, as text |
| datetime | `2024-01-02T03:04:05.123456789Z` |
| duration | `1h30m` |
| geometry | GeoJSON |
| NONE, NULL | empty (NULL) |

## Limitations

- Read-only: no grid edits, no table or index management.
- Cancelling a query (the Tabularis call timeout) stops waiting for it, but
  the statement keeps running on the server. See
  [Cancellation](docs/DESIGN.md#cancellation-and-timeouts).
- NONE and NULL both show as NULL in grid cells. Inside objects (JSON
  viewer) a NONE field is absent and a NULL field is `null`.
- Fields of SCHEMALESS tables that appear only after the first 100 records
  are not listed in the column inspector; they still appear in query
  results.
- Record links are not shown as foreign keys yet.
- TLS uses the bundled Mozilla root certificates. Custom CA certificates
  and client certificates are not supported yet.
- The grid's filter bar may generate SQL operators SurrealQL does not have
  (for example `LIKE`); type the filter as SurrealQL instead.
- The connection form has no field to pin the sign-in level; `auto` is used.

## Installation

Not yet published to the registry. To try a development build, see
[Development](#development).

Manual install of a release archive, once releases exist: unzip
`surrealdb-plugin-<platform>.zip` into the Tabularis plugins folder as
`plugins/drivers/surrealdb/` and restart Tabularis:

- Linux: `~/.local/share/tabularis/plugins/drivers/surrealdb/`
- macOS: `~/Library/Application Support/tabularis/plugins/drivers/surrealdb/`
- Windows: `%APPDATA%\tabularis\plugins\drivers\surrealdb\`

## Development

Requires Rust (stable), [just](https://github.com/casey/just) and, for the
live test, the `surreal` CLI or Docker.

```bash
just build          # cargo build
just test           # unit tests
just lint           # cargo clippy --all-targets -- -D warnings
just dev-install    # build and copy into the Tabularis plugins folder
```

Live test against a local server (seeds `fixture/seed.surql` into
`test/test` and `test/test_iso`):

```bash
just surreal        # terminal 1: in-memory SurrealDB on 127.0.0.1:8000
just live-test      # terminal 2
```

With Docker instead of the CLI, for example to check 2.x:

```bash
docker run --rm -p 8000:8000 surrealdb/surrealdb:v2 start --user root --pass root memory
```

Interactive JSON-RPC session against the same server, using the real plugin
binary:

```bash
cargo build && just repl
> test
> tables
> columns person
> SELECT ->knows->person.name AS friends FROM person:alice
```

Layout:

```text
src/
├── main.rs            entry point
├── server.rs          stdio loop, concurrent requests, cancel
├── rpc.rs             method routing
├── models.rs          connection fields → endpoint, namespace/database, sign-in
├── handlers/          metadata, query execution, result mapping, templates
├── surreal/           WebSocket RPC client, sessions, error messages
├── utils/             SurrealQL scanner, DEFINE parsing, value conversion
└── bin/test_plugin.rs REPL
tests/live_db.rs       live integration test
fixture/seed.surql     test data
```

## License

Apache-2.0
