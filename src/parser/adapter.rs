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
        let lang_id = adapter.language_id().to_string();
        #[allow(deprecated)] // tree-sitter 0.25 prefers progress_callback; migrate when 0.26 drops the old API
        let parser = self.parsers.entry(lang_id).or_insert_with(|| {
            let mut p = Parser::new();
            p.set_language(&adapter.grammar())
                .expect("adapter returned invalid grammar");
            p.set_timeout_micros(self.timeout_micros);
            p
        });

        let tree = parser
            .parse(source, None)
            .ok_or_else(|| SutraError::Parse("tree-sitter parse timed out or returned no tree".into()))?;

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

pub trait FcaAttributeSource: Send + Sync {}

pub trait LanguageAdapter: Send + Sync {
    fn language_id(&self) -> &str;
    fn extensions(&self) -> &[&str];
    fn grammar(&self) -> Language;
    fn parse(&self, ctx: &ParseContext) -> Result<ParseResult>;
    fn as_fca_source(&self) -> Option<&dyn FcaAttributeSource> {
        None
    }
}

pub struct LanguageRegistry {
    adapters: Vec<Box<dyn LanguageAdapter>>,
    ext_map: HashMap<String, usize>,
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
        self.ext_map.get(ext).map(|&idx| self.adapters[idx].as_ref())
    }

    pub fn adapter_for_language(&self, lang: &str) -> Option<&dyn LanguageAdapter> {
        self.adapters
            .iter()
            .find(|a| a.language_id() == lang)
            .map(|a| a.as_ref())
    }

    pub fn extensions_for_languages(&self, langs: &[String]) -> Vec<&str> {
        self.adapters
            .iter()
            .filter(|a| langs.iter().any(|l| l == a.language_id()))
            .flat_map(|a| a.extensions().iter().copied())
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
}

impl FcaAttributeSource for RustAdapter {}

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
}

pub fn default_registry() -> LanguageRegistry {
    let mut r = LanguageRegistry::new();
    r.register(Box::new(RustAdapter));
    r.register(Box::new(DartAdapter));
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

        assert!(registry.adapter_for_language("python").is_none());
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
    fn fca_source_capability() {
        let rust = RustAdapter;
        assert!(rust.as_fca_source().is_some());

        let dart = DartAdapter;
        assert!(dart.as_fca_source().is_none());

        let test = TestAdapter;
        assert!(test.as_fca_source().is_none());
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
}
