# PRD: core model redesign

Phase 5a capability PRD. Redesigns sutra's Layer 0 (structural facts),
storage partitioning, and language adapter interface to support multi-language
analysis and prepare the foundation for all higher layers.

Brainstorm decisions: sutra/29. Architecture context: `sutra-architecture.md`,
`sutra-vision.md`.

## Problem statement

Sutra's core model was built for two languages (Rust, Dart) with a direct
dispatch approach. As sutra grows toward the full vision -- conventions,
constraints, components, health, verification -- the core model needs to
support:

1. **Adding languages without touching core code.** Currently `parse_file()`
   is a match statement. Adding Python or C means editing the dispatcher and
   knowing the internal module structure.

2. **Language-specific richness without language-specific coupling.** FCA
   attribute extraction string-matches against Rust signatures (`"Result"`,
   `"&self"`). This is brittle for Rust and impossible for other languages.
   Higher layers need structured, language-specific data without knowing which
   language they're consuming.

3. **Ephemeral vs durable data.** Everything is in one undifferentiated set of
   tables. When higher layers add human-authored constraints, convention
   lifecycle states, and component boundaries, the system needs to know what
   can be blown away on reindex and what must be preserved and migrated.

4. **Tree-sitter lifecycle management.** Each parser module independently
   creates tree-sitter parsers. There is no shared infrastructure for parser
   pooling, timeout enforcement, or memory management.

## Solution

Introduce a trait-based language adapter system with core-owned tree-sitter
infrastructure, restructure the parse output as a tree, add structured
language-specific attributes, and establish the ephemeral/durable storage
partition.

The redesign is incremental -- five sequential PRs, each independently
testable and mergeable, no big rewrite.

## User stories

1. As a sutra contributor adding Python support, I want to implement a single
   trait and register my adapter, so that Python files are parsed without
   modifying core dispatch logic.

2. As the FCA convention engine, I want to read structured language attributes
   from a typed interface, so that convention detection works correctly across
   languages without string-matching signatures.

3. As a sutra user running `sutra reindex`, I want ephemeral data rebuilt from
   scratch while my convention overrides, waivers, and component boundaries
   are preserved, so that reindexing is safe and fast.

4. As the pipeline, I want to receive parse results as a tree of symbols with
   explicit parent-child relationships, so that containment is structural
   rather than reconstructed from name strings at insert time.

5. As a tool author querying symbols, I want a `language_attrs` field with
   structured data (is_async, is_unsafe, returns_result), so that I can build
   language-aware analysis without parsing display strings.

6. As the guard binary, I want tree-sitter parsing to have enforced timeouts,
   so that pathological files cannot hang the git hook.

7. As a sutra contributor, I want the adapter trait to declare its
   capabilities (FCA attribute enrichment, verification tools), so that
   higher layers degrade gracefully when a language adapter does not support
   a capability.

8. As the FCA engine for Rust, I want to extract attributes like
   `returns_result`, `is_async`, `is_unsafe` from structured data populated
   at parse time, so that attribute extraction is reliable and not dependent
   on signature string content.

9. As a sutra contributor, I want a clear convention for which DB tables are
   ephemeral (droppable on reindex) vs durable (require proper migration), so
   that I add new tables correctly.

10. As the convention system, I want detected conventions stored separately
    from human overrides (lifecycle state, suppression), so that reindexing
    rebuilds detections without losing human decisions.

11. As a sutra user backing up my workspace, I want to know that durable
    tables contain all human intent, so that backing up one DB file preserves
    my architectural decisions.

12. As the pipeline, I want tree-sitter parsers pooled and reused across files
    of the same language, so that full-workspace parses are efficient.

13. As a language adapter, I want to receive a `ParseContext` with the
    already-parsed tree-sitter tree, so that I focus on extraction logic
    without managing parser lifecycle.

14. As the pipeline inserting parse results, I want a well-defined
    tree-to-flat conversion, so that tree-structured parse output maps cleanly
    to the existing flat symbols table with `parent_symbol_id`.

## Implementation decisions

### Language adapter trait system

- A `LanguageAdapter` trait is the core abstraction. Required methods:
  `language_id()`, `grammar()` (returns tree-sitter language), `parse()`
  (receives `ParseContext`, returns `ParseResult`).
- Optional capabilities via default methods returning `None`:
  `as_fca_source() -> Option<&dyn FcaAttributeSource>`. Future extension
  traits added as capability PRDs need them.
- A `LanguageRegistry` holds registered adapters and dispatches by file
  extension. Built at startup, passed through the pipeline.
- Existing `rust::parse()` and `dart::parse()` wrap into `RustAdapter` and
  `DartAdapter` struct implementations. No behavior change in the first PR.

### Core-owned tree-sitter infrastructure

- Core owns tree-sitter parser creation, pooling, and lifecycle. One parser
  per language, reused across files.
- Timeout enforcement on parsing (configurable, with a sensible default).
- `ParseContext` struct: `source: &[u8]`, `tree: &tree_sitter::Tree`,
  `file_path: &str`. Adapters receive this; they do not create parsers.
- Core calls `adapter.grammar()`, creates/pools the parser, parses the
  source, passes `ParseContext` to `adapter.parse()`.

### Tree-structured parse result

- `ExtractedSymbol` gains a `children: Vec<ExtractedSymbol>` field.
- `parent_qualified_name: Option<String>` is removed from `ExtractedSymbol`.
- Parent-child relationships are explicit in the parse output.
- The pipeline flattens the tree for DB insertion, assigning
  `parent_symbol_id` during the walk. This is a well-defined conversion in
  one place rather than ad-hoc reconstruction.

### Symbol schema evolution

- `SymbolKind` stays as a flat enum. New variants added as languages are
  added. The DB stores kinds as strings; the enum is a Rust-side convenience
  with exhaustive match checking.
- New `language_attrs TEXT` column on the symbols table. JSON blob populated
  by the language adapter during parsing. Rust: `{"is_async": true,
  "is_unsafe": false, "returns_result": true, ...}`. Dart: `{"is_abstract":
  true, "is_factory": false, ...}`.
- `signature` remains as human-display text. Not parsed downstream.
- `flags` field stays for cross-language booleans (is_test, is_generated).
  Language-specific attributes go in `language_attrs`.

### FCA attribute extraction split

- `FcaAttributeSource` trait: `extract_attributes(&self, sym: &SymbolRow,
  file_path: &str) -> Option<SymbolAttrs>`.
- Generic attribute extraction (complexity bucketing, naming convention
  detection, directory context) becomes a shared helper function that any
  adapter's `FcaAttributeSource` implementation can call.
- Rust-specific extraction (returns_result, is_async, takes_self_ref) moves
  into `impl FcaAttributeSource for RustAdapter`, reading from
  `language_attrs` JSON.
- FCA engine queries the adapter registry for `FcaAttributeSource`
  capability. Languages without it simply produce fewer attributes.

### Ephemeral/durable storage partition

- Single SQLite database. Ephemeral and durable tables coexist.
- A constant or metadata table declares which tables are ephemeral vs durable.
- Ephemeral tables (files, symbols, symbols_fts, refs, imports, snapshots,
  conventions): can be DROP+CREATE on reindex or schema change.
- Durable tables (convention_overrides, and future: component_boundaries,
  aliases, constraints, waivers): require proper ALTER migrations. Never
  dropped on reindex.
- **Key rule:** durable tables reference ephemeral data by name
  (qualified_name, file_path), never by integer PK. Integer PKs change on
  reindex.
- Orphaned durable references (name no longer exists after reindex) are
  detected by a reconciliation pass and surfaced as a report. Not
  auto-deleted.

### Durable companion pattern

- The existing `conventions` table is split:
  - `conventions` (ephemeral): id, antecedent, consequent, support,
    confidence, first_seen, last_seen. Rebuilt on each FCA run.
  - `convention_overrides` (durable): convention_id (TEXT, matches
    conventions.id), lifecycle_state (descriptive/preferred/deprecated/
    forbidden), override_reason, created_at, updated_at.
- This establishes the pattern for all future durable companion tables.

### Pipeline integration

- `parse_workspace` and `parse_changed_files` take a `&LanguageRegistry`
  parameter instead of hardcoding language dispatch.
- `parse_single_file` asks the registry for the right adapter by file
  extension.
- The pipeline handles tree-to-flat conversion when inserting symbols.

## Testing decisions

Tests should verify external behavior through module interfaces, not
implementation details. Prior art: `tests/workspace_test.rs` (integration),
`tests/db-test.rs` (DB operations), `tests/dd-test.rs` (DD engine),
`fca/attributes.rs` (unit tests within module).

### Modules to test

**LanguageAdapter trait + LanguageRegistry:**
- Registration: register adapter, look up by extension, look up by language
  ID. Unknown extension returns None.
- Dispatch: registry.parse() dispatches to correct adapter and returns
  correct ParseResult.
- Capability query: as_fca_source() returns Some for Rust, None for a
  minimal adapter.
- Test with a mock `TestAdapter` that implements the trait minimally.

**ParseContext + tree-sitter infrastructure:**
- Parser pooling: same grammar reuses parser across multiple parse calls.
- Timeout: parsing a pathological input respects the timeout (may need a
  crafted input or a test-only short timeout).
- ParseContext correctness: source bytes and tree are consistent.

**Tree-structured ParseResult:**
- Adapter produces tree with correct parent-child nesting (e.g., methods
  inside impl blocks, functions inside modules).
- Round-trip: tree output flattened for DB insertion, then queried back,
  matches the original structure.
- Edge case: top-level symbols have no parent. Deeply nested symbols
  (method inside impl inside module) maintain full chain.

**Concrete adapters (Rust, Dart):**
- Behavioral parity: existing test files produce the same symbols, refs,
  and imports as before the refactor. This is the regression safety net.
- language_attrs population: Rust adapter populates is_async, is_unsafe,
  returns_result correctly for known test inputs.
- Dart adapter populates its language_attrs correctly.

**FcaAttributeSource trait + extraction split:**
- Rust FCA source extracts the same attributes as the current
  extract_symbol_attrs for existing test cases (regression).
- Rust FCA source reads from language_attrs JSON, not from signature
  strings.
- Generic helper produces correct cross-language attributes (complexity
  bucket, naming, directory).
- Adapter without FcaAttributeSource: FCA engine handles gracefully,
  produces fewer attributes, no errors.

**DB schema evolution:**
- language_attrs column: insert symbol with language_attrs JSON, read it
  back, parse correctly.
- Partition metadata: ephemeral tables listed correctly, durable tables
  listed correctly.
- Reindex operation: drops ephemeral tables, preserves durable tables and
  their data.

**Durable companion pattern (conventions split):**
- Convention override survives reindex: create override, reindex, override
  still present.
- Orphan detection: create override for convention X, reindex without X,
  reconciliation reports the orphan.
- Convention with override: query returns merged view (detected data +
  lifecycle state).

**Pipeline integration:**
- Full parse with registry: parse a workspace, verify symbols in DB match
  expected output. Existing workspace_test patterns.
- Tree-to-flat conversion: parse result tree correctly maps to
  parent_symbol_id in DB.

## Out of scope

- **Module renames** (fca/ to conventions/, dd/ to constraints/). Deferred
  to each subsystem's capability PRD.
- **New language adapters** (Python, C). This PRD builds the interface; new
  adapters are separate work.
- **Convention lifecycle UI/UX.** The durable companion pattern provides
  storage; how users interact with lifecycle states is a convention system
  PRD concern.
- **Component boundaries, aliases, constraints, waivers.** These are future
  durable tables that follow the pattern established here but belong to
  their respective capability PRDs.
- **Concurrency model changes.** DD already has its threading story. Other
  substrates handle concurrency in their own PRDs.
- **FCA engine changes** beyond wiring up the new trait interface.
- **DD engine changes.**
- **MCP/REST/CLI surface changes** beyond what's needed to expose
  language_attrs in existing tool output.

## Further notes

### Evolution ordering

The five PRs must land in order due to dependencies:

1. **Adapter trait extraction** -- pure structural refactor, no behavior
   change. Unlocks everything else.
2. **FCA trait extraction** -- depends on adapter trait existing.
3. **language_attrs column** -- depends on adapter trait (adapters populate
   it). FCA source switches to reading from it.
4. **Storage partition annotation** -- independent of 2/3 but logically
   follows. Groundwork for durable tables.
5. **Durable companion pattern** -- depends on partition annotation. Splits
   conventions table.

### Risk areas

- **Tree-structured ParseResult** touches the most code paths. The adapters
  produce trees, the pipeline flattens them, and every tool that reads
  symbols sees the result. Regression testing on existing workspace parses
  is the primary safety net.
- **language_attrs JSON** adds a schemaless column. Each adapter defines its
  own JSON shape. The FcaAttributeSource trait is the typed interface that
  prevents consumers from reaching into raw JSON. Discipline required to
  keep it that way.
- **Behavioral parity** during adapter extraction. The refactor must not
  change parse output. Diff the symbol/ref/import counts before and after
  on real workspaces.
