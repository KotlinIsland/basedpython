use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Operator, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// rewrites `a ?? b` to `a if a is not None else b`.
///
/// emits *template* edits: operands are `Src` passthrough spans, so any
/// sibling lowering inside an operand (`?.`, `!`, `expr.N`, …) is materialized
/// inside the rewrite instead of being clobbered by first-wins overlap dedup
pub(crate) struct NoneCoalesce<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<(TextRange, Vec<Fragment>)>,
}

impl<'src> NoneCoalesce<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }
}

fn expand_none_chain(expr: &Expr, source: &str, types: &dyn TypeInfo) -> Option<String> {
    let (form, guards) = super::none_chain::expand_chain(expr, source, types)?;
    Some(super::none_chain::build_expansion(&guards, &form, "_t"))
}

impl<'ast> Visitor<'ast> for NoneCoalesce<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::BinOp(b) = expr
            && matches!(b.op, Operator::Coalesce)
        {
            // build the whole (possibly chained) `??` expansion in one edit over
            // the outer expression's range, then stop — recursing into the
            // operands here would emit overlapping edits for any nested `??`
            let template = self.expand_coalesce(expr);
            self.edits.push((expr.range(), template));
            return;
        }
        walk_expr(self, expr);
    }
}

impl NoneCoalesce<'_> {
    /// Lower a `??` expression to a conditional template. Chained `??`
    /// recurses, so `a ?? b ?? c` becomes a nested `… if … is not None else …`
    /// rather than stranding the inner `??` as verbatim source. The walrus
    /// temp `_t` is reused safely: a nested `_t` only lives inside a branch
    /// that is no longer referenced once the enclosing `(_t := …)` is taken.
    fn expand_coalesce(&self, expr: &Expr) -> Vec<Fragment> {
        let Expr::BinOp(b) = expr else {
            return self.operand_value(expr);
        };
        if !matches!(b.op, Operator::Coalesce) {
            return self.operand_value(expr);
        }
        // a literal LHS statically known to be non-None short-circuits to the LHS
        // (avoids a `1 is not None` constant-fold warning)
        if is_non_none_literal(&b.left) {
            return self.operand_value(&b.left);
        }
        let rhs = self.operand_value(&b.right);
        // a wrapped optional (`int??`, a generic `T?`) keeps its present value
        // inside the runtime wrapper — the present branch unwraps with `.value`
        let unwrap = if self.types.wrapped_optional(&b.left) {
            ".value"
        } else {
            ""
        };
        match expand_none_chain(&b.left, self.source, self.types) {
            Some(expanded) => {
                let mut t = vec![
                    Fragment::Lit(format!("_t{unwrap} if (_t := ")),
                    Fragment::Lit(expanded),
                    Fragment::Lit(") is not None else ".to_owned()),
                ];
                t.extend(rhs);
                t
            }
            None if is_trivially_pure(&b.left) => {
                let mut t = vec![
                    Fragment::Src(b.left.range()),
                    Fragment::Lit(format!("{unwrap} if ")),
                    Fragment::Src(b.left.range()),
                    Fragment::Lit(" is not None else ".to_owned()),
                ];
                t.extend(rhs);
                t
            }
            None => {
                // a chained (left-associative) `??` puts another coalesce on the
                // left; recurse through `operand_value` so it is lowered rather
                // than copied verbatim. hoist to walrus so the LHS runs once
                let mut t = vec![Fragment::Lit(format!("_t{unwrap} if (_t := "))];
                t.extend(self.operand_value(&b.left));
                t.push(Fragment::Lit(") is not None else ".to_owned()));
                t.extend(rhs);
                t
            }
        }
    }

    /// Render an operand: a nested `??` is expanded, a `?.` chain is lowered,
    /// otherwise it is a passthrough span (any sibling edits inside it apply
    /// when the template is materialized).
    fn operand_value(&self, expr: &Expr) -> Vec<Fragment> {
        if let Expr::BinOp(b) = expr
            && matches!(b.op, Operator::Coalesce)
        {
            return self.expand_coalesce(expr);
        }
        if let Some(expanded) = expand_none_chain(expr, self.source, self.types) {
            return vec![Fragment::Lit(expanded)];
        }
        vec![Fragment::Src(expr.range())]
    }
}

/// Expressions whose evaluation has no side effects and whose value is
/// stable across two reads — safe to re-emit in both branches of the
/// rewrite without changing program semantics
fn is_trivially_pure(expr: &Expr) -> bool {
    match expr {
        Expr::Name(_)
        | Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_) => true,
        Expr::UnaryOp(u) => is_trivially_pure(&u.operand),
        _ => false,
    }
}

/// Literals whose value cannot be `None`. `??` on these is a no-op and the
/// `is not None` rewrite would otherwise produce a `SyntaxWarning` from
/// CPython's constant-folder
fn is_non_none_literal(expr: &Expr) -> bool {
    match expr {
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::FString(_) => true,
        // `-1` / `+1` etc. — the inner literal still isn't None
        Expr::UnaryOp(u) => is_non_none_literal(&u.operand),
        _ => false,
    }
}

pub(crate) struct NoneCoalescePass<'src> {
    source: &'src str,
}

impl<'src> NoneCoalescePass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for NoneCoalescePass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = NoneCoalesce::new(self.source, types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.template_edits.extend(inner.edits);
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
    fn basic_coalesce() {
        check("x = a ?? b\n", "x = a if a is not None else b\n");
    }

    #[test]
    fn chained_coalesce_recurses() {
        // chained `??` lowers in one edit, recursing into the nested coalesce
        // rather than stranding it as verbatim `??` source
        check(
            "x = a ?? b ?? c\n",
            "x = _t if (_t := a if a is not None else b) is not None else c\n",
        );
    }

    #[test]
    fn chained_coalesce_composes_with_optional_annotation() {
        // the chain and the `int?` annotation are lowered by disjoint edits in
        // the same statement — neither clobbers the other
        check(
            indoc::indoc! {"
                def f(a: int?, b: int?, c: int) -> int:
                    return a ?? b ?? c
            "},
            indoc::indoc! {"
                def f(a: int | None, b: int | None, c: int) -> int:
                    return _t if (_t := a if a is not None else b) is not None else c
            "},
        );
    }

    #[test]
    fn coalesce_with_optional_chain() {
        check(
            indoc::indoc! {"
                def f(a):
                    a?.a.b ?? 1
            "},
            indoc::indoc! {"
                def f(a):
                    _t if (_t := None if a is None else a.a.b) is not None else 1
            "},
        );
    }

    #[test]
    fn optional_chain_in_rhs_composes() {
        // the RHS is a passthrough span, so a `?.` chain inside it is lowered
        // by the none-chain pass rather than copied verbatim
        check(
            indoc::indoc! {"
                def f(a, b):
                    a ?? b?.c
            "},
            indoc::indoc! {"
                def f(a, b):
                    a if a is not None else None if b is None else b.c
            "},
        );
    }

    #[test]
    fn force_unwrap_in_lhs_composes() {
        // `!` inside the hoisted LHS is lowered inside the walrus, not
        // stranded
        let out = transpile("x = f()! ?? 1\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("x = _t if (_t := _force_unwrap(f())) is not None else 1\n"),
            "got: {out}"
        );
    }

    #[test]
    fn wrapped_lhs_unwraps_present_value() {
        // a wrapped optional's present value lives inside the runtime wrapper —
        // the present branch reads `.value`
        let out = transpile(
            "def g() -> int??:\n    return Some(5)\nx = g() ?? -1\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("x = _t.value if (_t := g()) is not None else -1\n"),
            "got: {out}"
        );
    }

    #[test]
    fn python_unchanged() {
        let src = "x = a if a is not None else b\n";
        unchanged(src);
    }

    #[test]
    fn call_lhs_hoisted_to_walrus_single_eval() {
        check(
            "x = f() ?? 1\n",
            "x = _t if (_t := f()) is not None else 1\n",
        );
    }

    #[test]
    fn attr_lhs_hoisted_to_walrus_single_eval() {
        check(
            "x = a.b ?? 1\n",
            "x = _t if (_t := a.b) is not None else 1\n",
        );
    }

    #[test]
    fn literal_lhs_collapses() {
        check("x = 1 ?? 2\n", "x = 1\n");
        check("x = (1 ?? 2) + 3\n", "x = (1) + 3\n");
        check("x = -5 ?? 2\n", "x = -5\n");
        check("x = \"a\" ?? \"b\"\n", "x = \"a\"\n");
    }
}
