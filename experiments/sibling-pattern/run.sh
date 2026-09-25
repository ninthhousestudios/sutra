#!/usr/bin/env bash
# Back-test + noise sample for sutra/462. Usage: run.sh [--v2]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
S=$(cd "$here/../.." && pwd); Y="$S/../yojana"; B="$HOME/adityas/backend"
p() { python3 "$here/proto.py" "$1" "$2" "${@:3}"; }
echo "### back-test (first fix of each PAR pair)"
for c in 91fedc7 f114fa5 af68577 d70bf20 ac25e6e 6e5d60e 723ac70; do p "$S" $c "$@"; done
p "$Y" 0b3ff34 "$@"; p "$B" a06a497 "$@"
echo "### noise sample (sample.py seed 462)"
for c in 9253f6f ac12896 45e6f84 95e0ed5 627ccb8 4c3d330 6a6bbb3 c7e0e98; do p "$S" $c "$@"; done
for c in cc9c4e5 c0b57ea 3cdae7e 786da8b 53db88d 3cb3ebb dd8dc0a; do p "$Y" $c "$@"; done
for c in 7dadc4b a2997e3 b46afde a00c6f1 cac8067 c6acd9e a83bca4 2f64202; do p "$B" $c "$@"; done
echo "### held-out sample (sample.py seed 7; tuning-set commits and bug fixes excluded)"
for c in d03e17e 966f565 b5952ba 01a9cb7 9949356 11e8851 824c92d 4b272aa cc674e8 a75e539 f56c8a6 8c887e3 208d371 2d3e7a2 c20f93f; do p "$S" $c "$@"; done
for c in 5e15a76 513bb17 4b7f815 43a7e08 1138076 4b42a32 7c9671b 7e77aab e7a0ec4 18ba87e 3cbcc36; do p "$Y" $c "$@"; done
for c in 3a934a2 806a6ae 5e06c98 d1736d8 ece4e88 7e8ea84 1915710 557ffa5 5fa0501 348d4c9 399d2f5 ff8d03b; do p "$B" $c "$@"; done
echo "### final sample (sample.py seed 99, v6 frozen; earlier-sample commits and bug fixes excluded)"
for c in 6576c69 fa1867d b1fcc26 5b5d52c 6ff97b3 bafae79 3ec7483 e7e8a0c dac2080 302be1d 49bbe74; do p "$S" $c "$@"; done
for c in 5fc5716 bdc147f 0b2b4a9 16b954d 0241b88 ebe3192 43d51d7 93ea5e7 c1c7a60 e70980c; do p "$Y" $c "$@"; done
for c in 4fb76ce 124f706 7694284 263c2fc 514e866 d7dc4a2 db1aafa 9fc2c0a a2725e7 80b61dc; do p "$B" $c "$@"; done
