# Code Review: Complexity Metrics + Code Health Features

**Reviewer**: GLM-5.1
**Date**: 2026-04-29
**Commits reviewed** (oldest → newest):

| SHA | Subject |
|---|---|
| `8857a4c` | feat: per-symbol complexity metrics (cyclomatic + cognitive) |
| `4d10b09` | feat: dead code detection, hotspots, file health scores, audit verdicts |

**Scope**: 20 files changed, +860 / -16 lines.

---

## Summary

The first commit adds cyclomatic and cognitive complexity computation during tree-sitter parsing, persisting results into the SQLite database. The second commit builds four new tools on that foundation: dead-symbol detection, git-churn-based hotspots, per-file health scores, and diff-impact audit verdicts. Together these transform Sutra from a structural index into a maintainability analysis tool.

The complexity metrics implementation is solid — the Sonar-style cognitive model is correctly applied, and the unit tests cover the most important cases. The downstream tools are functional and their scoring formulas are reasonable first approximations.

The review below identifies issues ordered by severity. Clippy reports 9 warnings across these two commits, 4 of which are `collapsible_if` suggestions directly from the new code.

---

## Critical Issues

### 1. Migration strategy is inconsistent and will break on fresh DBs

**Files**: `migrations/0001_initial.sql`, `migrations/0002_complexity.sql`, `src/db.rs:147-161`

The `0001_initial.sql` schema was retroactively edited to include `cyclomatic` and `cognitive` columns inline, AND a separate `0002_complexity.sql` migration was added that runs `ALTER TABLE ADD COLUMN` for the same two columns. The migration runner now:

1. Creates the table *with* `cyclomatic`/`cognitive` via `0001_initial.sql` (using `CREATE TABLE IF NOT EXISTS`)
2. Immediately runs `ALTER TABLE ADD COLUMN cyclomatic` and `ALTER TABLE ADD COLUMN cognitive` via `0002_complexity.sql` — which will fail with "duplicate column" since they already exist from step 1

The runner swallows `duplicate column` errors (`db.rs:155-159`), so this doesn't crash, but it means:
- Every startup logs nothing on fresh DBs (silent double-add) 
- The 0002 migration is useless on fresh DBs and only meaningful on DBs created before this commit
- The approach is fragile — if 0002 ever adds logic beyond ALTER COLUMN, the swallowing of errors could mask real problems

**Recommendation**: Remove `cyclomatic`/`cognitive` from `0001_initial.sql` and rely solely on `0002_complexity.sql` for migration. Or, invert the approach: keep 0001 as the single source of truth and drop 0002 entirely (accepting that pre-existing DBs must be re-created). The current split creates a confusing maintenance burden.

### 2. `find_dead_symbols` SQL uses `format!` to inject visibility filter — SQL injection vector

**File**: `src/db.rs:543-560`

The `include_pub` parameter controls whether a raw SQL fragment is spliced into the query via `format!`:

```rust
if include_pub {
    ""
} else {
    "AND (s.visibility IS NULL OR s.visibility NOT IN ('pub','public'))"
}
```

While this specific case is safe (the injected string is a constant, not user input), the pattern of `format!`-based SQL construction is a code smell that will become a real injection risk if anyone extends it with user-supplied values. The kind filter also hard-codes a literal list.

**Recommendation**: Use parameterized queries. Even for the visibility check, restructure as `AND (?1 = 1 OR s.visibility IS NULL OR s.visibility NOT IN ('pub','public'))` with `include_pub` bound as a boolean parameter.

---

## Major Issues

### 3. `extract_impl_symbol` sets `cyclomatic: None, cognitive: None` — but `impl` blocks can contain methods with bodies

**File**: `src/parser/rust.rs:249-250`

The `extract_impl_symbol` function hard-codes both complexity values to `None`. While `impl` blocks are tagged as `SymbolKind::Impl` (which is correct — they aren't functions), the downstream `complexity_by_file` query (`db.rs:522-534`) averages over ALL symbols in a file with `cognitive IS NOT NULL`. Since `impl` symbols return `None`, they're correctly excluded from averages. However, the `map` tool's `complexity_boost` uses `max_cognitive` which could be misleading if a file's highest-complexity item is a method inside an `impl` block — the method itself gets scored, but the `impl` entry is dead weight in the symbol table.

This is a design choice rather than a bug, but it's worth documenting that `impl` symbols intentionally carry no complexity.

### 4. `git_churn` counts file-name occurrences per commit, not distinct commits per file

**File**: `src/git.rs:73-98`

`git log --format= --name-only` emits one line per file per commit, but if a commit modifies the same file name in different paths (unlikely but possible with renames), or if the same file appears in merge commits, the count is inflated. More importantly, `--name-only` without `--no-renames` or `--diff-filter` will count rename source and destination as two entries.

**Recommendation**: Use `git log --format= --name-only --no-renames --diff-filter=ACDMRT` or `git log --numstat` for more accurate churn counting. Also consider `--follow` semantics.

### 5. Hotspots scoring is multiplicative — files with zero complexity are invisible

**File**: `src/tools/hotspots.rs:46`

```rust
let score = (churn_norm * blast_norm * complexity_norm * 1000.0) as i64;
```

If a file has `max_cog = 0` (no functions, e.g. a constants file or pure data module), `complexity_norm = 0.0`, and the hotspot score is always 0 regardless of churn or blast radius. Files with zero complexity but high churn and high blast radius are exactly the kind of file that should appear on a risk radar — they're volatile and widely depended upon, even if mechanically simple.

**Recommendation**: Add a floor of 0.1 to `complexity_norm`, or switch to an additive model (e.g. `0.4*churn_norm + 0.3*blast_norm + 0.3*complexity_norm`). Multiplicative scoring creates blind spots.

### 6. `diff_impact` verdict double-counts: `find_symbols_by_file` called twice for changed files

**File**: `src/tools/diff_impact.rs:25-33` and `:61-74`

The original code at lines 25-33 already calls `db.find_symbols_by_file(file.id)` for each changed path (to build `changed_files` JSON and collect `all_symbol_ids`). The new verdict code at lines 61-74 calls it *again* for the same set of paths to scan cognitive scores. This is an N+1-duplicated-query pattern.

**Recommendation**: Reuse the `syms` vector from the first loop. Collect `all_symbol_ids`, build `changed_files` JSON, and track `max_cognitive` all in a single pass.

### 7. `file_health` penalty model can produce negative health before `max(0.0)` clamp

**File**: `src/tools/file_health.rs:33-41`

Maximum possible total penalty: `25 + 25 + 15 + 20 + 15 = 100`. This means a file at the ceiling of every penalty dimension scores exactly 0. That's the intended floor. But if any dimension's cap is raised even slightly in the future, the model silently allows negative health values before the `.max(0.0)` clamp. The current code is correct, but the formula is fragile — there's no invariant enforcement that total cap = 100.

**Recommendation**: Normalize total_penalty to a 0–100 range explicitly: `let health = (100.0 * (1.0 - total_penalty / MAX_PENALTY)).max(0.0)`. Or add a const `MAX_PENALTY = 100.0` and document the invariant.

---

## Minor Issues

### 8. Clippy: collapsible `if` chains

**Files**: `src/parser/complexity.rs:158-163`, `src/tools/diff_impact.rs:66-71`

Clippy emits `collapsible_if` warnings for nested `if let` / `if` patterns. These are cosmetic but should be fixed to keep the lint baseline clean:

```rust
// complexity.rs:158 — use:
if let Some(op) = node.child_by_field_name("operator")
    && let Ok(text) = op.utf8_text(src)
{
    return text == "&&" || text == "||";
}

// diff_impact.rs:66 — use:
if let Some(c) = s.cognitive
    && max_cognitive.is_none_or(|prev| c > prev)
{
    max_cognitive = Some(c);
    max_cognitive_symbol = Some(s.qualified_name.clone());
}
```

### 9. Dart else-if handling missing from cognitive complexity

**File**: `src/parser/complexity.rs:115-128`

The `else_clause` → `if_expression` flattening (else-if chain without extra nesting) is only implemented for `lang == "rust"`. Dart `if_statement` nodes have the same else-if chain pattern but it's not handled — every nested `else if` in Dart will accumulate nesting penalties that shouldn't apply per the Sonar model.

**Recommendation**: Add the equivalent Dart handling for `else_clause` / `else_if_clause` (depending on tree-sitter-dart node naming).

### 10. Cognitive complexity for `match_expression` increments nesting — but match arms are flat

**File**: `src/parser/complexity.rs:139-140`

In `classify_cognitive`, Rust `match_expression` returns `(true, true)` — it's both a flow break AND increments nesting. The Sonar cognitive model treats `match`/`switch` as a single +1 flow break but does NOT add nesting for the arms (they're parallel, not nested). Currently, anything inside a match arm body (e.g. an `if` inside an arm) gets an extra +1 nesting penalty that the Sonar spec wouldn't apply.

**Recommendation**: Change `match_expression` to `(true, false)` — flow break but not a nesting increment. Arms are parallel paths, not nested scope. Same for Dart `switch_statement` at line 148.

### 11. `dead_symbol_ratio_by_file` SQL has inconsistent kind filters

**File**: `src/db.rs:577-589`

The `dead_symbol_ratio_by_file` query filters with:
```sql
WHERE s.kind IN ('function','method','struct','enum','trait',
                 'type_alias','class','mixin','const','static')
  AND s.kind != 'impl'
```

The `s.kind != 'impl'` filter is redundant — `'impl'` isn't in the `IN` list. This is harmless but suggests the two queries were written at different times and the redundancy could confuse future maintainers.

### 12. `find_unreachable_files` hard-codes Rust entry-point patterns

**File**: `src/db.rs:604-619`

The SQL excludes `lib.rs`, `main.rs`, `mod.rs`, `src/bin/%`, `lib/%` — all Rust-specific. A Dart workspace would have `lib/*.dart`, `bin/*.dart`, `test/*.dart` as entry points. These would all be flagged as unreachable.

**Recommendation**: Make the root-file exclusion list language-aware, or at minimum add Dart patterns (`lib/`, `bin/`, `web/`, `test/`).

### 13. `complexity_by_file` query uses `MAX(cognitive)` and `AVG(cognitive)` but no index on `cognitive`

**File**: `src/db.rs:522-534`

There's no index on `symbols.cognitive`. For large workspaces, the `GROUP BY file_id WHERE cognitive IS NOT NULL` query will require a full table scan. The existing `idx_symbols_file` index helps with the GROUP BY, but the filter on `cognitive IS NOT NULL` prevents index-only coverage.

**Recommendation**: Consider adding a partial index: `CREATE INDEX idx_symbols_cognitive ON symbols(file_id) WHERE cognitive IS NOT NULL`. This is a minor performance concern for now but will matter at scale.

### 14. Test helpers in test files have inconsistent indentation for new fields

**Files**: `tests/calls-test.rs:15-16`, `tests/db-test.rs:20-21`, `tests/impact_test.rs:14-15`, etc.

The `cyclomatic: None` and `cognitive: None` additions are indented with extra spacing that doesn't match the surrounding struct fields. For example:

```rust
        parent_symbol_id: None, docstring: None,
            cyclomatic: None,
            cognitive: None,
```

The extra indentation makes it look like these are nested rather than sibling fields. Cosmetic, but it signals a rushed addition.

### 15. `sutra_dead` handler does in-memory filtering that should be in SQL

**File**: `src/tools/dead.rs:16-18`

The `path_prefix` filter is applied as a Rust `.filter()` over the full result set from `find_dead_symbols`. The SQL query already supports parameterization — a `WHERE f.path LIKE ?1` clause would push the filter into the database and avoid fetching dead symbols from unrelated paths.

Similarly for `unreachable_files` at line 32-35.

---

## Observations (not issues)

### On the scoring models

The health-score formula in `file_health.rs` is a reasonable first cut. The caps (25, 25, 15, 20, 15) sum to 100 which is clean. The key insight — that blast radius, complexity, fan-in, dead symbols, and PageRank are all negative signals for maintainability — is sound. The relative weight given to each factor is subjective and will likely need tuning based on real-world feedback.

The hotspots multiplicative scoring (churn × blast × complexity) has the mathematical property that a file must be high on ALL three dimensions to rank high. This is a deliberate design choice ("riskiest" = simultaneously volatile, depended-upon, and complex). It's valid, but it will systematically miss "high churn + high blast + zero complexity" files that are still high-risk (see issue #5).

### On the complexity test coverage

The 6 unit tests in `complexity.rs` cover: simple function, single if, nested if, logical operators, match arms, and for-loop. Missing coverage:
- `while` and `loop` expressions (Rust)
- `try_expression` (Rust)
- `do_statement` (Dart)
- `conditional_expression` / ternary (Dart)
- `switch_statement` with `switch_case` (Dart)
- Closures / anonymous functions (nesting increment)
- `else if` chains in both languages
- `break`/`continue` with labels

The cyclomatic and cognitive models are subtle enough that edge cases in tree-sitter AST structure could easily produce off-by-one errors. Expanding test coverage would be a high-value investment.

### On the verdict thresholds

The diff-impact verdict thresholds (10/30 files, 15/25 cognitive, 20/50 blast) are reasonable defaults but should probably be configurable per workspace. A 30-file change in a 500-file monorepo is different from a 30-file change in a 50-file library.

---

## Summary Table

| # | Severity | Issue | File(s) |
|---|----------|-------|---------|
| 1 | Critical | Migration 0001/0002 double-define columns | `migrations/0001_initial.sql`, `migrations/0002_complexity.sql` |
| 2 | Critical | `format!` SQL construction in `find_dead_symbols` | `src/db.rs:543-560` |
| 3 | Major | `impl` symbols carry None complexity — by design, but undocumented | `src/parser/rust.rs:249-250` |
| 4 | Major | `git_churn` may over-count due to renames | `src/git.rs:73-98` |
| 5 | Major | Hotspots multiplicative scoring hides zero-complexity files | `src/tools/hotspots.rs:46` |
| 6 | Major | `diff_impact` calls `find_symbols_by_file` twice per changed file | `src/tools/diff_impact.rs:25-33,61-74` |
| 7 | Major | File health penalty model has no invariant enforcement | `src/tools/file_health.rs:33-41` |
| 8 | Minor | Clippy collapsible_if warnings | `src/parser/complexity.rs:158`, `src/tools/diff_impact.rs:66` |
| 9 | Minor | Dart else-if chain handling missing from cognitive | `src/parser/complexity.rs:115-128` |
| 10 | Minor | `match_expression` incorrectly increments nesting in cognitive | `src/parser/complexity.rs:139-140` |
| 11 | Minor | Redundant `kind != 'impl'` filter in dead_ratio SQL | `src/db.rs:577-589` |
| 12 | Minor | Unreachable-file exclusion hard-codes Rust entry points | `src/db.rs:604-619` |
| 13 | Minor | No index on `cognitive` column for aggregation queries | `src/db.rs:522-534` |
| 14 | Minor | Inconsistent indentation in test struct literals | `tests/*.rs` |
| 15 | Minor | `path_prefix` filtering in Rust instead of SQL | `src/tools/dead.rs:16-18` |

**Verdict**: Two critical issues to address before release (migration consistency, SQL construction pattern). Five major issues that affect correctness or observability. Eight minor issues for follow-up. The overall architecture is sound — these are refinements, not rewrites.
