# sutra software architecture

Decisions about how sutra is structured as software — distinct from what it
does (see `sutra-vision.md`) and what its terms mean (see `CONTEXT.md`).

Decided during Phase 3 brainstorm. These decisions apply across all Phase 5
capability PRDs.

## Deployment: monolith, library-first discipline

Single binary, everything in-process. Sutra is a developer tool running on
one machine against one workspace — the complexity of multi-process
coordination isn't justified.

The library (`src/lib.rs`) is the real product. Binaries (`sutra`, `sutra-guard`)
are thin shells. The library is structured with clean module boundaries and no
hidden shared state so that a future split (sidecar processes, analysis workers)
is straightforward if the need arises.

**Considered alternatives:**
- Core + sidecar (main binary for fast path, heavy analyses as separate
  processes). Rejected: IPC coordination cost, shared database complexity,
  all for a single-user tool.
- Multiple specialized binaries. Rejected: same coordination cost, plus
  deployment complexity.

## Storage: SQLite-only, ephemeral/durable partition

One SQLite file per workspace. No additional stores (no vector DB, no
time-series DB). At the scale of one developer's codebase (thousands of
symbols, not millions), SQLite handles everything including vector scans
and timestamped health snapshots.

The database has a first-class partition between two kinds of data:

**Ephemeral** — recomputable from code. Symbols, edges, parse artifacts,
cached metrics. Can be blown away and rebuilt from a fresh parse. This is
the "index" part of sutra.

**Durable** — human intent that must survive re-indexing. Constraints,
boundaries, convention promotions, waivers, aliases, component names,
lifecycle states. Losing this data loses the human's architectural
decisions.

This partition drives:
- **Recovery:** `sutra reindex` rebuilds ephemeral data without touching
  durable data.
- **Migration:** schema changes to ephemeral tables can drop and recreate.
  Durable tables require proper migrations.
- **Backup:** durable data is the valuable part. Ephemeral data is
  disposable.

**Considered alternatives:**
- SQLite + vector store (e.g., for HRR similarity). Rejected: at
  single-codebase scale, a linear scan over HRR vectors in SQLite is fast
  enough. Not worth the operational complexity.
- SQLite + time-series store for health trends. Rejected: timestamped rows
  in SQLite are fine for this scale.

## Language adapters: layered traits

Language support is provided through a layered trait system:

**Required — core parse trait:** Given a file's bytes and tree-sitter tree,
produce symbols and edges. This is the minimum for a language adapter. A new
language that implements only this trait immediately works for Layer 0
(structural facts) and everything built on top of it.

**Optional — extension traits:** Higher-layer concerns are opt-in traits
that a language adapter may implement:

- `FcaAttributeSource` — enrich the symbol-attribute matrix with
  language-specific attributes (e.g., Rust's `async`, `unsafe`, visibility)
- `VerificationToolProvider` — declare which verification tools apply and
  how to invoke them
- (Future traits added as capability PRDs define their needs)

Each adapter declares capability levels by which traits it implements. The
core queries capabilities at runtime and adapts gracefully — a language with
no `FcaAttributeSource` simply produces fewer convention attributes, not
wrong ones.

**Extraction as a deliberate step:** The current parser modules
(`parser/rust.rs`, `parser/dart.rs`) mix parsing with ad-hoc
language-specific logic. Extracting the clean trait interface is worth doing
as a deliberate structural change before capability PRDs that depend on the
adapter boundary.

**Considered alternatives:**
- Narrow adapter (parsing only, everything else generic). Rejected: loses
  the ability to leverage language-specific richness (Rust's type system,
  trait impls, visibility) for higher-layer analysis.
- Wide adapter (one large trait with everything). Rejected: forces every
  language to stub out methods it can't support. Growing surface as
  analyses are added.

## API surface: library-first, thin adapters

The Rust library API (structs, methods, proper types) is the canonical
interface. Every consumer surface is a thin translation layer:

| Surface | Role | Translation |
|---|---|---|
| MCP tools (`mcp.rs`) | Agent interaction | Flatten library types to JSON for tool responses |
| HTTP/REST (`rest.rs`) | UI, external tools | Structure library types as REST resources |
| CLI | Human terminal use | Format library types for display |
| Guard binary (`bin/guard.rs`) | Git hook | Call library directly, exit code + minimal output |

Each surface is ~50 lines of translation per operation. When a new
capability is added, it's implemented once in the library and exposed
through whichever surfaces make sense.

The guard binary links the library directly — no HTTP overhead. This keeps
the hook fast.

**Considered alternatives:**
- MCP-first (everything wraps MCP). Rejected: MCP's flat tool-call model
  is a poor fit for richer interactions (streaming, navigation).
- HTTP-first (MCP wraps HTTP). Rejected: awkward for an in-process
  architecture. The guard would need an HTTP client to talk to itself.

## Evolution strategy: evolve in place

The existing codebase is well-aligned with the vision's architecture.
Each Phase 5 capability PRD reshapes its area incrementally. No big
rewrite.

**What's solid and stays:** `parser/` (tree-sitter extraction), `db/`
(SQLite storage), `fca/` (convention detection), `dd/` (differential
dataflow constraints), `freshness.rs` (incrementality), `guard.rs` (hook
enforcement), `tools/` (analysis tools).

**What evolves as capability PRDs land:** `pipeline.rs` (high complexity,
needs refactoring as coordination logic grows), `mcp.rs` (stays thin but
grows tool count), `workspace.rs` (becomes the main coordination point).

**What's new:** `components/` (discovery, identity, lifecycle),
`vocabulary/` (aliases, concept mapping), `similarity/` (HRR vectors),
`health/` (metrics, trends), `verification/` (tool orchestration).

**One exception:** Adapter trait extraction from `parser/rust.rs` and
`parser/dart.rs` is a deliberate structural step, not piecemeal. Do this
before capability PRDs that depend on the adapter interface.

## Module naming: by concern

Modules are named for what they do, not which vision layer they correspond
to or which computational substrate they use.

| Concern | Current | Target |
|---|---|---|
| Structural facts (parsing) | `parser/` | `parser/` (stays) |
| Architecture (components) | — | `components/` |
| Conventions (FCA) | `fca/` | `conventions/` |
| Constraints (DD) | `dd/` | `constraints/` |
| Health (metrics) | — | `health/` |
| Vocabulary (aliases) | — | `vocabulary/` |
| Similarity (HRR) | — | `similarity/` |
| Verification | — | `verification/` |

Renames happen as each area is built out, not all at once.

## Deferred

**Concurrency model for heavy computations.** DD already runs on dedicated
timely threads. Other substrates (FCA, HRR, clustering) will need their
own threading story. Each capability PRD handles this for its substrate —
no blanket decision needed. The standard Rust toolkit (tokio, spawn_blocking,
dedicated threads) is sufficient.
