#!/usr/bin/env python3
"""Replay behavioral_coupling's co-change partner list at a fix commit's parent.

History = commits reachable from C^ within 90 days before C, cap total paths <= 30
(the new rule), indexed = paths with the workspace's extensions that exist at C^.
Static edges are not reconstructable at C^; reported unfiltered.
"""

import subprocess, sys
from collections import defaultdict, Counter


def git(root, *a):
    return subprocess.run(
        ["git", "-C", root, *a], capture_output=True, text=True, check=True
    ).stdout


def main(root, commit, exts, followup_files):
    parent = commit + "^"
    ts = int(git(root, "log", "-1", "--format=%ct", commit).strip())
    tree = set(git(root, "ls-tree", "-r", "--name-only", parent).split())
    indexed = {p for p in tree if p.endswith(tuple(exts))}
    out = git(
        root,
        "log",
        parent,
        f"--since=@{ts - 90 * 86400}",
        "--format=@%H",
        "--name-only",
        "--no-renames",
    )
    commits, h = defaultdict(set), None
    for line in out.splitlines():
        if line.startswith("@"):
            h = line[1:]
            commits[h]
        elif line.strip():
            commits[h].add(line.strip())
    changed = {
        p
        for p in git(root, "show", "--name-only", "--format=", commit).split()
        if p in indexed
    }
    for label, cap in (
        ("old(indexed<=50)", lambda c: len(c & indexed) <= 50),
        ("new(total<=30)", lambda c: len(c) <= 30),
    ):
        elig = [c & indexed for c in commits.values() if cap(c)]
        cnt = Counter(f for c in elig for f in c)
        partners = []
        for f in changed:
            shared = Counter(g for c in elig if f in c for g in c if g not in changed)
            for g, s in shared.items():
                j = s / (cnt[f] + cnt[g] - s)
                if j >= 0.5:
                    partners.append((f, g, round(j, 2), s))
        hit = sorted({g for _, g, _, _ in partners} & set(followup_files))
        print(
            f"  {label}: eligible={len(elig)} partners={len(partners)} follow-up files named={hit}"
        )
        for p in sorted(partners, key=lambda p: (-p[3], -p[2]))[:10]:
            print("     ", p)


if __name__ == "__main__":
    root, commit, exts, follow = (
        sys.argv[1],
        sys.argv[2],
        sys.argv[3].split(","),
        sys.argv[4].split(","),
    )
    print(f"== {commit} (follow-up files: {follow})")
    main(root, commit, exts, follow)
