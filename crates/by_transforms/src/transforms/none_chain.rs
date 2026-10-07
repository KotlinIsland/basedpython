use std::fmt::Write as _;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprName, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::transforms::repeated_underscore::WrittenNames;
use crate::transforms::source_util::temporary_name;
use crate::transforms::walrus_scope::WalrusRegions;
use crate::type_info::TypeInfo;

/// rewrites `a?.b` to `(None if a is None else a.b)` and chains like `a?.b?.c`
/// to `(None if a is None else None if (__by_t_0__ := a.b) is None else __by_t_0__.c)`
pub(crate) struct NoneChain<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    regions: &'src WalrusRegions,
    template_edits: Vec<(TextRange, Vec<Fragment>)>,
    errors: Vec<String>,
}

/// How many temporaries a single expansion may need before it starts reusing
/// the last one — a chain nests one guard per `?.`, and ten is well past any
/// chain anyone writes
const TEMP_VARS: usize = 10;

/// The name a `?.` or `??` expansion walruses its receiver into.
pub(super) fn temp_var(written: WrittenNames<'_>, index: usize) -> String {
    temporary_name(written, "t", index)
}

fn pick_temp_var(written: WrittenNames<'_>, types: &dyn TypeInfo, anchor: &Expr) -> String {
    (0..TEMP_VARS)
        .map(|index| temp_var(written, index))
        .find(|name| types.is_unbound_at(name, anchor))
        .unwrap_or_else(|| temp_var(written, TEMP_VARS))
}

/// walks an attribute-access chain and returns `Some((python_form, guards, base))`
/// when any `?.` is present, where `python_form` has all `?.` replaced by `.`,
/// `guards` is the ordered list of accumulated sub-expressions that must be
/// non-None before each subsequent optional access is safe, and `base` is the
/// first `?.`'s receiver — everything before the first `?`
///
/// a `?.` whose base is a *wrapped* optional (`int??`, a generic `T?`)
/// reaches its present value through the runtime wrapper: the guard still
/// tests the wrapper against `None`, but the access reads `.value` first
fn expand_chain<'a>(
    expr: &'a Expr,
    source: &str,
    types: &dyn TypeInfo,
) -> Option<(String, Vec<String>, &'a Expr)> {
    let Expr::Attribute(attr) = expr else {
        return None;
    };
    let field = attr.attr.as_str();
    let unwrap = if attr.optional && types.wrapped_optional(&attr.value) {
        ".value"
    } else {
        ""
    };
    match expand_chain(&attr.value, source, types) {
        Some((v_form, mut guards, base)) => {
            if attr.optional {
                guards.push(v_form.clone());
            }
            Some((format!("{v_form}{unwrap}.{field}"), guards, base))
        }
        None => {
            if !attr.optional {
                return None;
            }
            let start = usize::from(attr.value.range().start());
            let end = usize::from(attr.value.range().end());
            let v_form = source[start..end].to_owned();
            Some((
                format!("{v_form}{unwrap}.{field}"),
                vec![v_form],
                &attr.value,
            ))
        }
    }
}

/// builds a `None if ... is None else ...` chain from guards and final result,
/// using walrus assignment to avoid evaluating compound intermediate
/// expressions twice. `trailers` — the calls, subscripts and attributes applied to
/// the chain's last access — pass through after the result
///
/// returns fragments rather than text: a compound chain base is walrus-bound
/// exactly once, and passing it through as a [`Fragment::Src`] span lets
/// sibling lowerings inside it (an extension call, a cast, …) materialize
/// instead of being clobbered. only the first guard can carry the raw base —
/// every later guard is an incremental `temp.suffix` — and a name-only base
/// has no interior for an edit to target, so the `Src` is needed exactly there
///
/// where python refuses `:=` (see [`WalrusRegions`]), the guards from the first
/// compound one on are written as the clauses of a comprehension instead, each
/// binding the temporary as its variable — `([t.c for t in (a.b,) if t is not None
/// for t in (t.d,) if t is not None] or [None])[0]` — which evaluates each guard
/// once and stops at the first `None` the same way
fn build_expansion(
    guards: &[String],
    result: &str,
    temp: &str,
    base: TextRange,
    trailers: TextRange,
    walrus: bool,
) -> Vec<Fragment> {
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut s = String::new();
    let mut use_t = false;
    let mut prev_guard: Option<&str> = None;
    // the comprehension's clauses, once a compound guard has opened one
    let mut clauses: Vec<Fragment> = Vec::new();

    for (position, guard) in guards.iter().enumerate() {
        let guard_expr = if let Some(prev) = prev_guard.filter(|_| use_t) {
            let incremental = &guard[prev.len() + 1..];
            format!("{temp}.{incremental}")
        } else {
            guard.clone()
        };

        if guard_expr.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let _ = write!(s, "None if {guard_expr} is None else ");
        } else if walrus {
            if position == 0 {
                let _ = write!(s, "None if ({temp} := ");
                fragments.push(Fragment::Lit(std::mem::take(&mut s)));
                fragments.push(Fragment::Src(base));
                let _ = write!(s, ") is None else ");
            } else {
                let _ = write!(s, "None if ({temp} := {guard_expr}) is None else ");
            }
            use_t = true;
        } else {
            if clauses.is_empty() {
                s.push_str("([");
            }
            clauses.push(Fragment::Lit(format!(" for {temp} in (")));
            if position == 0 {
                clauses.push(Fragment::Src(base));
            } else {
                clauses.push(Fragment::Lit(guard_expr));
            }
            clauses.push(Fragment::Lit(format!(",) if {temp} is not None")));
            use_t = true;
        }
        prev_guard = Some(guard.as_str());
    }

    if let Some(last) = prev_guard.filter(|_| use_t) {
        let incremental = &result[last.len() + 1..];
        let _ = write!(s, "{temp}.{incremental}");
    } else {
        s.push_str(result);
    }
    fragments.push(Fragment::Lit(s));
    fragments.push(Fragment::Src(trailers));
    if !clauses.is_empty() {
        fragments.extend(clauses);
        fragments.push(Fragment::Lit("] or [None])[0]".to_owned()));
    }
    fragments
}

/// the `?.` attribute chain at the base of `expr`, with its expansion, when `expr` is a
/// link of an optional chain.
///
/// a chain runs from its first `?.` out through the trailers applied to it — `.attr`,
/// `(...)`, `[...]`. only the attribute part expands; the trailers stay source
fn chain_head<'a>(
    expr: &'a Expr,
    source: &str,
    types: &dyn TypeInfo,
) -> Option<(&'a Expr, String, Vec<String>, &'a Expr)> {
    if let Some((form, guards, base)) = expand_chain(expr, source, types) {
        return Some((expr, form, guards, base));
    }
    match expr {
        Expr::Attribute(attribute) => chain_head(&attribute.value, source, types),
        Expr::Call(call) => chain_head(&call.func, source, types),
        Expr::Subscript(subscript) => chain_head(&subscript.value, source, types),
        _ => None,
    }
}

pub(crate) struct NoneChainPass<'src> {
    source: &'src str,
}

impl<'src> NoneChainPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for NoneChainPass<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::NoneChain)
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = NoneChain {
            source: self.source,
            types,
            regions: &ctx.walrus_regions,
            template_edits: Vec::new(),
            errors: Vec::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.template_edits.extend(inner.template_edits);
        ctx.errors.extend(inner.errors);
    }
}

impl NoneChain<'_> {
    /// where python refuses `:=` the trailers move into the comprehension that binds
    /// the chain's temporary. a name they read in a class body's own scope — from a
    /// comprehension's first iterable there — then no longer sees what the class body
    /// bound, and would fail or quietly read a module-level name of the same name.
    /// such a chain is refused rather than lowered
    fn refuse_class_body_names(&mut self, expr: &Expr, head: &Expr) {
        struct Names<'a>(Vec<&'a ExprName>);
        impl<'a> Visitor<'a> for Names<'a> {
            fn visit_expr(&mut self, expr: &'a Expr) {
                if let Expr::Name(name) = expr {
                    self.0.push(name);
                }
                walk_expr(self, expr);
            }
        }
        let mut names = Names(Vec::new());
        trailer_operands(expr, head, &mut |operand| names.visit_expr(operand));
        // a reference ty did not place may be one, so it is refused too
        if let Some(name) = names
            .0
            .into_iter()
            .find(|name| self.types.reads_class_scope_binding(name) != Some(false))
        {
            let line = self.source[..usize::from(expr.range().start())]
                .matches('\n')
                .count()
                + 1;
            self.errors.push(format!(
                "the `?.` chain on line {line} cannot be lowered: it is in a comprehension's \
                 first iterable in a class body, where python refuses `:=`, and the \
                 comprehension that holds its value instead would not see the class body's \
                 `{}`",
                name.id
            ));
        }
    }

    /// visit the sub-expressions of the trailers between `head` and `expr` — the parts that
    /// pass through as source and so can carry chains of their own. `head`'s own links are
    /// rendered from its expansion, not walked
    fn visit_trailers<'a>(&mut self, expr: &'a Expr, head: &'a Expr) {
        trailer_operands(expr, head, &mut |operand| self.visit_expr(operand));
    }
}

/// calls `visit` on each operand of the trailers between `head` and `expr`: the
/// arguments of a call, the index of a subscript
fn trailer_operands<'a>(expr: &'a Expr, head: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    if expr.range() == head.range() {
        return;
    }
    match expr {
        Expr::Attribute(attribute) => trailer_operands(&attribute.value, head, visit),
        Expr::Call(call) => {
            trailer_operands(&call.func, head, visit);
            for arg in &*call.arguments.args {
                visit(arg);
            }
            for keyword in &*call.arguments.keywords {
                visit(&keyword.value);
            }
        }
        Expr::Subscript(subscript) => {
            trailer_operands(&subscript.value, head, visit);
            visit(&subscript.slice);
        }
        _ => {}
    }
}

impl<'ast> Visitor<'ast> for NoneChain<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // top-down, so the first link of a chain we reach is its outermost: the edit spans
        // the whole chain, not just the `?.` access. a conditional binds looser than every
        // operator, so an expansion that stopped at the access would regroup whatever
        // followed into its `else` branch — `not a?.b` would yield `a.b`, and
        // `[i for i in a?.b]` would not even parse. the parentheses keep the emitted tree
        // the one that was parsed
        if let Some((head, form, guards, base)) = chain_head(expr, self.source, self.types) {
            let temp = pick_temp_var(WrittenNames::new(self.source), self.types, expr);
            let trailers = TextRange::new(head.range().end(), expr.range().end());
            let walrus = self.regions.allows(expr.range());
            if !walrus {
                self.refuse_class_body_names(expr, head);
            }
            let mut fragments = vec![Fragment::Lit("(".to_owned())];
            fragments.extend(build_expansion(
                &guards,
                &form,
                &temp,
                base.range(),
                trailers,
                walrus,
            ));
            fragments.push(Fragment::Lit(")".to_owned()));
            self.template_edits.push((expr.range(), fragments));
            // the base passes through as source, so a chain or a `??` inside it
            // lowers on its own
            self.visit_expr(base);
            self.visit_trailers(expr, head);
            return;
        }
        walk_expr(self, expr);
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
    fn wrapped_base_unwraps_value() {
        // `?.` on a wrapped optional reads the present value through the
        // runtime wrapper: the guard tests the wrapper, the access goes
        // through `.value`
        let out = transpile(
            "def g() -> int??:\n    return Some(5)\nw = g()\nx = w?.bit_length\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("x = (None if w is None else w.value.bit_length)\n"),
            "got: {out}"
        );
    }

    #[test]
    fn basic_chain() {
        check("x = a?.b\n", "x = (None if a is None else a.b)\n");
    }

    #[test]
    fn double_chain() {
        check(
            "x = a?.a?.b\n",
            "x = (None if a is None else None if (__by_t_0__ := a.a) is None else __by_t_0__.b)\n",
        );
    }

    #[test]
    fn double_chain_t_taken() {
        check(
            "__by_t_0__ = 1\nx = a?.a?.b\n",
            "__by_t_0__ = 1\nx = (None if a is None else None if (__by_t_0__2 := a.a) is None else __by_t_0__2.b)\n",
        );
    }

    #[test]
    fn triple_chain() {
        check(
            "x = a?.b?.c?.d\n",
            "x = (None if a is None else None if (__by_t_0__ := a.b) is None else None if (__by_t_0__ := __by_t_0__.c) is None else __by_t_0__.d)\n",
        );
    }

    #[test]
    fn mixed_chain() {
        check("x = a?.b.c\n", "x = (None if a is None else a.b.c)\n");
    }

    #[test]
    fn optional_after_plain_attr() {
        check(
            "x = a.b?.c\n",
            "x = (None if (__by_t_0__ := a.b) is None else __by_t_0__.c)\n",
        );
    }

    // the edit spans the whole chain — the `?.` access plus the trailers applied to it — and
    // parenthesizes it, so the emitted tree is the one that was parsed. the trailers pass
    // through as source, so chains nested in them still expand on their own

    #[test]
    fn call_on_chain() {
        check("x = a?.b()\n", "x = (None if a is None else a.b())\n");
    }

    #[test]
    fn call_on_double_chain() {
        check(
            "x = a?.b?.c()\n",
            "x = (None if a is None else None if (__by_t_0__ := a.b) is None else __by_t_0__.c())\n",
        );
    }

    #[test]
    fn call_with_args_on_chain() {
        check(
            "x = a?.b(1, k=2)\n",
            "x = (None if a is None else a.b(1, k=2))\n",
        );
    }

    #[test]
    fn chain_in_call_argument_expands_separately() {
        // the outer edit replaces only `a?.b`, so a chain nested in an argument is a
        // non-overlapping edit of its own rather than text copied verbatim
        check(
            "x = a?.b(c?.d)\n",
            "x = (None if a is None else a.b((None if c is None else c.d)))\n",
        );
    }

    #[test]
    fn trailers_after_call_on_chain() {
        check(
            "x = a?.b().c[0]\n",
            "x = (None if a is None else a.b().c[0])\n",
        );
    }

    #[test]
    fn subscript_on_chain() {
        check("x = a?.b[0]\n", "x = (None if a is None else a.b[0])\n");
    }

    #[test]
    fn call_on_chain_with_coalesce() {
        check(
            "x = a?.b() ?? c\n",
            "x = __by_t_0__ if (__by_t_0__ := (None if a is None else a.b())) is not None else c\n",
        );
    }

    // a conditional binds looser than every operator, so an expansion that stopped at the
    // `?.` access regrouped whatever followed into its `else` branch. each of these was
    // wrong before the chain got its own parentheses — silently, in code ty accepts

    #[test]
    fn operand_of_a_binary_op() {
        // was `None if a is None else a.b + 1` — the `+ 1` moved inside the else
        check("x = a?.b + 1\n", "x = (None if a is None else a.b) + 1\n");
    }

    #[test]
    fn operand_of_a_unary_op() {
        // was `-None if a is None else a.b`, which evaluates `-None` and raises
        check("x = -a?.b\n", "x = -(None if a is None else a.b)\n");
    }

    #[test]
    fn operand_of_not() {
        // was `not None if a is None else a.b`, which yields `a.b` — not `not a.b`
        check("x = not a?.b\n", "x = not (None if a is None else a.b)\n");
    }

    #[test]
    fn operand_of_a_comparison() {
        // was `None if a is None else a.b == z`, yielding `None` rather than `None == z`
        check("x = a?.b == z\n", "x = (None if a is None else a.b) == z\n");
    }

    #[test]
    fn body_of_a_conditional() {
        // was `None if a is None else a.b if z else w`, which ignored `z` entirely
        check(
            "x = a?.b if z else w\n",
            "x = (None if a is None else a.b) if z else w\n",
        );
    }

    #[test]
    fn test_of_a_conditional() {
        // an unparenthesized conditional is not a valid `if` test — this did not parse
        check(
            "x = z if a?.b else w\n",
            "x = z if (None if a is None else a.b) else w\n",
        );
    }

    #[test]
    fn comprehension_iterable() {
        // an unparenthesized conditional is not a valid comprehension iterable — this did
        // not parse
        check(
            "x = [i for i in a?.b]\n",
            "x = [i for i in (None if a is None else a.b)]\n",
        );
    }

    #[test]
    fn delimited_positions_stay_correct() {
        check("x = [a?.b]\n", "x = [(None if a is None else a.b)]\n");
        check("x = z[a?.b]\n", "x = z[(None if a is None else a.b)]\n");
    }

    /// a chain's base passes through as source, so a chain inside it, behind a call,
    /// lowers on its own
    #[test]
    fn chain_through_a_call() {
        check(
            "x = a?.b()?.c\n",
            "x = (None if (__by_t_0__ := (None if a is None else a.b())) is None else __by_t_0__.c)\n",
        );
    }

    /// python refuses `:=` in a comprehension's iterable, so the guards are the clauses
    /// of a comprehension there, binding the temporary as its variable
    #[test]
    fn chain_in_a_comprehension_iterable() {
        check(
            "x = [i for i in f()?.b?.c(1)]\n",
            "x = [i for i in (([__by_t_0__.c(1) for __by_t_0__ in (f(),) if __by_t_0__ is not None for __by_t_0__ in (__by_t_0__.b,) if __by_t_0__ is not None] or [None])[0])]\n",
        );
        check(
            "x = [i for i in a?.b?.c]\n",
            "x = [i for i in (None if a is None else ([__by_t_0__.c for __by_t_0__ in (a.b,) if __by_t_0__ is not None] or [None])[0])]\n",
        );
    }

    /// moving the trailers into that comprehension would take them out of a class
    /// body's scope, so a name the class body binds there is refused
    #[test]
    fn class_body_name_in_a_moved_trailer_is_refused() {
        let err = transpile(
            "class C:\n    k = 1\n    x = [i for i in f()?.g(k)]\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            err.contains("the `?.` chain on line 3 cannot be lowered") && err.contains("`k`"),
            "got: {err}"
        );
    }

    /// a builtin, a module-level name and an enclosing function's local resolve the
    /// same from the comprehension, so a chain reading them is lowered
    #[test]
    fn a_name_the_class_body_does_not_bind_is_lowered() {
        for source in [
            "class C:\n    x = [i for i in f()?.g(len)]\n",
            "k = 1\nclass C:\n    x = [i for i in f()?.g(k)]\n",
            "def outer():\n    k = 3\n    class C:\n        x = [i for i in f()?.g(k)]\n    return C\n",
            "class C:\n    x = [i for i in f()?.g(k)]\n    k = 1\n",
        ] {
            transpile(source, &Config::test_default())
                .unwrap_or_else(|err| panic!("refused {source:?}: {err}"));
        }
    }

    #[test]
    fn python_unchanged() {
        unchanged("x = None if a is None else a.b\n");
    }
}
