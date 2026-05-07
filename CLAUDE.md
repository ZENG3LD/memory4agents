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
| `/health` | Server status + index stats |

## impl_trait tracking (v0.2.10+)

Column `impl_trait TEXT` on `symbols` — populated for `kind=impl` only. NULL for inherent impls. Use `/ast/impls`, `/ast/inherent_impls`, `/ast/impl_summary` endpoints. `/ast/find_twins` returns `implementors` section when seed is a trait.

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
