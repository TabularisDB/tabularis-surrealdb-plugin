# Tabularis SurrealDB Plugin

SurrealDB driver plugin for [Tabularis](https://tabularis.dev): select a
namespace and database, browse tables, fields and indexes, inspect records,
and run native SurrealQL from the editor and SQL notebooks.

Requires Tabularis 0.26.0 or later; see [Installation](#installation).
Scope and progress:
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
  "Run all" in the editor sends the script as one SurrealQL query, so `LET`
  variables and transactions work across statements, and each statement
  gets its own result tab or error.
- Record links (`record<table>` fields, relation `in`/`out`) appear as
  foreign keys: the ER diagram shows them and the grid can open the linked
  record.
- Record ids are shown as `table:id` text you can paste back into a query.
  Objects and arrays open in the JSON viewer. Decimals and large integers
  keep their exact digits.
- TLS with the connection's SSL tab, including private CA certificates.
- Column statistics in the grid's column menu (row, NULL and distinct
  counts, min / max, top values), computed in SurrealQL. Value statistics on
  columns with more than 100 000 values come from a sample; see
  [Column statistics](docs/DESIGN.md#column-statistics). Needs Tabularis
  0.29.0 or later.
- Optional query timeout (plugin settings).
- Read-only: grid edits and schema changes are disabled. SurrealQL typed
  in the editor runs with your user's own permissions, so editor queries
  can still write.

## Supported SurrealDB versions

| Version | Status |
|---|---|
| 3.x | Supported. Tested against 3.2.3 and 3.2.6 locally, and against `surrealdb/surrealdb:v3` in CI. |
| 2.x | Supported. Tested against 2.7.0 locally, and against `surrealdb/surrealdb:v2` in CI. |
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

TLS: use the SSL tab, or put a `wss://` (or `https://`) URL in Host, for
example `wss://my-instance.aws-euw1.surreal.cloud`.

| SSL mode | Connection |
|---|---|
| Disabled, Preferred | `ws://`, no TLS |
| Required (also when the mode is empty) | `wss://`, certificate not checked |
| Verify CA | `wss://`, certificate must chain to a trusted CA |
| Verify Identity | `wss://`, chain and host name checked (default for `wss://` URLs) |

The SSL tab shows Required until you pick a mode, and that is what the
plugin does: a new connection to a host name uses TLS. For a local server
without TLS (`surreal start` without `--web-crt`), set SSL mode to Disabled,
or put a `ws://` URL in Host; with an empty mode, a URL's scheme decides.

Trusted CAs are the usual public roots, or, when "CA certificate" (PEM) is
set, only the CAs in that file: for a server with a private or self-signed
CA, a certificate from any other CA is refused (as with PostgreSQL's
`sslrootcert`). Required checks no certificate, so it ignores the file. Client certificates are
not used by SurrealDB and are rejected.

The namespace and database must exist. The plugin checks this before
selecting them and reports a missing one, instead of letting SurrealDB 3.x
create it.

Sign-in level: by default the plugin tries a database user, then a
namespace user, then a root user, and reports every level it tried if all
fail. Namespace and database users need the namespace (and database) in the
Database field.

Connection strings can be imported:

```text
surrealdb://root:root@localhost:8000/namespace/database
```

The imported connection has an empty SSL mode, which is Required (see
above); set Disabled for a server without TLS, or Verify Identity to check
the certificate.

Plugin settings (Settings, SurrealDB plugin):

| Setting | Effect |
|---|---|
| Query timeout (seconds) | Stop waiting for a query after this long and say so. Table-grid SELECTs also get a SurrealQL `TIMEOUT`, so the server fails them at that point. 0 or empty: off (the Tabularis call timeout still applies). |

## Browsing and querying

- Opening a table runs `SELECT * FROM <table>` with `LIMIT … START …` for
  the current page.
- Statement results map to the grid as follows: an array of records becomes
  rows with the union of their fields (`id` first, missing fields empty); a
  single object becomes one row; scalars and mixed arrays use a single
  `result` column.
- In a script, statements that never return data (`USE`, `LET`, `DEFINE`,
  `REMOVE`, `BEGIN`, `COMMIT`, …) produce no result tab, so a notebook cell
  shows the first real result. This goes by statement, so the result is the
  same on SurrealDB 2.x and 3.x.
- Writes (CREATE, UPDATE, …) report the records they returned as affected
  rows.
- The query Tabularis builds to follow a record link
  (`WHERE id = 'person:alice'`) is cast to a record id, so it finds the
  linked record.
- `USE NS … DB …` inside a query applies to that query only; the
  connection keeps its namespace and database.
- Generate SQL (SELECT, UPDATE, DELETE templates) produces SurrealQL.
- Column statistics apply the tab's filter, like the grid.

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
- Visual EXPLAIN is off (SurrealDB's plan format differs between versions
  and is documented as subject to change); typing `EXPLAIN` in the editor
  works. See [EXPLAIN](docs/DESIGN.md#explain).
- A transaction cannot span separate runs; keep `BEGIN … COMMIT` in one run.
- The grid's filter bar may generate SQL operators SurrealQL does not have
  (for example `LIKE`); type the filter as SurrealQL instead.
- Filtering by a value from the column statistics popover or a grid cell
  quotes it as text, which SurrealQL never equals a record link, datetime,
  duration or UUID (nor, from the popover, a boolean). Type those filters with a typed literal, for
  example `author = person:1` or `d = d'2024-01-02T00:00:00Z'`. See
  [Column statistics](docs/DESIGN.md#column-statistics).
- The connection form has no field to pin the sign-in level; `auto` is used.

## Installation

Requires Tabularis 0.26.0 or later. Column statistics need 0.29.0.

### From the plugin registry

In Tabularis, open **Settings → Plugins**, search for **SurrealDB** and
install it. You can also pick SurrealDB in the new-connection catalogue and
install it from there, or open `tabularis://install/surrealdb`. Tabularis
downloads the archive for your platform, checks it against the registry's
signed SHA-256, and loads the driver without a restart. Updates show up in
the same place.

The plugin is listed once its first release has been submitted to the
[Tabularium registry](https://registry.tabularis.dev). Until then, install
it manually.

### Manual install from a release archive

Every [GitHub release](https://github.com/TabularisDB/tabularis-surrealdb-plugin/releases)
has one archive per platform:

| Platform | Archive |
|---|---|
| Linux x86-64 | `surrealdb-plugin-linux-x64.zip` |
| Linux ARM64 | `surrealdb-plugin-linux-arm64.zip` |
| macOS, Apple Silicon | `surrealdb-plugin-darwin-arm64.zip` |
| macOS, Intel | `surrealdb-plugin-darwin-x64.zip` |
| Windows x86-64 | `surrealdb-plugin-win-x64.zip` |

An archive holds the executable (`surrealdb-plugin`, or
`surrealdb-plugin.exe` on Windows) and the `.tabularium` manifest, with no
folder around them. Extract it into this folder, creating it if needed:

| OS | Folder |
|---|---|
| Linux | `~/.local/share/tabularis/plugins/drivers/surrealdb/` |
| macOS | `~/Library/Application Support/tabularis/plugins/drivers/surrealdb/` |
| Windows | `%APPDATA%\tabularis\plugins\drivers\surrealdb\` |

Linux (on macOS, use the macOS folder and archive name):

```bash
dir="$HOME/.local/share/tabularis/plugins/drivers/surrealdb"
mkdir -p "$dir"
unzip -o surrealdb-plugin-linux-x64.zip -d "$dir"
```

Windows (PowerShell):

```powershell
$dir = Join-Path $env:APPDATA "tabularis\plugins\drivers\surrealdb"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Expand-Archive surrealdb-plugin-win-x64.zip -DestinationPath $dir -Force
```

Then restart Tabularis. SurrealDB appears in the new-connection catalogue.

- `unzip` keeps the executable bit. If you extract the archive another way
  on Linux or macOS, run `chmod +x surrealdb-plugin` in that folder.
- On macOS, a browser marks downloaded files as quarantined. If macOS
  refuses to run the plugin after a manual install, clear the flag:
  `xattr -dr com.apple.quarantine "$HOME/Library/Application Support/tabularis/plugins/drivers/surrealdb"`.
- To update, quit Tabularis, extract the new archive over the old files
  and start it again. To uninstall, remove the plugin in Settings or delete
  the folder.

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

TLS test (needs the `surreal` and `openssl` CLIs; it creates a private CA,
starts its own TLS server on a free port and stops it):

```bash
just tls-test
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

Releases are cut by pushing a tag; the steps, including the registry
submission, are in [docs/RELEASING.md](docs/RELEASING.md).

Layout:

```text
src/
├── main.rs            entry point
├── server.rs          stdio loop, concurrent requests, cancel
├── rpc.rs             method routing
├── models.rs          connection fields → endpoint, namespace/database, sign-in
├── handlers/          metadata, query execution, result mapping, templates, column stats
├── surreal/           WebSocket RPC client, TLS, sessions, error messages
├── utils/             SurrealQL scanner, DEFINE parsing, value conversion
└── bin/test_plugin.rs REPL
tests/live_db.rs       live integration test
tests/live_tls.rs      live TLS test (private CA, SSL modes)
fixture/seed.surql     test data
```

## License

Apache-2.0
