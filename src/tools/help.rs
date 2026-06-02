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
   sutra_status(path=\"/absolute/path/to/project\")
   ```

2. Browse the codebase:
   ```
   sutra_map(workspace=\"myproject\", limit=20)
   ```
   Returns files ranked by importance (symbol count + fan-in + blast radius).

3. Find a symbol:
   ```
   sutra_find(workspace=\"myproject\", name=\"handle_request\")
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
   sutra_parse(workspace=\"myproject\")
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
sutra_status(path=\"/home/user/project\")
```
This registers the workspace (deriving an ID from the directory name) and returns \
file/symbol counts and parse freshness.

## Force reparse
```
sutra_add_root(path=\"/home/user/project\")
```
Re-registers and triggers a fresh parse. Use after major changes (branch switch, rebase).

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
sutra_find(workspace=\"myproject\", name=\"Config\")
```
Three-tier search: exact short name → exact qualified name → FTS5 fuzzy.

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
sutra_parse(workspace=\"myproject\")
```
Triggers a reparse. After it completes, subsequent queries return fresh data.

## Check workspace freshness
```
sutra_status(path=\"/home/user/project\")
```
Returns the last parse time and whether the workspace is stale.

Returns per-workspace file counts, symbol counts, parse errors, and staleness.",
    },
    Topic {
        name: "conventions",
        summary: "Detect attribute-implication and structural pattern violations",
        content: "\
# Conventions

`sutra_review` includes convention violation detection powered by Formal Concept \
Analysis (FCA). It extracts attribute implications from your codebase and flags \
violations in changed code.

## Check conventions in a diff
```
sutra_review(workspace=\"myproject\")
```
The `convention_violations` section lists symbols that violate learned implications, \
showing which attributes were expected but missing.

## What it detects
- **Attribute implications** — rules like 'pub functions in this module tend to have \
  docs' or 'async functions here return Result'. FCA infers these from symbol \
  attributes (name prefixes/suffixes, kind, visibility, module, doc presence).
- **Structural patterns** — DD-constraint violations where dependencies between \
  changed symbols break established dependency directions

## How it works
FCA extracts approximate implications from a formal context of symbol attributes. \
Each implication has the form 'if a symbol has attributes A, it should also have \
attributes B' (with support and confidence thresholds). When a changed symbol \
satisfies the antecedent but lacks the consequent attributes, it's flagged.",
    },
    Topic {
        name: "troubleshooting",
        summary: "Common issues and how to fix them",
        content: "\
# Troubleshooting

## \"workspace not found\"
The workspace hasn't been registered yet. Register it:
```
sutra_status(path=\"/absolute/path/to/project\")
```

## \"analysis tier required\"
Some tools (sutra_refs, sutra_calls, sutra_review, etc.) require the analysis tier. \
Enable it:
```
sutra_tools(enable=\"analysis\")
```

## Stale results
Files changed since last parse. Reparse:
```
sutra_parse(workspace=\"myproject\")
```

## Empty results from sutra_map or sutra_find
The workspace may not have been parsed yet, or the language isn't indexed. \
Check status:
```
sutra_status(path=\"/absolute/path/to/project\")
```
By default sutra indexes Rust and Dart. Pass `languages=[\"rust\", \"python\"]` \
to `sutra_status` to index other languages.

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
sutra_tools(enable=\"analysis\")
sutra_review(workspace=\"myproject\")
```
Returns risk score, changed symbols, transitive impact, convention violations, \
and recommended reads sorted by review priority.

## Find callers and affected tests for a function
```
sutra_tools(enable=\"analysis\")
sutra_calls(workspace=\"myproject\", symbol=\"handle_request\", direction=\"callers\", depth=2)
sutra_refs(workspace=\"myproject\", symbol=\"handle_request\", context_kind=\"call\")
sutra_winnow(workspace=\"myproject\", calls_to=\"handle_request\", file_glob=\"tests/**\")
```
`sutra_calls` shows the call hierarchy. `sutra_refs` shows all usage sites. \
`sutra_winnow` with `calls_to` + `file_glob` finds test files that exercise the function.

## Explain why a result is stale
```
sutra_status(path=\"/absolute/path/to/project\")
```
Check `is_stale` and `last_parse`. If stale, files changed after the last parse. Fix with:
```
sutra_parse(workspace=\"myproject\")
```

## Check whether a change violates local conventions
```
sutra_tools(enable=\"analysis\")
sutra_review(workspace=\"myproject\", diff=\"staged\")
```
Look at the `convention_violations` and `constraint_violations` sections. \
FCA-derived attribute implications and DD-constraint dependency directions are checked \
against your staged changes.

## Trace a path between two symbols
```
sutra_tools(enable=\"analysis\")
sutra_trace(workspace=\"myproject\", symbol=\"target_function\", direction=\"forward\")
```
`direction=forward` traces from entry points to the symbol. \
`direction=backward` traces from the symbol to leaf functions. \
Use this to understand how control flows through the codebase.",
    },
    Topic {
        name: "orient",
        summary: "Get convention-aware orientation for a component",
        content: "\
## Orient to a component's conventions
```
sutra_orient(workspace=\"myproject\", scope=\"conventions\")
```
Returns preferred conventions with signature templates, anti-pattern warnings, \
drift alerts, waivers, and pending proposals for the target scope.

Scope can be a component name, component ID, or a file path. \
When given a file path, the tool resolves it to the owning component.

Use this before writing new code in a component to understand \
what patterns to follow and what to avoid.",
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
