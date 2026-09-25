#!/usr/bin/env python3
"""Back-test harness for the "this already exists" check (sutra/463).

Snapshots a commit's function corpus (HRR strip/embed vectors from a real
`sutra parse`, plus source text) and ranks, for each function the commit
added, the most similar functions that existed at its parent.

  bt.py snap  <repo> <sha>                 parse <sha> in a scratch worktree, dump
  bt.py rank  <repo> <sha> <new> <orig>    where does <orig> rank for <new>?
  bt.py noise <repo> <sha> [--k K]         top hits for every function <sha> added

Snapshots are cached under /tmp/bt463/snap. One worktree + one registered
workspace (id bt463-<name>) per repo; walking commits reparses incrementally.
"""

import math
import os
import pickle
import re
import sqlite3
import subprocess
import sys
from collections import Counter

import numpy as np

ROOT = "/tmp/bt463"
DIM = 1024


def sh(*args, cwd=None):
    return subprocess.run(
        args, cwd=cwd, check=True, capture_output=True, text=True
    ).stdout


def repo_name(repo):
    return os.path.basename(os.path.abspath(repo)).lower().replace("_", "-")


def full_sha(repo, sha):
    return sh("git", "rev-parse", sha + "^{commit}", cwd=repo).strip()


def registered_languages(repo):
    root = os.path.abspath(repo)
    for line in sh("sutra", "workspaces", "list").splitlines():
        parts = line.split("\t")
        if len(parts) >= 3 and os.path.abspath(parts[1]) == root:
            return [l.strip() for l in parts[2].strip("[] ").split(",") if l.strip()]
    sys.exit(
        f"{repo} is not a registered sutra workspace; pass languages via SUTRA_BT_LANGS"
    )


def snap_path(repo, sha):
    return f"{ROOT}/snap/{repo_name(repo)}-{sha[:12]}.pkl"


def decode(blob):
    scale = np.frombuffer(blob[:4], dtype="<f4")[0]
    v = np.frombuffer(blob[4:], dtype=np.int8).astype(np.float32) * scale
    n = np.linalg.norm(v)
    return v / n if n else v


def snap(repo, sha):
    sha = full_sha(repo, sha)
    out = snap_path(repo, sha)
    if os.path.exists(out):
        return out
    name = f"bt463-{repo_name(repo)}"
    wt = f"{ROOT}/wt/{name}"
    if not os.path.isdir(wt):
        os.makedirs(f"{ROOT}/wt", exist_ok=True)
        sh("git", "worktree", "add", "--detach", wt, sha, cwd=repo)
        langs = os.environ.get("SUTRA_BT_LANGS", "").split() or registered_languages(
            repo
        )
        sh("sutra", "workspaces", "add", name, wt, *langs)
    sh("git", "checkout", "-q", "--detach", sha, cwd=wt)
    sh("sutra", "parse", name)
    db = sqlite3.connect(os.path.expanduser(f"~/.sutra/{name}/index.db"))
    rows = db.execute(
        """SELECT s.id, f.path, s.qualified_name, s.kind, s.start_line, s.end_line,
                  hs.vector, he.vector, f.language
           FROM symbols s JOIN files f ON f.id = s.file_id
           JOIN hrr_vectors hs ON hs.symbol_id = s.id AND hs.mode = 'strip'
           LEFT JOIN hrr_vectors he ON he.symbol_id = s.id AND he.mode = 'embed'"""
    ).fetchall()
    texts = {}
    fns = []
    for sid, path, qn, kind, a, b, vs, ve, lang in rows:
        if path not in texts:
            try:
                with open(
                    os.path.join(wt, path), encoding="utf-8", errors="replace"
                ) as f:
                    texts[path] = f.read().split("\n")
            except OSError:
                texts[path] = []
        body = "\n".join(texts[path][a - 1 : b])
        fns.append(
            dict(
                path=path,
                qn=qn,
                kind=kind,
                lines=(a, b),
                lang=lang,
                body=body,
                strip=decode(vs),
                embed=decode(ve) if ve else None,
            )
        )
    os.makedirs(f"{ROOT}/snap", exist_ok=True)
    with open(out, "wb") as f:
        pickle.dump(fns, f)
    return out


def load(repo, sha):
    with open(snap(repo, sha), "rb") as f:
        return pickle.load(f)


# ---------------------------------------------------------------- scoring

IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
KEYWORDS = set(
    """fn let mut pub use mod impl self Self return if else match for in while loop
    break continue as ref struct enum trait where async await move const static type
    true false None Some Ok Err crate super dyn unsafe extern final var void class new
    this null final late required import await def elif pass lambda not and or is
    String str Vec Option Result usize i64 i32 u8 u32 u64 f64 bool int double dynamic
    override get set async""".split()
)


def subtokens(text):
    out = []
    for w in IDENT.findall(text):
        if w in KEYWORDS:
            continue
        for p in re.split(r"_+|(?<=[a-z0-9])(?=[A-Z])", w):
            if len(p) > 1:
                out.append(p.lower())
    return out


def body_only(fn):
    """Drop the signature line(s) so a shared name doesn't dominate lexical score."""
    body = fn["body"]
    i = body.find("{")
    j = body.find("=>")
    cut = min(x for x in (i, j, len(body)) if x >= 0)
    return body[cut:]


TOKEN = re.compile(r'[A-Za-z_][A-Za-z0-9_]*|\d+|"[^"\n]*"|\'[^\'\n]*\'|\S')
SHINGLE_K = 12
BLOCK_MAX_DF = 3


def shingles(fn):
    """Token 12-grams of the body: a shared one is a shared run of >= 12 tokens."""
    toks = TOKEN.findall(body_only(fn))
    return {
        hash(tuple(toks[i : i + SHINGLE_K])) for i in range(len(toks) - SHINGLE_K + 1)
    }


class Lexical:
    """tf-idf cosine over identifier subtokens of function bodies."""

    def __init__(self, corpus):
        self.df = Counter()
        for fn in corpus:
            self.df.update(set(subtokens(body_only(fn))))
        self.n = len(corpus)
        self.vecs = [self.vec(fn) for fn in corpus]
        self.sh = [shingles(fn) for fn in corpus]
        self.sh_df = Counter()
        for c in self.sh:
            self.sh_df.update(c)

    def block_scores(self, q):
        """Shared token runs, counting only runs rare in the corpus (in <= 3
        functions): common idioms like a tree-sitter cursor walk are not copies."""
        qs = {h for h in shingles(q) if self.sh_df.get(h, 0) <= BLOCK_MAX_DF}
        return np.array([len(qs & c) for c in self.sh], dtype=float)

    def vec(self, fn):
        tf = Counter(subtokens(body_only(fn)))
        v = {
            t: (1 + math.log(c)) * math.log((self.n + 1) / (self.df.get(t, 0) + 1))
            for t, c in tf.items()
        }
        n = math.sqrt(sum(x * x for x in v.values())) or 1.0
        return {t: x / n for t, x in v.items()}

    def scores(self, q):
        qv = self.vec(q)
        return np.array([sum(qv[t] * d.get(t, 0.0) for t in qv) for d in self.vecs])


def is_test(fn):
    p, qn = fn["path"], fn["qn"]
    return (
        "tests::" in qn
        or qn.startswith("test_")
        or "::test_" in qn
        or re.search(r"(^|/)(tests?|test_driver|integration_test|benches)/", p)
        or re.search(r"(_test|_tests|\.test)\.\w+$", p)
    )


def nlines(fn):
    return fn["lines"][1] - fn["lines"][0] + 1


def norm_body(fn):
    return re.sub(r"\s+", "", body_only(fn))


def added_functions(parent, child):
    """Functions in child whose name is new to their file and whose body has no exact copy at parent."""
    before = {(f["path"], f["qn"]) for f in parent}
    bodies = {norm_body(f) for f in parent}
    names = Counter(f["qn"] for f in parent)
    out = []
    for f in child:
        if (f["path"], f["qn"]) in before:
            continue
        moved = norm_body(f) in bodies
        out.append(
            dict(f, moved=moved, renamed_or_moved_name=names.get(f["qn"], 0) > 0)
        )
    return out


def short(qn):
    return re.split(r"::|\.", qn)[-1]


def calls(body, name):
    return (
        re.search(r"(?<![A-Za-z0-9_])" + re.escape(name) + r"\s*(<[^>]*>)?\s*\(", body)
        is not None
    )


def hit_flags(q, c, child_by_key):
    """Why a hit is not a duplicate-in-waiting. delegates: q calls c (reuse).
    extracted: c was edited in the same commit to call q, or c is gone from its
    file (the commit moved/extracted it) — both only knowable at review time."""
    flags = []
    body_q = body_only(q)
    if calls(body_q, short(c["qn"])):
        flags.append("delegates")
    after = child_by_key.get((c["path"], c["qn"]))
    if after is None:
        flags.append("gone")
    elif norm_body(after) != norm_body(c):
        shared_before = len(shingles(q) & shingles(c))
        shared_after = len(shingles(q) & shingles(after))
        if calls(after["body"], short(q["qn"])) or (
            shared_before >= 4 and shared_after < shared_before / 2
        ):
            flags.append("extracted")
    return flags


def rank_all(q, corpus, lex, lang_only=True):
    s = np.stack([c["strip"] for c in corpus]) @ q["strip"]
    e = (
        np.stack([c["embed"] for c in corpus]) @ q["embed"]
        if q["embed"] is not None and all(c["embed"] is not None for c in corpus)
        else np.zeros(len(corpus))
    )
    l = lex.scores(q)
    b = lex.block_scores(q)
    if lang_only:
        mask = np.array([c["lang"] == q["lang"] for c in corpus])
        s, e, l, b = (np.where(mask, x, -1) for x in (s, e, l, b))
    return dict(strip=s, embed=e, lex=l, combo=(e + l) / 2, block=b)


def eligible(corpus, min_lines):
    return [c for c in corpus if not is_test(c) and nlines(c) >= min_lines]


def find(fns, name, path=None):
    hits = [
        f
        for f in fns
        if (
            f["qn"] == name
            or f["qn"].endswith("::" + name)
            or f["qn"].endswith("." + name)
        )
        and (path is None or f["path"] == path)
    ]
    return hits


# ---------------------------------------------------------------- commands


def added_lines(old, new):
    import difflib

    a, b = old.split("\n"), new.split("\n")
    out = []
    for op, _i1, _i2, j1, j2 in difflib.SequenceMatcher(
        None, a, b, autojunk=False
    ).get_opcodes():
        if op in ("insert", "replace"):
            out.extend(b[j1:j2])
    return "\n".join(out)


def cmd_rank(repo, sha, new, orig, new_path=None, orig_path=None, label=""):
    """Rank of <orig> among the parent's non-test functions for query <new> (child
    version). If <orig> only exists in the child (both copies landed in one
    commit), its child version joins the corpus: the second copy is written
    after the first, so an edit-time check would see it."""
    sha = full_sha(repo, sha)
    child, parent = load(repo, sha), load(repo, sha + "^")
    qs = find(child, new, new_path)
    corpus = [c for c in parent if not is_test(c)]
    os_ = find(corpus, orig, orig_path)
    same = False
    if not os_:
        os_ = [o for o in find(child, orig, orig_path) if not is_test(o)]
        same = bool(os_)
        corpus += os_
    tag = f"{label:18s} {repo_name(repo)[:12]:12s} {sha[:8]}"
    if not qs or not os_:
        print(f"{tag} MISSING new={len(qs)} orig={len(os_)} ({new} / {orig})")
        return None
    q = qs[0]
    before = [c for c in parent if c["path"] == q["path"] and c["qn"] == q["qn"]]
    corpus = [c for c in corpus if c is not q and not any(c is b for b in before)]
    lex = Lexical(corpus)
    if before:
        # Modified function: the edit is the added lines, not the whole body.
        q = dict(q, body="{\n" + added_lines(before[0]["body"], q["body"]))
    sc = rank_all(q, corpus, lex)
    idx = [i for i, c in enumerate(corpus) if any(c is o for o in os_)]
    child_by_key = {(f["path"], f["qn"]): f for f in child}
    fl = hit_flags(q, os_[0], child_by_key)
    res = {}
    for m, v in sc.items():
        order = np.argsort(-v, kind="stable")
        pos = {int(i): r + 1 for r, i in enumerate(order)}
        best = min(idx, key=lambda i: pos[i])
        res[m] = (pos[best], float(v[best]))
    was_new = not before
    cols = "  ".join(f"{m} {r:>4}/{v:.2f}" for m, (r, v) in res.items() if m != "strip")
    print(
        f"{tag} {'new' if was_new else 'mod'}{'+same' if same else '':5s} "
        f"q={short(q['qn'])}({nlines(q)}L) o={short(os_[0]['qn'])}({nlines(os_[0])}L) "
        f"N={len(corpus)}  strip {res['strip'][0]:>4}/{res['strip'][1]:.2f}  {cols}  {','.join(fl)}"
    )
    return res


def cmd_noise(repo, sha, k=3, min_lines=5, show=True):
    sha = full_sha(repo, sha)
    child, parent = load(repo, sha), load(repo, sha + "^")
    news = [
        f
        for f in added_functions(parent, child)
        if not is_test(f) and not f["moved"] and nlines(f) >= min_lines
    ]
    corpus = eligible(parent, min_lines)
    if not corpus:
        return []
    lex = Lexical(corpus)
    child_by_key = {(f["path"], f["qn"]): f for f in child}
    out = []
    for q in news:
        sc = rank_all(q, corpus, lex)
        top = np.argsort(-sc["combo"])[:k]
        out.append(
            (
                q,
                [
                    (
                        corpus[i],
                        {m: float(v[i]) for m, v in sc.items()},
                        hit_flags(q, corpus[i], child_by_key),
                    )
                    for i in top
                ],
            )
        )
    if show:
        print(
            f"== {repo_name(repo)} {sha[:9]} {sh('git', 'log', '-1', '--format=%s', sha, cwd=repo).strip()[:70]}"
        )
        print(f"   added={len(news)} corpus={len(corpus)}")
        for q, hits in out:
            print(f"   + {q['qn']} ({q['path']}:{q['lines'][0]}, {nlines(q)}L)")
            for c, s, fl in hits:
                print(
                    f"     {','.join(fl):>10s} combo {s['combo']:.3f} emb {s['embed']:.3f} lex {s['lex']:.3f} strip {s['strip']:.3f}  "
                    f"{c['qn']} ({c['path']}:{c['lines'][0]}, {nlines(c)}L)"
                )
    return out


if __name__ == "__main__":
    a = sys.argv[1:]
    if a[0] == "snap":
        print(snap(a[1], full_sha(a[1], a[2])))
    elif a[0] == "rank":
        cmd_rank(*a[1:])
    elif a[0] == "cases":
        import csv

        for r in csv.DictReader(open(a[1]), delimiter="\t"):
            if (
                r["intro_commit"] in ("UNFOUND", "-")
                or not r["new_fn"]
                or r["new_fn"] == "-"
            ):
                continue
            if os.environ.get("ONLY") and r["case"] not in os.environ["ONLY"].split():
                continue
            if "LANGS" in r and r["LANGS"]:
                os.environ["SUTRA_BT_LANGS"] = r["LANGS"]
            cmd_rank(
                r["repo_path"],
                r["intro_commit"],
                r["new_fn"],
                r["orig_fn"],
                r.get("new_file") or None,
                r.get("orig_file") or None,
                label=r["case"],
            )
    elif a[0] == "noise":
        cmd_noise(a[1], a[2], k=int(a[a.index("--k") + 1]) if "--k" in a else 3)
