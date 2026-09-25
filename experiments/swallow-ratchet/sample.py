#!/usr/bin/env python3
"""Seeded sample of ordinary commits for the noise sweep (sutra/465). Prints
`repo sha` lines; see sample_pool for the eligibility filter."""

import random
from sample_pool import REPOS, excluded, repo_pool

rng = random.Random(465)
ex = excluded()
for repo, n in REPOS.items():
    for s in sorted(rng.sample(repo_pool(repo, ex), n)):
        print(repo, s)
