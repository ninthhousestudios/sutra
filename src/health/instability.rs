use std::collections::HashMap;

use crate::db::Db;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct ComponentInstability {
    pub ce: usize,
    pub ca: usize,
    pub instability: f64,
}

pub fn compute_component_instability(db: &Db) -> Result<HashMap<String, ComponentInstability>> {
    let edges = db.import_edges()?;
    let memberships = db.component_members_with_line_count()?;

    let mut file_to_comp: HashMap<i64, &str> = HashMap::new();
    let mut comp_ids: Vec<&str> = Vec::new();
    for (comp_id, file_id, _line_count) in &memberships {
        file_to_comp.insert(*file_id, comp_id.as_str());
        if !comp_ids.contains(&comp_id.as_str()) {
            comp_ids.push(comp_id.as_str());
        }
    }

    let mut ce: HashMap<&str, usize> = HashMap::new();
    let mut ca: HashMap<&str, usize> = HashMap::new();

    for (src_file_id, dst_file_id) in &edges {
        let src_comp = file_to_comp.get(src_file_id);
        let dst_comp = file_to_comp.get(dst_file_id);

        match (src_comp, dst_comp) {
            (Some(sc), Some(dc)) if sc != dc => {
                *ce.entry(sc).or_default() += 1;
                *ca.entry(dc).or_default() += 1;
            }
            _ => {}
        }
    }

    let mut result = HashMap::new();
    for comp_id in &comp_ids {
        let efferent = ce.get(comp_id).copied().unwrap_or(0);
        let afferent = ca.get(comp_id).copied().unwrap_or(0);
        let total = efferent + afferent;
        let instability = if total > 0 {
            efferent as f64 / total as f64
        } else {
            0.0
        };
        result.insert(
            comp_id.to_string(),
            ComponentInstability {
                ce: efferent,
                ca: afferent,
                instability,
            },
        );
    }

    Ok(result)
}
