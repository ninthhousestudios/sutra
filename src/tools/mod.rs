pub mod advisory;
pub mod calls;
pub mod changed_symbols;
pub mod check;
pub mod constraints;
pub mod deps;
pub mod dup_exists;
pub mod explore;
pub mod explore_lexical;
pub mod find;
pub mod firings;
pub mod help;
pub mod impact;
pub mod lessons;
pub mod lookup;
pub mod map;
pub mod orphans;
pub mod outline;
pub mod parse;
pub mod read;
pub mod refs;
pub mod release_pack;
pub mod remember;
pub mod review;
pub mod sibling_pattern;
pub mod similar;
pub mod symbol_diff;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;
use crate::workspace::WorkspaceEntry;

/// Say how many calls bind to a symbol by name alone (sutra/513): dot-calls
/// with a std method name and an unknown receiver type. Caller lists and
/// counts leave them out, so a tool reporting callers adds
/// `name_only_callers` and a note naming the next step; nothing when none.
pub(crate) fn add_name_only_callers(
    db: &Db,
    symbol_id: i64,
    short_name: &str,
    result: &mut serde_json::Value,
) -> Result<()> {
    let count = db.count_name_only_calls_to_symbol(symbol_id)?;
    if count > 0 {
        result["name_only_callers"] = serde_json::json!(count);
        result["name_only_note"] = serde_json::json!(format!(
            "{count} `.{short_name}()` calls are not counted: std has a method of that \
             name and the receiver type is unknown, so they may not call this. \
             Check the receivers with `rg '\\.{short_name}\\('`."
        ));
    }
    Ok(())
}

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
