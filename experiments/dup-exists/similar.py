#!/usr/bin/env python3
"""Rank of each back-test original under `sutra_similar` (sutra/484): the
copy is the query, at its introducing commit, in each mode.

  similar.py [--mode dup,embed,strip]

Reuses prod.py's scratch worktrees and isolated HOME, and talks to
`sutra serve --stdio` over MCP. The AC for sutra/484: the originals of
sutra/459 and 437 rank top-3 under the default mode.
"""

import csv
import json
import os
import subprocess
import sys

import prod


class Server:
    def __init__(self):
        self.p = subprocess.Popen(
            [prod.BIN, "serve", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=prod.ENV,
            text=True,
        )
        self.n = 0
        self.call_raw(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "similar.py", "version": "0"},
            },
        )
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, msg):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def call_raw(self, method, params):
        self.n += 1
        self.send({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("id") == self.n:
                if "error" in msg:
                    raise RuntimeError(msg["error"])
                return msg["result"]

    def tool(self, name, args):
        res = self.call_raw("tools/call", {"name": name, "arguments": args})
        text = res["content"][0]["text"]
        if res.get("isError"):
            raise RuntimeError(text)
        return json.loads(text)

    def close(self):
        self.p.stdin.close()
        self.p.wait()


def rank(matches, orig_fn, orig_file):
    for i, m in enumerate(matches, 1):
        if prod.names_match(m["symbol"], orig_fn) and (
            not orig_file or m["file"] == orig_file
        ):
            return i, m
    return None, None


def main():
    a = sys.argv[1:]
    modes = (a[a.index("--mode") + 1] if "--mode" in a else "dup,embed,strip").split(
        ","
    )
    with open(os.path.join(prod.HERE, "cases.tsv")) as f:
        rows = [
            r
            for r in csv.DictReader(f, delimiter="\t")
            if r["intro_commit"] not in ("UNFOUND", "-")
            and r["new_fn"] not in ("", "-")
            and r["orig_fn"]
            and os.path.isdir(r["repo_path"])
        ]
    rows.sort(key=lambda r: r["repo_path"])
    print(f"{'case':16s} " + " ".join(f"{m:>18s}" for m in modes))
    for r in rows:
        ws, _ = prod.checkout(r["repo_path"], r["intro_commit"])
        server = Server()
        cells = []
        for mode in modes:
            try:
                res = server.tool(
                    "sutra_similar",
                    {
                        "workspace": ws,
                        "symbol": r["new_fn"],
                        "mode": mode,
                        "limit": 5000,
                        "threshold": -1.0,
                    },
                )
            except RuntimeError as e:
                cells.append(f"error {str(e)[:40]}")
                continue
            if "diagnostic" in res:
                cells.append(
                    str(res["diagnostic"])[:18]
                    if isinstance(res["diagnostic"], str)
                    else "ambiguous"
                )
                continue
            i, m = rank(res["matches"], r["orig_fn"], r["orig_file"])
            if i is None:
                cells.append(f"absent/{len(res['matches'])}")
            else:
                mark = "*" if m.get("likely_duplicate") else ""
                cells.append(f"{i}/{len(res['matches'])} ({m['similarity']:.2f}){mark}")
        server.close()
        print(f"{r['case']:16s} " + " ".join(f"{c:>18s}" for c in cells), flush=True)


if __name__ == "__main__":
    main()
