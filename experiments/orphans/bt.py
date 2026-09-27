#!/usr/bin/env python3
"""Back-test harness for the orphans advisory (sutra/483).

For a commit, lists the symbols it adds that have zero non-test references in
the post-commit tree: the UNWIRED shape, "built ahead of its call site".

  bt.py snap    <repo> <sha>        parse <sha> in a scratch worktree, dump symbols
  bt.py orphans <repo> <sha>        added-and-unreferenced symbols of <sha>
  bt.py cases   <cases.tsv>         run every back-test case
  bt.py sweep   <sample.tsv>        run a noise sample (repo<TAB>sha per line)

Liveness mirrors `Db::find_dead_symbols` (include_pub): kinds, flags & 15,
same-file twins, method-by-name for unqualified calls, `main`, test paths. The
one change is the point of the check: a reference only counts when it comes
from non-test code, so a helper exercised only by its own unit test still
fires ("nothing calls this yet").

A symbol counts as added when (path, qualified_name, kind) is absent at the
parent. One whose (qualified_name, kind) existed elsewhere at the parent is
`moved` and is not reported.

Snapshots are cached under /tmp/bt483/snap. One worktree + one registered
workspace (bt483-<name>) per repo; walking commits reparses incrementally.
SUTRA_BIN picks the binary (default: this repo's target/release/sutra).
"""

import json
import re
import os
import sqlite3
import subprocess
import sys

ROOT = "/tmp/bt483"
HERE = os.path.dirname(os.path.abspath(__file__))
SUTRA = os.environ.get(
    "SUTRA_BIN", os.path.join(HERE, "..", "..", "target", "release", "sutra")
)

KINDS = (
    "function",
    "method",
    "struct",
    "enum",
    "trait",
    "type_alias",
    "class",
    "mixin",
    "const",
    "static",
)


def sh(*args, cwd=None):
    return subprocess.run(
        args, cwd=cwd, check=True, capture_output=True, text=True
    ).stdout


def repo_name(repo):
    return os.path.basename(os.path.abspath(repo)).lower().replace("_", "-")


def full_sha(repo, sha):
    return sh("git", "rev-parse", sha + "^{commit}", cwd=repo).strip()


def registered_languages(repo):
    root = os.path.abspath(repo)
    for line in sh(SUTRA, "workspaces", "list").splitlines():
        parts = line.split("\t")
        if len(parts) >= 3 and os.path.abspath(parts[1]) == root:
            return [l.strip() for l in parts[2].strip("[] ").split(",") if l.strip()]
    sys.exit(f"{repo} is not a registered sutra workspace")


def is_test_path(path):
    return (
        path.startswith(("tests/", "test/", "integration_test/"))
        or "/tests/" in path
        or "/test/" in path
        or "/integration_test/" in path
        or path.endswith("_test.dart")
        or path.endswith("_test.rs")
    )


# Every symbol of the tree with its non-test liveness. A ref is from test code
# when its file is a test path or its innermost enclosing symbol is a test
# (flags & 3: #[test], #[cfg(test)]).
QUERY = f"""
WITH ref_src AS (
  SELECT r.id, r.target_symbol_id AS tgt, r.context_kind, r.qualifier, f.path AS src_path,
         (SELECT e.flags FROM symbols e
           WHERE e.file_id = r.file_id AND e.start_line <= r.line AND e.end_line >= r.line
           ORDER BY e.end_line - e.start_line LIMIT 1) AS src_flags
  FROM refs r JOIN files f ON f.id = r.file_id
  WHERE r.target_symbol_id IS NOT NULL
),
live_ref AS (
  SELECT * FROM ref_src
  WHERE NOT ({
    " OR ".join(
        [
            "src_path LIKE 'tests/%'",
            "src_path LIKE '%/tests/%'",
            "src_path LIKE 'test/%'",
            "src_path LIKE '%/test/%'",
            "src_path LIKE '%integration_test/%'",
            "src_path LIKE '%\\_test.dart' ESCAPE '\\'",
            "src_path LIKE '%\\_test.rs' ESCAPE '\\'",
        ]
    )
})
    AND (src_flags IS NULL OR (src_flags & 3) = 0)
)
SELECT s.id, f.path, s.qualified_name, s.short_name, s.kind, s.start_line, s.end_line,
       s.visibility, s.flags,
       (SELECT COUNT(*) FROM refs r WHERE r.target_symbol_id = s.id) AS any_refs,
       (SELECT COUNT(*) FROM live_ref r WHERE r.tgt = s.id) AS live_refs,
       EXISTS (SELECT 1 FROM symbols twin JOIN live_ref tr ON tr.tgt = twin.id
               WHERE twin.file_id = s.file_id AND twin.qualified_name = s.qualified_name
                 AND twin.id != s.id) AS twin_live,
       (s.kind = 'method' AND EXISTS (
          SELECT 1 FROM live_ref r2 JOIN symbols s2 ON s2.id = r2.tgt
          WHERE s2.short_name = s.short_name AND s2.kind = 'method'
            AND r2.context_kind = 'call' AND r2.qualifier IS NULL)) AS method_by_name,
       EXISTS (SELECT 1 FROM imports i1
               JOIN imports i2 ON i2.file_id = i1.file_id AND i2.line = i1.line
                              AND i2.resolved_file_id != i1.resolved_file_id
               JOIN symbols t ON t.file_id = i2.resolved_file_id
                             AND t.qualified_name = s.qualified_name AND t.kind = s.kind
               JOIN live_ref lt ON lt.tgt = t.id
               WHERE i1.resolved_file_id = s.file_id) AS alt_live
FROM symbols s JOIN files f ON f.id = s.file_id
"""


def snap(repo, sha):
    sha = full_sha(repo, sha)
    out = f"{ROOT}/snap/{repo_name(repo)}-{sha[:12]}.json"
    if os.path.exists(out):
        with open(out) as f:
            return json.load(f)
    name = f"bt483-{repo_name(repo)}"
    wt = f"{ROOT}/wt/{name}"
    if not os.path.isdir(wt):
        os.makedirs(f"{ROOT}/wt", exist_ok=True)
        sh("git", "worktree", "add", "--detach", wt, sha, cwd=repo)
        sh(SUTRA, "workspaces", "add", name, wt, *registered_languages(repo))
    sh("git", "checkout", "-q", "--detach", sha, cwd=wt)
    sh(SUTRA, "parse", name)
    db = sqlite3.connect(os.path.expanduser(f"~/.sutra/{name}/index.db"))
    cols = [
        "id",
        "path",
        "qn",
        "short",
        "kind",
        "start",
        "end",
        "vis",
        "flags",
        "any_refs",
        "live_refs",
        "twin_live",
        "method_by_name",
        "alt_live",
    ]
    syms = [dict(zip(cols, row)) for row in db.execute(QUERY)]
    os.makedirs(f"{ROOT}/snap", exist_ok=True)
    with open(out, "w") as f:
        json.dump(syms, f)
    return syms


def candidate(s):
    """Could this symbol be reported at all (dead-query exclusions)?"""
    return (
        s["kind"] in KINDS
        and s["short"] != "main"
        and not is_test_path(s["path"])
        and (s["flags"] & 15) == 0
    )


def live(s):
    # alt_live: a same-named twin in another URI of the same configurable
    # import is referenced (`staging_io.dart` / `staging_web.dart`); calls bind
    # to one alternative only.
    return (
        s["live_refs"] > 0 or s["twin_live"] or s["method_by_name"] or s.get("alt_live")
    )


def is_dart_variable(s):
    # The Dart adapter emits read refs only for private names (sutra/288), so a
    # public top-level or static variable read (`ref.watch(fooProvider)`,
    # `Xsd.xdouble`) never binds and every such variable reads as unreferenced.
    return s["path"].endswith(".dart") and s["kind"] in ("const", "static")


def with_structure(syms):
    """Liveness through structure: a type is live when any member is (a Dart
    class used only through static members, `CanonIri.rashi(...)`, has refs on
    the members and none on the class); a constructor is live when its class
    is (`CanonIri._()` exists to be uncallable)."""
    by_qn = {}
    for s in syms:
        by_qn.setdefault((s["path"], s["qn"]), []).append(s)
    live_types = set()
    for s in syms:
        if live(s) and "::" in s["qn"]:
            live_types.add((s["path"], s["qn"].rsplit("::", 1)[0]))
    out = []
    for s in syms:
        parent = s["qn"].rsplit("::", 1)[0] if "::" in s["qn"] else None
        # Dart extracts constructors, named or not, as `Class::Class`.
        ctor = (
            parent is not None
            and s["kind"] == "method"
            and (s["short"] == parent.rsplit("::", 1)[-1])
        )
        struct_live = (s["path"], s["qn"]) in live_types or (
            ctor
            and any(
                live(p) or (s["path"], parent) in live_types
                for p in by_qn.get((s["path"], parent), [])
            )
        )
        out.append(dict(s, struct_live=bool(struct_live)))
    return out


def live2(s):
    return live(s) or s.get("struct_live", False)


def removed_names(repo, parent, sha):
    """Identifier-ish words on lines the commit removed from non-test files."""
    diff = sh("git", "diff", "-U0", "--no-color", parent, sha, cwd=repo)
    words, path = set(), None
    for line in diff.splitlines():
        if line.startswith("--- "):
            path = line[6:] if line.startswith("--- a/") else None
        elif line.startswith("-") and not line.startswith("---"):
            if path and not is_test_path(path):
                words.update(re.findall(r"[A-Za-z_][A-Za-z0-9_]*", line))
    return words


def orphans(repo, sha):
    """(added, fired, orphaned) for commit `sha`.

    added:    candidate symbols the commit adds (moved ones flagged `moved`).
    fired:    added, not moved, and unreferenced from non-test code.
    orphaned: symbols live at the parent that the commit leaves unreferenced,
              whose name is on a non-test line the commit removed (it deleted
              their last caller), matched across moves by (qualified_name, kind).
    """
    sha = full_sha(repo, sha)
    parent = full_sha(repo, sha + "~1")
    # Parse the parent first so the commit's parse is incremental.
    before = with_structure(snap(repo, parent))
    after = with_structure(snap(repo, sha))
    had = {(s["path"], s["qn"], s["kind"]) for s in before}
    had_anywhere = {(s["qn"], s["kind"]) for s in before}
    live_before = {(s["qn"], s["kind"]) for s in before if live2(s)}
    removed = removed_names(repo, parent, sha)
    # Same-file twins (cfg variants, getter/setter) and impl-split types share
    # a qualified name: report each (path, qn, kind) once, live if any is.
    uniq = {}
    for s in after:
        if not candidate(s) or is_dart_variable(s):
            continue
        key = (s["path"], s["qn"], s["kind"])
        if key not in uniq or live2(s):
            uniq[key] = s
    added, orphaned = [], []
    for key, s in uniq.items():
        if key not in had:
            added.append(dict(s, moved=(s["qn"], s["kind"]) in had_anywhere))
        elif (
            (s["qn"], s["kind"]) in live_before
            and not live2(s)
            and s["short"] in removed
        ):
            orphaned.append(s)
    # A moved symbol that lost its callers in the move is orphaned too.
    orphaned += [
        s
        for s in added
        if s["moved"]
        and not live2(s)
        and (s["qn"], s["kind"]) in live_before
        and s["short"] in removed
    ]
    fired = [s for s in added if not s["moved"] and not live2(s)]
    return added, fired, orphaned


def masked(added):
    """Added methods kept live only by method-by-name: no reference binds to
    them, but an unqualified call to a same-named method exists somewhere."""
    return [
        s
        for s in added
        if not s["moved"]
        and s["live_refs"] == 0
        and not s["twin_live"]
        and s["method_by_name"]
    ]


def fmt(s):
    why = "test-only refs" if s["any_refs"] else "no refs"
    return f"{s['path']}:{s['start']}\t{s['kind']}\t{s['qn']}\t{s['vis'] or '-'}\t{why}"


def cmd_orphans(repo, sha, quiet=False):
    added, fired, orphaned = orphans(repo, sha)
    by_name = masked(added)
    if not quiet:
        print(
            f"# {repo_name(repo)} {sha[:12]}: {len(added)} added, {len(fired)} fire, "
            f"{len(orphaned)} orphaned, {len(by_name)} masked by name"
        )
        for s in fired:
            print("  [added] " + fmt(s))
        for s in orphaned:
            print("  [orphaned] " + fmt(s))
        for s in by_name:
            print("  [by-name] " + fmt(s))
    return added, fired, orphaned, by_name


def matches(want, items):
    return [
        w
        for w in want
        # A case names a type or a member; a type matches through its members.
        if any(
            s["qn"] == w or s["qn"].endswith("::" + w) or s["qn"].startswith(w + "::")
            for s in items
        )
    ]


def cmd_cases(tsv):
    rows = [l.rstrip("\n").split("\t") for l in open(tsv) if l.strip()]
    head, rows = rows[0], rows[1:]
    for r in rows:
        c = dict(zip(head, r))
        if not c.get("commit") or c["commit"] == "-":
            print(f"{c['case']}\t{c['role']}\tn/a")
            continue
        added, fired, orphaned = orphans(c["repo_path"], c["commit"])
        want = [w.strip() for w in c["symbol"].split(",")]
        hit_add, hit_orph = matches(want, fired), matches(want, orphaned)
        hit_masked = matches(want, masked(added))
        verdict = (
            "FIRES:added"
            if hit_add
            else "FIRES:orphaned"
            if hit_orph
            else "masked-by-name"
            if hit_masked
            else "silent"
        )
        print(
            f"{c['case']}\t{c['role']}\t{c['commit'][:12]}\t{verdict}\t"
            f"{','.join(hit_add + hit_orph + hit_masked) or '-'}\t"
            f"{len(fired)} added-items, {len(orphaned)} orphaned-items"
        )


def cmd_sweep(tsv, verbose):
    n = dict(commits=0, added=0, fired=0, orphaned=0, masked=0, c_fired=0, c_orph=0)
    for line in open(tsv):
        if not line.strip() or line.startswith("#"):
            continue
        repo, sha = line.split("\t")[:2]
        added, fired, orphaned, by_name = cmd_orphans(
            repo, sha.strip(), quiet=not verbose
        )
        n["commits"] += 1
        n["added"] += sum(1 for s in added if not s["moved"])
        n["fired"] += len(fired)
        n["orphaned"] += len(orphaned)
        n["masked"] += len(by_name)
        n["c_fired"] += bool(fired)
        n["c_orph"] += bool(orphaned)
    print(
        f"commits {n['commits']}, added symbols {n['added']}, "
        f"added-unreferenced {n['fired']} ({100 * n['fired'] / max(n['added'], 1):.0f}%) "
        f"on {n['c_fired']} commits, orphaned {n['orphaned']} on {n['c_orph']} commits, "
        f"masked by name {n['masked']}"
    )


if __name__ == "__main__":
    a = sys.argv[1:]
    if a[0] == "snap":
        print(len(snap(a[1], a[2])), "symbols")
    elif a[0] == "orphans":
        cmd_orphans(a[1], a[2])
    elif a[0] == "cases":
        cmd_cases(a[1])
    elif a[0] == "sweep":
        cmd_sweep(a[1], "-v" in a)
