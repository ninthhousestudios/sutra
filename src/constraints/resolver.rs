use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use glob::{MatchOptions, Pattern};
use tracing::warn;

use crate::db::{ComponentRow, Db};
use crate::error::{Result, SutraError};
use crate::rules::{Constraint, ConstraintKind};

struct ResolvedCache {
    pairs: Vec<(i64, i64)>,
    membership_generation: Option<(i64, i64, String, i64, i64)>,
    input_hash: u64,
}

pub struct ConstraintResolver {
    cache: Option<ResolvedCache>,
}

impl Default for ConstraintResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ConstraintResolver {
    pub fn new() -> Self {
        Self { cache: None }
    }

    pub fn resolve(
        &mut self,
        constraints: &[Constraint],
        db: &Db,
        path_map: &HashMap<i64, &str>,
    ) -> Result<Vec<(i64, i64)>> {
        let current_gen = db.clustering_meta()?;
        let current_input_hash = compute_input_hash(constraints, path_map);

        if let Some(ref cache) = self.cache
            && cache.membership_generation == current_gen
            && cache.input_hash == current_input_hash
        {
            return Ok(cache.pairs.clone());
        }

        let components = db.all_components()?;
        let mut file_ids_cache: HashMap<String, Vec<i64>> = HashMap::new();
        let mut pairs = Vec::new();

        let glob_opts = MatchOptions {
            require_literal_separator: true,
            ..MatchOptions::default()
        };

        for constraint in constraints {
            match &constraint.kind {
                ConstraintKind::Boundary {
                    from_component,
                    to_component,
                } => {
                    let from_cid = find_component(&components, from_component);
                    let to_cid = find_component(&components, to_component);
                    let (Some(from_cid), Some(to_cid)) = (from_cid, to_cid) else {
                        warn!(
                            constraint_id = %constraint.id,
                            from = %from_component,
                            to = %to_component,
                            "skipping boundary constraint: component not found"
                        );
                        continue;
                    };
                    let from_files = file_ids_for(db, &mut file_ids_cache, &from_cid)?;
                    let to_files = file_ids_for(db, &mut file_ids_cache, &to_cid)?;
                    for &f in &from_files {
                        for &t in &to_files {
                            pairs.push((f, t));
                        }
                    }
                }
                ConstraintKind::ForbiddenDep { from, to } => {
                    let from_pat = Pattern::new(from).map_err(|e| {
                        SutraError::Internal(format!("invalid glob from='{from}': {e}"))
                    })?;
                    let to_pat = Pattern::new(to).map_err(|e| {
                        SutraError::Internal(format!("invalid glob to='{to}': {e}"))
                    })?;
                    let from_files: Vec<i64> = path_map
                        .iter()
                        .filter(|(_, p)| from_pat.matches_with(p, glob_opts))
                        .map(|(&id, _)| id)
                        .collect();
                    for &fid in &from_files {
                        for (&tid, tpath) in path_map {
                            if to_pat.matches_with(tpath, glob_opts) {
                                pairs.push((fid, tid));
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        pairs.sort();
        pairs.dedup();

        self.cache = Some(ResolvedCache {
            pairs: pairs.clone(),
            membership_generation: current_gen,
            input_hash: current_input_hash,
        });

        Ok(pairs)
    }

    pub fn invalidate(&mut self) {
        self.cache = None;
    }
}

fn file_ids_for(
    db: &Db,
    cache: &mut HashMap<String, Vec<i64>>,
    component_id: &str,
) -> Result<Vec<i64>> {
    if let Some(ids) = cache.get(component_id) {
        return Ok(ids.clone());
    }
    let ids = db.component_file_ids(component_id)?;
    cache.insert(component_id.to_string(), ids.clone());
    Ok(ids)
}

fn compute_input_hash(constraints: &[Constraint], path_map: &HashMap<i64, &str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut ids: Vec<&str> = constraints.iter().map(|c| c.id.as_str()).collect();
    ids.sort();
    for id in ids {
        id.hash(&mut hasher);
    }
    let mut entries: Vec<_> = path_map.iter().collect();
    entries.sort_by_key(|&(&k, _)| k);
    for (k, v) in entries {
        k.hash(&mut hasher);
        v.hash(&mut hasher);
    }
    hasher.finish()
}

fn find_component(components: &[ComponentRow], name_or_id: &str) -> Option<String> {
    components
        .iter()
        .find(|c| c.name == name_or_id || c.id == name_or_id)
        .map(|c| c.id.clone())
}
