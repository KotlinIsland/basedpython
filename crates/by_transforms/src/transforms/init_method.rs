//! AST pass: basedpython `init(...)` method shorthand.
//!
//! The parser emits a `FunctionDef` already named `__init__` and carrying a
//! synthetic `__init_method__` decorator over the keyword text whenever it
//! sees `init(...)` inside a class body. This transform rewrites the source
//! `init` keyword to `def __init__` and promotes any parameter prefixed with
//! `let` to a `self.<name>: <ann> = <name>` line in the method body — the
//! parser also synthesises those assignments into the AST so ty sees the
//! instance attributes without re-parsing the transpiled source.
//!
//! - bodyless `init(self, let a: int, b: str)` becomes a full method with a
//!   synthetic colon and indented body containing the self-assignments
//! - `init(self, ...):` with an existing body has self-assignments prepended

use std::cell::RefCell;

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Parameter, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::callable::lower_type_expr_full;
use crate::type_info::TypeInfo;

pub(crate) struct InitMethod<'src> {
    source: &'src str,
}

impl<'src> InitMethod<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for InitMethod<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            types,
            edits: RefCell::new(Vec::new()),
        };
        for stmt in stmts {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits.into_inner());
    }
}

struct State<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    edits: RefCell<Vec<(TextRange, String)>>,
}

impl State<'_> {
    fn line_indent(&self, pos: TextSize) -> &str {
        super::source_util::line_indent(self.source, pos)
    }

    /// Returns the source span between `param.range.start()` and
    /// `param.name.range.start()` — non-empty when the parameter carries a
    /// basedpython `let` prefix.
    fn let_prefix_range(&self, param: &Parameter) -> Option<TextRange> {
        let prefix = TextRange::new(param.range.start(), param.name.range.start());
        let text = &self.source[usize::from(prefix.start())..usize::from(prefix.end())];
        if text.trim_start().starts_with("let") {
            Some(prefix)
        } else {
            None
        }
    }

    fn push(&self, range: TextRange, repl: String) {
        self.edits.borrow_mut().push((range, repl));
    }

    /// The annotation text for the synthesized `self.<name>: <ann> = <name>`
    /// line. That line is fresh output, so any type-position lowering the
    /// parameter's own annotation gets — a callable arrow `() -> None`, a `T?`
    /// optional, a bare `float` — has to be re-applied here rather than copied
    /// verbatim, or the invalid basedpython surface leaks into the `.py` output.
    /// the required imports and any hoisted `Protocol` class come from the
    /// sibling passes' visit of the same parameter annotation, so this only
    /// needs to reproduce the lowered text. falls back to the source verbatim
    /// when nothing lowers
    fn lower_annotation(&self, ann: &Expr) -> String {
        lower_type_expr_full(self.source, self.types, ann).unwrap_or_else(|| {
            self.source[usize::from(ann.range().start())..usize::from(ann.range().end())].to_owned()
        })
    }

    fn process_function(&mut self, func: &StmtFunctionDef) {
        let Some(dec) = func
            .decorator_list
            .iter()
            .find(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "__init_method__"))
        else {
            return;
        };

        // 1. rewrite `init` keyword to `def __init__`
        self.push(dec.range(), "def __init__".to_owned());

        // `init(...)` implies `self` as the first parameter. if the user
        // omitted it, inject `self` right after the opening `(`
        let first_pos = func
            .parameters
            .posonlyargs
            .first()
            .map(|p| &p.parameter)
            .or_else(|| func.parameters.args.first().map(|p| &p.parameter));
        let has_self = first_pos.is_some_and(|p| p.name.as_str() == "self");
        if !has_self {
            let params_start = func.parameters.range.start();
            let after_paren = params_start + TextSize::from(1u32);
            let has_other_param = first_pos.is_some()
                || func.parameters.vararg.is_some()
                || func.parameters.kwarg.is_some()
                || !func.parameters.kwonlyargs.is_empty();
            let insert = if has_other_param { "self, " } else { "self" };
            self.push(TextRange::new(after_paren, after_paren), insert.to_owned());
        }

        // 2. collect `let` parameters from every parameter slot and strip the
        //    keyword from the source
        let params_end = func.parameters.range.end();
        let mut let_assignments: Vec<String> = Vec::new();
        let mut handle = |param: &Parameter| {
            let Some(prefix) = self.let_prefix_range(param) else {
                return;
            };
            self.push(prefix, String::new());
            let name = param.name.as_str();
            let line = if let Some(ann) = &param.annotation {
                let ann_src = self.lower_annotation(ann);
                format!("self.{name}: {ann_src} = {name}")
            } else {
                format!("self.{name} = {name}")
            };
            let_assignments.push(line);
        };
        for p in &func.parameters.posonlyargs {
            handle(&p.parameter);
        }
        for p in &func.parameters.args {
            handle(&p.parameter);
        }
        if let Some(v) = &func.parameters.vararg {
            handle(v);
        }
        for p in &func.parameters.kwonlyargs {
            handle(&p.parameter);
        }
        if let Some(k) = &func.parameters.kwarg {
            handle(k);
        }

        // 3. insert the self-assignments
        let first_user_stmt = func.body.iter().find(|s| s.range().start() >= params_end);
        if let Some(first) = first_user_stmt {
            if !let_assignments.is_empty() {
                let stmt_indent = self.line_indent(first.range().start()).to_owned();
                let mut text = String::new();
                for line in &let_assignments {
                    text.push_str(line);
                    text.push('\n');
                    text.push_str(&stmt_indent);
                }
                let pos = first.range().start();
                self.push(TextRange::new(pos, pos), text);
            }
        } else {
            let header_indent = self.line_indent(func.range.start()).to_owned();
            let body_indent = format!("{header_indent}    ");
            let mut text = String::from(":");
            if let_assignments.is_empty() {
                text.push_str(" ...");
            } else {
                for line in &let_assignments {
                    text.push('\n');
                    text.push_str(&body_indent);
                    text.push_str(line);
                }
            }
            let pos = func.range.end();
            self.push(TextRange::new(pos, pos), text);
        }
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            self.process_function(f);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    // a `let` parameter whose annotation is a callable arrow lowers in both
    // the parameter position and the synthesized `self.<name>: <ann> = <name>`
    // — the arrow is invalid python and must not leak into either
    #[test]
    fn let_param_callable_annotation_lowers() {
        check(
            indoc! {"
                class A:
                    init(self, let fn: () -> None)
            "},
            indoc! {"
                from typing import Callable
                class A:
                    def __init__(self, fn: Callable[[], None]):
                        self.fn: Callable[[], None] = fn
            "},
        );
    }

    // same lowering on the with-body path, where the assignment is prepended
    // before the first user statement rather than into a synthesized body
    #[test]
    fn let_param_callable_annotation_lowers_with_body() {
        check(
            indoc! {"
                class A:
                    init(self, let fn: (int) -> str):
                        print(\"hi\")
            "},
            indoc! {"
                from typing import Callable
                class A:
                    def __init__(self, fn: Callable[[int], str]):
                        self.fn: Callable[[int], str] = fn
                        print(\"hi\")
            "},
        );
    }

    #[test]
    fn bodyless_init_with_let_params() {
        check(
            indoc! {"
                class A:
                    init(self, let a: int, b: str)
            "},
            indoc! {"
                class A:
                    def __init__(self, a: int, b: str):
                        self.a: int = a
            "},
        );
    }

    #[test]
    fn init_with_body() {
        check(
            indoc! {"
                class A:
                    init(self, a: int):
                        self.b = str(a)
            "},
            indoc! {"
                class A:
                    def __init__(self, a: int):
                        self.b = str(a)
            "},
        );
    }

    #[test]
    fn init_with_body_and_let_params() {
        check(
            indoc! {"
                class A:
                    init(self, let a: int):
                        print(\"hi\")
            "},
            indoc! {"
                class A:
                    def __init__(self, a: int):
                        self.a: int = a
                        print(\"hi\")
            "},
        );
    }

    #[test]
    fn init_no_params_other_than_self() {
        check(
            indoc! {"
                class A:
                    init(self)
            "},
            indoc! {"
                class A:
                    def __init__(self): ...
            "},
        );
    }

    #[test]
    fn multiple_let_params_bodyless() {
        check(
            indoc! {"
                class A:
                    init(self, let a: int, let b: str)
            "},
            indoc! {"
                class A:
                    def __init__(self, a: int, b: str):
                        self.a: int = a
                        self.b: str = b
            "},
        );
    }

    #[test]
    fn let_param_without_annotation() {
        check(
            indoc! {"
                class A:
                    init(self, let a)
            "},
            indoc! {"
                class A:
                    def __init__(self, a):
                        self.a = a
            "},
        );
    }

    #[test]
    fn init_outside_class_unchanged() {
        check("init(5)\n", "init(5)\n");
    }

    #[test]
    fn init_auto_injects_self() {
        check(
            indoc! {"
                class A:
                    init(let a: int, let b: str):
                        self.c = a + str(b)
            "},
            indoc! {"
                class A:
                    def __init__(self, a: int, b: str):
                        self.a: int = a
                        self.b: str = b
                        self.c = a + str(b)
            "},
        );
    }

    #[test]
    fn init_auto_injects_self_bodyless_no_params() {
        check(
            indoc! {"
                class A:
                    init()
            "},
            indoc! {"
                class A:
                    def __init__(self): ...
            "},
        );
    }

    #[test]
    fn init_call_inside_method_is_left_alone() {
        // `init(...)` is the method shorthand only *directly* in a class body.
        // a call to a function named `init` inside a method body (as in
        // cpython's `mimetypes.py`) must stay a plain call, not become a nested
        // `def __init__`
        check(
            indoc! {"
                class C:
                    def __init__(self):
                        init()
            "},
            indoc! {"
                class C:
                    def __init__(self):
                        init()
            "},
        );
    }
}
