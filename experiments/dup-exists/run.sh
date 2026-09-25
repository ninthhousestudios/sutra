#!/usr/bin/env bash
# Back-test + noise sweep for sutra/463. First run parses each commit and its
# parent in scratch worktrees under /tmp/bt463 (~95 s per cold parse, then
# incremental); snapshots are cached, so reruns take seconds.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
echo "### back-test: rank of the original for each introducing commit"
python3 bt.py cases cases.tsv
echo "### noise: tuning sample (sample.py seed 463)"
python3 sweep.py "$@"
echo "### noise: held-out sample (seed 7, rule frozen)"
python3 sweep.py --held-out "$@"
