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
as_fca_source()         → Option<&dyn FcaAttributeSource>
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
`SymbolKind::Static`. Without this, `sutra_grep` can't find module-level
configuration, constants, or global state.

Everything above Layer 0 — conventions (FCA), constraints (DD), health
(biomarkers), similarity (HRR), components, review — is language-agnostic.
A new language only needs a parser module and an adapter registration in
`default_registry()`.

### Optional: FcaAttributeSource

Languages that implement `FcaAttributeSource` get richer convention discovery:

```
extract_attributes(sym, file_path) → Option<SymbolAttrs>
effect_patterns()                  → &[EffectPattern]
```

Without it, FCA uses only cross-language attributes (kind, visibility,
has_doc, has_sig, complexity bucket, naming convention, directory). This is
functional but shallower — fewer language-specific patterns detected.

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

**Proposed approach:** Resolve relative and project-root-relative includes.
System includes (`<...>`) become unresolved external edges. If a project has
a `compile_commands.json`, optionally parse it for `-I` paths to improve
resolution. Start without `compile_commands.json` support — relative
includes cover the majority of internal dependencies.

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

### FCA attributes

Implement `FcaAttributeSource` for `CAdapter`:

| Attribute | Source |
|---|---|
| `returns_ptr` | Return type contains `*` |
| `takes_ptr` | Any parameter contains `*` |
| `is_static` | `static` storage class |
| `is_inline` | `inline` specifier |
| `is_variadic` | `...` in parameter list |
| `has_const` | `const` in signature |
| `returns_void` | Return type is `void` |
| `has_struct_param` | Parameter is a struct type |

Effect patterns:
- `malloc`, `calloc`, `realloc`, `free` → `effect:heap`
- `fopen`, `fclose`, `fread`, `fwrite`, `fprintf` → `effect:fs`
- `socket`, `connect`, `send`, `recv` → `effect:net`
- `printf`, `puts`, `fputs` → `effect:io`

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
| FCA attributes + effect patterns | 1 |
| Flags (test/FFI heuristics) | 0.5 |
| **Total** | **5** |

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

### FCA attributes

Implement `FcaAttributeSource` for `PythonAdapter`:

| Attribute | Source |
|---|---|
| `is_async` | `async` keyword on function_definition |
| `has_decorator` | Has decorated_definition parent |
| `decorator:X` | Specific decorator name (e.g. `decorator:staticmethod`) |
| `is_classmethod` | `@classmethod` decorator |
| `is_staticmethod` | `@staticmethod` decorator |
| `is_property` | `@property` decorator |
| `has_type_hints` | Any parameter or return has type annotation |
| `returns_none` | Return type is `None` or no return statement |
| `is_generator` | Contains `yield` statement |
| `is_contextmanager` | `@contextmanager` decorator |
| `has_dataclass` | Class with `@dataclass` decorator |

Decorators are an especially rich FCA signal — Python projects lean on
them heavily and they encode strong conventions.

Effect patterns:
- `open`, `Path().read_text` → `effect:fs`
- `requests.*`, `urllib.*`, `aiohttp.*` → `effect:net`
- `cursor.execute`, `session.query` → `effect:db`
- `print`, `logging.*` → `effect:io`
- `subprocess.*`, `os.system` → `effect:process`

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
| FCA attributes + decorator analysis | 1.5 |
| Flags (pytest/unittest detection) | 0.5 |
| **Total** | **6** |

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
