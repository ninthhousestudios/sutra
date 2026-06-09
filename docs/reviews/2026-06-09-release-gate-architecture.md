# Architecture Deepening Pass — Release Gate, 2026-06-09

- **Scope:** all of HEAD @ `8a70710`
- **Process:** vidhi-deepen, stopped before grilling loop (candidates only, no interface design)
- **Inputs:** review pack 2026-06-09 (code-intel + verification), CONTEXT.md, ADRs 0001–0003, architecture maps
- **Vocabulary:** domain terms per CONTEXT.md (Constraint, Convention, Waiver, Check/Review/Orient, Component, Severity); architecture terms per vidhi-deepen LANGUAGE.md (module, interface, seam, depth, locality, leverage)

## Headline

The three recent arcs (conventions/FCA, constraints/DD, health/similarity) each
built a working vertical slice — and each slice re-implemented the horizontal
infrastructure it needed instead of sharing it. The result is not complexity
disease (the code inside each copy is fine); it is **organizational scatter**:
the same concept evaluated in three or four places with slightly divergent
semantics. The code-intel co-change data confirms it — the analysis tools move
as a clump (80% co-change hotspots↔dead, `pipeline.rs` at 90 co-change
partners, `review.rs` at 60) because every concept change must be applied in
every copy.

Seven candidates, ordered. The first three are the release-significant ones:
they concentrate the three concepts CONTEXT.md says are central — Constraint,
Convention, Waiver — each of which currently fails the locality test.

---

## 1. Constraint evaluation: four implementations of one concept

**Current shape.** "Given declared Constraints and the import-edge graph,
which violations exist, and which are waived?" is implemented four times:

| Site | Lines | Variant |
|---|---|---|
| `src/tools/review.rs:306-500` (inside `build_findings`) | ~195 | DD ingest/reload → resolver → `set_forbidden_pairs` → `query_violations` → delta labeling (edge-removal replay) → cycle check → waiver partition |
| `src/tools/constraints.rs:101-258` (`handle_violations`) | ~157 | near-verbatim copy of the above minus delta labeling |
| `src/tools/orient.rs:160-242` (`compute_violations`) | ~82 | same DD sequence, no waiver partition (waivers fetched separately at `orient.rs:401`) |
| `src/guard.rs:299-438` (`check_file_constraints` + `build_component_maps`) | ~140 | raw-SQL re-implementation: hand-rolled edge query, own component-map builder, own waiver match |

All four repeat the same prologue: `rules::load_rules` → `all_constraints()`
→ build `path_map` → build `file_to_component`/`comp_name_to_id` →
DD `is_invalidated`/`is_loaded` dance (or raw SQL) →
`constraints::find_matching_constraint` → `format_violation_detail`. Each
also defines its own violation struct: `ConstraintViolation` (review.rs:40),
`ViolationEntry` (constraints.rs:260), `OrientViolation` (orient.rs:151),
`ConstraintFinding` (guard.rs:288) — four shapes for one domain object.

Worse, the waiver semantics have already drifted: review and the constraints
tool match a waiver on `file_path == from_path` only
(`review.rs:474-478`, `constraints.rs:220-224`), while guard matches
`from || to` (`guard.rs:389-391`). Duplication has produced divergence in
*enforcement* code — exactly where divergence is most expensive.

**Target shape.** A deep `constraints::check` module (sibling of
`engine`/`resolver`/`worker`):

- Core is a pure function: `evaluate(facts, constraints, waivers, scope) ->
  CheckOutcome` where `facts` = (edges, path_map, component maps), `scope` =
  `Workspace | ChangedFiles(set) | SingleFile(id)`, and `CheckOutcome` carries
  one canonical `ConstraintFinding` type partitioned into
  active/waived/resolved with Severity and Provenance attached.
- DD engine management (ingest-or-reload, ephemeral fallback, forbidden-pair
  loading, cycle query) lives behind this interface; callers never see
  `DdFacts`/`DdDelta` again.
- The delta-labeling trick (remove changed-file edges, re-query, diff) becomes
  an option of `scope = ChangedFiles`, not review-private logic.
- Guard stays on its fast raw-SQL fact-gathering path — that is a legitimate
  second adapter at the *facts* seam (guard cannot afford full `Db` open) —
  but feeds the same pure core. Two adapters = the seam is real.

**Why it pays.** This is the deletion test in reverse: delete any one of the
four copies and its complexity reappears in the others. Leverage: Check,
Review, Orient, and the constraints tool all become thin callers of one
interface, and the next consumer (e.g. `sutra_pr_risk` wanting constraint
context) is one call. Locality: the waiver-semantics divergence becomes
impossible rather than merely fixed. Tests: today the same logic is tested
through four integration paths (guard tests, review-test, constraints-test,
orient tests); a pure core is testable with in-memory facts, and the four
integration suites shrink to adapter smoke tests. Note the unification must
*choose* one waiver-match rule deliberately — flag for the human.

**Blast radius.** Medium-high: 4 source files, the `constraints/` module, 4
test suites. No schema change, no ADR conflict. Mechanical once the core
interface is agreed.

---

## 2. Convention model rebuild trapped inside Review

**Current shape.** `src/tools/review.rs::build_findings` (275–781, cognitive
131, the worst-health symbol in the repo) is two unrelated modules fused:

1. Lines 306–500: constraint evaluation (candidate 1).
2. Lines 503–715: the **entire FCA convention pipeline** — attribute
   extraction over *all* files (not just changed ones), effect enrichment via
   the FCA adapter seam, global + per-component FCA rebuild
   (`review.rs:575-599`), dedup, convention upsert, history snapshots,
   stale-convention deletion, lifecycle proposal generation, drift recording
   (both kinds), and template generation — all as a *side effect* of calling
   the `sutra_review` tool.

`FcaEngine::rebuild` is called nowhere else in src (verified by grep). So the
persisted convention model that Orient serves (`orient.rs:394`), the
conventions tool lists, and guard consults is only as fresh as the last time
someone happened to run a Review. Meanwhile `pipeline.rs::post_parse_sequence`
(597–712) already rebuilds every *other* derived layer at parse time:
components, anchors, vocabulary aliases, health findings, HRR vectors, pattern
families. Conventions are the one layer that opted out, and the DD engine is a
third pattern (lazy tool-time ingest with invalidation + idle eviction). Three
arcs, three different answers to "when does my derived layer recompute" — this
is the structural drift at the heart of the scatter findings.

**Target shape.**

- Extract `conventions::pipeline::rebuild(db, registry, workspace_root) ->
  ConventionModel` owning steps: extract attrs → enrich → global+component FCA
  → dedup → persist (upsert/history/stale-delete) → lifecycle proposals →
  drift recording → templates. One function, one place, its own unit tests.
- Call it from `post_parse_sequence` alongside health/HRR — the convention
  model becomes parse-fresh like everything else, and Orient stops serving
  stale Conventions.
- Review keeps only what is Review's: `check`/`check_inverse` of *changed*
  symbols against the persisted model, plus the convention-waiver partition
  (review.rs:729-770). `build_findings` drops from ~500 lines to ~150 and its
  cognitive 131 collapses with it.

**Why it pays.** Locality: the conventions-map doc lists 7 conventions modules
plus db/conventions.rs, yet the orchestration that ties them together lives in
a *tool handler* — anyone exploring "how do conventions get built" starts in
the wrong file today (the 60-partner co-change scatter of review.rs is the
measured cost). Leverage: Check (guard) could later consult parse-fresh
conventions without invoking review machinery. Tests: the FCA pipeline becomes
testable without constructing a git diff; review-test.rs (1306 lines) stops
being the de facto conventions-pipeline test suite.

**Cost note.** Full-workspace attribute extraction moves to parse time; it is
already full-workspace *per review call* today, so this amortizes rather than
adds work. Incremental rebuild (changed-files-only FCA) becomes possible later
precisely because the step gains an interface.

**Blast radius.** Medium: review.rs, pipeline.rs, new conventions/pipeline.rs,
review-test + pipeline-test. No schema change. No ADR conflict; consistent
with the architecture map's "library-first, tools are thin shells" decision —
which review.rs currently violates in spirit.

---

## 3. Waiver: one CONTEXT.md concept, three implementations

**Current shape.** CONTEXT.md defines Waiver once ("a human decision to
accept a specific finding, with recorded rationale"). The code has three:

- `db/conventions.rs:67-75, 391-519` — `ConventionWaiverRow`, create/list/
  revoke/`waivers_for_check`/`reconcile_orphaned_waivers`
- `db/constraints.rs:7-169` — `ConstraintWaiverRow`, create/get/update/delete/
  `reconcile_orphaned_constraint_waivers`
- `db/health.rs:21-30, 181-254` — `HealthWaiverRow`, create/delete/
  `get_health_findings_with_waiver_status`

and at least six hand-rolled partition-into-waived/active loops at call
sites: `review.rs:472-500` (constraints), `review.rs:729-770` (conventions),
`constraints.rs:216-258`, `guard.rs:360-400`, `file_health.rs:48-53`,
`pipeline.rs:859-864`. Each loop re-invents the match key (constraint_id +
file vs convention_id + symbol + component fallback chain vs finding-id) and,
per candidate 1, they have already diverged.

**Target shape.** Two steps, deliberately separated because of ADR-0002:

1. **Code seam now (cheap):** a `waivers` module defining
   `WaiverTarget { Constraint{..}, Convention{..}, HealthFinding{..} }`, a
   single `Waiver` type with rationale/author/created, a `Waivable` trait
   (finding → match key), and one
   `partition(findings, waivers) -> (active, waived)` used by all six sites.
   The three Db impls stay but become row adapters behind the module.
2. **Schema merge later (optional):** one durable `waivers` table with a
   `target_kind` column. This touches durable tables, which ADR-0002 says
   require proper migrations — do it as its own change with its own migration,
   not bundled into the refactor.

**Why it pays.** The waiver is sutra's trust-model keystone ("appears in every
review report that touches the waived area, can be revoked") — guarantees that
are currently re-earned per subsystem and verifiably unequal (health waivers
have no reconcile-orphans; constraint vs convention match keys differ in
structure). Locality: waiver behavior questions get one answer. Tests: the
partition semantics get direct unit tests instead of being exercised
incidentally through three tool suites.

**Blast radius.** Medium for step 1 (pure refactor, six call sites, three db
files). Step 2 is durable-data migration — gated, separate.

---

## 4. Health rollup: file scores → component scores, three times

**Current shape.** "Score every file from its findings, then aggregate to
weighted Component scores with nloc" is implemented three times:

- `src/pipeline.rs:836-946` (`compute_snapshot_health`) — for snapshots
- `src/tools/file_health.rs:219-283` (`build_component_scores`) — for the tool
- `src/tools/orient.rs:~680-700` — inline, for the component health block

All three do: `get_health_findings_with_waiver_status` → filter waived →
group by file → `scoring::score_file` → group memberships
(`component_members_with_line_count`) → `scoring::score_component`. The
category-deduction accumulation (`cat_totals`) is duplicated in pipeline and
file_health with different output encodings (JSON string vs map).

**Target shape.** `health::scoring` (or a new `health::rollup`) grows one deep
function: `score_workspace(db) -> WorkspaceHealth { file_scores (with
category deductions), component_scores (with member_count/total_nloc),
instability }`. Pipeline serializes it into snapshot rows; file_health and
orient render slices of it. `round2` stops being defined in three files.

**Why it pays.** This is the cheapest of the big wins — the three copies are
already semantically identical, so the refactor is pure concentration. It also
explains part of the measured hidden-coupling clump among the analysis tools:
they co-change because the rollup recipe changes in three places. Tests:
health-test.rs (1898 lines) currently pins behavior through the tool JSON;
a typed `WorkspaceHealth` is directly assertable.

**Blast radius.** Low-medium: 3 files + health module, health/orient tests.

---

## 5. Change-risk signals: three generations of the same gathering

**Current shape.** Three modules answer "given changed paths, how risky":

- `src/tools/diff_impact.rs:21-134` — inline gathering (per-file blast,
  per-symbol max cognitive, affected files via
  `find_files_referencing_symbols`) + hard-coded threshold verdict
  (10/30 files, 15/25 cognitive, 20/50 blast)
- `src/tools/pr_risk.rs:56-152` — same gathering re-done + normalized
  weighted composite via `tools/scoring.rs`
- `src/tools/review.rs:793-1034` (`gather_change_stats`, `gather_affected`,
  `behavioral_coupling`, `build_recommended_reads`) — same gathering a third
  time, feeding the architectural change report's risk_breakdown
  (review.rs:1219-1240)

(The review-pack dead-code list flags the review.rs cluster as dead; it is
not — `compute` calls it at review.rs:1068-1250. Daemon-intel false positive,
consistent with the v0.2.1/v0.7.2 deploy gap noted in the pack meta.)

The signal *gathering* is identical; only the presentation differs (verdict
vs composite vs report section). The pack's hidden-coupling cluster
(`tools/{hotspots,dead,diff_impact,file_health}` at 80% co-change with no
static edge) is the behavioral footprint of this: a metric change fans out
across tools that each hand-roll the same db-metric access.

**Target shape.** A `tools::change_signals` (or `analysis::signals`) module:
`gather(db, changed_paths, churn) -> ChangeSignals { per_file, total_blast,
max_cognitive, affected_files, affected_symbols }`. diff_impact becomes a
threshold-verdict view over it (~30 lines), pr_risk a weighted-composite view
(~60), review's `compute` a consumer. Keep the two scoring presentations —
verdict and composite serve different callers — but make the inputs one
module. Fold `tools/scoring.rs` constants (BLAST_NORM etc.) in; they are the
same concept.

**Why it pays.** Leverage: the next signal (e.g. constraint-violation count in
pr_risk, convention conformance in diff_impact) is added once and appears in
all three surfaces. Tests: signal gathering gets one fixture-based suite
instead of being implied by three tool-output suites.

**Blast radius.** Medium: 3 tool files + scoring, 3 test suites.

---

## 6. components.rs has outgrown its seam

**Current shape.** `src/components.rs` (1296 lines, churn×blast rank 1, max
cognitive 56) is five modules in one file: config parsing (23–58), weighted
adjacency + co-change edge blending (64–192), Louvain + auto-tune (198–354),
auto-naming (360–390), component identity/reconciliation + event detection
(404–668), staleness hashing (675–744), orchestration (746–820), semantic
anchors (826–1024), and concept-density vocabulary helpers (1030–1048).
Discovery, identity, and anchors are distinct CONTEXT.md concepts (Component
lifecycle: "merges, splits, or drifts, not fresh sets"; Semantic anchor is its
own glossary entry) sharing a file by accident of growth.

**Target shape.** A `components/` module directory: `clustering.rs`
(adjacency, Louvain, auto-tune), `identity.rs` (reconcile, detect_events,
staleness), `anchors.rs` (scoring, name alignment, concept density),
`mod.rs` (config + `discover_components` orchestration, same public
interface). Pure file moves — no logic change, public fns stay put.

**Why it pays.** Locality only, but for the file the intel ranks most
important: the recently-landed compact-mode change and the dense Louvain math
currently share a blast zone. Anchor heuristics are explicitly slated for
human-confirmation features (CONTEXT.md: "confirmable by the human"), so they
will churn; isolating them keeps that churn out of the clustering core.

**Blast radius.** Low: one file → four, import paths, no test changes beyond
`use` lines.

---

## 7. Tool handler seam: the `_with_freshness` split and the mcp.rs chorus

**Current shape.** Per-item freshness annotation is hand-woven into each tool
that wants it via duplicated `handle`/`handle_with_freshness` pairs
(`hotspots.rs:21-28/30-111`, `file_health.rs:28-36/38-217`, map/grep/find/read
via boolean params), while envelope freshness (`as_of`/`is_stale`) is added
separately by `mcp.rs::wrap_response` (264–278). Meanwhile every one of the
~33 `mcp.rs` tool methods repeats the same four-line ritual
(`require_analysis` / `resolve_workspace` / `get_db` / `wrap_response`), and
`main.rs` CLI calls the plain `handle` variants — so the freshness concern has
two ad-hoc adapters and no seam.

**Target shape.** A `ToolContext { db, workspace_root, annotate_freshness }`
passed to tool handlers, plus a small `FreshnessCounts` annotator used
uniformly; the `handle`/`handle_with_freshness` pairs collapse to one
function. The mcp.rs ritual folds into one helper on `SutraServer`.

**Why it pays.** Shallow-module cleanup: each pair's interface is currently as
wide as its implementation delta (one bool). Modest, but it removes the
copy-paste template that every *new* tool currently inherits — this is how the
scatter reproduces.

**Blast radius.** Wide but shallow: mechanical edits across tools/ and mcp.rs;
contract tests unaffected (JSON shape unchanged).

---

## Order of work

1. **Candidate 1** (constraint evaluation core) — highest divergence risk,
   release-relevant enforcement semantics, unblocks 2.
2. **Candidate 2** (convention rebuild → pipeline) — guts review.rs; do after
   1 so `build_findings` is being dismantled once, not twice.
3. **Candidate 3 step 1** (waiver code seam) — call sites are fewest right
   after 1+2 land. Schema merge deferred behind an ADR-0002-compliant
   migration.
4. **Candidate 4** (health rollup) — cheap, independent, can interleave.
5. **Candidate 5** (change signals) — after 2, since review's gathering code
   is in motion until then.
6. **Candidates 6, 7** — anytime; pure-move refactors suitable for low-context
   sessions.

Not in scope here (other pass): rustfmt drift (8.4k-line diff), 126 clippy
warnings, version-string gap (Cargo 0.7.2 vs daemon-reported 0.2.1),
`db/graph.rs` reachability, `SKIP_DIRS` copy-paste, daemon dead-code/co-change
intel regressions surfaced above as false positives.
