# Qartez feature survey

Assessment of qartez-mcp's 30 tools against sutra's current capabilities.
Goal: identify features worth porting vs. noise. Reviewed 2026-04-29 against
qartez v0.7.3.

Sutra has ~15 tools. Qartez has 30 across four tiers (core, analysis, refactor,
meta). The core and most analysis tools overlap with what sutra already does
(map, find, grep, read, outline, impact, deps, refs, calls, cochange,
diff_impact, hotspots, unused/dead). The interesting delta is in the analysis
tier features sutra lacks.

## Implement next

### `context` — related files for a task

Given seed files and an optional natural-language task description, combines
import edges, co-change history, and transitive dependencies to surface files
the agent would otherwise miss. This is the tool that prevents "I edited X but
forgot about Y."

Sutra already has all the underlying data: import graph, co-change pairs, blast
radius, PageRank. Implementation is a new scoring/ranking layer that fuses
these signals, not new infrastructure.

**Effort**: small-medium. New tool handler, scoring function, optional text
boosting via FTS5 or smriti embeddings.

### `test_gaps` — test coverage gap analysis

Three modes:
- **map** — which test files cover which source files (via import edges)
- **gaps** — untested source files ranked by risk (PageRank × health × blast)
- **suggest** — given a git diff, which test files to run

High value for agents doing review or pre-merge audit. Sutra already has the
import graph, PageRank, and file health scores to build all three modes.

**Effort**: medium. Needs a heuristic to identify test files (path patterns
like `tests/`, `test/`, `*_test.rs`, `*_test.dart`). The ranking and suggest
modes are straightforward compositions of existing queries.

### `clones` — duplicate code detection

Groups symbols with identical AST structure (ignoring identifier names) via
shape hashing. Useful for review and refactoring recommendations — "these 3
functions are structurally identical, consider extracting."

Sutra already does tree-sitter parsing. Adding a shape hash during symbol
extraction is a modest extension to the parser. Storage is one new column.
The query tool groups by hash and filters by minimum line count.

**Effort**: small-medium. Hash computation during parse, new column, new tool
handler with grouping query.

## Consider later

### `smells` — code smell detection

God functions (high complexity), long parameter lists, feature envy (functions
referencing more external symbols than internal). Sutra has complexity metrics;
parameter count comes free from signatures. Feature envy needs cross-referencing
external vs. internal symbol usage — moderate parser and query work.

### `trend` — complexity trend over git history

Shows how a function's cyclomatic complexity changed across commits. Useful for
spotting creeping complexity, but requires re-parsing old file revisions from
git — nontrivial infrastructure that doesn't exist in sutra today.

### `boundaries` — architectural layer checking

Validates import rules between layers defined in a config file. Qartez uses
Leiden community detection to suggest initial boundaries. Powerful concept but
big scope: needs a config format, a clustering algorithm, and a violation
checker.

### `knowledge` / bus factor — git blame analysis

Per-file authorship concentration. Bus factor = minimum authors whose lines
exceed 50%. Interesting for review tooling but `git blame` is expensive to run
at scale and the use case is narrower than the above.

## Not a fit for sutra

- **Refactor tools** (rename, move, rename_file) — sutra is read-only
  intelligence. The agent already has Edit/Write for modifications.
- **`project`** / build toolchain — orthogonal to code intelligence.
- **`wiki`** / auto-doc generation — demo-quality feature, low daily value.
- **`security`** — static vulnerability scanning is a separate domain better
  served by dedicated tools.
- **`semantic`** — embedding-based search. Sutra delegates this to smriti.
- **`hierarchy`** — type/trait inheritance trees. Narrow use case, big parser
  investment for two languages.
- **`stats`** — basic codebase metrics. Sutra's `map` and `health` tools
  already cover this.
- **37-language support** — sutra intentionally focuses on Rust + Dart. The
  manas ecosystem doesn't need Go/Python/etc. indexing.
