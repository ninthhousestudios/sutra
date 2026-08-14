# sutra

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

Code intelligence for [manas](https://github.com/ninthhousestudios/manas) — a living architectural model of your codebase, served as an MCP server.

Sutra parses your code with tree-sitter, discovers implicit patterns with formal concept analysis (queryable via `sutra_conventions`), enforces constraints with differential dataflow, detects structural similarity with holographic reduced representations, tracks codebase health with empirically calibrated biomarkers, and accumulates code-anchored lessons from agent experience. It exposes all of this through 31 MCP tools that AI coding agents (and humans) can call.

The core loop: **explore** (find relevant code in one call, with lessons and conventions surfaced contextually as an agent reads) → **check** (flag architectural violations as code is written) → **review** (produce an architectural change report the human can assess without reading every line) → **teach** (human refines the model by updating constraints and boundaries).

## Install

```bash
cargo install --path .
```

## Quick start

```bash
# Start the MCP server (stdio, one per client)
sutra serve --stdio
```

Workspaces are registered automatically when an agent calls `sutra_workspace` with a path, or explicitly:

```bash
sutra workspaces add myproject /path/to/project rust
sutra parse myproject
```

## What sutra does

### 1. Structural index (Layer 0)

Tree-sitter parses your codebase into a SQLite index of files, symbols (functions, types, traits, modules), and relationships (calls, imports, contains, implements). This is the ground truth that everything else builds on.

Every response includes a **freshness envelope** (`as_of`, `is_stale`) so callers always know how current the data is.

### 2. Architectural components (Layer 1)

Sutra discovers components — groups of related code — via directory-structure clustering and human refinement. Components have stable identity, lifecycle state (`stable` or `sketch`), and human-assigned aliases. They're the unit of convention scoping and health scoring.

### 3. Convention detection (Layer 2)

Formal Concept Analysis (FCA) discovers implicit patterns in your code: "public functions return Result," "handlers take &self as first parameter," "error types implement Display." An identity→obligation filter separates "what things are" (structural facts) from "what they should do" (behavioral patterns worth surfacing), and toolchain-enforced patterns (e.g. `async` implying a returned future) are automatically excluded since the compiler already guarantees them.

Detection runs in the parse pipeline and persists to the index. `sutra_conventions(action="list")` returns the discovered conventions — each with its antecedent→consequent implication, support, confidence, and component scope. Conventions are surfaced as a queryable list; they are not enforced in review. (Two earlier in-loop consumers — an orientation summary and a review-time deviation report — were retired after live use showed a high false-positive rate.)

### 4. Constraint enforcement (Layer 3)

Constraints are explicit architectural rules authored in `.sutra/rules.toml`:

```toml
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
name = "tools-must-not-import-daemon"

[[constraint]]
kind = "boundary"
from_component = "db"
to_component = "http"

[[constraint]]
kind = "no_cycles"
scope = "src/core/"   # directory prefix or glob ("src/**")

[[constraint]]
kind = "max_fan_in"
target = "src/config.rs"
threshold = 10
severity = "advisory"

# External-crate constraints: forbid crates outside the workspace.
# Checked from two signals: use-statement/import paths (file:level findings)
# and Cargo.toml [dependencies] (the linking truth — catches deps that are
# linked but never imported). dev/build-dependencies are exempt unless
# include_dev = true. Crate-name globs allowed; hyphens/underscores equivalent.
[[constraint]]
kind = "forbidden_external"
from = "report/**"               # path glob, default "**" (whole workspace)
crates = ["axum", "sqlx"]
name = "report-stays-pure"

# Confinement: these crates may ONLY be imported from the listed paths.
# allowed_in = [] bans them everywhere. The manifest signal skips the package
# that owns an allowed_in path — declaring a dependency for your own confined
# module is not a violation; a sibling crate declaring it still is.
[[constraint]]
kind = "confined_external"
crates = ["tonic", "prost"]
allowed_in = ["quiver-client/**"]
name = "protos-only-in-quiver-client"

# AST-pattern constraints: forbid structural patterns via tree-sitter queries.
# Checked per-file (no DD engine). Default severity: advisory (heuristic).
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(call_expression function: (field_expression field: (field_identifier) @m (#eq? @m "clone"))) @match'
name = "no-clone-driven-dev"
severity = "blocking"
scope = "src/"
provenance = "CLAUDE.md coding_discipline"
ratchet = true                       # monotonic: severity can never be lowered without release
```

Constraints are checked via a differential dataflow engine — a timely dataflow worker maintains views over the import graph, so cycle detection, forbidden dependency violations, and blast radius queries update incrementally as code changes. External-crate constraints are checked directly against unresolved import rows and workspace Cargo manifests (no DD view needed).

In a multi-crate Cargo workspace, sibling-crate imports (`use server::…` from `report/`) are classified as external, so `forbidden_external` / `confined_external` also express crate-to-crate seams. Dart `package:` and `dart:` imports are matched by package name; pubspec.yaml manifests are not yet checked. JS/TS bare specifiers (`react`, `@angular/core`) are treated as external; `node_modules` is not traversed.

Each constraint has a **severity** (blocking, advisory, informational). The **guard binary** (`sutra-guard`) runs as a Claude Code `PreToolUse` hook and blocks edits that introduce blocking violations in real time. For pattern constraints, the guard uses **introduced-only** semantics: it parses both the proposed and on-disk content, and denies only if the match count increased — pre-existing matches are grandfathered.

Constraints support **waivers** — human-granted exceptions with tracked rationale that appear in every review touching the waived area. Pattern constraint waivers support symbol-level granularity: a waiver on a specific function suppresses matches inside that function only.

Constraints can be **ratcheted** by adding `ratchet = true` — this registers a monotonic severity floor in a durable registry at index time. Once ratcheted, the constraint's severity can never be lowered and it cannot be removed from rules.toml without a human running `sutra ratchet release <id> --rationale "..."` first. Ratchet violations are structurally non-waivable (they bypass the waiver partition). Two enforcement layers: the guard blocks weakening edits to rules.toml in real time, and `check::evaluate` detects drift (deletion or downgrade) at analysis time.

### 5. Health metrics (Layer 4)

Per-file and per-component health scores (1.0–10.0 scale) derived from empirically calibrated biomarkers:

| Biomarker | What it detects | Source |
|---|---|---|
| `co_change_scatter` | Files that change with many unrelated files | git history |
| `change_entropy` | Historically volatile files (Hassan's HCM) | git history |
| `ownership_risk` | Diffuse ownership (no clear owner, many minor contributors) | git history |
| `nested_complexity` | Deeply nested control flow (> 4 levels) | AST |
| `function_hotspot` | High-churn + high-complexity functions | git blame (on-demand) |
| `code_age_volatility` | Old code being frequently touched | git blame (on-demand) |
| `hidden_coupling` | Files that co-change but have no static dependency | git + import graph |
| `convention_drift` | Component diverging from its own conventions | FCA + HRR vectors |
| `hrr_shape_change` | Subtle structural changes hidden in small text diffs | HRR vectors |
| `component_instability` | Martin's instability metric (Ce/(Ca+Ce)) | import graph |

Scores use category-capped deductions so no single dimension can dominate. Component scores are NLOC-weighted averages of member files. Health waivers let you acknowledge known issues without suppressing the signal.

### 6. Vocabulary mapping (Layer 5)

Sutra lets you define human-readable names for code concepts so agents (and humans) can refer to them naturally. Create `.sutra/aliases.toml` in your project root:

```toml
[component]
"being detail cards" = "being_detail"
"auth" = "authentication"

[file]
"config" = "src/config.rs"
"main entry" = "lib/main.dart"

[symbol]
"UP" = "UserProfile"
"parse" = "parse_rules"
```

Three sections map terms to different target kinds:

- **`[component]`** — string value maps a human name to a component name (as shown in `sutra_components`)
- **`[file]`** — maps a human name to a file path
- **`[symbol]`** — maps a human name to a symbol name

#### Hierarchical schema: namespaced symbols + membership groups

For large curated maps (e.g. a name→symbol map over a decompiled binary), two richer shapes are supported alongside the flat form above:

```toml
[symbol]
# Namespaced terms: "<group>/<human_name>" = target
"positions/deg_to_rashi" = "FUN_008d1c50"
"positions/is_own_sign"  = "FUN_008e5270"

[component]
# Array value = a membership GROUP over alias terms
positions = ["positions/deg_to_rashi", "positions/is_own_sign"]
```

- **Namespaced `[symbol]` terms** resolve by **both** the full path (`positions/deg_to_rashi`) and the bare trailing segment (`deg_to_rashi`). When a short name is ambiguous across groups, resolution returns **all** matches.
- **Array-valued `[component]` entries** define a membership group: resolving the group name (`positions`) expands to the union of every member symbol's locations. This is distinct from the string-valued `[component]` form — a **string** is a nickname pointing at a clustering-derived component, an **array** is an explicit group. The value type is the discriminator.

Aliases are synced to the database during workspace indexing. `sutra_explore` resolves them as its first priority tier:

```
Agent: sutra_explore(query="being detail cards")
→ alias match: component "being_detail"
→ file locations for all member files
```

Resolution searches in priority order: exact alias term → short-name match for namespaced terms → component names (substring) → semantic anchor names (substring). Orphan detection warns when an alias points to a dissolved component, missing file, or absent symbol.

This means you can tell an agent "find the being detail cards code" and it resolves to concrete file locations without the agent having to rediscover the mapping each time.

### 7. Structural similarity (Layer 6)

Holographic Reduced Representations (HRR) encode each function's AST into a 1024-dimensional vector. Two modes:

- **strip** — structure only, identifiers removed. Finds copy-paste variants regardless of naming.
- **embed** — structure + identifiers. Finds semantically similar code.

This powers duplicate detection (pattern families of 3+ structurally identical functions) and semantic diff in review (classifying changes as safe-refactor vs. subtle-behavioral-change based on HRR delta vs. text delta).

### 8. Code-anchored lessons (Layer 7)

Lessons are the negative complement to conventions: conventions say "do this," lessons say "don't do that, here's why." They capture experiential knowledge — things learned about code that a future editor needs to know — in a shared SQLite store (`~/.sutra/lessons.db`) that all sutra instances read.

**Writing:** Agents call `sutra_remember` with text and location anchors (the symbol or file they were working on). Sutra enriches the lesson automatically — inferring import-pattern anchors, directory globs, and category tags from the workspace index. Writing is low-ceremony; quality is controlled reactively.

**Surfacing:** Lessons appear contextually through tools agents already call. `sutra_symbol` shows lessons anchored to the symbol being read. `sutra_impact` surfaces warnings about affected symbols. For explicit queries, `sutra_lessons` provides FTS5 text search with structured filters.

**Confidence lifecycle:** Lessons are born unverified with zero confidence. When an agent cites a lesson during a yojana task close-out (`sutra_remember(cite=<id>)`), confidence rises. After crossing a citation threshold, lessons flip to verified. Unverified lessons only surface when no verified lessons cover the same context, and are flagged `[unverified]`. Lessons that go uncited decay and are eventually auto-archived.

**Cross-project scope:** Because lessons attach to technologies and patterns (rust, sqlite, concurrency) rather than projects, knowledge isn't siloed. A lesson learned in one project surfaces in any workspace where the anchors match and category filters pass.

## MCP tools

### Core (always available)

| Tool | Purpose |
|---|---|
| `sutra_workspace` | Register workspace, check freshness, reparse, and manage tool tiers |
| `sutra_health` | Per-workspace file/symbol counts, parse errors, staleness |
| `sutra_map` | Project file skeleton ranked by importance (symbol count + fan-in + blast radius) |
| `sutra_outline` | File symbol table of contents — all symbols with kinds, line ranges, signatures |
| `sutra_explore` | Structural exploration — resolves aliases, qualified names, and fuzzy queries → ranked symbol map with fetch instructions and strategy hint |
| `sutra_grep` | Search indexed symbols by name pattern (FTS5-backed) |
| `sutra_symbol` | Read a symbol's source code with line numbers and context |
| `sutra_context` | Token-budgeted context packing — symbol + deps + dependents within a budget |
| `sutra_impact` | Blast radius analysis — direct callers, BFS depth-3, risk level |
| `sutra_deps` | File-level import dependency graph (BFS from a file, or all edges) |
| `sutra_components` | List discovered architectural components and member files |
| `sutra_conventions` | List discovered conventions (FCA-derived patterns) |
| `sutra_constraints` | Manage constraints (list, check violations, waive/unwaive) |
| `sutra_remember` | Write a code-anchored lesson with text and location anchors (auto-enriched with patterns and categories) |
| `sutra_lessons` | Query lessons — FTS5 text search with structured filters (category, symbol, verified status, project) |
| `sutra_help` | Agent-oriented help and workflow recipes |

### Analysis (enable via `sutra_workspace`)

| Tool | Purpose |
|---|---|
| `sutra_refs` | All usages of a symbol across the codebase, grouped by file. Optional `context_kind` filter (call, construction, type_use) |
| `sutra_calls` | Call hierarchy — callers or callees, BFS to configurable depth |
| `sutra_diff_impact` | Blast radius of a git diff — changed files, affected symbols, their callers |
| `sutra_cochange` | Files that historically change together with a given file |
| `sutra_pr_risk` | Composite PR risk score (0.0–1.0) combining blast radius, complexity, churn, and volume |
| `sutra_provenance` | Git history of a symbol's file with commit classification (feature, bugfix, refactor, etc.) |
| `sutra_trace` | Trace call chains — forward (entry points → symbol) or backward (symbol → leaves). Detects cycles |
| `sutra_winnow` | Multi-axis composite query — AND-intersect filters (kind, complexity, churn, calls_to, file_glob, name_regex) and rank by importance/complexity/churn |
| `sutra_review` | Structural review compositor — diffs current branch, computes risk score, identifies constraint violations, health findings, HRR shape changes, health delta, and ranks recommended reads |
| `sutra_file_health` | Per-file and per-component health report with scores, active findings, category deductions, and component instability |
| `sutra_hotspots` | Riskiest files ranked by git churn × blast radius × complexity |
| `sutra_dead` | Dead symbols (zero inbound references) and unreachable files. Auto-excludes tests, FFI entrypoints, benchmarks |
| `sutra_similar` | Find structurally similar functions (with symbol) or near-duplicate pattern families (without symbol) |
| `sutra_trend` | Health trend — compare two snapshots with per-file/per-component deltas, or query a file's score history over time |
| `sutra_commit_manifest` | Manifest of symbols and files changed in a commit or range |

## Common workflows

### Explore unfamiliar code

```
Agent: sutra_explore(query="constraint enforcement")
→ ranked symbol list with scores, components, estimated tokens
→ literal sutra_symbol fetch instructions for each item
→ strategy hint: read_top_n (n=3) — "top 3 are high-confidence matches in the constraints component"
→ edges between result items (call/dep relationships)
```

One call replaces the iterative `sutra_map` → `sutra_outline` → `sutra_symbol` → backtrack cycle. The strategy hint (`read_top_n`, `read_all`, `narrow_query`, `explore_component`) tells the agent what to do next.

### Review a branch

```
Agent: sutra_review(diff="branch")
→ risk score (0.0–1.0) with per-signal breakdown
→ constraint violations (blocking/advisory)
→ health findings and health delta vs. last snapshot
→ HRR shape changes (subtle structural shifts)
→ recommended files to inspect manually
```

### Investigate a symbol

```
sutra_explore(query="parse_rules")  → definition location
sutra_impact(symbol="parse_rules") → blast radius and risk level
sutra_calls(symbol="parse_rules")  → who calls it, what it calls
sutra_refs(symbol="parse_rules")   → every usage site
sutra_provenance(symbol="parse_rules") → git history with commit types
sutra_trace(symbol="parse_rules")  → call chains from entry points
```

### Record and query lessons

```
# Agent learns something while fixing a bug
Agent: sutra_remember(text="WAL checkpoint can stall if ...", anchors=["LessonsDb", "src/lessons/db.rs"])
→ lesson stored with inferred import-pattern and category anchors

# Later, another agent reads the same code
Agent: sutra_symbol(symbol="LessonsDb")
→ source code + [lesson] WAL checkpoint can stall if ...

# Explicit search
Agent: sutra_lessons(query="sqlite concurrency")
→ matching lessons ranked by relevance, flagged [verified] or [unverified]

# Citation during task close-out
Agent: sutra_remember(cite="01J...", source_tasks=["sutra/180"])
→ confidence increased, citation recorded
```

### Find code quality issues

```
sutra_file_health()                → worst files with findings and scores
sutra_hotspots()                   → riskiest files (churn × blast radius × complexity)
sutra_dead()                       → unreferenced symbols and files
sutra_similar()                    → near-duplicate function families
sutra_trend()                      → health changes between snapshots
```

## Guard (real-time constraint enforcement)

`sutra-guard` is a separate binary that runs as a Claude Code `PreToolUse` hook. When an agent is about to edit a file, the guard parses the proposed content to extract would-be import edges and checks them against constraint rules — blocking violations at introduce time, before they land in the index:

- **blocking** violations → edit denied with explanation
- **advisory/informational** → warning on stderr, edit proceeds
- **waived** → silent pass (from-file scoped: waiver on the importing file, not the target)

An edit that removes a blocking violation is always allowed through. If the guard cannot parse the proposed content (unsupported language, syntax error), it falls back to checking the file's current indexed edges.

For **ratcheted constraints**, the guard additionally intercepts edits to `.sutra/rules.toml` that would delete or lower the severity of a registered constraint. This check runs before any file-level analysis (rules.toml is not an indexed file). The deny message teaches the release ceremony (`sutra ratchet release`) and the strengthen-by-release-then-re-add pattern.

Configure in `.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Edit|Write",
      "command": "sutra-guard"
    }]
  }
}
```

## MCP configuration

### Claude Code (`~/.claude/settings.json`)

```json
{
  "mcpServers": {
    "sutra": {
      "command": "/home/you/.cargo/bin/sutra",
      "args": ["serve", "--stdio"]
    }
  }
}
```

### Gemini CLI (`~/.gemini/settings.json`)

```json
{
  "mcpServers": {
    "sutra": {
      "command": "/home/you/.cargo/bin/sutra",
      "args": ["serve", "--stdio"]
    }
  }
}
```

### OpenCode (`~/.config/opencode/opencode.json`)

```json
{
  "mcp": {
    "sutra": {
      "command": ["/home/you/.cargo/bin/sutra", "serve", "--stdio"],
      "enabled": true,
      "type": "local"
    }
  }
}
```

## Languages

- **Rust** — full support (functions, structs, enums, traits, impls, methods, modules, consts, macros)
- **Dart** — full support (classes, methods, functions, enums, mixins, extensions, type aliases)
- **Python** — full support (functions, classes, methods, decorators, async/generators, module-level variables, imports with package root discovery)
- **JavaScript** — full support (functions, arrow functions, classes, methods, generators, ES imports, CommonJS require, dynamic imports, re-exports, JSX component refs, effect detection)
- **TypeScript** — full support (all JS features plus interfaces, type aliases, enums, generics, access modifiers, decorators, ambient declarations, TSX)
- **C** — full support (functions, structs, enums, typedefs, macros, global variables, `#include` resolution)

JS and TS share an import system — cross-file import resolution handles relative imports with Node-style extension guessing (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.mts`, `.cts`) and index file resolution (`./dir` → `./dir/index.ts`). Bare specifiers (`react`, `@angular/core`) are left unresolved since `node_modules` is out-of-tree.

The core model is language-agnostic. Per-language adapters handle parsing and attribute extraction; the schema (files, symbols, edges) is uniform. Adding a language requires a tree-sitter grammar and an adapter that maps AST nodes to sutra's symbol kinds.

## Configuration

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SUTRA_DB_DIR` | `~/.sutra/` | Database directory |
| `SUTRA_WORKSPACES` | `~/.sutra/workspaces.toml` | Workspace registry |
| `SUTRA_PARSE_PARALLELISM` | CPU count | Max parallel parse workers |
| `SUTRA_STALE_THRESHOLD_SEC` | `600` | Seconds before an index snapshot is marked stale |
| `SUTRA_PARSE_TIMEOUT_SEC` | `60` | Max wall-clock for a single workspace reparse |
| `SUTRA_LOG_LEVEL` | `info` | Tracing filter when `RUST_LOG` is unset |
| `SUTRA_SIMILARITY_MODE` | `auto` | HRR similarity fidelity: `full`, `strip-only`, `off`, or `auto` (downgrades to strip-only above 200k function symbols) |
| `SUTRA_HRR_PARALLELISM` | CPU count | Max parallel HRR encode workers |

### Project configuration (`.sutra/`)

| File | Purpose |
|------|---------|
| `rules.toml` | Architectural constraints (forbidden deps, boundaries, cycles, fan-in, external crates, AST patterns) |
| `aliases.toml` | Vocabulary aliases — human-readable names for components, files, and symbols (see [Layer 5](#5-vocabulary-mapping-layer-5)) |
| `owners.toml` | Author alias mapping for ownership risk biomarker (maps agent emails to canonical human) |

## How it works

### Computational substrates

| Substrate | What it powers |
|---|---|
| **Tree-sitter** | Parsing — extracts symbols, references, imports from source code |
| **SQLite (WAL)** | Persistence — relational storage for all layers, snapshot history |
| **Differential dataflow** (timely) | Constraint enforcement — maintained views over the import graph for cycle detection, forbidden deps, and blast radius. Incremental: feed it edge deltas, all views update automatically |
| **Formal Concept Analysis** (FCA) | Convention detection — discovers implications in the symbol-attribute matrix, validated by support/confidence thresholds. Per-component adaptive thresholds |
| **HRR vectors** (1024-dim) | Structural similarity — FFT-based circular convolution encodes AST subtrees into fixed-size vectors. Strip mode removes identifiers for pure structural matching; embed mode preserves them |
| **Graph metrics** | Health scoring — fan-in, fan-out, instability, PageRank importance, cognitive/cyclomatic complexity from AST, churn and co-change from git history |

### Parse pipeline

```
file changed
  → tree-sitter re-parse (Layer 0 delta)
  → ref resolution (local, module, import edges)
  → graph rollups (fan_in, blast_radius)
  → git co-change computation
  → component membership update
  → FCA convention rebuild
  → health finding computation
  → HRR vector encoding
  → snapshot recording (per-file + per-component health scores)
```

### Freshness

Every tool response includes:

- `as_of` — timestamp of the latest snapshot
- `is_stale` — whether the snapshot exceeds the stale threshold

## Architecture

```
workspace files ───► tree-sitter → symbols, refs, imports
                          │
                          ▼
                ┌──────────────────────┐
                │ ref resolution       │
                │ graph rollups        │
                │ component membership │
                └────────┬─────────────┘
                         ▼
    ┌────────────────────┼────────────────────┐
    ▼                    ▼                     ▼
FCA conventions    DD constraints         HRR vectors
(patterns,         (forbidden deps,       (similarity,
 list-only)         boundaries, cycles)    duplicates)
    │                    │                     │
    └────────────────────┼─────────────────────┘
                         ▼
                health findings + scoring
                         │
                         ▼
                SQLite snapshots (WAL)          lessons store
                         │                 (~/.sutra/lessons.db)
                         │              contextual surfacing via
                         │               read / impact tools
                         │
                         ▼
                MCP server (stdio) → 31 tools with freshness envelopes
```

## Vision

Sutra's mission is to help human-AI teams produce *coherent* software, not just functional software. The full vision is documented in `docs/sutra-vision.md` and organized as layers:

| Layer | Domain | Status |
|---|---|---|
| 0 | Structural facts (tree-sitter → symbols, refs, imports) | Implemented |
| 1 | Architecture (components, hierarchy, boundaries) | Implemented (directory-based clustering; graph clustering planned) |
| 2 | Conventions (FCA detection, `sutra_conventions` list) | Implemented (detection + list; in-loop surfacing retired) |
| 3 | Constraints (DD enforcement, guard, waivers) | Implemented |
| 4 | Health (biomarkers, scoring, snapshots, trends) | Implemented |
| 5 | Vocabulary (human-to-code concept mapping) | Partial (aliases; HRR fuzzy matching planned) |
| 6 | Similarity (HRR vectors, duplicates, semantic diff) | Implemented |
| 7 | Lessons (code-anchored negative knowledge, contextual surfacing, confidence lifecycle) | Implemented |
| 8 | Verification (property tests, model checking, mutation testing) | Deferred |
