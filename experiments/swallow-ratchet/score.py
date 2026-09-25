#!/usr/bin/env python3
"""Re-score the v1 labelled draw under the current candidates.toml (sutra/465):
which labelled sites does the refined rule set still report? A v2 hit counts
for a site if it is on the site's line or up to 2 lines above (a receiver
capture reports the receiver's line in a split chain)."""

import collections, sys

sys.path.insert(0, ".")
import swr

REPOS = {
    "sutra": "/home/josh/soft/manas/sutra",
    "yojana": "/home/josh/soft/manas/yojana",
    "swe_dashboard": "/home/josh/nhs/soft/astrology/swe_dashboard",
    "explore": "/home/josh/adityas/explore",
}
rows = [l.rstrip("\n").split("\t") for l in open("labels-v1.tsv")][1:]
cache, kept, dropped = {}, collections.Counter(), collections.Counter()
for repo, sha, site, rule, label, note in rows:
    key = (repo, sha)
    if key not in cache:
        cache[key] = swr.commit_hits(REPOS[repo], sha)
    path, line = site.rsplit(":", 1)
    line = int(line)
    still = [
        h for h in cache[key] if h["path"] == path and line - 2 <= h["line"] <= line
    ]
    (kept if still else dropped)[label] += 1
    print(
        f"{'kept   ' if still else 'DROPPED'} {label} {rule:<26} {site}  {','.join(sorted({h['rule'] for h in still}))}"
    )
print("\nkept:", dict(kept), " dropped:", dict(dropped))
