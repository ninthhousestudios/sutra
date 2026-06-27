mod anchors;
mod clustering;
mod identity;

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::db::{Db, FileRow};
use crate::error::{Result, SutraError};
use crate::graph::GraphData;

pub use anchors::{
    ANCHOR_KINDS, anchor_count, compute_semantic_anchors, concept_density, extract_stems,
};
pub(crate) use clustering::is_test_file;

const DEFAULT_STALENESS_THRESHOLD: f64 = 0.10;
const DEFAULT_COCHANGE_THRESHOLD: f64 = 0.5;
const DEFAULT_COCHANGE_WEIGHT: f64 = 5.0;
const DEFAULT_COCHANGE_WINDOW_DAYS: u32 = 90;

pub(super) fn to_json<T: serde::Serialize>(val: &T) -> String {
    serde_json::to_string(val).unwrap_or_default()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComponentsConfig {
    #[serde(default)]
    pub resolution: Option<f64>,
    #[serde(default)]
    pub staleness_threshold: Option<f64>,
    #[serde(default)]
    pub cochange_threshold: Option<f64>,
    #[serde(default)]
    pub cochange_weight: Option<f64>,
    #[serde(default)]
    pub cochange_window_days: Option<u32>,
    #[serde(default)]
    pub max_community_size: Option<usize>,
}

pub fn load_config(root: &Path) -> Result<ComponentsConfig> {
    let path = root.join(".sutra/components.toml");
    if !path.exists() {
        return Ok(ComponentsConfig::default());
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| SutraError::Internal(format!("{e}")))?;
    toml::from_str(&content)
        .map_err(|e| SutraError::Internal(format!("components.toml parse error: {e}")))
}

pub fn parse_config(content: &str) -> Result<ComponentsConfig> {
    let mut config: ComponentsConfig = toml::from_str(content)
        .map_err(|e| SutraError::Internal(format!("components.toml parse error: {e}")))?;
    if let Some(t) = config.staleness_threshold {
        config.staleness_threshold = Some(t.clamp(0.0, 1.0));
    }
    Ok(config)
}

pub fn discover_components(
    db: &Db,
    files: &[FileRow],
    gd: &GraphData,
    workspace_root: &Path,
    boundary_multipliers: &HashMap<String, f64>,
) -> Result<usize> {
    if files.is_empty() {
        return Ok(0);
    }

    let config = load_config(workspace_root)?;
    let threshold = config
        .staleness_threshold
        .unwrap_or(DEFAULT_STALENESS_THRESHOLD);
    let has_existing = db.component_count()? > 0;
    let has_membership = db.membership_count()? > 0;
    let file_count = files.len() as i64;
    let cfg_hash = identity::clustering_config_hash(boundary_multipliers, &config);
    let commit_file_count = db.commit_file_count()?;
    let newest_commit_at = db.newest_commit_at()?;

    if has_existing && has_membership {
        let current_edge_count = identity::edge_count(files, gd);
        if !identity::is_clustering_stale(
            db,
            current_edge_count,
            file_count,
            commit_file_count,
            newest_commit_at,
            threshold,
            &cfg_hash,
        )? {
            return Ok(0);
        }
    }

    let Some((clusters, file_map, edge_count)) =
        clustering::run_clustering(db, files, gd, &config, boundary_multipliers)?
    else {
        return Ok(0);
    };

    let count = if has_existing {
        identity::reconcile_components(db, &clusters, &file_map)?
    } else {
        identity::create_fresh_components(db, &clusters, &file_map)?
    };

    db.upsert_clustering_meta(
        edge_count as i64,
        file_count,
        &cfg_hash,
        commit_file_count,
        newest_commit_at,
    )?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_empty() {
        let c = parse_config("").unwrap();
        assert!(c.resolution.is_none());
    }

    #[test]
    fn test_parse_config_with_resolution() {
        let c = parse_config("resolution = 1.5").unwrap();
        assert_eq!(c.resolution, Some(1.5));
    }

    #[test]
    fn test_parse_config_with_staleness_threshold() {
        let c = parse_config("staleness_threshold = 0.25").unwrap();
        assert_eq!(c.staleness_threshold, Some(0.25));
    }

    #[test]
    fn test_parse_config_default_staleness_threshold() {
        let c = parse_config("resolution = 1.0").unwrap();
        assert!(c.staleness_threshold.is_none());
    }

    #[test]
    fn test_parse_config_staleness_threshold_clamped() {
        let c = parse_config("staleness_threshold = -0.5").unwrap();
        assert_eq!(c.staleness_threshold, Some(0.0));
        let c = parse_config("staleness_threshold = 2.0").unwrap();
        assert_eq!(c.staleness_threshold, Some(1.0));
    }

    #[test]
    fn test_parse_config_cochange_fields() {
        let c = parse_config(
            "cochange_threshold = 0.3\ncochange_weight = 8.0\ncochange_window_days = 180",
        )
        .unwrap();
        assert_eq!(c.cochange_threshold, Some(0.3));
        assert_eq!(c.cochange_weight, Some(8.0));
        assert_eq!(c.cochange_window_days, Some(180));
    }

    #[test]
    fn test_parse_config_cochange_defaults() {
        let c = parse_config("").unwrap();
        assert!(c.cochange_threshold.is_none());
        assert!(c.cochange_weight.is_none());
        assert!(c.cochange_window_days.is_none());
    }
}
