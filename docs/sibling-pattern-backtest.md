# "You fixed 1 of N": sibling-pattern check (sutra/462)

Step 1 of sutra/457. The evidence base ([ai-failure-modes-evidence.md](ai-failure-modes-evidence.md))
ranked PAR (incomplete propagation) as the most frequent AI-pattern bug mode.
Its cheapest subtype: a diff rewrites an idiom at one site, and the same idiom
survives at sibling sites. This doc designs a check for that subtype and
back-tests a prototype against known fixes and ordinary commits.

Prototype: [`experiments/sibling-pattern/proto.py`](../experiments/sibling-pattern/proto.py)
(regex tokenizer, about 1.5 s per commit on sutra). Reproduce every number
below with `experiments/sibling-pattern/run.sh [--explain]`. Earlier
iterations can be reproduced with `--v2` … `--v5`.

## Verdict

**GO, as a review-time advisory for the rewrite/wrap subtype only.** It is not
a PAR detector in general.

- **Recall on the target subtype: 4 of 4.** It fired on the diff that caused
  each of yojana/47, sutra/283, sutra/441 and sutra/459. The replays also
  flagged **3 more sites that later needed their own fixes**: two `check.rs`
  sites fixed by sutra/461, and `symbol_diff.rs::language_for_path`, fixed by
  sutra/261 and flagged by both language-list replays.
- **Recall on additive PAR: 0 of 4, by construction.** sutra/220, 455 and 296
  and adityas/ai/115 were fixes that *added* a guard or path at one site. A
  removed-pattern check cannot see them.
- **Noise on ordinary work (final, never-tuned sample): 29 of 31 commits
  silent. 3 items, all noise.** Across all 92 ordinary commits, v6 reported on
  9, with 13 items: **1 live defect** (yojana `done --commit` ref clobber), 3
  sites worth a look, and 8 noise. An earlier iteration (v4) also caught a
  second live yojana defect that v6 trades away (see the v6 labels).

Precision per reported item is low: 2 real items (one defect) and 3 relevant
out of 13 across the noise samples. The rate of reports is also low, about one per ten
ordinary commits. Every item is a one-line glance ("this idiom you just
removed survives at X"), so a false item costs seconds. A true one saved a
follow-up bug task in every back-test case. That asymmetry is the case for
GO. It is not a case for blocking.

## Design

### What "the pattern" is

The pattern is a small, named **idiom**, not an arbitrary token n-gram. v1 used
k-gram shingles over whole removed hunks. Incidental boilerplate
(`} x.insert(…)`, `.as_deref()`) made each shingle rare, so three of the four
fixes reported 29, 64 and 80 sites. Only one of the four known targets
appeared in the top 10.

The prototype extracts four idiom kinds from each removed hunk's lines. Locals
don't matter, because only callees, fields and literals are kept.

| Kind | Shape | Example (from the back-test) |
|---|---|---|
| `chain` | `a(…).b(…)`, where at least one of a and b is not a generic std method | `serde_json::from_str.unwrap_or_default` |
| `argfld` | `f(… x.field …)`, where f is non-generic **and** some `f(…).g` chain was rewritten in the same diff | `serde_json::from_str(.category_scores)` |
| `litset` | string literals in value position within 30 tokens of each other, one item per removed hunk | `{"dart", "rust"}` |
| `sql` | the first 40 chars of a string literal of 30 or more chars | `"SELECT id, constraint_id, constraint_na…` |

Each extracted idiom is then classed:

- **rewritten**: the diff lowered its count in the files it touched.
- **wrapped**: the count is unchanged, the idiom was re-added near where it was
  removed, **and** the diff added a new non-generic call nearby. This is old
  code kept inside a new branch. sutra/308 kept the direct `SELECT` as the
  "fresh" arm of a new `is_cache_fresh_conn` branch; that is the diff that
  produced sutra/459.
- **moved**: the count is unchanged and the idiom was re-added elsewhere.
  Dropped: a move is not a fix.

A **survivor** is an occurrence in the post-commit tree that meets all of these:

- same language as the removed hunk
- outside test paths and `#[cfg(test)]` tails
- not on a line the diff added

### Noise controls

Every control below was measured, not assumed. Each one fixed a failure seen
in a sample:

| Control | Failure it fixed |
|---|---|
| Repo-wide pre-commit DF ≤ 40, survivors ≤ 25 | Ubiquitous idioms. |
| Generic std method list (`iter`, `map`, `unwrap_or`, `extension`, `elapsed`, …) | `.iter.map`, `.entry.or_insert` and similar dominated v2. |
| Literals excluded when used as map keys (`"k":`, `insert("k".into()`), format templates, or punctuation | JSON-key pairs such as `"from"+"to"`. |
| Hunks whose removed and added token multisets match, ignoring commas | The rustfmt sweep c7e0e98 reported 108 items. |
| Pure-deletion hunks skipped | Deleted features (b5952ba, 348d4c9): 36 and 28 items. |
| One `litset` item per hunk instead of pairs | N literals gave N² pairs (status lists). |
| Sweep suppression: removed at ≥ 3 hunks | Enum sweeps, whose survivor is the canonical mapping. |
| A wrap must add a new call nearby | A call re-added with a renamed local (`f.path` → `s.file.path`). |
| `argfld` needs a rewritten chain on the same callee | "You touched one call to `f`; here are the other callers of `f`." |

### Trigger point: review time

The check runs on the **whole change**: `sutra_review` / `sutra check --diff`
at close, advisory, never blocking.

**Rejected: the edit-time guard.** This was the strongest alternative, since
it catches the miss before the agent moves on. It fails on mechanism, not
taste:

1. The rewritten/wrapped/moved classification needs the whole diff. A move's
   removal edit looks exactly like a rewrite until the matching addition lands
   in another file, and moves were the largest v2 noise source (ac12896
   decomposition, c7e0e98).
2. Sweep suppression needs the hunk count across the change. Mid-sweep, the
   guard would list the very sites the agent is about to edit.
3. The guard sees one file's proposed content. The survivor search needs the
   post-change tree.

Edit time could still offer a *checklist* once review-time precision is
established. That is not first.

### Output shape

The output is pattern-centric, not site-centric. The same idiom surviving at
14 sites is one finding, not 14:

```json
{
  "kind": "chain",
  "idiom": "serde_json::from_str(…).unwrap_or_default()",
  "class": "rewritten",
  "removed_at": ["src/display.rs:227"],
  "survivors": [{"file": "src/tools/task.rs", "line": 208, "symbol": "json_array"}, …],
  "survivor_count": 14
}
```

In production each survivor would carry its enclosing symbol from the index.
The prototype does not do this.

## Back-test

Each row replays the diff of the **first** fix in a known PAR pair, at that
commit, and asks whether the later bug's site is among the survivors.

| Pair | Replayed commit | Fired? | Known site | Other survivors (labelled) |
|---|---|---|---|---|
| yojana/42 → 47 | 0b3ff34 | yes, `chain` rewritten | `tools/task.rs:208` `json_array` ✓. `context.rs` helper ✗: it is a different shape (`match` + warn + `Vec::new()`). | 13 more `from_str(…).unwrap_or_default()` on other JSON columns: the same swallow, deliberately scoped out by yojana/47. **Relevant.** |
| sutra/280 → 283 | 91fedc7 | yes, `litset {"dart","rust"}` | `guard.rs:407` `language_from_path` ✓ | `symbol_diff.rs:539` `language_for_path`: **real**, it became sutra/261 (5101b41, 12 days later). `review.rs:246` `extract_outgoing_edges` and `review.rs:481` (six-language literal list): relevant. `similarity/diff.rs:88`: relevant. |
| sutra/283 → 261 | f114fa5 | yes, `litset` | none (this is the fix of 283 itself) | `symbol_diff.rs:539` **real** (261). `review.rs:245–247` relevant: still present at HEAD, but import-edge extraction is rust/dart-only by design. `read.rs:441` noise. |
| sutra/438 → 441 | af68577 | yes, `argfld` wrapped | `trend.rs:589` `aggregate_categories` ✓ | none (v2–v5 also flagged `trend.rs::incomparable_entry`, which still gives basis-less legacy rows a numeric score; v6's argfld rule dropped it). |
| sutra/308 → 459 | d70bf20 | yes, `sql` wrapped | `guard.rs:765` `check_proposed_patterns` ✓ | `check.rs:1109` and `:1284` (manifest and pubspec guard checks): **real**. sutra/461 (3a57d3d) later fixed "the same stale-cache gap" in exactly these. |
| sutra/436 → 455 | 6e5d60e | no | the workspace-aggregate path | Additive fix: the component path gained weight-split logic and nothing was removed. Out of scope. |
| sutra/219 → 220 | ac25e6e | no | none | Additive guard (`continue` on unresolved ids), and a different staleness mechanism. Out of scope. |
| sutra/292 → 296 | 723ac70 | no | none | Additive escape hatch in one consumer. Out of scope. |
| adityas/ai/110 → 115 | a06a497 | no | `engine.rs` `ai.tool` log | Additive `record_tool` call. Out of scope. |

## Noise on ordinary commits

The samples are seeded random non-fix commits that remove at least 5 non-test
source lines ([`sample.py`](../experiments/sibling-pattern/sample.py)), drawn
from sutra, yojana and adityas/backend. Each reported item is labelled:

- **real**: a site that should have changed, confirmed by a later fix or by
  reading the code at HEAD.
- **relevant**: the same idiom, worth a glance, but legitimate or a judgment
  call.
- **noise**: everything else.

### How the numbers moved (items / commits with any report / commits)

| Version | Tuning (seed 462) | Held-out (seed 7) | Final (seed 99) |
|---|---|---|---|
| v1 k-gram shingles | 4, 29, 64 and 80 sites on the four back-test fixes; not run on samples | — | — |
| v2 idiom features | 159 / 14 / 23 | — | — |
| v3 generic + key-literal + move/format filters | 11 / 6 / 23 | 127 / 18 / 38 | — |
| v4 deletions, litset, sweeps | 8 / 5 / 23 | 27 / 16 / 38 | — |
| v5 wrap needs a new call | 4 / 3 / 23 | 18 / 9 / 38 | — |
| **v6 argfld needs a rewritten chain** | **4 / 3 / 23** | **6 / 4 / 38** | **3 / 2 / 31** |

The tuning sample was used to design v3. The held-out sample was clean for v3,
then used to design v4–v6. **Only the final sample is an unbiased estimate**,
and v6 was frozen before it was drawn. v3's held-out collapse (127 items) is
the honest measure of how much tuning-set overfit there was.

### v6 labels

| Sample | Commit | Item | Label |
|---|---|---|---|
| tuning | yojana c0b57ea (typed ContextRef) | `from_str(.context_refs)` and the `from_str.unwrap_or_default` chain, both pointing at `main.rs:457` | **real**. The CLI `done --commit` path still parses `context_refs` with `unwrap_or_default`, then pushes and writes back. A malformed column silently drops every existing ref. Live at HEAD (`main.rs:503`). |
| tuning | backend 7dadc4b (configurable CORS) | `{"https://84beings.com", …}` → `origin_guard.rs:6` | relevant: a second hardcoded origin list mirroring the CORS config |
| tuning | backend b46afde | the same origin set | noise (the rewrite was parse→`from_static`) |
| held-out | yojana 4b7f815 (enum schema) | `{"AFK","HITL"}` → `db.rs:493` | relevant: a literal list mirroring `SliceType`, still at HEAD `db.rs:508` |
| held-out | yojana 7e77aab (TaskStatus sweep) | `{"done","wontfix"}` → `context.rs:344` | relevant: stringly status survived the enum sweep, still at HEAD |
| held-out | yojana 7e77aab | two more litsets → `state.rs` mapping | noise (canonical `Self::X => "x"` mapping) |
| held-out | yojana 513bb17, 7c9671b | table headers; "unknown action" text | noise |
| final | yojana 5fc5716 (fmt) | `get_arc_by_uuid.ok_or_else` | noise (format residue across split hunks) |
| final | yojana 16b954d (TaskStatus enum) | two litsets → `tools/task.rs:78–83` | noise (canonical mapping) |

v5 also caught **yojana dd8dc0a**: it added a `production` project status but
left `tools/project.rs:9` `VALID_STATUSES = ["active","paused","archived"]`.
Live at HEAD: `yojana_project` create and update reject `production` while the
DB layer accepts it. v5's wrap rule dropped it, because the list was re-added
with no new call. That is the recall this precision cost.

## Next noise controls (untested)

1. **Canonical-mapping survivors.** Drop a `litset` survivor whose literal sits
   in a `Path::Variant => "lit"` or `"lit" => Path::Variant` arm. This is the
   remaining final-sample noise class (the enum-sweep leftovers). Check it does
   not drop `guard.rs:407` (`Some("rust")` in an if-chain, not an arm).
2. **Group items by survivor set.** Two idioms pointing at the same site
   (c0b57ea) are one finding.
3. **Recover dd8dc0a.** A `litset` whose set *grew* (the added side is a
   strict superset of the removed side) is a list extension, and survivors of
   the old list are precisely the missed sites. Treat it as rewritten.

## Productionizing notes

- **Substrate already in sutra.** `tools::symbol_diff`:
  - `classify_symbols` detects cross-file moves (body hash) and cosmetic
    reformats (structural hash). That replaces the prototype's move and
    format heuristics with something exact.
  - `callee_diff` gives per-symbol added and removed callees. That is the
    `chain`/wrap signal from tree-sitter rather than regex.
  - `sutra_review` already resolves the diff spec.
  - The survivor search and DF counts belong on the index (tree-sitter call
    nodes), not a re-tokenize of the tree.
- **Language scope.** The prototype saw Rust only; its Dart paths are
  untested. The generic-method list is per language.
- **Not covered, by design:** additive PAR (half of the back-test pairs).
  That needs knowing that N sites share a responsibility (co-change partners,
  near-duplicate siblings), which is mechanism 2 in the evidence doc.

## Caveats

- **Small, author-labelled back-test.** One labeller (Claude). "Real" was
  confirmed by a later fix commit or by reading HEAD. "Relevant" is judgment.
- **Rust only in practice.** All three repos are Rust.
- **The generic list is the weakest part.** It is a stoplist; a new codebase's
  own ubiquitous helpers will read as idioms until DF catches them.
- **Regex tokenizer.** Raw strings, nested generics and macros are
  approximated. Production should use the tree-sitter parse.
