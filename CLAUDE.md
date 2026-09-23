# memory4agent — Code Intelligence Server

Renamed from `dig2memory` (2026-09-23) — this crate is code-intelligence and
memory infrastructure for coding agents (Claude Code, Codex, Kimi, Grok CLIs),
not just an AST index. The rename plan is
`docs/memory4agent/plans/2026-09-23-memory4agent-plan.md`; read its
"Executor amendments" section first, it overrides the phase text below it.

- **Port**: 18200 (range 18200-18299)
- **What today**: AST index + dependency graph + fuzzy search over entire
  nemo workspace. Later phases (see the plan) add an MCP server, a SCIP
  precise index, personalized-PageRank repo-map, a decision log, and a
  hybrid BM25+vector retrieval core.
- **Stack**: tree-sitter (Rust/TS/Python/Go grammars), SQLite (WAL), axum,
  cargo_metadata.
- **Location**: this directory is still named `dig2memory/` on disk — the
  directory/repo rename is plan piece 0.1, deliberately deferred to land
  together with the first deploy of the renamed binary. The crate/package
  name, both binaries, env vars, and the d-watchdog service name are
  `memory4agent` already (piece 0.2).
- **Binaries**: `memory4agent-server` (HTTP daemon) and `memory4agent` (CLI,
  reads the SQLite index directly — no server needed).
- **DB**: `./data/index.db` (~2.76 GB, ~580K symbols, ~30K files
  as of 2026-09). The on-disk directory name is unchanged by the rename —
  only the env var that points at it changed name (`MEMORY4AGENT_DATA_DIR`).
  The 126 KB `dig2memory/data/index.db` some older docs pointed at is a
  stale stub, not the live database — always use the env-var path.
- **Agent access**: Only agents with Bash + `permissionMode: default` can
  curl. WebFetch doesn't reach localhost (runs on Anthropic servers). Agents
  with `dontAsk` don't get Bash even if listed in tools.
- **Dedicated agent**: `refactoring-explorer` — deep analysis agent with
  Bash+Grep+Read, 200 turns. Use ONLY when task touches 2+ crates
  (cross-crate deps, impact analysis). For simple exploration use regular
  `explorer`.
- **New agents need IDE restart** to register.
- **DB access from a route handler**: always
  `crate::server::state::with_db(&state, move |conn| { ... })` — runs the
  closure in `spawn_blocking` over one connection checked out from
  `state.reader_pool` for the closure's duration only. Never touch
  `state.writer` directly from a route handler (that's the single writer —
  see "Storage engine" below). `/health` is the one exception: it never
  touches the database at all, see below.

## Storage engine

`AppState`, every route (including every heavy analytics route in
`server::routes::usage`), and the async `Indexer` are wired onto
`crate::storage` (plan pieces 0.3-0.6, all landed). The original mutex-per-
connection storage (`crate::db::schema::init_schema` + a raw
`Arc<Mutex<Connection>>` in `AppState`) is retired — `crate::db` now holds
only `reader`/`writer`, pure query/mutation functions over a
`&Connection`/`&mut Connection` regardless of how that connection was
obtained, called from both `with_db` closures (reader pool) and `WriteOp`
closures (the batch writer).

`crate::storage`, modeled on `servertoolkit-db` the way
`mail4agent-store-sqlite` modeled the same source — own types in this
crate, no `servertoolkit` dependency:
  - `storage::Db` — the sole writer connection (WAL + foreign_keys=ON),
    never exposed to a route handler; only `storage::BatchWriter` and the
    migration bootstrap touch it.
  - `storage::DbPool` — N read-only connections (`MEMORY4AGENT_READER_POOL_SIZE`,
    default 8), each additionally pragma'd `query_only = ON` so an
    accidental write on a pooled connection fails loudly
    (`SQLITE_READONLY`) instead of racing the writer silently.
  - `storage::BatchWriter` — one dedicated worker thread draining a queue
    into batched transactions (`max_ops` 256 or `flush_window` 50ms,
    whichever first), one `SAVEPOINT` per op so a single op's failure
    doesn't lose the rest of the batch, `barrier()`/`barrier_async()` as the
    durability point, `stats()` for enqueued/committed/failed/batches.
  - `storage::migrations` — `Migration` + `MigrationRunner`, a versioned
    `schema_migrations(version, label, applied_at)` ledger (replacing
    `crate::db::schema::init_schema`'s swallow-errors `ALTER TABLE` chain),
    plus `seed_legacy_versions_if_needed` — a one-time backfill that
    inspects an existing database's columns/tables and records which
    legacy versions (v1-v7, mirroring `crate::db::schema`'s own history) are
    already satisfied, without re-running their SQL. This is what lets the
    live 2.76 GB database adopt the new ledger without a "duplicate column"
    failure on first boot under the new code.

`AppState` is `{ writer: BatchWriter, reader_pool: DbPool, data_dir, stats_cache }`.
Boot sequence in `bin/server.rs`: `Db::open` (writer connection) →
`seed_legacy_versions_if_needed` → `MigrationRunner::run(legacy_schema_migrations())`
on that same connection → `DbPool::open_with_size` (reader pool) →
`BatchWriter::new` (writer). `index::indexer::Indexer::run(reader_pool,
writer, req)` is `async`: reads (mtime filtering) go through `reader_pool`,
writes are one `WriteOp` per file/phase enqueued on `writer`, with
`barrier_async()` between phases that must see an earlier phase's writes
committed (stale-file removal, callsite/type-use count recompute, the final
`indexed_at` stamp). A `POST /index` caller sees committed data the instant
the call returns — the last thing `Indexer::run` does is `barrier_async()`.
Covered by `cargo test -p memory4agent --release --lib storage::`,
`--lib index::indexer::`, and `--lib server::` (the last includes a test
that a slow read on the pool never blocks a concurrent read, a `/health`
answer, or a write landing on the writer — the fix for the retired
global-mutex defect).

## Starting the server

```bash
cd dig2memory && cargo run --release --bin memory4agent-server
```

Or with explicit data dir:
```bash
cd dig2memory && MEMORY4AGENT_DATA_DIR="./data" ./target/release/memory4agent-server.exe &
```

Env vars: `MEMORY4AGENT_PORT` (default 18200), `MEMORY4AGENT_DATA_DIR`
(default `./data`; the live service sets this to `./data`),
`MEMORY4AGENT_READER_POOL_SIZE` (default 8 — number of concurrent
`storage::DbPool` connections, see "Storage engine" above). The CLI's
`--db` flag overrides both env vars; if
unset it checks `MEMORY4AGENT_DATA_DIR` then falls back to the legacy
`DIG2MEMORY_DATA_DIR` for scripts/agent configs written before the rename.

## Re-index (incremental — only changed files)

```bash
curl -X POST "http://127.0.0.1:18200/index" \
  -H "Content-Type: application/json" \
  -d '{"workspace_id":"nemo","workspace_name":"nemo","root_path":"c:/src/nemo","force":false}'
```

## API (all GET unless noted, all return JSON)

| Endpoint | Use case |
|----------|----------|
| `/ast/search?q=NAME` | Fuzzy symbol search (functions, structs, traits) |
| `/ast/symbols?file=PATH&workspace=nemo` | List all symbols in a file |
| `/ast/callers?sym=NAME&workspace=nemo` | Who calls this symbol |
| `/ast/deps?file=PATH&workspace=nemo` | File dependencies |
| `/graph/crates?workspace=nemo` | Crate dependency tree |
| `/graph/impact?file=PATH&workspace=nemo` | What breaks if this file changes (file-level, BFS over `file_edges`) |
| `/graph/impact_symbol?sym=NAME&workspace=nemo&depth=N` | Symbol-level impact: BFS over callers via `call_edges`, N hops (default 2, max 4). Name-based until SCIP (phase 2) — two same-named symbols in the same workspace/file are only disambiguated when `call_edges.caller_symbol_id` is resolved; the callee side always matches by bare name. |
| `/graph/hotspots?workspace=nemo` | Most-connected modules |
| `/graph/resolve?sym=NAME&workspace=nemo` | Which crate owns this symbol |
| `/ast/duplicates?name=NAME` | List all definitions of NAME, classified by exact/structural/name-only |
| `/ast/all_duplicates?workspace=ID` | All symbols with 2+ definitions, ranked by count |
| `/ast/semantic_duplicates?workspace=ID` | Find type-shape duplicates: same struct fields / enum variants / trait methods under different names. Params: `path_prefix` (optional), `kind` (optional, struct/enum/trait/type_alias/all), `loose` (optional bool, default false — ignores field/method names), `min_names` (optional, default 1), `min_locations` (optional, default 2), `min_fields` (optional, default 0), `max_fields` (optional, default 999999) |
| `/ast/cluster?seed=NAME&workspace=ID&depth=N&path_prefix=P&include_dead=true` | BFS use-graph around seed type: nodes + edges to depth N (default 2, max 4). Each node has `callsite_count`. Edges: `uses` (type appears in signature) or `uses` (caller). |
| `/ast/parallel_systems?workspace=ID&path_prefix=P&min_size=N` | Find weakly-connected components of the structural type graph (size >= min_size, default 5). Returns components + `parallel_candidates` pairs with `profile_distance` < 0.2 (euclidean over struct%/enum%/trait%/alias%/dead%). |
| `/ast/dead_in_group?fingerprint=HASH&workspace=ID` | All symbols sharing a shape fingerprint, each with `callsite_count` and `is_dead` flag. |
| `/ast/type_usage?name=NAME&workspace=ID&path_prefix=P&breakdown=true` | Who uses NAME as a type: per-definition member list + users from type_use_edges. `breakdown=true` adds `users_by_kind` map (field/param/return/body/use_decl counts). |
| `/ast/dead_types?workspace=ID&path_prefix=P&kind=struct` | All symbols with `type_use_count=0 AND callsite_count=0` (dead candidates; includes signature+body+use_decl edges). |
| `/ast/usage_hotspots?workspace=ID&path_prefix=P&top=N` | Top-N types by `type_use_count` desc (most used as types across workspace). |
| `/ast/orphans?workspace=ID&path_prefix=P&kind=struct&exclude_pub_use=true` | Strict dead: `type_use_count=0 AND callsite_count=0` AND no meaningful `type_use_edges` row. `exclude_pub_use=true` (default) means `pub_use_decl` re-exports don't count as real usage. |
| `/ast/role_groups?workspace=ID&path_prefix=P&token=theme&min_size=3&kind=trait&use_synonyms=false` | Group types by shared name tokens. `token` (optional): only return group for that token. `min_size` (optional, default 3). `kind` (optional). `use_synonyms=true`: merge synonym tokens under canonical (e.g. style+theme+skin → "style"); response shape changes to `{groups:[{canonical_token,raw_tokens,size,members}]}`. Without `use_synonyms` returns `{groups:[{token,size,members}]}`. |
| `/ast/find_twins?seed=NAME&workspace=ID&path_prefix=P` | Find types similar to seed by 5 heuristics: `exact_shape` (same strict fingerprint), `loose_shape` (same loose fingerprint, same kind), `shared_token` (overlapping name tokens), `synonym_token` (tokens in same synonym group, includes `synonym_pair:[seed_tok,sym_tok]`), `similar_size` (same kind, field_count ±1). When seed is a trait — also returns `implementors` list. |
| `/ast/impls?trait=NAME&workspace=ID&path_prefix=P` | All types implementing trait NAME. Returns `{trait, implementors:[{type,file,line,impl_id}], total}`. |
| `/ast/inherent_impls?type=NAME&workspace=ID&path_prefix=P` | All inherent impl blocks for type NAME (no trait). Returns `{type, impls:[{file,line,impl_id,method_count}]}`. |
| `/ast/impl_summary?workspace=ID&path_prefix=P` | All traits with their implementors, sorted by `implementor_count` desc. Returns `{traits:[{name,implementor_count,implementors:[...]}]}`. |
| `/ast/workspace_summary?workspace=ID&path_prefix=P` | High-level workspace report: `totals` (files/symbols/structs/enums/traits/type_aliases/modules/functions), `dead` (dead struct/enum/trait counts + `by_subsystem` top-10), `hotspots_top10` (by type_use_count), `duplicate_clusters` (exact_count, loose_count, biggest_strict top-5), `role_groups_top` (top-10 by synonym-canonical size), `potential_savings` (exact_dup_locations, loose_dup_locations, estimated_lines_drop). |
| `/ast/cleanup_plan?workspace=ID&path_prefix=P&limit=20` | Prioritised cleanup action list. Each action: `priority`, `kind` (exact_duplicate_collapse / dead_subsystem_remove / synonym_role_consolidate), `description`, `members`, `estimated_savings_lines`. Priority 1–N ordered by savings desc within each kind tier. Default limit 20. |
| `/ast/parallel_systems?workspace=ID&path_prefix=P&min_size=N&mode=wcc` | `mode=wcc` (default): weakly-connected components of structural type graph + `parallel_candidates` by euclidean profile distance. `mode=token`: profile-clustering by semantic name tokens — `groups` + `candidate_pairs` with cosine similarity ≥ 0.7 and no shared members. |
| `/health` | Pure liveness probe — never touches the database. Returns the LAST KNOWN stats from an in-memory cache (`ok`, `db_path`, `workspace_count`, `symbol_count`, `file_count`, `stats_age_secs`), refreshed by a background task every 30s (`STATS_REFRESH_INTERVAL`, `src/server/stats.rs`). Answers instantly even while `/index` or a heavy `/ast`/`/graph`/`/usage` query holds the DB lock. |
| `/ready` | Readiness probe — runs a bounded (3s) `SELECT 1` via `spawn_blocking`; `503` if the DB is currently unreachable/locked rather than hanging. |

`POST /index` triggers indexing (see "Re-index" above).

## Crate graph (multi-manifest, piece 0.7)

`graph::crates::parse_multi_manifest_workspace` (used by `/index`'s Phase 6,
and by `/graph/crates`'s underlying `crate_nodes`/`crate_dep_edges` tables)
discovers every submodule-rooted `Cargo.toml` one level under the indexed
root and runs `cargo metadata --no-deps` per manifest — `nemo` itself has no
root `Cargo.toml`, so there is no single-manifest shortcut. Every reindex
prunes crate nodes/edges no longer present in the fresh parse
(`db::writer::delete_stale_crate_nodes`/`delete_stale_crate_dep_edges`) —
replace-all-for-workspace semantics, not upsert-only, so a submodule removed
from disk actually disappears from `/graph/crates` on the next `/index`
instead of accumulating dead rows forever.

## File watcher (piece 0.9)

`crate::watch::spawn_watcher`, wired from `bin/server.rs` right after the
stats refresher, watches a workspace root and runs one targeted incremental
`Indexer::run_changed_paths` call per debounced batch of filesystem events
— off the request path, through the same `reader_pool`/`writer` every other
caller uses, over exactly the paths that changed rather than a full
workspace walk (see "Changed-paths indexing" below). Filtered through the
same exclusion list `index::indexer`'s own workspace walk uses
(target/node_modules/.cargo/.git/build/hidden dirs), checked against every
ancestor path component of each event (a watcher gets one full path per
event, unlike a `WalkDir` traversal that simply never descends into an
excluded directory). There is no single "the workspace" this server owns by
default, so the watcher only starts when a root path is explicitly
configured:

- `MEMORY4AGENT_WATCH_ROOT_PATH` — unset by default; the watcher does not
  start at all until this is set.
- `MEMORY4AGENT_WATCH_WORKSPACE_ID` (default `nemo` once a root path is set).
- `MEMORY4AGENT_WATCH` (default `1`; `0` disables even when a root path is set).
- `MEMORY4AGENT_WATCH_DEBOUNCE_MS` (default `2000`).

If a debounced batch arrives while a watch-triggered run is still in
flight, it is never dropped and never spawns a second concurrent run — its
changed paths are merged into the one pending set the in-flight run's
driver will pick up and run once, immediately after, as a single follow-up
pass (`watch::CoalesceState`).

## Single-flight indexing (per workspace)

`BatchWriter` serializes individual write *ops* (one `SAVEPOINT` per op,
one transaction per flushed batch) — it does NOT order two whole
multi-phase `Indexer::run` calls against each other. Two concurrent runs
for the SAME workspace could otherwise interleave phases (e.g. a
`force=true` run's wipe landing between another run's per-file upserts).
`crate::index::lock::IndexLocks`, held in `AppState::index_locks`, closes
that gap: one `tokio::sync::Mutex<()>` per workspace id, acquired for the
FULL duration of a run. Both call sites hold it end-to-end:

- `POST /index` (`server/routes/index.rs`) acquires the lock, then calls
  `Indexer::run` — chosen behaviour when the lock is already held: **wait**,
  not a "409 index in progress" rejection (a caller-invoked `/index` wants
  the freshest index, not a poll).
- `crate::watch`'s debounced driver (`watch::run_reindex_driver`) acquires
  the SAME lock before each `Indexer::run_changed_paths` call.

Different workspace ids never contend with each other. Covered by
`cargo test -p memory4agent --release --lib index::lock::` (the primitive,
proven via deterministic channel-signaled hold/release, not timing) and
`--lib index::indexer::concurrent_runs_for_the_same_workspace_never_interleave_under_the_shared_lock`
(the same guarantee through real `Indexer::run` calls).

## Changed-paths indexing (`Indexer::run_changed_paths`)

`Indexer::run` (full workspace walk, used by `POST /index`) is unchanged.
`Indexer::run_changed_paths(reader_pool, writer, req, changed_paths)` is a
second entry point — used only by the watcher — that skips the
~24k-file workspace walk and the workspace-wide stored-mtime diff entirely:
the caller already knows exactly what changed (a real filesystem event is a
strictly better signal than an mtime comparison), so every given path that
still exists is (re)indexed unconditionally, and a path that no longer
exists on disk has its stored rows deleted directly
(`db::writer::delete_specific_files`) instead of via a full stale-file
scan. The crate-graph phase (a `cargo metadata` subprocess per submodule)
only runs if at least one changed path is itself named `Cargo.toml` — a
single edited `.rs` file never triggers it. `req.force` is ignored in this
mode; a targeted incremental pass never wipes workspace data.

## impl_trait tracking (v0.2.10+)

Column `impl_trait TEXT` on `symbols` — populated for `kind=impl` only. NULL
for inherent impls. Use `/ast/impls`, `/ast/inherent_impls`,
`/ast/impl_summary` endpoints. `/ast/find_twins` returns `implementors`
section when seed is a trait. All four `LanguageSupport` implementations
(`rust`/`ast/symbols.rs`, `typescript`, `python`, `go`) must set this field
on every `Symbol` they construct — a missing field here is a compile error,
not a runtime gap, since `Symbol` has no `Default` impl by design (every
field is a deliberate extraction decision, not a value that's safe to
default silently).

## Synonym token map (v0.2.9+)

Static hardcoded groups in `src/ast/synonyms.rs`. 12 groups / 45 tokens covered:

| Canonical | Synonyms |
|-----------|----------|
| style | theme, skin, appearance |
| default | defaults |
| state | status, data |
| config | settings, options, spec, params |
| view | render, display, draw |
| id | key, handle, ref |
| size | dim, extent, bounds |
| event | msg, message, signal |
| kind | type, variant, category |
| builder | factory, maker, constructor |
| action | command, op, operation |
| input | control, trigger |

Tokens not in any group are standalone (returned as-is by `canonical_label`).

## type_use_count sources (v0.2.8+)

`type_use_count` counts edges from four sources:
- **signature** — struct fields, fn params/return (`field`, `param`, `return`, `generic`)
- **body** — PascalCase type names inside function/method bodies — `use_kind = "body"`
- **use_decl** — `use crate::foo::Bar;` private import — `use_kind = "use_decl"`
- **pub_use_decl** — `pub use crate::foo::Bar;` re-export — `use_kind = "pub_use_decl"`

`/ast/orphans` with `exclude_pub_use=true` (default) treats `pub_use_decl` as non-usage, so re-exported types are not falsely marked as orphans.

## name token stop-words (tokenize_name)

`with`, `for`, `into`, `from`, `as`, `the`, `and`, `or`, `not`, `new`, `get`, `set`, `is`, `to`, `of`, `in`, `on`, `at`, `by`. Tokens < 2 chars also dropped.
