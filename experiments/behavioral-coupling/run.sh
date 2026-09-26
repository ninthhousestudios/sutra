#!/usr/bin/env bash
# sutra/476 measurement. measure.py: behavioral_coupling pairs before/after on
# copies of the live indexes (~/.sutra/<ws>/index.db). backtest.py: partner list
# replayed at the parent of each additive-PAR first fix from sutra/462.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
S=/home/josh/soft/manas/sutra
B=/home/josh/adityas/backend
python3 "$here/measure.py" 12
python3 "$here/backtest.py" $S 6e5d60e .rs src/tools/trend.rs
python3 "$here/backtest.py" $S ac25e6e .rs src/constraints/check.rs
python3 "$here/backtest.py" $S 723ac70 .rs src/constraints/check.rs,src/constraints/external.rs,src/constraints/mod.rs
python3 "$here/backtest.py" $B a06a497 .rs server/src/chat/engine.rs
