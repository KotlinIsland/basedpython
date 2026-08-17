//! Reverse of `crate::transforms::literal_types`:
//!   `Literal[1]`           → `1`
//!   `Literal[1, 2]`        → `1 | 2`
//!   `Literal[1, 2] | int`  → `1 | 2 | int`
//!   `list[Literal[1, 2]]`  → `list[1 | 2]`
//!
//! Only fires when every slice element is a true atomic literal (number /
//! string / bool / None / bytes, optionally with a unary +/- on a number).
//! Enum-member arguments like `Literal[Foo.BAR]` are left alone — the bare
//! `Foo.BAR` form isn't valid as a type expression outside `Literal[]`.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprSubscript, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct LiteralReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> LiteralReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// `Literal` or `typing.Literal` / `typing_extensions.Literal`, where the
    /// bare name resolves to an import or is unresolved
    fn is_literal_name(&self, value: &Expr) -> bool {
        trailing_name(value) == Some("Literal") && self.types.subscript_is_type_context(value)
    }

    fn rewrite_literal_subscript(&self, s: &ExprSubscript) -> Option<String> {
        if !self.is_literal_name(&s.value) {
            return None;
        }
        // Slice is either a single expression or an unparenthesized tuple of args.
        let elts: Vec<&Expr> = match s.slice.as_ref() {
            Expr::Tuple(t) if !t.parenthesized => t.elts.iter().collect(),
            other => vec![other],
        };
        if elts.is_empty() {
            return None;
        }
        if !elts.iter().all(|e| is_literal_value(e)) {
            return None;
        }
        let parts: Vec<&str> = elts.iter().map(|e| self.src(e.range())).collect();
        Some(parts.join(" | "))
    }
}

impl<'ast> Visitor<'ast> for LiteralReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(s) = expr
            && let Some(rewrite) = self.rewrite_literal_subscript(s)
        {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                rewrite,
                expr.range(),
            )));
            // Slice is composed of pure literals — nothing more to recurse into.
            return;
        }
        walk_expr(self, expr);
    }
}

fn is_literal_value(expr: &Expr) -> bool {
    match expr {
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::BytesLiteral(_) => true,
        Expr::UnaryOp(u) => {
            matches!(u.op, UnaryOp::USub | UnaryOp::UAdd)
                && matches!(u.operand.as_ref(), Expr::NumberLiteral(_))
        }
        _ => false,
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

    #[test]
    fn single_int() {
        check("a: Literal[1]\n", "a: 1\n");
    }

    #[test]
    fn int_union() {
        check("a: Literal[1, 2]\n", "a: 1 | 2\n");
    }

    #[test]
    fn three_int_union() {
        check("a: Literal[1, 2, 3]\n", "a: 1 | 2 | 3\n");
    }

    #[test]
    fn string_union() {
        check("a: Literal[\"foo\", \"bar\"]\n", "a: \"foo\" | \"bar\"\n");
    }

    #[test]
    fn mixed_string_int() {
        check("a: Literal[\"asdf\", 5]\n", "a: \"asdf\" | 5\n");
    }

    #[test]
    fn negative_int() {
        check("a: Literal[-1, -2]\n", "a: -1 | -2\n");
    }

    #[test]
    fn bool_union() {
        check("a: Literal[True, False]\n", "a: True | False\n");
    }

    #[test]
    fn none_in_union() {
        check("a: Literal[None, 1]\n", "a: None | 1\n");
    }

    #[test]
    fn left_of_pipe_with_other_type() {
        check("a: Literal[1, 2] | int\n", "a: 1 | 2 | int\n");
    }

    #[test]
    fn right_of_pipe_with_other_type() {
        check("a: int | Literal[1, 2]\n", "a: int | 1 | 2\n");
    }

    #[test]
    fn nested_inside_list() {
        check("a: list[Literal[1, 2]]\n", "a: list[1 | 2]\n");
    }

    #[test]
    fn nested_inside_dict() {
        check("a: dict[str, Literal[1, 2]]\n", "a: dict[str, 1 | 2]\n");
    }

    #[test]
    fn function_signature() {
        check(
            indoc! {"
                def f(x: Literal[1, 2]) -> Literal[3]:
                    pass
            "},
            indoc! {"
                def f(x: 1 | 2) -> 3:
                    pass
            "},
        );
    }

    #[test]
    fn enum_member_unchanged() {
        // Bare `Foo.BAR` isn't a valid type expression, so we leave it.
        check("a: Literal[Foo.BAR]\n", "a: Literal[Foo.BAR]\n");
    }

    #[test]
    fn shadowed_literal_unchanged() {
        // Local `Literal` shadows the typing import — don't touch it.
        check(
            indoc! {"
                Literal = object()
                a: Literal[1]
            "},
            indoc! {"
                Literal = object()
                a: Literal[1]
            "},
        );
    }

    #[test]
    fn typing_qualified_literal() {
        check(
            indoc! {"
                import typing
                a: typing.Literal[1, 2]
            "},
            indoc! {"
                import typing
                a: 1 | 2
            "},
        );
    }
}
