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

fn make_python_entry(id: &str, root: PathBuf) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root,
        languages: vec!["python".to_string()],
    }
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

#[test]
fn smoke_function() {
    let r = parse_file("def add(a, b):\n    return a + b\n", "python", "math.py").unwrap();
    assert!(r.parsed_ok);
    assert_eq!(r.symbols.len(), 1);
    let sym = &r.symbols[0];
    assert_eq!(sym.kind, SymbolKind::Function);
    assert_eq!(sym.qualified_name, "add");
    assert_eq!(sym.short_name, "add");
    assert!(
        sym.signature.as_ref().unwrap().contains("def add"),
        "sig: {}",
        sym.signature.as_deref().unwrap()
    );
    assert!(sym.signature_hash.is_some());
    assert_eq!(sym.visibility.as_deref(), Some("pub"));
}

#[test]
fn class_with_methods() {
    let src = "class Greeter:\n    def hello(self):\n        pass\n    def _internal(self):\n        pass\n";
    let r = parse_file(src, "python", "greet.py").unwrap();
    let flat = flatten_symbols(&r.symbols);

    let cls = flat.iter().find(|s| s.short_name == "Greeter").unwrap();
    assert_eq!(cls.kind, SymbolKind::Class);
    assert_eq!(cls.visibility.as_deref(), Some("pub"));

    let hello = flat.iter().find(|s| s.short_name == "hello").unwrap();
    assert_eq!(hello.kind, SymbolKind::Method);
    assert_eq!(hello.qualified_name, "Greeter::hello");
    assert_eq!(hello.visibility.as_deref(), Some("pub"));

    let internal = flat.iter().find(|s| s.short_name == "_internal").unwrap();
    assert_eq!(internal.kind, SymbolKind::Method);
    assert_eq!(internal.visibility.as_deref(), Some("private"));
}

#[test]
fn module_level_constants_and_statics() {
    let src = "MAX_SIZE = 100\ndefault_name = 'world'\n";
    let r = parse_file(src, "python", "config.py").unwrap();
    let max = r
        .symbols
        .iter()
        .find(|s| s.short_name == "MAX_SIZE")
        .unwrap();
    assert_eq!(max.kind, SymbolKind::Const);
    let default = r
        .symbols
        .iter()
        .find(|s| s.short_name == "default_name")
        .unwrap();
    assert_eq!(default.kind, SymbolKind::Static);
}

// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

#[test]
fn import_extraction() {
    let src = "\
import os
import os.path
from pathlib import Path
from . import sibling
from pkg.sub import foo, bar
import sys as system
from mod import *
";
    let r = parse_file(src, "python", "imports.py").unwrap();
    let paths: Vec<&str> = r.imports.iter().map(|i| i.raw_path.as_str()).collect();

    assert!(paths.contains(&"os"), "missing os: {paths:?}");
    assert!(paths.contains(&"os.path"), "missing os.path: {paths:?}");
    assert!(
        paths.contains(&"pathlib.Path"),
        "missing pathlib.Path: {paths:?}"
    );
    assert!(paths.contains(&".sibling"), "missing .sibling: {paths:?}");
    assert!(
        paths.contains(&"pkg.sub.foo"),
        "missing pkg.sub.foo: {paths:?}"
    );
    assert!(
        paths.contains(&"pkg.sub.bar"),
        "missing pkg.sub.bar: {paths:?}"
    );
    assert!(paths.contains(&"sys"), "missing sys (alias): {paths:?}");
    assert!(paths.contains(&"mod"), "missing mod (wildcard): {paths:?}");

    let os_imp = r.imports.iter().find(|i| i.raw_path == "os").unwrap();
    assert_eq!(os_imp.kind, "import");
    let path_imp = r
        .imports
        .iter()
        .find(|i| i.raw_path == "pathlib.Path")
        .unwrap();
    assert_eq!(path_imp.kind, "from_import");
}

// ---------------------------------------------------------------------------
// Decorator / language_attrs
// ---------------------------------------------------------------------------

#[test]
fn decorator_attrs_staticmethod() {
    let src = "\
class Foo:
    @staticmethod
    def create():
        pass
";
    let r = parse_file(src, "python", "decorators.py").unwrap();
    let flat = flatten_symbols(&r.symbols);
    let create = flat.iter().find(|s| s.short_name == "create").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(create.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["has_decorator"], true);
    assert_eq!(attrs["decorator:staticmethod"], true);
}

#[test]
fn decorator_attrs_property() {
    let src = "\
class Foo:
    @property
    def name(self):
        return self._name
";
    let r = parse_file(src, "python", "prop.py").unwrap();
    let flat = flatten_symbols(&r.symbols);
    let name = flat.iter().find(|s| s.short_name == "name").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(name.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["has_decorator"], true);
    assert_eq!(attrs["decorator:property"], true);
}

#[test]
fn decorator_attrs_pytest_fixture() {
    let src = "\
import pytest

@pytest.fixture
def db_conn():
    return connect()
";
    let r = parse_file(src, "python", "conftest.py").unwrap();
    let flat = flatten_symbols(&r.symbols);
    let db_conn = flat.iter().find(|s| s.short_name == "db_conn").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(db_conn.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["has_decorator"], true);
    assert_eq!(attrs["decorator:pytest.fixture"], true);
}

// ---------------------------------------------------------------------------
// Async generator
// ---------------------------------------------------------------------------

#[test]
fn async_generator_attrs() {
    let src = "async def stream(items):\n    for item in items:\n        yield item\n";
    let r = parse_file(src, "python", "stream.py").unwrap();
    let sym = &r.symbols[0];
    let attrs: serde_json::Value =
        serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["is_async"], true, "should be async");
    assert_eq!(attrs["is_generator"], true, "should be generator");
}

// ---------------------------------------------------------------------------
// Complexity scoring
// ---------------------------------------------------------------------------

#[test]
fn complexity_scoring() {
    let src = "\
def process(items):
    results = []
    for item in items:
        if item.valid:
            try:
                results.append(item.transform())
            except ValueError:
                pass
    filtered = [r for r in results if r is not None]
    return filtered
";
    let r = parse_file(src, "python", "complex.py").unwrap();
    let sym = &r.symbols[0];
    // for + if + except + list_comprehension = at least 5 cyclomatic
    assert!(
        sym.cyclomatic.unwrap() >= 5,
        "cyclomatic={}",
        sym.cyclomatic.unwrap()
    );
    assert!(
        sym.cognitive.unwrap() > 0,
        "cognitive should be non-zero, got {}",
        sym.cognitive.unwrap()
    );
    // for > if > try = nesting depth >= 3
    assert!(
        sym.max_nesting.unwrap() >= 3,
        "max_nesting={}",
        sym.max_nesting.unwrap()
    );
}

// ---------------------------------------------------------------------------
// TestCase detection (FLAG_TEST = 0x01)
// ---------------------------------------------------------------------------

#[test]
fn testcase_detection() {
    let src = "\
import unittest

class TestMath(unittest.TestCase):
    def test_add(self):
        self.assertEqual(1 + 1, 2)

    def helper(self):
        pass

def test_standalone():
    assert True
";
    // Use a non-test filename — TestCase inheritance and test_ prefix should
    // still trigger FLAG_TEST independent of file path.
    let r = parse_file(src, "python", "math_utils.py").unwrap();
    let flat = flatten_symbols(&r.symbols);

    let test_class = flat.iter().find(|s| s.short_name == "TestMath").unwrap();
    assert_ne!(
        test_class.flags & 0x01,
        0,
        "TestCase subclass should have FLAG_TEST"
    );

    let test_add = flat.iter().find(|s| s.short_name == "test_add").unwrap();
    assert_ne!(
        test_add.flags & 0x01,
        0,
        "test_ method should have FLAG_TEST"
    );

    let test_standalone = flat
        .iter()
        .find(|s| s.short_name == "test_standalone")
        .unwrap();
    assert_ne!(
        test_standalone.flags & 0x01,
        0,
        "test_ function should have FLAG_TEST"
    );

    let helper = flat.iter().find(|s| s.short_name == "helper").unwrap();
    assert_eq!(
        helper.flags & 0x01,
        0,
        "non-test method in non-test file should NOT have FLAG_TEST"
    );
}

// ---------------------------------------------------------------------------
// Multi-file import resolution (pipeline)
// ---------------------------------------------------------------------------

#[test]
fn cross_file_import_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("mypackage");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("__init__.py"), "").unwrap();
    std::fs::write(
        pkg.join("models.py"),
        "class User:\n    pass\n\nclass Post:\n    pass\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("app.py"),
        "from mypackage.models import User\n\ndef handler(u: User):\n    pass\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_python_entry("py-imports", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 3);

    let edges = db.import_edges().unwrap();
    let app_file = db
        .file_by_path("mypackage/app.py")
        .unwrap()
        .expect("app.py should be indexed");
    let models_file = db
        .file_by_path("mypackage/models.py")
        .unwrap()
        .expect("models.py should be indexed");
    assert!(
        edges
            .iter()
            .any(|&(from, to)| from == app_file.id && to == models_file.id),
        "app.py should have a resolved import edge to models.py, edges: {edges:?}"
    );
}

#[test]
fn cross_file_import_resolution_src_layout() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src").join("mypackage");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("__init__.py"), "").unwrap();
    std::fs::write(pkg.join("models.py"), "class User:\n    pass\n").unwrap();
    std::fs::write(
        pkg.join("app.py"),
        "from mypackage.models import User\n\ndef handler(u: User):\n    pass\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_python_entry("py-src-layout", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 3);

    let edges = db.import_edges().unwrap();
    let app_file = db
        .file_by_path("src/mypackage/app.py")
        .unwrap()
        .expect("app.py should be indexed");
    let models_file = db
        .file_by_path("src/mypackage/models.py")
        .unwrap()
        .expect("models.py should be indexed");
    assert!(
        edges
            .iter()
            .any(|&(from, to)| from == app_file.id && to == models_file.id),
        "app.py should have a resolved import edge to models.py in src layout, edges: {edges:?}"
    );
}

// ---------------------------------------------------------------------------
// Effect detection end-to-end
// ---------------------------------------------------------------------------

#[test]
fn effect_detection_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("effects.py"),
        "\
def read_file(path):
    f = open(path)
    return f.read()

def fetch_url(url):
    import requests
    return requests.get(url)

def run_cmd(cmd):
    import subprocess
    return subprocess.run(cmd)
",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_python_entry("py-effects", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let file = db
        .file_by_path("effects.py")
        .unwrap()
        .expect("effects.py should be indexed");
    let syms = db.find_symbols_by_file(file.id).unwrap();
    let refs = db.find_refs_in_file(file.id).unwrap();
    let adapter = registry.adapter_for_language("python").unwrap();
    let fca_source = adapter.as_fca_source().unwrap();
    let callee_cache = HashMap::new();

    for (fn_name, expected_effect) in [
        ("read_file", "effect:fs"),
        ("fetch_url", "effect:net"),
        ("run_cmd", "effect:process"),
    ] {
        let sym = syms
            .iter()
            .find(|s| &*s.short_name == fn_name)
            .unwrap_or_else(|| panic!("missing symbol {fn_name}"));
        let mut attrs = extract_attrs_for_symbol(sym, "effects.py", "python", &registry)
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
// Adapter registration + effect patterns
// ---------------------------------------------------------------------------

#[test]
fn python_adapter_registered() {
    let registry = default_registry();
    let adapter = registry
        .adapter_for_language("python")
        .expect("PythonAdapter should be registered");
    assert_eq!(adapter.language_id(), "python");
    assert!(adapter.extensions().contains(&"py"));

    let fca = adapter
        .as_fca_source()
        .expect("Python should have FCA source");
    let effect_names: Vec<_> = fca.effect_patterns().iter().map(|p| p.attr_name).collect();
    assert!(effect_names.contains(&"effect:fs"));
    assert!(effect_names.contains(&"effect:net"));
    assert!(effect_names.contains(&"effect:db"));
    assert!(effect_names.contains(&"effect:io"));
    assert!(effect_names.contains(&"effect:process"));
}

// ---------------------------------------------------------------------------
// Status: Python files appear as indexed
// ---------------------------------------------------------------------------

#[test]
fn python_files_indexed_in_status() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.py"), "def main():\n    pass\n").unwrap();
    std::fs::write(dir.path().join("utils.py"), "def helper():\n    pass\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_python_entry("py-status", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 2);

    let files = db.all_files().unwrap();
    let py_files: Vec<_> = files.iter().filter(|f| f.language == "python").collect();
    assert_eq!(py_files.len(), 2, "expected 2 Python files indexed");
}
