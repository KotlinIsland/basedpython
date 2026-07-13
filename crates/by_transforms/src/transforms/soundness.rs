//! Runtime type-soundness checks.
//!
//! ty's inference is sound only up to the assumptions the type system makes.
//! Three of those assumptions are pure annotation-level claims with no runtime
//! backing, and this pass validates each one where it is consumed:
//!
//! - **typevar solutions at generic call sites** — `def t[T]() -> T` called as
//!   `a: str = t()` solves `T = str` from context alone; nothing at runtime
//!   guarantees the function honored it. Also covers methods bound to a
//!   specialized generic instance (`d.get(k)` on a `dict[str, int]`), where
//!   the specialization itself is the unverified claim.
//! - **element projections out of specialized containers** — `a[0]` on an
//!   `a: list[str]`, and iteration (`for x in a:`, comprehensions): the
//!   annotation's element claim is only shallowly checkable on the container,
//!   so each projected element is validated where it surfaces.
//! - **explicit `Any` flowing into a declared binding** — `a: str = dyn_val`
//!   is permitted statically because `Any` is assignable to everything; the
//!   declared type is validated at the assignment.
//!
//! Each gated expression is wrapped in `_soundness_check(expr, target)` (or,
//! for iteration, the iterable in `_soundness_iter(...)`/`_soundness_aiter(...)`),
//! where `target` is a shallow `isinstance` second argument derived from the
//! inferred type (`str`, `(int, type(None))`, `list` for `list[str]` — the
//! element claim is validated at its own projection sites). Types with no
//! faithful shallow runtime test (protocols, callables, unsolved typevars,
//! dynamic types) and types whose name doesn't resolve at module scope emit
//! no check. A check whose target is exactly `type(None)` is dropped too:
//! validating a `None` result guards no data flowing onward.
//!
//! The wraps are [`Fragment`] template edits, so sibling lowerings inside the
//! wrapped expression (coalesce, force-unwrap, generic-call stripping) are
//! materialized inside the check's passthrough span, and nested checks
//! (`t()[0]`) compose by template recursion. Statements an AST-mutation pass
//! re-rendered (casts, coalesce chains, `typeof`) are skipped wholesale —
//! their source ranges no longer align with the output.
//!
//! Known gaps, deliberate for now: no checks on `await` results, `return`
//! values, argument-to-parameter `Any` boundaries, or unpacking targets; a
//! check argument naming a class defined later in the module can raise
//! `NameError` if the checked line runs at import time before the class body.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Comprehension, Expr, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::Config;
use crate::type_info::TypeInfo;

const CHECK_HELPER: &str = "\
def _soundness_check(_v, _t):
    if not isinstance(_v, _t):
        raise TypeError(
            f\"type soundness violation: expected {getattr(_t, '__name__', _t)}, \"
            f\"got {type(_v).__name__}\"
        )
    return _v
";

const ITER_HELPER: &str = "\
def _soundness_iter(_it, _t):
    for _x in _it:
        yield _soundness_check(_x, _t)
";

const AITER_HELPER: &str = "\
async def _soundness_aiter(_it, _t):
    async for _x in _it:
        yield _soundness_check(_x, _t)
";

struct Soundness<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    /// gated expressions already covered by a wrap on an enclosing `!`
    /// force-unwrap — wrapping them directly would splice the check's second
    /// argument into `_force_unwrap`'s call parens
    consumed: Vec<TextRange>,
    used_iter: bool,
    used_aiter: bool,
}

impl<'a> Soundness<'a> {
    fn new(types: &'a dyn TypeInfo) -> Self {
        Self {
            types,
            edits: Vec::new(),
            consumed: Vec::new(),
            used_iter: false,
            used_aiter: false,
        }
    }

    /// whether `expr` is a projection this pass validates: a call whose
    /// result type came from a typevar solution, or an element read out of a
    /// specialized container
    fn gated(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call(call) => self.types.call_result_is_typevar_derived(&call.func),
            Expr::Subscript(subscript) => {
                subscript.ctx.is_load()
                    && self.types.is_specialized_generic_instance(&subscript.value)
            }
            _ => false,
        }
    }

    /// the shallow check target for `expr`'s inferred type, with the
    /// pure-`None` noise case dropped
    fn check_target(&self, expr: &Expr) -> Option<String> {
        self.types
            .soundness_check_target(expr)
            .filter(|target| target != "type(None)")
    }

    fn wrap(&mut self, range: TextRange, helper: &str, target: &str) {
        self.edits.push((
            range,
            vec![
                Fragment::Lit(format!("{helper}(")),
                Fragment::Src(range),
                Fragment::Lit(format!(", {target})")),
            ],
        ));
    }

    /// wrap the iterable of a `for` / comprehension clause when the iterable
    /// carries a generic specialization and the element (loop target) type is
    /// checkable
    fn wrap_iteration(&mut self, iterable: &Expr, target_expr: &Expr, is_async: bool) {
        if !self.types.is_specialized_generic_instance(iterable) {
            return;
        }
        let Some(target) = self.check_target(target_expr) else {
            return;
        };
        let helper = if is_async {
            self.used_aiter = true;
            "_soundness_aiter"
        } else {
            self.used_iter = true;
            "_soundness_iter"
        };
        self.wrap(iterable.range(), helper, &target);
    }
}

impl<'ast> Visitor<'ast> for Soundness<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::For(for_stmt) => {
                self.wrap_iteration(&for_stmt.iter, &for_stmt.target, for_stmt.is_async);
            }
            Stmt::AnnAssign(ann) => {
                // the annotation node's stored type is the declared type;
                // it backs two checks the value's own type can't:
                // - an explicit `Any` flowing into the declared binding
                // - a gated value whose typevar solution came from this very
                //   declaration (`a: str = t()` stores `Unknown` for `t()` —
                //   the context, not the expression, carries the claim)
                if let Some(value) = &ann.value
                    && (self.types.is_any(value)
                        || (self.gated(value) && self.check_target(value).is_none()))
                    && let Some(target) = self.check_target(&ann.annotation)
                {
                    self.wrap(value.range(), "_soundness_check", &target);
                }
            }
            // a type-alias value is a type expression; nothing in it executes
            Stmt::TypeAlias(_) => return,
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    // annotations are type positions — never wrap inside them
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}

    fn visit_comprehension(&mut self, comprehension: &'ast Comprehension) {
        self.wrap_iteration(
            &comprehension.iter,
            &comprehension.target,
            comprehension.is_async,
        );
        ruff_python_ast::visitor::walk_comprehension(self, comprehension);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::UnaryOp(unary) = expr
            && unary.op == UnaryOp::Force
        {
            // the force lowering inserts `_force_unwrap(` at the operand's
            // first byte; a check wrapped around the bare operand would land
            // its second argument inside that call. wrap the whole `expr!`
            // instead (the force edits materialize inside the passthrough)
            // and consume the operand — and any inner `!` layers — so they
            // aren't wrapped again
            let already_consumed = self.consumed.contains(&expr.range());
            let mut operand = unary.operand.as_ref();
            while let Expr::UnaryOp(inner) = operand
                && inner.op == UnaryOp::Force
            {
                self.consumed.push(operand.range());
                operand = &inner.operand;
            }
            if self.gated(operand) {
                self.consumed.push(operand.range());
                if !already_consumed && let Some(target) = self.check_target(expr) {
                    self.wrap(expr.range(), "_soundness_check", &target);
                }
            }
        } else if self.gated(expr) && !self.consumed.contains(&expr.range()) {
            if let Some(target) = self.check_target(expr) {
                self.wrap(expr.range(), "_soundness_check", &target);
            }
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct SoundnessPass {
    enabled: bool,
}

impl SoundnessPass {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            // stubs never execute, so checks would only be noise there
            enabled: config.soundness_checks && !config.is_stub,
        }
    }
}

impl TypeAwarePass for SoundnessPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if !self.enabled {
            return;
        }
        let mut inner = Soundness::new(types);
        for (idx, stmt) in stmts.iter().enumerate() {
            // an AST-mutation pass re-rendered this statement; edits into its
            // original source range would be dropped and flagged as leaks
            if ctx.changed.contains(&idx) {
                continue;
            }
            inner.visit_stmt(stmt);
        }
        if inner.edits.is_empty() {
            return;
        }
        ctx.required_imports.push(CHECK_HELPER.to_owned());
        if inner.used_iter {
            ctx.required_imports.push(ITER_HELPER.to_owned());
        }
        if inner.used_aiter {
            ctx.required_imports.push(AITER_HELPER.to_owned());
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};

    fn soundness_config() -> Config {
        Config {
            soundness_checks: true,
            ..Config::test_default()
        }
    }

    fn check(input: &str) -> String {
        transpile(input, &soundness_config()).unwrap()
    }

    #[test]
    fn generic_call_result_checked() {
        let out = check("def t[T]() -> T: ...\ndef f():\n    a: str = t()\n");
        assert!(
            out.contains("a: str = _soundness_check(t(), str)"),
            "got:\n{out}"
        );
        assert!(out.contains("def _soundness_check"), "got:\n{out}");
    }

    #[test]
    fn annotated_container_subscript_checked() {
        let out = check("def f(a: list[str]):\n    b = a[0]\n");
        assert!(
            out.contains("b = _soundness_check(a[0], str)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn concrete_call_not_checked() {
        // a fully concrete return is verified statically by ty
        let out = check("def g() -> str: ...\ndef f():\n    a = g()\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn non_generic_subscript_not_checked() {
        // str.__getitem__'s return carries no specialization claim
        let out = check("def f(s: str):\n    c = s[0]\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn dict_method_result_checked() {
        let out = check("def f(d: dict[str, int]):\n    v = d.get(\"k\")\n");
        assert!(
            out.contains("v = _soundness_check(d.get(\"k\"), (int, type(None)))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn iteration_checked() {
        let out = check("def f(a: list[str]):\n    for x in a:\n        print(x)\n");
        assert!(
            out.contains("for x in _soundness_iter(a, str):"),
            "got:\n{out}"
        );
        assert!(out.contains("def _soundness_iter"), "got:\n{out}");
    }

    #[test]
    fn comprehension_iterable_checked() {
        let out = check("def f(a: list[int]):\n    b = [x + 1 for x in a]\n");
        assert!(
            out.contains("for x in _soundness_iter(a, int)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn any_into_declared_binding_checked() {
        let out = check("def f(x: dynamic):\n    a: int = x\n");
        assert!(
            out.contains("a: int = _soundness_check(x, int)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn none_only_result_not_checked() {
        // validating a None result guards no data flowing onward
        let out = check("def f(a: list[str]):\n    a.append(\"x\")\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn store_subscript_not_wrapped() {
        let out = check("def f(a: list[str]):\n    a[0] = \"x\"\n");
        assert!(!out.contains("_soundness_check(a[0]"), "got:\n{out}");
    }

    #[test]
    fn annotation_positions_untouched() {
        // the annotation's own subscript must never be wrapped
        let out =
            check("def f(a: list[str]) -> list[int]:\n    b: dict[str, int] = {}\n    return []\n");
        assert!(!out.contains("_soundness_check(list"), "got:\n{out}");
        assert!(!out.contains("_soundness_check(dict"), "got:\n{out}");
    }

    #[test]
    fn nested_projections_compose() {
        let out = check("def f(a: list[list[str]]):\n    b = a[0][1]\n");
        assert!(
            out.contains("b = _soundness_check(_soundness_check(a[0], list)[1], str)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn unsolved_typevar_not_checked() {
        // inside the generic function T is still abstract — nothing to test
        let out = check("def first[T](xs: list[T]) -> T:\n    return xs[0]\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn user_class_target_resolves() {
        let out = check("class Box: ...\ndef t[T]() -> T: ...\ndef f():\n    b: Box = t()\n");
        assert!(
            out.contains("b: Box = _soundness_check(t(), Box)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn shadowed_builtin_skips_check() {
        let out = check("str = 1\ndef t[T]() -> T: ...\ndef f():\n    a: str = t()\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn disabled_by_default_config_off() {
        let out = transpile(
            "def f(a: list[str]):\n    b = a[0]\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn helper_injected_once() {
        let out = check("def f(a: list[str], b: list[int]):\n    x = a[0]\n    y = b[0]\n");
        assert_eq!(
            out.matches("def _soundness_check").count(),
            1,
            "got:\n{out}"
        );
    }

    #[test]
    fn composes_with_force_unwrap() {
        // the check wraps the whole `a[0]!` and validates the unwrapped type
        let out = check("def f(a: list[str | None]):\n    b = a[0]!\n");
        assert!(
            out.contains("b = _soundness_check(_force_unwrap(a[0]), str)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn generator_iteration_checked() {
        // Iterator[str] is a generic *protocol* instance — its element claim
        // is gated the same as a nominal container's
        let out = check(
            "from collections.abc import Iterator\ndef g() -> Iterator[str]:\n    yield \"a\"\ndef f():\n    for x in g():\n        print(x)\n",
        );
        assert!(
            out.contains("for x in _soundness_iter(g(), str):"),
            "got:\n{out}"
        );
    }

    #[test]
    fn async_iteration_checked() {
        let out = check(
            "from collections.abc import AsyncIterator\nasync def g() -> AsyncIterator[str]:\n    yield \"a\"\nasync def f():\n    async for x in g():\n        print(x)\n",
        );
        assert!(
            out.contains("async for x in _soundness_aiter(g(), str):"),
            "got:\n{out}"
        );
        assert!(out.contains("async def _soundness_aiter"), "got:\n{out}");
    }

    #[test]
    fn nested_force_unwrap_not_distorted() {
        // a double `!` over-unwraps (its type is no longer checkable), so no
        // check fires — and crucially no per-layer wrap distorts the
        // `_force_unwrap` nesting
        let out = check("def f(a: list[str | None]):\n    b = a[0]!!\n");
        assert!(
            out.contains("b = _force_unwrap(_force_unwrap(a[0]))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn tuple_index_projection_checked() {
        let out = check("def f(t: tuple[str, int]):\n    a = t[0]\n");
        assert!(
            out.contains("a = _soundness_check(t[0], str)"),
            "got:\n{out}"
        );
    }
}
