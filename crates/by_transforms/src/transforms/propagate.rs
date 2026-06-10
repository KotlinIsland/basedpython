//! Runtime lowering for the postfix `^` propagate operator.
//!
//! `expr^` unwraps the present value; on the absent value it early-returns that
//! absent value from the enclosing function. It lowers to a guard hoisted
//! directly before the enclosing statement, with the operand left in place as
//! the unwrapped value:
//!
//! ```python
//! # f()^.bar   (f() : int?)   becomes
//! _prop0 = f()
//! if _prop0 is None: return _prop0
//! _prop0.bar
//! ```
//!
//! A trivially-pure operand (a bare name) needs no temp:
//! `f^` becomes `if f is None: return f` with the unwrapped `f` left in place.
//!
//! Only the optional (`is None`) form is lowered today; the `Result` absent
//! case (`isinstance(_, BaseException)`) is still being settled.
//!
//! Limitation: the operand source is hoisted verbatim, so a `^` whose operand
//! itself contains another basedpython operator is not lowered through that
//! inner operator. operands are simple names/calls in practice.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::{AbsentTest, TypeInfo};

/// The guard condition that detects the "absent" value for a given operand
/// `target` (a temp name or the operand source). `T?` tests `is None`; a
/// result-like `T | E` tests `isinstance(_, BaseException)`.
fn absent_condition(test: AbsentTest, target: &str) -> String {
    match test {
        AbsentTest::Optional | AbsentTest::WrappedOptional => format!("{target} is None"),
        AbsentTest::Result => format!("isinstance({target}, BaseException)"),
    }
}

/// Expressions safe to read twice without changing semantics — re-emitting them
/// in the guard avoids a hoisted temp.
fn is_trivially_pure(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Name(_)
            | Expr::NumberLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
    )
}

/// The innermost binding scope around a `^`. Propagation early-returns from a
/// `def`, so it is only meaningful when the nearest scope is a function — a
/// lambda or comprehension body has no `return` to hoist into, and module scope
/// has no enclosing function at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Function,
    Lambda,
    Comprehension,
}

struct Propagate<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
    /// (start offset, indentation) of each enclosing statement; the top is the
    /// statement the guard is hoisted before
    stmt_stack: Vec<(TextSize, String)>,
    /// the binding scopes enclosing the current node; the top decides whether a
    /// `^` here can early-return
    scope_stack: Vec<Scope>,
    /// hard errors for `^` used where it cannot early-return
    errors: Vec<String>,
    counter: usize,
}

impl<'src> Propagate<'src> {
    fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
            stmt_stack: Vec::new(),
            scope_stack: Vec::new(),
            errors: Vec::new(),
            counter: 0,
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn indent_of(&self, start: TextSize) -> String {
        let prefix = &self.source[..usize::from(start)];
        let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.source[line_start..usize::from(start)].to_owned()
    }
}

impl<'ast> Visitor<'ast> for Propagate<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let start = stmt.range().start();
        let indent = self.indent_of(start);
        self.stmt_stack.push((start, indent));
        if matches!(stmt, Stmt::FunctionDef(_)) {
            self.scope_stack.push(Scope::Function);
            walk_stmt(self, stmt);
            self.scope_stack.pop();
        } else {
            walk_stmt(self, stmt);
        }
        self.stmt_stack.pop();
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // a lambda / comprehension body is a separate scope: a `^` inside it
        // cannot early-return the surrounding function, so descend with that
        // scope marked
        let scope = match expr {
            Expr::Lambda(_) => Some(Scope::Lambda),
            Expr::ListComp(_) | Expr::SetComp(_) | Expr::DictComp(_) | Expr::Generator(_) => {
                Some(Scope::Comprehension)
            }
            _ => None,
        };
        if let Some(scope) = scope {
            self.scope_stack.push(scope);
            walk_expr(self, expr);
            self.scope_stack.pop();
            return;
        }
        if let Expr::UnaryOp(unary) = expr
            && unary.op == UnaryOp::Propagate
        {
            match self.scope_stack.last() {
                Some(Scope::Function) => {}
                Some(Scope::Lambda) => {
                    self.errors.push(
                        "`^` (propagate) is not valid inside a lambda — it has no `return`"
                            .to_owned(),
                    );
                    return;
                }
                Some(Scope::Comprehension) => {
                    self.errors
                        .push("`^` (propagate) is not valid inside a comprehension".to_owned());
                    return;
                }
                None => {
                    self.errors
                        .push("`^` (propagate) is only valid inside a function".to_owned());
                    return;
                }
            }
            let Some((stmt_start, indent)) = self.stmt_stack.last().cloned() else {
                return;
            };
            // the absent arm is `None` for an optional (`T?`) operand and a
            // `BaseException` for a result-like union (`T | E`). when ty can't
            // resolve the operand type, fall back to the optional form
            let absent = self
                .types
                .propagate_absent_test(&unary.operand)
                .unwrap_or(AbsentTest::Optional);
            let operand_src = self.src(unary.operand.range()).to_owned();
            // a wrapped optional's present value is inside the runtime wrapper
            let unwrap = if absent == AbsentTest::WrappedOptional {
                ".value"
            } else {
                ""
            };
            let (guard, value) = if is_trivially_pure(&unary.operand) {
                let cond = absent_condition(absent, &operand_src);
                (
                    format!("if {cond}: return {operand_src}\n{indent}"),
                    format!("{operand_src}{unwrap}"),
                )
            } else {
                let temp = format!("_prop{}", self.counter);
                self.counter += 1;
                let cond = absent_condition(absent, &temp);
                (
                    format!("{temp} = {operand_src}\n{indent}if {cond}: return {temp}\n{indent}"),
                    format!("{temp}{unwrap}"),
                )
            };
            // hoist the guard immediately before the enclosing statement, then
            // replace `operand^` with the unwrapped value
            self.edits.push((TextRange::empty(stmt_start), guard));
            self.edits.push((unary.range(), value));
            // do not descend into the operand: its source was captured verbatim
            return;
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct PropagatePass<'src> {
    source: &'src str,
}

impl<'src> PropagatePass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for PropagatePass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = Propagate::new(self.source, types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if !inner.errors.is_empty() {
            ctx.errors.extend(inner.errors);
            return;
        }
        ctx.text_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    fn check_err(input: &str, needle: &str) {
        let err = transpile(input, &Config::test_default()).unwrap_err();
        assert!(err.contains(needle), "got: {err}");
    }

    #[test]
    fn wrapped_operand_unwraps_value() {
        // a wrapped optional's present value lives inside the runtime wrapper:
        // the guard returns the absent `None`, the use site reads `.value`
        check(
            indoc::indoc! {"
                def g() -> int??:
                    return Some(5)

                def f() -> int?:
                    x = g()^
                    return x
            "},
            indoc::indoc! {"
                class Optional:
                    def __init__(self, value):
                        self.value = value

                    def __class_getitem__(cls, item):
                        return cls

                    def __repr__(self):
                        return f\"Some({self.value!r})\"

                def g() -> Optional[int | None]:
                    return Optional(5)

                def f() -> int | None:
                    _prop0 = g()
                    if _prop0 is None: return _prop0
                    x = _prop0.value
                    return x
            "},
        );
    }

    #[test]
    fn propagate_outside_function_is_an_error() {
        // hoisting a `return` to module scope, into a lambda, or out of a
        // comprehension all produce invalid python — reject them instead
        check_err("x = f()^\n", "only valid inside a function");
        check_err("g = lambda: f()^\n", "inside a lambda");
        check_err(
            "def g() -> int?:\n    return [f()^ for _ in range(3)]\n",
            "inside a comprehension",
        );
    }

    #[test]
    fn propagate_pure_name() {
        check(
            indoc! {"
                def f() -> int?:
                    return None

                def g() -> int?:
                    x = f()
                    x^
            "},
            indoc! {"
                def f() -> int | None:
                    return None

                def g() -> int | None:
                    x = f()
                    if x is None: return x
                    x
            "},
        );
    }

    #[test]
    fn propagate_call_hoists_temp() {
        check(
            indoc! {"
                def f() -> int?:
                    return None

                def g() -> str:
                    f()^.__class__.__name__
            "},
            indoc! {"
                def f() -> int | None:
                    return None

                def g() -> str:
                    _prop0 = f()
                    if _prop0 is None: return _prop0
                    _prop0.__class__.__name__
            "},
        );
    }

    #[test]
    fn propagate_in_assignment() {
        check(
            indoc! {"
                def g(a) -> int?:
                    y = a()^
                    return y
            "},
            indoc! {"
                def g(a) -> int | None:
                    _prop0 = a()
                    if _prop0 is None: return _prop0
                    y = _prop0
                    return y
            "},
        );
    }

    /// a result-like union (`T | E`, the error arm a `BaseException` subtype)
    /// propagates the error via an `isinstance` guard, not `is None`
    #[test]
    fn propagate_result_call_uses_isinstance() {
        check(
            indoc! {"
                def f() -> int | TypeError: ...

                def m() -> int | TypeError:
                    x = f()^
                    return x
            "},
            indoc! {"
                def f() -> int | TypeError: ...

                def m() -> int | TypeError:
                    _prop0 = f()
                    if isinstance(_prop0, BaseException): return _prop0
                    x = _prop0
                    return x
            "},
        );
    }

    /// a trivially-pure result operand needs no temp; the guard tests the
    /// name directly
    #[test]
    fn propagate_result_pure_name() {
        check(
            indoc! {"
                def m(r: int | ValueError) -> int | ValueError:
                    r^
                    return r
            "},
            indoc! {"
                def m(r: int | ValueError) -> int | ValueError:
                    if isinstance(r, BaseException): return r
                    r
                    return r
            "},
        );
    }

    #[test]
    fn plain_python_unchanged() {
        unchanged(indoc! {"
            def g(a):
                return a
        "});
    }
}
