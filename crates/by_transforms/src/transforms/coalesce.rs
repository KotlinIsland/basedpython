use ruff_python_ast::parenthesize::parenthesized_range;
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{AnyNodeRef, Comprehension, Expr, ExprBinOp, Operator, Stmt};
use ruff_python_trivia::CommentRanges;
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::repeated_underscore::WrittenNames;
use super::walrus_scope::WalrusRegions;
use crate::type_info::TypeInfo;

/// rewrites `a ?? b` to `a if a is not None else b`.
///
/// emits *template* edits: operands are `Src` passthrough spans, so any
/// sibling lowering inside an operand (`?.`, `!`, `expr.N`, a `??` of its own)
/// is materialized inside the rewrite instead of being clobbered by first-wins
/// overlap dedup
pub(crate) struct NoneCoalesce<'src, 'ast> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    comments: &'src CommentRanges,
    regions: &'src WalrusRegions,
    /// the node the next `??` visited is written directly under, when python reads
    /// a conditional expression there as something else, or refuses one: an operand
    /// of `and` / `or`, the test or value of a conditional expression, a
    /// comprehension's iterable or condition, what a `*` or `**` unpacks
    tight_parent: Option<AnyNodeRef<'ast>>,
    edits: Vec<(TextRange, Vec<Fragment>)>,
}

impl<'ast> Visitor<'ast> for NoneCoalesce<'_, 'ast> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        let tight_parent = self.tight_parent.take();
        match expr {
            Expr::BinOp(coalesce) if matches!(coalesce.op, Operator::Coalesce) => {
                // a literal that cannot be `None` is the whole value, and the
                // fallback never runs
                if is_non_none_literal(&coalesce.left) {
                    self.edits
                        .push((expr.range(), vec![Fragment::Src(coalesce.left.range())]));
                    self.visit_expr(&coalesce.left);
                    return;
                }
                let (mut template, conditional) = self.lower(coalesce);
                if let Some(parent) = tight_parent
                    && conditional
                    && parenthesized_range(expr.into(), parent, self.comments, self.source)
                        .is_none()
                {
                    template.insert(0, Fragment::Lit("(".to_owned()));
                    template.push(Fragment::Lit(")".to_owned()));
                }
                self.edits.push((expr.range(), template));
                self.visit_expr(&coalesce.left);
                self.visit_expr(&coalesce.right);
            }
            Expr::BoolOp(bool_op) => {
                for value in &bool_op.values {
                    self.tight_parent = Some(expr.into());
                    self.visit_expr(value);
                }
            }
            Expr::If(conditional) => {
                self.tight_parent = Some(expr.into());
                self.visit_expr(&conditional.test);
                self.tight_parent = Some(expr.into());
                self.visit_expr(&conditional.body);
                self.visit_expr(&conditional.orelse);
            }
            Expr::Starred(starred) => {
                self.tight_parent = Some(expr.into());
                self.visit_expr(&starred.value);
            }
            Expr::Dict(dict) => {
                for item in &dict.items {
                    match &item.key {
                        Some(key) => self.visit_expr(key),
                        None => self.tight_parent = Some(expr.into()),
                    }
                    self.visit_expr(&item.value);
                }
            }
            _ => walk_expr(self, expr),
        }
    }

    fn visit_comprehension(&mut self, comprehension: &'ast Comprehension) {
        self.visit_expr(&comprehension.target);
        self.tight_parent = Some(comprehension.into());
        self.visit_expr(&comprehension.iter);
        for condition in &comprehension.ifs {
            self.tight_parent = Some(comprehension.into());
            self.visit_expr(condition);
        }
    }
}

impl NoneCoalesce<'_, '_> {
    /// `a ?? b`, with `a` evaluated once and `b` only when `a` is `None`. each operand
    /// passes through as source, so a `??` written inside one lowers in place. the
    /// flag says whether the template is a conditional expression, which binds looser
    /// than any operator
    fn lower(&self, coalesce: &ExprBinOp) -> (Vec<Fragment>, bool) {
        let left = Fragment::Src(coalesce.left.range());
        let right = Fragment::Src(coalesce.right.range());
        // a wrapped optional (`int??`, a generic `T?`) keeps its present value
        // inside the runtime wrapper — the present branch unwraps with `.value`
        let unwrap = if self.types.wrapped_optional(&coalesce.left) {
            ".value"
        } else {
            ""
        };
        if is_trivially_pure(&coalesce.left) {
            let template = vec![
                left.clone(),
                Fragment::Lit(format!("{unwrap} if ")),
                left,
                Fragment::Lit(" is not None else ".to_owned()),
                right,
            ];
            return (template, true);
        }
        // every `??` binds the same temporary. each reads it back straight after
        // binding it, before anything else in the expression runs, so a `??` nested
        // in an operand never overwrites a value still to be read
        let temp = super::none_chain::temp_var(WrittenNames::new(self.source), 0);
        if self.regions.allows(coalesce.range) {
            let template = vec![
                Fragment::Lit(format!("{temp}{unwrap} if ({temp} := ")),
                left,
                Fragment::Lit(") is not None else ".to_owned()),
                right,
            ];
            (template, true)
        } else {
            // where python refuses `:=` the temporary is a comprehension variable,
            // and the fallback is chosen by `or` on whether the comprehension kept
            // the value: `a` is its first iterable and `b` is outside it, so both
            // are still evaluated in the scope they were written in
            let template = vec![
                Fragment::Lit(format!("([{temp}{unwrap} for {temp} in (")),
                left,
                Fragment::Lit(format!(",) if {temp} is not None] or [")),
                right,
                Fragment::Lit("])[0]".to_owned()),
            ];
            (template, false)
        }
    }
}

/// expressions whose evaluation has no side effects and whose value is stable
/// across two reads, so they are safe to re-emit in both branches of the rewrite.
/// a unary operator is not one: `-x` calls `x.__neg__()`, which may do anything
pub(crate) fn is_trivially_pure(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Name(_)
            | Expr::NumberLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
    )
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
    comments: &'src CommentRanges,
}

impl<'src> NoneCoalescePass<'src> {
    pub(crate) fn new(source: &'src str, comments: &'src CommentRanges) -> Self {
        Self { source, comments }
    }
}

impl TypeAwarePass for NoneCoalescePass<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::NoneCoalesce)
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = NoneCoalesce {
            source: self.source,
            types,
            comments: self.comments,
            regions: &ctx.walrus_regions,
            tight_parent: None,
            edits: Vec::new(),
        };
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

    /// the runtime imported from a module written beside the output, as a build writes it,
    /// so an expectation names the helpers it uses rather than spelling out their definitions
    fn check_beside_runtime(input: &str, expected: &str) {
        let config = Config {
            runtime_module: Some("_by_runtime".to_owned()),
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
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
            "x = __by_t_0__ if (__by_t_0__ := a if a is not None else b) is not None else c\n",
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
                    return __by_t_0__ if (__by_t_0__ := a if a is not None else b) is not None else c
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
                    __by_t_0__ if (__by_t_0__ := (None if a is None else a.a.b)) is not None else 1
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
                    a if a is not None else (None if b is None else b.c)
            "},
        );
    }

    #[test]
    fn force_unwrap_in_lhs_composes() {
        // `!` inside the hoisted LHS is lowered inside the walrus, not
        // stranded
        let out = transpile("x = f()! ?? 1\n", &Config::test_default()).unwrap();
        assert!(
            out.contains(
                "x = __by_t_0__ if (__by_t_0__ := _force_unwrap(f())) is not None else 1\n"
            ),
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
            out.contains("x = __by_t_0__.value if (__by_t_0__ := g()) is not None else -1\n"),
            "got: {out}"
        );
    }

    /// a parameter's default is a value like any other, and the `??` in one lowers
    /// here. a second pass used to claim it for an AST rewrite of the whole `def`,
    /// which printed the signature again over this edit and left the program refused
    #[test]
    fn coalesce_in_a_parameter_default() {
        check_beside_runtime(
            indoc::indoc! {"
                g: int? = None

                def f(y: int = g ?? 1) -> int:
                    return y
            "},
            indoc::indoc! {"
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"g ?? 1\", __name__, \"_MISSING\")
                g: int | None = None

                def f(y: int = _MISSING) -> int:
                    if y is _MISSING:
                        y = g if g is not None else 1
                    return y
            "},
        );
    }

    /// the same default with the body written on the clause's line
    #[test]
    fn coalesce_in_a_parameter_default_with_an_inline_body() {
        check_beside_runtime(
            indoc::indoc! {"
                g: int? = None

                def f(y: int = g ?? 1) -> int: return y
            "},
            "from typing import Any\nfrom _by_runtime import _by_default\n_MISSING: Any = _by_default(\"g ?? 1\", __name__, \"_MISSING\")\ng: int | None = None\n\ndef f(y: int = _MISSING) -> int: \n    if y is _MISSING:\n        y = g if g is not None else 1\n    return y\n",
        );
    }

    /// and a chain longer than one link, which recurses in the same edit
    #[test]
    fn a_chained_coalesce_in_a_parameter_default() {
        check_beside_runtime(
            indoc::indoc! {"
                g: int? = None
                h: int? = None

                def f(y: int = g ?? h ?? 1) -> int:
                    return y
            "},
            indoc::indoc! {"
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"g ?? h ?? 1\", __name__, \"_MISSING\")
                g: int | None = None
                h: int | None = None

                def f(y: int = _MISSING) -> int:
                    if y is _MISSING:
                        y = __by_t_0__ if (__by_t_0__ := g if g is not None else h) is not None else 1
                    return y
            "},
        );
    }

    /// a `??` inside an operand of another one, short of being the operand itself, is
    /// lowered by its own edit, inside the source the outer one passes through
    #[test]
    fn coalesce_inside_an_operand() {
        check(
            "x = (a ?? {}).get(k) ?? []\n",
            "x = __by_t_0__ if (__by_t_0__ := (a if a is not None else {}).get(k)) is not None else []\n",
        );
        check(
            "x = a ?? f(b ?? 1)\n",
            "x = a if a is not None else f(b if b is not None else 1)\n",
        );
        check(
            "x = f\"{a ?? 1}\"\n",
            "x = f\"{a if a is not None else 1}\"\n",
        );
    }

    /// a conditional expression binds looser than every operator, so where the `??`
    /// is an operand of `and` / `or`, the test or value of a conditional expression, a
    /// comprehension's iterable or condition, or what a `*` / `**` unpacks, it is
    /// parenthesized — unless the source already did
    #[test]
    fn coalesce_keeps_its_grouping() {
        check(
            "x = a ?? b or c\n",
            "x = (a if a is not None else b) or c\n",
        );
        check(
            "x = a ?? b if c else d\n",
            "x = (a if a is not None else b) if c else d\n",
        );
        check(
            "x = c if a ?? b else d\n",
            "x = c if (a if a is not None else b) else d\n",
        );
        check(
            "x = [i for i in xs if a ?? b]\n",
            "x = [i for i in xs if (a if a is not None else b)]\n",
        );
        check(
            "x = (a ?? b) or c\n",
            "x = (a if a is not None else b) or c\n",
        );
        check("x = [*a ?? b]\n", "x = [*(a if a is not None else b)]\n");
        check("x = {**a ?? b}\n", "x = {**(a if a is not None else b)}\n");
    }

    /// python refuses `:=` in a comprehension's iterable, so the value is bound as a
    /// comprehension variable there instead
    #[test]
    fn coalesce_in_a_comprehension_iterable() {
        check(
            "x = [i for i in f() ?? []]\n",
            "x = [i for i in ([__by_t_0__ for __by_t_0__ in (f(),) if __by_t_0__ is not None] or [[]])[0]]\n",
        );
    }

    /// and in every part of a comprehension in a class body
    #[test]
    fn coalesce_in_a_class_body_comprehension() {
        check(
            "class C:\n    x = [f(i) ?? 0 for i in xs]\n    y = f(0) ?? 0\n",
            "class C:\n    x = [([__by_t_0__ for __by_t_0__ in (f(i),) if __by_t_0__ is not None] or [0])[0] for i in xs]\n    y = __by_t_0__ if (__by_t_0__ := f(0)) is not None else 0\n",
        );
    }

    /// a unary operator calls a method of its operand, so its result is bound once
    #[test]
    fn unary_lhs_hoisted_to_walrus_single_eval() {
        check(
            "x = -a ?? 1\n",
            "x = __by_t_0__ if (__by_t_0__ := -a) is not None else 1\n",
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
            "x = __by_t_0__ if (__by_t_0__ := f()) is not None else 1\n",
        );
    }

    #[test]
    fn attr_lhs_hoisted_to_walrus_single_eval() {
        check(
            "x = a.b ?? 1\n",
            "x = __by_t_0__ if (__by_t_0__ := a.b) is not None else 1\n",
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
