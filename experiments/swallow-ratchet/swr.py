#!/usr/bin/env python3
"""Back-test driver for the swallowed-error ratchet (sutra/465).

For a commit, feeds every changed .rs/.dart file (parent vs commit content) to
the Rust harness, which runs sutra's real forbidden_pattern engine with guard
semantics (introduced = multiset of new matches minus old). Each introduced
hit is then tagged:

  added      the hit's line is a line the commit's diff added (the check-time
             attribution the ratchet needs; the guard's multiset diff can also
             attribute a hit to an untouched line when a same-key match moves)
  justified  a `swallow:` comment sits on the hit line or in the comment block
             directly above its statement (the proposed waiver convention)

  swr.py commit <repo> <sha>            hits introduced by one commit
  swr.py cases  <cases.tsv>             back-test: did a rule fire on the offending line?
  swr.py noise  <repo> <sha>...         hits for ordinary commits (for labelling)
"""

import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
HARNESS = os.path.join(HERE, "harness/target/release/swr-harness")
RULES = os.path.join(HERE, "candidates.toml")
EXTS = (".rs", ".dart")
JUSTIFY = re.compile(r"(//|/\*).*\bswallow:")


def git(repo, *args):
    return subprocess.run(
        ["git", "-C", repo, *args], capture_output=True, text=True, check=True
    ).stdout


def show(repo, rev, path):
    r = subprocess.run(
        ["git", "-C", repo, "show", f"{rev}:{path}"], capture_output=True, text=True
    )
    return r.stdout if r.returncode == 0 else ""


EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"


def parent(repo, sha):
    """The commit's first parent, or the empty tree for a root commit."""
    r = subprocess.run(
        ["git", "-C", repo, "rev-parse", "--verify", "-q", f"{sha}^"],
        capture_output=True,
        text=True,
    )
    return r.stdout.strip() if r.returncode == 0 else EMPTY_TREE


def added_lines(repo, sha):
    """path -> set of 1-based line numbers the commit added (new side)."""
    out = git(
        repo,
        "diff",
        "--unified=0",
        "--no-renames",
        parent(repo, sha),
        sha,
        "--",
        *[f"*{e}" for e in EXTS],
    )
    added, path, line = {}, None, 0
    for l in out.splitlines():
        if l.startswith("+++ "):
            path = None if l[4:] == "/dev/null" else l[6:]
        elif l.startswith("@@"):
            m = re.match(r"@@ -\S+ \+(\d+)(?:,(\d+))? @@", l)
            line = int(m.group(1))
        elif path and l.startswith("+"):
            added.setdefault(path, set()).add(line)
            line += 1
    return added


def justified(lines, n):
    """`swallow:` on line n, or in the comment run directly above it."""
    if JUSTIFY.search(lines[n - 1]):
        return True
    i = n - 2
    while i >= 0 and lines[i].strip().startswith(("//", "/*", "*")):
        if JUSTIFY.search(lines[i]):
            return True
        i -= 1
    return False


def commit_hits(repo, sha):
    added = added_lines(repo, sha)
    feed, sources = [], {}
    for path in sorted(added):
        new = show(repo, sha, path)
        feed.append(
            json.dumps(
                {"path": path, "old": show(repo, parent(repo, sha), path), "new": new}
            )
        )
        sources[path] = new.splitlines()
    if not feed:
        return []
    out = subprocess.run(
        [HARNESS, RULES],
        input="\n".join(feed) + "\n",
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    hits = []
    for l in out.splitlines():
        h = json.loads(l)
        src = sources[h["path"]]
        h["text"] = src[h["line"] - 1].strip() if 0 < h["line"] <= len(src) else ""
        h["added"] = h["line"] in added.get(h["path"], ())
        h["justified"] = justified(src, h["line"]) if h["text"] else False
        hits.append(h)
    return hits


def fmt(h):
    flags = ("A" if h["added"] else "-") + ("J" if h["justified"] else "-")
    return f"  {flags} {h['rule']:<26} {h['path']}:{h['line']}  {h['text'][:110]}"


def norm(s):
    return re.sub(r"\s+", "", s)


def cmd_cases(tsv):
    rows = [l.rstrip("\n").split("\t") for l in open(tsv) if l.strip()]
    head, rows = rows[0], rows[1:]
    ix = {k: i for i, k in enumerate(head)}
    fired = total = 0
    for r in rows:
        bug, repo, sha, path, want = (
            r[ix[k]]
            for k in ("bug", "repo", "intro_sha", "file_at_intro", "offending_line")
        )
        if sha == "?":
            print(f"{bug:<22} UNPINNED  {want[:90]}")
            continue
        total += 1
        hits = [h for h in commit_hits(repo, sha) if h["path"] == path and h["added"]]
        # The offending line(s) in the introducing commit. A hit counts on the
        # line itself or up to 2 lines above: a rule that captures the
        # receiver reports the receiver's line in a split method chain.
        src = show(repo, sha, path).splitlines()
        targets = [
            i + 1
            for i, t in enumerate(src)
            if t.strip() and (norm(want) in norm(t) or norm(t.strip()) == norm(want))
        ]
        on = [h for h in hits if any(n - 2 <= h["line"] <= n for n in targets)]
        if on:
            fired += 1
        rules = ",".join(sorted({h["rule"] for h in on})) or "-"
        print(
            f"{bug:<22} {'HIT ' if on else 'MISS'}  {r[ix['idiom']]:<20} {rules:<40} {want[:80]}"
        )
    print(f"\nfired on {fired} of {total} pinned cases")


def cmd_noise(repo, shas):
    for sha in shas:
        hits = commit_hits(repo, sha)
        subj = git(repo, "log", "-1", "--format=%h %s", sha).strip()
        print(
            f"{os.path.basename(repo)} {subj[:90]}  [{len(hits)} hit(s), {sum(h['added'] for h in hits)} on added lines]"
        )
        for h in hits:
            print(fmt(h))


def main():
    cmd, *args = sys.argv[1:]
    if cmd == "commit":
        for h in commit_hits(args[0], args[1]):
            print(fmt(h))
    elif cmd == "cases":
        cmd_cases(args[0])
    elif cmd == "noise":
        cmd_noise(args[0], args[1:])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
