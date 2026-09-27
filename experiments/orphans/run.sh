#!/usr/bin/env bash
# Back-test + noise sample for the orphans advisory (sutra/483).
# bt.py parses each commit and its parent in scratch worktrees under /tmp/bt483
# (~80 s per cold parse, then incremental) and caches snapshots; replay.py then
# runs the shipped `sutra check --diff HEAD` on the same commits.
# sample.tsv (tuning) was drawn with ../dup-exists/sample.py <repo> 8 483 per repo;
# sample-held-out.tsv with seed 7, first 8 per repo not already in sample.tsv.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
echo "### back-test: does each UNWIRED case fire?"
python3 bt.py cases cases.tsv
echo "### noise: seed-483 tuning sample (the Dart and structure rules were tuned on it)"
python3 bt.py sweep sample.tsv "$@"
echo "### production replay (sutra check --diff HEAD)"
python3 replay.py cases.tsv sample.tsv
echo "### held-out sample (seed 7, rules frozen before the draw), production path"
python3 replay.py - sample-held-out.tsv
