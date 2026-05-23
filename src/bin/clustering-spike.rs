use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use sutra::hrr::{self, Codebook as HrrCodebook, HrrVec};

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    C,
}

// ===== Undirected weighted graph =====

struct Graph {
    n: usize,
    adj: Vec<Vec<(usize, f64)>>,
    degree: Vec<f64>,
    m: f64,
}

impl Graph {
    fn new(n: usize) -> Self {
        Self { n, adj: vec![Vec::new(); n], degree: vec![0.0; n], m: 0.0 }
    }

    fn add_edge(&mut self, u: usize, v: usize, w: f64) {
        self.adj[u].push((v, w));
        self.adj[v].push((u, w));
        self.degree[u] += w;
        self.degree[v] += w;
        self.m += w;
    }

    fn from_edge_map(n: usize, edges: &HashMap<(usize, usize), f64>) -> Self {
        let mut g = Self::new(n);
        for (&(u, v), &w) in edges {
            if u < v && w > 0.0 {
                g.add_edge(u, v, w);
            }
        }
        g
    }

    fn edge_list(&self) -> Vec<(usize, usize, f64)> {
        let mut out = Vec::new();
        for u in 0..self.n {
            for &(v, w) in &self.adj[u] {
                if u < v {
                    out.push((u, v, w));
                }
            }
        }
        out
    }
}

// ===== Louvain community detection =====
//
// Single-level Louvain (phase 1 only). Sufficient for codebases
// up to ~1K files. Multi-level aggregation can be added if needed.
//
// Modularity gain for moving isolated node i into community C:
//   dQ = k_{i,in}/m - k_i * Sigma_tot_C / (2m^2)

fn louvain(graph: &Graph) -> Vec<usize> {
    if graph.n == 0 {
        return vec![];
    }
    if graph.m < 1e-15 {
        return (0..graph.n).collect();
    }

    let mut comm: Vec<usize> = (0..graph.n).collect();
    let mut sigma: Vec<f64> = graph.degree.clone();

    for _ in 0..100 {
        let mut moved = false;

        for i in 0..graph.n {
            let ki = graph.degree[i];
            if ki < 1e-15 {
                continue;
            }
            let ci = comm[i];

            let mut ki_in: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &graph.adj[i] {
                *ki_in.entry(comm[j]).or_default() += w;
            }

            sigma[ci] -= ki;

            let base_gain = ki_in.get(&ci).copied().unwrap_or(0.0) / graph.m
                - ki * sigma[ci] / (2.0 * graph.m * graph.m);
            let mut best_gain = base_gain;
            let mut best_c = ci;

            for (&c, &ki_c) in &ki_in {
                if c == ci {
                    continue;
                }
                let gain = ki_c / graph.m - ki * sigma[c] / (2.0 * graph.m * graph.m);
                if gain > best_gain {
                    best_gain = gain;
                    best_c = c;
                }
            }

            comm[i] = best_c;
            sigma[best_c] += ki;
            if best_c != ci {
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }

    renumber(&mut comm);
    comm
}

fn renumber(comm: &mut [usize]) {
    let mut map: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    for c in comm.iter_mut() {
        let n = *map.entry(*c).or_insert_with(|| {
            let l = next;
            next += 1;
            l
        });
        *c = n;
    }
}

fn modularity(graph: &Graph, comm: &[usize]) -> f64 {
    if graph.m < 1e-15 {
        return 0.0;
    }
    let m2 = 2.0 * graph.m;
    let mut q = 0.0;
    for i in 0..graph.n {
        for &(j, w) in &graph.adj[i] {
            if comm[i] == comm[j] {
                q += w - graph.degree[i] * graph.degree[j] / m2;
            }
        }
    }
    q / m2
}

fn n_clusters(comm: &[usize]) -> usize {
    comm.iter().copied().collect::<HashSet<_>>().len()
}

// ===== Evaluation metrics =====

fn nmi(a: &[usize], b: &[usize]) -> f64 {
    assert_eq!(a.len(), b.len());
    let n = a.len() as f64;
    if n < 1.0 {
        return 0.0;
    }

    let ka = *a.iter().max().unwrap_or(&0) + 1;
    let kb = *b.iter().max().unwrap_or(&0) + 1;

    let mut cont = vec![vec![0usize; kb]; ka];
    let mut ra = vec![0usize; ka];
    let mut rb = vec![0usize; kb];

    for i in 0..a.len() {
        cont[a[i]][b[i]] += 1;
        ra[a[i]] += 1;
        rb[b[i]] += 1;
    }

    let mut mi = 0.0;
    for i in 0..ka {
        for j in 0..kb {
            if cont[i][j] > 0 {
                let pij = cont[i][j] as f64 / n;
                mi += pij * (pij / (ra[i] as f64 / n * (rb[j] as f64 / n))).ln();
            }
        }
    }

    let ha: f64 = ra
        .iter()
        .filter(|&&s| s > 0)
        .map(|&s| {
            let p = s as f64 / n;
            -p * p.ln()
        })
        .sum();
    let hb: f64 = rb
        .iter()
        .filter(|&&s| s > 0)
        .map(|&s| {
            let p = s as f64 / n;
            -p * p.ln()
        })
        .sum();

    if ha + hb < 1e-15 {
        1.0
    } else {
        2.0 * mi / (ha + hb)
    }
}

fn dir_purity(dirs: &[String], comm: &[usize]) -> (f64, Vec<(usize, f64, String)>) {
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &c) in comm.iter().enumerate() {
        clusters.entry(c).or_default().push(i);
    }

    let mut details = Vec::new();
    let mut total = 0.0;

    for (_, members) in &clusters {
        let mut dc: HashMap<&str, usize> = HashMap::new();
        for &i in members {
            *dc.entry(&dirs[i]).or_default() += 1;
        }
        let (best_dir, best_n) = dc.iter().max_by_key(|(_, c)| **c).unwrap();
        let pur = *best_n as f64 / members.len() as f64;
        total += pur;
        details.push((members.len(), pur, best_dir.to_string()));
    }

    details.sort_by(|a, b| b.0.cmp(&a.0));
    let avg = if clusters.is_empty() { 0.0 } else { total / clusters.len() as f64 };
    (avg, details)
}

// ===== RNG for perturbation tests =====

struct PerturbRng {
    state: u64,
}

impl PerturbRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn f64(&mut self) -> f64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.state >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ===== Main =====

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map(String::as_str).unwrap_or("src");

    let rs_files = collect_files(root, "rs");
    let c_files = collect_files(root, "c");

    println!("=== Graph Clustering Spike: Component Discovery ===\n");
    println!("Root: {root}");
    println!(
        "Files: {} ({} .rs, {} .c)\n",
        rs_files.len() + c_files.len(),
        rs_files.len(),
        c_files.len()
    );

    if rs_files.is_empty() && c_files.is_empty() {
        println!("No source files found.");
        return;
    }

    // --- Parse all files, extract functions + calls + HRR vectors ---

    let t0 = Instant::now();

    let mut rs_parser = tree_sitter::Parser::new();
    rs_parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let mut c_parser = tree_sitter::Parser::new();
    c_parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .unwrap();

    let mut hrr_cb = HrrCodebook::new(42);

    struct FileData {
        path: String,
        dir: String,
        fns: Vec<String>,
        calls: Vec<Vec<String>>,
        hrr_vecs: Vec<HrrVec>,
    }

    let mut files: Vec<FileData> = Vec::new();
    let mut total_fns = 0usize;
    let mut total_calls = 0usize;

    for (file_list, lang, parser) in [
        (&rs_files, Lang::Rust, &mut rs_parser),
        (&c_files, Lang::C, &mut c_parser),
    ] {
        for path in file_list {
            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let tree = match parser.parse(&source, None) {
                Some(t) => t,
                None => continue,
            };

            let pstr = path.display().to_string();
            let short = pstr
                .strip_prefix(root)
                .and_then(|s| s.strip_prefix('/'))
                .unwrap_or(&pstr)
                .to_string();
            let dir = short.rsplit_once('/').map(|(d, _)| d).unwrap_or(".").to_string();

            let fn_list = extract_functions(&tree, source.as_bytes(), lang);

            let mut fns = Vec::new();
            let mut calls = Vec::new();
            let mut hrr_vecs = Vec::new();

            for (name, node_id) in &fn_list {
                if let Some(node) = find_node_by_id(&tree.root_node(), *node_id) {
                    fns.push(name.clone());
                    let c = extract_calls(&node, source.as_bytes());
                    total_calls += c.len();
                    calls.push(c);
                    hrr_vecs.push(encode_hrr(&node, source.as_bytes(), &mut hrr_cb, 4));
                    total_fns += 1;
                }
            }

            files.push(FileData { path: short, dir, fns, calls, hrr_vecs });
        }
    }

    let parse_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "Parsed: {} files, {} functions, {} call-sites ({:.0}ms)\n",
        files.len(),
        total_fns,
        total_calls,
        parse_ms
    );

    if files.len() < 3 {
        println!("Too few files for meaningful clustering.");
        return;
    }

    // --- Build function name → file indices ---

    let n = files.len();
    let mut fn_defs: HashMap<String, Vec<usize>> = HashMap::new();
    for (fi, f) in files.iter().enumerate() {
        for name in &f.fns {
            fn_defs.entry(name.clone()).or_default().push(fi);
        }
    }

    // --- Build file-level call graph ---
    // Edge weight = sum of calls, weighted by 1/ambiguity to downweight
    // common names (new, default, fmt, etc.)

    let mut file_edges: HashMap<(usize, usize), f64> = HashMap::new();
    let mut resolved = 0usize;

    for (fi, f) in files.iter().enumerate() {
        for call_list in &f.calls {
            for call in call_list {
                if let Some(targets) = fn_defs.get(call) {
                    let w = 1.0 / targets.len() as f64;
                    for &ti in targets {
                        if ti != fi {
                            let key = (fi.min(ti), fi.max(ti));
                            *file_edges.entry(key).or_default() += w;
                            resolved += 1;
                        }
                    }
                }
            }
        }
    }

    let graph = Graph::from_edge_map(n, &file_edges);
    let connected = (0..n).filter(|&i| graph.degree[i] > 0.0).count();
    println!(
        "File graph: {} nodes ({} connected), {} edges, weight {:.1}",
        n, connected, file_edges.len(), graph.m
    );
    println!("Resolved {} cross-file call references\n", resolved);

    // ================================================================
    // Experiment 1: File-level Louvain
    // ================================================================

    println!("{}\n=== 1. File-level clustering ===\n", "=".repeat(60));

    let t1 = Instant::now();
    let part = louvain(&graph);
    let q = modularity(&graph, &part);
    let k = n_clusters(&part);
    println!(
        "Louvain: {} communities, Q = {:.4} ({:.1}ms)\n",
        k,
        q,
        t1.elapsed().as_secs_f64() * 1000.0
    );

    let clusters: Vec<(usize, Vec<usize>)> = {
        let mut map: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, &c) in part.iter().enumerate() {
            map.entry(c).or_default().push(i);
        }
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by_key(|(_, m)| std::cmp::Reverse(m.len()));
        v
    };

    for (cid, members) in &clusters {
        let paths: Vec<&str> = members.iter().map(|&i| files[i].path.as_str()).collect();
        let show = if paths.len() > 6 {
            format!(
                "{}, ... (+{} more)",
                paths[..5].join(", "),
                paths.len() - 5
            )
        } else {
            paths.join(", ")
        };
        println!("  C{} ({} files): {}", cid, members.len(), show);
    }

    // ================================================================
    // Experiment 2: Directory purity
    // ================================================================

    println!("\n{}\n=== 2. Directory purity ===\n", "=".repeat(60));

    let dirs: Vec<String> = files.iter().map(|f| f.dir.clone()).collect();
    let (avg_pur, pur_details) = dir_purity(&dirs, &part);

    for (sz, pur, dir) in &pur_details {
        println!("  {} files: {:.0}% (majority: {})", sz, pur * 100.0, dir);
    }
    println!("\nAverage purity: {:.1}%", avg_pur * 100.0);
    let pur_pass = avg_pur >= 0.60;
    println!(
        "-> {} (threshold: 60%)",
        if pur_pass { "PASS" } else { "FAIL" }
    );

    // ================================================================
    // Experiment 3: Stability under perturbation
    // ================================================================

    println!(
        "\n{}\n=== 3. Stability under perturbation ===\n",
        "=".repeat(60)
    );

    let edges = graph.edge_list();
    let mut rng = PerturbRng::new(12345);
    let mut stability_nmis = Vec::new();

    for &frac in &[0.10f64, 0.20, 0.30] {
        let trials = 5;
        let mut nmi_sum = 0.0;
        for _ in 0..trials {
            let mut pe: HashMap<(usize, usize), f64> = HashMap::new();
            for &(u, v, w) in &edges {
                if rng.f64() >= frac {
                    pe.insert((u, v), w);
                }
            }
            let pg = Graph::from_edge_map(n, &pe);
            let pp = louvain(&pg);
            nmi_sum += nmi(&part, &pp);
        }
        let avg_nmi = nmi_sum / trials as f64;
        stability_nmis.push(avg_nmi);
        println!(
            "  {:.0}% removed: NMI = {:.3} (avg of {})",
            frac * 100.0,
            avg_nmi,
            trials
        );
    }

    let stable = stability_nmis[0] >= 0.70;
    println!(
        "\n-> {} (NMI >= 0.70 at 10% removal)",
        if stable { "STABLE" } else { "UNSTABLE" }
    );

    // ================================================================
    // Experiment 4: Function-level vs file-level granularity
    // ================================================================

    println!(
        "\n{}\n=== 4. Function vs file granularity ===\n",
        "=".repeat(60)
    );

    let mut fn_global: Vec<(usize, usize)> = Vec::new();
    let mut fn_name_idx: HashMap<String, Vec<usize>> = HashMap::new();

    for (fi, f) in files.iter().enumerate() {
        for (fni, name) in f.fns.iter().enumerate() {
            let gi = fn_global.len();
            fn_global.push((fi, fni));
            fn_name_idx.entry(name.clone()).or_default().push(gi);
        }
    }

    let fn_n = fn_global.len();
    let mut fn_edges: HashMap<(usize, usize), f64> = HashMap::new();

    for (gi, &(fi, fni)) in fn_global.iter().enumerate() {
        for call in &files[fi].calls[fni] {
            if let Some(targets) = fn_name_idx.get(call) {
                let w = 1.0 / targets.len() as f64;
                for &tgi in targets {
                    if tgi != gi {
                        let key = (gi.min(tgi), gi.max(tgi));
                        *fn_edges.entry(key).or_default() += w;
                    }
                }
            }
        }
    }

    let fn_graph = Graph::from_edge_map(fn_n, &fn_edges);
    let fn_part = louvain(&fn_graph);
    let fn_q = modularity(&fn_graph, &fn_part);
    let fn_k = n_clusters(&fn_part);

    println!(
        "Function-level: {} nodes, {} edges -> {} clusters, Q = {:.4}",
        fn_n,
        fn_edges.len(),
        fn_k,
        fn_q
    );
    println!(
        "File-level:     {} nodes, {} edges -> {} clusters, Q = {:.4}",
        n,
        file_edges.len(),
        k,
        q
    );

    let fn_sizes: Vec<usize> = {
        let mut m: HashMap<usize, usize> = HashMap::new();
        for &c in &fn_part {
            *m.entry(c).or_default() += 1;
        }
        let mut v: Vec<usize> = m.values().copied().collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    };
    let top: Vec<String> = fn_sizes.iter().take(10).map(|s| s.to_string()).collect();
    println!(
        "\nFn cluster sizes (top 10): [{}{}]",
        top.join(", "),
        if fn_sizes.len() > 10 { ", ..." } else { "" }
    );
    println!(
        "Avg fns/cluster: {:.1}",
        fn_n as f64 / fn_k.max(1) as f64
    );

    // ================================================================
    // Experiment 5: HRR-blended clustering (0.7 graph / 0.3 HRR)
    // ================================================================

    println!(
        "\n{}\n=== 5. HRR-blended clustering ===\n",
        "=".repeat(60)
    );

    let file_hrr: Vec<Option<HrrVec>> = files
        .iter()
        .map(|f| {
            if f.hrr_vecs.is_empty() {
                None
            } else {
                Some(hrr::bundle(&f.hrr_vecs))
            }
        })
        .collect();

    let avg_w = if file_edges.is_empty() {
        1.0
    } else {
        file_edges.values().sum::<f64>() / file_edges.len() as f64
    };

    let mut blended: HashMap<(usize, usize), f64> = HashMap::new();

    for (&(u, v), &w) in &file_edges {
        let sim = match (&file_hrr[u], &file_hrr[v]) {
            (Some(a), Some(b)) => a.cosine_similarity(b).max(0.0),
            _ => 0.0,
        };
        blended.insert((u, v), 0.7 * w + 0.3 * sim * avg_w);
    }

    let hrr_edges_before = blended.len();
    if n <= 1000 {
        for i in 0..n {
            for j in (i + 1)..n {
                if file_edges.contains_key(&(i, j)) {
                    continue;
                }
                if let (Some(a), Some(b)) = (&file_hrr[i], &file_hrr[j]) {
                    let sim = a.cosine_similarity(b);
                    if sim > 0.3 {
                        blended.insert((i, j), 0.3 * sim * avg_w);
                    }
                }
            }
        }
    }
    let hrr_extra = blended.len() - hrr_edges_before;

    let bg = Graph::from_edge_map(n, &blended);
    let bp = louvain(&bg);
    let bq = modularity(&bg, &bp);
    let bk = n_clusters(&bp);
    let (bavg, _) = dir_purity(&dirs, &bp);

    println!(
        "Graph-only: {} clusters, Q={:.4}, purity={:.1}%",
        k,
        q,
        avg_pur * 100.0
    );
    println!(
        "Blended:    {} clusters, Q={:.4}, purity={:.1}%",
        bk,
        bq,
        bavg * 100.0
    );
    println!("HRR-only edges added: {}", hrr_extra);

    let delta = bavg - avg_pur;
    println!(
        "-> Purity delta: {:+.1}pp{}",
        delta * 100.0,
        if delta > 0.02 {
            " (helps)"
        } else if delta < -0.02 {
            " (hurts)"
        } else {
            " (negligible)"
        }
    );

    // ================================================================
    // Experiment 6: Scale metrics
    // ================================================================

    println!("\n{}\n=== 6. Scale ===\n", "=".repeat(60));

    println!("Files:            {}", files.len());
    println!("Functions:        {}", total_fns);
    println!("File edges:       {}", file_edges.len());
    println!("Function edges:   {}", fn_edges.len());
    println!("File clusters:    {}", k);
    println!("Function clusters: {}", fn_k);
    println!("Files/cluster:    {:.1}", n as f64 / k.max(1) as f64);
    println!("Fns/cluster:      {:.1}", fn_n as f64 / fn_k.max(1) as f64);
    println!("Parse time:       {:.0}ms", parse_ms);

    // ================================================================
    // Summary
    // ================================================================

    println!("\n{}\n=== SUMMARY ===\n", "=".repeat(60));

    println!("{:<22} {:<14} {}", "Metric", "Value", "Assessment");
    println!("{:-<22} {:-<14} {:-<20}", "", "", "");
    println!(
        "{:<22} {:<14} {}",
        "Modularity (file)",
        format!("{:.4}", q),
        if q > 0.3 {
            "strong structure"
        } else if q > 0.1 {
            "moderate structure"
        } else {
            "weak structure"
        }
    );
    println!(
        "{:<22} {:<14} {}",
        "Directory purity",
        format!("{:.1}%", avg_pur * 100.0),
        if pur_pass { "PASS (>=60%)" } else { "FAIL (<60%)" }
    );
    println!(
        "{:<22} {:<14} {}",
        "Stability (10%)",
        format!("NMI {:.3}", stability_nmis[0]),
        if stable { "STABLE" } else { "UNSTABLE" }
    );
    println!(
        "{:<22} {:<14} {}",
        "HRR blend",
        format!("{:+.1}pp", delta * 100.0),
        if delta.abs() < 0.02 {
            "negligible"
        } else if delta > 0.0 {
            "helps"
        } else {
            "hurts"
        }
    );
    println!(
        "{:<22} {:<14}",
        "Clusters",
        format!("{} file, {} fn", k, fn_k)
    );
    println!(
        "{:<22} {:<14}",
        "Files/cluster",
        format!("{:.1}", n as f64 / k.max(1) as f64)
    );

    println!();

    if q > 0.3 && pur_pass {
        println!("-> VIABLE: Louvain produces recognizable architectural clusters");
    } else if q > 0.1 && avg_pur >= 0.50 {
        println!("-> VIABLE WITH CAVEATS: moderate cluster quality, may need tuning");
    } else {
        println!("-> NOT VIABLE: clusters don't map to recognizable subsystems");
    }
}

// ===== HRR encoding (from hdc-eval pattern) =====

fn encode_hrr(
    node: &tree_sitter::Node,
    source: &[u8],
    cb: &mut HrrCodebook,
    depth: usize,
) -> HrrVec {
    let kind = cb.get_or_create(node.kind());
    if depth == 0 || node.child_count() == 0 {
        return kind;
    }

    let mut children = Vec::new();
    let mut pos = 0usize;
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        if !child.is_named() {
            continue;
        }
        children.push(encode_hrr(&child, source, cb, depth - 1).permute(pos + 1));
        pos += 1;
    }

    if children.is_empty() {
        return kind;
    }
    kind.bind(&hrr::bundle(&children))
}

// ===== AST extraction =====

fn extract_functions(
    tree: &tree_sitter::Tree,
    source: &[u8],
    lang: Lang,
) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    walk_fns(&tree.root_node(), source, lang, &mut out);
    out
}

fn walk_fns(
    node: &tree_sitter::Node,
    source: &[u8],
    lang: Lang,
    out: &mut Vec<(String, usize)>,
) {
    let is_fn = match lang {
        Lang::Rust => node.kind() == "function_item" || node.kind() == "function_signature_item",
        Lang::C => node.kind() == "function_definition",
    };
    if is_fn {
        let name = node
            .child_by_field_name("name")
            .or_else(|| {
                node.child_by_field_name("declarator")
                    .and_then(|d| d.child_by_field_name("declarator"))
            })
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("<anon>")
            .to_string();
        out.push((name, node.id()));
    }
    for i in 0..node.child_count() {
        walk_fns(&node.child(i).unwrap(), source, lang, out);
    }
}

fn extract_calls(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    walk_calls(node, source, &mut out);
    out
}

fn walk_calls(node: &tree_sitter::Node, source: &[u8], out: &mut Vec<String>) {
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            if let Some(name) = call_target(&func, source) {
                out.push(name);
            }
        }
    }
    for i in 0..node.child_count() {
        walk_calls(&node.child(i).unwrap(), source, out);
    }
}

fn call_target(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(source).ok().map(String::from),
        "scoped_identifier" | "qualified_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .map(String::from),
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(source).ok())
            .map(String::from),
        _ => None,
    }
}

fn find_node_by_id<'a>(
    node: &tree_sitter::Node<'a>,
    id: usize,
) -> Option<tree_sitter::Node<'a>> {
    if node.id() == id {
        return Some(*node);
    }
    for i in 0..node.child_count() {
        if let Some(f) = find_node_by_id(&node.child(i).unwrap(), id) {
            return Some(f);
        }
    }
    None
}

// ===== File utilities =====

fn collect_files(dir: &str, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(dir.as_ref(), ext, &mut out);
    out.sort();
    out
}

fn walk_dir(dir: &std::path::Path, ext: &str, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}
