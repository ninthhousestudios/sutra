# MCP tool surface audit (sutra/454)

2026-09-29. What each of the 29 MCP tools is used for, whether its output is
worth reading, and where it should live. Status: **approved** 2026-09-29.
Implementation is arc sutra/~18 (tasks sutra/513–521).

## Method

- **Corpus.** `~/.sessions/claude-code` has 2,478 main-thread sessions from
  2026-04 to 2026-09, plus 53 subagent transcripts from
  `~/.claude/projects/**/subagents` (Aug–Sep only; older subagent calls
  aren't in the archive). The parser counts `tool_use` blocks, not tool
  listings, and records each call's arguments, result size, error flag and
  the next 3 tools the agent used.
- **Other harnesses.** Opencode made 44 calls and codex made none, so neither
  changes any verdict. CLI via Bash: mostly `parse`, `check`, `workspaces`
  and `firings` during back-test work. A repo-local pre-commit hook
  (`adityas/backend`) runs `sutra check --diff staged`, and so does the global
  dispatcher in every repo with a `.sutra/` marker (F6). Hook runs appear in
  transcripts only as `git commit` output, not as sutra calls.
- **Era.** The tool set was renamed several times. `read` became `symbol` on
  2026-08-12. `find`/`grep` became `explore`/`lookup`. `status`, `tools`,
  `add_root` and `parse` became `workspace`. `file_health`, `trend` and
  `orient` were deleted. Counts are shown for all time and since 2026-08-12,
  which is the current surface.
- **Output quality.** Each low-use tool was run live on sutra itself
  (`HEAD~3..HEAD` for the diff tools, which is the sutra/510–511 fixes), and
  `dead` also ran on varuna360-core (Python). Findings were spot-checked
  against the source.

## Usage

The "Driver" column says why the calls happen. Organic means the agent chose
the tool unprompted. Guard, skill and instructions mean something told it to.

| Tool | All-time | Since 08-12 | Sessions | Driver |
|---|---:|---:|---:|---|
| symbol | 1452 | 1452 | 302 | organic (+5725 as `read` before 08-12) |
| explore | 1609 | 522 | 557 | organic |
| outline | 1593 | 435 | 553 | organic |
| remember | 700 | 378 | 354 | instructions (manas-instructions, vidhi-reflect) |
| impact | 1185 | 202 | 496 | **guard**: the load-bearing block demands it; 79% are followed directly by an Edit |
| workspace | 371 | 179 | 269 | before 2026-08-25: the analysis-tier gate; now reparse/freshness |
| refs | 161 | 122 | 81 | organic |
| review | 476 | 118 | 397 | skill (vidhi-review step 3) |
| calls | 74 | 72 | 33 | organic |
| constraints | 403 | 57 | 94 | skill (sutra-seed/tend/adopt) |
| map | 406 | 43 | 345 | organic, falling off |
| lessons | 41 | 30 | 33 | instructions |
| lookup | 17 | 17 | 13 | organic (explore covers most of it) |
| cochange | 29 | 13 | 11 | release-review skill subagents |
| deps | 72 | 9 | 34 | skill (sutra-tend) |
| hotspots | 28 | 7 | 25 | release-review skill |
| dead | 15 | 5 | 11 | release-review skill |
| help | 13 | 4 | 8 | organic |
| conventions | 154 | 3 | 26 | sutra-adopt skill (all before in-loop removal) |
| similar | 3 | 3 | 2 | organic |
| components | 28 | 2 | 28 | sutra-adopt skill |
| health | 37 | 2 | 35 | release-review skill |
| context | 1 | 1 | 1 | manas-instructions recommends it; ignored |
| diff_impact | 5 | 0 | 2 | — |
| commit_manifest | 0 | 0 | 0 | — |
| pr_risk | 0 | 0 | 0 | — |
| provenance | 0 | 0 | 0 | — |
| trace | 0 | 0 | 0 | — |
| winnow | 0 | 0 | 0 | — |

Five tools (commit_manifest, pr_risk, provenance, trace, winnow) have never
been called. Seven more have ≤5 calls since 08-12. Only seven tools see real
organic use: symbol, explore, outline, refs, calls, map and lookup.

## Cross-cutting findings

These matter more than the per-tool verdicts. Several "unused" tools are
unused because their output is wrong, and the same defects affect tools that
do get used.

### F1. File blast radius saturates on Rust crates

`sutra_deps cycles=true` on sutra: 77 of 168 files sit in 5 import SCCs. One
SCC is all of `src/tools/*` plus `pipeline.rs`, another is `db` + `parser` +
`similarity` + `conventions`. Every file's transitive blast radius is 157–167
out of 168. Everything built on that number is therefore flat:

- **review.** `risk_score` came out 0.938 for three small similarity bug
  fixes (blast 1.0, churn 1.0, complexity_delta 1.0). `affected_files` and
  `recommended_reads` are the first files alphabetically among 167-way ties:
  `config.rs`, `constraints/engine.rs`, `conventions/bitset.rs`… None of
  them depend on the change.
- **pr_risk.** Composite 0.848 on the same range, with 3 of 4 signals pinned
  at 1.0.
- **diff_impact.** `verdict: "fail"` ("63 affected files", "total blast
  radius 659").
- **hotspots and map.** The blast term is a constant, so the ranking is
  really churn × complexity.

Related: sutra/252 (Rust-module-aware cycle detection). `mod` declarations
and `use crate::` edges create cycles that aren't real dependency cycles.

### F2. Dot-calls bind std methods to same-named user methods

`sutra_calls rank_neighbours direction=callees` lists `BitSet::iter`,
`BitSet::len` (×4) and `HunkSignals<'a>::collect` (×2). These are std
`Vec`/iterator calls. They bind to the only user-defined method with that
short name. The wrong edges propagate into:

- **context.** Its 3k-token pack for `rank_neighbours` spent 556 tokens on
  `HunkSignals::collect` from `sibling_pattern.rs`. It also packed `Corpus::build`, a
  real dependency, as a 3-token stub (`"fn build("`).
- **trace.** The same edges produce the same wrong paths.
- **blast radius.** Every `.len()` in the crate becomes a caller of
  `BitSet::len`.

This is broader than the known "no receiver types" limitation in
`orphans-backtest.md`. Binding by name is defensible for liveness, where
over-counting keeps a symbol alive. It's wrong for callees and dependency
packing, where it invents dependencies. Related: sutra/204 (edge confidence),
sutra/376 (flag ambiguous-name edges).

**Fixed (sutra/513).** A Rust dot-call bound by short name alone, whose name
is also a common std method (`resolver::RUST_STD_METHODS`), is stored as
`resolution_method = 'name_only'`. It keeps its target, so liveness is
unchanged. Every dependency read treats it as an unresolved call. impact,
refs and calls (callers) report the left-out sites as `name_only_callers`.

### F3. `review.changed_symbols` is every symbol in each changed file

`src/tools/review.rs` builds `changed_symbols` from `signals.per_file[].symbols`,
which is the whole file's symbol table. The sutra/511 fix changed
`sutra_similar`, `neighbours_json` and a few functions in `dup_exists.rs`.
`changed_symbols` listed ~290 symbols, including every `SutraServer::sutra_*`
method. `diff_impact`'s `symbol_changes` has the hunk-scoped version,
classified `added` / `deleted` / `signature_changed` / `body_changed` with a
callee diff. The one tool nobody calls holds the precise data, and the tool
everybody calls holds the noise.

**Fixed (sutra/517).** review and `sutra check` both report
`symbol_diff::diff_files`'s changes (`tools/changed_symbols.rs`). Every
dropped field in the fold contract below is gone, `affected_*` included, and
so is review's `explain` argument. On the sutra/510–511 range the list went
from ~290 symbols to 20. Three choices the contract left open:

- **Duplicate names.** Same-named symbols (`impl Foo` blocks, `Foo::fmt` in
  two trait impls) pair identical bodies first, then in source order.
- **Containers.** An `impl`, module or struct is compared on its own lines,
  outside its members (`ContainerScope::Own`). A member's doc comments and
  attributes count as the member's. A method edit reports the method, not
  the `impl`. The other `classify_symbols` callers (sibling_pattern,
  dup_exists, orphans) keep `ContainerScope::Whole`, so their behaviour is
  unchanged. A field added or deleted along with its whole struct is folded
  into the struct.
- **Flattening.** `changed_symbols` entries are {symbol, kind, change, file,
  cognitive}, plus from_symbol/from_file for renames and moves. The callee
  diff stays on `changed_files[].symbol_changes`, so it isn't sent twice.
  `cognitive` is null for a deleted symbol, or one the index has no metric
  for (it used to be 0).

### F4. vidhi-review points agents at the noise and never names the signal

vidhi-review step 3 describes `sutra_review` as a risk score, affected
symbols, "convention violations (FCA)" (removed in sutra/312–313) and
recommended reads that "prioritize convention violation sites" (no longer
true). It never mentions `dup_exists`, `sibling_patterns` or `orphans`.
Those are the mechanisms the purpose doc says sutra exists for.

In the 143 review calls since 08-01, the agent's narration over the next
4 tool calls mentions:

| Topic | Calls where mentioned |
|---|---:|
| risk | 79 |
| constraints | 64 |
| blast radius | 44 |
| duplicates | 6 |
| sibling patterns | 3 |
| orphans | 1 |

Agents copy the saturated risk score into written reviews ("Structural risk
**0.906**"). One reached the right conclusion unprompted: "The structural
signals … are pre-existing project-wide noise … The real review is the diff
itself." Constraint findings do get acted on. For example, one review found
that four `report/**` constraint globs were inert because the directory is
named `reports/`.

`vidhi-release-review` still calls `sutra_file_health` (deleted) and points
its reviewer at `sutra_read` / `sutra_grep` / `sutra_find` (renamed).
`vidhi-sutra-adopt` calls `sutra_conventions action="set_lifecycle"`, which
no longer exists.

### F5. Agents fail on parameter names

| Tool | Failed calls | Why |
|---|---:|---|
| remember | 98 | `anchors` instead of `location_anchors`. manas-instructions itself documents the call as `sutra_remember(text, anchors)`, so the instructions teach the error. |
| symbol | 16 | `query` or `name` instead of `symbol` |
| outline | 37 | `file` or `symbol` instead of `path` |
| symbol, outline, explore | 41 | `workspace` missing on older builds |

serde aliases would remove this whole class of error.

### F6. `sutra check` is the best review surface, and it already runs on every commit

`sutra check --diff HEAD~3..HEAD` took 2.3 s. The human-format output named
the forbidden dep, the pre-existing fan-in and the three dup-exists pairs
with their shared token runs. That is diff-scoped output that names sites,
which is what the purpose doc's design rules ask for.

The global dispatcher (`~/.config/git/hooks/pre-commit`, line 52) already
runs `sutra check --diff staged` in every repo with a `.sutra/` marker. That
is 33 local repos. So every agent `git commit` already shows the advisories
in its Bash output, whether or not anyone runs a review skill. For
comparison, `review` ran in 118 sessions in 7 weeks.

The open question is whether agents act on what the commit shows them.
sutra/485 measures that from the firings log. `sutra_review`'s score fields
add nothing that the commit-time check lacks.

### Smaller defects found along the way

- **similar.** `limit` is ignored in families mode: `limit=5` returned 105
  families (53k chars). The families are same-shape clusters (enum `as_str`
  arms, test fixtures across languages), which the tool description itself
  says is not a duplicate check. **Fixed (sutra/519):** `symbol` is required
  and `min_group` is gone. Nothing else read the stored families (the snapshot
  count was write-only), so the parse-time computation went too: duplicates.rs,
  minhash.rs, and the tables and column (migration 0095).
- **conventions list.** 460 rules (76k chars), almost all tautologies like
  `kind:function → naming:snake_case (1.0)`. The result is too big for the
  tool-result limit.
- **dead.** On varuna (Python), 5 of 5 spot-checked multi-hit findings were
  false positives: a function passed as a value (`_register_themed(x,
  _header_style)`, `_PROVIDERS = (_fetch_open_meteo, …)`, a dict value) or a
  decorator (`@_batched`). All 4 single-hit findings were really dead.
  `unreachable_files` lists every `__init__.py`. If `db::orphans` shares
  these liveness rules, the orphans advisory has the same false positives.
  **Fixed (sutra/515).** It did share them: both count the same `refs` rows.
  The Python adapter now emits `read` refs for bare identifiers in value
  position, kept only when the name reaches a definition or an import, and
  `__init__.py`/`__main__.py` are no longer reported as unreachable.
- **trace.** 9 of 10 forward paths start at a test function (zero callers
  makes it an entry point), and one path appears twice.
- **provenance.** Returns every commit to the symbol's *file*, labeled by its
  conventional-commit prefix. `git log --follow -- <file>` gives the same
  answer.
- **diff_impact / commit_manifest.** Every `impl SutraServer` block reports
  the same large `callee_diff`. Same-named impl blocks are matched against
  each other across the diff.
- **health.** Lists 70 registered workspaces, including stale `/tmp/bt4xx`
  back-test worktrees and a `linux` workspace (507k symbols) whose parse
  never finished.
- **components.** 41 clusters named `root`, `src`, `tests` and `parser`,
  anchored on test helpers like `make_config`.

## Verdicts

These follow the `sutra-purpose.md` design rules: diff-scoped, names
something, tied to a failure mode with bug evidence. Those rules govern the
write side. The read side is judged on accuracy and whether agents reach for
the tool unprompted.

| Tool | Verdict | Rationale |
|---|---|---|
| symbol | **keep** + aliases `query`/`name` | Main read path. |
| explore | **keep** | Main discovery path. |
| outline | **keep** + alias `file` | Organic use. |
| refs | **keep** | Organic use, growing. |
| calls | **keep**, fix F2 for callees | Callers are fine. Callees currently include invented std-method edges. |
| impact | **keep** | It's the guard's ack channel. Its risk level inherits F2. |
| map | **keep**, drop the blast term from ranking (F1) | Useful first look; the ranking is flat on Rust. |
| lookup | **keep for now** → sutra/368 | Consolidating navigation tools is 368's job. |
| workspace | **keep** | Reparse and freshness. |
| remember | **keep** + alias `anchors` | Fixes 98 failed calls. |
| lessons | **keep** | Low volume, instruction-driven, cheap. |
| help | **keep** | Long-form reference named in the server instructions. |
| constraints | **keep** | Skill-driven and acted on (inert globs found). |
| review | **keep, restructure**: see the fold below | Its advisories are sutra's reason to exist. Its score fields are F1 noise. |
| deps | **keep** | sutra-tend uses the path and cycles modes, and the cycles output is accurate. |
| similar | **keep symbol mode; retire families mode** | `mode=dup` is the ad-hoc "does this exist" query and is correct after 484/510/511. Families mode is noisy and ignores `limit`. |
| diff_impact | **fold into review, retire** | Its `symbol_changes` fixes F3. Its verdict is F1 noise. |
| pr_risk | **retire** | A single number (design rule 3), saturated, never called. |
| commit_manifest | **retire** | Never called. `review diff=<sha>` per commit gives the same per-commit view. |
| trace | **retire** | Never called. Test-dominated paths, F2 edges. `calls depth≤3` covers the use. |
| provenance | **retire** | Never called. Same as `git log --follow`. |
| context | **retire**; remove its line from manas-instructions | One call in 7 weeks despite an instruction recommending it, and F2 fills its budget with wrong neighbours. Subagents can call sutra themselves. |
| conventions | **retire the MCP tool** | 460 tautologies; the purpose doc already calls it descriptive only. Update sutra-adopt. |
| components | **retire the MCP tool** | Names are directory labels. The engine stays (boundary constraints, explore aliases). |
| health | **retire the MCP tool** | Admin dump of 70 workspaces. `workspace status` covers per-workspace freshness and `sutra health` stays on the CLI. Resolves the naming collision with the deleted health layer. |
| hotspots | **fold into a release-pack CLI** (below), retire the MCP tool | Only consumer is vidhi-release-review. Blast term is flat. |
| winnow | **fold into the release pack**, retire the MCP tool | Accurate (it surfaced `evaluate_dd`, cognitive 129, churn 34), but accretion has zero bug evidence and nobody calls it. Useful only as refactor-target input to a release review. |
| dead | **fold into the release pack**, retire the MCP tool | The Rust compiler already reports dead code. In Python it's ~50% precision. OK as a candidate list in a whole-repo review, not as an agent tool. |
| cochange | **fold into the release pack**, retire the MCP tool | Review already carries `behavioral_coupling` for the diff. The repo-wide view belongs in the release pack. |

Net result: 29 → 16 MCP tools (9 retired outright, 4 moved into the release
pack). If sutra/368 later merges lookup into explore, that's 15.

### Why a release pack and not four MCP tools

Standing-state signals don't fit a single diff, but a whole-repo release
review needs exactly that. vidhi-release-review is the only consumer. Its
step 3 is "curate, don't dump raw sutra JSON". A `sutra release-pack --md`
CLI command would do the curation mechanically and write
`20-code-intel/SUMMARY.md` directly:

- **Refactor targets.** Top symbols by churn × cognitive complexity
  (winnow's query, symbol-level).
- **Dead candidates.** Labeled as candidates, with the known misses listed.
- **Import cycles.** `deps cycles`.
- **Co-change pairs with no static edge.** Repo-wide behavioral coupling.

That turns 4 MCP tools the agent has to know about into 1 command the skill
runs.

## Fold contract: diff_impact → review

Per the "Refactor contract discipline" rule in `CLAUDE.md`, here is every
output field, what happens to it and who reads it.

| Field | Source | Disposition | Consumer |
|---|---|---|---|
| `changed_files[].path` | both | keep | skill |
| `changed_files[].symbol_count` | review | drop (whole-file count) | none |
| `changed_files[].blast_radius` | review | drop (F1) | none |
| `changed_files[].symbol_changes[]` {symbol, kind, change, callee_diff} | diff_impact | **add to review**; fix same-named impl matching first | new: the reviewer reads what changed, per symbol |
| `changed_files[].symbols[]` (whole file) | diff_impact | drop | none |
| `changed_symbols[]` {symbol, file, cognitive} | review | **redefine** as the flattened symbol_changes with cognitive attached. Today it is the whole file (F3). | vidhi-review "changed symbols" |
| `affected_files[]`, `affected_symbols[]`, `affected_total` | review | drop, or gate behind `explain=true` until F1/F2 are fixed | vidhi-review step 3 prose only |
| `risk_score`, `risk_breakdown`, `churn_window_days` | review | **drop** (design rule 3; saturated). Agents quote it in written reviews. | vidhi-review prose. Update the skill in the same change. |
| `recommended_reads` | review | drop (alphabetical ties under F1) | vidhi-review prose |
| `risk_metrics`, `verdict`, `verdict_reasons`, `impact_count` | diff_impact | drop | none |
| `constraint_violations`, `resolved_constraint_violations`, `waived_constraint_violations`, `constraint_violations_total` | review | keep | skill, acted on |
| `dup_exists`, `sibling_patterns`, `orphans`, `behavioral_coupling` | review | keep, and **name them in vidhi-review** | new |
| `diff_mode`, `workspace`, `as_of`, `is_stale` | both | keep | freshness contract |

Also: `sutra check` renders `changed_symbols` in its human format, so
`review` and `check` report the same data in the same shape.

## Workflow changes

In priority order:

1. **Commit-time check.** Already in place (F6). No change is needed.
   Measure whether agents act on it before building another surface
   (sutra/485).
2. **Rewrite vidhi-review step 3.** Lead with the advisories and constraint
   violations, and remove the score, FCA and recommended-reads prose.
   Refresh vidhi-release-review (dead tool names) and vidhi-sutra-adopt
   (removed actions).
3. **Fix manas-instructions.** `location_anchors` (or rely on the new alias),
   and drop the `sutra_context` line.
4. **Fix the read-side graph (F2)** before anything else builds on callees.

## Caveats

- Subagent calls before 2026-08 are missing from the archive. This could
  understate tools that release-review used through subagents (hotspots,
  dead, cochange). It doesn't change any verdict, because each of those
  verdicts rests on output quality, not call counts.
- Calls-per-session measures reach, not value. The quality verdicts come
  from running each tool on known changes, and cover sutra (Rust) plus one
  Python repo. The Dart output wasn't sampled.
