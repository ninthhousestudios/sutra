# Swallowed-error ratchet: design + back-test (sutra/465)

Step 1 of sutra/457. SWALLOW (fail-open, swallowed errors, a failure that reads
as valid data or as zero) is 25 primary / 39 any AI-pattern bugs in
[ai-failure-modes-evidence.md](ai-failure-modes-evidence.md). This doc picks
per-language idiom sets and a waiver convention, then back-tests them against
the commits that introduced known swallow bugs and against ordinary work.

The harness is [`experiments/swallow-ratchet/`](../experiments/swallow-ratchet/).
A small Rust binary links sutra's real `check_forbidden_patterns` and
`subtract_multiset`, so the matching and introduced-only semantics are exactly
what the guard runs. A Python driver adds git plumbing, added-line attribution
and the justification-comment check. Reproduce every number with
`experiments/swallow-ratchet/run.sh`. `candidates-v1.toml` is the first
iteration; `candidates.toml` is the final rule set.

## Verdict

**GO, as a diff-scoped ratchet with a justification-comment escape, in two
tiers.**

- **Recall: all 14 lexical-idiom bugs fire on their offending line** (16 of
  22 pinned rows). That covers yojana/42 and 47, sutra/147, 301, 417, 424,
  432 and 441, swisseph-rs/151, adityas/ai/104 and 141, and swe-dashboard/90,
  94 and 116. The six misses are three non-lexical cases (sutra/402, 408,
  423; expected), two log-and-default arms (`Err(e) => { warn!(…); default }`,
  secondary sites of yojana/47 and sutra/408), and one permissive `Option`
  fallback (adityas/backend/66).
- **Tier A (10 rules) is precise enough to gate on.** On a held-out draw, 18
  of 19 tier-A sites were real error discards: 4 defects, 2 ambiguous, and 12
  true boundaries that deserve a one-line reason. 1 was a false match. Volume
  is 0.50 sites per ordinary commit, and 21% of commits trip at least one
  (sutra 30%, swe_dashboard 12%, explore 8%, yojana 0%).
- **Tier B (4 rules) is advisory only.** On the same draw, 5 of 11 tier-B
  sites were false matches: `Option` defaults that tree-sitter cannot tell
  from `Result` defaults. Tier B is still needed for recall. yojana/42 and 47,
  sutra/417 and swisseph-rs/151 are all `unwrap_or*` on a `Result`.
- **The draws caught live and historical defects.** Examples:
  `Pattern::new(..).ok()` in `guard.rs` (one of the seven sites sutra/147
  later fixed), two `filter_map(|r| r.ok())` row droppers (the sutra/441
  shape), `from_str(..).ok().unwrap_or_default()` on stored JSON (the
  yojana/42 shape), `_errors` discards (the sutra/301 shape), and a
  git-failure-reads-as-no-churn fallback (the sutra/408/417 shape).

Precision "defect only" is about 21% held-out (4 of 19 in tier A). That is not
the number that justifies the rule. **swe-dashboard/94 showed that correct
swallows and defects look identical at the call site (15 of 16 catches were
correct), so the rule's job is to force a decision, not to find defects
unaided.** For tier A, 95% of hits are sites where the decision is real: fix
it, or write the sentence that says why silence is correct.

## Idiom sets

All rules capture one token, so the finding's line is the idiom's own line.

### Tier A: gate (blocking, with comment escape)

| Rule | Shape | Notes |
|---|---|---|
| `rs-ok` | `.ok()` with no args | `Result`→`Option` in practice. Includes `.ok()?`, which still drops the error value. |
| `rs-let-underscore` | `let _ = <call \| .await \| ?>` | Excludes macro values: `let _ = writeln!(string, …)` is infallible. |
| `rs-underscore-err-binding` | an identifier `_e`, `_err`, `_errs`, `_error`, `_errors` | The sutra/301 `let (x, _errors)` shape. |
| `rs-if-let-ok-no-else` | `if let Ok(..) = … { }` with no `else` | |
| `rs-let-ok-else` | `let Ok(..) = … else { … }` | |
| `rs-err-discard-arm` | `Err(_)` / `Err(_x)` pattern | Match arms and nested patterns. |
| `rs-closure-discards-err` | `.unwrap_or_else(\|_\| …)`, `.or_else(\|_\| …)`, `.map_or_else(\|_\| …, …)` | The closure's arity proves `Result`: `Option`'s versions take no argument. |
| `dart-empty-catch` | `catch … {}` | The existing house `no-silent-empty-catch` rule, unchanged. |
| `dart-catch-underscore` | `catch (_)` whose body does not `rethrow`/`throw` | |
| `dart-value-or-null` | `.valueOrNull` | Riverpod `AsyncValue`: error reads as loading/absent. |

### Tier B: advisory, review-time only

| Rule | Shape | Why not tier A |
|---|---|---|
| `rs-unwrap-or-default` | `<call>.unwrap_or_default()` | Same method on `Option`. The receiver must be a call, and the rule skips `Option` adapters (`get`, `cloned`, `copied`, `max`, `first`, `find`, `next`, `to_str`, `file_name`, …). |
| `rs-unwrap-or-lit` | `<call>.unwrap_or(<literal \| path \| -n>)` | Same receiver filter. That filter removed 8 of 9 false matches in the tuning draw (optional tool arguments, `map.get(k).copied()`, `max()`). |
| `dart-catch-all` | `catch (e)` with no `on T`, whose body does not rethrow | Also matches catches that surface the error (for example a snackbar). |
| `dart-tryparse-default` | `T.tryParse(x) ?? <v>` | All labelled hits were UI text-field defaults. |

`dart-catch-error` (`catchError`/`onError`) was dropped: it had zero hits in
787 commits.

### Known misses (not lexical)

- **Absence reads as zero** (sutra/402, 408, 423). A score or an outcome
  computed from missing data. No token marks it.
- **Log-and-default.** `Err(e) => { warn!("…{e}"); Vec::new() }` binds and
  logs the error, so every rule above passes it. For the consumer it is still
  a swallow. A rule for "an `Err` arm whose value is a default constructor"
  is possible, but it was not tried here.
- **Permissive `Option` fallbacks** on a security boundary
  (adityas/backend/66: `access_until.unwrap_or_else(Utc::now)`). This is
  semantic.

## Waiver: a justification comment, not a silencer

**Convention.** `// swallow: <reason>` on the match line, or in the
contiguous comment block directly above it. The reason must be non-empty.
Dart uses the same marker. The existing Dart `no-silent-empty-catch` rule
already takes a comment inside the braces as its escape, and it stays as is.

**Mechanism.** Add a new optional constraint field `justify = "swallow:"` on
`forbidden_pattern`. `check_forbidden_patterns` would drop (or partition as
justified) any match whose line or preceding comment run carries the marker
followed by text. Because the guard and `sutra check` both call that one
function, both surfaces agree. It is a property of the rule, so it does not
silence any other rule. This fits the house rule against lint silencing. The
comment does not turn a check off; it records the decision the check asks
for, in the diff, where a reviewer reads it.

**Strongest rejected alternative: instance waivers in `accepted.toml`**
(`sutra_constraints action=waive/ack`). It was rejected for three reasons:

1. **Volume.** Tier A trips about 0.5 sites per commit. An out-of-band entry
   per site bloats a tool-owned file and makes waiving the path of least
   resistance.
2. **Scope.** Symbol and file waivers blind the whole scope to future
   swallows. Instance acks are report-only, and the guard ignores them by
   design (sutra/305).
3. **Distance.** The reason belongs next to the code it justifies. A future
   reader of `.ok()` sees `// swallow: env var unset is the normal case`,
   not a row in a TOML file.

`unsafe-requires-waiver` deliberately chose the opposite (an out-of-band
waiver over a "ritually pasted SAFETY comment"). That works because `unsafe`
is rare. At swallow volume the ritual-comment risk is real, and it is covered
by review, not by moving the reason away from the code: `sutra_review` should
list every justified swallow the diff adds with its reason. A
`// swallow: ok` then shows up as the non-answer it is.

**Also rejected: banning the idioms.** swe-dashboard/94: 15 of 16 silent
catches were correct.

## Diff attribution and trigger points

- **Guard (edit time).** It already has introduced-only semantics: a multiset
  of `(constraint, enclosing symbol, snippet)` over proposed content minus
  disk. So backlog in untouched code never fires.
  **Caveat, observed:** the key includes the enclosing symbol, so *renaming*
  a function re-introduces every match inside it. f9e19e6 (a pure rename)
  "introduced" 2 matches on lines it never added. Before tier A blocks, key
  the guard's multiset per file (drop the enclosing symbol, or treat a
  renamed symbol as the same key). Otherwise a rename forces comments onto
  old code, which is backlog-chasing.
- **`sutra check` / `sutra_review` (commit/review time).** They currently
  scan each changed file **whole** (`evaluate_dd` in
  `constraints/check.rs`), so a one-line edit to a file with 20 old `.ok()`
  calls reports all 20. The ratchet needs **added-line attribution**: keep a
  pattern finding only if its line is one the diff added, which is what the
  harness does. Without that, tier A cannot run at commit time.
- **Report the `@match` capture, not the first capture.**
  `check_forbidden_patterns` reports `m.captures.first()`, which is the
  earliest node by position. A rule that captures the receiver
  (`@r … .unwrap_or`) therefore reports the receiver's line. In a split
  chain that line may not be the one the diff added (sutra/417's
  `.unwrap_or(false)` was reported on the `.map(…)` line above it).
  Preferring a capture named `match` when present fixes it without touching
  existing rules.

## Severity

**Tier A: blocking at the guard, with the comment escape. Tier B: advisory,
listed by `sutra_review` on added lines only.**

The deciding fact is structural. **An advisory forbidden_pattern hit at the
guard is invisible to the agent.** `src/bin/guard.rs` writes non-blocking
pattern findings with `eprintln!`, and a passing PreToolUse hook's stderr is
not shown to the model. (Lessons use `render_advisory_stdout`, which does
reach it.) So "warn at edit time" is not available today. The choice is
between block, and route advisories through the advisory stdout channel
first.

Block wins for tier A:

- 95% of tier-A hits are real decisions.
- The fix is one comment line.
- It matches the house precedents at the same volume: `no-unwrap` and
  `no-silent-empty-catch` are both blocking.

The cost is a denied edit and a retry on about 21% of commits. That is paid
while the context is loaded, which is the cheapest moment. Tier B at 45%
false matches would train agents to paste `// swallow:` on `Option` defaults,
so it must not block.

Prerequisites, in order:

1. The `justify` field.
2. Per-file guard keying (the rename caveat).
3. Reporting the `@match` capture.

Added-line attribution for check/review is needed before tier B is useful.
Adopting tier A without prerequisite 2 is not advised.

## Back-test

The introducing commit for each motivating bug was found from the fix
commit's removed lines, using `git log -S` or blame. A row is a **hit** if a
rule reports a line the introducing commit *added*, at the offending line or
up to 2 lines above it (the receiver-capture offset).

| Bug | Introducing | Idiom | Result |
|---|---|---|---|
| yojana/42 | 3b2cd343fcd9 | `from_str(..).unwrap_or_default()` | HIT `rs-unwrap-or-default` |
| yojana/47 | 2ec4b6985e71 | `from_str(s).unwrap_or_default()` | HIT `rs-unwrap-or-default` |
| yojana/47 | e164449c31b6 | `Err(e) => { warn!; Vec::new() }` | MISS (log-and-default) |
| sutra/147 ×2 | bafae79f27ae | `Pattern::new(p).ok()` | HIT `rs-ok` ×2 |
| sutra/417 | fab5f4894094 | `.map(..).unwrap_or(false)` | HIT `rs-unwrap-or-lit` |
| sutra/301 | 6e08d9c7abec | `let (constraints, _errors) = …` | HIT `rs-underscore-err-binding` |
| sutra/424 | d4f186ff9ba6 | `let _ = self.refresh_health_locked(..)` | HIT `rs-let-underscore` |
| sutra/432 | 2479d4a4e78d | config load `.ok()` | HIT `rs-ok` |
| sutra/441 | 9fa82dc059b7 | `if let Ok(map) = from_str(..) {` no else | HIT `rs-if-let-ok-no-else` |
| swisseph-rs/151 | 8c3cb751f165 | `try_from(..).unwrap_or(Placidus)` | HIT `rs-unwrap-or-lit` |
| adityas/ai/104 | 212d1fcdd4d3 | `env::var(..).ok().and_then(parse().ok()).unwrap_or(183)` | HIT `rs-ok`, `rs-unwrap-or-lit` |
| adityas/backend/66 | db1aafa82bfd | `access_until.unwrap_or_else(Utc::now)` | MISS (permissive `Option`) |
| adityas/ai/141 | 4ccebe3fc7f3 | `catch (_)` in cancel | HIT `dart-catch-underscore` |
| swe-dashboard/90 | 67f59436a54b | bare `catch (_)` above probes | HIT `dart-catch-underscore` |
| swe-dashboard/94 ×2 | f1950a06ff47, 58f3e430babf | `catch (_) {}` | HIT `dart-empty-catch`, `dart-catch-underscore` |
| swe-dashboard/116 | 1503bae | `ref.watch(atlasProvider).valueOrNull` | HIT `dart-value-or-null` |
| sutra/402 | b7f0c00dded7 | missing analysis → zero deduction | MISS (non-lexical) |
| sutra/408 | 5072089a6cb7 | `has_git: commit_file_count()? > 0` | MISS (non-lexical) |
| sutra/408 | 2cd563762532 | `Err(e) => { warn!; replace(&[], &[]) }` | MISS (log-and-default) |
| sutra/423 | 2479d4a4e78d | unmeasured → `Complete{0}` | MISS (non-lexical) |

Pinning notes:

- sutra/417's introducing commit is sutra/408's fix. The 408 fix introduced
  the naive probe.
- For adityas/ai/104, the fix is in `config.rs`. The `i32::try_from(..)
  .unwrap_or(i32::MAX)` quoted in the task is still present in `store.rs`.
- For swe-dashboard/116, the fix commit hardened the atlas load. The swallow
  it named (`LocationSearchField`'s `valueOrNull`) was introduced a day
  earlier, in 1503bae.

## Noise

Three measurements. The pool is every non-merge commit since 2026-06 that adds
at least 15 lines to non-test `.rs`/`.dart` files, excluding the introducing
commits: 787 commits across sutra (419), swe_dashboard (199), explore (145) and
yojana (24).

**1. Seeded 26-commit sample** (`sample.py`, seed 465: sutra 10, yojana 4,
swe_dashboard 7, explore 5). The final rules fire on 5 commits (21 are
silent), with 8 sites. Labels:

- **3 boundaries:** a fail-open guard read; `_parseError` falling back to a
  status-code message; `to_value(&lessons)`, which is effectively
  infallible, though `.expect` would be better.
- **1 questionable:** `jd_utils` returns the unconverted JD on a conversion
  failure.
- **1 log-only UI swallow:** `debugPrint` on a saved-charts fetch failure.
- **2 false matches:** an `Option<String>` default, and a catch-all that
  shows a snackbar.

The one guard-semantics hit on lines the commit never added came from a pure
rename (f9e19e6, v1 rules).

**2. Volume over the whole pool** (`volume.py`, added-line hits only):

| Rule | v1 hits | final hits | commits (final) |
|---|---|---|---|
| rs-ok | 179 | 179 | 84 |
| rs-unwrap-or-default | 116 | 115 | 68 |
| rs-unwrap-or-lit | 187 | 83 | 56 |
| dart-catch-all | 103 | 74 | 42 |
| rs-if-let-ok-no-else | 55 | 55 | 27 |
| dart-catch-underscore | 57 | 55 | 31 |
| rs-err-discard-arm | 39 | 39 | 28 |
| dart-tryparse-default | 29 | 29 | 6 |
| rs-let-underscore | 35 | 24 | 14 |
| rs-let-ok-else | 18 | 18 | 17 |
| dart-empty-catch | 16 | 16 | 8 |
| rs-closure-discards-err | 13 | 13 | 7 |
| rs-underscore-err-binding | 8 | 8 | 3 |
| dart-value-or-null | 4 | 4 | 4 |
| **any** | 859 hits, 244 commits | 712 hits, 218 commits (639 distinct sites) | |

Tier A: 396 sites in 162 of 787 commits (21%). Tier B: 300 sites in 144
commits.

**3. Labelled draws.** Labels:

- **D:** a real defect, where the error should surface.
- **?:** ambiguous (a NaN or empty sentinel in the value domain).
- **B:** a true boundary, where a one-line justification is the right
  outcome.
- **F:** a false match, not an error discard at all.

- *Tuning draw* (40 sites from the v1 volume, seed 4650,
  [`labels-v1.tsv`](../experiments/swallow-ratchet/labels-v1.tsv)): 11 D,
  2 ?, 14 B, 13 F. The refinements were designed on this draw. The final
  rules keep all 11 D and drop 11 of 13 F, at the cost of 1 B. That is an
  optimistic number by construction.
- *Held-out draw* (30 sites from the final volume, seed 7, excluding tuning
  sites, rules frozen before the draw,
  [`labels-heldout.tsv`](../experiments/swallow-ratchet/labels-heldout.tsv)):
  6 D, 2 ?, 16 B, 6 F. By tier:

| | D | ? | B | F | not F |
|---|---|---|---|---|---|
| Tier A (19 sites) | 4 | 2 | 12 | 1 | 95% |
| Tier B only (11 sites) | 2 | 0 | 4 | 5 | 55% |

The dominant true-boundary class in Rust is tree-sitter `utf8_text(src)`
(`.ok()`, `if let Ok`, `Err(_) => return`) on source already known to be
UTF-8. That accounts for 4 of the 16 held-out B, and 5 of the 14 in the
tuning draw. A parser-module scope
exclusion, or one helper that returns `&str` with an
`.expect("invariant: source is UTF-8")`, would remove the largest single
source of justification comments. That is a local fix, not a rule change.

## Side findings

- **The vidhi Rust catalog's prose-only note is wrong.** `rust.toml` says
  swallowed `let _ =` is "covered better by clippy::let_underscore_must_use
  in the workspace lints baseline". None of sutra, yojana, adityas/backend or
  swisseph-rs enables that lint, and sutra/424 (`let _ = refresh…`) is what
  it would have caught. `rs-let-underscore` replaces the claim. Alternatively,
  enabling the lint is a cheap complement, because it sees types.
- **Live swallow sites at HEAD, noted and not chased** (ratchet policy). The
  draws surfaced these D-labelled sites, some of which may still exist:
  - `db/components.rs` (`from_str(..).ok()`)
  - `lessons.rs` and `db/constraints.rs` (`filter_map(|r| r.ok())`)
  - `tools/impact.rs` (`distinct_languages().unwrap_or_default()`)
  - `components/anchors.rs` (`git_churn(..).unwrap_or_default()`)
  - `conventions/pipeline.rs` and `tools/review.rs` (`let _ = db.…`)
  - `constraints/external.rs` (`let Ok(paths) = glob::glob(..) else { continue }`)
  - `git.rs` (`parse().unwrap_or(0)`)
  - `parser/typescript.rs` (`from_str(language_attrs).ok()`)

  The `_errors` discard also survives at `guard.rs:1007`,
  `tools/constraints.rs:369,412` and `pipeline.rs:1275`. They get fixed when a
  task touches them, or when one causes a bug.
- **A `\b` in a tree-sitter query string is not a regex word boundary.** The
  query parser consumes the escape, so `#not-match? @b "\\b(rethrow)\\b"` in a
  TOML literal never matches. Use a plain alternation.
- **A post-fix reintroduction was a false alarm.** `Pattern::new(glob).ok()`
  in `external.rs` (02e2708) came six weeks *after* the sutra/147 fix. It
  operates on a segment of a glob already validated at parse time, so it is
  unreachable. That is exactly the site a `// swallow:` sentence is for.
