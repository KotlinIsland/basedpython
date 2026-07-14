//! `def __init__(self, ...) -> None` → `init(self, ...)`
//!
//! basedpython's `init(...)` shorthand is exactly a `__init__` returning `None`.
//! only a plain, undecorated getter-shaped `__init__` is converted; a decorated
//! one (`@overload`, ...) keeps the explicit form

use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct InitShorthand;

impl Patch for InitShorthand {
    fn name(&self) -> &'static str {
        "init-shorthand"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, _source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        walk_classes(&parsed.syntax().body, &mut |class| {
            for member in &class.body {
                if let Stmt::FunctionDef(func) = member
                    && is_convertible_init(func)
                {
                    // `def __init__` (through the name / type-params) → `init`
                    edits.push(Edit {
                        start: func.range().start().to_usize(),
                        end: func.name.range().end().to_usize(),
                        replacement: "init".to_string(),
                    });
                    // drop the ` -> None` return annotation
                    if let Some(returns) = &func.returns {
                        edits.push(Edit {
                            start: func.parameters.range().end().to_usize(),
                            end: returns.range().end().to_usize(),
                            replacement: String::new(),
                        });
                    }
                }
            }
        });
        edits
    }
}

fn is_convertible_init(func: &StmtFunctionDef) -> bool {
    func.name.as_str() == "__init__"
        && func.decorator_list.is_empty()
        && !func.is_async
        // the `init(...)` shorthand has no type-parameter form; leave a generic
        // `__init__[T]` as the explicit `def`
        && func.type_params.is_none()
        && matches!(func.returns.as_deref(), Some(Expr::NoneLiteral(_)))
}

fn walk_classes<'a>(body: &'a [Stmt], f: &mut impl FnMut(&'a StmtClassDef)) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class) => {
                f(class);
                walk_classes(&class.body, f);
            }
            Stmt::If(node) => {
                walk_classes(&node.body, f);
                for clause in &node.elif_else_clauses {
                    walk_classes(&clause.body, f);
                }
            }
            Stmt::Try(node) => {
                walk_classes(&node.body, f);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk_classes(&h.body, f);
                }
                walk_classes(&node.orelse, f);
                walk_classes(&node.finalbody, f);
            }
            Stmt::With(node) => walk_classes(&node.body, f),
            _ => {}
        }
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
        let edits = InitShorthand.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_bodyless_init() {
        let src = "\
class C:
    def __init__(self, x: int, /) -> None
";
        let expected = "\
class C:
    init(self, x: int, /)
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn converts_init_with_docstring() {
        let src = "\
class C:
    def __init__(self) -> None:
        \"\"\"doc\"\"\"
";
        let expected = "\
class C:
    init(self):
        \"\"\"doc\"\"\"
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn leaves_decorated_init() {
        let src = "\
class C:
    @overload
    def __init__(self, x: int) -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_non_none_return() {
        let src = "\
class C:
    def __init__(self) -> int: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_generic_init() {
        // the `init(...)` shorthand has no type-parameter form
        let src = "\
class C:
    def __init__[T](self, x: T) -> None
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn idempotent_on_converted_form() {
        // a re-parsed `init(...)` carries the synthetic `__init_method__`
        // decorator, so the `decorator_list.is_empty()` guard no-ops a second pass
        for src in [
            "class C:\n    init(self, x: int, /)\n",
            "class C:\n    init(self):\n        \"\"\"doc\"\"\"\n",
        ] {
            assert_eq!(run(src), src, "not idempotent on {src:?}");
        }
    }
}
