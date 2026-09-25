//! Back-test harness for sutra/465. Reads JSON lines `{"path","old","new"}` on
//! stdin and prints, per file, the forbidden_pattern matches the edit
//! INTRODUCED — the guard's semantics (`check_proposed_patterns`): multiset of
//! (constraint, enclosing symbol, first snippet line) on `new` minus `old`.
//! Waivers are not consulted. Rules come from the TOML file in argv[1].

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};
use sutra::constraints::patterns::{
    MatchKey, check_forbidden_patterns, match_key, subtract_multiset,
};

#[derive(Deserialize)]
struct Input {
    path: String,
    old: String,
    new: String,
}

#[derive(Serialize)]
struct Hit<'a> {
    path: &'a str,
    line: u32,
    rule: &'a str,
    snippet: &'a str,
    enclosing: Option<&'a str>,
}

fn dump_tree(lang: &str, path: &str) {
    let registry = sutra::parser::adapter::default_registry();
    let adapter = registry
        .adapter_for_language(lang)
        .expect("invariant: known language");
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&adapter.grammar())
        .expect("invariant: grammar loads");
    let source = std::fs::read_to_string(path).expect("invariant: file readable");
    let tree = parser
        .parse(&source, None)
        .expect("invariant: parse succeeds");
    println!("{}", tree.root_node().to_sexp());
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--sexp") {
        let arg = |i| {
            std::env::args()
                .nth(i)
                .expect("invariant: usage --sexp <lang> <file>")
        };
        return dump_tree(&arg(2), &arg(3));
    }
    let rules_path = std::env::args()
        .nth(1)
        .expect("invariant: usage swr-harness <rules.toml>");
    let text = std::fs::read_to_string(&rules_path).expect("invariant: rules file readable");
    let mut rules = sutra::rules::parse_rules(&text).expect("invariant: rules parse");
    let (constraints, errors) = rules.all_constraints();
    for e in &errors {
        eprintln!("rule parse error: {e:?}");
    }
    assert!(
        errors.is_empty(),
        "invariant: every candidate rule compiles"
    );
    let registry = sutra::parser::adapter::default_registry();

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.expect("invariant: stdin readable");
        let input: Input = serde_json::from_str(&line).expect("invariant: well-formed input line");
        let old = check_forbidden_patterns(&constraints, &[(&input.path, &input.old)], &registry);
        let new = check_forbidden_patterns(&constraints, &[(&input.path, &input.new)], &registry);
        let mut prior: HashMap<MatchKey, usize> = HashMap::new();
        for f in &old {
            *prior.entry(match_key(f)).or_default() += 1;
        }
        for f in subtract_multiset(new, prior) {
            let hit = Hit {
                path: &input.path,
                line: f.line.unwrap_or(0),
                rule: f.constraint_name.as_deref().unwrap_or("?"),
                snippet: f.snippet.as_deref().unwrap_or(""),
                enclosing: f.enclosing_symbol.as_deref(),
            };
            let json = serde_json::to_string(&hit).expect("invariant: hit serializes");
            writeln!(out, "{json}").expect("invariant: stdout writable");
        }
    }
}
