# Code Review: Health + Review + Orient — Integration Correctness

**Date:** 2026-06-05
**Scope:** Health output layer, review compositor, orient, file_health, MCP dispatch (~5.4k LOC, 12 files)
**Task:** sutra/104
**Verdict:** ship with follow-ups (1 high, 7 medium, 10 low — no critical)

## Verification
- Build: not run (read-only review)
- Tests: not run (read-only review)

## Summary

The scoring math is correct — category capping, proportional scaling, and NLOC-weighted component averages all produce sound results with proper edge-case guards (zero NLOC, empty findings, division-by-zero avoidance). Git output parsing in `git.rs` is well-structured: porcelain blame parsing correctly leverages format guarantees, `splitn` is used appropriately for author fields, and the HCM entropy formula matches Hassan's definition.

The main patterns of concern are: (1) **incomplete signal surfacing** in the review compositor — the `health_findings` output key only contains on-demand biomarkers, omitting DB-stored findings that contribute to health_delta scores, creating an information gap; (2) **constraint_violations_total undercounts** by not including cycle violations; (3) **nondeterministic file_id assignment** for convention drift findings due to HashMap iteration order; and (4) **alias resolution uses alias text instead of canonical component name** in orient.

## Findings

```yaml
# ── Review compositor: signal assembly ────────────────────────────────

- id: cycle-violations-total-undercount
  severity: high
  category: correctness
  title: constraint_violations_total does not include cycle violations
  location: src/tools/review.rs:340,442-468
  evidence: |
    Line 340: constraint_violations_total = current_violations.len();
    Lines 442-466: cycle violations are pushed to constraint_violations
    but constraint_violations_total is never updated.
  why: |
    The total reported in the review JSON output undercounts when cycles
    are detected. Downstream consumers (e.g., the architect reading the
    review) see a total that disagrees with the actual violations list.
    The fix is a single increment after the cycle loop.
  recommendation: |
    Add `constraint_violations_total += 1;` inside the cycle push block
    (after line 465), or recompute the total as constraint_violations.len()
    after both loops.
  confidence: high

- id: health-findings-omits-stored
  severity: medium
  category: information-gap
  title: review health_findings only contains on-demand biomarkers, not stored findings
  location: src/tools/review.rs:185-201
  evidence: |
    health_out is built solely from ondemand_findings (function_hotspot,
    code_age_volatility). DB-stored findings (nested_complexity,
    co_change_scatter, etc.) are used in health_delta scoring
    (ondemand.rs:228-239) but never surfaced in the health_findings key.
  why: |
    A reviewer sees health delta numbers driven by findings they cannot
    see. The health_delta might show a file degraded from 8.0 to 5.2,
    but if the degradation is primarily from stored findings, the
    health_findings section gives no explanation.
  recommendation: |
    Either merge stored findings for changed files into health_findings
    output, or add a separate "stored_health_findings" key, or document
    that health_findings is intentionally on-demand-only.
  confidence: high

- id: degraded-review-misleading-risk-score
  severity: medium
  category: correctness
  title: build_findings error produces a misleading zero-findings risk score
  location: src/tools/review.rs:151-155,173
  evidence: |
    When build_findings fails, ReviewFindings::default() (empty) is used.
    compute() at line 173 is still called, producing a risk score as if
    there are zero violations. findings_degraded=true is set but the
    risk_score field itself is not marked unreliable.
  why: |
    A rules parsing error or DB error causes the review to emit a low
    risk score that looks reassuring when it's actually uninformative.
    The architect could approve a risky diff based on a risk_score that
    reflects missing data, not actual safety.
  recommendation: |
    When findings_degraded is true, either omit the risk_score field
    or set it to null with a note that scoring was degraded.
  confidence: high

- id: driving-findings-only-ondemand
  severity: medium
  category: information-gap
  title: health delta driving_findings only reports on-demand findings
  location: src/health/ondemand.rs:246-250
  evidence: |
    let driving = if delta < 0.0 { ondemand_rows } else { vec![] };
    But score is computed from stored + ondemand (line 236-239).
  why: |
    When a file degrades primarily due to stored findings (e.g., new
    nested_complexity after a refactor), the driving_findings list is
    empty even though degradation is real. The reviewer sees a score
    drop with no attribution.
  recommendation: |
    Include stored findings that contribute to degradation, or rename
    the field to ondemand_drivers to clarify the limited scope.
  confidence: high

- id: convention-drift-findings-not-in-delta
  severity: medium
  category: design-gap
  title: convention_drift_findings not included in health delta computation
  location: src/tools/review.rs:167-171
  evidence: |
    build_findings produces convention_drift_findings (Vec<HealthFinding>)
    but only ondemand_findings is passed to compute_health_delta.
    Convention drift findings implement HealthFinding and have scoring
    weights but don't influence the delta.
  why: |
    A component's convention drift can degrade its health score in the
    snapshot system (if stored as findings), but the review-time delta
    won't reflect drift findings computed in the same review. This is
    potentially intentional (drift is component-scoped, delta is
    file-scoped), but creates an asymmetry.
  recommendation: |
    Either pass drift findings to compute_health_delta alongside
    ondemand_findings, or document the intentional exclusion.
  confidence: medium

# ── Orient: scope resolution ──────────────────────────────────────────

- id: orient-alias-uses-alias-term
  severity: medium
  category: correctness
  title: resolve_scope uses alias text instead of canonical component name
  location: src/tools/orient.rs:58-60
  evidence: |
    results.push(ResolvedComponent {
        id: alias.target_ref.clone(),
        name: alias.term,   // <-- alias text, not component name
    The component name is available from the `components` list (the find
    on line 53-57 matches by id but discards the name with `_`).
  why: |
    When a user accesses a component via alias (e.g., "auth" -> "authentication"),
    the orient output shows the alias text as the component name. This is
    misleading — the user sees "auth" instead of "authentication" in the
    output header, and downstream references to the component use the wrong
    name.
  recommendation: |
    Change the find on line 55 to capture the component name:
    .find(|(id, _, _)| id == &alias.target_ref)
    .map(|(_, name, p)| (name.clone(), p.clone()))
    Then use the captured name instead of alias.term.
  confidence: high

# ── Convention drift: nondeterminism ──────────────────────────────────

- id: drift-nondeterministic-file-id
  severity: medium
  category: correctness
  title: convention drift findings get nondeterministic file_id from HashMap iteration
  location: src/health/drift.rs:94-99
  evidence: |
    let comp_to_file: HashMap<&str, i64> = file_to_component
        .iter()  // HashMap iteration order is nondeterministic
        .filter_map(|(path, comp_id)| ...)
        .collect();  // last-wins on duplicate keys
  why: |
    Multiple files map to the same component. HashMap::collect() keeps the
    last inserted value, but HashMap iteration order is arbitrary and
    changes across runs (Rust uses random hash seeds). Different runs
    attach the same drift finding to different files. This makes health
    tracking that keys on file_id flicker between runs.
  recommendation: |
    Sort the iterator by path before collecting, or pick a deterministic
    representative (e.g., the file with the lowest id or first
    alphabetically).
  confidence: high

# ── File health: component filter ─────────────────────────────────────

- id: file-health-component-filter-leaks
  severity: medium
  category: correctness
  title: component filter applies to files but not to the components summary
  location: src/tools/file_health.rs:206-211
  evidence: |
    if path.is_none() {
        if let Ok(components) = build_component_scores(db, &findings_by_file) {
            result["components"] = json!(components);
    build_component_scores is called with unfiltered findings_by_file and
    iterates ALL components regardless of the component filter.
  why: |
    A user filtering to component "auth" sees per-file results for "auth"
    but component scores for every component in the codebase. This is
    inconsistent and noisy — the component section could contain dozens
    of unrelated entries.
  recommendation: |
    When component filter is active, either skip the component section
    or filter build_component_scores to only the requested component.
  confidence: high

# ── Scoring: snapshot vs component inconsistency ──────────────────────

- id: snapshot-unweighted-average
  severity: low
  category: design-inconsistency
  title: workspace health_score uses unweighted average; component scores use NLOC-weighted
  location: src/pipeline.rs:762-766 vs src/health/scoring.rs score_component
  evidence: |
    pipeline.rs:765: health_sum / files.len() as f64  (unweighted)
    scoring.rs score_component: NLOC-weighted average
  why: |
    A tiny 5-line file with score 1.0 and a 5000-line file with score 10.0
    contribute equally to the workspace metric, but the component metric
    heavily weights the large file. This inconsistency could produce a
    misleading workspace score.
  recommendation: |
    Consider using NLOC-weighted average for the workspace score too,
    or document the intentional difference.
  confidence: high

# ── Git metrics: edge cases ───────────────────────────────────────────

- id: ownership-risk-dual-trigger-metric
  severity: low
  category: information-loss
  title: ownership_risk metric_value/threshold only reflect top-owner when both triggers fire
  location: src/health/git_metrics.rs:156-164
  evidence: |
    When both top_trigger and minor_trigger are true, metric_value is set
    to max_share and threshold to OWNERSHIP_TOP_THRESHOLD. The minor
    contributor data is only in the human-readable detail string.
  why: |
    Structured consumers comparing metric_value against threshold only
    see the top-owner dimension. The minor contributor signal is lost
    from structured fields.
  recommendation: |
    Either emit two separate findings (one per trigger), or use a
    composite metric that captures both dimensions.
  confidence: high

- id: change-entropy-future-timestamp
  severity: low
  category: edge-case
  title: negative age_days from future-timestamped commits amplifies decay instead of dampening
  location: src/health/git_metrics.rs:80
  evidence: |
    age_days = (ref_time - committed_at) as f64 / 86400.0
    If committed_at > ref_time (clock skew, rebases), age_days is negative,
    producing decay > 1.0 via 2^(-negative).
  why: |
    Commits with future timestamps get amplified contribution instead of
    decayed. Could cause false positive entropy findings in repos with
    clock skew.
  recommendation: |
    Clamp age_days to 0.0 minimum, or clamp decay to 1.0 maximum.
  confidence: high

- id: hidden-coupling-dual-findings
  severity: low
  category: design-note
  title: hidden_coupling emits two findings per pair (one per file)
  location: src/health/git_metrics.rs:199
  evidence: |
    For each cochange pair (fa, fb), two findings are emitted.
  why: |
    Each file gets its own finding, which is reasonable for per-file
    scoring. But aggregation across the codebase would double-count
    coupling relationships. The scoring system handles this correctly
    (per-file scoring), but any future aggregate metric should be aware.
  recommendation: |
    Document the intentional duplication, or consider deduplicating in
    aggregate views.
  confidence: high

# ── On-demand: minor calculation issues ───────────────────────────────

- id: median-age-biased
  severity: low
  category: minor-imprecision
  title: median line age calculation picks upper-middle for even-length arrays
  location: src/health/ondemand.rs:102
  evidence: |
    let median_age = ages_days[ages_days.len() / 2];
    For 4 elements [10, 20, 30, 40], returns 30 instead of 25.
  why: |
    Consistently overestimates median age, making code_age_volatility
    slightly more trigger-happy. The bias is small and in the
    conservative direction (flags slightly more).
  recommendation: |
    Use average of two middle elements for even-length arrays, or
    accept the bias as intentional.
  confidence: high

- id: health-delta-new-file-penalty
  severity: low
  category: design-note
  title: files absent from previous snapshot default to perfect score (10.0)
  location: src/health/ondemand.rs:226
  evidence: |
    let prev_score = snapshot_scores.get(path).copied().unwrap_or(10.0);
  why: |
    Newly tracked files with any findings always appear as degraded
    relative to a hypothetical perfect baseline. Could produce noise
    for files that were always unhealthy but never snapshotted.
  recommendation: |
    Consider using the current stored-findings-only score as baseline
    for files without a snapshot, or flag them as "new (no baseline)".
  confidence: high

# ── Scoring: minor robustness ─────────────────────────────────────────

- id: score-file-ignores-confidence
  severity: low
  category: design-question
  title: score_file deduction formula ignores HealthFinding confidence field
  location: src/health/scoring.rs:113
  evidence: |
    Deduction = severity.weight() * kind.default_weight()
    confidence field is not factored in.
  why: |
    A finding with confidence 0.1 has the same scoring impact as one
    with confidence 1.0. Currently all producers emit confidence 1.0,
    so this is not an active bug, but the field exists and future
    biomarkers with variable confidence would be over-penalized.
  recommendation: |
    Either multiply by confidence in the deduction formula, or document
    that confidence is display-only and not used in scoring.
  confidence: medium

- id: score-file-drops-unknown-kinds
  severity: low
  category: robustness
  title: score_file silently skips findings with unrecognized biomarker_kind or severity strings
  location: src/health/scoring.rs:107-111
  evidence: |
    from_str returns None for unknown strings -> continue (silent skip).
  why: |
    Version mismatch or DB corruption would silently inflate scores.
    Currently not reachable with consistent code, but no warning logged.
  recommendation: |
    Log a warning when skipping unknown variants.
  confidence: high

# ── MCP / tool layer: minor issues ────────────────────────────────────

- id: file-health-mode-no-validation
  severity: low
  category: robustness
  title: file_health mode parameter accepts any string without validation
  location: src/tools/file_health.rs:47,107
  evidence: |
    Only "actionable" is checked. Typos like "actinable" silently fall
    through to "all" mode, returning significantly more data.
  why: |
    Silent misbehavior on invalid input. Not a security issue but could
    confuse users.
  recommendation: |
    Validate mode against known values and return an error for unknown modes.
  confidence: high

- id: orient-severity-sort-fragile
  severity: low
  category: fragility
  title: severity sort relies on accidental lexicographic ordering of string values
  location: src/tools/orient.rs:704-714
  evidence: |
    a.severity.cmp(&b.severity) sorts strings lexicographically.
    "advisory" < "informational" happens to match intended ordering.
  why: |
    Adding a third severity level (e.g., "critical" or "warning") would
    silently break the sort order. Not a current bug.
  recommendation: |
    Use a numeric severity weight for sorting instead of string comparison.
  confidence: high
```

## Verified correct

The following areas were explicitly checked and found correct:

- **BiomarkerKind from_str/as_str roundtrip**: All 13 variants present with matching snake_case strings, no missing arms.
- **Category capping math**: Proportional scaling is correct. `scale = cap / raw_total` only executes when `raw_total > cap`. Float rounding is epsilon-level; final clamp prevents out-of-range scores.
- **score_component zero-NLOC**: Returns MAX_SCORE (10.0) for empty components. Correct.
- **compute_nested_complexity threshold**: Uses `> threshold` (strict), so nesting 4 is not flagged, 5+ is. Correct.
- **HCM entropy formula**: `(1/f) * log2(f)` matches Hassan's simplified formulation. The `f <= 1.0` guard correctly skips single-file commits (log2(1) = 0).
- **Martin's Ce/(Ca+Ce)**: Correctly computed. `total == 0` returns 0.0 (isolated). Afferent/efferent correctly partitioned with intra-component edge exclusion.
- **FCA conformance**: Division by zero guarded (skips components with 0 matched symbols). Convention scoping is correct.
- **HRR coherence**: MIN_COHERENCE_SYMBOLS (3) correctly applied. Pairwise loop `i < j` avoids self-pairs.
- **check_metric_drop monotonicity**: Correctly assembled in chronological order. Threshold check fires on strict drop > 0.10, not equals.
- **BlameCache**: HashMap with `&mut self`, no thread safety issue (Rust borrow checker prevents concurrent access).
- **Waiver filtering in scoring**: Both pipeline and file_health correctly exclude waived findings before scoring.
- **Review weight sum**: W_BLAST + W_COMPLEXITY + W_HOTSPOT + W_CHURN + W_CONVENTIONS = 1.00. Correct.
- **MCP argument validation**: serde deserialization + JsonSchema provides type-level validation. No SQL injection possible (all parameterized queries).
- **git blame parsing**: Correctly uses porcelain format guarantees. Time cache covers repeat commit occurrences.
