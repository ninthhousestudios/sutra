# "This already exists": similarity check of added code (sutra/463)

Step 1 of sutra/457. The evidence base ([ai-failure-modes-evidence.md](ai-failure-modes-evidence.md))
found that DUP (logic re-implemented instead of reused, then the copies drift)
is 28 primary / 36 any AI-pattern bugs. This doc designs a check that tells the
agent "code like this already exists at X" and back-tests it against the
commits that introduced known duplicates and against ordinary commits.

Harness: [`experiments/dup-exists/`](../experiments/dup-exists/). `bt.py` parses
each commit and its parent with the real `sutra parse` in scratch worktrees,
dumps the HRR vectors and bodies of every function, and ranks. Reproduce every
number below with `experiments/dup-exists/run.sh [-v]`. Labels are in
`labels.tsv` (tuning sample) and `labels-held-out.tsv`.

## Verdict

**GO as a review-time advisory, grouped by matched file. NO-GO at the
edit-time guard for now.** It catches the copy-paste subtype of DUP. It does
not catch DUP in general.

- **Recall on the motivating set: 6 of 23 pinned introductions fire.**
  Restricted to the 9 cases where a similar body existed when the copy was
  written, 6 of 9 fire and all 9 rank in the top 3 on some channel. Comparing
  added code against other added code in the same change adds sutra/456, for
  7 of 9. The other
  14 cannot be seen by any function- or block-similarity check (table below).
- **List-shaped DUP: 0 of 6 fire.** That class is about half of DUP. It needs
  its own detector (see "List-shaped DUP" below).
- **Noise, held-out sample (rule frozen before it was drawn):** fires on 40 of
  95 added functions (42%) and on 13 of 24 commits. Labels: **8 real
  duplicates (20%), 28 accurate but not actionable (70%), 4 noise (10%).** On
  the tuning sample: 19 real / 9 migration copies / 17 family siblings /
  4 noise, out of 49 items.
- **Real duplicates are common in ordinary work.** In 14 of 53 sampled
  commits, the check found a real duplicate the author wrote. Four are still
  live in sutra HEAD (listed under "Found along the way").

The case for GO is the same asymmetry as [sibling-pattern-backtest.md](sibling-pattern-backtest.md):
an item costs a glance, and one true item stops a drift chain before it starts.
The case against the guard is volume. A note on 42% of new functions is
background noise, not a nudge. Most non-actionable items come in bulk from one
commit: a new language adapter mirrors an old one (17 of 28 held-out FAM), or
a module is cloned. **Grouping by (new file, matched file)** turns those into
one finding each, and that grouping only works on the whole change.

## Design

### Unit: added code, not just added functions

**The unit is every function the change adds, plus the added lines of every
function it modifies.** Only 5 of the 9 detectable introductions were new
functions. The other 4 put a copied block into an existing function
(explore/54 `_openChart`, ai/197 `ChatPill.build`, sutra/456 `evaluate_raw`,
sutra/438 `handle_history`). A new-functions-only ratchet misses all four.

For a modified function, the lexical and block channels use only the added
lines. HRR vectors exist only for whole symbols, so the embed channel sees the
whole post-change function.

Exclusions: test functions and test paths, functions under 5 lines, and
matches in another language. Three suppressions were measured on the samples:

| Suppression | What it drops |
|---|---|
| `delegates` | The new code calls the match. That is reuse, not duplication (`rebuild_rollups` → `compute_rollups`). |
| `gone` | The match no longer exists after the change. It was moved or deleted. |
| `extracted` | The match was edited in the same change and lost the shared runs, or now calls the new function. This is the extraction commit itself: the new helper matches the code it replaced (swe 72ef629 `showInvalidEntry`; the sutra/460 fix `content_import_edges`). |

`gone` and `extracted` need the post-change tree, so only review time can apply
them.

### Signal: three channels, not `sutra_similar`

| Channel | What it is | Verdict |
|---|---|---|
| strip (`sutra_similar` default) | HRR cosine on AST shape, identifiers stripped | **Unusable for this.** Small functions score 0.7–1.0 against unrelated code, and sutra/459's copied SELECT ranks 577th. |
| embed | HRR cosine, AST shape + identifiers | Good for whole-function copies, blind to string literals and to Dart method↔function pairs (see "Similarity quality"). |
| lex | tf-idf cosine over identifier subtokens of the body (signature dropped) | Carries most of the recall. Needs no index change. |
| **combo** | (embed + lex) / 2 | Main ranking score. |
| **block** | count of shared 12-token runs that occur in ≤ 3 corpus functions | Finds a block copied into a larger function (sutra/418, 459, 437) where whole-function cosine is diluted. The df ≤ 3 filter removes tree-sitter cursor-walk and `match head_commit` idioms. |

**Rule: fire when `block ≥ 6` or `combo ≥ 0.5`.** Show up to 3 matches per
added function that clear the rule. A back-test case counts as fired only if
the original is among them.

| Rule | Back-test fires (of 9 detectable) | Tuning sample: functions / commits | Held-out: functions / commits |
|---|---|---|---|
| combo ≥ 0.5 | 3 | 39 / 14 of 29 | 32 / 13 of 24 |
| block ≥ 10 | 3 | 30 / 12 | 20 / 6 |
| block ≥ 10 or combo ≥ 0.5 | 5 | 48 / 14 | 38 / 13 |
| **block ≥ 6 or combo ≥ 0.5** | **6** | **49 / 14** | **40 / 13** |
| block ≥ 10 or combo ≥ 0.6 | 3 | 42 / 13 | 29 / 11 |

Raising the combo cut to 0.6 loses explore/54's sibling ai/197 (0.53) and
sutra/261 (0.51) and removes few non-actionable items. Crowd size (how many
corpus functions score ≥ 0.45) was tried as a family-sibling filter and does
not separate: real DB-accessor duplicates have crowds of 45+.

### Trigger point: review time

The check runs on the whole change: `sutra_review` / `sutra check --diff` at
close. It is advisory and never blocks.

**Rejected: the edit-time guard.** This was the strongest alternative. The
function is fully present in the proposed content, and nothing calls it yet.
Latency is not the problem (see below). It fails on three measured
mechanisms:

1. **Same-change copies.** In 3 of the 9 detectable cases (sutra/456, 438,
   ai/197), and in list-shaped swe/91, both copies landed in one commit. At edit time the first
   copy lives only in an earlier proposed edit. The guard compares against the
   index, which holds the last parse. It would see the second copy only if the
   index were refreshed between edits. Review time sees both sides of the
   diff.
2. **Extraction noise.** When an agent extracts a helper, the helper's first
   edit matches the code it is about to replace. Only the post-change tree
   separates this case (`extracted`/`gone`) from a real copy.
3. **Volume without grouping.** 42% of new functions fire. Per-file grouping
   needs the whole change.

Edit time could come back later as a one-line hint, gated on a stricter rule
and fed by an index refreshed with the session's earlier edits. Not first.

**Guard latency, for the record.** HRR encoding took 1.03 s on 12 cores for
357 functions and 2.8 s for 1701, both including the file re-parse. That is
about 20–35 ms per function for strip + embed. Loading all 2 774 embed vectors
of the sutra index took 24 ms, and a brute-force scan took 40 ms (numpy, not
Rust). The total is well under the ~3 s budget.

### Output shape

Name what exists and why it matched. Never "reuse X": over-reuse is a real
counter-mode (sutra/282, 285, swisseph-rs/165). Group by matched file:

```json
{
  "added": "src/db/similarity.rs::Db::load_all_vectors_by_mode",
  "matches": [
    {"symbol": "Db::load_all_strip_vectors", "at": "src/db/similarity.rs:111",
     "combo": 0.88, "shared_runs": 48,
     "shared": "conn.prepare(\"SELECT symbol_id, vector FROM hrr_vectors WHERE mode = …\")"}
  ]
}
```

```json
{
  "group": "src/parser/python.rs ↔ src/parser/c.rs",
  "added_functions": 17,
  "strongest": [{"added": "parse", "match": "parse", "combo": 0.96}]
}
```

The `shared` excerpt is the longest shared token run. It lets the agent judge
in one glance whether the match is a real copy or an idiom.

## Back-test

The archaeology (pinning the commit that introduced the second copy) was done
per case from the yojana task, its fix commits, `git log -S`/`-L` and blame.
The corpus is the parent's non-test functions. For a same-commit copy, the
first copy's post-commit version is added to the corpus. The table shows the
rank of the original among about 160–1500 corpus functions, with the score in
brackets.

| Case | Intro | Unit | combo | block | lex | Fires? |
|---|---|---|---|---|---|---|
| explore/54 `_openChart` ← `_submitChart` | ddda8cd | modified | **1** (0.55) | **1** (18) | 2 | yes |
| ai/197 `ChatPill.build` ← `_ChatPanelState.build` | 19c9c3a | modified, same-commit | **1** (0.53) | 1 (1) | 1 | yes |
| sutra/418 `insert_snapshot_atomic` ← `insert_snapshot_files` INSERT | 25bca13 | new | 5 (0.56) | **3** (46) | 8 | yes |
| sutra/459 `check_proposed_patterns` ← `evaluate_raw` waiver SELECT | fdcc825 | new | 13 (0.30) | **2** (48) | 3 | yes (block) |
| sutra/437 `partition_ondemand` ← `publish_run` labelling | 7db1584 | new | 412 (0.11) | **3** (7) | 5 | yes (block) |
| sutra/261 `language_for_path` ← `guard::language_from_path` | 7501bb6 | new | **1** (0.51) | — | 1 | yes: names the sibling hand-rolled copy, not the registry |
| sutra/438 `handle_history` ← `score_value_json` | 7db1584 | modified, same-commit | 1 (0.44) | 1 (4) | 1 | no: rank 1 but under threshold |
| sutra/456 `evaluate_raw` ← `evaluate_dd` fan-in block | d411912 | modified, same-commit | 3 (0.34) | 8 (3) | 16 | no. Understated: the harness compared against the parent's `evaluate_dd`, which did not yet have the block. Added-vs-added, the two new blocks share 93 twelve-token runs, so a review-time check would fire. |
| sutra/401 `query_idents` ← `tokenize` | ed3ad15 | new | 2 (0.28) | 2 (1) | 36 | no: a 7-line re-tokenizer |
| **Out of reach by construction** | | | | | | |
| sutra/460 | a1db624 | inlined 2-line `parent.join + normalize_path` instead of calling the 10-line `resolve_relative_import` | 167 | — | 90 | no. Too small and not a copy. |
| ai/101 | 5fa0501 | the copy *lacks* the validation the original has (absent logic) | 123 | — | 67 | no |
| ai/119 | 7c21ace | two independently written boundary computations, no shared text | 48 | — | 17 | no |
| ai/211 | d55d91b | fn body vs config-field initializers | 270 | — | 35 | no |
| swisseph-rs/159 | a330a4f | hardcoded `&[]` at 2 of 3 sites (a PAR shape) | 181 | — | 15 | no |
| pyswisseph-rs/31 | a5483a4 | original lives in a dependency crate | — | — | — | no: cross-repo |
| ai/84 | 9c38925 | a second construction path that never shared code | — | — | — | not run |
| sutra/394 | 6ff97b3 | SQLite default tokenizer vs a Rust tokenizer | — | — | — | no: not code-vs-code |
| backend/57 | 4758e7e | copied smoke block in a justfile | — | — | — | no: not indexed |
| **List-shaped** | | | | | | |
| sutra/261 vs `LanguageRegistry` | 7501bb6 | match on extensions vs the registry | 122 | — | 4 | no |
| sutra/283 vs `LanguageRegistry` | 3f1e069 | same | 578 | — | 704 | no |
| sutra/280 `KNOWN_LANGUAGE_CATEGORIES` | 6ac455d | a `const` array, not a function | — | — | — | no: out of the unit |
| swe/80 `restoreFromPersistence` ← `loadContextBar` field list | 6be31e5 | modified | 82 | — | 764 | no |
| swe/81 `equinoxShiftMask` ← `flagToggles` | 77e6c09 | a `const` mask | — | — | — | no: out of the unit |
| swe/91 `_buildJdCard` ← `heliacalToExportRows` labels | f1950a0 | new, same-commit | 1 (0.31) | — | 1 | no: rank 1 but under threshold |

manas-cli/9 had no duplicated code to pin (the root cause is `include_str!`
freezing a prompt at compile time). It is excluded.

**What this corrects in the evidence doc.** The step-0 claim that the check
"would have fired on the dual engine (456/460) … and the edge extractors" does
not hold. 460 was a 2-line inline reimplementation of a helper. 456 ranks 3rd
but scores 0.34, because both copies sit inside 250-line engines. The copied
pipeline (explore/54) and the duplicated fork (ai/197) do fire.

**Ranking is good; the threshold does the losing.** The original is in the
top 3 on some channel in all 9 detectable cases. The 3 misses are small or
diluted copies (7-line tokenizer; a block inside a 250-line function). A lower
threshold recovers them but doubles the volume.

## Noise on ordinary commits

The samples are seeded random non-fix commits that add 20 to 1500 non-test
source lines ([`sample.py`](../experiments/dup-exists/sample.py)), drawn from
sutra (Rust), adityas/backend (Rust) and swe_dashboard (Dart). Each fired item
is labelled:

- **DUP**: real duplicated logic. Reuse, or an extracted helper, would have
  been better. Confirmed by reading both bodies. Where noted, it was also
  confirmed by a later removal or by HEAD.
- **MIG**: an accurate copy made during a migration whose original was
  retired later (backend `routes/ai.rs`, retired in 348d4c9). True, not
  actionable.
- **FAM**: an accurate structural sibling in an idiomatic family: DB
  accessors, enum `as_str`, parallel language adapters, `copyWith`/`fromJson`.
  Not actionable.
- **NOISE**: the match is not the same logic (a shared idiom or shape only).

| Sample | Commits | Added fns | Fired | DUP | MIG | FAM | NOISE | Commits with a DUP |
|---|---|---|---|---|---|---|---|---|
| tuning (seed 463) | 29 | 119 | 49 (41%) | 19 | 9 | 17 | 4 | 8 |
| **held-out (seed 7, rule frozen)** | 24 | 95 | 40 (42%) | **8** | 0 | 28 | 4 | 6 |

The held-out FAM count is dominated by df8712e (the Python adapter, 17
functions mirroring the C adapter). Grouped per file pair, that commit becomes
one finding. The tuning sample's DUP count is lifted by 7c2b489, which cloned
the user-ayanamsa module into a sign-set module (7 items).

Representative DUP items:

- swe 7c2b489 `SignNameSelector.build` hand-rolls the decorated dropdown that
  `LabeledDropdown` already provides (38 rare shared runs).
- swe 934028b `_runAtlasDownload` / `_handleAtlasCancel` /
  `_deleteAtlasRelease` are copies of `_runDownload` / `_handleCancel` /
  `_handleDelete`.
- sutra c303793 `probe_owners` is a second reader of `.sutra/owners.toml`
  with different failure semantics. `load_owners_config` was later removed.
- backend e77727a `body_for_name` is the inverse of `body_display_name`'s
  hand-maintained body list. This is a **list-shaped DUP the check does
  catch**, because both mirrors are functions.

## Found along the way

**Live duplicates in sutra HEAD** (all from the samples):

- `Db::load_all_vectors_by_mode` generalises `Db::load_all_strip_vectors`
  (`src/db/similarity.rs:230/260`). Both have callers.
- `Db::insert_hrr_vectors_and_hashes` copies the insert loop of
  `Db::replace_hrr_vectors`.
- `Db::function_symbols_for_hrr_files` copies `Db::function_symbols_for_hrr`.
  `replace_hrr_vectors` and `function_symbols_for_hrr` have no callers outside
  `db/similarity.rs`.
- `accepted_sync_marker_from_conn` runs the same SELECT as
  `Db::get_accepted_sync_marker` (`src/db/constraints.rs:181/484`). One
  swallows errors and the other propagates them: the sutra/461 shape.

## Similarity quality (sutra/407 concern)

- **strip mode, the `sutra_similar` default, is not a duplicate detector.**
  It scores nearly any two small functions of similar shape at 0.7–1.0
  (`Region::label` vs `ReportKind::as_str` 0.99; `resolveUserSignSet` vs
  `resolveActiveFile` 0.80). It ranks known copies 338th–908th (sutra/459,
  437, 460). Use embed or lexical for "does this exist".
- **Dart method vs top-level function: HRR is near-orthogonal even for
  token-identical bodies.** `_ContextDateFieldState._showInvalid` (method) vs
  `showInvalidEntry` (top-level function, same body minus a `mounted` guard)
  gives embed 0.10 and strip 0.14. The same two methods compared with each
  other give 1.00. Lexical scores the pair 0.98. Observed, not root-caused. A
  plausible mechanism is that the root node kind is bound over the whole
  subtree, so different wrapper kinds (method vs function) give unrelated
  vectors. This blinds HRR to the commonest Dart extraction shape (a method
  becomes a free helper).
- **embed ignores literals.** Match arms that map variants to strings score
  0.8 against any other such function (lex 0.0). This is why combo averages
  embed with lex instead of taking the max.

## List-shaped DUP

The function unit misses all 6 list-shaped cases: consts (sutra/280, swe/81),
matches mirroring a registry (sutra/261 and 283 vs `LanguageRegistry`), and
field or label lists inside larger functions (swe/80, 91). A separate
detector would target *a literal collection whose members are a subset of an
enum's variants or a registry's keys*. Examples:

- `["rust","dart"]` ⊂ registered language ids;
- `seFlgSidereal|seFlgJ2000|seFlgNoNut` ⊂ `flagToggles` bits;
- the card labels vs the export labels for the same result type.

The sibling-pattern `litset` extraction
([sibling-pattern-backtest.md](sibling-pattern-backtest.md)) already pulls
these literal sets out of the diff. What's missing is the "mirror of what"
lookup. Sketch only, not built here.

## Productionizing notes

- **Substrate in sutra:**
  - `hrr_vectors` (embed) already exists for every function.
  - `tools::symbol_diff::classify_symbols` gives added/modified/moved
    symbols and body hashes, which replaces the harness's name-based
    `added_functions` and `gone`.
  - `sutra_review` resolves the diff.
- **New in sutra:** a per-function lexical profile (identifier subtokens;
  `lexical_tokenize::tokenize` exists) and a rare-shingle index (12-token
  hashes with df) built at parse time. Both are cheap next to HRR encoding.
- **Same-change pairs:** at review, compare added code against other added
  code too. Three of the nine detectable cases need it.
- **Grouping:** collapse items that share a (new file, matched file) pair.

## Caveats

- **Author-labelled.** One labeller (Claude). DUP vs FAM is a judgment call on
  a few items. The arguable ones are noted in the label files (two severity
  enums; sibling HTTP endpoints). Moving all of them would shift the DUP rate
  by about 5 points.
- **Back-test archaeology was delegated.** A subagent pinned the introducing
  commits. Every commit I ranked was checked by the harness: the named
  functions resolve at those SHAs, and the original exists at the parent or in
  the same commit.
- **Harness shortcuts.** It matches functions by name. The `extracted` flag's
  call check uses the short name, so it wrongly flags ai/197 (both functions
  are called `build`). The flag does not affect ranks. Snapshots are cached
  under `/tmp/bt463`.
- **Two languages.** Rust and Dart only. The Python adapter was a *subject*
  in df8712e, not an indexed language here.
