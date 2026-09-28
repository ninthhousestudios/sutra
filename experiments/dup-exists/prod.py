#!/usr/bin/env python3
"""Run the back-test and noise samples through the production dup-exists
check (sutra/469): `sutra check --diff <sha>` in a scratch worktree checked
out at <sha>, so the index holds exactly the commit under review.

  prod.py run   [--repo PATH]   check every case and sample commit (cached)
  prod.py cases                 did each back-test case fire?
  prod.py noise [--held-out|--python] [-v]   volume per sample: units, pairs, groups

Uses $SUTRA_BIN (default: the release build of this checkout) under an
isolated HOME ($ROOT/home), so the scratch workspaces and their firing logs
stay out of the real registry. Results are cached as JSON under $ROOT/out.
"""

import csv
import json
import os
import statistics
import subprocess
import sys
from collections import defaultdict

import sweep

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.environ.get("SUTRA_BT_ROOT", "/tmp/bt469")
BIN = os.environ.get(
    "SUTRA_BIN", os.path.join(HERE, "..", "..", "target", "release", "sutra")
)
ENV = dict(os.environ, HOME=f"{ROOT}/home")


def sh(*args, cwd=None, env=None, check=True):
    return subprocess.run(
        args, cwd=cwd, env=env, check=check, capture_output=True, text=True
    ).stdout


def name(repo):
    return os.path.basename(os.path.abspath(repo)).lower().replace("_", "-")


def languages(repo):
    root = os.path.abspath(repo)
    for line in sh("sutra", "workspaces", "list").splitlines():
        parts = line.split("\t")
        if len(parts) >= 3 and os.path.abspath(parts[1]) == root:
            return [l.strip() for l in parts[2].strip("[] ").split(",") if l.strip()]
    sys.exit(f"{repo} is not a registered sutra workspace")


def out_path(repo, sha):
    return f"{ROOT}/out/{name(repo)}-{sha[:12]}.json"


def check(repo, sha):
    """The dup_exists section of `sutra check --diff <sha>` at <sha>."""
    sha = sh("git", "rev-parse", sha + "^{commit}", cwd=repo).strip()
    out = out_path(repo, sha)
    if os.path.exists(out):
        with open(out) as f:
            return json.load(f)
    ws = f"bt469-{name(repo)}"
    wt = f"{ROOT}/wt/{ws}"
    if not os.path.isdir(wt):
        os.makedirs(f"{ROOT}/wt", exist_ok=True)
        os.makedirs(ENV["HOME"], exist_ok=True)
        sh("git", "worktree", "prune", cwd=repo)
        sh("git", "worktree", "add", "--detach", wt, sha, cwd=repo)
        sh(BIN, "workspaces", "add", ws, wt, *languages(repo), env=ENV)
    sh("git", "checkout", "-q", "-f", "--detach", sha, cwd=wt)
    sh(BIN, "parse", ws, env=ENV)
    raw = sh(
        BIN, "check", "--diff", sha, "--format", "json", cwd=wt, env=ENV, check=False
    )
    result = json.loads(raw)["dup_exists"]
    os.makedirs(f"{ROOT}/out", exist_ok=True)
    with open(out, "w") as f:
        json.dump(result, f, indent=1)
    return result


def names_match(qn, want):
    return qn == want or qn.endswith("::" + want)


def cases():
    with open(os.path.join(HERE, "cases.tsv")) as f:
        rows = list(csv.DictReader(f, delimiter="\t"))
    for r in rows:
        if r["intro_commit"] in ("UNFOUND", "-") or r["new_fn"] in ("", "-"):
            continue
        if not os.path.isdir(r["repo_path"]):
            print(f"{r['case']:16s} SKIP no repo")
            continue
        res = check(r["repo_path"], r["intro_commit"])
        hit, unit = None, False
        for g in res.get("groups", []):
            for p in g["pairs"]:
                if names_match(p["symbol"], r["new_fn"]) and (
                    not r["new_file"] or g["file"] == r["new_file"]
                ):
                    unit = True
                    if names_match(p["matches"], r["orig_fn"]) and (
                        not r["orig_file"] or g["matched_file"] == r["orig_file"]
                    ):
                        hit = p
                # A same-change pair is reported once, from either side.
                if names_match(p["symbol"], r["orig_fn"]) and names_match(
                    p["matches"], r["new_fn"]
                ):
                    hit = hit or p
        verdict = (
            f"FIRES [{hit['kind']}] combo {hit['combo']:.2f} runs {hit['shared_runs']}"
            if hit
            else ("fires, other match" if unit else "no")
        )
        extra = "; ".join(res.get("incomplete", [])) or res.get("skipped") or ""
        print(f"{r['case']:16s} {r['intro_commit'][:8]} {verdict}  {extra[:120]}")


def noise(sample, verbose):
    samples = {"held-out": sweep.HELD_OUT, "python": sweep.PYTHON}.get(
        sample, sweep.SAMPLES
    )
    per_commit = []
    added_units = modified_units = 0
    existing = defaultdict(set)
    for repo, shas in samples.items():
        for sha in shas.split():
            res = check(repo, sha)
            groups = res.get("groups", [])
            units = defaultdict(set)
            for g in groups:
                for p in g["pairs"]:
                    units[p["kind"]].add((g["file"], p["symbol"]))
                    if not p.get("same_change"):
                        existing[p["kind"]].add((sha, g["file"], p["symbol"]))
            added_units += len(units["added"])
            modified_units += len(units["modified"])
            pairs = sum(len(g["pairs"]) for g in groups)
            per_commit.append(len(groups))
            print(
                f"{name(repo)[:10]:10s} {sha} checked {res.get('checked', 0):3d}  "
                f"fired added {len(units['added']):2d} modified {len(units['modified']):2d}  "
                f"pairs {pairs:3d}  groups {len(groups):2d}"
                + (f"  INCOMPLETE {res['incomplete']}" if res.get("incomplete") else "")
                + (f"  SKIPPED {res['skipped']}" if res.get("skipped") else "")
                + (f"  ERROR {res['error']}" if res.get("error") else "")
            )
            if verbose:
                for g in groups:
                    print(f"    {g['file']} ~ {g['matched_file']}")
                    for p in g["pairs"]:
                        print(
                            f"      [{p['kind'][0]}] {p['symbol']} ~ {p['matches']}  "
                            f"combo {p['combo']:.2f} (emb {p['embed']:.2f} lex {p['lex']:.2f}) "
                            f"runs {p['shared_runs']}"
                            + ("  same-change" if p.get("same_change") else "")
                        )
    print(
        f"commits {len(per_commit)}; fired units: added {added_units}, modified {modified_units}; "
        f"groups per review: median {statistics.median(per_commit)}, "
        f"mean {statistics.mean(per_commit):.1f}, max {max(per_commit)}; "
        f"commits with a group {sum(1 for g in per_commit if g)}"
    )
    print(
        f"fired against pre-existing code (not same-change): "
        f"added {len(existing['added'])}, modified {len(existing['modified'])}"
    )


if __name__ == "__main__":
    a = sys.argv[1:]
    if a[0] == "run":
        repos = [a[a.index("--repo") + 1]] if "--repo" in a else None
        todo = defaultdict(list)
        with open(os.path.join(HERE, "cases.tsv")) as f:
            for r in csv.DictReader(f, delimiter="\t"):
                if r["intro_commit"] not in ("UNFOUND", "-") and r["new_fn"] not in (
                    "",
                    "-",
                ):
                    todo[r["repo_path"]].append(r["intro_commit"])
        for s in (sweep.SAMPLES, sweep.HELD_OUT, sweep.PYTHON):
            for repo, shas in s.items():
                todo[repo].extend(shas.split())
        for repo, shas in todo.items():
            if repos and os.path.abspath(repo) not in map(os.path.abspath, repos):
                continue
            if not os.path.isdir(repo):
                continue
            for sha in shas:
                check(repo, sha)
                print(f"done {name(repo)} {sha}", flush=True)
    elif a[0] == "cases":
        cases()
    elif a[0] == "noise":
        sample = next((f[2:] for f in a if f in ("--held-out", "--python")), None)
        noise(sample, "-v" in a)
