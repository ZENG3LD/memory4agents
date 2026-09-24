---
name: memory4agent
description: Query the code intelligence index (AST symbols, callers, dependencies, impact analysis). Use when user says memory4agent, find symbol, who calls, impact analysis, code graph, crate deps, hotspots.
argument-hint: <subcommand> [args]
---

You have access to the `memory4agent` CLI tool for code intelligence queries.

The user wants: $ARGUMENTS

## Setup

The CLI binary must be built first:
```bash
cd memory4agent && cargo build --release --bin memory4agent
```

Set these variables in your shell commands:
```bash
DB="path/to/memory4agent/data/index.db"
DM="path/to/memory4agent/target/release/memory4agent.exe"  # or just `memory4agent` if in PATH
```

## Commands

### Fuzzy symbol search
```bash
"$DM" --db "$DB" search "SymbolName"
"$DM" --db "$DB" search "SymbolName" --limit 10
```

### List symbols in a file
```bash
"$DM" --db "$DB" symbols "path/to/file.rs"
```

### Who calls this symbol
```bash
"$DM" --db "$DB" callers "function_name"
```

### File dependencies (what it imports)
```bash
"$DM" --db "$DB" deps "path/to/file.rs"
```

### Impact analysis (who depends on this file)
```bash
"$DM" --db "$DB" impact "path/to/file.rs"
```

### Most connected files
```bash
"$DM" --db "$DB" hotspots --limit 20
```

### Crate dependency graph
```bash
"$DM" --db "$DB" crates
```

### Which crate owns a symbol
```bash
"$DM" --db "$DB" resolve "SymbolName"
```

### JSON output
Add `--json` flag to any command:
```bash
"$DM" --db "$DB" --json search "SymbolName"
```

## Strategy

1. Start with `search` to find the symbol
2. Use `callers` to understand usage patterns
3. Use `impact` to assess blast radius of changes
4. Use `hotspots` to find the most important files
5. Use `resolve` to map symbols to crates
