#!/usr/bin/env python3
"""Held-out draw for labelling (sutra/465): 30 v2 added-line hits (seed 7),
one per site, excluding sites in the tuning labels. Prints context."""

import json, random, subprocess

REPOS = {
    "sutra": "/home/josh/soft/manas/sutra",
    "yojana": "/home/josh/soft/manas/yojana",
    "swe_dashboard": "/home/josh/nhs/soft/astrology/swe_dashboard",
    "explore": "/home/josh/adityas/explore",
}
tuned = {
    (l.split("\t")[1], l.split("\t")[2])
    for l in open("labels-v1.tsv").read().splitlines()[1:]
}
sites = {}
for h in json.load(open("volume-hits.json")):
    site = f"{h['path']}:{h['line']}"
    if (h["sha"], site) not in tuned:
        sites.setdefault((h["repo"], h["sha"], site), []).append(h["rule"])
rng = random.Random(7)
for (repo, sha, site), rules in sorted(rng.sample(sorted(sites.items()), 30)):
    path, line = site.rsplit(":", 1)
    n = int(line)
    src = subprocess.run(
        ["git", "-C", REPOS[repo], "show", f"{sha}:{path}"],
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    print(f"### {repo}\t{sha}\t{site}\t{','.join(sorted(set(rules)))}")
    for i in range(max(0, n - 5), min(len(src), n + 3)):
        print(("> " if i == n - 1 else "  ") + src[i][:140])
