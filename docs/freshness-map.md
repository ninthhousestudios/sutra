# Freshness map

Compressed reference for the freshness / refresh subsystem: how sutra decides an
index is stale, and how it refreshes before answering a query. Read this before
touching `src/freshness.rs`, the refresh path in `src/mcp.rs`, or the incremental
parse in `src/pipeline.rs`.

The health evidence contract (sutra/412) and its validity/run layer were
removed in sutra/473; the contract is archived at
[archived/health-evidence-contract.md](archived/health-evidence-contract.md).
Session-start reparse remains enabled.

## Staleness is content, not time

Staleness is a claim about bytes, never about elapsed time — there is no grace
window (sutra/319, sutra/362). `probe_drift` walks the workspace with the same
walker/ignore rules the parse uses, scoped to the indexed languages, and compares
each source file to its stored fingerprint:

- `(size, mtime)` both match the baseline → clean without reading the file (fast
  path).
- otherwise hash the bytes; `blake3 == content_hash` → clean (catches a `touch`
  that preserved content, and an mtime-preserving edit).

The result is a structured `WorkspaceDrift { changed, added, removed }`, not a
bool, so a refresh can touch only what moved. Git HEAD is never consulted — a
docs-only commit must not invalidate the index.

`workspace_drift` returns `(last_parse_ts, Option<WorkspaceDrift>)`. `None` means
"no baseline / read failure" — the caller must treat that as *must parse*, never
as clean.

## Extractor identity is orthogonal to content (sutra/364)

Content staleness catches changed *bytes*. It cannot catch a changed
*extractor* — a tree-sitter grammar bump, an adapter fix, a symbol-kind change —
because the bytes are identical; only the symbols a re-parse *would* produce
differ. Historically each such change shipped a hand-written
`UPDATE files SET content_hash=''` migration (0054/0055/0056) to bust the skip,
the "version bump to forget" pattern graft eliminated by hashing the extractor
into its cache key.

`build.rs` computes a `PARSER_STAMP` (`src/parser::PARSER_STAMP`) — an FNV hash
over every `src/parser/**/*.rs` source (recursively), the pinned tree-sitter
grammar versions, and the crate version. `parse_workspace` compares it to
`index_meta.parser_stamp` at parse start; on a mismatch (or a `NULL` stamp on a
pre-364 index) it passes `force_reparse=true` into `parse_single_file`, which
bypasses **both** the mtime and content-hash short-circuits, then re-records the
stamp on success. So a parser change forces exactly one full re-extraction and
subsequent reparses skip unchanged files again.

The stamp is the **only** extractor-staleness signal the skip consults. Do not
infer staleness from row contents: an earlier gate treated any symbol with NULL
`language_attrs` as a pre-migration row, but several extractors legitimately emit
no attrs (Rust fields, most C/Python/TS symbols), so ~2/3 of files re-extracted on
every parse of an unchanged workspace (sutra/431). Re-extraction reassigns symbol
ids, so anything ordered by rowid shifts too — `all_symbols_summary` orders by
`(path, id)` so resolver tie-breaks give the same answer after a partial reparse
as after a fresh full parse.

The hashed boundary is *all code that shapes persisted extraction output*, not
just the grammars/adapters (sutra/383). The extraction→persistence normalization
— symbol-tree flattening, ref/import field mapping, the per-file size caps —
lives in `src/parser/persist.rs` precisely so the `src/parser/` hash covers it:
a change there changes what a re-parse of unchanged bytes writes, so it must rev
the stamp. Keeping it out of `src/pipeline.rs` is deliberate — that file is *not*
hashed (it is full of parse-orchestration and DD code that must not rev the stamp
on every unrelated edit). A `build.rs` assertion fails the build if
`flatten_symbols_dfs` is moved out of the hashed `src/parser/` tree. Schema-level
changes to how rows are written (`db::replace_file_data` and the insert SQL) go
through migrations, which reindex — so they need no stamp coverage.

Where the heal fires:
- The check lives inside `parse_workspace` (compares `index_meta.parser_stamp`
  to `PARSER_STAMP` at parse start), so **any** full parse heals: the explicit
  `reparse` action, the parse-all path, and `maybe_reparse_cwd` (stdio startup,
  which reparses when `is_stale || stamp_changed`).
- `parse_incremental` (the incremental drift reparse) always passes
  `force_reparse=false` — a stamp mismatch needs a full walk, not a drift-set
  reparse.
- The query path heals too (sutra/382). `refresh_before_answer` probes the stamp
  alongside content drift; on a mismatch it runs a full `parse_workspace` instead
  of the incremental reparse. This is the single choke point every transport
  (stdio + http) and every workspace (CWD or not) funnels through — without it an
  upgraded **http** deployment or a **non-CWD** workspace (neither covered by
  `maybe_reparse_cwd`) would serve stale symbols indefinitely while reporting
  `is_stale: false`. The heal runs *before* the caller reads freshness, so a
  healed workspace reports honestly; only the bounded-wait degradation window
  (a parse already in flight) answers stale, and it carries the "reparse in
  flight" note. A persistently failing file leaves the stamp unadvanced
  (sutra/381), so the query-path full heal can re-run until the file heals; the
  parse lock caps that to one walk at a time.
- `is_workspace_stale` / the `is_stale` envelope stay pure content (their
  contract). They are not folded with the stamp: the query path heals the stamp
  before freshness is computed, so is_stale stays honest without a stamp term
  that could otherwise wedge `is_stale=true` forever.

## Refresh before answering (sutra/363)

Every query tool funnels through `SutraServer::tool_context`, which calls
`refresh_before_answer` before computing the response freshness. The properties,
copied from graft's `ensureFreshGraph`:

1. **Probe first.** Clean → answer immediately, zero cost beyond the probe.
   Frozen workspace → skip entirely (immutable index). No baseline → leave it to
   startup / explicit reparse.
2. **Drift → take the parse lock with a bounded wait** (`REFRESH_LOCK_WAIT`, 2s;
   `tokio::time::timeout` over `lock_owned()`). If a parse already holds it past
   the deadline, answer from the current index with an envelope note
   (`"refresh": "a reparse is in flight, ..."`). The `is_stale` field still
   reflects reality.
3. **Re-probe under the lock.** A concurrent refresh may have already cleaned the
   drift, so N racing queries on one edit produce **one** parse, not N. If the
   re-probe is clean, return without parsing.
4. **Reparse on `spawn_blocking`, holding the lock + a `ParseMark` across it.**
   Content drift → incremental reparse of the drift set
   (`pipeline::parse_incremental`). A parser-stamp mismatch → a full
   `pipeline::parse_workspace` instead (sutra/382), which re-extracts every file,
   re-records the stamp, and subsumes any pending content drift.
5. **Never fatal.** A lock timeout, parse error, or task panic degrades to
   answering from the current index with a note. A query that works today must
   not start failing because a rebuild did.

The parse lock is the same per-workspace mutex (`ParseCoordinator::lock_for`) that
the startup reparse, the daemon timer, the `reparse` action, and DD-backed
evaluation use. DD tools (`sutra_constraints`, `sutra_review`) call `tool_context`
*first* (refresh + release), then `hold_parse_lock` across evaluation — so the DD
read reflects the post-refresh index and there is no self-deadlock on the lock.

The startup reparse (`maybe_reparse_cwd`) and the HTTP daemon timer are **not**
removed by sutra/363 — they are belt-and-braces. Removing them is a deliberate
follow-up once the query-path refresh has run for a while.

## What a refresh writes: inline vs deferred

`parse_incremental` writes exactly the tier that graph and symbol queries read,
and deliberately skips the expensive derived tiers. The split is the contract:

| Derived data | Refreshed inline (query path) | Deferred to full `reparse` |
|---|---|---|
| `files` rows (path, hashes, mtime/size, line_count) | ✅ | |
| `symbols` (+ FTS) | ✅ | |
| `imports` | ✅ | |
| `refs` + cross-file resolution (`needs_resolution`) | ✅ | |
| import edges (rust/dart/c/python/js) | ✅ | |
| file removals (`delete_file_cascade`) | ✅ | |
| PageRank | | ✅ |
| file rollups (fan_in, blast_radius) | | ✅ |
| components + semantic anchors | | ✅ |
| HRR vectors + pattern families | | ✅ |
| cochange / commit_files | | ✅ |
| health findings | | ✅ |
| conventions, ratcheted constraints | | ✅ |

Consequence: right after an incremental refresh, symbol/ref/import queries reflect
the edit, but importance-ranked or similarity/health-derived views (e.g.
`sutra_map` ordering by blast_radius, `sutra_health`) may lag one edit behind
until the next full parse. This is intentional — the deferred tiers carry O(n²)
phases that must not run on the query path.

Consumers that gate on a derived value must not read the deferred column. The
`max_fan_in` constraint computes fan-in from the live refs + import graph
rather than `files.fan_in_files`, so `sutra check` sees a working-tree edit
right after the incremental refresh (sutra/440). The guard's read-only path
does the same per target file via `db::file_importers_from_conn` (sutra/456).

`parse_incremental` records **no** snapshot and never calls
`set_derived_complete`: staleness is content-based (per-file fingerprints, updated
by `replace_file_data`), so the next drift probe reads clean without a snapshot,
and `data_generation` legitimately stays ahead of `derived_complete_generation`
(the derived tier really is behind).

### `replace_file_data` child-table lifecycle (sutra/413)

A content edit is **not** a deletion. When the path is already indexed,
`replace_file_data` keeps the existing `files` row (and its id) and treats every
table FK'd to `files`/`symbols` in one of three ways. Deleting the `files` row —
the pre-413 behavior — cascaded raw history away and reassigned the id on every
incremental reparse, orphaning `commit_files` and derived evidence.

| Child table | Class | Handling on content edit |
|---|---|---|
| `symbols` (+ `symbols_fts`) | extraction | replaced (delete by `file_id`, re-insert) |
| `refs` (outgoing) | extraction | replaced (delete by `file_id`, re-insert) |
| `imports` (outgoing) | extraction | replaced (delete by `file_id`, re-insert) |
| inbound `refs` (other files → this file's symbols) | resolution | detached (`target_symbol_id=NULL`, call-site name recovered) then re-resolved (sutra/378) |
| inbound `imports.resolved_file_id` (other files → this file) | resolution | **kept** — path identity is stable, so the edge stays correct |
| `component_membership` | global partition | **preserved** — freshness is the clustering gate's edge-drift threshold (sutra/439) |
| `hrr_file_hashes` | derived | invalidated (delete by `file_id`) |
| `hrr_vectors`, `pattern_family_members` | derived | invalidated (cascade off the `symbols` delete) |
| `commit_files` | raw history | **preserved** — never touched by a content edit |

Preserving the id must never let stale derived rows claim they reflect the new
content: extraction-derived analysis is invalidated independently of the raw
history it is preserved alongside (sutra/412). Actual
file removal stays in `delete_file_cascade`, which still deletes the row.

Shared helper: `resolve_references` (the resolution + import-edge tier) is called
by both the full parse (`post_parse_sequence`, which reuses the returned dirty set
for its graph rollups) and the incremental refresh.

## `stale_threshold_sec`

Dead. It exists only in the retired watcher-daemon PRD
(`docs/plans/watcher-daemon-prd.md`), never in source. There is no time-based
staleness knob to remove or document; staleness is content-based (above).
