//! `===` / `!==` are real python identity, which python spells `is` / `is not`.
//! basedpython gives the `is` keyword to the type test instead, and the parser
//! folds both spellings onto the same [`CmpOp`], recording which one it saw.
//!
//! This pass lowers only the identity spelling, by replacing the operator text.
//! Every type test belongs to [`parametric_is`](super::parametric_is), which
//! decides its lowering from the target's *type* rather than from the shape the
//! target was written in.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{CmpOp, Expr, ModModule, Stmt};
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct IdentitySwap<'src> {
    source: &'src str,
}

impl<'src> IdentitySwap<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for IdentitySwap<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            edits: Vec::new(),
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits);
    }
}

struct State<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
}

impl State<'_> {
    fn process_compare(&mut self, c: &ruff_python_ast::ExprCompare) {
        let mut lhs_end = c.left.range().end();
        for (index, rhs) in c.comparators.iter().enumerate() {
            let gap = TextRange::new(lhs_end, rhs.range().start());
            if c.is_identity_operator(index) {
                let replacement = match c.ops.get(index) {
                    Some(CmpOp::Is) => "is",
                    Some(CmpOp::IsNot) => "is not",
                    _ => unreachable!("only `is` / `is not` carry the identity spelling"),
                };
                if let Some(range) = identity_operator_range(self.source, gap) {
                    // `===` needs no space around it and `is` does, so an
                    // operator the source wrote tight against its operands
                    // (`a===b`) has to bring its own
                    let before = self.source[..usize::from(range.start())]
                        .ends_with(|c: char| c.is_whitespace());
                    let after = self.source[usize::from(range.end())..]
                        .starts_with(|c: char| c.is_whitespace());
                    let padded = format!(
                        "{}{replacement}{}",
                        if before { "" } else { " " },
                        if after { "" } else { " " },
                    );
                    self.edits.push((range, padded));
                }
            }
            lhs_end = rhs.range().end();
        }
    }
}

/// the source range of the `===` / `!==` written in `gap` — the span between
/// the two operands it joins.
///
/// The operand ranges stop inside any parentheses wrapping them, and a comment
/// may sit in the gap of a bracketed expression, so the operator is found by
/// tokenizing rather than by searching for its text.
fn identity_operator_range(source: &str, gap: TextRange) -> Option<TextRange> {
    let start = SimpleTokenizer::new(source, gap)
        .skip_trivia()
        .find(|token| token.kind() != SimpleTokenKind::RParen)?
        .start();
    let rest = &source[usize::from(start)..];
    ["===", "!=="]
        .into_iter()
        .find(|symbol| rest.starts_with(symbol))
        .map(|_| TextRange::at(start, TextSize::from(3u32)))
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Compare(compare) = expr {
            self.process_compare(compare);
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn triple_eq_to_is() {
        check("a === b\n", "a is b\n");
    }

    #[test]
    fn bang_eq_eq_to_is_not() {
        check("a !== b\n", "a is not b\n");
    }

    #[test]
    fn is_to_isinstance() {
        check("x is int\n", "isinstance(x, int)\n");
    }

    #[test]
    fn parenthesized_operands_still_lower() {
        // an operand's own range stops inside its parentheses, so the rewrite
        // has to close what it swallowed and open what the source closes after
        check("(x) is int\n", "(isinstance(x, int))\n");
        check("x is ( int )\n", "(isinstance(x, int) )\n");
    }

    #[test]
    fn is_not_to_not_isinstance() {
        check("x is not int\n", "not isinstance(x, int)\n");
    }

    #[test]
    fn python_is_unchanged() {
        unchanged("a is None\n");
    }

    #[test]
    fn is_none_kept() {
        check("a is None\n", "a is None\n");
    }

    #[test]
    fn is_not_none_kept() {
        check("a is not None\n", "a is not None\n");
    }

    #[test]
    fn is_literal_tests_the_literal_type() {
        // a literal names a type holding exactly the values equal to it, and the
        // class guard is what keeps python's `1 == True` from widening that
        check(
            "def f(a: object):\n    return a is True\n",
            "def f(a: object):\n    return (type(a) is bool and a == True)\n",
        );
        check(
            "def f(a: object):\n    return a is 0\n",
            "def f(a: object):\n    return (type(a) is int and a == 0)\n",
        );
        check(
            "def f(a: object):\n    return a is \"x\"\n",
            "def f(a: object):\n    return (type(a) is str and a == \"x\")\n",
        );
    }

    #[test]
    fn triple_eq_none_still_swaps() {
        check("a === None\n", "a is None\n");
    }

    #[test]
    fn bang_eq_eq_none_still_swaps() {
        check("a !== None\n", "a is not None\n");
    }

    #[test]
    fn an_unspaced_identity_operator_brings_its_own_spaces() {
        // `===` needs no space around it; `is` does
        check("c = a===b\n", "c = a is b\n");
        check("d = a!==b\n", "d = a is not b\n");
    }

    #[test]
    fn python_eq_unchanged() {
        unchanged("a == b\n");
    }
}
