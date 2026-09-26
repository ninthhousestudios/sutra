#!/usr/bin/env python3
"""sutra/476 measurement: behavioral_coupling pairs before vs after the fix.

Replays review's behavioral_coupling filter workspace-wide on a copy of each
index: cochange pairs (jaccard >= 0.5) minus static edges, test/non-test
mismatch dropped. OLD = indexed-count cap 50, refs-only static edges.
NEW = total-path cap 30 (git), refs + resolved imports.
"""

import re, shutil, sqlite3, subprocess, sys
from collections import Counter

WS = {
    "sutra": "/home/josh/soft/manas/sutra",
    "backend": "/home/josh/adityas/backend",
    "swe_dashboard": "/home/josh/nhs/soft/astrology/swe_dashboard",
    "varuna": "/home/josh/nhs/soft/astrology/varuna/varuna360-core",
}


def is_test(p):
    # mirrors components::is_test_file closely enough for labelling
    return bool(re.search(r"(^|/)(tests?|test_)|_test\.|\.test\.|/test/|^test/", p))


def git_counts(root, hashes):
    out = subprocess.run(
        [
            "git",
            "-C",
            root,
            "log",
            "--all",
            "--format=@%H",
            "--name-only",
            "--no-renames",
        ],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    counts, h = {}, None
    for line in out.splitlines():
        if line.startswith("@"):
            h = line[1:]
            counts[h] = 0
        elif line.strip() and h:
            counts[h] += 1
    return {k: counts.get(k) for k in hashes}


Q = """
WITH eligible AS (
  SELECT cf.commit_hash FROM commit_files cf JOIN fc ON fc.hash = cf.commit_hash
  GROUP BY cf.commit_hash HAVING {cap}
), cnt AS (
  SELECT file_id, COUNT(*) c FROM commit_files WHERE commit_hash IN (SELECT commit_hash FROM eligible) GROUP BY file_id
), sh AS (
  SELECT a.file_id fa, b.file_id fb, COUNT(*) s FROM commit_files a
  JOIN commit_files b ON a.commit_hash=b.commit_hash AND a.file_id<b.file_id
  WHERE a.commit_hash IN (SELECT commit_hash FROM eligible) GROUP BY 1,2
)
SELECT fa, fb, CAST(s AS REAL)/(ca.c+cb.c-s) j, s FROM sh
JOIN cnt ca ON ca.file_id=fa JOIN cnt cb ON cb.file_id=fb
WHERE CAST(s AS REAL)/(ca.c+cb.c-s) >= 0.5
"""


def run(ws, show=0):
    root = WS[ws]
    dst = f"/tmp/{ws}-476.db"
    shutil.copy(f"/home/josh/.sutra/{ws}/index.db", dst)
    db = sqlite3.connect(dst)
    hashes = [r[0] for r in db.execute("SELECT hash FROM commits")]
    counts = git_counts(root, hashes)
    db.execute("CREATE TEMP TABLE fc(hash TEXT PRIMARY KEY, n INTEGER)")
    db.executemany("INSERT INTO fc VALUES (?,?)", counts.items())
    paths = dict(db.execute("SELECT id, path FROM files"))
    sym_file = dict(db.execute("SELECT id, file_id FROM symbols"))
    ref_edges = set()
    for src, tgt in db.execute(
        "SELECT file_id, target_symbol_id FROM refs WHERE target_symbol_id IS NOT NULL"
    ):
        t = sym_file.get(tgt)
        if t is not None and t != src:
            ref_edges.add((min(src, t), max(src, t)))
    imp_edges = {
        (min(a, b), max(a, b))
        for a, b in db.execute(
            "SELECT file_id, resolved_file_id FROM imports WHERE resolved_file_id IS NOT NULL"
        )
        if a != b
    }

    def pairs(cap, static):
        res = []
        for fa, fb, j, s in db.execute(Q.format(cap=cap)):
            if (fa, fb) in static:
                continue
            pa, pb = paths.get(fa), paths.get(fb)
            if pa is None or pb is None or is_test(pa) != is_test(pb):
                continue
            res.append((pa, pb, round(j, 2), s))
        return res

    old = pairs("COUNT(*) <= 50", ref_edges)
    new = pairs("COALESCE(MAX(fc.n), COUNT(*)) <= 30", ref_edges | imp_edges)
    new_cap_only = pairs("COALESCE(MAX(fc.n), COUNT(*)) <= 30", ref_edges)
    new_s2 = [p for p in new if p[3] >= 2]
    sizes = Counter()
    for h, n in counts.items():
        if n is not None:
            sizes["<=30" if n <= 30 else ">30"] += 1
    print(f"== {ws}: commits={len(hashes)} {dict(sizes)}")
    print(
        f"   pairs OLD={len(old)}  cap-fix only={len(new_cap_only)}  NEW(cap+imports)={len(new)}  NEW & shared>=2={len(new_s2)}"
    )
    dropped_by_imports = set(map(tuple, new_cap_only)) - set(map(tuple, new))
    print(
        f"   dropped by import edges: {len(dropped_by_imports)}  e.g. {sorted(dropped_by_imports)[:3]}"
    )
    shared_hist = Counter(p[3] for p in new)
    print(f"   NEW shared_commits histogram: {dict(sorted(shared_hist.items()))}")
    for p in sorted(new, key=lambda p: (-p[3], -p[2]))[:show]:
        print("     ", p)
    return old, new


if __name__ == "__main__":
    show = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    for ws in WS:
        run(ws, show)
