//! Reverse of `crate::transforms::empty_declarations` and the bodyless-def
//! handling in `crate::transforms::overload`:
//!
//!   `class Foo: ...` → `class Foo`
//!   `def f(x: int) -> int: ...` → `def f(x: int) -> int`   (in a stub)
//!
//! Only fires when the body is exactly a single ellipsis expression statement,
//! which is what the forward transforms emit. `class Foo: pass` /
//! `def f(): pass` and other "trivially empty" forms are intentionally left
//! alone — they're not what the forward produces and rewriting them would lose
//! author intent.
//!
//! A `def` only loses its body in a stub. A class with no body is a whole class
//! wherever it is written, but a `def` with no body is a declaration, and
//! `missing-function-body` reports one written where the position asks for an
//! implementation. This pass cannot tell those positions apart, so outside a
//! stub the `: ...` the author wrote stays — valid basedpython either way.
//!
//! An `@overload`-decorated `def` is left to the overload-reverse pass, which
//! strips the decorator and the body together wherever the group is written.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_body, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

pub(crate) struct EmptyDeclarations {
    /// whether a `def` may be left bodyless: only in a stub, where declaring a
    /// signature is the point and the forward pass re-emits `: ...`
    is_stub: bool,
    pub(crate) edits: Vec<Fix>,
}

impl EmptyDeclarations {
    pub(crate) fn new(is_stub: bool) -> Self {
        Self {
            is_stub,
            edits: Vec::new(),
        }
    }

    fn is_ellipsis_body(body: &[Stmt]) -> bool {
        matches!(
            body,
            [Stmt::Expr(e)] if matches!(e.value.as_ref(), Expr::EllipsisLiteral(_))
        )
    }

    fn process_class(&mut self, class: &StmtClassDef) {
        if !Self::is_ellipsis_body(&class.body) {
            return;
        }

        let mut header_end: TextSize = class.name.range().end();
        if let Some(tp) = &class.type_params {
            header_end = header_end.max(tp.range().end());
        }
        if let Some(args) = &class.arguments {
            header_end = header_end.max(args.range().end());
        }

        let body_end = class.range().end();
        if body_end > header_end {
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    header_end, body_end,
                ))));
        }
    }

    fn process_function(&mut self, func: &StmtFunctionDef) {
        if !self.is_stub {
            return;
        }
        // `@overload`-decorated functions belong to the overload reverse pass,
        // which strips the decorator and the `: ...` body together. other
        // decorators (`@property`, `@deprecated`, modifier-backed ones like
        // `@abstractmethod`/`@final`) are fine to strip the body from — the
        // decorator/modifier survives in front of the now-bodyless def
        if func
            .decorator_list
            .iter()
            .any(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "overload"))
        {
            return;
        }
        if !Self::is_ellipsis_body(&func.body) {
            return;
        }

        let header_end = func
            .returns
            .as_ref()
            .map(|r| r.range().end())
            .unwrap_or_else(|| func.parameters.range().end());
        let body_end = func.range().end();
        if body_end > header_end {
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    header_end, body_end,
                ))));
        }
    }
}

impl EmptyDeclarations {
    fn process_body(&mut self, body: &[Stmt]) {
        for (i, stmt) in body.iter().enumerate() {
            match stmt {
                Stmt::ClassDef(c) => self.process_class(c),
                Stmt::FunctionDef(f) => {
                    // Skip funcs that share their name with a neighbor: those
                    // belong to an overload group, whose reverse pass handles
                    // the `: ...` body removal on its own terms.
                    let name = f.name.id.as_str();
                    let in_group = body.iter().enumerate().any(|(j, other)| {
                        i != j
                            && matches!(other, Stmt::FunctionDef(g) if g.name.id.as_str() == name)
                    });
                    if !in_group {
                        self.process_function(f);
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visitor<'ast> for EmptyDeclarations {
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
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    fn check_stub(input: &str, expected: &str) {
        let config = Config {
            is_stub: true,
            ..Config::test_default()
        };
        assert_eq!(reverse_transpile(input, &config).unwrap(), expected);
    }

    #[test]
    fn single_line_ellipsis() {
        check("class Foo: ...\n", "class Foo\n");
    }

    #[test]
    fn with_base_class() {
        check("class Foo(Base): ...\n", "class Foo(Base)\n");
    }

    #[test]
    fn with_empty_parens() {
        check("class Foo(): ...\n", "class Foo()\n");
    }

    #[test]
    fn multiline_ellipsis_body() {
        check(
            indoc! {"
                class Foo:
                    ...
            "},
            "class Foo\n",
        );
    }

    #[test]
    fn pass_body_unchanged() {
        check(
            indoc! {"
                class Foo:
                    pass
            "},
            indoc! {"
                class Foo:
                    pass
            "},
        );
    }

    #[test]
    fn nonempty_body_unchanged() {
        check(
            indoc! {"
                class Foo:
                    x: int
            "},
            indoc! {"
                class Foo:
                    x: int
            "},
        );
    }

    #[test]
    fn nested_empty_class() {
        check(
            indoc! {"
                class Outer:
                    class Inner: ...
            "},
            indoc! {"
                class Outer:
                    class Inner
            "},
        );
    }

    #[test]
    fn decorated_empty_class() {
        check("@dataclass\nclass Foo: ...\n", "@dataclass\nclass Foo\n");
    }

    #[test]
    fn multiple_empty_classes() {
        check(
            indoc! {"
                class A: ...
                class B(A): ...
            "},
            indoc! {"
                class A
                class B(A)
            "},
        );
    }

    #[test]
    fn single_empty_function_in_stub() {
        check_stub("def f(a: int) -> int: ...\n", "def f(a: int) -> int\n");
    }

    #[test]
    fn empty_function_no_return_type_in_stub() {
        check_stub("def f(): ...\n", "def f()\n");
    }

    #[test]
    fn empty_function_multiline_ellipsis_in_stub() {
        check_stub(
            indoc! {"
                def f(a: int) -> int:
                    ...
            "},
            "def f(a: int) -> int\n",
        );
    }

    #[test]
    fn empty_function_keeps_its_body_outside_a_stub() {
        // a `def` with no body declares a signature, which is what a stub is for. outside one
        // the position may well need an implementation, and nothing here can tell — so what
        // the author wrote stays
        check("def f(a: int) -> int: ...\n", "def f(a: int) -> int: ...\n");
        check("def f(): ...\n", "def f(): ...\n");
    }

    #[test]
    fn empty_class_loses_its_body_outside_a_stub() {
        // a class with no members is a whole class, so this one is not held back
        check("class Foo: ...\n", "class Foo\n");
    }

    #[test]
    fn function_with_pass_unchanged() {
        check(
            indoc! {"
                def f():
                    pass
            "},
            indoc! {"
                def f():
                    pass
            "},
        );
    }

    #[test]
    fn property_decorated_function_stripped_in_stub() {
        // non-`@overload` decorators don't defer to the overload pass; the
        // `: ...` body is stripped and the decorator survives in front
        check_stub(
            indoc! {"
                class A:
                    @property
                    def x(self) -> int: ...
            "},
            indoc! {"
                class A:
                    @property
                    def x(self) -> int
            "},
        );
    }

    #[test]
    fn abstract_function_keeps_body_outside_a_stub() {
        // `@abstractmethod` reverses to the `abstract` modifier, and the `: ...` body stays
        // with it: a bodyless `abstract def` forward-maps to `: raise NotImplementedError`,
        // so stripping it would not round-trip even where a declaration is allowed
        check(
            indoc! {"
                from abc import abstractmethod
                class A:
                    @abstractmethod
                    def f(self) -> None: ...
            "},
            indoc! {"
                from abc import abstractmethod
                class A:
                    abstract def f(self) -> None: ...
            "},
        );
    }

    #[test]
    fn abstract_function_stripped_in_stub() {
        // stub: bodyless is the idiom and the forward pass re-emits `: ...`
        // for an abstract method in a stub, so the body is dropped here
        check_stub(
            indoc! {"
                from abc import abstractmethod
                class A:
                    @abstractmethod
                    def f(self) -> None: ...
            "},
            indoc! {"
                from abc import abstractmethod
                class A:
                    abstract def f(self) -> None
            "},
        );
    }

    #[test]
    fn decorated_function_left_to_overload_pass() {
        // @overload-decorated stubs are handled by the overload reverse pass;
        // empty_declarations must not strip them out from under it.
        check(
            indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int: ...
                @overload
                def f(a: str) -> str: ...
                def f(a): ...
            "},
            indoc! {"
                from typing import overload
                def f(a: int) -> int
                def f(a: str) -> str
                def f(a): ...
            "},
        );
    }

    #[test]
    fn stub_style_no_impl_round_trip() {
        // Stub-style overload group with no implementation: reverse strips
        // `@overload` from all and removes `: ...` bodies.
        check(
            indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int: ...
                @overload
                def f(a: str) -> str: ...
            "},
            indoc! {"
                from typing import overload
                def f(a: int) -> int
                def f(a: str) -> str
            "},
        );
    }
}
