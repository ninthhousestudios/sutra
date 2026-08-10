use serde_json::{Value, json};

use crate::error::Result;

struct Topic {
    name: &'static str,
    summary: &'static str,
    content: &'static str,
}

const TOPICS: &[Topic] = &[
    Topic {
        name: "quickstart",
        summary: "Get started with sutra in 60 seconds",
        content: "\
# Quickstart

1. Register your workspace:
   ```
   sutra_workspace(path=\"/absolute/path/to/project\")
   ```

2. Browse the codebase:
   ```
   sutra_map(workspace=\"myproject\", limit=20)
   ```
   Returns files ranked by importance (symbol count + fan-in + blast radius).

3. Find a symbol:
   ```
   sutra_explore(workspace=\"myproject\", query=\"handle_request\")
   ```

4. Read its source:
   ```
   sutra_read(workspace=\"myproject\", symbol=\"handle_request\")
   ```

5. Check impact before editing:
   ```
   sutra_impact(workspace=\"myproject\", symbol=\"handle_request\")
   ```

6. After editing, reparse:
   ```
   sutra_workspace(path=\"/absolute/path/to/project\", action=\"reparse\")
   ```",
    },
    Topic {
        name: "workspaces",
        summary: "Register, manage, and switch between workspaces",
        content: "\
# Workspaces

A workspace is a project root directory. Sutra indexes each workspace independently.

## Register a workspace
```
sutra_workspace(path=\"/home/user/project\")
```
This registers the workspace (deriving an ID from the directory name) and returns \
file/symbol counts and parse freshness.

## Force reparse
```
sutra_workspace(path=\"/home/user/project\", action=\"reparse\")
```
Re-registers and triggers a synchronous reparse. Use after major changes (branch switch, rebase).

## Check health
```
sutra_health()
```
Returns per-workspace file counts, symbol counts, parse errors, and staleness.",
    },
    Topic {
        name: "review",
        summary: "Review diffs with structural analysis and risk scoring",
        content: "\
# Review

`sutra_review` is a structural review compositor. It diffs your current branch, \
identifies changed symbols, computes transitive impact, and produces a risk score \
with ranked recommended reads.

## Review current branch
```
sutra_review(workspace=\"myproject\")
```
Diffs against main merge-base. Returns:
- Changed files and symbols
- Transitive impact (who calls what you changed)
- Risk score (0.0–1.0) with per-signal breakdown (blast radius, complexity, churn, conventions)
- Recommended reads ranked by review priority

## Review staged changes only
```
sutra_review(workspace=\"myproject\", diff=\"staged\")
```

## Review unstaged changes
```
sutra_review(workspace=\"myproject\", diff=\"unstaged\")
```

## Interpret the risk score
The score combines weighted signals:
- **Blast radius** — how many symbols are transitively affected
- **Complexity** — cognitive complexity of changed code
- **Hotspot churn** — how often changed files have been modified recently
- **Convention violations** — naming or structural patterns broken

A score above 0.5 warrants careful review. Above 0.7 is high-risk.

## Complement with PR risk
```
sutra_pr_risk(workspace=\"myproject\")
```
Similar composite score but includes volume signals and per-symbol risk breakdown.",
    },
    Topic {
        name: "query",
        summary: "Find symbols, search code, and navigate dependencies",
        content: "\
# Query

## Find a symbol by name
```
sutra_explore(workspace=\"myproject\", query=\"Config\")
```
Resolves aliases, qualified names, and fuzzy queries. Returns ranked matches with fetch instructions.

## Search by pattern
```
sutra_grep(workspace=\"myproject\", pattern=\"handle\", kind=\"function\")
```
FTS5-backed search across symbol names, signatures, and docstrings. \
Optional `kind` filter (function, struct, method, trait, enum, etc.).

## File dependencies
```
sutra_deps(workspace=\"myproject\", path=\"src/main.rs\", depth=2)
```
BFS from a file showing its import graph. Omit `path` for all edges.

## Multi-axis composite query
```
sutra_winnow(workspace=\"myproject\", kind=\"function\", min_complexity=10, rank_by=\"complexity\", limit=10)
```
AND-intersects filters (kind, min_complexity, min_churn, calls_to, file_glob, name_regex) \
and ranks results. Great for finding complex hotspots or functions matching multiple criteria.",
    },
    Topic {
        name: "freshness",
        summary: "Understand and fix stale results",
        content: "\
# Freshness

Every sutra response includes `as_of` (last parse timestamp) and `is_stale` \
(whether files have changed since).

## Why is my result stale?
Results go stale when files on disk have been modified after the last parse. \
The `is_stale` flag warns you that symbol data may not reflect current code.

## Fix staleness
```
sutra_workspace(path=\"/home/user/project\", action=\"reparse\")
```
Triggers a synchronous reparse. After it completes, subsequent queries return fresh data.

## Check workspace freshness
```
sutra_workspace(path=\"/home/user/project\")
```
Returns the last parse time and whether the workspace is stale, \
plus file/symbol counts and parse errors.",
    },
    Topic {
        name: "troubleshooting",
        summary: "Common issues and how to fix them",
        content: "\
# Troubleshooting

## \"workspace not found\"
The workspace hasn't been registered yet. Register it:
```
sutra_workspace(path=\"/absolute/path/to/project\")
```

## \"analysis tier required\"
Some tools (sutra_refs, sutra_calls, sutra_review, etc.) require the analysis tier. \
Enable it:
```
sutra_workspace(path=\"/absolute/path/to/project\", enable=[\"analysis\"])
```

## Stale results
Files changed since last parse. Reparse:
```
sutra_workspace(path=\"/absolute/path/to/project\", action=\"reparse\")
```

## Empty results from sutra_map or sutra_explore
The workspace may not have been parsed yet, or the language isn't indexed. \
Check status:
```
sutra_workspace(path=\"/absolute/path/to/project\")
```
By default sutra indexes Rust and Dart. Pass `languages=[\"rust\", \"python\"]` \
to `sutra_workspace` to index other languages.

## Parse errors
```
sutra_health()
```
Shows per-workspace parse error counts. If errors are high, check that source \
files are syntactically valid.",
    },
    Topic {
        name: "recipes",
        summary: "Step-by-step workflows for common tasks",
        content: "\
# Recipes

## Review my current diff
```
sutra_workspace(path=\"/home/user/project\", enable=[\"analysis\"])
sutra_review(workspace=\"myproject\")
```
Returns risk score, changed symbols, transitive impact, convention violations, \
and recommended reads sorted by review priority.

## Find callers and affected tests for a function
```
sutra_workspace(path=\"/home/user/project\", enable=[\"analysis\"])
sutra_calls(workspace=\"myproject\", symbol=\"handle_request\", direction=\"callers\", depth=2)
sutra_refs(workspace=\"myproject\", symbol=\"handle_request\", context_kind=\"call\")
sutra_winnow(workspace=\"myproject\", calls_to=\"handle_request\", file_glob=\"tests/**\")
```
`sutra_calls` shows the call hierarchy. `sutra_refs` shows all usage sites. \
`sutra_winnow` with `calls_to` + `file_glob` finds test files that exercise the function.

## Explain why a result is stale
```
sutra_workspace(path=\"/absolute/path/to/project\")
```
Check `is_stale` and `last_parse`. If stale, files changed after the last parse. Fix with:
```
sutra_workspace(path=\"/absolute/path/to/project\", action=\"reparse\")
```

## Check whether a change violates local conventions
```
sutra_workspace(path=\"/home/user/project\", enable=[\"analysis\"])
sutra_review(workspace=\"myproject\", diff=\"staged\")
```
Look at the `constraint_violations` section — DD-constraint dependency directions \
checked against your staged changes.

## Review a branch commit-by-commit
```
sutra_workspace(path=\"/home/user/project\", enable=[\"analysis\"])
sutra_commit_manifest(workspace=\"myproject\")
```
Returns per-commit entries with changed files and symbol-level change classifications \
(added/deleted/signature_changed/body_changed). Useful for understanding why a branch \
was split into separate commits. Pass `base` and `head` for a custom range.

## Trace a path between two symbols
```
sutra_workspace(path=\"/home/user/project\", enable=[\"analysis\"])
sutra_trace(workspace=\"myproject\", symbol=\"target_function\", direction=\"forward\")
```
`direction=forward` traces from entry points to the symbol. \
`direction=backward` traces from the symbol to leaf functions. \
Use this to understand how control flows through the codebase.",
    },
];

pub fn handle(topic: Option<&str>) -> Result<Value> {
    match topic {
        None => {
            let topics: Vec<Value> = TOPICS
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "summary": t.summary,
                    })
                })
                .collect();
            Ok(json!({ "topics": topics }))
        }
        Some(name) => {
            let topic = TOPICS.iter().find(|t| t.name == name);
            match topic {
                Some(t) => Ok(json!({
                    "topic": t.name,
                    "content": t.content,
                })),
                None => {
                    let available: Vec<&str> = TOPICS.iter().map(|t| t.name).collect();
                    Ok(json!({
                        "error": format!("unknown topic: {name}"),
                        "available_topics": available,
                    }))
                }
            }
        }
    }
}
