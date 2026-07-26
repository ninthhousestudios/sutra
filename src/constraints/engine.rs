use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{Result, SutraError};

use glob::{MatchOptions, Pattern};

use super::worker::{self, Command, Response, WorkerHandle};
use super::{ConstraintViolation, Cycle, DdDelta, DdFacts};
use crate::rules::ForbiddenDep;

pub struct DdEngine {
    state: Mutex<DdState>,
    idle_timeout: Duration,
}

enum DdState {
    Cold,
    Loaded {
        edges: Vec<(i64, i64)>,
        forbidden_pairs: Vec<(i64, i64)>,
    },
    Warm {
        handle: WorkerHandle,
        edges: Vec<(i64, i64)>,
        forbidden_pairs: Vec<(i64, i64)>,
        last_query: Instant,
    },
}

impl DdEngine {
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(DdState::Cold),
            idle_timeout,
        }
    }

    pub fn ingest(&self, facts: DdFacts) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            DdState::Cold => {
                *state = DdState::Loaded {
                    edges: facts.import_edges,
                    forbidden_pairs: Vec::new(),
                };
                Ok(())
            }
            DdState::Loaded { .. } | DdState::Warm { .. } => {
                Err(SutraError::Internal("DD engine already loaded".into()))
            }
        }
    }

    /// Bring the engine's graph in line with the index's current edge set,
    /// applying the difference as a DD delta.
    ///
    /// A shared engine outlives any single request, and its graph goes stale the
    /// moment the index is rewritten. Staleness here is not merely a missing
    /// edge: `Db::replace_file_data` deletes and re-inserts the `files` row on
    /// every reparse, and the id column is `AUTOINCREMENT`, so a reparsed file
    /// gets a *brand-new* id and the cached graph keeps only dead ones. Forbidden
    /// pairs are resolved fresh against live ids each evaluation, so the
    /// violations semijoin intersects two disjoint id spaces and every
    /// DD-backed constraint silently reports zero (sutra/297).
    ///
    /// Callers sync before querying rather than relying on an invalidation
    /// signal reaching them — the previous `invalidate()` hook had no callers,
    /// which is exactly why the staleness went unnoticed.
    pub fn sync_edges(&self, edges: &[(i64, i64)]) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        // `Db::import_edges` yields one row per import statement, so an edge
        // repeats when a file imports several symbols from the same target.
        // The graph is a set: deduplicating here is what keeps a later
        // retraction exact.
        let desired: HashSet<(i64, i64)> = edges.iter().copied().collect();

        let current: HashSet<(i64, i64)> = match &*state {
            DdState::Cold => {
                let mut edges: Vec<(i64, i64)> = desired.into_iter().collect();
                edges.sort_unstable();
                *state = DdState::Loaded {
                    edges,
                    forbidden_pairs: Vec::new(),
                };
                return Ok(());
            }
            DdState::Loaded { edges, .. } | DdState::Warm { edges, .. } => {
                edges.iter().copied().collect()
            }
        };

        let added: Vec<(i64, i64)> = desired.difference(&current).copied().collect();
        let removed: Vec<(i64, i64)> = current.difference(&desired).copied().collect();
        if added.is_empty() && removed.is_empty() {
            return Ok(());
        }
        apply_edge_delta(
            &mut state,
            DdDelta {
                added_edges: added,
                removed_edges: removed,
            },
        )
    }

    pub fn update(&self, delta: DdDelta) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        apply_edge_delta(&mut state, delta)
    }

    pub fn query_cycles(&self) -> Result<Vec<Cycle>> {
        let mut state = self.state.lock().unwrap();
        ensure_warm(&mut state)?;

        match &mut *state {
            DdState::Warm {
                handle, last_query, ..
            } => {
                handle
                    .send(Command::QueryCycles)
                    .map_err(SutraError::Internal)?;

                match handle.recv() {
                    Ok(Response::Cycles(sccs)) => {
                        *last_query = Instant::now();
                        let cycles = sccs
                            .into_iter()
                            .map(|scc| {
                                let mut file_ids: Vec<i64> = scc.into_iter().collect();
                                file_ids.sort();
                                Cycle { file_ids }
                            })
                            .collect();
                        Ok(cycles)
                    }
                    Ok(Response::Error(e)) => Err(SutraError::Internal(e)),
                    _ => Err(SutraError::Internal("unexpected response".into())),
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn query_blast_radius(&self, node: i64) -> Result<usize> {
        let mut state = self.state.lock().unwrap();
        ensure_warm(&mut state)?;

        match &mut *state {
            DdState::Warm {
                handle, last_query, ..
            } => {
                handle
                    .send(Command::QueryBlastRadius(node))
                    .map_err(SutraError::Internal)?;

                match handle.recv() {
                    Ok(Response::BlastRadius(count)) => {
                        *last_query = Instant::now();
                        Ok(count)
                    }
                    Ok(Response::Error(e)) => Err(SutraError::Internal(e)),
                    _ => Err(SutraError::Internal("unexpected response".into())),
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn query_blast_radius_all(&self) -> Result<HashMap<i64, usize>> {
        let mut state = self.state.lock().unwrap();
        ensure_warm(&mut state)?;

        match &mut *state {
            DdState::Warm {
                handle, last_query, ..
            } => {
                handle
                    .send(Command::QueryBlastRadiusAll)
                    .map_err(SutraError::Internal)?;

                match handle.recv() {
                    Ok(Response::BlastRadiusAll(map)) => {
                        *last_query = Instant::now();
                        Ok(map)
                    }
                    Ok(Response::Error(e)) => Err(SutraError::Internal(e)),
                    _ => Err(SutraError::Internal("unexpected response".into())),
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn set_forbidden_pairs(&self, pairs: Vec<(i64, i64)>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        match &mut *state {
            DdState::Cold => Err(SutraError::Internal(
                "cannot set forbidden pairs before ingest".into(),
            )),
            DdState::Loaded {
                forbidden_pairs, ..
            } => {
                *forbidden_pairs = pairs;
                Ok(())
            }
            DdState::Warm {
                handle,
                forbidden_pairs,
                last_query,
                ..
            } => {
                handle
                    .send(Command::SetForbiddenPairs(pairs.clone()))
                    .map_err(SutraError::Internal)?;
                match handle.recv() {
                    Ok(Response::Ok) => {
                        *forbidden_pairs = pairs;
                        *last_query = Instant::now();
                        Ok(())
                    }
                    Ok(Response::Error(e)) => Err(SutraError::Internal(e)),
                    _ => Err(SutraError::Internal("unexpected response".into())),
                }
            }
        }
    }

    pub fn query_violations(&self) -> Result<Vec<(i64, i64)>> {
        let mut state = self.state.lock().unwrap();
        ensure_warm(&mut state)?;
        match &mut *state {
            DdState::Warm {
                handle, last_query, ..
            } => {
                handle
                    .send(Command::QueryViolations)
                    .map_err(SutraError::Internal)?;
                match handle.recv() {
                    Ok(Response::Violations(v)) => {
                        *last_query = Instant::now();
                        Ok(v)
                    }
                    Ok(Response::Error(e)) => Err(SutraError::Internal(e)),
                    _ => Err(SutraError::Internal("unexpected response".into())),
                }
            }
            _ => unreachable!(),
        }
    }

    #[deprecated(note = "use set_forbidden_pairs + query_violations instead")]
    pub fn query_forbidden_deps(
        &self,
        rules: &[ForbiddenDep],
        paths: &HashMap<i64, String>,
    ) -> Result<Vec<ConstraintViolation>> {
        if rules.is_empty() {
            return Ok(vec![]);
        }

        let mut compiled = Vec::with_capacity(rules.len());
        for r in rules {
            let from = Pattern::new(&r.from).map_err(|e| {
                SutraError::Internal(format!(
                    "invalid glob in forbidden_deps from='{}': {e}",
                    r.from
                ))
            })?;
            let to = Pattern::new(&r.to).map_err(|e| {
                SutraError::Internal(format!("invalid glob in forbidden_deps to='{}': {e}", r.to))
            })?;
            compiled.push((from, to, r));
        }

        let state = self.state.lock().unwrap();
        let edges = match &*state {
            DdState::Cold => {
                return Err(SutraError::Internal(
                    "DD engine is cold — ingest facts first".into(),
                ));
            }
            DdState::Loaded { edges, .. } | DdState::Warm { edges, .. } => edges,
        };

        let opts = MatchOptions {
            require_literal_separator: true,
            ..MatchOptions::default()
        };

        let mut violations = Vec::new();
        for &(src, dst) in edges {
            let src_path = match paths.get(&src) {
                Some(p) => p.as_str(),
                None => continue,
            };
            let dst_path = match paths.get(&dst) {
                Some(p) => p.as_str(),
                None => continue,
            };
            for (from_pat, to_pat, rule) in &compiled {
                if from_pat.matches_with(src_path, opts) && to_pat.matches_with(dst_path, opts) {
                    violations.push(ConstraintViolation {
                        from_id: src,
                        to_id: dst,
                        rule_from: rule.from.clone(),
                        rule_to: rule.to.clone(),
                    });
                }
            }
        }
        violations.sort_by_key(|v| (v.from_id, v.to_id));
        Ok(violations)
    }

    pub fn evict_if_idle(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        match &*state {
            DdState::Cold | DdState::Loaded { .. } => false,
            DdState::Warm { last_query, .. } => {
                if last_query.elapsed() >= self.idle_timeout {
                    let (edges, forbidden_pairs) =
                        match std::mem::replace(&mut *state, DdState::Cold) {
                            DdState::Warm {
                                edges,
                                forbidden_pairs,
                                ..
                            } => (edges, forbidden_pairs),
                            _ => unreachable!(),
                        };
                    *state = DdState::Loaded {
                        edges,
                        forbidden_pairs,
                    };
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn is_warm(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), DdState::Warm { .. })
    }

    pub fn is_loaded(&self) -> bool {
        matches!(
            &*self.state.lock().unwrap(),
            DdState::Loaded { .. } | DdState::Warm { .. }
        )
    }
}

/// The tracked edge list after `delta`. Removal wins over addition for an edge
/// named in both, preserving the push-then-retain order this had when it lived
/// inside `update`.
fn tracked(edges: &[(i64, i64)], delta: &DdDelta) -> Vec<(i64, i64)> {
    let removed: HashSet<(i64, i64)> = delta.removed_edges.iter().copied().collect();
    edges
        .iter()
        .chain(delta.added_edges.iter())
        .filter(|e| !removed.contains(e))
        .copied()
        .collect()
}

/// Apply an edge delta to whichever state the engine is in: forwarded to the
/// worker when warm, recorded against the tracked edge list either way. The
/// tracked list is only committed once the worker has acknowledged, so a failed
/// send leaves the two views in agreement.
fn apply_edge_delta(state: &mut DdState, delta: DdDelta) -> Result<()> {
    match state {
        DdState::Cold => Err(SutraError::Internal("DD engine is cold".into())),
        DdState::Loaded { edges, .. } => {
            *edges = tracked(edges, &delta);
            Ok(())
        }
        DdState::Warm {
            handle,
            edges,
            last_query,
            ..
        } => {
            let next = tracked(edges, &delta);
            handle
                .send(Command::Update {
                    added: delta.added_edges,
                    removed: delta.removed_edges,
                })
                .map_err(SutraError::Internal)?;

            match handle.recv() {
                Ok(Response::Ok) => {}
                Ok(Response::Error(e)) => return Err(SutraError::Internal(e)),
                _ => return Err(SutraError::Internal("unexpected response".into())),
            }

            *edges = next;
            *last_query = Instant::now();
            Ok(())
        }
    }
}

fn ensure_warm(state: &mut DdState) -> Result<()> {
    if matches!(&*state, DdState::Cold) {
        return Err(SutraError::Internal(
            "DD engine is cold — ingest facts first".into(),
        ));
    }
    if matches!(&*state, DdState::Loaded { .. }) {
        let (edges, forbidden_pairs) = match std::mem::replace(state, DdState::Cold) {
            DdState::Loaded {
                edges,
                forbidden_pairs,
            } => (edges, forbidden_pairs),
            _ => unreachable!(),
        };
        let handle = worker::spawn_worker();
        let rollback = |state: &mut DdState, edges, forbidden_pairs| {
            *state = DdState::Loaded {
                edges,
                forbidden_pairs,
            };
        };
        if let Err(e) = handle.send(Command::Ingest(edges.clone())) {
            rollback(state, edges, forbidden_pairs);
            return Err(SutraError::Internal(e));
        }
        match handle.recv() {
            Ok(Response::Ok) => {}
            Ok(Response::Error(e)) => {
                rollback(state, edges, forbidden_pairs);
                return Err(SutraError::Internal(e));
            }
            _ => {
                rollback(state, edges, forbidden_pairs);
                return Err(SutraError::Internal("unexpected response".into()));
            }
        }
        if !forbidden_pairs.is_empty() {
            if let Err(e) = handle.send(Command::SetForbiddenPairs(forbidden_pairs.clone())) {
                rollback(state, edges, forbidden_pairs);
                return Err(SutraError::Internal(e));
            }
            match handle.recv() {
                Ok(Response::Ok) => {}
                Ok(Response::Error(e)) => {
                    rollback(state, edges, forbidden_pairs);
                    return Err(SutraError::Internal(e));
                }
                _ => {
                    rollback(state, edges, forbidden_pairs);
                    return Err(SutraError::Internal("unexpected response".into()));
                }
            }
        }
        *state = DdState::Warm {
            handle,
            edges,
            forbidden_pairs,
            last_query: Instant::now(),
        };
    }
    Ok(())
}

impl Drop for DdEngine {
    fn drop(&mut self) {
        *self.state.get_mut().unwrap() = DdState::Cold;
    }
}
