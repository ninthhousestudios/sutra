# Adding language support to sutra

Sketch for adding C and Python adapters. Covers what exists, what each
language needs, and where the hard parts are.

## Current adapter interface

Every language implements the `LanguageAdapter` trait
(`src/parser/adapter.rs`):

```
language_id()           → &str              // "rust", "dart"
extensions()            → &[&str]           // &[".rs"], &[".dart"]
grammar()               → tree_sitter::Language
parse(ctx)              → ParseResult       // symbols, refs, imports
module_boundary_hints() → ModuleBoundaryStrength
```

`parse()` returns `ParseResult` containing:
- `Vec<ExtractedSymbol>` — definitions with kind, qualified name, visibility,
  signature, docstring, complexity metrics, language_attrs JSON, flags
- `Vec<ExtractedRef>` — identifier references with context_kind classification
- `Vec<ExtractedImport>` — import edges (raw path strings, resolved later)

**Variable/constant extraction:** Top-level and class-level variable/constant
declarations must be indexed. Map immutable bindings (`const`, `final`,
`readonly`) to `SymbolKind::Const` and mutable bindings to
`SymbolKind::Static`. Without this, `sutra_lookup` can't find module-level
configuration, constants, or global state.

Everything above Layer 0 — constraints (DD), similarity
(HRR), components, review — is language-agnostic.
A new language needs three things: a parser module, an adapter registration
in `default_registry()`, and an **import resolver** (see below).

### Required: import resolver module

The parser emits `Vec<ExtractedImport>` with raw path strings. These land
in the `imports` table with `resolved_file_id = NULL`. Without a resolver
that populates `resolved_file_id`, the DD constraint engine sees zero
import edges — every `forbidden_dep` and `boundary` constraint is dormant,
`sutra_deps` returns nothing, and PageRank has no import graph.

Each language needs a resolver module wired into `post_parse_sequence`
(`src/pipeline.rs`):

| Language | Module | Key logic |
|---|---|---|
| Rust | `src/rust_imports.rs` | `crate::`, `super::`, `self::` + workspace layout |
| Dart | `src/dart_packages.rs` | `package:` URIs via pubspec + relative `.dart` paths |
| C | `src/c_imports.rs` | Quoted includes (relative → root → `-I` paths), `<...>` left unresolved |

The pattern:
1. Add `Db::unresolved_{lang}_imports()` — `SELECT ... WHERE resolved_file_id IS NULL AND language = '{lang}'`
2. Create `src/{lang}_imports.rs` with `resolve_{lang}_imports(db, workspace_root) -> Result<usize>`
3. Build `path_to_id` / `id_to_path` lookups from `db.all_files()`
4. Resolve each import using language-specific rules, collect `(import_id, target_file_id)` pairs
5. Call `db.batch_update_import_resolved_file_ids(&updates)`
6. Wire into `post_parse_sequence` after the existing resolvers
7. Add `pub mod {lang}_imports` to `src/lib.rs`

This was missed for both Rust and Dart initially (sutra/119) — discovered
when `sutra_deps` returned zero edges despite imports being parsed
correctly. The parser alone is not enough; the resolver is load-bearing.

### Complexity metrics

`src/parser/complexity.rs` computes cyclomatic, cognitive, and max nesting
depth from tree-sitter AST nodes. It dispatches on a `lang: &str` parameter.
Adding a language requires adding branches to `classify_cognitive` and
`walk_cyclomatic` for that language's control flow node kinds.

## C

### Parser (src/parser/c.rs)

**Grammar:** `tree-sitter-c` — mature, well-maintained.

**Symbol kinds:**

| tree-sitter node | SymbolKind | Notes |
|---|---|---|
| function_definition | Function | |
| declaration (function pointer typedef) | Function | Heuristic needed |
| struct_specifier (with body) | Struct | |
| enum_specifier (with body) | Enum | |
| type_definition | TypeAlias | `typedef` |
| preproc_function_def | Macro | `#define FOO(x)` |
| preproc_def | Const | `#define FOO 42` |
| declaration (global variable) | Const | Top-level non-function decls |

**Qualified names:** C has no namespaces. Use `short_name` directly. For
static functions in different files, the file path disambiguates (already
handled by sutra's `file_path::symbol_name` convention).

**Visibility:** `static` → private, everything else → pub. Simpler than
Rust's `pub`/`pub(crate)`/private but the same field.

**References:** Identifier nodes that aren't definition names. Same walk
pattern as Rust/Dart. `classify_ref_context` needs C-specific parent node
mappings:
- `call_expression` → Call
- `field_expression` → FieldAccess
- `type_identifier` in parameter/return → TypeUse
- `struct_specifier` in initializer → Construction

**Imports — the hard part:**

`#include` directives map to import edges, but the resolution is
non-trivial:

- `#include "foo.h"` — relative to the including file. Sutra can resolve
  these directly (check sibling/parent directories). This covers most
  project-internal includes.
- `#include <stdio.h>` — system header search path. These are external
  dependencies; sutra should record them as unresolved external imports
  (similar to how Rust handles `use std::*` — the edge exists but points
  outside the workspace).
- `#include "path/to/foo.h"` — project-relative. Usually resolvable if
  sutra knows the project root, which it does.

**Implemented** (`src/c_imports.rs`): Resolve in order: relative to
includer → project root → `-I`/`-isystem` paths from
`compile_commands.json`. System includes (`<...>`) become unresolved
external edges. See sutra/185 for planned per-TU scoping of
compile_commands include paths.

**Signatures:** `return_type function_name(param_type param, ...)`. Extract
from the function_definition's declarator and type nodes.

**Docstrings:** `/* ... */` or `/** ... */` comments preceding definitions.
Same heuristic as Rust (walk preceding siblings for comment nodes).

**Flags:** No standard test framework annotation. Heuristics:
- Files matching `*_test.c`, `test_*.c`, `tests/*.c` → test file flag
- Functions named `test_*` in test files → test flag
- `__attribute__((constructor))`, `__attribute__((visibility("default")))` →
  FFI entry flag

**Complexity:** Add `"c"` branches to `classify_cognitive`:
- Flow breaks: `if_statement`, `for_statement`, `while_statement`,
  `do_statement`, `case_statement`, `goto_statement`
- Nesting: `if_statement`, `for_statement`, `while_statement`,
  `do_statement` (not `switch_statement`, matching Rust's `match` treatment)
- Logical operators: `&&`, `||` in binary_expression

### Module boundary strength

`Weak` — C has no module system. Files are compilation units with no
enforced boundaries. Header files create implicit interfaces but there's no
language-level encapsulation beyond `static`.

### Estimated effort

| Work | Days |
|---|---|
| Parser (symbols, refs, signatures, docstrings) | 2 |
| Import resolution (relative + project-root) | 1 |
| Complexity branches | 0.5 |
| Flags (test/FFI heuristics) | 0.5 |
| **Total** | **4** |

### Risks

- **Header-only libraries:** Projects that use headers extensively for
  inline code would have symbols extracted from `.h` files but the import
  graph might create duplicate edges. Need to decide: index `.h` files as
  peers, or only index `.c` files?
- **Preprocessor:** tree-sitter-c parses pre-preprocessor source. Macro
  expansions aren't visible. `#ifdef` blocks are parsed as nodes, not
  evaluated. This means some symbols exist conditionally — sutra would
  report them all unconditionally. This is probably fine (same as Rust
  with `#[cfg]` which sutra handles by flagging, not omitting).
- **Forward declarations:** A function declared in a header and defined in
  a `.c` file produces two nodes. Sutra should deduplicate by qualified
  name within a workspace, preferring the definition.

## Python

### Parser (src/parser/python.rs)

**Grammar:** `tree-sitter-python` — mature, well-maintained.

**Symbol kinds:**

| tree-sitter node | SymbolKind | Notes |
|---|---|---|
| function_definition | Function | Top-level |
| function_definition (inside class) | Method | Check parent |
| class_definition | Struct | Reuse Struct kind for classes |
| decorated_definition | (unwrap) | Extract inner function/class |
| global_statement / assignment | Const | Module-level assignments |

No enum kind in Python (enum.Enum is a class). Type aliases (`TypeAlias`
from Python 3.12, or `X = TypeVar(...)`) could be detected but aren't
critical.

**Qualified names:** `module.ClassName.method_name`. Python's nesting is
straightforward — class bodies and nested functions create scope. Same
`name_context` stack approach as Rust/Dart.

**Visibility:** `_name` → private, `__name` → private (name-mangled),
everything else → pub. Convention-based, like Dart.

**References — the fuzzy part:**

Python references are inherently less precise than Rust's:
- `foo.bar()` — without type info, we don't know what `foo` is. Sutra can
  still record `bar` as a reference and attempt name-based resolution
  (same as it does for Rust/Dart, just with lower confidence).
- `getattr(obj, "method")` — invisible to static analysis. Accept the gap.
- `*args`, `**kwargs` forwarding — calls through these are invisible.

**Proposed approach:** Same walk pattern as Rust/Dart. Record identifier
references, classify by parent context. Accept that call graph edges will
be noisier. The existing resolver already handles partial resolution
gracefully (unresolved refs are reported honestly).

`classify_ref_context` for Python:
- `call` node → Call
- `attribute` node → FieldAccess
- `type` annotation → TypeUse
- `argument_list` of class instantiation → Construction

**Imports:**

Python imports are well-structured in tree-sitter:
- `import foo` → `import_statement` with module name
- `from foo import bar` → `import_from_statement` with module + name
- `from . import bar` → relative import with level dots
- `from ..foo import bar` → relative import with depth

Resolution: relative imports resolve against the package root (look for
`__init__.py` to find package boundaries). Absolute imports of project
modules resolve by mapping dotted paths to file paths
(`foo.bar` → `foo/bar.py` or `foo/bar/__init__.py`). External packages
(anything not in the workspace) become unresolved external edges.

**Signatures:** `def name(param: Type, ...) -> ReturnType`. Python's
optional type hints map to signature strings naturally. Functions without
hints get a signature with just parameter names.

**Docstrings:** First expression statement in a function/class body, if
it's a string literal. Well-defined convention, easy to extract.

**Flags:**
- Functions named `test_*` → test flag
- Files matching `test_*.py`, `*_test.py`, `tests/*.py` → test file
- `@pytest.fixture` decorator → test infrastructure
- Classes inheriting `unittest.TestCase` → test class
- Functions with `@app.route` or similar framework decorators → entry point

**Complexity:** Add `"python"` branches to `classify_cognitive`:
- Flow breaks: `if_statement`, `for_statement`, `while_statement`,
  `try_statement`, `except_clause`, `with_statement`
- Nesting: `if_statement`, `for_statement`, `while_statement`,
  `try_statement`, `with_statement`
- Logical operators: `and`, `or` (keyword operators, not symbols)
- List/dict/set comprehensions: `list_comprehension`,
  `dictionary_comprehension`, `set_comprehension` — increment cognitive
  (they add mental load) but don't increment nesting

### Module boundary strength

`Weak` — Python has real modules but no visibility enforcement beyond the
`_` naming convention. Anything can import anything. The `__all__` list is
advisory.

### Estimated effort

| Work | Days |
|---|---|
| Parser (symbols, refs, signatures, docstrings) | 2.5 |
| Import resolution (relative + absolute project) | 1 |
| Complexity branches | 0.5 |
| Flags (pytest/unittest detection) | 0.5 |
| **Total** | **4.5** |

### Risks

- **Call graph noise:** Name-based reference resolution will produce false
  positives. `bar()` in one module matching `def bar()` in an unrelated
  module. The existing resolver's confidence/distance scoring helps, but
  Python will have more unresolved and misresolved references than Rust.
  Blast radius and impact numbers will be directionally correct but noisy.
- **Dynamic imports:** `importlib.import_module("foo")`, `__import__("foo")`
  are invisible. Accept the gap — these are uncommon in well-structured
  code.
- **Metaclasses and descriptors:** `__init_subclass__`, `__set_name__`,
  custom descriptors create implicit call edges. Accept the gap.
- **Monorepo package resolution:** Projects with multiple packages
  (namespace packages, src layouts) need the package root to be
  discoverable. Heuristic: look for `pyproject.toml`, `setup.py`,
  `setup.cfg` to find package boundaries. Could also accept a
  config hint in `.sutra/rules.toml`.

## C++

Design decisions settled in sutra/525; grammar measurements behind them are in
sutra/538 (leveldb, Hyprland, udis86, kala-reverse).

### Language id

`cpp`. `sutra workspaces add` also accepts `c++` and `cxx` as aliases.
`lessons.rs` already canonicalizes `cpp`/`c++` on both stored tags and
workspace languages, so lesson filtering is spelling-independent.

### Extensions

cpp owns `cc`, `cpp`, `cxx`, `c++`, `hpp`, `hh`, `hxx`, `h++`, `ipp`, `tpp`,
`inl`. C++20 module units (`cppm`, `ixx`) are not indexed. `.h` and `.c` are
shared with the C adapter; see the next section.

### Routing shared extensions (`.h`, `.c`)

An extension maps to an **ordered candidate list** of adapters:
`.h → [cpp, c]`, `.c → [c, cpp]`. The adapter is the first candidate the
workspace declares in `languages`:

| Workspace languages | `.h` | `.c` |
|---|---|---|
| `c` | c | c |
| `cpp` | cpp | cpp |
| `c`, `cpp` | cpp | c |

- Mixed workspaces send `.h` to cpp. On leveldb, the C++ grammar was never
  worse than C on any header, including pure-C API headers under
  `extern "C"`. The C grammar broke 39/40 class headers.
- A cpp-only workspace sends `.c` to cpp. Before this, such a workspace didn't
  index `.c` at all. This is also how a decompiled C++ corpus like
  kala-reverse opts into the C++ grammar: declare `languages = ["cpp"]`.
- **No fallback to C on parse errors.** On the sutra/538 corpora it would have
  rescued zero headers, while doubling parse cost on the 16–78% of files that
  macros push into error. It would also flip a file's grammar when an edit
  tips the error comparison, churning symbol ids.
- **No content sniffing.** A header would flip language when someone adds
  `class`.
- **No per-workspace extension → language override.** sutra/538 measured
  +0.35% function names on kala and 38 resolvable qualified callees. That
  isn't worth a permanent config surface, and the candidate-list rule above
  already covers the opt-in.

**Where resolution happens.** `LanguageRegistry::for_languages(&[String])`
builds a workspace-scoped registry. It resolves each shared extension once, at
construction, so `adapter_for_extension` keeps its signature.
`default_registry()` has no workspace context and keeps `.h → c`, so
behavior without a workspace is unchanged.

- The pipeline, freshness and the MCP tools build the scoped registry from
  `workspace.languages`.
- The guard builds it from `SELECT DISTINCT language FROM files`. This covers
  proposed files that aren't indexed yet.
- `any_language_is_test_path` unions every adapter's rules and never picks a
  grammar, so it is unaffected.

Changing a workspace's `languages` changes which grammar its `.h` files get,
and so changes their symbol ids. Freshness must treat a stored
`files.language` that differs from the resolved language as stale.

### Declarations vs definitions

- **In-class member declarations** → `Method`. These are the API surface and
  the only home of pure virtuals. A bodiless declaration carries
  `language_attrs.declaration_only: true`. Complexity, similarity and the
  shape diff skip it; otherwise every method would appear twice, once at
  complexity 0.
- **Out-of-line definitions** (`void Foo::bar() {}`) → `Method` with the same
  qualified name `Foo::bar`, and `language_attrs.out_of_line: true`.
- **Free-function prototypes** are skipped, following the C adapter
  (`c.rs` `tests::extern_declaration_skipped`).
- When a declaration and a definition share a name, the resolver prefers the
  bodied definition as a `Call` target.

### Overloads

Overloads share a `qualified_name` and differ by signature. The symbols table
has no uniqueness constraint that blocks this, and `sutra_symbol` returns
every overload.

The resolver does no overload resolution: a call to `foo` links to **every**
overload of `foo`. That over-reports callers but never misses one. Linking
none would make `sutra_refs` and `sutra_impact` silently wrong for
overloaded APIs.

### Test macros

Some test macros parse as a `function_definition`: `TEST`, `TEST_F`,
`TEST_P`, `TYPED_TEST`, `TEST_CASE` and `SCENARIO`. When the declarator name
is one of them, the macro name isn't used as the symbol name. Instead:

- The symbol is named from the macro's arguments, joined with `.`.
  String-literal quotes are stripped. So `TEST(Suite, Name)` becomes
  `Suite.Name` and `TEST_CASE("does x")` becomes `does x`.
- `FLAG_TEST` is set.
- The return type may be missing. Hyprland's `TEST_CASE(name) {` parses with a
  MISSING type node.
- The separator is `.`, not `::`, so the suite doesn't read as a class
  qualifier to name resolution.

Without this, `TEST` becomes a name collision hundreds of entries wide.

### Module boundary strength

Weak. Namespaces are open and nothing enforces them. Revisit if C++20 modules
are ever indexed.

### Layering

`cpp.rs` reuses `c.rs` helpers for the C subset, with their visibility widened
to `pub(super)`, the same way `typescript.rs` reuses `javascript.rs`. C
extraction is not copy-pasted.

### Risks

- **Macro noise dominates parse errors on real C++.** Common sources:
  - export macros between `class` and the name (`class LEVELDB_EXPORT DB`);
  - thread-safety annotations after declarators (`GUARDED_BY(mu_)`);
  - `if UNLIKELY(x)`;
  - `decltype(member)` in argument lists.

  Budget for it in the adapter's error tolerance.
- **tree-sitter-cpp defects:**
  - comma expressions inside parenthesized conditions;
  - `T *this;` local declarations, which Ghidra emits.

  Both matter only for decompiled `.c` routed to cpp.

## Shared work

Both languages benefit from infrastructure that's not language-specific:

- **Cargo.toml:** Add `tree-sitter-c` and `tree-sitter-python` as
  dependencies.
- **LanguageRegistry:** Register both adapters in `default_registry()`.
  Workspace registration needs to accept `"c"` and `"python"` as language
  strings.
- **`sutra workspaces add`:** Currently takes a single language. For mixed
  codebases, the workspace registration already accepts a list of languages
  — no change needed.
- **Test coverage:** Each adapter needs at least: a smoke parse test, a
  symbol extraction test covering all kinds, a reference classification
  test, an import edge test, and a complexity test. Follow the patterns in
  `src/parser/rust.rs::tests` and the integration tests in
  `tests/review-test.rs`.

## Priority

C is simpler to add (no dynamic dispatch ambiguity, no decorator analysis,
straightforward visibility) and the header resolution problem is solvable
with the relative-includes-first approach. Python is higher-value (much
larger ecosystem, more AI-assisted projects) but the reference noise is a
real quality concern.

Recommendation: C first (cleaner integration, validates the adapter
interface on a third language), Python second (benefits from any interface
adjustments discovered during C).
