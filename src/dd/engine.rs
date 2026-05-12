use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{Result, SutraError};

use super::worker::{self, Command, Response, WorkerHandle};
use super::{Cycle, DdDelta, DdFacts};

pub struct DdEngine {
    state: Mutex<DdState>,
    idle_timeout: Duration,
}

enum DdState {
    Cold,
    Warm {
        handle: WorkerHandle,
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
        if matches!(&*state, DdState::Warm { .. }) {
            return Err(SutraError::Internal("DD engine already warm".into()));
        }

        let handle = worker::spawn_worker();
        handle
            .send(Command::Ingest(facts.import_edges))
            .map_err(|e| SutraError::Internal(e))?;

        match handle.recv() {
            Ok(Response::Ok) => {}
            Ok(Response::Error(e)) => return Err(SutraError::Internal(e)),
            _ => return Err(SutraError::Internal("unexpected response".into())),
        }

        *state = DdState::Warm {
            handle,
            last_query: Instant::now(),
        };
        Ok(())
    }

    pub fn update(&self, delta: DdDelta) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        match &mut *state {
            DdState::Cold => Err(SutraError::Internal("DD engine is cold".into())),
            DdState::Warm {
                handle, last_query, ..
            } => {
                handle
                    .send(Command::Update {
                        added: delta.added_edges,
                        removed: delta.removed_edges,
                    })
                    .map_err(|e| SutraError::Internal(e))?;

                match handle.recv() {
                    Ok(Response::Ok) => {}
                    Ok(Response::Error(e)) => return Err(SutraError::Internal(e)),
                    _ => return Err(SutraError::Internal("unexpected response".into())),
                }

                *last_query = Instant::now();
                Ok(())
            }
        }
    }

    pub fn query_cycles(&self) -> Result<Vec<Cycle>> {
        let mut state = self.state.lock().unwrap();
        match &mut *state {
            DdState::Cold => Err(SutraError::Internal(
                "DD engine is cold — ingest facts first".into(),
            )),
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
        }
    }

    pub fn evict_if_idle(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        match &*state {
            DdState::Cold => false,
            DdState::Warm { last_query, .. } => {
                if last_query.elapsed() >= self.idle_timeout {
                    *state = DdState::Cold;
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
}

impl Drop for DdEngine {
    fn drop(&mut self) {
        *self.state.get_mut().unwrap() = DdState::Cold;
    }
}
