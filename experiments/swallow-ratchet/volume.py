#!/usr/bin/env python3
"""Hit volume per rule over every eligible commit (sample.py's filter, no
sampling), plus a seeded draw of added-line hits for labelling (sutra/465)."""

import json, random, re, subprocess, sys, collections

sys.path.insert(0, ".")
import swr, sample_pool

per_rule, commits_hit, n_commits, allhits = (
    collections.Counter(),
    collections.Counter(),
    0,
    [],
)
for repo, sha in sample_pool.pool():
    n_commits += 1
    hits = [h for h in swr.commit_hits(repo, sha) if h["added"]]
    for rule in {h["rule"] for h in hits}:
        commits_hit[rule] += 1
    for h in hits:
        per_rule[h["rule"]] += 1
        allhits.append((repo.rsplit("/", 1)[1], sha, h))
fired = len({(r, s) for r, s, _ in allhits})
print(f"{n_commits} commits, {fired} with >=1 added-line hit, {len(allhits)} hits")
for rule, n in per_rule.most_common():
    print(f"  {rule:<26} {n:>4} hits in {commits_hit[rule]:>3} commits")
rng = random.Random(4650)
json.dump(
    [dict(h, repo=r, sha=s) for r, s, h in allhits], open("volume-hits.json", "w")
)
for r, s, h in sorted(
    rng.sample(allhits, min(40, len(allhits))), key=lambda x: x[2]["rule"]
):
    print(f"{r} {s} {swr.fmt(h).strip()}")
