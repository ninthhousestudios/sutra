# Running the sutra guard across git worktrees

A practical setup guide for using `sutra-guard` (the edit-time constraint hook)
in a workflow that spins up **multiple git worktrees** and runs **concurrent
Claude sessions**. Every claim here is verified against sutra source at the
line references given.

## The one mental model

**Hooks are global; databases are per-directory-basename.**

- You register the guard hook **once**, in `~/.claude/settings.json`
  (`claude_settings_path()` → `~/.claude/settings.json`, `src/guard.rs:1152`).
  It then fires for every Claude session in every directory automatically.
- Whether the guard *does anything* in a given worktree is gated on whether a
  database exists at `~/.sutra/<slug>/index.db`, where `<slug>` is that
  worktree's **directory basename**, lowercased, spaces→dashes
  (`workspace_id_from_path`, `src/guard.rs:408`). If that file is absent the
  guard silently returns success and enforces **nothing**
  (`if !db_path.is_file() { return Ok(()) }`, `src/bin/guard.rs:101`). It is
  also fail-open on every error.

So: the guard is *armed everywhere, but acts only where a DB with the matching
name lives.* One global install + one DB per worktree.

## The coupling that bites worktree users

`sutra parse <id>` writes the DB to `~/.sutra/<id>/index.db`
(`src/db/mod.rs:493`). The guard reads `~/.sutra/<slug>/index.db` where `<slug>`
is derived from the basename (`src/bin/guard.rs:98-100`). **These must be the
same string.** If you register a worktree with any id other than its
basename-slug, parse succeeds, the DB exists, and the guard *still* silently
enforces nothing — because it is looking under a different name.

The helper below derives the id exactly the way the guard does, so they always
match. Do not register worktrees by hand with arbitrary ids.

## Step 1 — install the hook once (global)

`sutra guard install` registers **two** PreToolUse hooks (`src/guard.rs:1214`):

1. `Edit|Write|MultiEdit` → the modification/constraint guard (**the one you
   want**).
2. `Glob|Grep` → a routing hook that **denies** your search tools with
   "STOP: use `sutra_explore`/`sutra_lookup`/`sutra_map` instead", but only in
   directories that have a parsed DB (`src/bin/guard.rs:51-77`).

Because the hook is global and the routing deny self-arms in *every* parsed
worktree, you cannot scope it to some worktrees and not others. Choose:

- **You can guarantee `sutra serve` MCP is registered in every session** → run
  `sutra guard install`; the routing deny becomes a helpful nudge.
- **You cannot** (the realistic case for many ad-hoc concurrent sessions) →
  **do not run `sutra guard install`.** Hand-write only the Edit hook into
  `~/.claude/settings.json` so code search keeps working everywhere:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Edit|Write|MultiEdit",
        "hooks": [{ "type": "command", "command": "/absolute/path/to/sutra-guard", "timeout": 3000 }] }
    ]
  }
}
```

Find the absolute path with `command -v sutra-guard`. This is the exact hook
`install()` would write, minus the routing hook.

## Step 2 — the per-worktree helper

Save as `~/.config/sutra/worktree-guard.sh` and `source` it from your shell rc.
Requires `sutra`, `sutra-guard`, and `jq` on `PATH`. Assumes ASCII worktree
directory names (matches the guard's lowercase+dash slug).

```bash
# --- sutra worktree guard helpers ------------------------------------------
# Languages to index per worktree (override per repo).
: "${SUTRA_WT_LANGS:=python}"

# Negative-control trigger: a Write that MUST be denied by your rules.toml.
# Defaults to a Rule-4b-style self.gui.<attr> write. Point TRIP_PATH inside the
# rule's `scope` (e.g. apps/) or the pattern won't match and the check will
# false-fail. Override both for your repo's actual forbidden pattern.
: "${SUTRA_WT_TRIP_RELPATH:=_sutra_negctl_scratch.py}"
: "${SUTRA_WT_TRIP_CONTENT:=self.gui.__negctl = 1}"

# Derive the workspace id EXACTLY as the guard does: basename -> lowercase ->
# spaces to dashes (src/guard.rs:408). If this diverges, the guard no-ops.
_sutra_wt_id() {
  basename "$1" | tr '[:upper:]' '[:lower:]' | tr ' ' '-'
}

# Drive the guard with a synthetic PreToolUse payload and confirm it DENIES.
# A deny is signaled on STDOUT (permissionDecision:deny); the exit code stays 0,
# so we grep stdout, never $?. A silent allow => DB missing/misnamed or rule not
# loaded => governance is OFF in this worktree.
sutra-wt-verify() {
  local root; root="$(cd "${1:-$PWD}" && pwd)" || return 1
  local trip="$root/$SUTRA_WT_TRIP_RELPATH"
  jq -nc --arg fp "$trip" --arg c "$SUTRA_WT_TRIP_CONTENT" --arg cwd "$root" \
    '{tool_name:"Write",tool_input:{file_path:$fp,content:$c},
      cwd:$cwd,hook_event_name:"PreToolUse",session_id:"negctl"}' \
    | sutra-guard 2>/dev/null | grep -q '"permissionDecision":"deny"'
}

# Register + parse + verify the guard is actually armed in a worktree.
sutra-wt-enter() {
  local root; root="$(cd "${1:-$PWD}" && pwd)" || return 1
  local id; id="$(_sutra_wt_id "$root")"

  sutra workspaces add "$id" "$root" $SUTRA_WT_LANGS 2>/dev/null \
    || echo "sutra-wt-enter: workspace '$id' already registered (ok)"
  sutra parse "$id" || { echo "sutra-wt-enter: parse FAILED for '$id'" >&2; return 1; }

  if sutra-wt-verify "$root"; then
    echo "sutra-wt-enter: guard ARMED for '$id' at $root"
  else
    echo "sutra-wt-enter: GUARD NOT ARMED at $root — do NOT trust enforcement here." >&2
    echo "  Check: id/basename slug match, DB parsed, TRIP path inside the rule scope." >&2
    return 1
  fi
}

# Tear down at worktree exit: unregister + move the DB aside so a stale index
# can't linger. (House rule: mv to /tmp, never rm.)
sutra-wt-exit() {
  local root; root="$(cd "${1:-$PWD}" && pwd)" || return 1
  local id; id="$(_sutra_wt_id "$root")"
  sutra workspaces remove "$id" 2>/dev/null || true
  local db="${SUTRA_DB_DIR:-$HOME/.sutra}/$id"
  [ -d "$db" ] && mv "$db" "/tmp/sutra-wt-$id-$(date +%s)" \
    && echo "sutra-wt-exit: '$id' unregistered; DB moved to /tmp"
}
# ---------------------------------------------------------------------------
```

### Usage

```bash
git worktree add ../repo-featureX <base-sha>
cd ../repo-featureX
sutra-wt-enter          # register + parse + prove the guard denies
# ... run your Claude session(s) in this worktree ...
sutra-wt-exit           # unregister + retire the DB
git worktree remove ../repo-featureX
```

`sutra-wt-enter` **fails loudly** if the guard isn't actually denying — that is
the whole point. A guard that silently isn't wired is worse than no guard,
because you *think* you're protected. Never skip the verify step.

## Step 3 — merge-time backstop (matters more with worktrees)

The edit hook only sees `Edit|Write|MultiEdit`. Mutations via `Bash`
(`sed`, `python -c`, codegen) bypass it entirely, and fail-open means any guard
error just allows the edit. So gate every worktree **merge** with the separate
blocking check (`src/bin/guard.rs:12`):

```bash
sutra-guard --check-constraints --staged   # exit 1 if any staged change violates a constraint
```

Wire it into your pre-merge / CI gate. This is the net that catches what the
per-edit hook structurally can't.

## Gotchas specific to worktrees

- **A deny does not set a non-zero exit code.** The guard signals deny via a
  stdout JSON `permissionDecision:deny`; the process exits 0. Any script that
  checks `$?` will miss it — grep stdout (the helper does).
- **Detached worktrees pin a SHA and go stale** as the base branch advances.
  Before re-parsing a long-lived worktree:
  `git -C <wt> fetch && git -C <wt> checkout --detach <target-sha>`, record the
  SHA next to the snapshot, then `sutra parse`.
- **Parses across worktrees don't contend.** `parse.lock` only serializes
  reparses of the *same* workspace, so N worktrees parse independently and
  safely in parallel. But designate *one* session as the owner of any shared
  baseline workspace to avoid divergent baselines.
- **Don't compare index snapshots taken on different days.** sutra's git
  metrics (co-change, entropy) ride a 90-day window anchored at parse time.
  Valid before/after comparison = re-parse *both* SHAs at comparison time so
  they share the same window anchor.
- **The negative-control trip path must sit inside the rule's `scope`.** If your
  Rule 4b is scoped to `apps/`, set
  `SUTRA_WT_TRIP_RELPATH=apps/_sutra_negctl_scratch.py` — a scratch file outside
  scope won't be denied and will false-fail the verify. The scratch file is
  never actually written (the guard intercepts pre-write); it only needs a path.

## What sutra does and does not do for you here

sutra does **not** coordinate your agents. It provides two hard mechanisms —
the edit-time guard and the merge-time `--check-constraints` — keyed by
directory name, and leaves who-merges-when to your own tooling (worktree
isolation, a merge-slot lock, CI). There is no built-in cross-agent lock. Set
up the two mechanisms per the steps above and let your existing worktree
discipline handle serialization.
