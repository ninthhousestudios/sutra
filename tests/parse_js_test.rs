use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::conventions::{enrich_all_effects, extract_attrs_for_symbol};
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::parser::{SymbolKind, flatten_symbols, parse_file};
use sutra::pipeline;
use sutra::workspace::WorkspaceEntry;

fn make_config(db_dir: &std::path::Path) -> Config {
    Config {
        db_dir: db_dir.to_path_buf(),
        workspaces_path: db_dir.join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        stale_threshold_sec: 600,
        log_level: "warn".to_string(),
        constraints_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    }
}

fn make_js_entry(id: &str, root: PathBuf) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root,
        languages: vec!["javascript".to_string()],
        frozen: false,
    }
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

#[test]
fn smoke_function() {
    let src = "function add(a, b) {\n  return a + b;\n}\n";
    let r = parse_file(src, "javascript", "math.js").unwrap();
    assert!(r.parsed_ok);
    let sym = &r.symbols[0];
    assert_eq!(sym.kind, SymbolKind::Function);
    assert_eq!(sym.short_name, "add");
    assert!(sym.signature.as_ref().unwrap().contains("function add"));
    assert!(sym.signature_hash.is_some());
}

#[test]
fn arrow_function_const() {
    let src = "const multiply = (a, b) => a * b;\n";
    let r = parse_file(src, "javascript", "math.js").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);
    let sym = flat.iter().find(|s| s.short_name == "multiply");
    assert!(sym.is_some(), "expected const multiply, got: {flat:?}");
}

#[test]
fn class_with_methods() {
    let src = r#"
class Greeter {
  constructor(name) {
    this.name = name;
  }
  greet() {
    return `Hello, ${this.name}`;
  }
  static create(name) {
    return new Greeter(name);
  }
}
"#;
    let r = parse_file(src, "javascript", "greeter.js").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);

    let cls = flat.iter().find(|s| s.short_name == "Greeter");
    assert!(cls.is_some(), "expected class Greeter");
    assert_eq!(cls.unwrap().kind, SymbolKind::Class);

    let methods: Vec<_> = flat
        .iter()
        .filter(|s| s.kind == SymbolKind::Method)
        .collect();
    assert!(
        methods.len() >= 2,
        "expected at least 2 methods, got {}",
        methods.len()
    );
}

#[test]
fn async_function() {
    let src = "async function fetchData(url) {\n  const res = await fetch(url);\n  return res.json();\n}\n";
    let r = parse_file(src, "javascript", "api.js").unwrap();
    assert!(r.parsed_ok);
    let sym = &r.symbols[0];
    assert_eq!(sym.kind, SymbolKind::Function);
    assert_eq!(sym.short_name, "fetchData");
    let attrs: serde_json::Value =
        serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["async"], true);
}

#[test]
fn generator_function() {
    let src = "function* range(start, end) {\n  for (let i = start; i < end; i++) {\n    yield i;\n  }\n}\n";
    let r = parse_file(src, "javascript", "gen.js").unwrap();
    assert!(r.parsed_ok);
    let sym = &r.symbols[0];
    assert_eq!(sym.kind, SymbolKind::Function);
    assert_eq!(sym.short_name, "range");
    let attrs: serde_json::Value =
        serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["generator"], true);
}

// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

#[test]
fn es_import_extraction() {
    let src = r#"
import React from 'react';
import { useState, useEffect } from 'react';
import * as utils from './utils';
import './styles.css';
"#;
    let r = parse_file(src, "javascript", "app.js").unwrap();
    assert!(r.parsed_ok);
    let paths: Vec<&str> = r.imports.iter().map(|i| i.raw_path.as_str()).collect();

    assert!(paths.contains(&"react"), "missing react: {paths:?}");
    assert!(paths.contains(&"./utils"), "missing ./utils: {paths:?}");
    assert!(
        paths.contains(&"./styles.css"),
        "missing ./styles.css: {paths:?}"
    );
}

#[test]
fn commonjs_require_extraction() {
    let src = r#"
const fs = require('fs');
const { join } = require('path');
const config = require('./config');
"#;
    let r = parse_file(src, "javascript", "legacy.cjs").unwrap();
    assert!(r.parsed_ok);
    let paths: Vec<&str> = r.imports.iter().map(|i| i.raw_path.as_str()).collect();

    assert!(paths.contains(&"fs"), "missing fs: {paths:?}");
    assert!(paths.contains(&"path"), "missing path: {paths:?}");
    assert!(paths.contains(&"./config"), "missing ./config: {paths:?}");
}

#[test]
fn dynamic_import_extraction() {
    let src = r#"
async function loadModule() {
  const mod = await import('./lazy-module');
  return mod.default;
}
"#;
    let r = parse_file(src, "javascript", "loader.js").unwrap();
    assert!(r.parsed_ok);
    let paths: Vec<&str> = r.imports.iter().map(|i| i.raw_path.as_str()).collect();
    assert!(
        paths.contains(&"./lazy-module"),
        "missing dynamic import ./lazy-module: {paths:?}"
    );
}

// ---------------------------------------------------------------------------
// Reference extraction
// ---------------------------------------------------------------------------

#[test]
fn references_extracted() {
    let src = r#"
import { helper } from './utils';
function main() {
  const result = helper(42);
  console.log(result);
}
"#;
    let r = parse_file(src, "javascript", "main.js").unwrap();
    assert!(r.parsed_ok);
    let ref_names: Vec<&str> = r.references.iter().map(|r| r.name.as_str()).collect();
    assert!(
        ref_names.contains(&"helper"),
        "missing ref to helper: {ref_names:?}"
    );
}

// ---------------------------------------------------------------------------
// Complexity scoring
// ---------------------------------------------------------------------------

#[test]
fn complexity_scoring() {
    let src = r#"
function process(items) {
  const results = [];
  for (const item of items) {
    if (item.valid) {
      try {
        results.push(item.transform());
      } catch (e) {
        console.error(e);
      }
    }
  }
  return results.filter(r => r != null);
}
"#;
    let r = parse_file(src, "javascript", "complex.js").unwrap();
    let sym = &r.symbols[0];
    assert!(
        sym.cyclomatic.unwrap() >= 4,
        "cyclomatic={}, expected >= 4",
        sym.cyclomatic.unwrap()
    );
    assert!(
        sym.cognitive.unwrap() > 0,
        "cognitive should be non-zero, got {}",
        sym.cognitive.unwrap()
    );
    assert!(
        sym.max_nesting.unwrap() >= 3,
        "max_nesting={}, expected >= 3",
        sym.max_nesting.unwrap()
    );
}

// ---------------------------------------------------------------------------
// Destructuring — should not panic
// ---------------------------------------------------------------------------

#[test]
fn destructuring_no_panic() {
    let src = r#"
const { a, b: renamed, ...rest } = obj;
const [first, second, ...remaining] = arr;
function handle({ name, age = 25 }) {
  return `${name} is ${age}`;
}
"#;
    let r = parse_file(src, "javascript", "destruct.js").unwrap();
    assert!(r.parsed_ok);
}

// ---------------------------------------------------------------------------
// Optional chaining / nullish coalescing — no spurious refs
// ---------------------------------------------------------------------------

#[test]
fn optional_chaining_no_spurious_refs() {
    let src = r#"
function safe(obj) {
  const name = obj?.user?.name ?? 'anonymous';
  const len = obj?.items?.length ?? 0;
  return { name, len };
}
"#;
    let r = parse_file(src, "javascript", "safe.js").unwrap();
    assert!(r.parsed_ok);
    assert_eq!(r.symbols[0].short_name, "safe");
}

// ---------------------------------------------------------------------------
// Computed property names
// ---------------------------------------------------------------------------

#[test]
fn computed_property_names() {
    let src = r#"
const key = 'hello';
const obj = {
  [key]: 'world',
  [Symbol.iterator]() { return this; }
};
"#;
    let r = parse_file(src, "javascript", "computed.js").unwrap();
    assert!(r.parsed_ok);
}

// ---------------------------------------------------------------------------
// Template literals
// ---------------------------------------------------------------------------

#[test]
fn template_literal_expressions() {
    let src = r#"
function greet(name) {
  return `Hello, ${name.toUpperCase()}! Today is ${new Date().toLocaleDateString()}.`;
}
"#;
    let r = parse_file(src, "javascript", "template.js").unwrap();
    assert!(r.parsed_ok);
    assert_eq!(r.symbols[0].short_name, "greet");
}

// ---------------------------------------------------------------------------
// Export variations
// ---------------------------------------------------------------------------

#[test]
fn export_default_function() {
    let src = "export default function handler(req, res) {\n  res.send('ok');\n}\n";
    let r = parse_file(src, "javascript", "handler.js").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);
    let handler = flat.iter().find(|s| s.short_name == "handler");
    assert!(handler.is_some(), "expected handler function");
}

#[test]
fn named_exports() {
    let src = r#"
export function add(a, b) { return a + b; }
export const PI = 3.14159;
export class Calculator {
  multiply(a, b) { return a * b; }
}
"#;
    let r = parse_file(src, "javascript", "exports.js").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);

    assert!(flat.iter().any(|s| s.short_name == "add"), "missing add");
    assert!(flat.iter().any(|s| s.short_name == "PI"), "missing PI");
    assert!(
        flat.iter().any(|s| s.short_name == "Calculator"),
        "missing Calculator"
    );
}

// ---------------------------------------------------------------------------
// Adapter registration
// ---------------------------------------------------------------------------

#[test]
fn js_adapter_registered() {
    let registry = default_registry();
    let adapter = registry
        .adapter_for_language("javascript")
        .expect("JsAdapter should be registered");
    assert_eq!(adapter.language_id(), "javascript");
    assert!(adapter.extensions().contains(&"js"));
    assert!(adapter.extensions().contains(&"jsx"));
    assert!(adapter.extensions().contains(&"mjs"));
    assert!(adapter.extensions().contains(&"cjs"));

    let fca = adapter.as_fca_source().expect("JS should have FCA source");
    let effect_names: Vec<_> = fca.effect_patterns().iter().map(|p| p.attr_name).collect();
    assert!(effect_names.contains(&"effect:dom"));
    assert!(effect_names.contains(&"effect:net"));
    assert!(effect_names.contains(&"effect:fs"));
}

// ---------------------------------------------------------------------------
// Pipeline: multi-file workspace
// ---------------------------------------------------------------------------

#[test]
fn js_files_indexed_in_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("utils.js"),
        "export function helper() { return 42; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("index.mjs"),
        "import { helper } from './utils';\nexport default helper;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("legacy.cjs"),
        "const utils = require('./utils');\nmodule.exports = { run: utils.helper };\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("js-pipeline", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 3, "expected 3 JS files parsed");

    let files = db.all_files().unwrap();
    let js_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == "javascript")
        .collect();
    assert_eq!(js_files.len(), 3, "expected 3 JS files indexed");
}

#[test]
fn js_cross_file_import_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("utils.js"),
        "export function helper() { return 42; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        "import { helper } from './utils';\nfunction main() { return helper(); }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("js-imports", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 2);

    let app_file = db
        .file_by_path("app.js")
        .unwrap()
        .expect("app.js should be indexed");
    let _utils_file = db
        .file_by_path("utils.js")
        .unwrap()
        .expect("utils.js should be indexed");

    let imports = db.imports_for_file(app_file.id).unwrap();
    let paths: Vec<&str> = imports.iter().map(|i| i.imported_path.as_str()).collect();
    assert!(
        paths.contains(&"./utils"),
        "app.js should have import of ./utils, got: {paths:?}"
    );

    let utils_file = db
        .file_by_path("utils.js")
        .unwrap()
        .expect("utils.js should be indexed");
    let resolved_import = imports
        .iter()
        .find(|i| i.imported_path == "./utils")
        .expect("should have ./utils import");
    assert_eq!(
        resolved_import.resolved_file_id,
        Some(utils_file.id),
        "./utils should resolve to utils.js"
    );
}

// ---------------------------------------------------------------------------
// Effect detection end-to-end
// ---------------------------------------------------------------------------

#[test]
fn js_effect_detection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("effects.js"),
        r#"
function readDom() {
  const el = document.getElementById('app');
  return el.innerHTML;
}

async function fetchApi(url) {
  const res = await fetch(url);
  return res.json();
}

function writeFile(path, data) {
  fs.writeFileSync(path, data);
}
"#,
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("js-effects", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let file = db
        .file_by_path("effects.js")
        .unwrap()
        .expect("effects.js should be indexed");
    let syms = db.find_symbols_by_file(file.id).unwrap();
    let refs = db.find_refs_in_file(file.id).unwrap();
    let adapter = registry.adapter_for_language("javascript").unwrap();
    let fca_source = adapter.as_fca_source().unwrap();
    let callee_cache = HashMap::new();

    for (fn_name, expected_effect) in [
        ("readDom", "effect:dom"),
        ("fetchApi", "effect:net"),
        ("writeFile", "effect:fs"),
    ] {
        let sym = syms
            .iter()
            .find(|s| &*s.short_name == fn_name)
            .unwrap_or_else(|| panic!("missing symbol {fn_name}"));
        let mut attrs = extract_attrs_for_symbol(sym, "effects.js", "javascript", &registry)
            .unwrap_or_else(|| panic!("no attrs for {fn_name}"));
        enrich_all_effects(&mut attrs, sym, &refs, &callee_cache, fca_source, None);
        assert!(
            attrs.attributes.contains(&expected_effect.to_string()),
            "{fn_name} should have {expected_effect}, got: {:?}",
            attrs.attributes
        );
    }
}

// ---------------------------------------------------------------------------
// Test detection (FLAG_TEST = 0x01)
// ---------------------------------------------------------------------------

#[test]
fn test_detection_jest_style() {
    let src = r#"
describe('Calculator', () => {
  it('adds numbers', () => {
    expect(add(1, 2)).toBe(3);
  });

  test('multiplies numbers', () => {
    expect(multiply(2, 3)).toBe(6);
  });
});
"#;
    let r = parse_file(src, "javascript", "calculator.test.js").unwrap();
    assert!(r.parsed_ok);
}

// ---------------------------------------------------------------------------
// JSX component references
// ---------------------------------------------------------------------------

#[test]
fn jsx_component_refs() {
    let src = r#"
import React from 'react';
import Header from './Header';

function App() {
  return (
    <div>
      <Header title="Hello" />
      <main>Content</main>
    </div>
  );
}
"#;
    let r = parse_file(src, "javascript", "App.jsx").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);
    let app = flat.iter().find(|s| s.short_name == "App");
    assert!(app.is_some(), "expected App component");

    let ref_names: Vec<&str> = r.references.iter().map(|r| r.name.as_str()).collect();
    assert!(
        ref_names.contains(&"Header"),
        "missing JSX ref to Header: {ref_names:?}"
    );
}

// ---------------------------------------------------------------------------
// Import resolution: extension guessing priority
// ---------------------------------------------------------------------------

#[test]
fn js_import_extension_guessing_priority() {
    let dir = tempfile::tempdir().unwrap();
    // Both .ts and .js exist — .ts should win (higher priority)
    std::fs::write(dir.path().join("helper.ts"), "export const x = 1;\n").unwrap();
    std::fs::write(dir.path().join("helper.js"), "export const x = 2;\n").unwrap();
    std::fs::write(dir.path().join("app.js"), "import { x } from './helper';\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "ext-priority".to_string(),
        root: dir.path().to_path_buf(),
        languages: vec!["javascript".to_string(), "typescript".to_string()],
        frozen: false,
    };
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let app_file = db.file_by_path("app.js").unwrap().unwrap();
    let helper_ts = db.file_by_path("helper.ts").unwrap().unwrap();
    let imports = db.imports_for_file(app_file.id).unwrap();
    let imp = imports
        .iter()
        .find(|i| i.imported_path == "./helper")
        .unwrap();
    assert_eq!(
        imp.resolved_file_id,
        Some(helper_ts.id),
        ".ts should take priority over .js"
    );
}

// ---------------------------------------------------------------------------
// Import resolution: index file resolution
// ---------------------------------------------------------------------------

#[test]
fn js_import_index_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("components")).unwrap();
    std::fs::write(
        dir.path().join("components/index.js"),
        "export { Button } from './Button';\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        "import { Button } from './components';\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("index-resolve", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let app_file = db.file_by_path("app.js").unwrap().unwrap();
    let index_file = db.file_by_path("components/index.js").unwrap().unwrap();
    let imports = db.imports_for_file(app_file.id).unwrap();
    let imp = imports
        .iter()
        .find(|i| i.imported_path == "./components")
        .unwrap();
    assert_eq!(
        imp.resolved_file_id,
        Some(index_file.id),
        "./components should resolve to components/index.js"
    );
}

// ---------------------------------------------------------------------------
// Import resolution: bare specifiers stay unresolved
// ---------------------------------------------------------------------------

#[test]
fn js_bare_specifier_not_resolved() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        "import React from 'react';\nimport { map } from 'lodash';\nimport { Injectable } from '@angular/core';\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("bare-spec", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let app_file = db.file_by_path("app.js").unwrap().unwrap();
    let imports = db.imports_for_file(app_file.id).unwrap();
    for imp in &imports {
        assert_eq!(
            imp.resolved_file_id, None,
            "bare specifier '{}' should not resolve",
            imp.imported_path
        );
    }
}

// ---------------------------------------------------------------------------
// Import resolution: re-export edges
// ---------------------------------------------------------------------------

#[test]
fn js_reexport_creates_edge() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("math.js"),
        "export function add(a, b) { return a + b; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("index.js"),
        "export { add } from './math';\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("reexport", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let index_file = db.file_by_path("index.js").unwrap().unwrap();
    let math_file = db.file_by_path("math.js").unwrap().unwrap();
    let imports = db.imports_for_file(index_file.id).unwrap();
    let imp = imports
        .iter()
        .find(|i| i.imported_path == "./math")
        .unwrap();
    assert_eq!(
        imp.resolved_file_id,
        Some(math_file.id),
        "re-export should create resolved edge"
    );
}

// ---------------------------------------------------------------------------
// Import resolution: side-effect imports
// ---------------------------------------------------------------------------

#[test]
fn js_side_effect_import_resolves() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("polyfill.js"), "// side effects\n").unwrap();
    std::fs::write(dir.path().join("app.js"), "import './polyfill';\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("side-effect", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let app_file = db.file_by_path("app.js").unwrap().unwrap();
    let polyfill_file = db.file_by_path("polyfill.js").unwrap().unwrap();
    let imports = db.imports_for_file(app_file.id).unwrap();
    let imp = imports
        .iter()
        .find(|i| i.imported_path == "./polyfill")
        .unwrap();
    assert_eq!(
        imp.resolved_file_id,
        Some(polyfill_file.id),
        "side-effect import should resolve"
    );
}

// ---------------------------------------------------------------------------
// Import resolution: explicit extension resolves directly
// ---------------------------------------------------------------------------

#[test]
fn js_explicit_extension_resolves() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.mjs"), "export const x = 1;\n").unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        "import { x } from './lib.mjs';\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_js_entry("explicit-ext", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let app_file = db.file_by_path("app.js").unwrap().unwrap();
    let lib_file = db.file_by_path("lib.mjs").unwrap().unwrap();
    let imports = db.imports_for_file(app_file.id).unwrap();
    let imp = imports
        .iter()
        .find(|i| i.imported_path == "./lib.mjs")
        .unwrap();
    assert_eq!(
        imp.resolved_file_id,
        Some(lib_file.id),
        "explicit .mjs extension should resolve directly"
    );
}
