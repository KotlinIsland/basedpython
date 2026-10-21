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
//! annotation quoting is skipped when annotations are not evaluated eagerly
//! ([`crate::annotations_evaluated`]): on python >= 3.14 they are deferred
//! natively (PEP 649), and a `from __future__ import annotations` — written by
//! the user, opted into, or written for a target below 3.10 — defers every one,
//! where a quote would reach `__annotations__` as a string inside a string
//!
//! the checker reads two more kinds of type expression as deferred, though python
//! evaluates them where they stand on every version: the type arguments of a
//! specialization written as a value (`list[F]()`, `Alias = dict[str, F]`, a base
//! `Foo[F]`), and the type arguments of a typing construct assigned to a name
//! (`T = TypeVar("T", bound=F)`). which ones it read that way is its answer
//! ([`TypeInfo::is_read_deferred`]), and a forward reference in one is quoted too:
//!
//! `a = list[F](); class F: ...` → `a = list["F"]()`
//!
//! a position the checker reads where it is written — a base itself
//! (`class A(A)`), a reified class's type arguments — reports a name not yet bound
//! there, and is left alone
//!
//! an annotation holding a forward reference is quoted whole, as one wrapper
//! template passing its source through. no lowering reaches past the annotation
//! it lowers, so whatever the annotation becomes — a callable arrow, a `T?`, a
//! renamed type parameter, even when a lowering rewrites the whole annotation
//! as text — ends up between the quotes. a specialization cannot be a string, so
//! there only the type arguments holding a reference are quoted

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{AnyParameterRef, Expr, ExprName, Stmt, TypeParams};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::quotes::WrittenQuotes;
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_one_type_expr_with};
use crate::type_info::TypeInfo;

pub(crate) struct AutoQuote<'src> {
    pub(crate) quotes: &'src WrittenQuotes,
}

impl TypeAwarePass for AutoQuote<'_> {
    // nothing in a stub is evaluated, and a checker reads a forward reference
    // in one without quotes
    fn runtime_only(&self) -> bool {
        true
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut walk = Walk {
            quotes: self.quotes,
            types,
            quote_annotations: ctx.annotations_evaluated,
            edits: Vec::new(),
        };
        walk.block(stmts, Scope::Module);
        let mut deferred = DeferredReads {
            quotes: self.quotes,
            types,
            edits: walk.edits,
        };
        for stmt in stmts {
            deferred.visit_stmt(stmt);
        }
        ctx.template_edits.extend(deferred.edits);
    }
}

/// the kind of scope a block of statements runs in
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Module,
    Class,
    Function,
}

struct Walk<'a> {
    quotes: &'a WrittenQuotes,
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
            Stmt::ClassDef(class) => self.block(&class.body, Scope::Class),
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
            quotes: self.quotes,
            is_forward: &is_forward,
            edits: &mut self.edits,
            skip_root: false,
        };
        if quoter.contains_forward_reference(annotation) {
            quoter.quote(annotation.range());
        }
    }
}

/// quotes the forward references in what the checker reads as deferred though python
/// evaluates it where it stands: the type arguments of a specialization written as a value
/// (`list[F]()`, `Alias = dict[str, F]`, a base `Foo[F]`), and the type-expression arguments of
/// a typing construct assigned to a name (`T = TypeVar("T", bound=F)`). which those are is the
/// checker's answer ([`TypeInfo::is_read_deferred`]), so a position it reads where it is
/// written — a base itself, a reified class's type arguments, the type an `is` tests for — is
/// left alone, and a name in it that is not yet bound stays the error the checker reports
struct DeferredReads<'a> {
    quotes: &'a WrittenQuotes,
    types: &'a dyn TypeInfo,
    /// every quote written so far, the annotations' included: nothing inside one is evaluated
    /// where it stands any more
    edits: Vec<(TextRange, Vec<Fragment>)>,
}

impl DeferredReads<'_> {
    fn quoted_already(&self, range: TextRange) -> bool {
        self.edits
            .iter()
            .any(|(quoted, _)| quoted.contains_range(range))
    }

    /// quotes the type expression `expr` whole if it holds a forward reference. with
    /// `skip_root`, `expr` is the specialization, which is evaluated where it stands, and each
    /// of its type arguments is quoted that way instead
    fn quote_forward_references(&mut self, expr: &Expr, skip_root: bool) {
        let types = self.types;
        let is_forward = |name: &ExprName| types.is_forward_reference(name) == Some(true);
        let mut quoter = Quoter {
            quotes: self.quotes,
            is_forward: &is_forward,
            edits: &mut self.edits,
            skip_root,
        };
        walk_one_type_expr_with(expr, Some(self.types), &mut quoter);
    }
}

impl<'a> Visitor<'a> for DeferredReads<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            // a `type` statement's value is evaluated lazily, and the type parameters are the
            // generics polyfill's to write
            Stmt::TypeAlias(_) => {}
            // a `TypedDict`'s `extra_items=` is a type expression python evaluates as the class
            // is built
            Stmt::ClassDef(class) => {
                for keyword in class.keywords() {
                    if self.types.is_read_deferred(&keyword.value) {
                        self.quote_forward_references(&keyword.value, false);
                    }
                }
                walk_stmt(self, stmt);
            }
            _ => walk_stmt(self, stmt),
        }
    }

    // an annotation is quoted whole where python evaluates it, and is never evaluated
    // elsewhere
    fn visit_annotation(&mut self, _expr: &'a Expr) {}

    fn visit_type_params(&mut self, _type_params: &'a TypeParams) {}

    fn visit_expr(&mut self, expr: &'a Expr) {
        if self.quoted_already(expr.range()) {
            return;
        }
        match expr {
            Expr::Subscript(subscript) if self.types.is_read_deferred(&subscript.slice) => {
                self.quote_forward_references(expr, true);
            }
            Expr::Call(call) => {
                for argument in self.types.call_type_expression_arguments(call) {
                    if self.types.is_read_deferred(argument) {
                        self.quote_forward_references(argument, false);
                    }
                }
            }
            _ => {}
        }
        // what a quote left out may hold more: the metadata of an `Annotated`, a call's
        // other arguments
        walk_expr(self, expr);
    }
}

/// quotes the forward references in one type expression
struct Quoter<'a> {
    quotes: &'a WrittenQuotes,
    is_forward: &'a dyn Fn(&ExprName) -> bool,
    edits: &'a mut Vec<(TextRange, Vec<Fragment>)>,
    /// the root is the specialization whose type arguments are deferred, which is
    /// evaluated where it stands itself
    skip_root: bool,
}

impl TypeExprVisitor for Quoter<'_> {
    /// a type expression holding a forward reference is quoted whole, so whatever it lowers
    /// to — a callable arrow, a `T?`, a tuple type, a literal type — ends up between the
    /// quotes. quoting one name inside it would leave the construct around it evaluated
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if std::mem::take(&mut self.skip_root) {
            return Recurse::Descend;
        }
        if self.contains_forward_reference(expr) {
            self.quote(expr.range());
        }
        Recurse::Stop
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

    fn quote(&mut self, range: TextRange) {
        self.edits
            .push((range, vec![self.quotes.quote_span(range)]));
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
            indoc! {"
                class Node[T]: ...
                class Tree(Node[Tree]): ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Node(Generic[_T]): ...
                class Tree(Node[\"Tree\"]): ...
            "},
        );
    }

    /// what the checker cannot resolve it cannot read as a specialization, so a
    /// name in its subscript is left as it is
    #[test]
    fn an_unresolved_generic_quotes_nothing() {
        check(
            "class Tree(Node[Tree]): ...\n",
            "class Tree(Node[Tree]): ...\n",
        );
    }

    /// a specialization written as a value reads its type arguments when it runs,
    /// so a class defined further down is quoted wherever one stands
    #[test]
    fn a_later_class_in_a_value_specialization_is_quoted() {
        check(
            indoc! {"
                from typing import Callable, Union
                items = list[F]()
                Alias = dict[str, F]
                Maybe = Union[F, None]
                Handler = Callable[[F], F]
                nested = dict[str, list[F | None]]()
                class F: ...
            "},
            indoc! {"
                from typing import Callable, Union
                items = list[\"F\"]()
                Alias = dict[str, \"F\"]
                Maybe = Union[\"F\", None]
                Handler = Callable[[\"F\"], \"F\"]
                nested = dict[str, \"list[F | None]\"]()
                class F: ...
            "},
        );
    }

    /// a base's type arguments are a specialization like any other
    #[test]
    fn a_later_class_in_a_base_type_argument_is_quoted() {
        check(
            indoc! {"
                class Base[T]: ...
                class Bar(Base[F]): ...
                class F: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Base(Generic[_T]): ...
                class Bar(Base[\"F\"]): ...
                class F: ...
            "},
        );
    }

    /// the types a typing construct assigned to a name takes as call arguments
    #[test]
    fn a_later_class_in_a_typing_construct_is_quoted() {
        check(
            indoc! {"
                from typing import NamedTuple, NewType, TypedDict, TypeVar
                T = TypeVar(\"T\", bound=list[F])
                Id = NewType(\"Id\", F)
                Pair = NamedTuple(\"Pair\", [(\"first\", F)])
                Movie = TypedDict(\"Movie\", {\"lead\": F})
                class F: ...
            "},
            indoc! {"
                from typing import NamedTuple, NewType, TypedDict, TypeVar
                T = TypeVar(\"T\", bound=\"list[F]\")
                Id = NewType(\"Id\", \"F\")
                Pair = NamedTuple(\"Pair\", [(\"first\", \"F\")])
                Movie = TypedDict(\"Movie\", {\"lead\": \"F\"})
                class F: ...
            "},
        );
    }

    /// a type argument holding a reference is quoted whole, so the tuple type it
    /// lowers to is inside the quote
    #[test]
    fn a_tuple_type_argument_is_quoted_whole() {
        check(
            indoc! {"
                from typing import TypeVar
                Pairs = list[(F, int)]
                T = TypeVar(\"T\", bound=(F, int))
                class F: ...
            "},
            indoc! {"
                from typing import TypeVar
                Pairs = list[\"tuple[F, int]\"]
                T = TypeVar(\"T\", bound=\"tuple[F, int]\")
                class F: ...
            "},
        );
    }

    /// a `Callable` parameter list written as a tuple becomes python's list, and
    /// each parameter holding a reference is quoted on its own, as a parameter of
    /// a list is
    #[test]
    fn a_callable_tuple_parameter_list_quotes_each_parameter() {
        check(
            indoc! {"
                from typing import Callable
                Both = Callable[(F, int), F]
                Returns = Callable[(int, str), F]
                Nested = list[Callable[(F, int), bool]]
                class F: ...
            "},
            indoc! {"
                from typing import Callable
                Both = Callable[[\"F\", int], \"F\"]
                Returns = Callable[[int, str], \"F\"]
                Nested = list[\"Callable[[F, int], bool]\"]
                class F: ...
            "},
        );
    }

    /// basedpython type syntax around a reference lowers inside the quote
    #[test]
    fn a_basedpython_type_argument_is_quoted_with_its_lowering() {
        check(
            indoc! {"
                Maybe = list[F?]
                Handler = list[(F) -> None]
                Method = list[int.() -> F]
                class F: ...
            "},
            indoc! {"
                from typing import Callable
                Maybe = list[\"F | None\"]
                Handler = list[\"Callable[[F], None]\"]
                Method = list[\"Callable[[int], F]\"]
                class F: ...
            "},
        );
    }

    /// a `TypedDict`'s `extra_items=` is a type python evaluates as the class is
    /// built, and a keyword like `metaclass=` is an object it is built with
    #[test]
    fn extra_items_is_quoted_and_a_metaclass_is_not() {
        check(
            indoc! {"
                from typing_extensions import TypedDict
                class Movie(TypedDict, extra_items=F): ...
                class Meta(type): ...
                class Plain(metaclass=Meta): ...
                class F: ...
            "},
            indoc! {"
                from typing_extensions import TypedDict
                class Movie(TypedDict, extra_items=\"F\"): ...
                class Meta(type): ...
                class Plain(metaclass=Meta): ...
                class F: ...
            "},
        );
    }

    /// a string in a type argument is a literal type, and lands in the quote
    /// with the reference beside it
    #[test]
    fn a_literal_type_beside_a_quoted_reference_is_lowered_inside_the_quote() {
        check(
            indoc! {"
                items = list[\"a\" | F]()
                class F: ...
            "},
            indoc! {"
                from typing import Literal
                items = list['Literal[\"a\"] | F']()
                class F: ...
            "},
        );
    }

    /// the specialization itself is evaluated where it stands, so a generic bound
    /// only on some paths, which counts as a forward reference, is left as it is
    #[test]
    fn a_possibly_unbound_generic_is_not_quoted() {
        check(
            indoc! {"
                import sys
                if len(sys.argv) > 0:
                    Box = list
                items = Box[F]()
                class F: ...
            "},
            indoc! {"
                import sys
                if len(sys.argv) > 0:
                    Box = list
                items = Box[\"F\"]()
                class F: ...
            "},
        );
    }

    /// a specialization inside a reference already quoted is quoted with it,
    /// and not a second time inside the quote
    #[test]
    fn a_specialization_inside_a_quote_is_not_quoted_again() {
        check(
            indoc! {"
                from typing import Annotated
                items = list[F | Annotated[int, dict[str, G]()]]()
                class F: ...
                class G: ...
            "},
            indoc! {"
                from typing import Annotated
                items = list[\"F | Annotated[int, dict[str, G]()]\"]()
                class F: ...
                class G: ...
            "},
        );
    }

    /// a type argument is quoted whole, as an annotation is, so the metadata of
    /// an `Annotated` in it is read with it
    #[test]
    fn an_annotated_type_argument_is_quoted_whole() {
        check(
            indoc! {"
                from typing import Annotated
                items = list[Annotated[F, len]]()
                class F: ...
            "},
            indoc! {"
                from typing import Annotated
                items = list[\"Annotated[F, len]\"]()
                class F: ...
            "},
        );
    }

    /// a name a function binds further down is a forward reference within it,
    /// and one the module binds is bound by the time the function runs
    #[test]
    fn a_function_body_quotes_only_its_own_later_names() {
        check(
            indoc! {"
                def f():
                    x = list[G]()
                    y = list[F]()
                    G = int
                    return x, y
                class F: ...
            "},
            indoc! {"
                def f():
                    x = list[\"G\"]()
                    y = list[F]()
                    G = int
                    return x, y
                class F: ...
            "},
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
            "class A(dict[str, \"list[A]\"]): ...\n",
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

    /// below 3.10 the output defers every annotation with the future import, and a
    /// quote there would reach `__annotations__` as a string inside a string. a class
    /// base is still evaluated as the class is built, so its self-reference is quoted
    #[test]
    fn not_quoted_where_the_future_import_is_written() {
        let config = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        let out = transpile_with(
            "class A(list[A]):\n    item: A\n    def f(self, other: A) -> A: ...\n",
            &config,
        );
        assert!(
            out.starts_with("from __future__ import annotations\n"),
            "should write the future import, got: {out}"
        );
        assert!(
            out.contains("item: A\n") && out.contains("def f(self, other: A) -> A:"),
            "should leave annotations bare under the written future import, got: {out}"
        );
        assert!(
            out.contains("class A(list[\"A\"]):"),
            "a base self-ref must stay quoted, got: {out}"
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
