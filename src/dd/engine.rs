use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
    invalidated: AtomicBool,
}

enum DdState {
    Cold,
    Loaded {
        edges: Vec<(i64, i64)>,
    },
    Warm {
        handle: WorkerHandle,
        edges: Vec<(i64, i64)>,
        last_query: Instant,
    },
}

impl DdEngine {
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(DdState::Cold),
            idle_timeout,
            invalidated: AtomicBool::new(false),
        }
    }

    pub fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Release);
    }

    pub fn is_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }

    pub fn ingest(&self, facts: DdFacts) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            DdState::Cold => {
                *state = DdState::Loaded {
                    edges: facts.import_edges,
                };
                Ok(())
            }
            DdState::Loaded { .. } | DdState::Warm { .. } => {
                Err(SutraError::Internal("DD engine already loaded".into()))
            }
        }
    }

    pub fn update(&self, delta: DdDelta) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        match &mut *state {
            DdState::Cold => Err(SutraError::Internal("DD engine is cold".into())),
            DdState::Loaded { edges } => {
                for edge in &delta.added_edges {
                    edges.push(*edge);
                }
                edges.retain(|e| !delta.removed_edges.contains(e));
                Ok(())
            }
            DdState::Warm {
                handle,
                edges,
                last_query,
            } => {
                handle
                    .send(Command::Update {
                        added: delta.added_edges.clone(),
                        removed: delta.removed_edges.clone(),
                    })
                    .map_err(|e| SutraError::Internal(e))?;

                match handle.recv() {
                    Ok(Response::Ok) => {}
                    Ok(Response::Error(e)) => return Err(SutraError::Internal(e)),
                    _ => return Err(SutraError::Internal("unexpected response".into())),
                }

                for edge in &delta.added_edges {
                    edges.push(*edge);
                }
                edges.retain(|e| !delta.removed_edges.contains(e));
                *last_query = Instant::now();
                Ok(())
            }
        }
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
                    .map_err(|e| SutraError::Internal(e))?;

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
                    .map_err(|e| SutraError::Internal(e))?;

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
                    .map_err(|e| SutraError::Internal(e))?;

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
            DdState::Loaded { edges } | DdState::Warm { edges, .. } => edges,
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
                    let edges = match std::mem::replace(&mut *state, DdState::Cold) {
                        DdState::Warm { edges, .. } => edges,
                        _ => unreachable!(),
                    };
                    *state = DdState::Loaded { edges };
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

fn ensure_warm(state: &mut DdState) -> Result<()> {
    if matches!(&*state, DdState::Cold) {
        return Err(SutraError::Internal(
            "DD engine is cold — ingest facts first".into(),
        ));
    }
    if matches!(&*state, DdState::Loaded { .. }) {
        let edges = match std::mem::replace(state, DdState::Cold) {
            DdState::Loaded { edges } => edges,
            _ => unreachable!(),
        };
        let handle = worker::spawn_worker();
        if let Err(e) = handle.send(Command::Ingest(edges.clone())) {
            *state = DdState::Loaded { edges };
            return Err(SutraError::Internal(e));
        }
        match handle.recv() {
            Ok(Response::Ok) => {}
            Ok(Response::Error(e)) => {
                *state = DdState::Loaded { edges };
                return Err(SutraError::Internal(e));
            }
            _ => {
                *state = DdState::Loaded { edges };
                return Err(SutraError::Internal("unexpected response".into()));
            }
        }
        *state = DdState::Warm {
            handle,
            edges,
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
