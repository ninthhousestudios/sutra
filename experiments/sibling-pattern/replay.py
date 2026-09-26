#!/usr/bin/env python3
"""Replay one commit through the production sibling-pattern check (sutra/467).

Checks the commit out in a scratch worktree under /tmp/bt467 (one worktree and
one registered workspace, id bt467-<repo>, per repo), then runs
`sutra check --diff <sha> --format json` there. `sutra check` refreshes the
index first: a cold full parse the first time, incremental after that.
Prints the same one-block-per-commit summary as proto.py.

Usage: replay.py REPO COMMIT [--explain] [--json]
Env: SUTRA_BIN (default: target/release/sutra of this checkout),
     SUTRA_SIBLING_CONTROLS_OFF (passed through; see sibling_pattern::Controls).
"""

import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = "/tmp/bt467"
SUTRA = os.environ.get(
    "SUTRA_BIN", os.path.join(HERE, "..", "..", "target", "release", "sutra")
)


def sh(*args, cwd=None, check=True):
    return subprocess.run(args, cwd=cwd, check=check, capture_output=True, text=True)


def repo_name(repo):
    return os.path.basename(os.path.abspath(repo)).lower().replace("_", "-")


def registered_languages(repo):
    root = os.path.abspath(repo)
    for line in sh(SUTRA, "workspaces", "list").stdout.splitlines():
        parts = line.split("\t")
        if len(parts) >= 3 and os.path.abspath(parts[1]) == root:
            return [l.strip() for l in parts[2].strip("[] ").split(",") if l.strip()]
    sys.exit(f"{repo} is not a registered sutra workspace")


def worktree(repo, sha):
    name = f"bt467-{repo_name(repo)}"
    wt = f"{ROOT}/wt/{name}"
    if not os.path.isdir(wt):
        os.makedirs(f"{ROOT}/wt", exist_ok=True)
        sh("git", "worktree", "add", "--detach", wt, sha, cwd=repo)
        sh(SUTRA, "workspaces", "add", name, wt, *registered_languages(repo))
    # -f: sutra check writes .sutra/accepted.toml, which older commits track.
    sh("git", "checkout", "-q", "-f", "--detach", sha, cwd=wt)
    return wt


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    repo, sha = args[0], args[1]
    wt = worktree(repo, sha)
    # sutra logs to stdout outside `serve --stdio`, which would corrupt the
    # JSON whenever check refreshes the index first.
    out = subprocess.run(
        [SUTRA, "check", "--diff", sha, "--format", "json", "--severity", "blocking"],
        cwd=wt,
        capture_output=True,
        text=True,
        env={**os.environ, "RUST_LOG": "warn"},
    )
    try:
        report = json.loads(out.stdout)
    except json.JSONDecodeError as e:
        sys.exit(
            f"{sha}: sutra check failed (exit {out.returncode}, {e}):\n"
            f"stdout: {out.stdout[:300]!r}\nstderr: {out.stderr}"
        )
    adv = report.get("sibling_patterns") or {}
    if "--json" in sys.argv:
        print(json.dumps(adv))
        return
    if adv.get("error"):
        print(f"{sha} ERROR {adv['error']}")
        return
    findings = adv.get("findings", [])
    nrw = sum(f["class"] == "rewritten" for f in findings)
    inc = f" INCOMPLETE={len(adv['incomplete'])}" if adv.get("incomplete") else ""
    print(f"{sha} reported={len(findings)} (rewritten={nrw}){inc}")
    for f in findings:
        idiom = " | ".join(i["idiom"] for i in f["idioms"])
        kind = f["idioms"][0]["kind"]
        sites = [
            f"{s['file']}:{s['line']}"
            + (f"({s['symbol'].rsplit('::', 1)[-1]})" if s.get("symbol") else "")
            for s in f["survivors"]
        ]
        surv = ", ".join(sites[:6]) + (" …" if len(sites) > 6 else "")
        print(
            f"  {f['class']:<9} {kind:<7} {idiom[:60]:<60} n={f['survivor_count']}  {surv}"
        )
        if "--explain" in sys.argv:
            print(f"      - removed at {', '.join(f['removed_at'][:3])}")


if __name__ == "__main__":
    main()
