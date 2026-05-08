# Code Review: Complexity Metrics + Code Health Features

**Reviewer**: Qwen 3.6 Plus
**Date**: 2026-04-29
**Commits reviewed** (oldest → newest):

| SHA | Subject |
|---|---|
| `8857a4c` | feat: per-symbol complexity metrics (cyclomatic + cognitive) |
| `4d10b09` | feat: dead code detection, hotspots, file health scores, audit verdicts |

**Scope**: 20 files changed, +860 / -16 lines across 15 files (commit 1) and 8 files (commit 2).

---

## Summary

These two commits transform Sutra from a structural code index into a maintainability analysis platform. The first commit builds the complexity metrics engine (cyclomatic + cognitive) during tree-sitter parsing for Rust and Dart. The second commit adds four analysis tools: dead code detection, git-churn hotspots, per-file health scoring, and diff-impact audit verdicts.

The complexity engine is the strongest part — the Sonar-style cognitive model is a good choice, and the 6 unit tests validate the core logic. The downstream tools are functional but have several correctness and design issues.

Clippy reports 9 warnings (8 auto-fixable), all from these commits. All 6 complexity unit tests pass.

---

## Critical Issues

### 1. `checked_sub(1).is_some()` is a confusing guard that does nothing useful

**File**: `src/parser/complexity.rs:26-28`, `:48-50`

```rust
"match_expression" => {
    if count.checked_sub(1).is_some() {
        *count -= 1;
    }
}
```

`count` is initialized to `1` and only ever incremented before reaching a `match_expression` node (each `match_arm` adds +1). The `checked_sub` guard is meant to prevent underflow, but `count` can never be 0 at this point — there must be at least one arm, which already incremented it to 2+. The guard is dead code that obscures intent.

**Recommendation**: Replace with `*count -= 1;` directly, or if you want the guard for defensive reasons, use `*count = count.saturating_sub(1);` which is self-documenting.

### 2. Migration 0001/0002 double-defines columns with silent error swallowing

**Files**: `migrations/0001_initial.sql`, `migrations/0002_complexity.sql`, `src/db.rs:147-161`

`0001_initial.sql` was retroactively edited to include `cyclomatic` and `cognitive` columns inline. `0002_complexity.sql` then runs `ALTER TABLE ADD COLUMN` for the same columns. On a fresh database, the ALTER fails with "duplicate column" and the error is silently swallowed (`db.rs:155`). This means:

- Fresh DBs execute a no-op migration that produces a swallowed error
- The error-swallowing pattern (`if !msg.contains("duplicate column")`) is fragile — it will mask any future migration errors that happen to contain that substring
- The `eprintln!` on non-duplicate errors goes to stderr with no structured logging

**Recommendation**: Remove `cyclomatic`/`cognitive` from `0001_initial.sql` entirely. Let `0002_complexity.sql` be the sole source of truth for these columns. Or use a proper migration tracking table instead of `CREATE TABLE IF NOT EXISTS` + error swallowing.

---

## Major Issues

### 3. Cognitive complexity: `match_expression`/`switch_statement` incorrectly increment nesting

**File**: `src/parser/complexity.rs:139-140`, `:148`

```rust
"match_expression" => (true, true),  // flow_break=true, nesting=true
```

In the Sonar cognitive complexity model, `match`/`switch` is a single +1 flow break. The arms are **parallel** branches, not nested scope. An `if` inside a match arm should NOT receive an extra nesting penalty from the match wrapper. Currently, `increments_nesting = true` causes all descendants of a match arm to get +1 nesting they shouldn't have.

**Example**: `match x { A => { if cond { 1 } } }` — the `if` gets scored as `1 + 1(nesting from match) = 2`, but per Sonar it should be `1 + 0 = 1`.

**Recommendation**: Change both `match_expression` (Rust) and `switch_statement` (Dart) to `(true, false)` — flow break without nesting increment.

### 4. Hotspots multiplicative scoring creates blind spots

**File**: `src/tools/hotspots.rs:46`

```rust
let score = (churn_norm * blast_norm * complexity_norm * 1000.0) as i64;
```

A file with `max_cog = 0` (e.g., a constants module, data schema, or pure configuration file) gets `complexity_norm = 0.0`, making its score **always zero** regardless of churn or blast radius. A file that changes in every commit and is imported by 50 other files but has no functions will never appear in hotspots — even though it's arguably one of the riskiest files in the codebase.

**Recommendation**: Add a floor: `let complexity_norm = (max_cog as f64 / max_complexity).max(0.1);`. Or switch to a weighted additive model: `0.4*churn + 0.3*blast + 0.3*complexity`.

### 5. `diff_impact` calls `find_symbols_by_file` twice per changed file

**File**: `src/tools/diff_impact.rs:25-33` and `:61-74`

The first loop (lines 25-33) calls `db.find_symbols_by_file(file.id)` to build the `changed_files` JSON and collect `all_symbol_ids`. The verdict computation loop (lines 61-74) calls it **again** for the same set of paths to find max cognitive complexity. This is a duplicated N-query pattern.

**Recommendation**: Collect symbols in the first loop and reuse them. Store `Vec<SymbolRow>` alongside the JSON, or track `max_cognitive` during the initial pass.

### 6. `find_unreachable_files` hard-codes Rust entry-point patterns

**File**: `src/db.rs:604-619`

```sql
AND path NOT LIKE '%/lib.rs'
AND path NOT LIKE '%/main.rs'
AND path NOT LIKE '%/mod.rs'
AND path NOT LIKE 'src/bin/%'
AND path NOT LIKE 'lib/%'
```

These patterns are entirely Rust-specific. A Dart workspace would flag `lib/src/foo.dart`, `bin/server.dart`, and `web/main.dart` as unreachable. The `lib/%` pattern is especially problematic — it would match Dart's `lib/` directory (which is the standard source root for Dart packages).

**Recommendation**: Make the exclusion list language-aware. Either pass the workspace language as a parameter, or use a more generic heuristic (e.g., exclude files that are direct children of known root directories).

### 7. `git_churn` doesn't handle renames and may over-count

**File**: `src/git.rs:73-98`

`git log --format= --name-only` without `--no-renames` will emit both the rename source and destination as separate file entries, inflating churn counts. A file that was renamed in 10 commits would count as 20 touches.

**Recommendation**: Add `--no-renames` to the git log arguments, or use `--diff-filter=ACDMRT` to exclude rename sources.

---

## Minor Issues

### 8. Clippy: 9 warnings (8 auto-fixable)

All from these two commits:

| Warning | Location | Fix |
|---|---|---|
| `collapsible_match` (4x) | `complexity.rs:26,31,48,56` | Use match guards |
| `collapsible_if` (3x) | `complexity.rs:94,99,159` | Use `&&` chaining |
| `collapsible_if` (1x) | `diff_impact.rs:66` | Use `&&` chaining |
| `type_complexity` (1x) | `db.rs:541` | Extract type alias for the 5-tuple return |

Run `cargo clippy --fix --lib -p sutra` to auto-apply 8 of 9.

### 9. Dart else-if chain handling missing from cognitive complexity

**File**: `src/parser/complexity.rs:115-128`

The `else_clause` → `if_expression` flattening (treating `else if` chains as flat rather than nested) is only implemented for `lang == "rust"`. Dart has the same pattern — `else if` chains in tree-sitter-dart also produce nested `if_statement` nodes inside `else_clause` — but gets no special handling. Every nested `else if` in Dart will accumulate nesting penalties incorrectly.

**Recommendation**: Add equivalent handling for Dart. Check tree-sitter-dart's node naming (likely `else_clause` containing `if_statement`).

### 10. `dead_symbol_ratio_by_file` has redundant `kind != 'impl'` filter

**File**: `src/db.rs:577-589`

```sql
WHERE s.kind IN ('function','method','struct','enum','trait',
                 'type_alias','class','mixin','const','static')
  AND s.kind != 'impl'
```

`'impl'` is not in the `IN` list, making the `!= 'impl'` clause a no-op. Harmless but confusing.

### 11. `sutra_dead` filters in Rust instead of SQL

**File**: `src/tools/dead.rs:16-18`, `:32-35`

The `path_prefix` filter is applied as `.filter()` over the full result set. For large workspaces with many dead symbols, this fetches everything from SQLite then discards most of it in memory.

**Recommendation**: Add `WHERE f.path LIKE ?1` to the SQL queries in `find_dead_symbols` and `find_unreachable_files`.

### 12. No index on `cognitive` column for aggregation queries

**File**: `src/db.rs:522-534`

`complexity_by_file` runs `SELECT file_id, MAX(cognitive), AVG(cognitive) FROM symbols WHERE cognitive IS NOT NULL GROUP BY file_id`. Without an index on `cognitive`, this is a full table scan.

**Recommendation**: Add `CREATE INDEX idx_symbols_cognitive ON symbols(file_id) WHERE cognitive IS NOT NULL` to `0002_complexity.sql`.

### 13. `file_health` penalty caps sum to exactly 100 — no invariant enforcement

**File**: `src/tools/file_health.rs:33-41`

```
blast: min(25) + complexity: min(25) + fan_in: min(15) + dead: max(20) + pagerank: min(15) = 100
```

The model is designed so max penalty = 100, giving a floor of 0. If anyone raises a cap in the future, negative health values appear before the `.max(0.0)` clamp.

**Recommendation**: Add `const MAX_PENALTY: f64 = 100.0;` and normalize: `let health = (100.0 * (1.0 - total_penalty / MAX_PENALTY)).max(0.0) as i64;`.

### 14. `is_logical_operator` uses `child_by_field_name("operator")` — fragile to tree-sitter grammar changes

**File**: `src/parser/complexity.rs:158-164`

This assumes both `tree-sitter-rust` and `tree-sitter-dart` expose a field named `"operator"` on `binary_expression` nodes. If either grammar changes the field name, the function silently returns `false` and logical operators stop contributing to complexity.

**Recommendation**: Add a fallback that checks `node.child(1)` (the typical position of the operator token) if `child_by_field_name` returns `None`. Or add a test that validates the field name exists for both grammars.

### 15. Cognitive complexity: `loop_expression` (Rust infinite loop) treated same as `for`/`while`

**File**: `src/parser/complexity.rs:139`

```rust
"while_expression" | "for_expression" | "loop_expression" => (true, true),
```

Per the Sonar cognitive model, an infinite `loop {}` (no condition) should be scored differently from `while`/`for` (which have conditions). Sonar gives `loop` a +1 flow break but doesn't penalize it as heavily as conditional loops since there's no branching decision. This is a debatable interpretation, but worth noting.

### 16. Test coverage gaps in complexity module

The 6 unit tests cover basic cases. Missing:
- `while_expression`, `loop_expression` (Rust)
- `try_expression` (Rust) — increments cyclomatic but has no test
- `do_statement` (Dart)
- `conditional_expression` / ternary (Dart)
- `switch_statement` with `switch_case` (Dart)
- Closures / anonymous functions (nesting increment)
- `else if` chains (Rust and Dart)
- `break`/`continue` with labels
- Nested match arms with complex bodies

Given that cognitive complexity is subtle and off-by-one errors are easy, expanding test coverage would be high-value.

---

## Observations

### On the `checked_sub` pattern across both match/switch corrections

The `if count.checked_sub(1).is_some()` pattern appears for both Rust `match_expression` and Dart `switch_statement`. The intent is clearly "subtract 1 to correct for over-counting the first arm/case." But the `checked_sub` approach is unusual — a simple `*count -= 1;` would suffice given the invariant that at least one arm always exists. If the concern is robustness against malformed ASTs, `saturating_sub` is clearer.

### On the verdict thresholds in `diff_impact`

The thresholds (10/30 affected files, 15/25 cognitive, 20/50 blast radius) are reasonable defaults but should be workspace-configurable. A 30-file impact in a 500-file monorepo is qualitatively different from 30 files in a 50-file library.

### On the health score design

The formula is a solid first pass. The five penalty dimensions (blast radius, complexity, fan-in, dead symbols, PageRank) are well-chosen and the caps are proportional to perceived risk. The decision to use average (not max) cognitive complexity for health scoring is reasonable — it measures sustained complexity rather than worst-case.

---

## Summary Table

| # | Severity | Issue | File(s) |
|---|----------|-------|---------|
| 1 | Critical | `checked_sub(1).is_some()` is dead-code guard | `src/parser/complexity.rs:26,48` |
| 2 | Critical | Migration 0001/0002 double-defines columns with error swallowing | `migrations/*`, `src/db.rs:147-161` |
| 3 | Major | `match`/`switch` incorrectly increments nesting in cognitive model | `src/parser/complexity.rs:139,148` |
| 4 | Major | Hotspots multiplicative scoring hides zero-complexity files | `src/tools/hotspots.rs:46` |
| 5 | Major | `diff_impact` calls `find_symbols_by_file` twice per changed file | `src/tools/diff_impact.rs:25-33,61-74` |
| 6 | Major | `find_unreachable_files` hard-codes Rust-only entry points | `src/db.rs:604-619` |
| 7 | Major | `git_churn` doesn't handle renames | `src/git.rs:73-98` |
| 8 | Minor | 9 clippy warnings (8 auto-fixable) | Multiple |
| 9 | Minor | Dart else-if chain handling missing | `src/parser/complexity.rs:115-128` |
| 10 | Minor | Redundant `kind != 'impl'` in dead_ratio SQL | `src/db.rs:577-589` |
| 11 | Minor | `path_prefix` filtering in Rust instead of SQL | `src/tools/dead.rs:16-18` |
| 12 | Minor | No index on `cognitive` column | `src/db.rs:522-534` |
| 13 | Minor | Health penalty caps lack invariant enforcement | `src/tools/file_health.rs:33-41` |
| 14 | Minor | `is_logical_operator` fragile to grammar changes | `src/parser/complexity.rs:158-164` |
| 15 | Minor | `loop_expression` scored same as conditional loops | `src/parser/complexity.rs:139` |
| 16 | Minor | Test coverage gaps in complexity module | `src/parser/complexity.rs:192-249` |

**Verdict**: 2 critical issues (confusing dead-code guard, fragile migration strategy). 5 major issues affecting correctness (cognitive nesting bug, hotspot blind spots, duplicated queries, language-specific SQL, rename handling). 9 minor issues for follow-up. The architecture is sound — the complexity engine is well-designed and the downstream tools are a good foundation. Most issues are refinements rather than fundamental problems.
