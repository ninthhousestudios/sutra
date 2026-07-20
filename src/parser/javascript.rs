use crate::error::Result;
use crate::parser::ParseResult;
use crate::parser::adapter::ParseContext;

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;
    let line_count = std::str::from_utf8(src)
        .map(|s| s.lines().count())
        .unwrap_or(0);

    Ok(ParseResult {
        file_path: ctx.file_path.to_string(),
        language: "javascript".to_string(),
        symbols: Vec::new(),
        references: Vec::new(),
        imports: Vec::new(),
        parsed_ok,
        line_count,
    })
}
