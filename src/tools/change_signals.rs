use std::collections::HashMap;

use crate::db::Db;
use crate::error::Result;

pub const BLAST_NORM: f64 = 50.0;
pub const COMPLEXITY_NORM: f64 = 30.0;
pub const CHURN_NORM: f64 = 20.0;
pub const CHURN_WINDOW_DAYS: u32 = 90;

#[derive(Default)]
pub struct ChurnMap {
    pub counts: HashMap<String, u32>,
    pub window_days: u32,
}

pub struct SymbolSignal {
    pub id: i64,
    pub qualified_name: String,
    pub cognitive: i64,
}

pub struct FileSignal {
    pub path: String,
    pub blast_radius: i64,
    pub churn: u32,
    pub is_hotspot: bool,
    pub indexed: bool,
    pub symbols: Vec<SymbolSignal>,
}

pub struct AffectedFile {
    pub path: String,
    pub blast_radius: i64,
}

pub struct AffectedSymbol {
    pub qualified_name: String,
    pub file: String,
    pub blast_radius: i64,
    pub cognitive: i64,
}

pub struct ChangeSignals {
    pub per_file: Vec<FileSignal>,
    pub total_blast: i64,
    /// None when no symbol had cognitive data (all were null in DB).
    pub max_cognitive: Option<i64>,
    pub max_cognitive_symbol: Option<String>,
    pub total_churn: u32,
    pub hotspot_files: u32,
    pub affected_files: Vec<AffectedFile>,
    pub affected_symbols: Vec<AffectedSymbol>,
}

pub fn gather(
    db: &Db,
    changed_paths: &[String],
    churn: &ChurnMap,
    include_affected: bool,
) -> Result<ChangeSignals> {
    let mut per_file = Vec::new();
    let mut total_blast: i64 = 0;
    let mut max_cognitive: Option<i64> = None;
    let mut max_cognitive_symbol: Option<String> = None;
    let mut total_churn: u32 = 0;
    let mut hotspot_files: u32 = 0;
    let mut all_symbol_ids: Vec<i64> = Vec::new();

    for path in changed_paths {
        let file_churn = churn.counts.get(path).copied().unwrap_or(0);
        total_churn += file_churn;

        if let Some(file) = db.file_by_path(path)? {
            total_blast += file.blast_radius;
            if file_churn > 5 && file.blast_radius > 10 {
                hotspot_files += 1;
            }

            let syms = db.find_symbols_by_file(file.id)?;
            let mut symbols = Vec::with_capacity(syms.len());
            for s in &syms {
                let cog = s.cognitive.unwrap_or(0);
                if s.cognitive.is_some() && max_cognitive.is_none_or(|prev| cog > prev) {
                    max_cognitive = Some(cog);
                    max_cognitive_symbol = Some(s.qualified_name.to_string());
                }
                all_symbol_ids.push(s.id);
                symbols.push(SymbolSignal {
                    id: s.id,
                    qualified_name: s.qualified_name.to_string(),
                    cognitive: cog,
                });
            }

            per_file.push(FileSignal {
                path: path.clone(),
                blast_radius: file.blast_radius,
                churn: file_churn,
                is_hotspot: file_churn > 5 && file.blast_radius > 10,
                indexed: true,
                symbols,
            });
        } else {
            per_file.push(FileSignal {
                path: path.clone(),
                blast_radius: 0,
                churn: file_churn,
                is_hotspot: false,
                indexed: false,
                symbols: Vec::new(),
            });
        }
    }

    let mut affected_files: Vec<AffectedFile> = Vec::new();
    let mut affected_symbols: Vec<AffectedSymbol> = Vec::new();

    if include_affected {
        let affected_file_ids = if all_symbol_ids.is_empty() {
            Vec::new()
        } else {
            db.find_files_referencing_symbols(&all_symbol_ids)?
        };

        let changed_set: std::collections::HashSet<&str> =
            changed_paths.iter().map(|p| p.as_str()).collect();

        for fid in &affected_file_ids {
            if let Some(file) = db.file_by_id(*fid)? {
                if changed_set.contains(&*file.path) {
                    continue;
                }
                for s in db.find_symbols_by_file(file.id)? {
                    affected_symbols.push(AffectedSymbol {
                        qualified_name: s.qualified_name.to_string(),
                        file: file.path.to_string(),
                        blast_radius: file.blast_radius,
                        cognitive: s.cognitive.unwrap_or(0),
                    });
                }
                affected_files.push(AffectedFile {
                    path: file.path.to_string(),
                    blast_radius: file.blast_radius,
                });
            }
        }

        affected_files.sort_by(|a, b| a.path.cmp(&b.path));
        affected_files.dedup_by(|a, b| a.path == b.path);
        affected_files.sort_by_key(|x| std::cmp::Reverse(x.blast_radius));

        affected_symbols.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
        affected_symbols.dedup_by(|a, b| a.qualified_name == b.qualified_name);
        affected_symbols.sort_by_key(|x| std::cmp::Reverse(x.blast_radius));
    }

    Ok(ChangeSignals {
        per_file,
        total_blast,
        max_cognitive,
        max_cognitive_symbol,
        total_churn,
        hotspot_files,
        affected_files,
        affected_symbols,
    })
}
