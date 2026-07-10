pub mod calls;
pub mod change_signals;
pub mod cochange;
pub mod commit_manifest;
pub mod components;
pub mod constraints;
pub mod conventions;
pub mod dead;
pub mod deps;
pub mod diff_impact;
pub mod duplicates;
pub mod explore;
pub mod file_health;
pub mod find;
pub mod grep;
pub mod health;
pub mod help;
pub mod hotspots;
pub mod impact;
pub mod lessons;
pub mod map;
pub mod orient;
pub mod outline;
pub mod parse;
pub mod pr_risk;
pub mod provenance;
pub mod read;
pub mod refs;
pub mod remember;
pub mod resolve;
pub mod review;
pub mod scoring;
pub mod similar;
pub mod symbol_diff;
pub mod tools_meta;
pub mod trace;
pub mod trend;
pub mod winnow;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;
use crate::workspace::WorkspaceEntry;

pub fn get_or_open_db(
    cache: &Mutex<HashMap<String, Arc<Db>>>,
    workspace: &WorkspaceEntry,
    db_dir: &Path,
) -> Result<Arc<Db>> {
    let mut map = cache.lock();
    if let Some(db) = map.get(&workspace.id) {
        return Ok(Arc::clone(db));
    }
    let db = Arc::new(Db::open_for_workspace(workspace, db_dir)?);
    map.insert(workspace.id.clone(), Arc::clone(&db));
    Ok(db)
}

pub struct ToolContext {
    db: Arc<Db>,
    workspace_root: PathBuf,
    annotate_freshness: bool,
    response_freshness: serde_json::Value,
}

impl ToolContext {
    pub fn new(
        db: Arc<Db>,
        workspace_root: PathBuf,
        annotate_freshness: bool,
        response_freshness: serde_json::Value,
    ) -> Self {
        Self {
            db,
            workspace_root,
            annotate_freshness,
            response_freshness,
        }
    }

    pub fn for_test(db: Arc<Db>, workspace_root: PathBuf) -> Self {
        Self::new(db, workspace_root, false, serde_json::Value::Null)
    }

    pub fn for_test_with_freshness(db: Arc<Db>, workspace_root: PathBuf) -> Self {
        Self::new(db, workspace_root, true, serde_json::Value::Null)
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn is_stale(&self) -> bool {
        self.response_freshness
            .get("is_stale")
            .and_then(|v| v.as_bool())
            == Some(true)
    }

    pub fn wrap(self, mut result: serde_json::Value) -> serde_json::Value {
        if let Some(obj) = result.as_object_mut()
            && let Some(f_obj) = self.response_freshness.as_object()
        {
            for (k, v) in f_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        result
    }

    pub fn freshness_annotator(&self) -> Option<FreshnessAnnotator<'_>> {
        if self.annotate_freshness {
            Some(FreshnessAnnotator::new(&self.workspace_root))
        } else {
            None
        }
    }
}
