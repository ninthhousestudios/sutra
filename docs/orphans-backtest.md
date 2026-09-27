# Orphans: symbols a diff adds, or strands, that nothing calls (sutra/483)

Step 1 of sutra/483, from sutra/457. The evidence base ([ai-failure-modes-evidence.md](ai-failure-modes-evidence.md))
found UNWIRED (built ahead of its call site, never connected) in only 8 of
235 incidents. But it caused 2 of the 6 AI-pattern production escapes:
adityas/ai/110 (a telemetry type with zero callers, so every prod metric read
0) and adityas/ai/65 (a client half never wired). This doc back-tests an
advisory that names such symbols at review time, and records what shipped.

Harness: [`experiments/orphans/`](../experiments/orphans/). `bt.py` parses each
commit and its parent with the real `sutra parse` in scratch worktrees and
compares liveness. `replay.py` runs the shipped `sutra check --diff HEAD` on the
same commits. Reproduce everything below with `experiments/orphans/run.sh`.
Labels are in `labels.tsv`.

## Verdict

**GO as a review-time advisory, with two shapes, `added` and `orphaned`. It
never gates, and it does not run in the edit-time guard.**

- **Recall: 3 of 3 reachable incidents fire** (sutra/297, adityas/ai/110,
  arjuna/arrow/24). That is 3 of 8 UNWIRED rows. Of the other five, three
  are out of reach for any symbol-reference check (cross-repo, a field, never
  built as code), one is non-code, and one is not UNWIRED at all (table below).
- **Two of the three reachable incidents are the `orphaned` shape.** The
  symbol had a caller when it was written, and a later change deleted that
  caller. The mechanism as first specified ("symbols the diff adds") catches
  only arrow/24, and it misses the production escape.
- **Noise, held-out sample (24 commits, rules frozen before the draw):
  17 items on 5 commits.** Labels: 9 staged, 1 real orphan, 3 accurate but
  not actionable, and 4 false positives. All 4 false positives came from one
  Dart parser bug, which is now fixed (1c62d8c). After the fix: **13 items
  on 3 commits, 0 false positives.** The median commit shows nothing.
- **The back-test found real, live orphans in three HEADs** (under "Found
  along the way"). One is a bug: sutra's DD idle eviction has been inert
  since May (sutra/496).

Why ship at this precision? Most items are "staged": a symbol built in one
commit and wired in the next commit of the same task, usually within hours.
The finding is true ("nothing calls this yet") and costs a glance. A missed
orphan costs a production escape: ai/110 shipped a metrics path that read 0
everywhere. Volume is low: 3 to 5 commits in 24 show anything, one or two file
groups each. That is within the shared advisory budget (design rule 7 in
[sutra-purpose.md](sutra-purpose.md)).

**Rejected: gating, or the edit-time guard.** This was the strongest
alternative. In a single commit, "nothing calls this" is legitimate most of
the time: 20 of 24 non-false-positive items across both samples were staged.
A blocking check would push agents to wire code before it is ready. The guard
also sees one edit, not the change, so every new function would fire until
its caller's edit lands.

## Design

### Two shapes

| Shape | When it fires | Incidents |
|---|---|---|
| `added` | The diff adds a symbol and no non-test code references it | arrow/24 `ViewSpec.canonicalConfig`: zero callers from birth to deletion |
| `orphaned` | The diff removes a reference, and the symbol it could have bound to now has no non-test reference | sutra/297 `DdEngine::invalidate`: its callers went with the daemon (b9afe94), and a later live call went with the tool consolidation (bcca99d). ai/110 `Telemetry`: its only caller was the preview route that 348d4c9 deleted |

"Added" means the symbol is not in the base side of its file, after
[`resolve_renames`](../src/tools/symbol_diff.rs) removes renames and moves.
"Orphaned" candidates are the index symbols named by a reference on a
removed, non-test line. A qualified reference (`Telemetry::start()`,
`tools::dead::handle`) must name the candidate's type or module file. A
reference that names no type or module (unqualified, `self::`, `crate::`)
counts only when its name has exactly one definition in the index (sutra/499).
Production has no parse of the base tree, so it cannot check that a candidate
"was referenced before". Without the uniqueness rule, removing `helper()`
reported every dead `helper` in the workspace, including ones that were
already dead. The harness does check "was referenced before".

### Liveness

A symbol is live when a reference from non-test code binds to it, or to:

1. **a same-file twin** (same qualified name: getter/setter pairs, `#[cfg]`
   variants), as in `sutra_dead`;
2. **any same-named method, through an unqualified call**, for a method. There
   are no receiver types, so method calls bind by name, as in `sutra_dead`;
3. **one of its members**, for a type. A Dart class used only through static
   members (`CanonIri.rashi(...)`) has refs on the members and none on the
   class;
4. **its class or a sibling member**, for a constructor. Dart extracts every
   constructor as `Class::Class`, including the private `CanonIri._()` that
   exists to be uncallable;
5. **its twin in another URI of the same configurable import**
   (`import 'io.dart' if (dart.library.js_interop) 'web.dart'`). Calls bind to
   one alternative only.

References from test code (test paths, `#[test]`, `#[cfg(test)]`, Dart test
files) do not count. Their number is reported as `test_refs`: "exercised,
never called" is the UNWIRED shape, and sutra/297's `invalidate` had exactly
one test caller.

Never reported: kinds `sutra_dead` skips, `main`, test code, FFI and framework
entrypoints (`#[no_mangle]`, rmcp `#[tool]`), trait impls and `@override`
members (flags & 15). Also never reported:

- **Test support**: a symbol whose name contains "test" and that only tests
  reference (`Config::test_default`, `Db::conn_for_test`).
- **Dart public top-level and static variables**. The Dart adapter emits
  read refs only for private names (sutra/288), so every public variable read
  is missing (`ref.watch(fooProvider)`, `Xsd.xdouble`). This exclusion removed
  18 of the 25 Dart items in the first tuning run, all false positives. Private
  (`_`-prefixed) variables have their reads extracted, so they stay reportable
  (sutra/499). The exclusion lifts when sutra/497 lands.

### Where it runs and what it reads

`sutra_review` and `sutra check --diff`, both after the index refresh they
already do. Liveness comes from the index, which holds the worktree. So:

- The check runs when the reviewed side is the worktree, the staged index or
  HEAD. For each changed file whose reviewed content differs from the worktree
  (a staged hunk with unstaged edits on top), it adds an `incomplete` entry.
- **Outside the diff, too** (sutra/499). The index holds the whole worktree,
  so an uncommitted caller in a file the diff does not touch would hide an
  orphan, and a deleted one would invent one. A staged or HEAD review is
  `incomplete` when any tracked file in an indexed language differs between the
  worktree and the reviewed side, or when any such file is untracked. The
  entry names up to five of these files.
- **A diff that ends at a historical commit is `skipped`, with the reason.**
  Liveness from a different tree would be wrong everywhere.
- A changed file that is missing or stale in the index is `incomplete`, never
  clean. So is a file that fails to parse. A file in a language the workspace
  does not index is out of scope.

### Output

Grouped by shape and file. Each item names the symbol and its `file:line`.
The output never says "delete this".

```json
"orphans": {
  "advisory": true,
  "findings": [
    {"kind": "orphaned", "file": "server/src/ai_metrics.rs",
     "detail": "this change removed its last reference outside tests",
     "symbols": [{"symbol": "Telemetry::start", "kind": "method", "line": 115, "test_refs": 3},
                 {"symbol": "TurnMetrics::emit", "kind": "method", "line": 231, "test_refs": 0}]}
  ]
}
```

`sutra check` prints the same items as `[orphaned] file:line  symbol (kind, N
test ref(s))` under an "advisory, not gating" header.

### Firing log

Each item is recorded in the shared firing log (sutra/467) as mechanism
`orphan`, with kind `added` or `orphaned`, the qualified name as the key, and
the declaration line as the site. It uses the same review event the sibling
check resolved for the diff, so it adds no second event or logging
implementation. The acted-on proxy reads `changed` when the declaration is
deleted or rewritten. It reads `present` when the symbol gets wired up, so
for this mechanism the sutra/485 re-measure has to ask about liveness at
HEAD, not the line.

## Back-test

The archaeology was delegated: a subagent pinned each case's commits from its
yojana task, fix commits and `git log -S`. I checked every commit the harness
ran: the symbol exists there, and its references match the table.

| Case | Commit | Role | Fires? | Every item production names there |
|---|---|---|---|---|
| sutra/297 `DdEngine::invalidate` | 928f611 | introduced | no | Its callers in `daemon.rs` existed but were inert: `.invalidate()` on an Arc just removed from the map. No reference check sees that. |
| | b9afe94 | daemon removed | **yes, orphaned** | 3: the target; `DdEngine::evict_if_idle` (real, **still dead at HEAD**, sutra/496); `parse_changed_files` (real, deleted as dead in d4da4c7) |
| | bcca99d | tool consolidation dropped the live call | no, since sutra/499 (b9afe94 already fired) | 4: `FindArgs`, `ResolveArgs`, `resolve::handle` (real, deleted as dead in 884dfde); `ToolsMetaArgs` (real, deleted in a0f6d50). Before sutra/499 it also named the target and `ConstraintResolver::invalidate` (real). `.invalidate()` is unqualified and has two definitions, so the uniqueness rule drops both. |
| adityas/ai/110 `Telemetry`, `TurnMetrics::emit` | 514e866 | introduced | no | Called from the preview route at birth |
| | 348d4c9 | preview route deleted | **yes, orphaned** | 12: `Telemetry::{start, mark_first_token, record_round, record_tool, set_usage}` and `TurnMetrics::emit` (the target); `Knowledge::new`, `structural_rules`, `Relation::tokens`, `being_knowledge` (rewired 3 hours later in dd23961). Since sutra/499 this is 10 items: `AppState::{kosha, vidya}` (real, **still dead at HEAD**) were accessed by unqualified field names with namesakes, and they drop out |
| arjuna/arrow/24 `ViewSpec.canonicalConfig` | 7411163 | introduced | **yes, added** | 1: the target |
| vidya/39 `load_synonyms` | 4d3408b | introduced | no, correctly | Wired from birth. The bug is that synonyms never persisted across processes, not reachability. |
| adityas/ai/65 | — | — | out of reach | Cross-repo: the server reads `CreateTurnRequest.chart`, and the client in another repo never sends it |
| adityas/ai/69 | — | — | out of reach | A field: Stripe subscription metadata written, never read |
| adityas/ai/82 | — | — | out of reach | Non-code: a tool named in the prompt and in `KNOWN_TOOLS`, never declared |
| adityas/explore/47 | — | — | out of reach | The trigger was never built. There is no symbol to find. |

The `Telemetry` struct itself is not named at 348d4c9: the index records its
own `impl Telemetry` header as a type reference, which keeps it live. Its
members are named, which is where the fix went.

One of four production replays read `TurnMetrics::emit` as referenced at
348d4c9. Three reruns did not, including the failing run's exact order. It is
not explained yet (sutra/498). The case fires either way, on the five
`Telemetry` methods.

## Noise on ordinary commits

The samples are seeded random non-fix commits that add 20 to 1500 non-test
source lines ([`sample.py`](../experiments/dup-exists/sample.py)), 8 per repo
from sutra (Rust), adityas/backend (Rust) and swe_dashboard (Dart). Labels:

- **STAGED**: nothing references it yet; a later commit of the same task wires
  it. True, and expected inside a multi-commit task.
- **REAL**: nothing references it, and nothing ever did (or no longer does).
- **ACC**: accurate but not actionable: production code only tests call, on
  purpose.
- **FP**: something does reference it; the index missed it.

| Sample | Commits | Items | Commits with an item | STAGED | REAL | ACC | FP |
|---|---|---|---|---|---|---|---|
| tuning (seed 483), production, final rules | 24 | 12 | 5 | 11 | 0 | 0 | 1 |
| tuning, after the uniqueness rule (sutra/499) | 24 | 11 | 4 | 11 | 0 | 0 | 0 |
| **held-out (seed 7), rules frozen before the draw** | 24 | 17 | 5 | 9 | 1 | 3 | 4 |
| held-out, after the arrow-closure fix (1c62d8c) | 24 | 13 | 3 | 9 | 1 | 3 | 0 |
| held-out, after sutra/499 | 24 | 13 | 3 | 9 | 1 | 3 | 0 |

Every staged item was wired in the next commit of the same task, 0 minutes
to 2.5 hours later (five of the six within 9 minutes). A review of the task's whole branch would show
none of them. These repos commit straight to main, so the reviewed change is
the uncommitted work. In practice the agent sees the item while it is still
building.

The tuning FP is `TracingSwissEph::entries` (swe a782481). `runner.dart`
reads it as `_tracing.entries`, and the Dart resolver does not bind a getter
through a private field's type. The diff removed an unrelated `.entries`
access, so the item is attributed as `orphaned`. The uniqueness rule
(sutra/499) removes it, because `entries` has more than one definition. The held-out REAL item is
`Db::query_pattern_families` (sutra 06de2b6), which was never wired and was
deleted as dead API four months later (sutra/481).

### What the tuning sample changed

Before the rules above, the first tuning run (the `added` shape only) named
33 items on 8 of 24 commits. 19 of them were false positives, and 13 were
staged. The classes, including those found once `orphaned` and the
held-out sample were added:

| False-positive class | Items | Disposition |
|---|---|---|
| Dart public variable reads not extracted (`ref.watch(xProvider)`) | 18 | Excluded with reason; parser follow-up sutra/497 |
| Dart configurable imports: `web.dart` never recorded as imported | 1 (3 with `orphaned`) | **Fixed in the parser** (2526a1f), plus liveness rule 5 |
| Resolution flips among same-named free functions (`tools::*::handle` going live and dead from commit to commit) | 3 per commit, in the harness's base-vs-head liveness diff on sutra recall commits | Production uses the removed reference's qualifier, never a base-vs-head liveness diff |
| `#[tool(description = "... #[cfg(test)] ...")]` marked the method test-only, so every call in it read as a test call | fed the flips above | **Fixed in the parser** (4e6c7f0) |
| Dart static-holder classes and private constructors | 10, on arrow/24's commit | Liveness rules 3 and 4 |
| Dart call as an arrow closure's body, `() => f(a)`, parsed as `(() => f)(a)` | 4 (held-out) | **Fixed in the parser** (1c62d8c). This one affects every `onTap: () => doThing(x)` |

Known classes from sutra/477 and sutra/481:

- **rmcp `#[tool]`**: fixed in 481.
- **Test-only helpers**: handled by the test-support rule.
- **Cargo package renames**: not observed. backend's multi-crate workspace
  (chat-store, reports) produced no false positive.
- **Python `hasattr` dispatch**: unmeasured. No Python repo was sampled.

**Method-by-name hides real orphans.** In backend 7c21ace, three added
methods have no reference bound to them and stay live only by name.
`Trigger::as_str` and `CompactionEvent::emit` are really called: their calls
bound elsewhere for lack of a receiver type. `CompactionConfig::is_enabled`
is not called. It is still dead at HEAD, and its only same-named call
filters a provider config. The by-name rule stays, because without it two
of those three would be false positives.

## Found along the way

**Live orphans at HEAD:**

- sutra: `DdEngine::evict_if_idle` has had no production caller since
  b9afe94 (2026-05-18). `constraints_idle_timeout_sec` is read and passed to
  every engine, but nothing ever evicts one. Filed as sutra/496.
- adityas/backend: `AppState::kosha` and `AppState::vidya`
  (`server/src/state.rs:140/144`) have been dead since 348d4c9.
  `CompactionConfig::is_enabled` has been dead since 7c21ace (masked by name,
  above).

**Parser bugs fixed:** 4e6c7f0 (cfg-test substring), 2526a1f (Dart
configurable imports), 1c62d8c (Dart arrow-body calls). Each also changes
`sutra_refs`, `sutra_dead` and impact for the code it touches.

## Caveats

- **Author-labelled.** One labeller (Claude). STAGED vs REAL was decided from
  history, never by judgment: did a later commit add a caller, and when?
- **Recall set is small.** 3 reachable cases. The claim is "fires on the
  UNWIRED incidents a reference check can see", not a rate.
- **The held-out numbers after the fix are not blind.** The arrow-closure bug
  was found on the held-out sample. The frozen row is the honest measurement;
  the post-fix row shows what the parser bug cost.
- **The `orphaned` name heuristic trades recall for precision on shared
  names.** Since sutra/499, an unqualified removal counts only for a name with
  one definition. That rule removed the tuning sample's one false positive
  (`TracingSwissEph::entries`). It also removed three real items from recall
  commits (`ConstraintResolver::invalidate`, `AppState::{kosha, vidya}`) and
  bcca99d's firing on the target, which b9afe94 had already caught. Every
  incident still fires. Before the rule, the heuristic could attribute a
  symbol that was already dead. This happened once in the tuning sample, through the Dart getter
  miss.
- **Liveness is only as good as resolution.** A spurious binding hides an
  orphan (sutra/498). A missed binding invents one: the Dart classes above,
  some already fixed.
