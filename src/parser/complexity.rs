use tree_sitter::Node;

/// Cyclomatic complexity: count of linearly independent paths.
/// Starts at 1, increments for each decision point.
pub fn cyclomatic(node: Node, src: &[u8], lang: &str) -> u32 {
    let mut count = 1;
    walk_cyclomatic(node, src, lang, &mut count);
    count
}

fn walk_cyclomatic(node: Node, src: &[u8], lang: &str, count: &mut u32) {
    let kind = node.kind();

    match lang {
        "rust" => match kind {
            "if_expression" | "while_expression" | "for_expression" | "loop_expression" => {
                *count += 1;
            }
            "match_arm" => {
                *count += 1;
            }
            "match_expression" => {
                *count = count.saturating_sub(1);
            }
            "binary_expression" if is_logical_operator(node, src) => {
                *count += 1;
            }
            "try_expression" => {
                *count += 1;
            }
            _ => {}
        },
        "dart" => match kind {
            "if_statement" | "while_statement" | "for_statement" | "do_statement" => {
                *count += 1;
            }
            "switch_case" => {
                *count += 1;
            }
            "switch_statement" => {
                *count = count.saturating_sub(1);
            }
            "conditional_expression" => {
                *count += 1;
            }
            "binary_expression" if is_logical_operator(node, src) => {
                *count += 1;
            }
            _ => {}
        },
        "c" => match kind {
            "if_statement"
            | "while_statement"
            | "for_statement"
            | "do_statement"
            | "conditional_expression" => {
                *count += 1;
            }
            "case_statement" => {
                *count += 1;
            }
            "switch_statement" => {
                *count = count.saturating_sub(1);
            }
            "binary_expression" if is_logical_operator(node, src) => {
                *count += 1;
            }
            "goto_statement" => {
                *count += 1;
            }
            _ => {}
        },
        "python" => match kind {
            "if_statement"
            | "elif_clause"
            | "while_statement"
            | "for_statement"
            | "conditional_expression"
            | "except_clause"
            | "list_comprehension"
            | "dictionary_comprehension"
            | "set_comprehension"
            | "generator_expression" => {
                *count += 1;
            }
            _ if is_logical_operator(node, src) => {
                *count += 1;
            }
            _ => {}
        },
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_cyclomatic(child, src, lang, count);
    }
}

/// Cognitive complexity (Sonar model): penalizes nesting and control flow breaks.
pub fn cognitive(node: Node, src: &[u8], lang: &str) -> u32 {
    let mut score = 0;
    walk_cognitive(node, src, lang, 0, &mut score);
    score
}

fn walk_cognitive(node: Node, src: &[u8], lang: &str, nesting: u32, score: &mut u32) {
    let kind = node.kind();

    let (is_flow_break, increments_nesting) = classify_cognitive(kind, lang);

    if is_flow_break {
        // +1 for the construct itself, +nesting for depth
        *score += 1 + nesting;
    }

    if is_logical_operator(node, src) {
        *score += 1;
    }

    // break/continue with labels get +1
    if matches!(lang, "rust")
        && matches!(kind, "break_expression" | "continue_expression")
        && has_label(node)
    {
        *score += 1;
    }
    if matches!(lang, "dart")
        && matches!(kind, "break_statement" | "continue_statement")
        && has_label(node)
    {
        *score += 1;
    }

    let child_nesting = if increments_nesting {
        nesting + 1
    } else {
        nesting
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Python elif/else: score at parent nesting to keep the chain flat
        if lang == "python" && child.kind() == "elif_clause" {
            *score += 1 + nesting;
            let mut inner_cursor = child.walk();
            for grandchild in child.children(&mut inner_cursor) {
                walk_cognitive(grandchild, src, lang, child_nesting, score);
            }
            continue;
        }
        if lang == "python" && child.kind() == "else_clause" {
            *score += 1;
            let mut inner_cursor = child.walk();
            for grandchild in child.children(&mut inner_cursor) {
                walk_cognitive(grandchild, src, lang, child_nesting, score);
            }
            continue;
        }

        // else-if chains: the else clause increments but the nested if does NOT
        // add nesting — it's a flat chain, not deeper nesting.
        if lang == "rust" && child.kind() == "else_clause" {
            // +1 for else itself
            *score += 1;
            let mut inner_cursor = child.walk();
            for grandchild in child.children(&mut inner_cursor) {
                if grandchild.kind() == "if_expression" {
                    // else-if: don't increase nesting for the chained if
                    walk_cognitive(grandchild, src, lang, nesting, score);
                } else {
                    walk_cognitive(grandchild, src, lang, child_nesting, score);
                }
            }
            continue;
        }

        walk_cognitive(child, src, lang, child_nesting, score);
    }
}

/// Returns (is_flow_break, increments_nesting) for cognitive complexity.
fn classify_cognitive(kind: &str, lang: &str) -> (bool, bool) {
    match lang {
        "rust" => match kind {
            "if_expression" => (true, true),
            "while_expression" | "for_expression" | "loop_expression" => (true, true),
            "match_expression" => (true, false),
            // Closures increment nesting but aren't a flow break
            "closure_expression" => (false, true),
            _ => (false, false),
        },
        "dart" => match kind {
            "if_statement" => (true, true),
            "while_statement" | "for_statement" | "do_statement" => (true, true),
            "switch_statement" => (true, false),
            "conditional_expression" => (true, true),
            // Anonymous functions increment nesting
            "function_expression" => (false, true),
            _ => (false, false),
        },
        "c" => match kind {
            "if_statement" | "conditional_expression" => (true, true),
            "while_statement" | "for_statement" | "do_statement" => (true, true),
            "switch_statement" => (true, false),
            "case_statement" | "goto_statement" => (true, false),
            _ => (false, false),
        },
        "python" => match kind {
            "if_statement" | "conditional_expression" => (true, true),
            "while_statement" | "for_statement" => (true, true),
            "try_statement" => (true, true),
            "except_clause" => (true, false),
            "list_comprehension"
            | "dictionary_comprehension"
            | "set_comprehension"
            | "generator_expression" => (true, false),
            "lambda" => (false, true),
            _ => (false, false),
        },
        _ => (false, false),
    }
}

pub fn max_nesting_depth(node: Node, src: &[u8], lang: &str) -> u32 {
    let mut max_depth = 0;
    walk_nesting(node, src, lang, 0, &mut max_depth);
    max_depth
}

fn walk_nesting(node: Node, _src: &[u8], lang: &str, current_depth: u32, max_depth: &mut u32) {
    let kind = node.kind();
    let (_, increments_nesting) = classify_cognitive(kind, lang);
    let new_depth = if increments_nesting {
        current_depth + 1
    } else {
        current_depth
    };
    if new_depth > *max_depth {
        *max_depth = new_depth;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if lang == "rust" && child.kind() == "else_clause" {
            let mut inner_cursor = child.walk();
            for grandchild in child.children(&mut inner_cursor) {
                if grandchild.kind() == "if_expression" {
                    walk_nesting(grandchild, _src, lang, current_depth, max_depth);
                } else {
                    walk_nesting(grandchild, _src, lang, new_depth, max_depth);
                }
            }
            continue;
        }
        walk_nesting(child, _src, lang, new_depth, max_depth);
    }
}

fn is_logical_operator(node: Node, src: &[u8]) -> bool {
    if node.kind() == "boolean_operator" {
        return true;
    }
    if let Some(op) = node.child_by_field_name("operator")
        && let Ok(text) = op.utf8_text(src)
    {
        return text == "&&" || text == "||";
    }
    false
}

fn has_label(node: Node) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| c.kind() == "label" || c.kind() == "loop_label")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_rust_fn(src: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(src, None).unwrap()
    }

    fn body_node(tree: &tree_sitter::Tree) -> Node<'_> {
        let root = tree.root_node();
        let fn_item = root.child(0).unwrap();
        fn_item.child_by_field_name("body").unwrap()
    }

    #[test]
    fn simple_function_complexity_1() {
        let src = "fn foo() { 42 }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        assert_eq!(cyclomatic(body, src.as_bytes(), "rust"), 1);
        assert_eq!(cognitive(body, src.as_bytes(), "rust"), 0);
    }

    #[test]
    fn single_if() {
        let src = "fn foo(x: bool) { if x { 1 } else { 2 } }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        assert_eq!(cyclomatic(body, src.as_bytes(), "rust"), 2);
        // cognitive: +1 (if, nesting=0) +1 (else)
        assert_eq!(cognitive(body, src.as_bytes(), "rust"), 2);
    }

    #[test]
    fn nested_if() {
        let src = "fn foo(a: bool, b: bool) { if a { if b { 1 } } }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        assert_eq!(cyclomatic(body, src.as_bytes(), "rust"), 3);
        // cognitive: +1 (outer if, nesting=0) + +1+1 (inner if, nesting=1) = 3
        assert_eq!(cognitive(body, src.as_bytes(), "rust"), 3);
    }

    #[test]
    fn logical_operators() {
        let src = "fn foo(a: bool, b: bool, c: bool) { if a && b || c { 1 } }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        // cyclomatic: 1 + 1(if) + 1(&&) + 1(||) = 4
        assert_eq!(cyclomatic(body, src.as_bytes(), "rust"), 4);
    }

    #[test]
    fn match_arms() {
        let src = r#"fn foo(x: i32) { match x { 1 => {}, 2 => {}, _ => {} } }"#;
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        // cyclomatic: 1 + 3(arms) - 1(match correction) = 3
        assert_eq!(cyclomatic(body, src.as_bytes(), "rust"), 3);
    }

    #[test]
    fn for_loop() {
        let src = "fn foo() { for i in 0..10 { if i > 5 { break; } } }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        // cyclomatic: 1 + 1(for) + 1(if) = 3
        assert_eq!(cyclomatic(body, src.as_bytes(), "rust"), 3);
        // cognitive: +1(for, nesting=0) + +1+1(if, nesting=1) = 3
        assert_eq!(cognitive(body, src.as_bytes(), "rust"), 3);
    }

    #[test]
    fn nesting_depth_flat() {
        let src = "fn foo() { 42 }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        assert_eq!(max_nesting_depth(body, src.as_bytes(), "rust"), 0);
    }

    #[test]
    fn nesting_depth_single_if() {
        let src = "fn foo(x: bool) { if x { 1 } }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        assert_eq!(max_nesting_depth(body, src.as_bytes(), "rust"), 1);
    }

    #[test]
    fn nesting_depth_nested() {
        let src = "fn foo(a: bool, b: bool) { if a { for i in 0..10 { if b { } } } }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        assert_eq!(max_nesting_depth(body, src.as_bytes(), "rust"), 3);
    }

    #[test]
    fn nesting_depth_else_if_chain_is_flat() {
        let src = "fn foo(a: i32) { if a > 0 { } else if a < 0 { } else { } }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        assert_eq!(max_nesting_depth(body, src.as_bytes(), "rust"), 1);
    }

    #[test]
    fn nesting_depth_closure() {
        let src = "fn foo() { let f = |x| { if x { } }; }";
        let tree = parse_rust_fn(src);
        let body = body_node(&tree);
        // closure increments nesting, if inside closure = depth 2
        assert_eq!(max_nesting_depth(body, src.as_bytes(), "rust"), 2);
    }

    fn parse_python_fn(src: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        parser.parse(src, None).unwrap()
    }

    fn python_body_node(tree: &tree_sitter::Tree) -> Node<'_> {
        let root = tree.root_node();
        let fn_def = root.child(0).unwrap();
        fn_def.child_by_field_name("body").unwrap()
    }

    #[test]
    fn python_cyclomatic_comprehensive() {
        let src = "\
def foo(items, flag):
    for x in items:
        if x > 0 and flag:
            pass
        elif x < 0 or flag:
            pass
    try:
        pass
    except ValueError:
        pass
    result = [x for x in items]
";
        let tree = parse_python_fn(src);
        let body = python_body_node(&tree);
        // 1(base) + 1(for) + 1(if) + 1(and) + 1(elif) + 1(or) + 1(except) + 1(list_comp) = 8
        assert_eq!(cyclomatic(body, src.as_bytes(), "python"), 8);
    }

    #[test]
    fn python_cognitive_nested() {
        let src = "\
def foo(a, b):
    if a:
        for x in range(b):
            if x > 0:
                pass
";
        let tree = parse_python_fn(src);
        let body = python_body_node(&tree);
        // if@0: +1, for@1: +2, if@2: +3 = 6
        assert_eq!(cognitive(body, src.as_bytes(), "python"), 6);
    }

    #[test]
    fn python_cognitive_try_except() {
        let src = "\
def foo():
    try:
        if True:
            pass
    except:
        pass
";
        let tree = parse_python_fn(src);
        let body = python_body_node(&tree);
        // try@0: +1, if@1: +2, except@1(flow_break, no nesting incr): +2 = 5
        assert_eq!(cognitive(body, src.as_bytes(), "python"), 5);
    }
}
