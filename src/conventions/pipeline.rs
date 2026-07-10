use std::collections::HashMap;
use std::path::Path;

use crate::db::Db;
use crate::error::Result;
use crate::parser::adapter::LanguageRegistry;

use super::SymbolAttrs;
use super::engine::FcaEngine;

pub struct RebuildOutcome {
    pub convention_count: usize,
}

pub fn rebuild(
    db: &Db,
    registry: &LanguageRegistry,
    _workspace_root: &Path,
) -> Result<RebuildOutcome> {
    let all_files = db.all_files()?;

    let comp_with_paths = db.active_components_with_paths()?;
    let mut file_to_component: HashMap<&str, &str> = HashMap::new();
    for (comp_id, _name, paths) in &comp_with_paths {
        for path in paths {
            file_to_component.insert(path, comp_id);
        }
    }

    let mut all_sym_attrs = Vec::new();

    for f in &all_files {
        let syms = db.find_symbols_by_file(f.id)?;
        let refs = db.find_refs_in_file(f.id)?;

        let target_ids: Vec<i64> = refs
            .iter()
            .filter(|r| r.context_kind == "call")
            .filter_map(|r| r.target_symbol_id)
            .collect();
        let mut callee_cache: HashMap<i64, super::ResolvedCallee> = HashMap::new();
        for id in &target_ids {
            if !callee_cache.contains_key(id)
                && let Some(sym) = db.symbol_by_id(*id)?
            {
                callee_cache.insert(
                    *id,
                    super::ResolvedCallee {
                        qualified_name: sym.qualified_name.to_string(),
                        signature: sym.signature,
                    },
                );
            }
        }

        let dart_import_packages = if f.language == "dart" {
            super::dart_effect_packages(&db.imports_for_file(f.id)?)
        } else {
            None
        };

        for s in &syms {
            if let Some(mut attrs) =
                super::extract_attrs_for_symbol(s, &f.path, &f.language, registry)
            {
                if let Some(adapter) = registry.adapter_for_language(&f.language)
                    && let Some(fca_source) = adapter.as_fca_source()
                {
                    super::enrich_all_effects(
                        &mut attrs,
                        s,
                        &refs,
                        &callee_cache,
                        fca_source,
                        dart_import_packages.as_ref(),
                    );
                }
                all_sym_attrs.push(attrs);
            }
        }
    }

    for sa in &mut all_sym_attrs {
        sa.component_id = file_to_component
            .get(sa.file.as_str())
            .map(|s| s.to_string());
    }

    let combined_hash = {
        let mut h = blake3::Hasher::new();
        for sa in &all_sym_attrs {
            h.update(sa.file.as_bytes());
            h.update(sa.name.as_bytes());
            for attr in &sa.attributes {
                h.update(attr.as_bytes());
            }
            if let Some(cid) = &sa.component_id {
                h.update(cid.as_bytes());
            }
        }
        h.finalize()
    };
    let combined_bytes: [u8; 32] = *combined_hash.as_bytes();
    let fca_cache_hit =
        matches!(db.get_fca_hash(), Ok(Some(ref cached)) if *cached == combined_bytes);

    let all_convs: Vec<super::engine::Convention> = if fca_cache_hit {
        db.all_conventions()?
            .into_iter()
            .map(super::engine::Convention::from)
            .collect()
    } else {
        let mut global_engine = FcaEngine::new();
        global_engine.rebuild(&all_sym_attrs);
        let mut all_convs = global_engine.conventions().to_vec();

        let mut comp_symbol_groups: Vec<(String, String, Vec<SymbolAttrs>)> = Vec::new();
        for (comp_id, name, paths) in &comp_with_paths {
            let path_set: std::collections::HashSet<&str> =
                paths.iter().map(|p| p.as_str()).collect();
            let comp_symbols: Vec<_> = all_sym_attrs
                .iter()
                .filter(|s| path_set.contains(s.file.as_str()))
                .cloned()
                .collect();
            if comp_symbols.len() < 2 {
                continue;
            }
            comp_symbol_groups.push((comp_id.clone(), name.clone(), comp_symbols));
        }

        for (comp_id, _name, comp_symbols) in &comp_symbol_groups {
            let min_support = super::component_min_support(comp_symbols.len());
            let mut comp_engine = FcaEngine::new();
            comp_engine.rebuild_with_params(
                comp_symbols,
                min_support,
                super::MIN_CONFIDENCE,
                Some(comp_id),
            );
            let comp_convs = comp_engine.conventions().to_vec();
            let deduped =
                super::deduplicate_component_conventions(comp_convs, global_engine.conventions());
            all_convs.extend(deduped);
        }

        all_convs
    };

    for c in &all_convs {
        let _ = db.upsert_convention(
            &c.id,
            &c.antecedent.join(", "),
            &c.consequent.join(", "),
            c.support as i64,
            c.confidence,
            c.component_id.as_deref(),
        );
    }
    let current_ids: Vec<&str> = all_convs.iter().map(|c| &*c.id).collect();

    let _ = db.delete_stale_conventions(&current_ids);

    if !fca_cache_hit {
        let _ = db.set_fca_hash(&combined_bytes);
    }
    let convention_count = all_convs.len();

    Ok(RebuildOutcome { convention_count })
}
