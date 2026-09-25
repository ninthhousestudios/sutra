#!/usr/bin/env bash
# Back-test + noise sweep for sutra/465. Builds the harness (links sutra's real
# forbidden_pattern engine), then: back-test over cases.tsv, the 26-commit
# noise sample, the full-pool volume sweep (~15 min, writes volume-hits.json),
# and re-scores both labelled draws against the current candidates.toml.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
cargo build --release --quiet --manifest-path harness/Cargo.toml
echo "### back-test: did a rule fire on each introducing commit's offending line?"
python3 swr.py cases cases.tsv
echo "### noise sample (sample.py, seed 465)"
python3 sample.py > sample.txt
while read -r repo sha; do python3 swr.py noise "$repo" "$sha"; done < sample.txt
echo "### volume over every eligible commit"
python3 volume.py | head -16
echo "### tuning draw (v1 labels) under the current rules"
python3 score.py | tail -1
