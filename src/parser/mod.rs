pub mod adapter;
pub mod c;
pub mod complexity;
pub mod dart;
pub mod javascript;
pub mod python;
pub mod rust;
pub mod structural_hash;
pub mod typescript;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct ParseResult {
    pub file_path: String,
    pub language: String,
    pub symbols: Vec<ExtractedSymbol>,
    pub references: Vec<ExtractedRef>,
    pub imports: Vec<ExtractedImport>,
    pub parsed_ok: bool,
    pub line_count: usize,
}

#[derive(Debug, Clone)]
pub struct ExtractedSymbol {
    pub qualified_name: String,
    pub short_name: String,
    pub kind: SymbolKind,
    pub signature: Option<String>,
    pub signature_hash: Option<String>,
    pub structural_hash: Option<String>,
    pub visibility: Option<String>,
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub children: Vec<ExtractedSymbol>,
    pub parent_symbol_id: Option<i64>,
    pub docstring: Option<String>,
    pub cyclomatic: Option<u32>,
    pub cognitive: Option<u32>,
    pub max_nesting: Option<u32>,
    pub flags: u32,
    pub language_attrs: Option<String>,
}

pub fn flatten_symbols(tree: &[ExtractedSymbol]) -> Vec<&ExtractedSymbol> {
    let mut out = Vec::new();
    fn walk<'a>(symbols: &'a [ExtractedSymbol], out: &mut Vec<&'a ExtractedSymbol>) {
        for sym in symbols {
            out.push(sym);
            walk(&sym.children, out);
        }
    }
    walk(tree, &mut out);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Static,
    TypeAlias,
    Macro,
    Class,
    Mixin,
    Extension,
    Field,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "module",
            Self::Const => "const",
            Self::Static => "static",
            Self::TypeAlias => "type_alias",
            Self::Macro => "macro",
            Self::Class => "class",
            Self::Mixin => "mixin",
            Self::Extension => "extension",
            Self::Field => "field",
        }
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SymbolKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "function" => Ok(Self::Function),
            "method" => Ok(Self::Method),
            "struct" => Ok(Self::Struct),
            "enum" => Ok(Self::Enum),
            "trait" => Ok(Self::Trait),
            "impl" => Ok(Self::Impl),
            "module" => Ok(Self::Module),
            "const" => Ok(Self::Const),
            "static" => Ok(Self::Static),
            "type_alias" => Ok(Self::TypeAlias),
            "macro" => Ok(Self::Macro),
            "class" => Ok(Self::Class),
            "mixin" => Ok(Self::Mixin),
            "extension" => Ok(Self::Extension),
            "field" => Ok(Self::Field),
            _ => Err(format!("unknown symbol kind: {s}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractedRef {
    pub name: String,
    pub line: usize,
    pub col: usize,
    pub context_kind: RefContextKind,
    /// Hint from parse-time scope resolution: qualified name of the local target.
    /// The resolver uses this to short-circuit the local lookup step.
    pub resolved_local_target: Option<String>,
    /// Receiver identifier for method-call / field-access refs (e.g. `x` in `x.method()`).
    /// Captured at parse time; used by type-tracking resolution (Phase B).
    pub receiver: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefContextKind {
    Call,
    Construction,
    TypeUse,
    Import,
    FieldAccess,
    PatternBind,
    /// A bare value read of an identifier: a const/variable read or a
    /// function/method tear-off passed as a value (e.g. `onPressed: _openChart`).
    /// Distinct from `Call` (invocation) and `Other` (uninteresting position);
    /// kind-agnostic in resolution so it can bind to any symbol kind.
    Read,
    Other,
}

impl RefContextKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Construction => "construction",
            Self::TypeUse => "type_use",
            Self::Import => "import",
            Self::FieldAccess => "field_access",
            Self::PatternBind => "pattern_bind",
            Self::Read => "read",
            Self::Other => "other",
        }
    }
}

impl std::str::FromStr for RefContextKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "call" => Ok(Self::Call),
            "construction" => Ok(Self::Construction),
            "type_use" => Ok(Self::TypeUse),
            "import" => Ok(Self::Import),
            "field_access" => Ok(Self::FieldAccess),
            "pattern_bind" => Ok(Self::PatternBind),
            "read" => Ok(Self::Read),
            "other" => Ok(Self::Other),
            _ => Err(format!("unknown ref context kind: {s}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractedImport {
    pub raw_path: String,
    pub line: usize,
    pub kind: &'static str,
    /// Local alias (e.g. `np` for `import numpy as np`). None when unaliased.
    pub alias: Option<String>,
    /// Import sits in test-only code — either an attribute-gated region
    /// (Rust `#[cfg(test)]`) or a whole-file test target (Rust `tests/`, Dart
    /// `test/`). Such edges are excluded from constraint evaluation by default
    /// — see `include_tests`.
    pub is_test: bool,
}

pub fn parse_file(source: &str, language: &str, file_path: &str) -> Result<ParseResult> {
    let registry = adapter::default_registry();
    match registry.adapter_for_language(language) {
        Some(a) => {
            let mut pool = adapter::ParserPool::new(std::time::Duration::from_secs(5));
            pool.parse_with(a, source, file_path)
        }
        None => Ok(ParseResult {
            file_path: file_path.to_string(),
            language: language.to_string(),
            symbols: vec![],
            references: vec![],
            imports: vec![],
            parsed_ok: false,
            line_count: source.lines().count(),
        }),
    }
}
