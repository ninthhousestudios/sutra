#!/usr/bin/env python3
"""Throwaway prototype for sutra/462: "you fixed 1 of N".

Given a commit, extract the idioms its diff removed and list the places in the
post-commit tree where the same idiom survives. Not production code: it exists
to measure hit rate on known PAR fixes and noise on ordinary commits before
anything is built into sutra. Design and results: docs/sibling-pattern-backtest.md.

An idiom is a small named unit, not an arbitrary token n-gram. Features,
extracted from the removed lines of each non-test source hunk:

  chain   callee_a(...).method_b(...)     `serde_json::from_str.unwrap_or_default`
          at least one of the two names must be non-generic (not iter/map/...)
  argfld  callee(... x.field ...)         `serde_json::from_str(.category_scores)`
          callee must be non-generic
  litpair two value-position string literals <= PAIR_TOKENS tokens apart
          `"dart"+"rust"`; map keys, format templates, punctuation excluded
  sql     first 40 chars of a string literal >= 30 chars
          `"SELECT id, constraint_id, ...`

Classes, per feature:
  rewritten  the diff lowered its count in the files it touched
  wrapped    count unchanged, but re-added within NEAR lines of the hunk it was
             removed from (old code kept inside a new branch, sutra/308)
  (moved)    count unchanged and re-added elsewhere: dropped, a move is not a fix

Hunks whose removed and added token streams are identical (formatting) are
skipped.

Survivors are occurrences in the post-commit tree, same language, outside
tests and `#[cfg(test)]` tails, not on a line the diff added. A feature is
reported when 1 <= survivors <= MAX_SURV and its pre-commit repo-wide count is
<= MAX_DF.

Usage: proto.py REPO COMMIT [--json] [--v2]   (--v2 disables the v3 filters)
"""

import json
import re
import subprocess
import sys
from collections import defaultdict

MAX_SURV = 25
MAX_DF = 40
PAIR_TOKENS = 30
MIN_SQL = 30
SQL_PREFIX = 40
MAX_LIT = 40
NEAR = 40

V2 = "--v2" in sys.argv
V3 = V2 or "--v3" in sys.argv  # --v3 disables the v4 filters
SWEEP_HUNKS = 3
V4 = V3 or "--v4" in sys.argv  # --v4 disables the v5 wrap rule
V5 = V4 or "--v5" in sys.argv  # --v5 disables the v6 argfld rule and std names

EXTS = {".rs": "rust", ".dart": "dart"}

KEYWORDS = set(
    """fn let mut pub if else match for in while loop return self Self Some None
    Ok Err true false as ref impl struct enum use mod crate super where const
    static async await move dyn break continue trait type unsafe extern
    final var void new this null class extends implements with late required
    import export library part of is try catch on throw rethrow switch case
    default do get set factory override""".split()
)

# Standard-library / ubiquitous method names. A chain of two of these, or an
# argfld on one of them, is an idiom of the language, not of the codebase.
GENERIC = set(
    """iter iter_mut into_iter map filter filter_map flat_map flatten collect
    entry or_insert or_insert_with or_default push push_str insert get get_mut
    contains contains_key remove unwrap expect unwrap_or unwrap_or_default
    unwrap_or_else and_then or_else ok err ok_or ok_or_else map_err map_or
    map_or_else as_ref as_deref as_mut as_str as_bytes as_slice to_string
    to_owned to_vec clone cloned copied into find find_map any all position
    enumerate take skip zip chain rev sum count len is_empty keys values
    sort sort_by sort_by_key sort_unstable dedup split lines trim strip_prefix
    strip_suffix starts_with ends_with join extend windows chunks max min
    max_by min_by fold last first next peekable is_some is_none is_ok is_err
    transpose lock read write send json post parse format query_map prepare
    prepare_cached execute query_row display to_lowercase to_uppercase
    partial_cmp cmp eq retain drain split_whitespace chars bytes to_hex
    from new default with_capacity then then_some unwrap_or_default
    toList where firstWhere add addAll containsKey putIfAbsent""".split()
)
# Added in v6 (std path/time/numeric methods seen as held-out noise).
GENERIC_V6 = set(
    """extension file_name file_stem parent exists is_file is_dir to_str
    to_string_lossy canonicalize elapsed as_millis as_secs as_secs_f64 now
    duration_since saturating_sub saturating_add checked_sub checked_add abs
    round floor ceil powi sqrt pop push_back is_some_and is_none_or
    then_with""".split()
)

TOKEN_RE = re.compile(
    r"""
    (?P<ws>\s+)
  | (?P<lcomment>//[^\n]*)
  | (?P<bcomment>/\*.*?\*/)
  | (?P<rstr>r\#*"(?:.|\n)*?"\#*)
  | (?P<str>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\\n]){2,}')
  | (?P<char>'(?:\\.|[^'\\])')
  | (?P<life>'[A-Za-z_]\w*)
  | (?P<ident>[A-Za-z_]\w*)
  | (?P<num>\d[\w.]*)
  | (?P<op>::|->|=>|\.\.=?|==|!=|<=|>=|&&|\|\||\?\?|\?\.)
  | (?P<punct>.)
    """,
    re.VERBOSE | re.DOTALL,
)


def tokenize(text, first_line=1):
    """[(tok, line, kind)] with comments dropped."""
    out = []
    line = first_line
    for m in TOKEN_RE.finditer(text):
        kind = m.lastgroup
        tok = m.group()
        if kind not in ("ws", "lcomment", "bcomment"):
            if kind in ("str", "rstr"):
                tok = re.sub(r"\\\s*\n\s*", " ", tok)
                tok = re.sub(r"\s+", " ", tok)
            out.append((tok, line, kind))
        line += m.group().count("\n")
    return out


def skip_group(toks, i, open_, close):
    """Index just past the group opening at toks[i]."""
    depth = 0
    while i < len(toks):
        if toks[i][0] == open_:
            depth += 1
        elif toks[i][0] == close:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return i


def callee_at(toks, i):
    """If toks[i] is an identifier that is called: (name, bare, index of `(`)."""
    tok, _, kind = toks[i]
    if kind != "ident" or tok in KEYWORDS:
        return None
    j = i + 1
    if j < len(toks) and toks[j][0] == "!":  # macro
        return None
    if j + 1 < len(toks) and toks[j][0] == "::" and toks[j + 1][0] == "<":
        j = skip_group(toks, j + 1, "<", ">")
    if j < len(toks) and toks[j][0] == "(":
        name = tok
        if i >= 2 and toks[i - 1][0] == "::" and toks[i - 2][2] == "ident":
            name = f"{toks[i - 2][0]}::{tok}"
        elif i >= 1 and toks[i - 1][0] in (".", "?."):
            name = f".{tok}"
        return name, tok, j
    return None


def is_generic(bare, name):
    if V2 or "::" in name:
        return False
    return bare in GENERIC or (not V5 and bare in GENERIC_V6)


def value_literal(toks, i):
    """A string literal in value position: not a map key, not a format
    template, not bare punctuation."""
    tok = toks[i][0]
    if not (2 < len(tok) <= MAX_LIT):
        return False
    if V2:
        return True
    if "{" in tok or not re.search(r"[A-Za-z0-9]", tok):
        return False
    nxt = toks[i + 1][0] if i + 1 < len(toks) else ""
    if nxt == ":":
        return False
    if nxt == "." and i + 2 < len(toks) and toks[i + 2][0] == "into":
        return False  # m.insert("key".into(), ...)
    prev2 = [t for t, _, _ in toks[max(0, i - 2) : i]]
    if prev2 in (["insert", "("], ["get", "("], ["contains_key", "("]):
        return False
    return True


def features(text, first_line=1):
    """{feature: [line]}"""
    toks = tokenize(text, first_line)
    out = defaultdict(list)
    for i, (tok, line, kind) in enumerate(toks):
        call = callee_at(toks, i)
        if call:
            name, bare, paren = call
            end = skip_group(toks, paren, "(", ")")
            if not is_generic(bare, name):
                depth = 0
                for k in range(paren, end):
                    t = toks[k][0]
                    depth += t == "("
                    depth -= t == ")"
                    if (
                        depth == 1
                        and t == "."
                        and k + 1 < end
                        and toks[k + 1][2] == "ident"
                        and (k + 2 >= end or toks[k + 2][0] not in ("(", "::"))
                    ):
                        out[("argfld", f"{name}(.{toks[k + 1][0]})")].append(line)
            j = end
            if j < len(toks) and toks[j][0] == "?":
                j += 1
            if j + 1 < len(toks) and toks[j][0] in (".", "?."):
                nxt = callee_at(toks, j + 1)
                if nxt and not (is_generic(bare, name) and is_generic(nxt[1], nxt[0])):
                    out[("chain", f"{name}.{nxt[1]}")].append(line)
        elif V2 and (
            kind == "ident"
            and tok[0].isupper()
            and i + 1 < len(toks)
            and toks[i + 1][0] == "{"
            and i >= 1
            and toks[i - 1][0] in ("(", "=", ",", "=>", "return", "Ok", "Some", "[")
        ):
            out[("ctor", f"{tok}{{}}")].append(line)
        elif kind in ("str", "rstr") and len(tok) >= MIN_SQL:
            out[("sql", tok[:SQL_PREFIX])].append(line)

    lits = [
        (i, t, ln)
        for i, (t, ln, kind) in enumerate(toks)
        if kind == "str" and value_literal(toks, i)
    ]
    for a_i, (ia, a, la) in enumerate(lits):
        for ib, b, lb in lits[a_i + 1 :]:
            if ib - ia > PAIR_TOKENS:
                break
            if a != b:
                out[("litpair", "+".join(sorted((a, b))))].append(min(la, lb))
    return out


def git(repo, *args):
    return subprocess.run(
        ["git", "-C", repo, *args], capture_output=True, text=True, check=True
    ).stdout


def is_test_path(path):
    return bool(
        re.search(r"(^|/)(tests?|benches|test_driver|integration_test|examples)/", path)
        or re.search(r"[-_]test\.(rs|dart)$", path)
    )


def strip_test_region(text, path):
    """Blank a Rust `#[cfg(test)] mod` tail so inline tests are not sites."""
    if not path.endswith(".rs"):
        return text
    m = re.search(r"^#\[cfg\(test\)\]\s*\n\s*mod ", text, re.M)
    if not m:
        return text
    return text[: m.start()] + "\n" * text[m.start() :].count("\n")


def ext_of(path):
    return next((e for e in EXTS if path.endswith(e)), None)


def parse_diff(repo, commit):
    """{key: {"old", "new", "hunks": [{"removed": [(ln, text)], "added": [(ln, text)],
    "new_start"}], "added": {new_line}}}"""
    diff = git(repo, "diff", "-U0", "--no-color", "-M", f"{commit}^", commit)
    files = {}
    cur = None
    old_path = None
    old_ln = new_ln = 0
    for line in diff.splitlines():
        if line.startswith("diff --git"):
            cur = None
        elif line.startswith("--- "):
            old_path = line[6:] if line.startswith("--- a/") else None
        elif line.startswith("+++ "):
            new_path = line[6:] if line.startswith("+++ b/") else None
            key = new_path or old_path
            cur = files.setdefault(
                key, {"old": old_path, "new": new_path, "hunks": [], "added": set()}
            )
        elif line.startswith("@@"):
            m = re.match(r"@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", line)
            old_ln, new_ln = int(m.group(1)), int(m.group(2))
            if cur is not None:
                cur["hunks"].append({"removed": [], "added": [], "new_start": new_ln})
        elif cur is None:
            continue
        elif line.startswith("-"):
            cur["hunks"][-1]["removed"].append((old_ln, line[1:]))
            old_ln += 1
        elif line.startswith("+"):
            cur["hunks"][-1]["added"].append((new_ln, line[1:]))
            cur["added"].add(new_ln)
            new_ln += 1
    return files


def src_files(repo, rev, exts):
    return [
        p
        for p in git(repo, "ls-tree", "-r", "--name-only", rev).splitlines()
        if ext_of(p) in exts and not is_test_path(p)
    ]


def file_features(repo, rev, path):
    try:
        text = git(repo, "show", f"{rev}:{path}")
    except subprocess.CalledProcessError:
        return {}
    return features(strip_test_region(text, path))


def token_stream(lines):
    """Token multiset, commas dropped: rustfmt adds trailing commas and
    reorders imports, neither of which is a rewrite."""
    return sorted(t for t, _, _ in tokenize("\n".join(t for _, t in lines)) if t != ",")


def callees(text):
    """Non-generic called names in a code fragment."""
    toks = tokenize(text)
    out = set()
    for i in range(len(toks)):
        c = callee_at(toks, i)
        if c and not is_generic(c[1], c[0]):
            out.add(c[0])
    return out


def analyse(repo, commit):
    diff = parse_diff(repo, commit)
    removed = defaultdict(set)  # feature -> {removed-from path:line}
    origin = defaultdict(list)  # feature -> [(new_path, new_start)]
    hunks_of = defaultdict(set)  # feature -> {hunk id}
    wraps = set()  # hunk ids that add a new non-generic call nearby
    exts = set()
    hid = 0
    for key, info in diff.items():
        path = info["old"] or key
        if not ext_of(path) or is_test_path(path):
            continue
        for hunk in info["hunks"]:
            rem = hunk["removed"]
            if not rem:
                continue
            hid += 1
            if not V3 and not hunk["added"]:
                continue  # pure deletion: deleted code is not a rewritten idiom
            if not V2 and token_stream(rem) == token_stream(hunk["added"]):
                continue  # formatting only
            text = "\n".join(t for _, t in rem)
            lo, hi = (
                hunk["new_start"] - NEAR,
                hunk["new_start"] + len(hunk["added"]) + NEAR,
            )
            near_added = "\n".join(
                t for h in info["hunks"] for ln, t in h["added"] if lo <= ln <= hi
            )
            if callees(near_added) - callees(text):
                wraps.add(hid)
            for feat, lines in features(text, rem[0][0]).items():
                removed[feat].update(f"{path}:{ln}" for ln in lines)
                origin[feat].append((info["new"], hunk["new_start"]))
                hunks_of[feat].add(hid)
            exts.add(ext_of(path))
    if not removed:
        return {"commit": commit, "removed_features": 0, "reported": []}

    parent = f"{commit}^"
    pre_df = defaultdict(int)
    for path in src_files(repo, parent, exts):
        for feat, lines in file_features(repo, parent, path).items():
            if feat in removed:
                pre_df[feat] += len(lines)
    pre_changed = defaultdict(int)
    post_changed = defaultdict(int)
    readded = defaultdict(list)  # feature -> [(path, line)] on diff-added lines
    for key, info in diff.items():
        for rev, path, counts in (
            (parent, info["old"], pre_changed),
            (commit, info["new"], post_changed),
        ):
            if path and ext_of(path) and not is_test_path(path):
                for feat, lines in file_features(repo, rev, path).items():
                    if feat in removed:
                        counts[feat] += len(lines)
                        if rev == commit:
                            readded[feat].extend(
                                (path, ln) for ln in lines if ln in info["added"]
                            )

    survivors = defaultdict(list)
    for path in src_files(repo, commit, exts):
        added = diff.get(path, {}).get("added", set())
        for feat, lines in file_features(repo, commit, path).items():
            if feat in removed:
                survivors[feat].extend(
                    f"{path}:{ln}" for ln in lines if ln not in added
                )

    # v6: an argfld (callee + field) is only an idiom when the handling of that
    # callee's result was rewritten too (`from_str(&x.col).unwrap_or_default()`
    # -> typed/propagating parse). Otherwise it is just another caller of f.
    rewritten_heads = {
        f[1].split(".")[0] if not f[1].startswith(".") else "." + f[1].split(".")[1]
        for f in removed
        if f[0] == "chain" and pre_changed[f] > post_changed[f]
    }
    reported = []
    for feat, sites in survivors.items():
        if (
            not V5
            and feat[0] == "argfld"
            and feat[1].split("(")[0] not in rewritten_heads
        ):
            continue
        if not V3:
            sites = sorted(set(sites))
            if len(hunks_of[feat]) >= SWEEP_HUNKS:
                continue  # a sweep: its survivors are the canonical remainder
        if not sites or len(sites) > MAX_SURV or pre_df[feat] > MAX_DF:
            continue
        if pre_changed[feat] > post_changed[feat]:
            cls = "rewritten"
        elif V2 or (
            any(
                p == op and abs(ln - start) <= NEAR
                for p, ln in readded[feat]
                for op, start in origin[feat]
            )
            and (V4 or hunks_of[feat] & wraps)
        ):
            cls = "wrapped"
        else:
            continue  # moved
        reported.append(
            {
                "kind": feat[0],
                "feature": feat[1],
                "class": cls,
                "removed_at": sorted(removed[feat]),
                "pre_df": pre_df[feat],
                "survivors": sorted(sites),
                "hunks": sorted(hunks_of[feat]),
            }
        )
    if not V3:
        reported = group_litpairs(reported)
    reported.sort(key=lambda r: (r["class"] != "rewritten", len(r["survivors"])))
    return {"commit": commit, "removed_features": len(removed), "reported": reported}


def group_litpairs(reported):
    """One item per removed literal set: pairs removed from the same hunks are
    a single list, not N^2 findings."""
    out, groups = [], {}
    for r in reported:
        if r["kind"] != "litpair":
            out.append(r)
            continue
        key = tuple(r["hunks"])
        g = groups.get(key)
        if g is None:
            g = groups[key] = dict(r, kind="litset", lits=set())
            out.append(g)
        else:
            g["survivors"] = sorted(set(g["survivors"]) | set(r["survivors"]))
            g["removed_at"] = sorted(set(g["removed_at"]) | set(r["removed_at"]))
            if r["class"] == "rewritten":
                g["class"] = "rewritten"
        g["lits"].update(r["feature"].split("+"))
    for g in groups.values():
        g["feature"] = "{" + ", ".join(sorted(g.pop("lits"))) + "}"
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    res = analyse(args[0], args[1])
    if "--json" in sys.argv:
        print(json.dumps(res))
        return
    rep = res["reported"]
    nrw = sum(r["class"] == "rewritten" for r in rep)
    print(
        f"{res['commit']} removed_features={res['removed_features']} reported={len(rep)} (rewritten={nrw})"
    )
    for r in rep:
        surv = ", ".join(r["survivors"][:6]) + (" …" if len(r["survivors"]) > 6 else "")
        print(
            f"  {r['class']:<9} {r['kind']:<7} {r['feature'][:60]:<60} n={len(r['survivors'])}  {surv}"
        )
        if "--explain" in sys.argv:
            path, ln = r["removed_at"][0].rsplit(":", 1)
            old = git(args[0], "show", f"{args[1]}^:{path}").splitlines()
            print(f"      - {path}:{ln}: {old[int(ln) - 1].strip()[:110]}")
            for sv in r["survivors"][:3]:
                sp, sl = sv.rsplit(":", 1)
                new = git(args[0], "show", f"{args[1]}:{sp}").splitlines()
                print(f"      = {sv}: {new[int(sl) - 1].strip()[:110]}")


if __name__ == "__main__":
    main()
