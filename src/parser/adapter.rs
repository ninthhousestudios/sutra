use std::collections::HashMap;
use std::time::Duration;

use tree_sitter::{Language, Parser, Tree};

use crate::conventions::{EffectPattern, SymbolAttrs, extract_cross_language_attrs};
use crate::db::SymbolRow;
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
        let lang_id = adapter.language_id().to_string();
        if !self.parsers.contains_key(&lang_id) {
            let mut p = Parser::new();
            p.set_language(&adapter.grammar()).map_err(|e| {
                SutraError::Parse(format!("failed to set language for {}: {e}", lang_id))
            })?;
            #[allow(deprecated)]
            // tree-sitter 0.25 prefers progress_callback; migrate when 0.26 drops the old API
            p.set_timeout_micros(self.timeout_micros);
            self.parsers.insert(lang_id.clone(), p);
        }
        let parser = self.parsers.get_mut(&lang_id).unwrap();

        let tree = parser.parse(source, None).ok_or_else(|| {
            SutraError::Parse("tree-sitter parse timed out or returned no tree".into())
        })?;

        let ctx = ParseContext {
            source: source.as_bytes(),
            tree: &tree,
            file_path,
        };
        adapter.parse(&ctx)
    }

    #[cfg(test)]
    pub(crate) fn pool_size(&self) -> usize {
        self.parsers.len()
    }
}

#[derive(Clone, Copy)]
pub struct ToolchainPair {
    pub antecedent: &'static str,
    pub consequent: &'static str,
}

pub trait FcaAttributeSource: Send + Sync {
    fn extract_attributes(&self, sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs>;
    fn effect_patterns(&self) -> &[EffectPattern] {
        &[]
    }
    fn toolchain_enforced_pairs(&self) -> &[ToolchainPair] {
        &[]
    }
}

fn extract_attrs_with_language_bools(sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
    let mut sa = extract_cross_language_attrs(sym, file_path)?;
    if let Some(ref la_json) = sym.language_attrs {
        match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(la_json) {
            Ok(map) => {
                for (key, val) in &map {
                    if val.as_bool() == Some(true) {
                        sa.attributes.push(key.to_owned());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    symbol = %sym.qualified_name,
                    file = %file_path,
                    error = %e,
                    "malformed language_attrs JSON, skipping language-specific attributes"
                );
            }
        }
    }
    Some(sa)
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
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        None
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

    pub fn adapter_for_extension(&self, ext: &str) -> Option<&dyn LanguageAdapter> {
        self.ext_map
            .get(ext)
            .map(|&idx| self.adapters[idx].as_ref())
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
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        Some(self)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Strong
    }
}

const RUST_TOOLCHAIN_PAIRS: &[ToolchainPair] = &[
    ToolchainPair {
        antecedent: "kind:function",
        consequent: "naming:snake_case",
    },
    ToolchainPair {
        antecedent: "kind:const",
        consequent: "naming:SCREAMING",
    },
    ToolchainPair {
        antecedent: "kind:struct",
        consequent: "naming:CamelCase",
    },
    ToolchainPair {
        antecedent: "kind:enum",
        consequent: "naming:CamelCase",
    },
    ToolchainPair {
        antecedent: "kind:trait",
        consequent: "naming:CamelCase",
    },
    ToolchainPair {
        antecedent: "kind:type_alias",
        consequent: "naming:CamelCase",
    },
];

const RUST_EFFECT_PATTERNS: &[EffectPattern] = &[
    EffectPattern {
        attr_name: "effect:fs",
        callee_prefixes: &["std::fs::", "tokio::fs::"],
    },
    EffectPattern {
        attr_name: "effect:net",
        callee_prefixes: &["std::net::", "tokio::net::", "hyper::", "reqwest::"],
    },
    EffectPattern {
        attr_name: "effect:db",
        callee_prefixes: &["sqlx::", "diesel::", "rusqlite::"],
    },
];

impl FcaAttributeSource for RustAdapter {
    fn extract_attributes(&self, sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
        extract_attrs_with_language_bools(sym, file_path)
    }

    fn effect_patterns(&self) -> &[EffectPattern] {
        RUST_EFFECT_PATTERNS
    }

    fn toolchain_enforced_pairs(&self) -> &[ToolchainPair] {
        RUST_TOOLCHAIN_PAIRS
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
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        Some(self)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Moderate
    }
}

const DART_TOOLCHAIN_PAIRS: &[ToolchainPair] = &[
    ToolchainPair {
        antecedent: "kind:class",
        consequent: "naming:CamelCase",
    },
    ToolchainPair {
        antecedent: "kind:mixin",
        consequent: "naming:CamelCase",
    },
    ToolchainPair {
        antecedent: "kind:enum",
        consequent: "naming:CamelCase",
    },
    ToolchainPair {
        antecedent: "kind:type_alias",
        consequent: "naming:CamelCase",
    },
];

const DART_EFFECT_PATTERNS: &[EffectPattern] = &[
    EffectPattern {
        attr_name: "effect:fs",
        callee_prefixes: &["dart:io::File", "dart:io::Directory", "dart:io::FileSystem"],
    },
    EffectPattern {
        attr_name: "effect:net",
        callee_prefixes: &["http::", "dio::", "dart:io::HttpClient", "dart:io::Socket"],
    },
    EffectPattern {
        attr_name: "effect:db",
        callee_prefixes: &["sqflite::", "drift::", "isar::", "hive::"],
    },
];

impl FcaAttributeSource for DartAdapter {
    fn extract_attributes(&self, sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
        extract_attrs_with_language_bools(sym, file_path)
    }

    fn effect_patterns(&self) -> &[EffectPattern] {
        DART_EFFECT_PATTERNS
    }

    fn toolchain_enforced_pairs(&self) -> &[ToolchainPair] {
        DART_TOOLCHAIN_PAIRS
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
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        Some(self)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Weak
    }
}

const C_EFFECT_PATTERNS: &[EffectPattern] = &[
    EffectPattern {
        attr_name: "effect:heap",
        callee_prefixes: &["malloc", "calloc", "realloc", "free"],
    },
    EffectPattern {
        attr_name: "effect:fs",
        callee_prefixes: &["fopen", "fclose", "fread", "fwrite", "fprintf"],
    },
    EffectPattern {
        attr_name: "effect:net",
        callee_prefixes: &["socket", "connect", "send", "recv"],
    },
    EffectPattern {
        attr_name: "effect:io",
        callee_prefixes: &["printf", "puts", "fputs"],
    },
];

impl FcaAttributeSource for CAdapter {
    fn extract_attributes(&self, sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
        extract_attrs_with_language_bools(sym, file_path)
    }

    fn effect_patterns(&self) -> &[EffectPattern] {
        C_EFFECT_PATTERNS
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
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        Some(self)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Weak
    }
}

const PYTHON_EFFECT_PATTERNS: &[EffectPattern] = &[
    EffectPattern {
        attr_name: "effect:fs",
        callee_prefixes: &[
            "open",
            "os.path",
            "os.mkdir",
            "os.remove",
            "shutil",
            "pathlib",
        ],
    },
    EffectPattern {
        attr_name: "effect:net",
        callee_prefixes: &["requests", "urllib", "http.client", "socket", "aiohttp"],
    },
    EffectPattern {
        attr_name: "effect:db",
        callee_prefixes: &["sqlite3", "psycopg", "sqlalchemy", "pymongo", "redis"],
    },
    EffectPattern {
        attr_name: "effect:io",
        callee_prefixes: &["print", "input", "sys.stdout", "sys.stderr", "logging"],
    },
    EffectPattern {
        attr_name: "effect:process",
        callee_prefixes: &["subprocess", "os.system", "os.exec", "os.spawn"],
    },
];

impl FcaAttributeSource for PythonAdapter {
    fn extract_attributes(&self, sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
        extract_attrs_with_language_bools(sym, file_path)
    }

    fn effect_patterns(&self) -> &[EffectPattern] {
        PYTHON_EFFECT_PATTERNS
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
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        Some(self)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Moderate
    }
}

const JS_EFFECT_PATTERNS: &[EffectPattern] = &[
    EffectPattern {
        attr_name: "effect:dom",
        callee_prefixes: &["document.", "window.", "globalThis."],
    },
    EffectPattern {
        attr_name: "effect:net",
        callee_prefixes: &["fetch", "XMLHttpRequest", "axios."],
    },
    EffectPattern {
        attr_name: "effect:fs",
        callee_prefixes: &["fs.", "readFile", "writeFile"],
    },
    EffectPattern {
        attr_name: "effect:process",
        callee_prefixes: &["process.exit", "process.env"],
    },
    EffectPattern {
        attr_name: "effect:console",
        callee_prefixes: &["console.log", "console.error"],
    },
    EffectPattern {
        attr_name: "effect:async",
        callee_prefixes: &["Promise", "then"],
    },
];

const JS_TOOLCHAIN_PAIRS: &[ToolchainPair] = &[ToolchainPair {
    antecedent: "async",
    consequent: "await",
}];

impl FcaAttributeSource for JsAdapter {
    fn extract_attributes(&self, sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
        extract_attrs_with_language_bools(sym, file_path)
    }

    fn effect_patterns(&self) -> &[EffectPattern] {
        JS_EFFECT_PATTERNS
    }

    fn toolchain_enforced_pairs(&self) -> &[ToolchainPair] {
        JS_TOOLCHAIN_PAIRS
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
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        Some(self)
    }
    fn module_boundary_hints(&self) -> ModuleBoundaryStrength {
        ModuleBoundaryStrength::Moderate
    }
}

const TS_TOOLCHAIN_PAIRS: &[ToolchainPair] = &[ToolchainPair {
    antecedent: "async",
    consequent: "await",
}];

impl FcaAttributeSource for TsAdapter {
    fn extract_attributes(&self, sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
        extract_attrs_with_language_bools(sym, file_path)
    }

    fn effect_patterns(&self) -> &[EffectPattern] {
        JS_EFFECT_PATTERNS
    }

    fn toolchain_enforced_pairs(&self) -> &[ToolchainPair] {
        TS_TOOLCHAIN_PAIRS
    }
}

pub fn default_registry() -> LanguageRegistry {
    let mut r = LanguageRegistry::new();
    r.register(Box::new(RustAdapter));
    r.register(Box::new(DartAdapter));
    r.register(Box::new(CAdapter));
    r.register(Box::new(PythonAdapter));
    r.register(Box::new(JsAdapter));
    r.register(Box::new(TsAdapter));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn fca_source_capability() {
        let rust = RustAdapter;
        assert!(rust.as_fca_source().is_some());

        let dart = DartAdapter;
        assert!(dart.as_fca_source().is_some());

        let test = TestAdapter;
        assert!(test.as_fca_source().is_none());
    }

    fn make_test_symbol_row(
        kind: &str,
        visibility: Option<&str>,
        signature: Option<&str>,
        language_attrs: Option<&str>,
    ) -> SymbolRow {
        SymbolRow {
            id: 1,
            file_id: 1,
            qualified_name: "mod::my_func".into(),
            short_name: "my_func".into(),
            kind: kind.into(),
            signature: signature.map(Into::into),
            signature_hash: None,
            structural_hash: None,
            visibility: visibility.map(Into::into),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            pagerank: None,
            cyclomatic: None,
            cognitive: Some(3),
            max_nesting: None,
            flags: 0,
            language_attrs: language_attrs.map(Into::into),
        }
    }

    #[test]
    fn rust_fca_source_extracts_from_language_attrs() {
        let rust = RustAdapter;
        let fca = rust.as_fca_source().unwrap();
        let sym = make_test_symbol_row(
            "function",
            Some("pub"),
            Some("fn my_func() -> Result<()>"),
            Some(r#"{"returns_result":true,"is_async":true}"#),
        );
        let attrs = fca.extract_attributes(&sym, "src/tools/foo.rs").unwrap();
        assert!(attrs.attributes.contains(&"kind:function".to_string()));
        assert!(attrs.attributes.contains(&"vis:pub".to_string()));
        assert!(attrs.attributes.contains(&"has_sig".to_string()));
        assert!(attrs.attributes.contains(&"complexity:low".to_string()));
        assert!(attrs.attributes.contains(&"returns_result".to_string()));
        assert!(attrs.attributes.contains(&"is_async".to_string()));
    }

    #[test]
    fn rust_fca_source_works_without_language_attrs() {
        let rust = RustAdapter;
        let fca = rust.as_fca_source().unwrap();
        let sym = make_test_symbol_row("function", Some("pub"), Some("fn foo()"), None);
        let attrs = fca.extract_attributes(&sym, "src/foo.rs").unwrap();
        assert!(attrs.attributes.contains(&"kind:function".to_string()));
        assert!(!attrs.attributes.contains(&"returns_result".to_string()));
    }

    #[test]
    fn dart_fca_source_extracts_language_attrs() {
        let dart = DartAdapter;
        let fca = dart.as_fca_source().unwrap();
        let mut sym = make_test_symbol_row(
            "function",
            Some("public"),
            Some("Future<void> fetch()"),
            None,
        );
        sym.language_attrs = Some(r#"{"is_async":true,"returns_future":true}"#.into());
        let attrs = fca.extract_attributes(&sym, "lib/api.dart").unwrap();
        assert!(attrs.attributes.contains(&"is_async".to_string()));
        assert!(attrs.attributes.contains(&"returns_future".to_string()));
        assert!(attrs.attributes.contains(&"kind:function".to_string()));
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
        assert_eq!(mults.get("python"), Some(&1.0));
        assert_eq!(mults.get("javascript"), Some(&1.5));
        assert_eq!(mults.get("typescript"), Some(&1.5));
        assert_eq!(mults.len(), 6);
    }

    #[test]
    fn rust_adapter_has_effect_patterns() {
        let rust = RustAdapter;
        let source = rust.as_fca_source().unwrap();
        let patterns = source.effect_patterns();
        assert!(!patterns.is_empty());
        let names: Vec<_> = patterns.iter().map(|p| p.attr_name).collect();
        assert!(names.contains(&"effect:fs"));
        assert!(names.contains(&"effect:net"));
        assert!(names.contains(&"effect:db"));
    }

    #[test]
    fn dart_adapter_has_effect_patterns() {
        let dart = DartAdapter;
        let source = dart.as_fca_source().unwrap();
        let patterns = source.effect_patterns();
        assert!(!patterns.is_empty());
        let names: Vec<_> = patterns.iter().map(|p| p.attr_name).collect();
        assert!(names.contains(&"effect:fs"));
        assert!(names.contains(&"effect:net"));
        assert!(names.contains(&"effect:db"));
    }

    #[test]
    fn js_extract_attributes_surfaces_language_bools() {
        let js = JsAdapter;
        let fca = js.as_fca_source().unwrap();
        let sym = make_test_symbol_row(
            "function",
            Some("export"),
            Some("async function fetchData()"),
            Some(r#"{"async":true,"await":true}"#),
        );
        let attrs = fca.extract_attributes(&sym, "src/api.js").unwrap();
        assert!(attrs.attributes.contains(&"async".to_string()));
        assert!(attrs.attributes.contains(&"await".to_string()));
        assert!(attrs.attributes.contains(&"vis:pub".to_string()));
    }

    #[test]
    fn ts_extract_attributes_surfaces_language_bools() {
        let ts = TsAdapter;
        let fca = ts.as_fca_source().unwrap();
        let sym = make_test_symbol_row(
            "method",
            None,
            Some("async getData(): Promise<Data>"),
            Some(r#"{"async":true,"readonly":true}"#),
        );
        let attrs = fca.extract_attributes(&sym, "src/service.ts").unwrap();
        assert!(attrs.attributes.contains(&"async".to_string()));
        assert!(attrs.attributes.contains(&"readonly".to_string()));
    }

    #[test]
    fn js_async_await_toolchain_pair_matches() {
        let js = JsAdapter;
        let fca = js.as_fca_source().unwrap();
        let pairs = fca.toolchain_enforced_pairs();
        let async_await = pairs
            .iter()
            .find(|p| p.antecedent == "async")
            .expect("async→await pair should exist");
        assert_eq!(async_await.consequent, "await");
    }

    #[test]
    fn export_visibility_maps_to_vis_pub() {
        let js = JsAdapter;
        let fca = js.as_fca_source().unwrap();
        let sym = make_test_symbol_row("function", Some("export"), Some("function init()"), None);
        let attrs = fca.extract_attributes(&sym, "src/app.js").unwrap();
        assert!(attrs.attributes.contains(&"vis:pub".to_string()));

        let sym_default = make_test_symbol_row(
            "function",
            Some("export default"),
            Some("function main()"),
            None,
        );
        let attrs_default = fca.extract_attributes(&sym_default, "src/main.js").unwrap();
        assert!(attrs_default.attributes.contains(&"vis:pub".to_string()));
    }
}
