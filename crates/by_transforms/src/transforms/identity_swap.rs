//! `===` / `!==` are real python identity; `is` / `is not` mean
//! `isinstance` / `not isinstance`. parser flattens both spellings to
//! `CmpOp::Is`/`IsNot`, so disambiguation reads the operator text from
//! source.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    Arguments, AtomicNodeIndex, CmpOp, Expr, ExprCall, ExprContext, ExprName, ExprUnaryOp,
    ModModule, Stmt, UnaryOp, name::Name,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext, render_expr};

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
        let mut lhs: &Expr = c.left.as_ref();
        for (op, rhs) in c.ops.iter().zip(c.comparators.iter()) {
            let rhs_start = rhs.range().start();
            let between = &self.source[usize::from(lhs_end)..usize::from(rhs_start)];
            let trimmed = between.trim();
            match op {
                CmpOp::Is => {
                    if trimmed.starts_with("===") {
                        if let Some(pos) = between.find("===") {
                            let op_start = lhs_end + TextSize::try_from(pos).unwrap();
                            let op_range =
                                TextRange::new(op_start, op_start + TextSize::from(3u32));
                            self.edits.push((op_range, "is".to_owned()));
                        }
                    } else if trimmed == "is"
                        && !rhs.is_literal_expr()
                        // a subscripted rhs may be a parametric type test
                        // (`x is list[int]`); the type-aware parametric_is
                        // pass lowers those (and falls back to isinstance
                        // for value subscripts)
                        && !matches!(rhs, Expr::Subscript(_))
                    {
                        let call = isinstance_call(lhs.clone(), rhs.clone(), false);
                        let pair_range = TextRange::new(lhs.range().start(), rhs.range().end());
                        self.edits.push((pair_range, render_expr(&call)));
                    }
                }
                CmpOp::IsNot => {
                    if trimmed.starts_with("!==") {
                        if let Some(pos) = between.find("!==") {
                            let op_start = lhs_end + TextSize::try_from(pos).unwrap();
                            let op_range =
                                TextRange::new(op_start, op_start + TextSize::from(3u32));
                            self.edits.push((op_range, "is not".to_owned()));
                        }
                    } else if !rhs.is_literal_expr() && !matches!(rhs, Expr::Subscript(_)) {
                        let call = isinstance_call(lhs.clone(), rhs.clone(), true);
                        let pair_range = TextRange::new(lhs.range().start(), rhs.range().end());
                        self.edits.push((pair_range, render_expr(&call)));
                    }
                }
                _ => {}
            }
            lhs_end = rhs.range().end();
            lhs = rhs;
        }
    }
}

fn isinstance_call(lhs: Expr, rhs: Expr, negate: bool) -> Expr {
    let call = Expr::Call(ExprCall {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        func: Box::new(Expr::Name(ExprName {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            id: Name::from("isinstance"),
            ctx: ExprContext::Load,
        })),
        arguments: Arguments {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            args: Box::new([lhs, rhs]),
            keywords: thin_vec::ThinVec::new(),
        },
        is_cast: false,
        is_checked_cast: false,
        is_string_tag: false,
    });
    if negate {
        Expr::UnaryOp(ExprUnaryOp {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            op: UnaryOp::Not,
            operand: Box::new(call),
        })
    } else {
        call
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Compare(c) = expr {
            self.process_compare(c);
        }
        walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
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
    fn is_bool_kept() {
        check("a is True\n", "a is True\n");
        check("a is False\n", "a is False\n");
    }

    #[test]
    fn is_number_kept() {
        check("a is 0\n", "a is 0\n");
    }

    #[test]
    fn is_string_kept() {
        check("a is \"x\"\n", "a is \"x\"\n");
    }

    #[test]
    fn is_ellipsis_kept() {
        check("a is ...\n", "a is ...\n");
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
    fn python_eq_unchanged() {
        unchanged("a == b\n");
    }
}
