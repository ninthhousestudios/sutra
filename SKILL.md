# sutra CLI — code intelligence queries

Binary: `~/.cargo/bin/sutra` (or just `sutra` on PATH).
All query subcommands output JSON to stdout.

## Subcommands

### map — file skeleton ranked by importance

```
sutra map <workspace> [--path-prefix <prefix>] [--limit <n>]
```

Returns `{files: [{path, line_count, symbols, importance, blast_radius, ...}], total}`.

### grep — FTS5 search across symbol names, signatures, docstrings

```
sutra grep <workspace> <pattern> [--kind <kind>] [--limit <n>]
```

Returns `{matches: [{short_name, qualified_name, file, kind, start_line, end_line, signature}], total}`.

### find — jump to symbol definition by name (exact then fuzzy)

```
sutra find <workspace> <name> [--kind <kind>] [--limit <n>]
```

Same output shape as grep.

### read — read a symbol's source code with line numbers

```
sutra read <workspace> <symbol> [--context-lines <n>]
```

Returns `{symbol, file, kind, start_line, end_line, content, signature}`.

### outline — file symbol table of contents

```
sutra outline <workspace> <path>
```

Returns `{path, language, symbols: [{short_name, qualified_name, kind, start_line, end_line, signature}]}`.

### impact — blast radius analysis

```
sutra impact <workspace> <symbol>
```

Returns `{symbol, direct_callers, transitive_count, risk_level, ...}`. Call before editing load-bearing code.

## Workspace ID

The workspace id is whatever was registered via `sutra workspaces add` or `sutra_status`. For the manas monorepo the id is `manas`.

## When to use

Use these CLI subcommands instead of sutra MCP tools when available. They avoid MCP tool schema loading overhead. Fall back to MCP for tools not yet on the CLI (refs, calls, trace, diff-impact, pr-risk, etc.).

| Task | Command |
|------|---------|
| Find files | `sutra map <ws>` |
| Search symbols | `sutra grep <ws> <pattern>` or `sutra find <ws> <name>` |
| Read code | `sutra read <ws> <symbol>` |
| File TOC | `sutra outline <ws> <path>` |
| Pre-edit check | `sutra impact <ws> <symbol>` |
