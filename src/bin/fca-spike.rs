use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use sutra::db::Db;

const SEP: &str = "============================================================";
const MIN_SUPPORT: usize = 3;

// -------------------------------------------------------------------------
// Bitset — compact attribute/object sets over a fixed universe
// -------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Hash)]
struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    fn new(len: usize) -> Self {
        let nwords = (len + 63) / 64;
        Self { words: vec![0; nwords], len }
    }

    fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1u64 << (i % 64);
    }

    fn clear(&mut self, i: usize) {
        self.words[i / 64] &= !(1u64 << (i % 64));
    }

    fn contains(&self, i: usize) -> bool {
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    fn intersect(&self, other: &Self) -> Self {
        let mut r = self.clone();
        for (w, o) in r.words.iter_mut().zip(&other.words) {
            *w &= *o;
        }
        r
    }

    fn union(&self, other: &Self) -> Self {
        let mut r = self.clone();
        for (w, o) in r.words.iter_mut().zip(&other.words) {
            *w |= *o;
        }
        r
    }

    fn is_subset_of(&self, other: &Self) -> bool {
        self.words.iter().zip(&other.words).all(|(a, b)| *a & *b == *a)
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len).filter(|&i| self.contains(i))
    }

    fn from_indices(len: usize, indices: impl IntoIterator<Item = usize>) -> Self {
        let mut bs = Self::new(len);
        for i in indices {
            bs.set(i);
        }
        bs
    }
}

impl fmt::Debug for BitSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        let mut first = true;
        for i in self.iter() {
            if !first { write!(f, ", ")?; }
            write!(f, "{i}")?;
            first = false;
        }
        write!(f, "}}")
    }
}

// -------------------------------------------------------------------------
// Formal Context
// -------------------------------------------------------------------------

struct FormalContext {
    object_names: Vec<String>,
    attribute_names: Vec<String>,
    object_attrs: Vec<BitSet>,   // object → set of attributes
    attr_objects: Vec<BitSet>,   // attribute → set of objects
    n_objects: usize,
    n_attrs: usize,
}

impl FormalContext {
    fn new(object_names: Vec<String>, attribute_names: Vec<String>, relations: Vec<(usize, usize)>) -> Self {
        let n_objects = object_names.len();
        let n_attrs = attribute_names.len();
        let mut object_attrs: Vec<BitSet> = (0..n_objects).map(|_| BitSet::new(n_attrs)).collect();
        let mut attr_objects: Vec<BitSet> = (0..n_attrs).map(|_| BitSet::new(n_objects)).collect();

        for (g, m) in relations {
            object_attrs[g].set(m);
            attr_objects[m].set(g);
        }

        Self { object_names, attribute_names, object_attrs, attr_objects, n_objects, n_attrs }
    }

    fn density(&self) -> f64 {
        if self.n_objects == 0 || self.n_attrs == 0 { return 0.0; }
        let total: usize = self.object_attrs.iter().map(|bs| bs.count()).sum();
        total as f64 / (self.n_objects * self.n_attrs) as f64
    }

    // A' — attributes common to all objects in A
    fn object_prime(&self, objects: &BitSet) -> BitSet {
        if objects.is_empty() {
            return BitSet::from_indices(self.n_attrs, 0..self.n_attrs);
        }
        let mut result = BitSet::from_indices(self.n_attrs, 0..self.n_attrs);
        for g in objects.iter() {
            result = result.intersect(&self.object_attrs[g]);
        }
        result
    }

    // B' — objects having all attributes in B
    fn attr_prime(&self, attrs: &BitSet) -> BitSet {
        if attrs.is_empty() {
            return BitSet::from_indices(self.n_objects, 0..self.n_objects);
        }
        let mut result = BitSet::from_indices(self.n_objects, 0..self.n_objects);
        for m in attrs.iter() {
            result = result.intersect(&self.attr_objects[m]);
        }
        result
    }

    // B'' — closure of an attribute set
    fn closure(&self, attrs: &BitSet) -> BitSet {
        self.object_prime(&self.attr_prime(attrs))
    }
}

// -------------------------------------------------------------------------
// FCA Algorithms
// -------------------------------------------------------------------------

struct Concept {
    extent: BitSet,
    intent: BitSet,
}

fn all_concepts(ctx: &FormalContext) -> Vec<Concept> {
    let mut concepts = Vec::new();

    // Start with closure of empty set
    let mut current = ctx.closure(&BitSet::new(ctx.n_attrs));
    let extent = ctx.attr_prime(&current);
    concepts.push(Concept { extent, intent: current.clone() });

    while let Some(next) = next_closure(ctx, &current) {
        let extent = ctx.attr_prime(&next);
        concepts.push(Concept { extent, intent: next.clone() });
        current = next;
    }

    concepts
}

fn next_closure(ctx: &FormalContext, current: &BitSet) -> Option<BitSet> {
    for i in (0..ctx.n_attrs).rev() {
        if current.contains(i) {
            continue;
        }
        // B = (current ∩ {0..i-1}) ∪ {i}
        let mut candidate = BitSet::new(ctx.n_attrs);
        for j in 0..i {
            if current.contains(j) {
                candidate.set(j);
            }
        }
        candidate.set(i);

        let closed = ctx.closure(&candidate);

        // Check: closed ∩ {0..i-1} == candidate ∩ {0..i-1}
        let mut valid = true;
        for j in 0..i {
            if closed.contains(j) != candidate.contains(j) {
                valid = false;
                break;
            }
        }
        if valid {
            return Some(closed);
        }
    }
    None
}

// -------------------------------------------------------------------------
// Implication extraction
// -------------------------------------------------------------------------

struct Implication {
    premise: BTreeSet<usize>,
    conclusion: BTreeSet<usize>,
    support: usize,
    confidence: f64,
}

fn extract_exact_implications(ctx: &FormalContext, min_support: usize) -> Vec<Implication> {
    let mut implications = Vec::new();

    // For each single attribute a, compute closure({a}).
    // If closure({a}) contains extra attributes, that's an implication.
    for a in 0..ctx.n_attrs {
        let mut single = BitSet::new(ctx.n_attrs);
        single.set(a);
        let extent = ctx.attr_prime(&single);
        let support = extent.count();
        if support < min_support { continue; }

        let closed = ctx.object_prime(&extent);
        let conclusion_bits: BTreeSet<usize> = closed.iter().filter(|&m| m != a).collect();
        if conclusion_bits.is_empty() { continue; }

        implications.push(Implication {
            premise: [a].into(),
            conclusion: conclusion_bits,
            support,
            confidence: 1.0,
        });
    }

    // For each pair of attributes (a, b), compute closure({a, b}).
    for a in 0..ctx.n_attrs {
        for b in (a + 1)..ctx.n_attrs {
            let mut pair = BitSet::new(ctx.n_attrs);
            pair.set(a);
            pair.set(b);
            let extent = ctx.attr_prime(&pair);
            let support = extent.count();
            if support < min_support { continue; }

            let closed = ctx.object_prime(&extent);
            let conclusion_bits: BTreeSet<usize> =
                closed.iter().filter(|&m| m != a && m != b).collect();
            if conclusion_bits.is_empty() { continue; }

            // Skip if this is already covered by a single-attribute implication
            let single_a_closed = ctx.closure(&BitSet::from_indices(ctx.n_attrs, [a]));
            let single_b_closed = ctx.closure(&BitSet::from_indices(ctx.n_attrs, [b]));
            let combined_singles = single_a_closed.union(&single_b_closed);
            if closed.is_subset_of(&combined_singles) { continue; }

            implications.push(Implication {
                premise: [a, b].into(),
                conclusion: conclusion_bits,
                support,
                confidence: 1.0,
            });
        }
    }

    implications.sort_by(|a, b| b.support.cmp(&a.support));
    implications
}

fn extract_approximate_implications(ctx: &FormalContext, min_support: usize, min_confidence: f64) -> Vec<Implication> {
    let mut implications = Vec::new();

    for a in 0..ctx.n_attrs {
        let support_a = ctx.attr_objects[a].count();
        if support_a < min_support { continue; }

        for b in 0..ctx.n_attrs {
            if a == b { continue; }
            let both = ctx.attr_objects[a].intersect(&ctx.attr_objects[b]);
            let support_ab = both.count();
            let confidence = support_ab as f64 / support_a as f64;

            if confidence >= min_confidence && confidence < 1.0 && support_ab >= min_support {
                implications.push(Implication {
                    premise: [a].into(),
                    conclusion: [b].into(),
                    support: support_ab,
                    confidence,
                });
            }
        }
    }

    implications.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap()
        .then(b.support.cmp(&a.support)));
    implications
}

// -------------------------------------------------------------------------
// Data loading
// -------------------------------------------------------------------------

struct SutraSnapshot {
    files: Vec<(i64, String)>,
    symbols: Vec<SymbolInfo>,
    import_edges: Vec<(i64, i64)>,
    imports_raw: Vec<(i64, String)>,
    recently_changed: HashSet<String>,
}

struct SymbolInfo {
    id: i64,
    file_id: i64,
    qualified_name: String,
    short_name: String,
    kind: String,
    signature: Option<String>,
    visibility: Option<String>,
    docstring: Option<String>,
    cyclomatic: Option<i64>,
    cognitive: Option<i64>,
    flags: i64,
}

fn load_snapshot(db: &Db, workspace_root: &Path) -> SutraSnapshot {
    let all_files = db.all_files().unwrap();
    let files: Vec<(i64, String)> = all_files.iter().map(|f| (f.id, f.path.clone())).collect();

    let syms_raw = db.all_symbols_summary().unwrap();
    let mut symbols = Vec::new();
    for (id, qname, short, kind) in syms_raw {
        let row = db.symbol_by_id(id).unwrap();
        if let Some(r) = row {
            symbols.push(SymbolInfo {
                id: r.id,
                file_id: r.file_id,
                qualified_name: qname,
                short_name: short,
                kind,
                signature: r.signature,
                visibility: r.visibility,
                docstring: r.docstring,
                cyclomatic: r.cyclomatic,
                cognitive: r.cognitive,
                flags: r.flags,
            });
        }
    }

    let import_edges = db.import_edges().unwrap();

    let mut imports_raw = Vec::new();
    for &(fid, _) in &files {
        if let Ok(imps) = db.imports_for_file(fid) {
            for imp in imps {
                imports_raw.push((fid, imp.imported_path));
            }
        }
    }

    let recently_changed = load_recently_changed(workspace_root, 30);

    SutraSnapshot { files, symbols, import_edges, imports_raw, recently_changed }
}

fn load_recently_changed(workspace_root: &Path, days: u32) -> HashSet<String> {
    let output = Command::new("git")
        .arg("-C").arg(workspace_root)
        .args(["log", "--name-only", "--pretty=format:", "--since", &format!("{days} days ago")])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .collect()
        }
        _ => HashSet::new(),
    }
}

// -------------------------------------------------------------------------
// Attribute extraction — file level
// -------------------------------------------------------------------------

fn build_file_context(snap: &SutraSnapshot) -> FormalContext {
    let file_ids: Vec<i64> = snap.files.iter().map(|(id, _)| *id).collect();
    let _file_id_to_idx: HashMap<i64, usize> = file_ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let object_names: Vec<String> = snap.files.iter().map(|(_, p)| p.clone()).collect();

    let mut attr_map: HashMap<String, usize> = HashMap::new();
    let mut relations: Vec<(usize, usize)> = Vec::new();

    let ensure_attr = |name: &str, map: &mut HashMap<String, usize>| -> usize {
        let len = map.len();
        *map.entry(name.to_string()).or_insert(len)
    };

    // Precompute per-file symbol stats
    let mut file_test_count: HashMap<i64, usize> = HashMap::new();
    let mut file_pub_count: HashMap<i64, usize> = HashMap::new();
    let mut file_doc_count: HashMap<i64, usize> = HashMap::new();
    let mut file_max_cognitive: HashMap<i64, i64> = HashMap::new();
    let mut file_has_pub_struct: HashSet<i64> = HashSet::new();
    let mut file_has_pub_fn: HashSet<i64> = HashSet::new();
    let mut file_has_impl: HashSet<i64> = HashSet::new();
    let mut file_sym_kinds: HashMap<i64, HashSet<String>> = HashMap::new();

    for sym in &snap.symbols {
        if sym.flags & 0x03 != 0 {
            *file_test_count.entry(sym.file_id).or_default() += 1;
        }
        if sym.visibility.as_deref() == Some("pub") {
            *file_pub_count.entry(sym.file_id).or_default() += 1;
            if sym.docstring.is_some() {
                *file_doc_count.entry(sym.file_id).or_default() += 1;
            }
        }
        if let Some(cog) = sym.cognitive {
            let cur = file_max_cognitive.entry(sym.file_id).or_insert(0);
            if cog > *cur { *cur = cog; }
        }
        if sym.visibility.as_deref() == Some("pub") && sym.kind == "struct" {
            file_has_pub_struct.insert(sym.file_id);
        }
        if sym.visibility.as_deref() == Some("pub") && (sym.kind == "function" || sym.kind == "method") {
            file_has_pub_fn.insert(sym.file_id);
        }
        if sym.kind == "impl" {
            file_has_impl.insert(sym.file_id);
        }
        file_sym_kinds.entry(sym.file_id).or_default().insert(sym.kind.clone());
    }

    // Compute fan-in quartiles
    let mut fan_ins: Vec<usize> = Vec::new();
    let import_target_count: HashMap<i64, usize> = {
        let mut m = HashMap::new();
        for &(_, target) in &snap.import_edges {
            *m.entry(target).or_default() += 1;
        }
        m
    };
    for &(fid, _) in &snap.files {
        fan_ins.push(*import_target_count.get(&fid).unwrap_or(&0));
    }
    fan_ins.sort();
    let q1 = if fan_ins.is_empty() { 0 } else { fan_ins[fan_ins.len() / 4] };
    let q3 = if fan_ins.is_empty() { 0 } else { fan_ins[fan_ins.len() * 3 / 4] };

    // Compute line-count quartiles
    let mut file_sym_count: HashMap<i64, usize> = HashMap::new();
    for sym in &snap.symbols {
        *file_sym_count.entry(sym.file_id).or_default() += 1;
    }

    // Frequently imported files (for import-target attributes)
    let mut import_target_names: HashMap<i64, String> = HashMap::new();
    for &(fid, ref path) in &snap.files {
        let short = Path::new(path).file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        import_target_names.insert(fid, short);
    }
    let frequent_targets: HashSet<i64> = import_target_count.iter()
        .filter(|&(_, count)| *count >= 3)
        .map(|(&id, _)| id)
        .collect();

    for (idx, &(fid, ref path)) in snap.files.iter().enumerate() {
        // Directory attribute
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            let dir = if parts[0] == "src" && parts.len() >= 3 {
                format!("dir:{}/{}", parts[0], parts[1])
            } else {
                format!("dir:{}", parts[0])
            };
            let a = ensure_attr(&dir, &mut attr_map);
            relations.push((idx, a));
        }

        // Extension / language
        if let Some(ext) = Path::new(path).extension() {
            let a = ensure_attr(&format!("ext:{}", ext.to_string_lossy()), &mut attr_map);
            relations.push((idx, a));
        }

        // Has tests
        if file_test_count.get(&fid).copied().unwrap_or(0) > 0 {
            let a = ensure_attr("has_tests", &mut attr_map);
            relations.push((idx, a));
        }

        // Has documented API (>50% pub symbols have docs)
        let pub_count = file_pub_count.get(&fid).copied().unwrap_or(0);
        let doc_count = file_doc_count.get(&fid).copied().unwrap_or(0);
        if pub_count > 0 && doc_count as f64 / pub_count as f64 > 0.5 {
            let a = ensure_attr("has_docs", &mut attr_map);
            relations.push((idx, a));
        }

        // Has pub struct / pub fn / impl
        if file_has_pub_struct.contains(&fid) {
            let a = ensure_attr("has_pub_struct", &mut attr_map);
            relations.push((idx, a));
        }
        if file_has_pub_fn.contains(&fid) {
            let a = ensure_attr("has_pub_fn", &mut attr_map);
            relations.push((idx, a));
        }
        if file_has_impl.contains(&fid) {
            let a = ensure_attr("has_impl", &mut attr_map);
            relations.push((idx, a));
        }

        // Fan-in bucket
        let fi = *import_target_count.get(&fid).unwrap_or(&0);
        let bucket = if fi == 0 { "fan_in:zero" }
            else if fi <= q1 { "fan_in:low" }
            else if fi <= q3 { "fan_in:med" }
            else { "fan_in:high" };
        let a = ensure_attr(bucket, &mut attr_map);
        relations.push((idx, a));

        // Complexity bucket
        let max_cog = file_max_cognitive.get(&fid).copied().unwrap_or(0);
        let cbucket = if max_cog <= 5 { "complexity:low" }
            else if max_cog <= 15 { "complexity:med" }
            else { "complexity:high" };
        let a = ensure_attr(cbucket, &mut attr_map);
        relations.push((idx, a));

        // Size bucket (by symbol count)
        let sc = file_sym_count.get(&fid).copied().unwrap_or(0);
        let sbucket = if sc <= 5 { "size:small" }
            else if sc <= 20 { "size:med" }
            else { "size:large" };
        let a = ensure_attr(sbucket, &mut attr_map);
        relations.push((idx, a));

        // Import targets (only frequent ones)
        for &(src, target) in &snap.import_edges {
            if src == fid && frequent_targets.contains(&target) {
                if let Some(name) = import_target_names.get(&target) {
                    let a = ensure_attr(&format!("imports:{name}"), &mut attr_map);
                    relations.push((idx, a));
                }
            }
        }
    }

    // Build sorted attribute names
    let mut attrs_sorted: Vec<(String, usize)> = attr_map.into_iter().collect();
    attrs_sorted.sort_by_key(|(_, idx)| *idx);
    let attribute_names: Vec<String> = attrs_sorted.into_iter().map(|(name, _)| name).collect();

    FormalContext::new(object_names, attribute_names, relations)
}

// -------------------------------------------------------------------------
// Attribute extraction — symbol level
// -------------------------------------------------------------------------

fn build_symbol_context(snap: &SutraSnapshot) -> FormalContext {
    // Only include non-test symbols with a meaningful kind
    let meaningful_kinds: HashSet<&str> = ["function", "method", "struct", "enum", "trait", "impl", "type_alias", "constant"]
        .into_iter().collect();

    let symbols: Vec<&SymbolInfo> = snap.symbols.iter()
        .filter(|s| s.flags & 0x03 == 0) // exclude tests
        .filter(|s| meaningful_kinds.contains(s.kind.as_str()))
        .collect();

    let file_id_to_path: HashMap<i64, &str> = snap.files.iter()
        .map(|(id, p)| (*id, p.as_str()))
        .collect();

    let object_names: Vec<String> = symbols.iter()
        .map(|s| s.qualified_name.clone())
        .collect();

    let mut attr_map: HashMap<String, usize> = HashMap::new();
    let mut relations: Vec<(usize, usize)> = Vec::new();

    let ensure_attr = |name: &str, map: &mut HashMap<String, usize>| -> usize {
        let len = map.len();
        *map.entry(name.to_string()).or_insert(len)
    };

    for (idx, sym) in symbols.iter().enumerate() {
        // Kind
        let a = ensure_attr(&format!("kind:{}", sym.kind), &mut attr_map);
        relations.push((idx, a));

        // Visibility
        match sym.visibility.as_deref() {
            Some("pub") => {
                let a = ensure_attr("vis:pub", &mut attr_map);
                relations.push((idx, a));
            }
            Some("pub(crate)") => {
                let a = ensure_attr("vis:pub_crate", &mut attr_map);
                relations.push((idx, a));
            }
            _ => {
                let a = ensure_attr("vis:private", &mut attr_map);
                relations.push((idx, a));
            }
        }

        // Has docstring
        if sym.docstring.is_some() {
            let a = ensure_attr("has_doc", &mut attr_map);
            relations.push((idx, a));
        }

        // Has signature
        if sym.signature.is_some() {
            let a = ensure_attr("has_sig", &mut attr_map);
            relations.push((idx, a));
        }

        // Return type patterns (from signature)
        if let Some(ref sig) = sym.signature {
            if sig.contains("Result") {
                let a = ensure_attr("returns_result", &mut attr_map);
                relations.push((idx, a));
            }
            if sig.contains("Option") {
                let a = ensure_attr("returns_option", &mut attr_map);
                relations.push((idx, a));
            }
            if sig.contains("-> Self") || sig.contains("-> &Self") {
                let a = ensure_attr("returns_self", &mut attr_map);
                relations.push((idx, a));
            }
            if sig.contains("&self") {
                let a = ensure_attr("takes_self_ref", &mut attr_map);
                relations.push((idx, a));
            }
            if sig.contains("&mut self") {
                let a = ensure_attr("takes_self_mut", &mut attr_map);
                relations.push((idx, a));
            }
        }

        // Complexity buckets
        if let Some(cog) = sym.cognitive {
            let cbucket = if cog == 0 { "complexity:zero" }
                else if cog <= 5 { "complexity:low" }
                else if cog <= 15 { "complexity:med" }
                else { "complexity:high" };
            let a = ensure_attr(cbucket, &mut attr_map);
            relations.push((idx, a));
        }

        // Naming convention
        let naming = if sym.short_name.chars().all(|c| c.is_uppercase() || c == '_') && sym.short_name.len() > 1 {
            "naming:SCREAMING"
        } else if sym.short_name.chars().next().is_some_and(|c| c.is_uppercase()) {
            "naming:CamelCase"
        } else {
            "naming:snake_case"
        };
        let a = ensure_attr(naming, &mut attr_map);
        relations.push((idx, a));

        // Has parent (nested)
        if sym.id != sym.file_id {
            // Check if this symbol has a parent
            // We'll use a heuristic: methods have a parent impl
            if sym.kind == "method" {
                let a = ensure_attr("is_method", &mut attr_map);
                relations.push((idx, a));
            }
        }

        // Directory context
        if let Some(path) = file_id_to_path.get(&sym.file_id) {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 2 {
                let dir = if parts[0] == "src" && parts.len() >= 3 {
                    format!("in:{}/{}", parts[0], parts[1])
                } else {
                    format!("in:{}", parts[0])
                };
                let a = ensure_attr(&dir, &mut attr_map);
                relations.push((idx, a));
            }
        }
    }

    let mut attrs_sorted: Vec<(String, usize)> = attr_map.into_iter().collect();
    attrs_sorted.sort_by_key(|(_, idx)| *idx);
    let attribute_names: Vec<String> = attrs_sorted.into_iter().map(|(name, _)| name).collect();

    FormalContext::new(object_names, attribute_names, relations)
}

// -------------------------------------------------------------------------
// Experiments
// -------------------------------------------------------------------------

fn experiment_context_stats(ctx: &FormalContext, label: &str) {
    println!("\n{SEP}");
    println!("Context: {label}");
    println!("{SEP}");
    println!("  Objects (|G|):    {}", ctx.n_objects);
    println!("  Attributes (|M|): {}", ctx.n_attrs);
    println!("  Density:          {:.3}", ctx.density());
    println!("\n  Attributes:");
    for (i, name) in ctx.attribute_names.iter().enumerate() {
        let count = ctx.attr_objects[i].count();
        println!("    {name:30} support={count}");
    }
}

fn experiment_lattice(ctx: &FormalContext, label: &str) -> Vec<Concept> {
    println!("\n{SEP}");
    println!("Lattice: {label}");
    println!("{SEP}");

    let t0 = Instant::now();
    let concepts = all_concepts(ctx);
    let elapsed = t0.elapsed();

    println!("  Concepts: {}", concepts.len());
    println!("  Time: {elapsed:?}");

    // Lattice depth: longest chain
    let max_intent_size = concepts.iter().map(|c| c.intent.count()).max().unwrap_or(0);
    let min_intent_size = concepts.iter().map(|c| c.intent.count()).min().unwrap_or(0);
    println!("  Intent range: {min_intent_size}..{max_intent_size}");

    // Distribution of extent sizes
    let mut extent_sizes: Vec<usize> = concepts.iter().map(|c| c.extent.count()).collect();
    extent_sizes.sort();
    println!("  Extent sizes: min={}, median={}, max={}",
        extent_sizes.first().unwrap_or(&0),
        extent_sizes.get(extent_sizes.len() / 2).unwrap_or(&0),
        extent_sizes.last().unwrap_or(&0));

    // Top-10 concepts by extent size (excluding trivial top/bottom)
    println!("\n  Top-10 concepts (by extent size, excluding trivial):");
    let mut sorted: Vec<&Concept> = concepts.iter()
        .filter(|c| c.extent.count() > 0 && c.extent.count() < ctx.n_objects)
        .collect();
    sorted.sort_by(|a, b| b.extent.count().cmp(&a.extent.count()));
    for (rank, c) in sorted.iter().take(10).enumerate() {
        let intent_names: Vec<&str> = c.intent.iter()
            .map(|i| ctx.attribute_names[i].as_str())
            .collect();
        println!("    #{}: extent={}, intent=[{}]",
            rank + 1, c.extent.count(), intent_names.join(", "));
    }

    concepts
}

fn experiment_implications(ctx: &FormalContext, label: &str) {
    println!("\n{SEP}");
    println!("Implications: {label}");
    println!("{SEP}");

    let t0 = Instant::now();
    let exact = extract_exact_implications(ctx, MIN_SUPPORT);
    let elapsed = t0.elapsed();

    println!("  Exact implications (support >= {MIN_SUPPORT}): {} in {elapsed:?}", exact.len());

    println!("\n  Top-20 exact implications:");
    for (i, imp) in exact.iter().take(20).enumerate() {
        let premise: Vec<&str> = imp.premise.iter()
            .map(|&idx| ctx.attribute_names[idx].as_str())
            .collect();
        let conclusion: Vec<&str> = imp.conclusion.iter()
            .map(|&idx| ctx.attribute_names[idx].as_str())
            .collect();
        println!("    #{}: [{}] → [{}]  (support={})",
            i + 1, premise.join(", "), conclusion.join(", "), imp.support);
    }

    // Approximate implications (confidence >= 0.8)
    let approx = extract_approximate_implications(ctx, MIN_SUPPORT, 0.8);
    println!("\n  Approximate implications (conf >= 0.8, support >= {MIN_SUPPORT}): {}", approx.len());
    println!("\n  Top-20 approximate implications:");
    for (i, imp) in approx.iter().take(20).enumerate() {
        let premise: Vec<&str> = imp.premise.iter()
            .map(|&idx| ctx.attribute_names[idx].as_str())
            .collect();
        let conclusion: Vec<&str> = imp.conclusion.iter()
            .map(|&idx| ctx.attribute_names[idx].as_str())
            .collect();
        println!("    #{}: [{}] → [{}]  (support={}, conf={:.2})",
            i + 1, premise.join(", "), conclusion.join(", "), imp.support, imp.confidence);
    }
}

fn experiment_violations(ctx: &FormalContext, snap: &SutraSnapshot, label: &str) {
    println!("\n{SEP}");
    println!("Violations: {label}");
    println!("{SEP}");

    let exact = extract_exact_implications(ctx, MIN_SUPPORT);

    // For approximate implications with conf >= 0.9, find violations
    let approx = extract_approximate_implications(ctx, MIN_SUPPORT, 0.9);

    let mut stable_violations = 0;
    let mut stable_objects = 0;

    let is_file_context = ctx.object_names.first().map(|n| n.contains('/')).unwrap_or(false)
        && !ctx.object_names.first().map(|n| n.contains(":")).unwrap_or(false);

    // Count stable objects (unchanged in last 30 days)
    for name in &ctx.object_names {
        if is_file_context {
            if !snap.recently_changed.contains(name.as_str()) {
                stable_objects += 1;
            }
        }
    }

    println!("  Exact implications: {}", exact.len());
    println!("  Approximate implications (conf >= 0.9): {}", approx.len());
    if is_file_context {
        println!("  Stable files (unchanged 30d): {stable_objects}/{}", ctx.n_objects);
    }

    println!("\n  Violations of high-confidence approximate implications:");
    for imp in approx.iter().take(15) {
        let premise_attrs: Vec<&str> = imp.premise.iter()
            .map(|&idx| ctx.attribute_names[idx].as_str())
            .collect();
        let conclusion_attrs: Vec<&str> = imp.conclusion.iter()
            .map(|&idx| ctx.attribute_names[idx].as_str())
            .collect();

        // Find objects that match premise but not conclusion
        let mut premise_bs = BitSet::new(ctx.n_attrs);
        for &a in &imp.premise { premise_bs.set(a); }
        let premise_extent = ctx.attr_prime(&premise_bs);

        let mut violators = Vec::new();
        for g in premise_extent.iter() {
            let has_conclusion = imp.conclusion.iter()
                .all(|&m| ctx.object_attrs[g].contains(m));
            if !has_conclusion {
                violators.push(g);
                if is_file_context && !snap.recently_changed.contains(ctx.object_names[g].as_str()) {
                    stable_violations += 1;
                }
            }
        }

        if !violators.is_empty() {
            println!("    [{}] → [{}] (conf={:.2})",
                premise_attrs.join(", "), conclusion_attrs.join(", "), imp.confidence);
            for &g in violators.iter().take(5) {
                let changed = if is_file_context {
                    if snap.recently_changed.contains(ctx.object_names[g].as_str()) { " (recently changed)" } else { " (stable)" }
                } else { "" };
                println!("      VIOLATION: {}{changed}", ctx.object_names[g]);
            }
            if violators.len() > 5 {
                println!("      ... and {} more", violators.len() - 5);
            }
        }
    }

    if is_file_context && stable_objects > 0 {
        let fp_rate = stable_violations as f64 / stable_objects as f64;
        println!("\n  False positive rate on stable code: {stable_violations}/{stable_objects} = {fp_rate:.2}");
    }
}

fn experiment_cross_codebase(
    ctx1: &FormalContext, label1: &str,
    ctx2: &FormalContext, label2: &str,
) {
    println!("\n{SEP}");
    println!("Cross-codebase: {label1} vs {label2}");
    println!("{SEP}");

    let impl1 = extract_exact_implications(ctx1, MIN_SUPPORT);
    let impl2 = extract_exact_implications(ctx2, MIN_SUPPORT);

    // Normalize implications to (premise_names, conclusion_names) for comparison
    let normalize = |imp: &Implication, ctx: &FormalContext| -> (BTreeSet<String>, BTreeSet<String>) {
        let p: BTreeSet<String> = imp.premise.iter().map(|&i| ctx.attribute_names[i].clone()).collect();
        let c: BTreeSet<String> = imp.conclusion.iter().map(|&i| ctx.attribute_names[i].clone()).collect();
        (p, c)
    };

    let set1: HashSet<(BTreeSet<String>, BTreeSet<String>)> =
        impl1.iter().map(|i| normalize(i, ctx1)).collect();
    let set2: HashSet<(BTreeSet<String>, BTreeSet<String>)> =
        impl2.iter().map(|i| normalize(i, ctx2)).collect();

    let shared: Vec<_> = set1.intersection(&set2).collect();
    let only1: Vec<_> = set1.difference(&set2).collect();
    let only2: Vec<_> = set2.difference(&set1).collect();

    println!("  {label1} implications: {}", impl1.len());
    println!("  {label2} implications: {}", impl2.len());
    println!("  Shared: {}", shared.len());
    println!("  Only in {label1}: {}", only1.len());
    println!("  Only in {label2}: {}", only2.len());

    if !shared.is_empty() {
        println!("\n  Shared implications (potential universal conventions):");
        for (i, (p, c)) in shared.iter().enumerate().take(10) {
            let pv: Vec<&str> = p.iter().map(|s| s.as_str()).collect();
            let cv: Vec<&str> = c.iter().map(|s| s.as_str()).collect();
            println!("    #{}: [{}] → [{}]", i + 1, pv.join(", "), cv.join(", "));
        }
    }

    if !only1.is_empty() {
        println!("\n  {label1}-specific implications (top 5):");
        for (i, (p, c)) in only1.iter().enumerate().take(5) {
            let pv: Vec<&str> = p.iter().map(|s| s.as_str()).collect();
            let cv: Vec<&str> = c.iter().map(|s| s.as_str()).collect();
            println!("    #{}: [{}] → [{}]", i + 1, pv.join(", "), cv.join(", "));
        }
    }

    if !only2.is_empty() {
        println!("\n  {label2}-specific implications (top 5):");
        for (i, (p, c)) in only2.iter().enumerate().take(5) {
            let pv: Vec<&str> = p.iter().map(|s| s.as_str()).collect();
            let cv: Vec<&str> = c.iter().map(|s| s.as_str()).collect();
            println!("    #{}: [{}] → [{}]", i + 1, pv.join(", "), cv.join(", "));
        }
    }
}

fn experiment_bootstrap_precision(ctx: &FormalContext, label: &str) {
    println!("\n{SEP}");
    println!("Bootstrap precision: {label}");
    println!("{SEP}");

    let full_impls = extract_exact_implications(ctx, MIN_SUPPORT);

    // Split objects into two random halves (deterministic: even/odd index)
    let half_a_objects: Vec<usize> = (0..ctx.n_objects).filter(|i| i % 2 == 0).collect();
    let half_b_objects: Vec<usize> = (0..ctx.n_objects).filter(|i| i % 2 == 1).collect();

    // Build sub-contexts
    let build_sub = |indices: &[usize]| -> FormalContext {
        let obj_names: Vec<String> = indices.iter().map(|&i| ctx.object_names[i].clone()).collect();
        let mut rels = Vec::new();
        for (new_idx, &old_idx) in indices.iter().enumerate() {
            for m in ctx.object_attrs[old_idx].iter() {
                rels.push((new_idx, m));
            }
        }
        FormalContext::new(obj_names, ctx.attribute_names.clone(), rels)
    };

    let ctx_a = build_sub(&half_a_objects);
    let ctx_b = build_sub(&half_b_objects);

    let impls_a = extract_exact_implications(&ctx_a, MIN_SUPPORT.max(2));
    let impls_b = extract_exact_implications(&ctx_b, MIN_SUPPORT.max(2));

    let normalize = |imp: &Implication, c: &FormalContext| -> (BTreeSet<String>, BTreeSet<String>) {
        let p: BTreeSet<String> = imp.premise.iter().map(|&i| c.attribute_names[i].clone()).collect();
        let c_set: BTreeSet<String> = imp.conclusion.iter().map(|&i| c.attribute_names[i].clone()).collect();
        (p, c_set)
    };

    let set_a: HashSet<_> = impls_a.iter().map(|i| normalize(i, &ctx_a)).collect();
    let set_b: HashSet<_> = impls_b.iter().map(|i| normalize(i, &ctx_b)).collect();
    let stable = set_a.intersection(&set_b).count();
    let total_a = set_a.len();
    let total_b = set_b.len();

    let precision = if total_a + total_b > 0 {
        (2 * stable) as f64 / (total_a + total_b) as f64
    } else { 0.0 };

    println!("  Full context implications: {}", full_impls.len());
    println!("  Half-A implications: {total_a}");
    println!("  Half-B implications: {total_b}");
    println!("  Stable across halves: {stable}");
    println!("  Bootstrap precision: {precision:.2}");
    println!("  (Implications that hold in both halves are likely real conventions)");
}

fn experiment_incremental_feasibility(ctx: &FormalContext, label: &str) {
    println!("\n{SEP}");
    println!("Incremental update: {label}");
    println!("{SEP}");

    // Measure: time to recompute full lattice
    let t0 = Instant::now();
    let concepts = all_concepts(ctx);
    let full_time = t0.elapsed();

    if ctx.n_objects < 2 {
        println!("  Too few objects for incremental test");
        return;
    }

    // Measure: time to compute lattice after removing last object
    let reduced_names: Vec<String> = ctx.object_names[..ctx.n_objects - 1].to_vec();
    let mut reduced_rels = Vec::new();
    for (g, attrs) in ctx.object_attrs[..ctx.n_objects - 1].iter().enumerate() {
        for m in attrs.iter() {
            reduced_rels.push((g, m));
        }
    }
    let reduced = FormalContext::new(reduced_names, ctx.attribute_names.clone(), reduced_rels);

    let t0 = Instant::now();
    let reduced_concepts = all_concepts(&reduced);
    let reduced_time = t0.elapsed();

    println!("  Full lattice: {} concepts in {full_time:?}", concepts.len());
    println!("  Reduced (N-1 objects): {} concepts in {reduced_time:?}", reduced_concepts.len());
    println!("  Delta: {} concepts", (concepts.len() as i64 - reduced_concepts.len() as i64).abs());

    // AddExtent feasibility note
    println!("\n  Incremental path: AddExtent algorithm can add one object");
    println!("  without recomputing the full lattice. Complexity: O(|L| * |M|)");
    println!("  where |L| = concept count. For {} concepts and {} attributes,", concepts.len(), ctx.n_attrs);
    println!("  that's ~{} operations per update.", concepts.len() * ctx.n_attrs);
    let feasible = concepts.len() * ctx.n_attrs < 1_000_000;
    println!("  Feasible for live updates: {}", if feasible { "YES" } else { "MARGINAL — consider batching" });
}

// -------------------------------------------------------------------------
// Main
// -------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let workspace_id = args.get(1).map(String::as_str).unwrap_or("sutra");
    let workspace_root = args.get(2).map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let workspace2_id = args.get(3).map(String::as_str);
    let workspace2_root = args.get(4).map(PathBuf::from);

    let db_dir = std::env::var("SUTRA_DB_DIR").map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".sutra")
        });

    println!("=== FCA Convention Detection Spike ===");
    println!("Workspace: {workspace_id}");
    println!("Root: {}", workspace_root.display());
    println!("DB: {}", db_dir.display());

    let db = Db::open(workspace_id, &db_dir).expect("Failed to open Sutra DB");

    let t0 = Instant::now();
    let snap = load_snapshot(&db, &workspace_root);
    println!("Loaded in {:?}: {} files, {} symbols, {} import edges",
        t0.elapsed(), snap.files.len(), snap.symbols.len(), snap.import_edges.len());

    // File-level analysis
    let file_ctx = build_file_context(&snap);
    experiment_context_stats(&file_ctx, &format!("{workspace_id} (files)"));
    experiment_lattice(&file_ctx, &format!("{workspace_id} (files)"));
    experiment_implications(&file_ctx, &format!("{workspace_id} (files)"));
    experiment_violations(&file_ctx, &snap, &format!("{workspace_id} (files)"));
    experiment_bootstrap_precision(&file_ctx, &format!("{workspace_id} (files)"));
    experiment_incremental_feasibility(&file_ctx, &format!("{workspace_id} (files)"));

    // Symbol-level analysis
    let sym_ctx = build_symbol_context(&snap);
    experiment_context_stats(&sym_ctx, &format!("{workspace_id} (symbols)"));
    experiment_lattice(&sym_ctx, &format!("{workspace_id} (symbols)"));
    experiment_implications(&sym_ctx, &format!("{workspace_id} (symbols)"));
    experiment_violations(&sym_ctx, &snap, &format!("{workspace_id} (symbols)"));
    experiment_bootstrap_precision(&sym_ctx, &format!("{workspace_id} (symbols)"));
    experiment_incremental_feasibility(&sym_ctx, &format!("{workspace_id} (symbols)"));

    // Cross-codebase comparison
    if let (Some(w2_id), Some(w2_root)) = (workspace2_id, workspace2_root.as_ref()) {
        println!("\n\n{SEP}");
        println!("=== Second codebase: {w2_id} ===");

        let db2 = Db::open(w2_id, &db_dir).expect("Failed to open second DB");
        let snap2 = load_snapshot(&db2, w2_root);
        println!("Loaded: {} files, {} symbols, {} import edges",
            snap2.files.len(), snap2.symbols.len(), snap2.import_edges.len());

        let file_ctx2 = build_file_context(&snap2);
        experiment_context_stats(&file_ctx2, &format!("{w2_id} (files)"));
        experiment_lattice(&file_ctx2, &format!("{w2_id} (files)"));
        experiment_implications(&file_ctx2, &format!("{w2_id} (files)"));

        let sym_ctx2 = build_symbol_context(&snap2);
        experiment_context_stats(&sym_ctx2, &format!("{w2_id} (symbols)"));
        experiment_lattice(&sym_ctx2, &format!("{w2_id} (symbols)"));
        experiment_implications(&sym_ctx2, &format!("{w2_id} (symbols)"));

        experiment_cross_codebase(&file_ctx, workspace_id, &file_ctx2, w2_id);
        experiment_cross_codebase(&sym_ctx, workspace_id, &sym_ctx2, w2_id);
    }

    println!("\n{SEP}");
    println!("All experiments complete.");
}
