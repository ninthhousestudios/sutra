//! Parser-identity stamp (sutra/364).
//!
//! The pipeline skips re-parsing a file whose stored `content_hash` still
//! matches its bytes. That memo is keyed on source bytes alone, so when the
//! *extractor* changes — a new tree-sitter query, an adapter fix, a symbol-kind
//! change, a grammar bump — every unchanged file replays stale symbols until
//! someone remembers to ship a `UPDATE files SET content_hash=''` migration
//! (see 0054/0055/0056). That is exactly the "version bump to forget" failure
//! graft eliminated by hashing the extractor code into its cache key.
//!
//! This build script computes a stamp over the extractor's identity — the
//! `src/parser/` sources plus the pinned tree-sitter grammar versions plus the
//! crate version — and exposes it as `SUTRA_PARSER_STAMP`. `parse_workspace`
//! compares the stamp stored in `index_meta` against this value at parse start
//! and, on a mismatch, forces exactly one full re-extraction, then records the
//! new stamp. No migration to forget.

use std::fs;
use std::path::Path;

/// FNV-1a 64-bit. Deterministic across toolchains (unlike `DefaultHasher`), so
/// an identical extractor never yields a spurious stamp change on rebuild. The
/// stamp only has to *change when the inputs change*; no cryptographic strength
/// is needed.
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1_0000_0000_01b3);
    }
    h
}

/// Collect every `.rs` file under `dir`, recursing into subdirectories, so the
/// parser stamp covers extractor code regardless of how src/parser/ is
/// organized into submodules.
fn collect_rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let parser_dir = Path::new(&manifest_dir).join("src").join("parser");
    let lock_path = Path::new(&manifest_dir).join("Cargo.lock");

    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis

    // 1. Every extractor source file under src/parser/ (recursively), hashed
    //    relative-path-then-contents in a stable order. This is where the
    //    symbol/ref/raw-import extraction lives AND the extraction→persistence
    //    normalization (`persist.rs`, sutra/383); a change to any of them
    //    changes what a re-parse of unchanged bytes would produce. Watch the
    //    directory itself too, so adding or removing a file re-runs this script.
    println!("cargo:rerun-if-changed={}", parser_dir.display());
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    collect_rs_files(&parser_dir, &mut sources);
    sources.sort();
    // Guard (sutra/383): the persisted-output normalization must stay inside the
    // hashed tree. If `flatten_symbols_dfs` is moved out of src/parser/ (e.g.
    // back into the un-hashed src/pipeline.rs), the stamp would stop covering it
    // and a normalization change would silently skip unchanged files again.
    let marker = b"fn flatten_symbols_dfs";
    let mut has_normalization = false;
    for path in &sources {
        let rel = path.strip_prefix(&manifest_dir).unwrap_or(path);
        h = fnv1a(h, rel.to_string_lossy().as_bytes());
        let contents = fs::read(path).expect("read parser source");
        if contents.windows(marker.len()).any(|w| w == marker) {
            has_normalization = true;
        }
        h = fnv1a(h, &contents);
        println!("cargo:rerun-if-changed={}", path.display());
    }
    assert!(
        has_normalization,
        "parser-stamp guard (sutra/383): `fn flatten_symbols_dfs` was not found under \
         src/parser/. Extraction→persistence normalization must live inside the hashed \
         parser tree so PARSER_STAMP invalidates unchanged files when it changes. If you \
         moved or renamed it, keep it under src/parser/ — not src/pipeline.rs, which is \
         not hashed."
    );

    // 2. The pinned tree-sitter grammar versions. A grammar bump can change node
    //    kinds without touching our sources, so hash the resolved versions too.
    let lock = fs::read_to_string(&lock_path).expect("read Cargo.lock");
    let mut grammars: Vec<(String, String)> = Vec::new();
    let mut pending_name: Option<String> = None;
    for line in lock.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name = \"") {
            pending_name = rest.strip_suffix('"').map(str::to_string);
        } else if let Some(rest) = line.strip_prefix("version = \"") {
            if let Some(name) = pending_name.take() {
                if name.starts_with("tree-sitter") {
                    if let Some(ver) = rest.strip_suffix('"') {
                        grammars.push((name, ver.to_string()));
                    }
                }
            }
        }
    }
    grammars.sort();
    for (name, ver) in &grammars {
        h = fnv1a(h, name.as_bytes());
        h = fnv1a(h, b"=");
        h = fnv1a(h, ver.as_bytes());
    }

    // 3. The crate version — a coarse backstop so a release bump alone still
    //    revs the stamp even if the two hashes above somehow collided.
    let crate_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    h = fnv1a(h, crate_version.as_bytes());

    println!("cargo:rerun-if-changed={}", lock_path.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=SUTRA_PARSER_STAMP={h:016x}");
}
