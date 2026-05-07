# dig2memory Debugging Notes

## TS File-Edge Path Normalization Bug

### What was found

When indexing the TypeScript-heavy workspace:

- workspace root: `.\sandbox-ts`
- workspace id: `sample-ts`
- DB: `dig2memory\data-sandbox-ts\index.db`

the AST/symbol index works, but graph queries are partially broken because
TypeScript file edges are stored with non-normalized paths like:

- `src/./Tool.ts`

instead of:

- `src/Tool.ts`

### Observed symptoms

These queries were run with:

- binary:
  `dig2memory\target-ts\release\dig2memory.exe`
- DB:
  `dig2memory\data-sandbox-ts\index.db`
- workspace:
  `sample-ts`

Working query:

```powershell
dig2memory.exe --db ... --workspace sample-ts --json deps "src/tools.ts"
```

It returns edges like:

- `src/./Tool.ts`
- `src/./tools/TaskCreateTool/TaskCreateTool.ts`

Broken query:

```powershell
dig2memory.exe --db ... --workspace sample-ts --json impact "src/Tool.ts"
```

It returns:

```json
[]
```

But this works:

```powershell
dig2memory.exe --db ... --workspace sample-ts --json impact "src/./Tool.ts"
```

It returns reverse deps such as:

- `src/QueryEngine.ts`
- `src/main.tsx`
- `src/query.ts`
- `src/tools.ts`

### Impact

Because of path mismatch:

- `impact` is unreliable for TS/TSX workspaces
- `hotspots` can be empty or misleading
- graph analysis on TS imports is weaker than symbol extraction

### Likely cause

The TypeScript file-edge extractor appears to preserve relative import forms
with `./` segments instead of normalizing to canonical workspace-relative
paths.

Candidate area:

- `src/ast/languages/typescript.rs`

Especially around import resolution and emitted `file_edges`.

### Why this matters

For TS codebases like `sample-ts`, `dig2memory` is still useful for:

- symbols
- symbol search
- file-local AST inventory
- some direct deps

But graph features are not yet trustworthy until path normalization is fixed.

### Reproduction context

Sandbox source shape:

- `1902` files indexed
- `49515` symbols extracted
- dominant file types:
  - `1332` `.ts`
  - `552` `.tsx`
  - `18` `.js`

So this is not an edge case. It materially affects graph usefulness on large
TS workspaces.
