# dig2memory — Code Intelligence Server

- **Port**: 18200 (range 18200-18299)
- **What**: AST index + dependency graph + fuzzy search over entire nemo workspace
- **Stack**: tree-sitter-rust, SQLite (WAL), axum, cargo_metadata
- **Location**: `dig2memory/` submodule (2 crates: core + server)
- **DB**: `data/index.db` (~700MB, 250K symbols, 13K files)
- **Agent access**: Only agents with Bash + `permissionMode: default` can curl. WebFetch doesn't reach localhost (runs on Anthropic servers). Agents with `dontAsk` don't get Bash even if listed in tools.
- **Dedicated agent**: `refactoring-explorer` — deep analysis agent with Bash+Grep+Read, 200 turns. Use ONLY when task touches 2+ crates (cross-crate deps, impact analysis). For simple exploration use regular `explorer`.
- **New agents need IDE restart** to register.

## Starting the server

```bash
cd dig2memory && cargo run --release --bin dig2memory-server
```

Or with explicit data dir:
```bash
cd dig2memory && DIG2MEMORY_DATA_DIR="./data" ./target/release/dig2memory-server.exe &
```

## Re-index (incremental — only changed files)

```bash
curl -X POST "http://127.0.0.1:18200/index" \
  -H "Content-Type: application/json" \
  -d '{"workspace_id":"nemo","workspace_name":"nemo","root_path":"c:/src/nemo","force":false}'
```

## API (all GET, all return JSON)

| Endpoint | Use case |
|----------|----------|
| `/ast/search?q=NAME` | Fuzzy symbol search (functions, structs, traits) |
| `/ast/symbols?file=PATH&workspace=nemo` | List all symbols in a file |
| `/ast/callers?sym=NAME&workspace=nemo` | Who calls this symbol |
| `/ast/deps?file=PATH&workspace=nemo` | File dependencies |
| `/graph/crates?workspace=nemo` | Crate dependency tree |
| `/graph/impact?file=PATH&workspace=nemo` | What breaks if this file changes |
| `/graph/hotspots?workspace=nemo` | Most-connected modules |
| `/graph/resolve?sym=NAME&workspace=nemo` | Which crate owns this symbol |
| `/ast/duplicates?name=NAME` | List all definitions of NAME, classified by exact/structural/name-only |
| `/ast/all_duplicates?workspace=ID` | All symbols with 2+ definitions, ranked by count |
| `/ast/semantic_duplicates?workspace=ID` | Find type-shape duplicates: same struct fields / enum variants / trait methods under different names. Params: `path_prefix` (optional), `kind` (optional, struct/enum/trait/type_alias/all), `loose` (optional bool, default false — ignores field/method names), `min_names` (optional, default 1), `min_locations` (optional, default 2), `min_fields` (optional, default 0), `max_fields` (optional, default 999999) |
| `/ast/cluster?seed=NAME&workspace=ID&depth=N&path_prefix=P&include_dead=true` | BFS use-graph around seed type: nodes + edges to depth N (default 2, max 4). Each node has `callsite_count`. Edges: `uses` (type appears in signature) or `uses` (caller). |
| `/ast/parallel_systems?workspace=ID&path_prefix=P&min_size=N` | Find weakly-connected components of the structural type graph (size >= min_size, default 5). Returns components + `parallel_candidates` pairs with `profile_distance` < 0.2 (euclidean over struct%/enum%/trait%/alias%/dead%). |
| `/ast/dead_in_group?fingerprint=HASH&workspace=ID` | All symbols sharing a shape fingerprint, each with `callsite_count` and `is_dead` flag. |
| `/ast/type_usage?name=NAME&workspace=ID&path_prefix=P` | Who uses NAME as a type: per-definition member list + users from type_use_edges. |
| `/ast/dead_types?workspace=ID&path_prefix=P&kind=struct` | All symbols with `type_use_count=0 AND callsite_count=0` (honest dead candidates). |
| `/ast/usage_hotspots?workspace=ID&path_prefix=P&top=N` | Top-N types by `type_use_count` desc (most used as types across workspace). |
| `/health` | Server status + index stats |
