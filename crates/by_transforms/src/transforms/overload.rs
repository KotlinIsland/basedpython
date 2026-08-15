//! AST pass: rewrites consecutive bodyless function declarations with the
//! same name to `@overload`-decorated stubs.
//!
//! ```python
//! def f(a: int) -> int
//! def f(a: str) -> str
//! def f(a): ...
//! ```
//! →
//! ```python
//! @overload
//! def f(a: int) -> int: ...
//! @overload
//! def f(a: str) -> str: ...
//! def f(a): ...
//! ```
//!
//! Also adds a `: ...` stub body to any standalone bodyless function def
//! (not part of an overload run). Fires at every nesting level

use std::cell::RefCell;

use ruff_python_ast::visitor::{Visitor, walk_body, walk_stmt};
use ruff_python_ast::{Expr, ModModule, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct Overload<'src> {
    source: &'src str,
    is_stub: bool,
}

impl<'src> Overload<'src> {
    pub(crate) fn new(source: &'src str, is_stub: bool) -> Self {
        Self { source, is_stub }
    }
}

impl AstPass for Overload<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            is_stub: self.is_stub,
            edits: RefCell::new(Vec::new()),
            needs_overload: false,
        };
        state.visit_body(&module.body);
        if state.needs_overload {
            ctx.required_imports
                .push("from typing import overload".to_owned());
        }
        ctx.text_edits.extend(state.edits.into_inner());
    }
}

struct State<'src> {
    source: &'src str,
    is_stub: bool,
    edits: RefCell<Vec<(TextRange, String)>>,
    needs_overload: bool,
}

impl State<'_> {
    fn line_indent(&self, pos: TextSize) -> &str {
        super::source_util::line_indent(self.source, pos)
    }

    fn is_abstract(&self, func: &StmtFunctionDef) -> bool {
        func.decorator_list.iter().any(|dec| {
            super::source_util::is_synthetic_decorator(self.source, dec)
                && matches!(&dec.expression, Expr::Name(n) if n.id.as_str() == "abstract")
        })
    }

    fn is_init_method(func: &StmtFunctionDef) -> bool {
        func.decorator_list
            .iter()
            .any(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "__init_method__"))
    }

    /// a `decorator def` expands to overload stubs plus a runtime dispatcher, so
    /// its own pass writes the whole declaration — including the body a bodyless
    /// one needs. adding `: ...` here as well would land inside that rewrite
    fn is_decorator_keyword(&self, func: &StmtFunctionDef) -> bool {
        func.decorator_list.iter().any(|dec| {
            super::source_util::is_synthetic_decorator(self.source, dec)
                && matches!(&dec.expression, Expr::Name(n) if n.id.as_str() == "decorator_keyword")
        })
    }

    fn push(&self, range: TextRange, repl: String) {
        self.edits.borrow_mut().push((range, repl));
    }

    fn add_stub_body(&mut self, func: &StmtFunctionDef) {
        if Self::is_init_method(func) {
            return;
        }
        let end = func.range().end();
        // in a stub, an abstract method declares no runtime body — `: ...` is
        // the stub idiom and round-trips with the reverse pass. only a runtime
        // `.by` file needs the `raise NotImplementedError` body
        let body = if self.is_abstract(func) && !self.is_stub {
            ": raise NotImplementedError"
        } else {
            ": ..."
        };
        self.push(TextRange::new(end, end), body.to_owned());
    }

    fn is_stub_shaped(func: &StmtFunctionDef) -> bool {
        if func.body.is_empty() {
            return true;
        }
        matches!(
            func.body.as_slice(),
            [Stmt::Expr(e)] if matches!(e.value.as_ref(), Expr::StringLiteral(_))
        )
    }

    fn add_overload_stub(&mut self, func: &StmtFunctionDef) {
        self.needs_overload = true;
        let indent = self.line_indent(func.range().start()).to_owned();
        let start = func.range().start();
        self.push(TextRange::new(start, start), format!("@overload\n{indent}"));
        if func.body.is_empty() {
            let end = func.range().end();
            self.push(TextRange::new(end, end), ": ...".to_owned());
        }
    }

    fn process_body(&mut self, body: &[Stmt]) {
        let mut i = 0;
        while i < body.len() {
            let Stmt::FunctionDef(first) = &body[i] else {
                i += 1;
                continue;
            };
            if Self::is_init_method(first) || self.is_decorator_keyword(first) {
                i += 1;
                continue;
            }
            let name = first.name.id.as_str();

            let mut run_end = i + 1;
            while run_end < body.len() {
                if let Stmt::FunctionDef(f) = &body[run_end] {
                    if f.name.id.as_str() == name && !self.is_decorator_keyword(f) {
                        run_end += 1;
                        continue;
                    }
                }
                break;
            }

            if run_end > i + 1 {
                let is_stub =
                    |s: &Stmt| matches!(s, Stmt::FunctionDef(f) if Self::is_stub_shaped(f));
                let all_stub = body[i..run_end].iter().all(is_stub);
                let all_but_last_stub = body[i..run_end - 1].iter().all(is_stub);

                if all_stub {
                    for stmt in &body[i..run_end] {
                        if let Stmt::FunctionDef(f) = stmt {
                            self.add_overload_stub(f);
                        }
                    }
                } else if all_but_last_stub {
                    for stmt in &body[i..run_end - 1] {
                        if let Stmt::FunctionDef(f) = stmt {
                            self.add_overload_stub(f);
                        }
                    }
                } else {
                    for stmt in &body[i..run_end] {
                        if let Stmt::FunctionDef(f) = stmt {
                            if f.body.is_empty() {
                                self.add_stub_body(f);
                            }
                        }
                    }
                }
            } else if first.body.is_empty() {
                self.add_stub_body(first);
            }

            i = run_end;
        }
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_body(&mut self, body: &'ast [Stmt]) {
        self.process_body(body);
        walk_body(self, body);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple_overload() {
        check(
            indoc! {"
                def f(a: int) -> int
                def f(a: str) -> str
                def f(a): ...
            "},
            indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int: ...
                @overload
                def f(a: str) -> str: ...
                def f(a): ...
            "},
        );
    }

    #[test]
    fn overload_in_class() {
        check(
            indoc! {"
                class A:
                    def method(self, x: int) -> int
                    def method(self, x: str) -> str
                    def method(self, x): ...
            "},
            indoc! {"
                from typing import overload
                class A:
                    @overload
                    def method(self, x: int) -> int: ...
                    @overload
                    def method(self, x: str) -> str: ...
                    def method(self, x): ...
            "},
        );
    }

    #[test]
    fn single_def_not_overloaded() {
        check("def f(a: int) -> int: ...\n", "def f(a: int) -> int: ...\n");
    }

    #[test]
    fn bodyless_single_def_gets_stub() {
        check("def f(a: int) -> int\n", "def f(a: int) -> int: ...\n");
    }

    #[test]
    fn regular_arg_unchanged() {
        check(
            "def f(a: int, b: str) -> bool: ...\n",
            "def f(a: int, b: str) -> bool: ...\n",
        );
    }

    #[test]
    fn stub_style_all_bodyless() {
        check(
            indoc! {"
                def f(a: int) -> int
                def f(a: str) -> str
            "},
            indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int: ...
                @overload
                def f(a: str) -> str: ...
            "},
        );
    }

    #[test]
    fn docstring_bearing_overload_group() {
        check(
            indoc! {"
                def f(a: int) -> int:
                    \"\"\"int variant\"\"\"
                def f(a: str) -> str
            "},
            indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int:
                    \"\"\"int variant\"\"\"
                @overload
                def f(a: str) -> str: ...
            "},
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged(indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int: ...
                @overload
                def f(a: str) -> str: ...
                def f(a): ..."});
    }

    #[test]
    fn abstract_bodyless_gets_raise() {
        check(
            indoc! {"
                from abc import ABC

                class A(ABC):
                    abstract def f(self) -> int
            "},
            indoc! {"
                from abc import abstractmethod
                from abc import ABC

                class A(ABC):
                    @abstractmethod
                    def f(self) -> int: raise NotImplementedError
            "},
        );
    }

    #[test]
    fn init_overload_group_skipped() {
        check(
            indoc! {"
                class A:
                    def __init__(self, a: int) -> None
                    def __init__(self, a: str) -> None
                    def __init__(self, a):
                        self.a = a
            "},
            indoc! {"
                from typing import overload
                class A:
                    @overload
                    def __init__(self, a: int) -> None: ...
                    @overload
                    def __init__(self, a: str) -> None: ...
                    def __init__(self, a):
                        self.a = a
            "},
        );
    }
}
