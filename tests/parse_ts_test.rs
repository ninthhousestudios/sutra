use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
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

fn make_ts_entry(id: &str, root: PathBuf) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root,
        languages: vec!["typescript".to_string()],
        frozen: false,
    }
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

#[test]
fn smoke_function() {
    let src = "function add(a: number, b: number): number {\n  return a + b;\n}\n";
    let r = parse_file(src, "typescript", "math.ts").unwrap();
    assert!(r.parsed_ok);
    let sym = &r.symbols[0];
    assert_eq!(sym.kind, SymbolKind::Function);
    assert_eq!(sym.short_name, "add");
    assert!(sym.signature.as_ref().unwrap().contains("function add"));
}

#[test]
fn interface_extraction() {
    let src = r#"
interface User {
  id: number;
  name: string;
  email?: string;
}
"#;
    let r = parse_file(src, "typescript", "types.ts").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);
    let iface = flat.iter().find(|s| s.short_name == "User");
    assert!(iface.is_some(), "expected interface User, got: {flat:?}");
}

#[test]
fn type_alias_extraction() {
    let src = "type Result<T> = { ok: true; value: T } | { ok: false; error: Error };\n";
    let r = parse_file(src, "typescript", "result.ts").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);
    let alias = flat.iter().find(|s| s.short_name == "Result");
    assert!(alias.is_some(), "expected type alias Result, got: {flat:?}");
}

#[test]
fn enum_extraction() {
    let src = r#"
enum Direction {
  Up = "UP",
  Down = "DOWN",
  Left = "LEFT",
  Right = "RIGHT",
}
"#;
    let r = parse_file(src, "typescript", "direction.ts").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);
    let e = flat.iter().find(|s| s.short_name == "Direction");
    assert!(e.is_some(), "expected enum Direction");
    assert_eq!(e.unwrap().kind, SymbolKind::Enum);
}

#[test]
fn class_with_generics_and_access_modifiers() {
    let src = r#"
class Repository<T> {
  private items: T[] = [];

  constructor(private readonly name: string) {}

  add(item: T): void {
    this.items.push(item);
  }

  getAll(): T[] {
    return [...this.items];
  }
}
"#;
    let r = parse_file(src, "typescript", "repo.ts").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);

    let cls = flat.iter().find(|s| s.short_name == "Repository");
    assert!(cls.is_some(), "expected class Repository");
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
fn async_function_with_types() {
    let src = "async function fetchUser(id: number): Promise<User> {\n  return await api.get(`/users/${id}`);\n}\n";
    let r = parse_file(src, "typescript", "api.ts").unwrap();
    assert!(r.parsed_ok);
    let sym = &r.symbols[0];
    assert_eq!(sym.short_name, "fetchUser");
    let attrs: serde_json::Value =
        serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["async"], true);
}

// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

#[test]
fn import_extraction() {
    let src = r#"
import { Component } from '@angular/core';
import type { Observable } from 'rxjs';
import * as path from 'path';
import express from 'express';
"#;
    let r = parse_file(src, "typescript", "app.ts").unwrap();
    assert!(r.parsed_ok);
    let paths: Vec<&str> = r.imports.iter().map(|i| i.raw_path.as_str()).collect();

    assert!(
        paths.contains(&"@angular/core"),
        "missing @angular/core: {paths:?}"
    );
    assert!(paths.contains(&"rxjs"), "missing rxjs: {paths:?}");
    assert!(paths.contains(&"path"), "missing path: {paths:?}");
    assert!(paths.contains(&"express"), "missing express: {paths:?}");
}

// ---------------------------------------------------------------------------
// TSX / JSX support
// ---------------------------------------------------------------------------

#[test]
fn tsx_component_with_props() {
    let src = r#"
import React from 'react';

interface ButtonProps {
  label: string;
  onClick: () => void;
  disabled?: boolean;
}

function Button({ label, onClick, disabled = false }: ButtonProps) {
  return (
    <button onClick={onClick} disabled={disabled}>
      {label}
    </button>
  );
}

export default Button;
"#;
    let r = parse_file(src, "typescript", "Button.tsx").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);

    let iface = flat.iter().find(|s| s.short_name == "ButtonProps");
    assert!(iface.is_some(), "expected ButtonProps interface");

    let component = flat.iter().find(|s| s.short_name == "Button");
    assert!(component.is_some(), "expected Button component");
}

#[test]
fn tsx_jsx_refs_extracted() {
    let src = r#"
import React from 'react';
import Header from './Header';
import Footer from './Footer';

const Page: React.FC = () => (
  <div>
    <Header />
    <main>Content</main>
    <Footer />
  </div>
);
"#;
    let r = parse_file(src, "typescript", "Page.tsx").unwrap();
    assert!(r.parsed_ok);
    let ref_names: Vec<&str> = r.references.iter().map(|r| r.name.as_str()).collect();
    assert!(
        ref_names.contains(&"Header"),
        "missing JSX ref to Header: {ref_names:?}"
    );
    assert!(
        ref_names.contains(&"Footer"),
        "missing JSX ref to Footer: {ref_names:?}"
    );
}

// ---------------------------------------------------------------------------
// Ambient declarations
// ---------------------------------------------------------------------------

#[test]
fn declare_ambient() {
    let src = r#"
declare module 'express' {
  interface Request {
    user?: User;
  }
}

declare const __DEV__: boolean;
declare function require(id: string): any;
"#;
    let r = parse_file(src, "typescript", "ambient.d.ts").unwrap();
    assert!(r.parsed_ok);
}

// ---------------------------------------------------------------------------
// Decorators
// ---------------------------------------------------------------------------

#[test]
fn decorator_on_class() {
    let src = r#"
@Component({
  selector: 'app-root',
  template: '<h1>Hello</h1>'
})
class AppComponent {
  title = 'my-app';
}
"#;
    let r = parse_file(src, "typescript", "app.component.ts").unwrap();
    assert!(r.parsed_ok);
    let flat = flatten_symbols(&r.symbols);
    let cls = flat.iter().find(|s| s.short_name == "AppComponent");
    assert!(cls.is_some(), "expected decorated class AppComponent");
}

// ---------------------------------------------------------------------------
// Complexity scoring
// ---------------------------------------------------------------------------

#[test]
fn complexity_scoring() {
    let src = r#"
function validate(input: unknown): input is string {
  if (typeof input !== 'string') return false;
  if (input.length === 0) return false;
  for (const char of input) {
    if (char === ' ') {
      continue;
    }
    if (!isAlpha(char)) {
      return false;
    }
  }
  return true;
}
"#;
    let r = parse_file(src, "typescript", "validate.ts").unwrap();
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
}

// ---------------------------------------------------------------------------
// Destructuring — no panics
// ---------------------------------------------------------------------------

#[test]
fn destructuring_no_panic() {
    let src = r#"
const { a, b: renamed, ...rest }: Record<string, number> = obj;
const [first, ...remaining]: number[] = arr;
function handle({ name, age = 25 }: { name: string; age?: number }) {
  return `${name} is ${age}`;
}
"#;
    let r = parse_file(src, "typescript", "destruct.ts").unwrap();
    assert!(r.parsed_ok);
}

// ---------------------------------------------------------------------------
// Optional chaining / nullish coalescing
// ---------------------------------------------------------------------------

#[test]
fn optional_chaining_no_spurious_refs() {
    let src = r#"
function safe(obj: any): { name: string; len: number } {
  const name = obj?.user?.name ?? 'anonymous';
  const len = obj?.items?.length ?? 0;
  return { name, len };
}
"#;
    let r = parse_file(src, "typescript", "safe.ts").unwrap();
    assert!(r.parsed_ok);
    assert_eq!(r.symbols[0].short_name, "safe");
}

// ---------------------------------------------------------------------------
// Adapter registration
// ---------------------------------------------------------------------------

#[test]
fn ts_adapter_registered() {
    let registry = default_registry();
    let adapter = registry
        .adapter_for_language("typescript")
        .expect("TsAdapter should be registered");
    assert_eq!(adapter.language_id(), "typescript");
    assert!(adapter.extensions().contains(&"ts"));
    assert!(adapter.extensions().contains(&"tsx"));
    assert!(adapter.extensions().contains(&"mts"));
    assert!(adapter.extensions().contains(&"cts"));

    let fca = adapter.as_fca_source().expect("TS should have FCA source");
    let effect_names: Vec<_> = fca.effect_patterns().iter().map(|p| p.attr_name).collect();
    assert!(effect_names.contains(&"effect:dom"));
    assert!(effect_names.contains(&"effect:net"));
}

// ---------------------------------------------------------------------------
// Pipeline: multi-file workspace
// ---------------------------------------------------------------------------

#[test]
fn ts_files_indexed_in_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("utils.ts"),
        "export function helper(): number { return 42; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("app.ts"),
        "import { helper } from './utils';\nexport const val = helper();\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Button.tsx"),
        r#"
import React from 'react';
interface Props { label: string; }
function Button({ label }: Props) {
  return <button>{label}</button>;
}
export default Button;
"#,
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_ts_entry("ts-pipeline", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 3, "expected 3 TS files parsed");

    let files = db.all_files().unwrap();
    let ts_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == "typescript")
        .collect();
    assert_eq!(ts_files.len(), 3, "expected 3 TS files indexed");
}

#[test]
fn ts_cross_file_import_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("models.ts"),
        "export interface User { id: number; name: string; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("service.ts"),
        "import { User } from './models';\nexport function getUser(): User { return { id: 1, name: 'a' }; }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_ts_entry("ts-imports", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 2);

    let service_file = db
        .file_by_path("service.ts")
        .unwrap()
        .expect("service.ts should be indexed");
    let _models_file = db
        .file_by_path("models.ts")
        .unwrap()
        .expect("models.ts should be indexed");

    let imports = db.imports_for_file(service_file.id).unwrap();
    let paths: Vec<&str> = imports.iter().map(|i| i.imported_path.as_str()).collect();
    assert!(
        paths.contains(&"./models"),
        "service.ts should have import of ./models, got: {paths:?}"
    );

    let models_file = db
        .file_by_path("models.ts")
        .unwrap()
        .expect("models.ts should be indexed");
    let resolved_import = imports
        .iter()
        .find(|i| i.imported_path == "./models")
        .expect("should have ./models import");
    assert_eq!(
        resolved_import.resolved_file_id,
        Some(models_file.id),
        "./models should resolve to models.ts"
    );
}

// ---------------------------------------------------------------------------
// Mixed JS/TS workspace
// ---------------------------------------------------------------------------

#[test]
fn mixed_js_ts_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("legacy.js"),
        "function oldHelper() { return 1; }\nmodule.exports = { oldHelper };\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("modern.ts"),
        "export function newHelper(): number { return 2; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("App.tsx"),
        "import React from 'react';\nfunction App() { return <div>Hello</div>; }\nexport default App;\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "mixed-js-ts".to_string(),
        root: dir.path().to_path_buf(),
        languages: vec!["javascript".to_string(), "typescript".to_string()],
        frozen: false,
    };
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(
        snap.files_parsed, 3,
        "expected 3 files parsed in mixed workspace"
    );

    let files = db.all_files().unwrap();
    let js_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == "javascript")
        .collect();
    let ts_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == "typescript")
        .collect();
    assert_eq!(js_files.len(), 1, "expected 1 JS file");
    assert_eq!(ts_files.len(), 2, "expected 2 TS files (.ts + .tsx)");
}

// ---------------------------------------------------------------------------
// Import resolution: mixed JS/TS cross-imports
// ---------------------------------------------------------------------------

#[test]
fn mixed_js_ts_cross_import_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("utils.ts"),
        "export function greet(name: string): string { return `Hi ${name}`; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        "import { greet } from './utils';\nconsole.log(greet('world'));\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "mixed-cross".to_string(),
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
    let utils_file = db.file_by_path("utils.ts").unwrap().unwrap();
    let imports = db.imports_for_file(app_file.id).unwrap();
    let imp = imports
        .iter()
        .find(|i| i.imported_path == "./utils")
        .unwrap();
    assert_eq!(
        imp.resolved_file_id,
        Some(utils_file.id),
        ".js file importing from .ts should resolve cross-language"
    );
}

// ---------------------------------------------------------------------------
// Import resolution: index.ts resolution
// ---------------------------------------------------------------------------

#[test]
fn ts_import_index_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("lib")).unwrap();
    std::fs::write(
        dir.path().join("lib/index.ts"),
        "export const VERSION = '1.0';\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.ts"),
        "import { VERSION } from './lib';\nconsole.log(VERSION);\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_ts_entry("ts-index", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let main_file = db.file_by_path("main.ts").unwrap().unwrap();
    let index_file = db.file_by_path("lib/index.ts").unwrap().unwrap();
    let imports = db.imports_for_file(main_file.id).unwrap();
    let imp = imports.iter().find(|i| i.imported_path == "./lib").unwrap();
    assert_eq!(
        imp.resolved_file_id,
        Some(index_file.id),
        "./lib should resolve to lib/index.ts"
    );
}

// ---------------------------------------------------------------------------
// Import resolution: tsconfig paths aliases
// ---------------------------------------------------------------------------

#[test]
fn tsconfig_path_alias_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/components")).unwrap();
    std::fs::write(
        dir.path().join("tsconfig.json"),
        r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@/*": ["src/*"],
                    "@components/*": ["src/components/*"]
                }
            }
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/components/Button.ts"),
        "export class Button {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/app.ts"),
        "import { Button } from '@components/Button';\nimport { Button as B2 } from '@/components/Button';\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_ts_entry("tsconfig-paths", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let app_file = db.file_by_path("src/app.ts").unwrap().unwrap();
    let button_file = db
        .file_by_path("src/components/Button.ts")
        .unwrap()
        .unwrap();
    let imports = db.imports_for_file(app_file.id).unwrap();

    let imp1 = imports
        .iter()
        .find(|i| i.imported_path == "@components/Button")
        .expect("should find @components/Button import");
    assert_eq!(
        imp1.resolved_file_id,
        Some(button_file.id),
        "@components/Button should resolve via tsconfig paths"
    );

    let imp2 = imports
        .iter()
        .find(|i| i.imported_path == "@/components/Button")
        .expect("should find @/components/Button import");
    assert_eq!(
        imp2.resolved_file_id,
        Some(button_file.id),
        "@/components/Button should resolve via tsconfig paths"
    );
}

#[test]
fn tsconfig_base_url_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/utils")).unwrap();
    std::fs::write(
        dir.path().join("tsconfig.json"),
        r#"{
            "compilerOptions": {
                "baseUrl": "src"
            }
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/utils/helper.ts"),
        "export function help() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/main.ts"),
        "import { help } from 'utils/helper';\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_ts_entry("tsconfig-baseurl", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let main_file = db.file_by_path("src/main.ts").unwrap().unwrap();
    let helper_file = db.file_by_path("src/utils/helper.ts").unwrap().unwrap();
    let imports = db.imports_for_file(main_file.id).unwrap();

    let imp = imports
        .iter()
        .find(|i| i.imported_path == "utils/helper")
        .expect("should find utils/helper import");
    assert_eq!(
        imp.resolved_file_id,
        Some(helper_file.id),
        "bare specifier should resolve via baseUrl"
    );
}
