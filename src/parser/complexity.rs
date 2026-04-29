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

    // Logical operators get +1 but no nesting penalty
    if kind == "binary_expression" && is_logical_operator(node, src) {
        *score += 1;
    }

    // break/continue with labels get +1
    if matches!(lang, "rust") && matches!(kind, "break_expression" | "continue_expression") && has_label(node) {
        *score += 1;
    }
    if matches!(lang, "dart") && matches!(kind, "break_statement" | "continue_statement") && has_label(node) {
        *score += 1;
    }

    let child_nesting = if increments_nesting {
        nesting + 1
    } else {
        nesting
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
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
        _ => (false, false),
    }
}

fn is_logical_operator(node: Node, src: &[u8]) -> bool {
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
}
