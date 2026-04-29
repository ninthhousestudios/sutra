# Review follow-ups

Items deferred from the GLM-5.1 and Qwen 3.6 third-round reviews (2026-04-29).
Items 1–10 were implemented in the review-fixes commit; this doc tracks what remains.

## Dart language support gaps

The complexity engine and dead-code detection have several Rust-specific assumptions
that produce incorrect results on Dart workspaces.

### Dart else-if chain handling (cognitive complexity)

`src/parser/complexity.rs` — The `else_clause` → `if_expression` flattening
(treating `else if` chains as flat rather than nested) is only implemented for
`lang == "rust"`. Dart's tree-sitter grammar has the same pattern: `else_clause`
containing a nested `if_statement`. Without the equivalent handling, every `else if`
in Dart accumulates nesting penalties that the Sonar model doesn't intend.

**Fix**: Add the equivalent Dart block in `walk_cognitive` that checks for
`child.kind() == "else_clause"` when `lang == "dart"`, with the same flattening
logic. Need to verify tree-sitter-dart's exact node naming first.

### Unreachable-file exclusion patterns

`src/db.rs` `find_unreachable_files` — The SQL excludes `lib.rs`, `main.rs`,
`mod.rs`, `src/bin/%`, `lib/%` — all Rust-specific. A Dart workspace would
have `lib/`, `bin/`, `web/`, `test/` as root directories. Files in those
directories would all be flagged as unreachable.

**Fix**: Make the exclusion list language-aware. Options:
1. Accept a `language` parameter and branch on it
2. Query the `files.language` column and auto-detect
3. Use a configurable exclusion list per workspace

### `loop_expression` scoring

`src/parser/complexity.rs:139` — Rust's infinite `loop {}` is scored the same
as `while`/`for`. The Sonar model arguably treats `loop` differently since there's
no branching condition. This is a debatable interpretation — document the choice
or adjust if real-world scores look inflated.

## Performance

### Missing index on `cognitive` column

`src/db.rs` `complexity_by_file` runs `GROUP BY file_id WHERE cognitive IS NOT NULL`
without an index. A partial index would help at scale:

```sql
CREATE INDEX idx_symbols_cognitive ON symbols(file_id) WHERE cognitive IS NOT NULL;
```

Add this to `0002_complexity.sql` when workspace sizes warrant it.

## Code quality

### `type_complexity` clippy warning on `find_dead_symbols`

`src/db.rs:542` — The return type `Vec<(String, String, String, i64, Option<String>)>`
triggers clippy's `type_complexity` lint. Extract a named struct (e.g. `DeadSymbolRow`)
when touching this code next.

### `is_logical_operator` fragility

`src/parser/complexity.rs` — Uses `child_by_field_name("operator")` which assumes
both tree-sitter-rust and tree-sitter-dart expose that field name on `binary_expression`.
If either grammar changes the field name, logical operators silently stop contributing
to complexity. Consider adding a fallback to `node.child(1)` or a test that validates
the field name for both grammars.

## Test coverage

The complexity module has 6 unit tests covering the core cases. Missing coverage:

- `while_expression` and `loop_expression` (Rust)
- `try_expression` (Rust) — increments cyclomatic but has no test
- `do_statement` (Dart)
- `conditional_expression` / ternary (Dart)
- `switch_statement` with `switch_case` (Dart)
- Closures / anonymous functions (nesting increment)
- `else if` chains (Rust and Dart)
- `break`/`continue` with labels
- Nested match arms with complex bodies (validates the nesting fix)

## Verdict thresholds

`src/tools/diff_impact.rs` — The thresholds (10/30 affected files, 15/25 cognitive,
20/50 blast radius) are hard-coded. A 30-file impact in a 500-file monorepo is
different from 30 files in a 50-file library. Consider making these configurable
per workspace via `sutra.toml`.
