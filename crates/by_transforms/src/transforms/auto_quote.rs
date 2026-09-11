//! Type-aware pass: quotes the forward references python would fail to
//! evaluate
//!
//! basedpython has no manual forward-reference syntax — a string in an
//! annotation is a string-literal *type* — and the checker reads every
//! annotation as deferred, so `def f() -> Later` is fine with `class Later`
//! further down. python before 3.14 evaluates an annotation as its definition
//! runs, and there the same annotation raises `NameError`. this pass quotes
//! each reference that would:
//!
//! `class A: def f(self) -> A` → `def f(self) -> "A"`
//!
//! an annotation is evaluated when its definition runs if it is a parameter or
//! return annotation, or annotates a class-body or module-level variable. a
//! local variable's annotation is never evaluated. which names need quoting is
//! ty's to say ([`TypeInfo::is_forward_reference`]): one the program binds, but
//! not by the point the annotation runs — defined further down, the class the
//! annotation sits in, or imported only under `if TYPE_CHECKING:`
//!
//! annotation quoting is skipped when annotations are not evaluated eagerly:
//! on python >= 3.14 they are deferred natively (PEP 649), and a user-written
//! or opt-in `from __future__ import annotations` defers every one. a class
//! *base*, and a value-position subscript in a class body (`list[A]()`),
//! evaluates while the class is being built on every version, so a
//! self-reference there is always quoted. a direct base (`class A(A):`) is
//! left alone — that is a runtime error regardless of quoting
//!
//! an annotation holding a forward reference is quoted whole, as one wrapper
//! template passing its source through. no lowering reaches past the annotation
//! it lowers, so whatever the annotation becomes — a callable arrow, a `T?`, a
//! renamed type parameter, even when a lowering rewrites the whole annotation
//! as text — ends up between the quotes. a base or a value-position subscript
//! cannot be a string, so there only the self-reference itself is quoted

use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{AnyParameterRef, Expr, ExprName, PythonVersion, Stmt, StmtClassDef};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_one_type_expr};
use crate::type_info::TypeInfo;

pub(crate) struct AutoQuote<'src> {
    source: &'src str,
    min_version: PythonVersion,
    inject_future: bool,
}

impl<'src> AutoQuote<'src> {
    pub(crate) fn new(source: &'src str, min_version: PythonVersion, inject_future: bool) -> Self {
        Self {
            source,
            min_version,
            inject_future,
        }
    }
}

impl TypeAwarePass for AutoQuote<'_> {
    // nothing in a stub is evaluated, and a checker reads a forward reference
    // in one without quotes
    fn runtime_only(&self) -> bool {
        true
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let quote_annotations = !(self.min_version.defers_annotations()
            || self.inject_future
            || has_future_annotations(stmts));
        let mut walk = Walk {
            source: self.source,
            types,
            quote_annotations,
            edits: Vec::new(),
        };
        walk.block(stmts, Scope::Module);
        ctx.template_edits.extend(walk.edits);
    }
}

fn has_future_annotations(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| {
        matches!(s, Stmt::ImportFrom(node)
            if node.module.as_deref() == Some("__future__")
                && node.names.iter().any(|a| a.name.as_str() == "annotations"))
    })
}

/// the kind of scope a block of statements runs in
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Module,
    Class,
    Function,
}

struct Walk<'a> {
    source: &'a str,
    types: &'a dyn TypeInfo,
    quote_annotations: bool,
    edits: Vec<(TextRange, Vec<Fragment>)>,
}

impl Walk<'_> {
    fn block(&mut self, stmts: &[Stmt], scope: Scope) {
        for stmt in stmts {
            self.stmt(stmt, scope);
        }
    }

    fn stmt(&mut self, stmt: &Stmt, scope: Scope) {
        match stmt {
            Stmt::ClassDef(class) => self.class(class),
            Stmt::FunctionDef(function) => {
                if self.quote_annotations {
                    for parameter in function
                        .parameters
                        .iter()
                        .map(AnyParameterRef::as_parameter)
                    {
                        if let Some(annotation) = parameter.annotation.as_deref() {
                            self.annotation(annotation);
                        }
                    }
                    if let Some(returns) = &function.returns {
                        self.annotation(returns);
                    }
                }
                self.block(&function.body, Scope::Function);
            }
            // a local variable's annotation is never evaluated
            Stmt::AnnAssign(assign) if scope != Scope::Function && self.quote_annotations => {
                self.annotation(&assign.annotation);
            }
            Stmt::If(node) => {
                self.block(&node.body, scope);
                for clause in &node.elif_else_clauses {
                    self.block(&clause.body, scope);
                }
            }
            Stmt::While(node) => {
                self.block(&node.body, scope);
                self.block(&node.orelse, scope);
            }
            Stmt::For(node) => {
                self.block(&node.body, scope);
                self.block(&node.orelse, scope);
            }
            Stmt::With(node) => self.block(&node.body, scope),
            Stmt::Try(node) => {
                self.block(&node.body, scope);
                for ruff_python_ast::ExceptHandler::ExceptHandler(handler) in &node.handlers {
                    self.block(&handler.body, scope);
                }
                self.block(&node.orelse, scope);
                self.block(&node.finalbody, scope);
            }
            Stmt::Match(node) => {
                for case in &node.cases {
                    self.block(&case.body, scope);
                }
            }
            _ => {}
        }
    }

    fn annotation(&mut self, annotation: &Expr) {
        let types = self.types;
        let is_forward = |name: &ExprName| types.is_forward_reference(name) == Some(true);
        let mut quoter = Quoter {
            source: self.source,
            is_forward: &is_forward,
            edits: &mut self.edits,
            skip_root: false,
        };
        if quoter.contains_forward_reference(annotation) {
            quoter.quote(annotation.range());
        }
    }

    /// a class's bases and the value-position subscripts in its body run while
    /// the class is being built, whatever the version, and the class's own name
    /// is not bound until it is
    fn class(&mut self, class: &StmtClassDef) {
        let class_name = class.name.id.as_str();
        let is_self = |name: &ExprName| name.id.as_str() == class_name;
        if let Some(arguments) = &class.arguments {
            for base in &arguments.args {
                // a direct `class A(A)` base must not be quoted — that would
                // mask a runtime error rather than fix it
                let mut quoter = Quoter {
                    source: self.source,
                    is_forward: &is_self,
                    edits: &mut self.edits,
                    skip_root: true,
                };
                walk_one_type_expr(base, &mut quoter);
            }
        }
        for stmt in &class.body {
            let value = match stmt {
                Stmt::Expr(node) => Some(node.value.as_ref()),
                Stmt::Assign(node) => Some(node.value.as_ref()),
                Stmt::AnnAssign(node) => node.value.as_deref(),
                _ => None,
            };
            if let Some(value) = value {
                let mut quoter = Quoter {
                    source: self.source,
                    is_forward: &is_self,
                    edits: &mut self.edits,
                    skip_root: false,
                };
                quoter.value_subscripts(value);
            }
        }
        self.block(&class.body, Scope::Class);
    }
}

/// quotes the forward references in one type expression
struct Quoter<'a> {
    source: &'a str,
    is_forward: &'a dyn Fn(&ExprName) -> bool,
    edits: &'a mut Vec<(TextRange, Vec<Fragment>)>,
    /// when walking a class base, the root expression must not be quoted even
    /// if it's a bare self-reference — that would mask the runtime error
    skip_root: bool,
}

impl TypeExprVisitor for Quoter<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if std::mem::take(&mut self.skip_root) {
            return match expr {
                Expr::Subscript(_) | Expr::BinOp(_) => Recurse::Descend,
                _ => Recurse::Stop,
            };
        }
        if !self.contains_forward_reference(expr) {
            return Recurse::Stop;
        }
        match expr {
            Expr::Tuple(_) => Recurse::Descend,
            // a generic whose base is not itself a forward reference: quote the
            // references inside its arguments at their own level
            Expr::Subscript(subscript) if !self.contains_forward_reference(&subscript.value) => {
                Recurse::Descend
            }
            // a union is quoted whole: quoting one arm alone would evaluate
            // `str | NoneType` at runtime. anything else that holds a reference
            // — a name, a generic rooted at one, an arrow type — is quoted whole
            // as the reference it is
            _ => {
                self.quote(expr.range());
                Recurse::Stop
            }
        }
    }
}

impl Quoter<'_> {
    fn contains_forward_reference(&self, expr: &Expr) -> bool {
        struct Finder<'a> {
            is_forward: &'a dyn Fn(&ExprName) -> bool,
            found: bool,
        }
        impl<'ast> Visitor<'ast> for Finder<'_> {
            fn visit_expr(&mut self, expr: &'ast Expr) {
                if self.found {
                    return;
                }
                if let Expr::Name(name) = expr
                    && (self.is_forward)(name)
                {
                    self.found = true;
                    return;
                }
                walk_expr(self, expr);
            }
        }
        let mut finder = Finder {
            is_forward: self.is_forward,
            found: false,
        };
        finder.visit_expr(expr);
        finder.found
    }

    /// `list[A]()` and similar — quote a self-reference inside a value-position
    /// subscript on the LHS of a call. doesn't descend into call arguments
    fn value_subscripts(&mut self, expr: &Expr) {
        match expr {
            Expr::Subscript(subscript) => {
                walk_one_type_expr(subscript.slice.as_ref(), self);
                self.value_subscripts(&subscript.value);
            }
            Expr::Call(call) => self.value_subscripts(&call.func),
            Expr::Attribute(attribute) => self.value_subscripts(&attribute.value),
            _ => {}
        }
    }

    /// wrap `range` in quotes, as one template passing the source through so
    /// the lowerings inside it land between the quotes. the delimiter is one the
    /// source it wraps does not already use
    fn quote(&mut self, range: TextRange) {
        let text = &self.source[range];
        let delimiter = ["\"", "'", "\"\"\"", "'''"]
            .into_iter()
            .find(|delimiter| {
                !text.contains(delimiter)
                    && delimiter
                        .chars()
                        .next()
                        .is_none_or(|quote| !text.ends_with(quote))
            })
            .unwrap_or("\"");
        self.edits.push((
            range,
            vec![
                Fragment::Lit(delimiter.to_owned()),
                Fragment::Src(range),
                Fragment::Lit(delimiter.to_owned()),
            ],
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::config::PythonVersion;
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// a callable type is basedpython syntax, so a self-reference inside one
    /// cannot be quoted where it is written. the quote wraps the arrow, and the
    /// callable lowering's `Callable[…]` lands inside it — without it the
    /// annotation evaluates `Tag` while the class body is still running
    #[test]
    fn a_callable_annotation_naming_its_class_is_quoted() {
        check(
            indoc! {"
                class Tag:
                    def a(self, x: (Tag) -> None): ...
                    def b(self, x: Tag.() -> None): ...
            "},
            indoc! {"
                from typing import Callable
                class Tag:
                    def a(self, x: \"Callable[[Tag], None]\"): ...
                    def b(self, x: \"Callable[[Tag], None]\"): ...
            "},
        );
    }

    /// the quote wraps the whole annotation, so a callable nested inside one
    /// carries its lowering with it
    #[test]
    fn a_nested_callable_self_reference_quotes_the_whole_annotation() {
        check(
            indoc! {"
                class Tag:
                    def a(self, x: list[(Tag) -> None]): ...
            "},
            indoc! {"
                from typing import Callable
                class Tag:
                    def a(self, x: \"list[Callable[[Tag], None]]\"): ...
            "},
        );
    }

    /// a callable that names no self-reference is left alone
    #[test]
    fn a_callable_without_a_self_reference_is_not_quoted() {
        check(
            indoc! {"
                class Tag:
                    def a(self, x: (int) -> None): ...
            "},
            indoc! {"
                from typing import Callable
                class Tag:
                    def a(self, x: Callable[[int], None]): ...
            "},
        );
    }

    #[test]
    fn simple_self_reference() {
        check("class A(list[A]): ...\n", "class A(list[\"A\"]): ...\n");
    }

    #[test]
    fn nested_self_reference() {
        check(
            "class Tree(Node[Tree]): ...\n",
            "class Tree(Node[\"Tree\"]): ...\n",
        );
    }

    #[test]
    fn self_ref_in_union() {
        check(
            "class A(list[A | None]): ...\n",
            "class A(list[\"A | None\"]): ...\n",
        );
    }

    #[test]
    fn self_ref_in_nested_subscript() {
        check(
            "class A(dict[str, list[A]]): ...\n",
            "class A(dict[str, list[\"A\"]]): ...\n",
        );
    }

    #[test]
    fn direct_base_not_quoted() {
        check("class A(A): ...\n", "class A(A): ...\n");
    }

    #[test]
    fn other_names_not_quoted() {
        check("class A(list[B]): ...\n", "class A(list[B]): ...\n");
    }

    #[test]
    fn multiple_occurrences() {
        check(
            "class A(Union[A, A]): ...\n",
            indoc! {"
                from typing import Union
                class A(Union[\"A\", \"A\"]): ...
            "},
        );
    }

    #[test]
    fn body_expr_stmt_call() {
        check(
            indoc! {"
                class A(list[A], dict[int]):
                    list[A]()
            "},
            indoc! {"
                class A(list[\"A\"], dict[int]):
                    list[\"A\"]()
            "},
        );
    }

    #[test]
    fn body_ann_assign() {
        check(
            indoc! {"
                class A(list[A]):
                    x: list[A] = list[A]()
            "},
            indoc! {"
                class A(list[\"A\"]):
                    x: \"list[A]\" = list[\"A\"]()
            "},
        );
    }

    #[test]
    fn body_method_annotations() {
        check(
            indoc! {"
                class A(list[A]):
                    def method(self, x: list[A]) -> list[A]: ...
            "},
            indoc! {"
                class A(list[\"A\"]):
                    def method(self, x: \"list[A]\") -> \"list[A]\": ...
            "},
        );
    }

    #[test]
    fn body_method_body_not_quoted() {
        check(
            indoc! {"
                class A(list[A]):
                    def method(self):
                        return list[A]()
            "},
            indoc! {"
                class A(list[\"A\"]):
                    def method(self):
                        return list[A]()
            "},
        );
    }

    #[test]
    fn nested_class_inner_quotes_own_name() {
        check(
            indoc! {"
                class Outer:
                    class Inner(list[Inner]): ...
            "},
            indoc! {"
                class Outer:
                    class Inner(list[\"Inner\"]): ...
            "},
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("class A(list[A]): ...\n");
    }

    #[test]
    fn body_field_with_generic_self_ref() {
        check(
            indoc! {"
                class Tree:
                    children: list[Tree[int]]
            "},
            indoc! {"
                class Tree:
                    children: \"list[Tree[int]]\"
            "},
        );
    }

    #[test]
    fn bare_self_ref_in_method_signature() {
        check(
            indoc! {"
                class A:
                    def f(self) -> A: ...
            "},
            indoc! {"
                class A:
                    def f(self) -> \"A\": ...
            "},
        );
    }

    /// a class defined further down is not bound when a signature above it runs,
    /// whether the signature is a module-level function's or a method's
    #[test]
    fn a_class_defined_later_is_quoted() {
        check(
            indoc! {"
                def later() -> Later:
                    return Later()


                class Plain:
                    def other(self, x: Later) -> Later: ...


                class Later: ...
            "},
            indoc! {"
                def later() -> \"Later\":
                    return Later()


                class Plain:
                    def other(self, x: \"Later\") -> \"Later\": ...


                class Later: ...
            "},
        );
    }

    /// a name bound by the time the annotation runs is left as it is
    #[test]
    fn a_class_defined_earlier_is_not_quoted() {
        check(
            indoc! {"
                class Earlier: ...


                def f(x: Earlier) -> list[Earlier]: ...


                y: Earlier = Earlier()
            "},
            indoc! {"
                class Earlier: ...


                def f(x: Earlier) -> list[Earlier]: ...


                y: Earlier = Earlier()
            "},
        );
    }

    /// a module-level variable annotation runs too
    #[test]
    fn a_module_level_variable_annotation_is_quoted() {
        check(
            indoc! {"
                x: Later


                class Later: ...
            "},
            indoc! {"
                x: \"Later\"


                class Later: ...
            "},
        );
    }

    /// a local variable's annotation is never evaluated
    #[test]
    fn a_local_variable_annotation_is_not_quoted() {
        check(
            indoc! {"
                def f() -> None:
                    x: Later = Later()


                class Later: ...
            "},
            indoc! {"
                def f() -> None:
                    x: Later = Later()


                class Later: ...
            "},
        );
    }

    /// an import made only under `if TYPE_CHECKING:` never runs, so the
    /// annotation has nothing to find
    #[test]
    fn a_type_checking_import_is_quoted() {
        check(
            indoc! {"
                from typing import TYPE_CHECKING

                if TYPE_CHECKING:
                    from collections import OrderedDict


                def f(x: OrderedDict[str, int]) -> None: ...
            "},
            indoc! {"
                from typing import TYPE_CHECKING

                if TYPE_CHECKING:
                    from collections import OrderedDict


                def f(x: \"OrderedDict[str, int]\") -> None: ...
            "},
        );
    }

    /// the lowerings inside a quoted annotation land between the quotes
    #[test]
    fn a_lowering_inside_the_reference_is_quoted_with_it() {
        check(
            indoc! {"
                def f(x: Later?) -> list[Later?]: ...


                class Later: ...
            "},
            indoc! {"
                def f(x: \"Later | None\") -> \"list[Later | None]\": ...


                class Later: ...
            "},
        );
    }

    /// a name basedpython supplies itself is the transpiler's to make available,
    /// not a reference to quote
    #[test]
    fn a_name_basedpython_supplies_is_not_quoted() {
        check(
            "def f(x: dynamic) -> None: ...\n",
            indoc! {"
                from typing import Any
                def f(x: Any) -> None: ...
            "},
        );
    }

    fn transpile_with(input: &str, config: &Config) -> String {
        transpile(input, config).unwrap()
    }

    #[test]
    fn not_quoted_when_version_defers_annotations() {
        // 3.14+ evaluates annotations lazily (PEP 649); nothing to quote
        let config = Config {
            min_version: PythonVersion::from((3, 14)),
            ..Config::test_default()
        };
        let out = transpile_with("class A:\n    def f(self) -> A: ...\n", &config);
        assert!(
            out.contains("-> A:"),
            "should leave the self-ref bare on 3.14+, got: {out}"
        );
    }

    #[test]
    fn class_base_still_quoted_when_version_defers_annotations() {
        // pep 649 defers annotations only — a class *base* evaluates eagerly,
        // so its self-reference still needs the quote
        let config = Config {
            min_version: PythonVersion::from((3, 14)),
            ..Config::test_default()
        };
        let out = transpile_with("class A(list[A]):\n    pass\n", &config);
        assert!(
            out.contains("class A(list[\"A\"]):"),
            "base self-ref must stay quoted on 3.14+, got: {out}"
        );
    }

    #[test]
    fn not_quoted_when_source_has_future() {
        // a user-written future import already defers every annotation
        let config = Config::test_default();
        let out = transpile_with(
            "from __future__ import annotations\nclass A:\n    def f(self) -> A: ...\n",
            &config,
        );
        assert!(
            out.contains("-> A:"),
            "should leave the self-ref bare when future is present, got: {out}"
        );
    }

    #[test]
    fn not_quoted_when_inject_future_opted_in() {
        // opting into the blanket future import defers annotations, so the
        // surgical quote is skipped and the import is prepended instead
        let config = Config {
            inject_future_annotations: true,
            ..Config::test_default()
        };
        let out = transpile_with("class A:\n    def f(self) -> A: ...\n", &config);
        assert!(
            out.starts_with("from __future__ import annotations\n"),
            "should inject the future import, got: {out}"
        );
        assert!(
            out.contains("-> A:"),
            "should leave the self-ref bare when future is injected, got: {out}"
        );
    }

    /// nothing in a stub is evaluated, and a checker reads a forward reference in
    /// one without quotes
    #[test]
    fn a_stub_quotes_nothing() {
        let source = "class A(list[A]):\n    def f(self, other: list[A]) -> A: ...\n";
        let config = Config {
            is_stub: true,
            ..Config::test_default()
        };
        assert_eq!(transpile(source, &config).unwrap(), source);
    }
}
