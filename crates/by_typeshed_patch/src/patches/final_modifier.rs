//! `@final` decorator → `final` class/def modifier
//!
//! reverse-transpile already rewrites a lone `@final` to `final class`/`final
//! def`, but leaves it alone when it is stacked with other decorators (e.g.
//! `@final`/`@type_check_only`). this finishes the job: the `@final` decorator is
//! removed and the `final` modifier is prefixed to the `class`/`def` keyword,
//! leaving any sibling decorators in place (`@type_check_only` + `final class C`)

use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Stmt};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

pub struct FinalModifier;

impl Patch for FinalModifier {
    fn name(&self) -> &'static str {
        "final-modifier"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        walk(&parsed.syntax().body, source, &mut edits);
        edits
    }
}

fn walk(body: &[Stmt], source: &str, edits: &mut Vec<Edit>) {
    for stmt in body {
        let (decorators, keyword_line_target, nested) = match stmt {
            Stmt::ClassDef(class) => (&class.decorator_list, class.name.range(), Some(&class.body)),
            Stmt::FunctionDef(func) => (&func.decorator_list, func.name.range(), Some(&func.body)),
            Stmt::If(node) => {
                walk(&node.body, source, edits);
                for clause in &node.elif_else_clauses {
                    walk(&clause.body, source, edits);
                }
                continue;
            }
            Stmt::Try(node) => {
                walk(&node.body, source, edits);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk(&h.body, source, edits);
                }
                walk(&node.orelse, source, edits);
                walk(&node.finalbody, source, edits);
                continue;
            }
            Stmt::With(node) => {
                walk(&node.body, source, edits);
                continue;
            }
            _ => continue,
        };

        // match a real `@final` decorator, not the synthetic decorator the
        // parser emits for an existing `final` modifier keyword (its source text
        // starts with `final`, not `@`)
        if let Some(final_decorator) = decorators
            .iter()
            .find(|d| is_final(&d.expression) && source[d.range()].trim_start().starts_with('@'))
        {
            edits.push(delete_line(final_decorator.range(), source));
            edits.push(insert_modifier_before_keyword(keyword_line_target, source));
        }
        if let Some(body) = nested {
            walk(body, source, edits);
        }
    }
}

/// `@final` / `@typing.final`
fn is_final(expr: &Expr) -> bool {
    match expr {
        Expr::Name(name) => name.id.as_str() == "final",
        Expr::Attribute(attr) => attr.attr.as_str() == "final",
        _ => false,
    }
}

/// insert `final ` at the first non-whitespace column of the line that holds
/// the class/def keyword (the line `name` sits on), so it precedes
/// `class`/`def`/`async`
fn insert_modifier_before_keyword(name: TextRange, source: &str) -> Edit {
    let bytes = source.as_bytes();
    let mut line_start = name.start().to_usize();
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let mut keyword = line_start;
    while keyword < bytes.len() && matches!(bytes[keyword], b' ' | b'\t') {
        keyword += 1;
    }
    Edit {
        start: keyword,
        end: keyword,
        replacement: "final ".to_string(),
    }
}

/// delete the whole physical line `range` sits on, including its newline
fn delete_line(range: TextRange, source: &str) -> Edit {
    let bytes = source.as_bytes();
    let mut start = range.start().to_usize();
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = range.end().to_usize();
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() {
        end += 1;
    }
    Edit {
        start,
        end,
        replacement: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = FinalModifier.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_stacked_final_class() {
        let src = "\
@final
@type_check_only
class C:
    x: int
";
        let expected = "\
@type_check_only
final class C:
    x: int
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn converts_final_between_decorators() {
        let src = "\
@type_check_only
@final
class C: ...
";
        let expected = "\
@type_check_only
final class C: ...
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn converts_final_method() {
        let src = "\
class C:
    @final
    @override
    def m(self) -> int: ...
";
        let expected = "\
class C:
    @override
    final def m(self) -> int: ...
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn leaves_non_final() {
        let src = "@type_check_only\nclass C: ...\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn idempotent_on_final_modifier() {
        let src = "@type_check_only\nfinal class C: ...\n";
        assert_eq!(run(src), src);
    }
}
