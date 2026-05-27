use std::collections::HashMap;

use crate::error::Result;

use super::ParseResult;

pub trait FcaAttributeSource: Send + Sync {}

pub trait LanguageAdapter: Send + Sync {
    fn language_id(&self) -> &str;
    fn extensions(&self) -> &[&str];
    fn parse(&self, source: &str, file_path: &str) -> Result<ParseResult>;
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
    fn parse(&self, source: &str, file_path: &str) -> Result<ParseResult> {
        super::rust::parse(source, file_path)
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
    fn parse(&self, source: &str, file_path: &str) -> Result<ParseResult> {
        super::dart::parse(source, file_path)
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
        fn parse(&self, _source: &str, file_path: &str) -> Result<ParseResult> {
            Ok(ParseResult {
                file_path: file_path.to_string(),
                language: "test".to_string(),
                symbols: vec![],
                references: vec![],
                imports: vec![],
                parsed_ok: true,
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

        let result = adapter.parse("", "foo.tst").unwrap();
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
}
