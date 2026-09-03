//! reverse of `crate::transforms::optional_type`:
//!   `T | None` → `T?`
//!
//! only fires in annotation positions. the rewrite is one edit over the
//! ` | None` tail — the operand text is never re-rendered, so another
//! transform's edit inside it survives and a multi-line union keeps its layout
//!
//! ## where the `?` may stand
//!
//! `?` binds looser than `|`, so it absorbs the whole union to its left: `A |
//! B?` is `(A | B)?`. that is the same type `A | B | None` named, but it does
//! not *read* like it — the marker looks as though it belongs to the last arm
//! alone. so only a union with a single arm besides the `None` is marked, and a
//! wider one is left spelled out
//!
//! the same looseness is why the rewrite is refused unless the union's extent
//! is already fenced by punctuation. inside `not (A | None)` the union is the
//! operand of a `not`, which binds *tighter* than `?`, so `not A?` would read
//! as `(not A)?` — a different type. only positions bounded by a bracket, a
//! comma or the start of the annotation are rewritten; everything else is left
//! alone
//!
//! ## when `T?` would not mean `T | None`
//!
//! `?` over a bare type variable is the *wrapped* optional
//! (`WrappedOptional(T | None)`), because specializing a plain `T | None` with
//! an optional `T` would flatten the layer. a stub's `Value | None` therefore
//! has to stay a union — see [`TypeInfo::optional_wraps`]

use ruff_python_ast::helpers::{type_modifier_marker, use_site_variance_marker};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Operator, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::TypeInfo;

pub(crate) struct OptionalTypeReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    /// return annotations of functions that carry a `raises` clause. `raises`
    /// is a plain name, so a `?` written directly before one reads as the
    /// result type `T ? E` with the exception set as its error arm. those
    /// annotations keep their `| None` at the top level
    guarded_returns: Vec<TextRange>,
    /// whether the expression being visited is fenced by punctuation, so a
    /// trailing `?` cannot reach past it
    delimited: bool,
    pub(crate) edits: Vec<(TextRange, String)>,
}

impl<'src> OptionalTypeReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            guarded_returns: Vec::new(),
            delimited: false,
            edits: Vec::new(),
        }
    }

    /// the ` | None` tail of `union`, rewritten as `?`.
    ///
    /// close parentheses in the tail belong to the operand's own grouping
    /// (`(A) | None`) and are kept, so the `?` still lands outside them
    fn rewrite_tail(&mut self, left: &Expr, union: &Expr) {
        let tail = TextRange::new(left.end(), union.end());
        let text = &self.source[usize::from(tail.start())..usize::from(tail.end())];
        // a comment between the operand and the `None` would be swallowed by
        // the replacement
        if text.contains('#') {
            return;
        }
        let mut replacement: String = text.chars().filter(|c| *c == ')').collect();
        replacement.push('?');
        self.edits.push((tail, replacement));
    }

    /// visit a type expression, recording whether a `?` written at its end
    /// would bind to exactly this expression
    fn visit_type_expr(&mut self, expr: &Expr, delimited: bool) {
        self.delimited = delimited;
        self.visit_expr(expr);
    }

    fn visit_annotation(&mut self, ann: &Expr) {
        let delimited = !self.guarded_returns.contains(&ann.range());
        self.visit_type_expr(ann, delimited);
    }
}

impl<'ast> Visitor<'ast> for OptionalTypeReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let mut guards = GuardedReturns {
            ranges: &mut self.guarded_returns,
        };
        guards.visit_stmt(stmt);
        crate::transforms::source_util::for_each_annotation_in_stmt(stmt, |ann| {
            self.visit_annotation(ann);
        });
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        let delimited = self.delimited;
        match expr {
            Expr::BinOp(b) if b.op == Operator::BitOr && b.right.is_none_literal_expr() => {
                // a union of two or more arms besides the `None` keeps its
                // spelling: `A | B?` means `(A | B)?`, which is right but reads
                // as though only `B` were optional
                let single_arm = !matches!(
                    b.left.as_ref(),
                    Expr::BinOp(inner) if inner.op == Operator::BitOr
                );
                if delimited && single_arm && !self.types.optional_wraps(&b.left) {
                    self.rewrite_tail(&b.left, expr);
                }
                // the union's left operand reaches the same boundary this one
                // does, so it inherits the verdict; the right operand is the
                // `None` that just went away
                self.visit_type_expr(&b.left, delimited);
            }
            Expr::BinOp(b) if matches!(b.op, Operator::BitOr | Operator::BitAnd) => {
                self.visit_type_expr(&b.left, delimited);
                // `|` and `&` are left-associative, so a union standing as the
                // right operand was parenthesized in the source
                self.visit_type_expr(&b.right, true);
            }
            // a use-site modifier (`literal T`, `out T`) is a subscript the
            // parser synthesized, with no brackets of its own — what fences the
            // type it wraps is whatever fences the modifier
            _ if let Some(inner) = marker_inner(expr) => self.visit_type_expr(inner, delimited),
            // a subscript's arguments are fenced by its brackets, a tuple's or
            // a list's elements by the commas between them — the bracketed list
            // being how a `Callable[[int], str]` spells its parameters
            Expr::Subscript(s) => self.visit_type_expr(&s.slice, true),
            Expr::Tuple(t) => {
                for elt in &t.elts {
                    self.visit_type_expr(elt, true);
                }
            }
            Expr::List(l) => {
                for elt in &l.elts {
                    self.visit_type_expr(elt, true);
                }
            }
            // a dict-literal type (`{"a": int}`) fences each member type with
            // its braces and commas
            Expr::Dict(d) => {
                for item in &d.items {
                    self.visit_type_expr(&item.value, true);
                }
            }
            // an arrow's parameters sit inside its parentheses. its return type
            // is parsed only as far as the first `|`, so a union standing there
            // was parenthesized too
            Expr::CallableType(c) => {
                for arg in &c.args {
                    self.visit_type_expr(arg, true);
                }
                self.visit_type_expr(&c.returns, true);
            }
            // `a: int` — a named member of an inline protocol or an anonymous
            // named tuple, whose type runs to the next `;` or `,`
            Expr::Named(n) => self.visit_type_expr(&n.value, true),
            _ => {
                self.delimited = false;
                walk_expr(self, expr);
            }
        }
    }
}

/// the type a parser-synthesized use-site marker stands in front of —
/// `literal T`, `final T`, `out T`, `in T` — or `None` for any other expression
fn marker_inner(expr: &Expr) -> Option<&Expr> {
    type_modifier_marker(expr)
        .map(|(_, inner)| inner)
        .or_else(|| use_site_variance_marker(expr).map(|(_, inner)| inner))
}

/// collects the return annotations of every function carrying a `raises`
/// clause, including nested ones
struct GuardedReturns<'a> {
    ranges: &'a mut Vec<TextRange>,
}

impl<'ast> Visitor<'ast> for GuardedReturns<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt
            && f.raises.is_some()
            && let Some(returns) = &f.returns
        {
            self.ranges.push(returns.range());
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    /// the source is left exactly as it stands
    fn unchanged(source: &str) {
        assert_eq!(
            reverse_transpile(source, &Config::test_default()).unwrap(),
            source
        );
    }

    /// the reversed source lowers back to the python it came from
    fn round_trips(python: &str) {
        let by = reverse_transpile(python, &Config::test_default()).unwrap();
        assert_eq!(transpile(&by, &Config::test_default()).unwrap(), python);
    }

    #[test]
    fn variable_annotation() {
        check("x: int | None\n", "x: int?\n");
        round_trips("x: int | None\n");
    }

    #[test]
    fn parameter_and_return_annotations() {
        check(
            "def f(x: str | None = None) -> bytes | None: ...\n",
            "def f(x: str? = None) -> bytes?\n",
        );
    }

    /// `str | int?` would be that same type — `?` absorbs the union to its
    /// left — but it reads as though only `int` were optional, so a union with
    /// more than one arm besides the `None` keeps its spelling
    #[test]
    fn a_union_of_several_arms_is_left_alone() {
        unchanged("x: str | int | None\n");
    }

    /// a `None` in the middle of a union marks only the arm before it, which is
    /// where the union already put it: `A? | B` is `A | None | B`
    #[test]
    fn a_none_between_two_arms_marks_the_arm_before_it() {
        check("x: str | None | int\n", "x: str? | int\n");
        round_trips("x: str | None | int\n");
    }

    #[test]
    fn nested_in_a_subscript() {
        check(
            "x: dict[str, list[int | None] | None]\n",
            "x: dict[str, list[int?]?]\n",
        );
        round_trips("x: dict[str, list[int | None] | None]\n");
    }

    /// an optional inside a callable's bracketed parameter list is fenced by
    /// those brackets, whether it is read before or after the arrow rewrite
    #[test]
    fn inside_a_callable_parameter_list() {
        check(
            indoc! {"
                from typing import Callable
                def f(cb: Callable[[int | None], str]) -> None: ...
            "},
            indoc! {"
                from typing import Callable
                def f(cb: (int?) -> str) -> None
            "},
        );
    }

    /// a leading `None` has no arm before it to mark
    #[test]
    fn a_leading_none_arm_is_left_alone() {
        unchanged("x: None | int | str\n");
    }

    /// `?` over a bare type variable is the wrapped optional, a different type
    /// from the union the stub wrote
    #[test]
    fn type_variable_arm_stays_a_union() {
        check(
            indoc! {"
                class Box[T]:
                    def get(self) -> T | None: ...
            "},
            indoc! {"
                class Box[T]:
                    def get(self) -> T | None
            "},
        );
    }

    /// the same holds for a legacy `TypeVar`, which is the form a freshly
    /// reverse-transpiled stub arrives in
    #[test]
    fn legacy_type_variable_arm_stays_a_union() {
        check(
            indoc! {r#"
                from typing import Generic, TypeVar
                _T = TypeVar("_T")

                class Box(Generic[_T]):
                    def get(self) -> _T | None: ...
            "#},
            indoc! {r#"
                from typing import Generic, TypeVar
                _T = TypeVar("_T")

                class Box(Generic[_T]):
                    def get(self) -> _T | None
            "#},
        );
    }

    /// `Self` is the one type variable that can never bind to an optional, so
    /// `Self?` is the plain union and the rewrite applies
    #[test]
    fn a_self_arm_converts() {
        check(
            indoc! {"
                from typing import Self

                class Box:
                    def peek(self) -> Self | None: ...
            "},
            indoc! {"
                from typing import Self

                class Box:
                    def peek(self) -> Self?
            "},
        );
    }

    /// a specialization of a type variable is an ordinary type, not a bare
    /// variable, so it takes the marker
    #[test]
    fn a_specialization_of_a_type_variable_converts() {
        check(
            indoc! {"
                class Box[T]:
                    def get(self) -> list[T] | None: ...
            "},
            indoc! {"
                class Box[T]:
                    def get(self) -> list[T]?
            "},
        );
    }

    /// `not` binds tighter than `?`, so the union it negates cannot take the
    /// marker without changing what the annotation means
    #[test]
    fn a_negated_union_is_left_alone() {
        unchanged("class A\nx: not (A | None)\n");
    }

    /// the union is only rewritten where it is fenced, and parentheses fence it
    #[test]
    fn a_parenthesized_operand_keeps_its_parentheses() {
        check("x: (int | None) & str\n", "x: (int?) & str\n");
    }

    /// a use-site type modifier binds to the operand it precedes, so the
    /// optional is over the modified type either way: `literal str?` is
    /// `(literal str)?`, which is what `literal str | None` said
    #[test]
    fn a_modified_operand_converts() {
        check(
            indoc! {"
                from typing import LiteralString
                x: LiteralString | None
            "},
            indoc! {"
                from typing import LiteralString
                x: literal str?
            "},
        );
    }

    /// `&` binds tighter than `|`, so an intersection is one arm and takes the
    /// marker: `A & B?` is `(A & B)?`
    #[test]
    fn an_intersection_arm_converts() {
        check(
            indoc! {"
                class A
                class B
                x: A & B | None
            "},
            indoc! {"
                class A
                class B
                x: A & B?
            "},
        );
    }

    /// a value-position union is not an annotation and is never touched
    #[test]
    fn a_runtime_union_is_left_alone() {
        check(
            "def f(v: object):\n    return isinstance(v, int | None)\n",
            "def f(v: object):\n    return v is int | None\n",
        );
    }

    /// `raises` is a plain name, so `-> int? raises E` would read as the result
    /// type `int ? raises` — the annotation keeps its union
    #[test]
    fn a_return_before_a_raises_clause_stays_a_union() {
        unchanged("def f() -> int | None raises ValueError:\n    return None\n");
    }

    /// only the top level of such a return is at risk — an optional fenced by
    /// brackets inside it still converts
    #[test]
    fn a_fenced_optional_converts_before_a_raises_clause() {
        check(
            "def f() -> list[int | None] raises ValueError:\n    return []\n",
            "def f() -> list[int?] raises ValueError:\n    return []\n",
        );
    }

    #[test]
    fn multi_line_union_keeps_its_layout() {
        check(
            indoc! {"
                x: (
                    int
                    | None
                )
            "},
            indoc! {"
                x: (
                    int?
                )
            "},
        );
    }

    /// a comment between the operand and the `None` would be lost, so the
    /// union is left as it stands
    #[test]
    fn a_commented_union_is_left_alone() {
        unchanged("x: (\n    int  # the value\n    | None\n)\n");
    }
}
