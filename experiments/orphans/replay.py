#!/usr/bin/env python3
"""Replay the shipped orphans advisory (`sutra check --diff HEAD`) on the
back-test commits, to confirm the production path matches bt.py.

  replay.py cases.tsv sample.tsv      (`-` for cases skips the back-test)

Checks out each commit in bt.py's scratch worktree (bt483-<name>, which must
exist: run bt.py first), lets `sutra check` refresh the index, and prints the
`orphans` findings. Production differs from bt.py in one place: `orphaned`
is decided from the references on the diff's removed lines (name, and the
qualifier where there is one), not from a parse of the parent tree.
"""

import json
import os
import subprocess
import sys

from bt import ROOT, SUTRA, full_sha, repo_name, sh


def replay(repo, sha):
    wt = f"{ROOT}/wt/bt483-{repo_name(repo)}"
    # `sutra check` can leave an untracked .sutra/accepted.toml in the way.
    sh("git", "checkout", "-q", "-f", "--detach", full_sha(repo, sha), cwd=wt)
    out = subprocess.run(
        [
            SUTRA,
            "check",
            "--diff",
            "HEAD",
            "--format",
            "json",
            "--severity",
            "informational",
        ],
        cwd=wt,
        capture_output=True,
        text=True,
    )
    report = json.loads(out.stdout)["orphans"]
    items = [
        (g["kind"], f"{g['file']}:{s['line']}", s["symbol"], s["test_refs"])
        for g in report["findings"]
        for s in g["symbols"]
    ]
    return items, report


def show(label, repo, sha):
    items, report = replay(repo, sha)
    extra = {k: report[k] for k in ("incomplete", "skipped", "error") if k in report}
    print(f"# {label} {repo_name(repo)} {sha[:12]}: {len(items)} items {extra or ''}")
    for kind, at, symbol, tests in items:
        print(f"  [{kind}] {at}\t{symbol}\ttest_refs={tests}")
    return items


if __name__ == "__main__":
    cases, sample = sys.argv[1], sys.argv[2]
    rows = (
        [l.rstrip("\n").split("\t") for l in open(cases) if l.strip()]
        if cases != "-"
        else [[]]
    )
    head = rows[0]
    for r in rows[1:]:
        c = dict(zip(head, r))
        if c["commit"] != "-":
            show(
                f"{c['case']} ({c['role']}, want {c['symbol']})",
                c["repo_path"],
                c["commit"],
            )
    total = commits = firing = 0
    for line in open(sample):
        if line.strip():
            repo, sha = line.split("\t")[:2]
            items = show("sample", repo, sha)
            commits += 1
            total += len(items)
            firing += bool(items)
    print(f"sample: {commits} commits, {total} items, {firing} commits with an item")
