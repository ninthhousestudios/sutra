# repowise health system: lessons for sutra L4

Survey of [repowise](https://github.com/ARadRar);
code at `/home/josh/soft/sutra-surveys/repowise` (v0.15.2, AGPL-3.0).

Repowise is a Python code health analyzer: tree-sitter parse -> git indexing
-> 25 biomarker detectors -> weighted scoring -> dashboard/API. The health
pipeline is deterministic, zero-LLM. Its scope maps almost entirely to sutra's
Layer 4, with light touches on L0 (tree-sitter) and L6 (clone detection).

## The headline finding

**Organizational/git metrics are stronger defect predictors than structural
complexity metrics.** This is the most important takeaway for sutra's L4
design.

Repowise calibrated biomarker weights on a 13-repo, 5-language defect corpus
(Python/TS/JS/Rust/Go; 830 files, 216 bug-fix-bearing) using L2-regularized
logistic regression with an NLOC control column and time-correct T0 evaluation.
Cross-project leave-one-repo-out OOF AUC ~0.70.

Top calibrated predictors by weight:

| Biomarker | Weight | Category | What it measures |
|---|---|---|---|
| co_change_scatter | 1.80 | organizational | File co-changes with 8+ distinct partners (shotgun surgery) |
| change_entropy | 1.51 | organizational | Hassan's HCM — how scattered across the codebase are the commits this file participates in |
| ownership_risk | 1.38 | organizational | Bird et al. — dispersed ownership (top owner <40%, or 3+ minor contributors) |
| nested_complexity | 1.34 | structural | Max nesting depth within a function |
| complex_conditional | 1.33 | structural | Boolean operator count per condition clause |
| large_method | 1.25 | size | NLOC above threshold, gated on CCN>=2 |
| complex_method | 1.21 | size | CCN above threshold |
| function_hotspot | 1.16 | organizational | Function mod count >= repo p80 AND high complexity |

Meanwhile, classic "code smell" detectors were floored as weak predictors:
brain_method (0.5), low_cohesion (0.5), bumpy_road (0.5),
primitive_obsession (0.5), dry_violation (0.5).

**Implication for sutra:** L4 should treat git-organizational signals as
first-class, not secondary to structural complexity. The vision doc mentions
"complexity, coupling, cohesion, churn, instability" — the churn piece needs
to be expanded significantly.


## Specific biomarkers worth adopting

### Co-change scatter (weight 1.80, strongest predictor)

A file that co-changes with many distinct partners signals shotgun surgery.
Fires when scatter >= 8 distinct co-change partners AND >= 3 commits in 90d.
Uses decay-weighted co-change counts from git log.

Reference: `repowise/packages/core/src/repowise/core/analysis/health/biomarkers/co_change_scatter.py`

**For sutra:** Sutra already plans to compute co-change from git history for
L1 clustering. Exposing the per-file scatter count as an L4 health metric is
nearly free once that data exists. This is the single highest-value addition.


### Change entropy (weight 1.51)

Hassan's History Complexity Metric (ICSE 2009). For each commit touching a set
F of tracked files: entropy contribution = log2(|F|), distributed uniformly
1/|F| per file, decayed with tau=180d half-life. Commits wider than 30 files
are dropped as noise. Computed in a single `git log --name-only` pass.

Reference: `repowise/packages/core/src/repowise/core/ingestion/git_indexer/co_change.py`

**For sutra:** Straightforward to compute during git indexing. A file with
high change entropy participates in sprawling, cross-cutting commits — a
strong signal of architectural entanglement.


### Ownership risk (weight 1.38)

Based on Bird et al. FSE 2011 (they found ownership dispersion the strongest
defect predictor, r=0.86-0.93). Fires when:
- 3+ minor contributors (each <5% of commits), OR
- Top owner share <40%

**For sutra:** Simple to compute from blame/log data. Particularly relevant
for sutra's multi-agent development focus — agent sessions may create exactly
this pattern of dispersed ownership.


### Hidden coupling (special — see L1 note below)

Files that co-change >= 50% of shared commits but have NO static import edge.
Excludes test<->production pairs. Severity escalates at 65% (HIGH) and 80%
(CRITICAL) correlation.

Reference: `repowise/packages/core/src/repowise/core/analysis/health/biomarkers/hidden_coupling.py`

**For sutra:** This is both an L1 fact (emergent architectural relationship)
and an L4 health signal. See separate L1 task for the architectural angle.
For L4, surface it as a health finding: behavioral coupling invisible to the
dependency graph.


### Function hotspot (weight 1.16)

Per-function modification count (from blame rollup onto function line ranges)
>= repo p80 AND high complexity (CCN>=10 or nesting>=3). This is more
actionable than file-level churn because it points to the specific function.

**For sutra:** L0 already has symbol line ranges. Joining with blame data to
get per-function modification counts is cheap and makes health findings
function-granular rather than file-granular.


### Code age volatility

"Dormant code suddenly modified." Function with median line age >= 365d that
got >= 2 commits in the last 30d. Calibration couldn't evaluate it (too rare
at T0) so it kept its prior weight (1.1). Interesting as a review signal even
if its defect-prediction value is uncertain.

**For sutra:** Fits naturally into the review report — "this change touches
code that hasn't been modified in over a year."


### Coverage gradient (continuous)

`4.0 * (1 - line_coverage_pct/100)`, capped at 2.0. Three design choices:
- Continuous, not binary — 80% coverage is healthier than 30%
- Absent coverage != zero coverage — silent when no data ingested
- Separate category cap from binary test-coverage gates

Recovers +0.043 corpus AUC on the covered subset.

Reference: `repowise/packages/core/src/repowise/core/analysis/health/biomarkers/coverage_gradient.py`

**For sutra:** When L4 integrates coverage, use a continuous signal rather
than a threshold gate.


## Scoring design worth studying

### Category capping with proportional scaling

Every file starts at 10.0. Findings deduct based on
`severity * per_biomarker_weight`. Deductions are capped per category:

| Category | Cap |
|---|---|
| organizational | -3.5 |
| structural_complexity | -2.5 |
| test_coverage | -2.0 |
| test_coverage_gradient | -2.0 |
| size_and_complexity | -1.5 |
| duplication | -1.0 |
| test_quality | -0.5 |

When a category's total exceeds its cap, all findings in that category are
scaled proportionally so individual `health_impact` values remain attributable
in the UI. Final score clamped to [1.0, 10.0].

Reference: `repowise/packages/core/src/repowise/core/analysis/health/scoring.py`

**For sutra:** Sutra's health model is richer (per-component, integrated with
architectural context), but the category-capping-with-proportional-scaling
pattern is worth adopting. It prevents one category from dominating and keeps
per-finding attribution linear.


### Calibration methodology (T0 protocol)

The most methodologically interesting piece. Key elements:

1. **Time-correct evaluation:** Score each file at commit T0 (pre-window),
   then check if it received a bug-fix in (T0, T1]. No future leakage.
2. **NLOC control column:** L2 regression includes file size explicitly, so
   each weight reflects defect lift *beyond* file size.
3. **Leave-one-repo-out cross-validation:** Weights must generalize across
   projects, not just within.
4. **Leakage discovery:** `developer_congestion` had weight 1.5 under naive
   HEAD evaluation but dropped to 0.5 under T0 — the HEAD-leakage hero.

**For sutra:** When building our own health calibration, use this protocol.
The T0 insight alone would have led us to over-weight developer_congestion
and under-weight co_change_scatter. Consider building or sourcing a similar
multi-repo defect corpus for validation.


## Function-level blame rollup

Repowise projects git blame onto function line ranges to get per-function:
- Modification count (distinct commits touching lines in range)
- Recent modification count (commits in last 30d)
- Median line age
- Function-level owner

This enables function-granularity health findings rather than file-granularity.

Reference: `repowise/packages/core/src/repowise/core/analysis/health/function_blame_rollup.py`

**For sutra:** L0 already stores symbol line ranges. The blame rollup is a
natural join that makes L4 more actionable. Consider whether this belongs in
L0 (it's a fact about the code, derived from git) or L4 (it's only used for
health scoring).


## What NOT to adopt from repowise

- **Rabin-Karp clone detection** — sutra's HRR similarity (L6) is strictly
  more capable (structural similarity, not just token-level clones).
- **The 1-10 deduction score model** — too flat for sutra's layered
  architecture. Sutra needs per-component health that integrates with
  architectural context, not just per-file scores.
- **Governance biomarkers** (ungoverned_hotspot, stale_governance,
  contradictory_decision) — sutra's L3 constraint system already handles
  this more powerfully.
- **LLM-based doc generation** — out of scope.
- **The biomarker registry pattern** — fine for a standalone tool, but
  sutra's health metrics should derive from the same DD/graph substrate as
  constraints, not be a separate detector pipeline.


## Biomarkers repowise discovered are weak

Worth noting what didn't work, so we don't waste time on them:

- **brain_method** (0.5) — function with NLOC>=70, CCN>=9, high fan-in.
  Fires rarely, weak predictor when it does.
- **low_cohesion / LCOM4** (0.5) — connected components in the
  method-shares-field graph. Classic OO metric, weak in practice.
- **bumpy_road** (0.5) — multiple independent nested regions. Too noisy.
- **primitive_obsession** (0.5) — high parameter count. Fires everywhere.
- **dry_violation** (0.5) — token-level duplication. Weak as a defect
  predictor (though still useful as a maintainability signal).
- **knowledge_loss** (0.4) — bus_factor<=1 with owner change. Confirmed
  weak-negative; survivor bias is real.
- **developer_congestion** (0.5) — too many distinct authors recently.
  Looked strong at HEAD but collapsed under T0 evaluation (leakage).

**Implication:** Don't invest heavily in classic structural code smells as
defect predictors. They have value as maintainability signals (advisory
severity in sutra's trust model) but shouldn't drive health scores.


## Open questions for the brainstorm

1. Should sutra's health scoring be per-file, per-component, or both?
   Repowise is per-file only. Per-component aggregation (NLOC-weighted
   average?) is a natural extension for sutra.

2. Where does git-organizational data live in sutra's layer model? It's
   derived from git history (like L0), used for health (L4), and relevant
   to architecture (L1 hidden coupling). Might warrant its own sublayer
   or explicit cross-layer data flow.

3. How should sutra's health metrics interact with the trust model?
   Calibrated-strong metrics (co_change_scatter, change_entropy) could
   default to advisory severity; calibrated-weak ones (low_cohesion,
   bumpy_road) to informational.

4. Should we build or source a defect corpus for calibration? Repowise's
   13-repo corpus is specific to their biomarker set. Sutra's richer
   model (conventions, constraints, components) may need its own
   validation methodology.

5. How do organizational metrics interact with multi-agent development?
   Ownership_risk may fire constantly when multiple agents write code.
   Should agent sessions be attributed to the human architect for
   ownership purposes?
