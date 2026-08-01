use std::fmt::Write as _;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::transforms::source_util::temporary_name;
use crate::type_info::TypeInfo;

/// rewrites `a?.b` to `(None if a is None else a.b)` and chains like `a?.b?.c`
/// to `(None if a is None else None if (__by_t_0__ := a.b) is None else __by_t_0__.c)`
pub(crate) struct NoneChain<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) template_edits: Vec<(TextRange, Vec<Fragment>)>,
}

impl<'src> NoneChain<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            template_edits: Vec::new(),
        }
    }
}

/// How many temporaries a single expansion may need before it starts reusing
/// the last one — a chain nests one guard per `?.`, and ten is well past any
/// chain anyone writes
const TEMP_VARS: usize = 10;

/// The name a `?.` or `??` expansion walruses its receiver into.
pub(super) fn temp_var(index: usize) -> String {
    temporary_name("t", index)
}

fn pick_temp_var(types: &dyn TypeInfo, anchor: &Expr) -> String {
    (0..TEMP_VARS)
        .map(temp_var)
        .find(|name| types.is_unbound_at(name, anchor))
        .unwrap_or_else(|| temp_var(TEMP_VARS))
}

/// walks an attribute-access chain and returns `Some((python_form, guards, base))`
/// when any `?.` is present, where `python_form` has all `?.` replaced by `.`,
/// `guards` is the ordered list of accumulated sub-expressions that must be
/// non-None before each subsequent optional access is safe, and `base` is the
/// source range of the first `?.`'s receiver — everything before the first `?`
///
/// a `?.` whose base is a *wrapped* optional (`int??`, a generic `T?`)
/// reaches its present value through the runtime wrapper: the guard still
/// tests the wrapper against `None`, but the access reads `.value` first
pub(super) fn expand_chain(
    expr: &Expr,
    source: &str,
    types: &dyn TypeInfo,
) -> Option<(String, Vec<String>, TextRange)> {
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
                attr.value.range(),
            ))
        }
    }
}

/// builds a `None if ... is None else ...` chain from guards and final result,
/// using walrus assignment to avoid evaluating compound intermediate
/// expressions twice.
///
/// returns fragments rather than text: a compound chain base is walrus-bound
/// exactly once, and passing it through as a [`Fragment::Src`] span lets
/// sibling lowerings inside it (an extension call, a cast, …) materialize
/// instead of being clobbered. only the first guard can carry the raw base —
/// every later guard is an incremental `temp.suffix` — and a name-only base
/// has no interior for an edit to target, so the `Src` is needed exactly there
pub(super) fn build_expansion(
    guards: &[String],
    result: &str,
    temp: &str,
    base: TextRange,
) -> Vec<Fragment> {
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut s = String::new();
    let mut use_t = false;
    let mut prev_guard: Option<&str> = None;

    for (position, guard) in guards.iter().enumerate() {
        let guard_expr = if let Some(prev) = prev_guard.filter(|_| use_t) {
            let incremental = &guard[prev.len() + 1..];
            format!("{temp}.{incremental}")
        } else {
            guard.clone()
        };

        if guard_expr.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let _ = write!(s, "None if {guard_expr} is None else ");
        } else {
            if position == 0 {
                let _ = write!(s, "None if ({temp} := ");
                fragments.push(Fragment::Lit(std::mem::take(&mut s)));
                fragments.push(Fragment::Src(base));
                let _ = write!(s, ") is None else ");
            } else {
                let _ = write!(s, "None if ({temp} := {guard_expr}) is None else ");
            }
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
    if !s.is_empty() {
        fragments.push(Fragment::Lit(s));
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
) -> Option<(&'a Expr, String, Vec<String>, TextRange)> {
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
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = NoneChain::new(self.source, types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.template_edits.extend(inner.template_edits);
    }
}

impl NoneChain<'_> {
    /// visit the sub-expressions of the trailers between `head` and `expr` — the parts that
    /// pass through as source and so can carry chains of their own. `head` itself is
    /// rendered from its expansion, not walked
    fn visit_trailers(&mut self, expr: &Expr, head: &Expr) {
        if expr.range() == head.range() {
            return;
        }
        match expr {
            Expr::Attribute(attribute) => self.visit_trailers(&attribute.value, head),
            Expr::Call(call) => {
                self.visit_trailers(&call.func, head);
                for arg in &*call.arguments.args {
                    self.visit_expr(arg);
                }
                for keyword in &*call.arguments.keywords {
                    self.visit_expr(&keyword.value);
                }
            }
            Expr::Subscript(subscript) => {
                self.visit_trailers(&subscript.value, head);
                self.visit_expr(&subscript.slice);
            }
            _ => {}
        }
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
            let temp = pick_temp_var(self.types, expr);
            let trailers = TextRange::new(head.range().end(), expr.range().end());
            let mut fragments = vec![Fragment::Lit("(".to_owned())];
            fragments.extend(build_expansion(&guards, &form, &temp, base));
            fragments.push(Fragment::Src(trailers));
            fragments.push(Fragment::Lit(")".to_owned()));
            self.template_edits.push((expr.range(), fragments));
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
            "__by_t_0__ = 1\nx = (None if a is None else None if (__by_t_1__ := a.a) is None else __by_t_1__.b)\n",
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

    #[test]
    fn python_unchanged() {
        unchanged("x = None if a is None else a.b\n");
    }
}
