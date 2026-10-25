# Echo — databases

Status: draft 2026-10-24. Source of truth for the `db` crates.

## Goal

A database IDE built into Echo, plus the same engine exposed to agents.

The moment it exists for: data that spans repos and engines. Today the Ryse →
Atlas pull sync means the same seller or PM lives in Ryse MySQL and in Atlas
Convex (`ryseSellerUserLinks`), and checking that they agree means two
dashboards, a SQL client, and copy-paste. Self-hosted Convex (Lightbox) and the
ClickHouse setups for Bartr and Ryse add more places to look.

Echo is built for us. Nothing here is aimed at upstream Zed, so we borrow
freely from GPL code (ZQLZ) and couple to Echo's internals where that is
simpler.

## Sources in scope (v1)

| Source | Engine | Notes |
|---|---|---|
| Ryse | MySQL/MariaDB | Local docker `ryse-mysql-1`; dev RDS; prod replica is **read-only** for us |
| Bartr warehouse | ClickHouse 26.3 on Railway | Private network only (`clickhouse.railway.internal:8123`); reached through the `warehouse` relay, see below |
| Ryse analytics | ClickHouse on Railway | Not in the Railway account Echo's CLI sees; access path unconfirmed |
| Atlas | Convex cloud | `landing-ryse-pull*` |
| Ryse admin | Convex cloud | `ryse-admin` |
| Lightbox | Convex self-hosted (ECS) | `lightbox/convex` terraform |
| — | Postgres | Not in use today; comes nearly free from ZQLZ, so included |

## Shape

One engine, two front-ends.

```
crates/db            connections, drivers, permissions, query execution, history
crates/db_ui         dock panel, query tabs, results grid, compare, row detail
crates/db_mcp        MCP server over `db`, registered via `context_server`
```

Agents in Echo are Claude Code over ACP, so the agent surface is an MCP server.
Registering it through `context_server` gives it to both Claude Code threads
and Zed's native agent.

## Borrowed from ZQLZ

ZQLZ (github.com/samurmaykrr/zqlz, GPL-3.0-or-later) is a GPUI database IDE.
Its lower layers have no GPUI dependency.

| Take | Crate | Why |
|---|---|---|
| Yes | `zqlz-core` | `DatabaseDriver`, `Connection`, `DocumentStore`, `SchemaIntrospection`, `Transaction` traits |
| Yes | `crates/zqlz-drivers/crates/`: `zqlz-driver-mysql` (`mysql_async`), `-postgres` (`tokio-postgres`), `-clickhouse` (`clickhouse`) | ~30k lines of working drivers we don't want to write |
| Yes (required) | `zqlz-schema-engine`, `zqlz-analyzer` | Every driver depends on them; neither depends on gpui |
| No | `zqlz-ui`, `zqlz-text-editor`, `zqlz-app`, `zqlz-lsp` | Own editor, `gpui-component`, pinned old gpui; the "LSP" is in-process and tied to their editor |

Consume as a git dependency pinned to a rev, not vendored. None of the taken
crates depend on gpui, so ZQLZ's gpui pin never meets ours. Drivers need tokio;
Echo already has `gpui_tokio`.

If ZQLZ's traits fight us (they carry UI concerns like icons and connection
form schemas), wrap them behind a thin Echo trait in `db` rather than forking.

**Convex is new work.** ZQLZ has no Convex driver. Implement it against
`DocumentStore`, using a deploy/admin key and the HTTP APIs the Convex
dashboard uses (table listing, paginated documents, document deltas). Cloud
and self-hosted take the same code path; only the URL differs.

## Connections

- Defined globally (user settings) and per project (`.zed/settings.json`), so
  opening the Atlas repo surfaces Atlas's connections.
- No secrets in settings. Passwords and Convex keys go in the macOS keychain
  via Zed's existing credentials provider. Prod Convex keys are allowed.
- Each connection has an environment tag: `local | dev | prod`. The tag drives
  colour and confirmation prompts, not permissions.
- SSH tunnels: ZQLZ drivers support them; expose if a source needs it.

### ClickHouse behind a relay (Bartr)

Bartr's ClickHouse has no public endpoint by design: the Railway public domain
was deleted because ClickHouse does not throttle failed logins, and its README
says not to re-add one. The only external surface is the `warehouse` service,
gated by `x-relay-secret`, with one route per credential:

| Route | Runs as | Use in Echo |
|---|---|---|
| `POST /query` | `console_reader` (`readonly=2`, capped limits) | The panel |
| `POST /query/ai` | `ai_reader` (built for model-authored SQL) | MCP tools |

So ClickHouse connections have two transports:

- **Direct HTTP** (ZQLZ `clickhouse` driver): local docker and any instance
  with a reachable HTTP port.
- **Relay**: a small transport in `db` that posts SQL to a `warehouse`-style
  route with the relay secret from the keychain. Always read-only; the
  permission probe is skipped and the connection is pinned read-only.

Ryse's ClickHouse is assumed to follow one of these two; confirm before
building its connection.

### Remote projects

When a project is opened over SSH (Zed remote), its connections run on the
remote host, so `localhost`/docker connections mean the remote's. The `db`
engine runs inside Zed's remote server and the panel talks to it over the
existing project RPC; credentials stay on the local keychain and are sent per
connect. Global (user-level) connections always run locally. Lands after the
local-only path works; the `db` API must not assume it is in-process with the
UI.

## Permissions: the database decides

Editing is allowed wherever the credentials allow it. Echo must know up front
when they don't, and never offer an edit it can't make.

- On connect, each driver probes what it may do:
  - MySQL: `SHOW GRANTS`, `@@read_only` / `@@super_read_only`
  - Postgres: `has_table_privilege`, `transaction_read_only`
  - ClickHouse: the `readonly` setting and grants
  - Convex: an admin/deploy key can write; no finer grain exists
- The result is a per-connection (and where cheap, per-table) capability:
  read-only or writable.
- Read-only connections render without edit affordances, and the MCP tools
  refuse writes before they reach the server.
- A connection may also be pinned read-only in settings, overriding the probe.
- The database's own error is still the final authority; surface it verbatim.

## The panel

A dock panel built on Zed's `ui` crate.

- **Tree:** connections → databases/schemas → tables/collections → columns/fields.
- **Query tab:** a Zed `editor` buffer (SQL highlighting comes from the
  existing SQL language) above a virtualized results grid. For Convex the tab is
  a table browser with field filters instead of free-form SQL.
- **Editing:** cells editable in the grid on writable connections. Edits batch
  into a pending changeset that previews as SQL (or Convex patch) before
  commit. `prod` connections require an extra confirmation.
- **Row detail:** opening a row shows all fields, plus:
  - *context*: rows it references and rows that reference it (foreign keys in
    SQL; `Id<"table">` fields in Convex)
  - *history*: change history where the engine has it. Convex document
    deltas are a forward-only log with limited retention and no per-document
    lookup, so history is opt-in per connection: Echo ingests deltas in the
    background into a local index (sqlez) and answers row history from that.
    Without it, row detail says history is unavailable. MySQL/ClickHouse only
    if an audit table is configured.
- **Query history:** every query run, per connection, searchable, re-runnable.

## Compare

Single-source or cross-source is chosen at the time of looking, not
configured ahead.

- Pick two result sets: any two queries/tables, same or different connections,
  same or different engines.
- Pick a join key on each side (e.g. Ryse `seller.id` ↔ Atlas
  `ryseSellerUserLinks.ryseSellerId`), plus an optional field mapping when
  names differ.
- Output: rows only on the left, only on the right, and matched rows with
  differing fields highlighted.
- Full tables, not capped samples: both sides page in key order and are merged
  as a stream, so nothing past a cap shows up as falsely "only on one side".
- Values compare after normalizing representation (int vs string IDs,
  DATETIME vs epoch ms, NULL vs missing field). A representation-only
  difference gets its own muted "type differs" marker, separate from a real
  value difference, so it is visible without drowning out sync bugs.
- From a compare row, open row detail on either side.
- Saved compares (named, per project) so "Ryse ↔ Atlas sellers" is one click.

## MCP tools

Same engine, same permission rules. The MCP server runs inside Echo, not as a
separate process, so it can open approval UI in the panel.

### Agent access policy

Set per connection, overridable per project (`.zed/settings.json`), so a repo
can loosen or tighten what its agents see:

| Policy | Agent reads | Agent writes |
|---|---|---|
| `off` | Refused; the connection is hidden from `list_connections` | Refused |
| `approve` | Each query shown to you before it runs | Each change approved |
| `allow` | Run freely | Still approved (see below) |

Defaults by environment tag: `prod` → `off`, `dev` → `approve`, `local` →
`allow`. Results from `approve`/`allow` reads go to the model provider; the
setting UI says so.

### Write approval

A setting (global, overridable per connection) picks how agent writes are
approved:

- `preview` (default): Echo shows the exact SQL or Convex patch and row
  counts in the panel; you approve or reject there.
- `prompt`: rely on Claude Code's normal tool permission prompt, which shows
  the tool arguments only.

Agent writes are never unapproved, whichever mode is set.

| Tool | Does |
|---|---|
| `list_connections` | Connections visible in the current project, with env tag and read/write capability |
| `describe` | Schema for a connection, table, or collection |
| `query` | Run a read; results capped and paginated |
| `compare` | Keyed diff across two queries |
| `write` | Only on writable connections; approved via the write approval mode |

## Non-goals (v1)

- Table, trigger, or sequence designers
- Schema migration or schema sync tooling
- Monitoring, explain visualizers
- Import/export beyond copying grid selections
- Redis, Mongo, MSSQL, DuckDB, SQLite (ZQLZ has them; add on demand)

## Anti-goals

- Writing to a source we only have read access to, or appearing able to.
- Secrets in any settings file or repo.
- Agents writing without a human approving the specific change.

## Calls made by the designer (not discussed)

- Three crates split as above.
- Environment tag drives prompts; permission probe drives capability.
- Prod writes need an extra confirmation.
- "History" means per-row change history plus query history.
- "Context" means referenced and referencing rows.
- Agent access policy defaults: prod `off`, dev `approve`, local `allow`.
- Write approval defaults to `preview`; `prompt` is opt-in.
- Bartr ClickHouse goes through the `warehouse` relay; MCP uses `/query/ai`.
- Remote-project support lands after the local path.
- Postgres included because it's nearly free.
- Compare streams full tables in key order rather than diffing capped results.
- Cross-engine values are normalized, with representation-only differences
  marked separately.
- Convex row history comes from an opt-in local delta index.

## Left to the builder

- Grid virtualization approach, cell rendering, and column sizing.
- Exact Convex HTTP endpoints and pagination.
- Result-size caps and streaming strategy.
- Settings schema field names.
- Order of milestones, as long as Ryse MySQL ↔ Atlas Convex compare is usable early.
