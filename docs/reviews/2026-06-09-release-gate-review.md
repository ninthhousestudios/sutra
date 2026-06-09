# Code Review: sutra @ all of HEAD (8a70710)

**Date:** 2026-06-09
**Scope:** all of HEAD, commit 8a70710, release gate (pre-tag, v0.7.2)
**Verdict:** hold for fixes

## Verification

- Build: pass (clean)
- Tests: pass — 738 tests, 0 fail, 30 binaries. But see finding `cochange-tests-vacuous`: green tests coexist with a fully broken tool.
- Lint: 126 clippy warnings (lib: 81)
- Format: fail — 8,465-line `cargo fmt --check` diff; tree is not rustfmt-clean

## Design

The layered architecture (Layer 0 structural facts → components → conventions → constraints → health → similarity) is sound, and the ADRs hold up well: the ephemeral/durable partition (ADR-0002) is exactly right — it's what makes the index-corruption finding below recoverable instead of catastrophic — and the layered adapter traits (ADR-0003) are correctly reflected in the parser registry. The problem is that every layer rests on Layer-0 ref resolution, and Layer 0 has a systematic precision hole: **the Rust parser never extracts method-call names**. `walk_refs_recursive` (src/parser/rust.rs:603) collects only `identifier` and `type_identifier` nodes; tree-sitter-rust represents `db.upsert_file(...)` as a `field_identifier`, which appears nowhere in src/parser/. Additionally, receiver identifiers (`SKIP_DIRS.contains(...)`) are classified `FieldAccess` and deliberately skipped by the resolver (src/resolver.rs:46-57). The result, verified live against sutra's own index: `Db::upsert_file` — called from the pipeline at dozens of sites — reports `direct_callers: 0, risk: "low", "no external callers found"`. For a product whose stated consumers are LLM agents that act on these answers, a confidently wrong "0 callers" is strictly worse than no tool. This single gap poisons `sutra_impact`, `sutra_refs`, `sutra_calls`, `sutra_dead`, and the dead-symbol inputs to health — and it is why **every entry** in the review pack's "credible dead code" list turned out to be alive (see Slop list).

The second design-level gap is write topology. The index is one SQLite file shared by multiple writer processes — stdio MCP server per client (each with a background startup reparse, src/main.rs:406), the CLI `sutra parse` path (src/main.rs:175, no lock at all), and the daemon. The only coordination is `ParseCoordinator`, an *in-process* tokio mutex. Per-file replacement is not atomic either: `parse_file` does lookup → `delete_file_cascade` (own transaction) → `upsert_file` → symbol inserts as separate statements (src/pipeline.rs:199-238), and the symbols table has no uniqueness constraint. Two processes interleaving produce duplicate symbol rows — and this is not theoretical: the live index currently holds every `src/tools/review.rs` symbol **twice** (`sutra_outline` returns 46 symbols for ~23 declarations; `sutra_dead` lists `W_BLAST` twice in its own output). Because unchanged-hash files are skipped on reparse (src/pipeline.rs:176-181), the corruption never self-heals. CONTEXT.md's provenance model promises that "computed" facts are deterministic — duplicated symbol rows with one referenced twin and one "dead" twin break that promise visibly.

Third, the enforcement loop's flagship — the guard — has its check inverted relative to its contract. README: "blocks edits that introduce blocking violations in real time." The guard actually checks the file's *current indexed* import edges (src/bin/guard.rs:148-176, src/guard.rs:299-310) and never parses the proposed `new_string`/`content`. So the edit that introduces a forbidden import sails through (the edge isn't in the index yet), and once the violation lands in the index, *every subsequent edit to that file is denied — including the one that removes the forbidden import*. Escape hatches exist (waiver, env disable, Bash writes), but the mechanism enforces one edit too late and then blocks the remediation. Overall: the vision and the layer contracts are right; the release isn't ready because three load-bearing mechanisms (ref precision, index integrity, guard timing) don't yet deliver what the contracts state, and the test suite asserts shapes rather than behavior, so none of this was caught by 738 green tests.

## Findings

```yaml
- id: rust-refs-miss-method-calls
  severity: high
  category: correctness
  title: Rust parser never extracts method-call names; receivers skipped as FieldAccess
  location: src/parser/rust.rs:603-630, src/resolver.rs:46-57
  evidence: |
    walk_refs_recursive collects only "identifier"/"type_identifier" nodes; "field_identifier"
    (the method name in `x.method()`) appears nowhere in src/parser/. Receiver identifiers
    (`SKIP_DIRS.contains(...)`) classify as FieldAccess, which resolve_single skips entirely.
    Verified live: sutra_impact(Db::upsert_file) → direct_callers: 0, risk "low",
    "no external callers found"; sutra_refs(ConstraintKind::default_severity) → 0 refs
    despite the call at src/rules.rs:158.
  why: |
    sutra_impact, sutra_refs, sutra_calls, and sutra_dead are systematically wrong for every
    method invoked via method-call syntax and every const/static used as a receiver — the most
    common shapes in idiomatic Rust. LLM agents are told central methods are safe to change
    and live code is dead. This caused a ~100% false-positive rate in this review's own
    dead-code pack.
  recommendation: |
    Extract field_identifier nodes inside call_expression as Call-context refs, and
    reclassify receiver-position identifiers as usable refs (the *field* of a field_expression
    is the only part that needs type info; the receiver and method name do not). Name-based
    resolution will mis-resolve some overloaded method names across types — acceptable;
    under-reporting is the worse failure for this product. Then reparse and re-baseline health.
  confidence: high

- id: parse-race-duplicate-symbols
  severity: high
  category: correctness
  title: Non-atomic file replace + no cross-process parse exclusion duplicates symbol rows; never self-heals
  location: src/pipeline.rs:176-238, src/main.rs:175, src/db/mod.rs:435-470
  evidence: |
    parse_file: file_by_path → delete_file_cascade (own tx) → upsert_file (ON CONFLICT path)
    → per-symbol inserts. symbols has no uniqueness constraint (migrations/0001). The only
    parse lock is the in-process ParseCoordinator; the CLI parse path (main.rs:175) takes no
    lock, and each stdio server spawns a startup reparse. Observed in the live self-index:
    every src/tools/review.rs symbol exists twice (outline total 46; sutra_dead lists W_BLAST
    twice). Unchanged-hash files are skipped on reparse, so duplicates persist indefinitely.
  why: |
    Index integrity is the product's ground truth. Duplicates split inbound refs across twins
    so the unreferenced twin reports as dead (this manufactured the review.rs "dead cluster"),
    inflate symbol counts, and skew FCA support and health denominators. Recoverable only by
    manual ephemeral rebuild, which nothing triggers.
  recommendation: |
    Wrap delete+upsert+inserts for a file in one transaction, and add a cross-process guard:
    either a UNIQUE(file_id, qualified_name, start_line) index with ON CONFLICT REPLACE as a
    backstop, plus an OS-level advisory lock (flock on the db dir) around parse_workspace.
    Also add a startup integrity sweep that detects and clears duplicate rows so existing
    corrupted indexes heal.
  confidence: high

- id: cochange-always-empty
  severity: high
  category: correctness
  title: sutra_cochange always returns an empty list — git pathspec filters out co-changed files
  location: src/git.rs:103-144, src/tools/cochange.rs
  evidence: |
    git_cochange_files runs `git log --name-only --pretty=format:COMMIT_SEP --since … -- <path>`.
    With a pathspec, git filters the --name-only output to that pathspec, so every line is
    COMMIT_SEP or the queried path itself (verified empirically in a scratch repo); the
    skip-self filter then drops everything. The pack observed empty results for all probed
    files and attributed it to daemon lag — it is a source bug at HEAD.
  why: |
    An advertised tool silently returns "no co-change partners" for every file. Agents conclude
    behavioral coupling doesn't exist. Meanwhile the health pipeline uses a different, working
    path (git_commit_files → commit_files tables), so the two surfaces contradict each other.
  recommendation: |
    Delete the git plumbing and answer sutra_cochange from the already-populated commit_files
    tables (db.cochange_pairs_above_threshold / file_cochange_partners), which the health
    biomarkers prove correct. Alternative (two-pass git: hashes for path, then show per hash)
    re-implements what the DB already has.
  confidence: high

- id: guard-checks-pre-edit-state
  severity: high
  category: design
  title: Guard blocks edits to already-violating files instead of edits that introduce violations
  location: src/bin/guard.rs:148-176, src/guard.rs:299-420, README.md (Guard section)
  evidence: |
    check_file_constraints reads the file's current import edges from the index; the hook's
    new_string/content is parsed only for the additive-edit check. README promises the guard
    "blocks edits that introduce blocking violations in real time."
  why: |
    Enforcement is inverted: the violating edit passes (edge not yet indexed); after reparse,
    every subsequent Edit/Write to that file is denied — including the edit that removes the
    forbidden import. An agent following the deny message into a fix gets deadlocked into
    waiving or shelling out via Bash.
  recommendation: |
    Parse the proposed content: extract import lines from new_string/content with the language
    adapter (guard already links sutra as a lib) and check the *would-be* edge set; at minimum,
    allow edits whose proposed content removes the offending import. Update the README if
    pre-edit checking is retained as a deliberate "flag on next touch" semantic — but that
    semantic still needs the fix-path exemption.
  confidence: high

- id: rules-error-silently-disables-constraints
  severity: medium
  category: correctness
  title: One malformed constraint in rules.toml silently disables all constraints in orient and guard
  location: src/tools/orient.rs:400, src/guard.rs:304-306, src/rules.rs:184-224
  evidence: |
    Rules::all_constraints() fails wholesale on the first bad entry (unknown kind, missing
    field). orient does `.all_constraints().unwrap_or_default()`; guard's closure fails open.
    sutra_review propagates the same error as a hard failure.
  why: |
    A typo in a constraint kind switches the entire enforcement layer off in orient and guard
    with no signal, while sutra_review errors out — inconsistent error contracts across the
    core loop for the same human mistake. Human-authored constraints are the durable,
    "irreplaceable" tier (ADR-0002); dropping them silently is the worst failure mode.
  recommendation: |
    Make all_constraints partition into (valid, errors): enforce the valid ones everywhere and
    surface the errors as a blocking-severity finding in orient/review/guard output. Uniform
    behavior, no silent loss.
  confidence: high

- id: review-introduced-label-overclaims
  severity: medium
  category: correctness
  title: Review labels pre-existing violations from changed files as [introduced]
  location: src/tools/review.rs:340-380
  evidence: |
    The baseline removes ALL outgoing edges of changed files before re-querying violations,
    so any violation whose source file is in the diff appears "introduced" — even when the
    forbidden import predates the diff and the diff didn't touch it.
  why: |
    [introduced] is the highest-trust claim in the architectural change report; agents and
    humans triage on it. Overclaiming trains consumers to ignore it.
  recommendation: |
    Compute the baseline from the merge-base's actual imports for changed files (git show of
    the old content through the adapter's import extractor), or rename the label to
    [involves-changed-file] until the baseline is honest.
  confidence: medium

- id: server-contract-drift
  severity: medium
  category: contract
  title: Server self-description hardcodes v0.2.1 and lists only 21 of 33 tools; README says 28
  location: src/mcp.rs:990-998, README.md
  evidence: |
    get_info() hardcodes "sutra v0.2.1" (Cargo.toml: 0.7.2) and a tool roster missing
    sutra_status, sutra_conventions, sutra_constraints, sutra_resolve, sutra_add_root, and all
    seven newer analysis tools (dead, hotspots, file_health, trend, winnow, duplicates,
    similar). README claims "28 MCP tools"; mcp.rs registers 33.
  why: |
    The instructions string is the LLM consumer's primer — agents won't discover half the
    surface. The stale version string misled this very review's pack-builder into treating
    live-daemon evidence as "deployed-daemon lag," muddying provenance of every intel signal.
  recommendation: |
    Use env!("CARGO_PKG_VERSION") and generate the roster from the tool router instead of
    hand-maintaining prose. Fix the README count in the same pass.
  confidence: high

- id: cochange-tests-vacuous
  severity: medium
  category: correctness
  title: Cochange tests assert shape, not behavior — a fully broken tool passes green
  location: tests/cochange-test.rs:16-44
  evidence: |
    Tests assert is_ok(), descending sort, and self-exclusion — all trivially satisfied by the
    empty vec that git_cochange_files always returns.
  why: |
    This is how a broken advertised tool shipped through 738 green tests. The same shape-only
    pattern is a risk wherever tools wrap git plumbing.
  recommendation: |
    When fixing cochange-always-empty, add an assertion that a known co-changing pair in a
    fixture repo (two files committed together twice) yields a non-empty result. Audit other
    git-backed tool tests for non-emptiness assertions.
  confidence: high

- id: dead-symbol-ratio-unused
  severity: low
  category: slop
  title: Db::dead_symbol_ratio_by_file has zero callers
  location: src/db/mod.rs:772-797
  evidence: |
    Defined, documented, never invoked from src/ or tests — ironically a genuinely dead symbol
    that sutra_dead's curated list missed while flagging ~25 live ones.
  why: |
    Dead weight in the highest-traffic DB module; suggests an abandoned health-biomarker input.
  recommendation: |
    Wire it into a biomarker or delete it.
  confidence: high

- id: fmt-clippy-hygiene
  severity: low
  category: slop
  title: 8,465-line rustfmt drift and 126 clippy warnings at a release tag
  location: project-wide
  evidence: |
    cargo fmt --check fails across many files (no rustfmt.toml exists, so this is default-profile
    drift — fmt was likely never enforced); clippy: collapsible-if ×38, needless-borrow ×19, etc.
  why: |
    Tagging a release with this much drift bakes noise into every future diff and review pack.
  recommendation: |
    One mechanical `cargo fmt` commit plus a clippy sweep before tagging; add both to CI as
    gates so drift can't re-accumulate.
  confidence: high

- id: skip-dirs-duplicated-const
  severity: low
  category: slop
  title: SKIP_DIRS defined identically in two files
  location: src/pipeline.rs:70, src/dart_packages.rs:13
  evidence: |
    Two copies of the same skip-directory list (both alive, contrary to the pack's dead list).
  why: |
    They will drift; a directory added to one walker and not the other yields inconsistent
    indexing between the pipeline and Dart package discovery.
  recommendation: |
    Hoist into a shared const in config.rs or workspace.rs.
  confidence: high
```

## Synthesis

Two root causes generate most of the board. **Layer-0 ref precision** (`rust-refs-miss-method-calls`) is the deepest: it falsifies impact/refs/calls/dead for the dominant Rust call shape, and combined with **index duplication** (`parse-race-duplicate-symbols`) it manufactured the entire "credible dead code" list this review was asked to verify — fix those two and the product's factual claims become trustworthy again, the health dead-ratio inputs become meaningful, and the next review pack stops chasing ghosts. The third theme is **contract honesty**: the guard promises introduce-time blocking but checks pre-edit state; review promises "[introduced]" but labels involvement; the server promises v0.2.1 and 21 tools while shipping 0.7.2 and 33; cochange promises partners and returns nothing. Each is a different file but the same disease — prose contracts maintained by hand, divorced from the mechanism — and the same consumer is hurt: an LLM agent with no way to second-guess the tool.

Fix order: (1) `parse-race-duplicate-symbols` first — atomic replace + cross-process lock + integrity sweep — because until the index is trustworthy, verifying any other fix against the self-index is unreliable. (2) `rust-refs-miss-method-calls`, then force a full reparse; re-run `sutra_dead` and confirm the false-positive rate collapses (acceptance test: `Db::upsert_file` shows real callers). (3) `cochange-always-empty` + `cochange-tests-vacuous` together — the fix is mostly deletion, since the DB path already works. (4) `guard-checks-pre-edit-state` — the design decision (parse proposed content vs. rename the semantic) deserves a deliberate choice, not a patch. (5) The contract batch (`server-contract-drift`, `rules-error-silently-disables-constraints`, `review-introduced-label-overclaims`) plus the hygiene sweep (`fmt-clippy-hygiene`) as the final pre-tag pass. Items 1-3 are the hold-blockers; 4 blocks because the README ships the false claim; 5 is the follow-up tail.

## Slop list

Feature-introduced / current:

1. src/db/mod.rs:772 — `dead_symbol_ratio_by_file` unused (genuinely dead).
2. src/pipeline.rs:70 + src/dart_packages.rs:13 — duplicated `SKIP_DIRS` const.
3. src/mcp.rs:990 — hardcoded "v0.2.1" + stale tool roster in instructions string.
4. README.md — "28 MCP tools" (33 registered); guard section overstates introduce-time blocking.
5. tests/cochange-test.rs — shape-only assertions that pass on empty output.

Pre-existing in touched files:

6. src/tools/review.rs — cognitive complexity 131 / nesting 7 in `build_findings`; worst health in repo by sutra's own measure. Coherent but overdue for decomposition into constraint/convention/health sub-builders.
7. Project-wide rustfmt drift (8,465 lines) and 126 clippy warnings.

Sutra false-positives for the pack-builder's filter list (**every** "credible candidate" was alive — update filters for these mechanisms):

- Method-call syntax invisible (`field_identifier` never extracted): `ConstraintKind::default_severity`, `RawConstraint::into_constraint`, all of `src/db/graph.rs` (hence its "unreachable file" status), `Db::run_migrations`, `Db::ephemeral_migration_names`, `SimCache::get_or_compute` et al.
- Receiver-position consts skipped as FieldAccess: `SKIP_DIRS` (both copies), `MEANINGFUL_KINDS`.
- format!-string interpolation invisible to tree-sitter: `FINDING_SELECT_COLS`, `WAIVER_SELECT_COLS`.
- Duplicate symbol rows (one twin referenced, one reported dead): entire `src/tools/review.rs` cluster (`ChangeStats`, `gather_change_stats`, `gather_affected`, `file_freshness`, `behavioral_coupling`, `build_recommended_reads`, weight consts), likely also the db/mod.rs row mappers and main.rs type aliases (`DbCache`/`WsConfig` are used at main.rs:327/364).
- `src/similarity/duplicates.rs` machinery: fully wired — `find_pattern_families` → `tools/duplicates.rs` → `sutra_duplicates`.
