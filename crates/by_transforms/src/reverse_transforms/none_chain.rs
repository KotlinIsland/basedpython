//! reverse of `crate::transforms::none_chain`:
//!   `None if a is None else a.b` → `a?.b`
//!   `None if (_t := a.b) is None else _t.c` → `a.b?.c`
//!
//! recursive: handles chained optional access by decoding nested none-if expressions

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{CmpOp, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

pub(crate) struct NoneChainReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> NoneChainReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// recursively decode a none-chain expression to basedpython optional-access form
    fn rewrite_chain(&self, expr: &Expr) -> Option<String> {
        let Expr::If(ternary) = expr else { return None };
        // body must be `None`
        if !matches!(ternary.body.as_ref(), Expr::NoneLiteral(_)) {
            return None;
        }
        // test must be `... is None`
        let Expr::Compare(cmp) = ternary.test.as_ref() else {
            return None;
        };
        if !matches!(&*cmp.ops, [CmpOp::Is]) {
            return None;
        }
        if !matches!(cmp.comparators(), [Expr::NoneLiteral(_)]) {
            return None;
        }

        // recursively decode orelse (handles chained none-if)
        let orelse_str = self
            .rewrite_chain(&ternary.orelse)
            .unwrap_or_else(|| self.src(ternary.orelse.range()).to_owned());

        // `orelse_str` is the rewritten orelse — already in `name?.…` /
        // `expr?.…` form when the orelse itself was a none-if chain, or the
        // verbatim source otherwise. either way the leading `{name}.` prefix
        // is the guard name (or walrus temp); strip it and prepend `name?.`
        match cmp.first_operand() {
            Expr::Name(guard_name) => {
                let prefix = format!("{}.", guard_name.id.as_str());
                let rest = orelse_str.strip_prefix(prefix.as_str())?;
                Some(format!("{}?.{rest}", guard_name.id.as_str()))
            }
            Expr::Named(walrus) => {
                let Expr::Name(temp_name) = walrus.target.as_ref() else {
                    return None;
                };
                let prefix = format!("{}.", temp_name.id.as_str());
                let rest = orelse_str.strip_prefix(prefix.as_str())?;
                let expr_str = self
                    .rewrite_chain(&walrus.value)
                    .unwrap_or_else(|| self.src(walrus.value.range()).to_owned());
                Some(format!("{expr_str}?.{rest}"))
            }
            _ => None,
        }
    }
}

impl<'ast> Visitor<'ast> for NoneChainReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Some(replacement) = self.rewrite_chain(expr) {
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
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn basic_chain() {
        check("x = None if a is None else a.b\n", "x = a?.b\n");
    }

    #[test]
    fn mixed_chain() {
        check("x = None if a is None else a.b.c\n", "x = a?.b.c\n");
    }

    #[test]
    fn optional_after_plain_attr() {
        check(
            "x = None if (_t := a.b) is None else _t.c\n",
            "x = a.b?.c\n",
        );
    }

    #[test]
    fn double_chain() {
        check(
            "x = None if a is None else None if (_t := a.a) is None else _t.b\n",
            "x = a?.a?.b\n",
        );
    }

    #[test]
    fn triple_chain() {
        check(
            "x = None if a is None else None if (_t := a.b) is None else None if (_t := _t.c) is None else _t.d\n",
            "x = a?.b?.c?.d\n",
        );
    }

    #[test]
    fn chain_in_function() {
        check(
            indoc! {"
                def f(a):
                    x = None if a is None else a.b
            "},
            indoc! {"
                def f(a):
                    x = a?.b
            "},
        );
    }
}
