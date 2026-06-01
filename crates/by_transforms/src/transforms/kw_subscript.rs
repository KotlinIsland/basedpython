//! Lowers keyword arguments in subscriptions to a `__getitem__` call.
//!
//! `x[a, z=1]` → `x.__getitem__(a, z=1)`
//!
//! Python's subscript grammar doesn't accept keyword args (PEP 637 was
//! rejected), so basedpython's surface syntax falls back to the explicit
//! method call. positional and keyword args are forwarded in source order

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

pub(crate) struct KwSubscript<'src, T: TypeInfo + ?Sized> {
    source: &'src str,
    types: Option<&'src T>,
    pub(crate) edits: Vec<Fix>,
}

impl<'src, T: TypeInfo + ?Sized> KwSubscript<'src, T> {
    pub(crate) fn new(source: &'src str, types: Option<&'src T>) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// render a subscript argument value, lowering any postfix `?` it carries
    /// (`M[V=int?]`). our whole-subscript replacement subsumes `optional_type`'s
    /// narrow edits, so we lower the optional here; the runtime `Optional[...]`
    /// import for nested `??` is still raised by `OptionalTypePass`, which walks
    /// every expression independently
    fn value_src(&self, expr: &Expr) -> String {
        crate::transforms::optional_type::rewrite_type_expr(self.source, expr)
            .unwrap_or_else(|| self.src(expr.range()).to_owned())
    }

    fn rewrite_subscript(&mut self, sub: &ruff_python_ast::ExprSubscript) {
        // single keyword arg, e.g. `A[T=int]` (no surrounding tuple).
        // for a multi-typevar class with declared defaults, expand to a
        // positional list filling unbound slots with their declared defaults
        // (`A[R=int]` with `class A[T=int, R=str]` → `A[int, int]`).
        // single-typevar class falls back to dropping the kw name
        if let Expr::Named(n) = sub.slice.as_ref()
            && let Expr::Name(target) = n.target.as_ref()
            && matches!(target.ctx, ruff_python_ast::ExprContext::Invalid)
        {
            if let Some(types) = self.types
                && let Some(typevars) = types.class_typevars(&sub.value)
                && typevars.len() > 1
            {
                let value_src = self.value_src(n.value.as_ref());
                let mut parts: Vec<String> = Vec::with_capacity(typevars.len());
                for (tv_name, tv_default) in &typevars {
                    if tv_name == target.id.as_str() {
                        parts.push(value_src.clone());
                    } else if let Some(default) = tv_default {
                        parts.push(default.clone());
                    } else {
                        // typevar has no default and no kw arg — fall back
                        // to drop-name behavior; ty's diagnostics will catch
                        // the missing-arg case
                        let value_src = self.value_src(n.value.as_ref());
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            value_src,
                            n.range(),
                        )));
                        return;
                    }
                }
                let value_src = self.src(sub.value.range()).to_owned();
                let replacement = format!("{value_src}[{}]", parts.join(", "));
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    replacement,
                    sub.range(),
                )));
                return;
            }
            let value_src = self.value_src(n.value.as_ref());
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                value_src,
                n.range(),
            )));
            return;
        }
        let Expr::Tuple(t) = sub.slice.as_ref() else {
            return;
        };
        if t.parenthesized {
            return;
        }
        let has_kw = t.elts.iter().any(|e| {
            if let Expr::Named(n) = e {
                if let Expr::Name(name) = n.target.as_ref() {
                    return matches!(name.ctx, ruff_python_ast::ExprContext::Invalid);
                }
            }
            false
        });
        if !has_kw {
            return;
        }
        let all_kw = t.elts.iter().all(|e| {
            matches!(
                e,
                Expr::Named(n) if matches!(
                    n.target.as_ref(),
                    Expr::Name(name) if matches!(name.ctx, ruff_python_ast::ExprContext::Invalid)
                )
            )
        });
        // when every arg is a kw binding and the value is a known generic
        // class, reorder by typevar declaration and emit positional subscript.
        // unbound typevars fall back to their declared default
        // (`A[R=str, T=int]` → `A[int, str]`;
        //  `A[R=int]` with `A[T=int, R=str]` → `A[int, int]`)
        if all_kw
            && let Some(types) = self.types
            && let Some(typevars) = types.class_typevars(&sub.value)
        {
            let mut by_name: std::collections::HashMap<&str, &Expr> =
                std::collections::HashMap::new();
            for elt in &t.elts {
                if let Expr::Named(n) = elt
                    && let Expr::Name(target) = n.target.as_ref()
                {
                    by_name.insert(target.id.as_str(), n.value.as_ref());
                }
            }
            let mut parts: Vec<String> = Vec::with_capacity(typevars.len());
            let mut filled_all = true;
            for (tv_name, tv_default) in &typevars {
                if let Some(value_expr) = by_name.get(tv_name.as_str()) {
                    parts.push(self.value_src(value_expr));
                } else if let Some(default) = tv_default {
                    parts.push(default.clone());
                } else {
                    filled_all = false;
                    break;
                }
            }
            if filled_all {
                let value_src = self.src(sub.value.range()).to_owned();
                let replacement = format!("{value_src}[{}]", parts.join(", "));
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    replacement,
                    sub.range(),
                )));
                return;
            }
            // missing typevar without default — fall through to the
            // generic `value.__getitem__(name=value, ...)` form so the
            // output is at least syntactically valid Python
        }
        // Build `value.__getitem__(<args>)` where each Named field renders
        // as `name=value` and bare exprs render verbatim
        let value_src = self.src(sub.value.range()).to_owned();
        let parts: Vec<String> = t
            .elts
            .iter()
            .map(|e| {
                if let Expr::Named(n) = e
                    && let Expr::Name(target) = n.target.as_ref()
                    && matches!(target.ctx, ruff_python_ast::ExprContext::Invalid)
                {
                    return format!(
                        "{}={}",
                        target.id.as_str(),
                        self.value_src(n.value.as_ref())
                    );
                }
                self.value_src(e)
            })
            .collect();
        let replacement = format!("{value_src}.__getitem__({})", parts.join(", "));
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            replacement,
            sub.range(),
        )));
    }
}

pub(crate) struct KwSubscriptPass<'src> {
    source: &'src str,
}

impl<'src> KwSubscriptPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for KwSubscriptPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner: KwSubscript<'_, dyn TypeInfo> = KwSubscript::new(self.source, Some(types));
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
    }
}

impl<'ast, T: TypeInfo + ?Sized> Visitor<'ast> for KwSubscript<'_, T> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(s) = expr {
            self.rewrite_subscript(s);
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

    // multi-arg kw reorder lives under transpile_typed (needs ty type info).
    // exercised via mdtest `basedpython_kw_subscript`

    #[test]
    fn simple_kwarg() {
        check("x[a, z=1]\n", "x.__getitem__(a, z=1)\n");
    }

    #[test]
    fn multiple_kwargs() {
        check(
            "x[a, b, key=1, val=\"v\"]\n",
            "x.__getitem__(a, b, key=1, val=\"v\")\n",
        );
    }

    #[test]
    fn no_kwargs_unchanged() {
        check("x[a, b]\n", "x[a, b]\n");
    }

    #[test]
    fn single_kw_drops_name() {
        check("a: A[T=int]\n", "a: A[int]\n");
    }

    /// a `?` on a kw-subscript value lowers instead of leaking the bare token
    /// (our whole-subscript edit would otherwise subsume `optional_type`'s)
    #[test]
    fn single_kw_value_optional_lowers() {
        check("a: A[T=int?]\n", "a: A[int | None]\n");
    }

    #[test]
    fn getitem_kw_value_optional_lowers() {
        check(
            "d = data[idx, mode=int?]\n",
            "d = data.__getitem__(idx, mode=int | None)\n",
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("x[a, b]\n");
    }
}
