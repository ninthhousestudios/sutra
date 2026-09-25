#!/usr/bin/env python3
"""Noise sweep for sutra/463: for every function the sampled commits added,
the strongest existing match by combo (embed+lex)/2 and by block (shared
12-token runs), and how many would fire under candidate rules."""

import io, contextlib, sys
import numpy as np
import bt

SAMPLES = {
    "/home/josh/soft/manas/sutra": "2479d4a c08e685 badd76f fbbb77c 1ac2818 646259e b8743a0 9b68786 59967be 10f905d",
    "/home/josh/adityas/backend": "124f706 9fc2c0a 80e72cf 202d36d c82b8fe ed46982 cac8067 57bb1c7 631fde6 85ae96e",
    "/home/josh/nhs/soft/astrology/swe_dashboard": "72ef629 79d4489 7c2b489 83050ea ee22e79 8f8d4b2 70ba0d0 9142724 58f3e43 ebd9dc1",
}

# Held-out sample (sample.py seed 7, tuning-sample commits excluded). The rule
# was frozen before this sample was parsed.
HELD_OUT = {
    "/home/josh/soft/manas/sutra": "d70bf20 3b5f777 9253f6f c303793 f56c8a6 d5420db 01a9cb7 df8712e",
    "/home/josh/adityas/backend": "e0d9634 2ca9882 399d2f5 20f8639 3460c9f e77727a c6acd9e 312aec9",
    "/home/josh/nhs/soft/astrology/swe_dashboard": "ee3c419 0576cd5 de1ca59 cd86c7c d6631f7 eb2a2bc 6be561a 934028b",
}


def rows(samples):
    for repo, shas in samples.items():
        for sha in shas.split():
            with contextlib.redirect_stdout(io.StringIO()):
                out = bt.cmd_noise(repo, sha, k=40, show=False)
            for q, hits in out:
                live = [
                    h
                    for h in hits
                    if not set(h[2]) & {"delegates", "gone", "extracted"}
                ]
                if not live:
                    continue
                bc = max(live, key=lambda h: h[1]["combo"])
                bb = max(live, key=lambda h: h[1]["block"])
                yield repo, sha, q, bc, bb


def main():
    rules = {
        "combo>=0.5": lambda c, b: c[1]["combo"] >= 0.5,
        "combo>=0.6": lambda c, b: c[1]["combo"] >= 0.6,
        "block>=10": lambda c, b: b[1]["block"] >= 10,
        "block>=10|combo>=0.5": lambda c, b: (
            b[1]["block"] >= 10 or c[1]["combo"] >= 0.5
        ),
        "block>=10|combo>=0.6": lambda c, b: (
            b[1]["block"] >= 10 or c[1]["combo"] >= 0.6
        ),
        "block>=6|combo>=0.5": lambda c, b: b[1]["block"] >= 6 or c[1]["combo"] >= 0.5,
    }
    fired = {r: 0 for r in rules}
    commits = {r: set() for r in rules}
    n = 0
    samples = HELD_OUT if "--held-out" in sys.argv else SAMPLES
    for repo, sha, q, c, b in rows(samples):
        n += 1
        tags = [r for r, f in rules.items() if f(c, b)]
        for r in tags:
            fired[r] += 1
            commits[r].add((repo, sha))
        if tags and "-v" in sys.argv:
            print(f"{bt.repo_name(repo)[:8]:8s} {sha} {q['qn']} ({bt.nlines(q)}L)")
            print(
                f"    combo {c[1]['combo']:.2f} (emb {c[1]['embed']:.2f} lex {c[1]['lex']:.2f}) {c[0]['qn']} {c[0]['path']}:{c[0]['lines'][0]}"
            )
            print(
                f"    block {b[1]['block']:.0f} {b[0]['qn']} {b[0]['path']}:{b[0]['lines'][0]}"
            )
    print(f"new functions (>=5 lines, non-test): {n}")
    for r in rules:
        print(
            f"{r:24s} fires on {fired[r]:3d} functions ({fired[r] / n:.0%}), {len(commits[r])} commits"
        )


if __name__ == "__main__":
    main()
