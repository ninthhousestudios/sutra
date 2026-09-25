#!/usr/bin/env python3
"""Seeded sample of ordinary (non-fix) commits that remove >= 5 non-test
source lines, for the sutra/462 noise measurement. Usage: sample.py REPO N [SEED]"""

import random
import subprocess
import sys

repo, n = sys.argv[1], int(sys.argv[2])
log = subprocess.run(
    ["git", "-C", repo, "log", "--no-merges", "--format=%h %s", "--numstat"],
    capture_output=True,
    text=True,
    check=True,
).stdout
commits, cur = [], None
for line in log.splitlines():
    if not line.strip():
        continue
    parts = line.split("\t")
    if len(parts) == 3 and cur:
        added, removed, path = parts
        if (
            path.endswith((".rs", ".dart"))
            and removed.isdigit()
            and "test" not in path
            and not path.startswith("examples/")
        ):
            cur[2] += int(removed)
    else:
        sha, _, subj = line.partition(" ")
        cur = [sha, subj, 0]
        commits.append(cur)
pool = [
    c
    for c in commits
    if c[2] >= 5
    and not c[1].lower().startswith(("fix", "revert"))
    and "fix" not in c[1].split(":")[0].lower()
]
random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 462).shuffle(pool)
for sha, subj, rem in pool[:n]:
    print(f"{sha}\t{rem}\t{subj[:90]}")
