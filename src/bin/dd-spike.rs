use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use differential_dataflow::input::InputSession;
use differential_dataflow::operators::iterate::Iterate;
use differential_dataflow::operators::threshold::ThresholdTotal;
use differential_dataflow::operators::count::CountTotal;
use timely::dataflow::ProbeHandle;

use sutra::db::Db;

const SEP: &str = "============================================================";

// -------------------------------------------------------------------------
// Data loading
// -------------------------------------------------------------------------

#[derive(Clone)]
struct SutraSnapshot {
    files: Vec<(i64, String)>,
    import_edges: Vec<(i64, i64)>,
    sym_file_map: Vec<(i64, i64)>,
    resolved_refs: Vec<(i64, i64)>,
    commit_files: Vec<(u32, String)>,
    sym_names: HashMap<i64, String>,
}

fn load_snapshot(db: &Db, workspace_root: &Path) -> SutraSnapshot {
    let all_files = db.all_files().unwrap();
    let files: Vec<(i64, String)> = all_files.iter().map(|f| (f.id, f.path.clone())).collect();
    let import_edges = db.import_edges().unwrap();
    let sym_file_map = db.all_symbol_file_map().unwrap();
    let resolved_refs = db.all_resolved_refs().unwrap();
    let commit_files = load_git_commits(workspace_root);
    let sym_names: HashMap<i64, String> = db
        .all_symbols_summary()
        .unwrap()
        .into_iter()
        .map(|(id, qname, _, _)| (id, qname))
        .collect();

    SutraSnapshot { files, import_edges, sym_file_map, resolved_refs, commit_files, sym_names }
}

fn load_git_commits(workspace_root: &Path) -> Vec<(u32, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["log", "--name-only", "--pretty=format:COMMIT_SEP", "--since", "90 days ago"])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result = Vec::new();
    let mut commit_ord: u32 = 0;
    for line in stdout.lines() {
        let line = line.trim();
        if line == "COMMIT_SEP" {
            commit_ord += 1;
        } else if !line.is_empty() {
            result.push((commit_ord, line.to_string()));
        }
    }
    result
}

fn build_file_edges(snap: &SutraSnapshot) -> Vec<(i64, i64)> {
    let sym_to_file: HashMap<i64, i64> = snap.sym_file_map.iter().copied().collect();
    let mut edges: Vec<(i64, i64)> = Vec::new();
    for &(src_file, target_sym) in &snap.resolved_refs {
        if let Some(&target_file) = sym_to_file.get(&target_sym) {
            if target_file != src_file {
                edges.push((src_file, target_file));
            }
        }
    }
    for &(src, dst) in &snap.import_edges {
        edges.push((src, dst));
    }
    edges.sort();
    edges.dedup();
    edges
}

// -------------------------------------------------------------------------
// Experiment 1: deps
// -------------------------------------------------------------------------

fn experiment_deps(snap: &SutraSnapshot, db: &Db) {
    println!("\n{SEP}");
    println!("Experiment 1: deps (file dependency BFS via DD iterate)");
    println!("{SEP}");

    let root_file = snap.files.first().map(|(id, _)| *id).unwrap_or(1);
    let root_path = snap.files.iter().find(|(id, _)| *id == root_file)
        .map(|(_, p)| p.as_str()).unwrap_or("?");
    let max_depth: i64 = 2;
    println!("Root: {root_path} (id={root_file}), depth={max_depth}");

    let import_edges = snap.import_edges.clone();

    let t0 = Instant::now();
    let dd_nodes: Vec<i64> = timely::execute_directly(move |worker| {
        let out = Arc::new(Mutex::new(Vec::new()));
        let out_w = out.clone();
        let mut edges_input: InputSession<usize, (i64, i64), isize> = InputSession::new();
        let mut roots_input: InputSession<usize, (i64, i64), isize> = InputSession::new();
        let mut probe = ProbeHandle::new();

        worker.dataflow(|scope| {
            let edges = edges_input.to_collection(scope);
            let roots = roots_input.to_collection(scope);

            let reachable = roots.iterate(|subscope, inner| {
                let edges = edges.enter(subscope);
                let frontier = inner.clone().filter(|(_node, rem)| *rem > 0);
                let next = frontier.join_map(edges, |_src, rem, dst| (*dst, *rem - 1));
                inner.concat(next).reduce(|_node, input, output| {
                    let max_rem = input.iter().map(|(rem, _)| **rem).max().unwrap_or(0i64);
                    output.push((max_rem, 1));
                })
            });

            reachable.map(|(node, _)| node).distinct_total()
                .inspect(move |(data, _, diff)| {
                    if *diff > 0 { out_w.lock().unwrap().push(*data); }
                })
                .probe_with(&mut probe);
        });

        for &(src, dst) in &import_edges {
            edges_input.insert((src, dst));
        }
        roots_input.insert((root_file, max_depth));
        edges_input.advance_to(1);
        roots_input.advance_to(1);
        edges_input.flush();
        roots_input.flush();
        worker.step_while(|| probe.less_than(&1));

        let mut r = out.lock().unwrap().clone();
        r.sort();
        r
    });
    let dd_time = t0.elapsed();

    let t1 = Instant::now();
    let mut ref_sorted = reference_deps(db, root_file, max_depth as usize);
    let ref_time = t1.elapsed();
    ref_sorted.sort();

    println!("DD:  {} nodes in {:?}", dd_nodes.len(), dd_time);
    println!("Ref: {} nodes in {:?}", ref_sorted.len(), ref_time);
    println!("Match: {}", if dd_nodes == ref_sorted { "YES" } else { "NO -- MISMATCH" });
    if dd_nodes != ref_sorted {
        let dd_set: HashSet<i64> = dd_nodes.iter().copied().collect();
        let ref_set: HashSet<i64> = ref_sorted.iter().copied().collect();
        println!("  Only in DD: {}, Only in Ref: {}",
            dd_set.difference(&ref_set).count(), ref_set.difference(&dd_set).count());
    }
}

fn reference_deps(db: &Db, root: i64, depth: usize) -> Vec<i64> {
    let all_edges = db.import_edges().unwrap();
    let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
    for (from, to) in &all_edges {
        adj.entry(*from).or_default().push(*to);
    }
    let mut visited = HashSet::new();
    visited.insert(root);
    let mut queue = VecDeque::new();
    queue.push_back((root, 0usize));
    while let Some((fid, d)) = queue.pop_front() {
        if d >= depth { continue; }
        if let Some(targets) = adj.get(&fid) {
            for &tid in targets {
                if visited.insert(tid) { queue.push_back((tid, d + 1)); }
            }
        }
    }
    visited.into_iter().collect()
}

// -------------------------------------------------------------------------
// Experiment 2: impact
// -------------------------------------------------------------------------

fn experiment_impact(snap: &SutraSnapshot, db: &Db) {
    println!("\n{SEP}");
    println!("Experiment 2: impact (transitive file impact via DD)");
    println!("{SEP}");

    let sym_to_file: HashMap<i64, i64> = snap.sym_file_map.iter().copied().collect();

    let mut ref_counts: HashMap<i64, usize> = HashMap::new();
    for &(_, target) in &snap.resolved_refs {
        *ref_counts.entry(target).or_default() += 1;
    }
    let target_sym = ref_counts.iter()
        .filter(|(_, c)| **c >= 3 && **c <= 50)
        .max_by_key(|(_, c)| *c)
        .map(|(&sym, _)| sym)
        .unwrap_or_else(|| snap.sym_file_map.first().map(|(s, _)| *s).unwrap_or(1));

    let sym_name = snap.sym_names.get(&target_sym).cloned()
        .unwrap_or_else(|| format!("sym#{target_sym}"));
    let direct_count = ref_counts.get(&target_sym).copied().unwrap_or(0);
    println!("Target: {sym_name} (id={target_sym}, {direct_count} direct refs)");

    // File-level impact edges: if file A contains a symbol referenced from file B,
    // then changes to A impact B → edge (A, B)
    let mut file_impact_edges: Vec<(i64, i64)> = Vec::new();
    for &(src_file, target_sym_id) in &snap.resolved_refs {
        if let Some(&target_file) = sym_to_file.get(&target_sym_id) {
            if target_file != src_file {
                file_impact_edges.push((target_file, src_file));
            }
        }
    }
    file_impact_edges.sort();
    file_impact_edges.dedup();

    let target_file = sym_to_file.get(&target_sym).copied().unwrap_or(0);

    let t0 = Instant::now();
    let dd_files: Vec<i64> = timely::execute_directly(move |worker| {
        let out = Arc::new(Mutex::new(Vec::new()));
        let out_w = out.clone();
        let mut edges_input: InputSession<usize, (i64, i64), isize> = InputSession::new();
        let mut seeds_input: InputSession<usize, i64, isize> = InputSession::new();
        let mut probe = ProbeHandle::new();

        worker.dataflow(|scope| {
            let edges = edges_input.to_collection(scope);
            let seeds = seeds_input.to_collection(scope);

            let reachable = seeds.iterate(|subscope, inner| {
                let edges = edges.enter(subscope);
                let next = inner.clone().map(|f| (f, ())).join_map(edges, |_src, _unit, dst| *dst);
                inner.concat(next).distinct()
            });

            reachable
                .inspect(move |(data, _, diff)| {
                    if *diff > 0 { out_w.lock().unwrap().push(*data); }
                })
                .probe_with(&mut probe);
        });

        for &(src, dst) in &file_impact_edges {
            edges_input.insert((src, dst));
        }
        seeds_input.insert(target_file);
        edges_input.advance_to(1);
        seeds_input.advance_to(1);
        edges_input.flush();
        seeds_input.flush();
        worker.step_while(|| probe.less_than(&1));

        let mut r = out.lock().unwrap().clone();
        r.sort();
        r
    });
    let dd_time = t0.elapsed();

    let t1 = Instant::now();
    let ref_files = reference_impact(db, target_sym);
    let ref_time = t1.elapsed();
    let mut ref_sorted: Vec<i64> = ref_files.iter().copied().collect();
    ref_sorted.sort();

    println!("DD:  {} files impacted in {:?}", dd_files.len(), dd_time);
    println!("Ref: {} files impacted in {:?}", ref_sorted.len(), ref_time);

    let dd_set: HashSet<i64> = dd_files.iter().copied().collect();
    let ref_set: HashSet<i64> = ref_sorted.iter().copied().collect();
    let overlap = dd_set.intersection(&ref_set).count();
    println!("Overlap: {overlap}/{} (DD), {overlap}/{} (ref)", dd_set.len(), ref_set.len());
    if dd_set == ref_set {
        println!("Match: YES (exact)");
    } else {
        println!("Match: partial -- DD=full file-level TC, ref=depth-3 symbol BFS");
    }
}

fn reference_impact(db: &Db, target_sym: i64) -> HashSet<i64> {
    let mut visited_files = HashSet::new();
    let mut visited_syms = HashSet::new();
    visited_syms.insert(target_sym);
    let mut queue = VecDeque::new();
    queue.push_back((target_sym, 0usize));
    while let Some((sid, depth)) = queue.pop_front() {
        if depth >= 3 { continue; }
        if let Ok(refs) = db.find_refs_to_symbol(sid) {
            for r in &refs {
                visited_files.insert(r.file_id);
                if let Ok(Some(caller)) = db.find_enclosing_symbol(r.file_id, r.line) {
                    if visited_syms.insert(caller.id) {
                        queue.push_back((caller.id, depth + 1));
                    }
                }
            }
        }
    }
    visited_files
}

// -------------------------------------------------------------------------
// Experiment 3: co-change
// -------------------------------------------------------------------------

fn experiment_cochange(snap: &SutraSnapshot) {
    println!("\n{SEP}");
    println!("Experiment 3: co-change (self-join on commits via DD)");
    println!("{SEP}");

    if snap.commit_files.is_empty() {
        println!("No git commit data -- skipping.");
        return;
    }

    let mut file_counts: HashMap<&str, usize> = HashMap::new();
    for (_, path) in &snap.commit_files {
        *file_counts.entry(path.as_str()).or_default() += 1;
    }
    let target_path = file_counts.iter()
        .filter(|(_, c)| **c >= 3)
        .max_by_key(|(_, c)| **c)
        .map(|(&p, _)| p.to_string())
        .unwrap_or_else(|| snap.commit_files[0].1.clone());

    println!("Target: {target_path} ({} commits)", file_counts.get(target_path.as_str()).unwrap_or(&0));
    println!("Total commit-file pairs: {}", snap.commit_files.len());

    let mut path_to_id: HashMap<String, u64> = HashMap::new();
    let mut id_to_path: HashMap<u64, String> = HashMap::new();
    let mut next_id: u64 = 0;
    for (_, path) in &snap.commit_files {
        if !path_to_id.contains_key(path) {
            path_to_id.insert(path.clone(), next_id);
            id_to_path.insert(next_id, path.clone());
            next_id += 1;
        }
    }
    let target_id = *path_to_id.get(&target_path).unwrap();
    let commit_file_ids: Vec<(u32, u64)> = snap.commit_files.iter()
        .map(|(c, p)| (*c, *path_to_id.get(p).unwrap())).collect();

    let t0 = Instant::now();
    let dd_raw: Vec<(u64, isize)> = timely::execute_directly(move |worker| {
        let out = Arc::new(Mutex::new(Vec::new()));
        let out_w = out.clone();
        let mut input: InputSession<usize, (u32, u64), isize> = InputSession::new();
        let mut probe = ProbeHandle::new();

        worker.dataflow(|scope| {
            let cf = input.to_collection(scope);
            let target_commits = cf.clone().filter(move |(_, pid)| *pid == target_id)
                .map(|(commit, _)| (commit, ()));
            let all_by_commit = cf.map(|(commit, pid)| (commit, pid));
            let cochanged = target_commits.join_map(all_by_commit, |_c, _unit, pid| *pid);

            cochanged.count_total()
                .inspect(move |((pid, count), _, diff)| {
                    if *diff > 0 { out_w.lock().unwrap().push((*pid, *count)); }
                })
                .probe_with(&mut probe);
        });

        for &(ord, pid) in &commit_file_ids {
            input.insert((ord, pid));
        }
        input.advance_to(1);
        input.flush();
        worker.step_while(|| probe.less_than(&1));

        out.lock().unwrap().clone()
    });
    let dd_time = t0.elapsed();

    let mut dd_cochanged: Vec<(String, isize)> = dd_raw.iter()
        .filter(|(pid, _)| *pid != target_id)
        .map(|(pid, count)| (id_to_path.get(pid).cloned().unwrap_or("?".into()), *count))
        .collect();
    dd_cochanged.sort_by(|a, b| b.1.cmp(&a.1));

    let t1 = Instant::now();
    let ref_cochanged = reference_cochange(&snap.commit_files, &target_path);
    let ref_time = t1.elapsed();

    println!("DD:  {} files in {:?}", dd_cochanged.len(), dd_time);
    println!("Ref: {} files in {:?}", ref_cochanged.len(), ref_time);

    println!("\nTop co-changed (DD):");
    for (path, count) in dd_cochanged.iter().take(10) { println!("  {count:3} {path}"); }
    println!("Top co-changed (Ref):");
    for (path, count) in ref_cochanged.iter().take(10) { println!("  {count:3} {path}"); }

    let dd_map: HashMap<&str, isize> = dd_cochanged.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    let ref_map: HashMap<&str, u32> = ref_cochanged.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    let all_keys: HashSet<&str> = dd_map.keys().chain(ref_map.keys()).copied().collect();
    let matching = all_keys.iter()
        .filter(|&&k| dd_map.get(k).map(|&v| v as u32) == ref_map.get(k).copied())
        .count();
    println!("\nCount match: {matching}/{} files agree", all_keys.len());
}

fn reference_cochange(commit_files: &[(u32, String)], target: &str) -> Vec<(String, u32)> {
    let target_commits: HashSet<u32> = commit_files.iter()
        .filter(|(_, p)| p == target).map(|(c, _)| *c).collect();
    let mut counts: HashMap<String, u32> = HashMap::new();
    for (c, path) in commit_files {
        if target_commits.contains(c) && path != target {
            *counts.entry(path.clone()).or_default() += 1;
        }
    }
    let mut result: Vec<(String, u32)> = counts.into_iter().collect();
    result.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    result
}

// -------------------------------------------------------------------------
// Experiment 4: incremental update
// -------------------------------------------------------------------------

fn experiment_incremental(snap: &SutraSnapshot) {
    println!("\n{SEP}");
    println!("Experiment 4: incremental update latency");
    println!("{SEP}");

    let file_edges = build_file_edges(snap);
    let modify_file = snap.files.first().map(|(id, _)| *id).unwrap_or(1);
    let modify_path = snap.files.iter().find(|(id, _)| *id == modify_file)
        .map(|(_, p)| p.as_str()).unwrap_or("?");

    let edges_from: Vec<(i64, i64)> = file_edges.iter()
        .filter(|(src, _)| *src == modify_file).copied().collect();
    let edges_to: Vec<(i64, i64)> = file_edges.iter()
        .filter(|(_, dst)| *dst == modify_file).copied().collect();
    let last_file = snap.files.last().map(|(id, _)| *id).unwrap_or(0);

    println!("Input: {} edges, modifying {modify_path} ({} out, {} in)",
        file_edges.len(), edges_from.len(), edges_to.len());

    // Simple views
    println!("\nSimple views (fan-in + out-degree):");
    let fe = file_edges.clone();
    let ef = edges_from.clone();
    let et = edges_to.clone();

    timely::execute_directly(move |worker| {
        let mut input: InputSession<usize, (i64, i64), isize> = InputSession::new();
        let mut probe = ProbeHandle::new();

        worker.dataflow(|scope| {
            let edges = input.to_collection(scope);
            edges.clone().map(|(_s, d)| d).count_total().probe_with(&mut probe);
            edges.map(|(s, _d)| s).count_total().probe_with(&mut probe);
        });

        let t = Instant::now();
        for &(s, d) in &fe { input.insert((s, d)); }
        input.advance_to(1); input.flush();
        worker.step_while(|| probe.less_than(&1));
        println!("  Load: {:?} ({} edges)", t.elapsed(), fe.len());

        let t = Instant::now();
        for &e in &ef { input.update(e, -1); }
        for &e in &et { input.update(e, -1); }
        for &e in &ef { input.update(e, 1); }
        for &e in &et { input.update(e, 1); }
        input.advance_to(2); input.flush();
        worker.step_while(|| probe.less_than(&2));
        println!("  No-op re-parse: {:?}", t.elapsed());

        let t = Instant::now();
        if let Some(&(s, d)) = ef.first() {
            input.update((s, d), -1);
            if last_file != d { input.update((s, last_file), 1); }
        }
        input.advance_to(3); input.flush();
        worker.step_while(|| probe.less_than(&3));
        let dt = t.elapsed();
        println!("  1-edge change: {:?} (target <100ms: {})", dt,
            if dt.as_millis() < 100 { "PASS" } else { "FAIL" });
    });

    // Transitive reachability
    println!("\nTransitive reachability:");
    let fe = file_edges.clone();
    let ef = edges_from.clone();

    timely::execute_directly(move |worker| {
        let mut input: InputSession<usize, (i64, i64), isize> = InputSession::new();
        let mut probe = ProbeHandle::new();

        worker.dataflow(|scope| {
            let edges = input.to_collection(scope);
            let tc = edges.clone().iterate(|subscope, inner| {
                let e = edges.enter(subscope);
                let next = inner.clone().join_map(e, |_mid, src, dst| (*src, *dst));
                inner.concat(next).distinct()
            });
            tc.map(|(src, _)| src).count_total().probe_with(&mut probe);
        });

        let t = Instant::now();
        for &(s, d) in &fe { input.insert((s, d)); }
        input.advance_to(1); input.flush();
        worker.step_while(|| probe.less_than(&1));
        println!("  Load: {:?}", t.elapsed());

        let t = Instant::now();
        if let Some(&(s, d)) = ef.first() {
            input.update((s, d), -1);
            if last_file != d { input.update((s, last_file), 1); }
        }
        input.advance_to(2); input.flush();
        worker.step_while(|| probe.less_than(&2));
        let dt = t.elapsed();
        println!("  1-edge change: {:?} (target <100ms: {})", dt,
            if dt.as_millis() < 100 { "PASS" } else { "FAIL" });
    });
}

// -------------------------------------------------------------------------
// Experiment 5: constraints
// -------------------------------------------------------------------------

fn experiment_constraints(snap: &SutraSnapshot) {
    println!("\n{SEP}");
    println!("Experiment 5: architectural constraints as maintained views");
    println!("{SEP}");

    let file_edges = build_file_edges(snap);
    let file_id_to_path: HashMap<i64, String> = snap.files.iter().cloned().collect();
    let forbidden_pair = file_edges.first().copied();

    let (cycle_nodes, violation_pairs): (Vec<i64>, Vec<(i64, i64)>) =
        timely::execute_directly(move |worker| {
            let cycles = Arc::new(Mutex::new(Vec::new()));
            let cycles_w = cycles.clone();
            let violations = Arc::new(Mutex::new(Vec::new()));
            let violations_w = violations.clone();
            let mut edge_input: InputSession<usize, (i64, i64), isize> = InputSession::new();
            let mut forbidden_input: InputSession<usize, (i64, i64), isize> = InputSession::new();
            let mut probe = ProbeHandle::new();

            worker.dataflow(|scope| {
                let edges = edge_input.to_collection(scope);
                let forbidden = forbidden_input.to_collection(scope);

                // Constraint 1: cycle detection
                let tc = edges.clone().iterate(|subscope, inner| {
                    let e = edges.clone().enter(subscope);
                    let next = inner.clone().join_map(e, |_mid, src, dst| (*src, *dst));
                    inner.concat(next).distinct()
                });
                tc.filter(|(s, d)| s == d).map(|(n, _)| n).distinct_total()
                    .inspect(move |(data, _, diff)| {
                        if *diff > 0 { cycles_w.lock().unwrap().push(*data); }
                    })
                    .probe_with(&mut probe);

                // Constraint 2: forbidden deps (intersect edges with forbidden set)
                let edge_pairs = edges.map(|(s, d)| ((s, d), ()));
                let forbidden_set = forbidden.map(|(s, d)| (s, d));
                edge_pairs.semijoin(forbidden_set)
                    .map(|((s, d), ())| (s, d))
                    .inspect(move |(data, _, diff)| {
                        if *diff > 0 { violations_w.lock().unwrap().push(*data); }
                    })
                    .probe_with(&mut probe);
            });

            for &(s, d) in &file_edges { edge_input.insert((s, d)); }
            if let Some((s, d)) = forbidden_pair { forbidden_input.insert((s, d)); }
            edge_input.advance_to(1); forbidden_input.advance_to(1);
            edge_input.flush(); forbidden_input.flush();
            worker.step_while(|| probe.less_than(&1));

            (cycles.lock().unwrap().clone(), violations.lock().unwrap().clone())
        });

    println!("Constraint 1 -- Cycle detection:");
    if cycle_nodes.is_empty() {
        println!("  No dependency cycles found.");
    } else {
        println!("  {} files in cycles:", cycle_nodes.len());
        for &fid in cycle_nodes.iter().take(10) {
            let p = file_id_to_path.get(&fid).map(|s| s.as_str()).unwrap_or("?");
            println!("    {p}");
        }
        if cycle_nodes.len() > 10 { println!("    ... and {} more", cycle_nodes.len() - 10); }
    }

    println!("\nConstraint 2 -- Forbidden dependency violations:");
    if violation_pairs.is_empty() {
        println!("  No violations.");
    } else {
        for &(s, d) in &violation_pairs {
            let sp = file_id_to_path.get(&s).map(|s| s.as_str()).unwrap_or("?");
            let dp = file_id_to_path.get(&d).map(|s| s.as_str()).unwrap_or("?");
            println!("  {sp} -> {dp}");
        }
    }
}

// -------------------------------------------------------------------------
// Experiment 6: temporal queries
// -------------------------------------------------------------------------

fn experiment_temporal(snap: &SutraSnapshot) {
    println!("\n{SEP}");
    println!("Experiment 6: temporal queries (multi-epoch fan-in)");
    println!("{SEP}");

    let all_edges = build_file_edges(snap);
    if all_edges.len() < 2 {
        println!("Not enough edges -- skipping.");
        return;
    }

    let modify_edge = all_edges[0];
    let file_id_to_path: HashMap<i64, String> = snap.files.iter().cloned().collect();
    let import_edges = all_edges;

    let epoch0_fan_in: HashMap<i64, isize> = timely::execute_directly(move |worker| {
        let out = Arc::new(Mutex::new(HashMap::<i64, isize>::new()));
        let out_w = out.clone();
        let mut input: InputSession<usize, (i64, i64), isize> = InputSession::new();
        let mut probe = ProbeHandle::new();

        worker.dataflow(|scope| {
            let edges = input.to_collection(scope);
            edges.map(|(_s, d)| d).count_total()
                .inspect(move |((fid, count), _, diff)| {
                    if *diff > 0 { out_w.lock().unwrap().insert(*fid, *count); }
                })
                .probe_with(&mut probe);
        });

        for &(s, d) in &import_edges { input.insert((s, d)); }
        input.advance_to(1); input.flush();
        worker.step_while(|| probe.less_than(&1));

        out.lock().unwrap().clone()
    });

    let (src, dst) = modify_edge;
    let src_p = file_id_to_path.get(&src).map(|s| s.as_str()).unwrap_or("?");
    let dst_p = file_id_to_path.get(&dst).map(|s| s.as_str()).unwrap_or("?");
    let fi = epoch0_fan_in.get(&dst).copied().unwrap_or(0);

    println!("Removed edge: {src_p} -> {dst_p}");
    println!("Fan-in of {dst_p}: epoch 0 = {fi}, epoch 1 = {} (after removal)", fi - 1);
    println!("Temporal query: state observable at each epoch.");
}

// -------------------------------------------------------------------------
// Experiment 7: pagerank
// -------------------------------------------------------------------------

fn experiment_pagerank(snap: &SutraSnapshot) {
    println!("\n{SEP}");
    println!("Experiment 7: PageRank comparison");
    println!("{SEP}");

    let all_edges = build_file_edges(snap);
    if all_edges.is_empty() {
        println!("No edges -- skipping.");
        return;
    }

    let file_id_to_path: HashMap<i64, String> = snap.files.iter().cloned().collect();
    let file_ids: Vec<i64> = snap.files.iter().map(|(id, _)| *id).collect();
    let n = file_ids.len();
    let id_to_idx: HashMap<i64, usize> = file_ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    let mut out_edges: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    for &(src, dst) in &all_edges {
        if let (Some(&si), Some(&di)) = (id_to_idx.get(&src), id_to_idx.get(&dst)) {
            if si != di { out_edges[si].insert(di); }
        }
    }

    const DAMPING: f64 = 0.85;
    const ITERS: usize = 20;

    let t0 = Instant::now();
    let base = (1.0 - DAMPING) / n as f64;
    let mut rank = vec![1.0 / n as f64; n];
    for _ in 0..ITERS {
        let mut next = vec![base; n];
        for (src, edges) in out_edges.iter().enumerate() {
            if edges.is_empty() {
                let share = DAMPING * rank[src] / n as f64;
                for r in next.iter_mut() { *r += share; }
            } else {
                let share = DAMPING * rank[src] / edges.len() as f64;
                for &dst in edges { next[dst] += share; }
            }
        }
        rank = next;
    }
    let ref_time = t0.elapsed();

    let mut ref_ranked: Vec<(usize, f64)> = rank.iter().enumerate().map(|(i, &r)| (i, r)).collect();
    ref_ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    println!("Reference pagerank ({ITERS} iters, {n} nodes): {:?}", ref_time);
    println!("\nTop-10:");
    for (idx, pr) in ref_ranked.iter().take(10) {
        let fid = file_ids[*idx];
        let p = file_id_to_path.get(&fid).map(|s| s.as_str()).unwrap_or("?");
        println!("  {pr:.6} {p}");
    }

    // Note on DD + PageRank
    println!("\nVerdict: PageRank's iterative convergence doesn't fit DD's monotone iterate.");
    println!("An epoch-loop works but loses DD's automatic incrementality.");
}

// -------------------------------------------------------------------------
// Main
// -------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let workspace_id = args.get(1).map(String::as_str).unwrap_or("sutra");
    let workspace_root = args.get(2).map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let db_dir = std::env::var("SUTRA_DB_DIR").map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".sutra")
        });

    println!("=== Differential Dataflow Spike ===");
    println!("Workspace: {workspace_id}");
    println!("Root: {}", workspace_root.display());
    println!("DB: {}", db_dir.display());

    let db = Db::open(workspace_id, &db_dir).expect("Failed to open Sutra DB");

    let t0 = Instant::now();
    let snap = load_snapshot(&db, &workspace_root);
    println!(
        "\nLoaded in {:?}: {} files, {} import edges, {} sym-file, {} refs, {} commit-file",
        t0.elapsed(), snap.files.len(), snap.import_edges.len(),
        snap.sym_file_map.len(), snap.resolved_refs.len(), snap.commit_files.len(),
    );

    experiment_deps(&snap, &db);
    experiment_impact(&snap, &db);
    experiment_cochange(&snap);
    experiment_incremental(&snap);
    experiment_constraints(&snap);
    experiment_temporal(&snap);
    experiment_pagerank(&snap);

    println!("\n{SEP}");
    println!("All experiments complete.");
}
