//! `x: Final[T]` → `final x: T`
//!
//! basedpython spells an explicit `Final` declaration with the `final` modifier
//! (`final x: T`), which ty resolves as `Final` in every scope. only the
//! subscripted `Final[T]` form is converted — a bare `x: Final = v` (inferred
//! type) has no type to move and is left alone

use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Stmt, StmtAnnAssign};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct FinalAnnotation;

impl Patch for FinalAnnotation {
    fn name(&self) -> &'static str {
        "final-annotation"
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
        match stmt {
            Stmt::AnnAssign(assign) => convert(assign, source, edits),
            Stmt::ClassDef(class) => walk(&class.body, source, edits),
            Stmt::If(node) => {
                walk(&node.body, source, edits);
                for clause in &node.elif_else_clauses {
                    walk(&clause.body, source, edits);
                }
            }
            Stmt::Try(node) => {
                walk(&node.body, source, edits);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk(&h.body, source, edits);
                }
                walk(&node.orelse, source, edits);
                walk(&node.finalbody, source, edits);
            }
            Stmt::With(node) => walk(&node.body, source, edits),
            _ => {}
        }
    }
}

fn convert(assign: &StmtAnnAssign, source: &str, edits: &mut Vec<Edit>) {
    // a plain `NAME: Final[T] [= v]`
    if assign.target.as_name_expr().is_none() {
        return;
    }
    let Expr::Subscript(sub) = assign.annotation.as_ref() else {
        return;
    };
    if !is_final(&sub.value) {
        return;
    }
    let inner = &source[sub.slice.range()];
    let target_start = assign.target.range().start().to_usize();
    edits.push(Edit {
        start: target_start,
        end: target_start,
        replacement: "final ".to_string(),
    });
    edits.push(Edit {
        start: assign.annotation.range().start().to_usize(),
        end: assign.annotation.range().end().to_usize(),
        replacement: inner.to_string(),
    });
}

/// `Final` / `typing.Final`
fn is_final(expr: &Expr) -> bool {
    match expr {
        Expr::Name(name) => name.id.as_str() == "Final",
        Expr::Attribute(attr) => attr.attr.as_str() == "Final",
        _ => false,
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
        let edits = FinalAnnotation.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_valueless_final() {
        assert_eq!(run("CONST: Final[int]\n"), "final CONST: int\n");
    }

    #[test]
    fn converts_final_with_value() {
        assert_eq!(run("CONST: Final[int] = 5\n"), "final CONST: int = 5\n");
    }

    #[test]
    fn converts_complex_type() {
        assert_eq!(
            run("X: Final[tuple[int, str]]\n"),
            "final X: tuple[int, str]\n"
        );
    }

    #[test]
    fn converts_in_class() {
        let src = "\
class C:
    TAG: Final[str] = \"c\"
";
        let expected = "\
class C:
    final TAG: str = \"c\"
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn leaves_bare_final() {
        // no subscript — inferred type, nothing to move
        assert_eq!(run("CONST: Final = 5\n"), "CONST: Final = 5\n");
    }

    #[test]
    fn idempotent_on_final_modifier() {
        // already `final x: T` (parses to a `__final__[T]` annotation, not `Final[T]`)
        assert_eq!(run("final CONST: int\n"), "final CONST: int\n");
    }
}
