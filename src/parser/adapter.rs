use std::collections::HashMap;
use std::time::Duration;

use tree_sitter::{Language, Parser, Tree};

use crate::error::{Result, SutraError};

use super::ParseResult;

pub struct ParseContext<'a> {
    pub source: &'a [u8],
    pub tree: &'a Tree,
    pub file_path: &'a str,
}

pub struct ParserPool {
    parsers: HashMap<String, Parser>,
    timeout_micros: u64,
}

impl ParserPool {
    pub fn new(timeout: Duration) -> Self {
        Self {
            parsers: HashMap::new(),
            timeout_micros: timeout.as_micros() as u64,
        }
    }

    pub fn parse_with(
        &mut self,
        adapter: &dyn LanguageAdapter,
        source: &str,
        file_path: &str,
    ) -> Result<ParseResult> {
        self.parse_tree(adapter, source, file_path)
            .map(|(_, result)| result)
    }

    /// [`parse_with`](Self::parse_with) plus the file's string literals, for
    /// the index (empty for a language whose literals are not indexed).
    pub fn parse_for_index(
        &mut self,
        adapter: &dyn LanguageAdapter,
        source: &str,
        file_path: &str,
    ) -> Result<(ParseResult, Vec<super::literals::ExtractedLiteral>)> {
        let (tree, result) = self.parse_tree(adapter, source, file_path)?;
        let literals = if super::literals::indexed_language(adapter.language_id()) {
            super::literals::extract(&tree, source.as_bytes())
        } else {
            Vec::new()
        };
        Ok((result, literals))
    }

    /// The syntax tree alone, without the adapter's symbol extraction.
    pub fn tree(&mut self, adapter: &dyn LanguageAdapter, source: &str) -> Result<Tree> {
        let lang_id = adapter.language_id();
        let parser = match self.parsers.entry(lang_id.to_string()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut p = Parser::new();
                p.set_language(&adapter.grammar()).map_err(|e| {
                    SutraError::Parse(format!("failed to set language for {lang_id}: {e}"))
                })?;
                #[allow(deprecated)]
                // tree-sitter 0.25 prefers progress_callback; migrate when 0.26 drops the old API
                p.set_timeout_micros(self.timeout_micros);
                e.insert(p)
            }
        };

        parser.parse(source, None).ok_or_else(|| {
            SutraError::Parse("tree-sitter parse timed out or returned no tree".into())
        })
    }

    fn parse_tree(
        &mut self,
        adapter: &dyn LanguageAdapter,
        source: &str,
        file_path: &str,
    ) -> Result<(Tree, ParseResult)> {
        let tree = self.tree(adapter, source)?;
        let ctx = ParseContext {
            source: source.as_bytes(),
            tree: &tree,
            file_path,
        };
        let mut result = adapter.parse(&ctx)?;

        // A whole-file test target (Rust `tests/`, Dart `test/`) carries no
        // per-item attribute for an adapter's `test_line_ranges` to find, so
        // path is the only signal that its imports are not production
        // dependencies (sutra/292).
        if adapter.is_test_path(file_path) {
            for imp in &mut result.imports {
                imp.is_test = true;
            }
        }
        Ok((tree, result))
    }

    #[cfg(test)]
    pub(crate) fn pool_size(&self) -> usize {
        self.parsers.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleBoundaryStrength {
    Strong,
    Moderate,
    Weak,
}

impl ModuleBoundaryStrength {
    pub fn multiplier(&self) -> f64 {
        match self {
            Self::Strong => 2.0,
            Self::Moderate => 1.5,
            Self::Weak => 1.0,
        }
    }
}

/// True when `line` (1-based) falls inside any of `ranges`, as produced by
/// [`LanguageAdapter::test_line_ranges`]. Ranges are few — one per test module —
/// so a linear scan beats building an interval tree.
pub fn line_in_ranges(ranges: &[(u32, u32)], line: u32) -> bool {
    ranges
        .iter()
        .any(|&(start, end)| line >= start && line <= end)
}

/// The source text a tree-sitter node spans. Every adapter parses a `&str`, and
/// tree-sitter only places node boundaries on code-point boundaries of valid
/// UTF-8 input, so `utf8_text` cannot fail here — a failure means the bytes
/// handed in are not the source that was parsed.
pub fn node_text<'a>(node: tree_sitter::Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src)
        .expect("invariant: source is UTF-8 and node boundaries fall on char boundaries")
}

/// True when a *directory* component of `path` equals `dir`. Matches both a
/// root-level `dir/...` and a nested `packages/foo/dir/...`, since indexed
/// paths are relative to the workspace root and a monorepo buries each crate's
/// or package's test target one level down. The final component is excluded so
/// a source file literally named `test` is not mistaken for a test directory.
pub fn path_has_dir_segment(path: &str, dir: &str) -> bool {
    let Some((parent, _)) = path.rsplit_once('/') else {
        return false;
    };
    parent.split('/').any(|seg| seg == dir)
}

/// True when `path` lives under a directory that conventionally holds tests —
/// `test/` or `tests/` at any depth. Shared by every language whose test layout
/// is directory-based; Rust deliberately does not use it, because Cargo gives
/// `tests/` and `benches/` an exact meaning that a bare `test/` lacks.
pub fn path_in_test_dir(path: &str) -> bool {
    path_has_dir_segment(path, "test") || path_has_dir_segment(path, "tests")
}

pub trait LanguageAdapter: Send + Sync {
    fn language_id(&self) -> &str;
    /// Extensions this adapter indexes: parsed into symbols, imports and rollups.
    fn extensions(&self) -> &[&str];
    /// Extensions eligible for `forbidden_pattern` matching. Superset of
    /// `extensions()`; the extras are parsed for constraint evaluation only and
    /// never enter the symbol graph (e.g. Python `.pyi` stubs, which would
    /// otherwise double-count every symbol their `.py` sibling declares).
    fn pattern_extensions(&self) -> &[&str] {
        self.extensions()
    }
    fn grammar(&self) -> Language;
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult>;
    /// Line ranges (1-based, inclusive) covering test-only code — Rust's
    /// `#[cfg(test)]` items and `#[test]` functions. Constraint evaluation
    /// skips matches inside these ranges unless the rule sets
    /// `include_tests = true`. Default: none, so a language opts in by
    /// overriding rather than by accident.
    fn test_line_ranges(&self, _ctx: &ParseContext) -> Vec<(u32, u32)> {
        Vec::new()
    }
    /// Whether `path` is test code in its entirety by convention — Rust's
    /// `tests/` and `benches/` targets, Dart's `test/` directory and
    /// `_test.dart` files. These carry no per-item attribute, so
    /// [`LanguageAdapter::test_line_ranges`] cannot see them. Constraint
    /// evaluation skips such files unless the rule sets `include_tests = true`
    /// or scopes itself into a test path. Default: false, so a language opts in
    /// by overriding rather than by accident.
    fn is_test_path(&self, _path: &str) -> bool {
        false
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Weak
    }
}

pub struct LanguageRegistry {
    adapters: Vec<Box<dyn LanguageAdapter>>,
    ext_map: HashMap<String, usize>,
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self {
            adapters: Vec::new(),
            ext_map: HashMap::new(),
        }
    }

    pub fn register(&mut self, adapter: Box<dyn LanguageAdapter>) {
        let idx = self.adapters.len();
        for ext in adapter.extensions() {
            self.ext_map.insert(ext.to_string(), idx);
        }
        self.adapters.push(adapter);
    }

    /// Language ids of every registered adapter, in registration order.
    pub fn language_ids(&self) -> Vec<&str> {
        self.adapters.iter().map(|a| a.language_id()).collect()
    }

    pub fn adapter_for_extension(&self, ext: &str) -> Option<&dyn LanguageAdapter> {
        self.ext_map
            .get(ext)
            .map(|&idx| self.adapters[idx].as_ref())
    }

    /// The adapter claiming `path`'s extension. `None` when the path has no
    /// extension, a non-UTF-8 one, or one no adapter registers.
    pub fn adapter_for_path(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Option<&dyn LanguageAdapter> {
        let ext = path.as_ref().extension()?.to_str()?;
        self.adapter_for_extension(ext)
    }

    pub fn adapter_for_language(&self, lang: &str) -> Option<&dyn LanguageAdapter> {
        self.adapters
            .iter()
            .find(|a| a.language_id().eq_ignore_ascii_case(lang))
            .map(|a| a.as_ref())
    }

    pub fn extensions_for_languages(&self, langs: &[String]) -> Vec<&str> {
        self.adapters
            .iter()
            .filter(|a| {
                langs
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(a.language_id()))
            })
            .flat_map(|a| a.extensions().iter().copied())
            .collect()
    }

    /// The adapter whose pattern-eligible extensions cover `path` — unlike
    /// [`Self::adapter_for_extension`], this also resolves unindexed stubs
    /// (`.pyi`).
    pub fn adapter_for_pattern_path(&self, path: &str) -> Option<&dyn LanguageAdapter> {
        self.adapters
            .iter()
            .find(|a| {
                a.pattern_extensions()
                    .iter()
                    .any(|ext| path.ends_with(&format!(".{ext}")))
            })
            .map(|a| a.as_ref())
    }

    /// Extensions that are pattern-eligible but never indexed. These files are
    /// invisible to the symbol graph, so constraint evaluation has to find them
    /// on disk rather than through the files table.
    pub fn pattern_only_extensions(&self) -> Vec<&str> {
        self.adapters
            .iter()
            .flat_map(|a| {
                let indexed = a.extensions();
                a.pattern_extensions()
                    .iter()
                    .copied()
                    .filter(move |ext| !indexed.contains(ext))
            })
            .collect()
    }

    /// Whether any registered language's conventions mark `path` as test scope.
    pub fn any_is_test_path(&self, path: &str) -> bool {
        self.adapters.iter().any(|a| a.is_test_path(path))
    }

    pub fn boundary_multipliers(&self) -> HashMap<String, f64> {
        self.adapters
            .iter()
            .map(|a| {
                (
                    a.language_id().to_string(),
                    a.module_boundary_hints().multiplier(),
                )
            })
            .collect()
    }
}

pub struct RustAdapter;

impl LanguageAdapter for RustAdapter {
    fn language_id(&self) -> &str {
        "rust"
    }
    fn extensions(&self) -> &[&str] {
        &["rs"]
    }
    fn grammar(&self) -> Language {
        tree_sitter_rust::LANGUAGE.into()
    }
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
        super::rust::parse(ctx)
    }
    fn test_line_ranges(&self, ctx: &ParseContext) -> Vec<(u32, u32)> {
        super::rust::test_line_ranges(ctx)
    }
    fn is_test_path(&self, path: &str) -> bool {
        super::rust::is_test_path(path)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Strong
    }
}

pub struct DartAdapter;

impl LanguageAdapter for DartAdapter {
    fn language_id(&self) -> &str {
        "dart"
    }
    fn extensions(&self) -> &[&str] {
        &["dart"]
    }
    fn grammar(&self) -> Language {
        tree_sitter_dart::LANGUAGE.into()
    }
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
        super::dart::parse(ctx)
    }
    fn is_test_path(&self, path: &str) -> bool {
        super::dart::is_test_path(path)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Moderate
    }
}

pub struct CAdapter;

impl LanguageAdapter for CAdapter {
    fn language_id(&self) -> &str {
        "c"
    }
    fn extensions(&self) -> &[&str] {
        &["c", "h"]
    }
    fn grammar(&self) -> Language {
        tree_sitter_c::LANGUAGE.into()
    }
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
        super::c::parse(ctx)
    }
    fn is_test_path(&self, path: &str) -> bool {
        super::c::is_test_path(path)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Weak
    }
}

pub struct CppAdapter;

impl LanguageAdapter for CppAdapter {
    fn language_id(&self) -> &str {
        "cpp"
    }
    fn extensions(&self) -> &[&str] {
        super::cpp::EXTENSIONS
    }
    fn grammar(&self) -> Language {
        tree_sitter_cpp::LANGUAGE.into()
    }
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
        super::cpp::parse(ctx)
    }
    fn test_line_ranges(&self, ctx: &ParseContext) -> Vec<(u32, u32)> {
        super::cpp::test_line_ranges(ctx)
    }
    fn is_test_path(&self, path: &str) -> bool {
        super::cpp::is_test_path(path)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        // Namespaces are open and nothing enforces them (sutra/525).
        ModuleBoundaryStrength::Weak
    }
}

pub struct PythonAdapter;

impl LanguageAdapter for PythonAdapter {
    fn language_id(&self) -> &str {
        "python"
    }
    fn extensions(&self) -> &[&str] {
        &["py"]
    }
    fn pattern_extensions(&self) -> &[&str] {
        &["py", "pyi"]
    }
    fn grammar(&self) -> Language {
        tree_sitter_python::LANGUAGE.into()
    }
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
        super::python::parse(ctx)
    }
    fn is_test_path(&self, path: &str) -> bool {
        super::python::is_test_path(path)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Weak
    }
}

pub struct JsAdapter;

impl LanguageAdapter for JsAdapter {
    fn language_id(&self) -> &str {
        "javascript"
    }
    fn extensions(&self) -> &[&str] {
        &["js", "jsx", "mjs", "cjs"]
    }
    fn grammar(&self) -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
        super::javascript::parse(ctx)
    }
    fn is_test_path(&self, path: &str) -> bool {
        super::javascript::is_test_path(path)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Moderate
    }
}

pub struct TsAdapter;

impl LanguageAdapter for TsAdapter {
    fn language_id(&self) -> &str {
        "typescript"
    }
    fn extensions(&self) -> &[&str] {
        &["ts", "tsx", "mts", "cts"]
    }
    fn grammar(&self) -> Language {
        // TSX grammar is a superset of TS — handles both correctly
        tree_sitter_typescript::LANGUAGE_TSX.into()
    }
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
        super::typescript::parse(ctx)
    }
    fn is_test_path(&self, path: &str) -> bool {
        // Shared with JS: the `.test.`/`.spec.` conventions and their
        // extensions are one vocabulary across both languages.
        super::javascript::is_test_path(path)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Moderate
    }
}

/// Registry for path-only queries that arrive without a language in hand.
static PATH_REGISTRY: std::sync::OnceLock<LanguageRegistry> = std::sync::OnceLock::new();

/// Whether any registered language's conventions mark `path` as test scope.
///
/// Dependency, cycle and external rules are not written against one grammar — a
/// `tests/**` glob spans every language in the workspace — so the test-directed
/// escape hatch asks the whole registry rather than a single adapter (sutra/296).
pub fn any_language_is_test_path(path: &str) -> bool {
    PATH_REGISTRY
        .get_or_init(default_registry)
        .any_is_test_path(path)
}

/// Language id for a path, derived from the registered adapters' extension map.
///
/// Deliberately not a literal. Hardcoded ext→language tables drifted from the
/// registry three times: the lessons language filter (sutra/280), the guard's
/// `parse_proposed` (rust/dart only, so no imports were extracted for other
/// languages), and `symbol_diff` (missing JS/TS, so symbol diffs silently
/// reported no changes — sutra/526). Every path-only caller goes through here
/// so a newly registered adapter is picked up everywhere at once.
pub fn language_for_path(path: &str) -> Option<&'static str> {
    PATH_REGISTRY
        .get_or_init(default_registry)
        .adapter_for_path(path)
        .map(|a| a.language_id())
}

pub fn default_registry() -> LanguageRegistry {
    let mut r = LanguageRegistry::new();
    r.register(Box::new(RustAdapter));
    r.register(Box::new(DartAdapter));
    r.register(Box::new(CAdapter));
    r.register(Box::new(CppAdapter));
    r.register(Box::new(PythonAdapter));
    r.register(Box::new(JsAdapter));
    r.register(Box::new(TsAdapter));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_for_path_covers_every_registered_adapter() {
        // Pinned against the registry rather than a literal list: if an adapter
        // is added and this fails, the fix is to extend the expectation, not to
        // re-hardcode the mapping.
        for (path, want) in [
            ("src/lib.rs", "rust"),
            ("lib/main.dart", "dart"),
            ("src/parse.c", "c"),
            ("src/parse.h", "c"),
            ("db/db_impl.cc", "cpp"),
            ("src/main.cpp", "cpp"),
            ("include/leveldb/db.hpp", "cpp"),
            ("src/parse.c++", "cpp"),
            ("app/models.py", "python"),
            ("web/index.js", "javascript"),
            ("web/App.jsx", "javascript"),
            ("web/index.ts", "typescript"),
            ("web/App.tsx", "typescript"),
        ] {
            assert_eq!(
                language_for_path(path),
                Some(want),
                "{path} should map to {want}"
            );
        }
        assert_eq!(language_for_path("README.md"), None);
        assert_eq!(language_for_path("Makefile"), None);
        // .pyi is pattern-eligible but not indexed (sutra/275) — the Python
        // adapter does not claim it, and path lookup must not either.
        assert_eq!(language_for_path("stubs/foo.pyi"), None);
    }

    struct TestAdapter;

    impl LanguageAdapter for TestAdapter {
        fn language_id(&self) -> &str {
            "test"
        }
        fn extensions(&self) -> &[&str] {
            &["tst", "test"]
        }
        fn grammar(&self) -> Language {
            tree_sitter_rust::LANGUAGE.into()
        }
        fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
            Ok(ParseResult {
                file_path: ctx.file_path.to_string(),
                language: "test".to_string(),
                symbols: vec![],
                references: vec![],
                imports: vec![],
                parsed_ok: !ctx.tree.root_node().has_error(),
                line_count: 0,
            })
        }
    }

    #[test]
    fn register_and_dispatch() {
        let mut registry = LanguageRegistry::new();
        registry.register(Box::new(TestAdapter));

        let adapter = registry.adapter_for_extension("tst").unwrap();
        assert_eq!(adapter.language_id(), "test");

        let also = registry.adapter_for_extension("test").unwrap();
        assert_eq!(also.language_id(), "test");

        let mut pool = ParserPool::new(Duration::from_secs(5));
        let result = pool.parse_with(adapter, "fn x() {}", "foo.tst").unwrap();
        assert_eq!(result.language, "test");
        assert!(result.parsed_ok);

        assert!(registry.adapter_for_extension("unknown").is_none());
    }

    #[test]
    fn adapter_for_path_resolves_by_extension() {
        let mut registry = LanguageRegistry::new();
        registry.register(Box::new(TestAdapter));

        let by_str = registry
            .adapter_for_path("src/a.tst")
            .map(|a| a.language_id());
        assert_eq!(by_str, Some("test"));
        let by_path = registry
            .adapter_for_path(std::path::Path::new("/abs/b.test"))
            .map(|a| a.language_id());
        assert_eq!(by_path, Some("test"));

        assert!(registry.adapter_for_path("src/a.unknown").is_none());
        assert!(registry.adapter_for_path("Makefile").is_none());
        assert!(registry.adapter_for_path("src/.tst").is_none());
    }

    #[test]
    fn lookup_by_language() {
        let registry = default_registry();
        let rust = registry.adapter_for_language("rust").unwrap();
        assert_eq!(rust.language_id(), "rust");

        let dart = registry.adapter_for_language("dart").unwrap();
        assert_eq!(dart.language_id(), "dart");

        let python = registry.adapter_for_language("python").unwrap();
        assert_eq!(python.language_id(), "python");
    }

    #[test]
    fn extensions_for_languages() {
        let registry = default_registry();
        let exts = registry.extensions_for_languages(&["rust".to_string(), "dart".to_string()]);
        assert!(exts.contains(&"rs"));
        assert!(exts.contains(&"dart"));

        let rust_only = registry.extensions_for_languages(&["rust".to_string()]);
        assert_eq!(rust_only, vec!["rs"]);
    }

    #[test]
    fn case_insensitive_language_matching() {
        let registry = default_registry();

        assert!(registry.adapter_for_language("Rust").is_some());
        assert!(registry.adapter_for_language("DART").is_some());
        assert!(registry.adapter_for_language("Python").is_some());

        let exts = registry.extensions_for_languages(&["Rust".to_string()]);
        assert_eq!(exts, vec!["rs"]);

        let exts = registry.extensions_for_languages(&["DART".to_string(), "rust".to_string()]);
        assert!(exts.contains(&"rs"));
        assert!(exts.contains(&"dart"));
    }

    /// `parse_with` flags every import in a test-path file, which is what feeds
    /// `db::production_import_edges` for the dep-shaped constraint kinds
    /// (sutra/292, extended to these languages in sutra/295).
    #[test]
    fn parse_with_flags_imports_in_test_paths() {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        let cases: [(&dyn LanguageAdapter, &str, &str, &str); 4] = [
            (
                &PythonAdapter,
                "import os\nfrom django.db import models\n",
                "tests/test_models.py",
                "app/models.py",
            ),
            (
                &CAdapter,
                "#include \"engine.h\"\n",
                "tests/engine.c",
                "src/engine.c",
            ),
            (
                &TsAdapter,
                "import { z } from 'zod';\n",
                "src/app.test.ts",
                "src/app.ts",
            ),
            (
                &JsAdapter,
                "import { z } from 'zod';\n",
                "__tests__/app.js",
                "src/app.js",
            ),
        ];
        for (adapter, source, test_path, prod_path) in cases {
            let in_test = pool.parse_with(adapter, source, test_path).unwrap();
            assert!(
                !in_test.imports.is_empty() && in_test.imports.iter().all(|i| i.is_test),
                "{test_path}: expected every import flagged, got {:?}",
                in_test.imports
            );
            let in_prod = pool.parse_with(adapter, source, prod_path).unwrap();
            assert!(
                in_prod.imports.iter().all(|i| !i.is_test),
                "{prod_path}: imports must stay production, got {:?}",
                in_prod.imports
            );
        }
    }

    #[test]
    fn parser_pool_reuses_parser() {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        let adapter = RustAdapter;

        let r1 = pool.parse_with(&adapter, "fn a() {}", "a.rs").unwrap();
        assert!(r1.parsed_ok);

        let r2 = pool.parse_with(&adapter, "fn b() {}", "b.rs").unwrap();
        assert!(r2.parsed_ok);

        assert_eq!(pool.pool_size(), 1);
    }

    #[test]
    fn timeout_rejects_pathological_input() {
        let mut pool = ParserPool::new(Duration::from_micros(1));
        let adapter = RustAdapter;

        // Generate deeply nested input that takes measurable time to parse
        let depth = 200;
        let mut src = String::new();
        for _ in 0..depth {
            src.push_str("fn f() { if true { ");
        }
        for _ in 0..depth {
            src.push_str("} }");
        }

        let result = pool.parse_with(&adapter, &src, "test.rs");
        assert!(result.is_err());
    }

    #[test]
    fn module_boundary_hints_defaults() {
        let test = TestAdapter;
        assert_eq!(test.module_boundary_hints(), ModuleBoundaryStrength::Weak);
        assert_eq!(ModuleBoundaryStrength::Weak.multiplier(), 1.0);
    }

    #[test]
    fn module_boundary_hints_rust_strong() {
        let rust = RustAdapter;
        assert_eq!(rust.module_boundary_hints(), ModuleBoundaryStrength::Strong);
        assert_eq!(ModuleBoundaryStrength::Strong.multiplier(), 2.0);
    }

    #[test]
    fn module_boundary_hints_dart_moderate() {
        let dart = DartAdapter;
        assert_eq!(
            dart.module_boundary_hints(),
            ModuleBoundaryStrength::Moderate
        );
        assert_eq!(ModuleBoundaryStrength::Moderate.multiplier(), 1.5);
    }

    #[test]
    fn boundary_multipliers_from_registry() {
        let registry = default_registry();
        let mults = registry.boundary_multipliers();
        assert_eq!(mults.get("rust"), Some(&2.0));
        assert_eq!(mults.get("dart"), Some(&1.5));
        assert_eq!(mults.get("c"), Some(&1.0));
        assert_eq!(mults.get("cpp"), Some(&1.0));
        assert_eq!(mults.get("python"), Some(&1.0));
        assert_eq!(mults.get("javascript"), Some(&1.5));
        assert_eq!(mults.get("typescript"), Some(&1.5));
        assert_eq!(mults.len(), 7);
    }
}
