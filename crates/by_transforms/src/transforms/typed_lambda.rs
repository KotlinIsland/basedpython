//! Strips basedpython typed lambda syntax down to standard python:
//!   `lambda (a: int, b: str) -> int: a + b`  →  `lambda a, b: a + b`
//!
//! The parser produces `ExprLambda { parameters, returns, body }` with
//! optional per-parameter annotations and an optional return type — both
//! valid in `.by` but invalid python at value position. The parentheses around
//! the parameter list are basedpython-only surface as well, even with no
//! annotation on them.
//!
//! All of that is removed as **source deletions** rather than by clearing the
//! AST: a cleared node would make the driver re-render the whole enclosing
//! statement, and a re-render drops every sub-statement edit inside it. A
//! lambda is a value, so what surrounds it is ordinary code that other passes
//! rewrite — a loop's [per-iteration binding](super::unique_loop_bindings)
//! wraps it, `??` lowers inside its body — and those edits have to survive.
//! Deletions leave the rest of the statement's bytes alone, so they do.
//!
//! A shape with no parenthesized parameter list to anchor to keeps the old AST
//! rewrite, and the re-render that comes with it.

use std::cell::{Cell, RefCell};

use ruff_python_ast::visitor::transformer::{Transformer, walk_expr};
use ruff_python_ast::{Expr, Parameters, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

pub(crate) struct TypedLambda<'src> {
    source: &'src str,
    changed: Cell<bool>,
    edits: RefCell<Vec<(TextRange, String)>>,
}

impl<'src> TypedLambda<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            changed: Cell::new(false),
            edits: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn changed_cell(&self) -> &Cell<bool> {
        &self.changed
    }

    /// the source deletions collected across the run, for the driver to apply
    pub(crate) fn take_edits(&self) -> Vec<(TextRange, String)> {
        self.edits.take()
    }

    /// whether the parameter list is the parenthesized basedpython form, whose
    /// span the deletions anchor to
    fn parenthesized(&self, parameters: &Parameters) -> bool {
        parameters.range() != TextRange::default()
            && self
                .source
                .as_bytes()
                .get(usize::from(parameters.range().start()))
                == Some(&b'(')
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
        let Some(parameters) = lambda.parameters.as_deref() else {
            // no parameter list at all: only a return type could need removing,
            // and there is nothing to anchor a deletion to
            if lambda.returns.take().is_some() {
                self.changed.set(true);
            }
            return;
        };
        if !self.parenthesized(parameters) {
            // a synthesized lambda, or one already in python's bare form —
            // fall back to the AST rewrite for any annotation on it
            let mut any = lambda.returns.take().is_some();
            if let Some(parameters) = lambda.parameters.as_deref_mut() {
                any |= Self::strip_annotations(parameters);
            }
            if any {
                self.changed.set(true);
            }
            return;
        }

        let span = parameters.range();
        let one = TextSize::from(1);
        let mut edits = self.edits.borrow_mut();
        // the parentheses themselves
        edits.push((
            TextRange::new(span.start(), span.start() + one),
            String::new(),
        ));
        edits.push((TextRange::new(span.end() - one, span.end()), String::new()));
        // each `: annotation`, from the end of the name it follows
        for parameter in parameters {
            if let Some(annotation) = parameter.annotation() {
                edits.push((
                    TextRange::new(parameter.name().range().end(), annotation.range().end()),
                    String::new(),
                ));
            }
        }
        // the ` -> return`, which sits between the list and the body's colon
        if let Some(returns) = lambda.returns.as_deref() {
            edits.push((
                TextRange::new(span.end(), returns.range().end()),
                String::new(),
            ));
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

    /// the deletions leave every other byte of the statement in place, so a
    /// lowering inside the lambda's own body still applies — re-rendering the
    /// statement, as this pass used to, dropped it
    #[test]
    fn a_lowering_inside_the_body_survives() {
        check(
            "f = lambda (s: str?): s ?? \"d\"\n",
            "f = lambda s: s if s is not None else \"d\"\n",
        );
    }

    /// only the annotation is cut from between a parameter and its default, so
    /// the default itself is untouched
    #[test]
    fn an_annotated_default_keeps_its_value() {
        check("a = lambda (x: int = 2): x\n", "a = lambda x = 2: x\n");
    }

    #[test]
    fn typed_lambda_with_star_args() {
        check(
            "a = lambda (*args, **kwargs) -> int: 0\n",
            "a = lambda *args, **kwargs: 0\n",
        );
    }
}
