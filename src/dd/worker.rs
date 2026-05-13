use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::thread;

use crossbeam_channel::{Receiver, Sender};
use differential_dataflow::input::Input;
use differential_dataflow::operators::CountTotal;
use differential_dataflow::operators::iterate::Iterate;
use timely::dataflow::operators::probe::Handle as ProbeHandle;

pub(super) enum Command {
    Ingest(Vec<(i64, i64)>),
    Update {
        added: Vec<(i64, i64)>,
        removed: Vec<(i64, i64)>,
    },
    QueryCycles,
    QueryBlastRadius(i64),
    QueryBlastRadiusAll,
    Shutdown,
}

pub(super) enum Response {
    Ok,
    Cycles(Vec<HashSet<i64>>),
    BlastRadius(usize),
    BlastRadiusAll(HashMap<i64, usize>),
    #[expect(dead_code)]
    Error(String),
}

pub(super) struct WorkerHandle {
    cmd_tx: Sender<Command>,
    resp_rx: Receiver<Response>,
    thread: Option<thread::JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn send(&self, cmd: Command) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .map_err(|e| format!("worker send failed: {e}"))
    }

    pub fn recv(&self) -> Result<Response, String> {
        self.resp_rx
            .recv()
            .map_err(|e| format!("worker recv failed: {e}"))
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub(super) fn spawn_worker() -> WorkerHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded();

    let thread = thread::spawn(move || {
        run_worker(cmd_rx, resp_tx);
    });

    WorkerHandle {
        cmd_tx,
        resp_rx,
        thread: Some(thread),
    }
}

fn run_worker(cmd_rx: Receiver<Command>, resp_tx: Sender<Response>) {
    timely::execute_directly(move |worker| {
        let cycle_nodes = Rc::new(RefCell::new(HashSet::<i64>::new()));
        let cycle_nodes_inspect = cycle_nodes.clone();

        let edges_store = Rc::new(RefCell::new(HashSet::<(i64, i64)>::new()));

        let blast_counts = Rc::new(RefCell::new(HashMap::<i64, isize>::new()));
        let blast_counts_inspect = blast_counts.clone();

        let mut probe = ProbeHandle::new();
        let mut timestamp: usize = 0;

        let mut input = worker.dataflow::<usize, _, _>(|scope| {
            let (input, edges) = scope.new_collection::<(i64, i64), isize>();

            // Transitive closure via iterate
            let reachable = edges.clone().iterate(|scope, inner| {
                let edges_inner = edges.enter(scope);
                let inner_keep = inner.clone();

                // Extend: if (src, mid) ∈ inner and (mid, dst) ∈ edges, produce (src, dst)
                let extended = inner
                    .map(|(src, dst)| (dst, src))
                    .join_map(edges_inner, |_mid, src, to| (*src, *to));

                inner_keep.concat(extended).distinct()
            });

            // Self-loops in TC = cycle-participating nodes
            reachable
                .clone()
                .filter(|(src, dst)| src == dst)
                .map(|(s, _)| s)
                .distinct()
                .inspect(move |(node, _time, diff)| {
                    let mut buf = cycle_nodes_inspect.borrow_mut();
                    if *diff > 0 {
                        buf.insert(*node);
                    } else if *diff < 0 {
                        buf.remove(node);
                    }
                })
                .probe_with(&mut probe);

            reachable
                .filter(|(src, dst)| src != dst)
                .map(|(_, dst)| dst)
                .count_total()
                .inspect(move |((dst, count), _time, diff)| {
                    let mut map = blast_counts_inspect.borrow_mut();
                    if *diff > 0 {
                        map.insert(*dst, *count);
                    } else if *diff < 0 {
                        map.remove(dst);
                    }
                })
                .probe_with(&mut probe);

            input
        });

        loop {
            match cmd_rx.recv() {
                Ok(Command::Ingest(edges)) => {
                    {
                        let mut store = edges_store.borrow_mut();
                        for &edge in &edges {
                            store.insert(edge);
                            input.insert(edge);
                        }
                    }
                    timestamp += 1;
                    input.advance_to(timestamp);
                    input.flush();
                    worker.step_while(|| probe.less_than(input.time()));
                    let _ = resp_tx.send(Response::Ok);
                }
                Ok(Command::Update { added, removed }) => {
                    {
                        let mut store = edges_store.borrow_mut();
                        for &edge in &added {
                            store.insert(edge);
                            input.insert(edge);
                        }
                        for &edge in &removed {
                            store.remove(&edge);
                            input.remove(edge);
                        }
                    }
                    timestamp += 1;
                    input.advance_to(timestamp);
                    input.flush();
                    worker.step_while(|| probe.less_than(input.time()));
                    let _ = resp_tx.send(Response::Ok);
                }
                Ok(Command::QueryCycles) => {
                    let nodes = cycle_nodes.borrow();
                    if nodes.is_empty() {
                        let _ = resp_tx.send(Response::Cycles(vec![]));
                        continue;
                    }
                    let store = edges_store.borrow();
                    let sccs = compute_sccs(&nodes, &store);
                    let _ = resp_tx.send(Response::Cycles(sccs));
                }
                Ok(Command::QueryBlastRadius(node)) => {
                    let counts = blast_counts.borrow();
                    let count = counts.get(&node).copied().unwrap_or(0).max(0) as usize;
                    let _ = resp_tx.send(Response::BlastRadius(count));
                }
                Ok(Command::QueryBlastRadiusAll) => {
                    let counts = blast_counts.borrow();
                    let map = counts
                        .iter()
                        .map(|(&k, &v)| (k, v.max(0) as usize))
                        .collect();
                    let _ = resp_tx.send(Response::BlastRadiusAll(map));
                }
                Ok(Command::Shutdown) | Err(_) => break,
            }
        }
    });
}

// Kosaraju's algorithm for SCC detection on the subgraph of cycle-participating nodes.
fn compute_sccs(cycle_nodes: &HashSet<i64>, edges: &HashSet<(i64, i64)>) -> Vec<HashSet<i64>> {
    let mut forward: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut reverse: HashMap<i64, Vec<i64>> = HashMap::new();

    for &(src, dst) in edges {
        if cycle_nodes.contains(&src) && cycle_nodes.contains(&dst) {
            forward.entry(src).or_default().push(dst);
            reverse.entry(dst).or_default().push(src);
        }
    }

    // Phase 1: forward DFS, record finish order
    let mut visited = HashSet::new();
    let mut finish_order = Vec::new();
    for &node in cycle_nodes {
        if !visited.contains(&node) {
            dfs_finish(node, &forward, &mut visited, &mut finish_order);
        }
    }

    // Phase 2: reverse DFS in reverse finish order → each tree is an SCC
    let mut visited = HashSet::new();
    let mut sccs = Vec::new();
    for &node in finish_order.iter().rev() {
        if !visited.contains(&node) {
            let mut scc = HashSet::new();
            dfs_collect(node, &reverse, &mut visited, &mut scc);
            sccs.push(scc);
        }
    }
    sccs
}

fn dfs_finish(
    node: i64,
    graph: &HashMap<i64, Vec<i64>>,
    visited: &mut HashSet<i64>,
    finish_order: &mut Vec<i64>,
) {
    visited.insert(node);
    if let Some(neighbors) = graph.get(&node) {
        for &next in neighbors {
            if !visited.contains(&next) {
                dfs_finish(next, graph, visited, finish_order);
            }
        }
    }
    finish_order.push(node);
}

fn dfs_collect(
    node: i64,
    graph: &HashMap<i64, Vec<i64>>,
    visited: &mut HashSet<i64>,
    scc: &mut HashSet<i64>,
) {
    visited.insert(node);
    scc.insert(node);
    if let Some(neighbors) = graph.get(&node) {
        for &next in neighbors {
            if !visited.contains(&next) {
                dfs_collect(next, graph, visited, scc);
            }
        }
    }
}
