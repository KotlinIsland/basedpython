//! Text-edit lowering for basedpython tuple-member dot access: `expr.N` → `expr[N]`.
//!
//! The parser accepts `expr.N` where `N` is one or more decimal digits and
//! constructs an `ExprAttribute` whose `attr` is the digit string. That
//! shape is unreachable in stock python (identifiers cannot start with a
//! digit), so the attr-is-digits check is unambiguous.
//!
//! The rewrite replaces only the `.N` bytes with `[N]`, leaving the operand
//! untouched — so it composes with any sibling lowering inside the operand and
//! with wide template rewrites (`??`) that pass the whole expression through.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

struct TupleIndex {
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for TupleIndex {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        walk_expr(self, expr);

        let Expr::Attribute(attr) = expr else { return };
        let attr_id = attr.attr.id.as_str();
        if attr_id.is_empty() || !attr_id.bytes().all(|b| b.is_ascii_digit()) {
            return;
        }
        // strip leading zeros: `t.01` lowers to `t[1]`
        let trimmed = attr_id.trim_start_matches('0');
        let n: u64 = if trimmed.is_empty() {
            0
        } else {
            trimmed.parse().unwrap_or(0)
        };
        // replace the `.N` bytes (everything after the operand) with `[N]`
        self.edits.push((
            TextRange::new(attr.value.range().end(), expr.range().end()),
            format!("[{n}]"),
        ));
    }
}

pub(crate) struct TupleIndexPass;

impl TupleIndexPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for TupleIndexPass {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = TupleIndex { edits: Vec::new() };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.text_edits.extend(inner.edits);
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
    fn literal_tuple() {
        check("x = (1, 2).0\n", "x = (1, 2)[0]\n");
    }

    #[test]
    fn name_tuple() {
        check("y = t.1\n", "y = t[1]\n");
    }

    #[test]
    fn multi_digit() {
        check("z = t.10\n", "z = t[10]\n");
    }

    #[test]
    fn nested() {
        check("v = (1, (2, 3)).1.0\n", "v = (1, (2, 3))[1][0]\n");
    }

    #[test]
    fn followed_by_call() {
        check("r = t.0()\n", "r = t[0]()\n");
    }

    #[test]
    fn followed_by_subscript() {
        check("r = t.0[k]\n", "r = t[0][k]\n");
    }

    #[test]
    fn followed_by_attribute() {
        check("r = t.0.name\n", "r = t[0].name\n");
    }

    #[test]
    fn leading_zero() {
        check("x = t.01\n", "x = t[1]\n");
    }

    #[test]
    fn all_zero() {
        check("x = t.00\n", "x = t[0]\n");
    }

    #[test]
    fn composes_with_coalesce() {
        // `.N` inside a `??` operand is lowered inside the rewrite rather than
        // clobbered by it
        check(
            "x = t.0 ?? 9\n",
            "x = _t if (_t := t[0]) is not None else 9\n",
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("x = (1, 2)[0]\n");
    }

    #[test]
    fn float_literal_unchanged() {
        unchanged("x = 1.0\n");
    }
}
