# What sutra is for

Current statement of sutra's purpose. Decided in sutra/466 (2026-09-26),
closing the sutra/457 reorientation. It replaces the 2026-05 vision
([archived/sutra-vision.md](archived/sutra-vision.md)).

## Purpose

Sutra has two jobs, both for AI agents writing code:

1. **Read side: a cheap, accurate map of the code.** Agents find, read and
   trace code through the symbol graph instead of grepping and reading whole
   files. This has always been sutra's job and it's unchanged.
2. **Write side: stop the coding patterns that produce bugs in agent-written
   repos.** Sutra flags them at the moment of writing (guard) or review
   (`sutra_review`, `sutra check --diff`).

The old vision aimed at a "living architectural model" that told an architect
the codebase was coherent: health scores, trends, convention deviations and a
layered blueprint. The evidence didn't support it. The write side is now
scoped to failure modes with measured bug cost. Sutra doesn't try to judge the
code's quality in general.

## Evidence

The full analysis is in [ai-failure-modes-evidence.md](ai-failure-modes-evidence.md).
In short: 235 closed-bug incidents across all projects (2026-05 to 2026-09),
each classified by the mechanism that made it possible.

- 43% were domain bugs: logic, numerics, port fidelity, vendor behaviour. No
  structural tool helps with those.
- Of the AI-pattern bugs, **PAR + DUP + SWALLOW made up 63%** and most of the
  long recurrence chains. UNWIRED was rare, but it caused 2 of the 6
  AI-pattern production escapes.
- The vidhi/review/5 pilot found health findings 17% actionable, and trend had
  zero signal. The health layer was also sutra's largest single bug source:
  25 of its 89 bugs.
- Accretion (giant functions) had **zero** bug root causes. Layering had one.

## Targeted failure modes and their mechanisms

| Mode | What goes wrong | Mechanism | Trigger | Back-test | Build |
|---|---|---|---|---|---|
| **PAR** (rewrite subtype) | A fix changes a pattern at 1 of N sites; the rest survive | "You fixed 1 of N": list the surviving instances of a pattern the diff rewrote or wrapped | review, advisory | 4/4 known diffs, 29/31 ordinary commits silent ([sibling-pattern-backtest.md](sibling-pattern-backtest.md)) | sutra/467 |
| **DUP** | Logic re-implemented instead of reused; the copies drift | "This already exists": new functions vs the repo and vs the rest of the change (embed + lexical + rare shared token runs), grouped by file pair | review, advisory | 7/9 reachable introductions; fires on 42% of added functions, 20% of them real duplicates ([dup-exists-backtest.md](dup-exists-backtest.md)) | sutra/469 |
| **SWALLOW** | `.ok()`, `let _ =`, `catch (_)`: a failure reads as valid data | Swallowed-error ratchet: tier A blocks unless the site carries `// swallow: <reason>`; tier B is advisory | guard (A), review (B) | 14/14 lexical-idiom bugs; tier A 18/19 held-out sites real discards, 0.5 sites/commit ([swallow-ratchet-backtest.md](swallow-ratchet-backtest.md)) | sutra/472 (engine), sutra/486 (review, rules, tier A on) |
| **UNWIRED** | Built ahead of its call site, never connected | Orphans: symbols the diff adds that nothing references | review, advisory | not yet run; the back-test is the first step of the build | sutra/483 |

Build order (encoded as yojana dependencies):

1. sutra/472 (swallow engine: justify marker, rename-robust match key) and
   sutra/467 (sibling pattern) in parallel. They don't share files. 467 also
   builds the shared firing log that every later mechanism reuses.
2. sutra/486 (swallow review side, rule adoption, tier A on), after both.
3. sutra/483 (orphans), after 467, the 482 resolver fix and the 481 dead-API
   triage. It runs its back-test before building anything.
4. sutra/471 (Dart encoder), then sutra/469 (dup-exists), then sutra/484
   (strip-mode default, reusing 469's scorer). 469 has a volume condition:
   after grouping, the median review shows at most 2 dup groups, or it ships
   behind an opt-in flag.

### Known gaps, deliberately not built

| Gap | Why not |
|---|---|
| **Additive PAR** (a new path, table or write site misses a cross-cutting rule) | The sibling check misses it by construction (0/4). File-level co-change also misses it (0/4), and 3 of the 4 misses were in the diff's own file ([behavioral-coupling-backtest.md](behavioral-coupling-backtest.md)). It needs symbol-level knowledge that N functions share a responsibility, and there's no viable design yet. Open research item. |
| **List-shaped DUP** (label lists, field lists, language lists mirroring a registry) | About half of DUP, and function similarity can't see it (0/6). No mechanism yet. |
| **Non-lexical SWALLOW** (absence reads as zero, log-and-default arms) | Not lexically detectable. The ratchet catches the idioms, not the semantics. |
| **PREMISE** (20 bugs), **TESTVAC** (11) | No structural leverage. These are process problems: the verification protocol, reading the source instead of recalling it, red-green tests. Lessons can surface known traps. |
| **REFAC, DEBRIS, LAYER** | Low frequency and low leverage. Existing constraints cover layering. |
| **Accretion / growth gate** | Zero bug evidence. `sutra_diff_impact` still flags changed functions at cognitive ≥ 15. |

## Design rules for write-side mechanisms

Learned from the health layer and the three back-tests. A new mechanism
follows these or gives a reason.

1. **Back-test before building.** Measure recall on the historical bugs of its
   mode, and noise on a held-out sample of ordinary commits with rules frozen
   before the draw. No mechanism ships on an unmeasured hypothesis.
2. **Diff-scoped.** It speaks about the change in hand, never the standing
   state of a file or repo. Scores and trends fail this by construction.
3. **Names something.** A symbol, site or file:line to act on, never a number.
4. **Ties to a mode with bug evidence.** "Could be useful" is how 13
   biomarkers accumulated.
5. **Say what exists, not what to do.** Over-reuse is a real counter-mode
   (REFAC: sutra/282, 285, swisseph-rs/165). "This already exists" names the
   match and lets the agent judge fit. It never says "reuse X".
6. **Blocking only at high precision.** Otherwise advisory. Only SWALLOW
   tier A blocks. Blocking rules waive at the site with a stated reason
   (`// swallow: <reason>`). Central waiver files hide the reason far from the
   code and scale badly.
7. **Advisory volume is a shared budget.** An agent that learns to skim one
   advisory skims all of them. Group items, suppress idiomatic siblings, and
   treat high volume as a defect, not a tuning detail.
8. **Incomplete is never clean.** A search cut short by a cap or budget
   reports `incomplete: <cap>`, never "nothing found" (from sutra/406). The
   same rule applies to SWALLOW in sutra's own code.
9. **Log firings.** Every mechanism records what it flagged so the acted-on
   rate can be measured (see Metric). The log is the durable
   `mechanism_firings` table (`src/db/firings.rs`, built in sutra/467): one
   row per flagged site (mechanism, finding kind and key, file:line,
   enclosing symbol, line text), with the diff's identity (spec, revisions,
   content fingerprint) and the HEAD commit at firing time. Reviewing the
   same diff twice doesn't add rows. `sutra firings [--mechanism]
   [--since]` lists rows with a `site_status` (the flagged line is present,
   changed or its file is gone), the acted-on proxy for sutra/485.

## What sutra is today

| Area | Surfaces | Role |
|---|---|---|
| Structural index | tree-sitter → SQLite: files, symbols, refs, imports; per-language adapters (Rust, Dart, Python, TS/JS, C) | Ground truth for everything else |
| Navigation | `sutra_explore`, `sutra_lookup`, `sutra_symbol`, `sutra_outline`, `sutra_map`, `sutra_context`, `sutra_refs`, `sutra_calls`, `sutra_trace`, `sutra_deps`, `sutra_impact` | Read side |
| Freshness | Content-based staleness; refresh before answering ([freshness-map.md](freshness-map.md)) | Every answer carries `as_of`/`is_stale` |
| Constraints and guard | `.sutra/rules.toml` (forbidden deps, cycles, fan-in, forbidden_patterns), DD engine, `sutra-guard` edit hook, `sutra check`, `sutra_constraints` ([constraints-map.md](constraints-map.md)) | Write side, blocking. Home of the SWALLOW ratchet |
| Review | `sutra_review`, `sutra_diff_impact`, `sutra_pr_risk`, `sutra_commit_manifest` | Write side, advisory. Home of the PAR (`sibling_patterns`, sutra/467), DUP and orphans mechanisms. `behavioral_coupling` lists co-change partners with no static edge |
| Similarity | HRR vectors (embed, strip), lexical tokens, `sutra_similar` ([similarity-map.md](similarity-map.md)) | Substrate for the DUP mechanism. Strip mode is not a duplicate detector (sutra/484) |
| Dead code | `sutra_dead` | Substrate for the orphans mechanism (resolution corrected in sutra/477) |
| Git signals | `sutra_cochange`, `sutra_hotspots`, per-symbol cyclomatic/cognitive complexity | Review inputs; hotspots were the one health-era signal that was right on all 4 pilot repos |
| Components | Directory-based clustering, `sutra_components` | Boundary constraints and explore ranking |
| Conventions | FCA detection, `sutra_conventions` (list only) | Descriptive. In-loop consumers were removed after live use showed high false positives (sutra/312, 313) |
| Vocabulary | `.sutra/aliases.toml` | Human terms → code, resolved first by explore |
| Lessons | `~/.sutra/lessons.db`, `sutra_remember`, `sutra_lessons`, surfaced inline by symbol/impact | Cross-project negative knowledge anchored to code |

## Removed or not pursued from the old vision

- **Health layer** (scores, trend, biomarkers, erosion): deleted in
  sutra/473–475. The per-surface reasons are in
  [health-disposition.md](health-disposition.md).
- **Orient surface and the review-time convention deviation report**: removed
  in sutra/312 and 313 because of their false-positive rate.
- **Verification orchestration** (Kani, proptest, mutation testing as a sutra
  pipeline): not pursued. TESTVAC is real, but its fix is red-green discipline
  and mutation testing run by the agent, not a sutra layer. Revisit only if
  TESTVAC escapes grow in the re-measure.
- **Graph clustering, structural templates, HRR fuzzy vocabulary, ADRs as
  constraints, semantic diff via HRR**: no bug evidence ties them to a
  targeted mode. They aren't planned. Any one of them can come back through
  the design rules above.

## Strongest rejected alternatives

- **Keep health-as-scoring and calibrate it (sutra/407).** Rejected. The noise
  was mostly inherent to file-level and organizational metrics, and the
  estimated ceiling after every fix was about 30–35% precision. A score delta
  still doesn't say what to do, and each comparability contract was a bug
  source.
- **Build the growth gate first because complexity is cheap to measure.**
  Rejected. There's no bug evidence, and being easy to measure is not a
  reason to build something.
- **Freeze the health code instead of deleting it.** Rejected. Frozen code is
  debris that misleads the next agent, and it kept breaking on freshness
  changes (sutra/412–443).

## Metric

Bugs per failure mode over time, classified with the step-0 taxonomy. DOMAIN
is the control. The re-measure is sutra/485, run about 8 weeks after the
mechanisms go live. Two corrections to the step-0 baseline:

- **Exclude health-subsystem bugs from the baseline.** Deleting the layer
  alone will lower sutra's PAR/DUP/SWALLOW counts.
- **Add leading indicators.** Monthly counts are too thin to detect an effect
  soon: about 135 AI-pattern bugs over 4.5 months, ±20% classifier error. So
  also measure each mechanism's **acted-on rate** (did the diff change after a
  firing?) and replay its **escapes**: for every new bug in a targeted mode,
  did the mechanism fire and get ignored, or not fire at all? That says which
  mechanism failed and how.

## Unchanged

- Sutra doesn't write code, manage tasks (yojana), run the design process
  (vidhi) or store personal memory (chitta).
- The core is structural and needs no LLM. It runs locally.
- Software architecture (monolith, library-first, SQLite with an
  ephemeral/durable partition, layered adapter traits):
  [sutra-architecture.md](sutra-architecture.md).
