//! AST rewrite that strips basedpython typed lambda syntax down to standard
//! python:
//!   `lambda (a: int, b: str) -> int: a + b`  →  `lambda a, b: a + b`
//!
//! The parser produces `ExprLambda { parameters, returns, body }` with
//! optional per-parameter annotations and an optional return type — both
//! valid in `.by` but invalid python at value position. The AST pass clears
//! every annotation and the return type so the codegen emits a stock
//! `lambda a, b: a + b`.

use std::cell::Cell;

use ruff_python_ast::visitor::transformer::{Transformer, walk_expr};
use ruff_python_ast::{Expr, Parameters, Stmt};
use ruff_text_size::{Ranged, TextRange};

pub(crate) struct TypedLambda<'src> {
    source: &'src str,
    changed: Cell<bool>,
}

impl<'src> TypedLambda<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            changed: Cell::new(false),
        }
    }

    pub(crate) fn changed_cell(&self) -> &Cell<bool> {
        &self.changed
    }

    fn strip_annotations(params: &mut Parameters) -> bool {
        let mut changed = false;
        let strip = |ann: &mut Option<Box<Expr>>, changed: &mut bool| {
            if ann.is_some() {
                *ann = None;
                *changed = true;
            }
        };
        for pw in params
            .posonlyargs
            .iter_mut()
            .chain(params.args.iter_mut())
            .chain(params.kwonlyargs.iter_mut())
        {
            strip(&mut pw.parameter.annotation, &mut changed);
        }
        if let Some(v) = params.vararg.as_deref_mut() {
            strip(&mut v.annotation, &mut changed);
        }
        if let Some(k) = params.kwarg.as_deref_mut() {
            strip(&mut k.annotation, &mut changed);
        }
        changed
    }
}

impl Transformer for TypedLambda<'_> {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        ruff_python_ast::visitor::transformer::walk_stmt(self, stmt);
    }

    fn visit_expr(&self, expr: &mut Expr) {
        walk_expr(self, expr);

        let Expr::Lambda(lambda) = expr else { return };
        let mut any = false;
        if let Some(params) = lambda.parameters.as_deref_mut() {
            if Self::strip_annotations(params) {
                any = true;
            }
        }
        if lambda.returns.is_some() {
            lambda.returns = None;
            any = true;
        }
        // a parenthesized parameter list (`lambda (x): …`) is based-only
        // surface even without annotations — re-render to the bare form
        if let Some(p) = lambda.parameters.as_deref()
            && p.range() != TextRange::default()
            && self.source.as_bytes().get(usize::from(p.range().start())) == Some(&b'(')
        {
            any = true;
        }
        if any {
            self.changed.set(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::transpile;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &crate::Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn typed_lambda_basic() {
        check(
            "a = lambda (a: int, b: str) -> int: a\n",
            "a = lambda a, b: a\n",
        );
    }

    #[test]
    fn typed_lambda_no_return() {
        check("a = lambda (x: int): x\n", "a = lambda x: x\n");
    }

    #[test]
    fn typed_lambda_only_return() {
        // codegen emits `lambda : body` with a space when params is empty
        check("a = lambda () -> int: 42\n", "a = lambda : 42\n");
    }

    #[test]
    fn parenthesized_untyped_lambda() {
        // parens around lambda params are based-only surface even with no
        // annotations — they must not leak into the output
        check("g = lambda (x): str(x)\n", "g = lambda x: str(x)\n");
    }

    #[test]
    fn untyped_lambda_unchanged() {
        check("a = lambda x, y: x + y\n", "a = lambda x, y: x + y\n");
    }

    #[test]
    fn typed_lambda_with_star_args() {
        check(
            "a = lambda (*args, **kwargs) -> int: 0\n",
            "a = lambda *args, **kwargs: 0\n",
        );
    }
}
