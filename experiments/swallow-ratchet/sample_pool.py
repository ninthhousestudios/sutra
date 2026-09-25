"""Eligible-commit pool shared by sample.py and volume.py (sutra/465)."""

import re, subprocess

REPOS = {
    "/home/josh/soft/manas/sutra": 10,
    "/home/josh/soft/manas/yojana": 4,
    "/home/josh/nhs/soft/astrology/swe_dashboard": 7,
    "/home/josh/adityas/explore": 5,
}


def repo_pool(repo, exclude):
    """Non-merge commits since 2026-06 adding >=15 lines to non-test .rs/.dart files."""
    log = subprocess.run(
        [
            "git",
            "-C",
            repo,
            "log",
            "--no-merges",
            "--since=2026-06-01",
            "--format=@%h",
            "--numstat",
        ],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    out, sha, add = [], None, 0
    for l in log.splitlines() + ["@"]:
        if l.startswith("@"):
            if sha and add >= 15 and sha[:7] not in exclude:
                out.append(sha)
            sha, add = l[1:], 0
        elif l.strip():
            a, _, p = l.split("\t", 2)
            if (
                p.endswith((".rs", ".dart"))
                and not re.search(r"(^|/)(tests?|test_\w+)/|_test\.dart$", p)
                and a.isdigit()
            ):
                add += int(a)
    return out


def excluded():
    return {
        l.split("\t")[3][:7]
        for l in open("cases.tsv").read().splitlines()[1:]
        if "\t" in l
    }


def pool():
    ex = excluded()
    return [(r, s) for r in REPOS for s in repo_pool(r, ex)]
