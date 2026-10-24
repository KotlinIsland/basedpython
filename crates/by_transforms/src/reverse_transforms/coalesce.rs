//! reverse of `crate::transforms::coalesce`:
//!   `a if a is not None else b` → `a ?? b`
//!
//! only handles the simple form (same name in test and body).
//! the `_t := ...` walrus form is not reversed (too ambiguous)

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{CmpOp, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

pub(crate) struct CoalesceReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> CoalesceReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn rewrite(&self, expr: &Expr) -> Option<String> {
        let Expr::If(ternary) = expr else { return None };
        let Expr::Compare(cmp) = ternary.test.as_ref() else {
            return None;
        };
        if !matches!(&*cmp.ops, [CmpOp::IsNot]) {
            return None;
        }
        if !matches!(cmp.comparators(), [Expr::NoneLiteral(_)]) {
            return None;
        }
        let Expr::Name(test_name) = cmp.first_operand() else {
            return None;
        };
        let Expr::Name(body_name) = ternary.body.as_ref() else {
            return None;
        };
        if test_name.id != body_name.id {
            return None;
        }
        let lhs = self.src(ternary.body.range());
        let rhs = self
            .rewrite(&ternary.orelse)
            .unwrap_or_else(|| self.src(ternary.orelse.range()).to_owned());
        Some(format!("{lhs} ?? {rhs}"))
    }
}

impl<'ast> Visitor<'ast> for CoalesceReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Some(replacement) = self.rewrite(expr) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                replacement,
                expr.range(),
            )));
            return;
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn basic_coalesce() {
        check("x = a if a is not None else b\n", "x = a ?? b\n");
    }

    #[test]
    fn nested_coalesce() {
        check(
            "x = a if a is not None else (b if b is not None else c)\n",
            "x = a ?? b ?? c\n",
        );
    }

    #[test]
    fn walrus_form_unchanged() {
        // complex form with _t walrus is NOT reversed
        check(
            "_t if (_t := x) is not None else y\n",
            "_t if (_t := x) is not None else y\n",
        );
    }
}
