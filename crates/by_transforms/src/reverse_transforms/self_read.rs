//! reverse of what a method reads for `Self` ([`runtime_variance`]):
//!   `_by_self = type(self)  # basedpython: self is Self` is dropped, and every read of
//!   `_by_self` in the method is the `Self` the comment says the source wrote
//!
//! [`runtime_variance`]: crate::transforms::runtime_variance

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::runtime_variance::SELF_MARKER;
use crate::transforms::source_util::{line_past_end, line_start};

pub(crate) struct SelfReadReverse<'src> {
    source: &'src str,
    /// the name each method around the visitor binds its receiver's class to, with how the
    /// source writes `Self`
    bound: Vec<(String, String)>,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> SelfReadReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            bound: Vec::new(),
            edits: Vec::new(),
        }
    }

    /// the name and the source's `Self`, when `stmt` is the binding a method opens with
    fn binding(&self, stmt: &Stmt) -> Option<(String, String)> {
        let Stmt::Assign(assign) = stmt else {
            return None;
        };
        let [Expr::Name(target)] = &*assign.targets else {
            return None;
        };
        let line_end = line_past_end(self.source, stmt.end());
        let rest = self.source[TextRange::new(stmt.end(), line_end)].trim_end();
        let written = rest.trim_start().strip_prefix(SELF_MARKER)?;
        Some((target.id.to_string(), written.to_owned()))
    }
}

impl<'ast> Visitor<'ast> for SelfReadReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Some(binding) = self.binding(stmt) {
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    line_start(self.source, stmt.start()),
                    line_past_end(self.source, stmt.end()),
                ))));
            self.bound.push(binding);
            return;
        }
        let depth = self.bound.len();
        walk_stmt(self, stmt);
        if matches!(stmt, Stmt::FunctionDef(_)) {
            self.bound.truncate(depth);
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(name) = expr
            && let Some((_, written)) = self
                .bound
                .iter()
                .rev()
                .find(|(bound, _)| bound.as_str() == name.id.as_str())
        {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                written.clone(),
                expr.range(),
            )));
            return;
        }
        walk_expr(self, expr);
    }
}
