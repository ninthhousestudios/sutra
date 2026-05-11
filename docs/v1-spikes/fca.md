# Spike: formal concept analysis for convention detection

Task: sutra/v1/3

## Question

Can FCA automatically detect meaningful codebase conventions and produce a
concept hierarchy that agents can check their work against?

## Verdict: viable with caveats

FCA extracts real structural conventions from codebases and the concept
lattice is a natural representation for convention hierarchies. Cross-codebase
comparison distinguishes universal Rust conventions from project-specific
ones. But the attribute vocabulary is the make-or-break factor — the
framework is only as useful as the features you feed it.

## Results

### Lattice extraction (2 codebases)

| Codebase | Files | Symbols | File concepts | Sym concepts | Time (file) | Time (sym) |
|----------|-------|---------|---------------|--------------|-------------|------------|
| sutra | 72 | 651 | 128 | 532 | 3.3ms | 64ms |
| chitta | 32 | 300 | 114 | 370 | 1.9ms | 31ms |

Both well under 100ms. NextClosure scales linearly with concept count.

### Implication quality

**File-level**: 40 exact implications (sutra), 31 (chitta), 15 shared.
Most high-support implications are structural truisms (`ext:rs` ↔ `fan_in:zero`
because import resolution wasn't populated). The interesting ones are
project-specific: `has_pub_struct ∧ size:small → dir:src/tools` (sutra),
`dir:src ∧ has_docs → has_impl ∧ has_pub_struct ∧ has_pub_fn` (chitta).

**Symbol-level**: 63 exact (sutra), 50 (chitta), 24 shared. Much richer.
Genuine conventions surface:

| Implication | Support | Shared? | Convention? |
|---|---|---|---|
| `has_sig` ↔ `naming:snake_case` | 501/195 | yes | Rust naming rules |
| `kind:struct` → `naming:CamelCase` | 84/76 | yes | Rust naming rules |
| `kind:method` ↔ `is_method` + `has_sig` + `naming:snake_case` | 151/64 | yes | structural |
| `takes_self_ref` → `kind:method` + `is_method` | 113/40 | yes | structural |
| `in:src/bin` → `vis:private` | 143/11 | yes | binary convention |
| `in:src/tools` → `kind:function` + `has_sig` | 51/29 | yes | module convention |
| `kind:enum` → `naming:CamelCase` | 18/4 | yes | Rust naming rules |

The shared implications split cleanly into:
1. **Language rules** (naming conventions) — these are enforced by rustc/clippy anyway
2. **Project conventions** (module layout, visibility patterns) — these are useful

### Approximate implications (convention candidates)

High-confidence approximate implications are where FCA shines for convention
detection — they represent patterns that *almost always* hold, with specific
violations:

| Implication | Confidence | Violations |
|---|---|---|
| `dir:tests` → `has_tests` | 0.94 | `tests/provenance-test.rs` (no #[test] fns) |
| `dir:src/tools` → `has_pub_struct` | 0.87 | 3 tool files without pub structs |
| `takes_self_ref` → `in:src` | 0.96 | enum methods in lib root |
| `has_doc` → `has_sig` | 0.95 | documented structs/enums (no signature) |
| `kind:method` → `in:src` | 0.95 | enum methods in lib root |

The violations are specific and actionable — an agent could flag
`tests/provenance-test.rs` as "test file with no test functions" during review.

### Bootstrap precision

| Context | Full implications | Half-A | Half-B | Stable | Precision |
|---|---|---|---|---|---|
| sutra (files) | 40 | 29 | 28 | 15 | 0.53 |
| sutra (symbols) | 63 | 61 | 66 | 34 | 0.54 |

~54% of implications are stable across random halves. This is moderate —
roughly half the implications are genuine conventions, half are artifacts
of the specific object distribution. The stable ones are the actionable
subset. Meets the >50% precision criterion (barely).

### Violation detection

The `tests/provenance-test.rs` violation (`dir:tests` → `has_tests`, conf=0.94)
is a real catch — the file exists in tests/ but has no `#[test]` functions.
The `DbCache`/`WsConfig` violations (test-file structs breaking `in:tests` →
`kind:function`) are also valid (test helper structs, not test functions).

Recall is harder to measure without planted violations. The approximate
implication approach catches every object that deviates from a high-confidence
pattern — by construction, recall = 1.0 for any implication above the
confidence threshold. The question is whether the *right* implications surface.
At min_support=3 and confidence >= 0.9, we get 1 file-level and 11 symbol-level
violation-producing implications — a useful, non-overwhelming set.

### Cross-codebase comparison

**File-level**: 15/40 sutra implications shared with chitta (37%). The shared
set is mostly structural (`ext:rs` ↔ `fan_in:zero`, `has_pub_struct` →
`has_pub_fn`). Project-specific ones capture real differences: sutra has
`dir:tests ∧ size:med → has_tests ∧ complexity:low`, chitta has
`dir:src ∧ has_docs → has_impl ∧ has_pub_struct`.

**Symbol-level**: 24/63 shared (38%). The shared implications are genuine
cross-project conventions: `in:src/tools` → `kind:function` (both projects
organize tools as standalone functions), `in:src/bin` → `vis:private` (binary
modules don't export).

### Incremental feasibility

| Context | Concepts | Attributes | AddExtent ops/update |
|---|---|---|---|
| sutra (files) | 128 | 18 | ~2,304 |
| sutra (symbols) | 532 | 27 | ~14,364 |
| chitta (symbols) | 370 | 27 | ~9,990 |

All comfortably under 1M operations. AddExtent (incremental concept insertion)
is feasible for live updates as files are reparsed. Even a 10x larger codebase
would stay under 150K operations — microsecond territory.

## Assessment

**What works:**
- NextClosure is efficient and correct — concept counts match theory
- Symbol-level context produces richer, more useful implications than file-level
- Cross-codebase comparison cleanly separates universal vs. project-specific
- Approximate implications are the sweet spot for convention detection
- Violation detection is specific and actionable
- Incremental update is feasible at any reasonable codebase size

**What doesn't work (yet):**
- Import graph was empty (sutra's import edges aren't populated for this workspace
  configuration) — this eliminates a whole class of dependency-based conventions
- File-level context is too coarse with the current attribute vocabulary;
  `fan_in:zero` for all files makes many implications trivially true
- Bootstrap precision of ~54% means half the implications are noise; need better
  attribute selection or higher support thresholds
- No way to distinguish "Rust language rule" from "project convention" automatically
  (though cross-codebase diff helps)

**What needs work for production:**
- Richer attribute vocabulary: derive macros, error handling patterns, trait impls,
  attribute annotations (#[serde(...)] etc.), return type specifics beyond
  Result/Option
- Import-based attributes once sutra's import resolution is reliable
- A "convention registry" that persists extracted conventions and tracks them
  across parses — the lattice is the index, stored conventions are the product
- Filtering heuristic to auto-exclude language-rule implications (anything that
  holds across N>3 diverse codebases is probably a language rule, not a convention)

## Recommendation

**Use FCA for convention detection** in sutra's agent-facing API. The natural
workflow:

1. On workspace registration, build symbol-level formal context from parse DB
2. Extract approximate implications above (support >= 5, confidence >= 0.85)
3. Cross-check against a "baseline" set from diverse codebases to filter language rules
4. Expose remaining implications as "project conventions" via MCP tool
5. On file change, incrementally update context (AddExtent) and re-check
   the changed file against active conventions
6. Report violations in `sutra_pr_risk` or a new `sutra_conventions` tool

**Start with symbol-level only.** File-level needs better attributes (especially
import resolution) before it's useful. Symbol-level already produces actionable
results.

**Don't build a full lattice browser.** The lattice is an intermediate structure;
agents care about implications and violations, not concept navigation. Store the
implication set, not the lattice.

## LOC

| Component | Lines |
|---|---|
| BitSet | 85 |
| FormalContext + derivation operators | 55 |
| NextClosure + concept enumeration | 45 |
| Implication extraction (exact + approximate) | 90 |
| Attribute extraction (file + symbol) | 300 |
| Experiments + evaluation | 280 |
| Main + data loading | 100 |
| **Total** | **~955** |

Production would be ~400 LOC (context builder + NextClosure + implication
extraction + AddExtent), since evaluation/experiment code drops away.
