# memory4agent

Code-intelligence and memory infrastructure for coding agents (Claude Code,
Codex, Kimi, Grok CLIs and others). Indexes codebases using tree-sitter AST
parsing, builds dependency graphs, and provides fuzzy symbol search, caller
tracking, and impact analysis. Formerly published as `dig2memory` — this is
the same server and CLI, renamed and under active extension (MCP tool
surface, SCIP precise index, personalized-PageRank repo map, a decision
log, and hybrid BM25+vector retrieval are all in progress).

**Supported languages**: Rust, TypeScript/JavaScript (TSX/JSX), Python, Go

Built for [Claude Code](https://claude.ai/code) agents but works with any AI coding assistant that can run shell commands.

## Features

- **Multi-language** — Rust, TypeScript/JS/TSX/JSX, Python, Go (pluggable via `LanguageSupport` trait)
- **Fuzzy symbol search** — find functions, classes, structs, interfaces by name (trigram index)
- **Caller tracking** — who calls a given function/method (indexed, not grep)
- **File dependencies** — what a file imports (`use`, `mod`, `import`, `from`)
- **Impact analysis** — what breaks if a file changes (reverse dependency graph)
- **Crate/package dependency graph** — workspace-level relationships via cargo_metadata
- **Hotspots** — most depended-upon files in the codebase
- **Symbol-to-crate resolution** — which crate/package owns a symbol

## Architecture

A single Cargo package with two binaries, not a multi-crate split:

```
src/
├── ast/          # tree-sitter parsing, symbol/call/fingerprint extraction, one module per language
├── db/           # original storage: Arc<Mutex<Connection>>, shared by every route + the indexer
├── storage/      # replacement storage engine: Db (single writer) + DbPool (read-only pool) +
│                 # BatchWriter (batched, SAVEPOINT-isolated writes) + versioned migrations —
│                 # not yet wired onto AppState/the indexer, see CLAUDE.md's "Storage engine"
├── graph/        # crate dependency graph (cargo_metadata) + file dependency graph
├── index/        # the indexer: walks a workspace, parses, writes symbols/edges
├── search/       # trigram fuzzy search
├── server/       # axum HTTP server: routes, AppState, registration, stats cache
└── bin/
    ├── server.rs # memory4agent-server — the HTTP daemon (this is what d-watchdog runs)
    └── cli.rs    # memory4agent — CLI, reads the SQLite index directly, no server needed
```

**Stack**: tree-sitter (Rust/TS/Python/Go grammars), rusqlite (bundled), cargo_metadata, clap, axum + tokio (server only)

## Quick Start

### Build

```bash
# All languages (default)
cargo build --release

# Rust only (faster compile)
cargo build --release --no-default-features --features lang-rust
```

This produces two binaries:
- `target/release/memory4agent` — CLI (recommended for AI agents)
- `target/release/memory4agent-server` — HTTP server

### Feature Flags

| Feature | Languages | Default |
|---------|-----------|---------|
| `lang-rust` | `.rs` | Yes |
| `lang-typescript` | `.ts`, `.tsx`, `.js`, `.jsx` | Yes |
| `lang-python` | `.py` | Yes |
| `lang-go` | `.go` | Yes |

### Index a workspace

Start the server and trigger indexing:

```bash
# Start server
MEMORY4AGENT_DATA_DIR="./data" ./target/release/memory4agent-server &

# Index your workspace
curl -s -X POST http://127.0.0.1:18200/index \
  -H "Content-Type: application/json" \
  -d '{
    "workspace_id": "my-project",
    "workspace_name": "my-project",
    "root_path": "/path/to/your/workspace",
    "force": false
  }'
```

Indexing is incremental — only changed files are re-parsed on subsequent runs.

### CLI Usage

```bash
# Set up shortcuts
DB="./data/index.db"
DM="./target/release/memory4agent"

# Search for a symbol
"$DM" --db "$DB" search "MyStruct"

# Who calls this function?
"$DM" --db "$DB" callers "process_message"

# What depends on this file?
"$DM" --db "$DB" impact "src/core/engine.rs"

# Most important files in the codebase
"$DM" --db "$DB" hotspots --limit 20

# Crate dependency graph
"$DM" --db "$DB" crates

# Which crate owns this symbol?
"$DM" --db "$DB" resolve "Config"

# List symbols in a file
"$DM" --db "$DB" symbols "src/main.rs"

# File dependencies
"$DM" --db "$DB" deps "src/core/mod.rs"

# JSON output (for structured processing)
"$DM" --db "$DB" --json search "MyStruct"
```

### HTTP API

All endpoints return JSON. Default port: 18200.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Server status + index stats |
| `/index` | POST | Trigger incremental indexing |
| `/ast/search?q=NAME` | GET | Fuzzy symbol search |
| `/ast/symbols?file=PATH&workspace=ID` | GET | Symbols in a file |
| `/ast/callers?sym=NAME&workspace=ID` | GET | Callers of a symbol |
| `/ast/deps?file=PATH&workspace=ID` | GET | File dependencies |
| `/graph/crates?workspace=ID` | GET | Crate dependency graph |
| `/graph/impact?file=PATH&workspace=ID` | GET | Reverse file dependencies |
| `/graph/hotspots?workspace=ID&limit=N` | GET | Most connected files |
| `/graph/resolve?sym=NAME&workspace=ID` | GET | Symbol-to-crate mapping |

See `CLAUDE.md` for the full endpoint list (duplicate/dead-code/usage analysis routes).

## Claude Code Integration

### As a Skill

Copy `skills/dig2memory/SKILL.md` to your project's `.claude/skills/memory4agent/SKILL.md`. Then use `/memory4agent search MyStruct` in Claude Code.

### As an Agent

Copy `agents/refactoring-explorer.md` to your project's `.claude/agents/`. This creates a specialized agent for cross-crate dependency analysis that uses the CLI directly.

### In CLAUDE.md

Add CLI instructions to your project's `CLAUDE.md` so all agents know about memory4agent:

```markdown
## memory4agent (Code Intelligence)

CLI: `memory4agent/target/release/memory4agent --db memory4agent/data/index.db`

Commands: search, symbols, callers, deps, impact, hotspots, crates, resolve
Add --json for structured output.
```

## Performance

Tested on a mixed-language workspace (Rust + TS + Python + Go):

| Metric | Value |
|--------|-------|
| Files indexed | 30,618 |
| Symbols extracted | 580,612 |
| Full index | ~5 min |
| Incremental re-index | seconds |
| Database size | ~2.76 GB |
| Symbol search | <50ms |
| Callers query | <100ms |

## Support the Project

If you find this tool useful, consider supporting development:

| Currency | Network | Address |
|----------|---------|---------|
| USDT | TRC20 | `TNxMKsvVLYViQ5X5sgCYmkzH4qjhhh5U7X` |
| USDC | Arbitrum | `0xEF3B94Fe845E21371b4C4C5F2032E1f23A13Aa6e` |
| ETH | Ethereum | `0xEF3B94Fe845E21371b4C4C5F2032E1f23A13Aa6e` |
| BTC | Bitcoin | `bc1qjgzthxja8umt5tvrp5tfcf9zeepmhn0f6mnt40` |
| SOL | Solana | `DZJjmH8Cs5wEafz5Ua86wBBkurSA4xdWXa3LWnBUR94c` |

## License

MIT
