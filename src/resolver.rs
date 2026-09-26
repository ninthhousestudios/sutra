use std::collections::{HashMap, HashSet};

use crate::db::SymbolEntry;
use crate::parser::dart::TYPE_TRACKING_PREFIX;
use crate::parser::rust::{LOCAL_BINDING_SENTINEL, strip_generic_args};
use crate::parser::{ExtractedImport, ExtractedRef, ExtractedSymbol, RefContextKind, SymbolKind};
use crate::rust_imports::{WorkspaceLayout, file_to_module_segments};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMethod {
    ScopeChain,
    LocalBinding,
    Import,
    GlobalFallback,
    TypeTracking,
    QualifiedPath,
}

impl ResolutionMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScopeChain => "scope_chain",
            Self::LocalBinding => "local_binding",
            Self::Import => "import",
            Self::GlobalFallback => "global_fallback",
            Self::TypeTracking => "type_tracking",
            Self::QualifiedPath => "qualified_path",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub original: ExtractedRef,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
    /// True when resolution was skipped (Import context).
    pub skipped: bool,
    pub resolution_method: Option<ResolutionMethod>,
}

/// Maps parent_symbol_id → list of (member_short_name, member_symbol_id).
/// Borrows short_name from `all_symbols` to avoid allocation.
pub type ClassMembers<'a> = HashMap<i64, Vec<(&'a str, i64)>>;

pub fn build_class_members(all_symbols: &[SymbolEntry]) -> ClassMembers<'_> {
    let mut map: ClassMembers<'_> = HashMap::new();
    for s in all_symbols {
        if let Some(parent_id) = s.parent_symbol_id {
            map.entry(parent_id)
                .or_default()
                .push((s.short_name.as_str(), s.id));
        }
    }
    map
}

/// Keyed views over the all-symbols list, built once per resolution pass.
/// Candidate lists preserve `all_symbols` iteration order so first-match and
/// tie-break semantics are identical to the linear scans they replace.
pub struct SymbolIndex<'a> {
    by_short_name: HashMap<&'a str, Vec<&'a SymbolEntry>>,
    by_qualified_name: HashMap<&'a str, Vec<&'a SymbolEntry>>,
    class_members: ClassMembers<'a>,
    /// Module of each `.rs` file, for resolving `module::item` paths.
    rust_modules: HashMap<i64, RustFile>,
    /// Source root of each workspace crate, by import name.
    crate_srcs: HashMap<String, String>,
    /// Every Rust module path, file or inline, keyed by [`module_key`].
    rust_module_paths: HashSet<String>,
}

/// Where a Rust file sits: its crate's source root and its module path below
/// that root (`src/db/graph.rs` is `["db", "graph"]` under `src`).
struct RustFile {
    src: String,
    segs: Vec<String>,
}

/// Directories whose top-level files are each a crate root of their own
/// (`tests/api.rs`), with shared modules below (`tests/common/mod.rs`).
const TARGET_DIRS: &[&str] = &["tests", "benches", "examples"];

impl RustFile {
    /// A file outside every known crate is placed in an unnamed crate at `src`.
    fn locate(path: &str, layout: &WorkspaceLayout) -> Self {
        let src = layout
            .crate_for_file(path)
            .map_or_else(|| "src".to_string(), |(_, src)| src);
        let crate_dir = src.strip_suffix("src").unwrap_or_default();
        for dir in TARGET_DIRS {
            let root = format!("{crate_dir}{dir}");
            let Some(rest) = path
                .strip_prefix(root.as_str())
                .and_then(|r| r.strip_prefix('/'))
            else {
                continue;
            };
            let segs = if rest.contains('/') {
                file_to_module_segments(path, &root)
            } else {
                Vec::new()
            };
            return Self { src: root, segs };
        }
        let segs = file_to_module_segments(path, &src);
        Self { src, segs }
    }
}

impl<'a> SymbolIndex<'a> {
    pub fn build(all_symbols: &'a [SymbolEntry]) -> Self {
        let mut by_short_name: HashMap<&'a str, Vec<&'a SymbolEntry>> = HashMap::new();
        let mut by_qualified_name: HashMap<&'a str, Vec<&'a SymbolEntry>> = HashMap::new();
        for s in all_symbols {
            by_short_name
                .entry(s.short_name.as_str())
                .or_default()
                .push(s);
            by_qualified_name
                .entry(s.qualified_name.as_str())
                .or_default()
                .push(s);
        }
        Self {
            by_short_name,
            by_qualified_name,
            class_members: build_class_members(all_symbols),
            rust_modules: HashMap::new(),
            crate_srcs: HashMap::new(),
            rust_module_paths: HashSet::new(),
        }
    }

    /// Supply file paths and the Cargo layout so Rust `module::item` paths can
    /// resolve. Without them every path-qualified Rust ref stays unresolved.
    /// A file outside every known crate is placed in an unnamed crate at `src`.
    pub fn with_file_paths<'p>(
        mut self,
        paths: impl IntoIterator<Item = (i64, &'p str)>,
        layout: &WorkspaceLayout,
    ) -> Self {
        self.rust_modules = paths
            .into_iter()
            .filter(|(_, path)| path.ends_with(".rs"))
            .map(|(id, path)| (id, RustFile::locate(path, layout)))
            .collect();
        self.crate_srcs = layout
            .all_crate_names()
            .into_iter()
            .filter_map(|name| Some((name.to_string(), layout.src_prefix_for_crate(name)?)))
            .collect();
        let file_modules = self
            .rust_modules
            .values()
            .flat_map(|f| (0..=f.segs.len()).map(|n| module_key(&f.src, &f.segs[..n].join("::"))));
        let inline_modules = self
            .by_short_name
            .values()
            .flatten()
            .filter(|s| s.kind == "module")
            .filter_map(|s| {
                let src = &self.rust_modules.get(&s.file_id)?.src;
                let container = self.item_container(s, src)?;
                let path = if container.is_empty() {
                    s.short_name.to_string()
                } else {
                    format!("{container}::{}", s.short_name)
                };
                Some(module_key(src, &path))
            });
        self.rust_module_paths = file_modules.chain(inline_modules).collect();
        self
    }

    fn is_module(&self, src: &str, path: &str) -> bool {
        self.rust_module_paths.contains(&module_key(src, path))
    }

    /// The module path containing `s` (its file's module, then any inline
    /// module or type in its qualified name), joined with `::`, or `None` when
    /// `s` is not in the crate rooted at `src`.
    fn item_container(&self, s: &SymbolEntry, src: &str) -> Option<String> {
        let file = self.rust_modules.get(&s.file_id).filter(|f| f.src == src)?;
        let qn = strip_generic_args(&s.qualified_name);
        let parent = qn.rsplit_once("::").map(|(p, _)| p);
        Some(
            file.segs
                .iter()
                .map(String::as_str)
                .chain(parent)
                .collect::<Vec<_>>()
                .join("::"),
        )
    }

    fn short(&self, name: &str) -> &[&'a SymbolEntry] {
        self.by_short_name.get(name).map_or(&[], Vec::as_slice)
    }

    fn qualified(&self, name: &str) -> &[&'a SymbolEntry] {
        self.by_qualified_name.get(name).map_or(&[], Vec::as_slice)
    }
}

pub fn resolve_refs(
    file_symbols: &[ExtractedSymbol],
    refs: &[ExtractedRef],
    index: &SymbolIndex<'_>,
    file_imports: &[ExtractedImport],
    file_id: i64,
    lang: &str,
) -> Vec<ResolvedRef> {
    refs.iter()
        .map(|r| resolve_single(r, file_symbols, index, file_imports, file_id, lang))
        .collect()
}

fn kind_compatible(context: &RefContextKind, symbol_kind: &str) -> bool {
    match context {
        RefContextKind::TypeUse | RefContextKind::Construction => matches!(
            symbol_kind,
            "struct" | "enum" | "trait" | "type_alias" | "class" | "mixin" | "extension"
        ),
        RefContextKind::Call => matches!(symbol_kind, "function" | "method" | "macro"),
        RefContextKind::FieldAccess => matches!(symbol_kind, "field" | "method"),
        _ => true,
    }
}

/// Which symbol kinds a ref may bind to across the resolution steps.
#[derive(Clone, Copy)]
enum KindFilter<'a> {
    Any,
    Context(&'a RefContextKind),
    /// Rust `recv.name()`: method-call syntax can only dispatch to a method,
    /// never to a same-named free function (sutra/433).
    MethodOnly,
    /// Rust value read: a const or static, a fn or method passed as a value,
    /// or a unit struct. Never a field, module or type-only item.
    RustRead,
}

impl KindFilter<'_> {
    fn accepts(self, symbol_kind: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Context(context) => kind_compatible(context, symbol_kind),
            Self::MethodOnly => symbol_kind == "method",
            Self::RustRead => matches!(
                symbol_kind,
                "const" | "static" | "function" | "method" | "struct"
            ),
        }
    }
}

fn resolve_single(
    r: &ExtractedRef,
    file_symbols: &[ExtractedSymbol],
    index: &SymbolIndex<'_>,
    file_imports: &[ExtractedImport],
    file_id: i64,
    lang: &str,
) -> ResolvedRef {
    if matches!(r.context_kind, RefContextKind::Import) {
        return ResolvedRef {
            original: r.clone(),
            target_symbol_id: None,
            unresolved_name: Some(r.name.clone()),
            skipped: true,
            resolution_method: None,
        };
    }

    let name = &r.name;
    let use_kind_filter = matches!(
        r.context_kind,
        RefContextKind::TypeUse
            | RefContextKind::Call
            | RefContextKind::Construction
            | RefContextKind::FieldAccess
    );
    // A call through a receiver (`x.name()`) is a method call. Rust has no
    // other meaning for that syntax, so candidates narrow to methods. Dart's
    // `prefix.fn()` also carries a receiver and the parser does not record
    // import prefixes, so Dart only *prefers* methods in the global fallback.
    let receiver_call = matches!(r.context_kind, RefContextKind::Call) && r.receiver.is_some();
    let filter = if receiver_call && lang == "rust" {
        KindFilter::MethodOnly
    } else if lang == "rust" && matches!(r.context_kind, RefContextKind::Read) {
        KindFilter::RustRead
    } else if use_kind_filter {
        KindFilter::Context(&r.context_kind)
    } else {
        KindFilter::Any
    };

    // A Rust path names where the item lives, so it binds only there. A
    // `Self::` path is left to the impl's scope chain below.
    if lang == "rust"
        && let Some(qualifier) = r.qualifier.as_deref()
        && qualifier.split("::").next() != Some("Self")
    {
        let site = PathSite {
            index,
            file_symbols,
            file_imports,
            file_id,
        };
        return match resolve_rust_path(&site, qualifier, name, r.line, filter) {
            Some(id) => resolved(r, id, ResolutionMethod::QualifiedPath),
            None => unresolved(r),
        };
    }

    // --- Step 0: scope-chain / type-tracking hints from parse-time resolution ---
    if let Some(hint) = &r.resolved_local_target {
        if hint == LOCAL_BINDING_SENTINEL {
            return ResolvedRef {
                original: r.clone(),
                target_symbol_id: None,
                unresolved_name: Some(name.clone()),
                skipped: false,
                resolution_method: Some(ResolutionMethod::LocalBinding),
            };
        }

        // Type-tracking hint: "::type_tracking::ClassName" — look up ClassName.{name}
        // in the class_members map built from parent_symbol_id chains.
        if let Some(class_name) = hint.strip_prefix(TYPE_TRACKING_PREFIX) {
            let class_syms = index
                .short(class_name)
                .iter()
                .filter(|s| matches!(s.kind.as_str(), "class" | "mixin" | "extension" | "struct"));
            for class_sym in class_syms {
                if let Some(members) = index.class_members.get(&class_sym.id)
                    && let Some(&(_, member_id)) = members.iter().find(|(n, _)| *n == name.as_str())
                {
                    return resolved(r, member_id, ResolutionMethod::TypeTracking);
                }
            }
            // Type found but member not in DB — fall through to standard resolution.
        }

        // The scope chain matches by name alone, and an `impl Foo` block is in
        // scope under `Foo`: a type use must still bind to the type.
        if let Some(s) = index
            .qualified(hint)
            .iter()
            .find(|s| s.file_id == file_id && filter.accepts(&s.kind))
        {
            return resolved(r, s.id, ResolutionMethod::ScopeChain);
        }
    }

    // --- Step 1: local scope (file_symbols by short_name) ---
    let local_matches: Vec<&ExtractedSymbol> = file_symbols
        .iter()
        .filter(|s| s.short_name == *name && filter.accepts(s.kind.as_str()))
        .collect();

    if let Some(best) = pick_nearest_local(&local_matches, r.line, file_symbols) {
        if let Some(s) = index
            .qualified(&best.qualified_name)
            .iter()
            .find(|s| s.file_id == file_id)
        {
            return resolved(r, s.id, ResolutionMethod::ScopeChain);
        }
        if let Some(s) = index
            .short(&best.short_name)
            .iter()
            .find(|s| s.file_id == file_id)
        {
            return resolved(r, s.id, ResolutionMethod::ScopeChain);
        }
    }

    // --- Step 2: import-filtered match ---
    let mut visited: HashSet<&str> = HashSet::new();
    if let Some(id) = find_via_imports(name, filter, index, file_imports, &mut visited) {
        return resolved(r, id, ResolutionMethod::Import);
    }

    // --- Step 3: global match (all_symbols by short_name) ---
    let mut global_matches: Vec<&SymbolEntry> = index
        .short(name)
        .iter()
        .filter(|s| filter.accepts(&s.kind))
        .copied()
        .collect();
    // Receiver calls prefer methods: otherwise the shortest-qualified-name
    // tie-break below favours unqualified free functions over `Type::name`.
    if receiver_call && global_matches.iter().any(|s| s.kind == "method") {
        global_matches.retain(|s| s.kind == "method");
    }

    if global_matches.len() == 1 {
        return resolved(r, global_matches[0].id, ResolutionMethod::GlobalFallback);
    }

    if global_matches.len() > 1 {
        let same_file: Vec<&&SymbolEntry> = global_matches
            .iter()
            .filter(|s| s.file_id == file_id)
            .collect();
        if same_file.len() == 1 {
            return resolved(r, same_file[0].id, ResolutionMethod::GlobalFallback);
        }

        let pool = if same_file.len() > 1 {
            same_file.into_iter().copied().collect::<Vec<_>>()
        } else {
            global_matches.clone()
        };
        let best = pool.iter().min_by_key(|s| s.qualified_name.len()).unwrap();
        return resolved(r, best.id, ResolutionMethod::GlobalFallback);
    }

    // --- Step 3b: Python class-as-constructor (language-scoped) ---
    // `ClassName(...)` is emitted as a Call ref but constructs an instance.
    // `kind_compatible(Call, "class")` is false, so steps 1-3 (which match only
    // function|method|macro) never bind a class. Reaching here means no
    // function/method candidate resolved — so a same-named function still wins
    // (it would have returned above). Retry the name search as Construction,
    // which binds to `class` via `kind_compatible`, but keep the ref's Call
    // context: `sutra_impact` counts direct callers by `context_kind == "call"`
    // (impact.rs), so re-tagging Construction would *exclude* the site from the
    // caller count — the opposite of the goal. Python-only: Rust/Dart/TS
    // resolution is unchanged, and a dynamic `factory()` base stays unresolved
    // (no class symbol of that name exists to bind).
    if lang == "python" && matches!(r.context_kind, RefContextKind::Call) {
        let as_construction = ExtractedRef {
            context_kind: RefContextKind::Construction,
            ..r.clone()
        };
        let retry = resolve_single(
            &as_construction,
            file_symbols,
            index,
            file_imports,
            file_id,
            lang,
        );
        if let Some(id) = retry.target_symbol_id {
            let method = retry
                .resolution_method
                .unwrap_or(ResolutionMethod::GlobalFallback);
            return resolved(r, id, method);
        }
    }

    // Kind-agnostic last resort; a Rust receiver call stays method-only.
    if use_kind_filter && !matches!(filter, KindFilter::MethodOnly) {
        let fallback = index.short(name);
        if fallback.len() == 1 {
            return resolved(r, fallback[0].id, ResolutionMethod::GlobalFallback);
        }
    }

    unresolved(r)
}

fn unresolved(r: &ExtractedRef) -> ResolvedRef {
    ResolvedRef {
        original: r.clone(),
        target_symbol_id: None,
        unresolved_name: Some(r.name.clone()),
        skipped: false,
        resolution_method: None,
    }
}

/// The file a Rust path is written in.
struct PathSite<'a, 'i> {
    index: &'a SymbolIndex<'i>,
    file_symbols: &'a [ExtractedSymbol],
    file_imports: &'a [ExtractedImport],
    file_id: i64,
}

/// A module or type path from a crate's source root.
struct RustPath<'a> {
    src: &'a str,
    segs: Vec<&'a str>,
    /// Rooted in the workspace by `crate`/`self`/`super`, a crate name or an
    /// import, rather than guessed relative to the current module.
    anchored: bool,
}

impl RustPath<'_> {
    fn joined(&self) -> String {
        self.segs.join("::")
    }
}

/// How a path's first segment roots it.
enum Rooted<'a> {
    /// `crate`, `self`, `super` or a workspace crate name.
    Absolute(RustPath<'a>),
    /// `std`, `core`, `alloc`, `::x`, or `super` above the crate root.
    External,
    /// Anything else: an item or import of the current module, or a crate the
    /// workspace does not define.
    Relative,
}

/// Resolve the Rust path `qualifier::name` written on `line`, or `None` when it
/// leaves the workspace or reaches nothing. The qualifier is made absolute —
/// from `crate`/`self`/`super`, a crate name, a `use` import, or else the
/// current module and its glob imports — and the name is looked up only
/// there, never by name alone: `std::fs::read` must not bind a workspace
/// `fs::read`, nor `a::calls::handle` the `handle` of `b::calls`.
fn resolve_rust_path(
    site: &PathSite<'_, '_>,
    qualifier: &str,
    name: &str,
    line: usize,
    filter: KindFilter<'_>,
) -> Option<i64> {
    let file = site.index.rust_modules.get(&site.file_id)?;
    let crates = &site.index.crate_srcs;
    let qualifier = strip_generic_args(qualifier);
    let segs: Vec<&str> = qualifier.split("::").collect();
    let here = module_at(file, site.file_symbols, line);
    let mut targets = Vec::new();
    match root_path(&segs, &here, crates) {
        Rooted::Absolute(path) => targets.push(path),
        Rooted::External => return None,
        Rooted::Relative => {
            let imports: Vec<(&str, Option<&str>, usize)> = site
                .file_imports
                .iter()
                .map(|i| (i.raw_path.as_str(), i.alias.as_deref(), i.line))
                .collect();
            let named = imports.iter().find(|(path, alias, _)| {
                !path.ends_with("::*")
                    && alias
                        .or_else(|| path.rsplit("::").next())
                        .is_some_and(|n| n == segs[0])
            });
            if let Some(&(path, _, import_line)) = named {
                let import_site = module_at(file, site.file_symbols, import_line);
                let full: Vec<&str> = path.split("::").chain(segs[1..].iter().copied()).collect();
                targets.extend(import_target(full, import_site, crates));
            } else {
                targets.push(RustPath {
                    src: here.src,
                    segs: here.segs.iter().chain(&segs).copied().collect(),
                    anchored: false,
                });
                for &(path, _, import_line) in &imports {
                    let Some(prefix) = path.strip_suffix("::*") else {
                        continue;
                    };
                    let import_site = module_at(file, site.file_symbols, import_line);
                    let full: Vec<&str> = prefix.split("::").chain(segs.iter().copied()).collect();
                    targets.extend(import_target(full, import_site, crates));
                }
            }
        }
    }
    targets
        .iter()
        .find_map(|t| find_item(site.index, t, name, filter, site.file_id))
        .map(|s| s.id)
}

/// Where an imported path points. A `use child::item` path that no crate
/// root anchors is relative to the importing module.
fn import_target<'a>(
    path: Vec<&'a str>,
    site: RustPath<'a>,
    crates: &'a HashMap<String, String>,
) -> Option<RustPath<'a>> {
    match root_path(&path, &site, crates) {
        Rooted::Absolute(target) => Some(target),
        Rooted::External => None,
        Rooted::Relative => Some(RustPath {
            src: site.src,
            segs: site.segs.into_iter().chain(path).collect(),
            anchored: false,
        }),
    }
}

fn root_path<'a>(
    segs: &[&'a str],
    here: &RustPath<'a>,
    crates: &'a HashMap<String, String>,
) -> Rooted<'a> {
    let Some((&first, rest)) = segs.split_first() else {
        return Rooted::External;
    };
    let from = |src: &'a str, base: &[&'a str], rest: &[&'a str]| {
        Rooted::Absolute(RustPath {
            src,
            segs: base.iter().chain(rest).copied().collect(),
            anchored: true,
        })
    };
    match first {
        "" | "std" | "core" | "alloc" => Rooted::External,
        "crate" => from(here.src, &[], rest),
        "self" => from(here.src, &here.segs, rest),
        "super" => {
            let ups = segs.iter().take_while(|s| **s == "super").count();
            match here.segs.len().checked_sub(ups) {
                Some(keep) => from(here.src, &here.segs[..keep], &segs[ups..]),
                None => Rooted::External,
            }
        }
        crate_name => match crates.get(crate_name) {
            Some(src) => from(src, &[], rest),
            None => Rooted::Relative,
        },
    }
}

/// The module enclosing `line`: the file's module, extended by the innermost
/// inline `mod name { … }` around the line (`mod tests` in `src/a.rs` is
/// `a::tests`).
fn module_at<'a>(
    file: &'a RustFile,
    file_symbols: &'a [ExtractedSymbol],
    line: usize,
) -> RustPath<'a> {
    let inline = file_symbols
        .iter()
        .filter(|s| {
            s.kind == SymbolKind::Module
                && s.start_line < s.end_line
                && (s.start_line..=s.end_line).contains(&line)
        })
        .min_by_key(|s| s.end_line - s.start_line);
    RustPath {
        src: &file.src,
        segs: file
            .segs
            .iter()
            .map(String::as_str)
            .chain(
                inline
                    .into_iter()
                    .flat_map(|s| s.qualified_name.split("::")),
            )
            .collect(),
        anchored: true,
    }
}

/// The item `name` in `path`: directly there; else re-exported from a module
/// below it (`pub use child::*`, and a path to a child item that is not
/// re-exported would not compile); else, when `path` ends in a type, a member
/// of that type, whose `impl` may sit in any module of the crate.
fn find_item<'i>(
    index: &SymbolIndex<'i>,
    path: &RustPath<'_>,
    name: &str,
    filter: KindFilter<'_>,
    file_id: i64,
) -> Option<&'i SymbolEntry> {
    let candidates: Vec<(&'i SymbolEntry, String)> = index
        .short(name)
        .iter()
        .filter(|s| filter.accepts(&s.kind))
        .filter_map(|s| index.item_container(s, path.src).map(|c| (*s, c)))
        .collect();
    let pick = |matches: Vec<&(&'i SymbolEntry, String)>| {
        matches
            .iter()
            .find(|(s, _)| s.file_id == file_id)
            .or_else(|| matches.first())
            .map(|(s, _)| *s)
    };
    let target = path.joined();
    let direct: Vec<_> = candidates.iter().filter(|(_, c)| *c == target).collect();
    if !direct.is_empty() {
        return pick(direct);
    }

    // A `pub use child::*` or `pub use child::name` re-export: an item of a
    // direct child module. Deeper items are reached only through a chain of
    // re-exports, and taking them binds unrelated items (a `tests` helper).
    let child_of_target = |c: &str| {
        let rest = if target.is_empty() {
            Some(c)
        } else {
            c.strip_prefix(target.as_str())
                .and_then(|r| r.strip_prefix("::"))
        };
        rest.is_some_and(|r| !r.contains("::") && is_module_segment(r))
    };
    let reexported: Vec<_> = candidates
        .iter()
        .filter(|(_, c)| child_of_target(c))
        .collect();
    if !reexported.is_empty() {
        return pick(reexported);
    }

    let (&last, parent) = path.segs.split_last()?;
    // An anchored path through a module that does not exist there reaches
    // one re-exported under that name from elsewhere in the crate
    // (`pub use routes::shop::catalog;` in lib.rs makes `crate::catalog`).
    if is_module_segment(last) {
        if !path.anchored || index.is_module(path.src, &target) {
            return None;
        }
        let named: Vec<_> = candidates
            .iter()
            .filter(|(_, c)| {
                c.rsplit("::").next() == Some(last) && c.split("::").all(is_module_segment)
            })
            .collect();
        return pick(named);
    }
    let ty = last;
    let parent = RustPath {
        src: path.src,
        segs: parent.to_vec(),
        anchored: path.anchored,
    };
    find_item(
        index,
        &parent,
        ty,
        KindFilter::Context(&RefContextKind::TypeUse),
        file_id,
    )?;
    // An inherent `impl` sits in the type's crate, but a trait `impl` may sit
    // in any workspace crate (`impl TryFrom<Row> for chat_store::Message` in
    // the server), so members are taken from anywhere, the type's crate first.
    let members: Vec<(&'i SymbolEntry, bool)> = index
        .short(name)
        .iter()
        .filter(|s| filter.accepts(&s.kind))
        .filter_map(|s| {
            let file = index.rust_modules.get(&s.file_id)?;
            let qn = strip_generic_args(&s.qualified_name);
            let (owner, _) = qn.rsplit_once("::")?;
            (owner.rsplit("::").next() == Some(ty)).then_some((*s, file.src == path.src))
        })
        .collect();
    members
        .iter()
        .find(|(s, home)| *home && s.file_id == file_id)
        .or_else(|| members.iter().find(|(_, home)| *home))
        .or_else(|| members.iter().find(|(s, _)| s.file_id == file_id))
        .or_else(|| members.first())
        .map(|(s, _)| *s)
}

fn module_key(src: &str, path: &str) -> String {
    format!("{src}|{path}")
}

/// A module name segment (`db`, `r#type`), as opposed to a type (`Config`).
fn is_module_segment(seg: &str) -> bool {
    !seg.starts_with(char::is_uppercase)
}

fn resolved(r: &ExtractedRef, id: i64, method: ResolutionMethod) -> ResolvedRef {
    ResolvedRef {
        original: r.clone(),
        target_symbol_id: Some(id),
        unresolved_name: None,
        skipped: false,
        resolution_method: Some(method),
    }
}

fn pick_nearest_local<'a>(
    candidates: &[&'a ExtractedSymbol],
    ref_line: usize,
    file_symbols: &[ExtractedSymbol],
) -> Option<&'a ExtractedSymbol> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    candidates
        .iter()
        .min_by(|a, b| {
            let scope_a = tightest_common_scope_size(a, ref_line, file_symbols);
            let scope_b = tightest_common_scope_size(b, ref_line, file_symbols);
            scope_a
                .cmp(&scope_b)
                .then_with(|| line_proximity_key(a, ref_line).cmp(&line_proximity_key(b, ref_line)))
        })
        .copied()
}

fn tightest_common_scope_size(
    candidate: &ExtractedSymbol,
    ref_line: usize,
    file_symbols: &[ExtractedSymbol],
) -> usize {
    file_symbols
        .iter()
        .filter(|s| {
            s.start_line <= ref_line
                && s.end_line >= ref_line
                && s.start_line <= candidate.start_line
                && s.end_line >= candidate.end_line
                && s.qualified_name != candidate.qualified_name
        })
        .map(|s| s.end_line - s.start_line)
        .min()
        .unwrap_or(usize::MAX)
}

fn line_proximity_key(sym: &ExtractedSymbol, ref_line: usize) -> (bool, usize) {
    if sym.start_line <= ref_line {
        (false, ref_line - sym.start_line)
    } else {
        (true, sym.start_line - ref_line)
    }
}

fn split_import_path(path: &str) -> Vec<&str> {
    if path.contains("::") {
        path.split("::").collect()
    } else {
        path.split('.').collect()
    }
}

fn rfind_separator(path: &str) -> Option<(&str, usize)> {
    if let Some(pos) = path.rfind("::") {
        Some((&path[..pos], 2))
    } else {
        path.rfind('.').map(|pos| (&path[..pos], 1))
    }
}

fn find_via_imports(
    name: &str,
    filter: KindFilter<'_>,
    index: &SymbolIndex<'_>,
    file_imports: &[ExtractedImport],
    visited: &mut HashSet<&str>,
) -> Option<i64> {
    for imp in file_imports {
        if visited.contains(imp.raw_path.as_str()) {
            continue;
        }

        let alias_match = imp.alias.as_deref().is_some_and(|alias| alias == name);

        let path = &imp.raw_path;
        let segments = split_import_path(path);
        let last_segment = segments.last().copied().unwrap_or("");

        if !alias_match && last_segment != name && !segments.contains(&name) {
            continue;
        }

        // First qualified-name match is checked for kind compatibility but not
        // searched past — mirrors the original scan's let-chain semantics.
        if let Some(s) = index.qualified(path).first()
            && filter.accepts(&s.kind)
        {
            return Some(s.id);
        }

        let import_prefix = rfind_separator(path)
            .map(|(prefix, _)| prefix)
            .unwrap_or(path.as_str());

        if let Some(s) = index
            .short(name)
            .iter()
            .find(|s| s.qualified_name.starts_with(import_prefix) && filter.accepts(&s.kind))
        {
            return Some(s.id);
        }

        if (last_segment == name || alias_match)
            && let Some(s) = index.short(name).iter().find(|s| filter.accepts(&s.kind))
        {
            return Some(s.id);
        }

        if alias_match
            && let Some(s) = index
                .short(last_segment)
                .iter()
                .find(|s| filter.accepts(&s.kind))
        {
            return Some(s.id);
        }
    }

    None
}
