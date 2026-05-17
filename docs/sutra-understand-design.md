# sutra understand — design doc

## The shift

The v1 PRD positions sutra as a "review intelligence engine." This design
expands the scope: sutra becomes a **codebase understanding layer** — the thing
that sits between raw code and anyone (human or agent) trying to work with it.

Review is one of three modes:

| Mode | Question it answers | Primary user |
|---|---|---|
| **Orient** | "I'm about to work on X — what do I need to know?" | Agent before modifying, human before reading |
| **Navigate** | "Where is the settings screen? What's similar to this?" | Human exploring, agent resolving a vague instruction |
| **Review** | "I changed X — what did I break?" | Agent post-change, human reviewing a PR |

The v1 PRD covers Review. This doc covers Orient and Navigate, plus the
shared foundation (component architecture) that makes all three modes work
in terms of subsystems rather than raw files.

## Prior art surveyed

Two projects were analyzed for ideas:

**CodeBoarding** — LLM-assisted architecture extraction. Clusters a call graph
(Louvain/Leiden/greedy modularity), has an LLM label and group clusters into
named components, then deterministically assigns every symbol to exactly one
component. Produces a hierarchical component tree with descriptions, relations,
Mermaid diagrams, and health checks. Key ideas adopted: graph clustering for
components, 100% node coverage via orphan assignment, health metrics that reuse
the graph, incremental semantic tracing.

**GrapeRoot (Codex-CLI-Compact)** — a context pre-loading layer for coding
agents. Builds a file+symbol dependency graph, uses it as a retrieval oracle
with confidence-gated exploration caps. Maintains a "dual graph" (static
structure + session action history) so retrieval improves across turns. Key
ideas adopted: confidence + exploration budget on results, session memory
feeding back into retrieval, concept-to-code mapping via the graph, symbol-level
reads (`file::symbol`).

**HRR spike (sutra/spike/hdc-ast-encoding)** — proved that HDC/HRR vectors
computed from tree-sitter ASTs can discriminate structural similarity (strip
mode: pure AST shape) and semantic similarity (embed mode: AST + identifiers).
Functions sharing structural traits cluster together. Query-by-example works.
All computation is local, sub-millisecond per function.

## Component architecture

### What it is

A component is a named group of symbols and files that form a coherent
subsystem. Components are hierarchical: top-level components (5–10 for a
typical project) decompose into sub-components. Every symbol in the codebase
belongs to exactly one component.

Components are the bridge between human concepts ("the settings screen,"
"authentication," "the parser") and code locations. They are the unit of
architecture.

### How it's computed

Two signals, combined:

1. **Graph connectivity** — clustering on the call/dependency graph. Symbols
   that call each other or share dependencies cluster together. Algorithm
   selection follows CodeBoarding's approach: try Louvain, greedy modularity,
   and Leiden at method/class/file granularity, pick the best coverage score.
   Fall back to connected components.

2. **HRR structural similarity** — symbols that share AST structure cluster
   together, even without call graph edges. This catches interface
   implementations, handler functions, test helpers — structurally similar
   code that shares patterns but isn't connected by edges.

The combined signal: build a similarity matrix from graph adjacency (binary:
connected or not) blended with HRR cosine similarity (continuous: 0.0–1.0),
then cluster. The blend weight is tunable; default favors graph connectivity
(0.7 graph, 0.3 HRR) since direct dependencies are the stronger signal.

### Labeling

Labels come from three sources, in order of preference:

1. **Directory structure** — the cheapest signal. `src/auth/*.rs` is probably
   "Authentication." Most projects have meaningful directory names.
2. **Key entity names** — the top-3 symbols by fan-in within a component. If
   they share a prefix or domain term, use it.
3. **LLM labeling** (optional) — given the cluster's key files and symbols,
   ask an LLM to name and describe it. Works with cheap/local models. Not
   required for the system to function.

### 100% coverage

Every symbol must belong to a component. Orphans (symbols not in any cluster)
are assigned by:

1. File co-location — another symbol in the same file is in a cluster.
2. Graph distance — assign to the nearest cluster's component by shortest path.
3. Fallback — first component (alphabetical).

### Persistence

SQLite tables:

```
components(id INTEGER PRIMARY KEY, parent_id, name, label, description,
           hrr_vec BLOB, created_at, updated_at)

symbol_components(symbol_id, component_id)  -- every symbol, exactly one component

component_edges(src_id, dst_id, edge_count, coupling_strength)
```

The component map is recomputed when:
- A new workspace is registered (full compute)
- Files change and are reparsed (incremental: recompute affected clusters only)
- User forces a recompute (`sutra_components --recompute`)

Staleness: the component map includes the git SHA or file mtime set it was
computed from. Queries check staleness and report it in the freshness envelope.

## Cross-session memory

### The problem

An agent is told "fix the settings screen." Today it greps, reads files,
discovers that `src/ui/settings.rs` + `src/ui/settings_panel.rs` +
`src/config/user_prefs.rs` form the settings subsystem. Next session, it
does this again from scratch.

### The solution

The component map IS the cross-session memory. Once computed, the mapping
from "Settings" (component label) to its constituent files and symbols is
persisted in SQLite and survives across sessions. When the agent (or human)
queries "settings," it matches against component labels and key entities
without rediscovery.

For richer matching, each component gets an HRR embed-mode vector (bundle of
its constituent symbols' vectors). A natural-language concept query can be
matched against component vectors for fuzzy routing — "the screen where users
change their preferences" matches the Settings component even without an
exact keyword hit, because the identifier text embedded in the HRR vector
captures the semantic neighborhood.

### Human aliases

Users can pin aliases in `.sutra/aliases.toml`:

```toml
[aliases]
"settings screen" = "Settings"
"auth flow" = "Authentication"
"the parser" = "Parser::Core"
```

These are checked first, before fuzzy matching. They accumulate over time as
users establish their vocabulary for their own codebase.

## Orient mode

### `sutra_orient` (MCP tool + CLI + web UI panel)

Input: a file path, symbol name, or component name.

Output:
```
OrientResult {
  target: TargetInfo,              // what you asked about
  component: ComponentSummary,     // which subsystem, its description, parent chain
  conventions: Vec<Convention>,    // FCA conventions that apply here
  constraints: Vec<Constraint>,    // forbidden deps, architectural boundaries
  dependencies: DependencySummary, // fan-in, fan-out, key consumers, key providers
  health: HealthSummary,           // instability, complexity, churn, god-class flags
  recommended_reads: Vec<ReadRec>, // files to understand before modifying
  confidence: Confidence,          // how fresh/complete this information is
  exploration_budget: Budget,      // for agents: how much more exploration is warranted
}
```

The confidence + exploration budget idea from GrapeRoot: if the index is
fresh and coverage is complete, confidence is `high` and the budget is zero
(the agent has everything it needs). If the index is stale or the component
is poorly covered, confidence drops and the budget increases, giving the agent
permission to do supplementary exploration.

### For humans (web UI)

The orient panel is a sidebar that activates when you click a file or component.
Shows the same information as the MCP tool, rendered visually: component
breadcrumb, dependency mini-graph, health badges, convention list. No CLI
needed — it's contextual to what you're looking at.

### For agents (MCP)

Agents call `sutra_orient` before modifying code. The response is structured
so the agent can decide whether to proceed or gather more context. The
exploration budget prevents spiraling while still allowing necessary discovery.

## Navigate mode

### Component graph (web UI)

The primary navigation surface. Interactive graph visualization:

- **Nodes** = components. Sized by code volume, colored by health.
- **Edges** = dependency/call relationships. Thickness = coupling strength.
- **Click** a node → expand into sub-components (hierarchical drill-down).
- **Click** a sub-component → see its files, key symbols, health detail.
- **Search bar** → type a concept, symbol name, or file path. Matches against
  component labels, symbol names, HRR vectors. Results highlight the relevant
  nodes in the graph.

### `sutra_similar` (MCP tool + CLI + web UI)

Input: a symbol name.

Output: top-N similar symbols with similarity scores and mode indicator.

Two modes:
- **Structural** (HRR strip) — "functions that work like this one." Useful for
  finding patterns, understanding conventions, detecting near-duplicates.
- **Semantic** (HRR embed) — "functions about the same thing." Useful for
  finding related code across the codebase.

### `sutra_architecture` (MCP tool + CLI)

Returns the full component tree with edges, for agents that need the big
picture without the web UI. Bounded output (top-level components only by
default, expandable per-component).

## Health metrics

Computed from the existing graph, persisted per-component and per-file:

| Metric | What it measures | Source |
|---|---|---|
| Fan-in | How many symbols depend on this | refs/calls graph |
| Fan-out | How many symbols this depends on | refs/calls graph |
| Instability | Ce/(Ca+Ce) — Martin's metric | fan-in + fan-out |
| Cohesion | Internal edges / total edges per component | component graph |
| God class score | Method count × LOC × aggregate fan-out | symbol table + graph |
| Churn | Edit frequency over recent git history | git log |
| Complexity | Cyclomatic complexity estimate from AST | tree-sitter parse |

Cohesion is particularly interesting because it validates the clustering —
if a component's members talk more to outsiders than to each other, the
clustering is wrong and should be flagged.

## HRR integration points

Summary of where HRR vectors are used:

| Use | Mode | Purpose |
|---|---|---|
| Component clustering | Strip | Structural similarity as clustering signal |
| Component fingerprint | Embed | Compact representation for concept matching |
| Cosmetic change filter | Strip | Before vs after comparison to skip trivial changes |
| `sutra_similar` | Both | Find structurally/semantically similar code |
| Convention soft-detection | Strip | Groups of functions sharing patterns without formal rules |
| Concept-to-code routing | Embed | Match "settings screen" to the right component |

All HRR computation is local, fast (sub-ms per function), and requires no
LLM. This is the layer that makes sutra useful even on a laptop with no
cloud access.

## Web UI

### Architecture

Static HTML + vanilla JS, embedded in the sutra binary as static assets.
Served by the sutra daemon on a local port (configurable, default 9400).

No build step. One HTML file, one JS file, one CSS file. The graph
visualization library (d3-force or cytoscape.js) is the only substantial
dependency, vendored.

### Pages/views

**Architecture view** (default) — the component graph. Interactive
force-directed or hierarchical layout. Components as nodes, dependencies as
edges. Sidebar shows detail for selected node.

**File explorer** — tree view of files, colored by component ownership.
Click a file → orient panel. Useful for "which component owns this file?"

**Search** — unified search bar. Queries match against:
- Component labels and descriptions
- Symbol names (qualified and unqualified)
- File paths
- HRR similarity (if query looks like a concept rather than an identifier)

Results grouped by type, with component context.

**Health dashboard** — heatmap or table view of health metrics across
components. Sort by worst instability, lowest cohesion, highest churn.
Quick way to find trouble spots.

### API

The web UI talks to the sutra daemon via a local HTTP API:

```
GET  /api/components              — component tree
GET  /api/components/:id          — component detail + files + symbols
GET  /api/components/:id/graph    — sub-component graph for drill-down
GET  /api/orient/:path_or_symbol  — orient result
GET  /api/search?q=...            — unified search
GET  /api/similar/:symbol         — similar symbols
GET  /api/health                  — health summary
GET  /api/health/:component_id    — per-component health detail
```

JSON responses. The same API could be used by other tools later.

## CLI

Point queries for terminal use:

```
sutra orient src/daemon.rs        — orient on a file
sutra orient parse_config         — orient on a symbol
sutra similar parse_config        — find similar functions
sutra arch                        — print component tree
sutra arch --component Auth       — expand one component
sutra health                      — health summary
sutra health --worst 10           — top 10 worst files
sutra search "settings screen"    — concept search
```

Output is text/table by default. `--json` for structured output.

## LLM tiers

Everything above works without an LLM. LLMs add richness:

| Tier | Cost | What it adds |
|---|---|---|
| **Local only** | Zero | Components (auto-labeled), HRR similarity, health metrics, structural orient, graph navigation |
| **Cheap/local model** | Low | Component descriptions, natural-language orient summaries, richer search |
| **Capable cloud model** | Higher | Semantic tracing (multi-hop impact), rich architecture narratives, "explain this subsystem" |

The tier is configurable in `.sutra/config.toml`:

```toml
[llm]
tier = "local"  # "local" | "cheap" | "capable"
# provider/model settings per tier...
```

Each tool gracefully degrades: if the configured tier can't provide a field
(e.g. `description` requires at least "cheap"), the field is omitted and the
response notes what would be available at a higher tier.

## Incremental updates

The component map must stay fresh without full recomputation on every change.

**On file reparse** (already happens when sutra detects file changes):
1. Recompute HRR vectors for changed symbols.
2. Check if any symbol's cluster assignment changed (compare new similarity
   scores to cluster centroids).
3. If assignment is stable: update the symbol's vector in place. Done.
4. If assignment changed: re-cluster the affected region (the old component
   and candidate new component). This is local, not global.

**On significant structural change** (new files, deleted files, large diffs):
- Full re-cluster with the existing component labels as soft priors (prefer
  stability over churn in the component map).
- If the LLM tier is enabled, re-label any components whose composition
  changed significantly.

**Staleness tracking**: every component stores the SHA/mtime set it was
computed from. Queries against stale components include a `stale` flag in the
freshness envelope.

## Relationship to v1 PRD

This design is additive to the v1 PRD, not a replacement:

| v1 PRD feature | How it connects |
|---|---|
| `sutra_review` | Uses component context to scope review output. Convention violations and blast radius are reported per-component. |
| FCA conventions | Checked during orient. Surfaced in the web UI per-component. |
| DD maintained views | Cycle detection and blast-radius rollups feed into health metrics and orient. |
| Structured diagnostics | Extended with confidence + exploration budget from orient. |
| `.sutra/rules.toml` | Extended with `[aliases]` section for human vocabulary mapping. |

The implementation order would be:
1. Component architecture (clustering + persistence + incremental update)
2. Orient mode (MCP tool + CLI)
3. Web UI (architecture view + search + orient panel)
4. Navigate mode (`sutra_similar`, `sutra_architecture`)
5. Health metrics
6. Cross-session memory (alias resolution, concept routing)
7. LLM enrichment tiers

Items 1–3 are the MVP that answers "help me understand this codebase."

## Specific references from surveyed projects

### CodeBoarding — clustering and architecture

**Call graph + clustering algorithm** (`static_analyzer/graph.py`,
`CallGraph.cluster()`): Tries Louvain, greedy modularity, and Leiden at three
abstraction levels (raw node, class-collapsed, file-collapsed). For each
algorithm+level combo, scores by coverage ratio (fraction of nodes in clusters
of >= min_cluster_size). If best coverage >= 50% at a level, stops there. Picks
overall highest-scoring candidate. Fallback: connected components. Deterministic
seed (42) on Louvain/Leiden.

**Level-up contraction**: When clustering at "class" or "file" level, the graph
is contracted first — all methods of the same class become one node — then
clustered, then results mapped back to individual methods. This is how it
handles the granularity problem (method-level is too noisy, file-level misses
internal structure).

**Orphan assignment** (post-clustering, deterministic): (1) file co-location —
another symbol in same file has a cluster, (2) `nx.single_source_shortest_path_length`
on undirected graph — assign to nearest cluster, (3) first component. This is
what guarantees 100% coverage.

**Token budget pruning** (`ClusterMethodsMixin._build_cluster_string`,
`ClusterPromptBudget`): Computes available chars as
`(input_tokens - output_headroom - overhead) * 0.9 * chars_per_token`. If the
cluster string exceeds budget, `cfg_skip_planner.plan_skip_set()` iteratively
sheds nodes per-language until it fits. Relevant if we ever need to render
component context for LLM consumption with bounded size.

**Validation loop** (`CodeBoardingAgent._validation_invoke()`): Every LLM call
that produces structured output goes through best-of-N-with-feedback.
Validators have weights (coverage validators 4x higher than structural ones).
Fuzzy auto-correction via `difflib.SequenceMatcher` (threshold 0.75) handles
LLM typos before failing. Max 3 attempts, returns highest-scoring result. Worth
adapting if we add LLM labeling.

**Incremental semantic tracing** (`diagram_analysis/incremental_pipeline.py`,
`IncrementalUpdater.compute_delta()`): Groups changed methods by
strongly-connected-component region in the call graph. Cosmetic filter:
fingerprint comparison after stripping comments — skip if semantically
identical. Fast-path: if method body changed but signature is stable and no
upstream callers, mark impacted directly without LLM. Multi-hop LLM trace per
region: max 3 hops, max 30 fetched methods. Independent regions run in parallel
via `ThreadPoolExecutor`.

**Health checks** (`health/runner.py`): Fan-in, fan-out, god classes (method
count × LOC × aggregate fan-out), inheritance depth, circular dependencies,
package instability (Martin's Ce/(Ca+Ce)), component cohesion
(internal_edges/total_edges per cluster), unused code from LSP diagnostics.
Top-20 highest-risk files by composite score. Output:
`.codeboarding/health/health_report.json`.

**EASE encoding** (`diagram_analysis/ease.py`): Arrays encoded as dicts with
two-character keys (`aa`, `ab`, `ac`, ...) plus a `display_order` list. Makes
JSON Pointer paths stable under add/remove — `components/ab/description`
doesn't shift when a sibling is deleted. Used for incremental JSON patching
(RFC 6902). Worth considering if we need stable component IDs for external
references.

### GrapeRoot — retrieval oracle and session memory

**`graph_continue` tool** (MCP, `mcp_graph_server.py` in closed-source
`graperoot` package): Mandatory first call each turn. Returns
`recommended_files`, `confidence` (`high`/`medium`/`low`), and
`max_supplementary_greps` + `max_supplementary_files` caps. High confidence =
stop entirely. Medium = 1 extra grep + 1 extra file. Low = same caps. This is
the confidence-gated exploration pattern.

**`file::symbol` notation**: Instead of reading entire files, the agent requests
specific functions by `path::symbol_name`. The graph server knows symbol line
ranges and returns only that slice. Sutra's `sutra_read` already supports
symbol-level reads; the pattern here is surfacing symbol-level granularity in
*retrieval results* (recommended reads point to functions, not just files).

**Dual graph concept**: `info_graph.json` = static structural/semantic graph
(files, symbols, imports). `chat_action_graph.json` = session memory (which
files read, which edited, what queries made). The session graph grows across
turns and feeds back into retrieval: files already read get deprioritized,
files edited get elevated for re-checking. The compounding savings come from
not rediscovering context the agent already has.

**PreCompact hook** (`prime.sh`, run via Claude Code `PreCompact` hook): When
the LLM context window is about to be compressed, this script re-injects the
graph's recommended context (current file recommendations, recent context store
entries). Prevents quality degradation in long sessions. The insight: context
compaction destroys the working set, and re-injection recovers it cheaply.

**Context store** (`context-store.json`): `graph_add_memory` stores decisions,
tasks, facts, blockers. Entries are typed, tagged, limited to <15 words, and
linked to related files. At session start, `prime.sh` re-injects entries from
the last 7 days (max 15 lines). This is the cross-session continuity mechanism.
Relevant for sutra's alias/concept learning.

**Policy enforcement** (injected into `CLAUDE.md`): The agent is instructed to
always call `graph_continue` before any file exploration or grep. The graph is
the first-class tool; ripgrep is a fallback with strict per-turn call limits.
This inversion — graph first, then explore — is the behavioral pattern that
makes the token savings work. Sutra could adopt a similar pattern where
`sutra_orient` is the recommended first call before any modification.

**Token stop-hook** (`stop.sh`): Runs on the Claude Code `Stop` hook. Parses
the Claude transcript JSONL to extract real API token usage, uses an offset file
to avoid double-counting across resumed sessions, and POSTs totals to the
dashboard. Not directly relevant to sutra's core, but shows how to instrument
agent sessions.

**Benchmark methodology** (`benchmark/run_preinjection_benchmark.py`): 3-way
comparison — Normal Claude (native tools), MCP-DGC (graph via MCP),
Pre-Injection DGC (packed context injected into system prompt before the query,
removing tool-call round trips entirely). Quality scored with category-specific
regex checks. The pre-injection approach is interesting: instead of giving the
agent tools to query the graph, pack the relevant context directly into the
prompt. This is what `sutra_orient` effectively does — front-load the context
so the agent doesn't have to discover it.

## Open questions

- **Graph library**: d3-force (more control, more work) vs cytoscape.js
  (batteries-included, less flexible)? Leaning cytoscape for the MVP.

- **Component stability**: How aggressively should we preserve existing
  component boundaries when code changes? Too stable = stale architecture.
  Too reactive = confusing churn. Probably needs a configurable threshold.

- **HRR vector storage**: In SQLite as BLOBs (simple, query requires
  full scan) or in a separate vector index (faster similarity search, more
  complexity)? At sutra's scale (thousands of symbols, not millions), BLOB
  scan is probably fine.

- **Alias learning**: Should sutra automatically learn aliases from agent
  sessions? ("The agent was asked about 'settings screen' and ended up
  reading src/ui/settings.rs" → store the mapping.) Could be noisy.

- **Component count**: CodeBoarding targets 5–8 top-level components. Is that
  right for all project sizes? Probably needs to scale with codebase size,
  with a configurable cap.
