# sutra — design sketch

Status: sketch (supersedes `first-sutra-sketch.md`)
Date: 2026-04-27
Context: `/home/josh/soft/manas/docs/manas-architecture.md` — perception: code subsystem. Bespoke replacement for qartez.

## what it is

Sutra (सूत्र — "thread", "string", "concise rule") is a code intelligence service that gives the agent (and the human) a structural view of the codebases under its workspaces. AST-first, per-project, agent-native.

It is the **perception: code** subsystem of manas. Smriti perceives the filesystem; vedakosha perceives documents; sutra perceives code.

The name carries the design ethos:

- **Thread.** Code is a tangle of logical threads — calls, imports, types — that sutra exposes as graphs and tables, not as text.
- **Aphorism.** Each tool is a concise rule: one job, one return shape, one reason to exist. The opposite of a 39-tool surface area.

## what it is not

- **Not a text search engine.** `grep` and ripgrep already work. Sutra answers questions that require structure: scope, definitions, references, callers, type relationships.
- **Not a language server.** It does not implement LSP, does not run in-editor, does not provide completion or hover. It is read-only code intelligence consumed over MCP, not a development tool.
- **Not a temporal store.** Sutra reflects the *current* state of the workspace. File lifecycle (when did this symbol move?) is smriti's domain. Code history is git's. Sutra joins the two on demand; it does not duplicate them.
- **Not a semantic embedder.** Code search via dense embeddings is unreliable for structural questions ("what calls this function?") where AST is exact. Embeddings on docstrings/identifiers may arrive in v0.3 as an opt-in *secondary* leg behind structural retrieval, never as a replacement for it.
- **Not home-wide.** Unlike smriti, sutra does not roam under `~`. It operates on an explicit list of workspaces — typically the projects under `~/soft/` that the agent is actually working on.

## why rewrite (instead of using qartez)

Qartez is the current incumbent in this slot. Two reasons it has to go:

1. **Licensing.** Qartez is dual-licensed (small-team / commercial). Free for current personal use but not freely composable with the rest of the manas system, which is meant to be self-hosted, inspectable, and ours.
2. **Surface area mismatch.** Qartez ships ~39 tools across multiple tiers, much of it speculative (clones detection, hotspots, refactor planners). The agent uses ~6 of them in practice. The cost of a 39-tool surface is real — it shows up as schema bloat in the tool list and as ambiguity in tool selection.

Sutra is the thinned-down, owned version. The sketch is for *the kit we actually use*, not the kit we might want.

---

## first consumer: agent code navigation in manas

The immediate motivation is the same workflow qartez supports today:

1. **Map.** Agent boots, asks `sutra_map` for a workspace skeleton. Gets a token-efficient view ranked by importance.
2. **Find.** Agent looks up a symbol by name (`sutra_find`) without grepping for it.
3. **Read with context.** Agent reads symbol source (`sutra_read`) — gets the function plus its file/line anchors, not the whole file.
4. **Impact before edit.** Before modifying a load-bearing function, `sutra_impact` reports who calls it. CLAUDE.md already prescribes this — sutra owns the contract.
5. **References.** `sutra_refs` returns true references (scope-aware), not string matches.

Everything else (call hierarchy, dependency graph, complexity) follows from the same parsed AST and is layered on top.

---

## core concepts

### workspace, not root

A **workspace** is a directory tree containing one project — typically the root of a Rust crate, a Cargo workspace, a Dart package, a TypeScript repo, etc. Sutra is given an explicit list of workspaces; it does not auto-discover.

Two reasons:

- **Scope hygiene.** Cross-project symbol search is mostly noise (every project has a `Config`, every project has a `main`). Per-workspace queries are sharper.
- **Index size.** A combined index of every project under `~/soft/` would be huge and slow to rebuild on changes. Per-workspace indexes rebuild independently.

A small **workspace registry** (`~/.sutra/workspaces.toml`) maps workspace ids to root paths and language manifests. The agent passes a `workspace` parameter on every tool call. There is no implicit "current" workspace — the agent manages that state explicitly (per principle 4 of manas).

```toml
# ~/.sutra/workspaces.toml
[[workspace]]
id = "manas"
root = "/home/josh/soft/manas"
languages = ["rust", "markdown"]

[[workspace]]
id = "smriti"
root = "/home/josh/soft/smriti"
languages = ["rust"]

[[workspace]]
id = "arrow"
root = "/home/josh/nhs/soft/arrow"
languages = ["dart"]
```

### AST-first via tree-sitter

Parsing is via [tree-sitter](https://tree-sitter.github.io/) grammars. v0.1 ships with:

- **Rust** (full)
- **Dart** (full)

v0.1.5 / v0.2 candidates: Python, TypeScript/JavaScript, Markdown (for outline-style queries on docs).

Tree-sitter was chosen over LSP/compiler integration for three reasons:

1. **One toolchain across N languages.** LSP requires shipping a server per language; sutra is one binary.
2. **Crash-resilient.** Tree-sitter parses are tolerant of syntax errors — partial files still produce a usable tree. Important when the agent queries mid-edit.
3. **Fast and embeddable.** Parsing 100k lines of Rust takes seconds, not minutes. The grammar is a runtime dependency, not a separate process.

The cost: tree-sitter does **not** do type resolution. Sutra resolves "go to definition" via name+scope heuristics, not via a real type system. This is good enough for ~95% of queries and honest about its limits — if you ask sutra to disambiguate two methods named `send` on different traits, it returns both candidates and flags the ambiguity rather than guessing.

### symbol identity

A symbol's identity is the tuple `(workspace_id, file_path, qualified_name, kind, signature_hash)`.

- `workspace_id`: scopes the index.
- `file_path`: relative to workspace root.
- `qualified_name`: `module::Type::method` or language-equivalent.
- `kind`: function, method, struct, enum, trait, class, etc.
- `signature_hash`: blake3 hash of the canonicalized signature (params + return type, normalized whitespace). Lets sutra detect "same symbol, signature changed."

Identity is not content-addressed in smriti's sense — code symbols do not have a stable hash that survives renaming. But the `signature_hash` lets us answer "did this function's contract change?" without comparing whole bodies.

When smriti emits a `moved` event for a source file, sutra updates the `file_path` for all symbols defined in it without reparsing. When smriti emits `updated`, sutra reparses the file and diffs the symbol set.

### scope-aware references

The hard work is producing references that respect scope, not just name matches. For each occurrence of an identifier, sutra resolves it to the nearest enclosing definition: local binding, function parameter, module-level item, imported item, or `unresolved` (when the resolver can't tell).

The `unresolved` bucket is honest signal — sutra exposes it in the response so the agent knows confidence is partial. Better than silently inflating reference counts.

### relation to git history

Sutra is stateless w.r.t. its own history but knows how to read git. Two analysis tools (`sutra_cochange`, `sutra_blame`) shell out to `git` for queries that need history. They do not maintain their own DB of git events.

This is a deliberate split:

- **Smriti** owns the lifecycle of files (created, moved, updated, deleted) across all of `~`, including non-git directories.
- **Git** owns the lifecycle of code commits inside a repo.
- **Sutra** answers structural questions about the *current* state and joins to git on demand.

No subsystem stores another's history.

---

## storage

SQLite per workspace, at `~/.sutra/<workspace_id>/index.db`. Same single-inspectable-file story as chitta and smriti. No `sqlite-vec` in v0.1 — sutra's queries are structural, not vector. FTS5 is enabled for symbol-name fuzzy matching.

A workspace index is **fully rebuildable** from the source tree at any time. There is no precious state. If the schema changes, drop and rebuild.

### tables (sketch)

```sql
-- the source files sutra is aware of within this workspace
CREATE TABLE files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,        -- relative to workspace root
    language TEXT NOT NULL,
    content_hash TEXT NOT NULL,       -- mirrors smriti's hash when known; else local blake3
    line_count INTEGER NOT NULL,
    parsed_ok BOOLEAN NOT NULL,       -- false if tree-sitter hit unrecoverable errors
    last_parsed TIMESTAMP NOT NULL,
    -- denormalized load-bearing rollup (see "modification guard" section)
    fan_in_files INTEGER NOT NULL DEFAULT 0,    -- count of distinct files referencing any symbol in this file
    blast_radius INTEGER NOT NULL DEFAULT 0,    -- transitive upstream count, depth-capped (see sutra_impact)
    pagerank REAL                              -- file-level PageRank, NULL until v0.2
);
CREATE INDEX idx_files_fan_in ON files(fan_in_files);
CREATE INDEX idx_files_blast ON files(blast_radius);

-- one row per defined symbol
CREATE TABLE symbols (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    qualified_name TEXT NOT NULL,
    short_name TEXT NOT NULL,         -- last segment, for fuzzy lookup
    kind TEXT NOT NULL,               -- function, method, struct, enum, trait, class, ...
    signature TEXT,                   -- canonicalized signature text
    signature_hash TEXT,
    visibility TEXT,                  -- pub, pub(crate), private, ...
    start_line INTEGER NOT NULL,
    start_col INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    end_col INTEGER NOT NULL,
    parent_symbol_id INTEGER REFERENCES symbols(id),  -- for methods, nested types
    docstring TEXT,
    pagerank REAL                     -- symbol-level PageRank, NULL until v0.2
);
CREATE INDEX idx_symbols_short_name ON symbols(short_name);
CREATE INDEX idx_symbols_qualified_name ON symbols(qualified_name);
CREATE INDEX idx_symbols_file ON symbols(file_id);

-- BM25 over symbol short_name + qualified_name + docstring
CREATE VIRTUAL TABLE symbols_fts USING fts5(
    symbol_id UNINDEXED,
    short_name,
    qualified_name,
    docstring
);

-- references: every occurrence resolved (or marked unresolved)
CREATE TABLE refs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    target_symbol_id INTEGER REFERENCES symbols(id) ON DELETE CASCADE,  -- NULL = unresolved
    unresolved_name TEXT,             -- populated when target_symbol_id IS NULL
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    context_kind TEXT NOT NULL        -- call, type_use, import, ...
);
CREATE INDEX idx_refs_target ON refs(target_symbol_id);
CREATE INDEX idx_refs_file ON refs(file_id);
CREATE INDEX idx_refs_unresolved ON refs(unresolved_name) WHERE target_symbol_id IS NULL;

-- import / module-level dependency edges
CREATE TABLE imports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    imported_path TEXT NOT NULL,      -- raw import string
    resolved_file_id INTEGER REFERENCES files(id),  -- NULL if external/unresolved
    line INTEGER NOT NULL
);

-- snapshot of last full parse for the workspace
CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TIMESTAMP NOT NULL,
    files_parsed INTEGER NOT NULL,
    symbols_extracted INTEGER NOT NULL,
    refs_extracted INTEGER NOT NULL,
    parse_errors INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL
);
```

### why no vector index in v0.1

The sketch's strongest claim is *structure over text*. A vector table on day one weakens that — it makes the easy thing (semantic search over identifiers) the default, and the actual structural queries become a configuration choice.

Add it later if real queries demand it. Keep the core promise sharp.

---

## MCP tools

Twelve tools, two tiers. The cut is deliberate: each one earns its slot or doesn't ship.

All responses carry a freshness envelope (`as_of`, `is_stale`) for consistency with smriti.

### core (always loaded)

#### sutra_map

Workspace skeleton. The "where do I start" tool. Files ranked by a heuristic combining symbol count, fan-in (refs into) and fan-out (refs out), so important files surface first.

```
Input:  { workspace: string, path_prefix?: string, max_entries?: int }
Output: {
    workspace: string,
    root: string,
    entries: [{
        path: string,
        language: string,
        symbol_count: int,
        fan_in: int,
        fan_out: int,
        importance_score: float
    }],
    total_files: int,
    truncated: bool,
    as_of: timestamp,
    is_stale: bool
}
```

#### sutra_find

Look up symbols by name. Combines exact, qualified, and fuzzy (FTS5) match.

```
Input:  { workspace: string, name: string, kind?: string, k?: int }
Output: {
    matches: [{
        symbol_id: int,
        qualified_name: string,
        kind: string,
        file: string,
        line: int,
        signature: string,
        match_type: "exact" | "qualified" | "fuzzy"
    }],
    truncated: bool,
    as_of: timestamp,
    is_stale: bool
}
```

#### sutra_grep

Indexed text search restricted to symbol names, signatures, and docstrings. *Not* a general-purpose grep — that's what ripgrep is for. This is "search the index."

```
Input:  { workspace: string, query: string, in?: ["names" | "signatures" | "docstrings"], k?: int }
Output: { matches: [...], as_of, is_stale }
```

#### sutra_read

Read source for a symbol with anchors. Reads the file, returns the symbol's slice plus surrounding context (configurable).

```
Input:  { workspace: string, symbol_id?: int, qualified_name?: string, context_lines?: int }
Output: {
    qualified_name: string,
    file: string,
    start_line: int,
    end_line: int,
    source: string,                 -- with line numbers
    docstring?: string,
    as_of: timestamp,
    is_stale: bool
}
```

#### sutra_outline

Symbol table for a single file. The file's table of contents.

```
Input:  { workspace: string, path: string }
Output: {
    path: string,
    language: string,
    symbols: [{ qualified_name, kind, line, signature?, parent? }],
    parsed_ok: bool,
    as_of, is_stale
}
```

#### sutra_impact

Blast radius before editing. The single most important guard tool — CLAUDE.md instructs the agent to call this before modifying load-bearing files.

```
Input:  { workspace: string, symbol_id?: int, qualified_name?: string }
Output: {
    target: { qualified_name, file, kind },
    direct_callers: int,
    transitive_callers: int,
    files_touched: int,
    risk: "low" | "medium" | "high",
    risk_factors: [string],         -- e.g. "called from 18 files", "no tests cover any caller"
    sample_callers: [{ qualified_name, file, line }],   -- up to 10
    as_of, is_stale
}
```

The `risk` field is a coarse rollup of the underlying numbers, not a model judgment. Honest input to the agent's decision.

#### sutra_deps

File-level import graph for the workspace.

```
Input:  { workspace: string, path?: string, depth?: int }
Output: {
    nodes: [{ path, language }],
    edges: [{ from: string, to: string, kind: "import" | "module" | "external" }],
    truncated: bool,
    as_of, is_stale
}
```

#### sutra_health

Health check. Mirror of `chitta_health_check` and `smriti_health`.

```
Input:  {}
Output: {
    status: "ok" | "degraded",
    workspaces: [{
        id: string,
        root: string,
        last_parse: timestamp,
        parse_in_progress: bool,
        files_indexed: int,
        symbols: int,
        parse_errors: int
    }],
    version: string
}
```

### analysis (loaded on demand)

These are heavier and noisier; the agent enables them via a `sutra_tools enable: ["analysis"]` call (qartez convention, retained because it works).

#### sutra_refs

All usages of a symbol across the workspace.

```
Input:  { workspace: string, symbol_id?: int, qualified_name?: string, kind?: string }
Output: {
    target: { qualified_name, file, kind },
    refs: [{ file, line, col, context_kind, snippet }],
    unresolved_candidates: int,     -- how many name-matches couldn't be scope-resolved
    as_of, is_stale
}
```

#### sutra_calls

Call hierarchy: callers and callees of a function/method.

```
Input:  { workspace: string, qualified_name: string, direction: "callers" | "callees", depth?: int }
Output: {
    root: string,
    direction: string,
    edges: [{ caller, callee, file, line }],
    truncated: bool,
    as_of, is_stale
}
```

#### sutra_diff_impact

Blast radius of a git diff (HEAD vs working tree, or two refs). Shells out to git for the diff, parses changed files, joins to the symbol table.

```
Input:  { workspace: string, base?: string, head?: string }
Output: {
    changed_files: int,
    modified_symbols: [{ qualified_name, file, change: "added" | "removed" | "signature_changed" | "body_changed" }],
    affected_callers: int,
    risk: "low" | "medium" | "high",
    as_of, is_stale
}
```

#### sutra_cochange

Files that historically change together (git log over a window). Cheap, useful for "if I edit X, what should I look at?"

```
Input:  { workspace: string, path: string, window_days?: int, k?: int }
Output: {
    target: string,
    cochanged: [{ path, cochange_count, last_together: timestamp }],
    as_of, is_stale
}
```

That's the kit. Twelve tools. Anything else is YAGNI for v0.1.

---

## configuration

| Var | Default | Purpose |
|---|---|---|
| `SUTRA_DB_DIR` | `~/.sutra/` | Per-workspace SQLite indexes go here. |
| `SUTRA_WORKSPACES` | `~/.sutra/workspaces.toml` | Workspace registry. |
| `SUTRA_LISTEN_ADDR` | `unix:~/.sutra/sock` | Daemon listen address. |
| `SUTRA_PARSE_PARALLELISM` | num_cpus | Parser thread count. |
| `SUTRA_STALE_THRESHOLD_SEC` | `600` | `is_stale` threshold (10 min — code changes faster than docs). |

### transport — daemon

Same model as smriti:

1. **Long-lived daemon.** Full reparses can take seconds-to-minutes for big workspaces. The agent cannot block on them.
2. **Single writer, many readers.** Multiple sessions reading the index concurrently. Daemon holds the writer; SQLite WAL handles the rest.

Daemon exposes MCP over Unix socket, fronted by mcpjungle. CLI surface (`sutra parse`, `sutra workspaces add`, `sutra health`) talks to the same daemon.

### subscription to smriti

Sutra subscribes to smriti's file-event stream (whatever shape that takes — the smriti sketch flags this as an open question). On `created`/`updated`/`deleted`/`moved` for files within a registered workspace whose extension matches a registered language, sutra:

- **created / updated:** reparse the file, diff symbols.
- **moved:** update `files.path` and re-resolve imports that referenced the old path.
- **deleted:** cascade-delete symbols and refs (FK ON DELETE CASCADE).

Until smriti exposes the subscription API, sutra falls back to **on-demand reparse** (`sutra_parse`) plus periodic full scans (configurable, default off). This is the same fallback qartez uses today.

---

## modification guard

A two-stage feature with a v0.1 form (advisory tool) and a v0.1.5 form (enforcing hook). Inspired by qartez's `guard.rs` / `bin/qartez-guard.rs` — adapted, not copied.

The thesis: **the agent skips `sutra_impact` precisely when it would have helped.** CLAUDE.md instructs the agent to check impact before editing load-bearing files, but under context pressure the instruction gets dropped. The bug we want to catch — confident edit to a hub function without any check — happens when the agent has stopped reading guidance carefully. An advisory tool the agent must remember to call cannot solve a problem of forgetting.

The guard's job is to make the check non-skippable on the small set of files where the cost of a bad edit is highest.

### v0.1 — advisory only

`sutra_impact` (already in the core tier) is the only enforcement mechanism. CLAUDE.md prescribes calling it before edits to high-fan-in files. No hooks, no acks. We ship v0.1 with the advisory form so we can collect real `fan_in_files` and `blast_radius` distributions across actual workspaces before tuning thresholds. Calibration without data is theatre.

### v0.1.5 — enforcing hook

A separate `sutra-guard` binary registered in `~/.claude/settings.json` as a `PreToolUse` hook for `Edit`, `Write`, `MultiEdit`. The contract:

```
PreToolUse(tool_name, tool_input)
  ↓
sutra-guard:
  1. extract file_path from tool_input
  2. resolve file_path → workspace_id via the registry
     (no match → Allow; the file is outside any sutra-tracked workspace)
  3. open the workspace's index, look up files row by relative path
     (no row → Allow; sutra hasn't seen this file)
  4. compute hot = (fan_in_files >= FAN_IN_MIN)
                OR (blast_radius >= BLAST_MIN)
                OR (pagerank >= PR_MIN)              -- v0.2 only
     (not hot → Allow)
  5. check ack: ~/.sutra/acks/<workspace>/<blake3(rel_path)> exists
     and mtime within ACK_TTL_SECS
     (acked → Allow)
  6. else → Deny with structured remediation message
```

Latency budget: <50ms. The hot-path is one indexed PK lookup on a small SQLite file plus one stat() on the ack file. Both cheap.

#### the deny message

The qartez insight worth stealing verbatim: the deny message is not a veto, it's an instruction. The agent must be able to act on it without escalating to the user.

```
Sutra guard: edit blocked on src/scanner.rs (workspace=manas).

This file is load-bearing:
  - referenced by 23 other files (threshold: 10)
  - blast radius: 47 transitive dependents (threshold: 20)

Top inbound symbols:
  - manas::scanner::Scanner          (12 callers)
  - manas::scanner::ScanError         (8 callers)
  - manas::scanner::scan_workspace    (6 callers)

Before editing, call:
  sutra_impact workspace=manas qualified_name=manas::scanner::Scanner

This will record an acknowledgment valid for 10 minutes.
To disable the guard for this session: SUTRA_GUARD_DISABLE=1.
```

The agent reads this and routes to `sutra_impact`. `sutra_impact` does its real job and writes the ack as a side effect (a single `touch` on `~/.sutra/acks/<workspace>/<hash>`). The retry succeeds.

#### thresholds

The most important and most easily-wrong design choice. Defaults:

| Setting | Default | Rationale |
|---|---|---|
| `SUTRA_GUARD_FAN_IN_MIN` | 10 | Files referenced by ≥10 other files. Calibrate to fire on top ~1-3% of files in a typical workspace. |
| `SUTRA_GUARD_BLAST_MIN` | 20 | Transitive dependents at depth 3. Cheaper-to-compute proxy for "central." |
| `SUTRA_GUARD_PR_MIN` | 0.05 | v0.2 only. Top ~5% by qartez's calibration; we may go tighter. |
| `SUTRA_GUARD_ACK_TTL_SECS` | 600 | 10 minutes. Long enough for a focused edit; short enough that stale acks don't accumulate. |
| `SUTRA_GUARD_DISABLE` | unset | Single env var to bypass entirely. Set in CI, set when triaging false positives. |

The targeting principle: **fire on the top ~1-5% of files**, not more. False positives are not just annoying — they teach the agent to disable the guard, which makes it worse than nothing. Better to miss a few load-bearing edits than to fire on routine ones.

Calibration is an explicit v0.1 → v0.1.5 deliverable. Run sutra against ~10 real workspaces (manas, smriti, chitta-rs, arrow, sangha, etc.); plot the distribution of `fan_in_files` and `blast_radius`; pick thresholds at the 95th–99th percentile. Document the percentile per release so threshold drift is visible.

#### DoS hardening

Stolen from the qartez impl, retained because it's right:

- Invalid env var → log warning, use default. Never fatal.
- Missing or unreadable index → Allow (best-effort; we'd rather miss a guard fire than block all edits if sutra is broken).
- Ack write failure → log, do not error. The next `sutra_impact` will retry.
- Unknown fields in the PreToolUse JSON payload → ignored via `serde(default)`. Future Claude Code changes don't break the contract.
- The guard never *modifies* the index. It's a pure reader.

#### why not v0.1

Three reasons not to ship the guard with v0.1:

1. **No threshold data.** Defaults pulled from qartez may not transfer to sutra's signal mix (sutra has fan_in + blast, no PR until v0.2). Need real distributions first.
2. **Hook installation is a global Claude Code config change.** Wants to be opt-in and easy to disable. The opt-in path needs a small `sutra guard install`/`uninstall` CLI subcommand, plus docs.
3. **Bad first impressions are costly.** If the guard fires on routine edits in week one, agents and users will treat the whole tool as broken. Better to ship v0.1 with the advisory form, learn what "load-bearing" actually looks like in our workspaces, and ship the guard after calibration.

What v0.1 ships toward the guard: the schema fields (`fan_in_files`, `blast_radius`, `pagerank`-nullable), the computation in the parser, and `sutra_impact` returning the same numbers the guard will key on. Adding the binary in v0.1.5 is mostly hook plumbing.

---

## degradation

Smriti has the privacy-gate pattern; sutra inherits the spirit:

- **No workspaces registered:** all tools return empty results with `is_stale=true` and a hint to register a workspace.
- **Smriti unavailable:** sutra still works in standalone mode, scanning workspaces directly. Loses some efficiency (no event-driven incremental updates) but functional.
- **Tree-sitter grammar missing for a file's language:** the file is recorded with `parsed_ok=false`, `language="unknown"`, and excluded from symbol/ref queries. `sutra_health` reports unparseable file counts.
- **Parse error in a file:** the file is parsed best-effort (tree-sitter is error-tolerant); `parsed_ok` is true if any symbols were extractable, false otherwise.

---

## project structure

```
sutra/
├── Cargo.toml
├── src/
│   ├── main.rs              -- CLI: daemon | parse | workspaces | health
│   ├── daemon.rs            -- long-lived process, socket listener, parse scheduler
│   ├── mcp.rs               -- MCP server + tool handlers
│   ├── workspace.rs         -- workspace registry, root resolution
│   ├── parser/
│   │   ├── mod.rs           -- dispatch by language
│   │   ├── rust.rs          -- tree-sitter-rust + symbol/ref extraction
│   │   └── dart.rs          -- tree-sitter-dart + symbol/ref extraction
│   ├── resolver.rs          -- name+scope resolution; produces refs.target_symbol_id
│   ├── db.rs                -- SQLite operations
│   ├── smriti_client.rs     -- subscribe to smriti file events (or fallback poller)
│   ├── git.rs               -- shell-out for cochange and diff_impact
│   └── config.rs            -- env vars + workspaces.toml
├── migrations/
│   └── 0001_initial.sql
└── tests/
    ├── parse_rust_test.rs
    ├── parse_dart_test.rs
    ├── resolver_test.rs
    ├── impact_test.rs
    └── workspace_lifecycle_test.rs
```

~10 source files. The parsers and resolver are the bulk of the new code; everything else (daemon, MCP, db) follows the chitta/smriti pattern.

---

## what this enables

### immediate (v0.1)

1. **Replace qartez in CLAUDE.md.** Same workflow, owned binary, no licensing tail.
2. **Smaller tool surface.** 12 tools instead of 39 — less ambiguity in tool selection, less schema overhead.
3. **Smriti-driven incremental updates.** Edit a file → sutra reparses just that file → next query reflects it. No manual `qartez_maintenance` step.
4. **Honest unresolved signal.** `sutra_refs` reports unresolved-candidate counts so the agent knows when partial-resolution affects results.
5. **Per-workspace queries.** Searching "Scanner" in `manas` returns manas results, not 14 unrelated `Scanner` symbols across `~/soft/`.

### v0.1.5

- **Modification guard.** The PreToolUse hook described in the "modification guard" section. Ships once threshold calibration data is in.
- **`sutra guard install` / `sutra guard uninstall` CLI.** Small commands that edit `~/.claude/settings.json` to register / deregister the hook, with a confirmation prompt.

### v0.2

- **PageRank.** Both file-level (nodes = files, edges = imports) and symbol-level (nodes = symbols, edges = call/reference edges). Standard iterative algorithm, ~70 lines, run on full reparse and on a configurable trigger (insert-count threshold, or daily). Result written to `files.pagerank` and `symbols.pagerank` (already nullable in the v0.1 schema).
  - File-level PR upgrades the guard's hotness signal from `(fan_in OR blast)` to `(fan_in OR blast OR pagerank)`. PR catches indirect centrality (a file imported by few but pulled in transitively by many) that fan-in alone misses.
  - Symbol-level PR informs `sutra_outline` (rank symbols by importance), `sutra_impact` (refine risk score), and a future `sutra_hotspots` analysis tool.
  - Honest cost: PR is opaque to humans. Surface as percentile or tier label (`hub`/`connected`/`leaf`) in tool output, not raw floats.
- **Python and TypeScript grammars.** Tree-sitter grammars exist; the parser-per-language pattern is set up to extend.
- **`sutra_blame`.** `git blame` over a symbol range, summarized — "this function was last touched 3 weeks ago by commit abc123."
- **`sutra_hotspots`.** Composite score `pagerank * cyclomatic_complexity * (1 + recent_churn)` to rank refactor candidates. Diagnostic flip side of the guard's preventive role: guard says "don't touch this lightly," hotspots says "you should be touching these."
- **Smriti history join.** `sutra_history` answering "where did this symbol live before today?" by joining smriti's `paths` lifecycle with sutra's symbol-to-file mapping.

### v0.3 (maybe)

- **Optional embedding leg.** BGE-M3 or similar over docstrings + qualified names, exposed as a separate `sutra_semantic` tool, never mixed into the structural primitives. Disabled by default; turned on when concrete queries justify it.
- **Cross-workspace federated search.** Query "all my Rust workspaces for symbol X" by fanning out across registered workspace indexes. Useful or noisy — wait until the use case materializes.

---

## resolved

- **Name:** sutra (सूत्र — "thread, concise rule"). Naming family: chitta (memory), smriti (filesystem), sutra (code), vedakosha (documents).
- **Role:** the perception:code subsystem of manas. Direct replacement for qartez in that slot.
- **Scope:** per-workspace via explicit registry. Not home-wide like smriti.
- **Parsing:** tree-sitter only in v0.1. No LSP, no compiler integration.
- **Languages v0.1:** Rust + Dart.
- **Storage:** SQLite per workspace, FTS5 for symbol-name fuzzy match. **No vector index in v0.1.**
- **Stateless current-state index.** No temporal history; smriti owns file lifecycle, git owns commit lifecycle.
- **Symbol identity:** `(workspace_id, file_path, qualified_name, kind, signature_hash)`.
- **Daemon transport** (Unix socket, mcpjungle-fronted). Mirrors smriti.
- **12 MCP tools, 2 tiers** (8 core + 4 analysis). Concise rule, not feature-stuffed.
- **Subscribe to smriti** for incremental updates; fall back to on-demand parse if smriti is unavailable.
- **Honest resolution.** `unresolved_name` exposed to the agent rather than silently inflating ref counts.
- **Modification guard deferred to v0.1.5.** v0.1 ships the schema fields (`fan_in_files`, `blast_radius`, `pagerank`-nullable) and the advisory `sutra_impact` tool. The PreToolUse hook lands once we have real workspace data to calibrate thresholds at the 95th-99th percentile.
- **PageRank deferred to v0.2.** Both file-level and symbol-level. Schema columns are in v0.1 (nullable) so the addition is non-breaking.

## open questions

- **Workspace auto-discovery vs registry.** A registry is explicit and clean but means the agent has to register a new project before sutra knows it exists. Worth a `sutra workspaces auto` command that scans `~/soft/` for project markers (`Cargo.toml`, `pubspec.yaml`)? Probably yes, but as opt-in convenience, not default behavior.
- **Cargo workspace handling.** A Cargo workspace contains multiple crates. Treat each crate as its own sutra workspace, or treat the Cargo workspace root as one sutra workspace? Leaning: one sutra workspace per Cargo workspace (so cross-crate refs resolve), but the question deserves a concrete test.
- **Dart / Flutter monorepos.** Same question as Cargo. Arrow + arjuna may be the test case.
- **External symbol resolution.** Should `imports.resolved_file_id` ever point outside the workspace (into stdlib, into a dependency)? v0.1 says no — externals are unresolved. v0.2 may want a "vendor index" of stdlib symbols for resolution accuracy.
- **Reparse trigger when smriti is down.** v0.1 fallback is on-demand. Should the daemon also run a periodic mtime-based scan as a safety net? Probably yes, off by default, configurable.
- **Macros and codegen.** Rust macros and Dart codegen (build_runner, freezed) produce symbols that aren't in the source tree. Tree-sitter sees the macro invocation, not the expansion. Sutra acknowledges this gap; closing it would require a real compiler integration (out of scope for v0.1, possibly forever — this is the cost of being tree-sitter-only).
- **Tool surface drift.** The 12-tool cut is opinionated. Some qartez tools (`qartez_unused`, `qartez_smells`, `qartez_clones`) are genuinely useful in some workflows and absent here. Re-evaluate after 1–2 months of real use.
- **Guard escalation path.** What does the agent do when `sutra_impact` returns `risk: high` and the user is mid-refactor? Currently the guard just unblocks the edit after the impact call — no confirmation step. Maybe v0.2 adds an "are you sure" tier where `risk: high` requires a second ack. Risks UX fatigue; defer until v0.1.5 surfaces real cases.
- **Per-workspace threshold overrides.** Some workspaces (small libraries) will have everything below default thresholds; some (large monoliths) will fire constantly at defaults. Probably needs `[guard]` section in `workspaces.toml` with per-workspace overrides. Wait until v0.1.5 deploys before designing this.
- **MCP server instructions.** What does sutra tell the agent in its handshake? Likely: "use sutra in preference to grep/Glob for code questions; call `sutra_impact` before editing load-bearing files."
- **Dogfood timeline.** Smriti needs to land first (sutra subscribes to it). Before smriti exists, sutra v0.1 ships in standalone mode. Decide: ship sutra standalone first as a qartez replacement, or wait for smriti and ship them together?
