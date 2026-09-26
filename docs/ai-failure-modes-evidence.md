# AI failure modes: bug evidence (sutra/457 step 0)

Evidence base for reorienting sutra around preventing long-term-harmful AI
coding patterns. Question: which failure modes actually produce bugs in
agent-written repos, how often, and at what cost? The answer decides what
sutra builds first and sets the baseline for the ongoing metric (bugs per
failure mode over time).

Per-bug classification: [`ai-failure-modes-bugs.tsv`](ai-failure-modes-bugs.tsv).

## Method

- Population: every yojana task with `category=bug`, `status=done` and a
  non-empty `root_cause`, across all projects (2026-05-06 to 2026-09-24).
  296 closed bugs, 235 unique incidents classified. Excluded: about 55 with
  no root cause, and 4 duplicate incidents (adityas/42 = ai/197,
  backend/67 = sutra/443, swisseph-rs/152 = 151, rs-dart/54 = 52).
- Each root cause got one primary mode (the mechanism that made the bug
  possible) and optional secondary modes. One classifier (Claude), one pass,
  no inter-rater check.
- Cost: time-to-close is meaningless here (agents close in minutes; long
  durations are queue time). Cost is measured two ways instead: **production
  escapes** (the root cause says it reached users or prod) and **recurrence
  chains** (follow-up bugs from the same mechanism).

## Taxonomy

The hypothesised modes were refined against the data. Two were split, two
added, and two had almost no evidence.

| Code | Mode | Mechanism |
|---|---|---|
| DUP | Duplication / divergent copies | Logic re-implemented instead of reused, or hand-maintained lists that mirror a source of truth. The copies drift. |
| PAR | Incomplete propagation across parallel paths | Paths that are legitimately different each need a cross-cutting change, and only some get it. Examples: incremental vs full parse, every write path bumping a cache token, every entrance to a gate, a new sibling table getting the convention, a removal applied at the definition but not the call site, a fix applied at 1 of N sites. |
| SWALLOW | Fail-open / swallowed errors / absence reads as zero | `.ok()`, `unwrap_or_default`, `let _ =`, `catch (_)`, permissive `None` arms, defaults that make a failure look like valid data. |
| PREMISE | Unverified premise asserted from memory | API constants extrapolated rather than read, library/vendor behaviour assumed, fixes shipped on an unmeasured hypothesis, comments that state a false justification. |
| TESTVAC | Vacuous or mis-aimed tests | Tests pass without exercising the claim: zero-iteration loops, always-true comparators, silent skips, fakes standing in for the real gate, test keys that differ from prod. |
| UNWIRED | Built but never wired | Type or function built ahead of its call site and never connected. Deferred "follow-up" halves that were never done. |
| REFAC | Refactor/reuse silently changed a contract | Moved code lost a guard, a helper reused under a different cost or semantic contract, a lock overloaded so a flag's meaning widened. |
| DEBRIS | Stale docs/config/migration residue | Content that misleads the next reader or build. |
| LAYER | Layering shortcut | Importing whatever works. |
| — | Accretion (giant functions) | Hypothesised. **Zero root causes.** |
| DOMAIN | Not an AI pattern | Domain logic, concurrency design, numerics, port fidelity, platform/vendor behaviour, reverse-engineering analysis. |

## Results

### Frequency (235 incidents)

| Mode | Primary | Any (primary or secondary) |
|---|---:|---:|
| DOMAIN | 100 (43%) | 113 |
| **PAR** | **32** | **47** |
| **DUP** | **28** | **36** |
| **SWALLOW** | **25** | **39** |
| PREMISE | 20 | 28 |
| TESTVAC | 11 | 21 |
| UNWIRED | 8 | 8 |
| REFAC | 6 | 8 |
| DEBRIS | 4 | 9 |
| LAYER | 1 | 1 |
| Accretion | 0 | 0 |

Of the 135 incidents with an AI-pattern primary, **PAR + DUP + SWALLOW
account for 85 (63%)**. PAR and DUP together form one family (divergence
between things that should agree): 60 primaries, 44% of AI-pattern bugs.

By project, the AI-pattern share is highest where agents build features over
time: sutra 67%, swe-dashboard 70%, adityas 53%. It is lowest in the
port/RE work (swisseph-rs 33%, kala-reverse 0%), where bugs are fidelity and
numerics.

### Cost: production escapes

8 root causes state a production or user-facing impact. 6 of them are AI
patterns:

- **PAR:** ai/190 (removal half-applied, so every completed checkout broke);
  ai/107 (deploy path didn't sync templates, so every export returned 500).
- **UNWIRED:** ai/110 (metrics type had zero callers, so every prod metric
  read 0); ai/65 (client half of chart_facts never wired).
- **TESTVAC:** backend/16 (test JWTs were RS256, prod was ES256, so every
  prod token got a 401).
- **DEBRIS:** innerorbits/17 (Podfile line left after the migration).
- DOMAIN: adityas/38 (GRANT), swe-dashboard/110 (manifest permission).

The rest of the population was caught by review (codex/grok/agent review
follow-ups), which is a sampling bias. See caveats.

### Cost: recurrence chains

Chains are where AI patterns cost the most. The fix lands on one path,
the sibling path breaks later, and a new task is filed.

| Chain | Bugs | Mode | Mechanism |
|---|---:|---|---|
| sutra health validity/trend (402→408→412→414→416→417→418→419→423→424→426→427→432→436→438→441→447→448→455 …) | ~20 in 6 days | SWALLOW, PAR, DUP | Absence-reads-as-clean, incremental-vs-full paths, duplicated serializers and INSERTs. **The scoring subsystem itself was the largest single bug source in sutra** (25 of 89 sutra bugs). |
| Incremental vs full parse (320, 378, 386, 412, 416, 426, 439, 443, 382) | 9 | PAR | The full path re-derives, the incremental path deletes via cascade and never rebuilds. |
| Stale DD engine / id spaces (v1/30→219→220→297→298→300) | 6 | UNWIRED, PAR | `invalidate()` had zero callers; each fix covered one staleness mode. |
| Entitlement buy-stub (ai/183→195→196→197→198) | 5 | DUP, DOMAIN | The buy-vs-renew fork was duplicated in panel and pill and drifted, then identity-less caches. |
| Consent (ai/101, 135, 187, 191, 192) | 5 | DUP, PAR | Two write paths built separately, two gate layers, copy written independently of the registry. |
| Dual constraint engine (440→456→459→460) | 4 | DUP, PAR | Two hand-maintained copies of each rule. "Drifted twice." |
| Hardcoded language lists (261, 280→283→284) | 4 | DUP, PAR | A literal list duplicated the adapter registry. The 280 fix missed an identical copy 12 lines away. |
| Test-scope exclusion (290→293→294→296) | 4 | PAR | Filtering was applied only where a test existed. The escape hatch was local to 1 of 4 consumers. |
| Cancel path (ai/140→141→142→146) | 4 | SWALLOW, DOMAIN | cancel() swallowed every failure as success. |
| Heliacal dobs swap (swe-dashboard/34, rs-dart/49, swisseph.dart/4) | 3 | PREMISE | **Contradictory** API-slot claims across three repos, each asserted confidently. |
| yojana criteria (42→47) | 2 | PAR, SWALLOW | `unwrap_or_default` was fixed at 1 of 5 read sites. |

## Detectability: what a graph tool can catch at write time

This decides which modes sutra should target. A mode matters only if it is
frequent AND sutra has leverage at the moment of writing.

| Mode | Freq | Sutra leverage | Candidate mechanism |
|---|---|---|---|
| DUP | high | **High for function-level copies** (engines, extractors, pipelines, serializers). **Low for list-shaped duplication**: label lists, field lists, bit masks, language lists. Those are about half of DUP and function similarity won't see them. | Similarity check of new/changed functions against the repo (mechanism 1). Plus detecting literal collections that mirror an enum/registry. |
| PAR | highest | **Medium, and the subtype matters.** (a) Copies: reduces to DUP. (b) "Fix at 1 of N sites": a diff that changes idiom X to Y at one site while X survives at other sites is cheap and precise to detect (yojana/47, sutra/283, 441, 459). (c) Parallel pipeline paths (incremental vs full, every write path): needs knowledge that N functions share a responsibility. Co-change partners or declared invariants, noisy. | "Sibling pattern" check: when the diff removes or rewrites a pattern, list the remaining instances. Sibling completeness via near-duplicates and co-change (mechanism 2). |
| SWALLOW | high | **High, lexically.** The idioms are few and language-specific. Diff-scoped ratchet on new code. swe-dashboard/94 shows correct swallows and defects look identical, so the rule should require a justification annotation rather than ban the idiom. | forbidden_patterns with annotation-to-waive, diff-attributed (mechanism 6, promoted). |
| UNWIRED | low count, **high cost** (2 prod escapes) | **High.** A new pub symbol with zero callers at close time. | Orphans the diff creates (mechanism 5). Needs correct dead-code resolution first. |
| PREMISE | medium | **None structurally.** It's process: the verification protocol, and reading the source instead of recalling it. Lessons can surface known traps. | Out of scope for graph checks. |
| TESTVAC | medium | Low. A few lexical smells (loop over a collection with no non-empty assert; skip-on-missing-fixture). The real tool is red-green / mutation verification. | Maybe review-time only. |
| REFAC | low | Low. Contract diffing is hard. The CLAUDE.md discipline already covers it. | None. |
| DEBRIS | low | Low–medium (stale doc references to renamed symbols). | Not first. |
| LAYER | ~0 | High, but no evidence of bug cost. | Keep the existing constraints; build nothing new. |
| Accretion | 0 | High (complexity is easy to measure). | **No bug evidence.** See below. |

## Implications for step 1

1. **Build for the divergence family first (DUP + PAR, 44% of AI-pattern
   bugs, most of the long chains).** Two cheap, precise mechanisms fall out
   of the data:
   - "This already exists": similarity of new functions at guard/review
     time. It would have fired on the dual engine (456/460), the copied
     pipeline (explore/54), the duplicated fork (ai/197), and the edge
     extractors.
     **Back-tested (sutra/463, [dup-exists-backtest.md](dup-exists-backtest.md)):
     GO as a review-time advisory grouped by matched file; NO-GO at the
     guard.** It fires on 6 of 23 pinned DUP introductions: 6 of the 9 where a
     similar body existed, 7 of 9 with same-change comparison. That includes
     explore/54, ai/197, sutra/418 and 459. It misses 460, a 2-line inline
     reimplementation, and all 6 list-shaped cases. On a held-out sample it
     fires on 42% of added functions: 20% real duplicates, 70% accurate but
     idiomatic siblings, 10% noise. `sutra_similar`'s default strip mode is
     unusable for this; the check uses embed + lexical + rare shared token
     runs.
   - "You fixed 1 of N": when a diff rewrites a pattern at one site, list
     the surviving instances. It would have fired on yojana/47, sutra/283,
     sutra/441 and sutra/459. This mechanism was not on the candidate list
     and may have the best precision.
     **Back-tested (sutra/462, [sibling-pattern-backtest.md](sibling-pattern-backtest.md)):
     GO as a review-time advisory.** It fired on all 4 of those diffs, plus 3
     sites that later became their own fixes (sutra/261, 461). It misses
     additive PAR by construction (0 of 4). 29 of 31 fresh ordinary commits
     stayed silent. Per-item precision is low (about 1 real and 3 relevant per
     13 items), but items are rare and cheap to dismiss.
     Co-change partners (review `behavioral_coupling`) don't cover the
     additive gap either: 0 of 4, and 3 of those misses are in the diff's own
     file ([behavioral-coupling-backtest.md](behavioral-coupling-backtest.md),
     sutra/476). Additive PAR needs symbol-level sibling knowledge.
2. **SWALLOW is the second target**, and cheap: forbidden_patterns already
   exist. It needs per-language idiom sets, diff attribution, and
   annotation-based waivers.
   **Back-tested (sutra/465, [swallow-ratchet-backtest.md](swallow-ratchet-backtest.md)):
   GO in two tiers, waived by a `// swallow: <reason>` comment.** All 14
   lexical-idiom bugs fire on their offending line (16 of 22 pinned rows).
   The misses are the non-lexical absence-reads-as-zero cases (402, 408,
   423), log-and-default arms, and a permissive `Option` fallback.
   - Tier A (10 rules: `.ok()`, `let _ = call`, `Err(_)`, `if let Ok` with no
     else, `catch (_)`, `valueOrNull` …): 18 of 19 held-out sites were real
     error discards (4 defects, 12 boundaries worth a sentence), at 0.5 sites
     per ordinary commit. Blocking at the guard.
   - Tier B (`unwrap_or*` on a call, catch-all, `tryParse ??`): 5 of 11 were
     `Option` false matches. Advisory at review only.
   Prerequisites: a `justify` marker field, added-line attribution in
   check/review, and a guard match key that survives renames.
3. **UNWIRED is rare but escapes to prod.** Worth doing once dead-code
   resolution is correct. Until then it's noise.
4. **Deprioritise the growth gate and layering.** Neither shows up as a bug
   root cause. That doesn't prove giant functions are harmless: root causes
   name proximate mechanisms, and accretion could make PAR more likely by
   hiding the sibling branch. But no evidence here justifies building a
   growth gate before the divergence and swallow mechanisms.
5. **Over-reuse is a real counter-mode** (REFAC: sutra/282, 285,
   swisseph-rs/165: reusing a helper whose cost or semantic contract didn't
   fit). A "this already exists" nudge must say *what* exists and let the
   agent judge fit, not push blind reuse.
6. **The health scoring layer is a net bug producer.** 25 of sutra's 89
   bugs came from maintaining freshness, validity and comparability
   contracts for scores that the vidhi/review/5 pilot found 17% actionable.
   This is direct evidence for step 2's freeze/delete disposition.

## Caveats

- **One classifier, one pass.** DUP/PAR and SWALLOW/PREMISE boundaries are
  fuzzy. Treat counts as ±20%, and rankings between adjacent modes
  (PAR ≈ DUP ≈ SWALLOW) as ties.
- **Writer-vocabulary bias.** Root causes written after a pattern became a
  known lesson ("the parallel-site drift pattern", yojana/47) are easier to
  classify into that mode. PAR may be inflated by naming.
- **Review-selection bias.** Most bugs were found by agent or model review,
  so the population over-represents review-findable mechanisms and
  under-represents silent long-lived defects.
- **Category leakage.** Only `category=bug` was mined. Fixes filed as
  enhancements, and bugs fixed inside a feature task without their own
  ticket, are missing.
- **Sutra skew.** 89 of 235 incidents are sutra, and 25 of those are the
  health subsystem. Excluding health, sutra's AI-pattern mix still leads
  with PAR and DUP.

## Baseline metric

Going forward: count closed bugs per primary mode per month, per project,
classified the same way. The baseline is the table above
(2026-05 to 2026-09). The step-1 mechanisms succeed if PAR, DUP and SWALLOW
counts fall on the repos where they're enabled, relative to DOMAIN, which is
the control.
