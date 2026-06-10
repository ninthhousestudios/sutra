# sutra

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

Code intelligence for [manas](https://github.com/ninthhousestudios/manas) — a living architectural model of your codebase, served as an MCP server.

Sutra parses your code with tree-sitter, discovers conventions with formal concept analysis, enforces constraints with differential dataflow, detects structural similarity with holographic reduced representations, and tracks codebase health with empirically calibrated biomarkers. It exposes all of this through 33 MCP tools that AI coding agents (and humans) can call.

The core loop: **orient** (brief the agent before it writes code) → **check** (flag architectural violations as code is written) → **review** (produce an architectural change report the human can assess without reading every line) → **teach** (human refines the model by updating constraints, conventions, and boundaries).

## Install

```bash
cargo install --path .
```

## Quick start

```bash
# Start the MCP server (stdio, one per client)
sutra serve --stdio
```

Workspaces are registered automatically when an agent calls `sutra_status` with a path, or explicitly:

```bash
sutra workspaces add myproject /path/to/project rust
sutra parse myproject
```

## What sutra does

### 1. Structural index (Layer 0)

Tree-sitter parses your codebase into a SQLite index of files, symbols (functions, types, traits, modules), and relationships (calls, imports, contains, implements). This is the ground truth that everything else builds on.

Every response includes a **freshness envelope** (`as_of`, `is_stale`) so callers always know how current the data is.

### 2. Architectural components (Layer 1)

Sutra discovers components — groups of related code — via directory-structure clustering and human refinement. Components have stable identity, lifecycle state (`stable` or `sketch`), and human-assigned aliases. They're the unit of orientation, convention scoping, and health scoring.

### 3. Convention detection (Layer 2)

Formal Concept Analysis (FCA) discovers implicit patterns in your code: "public functions return Result," "handlers take &self as first parameter," "error types implement Display." Conventions have a lifecycle:

- **descriptive** — this pattern is common (auto-detected default)
- **preferred** — this pattern should continue (human-promoted)
- **deprecated** — this pattern exists but should fade
- **forbidden** — do not copy this pattern

Agents are oriented with preferred conventions and warned about deprecated ones. Sutra generates **structural templates** from conventions (e.g. `pub async fn $NAME(&self, $PARAMS) -> Result<$T>`) so agents can write code that fits.

**Convention drift detection** tracks entropy across snapshots — if each agent session introduces slightly different patterns, sutra alerts before the codebase diverges.

#### Convention management

Conventions are discovered automatically by FCA, but you control them two ways:

**File-based suppression and exemption** — in `.sutra/rules.toml` (the same file that holds constraints):

```toml
[conventions]
suppress = ["a1b4c2d1"]  # completely silence this convention during review

[[conventions.exempt]]
convention = "e5f6g7h8"
symbols = ["InternalError", "src/foo.rs::DebugHelper"]  # per-symbol exemptions
```

- `suppress` — list of convention IDs to ignore entirely during `sutra_review` checks
- `exempt` — per-convention, per-symbol exemptions. Bare names match across all files; file-qualified names (e.g. `src/foo.rs::DebugHelper`) scope to a specific file

These are check-time silencing only — they don't change the convention's lifecycle state in the database.

**Lifecycle management via MCP** — the `sutra_conventions` tool controls the full lifecycle:

```
sutra_conventions(action="list")
→ all conventions with lifecycle state + pending proposals

sutra_conventions(action="set_lifecycle", convention_id="<id>", lifecycle_state="preferred", reason="team consensus")
→ manually promote/demote (descriptive → preferred → deprecated → forbidden)

sutra_conventions(action="accept", proposal_id=<id>)
→ accept an auto-generated lifecycle proposal

sutra_conventions(action="waive", convention_id="<id>", symbol="src/foo.rs::process", rationale="...", waived_by="josh")
→ grant a tracked exception (shows in review output as waived_violations)
```

Waivers differ from `rules.toml` exemptions: waivers are database-stored with rationale and attribution, and appear as `waived_violations` in review output. File-based exemptions silence findings with no audit trail.

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
scope = "src/core/"

[[constraint]]
kind = "max_fan_in"
target = "src/config.rs"
threshold = 10
severity = "advisory"
```

Constraints are checked via a differential dataflow engine — a timely dataflow worker maintains views over the import graph, so cycle detection, forbidden dependency violations, and blast radius queries update incrementally as code changes.

Each constraint has a **severity** (blocking, advisory, informational). The **guard binary** (`sutra-guard`) runs as a Claude Code `PreToolUse` hook and blocks edits that introduce blocking violations in real time.

Constraints and conventions both support **waivers** — human-granted exceptions with tracked rationale that appear in every review touching the waived area.

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

### 5. Vocabulary mapping (Layer 5)

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

- **`[component]`** — maps a human name to a component name (as shown in `sutra_components`)
- **`[file]`** — maps a human name to a file path
- **`[symbol]`** — maps a human name to a symbol name

Aliases are synced to the database during workspace indexing. Use `sutra_resolve` to look up any term:

```
Agent: sutra_resolve(query="being detail cards")
→ alias match: component "being_detail"
→ file locations for all member files
```

Resolution searches in priority order: aliases (exact match) → component names (substring) → semantic anchor names (substring). Orphan detection warns when an alias points to a dissolved component or missing file.

This means you can tell an agent "find the being detail cards code" and it resolves to concrete file locations without the agent having to rediscover the mapping each time.

### 6. Structural similarity (Layer 6)

Holographic Reduced Representations (HRR) encode each function's AST into a 1024-dimensional vector. Two modes:

- **strip** — structure only, identifiers removed. Finds copy-paste variants regardless of naming.
- **embed** — structure + identifiers. Finds semantically similar code.

This powers duplicate detection (pattern families of 3+ structurally identical functions) and semantic diff in review (classifying changes as safe-refactor vs. subtle-behavioral-change based on HRR delta vs. text delta).

## MCP tools

### Core (always available)

| Tool | Purpose |
|---|---|
| `sutra_status` | Register a workspace and check freshness |
| `sutra_health` | Per-workspace file/symbol counts, parse errors, staleness |
| `sutra_map` | Project file skeleton ranked by importance (symbol count + fan-in + blast radius) |
| `sutra_outline` | File symbol table of contents — all symbols with kinds, line ranges, signatures |
| `sutra_find` | Jump to a symbol definition by name (exact + FTS5 fuzzy) |
| `sutra_grep` | Search indexed symbols by name pattern (FTS5-backed) |
| `sutra_read` | Read a symbol's source code with line numbers and context |
| `sutra_impact` | Blast radius analysis — direct callers, BFS depth-3, risk level |
| `sutra_deps` | File-level import dependency graph (BFS from a file, or all edges) |
| `sutra_orient` | Convention-aware orientation for a component or file — preferred conventions with templates, deprecated/forbidden warnings, drift alerts, active constraints and violations, health scores, waivers |
| `sutra_components` | List discovered architectural components and member files |
| `sutra_resolve` | Resolve a vocabulary term (alias, component name, or anchor) to code locations |
| `sutra_conventions` | Manage convention lifecycle (list, promote, deprecate, waive) and review proposals |
| `sutra_constraints` | Manage constraints (list, check violations, waive/unwaive) |
| `sutra_parse` | Trigger a workspace reparse |
| `sutra_tools` | Enable/disable tool tiers |
| `sutra_add_root` | Register a workspace root and start indexing |
| `sutra_help` | Agent-oriented help and workflow recipes |

### Analysis (enable via `sutra_tools`)

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
| `sutra_review` | Structural review compositor — diffs current branch, computes risk score, identifies constraint violations, convention violations/matches, health findings, HRR shape changes, convention drift, health delta, and ranks recommended reads |
| `sutra_file_health` | Per-file and per-component health report with scores, active findings, category deductions, and component instability |
| `sutra_hotspots` | Riskiest files ranked by git churn × blast radius × complexity |
| `sutra_dead` | Dead symbols (zero inbound references) and unreachable files. Auto-excludes tests, FFI entrypoints, benchmarks |
| `sutra_duplicates` | Near-duplicate function detection via HRR strip vector clustering into pattern families |
| `sutra_similar` | Find structurally similar functions — strip mode (AST shape) or embed mode (structure + naming) |
| `sutra_trend` | Health trend — compare two snapshots with per-file/per-component deltas, or query a file's score history over time |

## Common workflows

### Orient before editing

```
Agent: sutra_orient(scope="src/tools/review.rs")
→ preferred conventions with signature templates
→ deprecated patterns to avoid
→ active constraints and any current violations
→ health score and top findings
→ pending lifecycle proposals
```

### Review a branch

```
Agent: sutra_review(diff="branch")
→ risk score (0.0–1.0) with per-signal breakdown
→ constraint violations (blocking/advisory)
→ convention violations and matches (deprecated/forbidden)
→ health findings and health delta vs. last snapshot
→ HRR shape changes (subtle structural shifts)
→ convention drift alerts
→ recommended files to inspect manually
```

### Investigate a symbol

```
sutra_find(name="parse_rules")     → definition location
sutra_impact(symbol="parse_rules") → blast radius and risk level
sutra_calls(symbol="parse_rules")  → who calls it, what it calls
sutra_refs(symbol="parse_rules")   → every usage site
sutra_provenance(symbol="parse_rules") → git history with commit types
sutra_trace(symbol="parse_rules")  → call chains from entry points
```

### Find code quality issues

```
sutra_file_health()                → worst files with findings and scores
sutra_hotspots()                   → riskiest files (churn × blast radius × complexity)
sutra_dead()                       → unreferenced symbols and files
sutra_duplicates()                 → near-duplicate function families
sutra_trend()                      → health changes between snapshots
```

## Guard (real-time constraint enforcement)

`sutra-guard` is a separate binary that runs as a Claude Code `PreToolUse` hook. When an agent is about to edit a file, the guard parses the proposed content to extract would-be import edges and checks them against constraint rules — blocking violations at introduce time, before they land in the index:

- **blocking** violations → edit denied with explanation
- **advisory/informational** → warning on stderr, edit proceeds
- **waived** → silent pass (from-file scoped: waiver on the importing file, not the target)

An edit that removes a blocking violation is always allowed through. If the guard cannot parse the proposed content (unsupported language, syntax error), it falls back to checking the file's current indexed edges.

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

### Project configuration (`.sutra/`)

| File | Purpose |
|------|---------|
| `rules.toml` | Architectural constraints (forbidden deps, boundaries, cycle rules, fan-in limits) |
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
  → FCA convention rebuild + lifecycle proposals
  → template generation
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
(lifecycle, drift, (forbidden deps,       (similarity,
 templates)         boundaries, cycles)    duplicates)
    │                    │                     │
    └────────────────────┼─────────────────────┘
                         ▼
                health findings + scoring
                         │
                         ▼
                SQLite snapshots (WAL)
                         │
                         ▼
                MCP server (stdio) → 33 tools with freshness envelopes
```

## Vision

Sutra's mission is to help human-AI teams produce *coherent* software, not just functional software. The full vision is documented in `docs/sutra-vision.md` and organized as layers:

| Layer | Domain | Status |
|---|---|---|
| 0 | Structural facts (tree-sitter → symbols, refs, imports) | Implemented |
| 1 | Architecture (components, hierarchy, boundaries) | Implemented (directory-based clustering; graph clustering planned) |
| 2 | Conventions (FCA detection, lifecycle, templates, drift) | Implemented |
| 3 | Constraints (DD enforcement, guard, waivers) | Implemented |
| 4 | Health (biomarkers, scoring, snapshots, trends) | Implemented |
| 5 | Vocabulary (human-to-code concept mapping) | Partial (aliases, orient; HRR fuzzy matching planned) |
| 6 | Similarity (HRR vectors, duplicates, semantic diff) | Implemented |
| 7 | Verification (property tests, model checking, mutation testing) | Deferred |
