//! Runtime type-soundness checks.
//!
//! ty's inference is sound only up to the assumptions the type system makes.
//! Several of those assumptions are pure annotation-level claims with no
//! runtime backing, and this pass validates each one where a value crosses
//! from the unverified world into a typed slot. Each position is independently
//! toggled via [`SoundnessPositions`]:
//!
//! - **`generic_calls`** — results of calls whose type is typevar-derived: a
//!   generic function's return (`def t[T]() -> T`), or a method bound to a
//!   specialized generic instance (`d.get(k)` on a `dict[str, int]`).
//! - **`projections`** — element reads out of a specialized container
//!   (`a[0]` on an `a: list[str]`).
//! - **`iterations`** — loop / comprehension elements drawn from a specialized
//!   iterable (`for x in a:`, `[.. for x in a]`, `async for`).
//! - **`assignments`** — explicit `Any` (or a context-solved generic result)
//!   flowing into an annotated assignment target (`a: str = dyn_val`).
//! - **`returns`** — a returned value validated against the enclosing
//!   function's declared return type (`def g() -> str: return dyn_val`).
//! - **`arguments`** — a call argument validated against its matched
//!   parameter's annotation (`takes(dyn_val)` where `takes(s: str)`).
//! - **`parameters`** — a function's own parameters validated at entry, inside
//!   the body, defending its contract against callers the checker never saw
//!   (untyped / third-party code). off in the default set — it runs on every
//!   call — and inserted as body-prologue guards after any docstring.
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
//! When the target is a *user-defined* generic specialization (`A[int]`), the
//! check deepens to `_soundness_parametric(expr, A[int], variances)`, which
//! validates the base class *and* — when the value carries its reified
//! `__orig_class__`, as `A[int](…)` instances do — its type arguments, with
//! the target's declared variance. It reuses the `_parametric_is` probe from
//! [`parametric_is`](super::parametric_is). Builtin collections erase their
//! arguments at runtime, so they keep the shallow base check.
//!
//! The wraps are [`Fragment`] template edits, so sibling lowerings inside the
//! wrapped expression (coalesce, force-unwrap, generic-call stripping) are
//! materialized inside the check's passthrough span, and nested checks
//! (`t()[0]`, an argument inside a checked call) compose by template
//! recursion. Statements an AST-mutation pass re-rendered (casts, coalesce
//! chains, `typeof`) are skipped wholesale — their source ranges no longer
//! align with the output.
//!
//! The `returns`/`assignments`/`arguments` gates share a "value is unhelpful"
//! rule — the value is a plain `Any`, or a gated projection whose own type is
//! unresolved (a typevar solved only by the surrounding context) — so a value
//! that is checkable on its own is left to `generic_calls`/`projections` and
//! never wrapped twice.
//!
//! Known gaps, deliberate for now: no checks on `await` results, unpacking
//! targets, `*args`/`**kwargs` spreads, or arguments to non-function callees
//! (class constructors, overloaded functions); a check argument naming a class
//! defined later in the module can raise `NameError` if the checked line runs
//! at import time before the class body.

use std::fmt::Write as _;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Comprehension, Expr, ExprCall, Stmt, StmtFunctionDef, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::parametric_is::{PARAMETRIC_IS_RUNTIME, variance_tuple};
use super::source_util::{line_indent, line_start};
use crate::Config;
use crate::config::SoundnessPositions;
use crate::type_info::{SoundnessCheck, TypeInfo};

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

// deep check for a user-generic-specialized target: validates the base class
// always, and the reified type arguments when the value carries them
// (`__orig_class__`, stamped by `A[int](…)`). a value with no reification
// passes the argument check — its parameters aren't available to check,
// leaving the base `isinstance` as the guarantee. reuses `_parametric_is`
// (and its `_parametric_is_sub`) from `PARAMETRIC_IS_RUNTIME`
const PARAMETRIC_HELPER: &str = "\
def _soundness_parametric(_v, _alias, _variances):
    _origin = getattr(_alias, \"__origin__\", _alias)
    if not isinstance(_v, _origin):
        raise TypeError(
            f\"type soundness violation: expected {getattr(_origin, '__name__', _origin)}, \"
            f\"got {type(_v).__name__}\"
        )
    if getattr(_v, \"__orig_class__\", None) is not None and not _parametric_is(_v, _alias, _variances):
        raise TypeError(
            f\"type soundness violation: expected {_alias}, got {_v.__orig_class__}\"
        )
    return _v
";

const ITER_P_HELPER: &str = "\
def _soundness_iter_p(_it, _alias, _variances):
    for _x in _it:
        yield _soundness_parametric(_x, _alias, _variances)
";

const AITER_P_HELPER: &str = "\
async def _soundness_aiter_p(_it, _alias, _variances):
    async for _x in _it:
        yield _soundness_parametric(_x, _alias, _variances)
";

/// which parameter an argument binds to, for the `arguments` gate
enum ArgSlot<'a> {
    Positional(usize),
    Keyword(&'a str),
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent runtime-helper usage flags, not a state machine"
)]
struct Soundness<'a> {
    types: &'a dyn TypeInfo,
    positions: SoundnessPositions,
    /// the working source — needed to read a function body's indentation when
    /// inserting `parameters` entry guards
    source: &'a str,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    /// gated expressions already covered by a wrap on an enclosing `!`
    /// force-unwrap — wrapping them directly would splice the check's second
    /// argument into `_force_unwrap`'s call parens
    consumed: Vec<TextRange>,
    /// declared-return-type check plans of the enclosing functions (innermost
    /// last); `None` when a function has no annotation or an uncheckable one
    return_targets: Vec<Option<SoundnessCheck>>,
    used_iter: bool,
    used_aiter: bool,
    used_iter_p: bool,
    used_aiter_p: bool,
    /// any deep parametric check emitted — pulls in `_soundness_parametric`
    /// and the `_parametric_is` probe it reuses
    used_parametric: bool,
}

impl<'a> Soundness<'a> {
    fn new(types: &'a dyn TypeInfo, positions: SoundnessPositions, source: &'a str) -> Self {
        Self {
            types,
            positions,
            source,
            edits: Vec::new(),
            consumed: Vec::new(),
            return_targets: Vec::new(),
            used_iter: false,
            used_aiter: false,
            used_iter_p: false,
            used_aiter_p: false,
            used_parametric: false,
        }
    }

    /// whether `expr` is a value this pass classifies as resting on an
    /// unverified claim: a call whose result type came from a typevar
    /// solution, or an element read out of a specialized container. this is
    /// the position-agnostic *classification*; the direct-wrap sites also
    /// require the corresponding position to be enabled ([`Self::gated_enabled`])
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

    /// [`Self::gated`] restricted to the enabled positions: a `Call` needs
    /// `generic_calls`, a `Subscript` needs `projections`
    fn gated_enabled(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call(_) => self.positions.generic_calls && self.gated(expr),
            Expr::Subscript(_) => self.positions.projections && self.gated(expr),
            _ => false,
        }
    }

    /// whether `value` is "unhelpful" — its own inferred type doesn't back a
    /// standalone check, so a surrounding annotation (assignment / return /
    /// parameter) must supply the target: a plain `Any`, or a gated value
    /// whose type is unresolved (a typevar solved only by that context)
    fn value_needs_context_target(&self, value: &Expr) -> bool {
        self.types.is_any(value) || (self.gated(value) && self.check_plan(value).is_none())
    }

    /// the check plan for `expr`'s inferred type, with the pure-`None`
    /// isinstance noise case dropped (validating a `None` result guards nothing)
    fn check_plan(&self, expr: &Expr) -> Option<SoundnessCheck> {
        self.types
            .soundness_check_plan(expr)
            .filter(|plan| !matches!(plan, SoundnessCheck::Isinstance(t) if t == "type(None)"))
    }

    /// wrap `source[range]` in `helper(<source>, <trailing-args>)`. `trailing`
    /// carries its own leading `, ` (e.g. `", str"` or `", A[int], (0,)"`)
    fn wrap_call(&mut self, range: TextRange, helper: &str, trailing: &str) {
        self.edits.push((
            range,
            vec![
                Fragment::Lit(format!("{helper}(")),
                Fragment::Src(range),
                Fragment::Lit(format!("{trailing})")),
            ],
        ));
    }

    /// wrap `range` in the scalar check (`_soundness_check` /
    /// `_soundness_parametric`) named by `plan`
    fn wrap_check(&mut self, range: TextRange, plan: &SoundnessCheck) {
        match plan {
            SoundnessCheck::Isinstance(target) => {
                self.wrap_call(range, "_soundness_check", &format!(", {target}"));
            }
            SoundnessCheck::Parametric { alias, variances } => {
                self.used_parametric = true;
                self.wrap_call(
                    range,
                    "_soundness_parametric",
                    &format!(", {alias}, {}", variance_tuple(variances)),
                );
            }
        }
    }

    /// wrap the iterable of a `for` / comprehension clause when the iterable
    /// carries a generic specialization and the element (loop target) type is
    /// checkable. the iterable is wrapped in a validating generator whose form
    /// (shallow vs parametric) follows the element's check plan
    fn wrap_iteration(&mut self, iterable: &Expr, target_expr: &Expr, is_async: bool) {
        if !self.types.is_specialized_generic_instance(iterable) {
            return;
        }
        let Some(plan) = self.check_plan(target_expr) else {
            return;
        };
        let (helper, trailing) = match &plan {
            SoundnessCheck::Isinstance(target) => {
                let helper = if is_async {
                    self.used_aiter = true;
                    "_soundness_aiter"
                } else {
                    self.used_iter = true;
                    "_soundness_iter"
                };
                (helper, format!(", {target}"))
            }
            SoundnessCheck::Parametric { alias, variances } => {
                self.used_parametric = true;
                let helper = if is_async {
                    self.used_aiter_p = true;
                    "_soundness_aiter_p"
                } else {
                    self.used_iter_p = true;
                    "_soundness_iter_p"
                };
                (helper, format!(", {alias}, {}", variance_tuple(variances)))
            }
        };
        self.wrap_call(iterable.range(), helper, &trailing);
    }

    /// wrap each argument of `call` whose own type is unhelpful against its
    /// matched parameter's annotation. positional mapping stops at the first
    /// starred spread (positions past it are unknown); `**kwargs` spreads are
    /// skipped
    fn wrap_call_arguments(&mut self, call: &ExprCall) {
        let callee = call.func.as_ref();
        for (index, arg) in call.arguments.args.iter().enumerate() {
            if arg.is_starred_expr() {
                break;
            }
            self.maybe_wrap_argument(callee, arg, &ArgSlot::Positional(index));
        }
        for keyword in &call.arguments.keywords {
            if let Some(name) = &keyword.arg {
                self.maybe_wrap_argument(callee, &keyword.value, &ArgSlot::Keyword(name.as_str()));
            }
        }
    }

    fn maybe_wrap_argument(&mut self, callee: &Expr, arg: &Expr, slot: &ArgSlot<'_>) {
        if self.consumed.contains(&arg.range()) || !self.value_needs_context_target(arg) {
            return;
        }
        let plan = match *slot {
            ArgSlot::Positional(index) => self.types.call_positional_param_plan(callee, index),
            ArgSlot::Keyword(name) => self.types.call_keyword_param_plan(callee, name),
        };
        if let Some(plan) = plan {
            self.wrap_check(arg.range(), &plan);
        }
    }

    /// the plan to validate a returned `value` against — the enclosing
    /// function's declared return plan — but only when `value`'s own type
    /// is unhelpful (else `generic_calls`/`projections` already covers it)
    fn return_wrap_plan(&self, value: &Expr) -> Option<SoundnessCheck> {
        let plan = self.return_targets.last()?.clone()?;
        self.value_needs_context_target(value).then_some(plan)
    }

    /// the guard statement that validates parameter `name` against `plan`
    fn guard_stmt(&mut self, name: &str, plan: &SoundnessCheck) -> String {
        match plan {
            SoundnessCheck::Isinstance(target) => format!("_soundness_check({name}, {target})"),
            SoundnessCheck::Parametric { alias, variances } => {
                self.used_parametric = true;
                format!(
                    "_soundness_parametric({name}, {alias}, {})",
                    variance_tuple(variances)
                )
            }
        }
    }

    /// insert entry guards validating each annotated, checkable parameter of
    /// `func` at the top of its body — the `parameters` position, defending
    /// the contract against callers the checker never saw. variadic
    /// (`*args` / `**kwargs`) and unannotated parameters, and those whose type
    /// has no runtime test, are skipped
    fn insert_param_guards(&mut self, func: &StmtFunctionDef) {
        let params = &func.parameters;
        let mut guards: Vec<String> = Vec::new();
        for pwd in params
            .posonlyargs
            .iter()
            .chain(&params.args)
            .chain(&params.kwonlyargs)
        {
            let parameter = &pwd.parameter;
            if let Some(annotation) = &parameter.annotation
                && let Some(plan) = self.check_plan(annotation)
            {
                let guard = self.guard_stmt(parameter.name.as_str(), &plan);
                guards.push(guard);
            }
        }
        if guards.is_empty() {
            return;
        }

        // insert after a leading docstring (which must stay the first
        // statement); mirrors `mutable_defaults`' body-prologue insertion
        let docstring_count = usize::from(matches!(
            func.body.first(),
            Some(Stmt::Expr(e)) if matches!(e.value.as_ref(), Expr::StringLiteral(_))
        ));
        let mut text = String::new();
        let insert_at = if let Some(stmt) = func.body.get(docstring_count) {
            let at = stmt.range().start();
            let prefix = &self.source[usize::from(line_start(self.source, at))..usize::from(at)];
            if prefix.trim().is_empty() {
                // multi-line body: each guard sits at the body indent and
                // re-establishes it for the statement that follows
                for guard in &guards {
                    let _ = write!(text, "{guard}\n{prefix}");
                }
            } else {
                // single-line body (`def f(a: A[int]): ...`) — break the body
                // onto its own indented line after the guards
                let base = format!("{}    ", line_indent(self.source, func.range().start()));
                for guard in &guards {
                    let _ = write!(text, "\n{base}{guard}");
                }
                let _ = write!(text, "\n{base}");
            }
            at
        } else {
            // docstring-only body: append the guards after it
            let base = format!("{}    ", line_indent(self.source, func.range().start()));
            for guard in &guards {
                let _ = write!(text, "\n{base}{guard}");
            }
            func.body[docstring_count - 1].range().end()
        };
        self.edits
            .push((TextRange::empty(insert_at), vec![Fragment::Lit(text)]));
    }
}

impl<'ast> Visitor<'ast> for Soundness<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(func) => {
                if self.positions.parameters {
                    self.insert_param_guards(func);
                }
                // track the declared return plan so nested `return`s validate
                // against the right function's annotation
                let plan = func
                    .returns
                    .as_deref()
                    .and_then(|returns| self.check_plan(returns));
                self.return_targets.push(plan);
                walk_stmt(self, stmt);
                self.return_targets.pop();
                return;
            }
            Stmt::For(for_stmt) => {
                if self.positions.iterations {
                    self.wrap_iteration(&for_stmt.iter, &for_stmt.target, for_stmt.is_async);
                }
            }
            Stmt::AnnAssign(ann) => {
                // the annotation node's stored type is the declared type;
                // it backs two checks the value's own type can't:
                // - an explicit `Any` flowing into the declared binding
                // - a gated value whose typevar solution came from this very
                //   declaration (`a: str = t()` stores `Unknown` for `t()` —
                //   the context, not the expression, carries the claim)
                if self.positions.assignments
                    && let Some(value) = &ann.value
                    && self.value_needs_context_target(value)
                    && let Some(plan) = self.check_plan(&ann.annotation)
                {
                    self.wrap_check(value.range(), &plan);
                }
            }
            Stmt::Return(ret) => {
                if self.positions.returns
                    && let Some(value) = &ret.value
                    && let Some(plan) = self.return_wrap_plan(value)
                {
                    self.wrap_check(value.range(), &plan);
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
        if self.positions.iterations {
            self.wrap_iteration(
                &comprehension.iter,
                &comprehension.target,
                comprehension.is_async,
            );
        }
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
            if self.gated_enabled(operand) {
                self.consumed.push(operand.range());
                if !already_consumed && let Some(plan) = self.check_plan(expr) {
                    self.wrap_check(expr.range(), &plan);
                }
            }
        } else if self.gated_enabled(expr)
            && !self.consumed.contains(&expr.range())
            && let Some(plan) = self.check_plan(expr)
        {
            self.wrap_check(expr.range(), &plan);
        }
        if self.positions.arguments
            && let Expr::Call(call) = expr
        {
            self.wrap_call_arguments(call);
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct SoundnessPass<'src> {
    positions: SoundnessPositions,
    source: &'src str,
}

impl<'src> SoundnessPass<'src> {
    pub(crate) fn new(source: &'src str, config: &Config) -> Self {
        Self {
            // stubs never execute, so checks would only be noise there
            positions: if config.is_stub {
                SoundnessPositions::none()
            } else {
                config.soundness
            },
            source,
        }
    }
}

impl TypeAwarePass for SoundnessPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if !self.positions.any() {
            return;
        }
        let mut inner = Soundness::new(types, self.positions, self.source);
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
        // a deep parametric check reuses the `_parametric_is` probe (which
        // brings its own `_parametric_is_sub`); function names resolve at call
        // time, so the def order among these preamble helpers is irrelevant
        if inner.used_parametric {
            ctx.required_imports.push(PARAMETRIC_IS_RUNTIME.to_owned());
            ctx.required_imports.push(PARAMETRIC_HELPER.to_owned());
        }
        if inner.used_iter_p {
            ctx.required_imports.push(ITER_P_HELPER.to_owned());
        }
        if inner.used_aiter_p {
            ctx.required_imports.push(AITER_P_HELPER.to_owned());
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};

    fn soundness_config() -> Config {
        // the inference-gap positions (no `parameters` entry checks — those are
        // opt-in and exercised by their own tests via `check_with`)
        Config {
            soundness: crate::SoundnessPositions::defaults(),
            ..Config::test_default()
        }
    }

    fn check(input: &str) -> String {
        transpile(input, &soundness_config()).unwrap()
    }

    /// transpile with only the named positions enabled (rest off)
    fn check_with(input: &str, positions: crate::SoundnessPositions) -> String {
        transpile(
            input,
            &Config {
                soundness: positions,
                ..Config::test_default()
            },
        )
        .unwrap()
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

    // ── return position ──────────────────────────────────────────────────

    #[test]
    fn return_of_any_checked_against_declared() {
        let out = check("def f() -> dynamic:\n    return 1\ndef g() -> str:\n    return f()\n");
        assert!(
            out.contains("return _soundness_check(f(), str)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn return_context_solved_generic_checked() {
        // `return t()` in a `-> str` function: t()'s own type is unresolved,
        // the return annotation supplies the target
        let out = check("def t[T]() -> T: ...\ndef g() -> str:\n    return t()\n");
        assert!(
            out.contains("return _soundness_check(t(), str)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn return_without_annotation_not_checked() {
        let out = check("def f() -> dynamic:\n    return 1\ndef g():\n    return f()\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn return_of_concrete_value_not_checked() {
        // a concrete return is verified statically; no runtime guard needed
        let out = check("def g() -> str:\n    return \"ok\"\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn nested_function_returns_use_own_annotation() {
        let out = check(
            "def f() -> dynamic:\n    return 1\ndef outer() -> int:\n    def inner() -> str:\n        return f()\n    return f()\n",
        );
        assert!(
            out.contains("return _soundness_check(f(), str)"),
            "inner should check against str, got:\n{out}"
        );
        assert!(
            out.contains("return _soundness_check(f(), int)"),
            "outer should check against int, got:\n{out}"
        );
    }

    #[test]
    fn return_projection_checked_once_at_expr() {
        // `return a[0]` on `list[str]` in a `-> str` function: the projection
        // gate already validates against str, the return gate defers
        let out = check("def g(a: list[str]) -> str:\n    return a[0]\n");
        assert_eq!(
            out.matches("_soundness_check(a[0]").count(),
            1,
            "single wrap, got:\n{out}"
        );
    }

    // ── argument position ────────────────────────────────────────────────

    #[test]
    fn any_argument_checked_against_param() {
        let out = check("def f() -> dynamic:\n    return 1\ndef takes(s: str): ...\ntakes(f())\n");
        assert!(
            out.contains("takes(_soundness_check(f(), str))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn keyword_argument_checked_against_param() {
        let out =
            check("def f() -> dynamic:\n    return 1\ndef takes(s: str): ...\ntakes(s=f())\n");
        assert!(
            out.contains("takes(s=_soundness_check(f(), str))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn method_argument_checked_against_param() {
        // bound-method signature drops `self`, so index 0 is the first user arg
        let out = check(
            "def f() -> dynamic:\n    return 1\nclass C:\n    def m(self, s: str): ...\ndef g(c: C):\n    c.m(f())\n",
        );
        assert!(
            out.contains("c.m(_soundness_check(f(), str))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn concrete_argument_not_checked() {
        let out = check("def takes(s: str): ...\ntakes(\"ok\")\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn unannotated_param_argument_not_checked() {
        let out = check("def f() -> dynamic:\n    return 1\ndef takes(s): ...\ntakes(f())\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn variadic_argument_not_checked() {
        // `*args: str` describes each element, not the tuple as passed — an
        // isinstance against str would be wrong, so it's skipped
        let out =
            check("def f() -> dynamic:\n    return 1\ndef takes(*args: str): ...\ntakes(f())\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn starred_argument_stops_positional_mapping() {
        // a `*spread` makes later positions unknown; nothing after it is mapped
        let out = check(
            "def f() -> dynamic:\n    return 1\ndef takes(a: int, b: str): ...\nxs = [1]\ntakes(*xs, f())\n",
        );
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn argument_inside_checked_call_composes() {
        // `t(f())`: outer generic call checked, inner Any arg checked, balanced
        let out = check(
            "def t[T](x: str) -> T: ...\ndef f() -> dynamic:\n    return 1\ndef g():\n    a: int = t(f())\n",
        );
        let opens = out.matches('(').count();
        let closes = out.matches(')').count();
        assert_eq!(opens, closes, "unbalanced parens:\n{out}");
        assert!(
            out.contains("_soundness_check(f(), str)"),
            "inner arg checked, got:\n{out}"
        );
    }

    // ── granular gating ──────────────────────────────────────────────────

    #[test]
    fn only_returns_position_enabled() {
        let src = "def f() -> dynamic:\n    return 1\ndef g() -> str:\n    return f()\ndef h(a: list[str]):\n    b = a[0]\n";
        let out = check_with(
            src,
            crate::SoundnessPositions {
                returns: true,
                ..crate::SoundnessPositions::none()
            },
        );
        assert!(
            out.contains("return _soundness_check(f(), str)"),
            "return checked, got:\n{out}"
        );
        assert!(
            !out.contains("_soundness_check(a[0]"),
            "projection must stay off, got:\n{out}"
        );
    }

    #[test]
    fn only_projections_position_enabled() {
        let src = "def f() -> dynamic:\n    return 1\ndef g() -> str:\n    return f()\ndef h(a: list[str]):\n    b = a[0]\n";
        let out = check_with(
            src,
            crate::SoundnessPositions {
                projections: true,
                ..crate::SoundnessPositions::none()
            },
        );
        assert!(
            out.contains("_soundness_check(a[0], str)"),
            "projection checked, got:\n{out}"
        );
        assert!(
            !out.contains("return _soundness_check"),
            "returns must stay off, got:\n{out}"
        );
    }

    #[test]
    fn arguments_disabled_leaves_calls_bare() {
        let out = check_with(
            "def f() -> dynamic:\n    return 1\ndef takes(s: str): ...\ntakes(f())\n",
            crate::SoundnessPositions {
                arguments: false,
                ..crate::SoundnessPositions::defaults()
            },
        );
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    // ── deep parametric checks (user-generic specializations) ────────────

    const GENERIC: &str = "class A[T]:\n    t: T = None  # type: ignore\n";

    #[test]
    fn dynamic_into_user_generic_param_deep_checked() {
        // a value crossing into an `A[int]` parameter is validated against the
        // full specialization via `_parametric_is` on `__orig_class__`
        let out = check(&format!(
            "{GENERIC}def f(a: A[int]): ...\ndef g(x: dynamic):\n    f(x)\n"
        ));
        assert!(
            out.contains("f(_soundness_parametric(x, A[int], (0,)))"),
            "got:\n{out}"
        );
        assert!(
            out.contains("def _soundness_parametric"),
            "parametric helper injected: {out}"
        );
        assert!(
            out.contains("def _parametric_is("),
            "reused probe injected: {out}"
        );
    }

    #[test]
    fn dynamic_into_user_generic_assignment_deep_checked() {
        let out = check(&format!("{GENERIC}def g(x: dynamic):\n    a: A[int] = x\n"));
        assert!(
            out.contains("a: A[int] = _soundness_parametric(x, A[int], (0,))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn return_of_dynamic_as_user_generic_deep_checked() {
        let out = check(&format!(
            "{GENERIC}def g(x: dynamic) -> A[int]:\n    return x\n"
        ));
        assert!(
            out.contains("return _soundness_parametric(x, A[int], (0,))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn builtin_generic_stays_shallow() {
        // builtin collections erase their type arguments at runtime, so there
        // is nothing to probe — the check stays a shallow `isinstance`
        let out = check("def f(a: list[str]):\n    b = a[0]\n");
        assert!(
            out.contains("b = _soundness_check(a[0], str)"),
            "got:\n{out}"
        );
        assert!(!out.contains("_soundness_parametric"), "got:\n{out}");
    }

    #[test]
    fn covariant_param_emits_variance_code() {
        // an `out T` parameter carries variance code 1, so the runtime match
        // respects covariance
        let out = check(
            "class A[out T]:\n    def __init__(self): ...\ndef f(a: A[int]): ...\ndef g(x: dynamic):\n    f(x)\n",
        );
        assert!(
            out.contains("_soundness_parametric(x, A[int], (1,))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn multi_param_generic_renders_all_variances() {
        // both parameters are stored in invariant fields, so both carry code 0
        let out = check(
            "class P[K, V]:\n    k: K = None  # type: ignore\n    v: V = None  # type: ignore\ndef f(a: P[str, int]): ...\ndef g(x: dynamic):\n    f(x)\n",
        );
        assert!(
            out.contains("_soundness_parametric(x, P[str, int], (0, 0))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn parametric_iteration_uses_deep_generator() {
        // iterating a list of a user generic validates each element deeply
        let out = check(&format!(
            "{GENERIC}def g(xs: list[A[int]]):\n    for a in xs:\n        print(a)\n"
        ));
        assert!(
            out.contains("for a in _soundness_iter_p(xs, A[int], (0,)):"),
            "got:\n{out}"
        );
        assert!(
            out.contains("def _soundness_iter_p"),
            "parametric iter helper injected: {out}"
        );
    }

    #[test]
    fn unrelated_shallow_check_omits_parametric_helper() {
        // a file with only shallow checks must not pull in the parametric
        // runtime
        let out = check("def f(a: list[str]):\n    b = a[0]\n");
        assert!(!out.contains("_parametric_is"), "got:\n{out}");
        assert!(!out.contains("_soundness_parametric"), "got:\n{out}");
    }

    // ── parameter-entry checks (defensive, opt-in) ───────────────────────

    fn params_only() -> crate::SoundnessPositions {
        crate::SoundnessPositions {
            parameters: true,
            ..crate::SoundnessPositions::none()
        }
    }

    #[test]
    fn parameters_off_by_default() {
        // the default set does not include the defensive entry checks
        let out = check("def f(a: list[str]): ...\n");
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn shallow_param_guarded_at_entry() {
        let out = check_with("def f(s: str, n: int): ...\n", params_only());
        assert!(out.contains("_soundness_check(s, str)"), "got:\n{out}");
        assert!(out.contains("_soundness_check(n, int)"), "got:\n{out}");
    }

    #[test]
    fn user_generic_param_deep_guarded_at_entry() {
        let out = check_with(
            "class A[T]:\n    t: T = None  # type: ignore\ndef f(a: A[int]): ...\n",
            params_only(),
        );
        assert!(
            out.contains("_soundness_parametric(a, A[int], (0,))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn entry_guard_follows_docstring() {
        // the docstring must stay the first statement; the guard goes after it
        let out = check_with("def f(s: str):\n    \"doc\"\n    return s\n", params_only());
        let doc = out.find("\"doc\"").expect("docstring present");
        let guard = out.find("_soundness_check(s, str)").expect("guard present");
        assert!(doc < guard, "guard must follow the docstring:\n{out}");
    }

    #[test]
    fn single_line_body_param_guarded() {
        let out = check_with("def f(s: str): return s\n", params_only());
        assert!(out.contains("_soundness_check(s, str)"), "got:\n{out}");
        assert!(
            transpile(
                "def f(s: str): return s\n",
                &Config {
                    soundness: params_only(),
                    ..Config::test_default()
                }
            )
            .is_ok(),
            "output must be valid: {out}"
        );
    }

    #[test]
    fn unannotated_and_variadic_params_skipped() {
        let out = check_with("def f(x, *args: int, **kw: str): ...\n", params_only());
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn untestable_param_type_skipped() {
        // a Callable parameter has no shallow runtime test
        let out = check_with(
            "from typing import Callable\ndef f(cb: Callable[[], int]): ...\n",
            params_only(),
        );
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
    }

    #[test]
    fn self_param_skipped() {
        // `self` is unannotated, so it is naturally skipped
        let out = check_with("class C:\n    def m(self, s: str): ...\n", params_only());
        assert!(out.contains("_soundness_check(s, str)"), "got:\n{out}");
        assert!(!out.contains("_soundness_check(self"), "got:\n{out}");
    }

    #[test]
    fn entry_guard_composes_with_return_check() {
        // both a param guard (entry) and a return check apply in one function
        let out = check_with(
            "def f() -> dynamic:\n    return 1\ndef g(s: str) -> str:\n    return f()\n",
            crate::SoundnessPositions {
                parameters: true,
                returns: true,
                ..crate::SoundnessPositions::none()
            },
        );
        assert!(
            out.contains("_soundness_check(s, str)"),
            "entry guard: {out}"
        );
        assert!(
            out.contains("return _soundness_check(f(), str)"),
            "return check: {out}"
        );
    }

    #[test]
    fn all_includes_parameters() {
        let out = check_with("def f(s: str): ...\n", crate::SoundnessPositions::all());
        assert!(out.contains("_soundness_check(s, str)"), "got:\n{out}");
    }
}
