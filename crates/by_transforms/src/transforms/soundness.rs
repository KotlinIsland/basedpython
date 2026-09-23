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
//! recursion. In a top-level statement an AST-mutation pass rewrote (a
//! coalesce chain, `typeof`, a repeated `_` parameter) the checks are written
//! into the syntax tree instead — see [`place_in_rerendered`]: a check made at a
//! node the pass replaced has no source left to land on as a text edit, and
//! written into the tree it is either placed or reported.
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

use std::cell::Cell;
use std::collections::HashMap;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::transformer::{self, Transformer};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    AtomicNodeIndex, Comprehension, Expr, ExprCall, ExprName, HasNodeIndex, Parameter, Stmt,
    StmtFunctionDef, UnaryOp,
};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::types::soundness::CheckTarget;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::parametric_is::variance_tuple;
use super::repeated_underscore::WrittenNames;
use super::source_util::{PrologueStatement, body_prologue, needs_parentheses_as_argument};
use crate::Config;
use crate::config::SoundnessPositions;
use crate::type_info::{SoundnessCheck, TypeInfo};

// deep check for a user-generic-specialized target: validates the base class
// always, and the reified type arguments when the value carries them
// (`__orig_class__`, stamped by `A[int](…)`). a value with no reification
// passes the argument check — its parameters aren't available to check,
// leaving the base `isinstance` as the guarantee. reuses `_parametric_is`
// (and its `_parametric_is_sub`) from `PARAMETRIC_IS_RUNTIME`

/// which parameter an argument binds to, for the `arguments` gate
enum ArgSlot<'a> {
    Positional(usize),
    Keyword(&'a str),
}

/// one check the pass decided on, before anything is written for it
enum Site<'ast> {
    /// the value `expr` produces is checked as it is produced
    Value {
        expr: &'ast Expr,
        plan: SoundnessCheck,
    },
    /// each element drawn from `iterable` is checked as it is drawn
    Elements {
        iterable: &'ast Expr,
        is_async: bool,
        plan: SoundnessCheck,
    },
    /// `function`'s own parameters are checked where its body begins
    Parameters {
        function: &'ast StmtFunctionDef,
        /// each checked parameter by the name python binds it to, which for a repeated
        /// `_` is not the one the source spells
        guards: Vec<(Name, SoundnessCheck)>,
    },
}

/// where the checks go, decided once over a syntax tree, and what is not a place
/// for one
struct Soundness<'a, 'ast> {
    types: &'a dyn TypeInfo,
    positions: SoundnessPositions,
    /// the module as written, which a repeated `_` parameter's name is numbered around
    written: WrittenNames<'a>,
    sites: Vec<Site<'ast>>,
    /// gated expressions already covered by a wrap on an enclosing `!`
    /// force-unwrap — wrapping them directly would splice the check's second
    /// argument into `_force_unwrap`'s call parens
    consumed: Vec<TextRange>,
    /// declared-return-type check plans of the enclosing functions (innermost
    /// last); `None` when a function has no annotation or an uncheckable one
    return_targets: Vec<Option<SoundnessCheck>>,
}

impl<'a, 'ast> Soundness<'a, 'ast> {
    fn new(
        types: &'a dyn TypeInfo,
        positions: SoundnessPositions,
        written: WrittenNames<'a>,
    ) -> Self {
        Self {
            types,
            positions,
            written,
            sites: Vec::new(),
            consumed: Vec::new(),
            return_targets: Vec::new(),
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
            .filter(|plan| !matches!(plan, SoundnessCheck::Isinstance(CheckTarget::NoneType)))
    }

    /// [`Self::check_plan`] for a parameter, read off the parameter rather than off
    /// its annotation — which is the only way to see a type the source stated with a
    /// default instead of with an annotation
    fn parameter_plan(&self, parameter: &Parameter) -> Option<SoundnessCheck> {
        self.types
            .parameter_check_plan(parameter)
            .filter(|plan| !matches!(plan, SoundnessCheck::Isinstance(CheckTarget::NoneType)))
    }

    /// check the value `expr` produces against `plan`
    fn check_value(&mut self, expr: &'ast Expr, plan: SoundnessCheck) {
        self.sites.push(Site::Value { expr, plan });
    }

    /// check each element drawn from the iterable of a `for` / comprehension
    /// clause when the iterable carries a generic specialization and the element
    /// (loop target) type is checkable
    fn check_iteration(&mut self, iterable: &'ast Expr, target_expr: &Expr, is_async: bool) {
        if !self.types.is_specialized_generic_instance(iterable) {
            return;
        }
        let Some(plan) = self.check_plan(target_expr) else {
            return;
        };
        self.sites.push(Site::Elements {
            iterable,
            is_async,
            plan,
        });
    }

    /// check each argument of `call` whose own type is unhelpful against its
    /// matched parameter's annotation. positional mapping stops at the first
    /// starred spread (positions past it are unknown); `**kwargs` spreads are
    /// skipped
    fn check_call_arguments(&mut self, call: &'ast ExprCall) {
        let callee = call.func.as_ref();
        for (index, arg) in call.arguments.args.iter().enumerate() {
            if arg.is_starred_expr() {
                break;
            }
            self.maybe_check_argument(callee, arg, &ArgSlot::Positional(index));
        }
        for keyword in &call.arguments.keywords {
            if let Some(name) = &keyword.arg {
                self.maybe_check_argument(callee, &keyword.value, &ArgSlot::Keyword(name.as_str()));
            }
        }
    }

    fn maybe_check_argument(&mut self, callee: &Expr, arg: &'ast Expr, slot: &ArgSlot<'_>) {
        if self.consumed.contains(&arg.range()) || !self.value_needs_context_target(arg) {
            return;
        }
        let plan = match *slot {
            ArgSlot::Positional(index) => self.types.call_positional_param_plan(callee, index),
            ArgSlot::Keyword(name) => self.types.call_keyword_param_plan(callee, name),
        };
        if let Some(plan) = plan {
            self.check_value(arg, plan);
        }
    }

    /// the plan to validate a returned `value` against — the enclosing
    /// function's declared return plan — but only when `value`'s own type
    /// is unhelpful (else `generic_calls`/`projections` already covers it)
    fn return_plan(&self, value: &Expr) -> Option<SoundnessCheck> {
        let plan = self.return_targets.last()?.clone()?;
        self.value_needs_context_target(value).then_some(plan)
    }

    /// check each checkable parameter of `func` where its body begins — the
    /// `parameters` position, defending the contract against callers the checker
    /// never saw. variadic (`*args` / `**kwargs`) parameters are skipped, and so is
    /// any parameter whose source states no type: an unannotated one with no
    /// default states nothing, and `x=None` says the argument may be left out rather
    /// than that `None` belongs there
    ///
    /// the plan is read off the *parameter*, not off its annotation, because a
    /// default is a written type too. `def f(safe='/')` says `safe` is a `str`
    /// — the native backend lays the parameter out at that bound and checks it
    /// at the boundary, and asking the annotation node left the interpreted twin
    /// silently more permissive than its own compiled form
    fn check_parameters(&mut self, func: &'ast StmtFunctionDef) {
        let params = &func.parameters;
        let mut guards = Vec::new();
        for pwd in params
            .posonlyargs
            .iter()
            .chain(&params.args)
            .chain(&params.kwonlyargs)
        {
            let parameter = &pwd.parameter;
            if let Some(plan) = self.parameter_plan(parameter) {
                guards.push((
                    crate::python_parameter_name(params, parameter, self.written),
                    plan,
                ));
            }
        }
        if !guards.is_empty() {
            self.sites.push(Site::Parameters {
                function: func,
                guards,
            });
        }
    }
}

impl<'ast> Visitor<'ast> for Soundness<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(func) => {
                if self.positions.parameters {
                    self.check_parameters(func);
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
                    self.check_iteration(&for_stmt.iter, &for_stmt.target, for_stmt.is_async);
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
                    // a field-specifier RHS (`a: int = Field()`) is modelled as
                    // the field type but is a descriptor object at runtime, so a
                    // soundness check against the annotation would always fail
                    && !self.types.is_field_specifier(value)
                    && let Some(plan) = self.check_plan(&ann.annotation)
                {
                    self.check_value(value, plan);
                }
            }
            Stmt::Return(ret) => {
                if self.positions.returns
                    && let Some(value) = &ret.value
                    && let Some(plan) = self.return_plan(value)
                {
                    self.check_value(value, plan);
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
            self.check_iteration(
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
                    self.check_value(expr, plan);
                }
            }
        } else if self.gated_enabled(expr)
            && !self.consumed.contains(&expr.range())
            && let Some(plan) = self.check_plan(expr)
        {
            self.check_value(expr, plan);
        }
        if self.positions.arguments
            && let Expr::Call(call) = expr
        {
            self.check_call_arguments(call);
        }
        walk_expr(self, expr);
    }
}

/// a syntax node, as the checks a module makes are looked up by
///
/// the range alone is not enough: a node the lowering synthesised carries a range
/// it borrowed and no index, and must never be mistaken for the source node it
/// stands in for
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SiteKey {
    range: TextRange,
    index: u32,
}

impl SiteKey {
    fn of(node: &(impl Ranged + HasNodeIndex)) -> Option<Self> {
        let index = node.node_index().load().as_u32()?;
        Some(Self {
            range: node.range(),
            index,
        })
    }
}

/// every runtime soundness check a module's transpiled form makes, keyed by the
/// syntax node it is made at
///
/// the transpiler writes each one into the program it emits, and a native build of
/// the same module asks this for the same answers — so the two builds check the
/// same values against the same types, under the same `--soundness` positions
#[derive(Debug, Default)]
pub struct SoundnessSites {
    values: HashMap<SiteKey, SoundnessCheck>,
    elements: HashMap<SiteKey, SoundnessCheck>,
    parameters: HashMap<SiteKey, Vec<(String, SoundnessCheck)>>,
}

impl SoundnessSites {
    /// the check made on the value `expr` produces, as it is produced
    pub fn value(&self, expr: &Expr) -> Option<&SoundnessCheck> {
        self.values.get(&SiteKey::of(expr)?)
    }

    /// the check made on each element drawn from `iterable`, as it is drawn
    pub fn elements(&self, iterable: &Expr) -> Option<&SoundnessCheck> {
        self.elements.get(&SiteKey::of(iterable)?)
    }

    /// the checks made on `function`'s own parameters where its body begins, by the
    /// name python binds each parameter to, in the order the parameters are written
    pub fn parameters(&self, function: &StmtFunctionDef) -> &[(String, SoundnessCheck)] {
        SiteKey::of(function)
            .and_then(|key| self.parameters.get(&key))
            .map_or(&[], Vec::as_slice)
    }
}

/// decide every soundness check `suite` makes under `positions`. `written` is the
/// module `suite` is parsed from, as it was written
pub fn soundness_sites(
    model: &ty_python_semantic::SemanticModel<'_>,
    written: WrittenNames,
    suite: &[Stmt],
    positions: SoundnessPositions,
) -> SoundnessSites {
    let mut sites = SoundnessSites::default();
    if !positions.any() {
        return sites;
    }
    let mut walker = Soundness::new(model, positions, written);
    walker.visit_body(suite);
    for site in walker.sites {
        match site {
            Site::Value { expr, plan } => {
                if let Some(key) = SiteKey::of(expr) {
                    sites.values.insert(key, plan);
                }
            }
            Site::Elements { iterable, plan, .. } => {
                if let Some(key) = SiteKey::of(iterable) {
                    sites.elements.insert(key, plan);
                }
            }
            Site::Parameters { function, guards } => {
                if let Some(key) = SiteKey::of(function) {
                    let guards = guards
                        .into_iter()
                        .map(|(name, plan)| (name.to_string(), plan))
                        .collect();
                    sites.parameters.insert(key, guards);
                }
            }
        }
    }
    sites
}

/// One entry guard written at the top of a body. The guard is a call on a line
/// of its own and never continues onto a second one, so it re-establishes no
/// indentation
struct EntryGuard(String);

impl PrologueStatement for EntryGuard {
    fn push(&self, frags: &mut Vec<Fragment>, _indent: &str) {
        frags.push(Fragment::Lit(self.0.clone()));
    }
}

/// what the checks a pass decided on are written as
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent runtime-helper usage flags, not a state machine"
)]
struct Rendered {
    /// each value passed through a call: the span it is at, the text opening the call
    /// and the text closing it
    wraps: Vec<(TextRange, String, String)>,
    /// the entry-guard suites, anchored at the body statement they precede
    guards: Vec<(TextSize, Vec<Fragment>)>,
    /// functions whose body holds nothing the source wrote, so a guard has no
    /// position to anchor to
    unanchored: Vec<String>,
    used_iter: bool,
    used_aiter: bool,
    used_iter_p: bool,
    used_aiter_p: bool,
    /// any deep parametric check emitted — pulls in `_soundness_parametric`
    /// and the `_parametric_is` probe it reuses
    used_parametric: bool,
}

impl Rendered {
    /// wrap `expr` in `helper(<expr>, <trailing-args>)`. `trailing`
    /// carries its own leading `, ` (e.g. `", str"` or `", A[int], (0,)"`)
    fn wrap_call(&mut self, expr: &Expr, helper: &str, trailing: &str) {
        let (open, close) = if needs_parentheses_as_argument(expr) {
            ("(", ")")
        } else {
            ("", "")
        };
        self.wraps.push((
            expr.range(),
            format!("{helper}({open}"),
            format!("{close}{trailing})"),
        ));
    }

    /// wrap `expr` in the scalar check (`_soundness_check` /
    /// `_soundness_parametric`) named by `plan`
    fn wrap_check(&mut self, expr: &Expr, plan: &SoundnessCheck) {
        match plan {
            SoundnessCheck::Isinstance(target) => {
                self.wrap_call(expr, "_soundness_check", &format!(", {target}"));
            }
            SoundnessCheck::Parametric { alias, variances } => {
                self.used_parametric = true;
                self.wrap_call(
                    expr,
                    "_soundness_parametric",
                    &format!(", {alias}, {}", variance_tuple(variances)),
                );
            }
        }
    }

    /// wrap an iterable in a validating generator whose form (shallow vs
    /// parametric) follows the element's check plan
    fn wrap_iteration(&mut self, iterable: &Expr, is_async: bool, plan: &SoundnessCheck) {
        let (helper, trailing) = match plan {
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
        self.wrap_call(iterable, helper, &trailing);
    }

    /// the guard statements that validate each of `checks`, in order
    fn guard_stmts(&mut self, checks: &[(Name, SoundnessCheck)]) -> Vec<String> {
        checks
            .iter()
            .map(|(name, plan)| self.guard_stmt(name, plan))
            .collect()
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

    /// insert the entry guards at the top of `func`'s body, after any docstring
    fn insert_param_guards(
        &mut self,
        source: &str,
        func: &StmtFunctionDef,
        checks: &[(Name, SoundnessCheck)],
    ) {
        let guards: Vec<EntryGuard> = self
            .guard_stmts(checks)
            .into_iter()
            .map(EntryGuard)
            .collect();
        // the same anchor `mutable_defaults` and `init_method` hang their own body
        // insertions off, so the four agree about where the top of a body is
        match body_prologue(source, func, &guards) {
            Some(anchored) => self.guards.extend(anchored),
            // nothing in the body came from the source. a check with nowhere to go
            // is an error rather than a check dropped: a native build of the module
            // makes it from the same site, and the two builds would answer
            // differently
            None => self.unanchored.push(func.name.to_string()),
        }
    }
}

pub(crate) struct SoundnessPass<'src> {
    positions: SoundnessPositions,
    source: &'src str,
    written: WrittenNames<'src>,
}

impl<'src> SoundnessPass<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>, config: &Config) -> Self {
        Self {
            positions: config.soundness,
            source,
            written,
        }
    }
}

impl TypeAwarePass for SoundnessPass<'_> {
    // every check validates a value as the program produces it
    fn runtime_only(&self) -> bool {
        true
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if !self.positions.any() {
            return;
        }
        let mut inner = Rendered::default();
        let mut rerendered = Vec::new();
        for (idx, stmt) in stmts.iter().enumerate() {
            let mut walker = Soundness::new(types, self.positions, self.written);
            walker.visit_stmt(stmt);
            // an AST-mutation pass rewrote this statement, and an edit at a node it
            // replaced would be dropped: its checks go into the tree it is printed from
            if ctx.changed.contains(&idx) {
                let mut rendered = Rendered::default();
                let mut checks = Vec::new();
                for site in &walker.sites {
                    match site {
                        Site::Value { expr, plan } => rendered.wrap_check(expr, plan),
                        Site::Elements {
                            iterable,
                            is_async,
                            plan,
                        } => rendered.wrap_iteration(iterable, *is_async, plan),
                        Site::Parameters { function, guards } => {
                            checks.push(RerenderedCheck::Guards {
                                function: function.range(),
                                statements: rendered.guard_stmts(guards),
                            });
                        }
                    }
                }
                checks.extend(rendered.wraps.drain(..).map(|(range, before, after)| {
                    RerenderedCheck::Wrap {
                        range,
                        before,
                        after,
                    }
                }));
                inner.used_iter |= rendered.used_iter;
                inner.used_aiter |= rendered.used_aiter;
                inner.used_iter_p |= rendered.used_iter_p;
                inner.used_aiter_p |= rendered.used_aiter_p;
                inner.used_parametric |= rendered.used_parametric;
                if !checks.is_empty() {
                    rerendered.push((idx, checks));
                }
                continue;
            }
            for site in &walker.sites {
                match site {
                    Site::Value { expr, plan } => inner.wrap_check(expr, plan),
                    Site::Elements {
                        iterable,
                        is_async,
                        plan,
                    } => inner.wrap_iteration(iterable, *is_async, plan),
                    Site::Parameters { function, guards } => {
                        inner.insert_param_guards(self.source, function, guards);
                    }
                }
            }
        }
        if let Some(name) = inner.unanchored.first() {
            ctx.errors.push(format!(
                "`{name}` has a body nothing in the source anchors, so an entry guard has nowhere to go"
            ));
            return;
        }
        if inner.wraps.is_empty() && inner.guards.is_empty() && rerendered.is_empty() {
            return;
        }
        ctx.runtime.insert(crate::runtime::SOUNDNESS_CHECK);
        if inner.used_iter {
            ctx.runtime.insert(crate::runtime::SOUNDNESS_ITER);
        }
        if inner.used_aiter {
            ctx.runtime.insert(crate::runtime::SOUNDNESS_AITER);
        }
        // a deep parametric check reuses the `_parametric_is` probe (which
        // brings its own `_parametric_is_sub`); function names resolve at call
        // time, so the def order among these preamble helpers is irrelevant
        if inner.used_parametric {
            ctx.runtime.insert(crate::runtime::SOUNDNESS_PARAMETRIC);
        }
        if inner.used_iter_p {
            ctx.runtime.insert(crate::runtime::SOUNDNESS_ITER_P);
        }
        if inner.used_aiter_p {
            ctx.runtime.insert(crate::runtime::SOUNDNESS_AITER_P);
        }
        ctx.template_edits
            .extend(inner.wraps.into_iter().map(|(range, before, after)| {
                (
                    range,
                    vec![
                        Fragment::Lit(before),
                        Fragment::Src(range),
                        Fragment::Lit(after),
                    ],
                )
            }));
        ctx.statement_inserts.extend(inner.guards);
        ctx.rerendered_checks.extend(rerendered);
    }
}

/// a check to write into a top-level statement that is printed from its syntax tree,
/// found by the source range of the node it is made at
pub(crate) enum RerenderedCheck {
    /// the value the expression at `range` produces, passed through the call
    /// `before` opens and `after` closes
    Wrap {
        range: TextRange,
        before: String,
        after: String,
    },
    /// the entry guards of the function at `function`
    Guards {
        function: TextRange,
        statements: Vec<String>,
    },
}

/// write `checks` into `stmt`, a top-level statement an AST-mutation pass rewrote
///
/// a mutation leaves every node it did not replace at its source range, and a check
/// is made at a node the source holds, so each is found where the text edit would
/// have landed. a check with nowhere to go is an error rather than a check dropped:
/// a native build of the module makes it from the same sites, and the two builds
/// would answer differently
pub(crate) fn place_in_rerendered(
    stmt: &mut Stmt,
    checks: Vec<RerenderedCheck>,
) -> Result<(), String> {
    let mut wraps = Vec::new();
    let mut guards = Vec::new();
    for check in checks {
        match check {
            RerenderedCheck::Wrap {
                range,
                before,
                after,
            } => {
                let placeholder = "__by_soundness_value__";
                let Ok(parsed) =
                    ruff_python_parser::parse_expression(&format!("{before}{placeholder}{after}"))
                else {
                    return Err(format!(
                        "a soundness check could not be written: `{before}…{after}`"
                    ));
                };
                let mut parsed = *parsed.into_syntax().body;
                super::rerender::forget_expr_ranges(&mut parsed);
                let Expr::Call(call) = parsed else {
                    return Err(format!(
                        "a soundness check is not a call: `{before}…{after}`"
                    ));
                };
                wraps.push((range, call, Cell::new(false)));
            }
            RerenderedCheck::Guards {
                function,
                statements,
            } => {
                let Ok(parsed) = ruff_python_parser::parse_module(&statements.join("\n")) else {
                    return Err(format!(
                        "a parameter guard could not be written: `{}`",
                        statements.join("; ")
                    ));
                };
                let mut statements: Vec<Stmt> = parsed.into_syntax().body.into_iter().collect();
                statements
                    .iter_mut()
                    .for_each(super::rerender::forget_stmt_ranges);
                guards.push((function, statements, Cell::new(false)));
            }
        }
    }
    let placing = Placing { wraps, guards };
    placing.visit_stmt(stmt);
    let unplaced = placing
        .wraps
        .iter()
        .filter(|(_, _, placed)| !placed.get())
        .map(|(range, _, _)| *range)
        .chain(
            placing
                .guards
                .iter()
                .filter(|(_, _, placed)| !placed.get())
                .map(|(range, _, _)| *range),
        )
        .next();
    match unplaced {
        Some(range) => Err(format!(
            "a soundness check at {range:?} could not be written: a lowering rewrote the \
             expression it checks"
        )),
        None => Ok(()),
    }
}

struct Placing {
    wraps: Vec<(TextRange, ExprCall, Cell<bool>)>,
    guards: Vec<(TextRange, Vec<Stmt>, Cell<bool>)>,
}

impl Transformer for Placing {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        transformer::walk_stmt(self, stmt);
        let Stmt::FunctionDef(function) = stmt else {
            return;
        };
        let Some((_, statements, placed)) = self
            .guards
            .iter()
            .find(|(range, _, _)| *range == function.range)
        else {
            return;
        };
        // after a docstring, which has to stay the first statement
        let at = super::source_util::body_prologue_index(function);
        function.body.splice(at..at, statements.iter().cloned());
        placed.set(true);
    }

    fn visit_annotation(&self, _expr: &mut Expr) {}

    fn visit_expr(&self, expr: &mut Expr) {
        // the children first, so a check made inside this value is inside its check
        transformer::walk_expr(self, expr);
        // a node a lowering left twice — `a ?? b` repeats a side-effect-free `a` —
        // is checked wherever it is evaluated
        let at = expr.range();
        for (range, call, placed) in &self.wraps {
            if at != *range {
                continue;
            }
            let value = std::mem::replace(
                expr,
                Expr::Name(ExprName {
                    node_index: AtomicNodeIndex::NONE,
                    range: TextRange::default(),
                    id: ruff_python_ast::name::Name::new_static("None"),
                    ctx: ruff_python_ast::ExprContext::Load,
                }),
            );
            // a second check made at the same node is written around this one, as the
            // text edits compose
            let mut call = call.clone();
            if let Some(first) = call.arguments.args.first_mut() {
                *first = value;
            }
            *expr = Expr::Call(call);
            placed.set(true);
        }
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
    fn field_specifier_assignment_not_checked() {
        // a field-specifier RHS (`dataclasses.field()`, `pydantic.Field()`) is
        // modelled as the field type but is a descriptor object at runtime, so a
        // soundness check against the annotation would always fail
        let out = check(
            "from dataclasses import dataclass, field\n@dataclass\nclass A:\n    a: int = field()\n",
        );
        assert!(
            !out.contains("_soundness_check"),
            "field specifier must not be soundness-wrapped, got:\n{out}"
        );
        assert!(out.contains("a: int = field()"), "got:\n{out}");
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

    /// a `=` field is checked like any other expression. the check used to be skipped
    /// there, because `f"{x=}"` prints the source between the braces and the wrap would
    /// print itself — but the field is now taken apart around the value
    /// ([`super::debug_field`]), so the author's own text is what is printed and the value
    /// is free to be wrapped
    #[test]
    fn a_debug_interpolation_is_checked_beside_the_text_it_prints() {
        let out = check("def f(d: dict[str, int]):\n    return f\"{d.get('k')=}\"\n");
        assert!(
            out.contains("f\"d.get('k')={(_soundness_check(d.get('k'), (int, type(None))))!r}\""),
            "got:\n{out}"
        );
    }

    #[test]
    fn an_ordinary_interpolation_is_still_checked() {
        let out = check("def f(d: dict[str, int]):\n    return f\"{d.get('k')}\"\n");
        assert!(
            out.contains("_soundness_check(d.get('k'), (int, type(None)))"),
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

    /// a tuple written without parentheses is one iterable, and stays one inside the
    /// check's call rather than becoming its arguments
    #[test]
    fn an_unparenthesized_tuple_is_checked_as_one_iterable() {
        let out = check(
            "def f(a: list[int], b: list[int]):\n    for x in *a, *b:\n        print(x)\n    for y in a, b:\n        print(y)\n",
        );
        assert!(
            out.contains("for x in _soundness_iter((*a, *b), int):"),
            "got:\n{out}"
        );
        assert!(
            out.contains("for y in _soundness_iter((a, b), list):"),
            "got:\n{out}"
        );
    }

    /// a call takes a `yield` only parenthesized, and a `yield` the source parenthesized
    /// has its parentheses outside the check
    #[test]
    fn a_yield_is_checked_parenthesized() {
        let out = check(
            "from typing import Any, Generator\ndef g() -> Generator[int, Any, None]:\n    x: int = yield 1\n    y: int = (yield 2)\n",
        );
        assert!(
            out.contains("x: int = _soundness_check((yield 1), int)"),
            "got:\n{out}"
        );
        assert!(
            out.contains("y: int = (_soundness_check((yield 2), int))"),
            "got:\n{out}"
        );
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

    /// a default is a written type. `def f(safe='/')` says `safe` is a `str` as
    /// plainly as an annotation would — if something else belonged there, something
    /// else would be written — and the native backend already lays the parameter out
    /// at that bound and refuses `f(b'x')` at its boundary. reading the plan off the
    /// annotation *node* could not see it, so the interpreted twin was silently more
    /// permissive than its own compiled form
    #[test]
    fn a_default_states_a_type_as_an_annotation_does() {
        let out = check_with("def f(safe = \"/\", n = 1): ...\n", params_only());
        assert!(out.contains("_soundness_check(safe, str)"), "got:\n{out}");
        // and the literal is promoted, so it is the class rather than the value
        assert!(!out.contains("Literal"), "got:\n{out}");
        assert!(out.contains("_soundness_check(n, int)"), "got:\n{out}");
    }

    /// what the source states is the *default*, not everything the bound accumulates.
    /// a parameter with nothing to state gets no guard: `None` is the sentinel every
    /// optional parameter is spelled with — it says the argument may be left out, not
    /// that `None` is what belongs there — and a bare parameter states nothing at all
    #[test]
    fn a_parameter_whose_source_states_nothing_is_not_guarded() {
        let out = check_with(
            "def f(bare, maybe = None, *rest, **kw): ...\n",
            params_only(),
        );
        assert!(!out.contains("_soundness_check"), "got:\n{out}");
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

    /// an `init(…)` shorthand's body opens with the attribute declarations the parser
    /// synthesized from its parameters, whose ranges point back into the signature.
    /// anchored at one of those, the guard was spliced into the parameter list
    #[test]
    fn entry_guard_in_an_init_shorthand_sits_in_the_body() {
        let out = check_with(
            "class A:\n    init(let s: str):\n        print(s)\n",
            params_only(),
        );
        assert!(
            out.contains("    def __init__(self, s: str):\n        _soundness_check(s, str)\n        self.s: str = s\n        print(s)\n"),
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

    /// a repeated `_` is numbered in the python its `def` lowers to, so each guard checks
    /// the parameter python binds: spelled by its source name, the second `_` would be
    /// checked as the first, and an `int` refused where a `str` was declared
    #[test]
    fn a_repeated_underscore_is_guarded_by_its_numbered_name() {
        let out = check_with(
            "def f(_: int, _: str) -> int:\n    return 1\n",
            params_only(),
        );
        assert!(out.contains("_soundness_check(_, int)"), "got:\n{out}");
        assert!(out.contains("_soundness_check(_2, str)"), "got:\n{out}");
    }

    #[test]
    fn all_includes_parameters() {
        let out = check_with("def f(s: str): ...\n", crate::SoundnessPositions::all());
        assert!(out.contains("_soundness_check(s, str)"), "got:\n{out}");
    }

    // ── statements a syntax-tree pass prints again ──────────────────────

    /// `typeof` is lowered by rewriting the syntax tree, and a top-level statement
    /// holding one is printed again from that tree rather than edited in place. the
    /// checks everywhere else in the statement are made all the same — a native build
    /// of the module makes them, from the same sites
    #[test]
    fn a_statement_printed_from_its_syntax_tree_keeps_its_checks() {
        let out = check(
            "from typing import Any\n\ndef f(x: Any, a: list[str], n: int) -> int:\n    b: int = x\n    for s in a:\n        print(s)\n    m: typeof(n) = n\n    return m\n",
        );
        assert!(
            out.contains("TypeOf[n]"),
            "`typeof` no longer rewrites the tree, got:\n{out}"
        );
        assert!(
            out.contains("b: int = _soundness_check(x, int)"),
            "got:\n{out}"
        );
        assert!(
            out.contains("for s in _soundness_iter(a, str):"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_function_printed_from_its_syntax_tree_keeps_its_entry_guards() {
        // a repeated `_` parameter is renamed on the syntax tree
        let out = check_with(
            "def f(_: int, _: int, v: str) -> str:\n    return v\n",
            params_only(),
        );
        assert!(
            out.contains("_2"),
            "the parameters are no longer renamed, got:\n{out}"
        );
        assert!(out.contains("_soundness_check(v, str)"), "got:\n{out}");
    }

    /// and it keeps its docstring. the parser writes an `init(…)` shorthand's attribute
    /// declarations ahead of everything the source wrote, so the docstring is not the
    /// body's first statement in the tree — read as though it were, the guard landed
    /// above the string and the method lost its `__doc__`
    #[test]
    fn an_entry_guard_follows_the_docstring_of_an_init_shorthand() {
        // `typeof` rewrites the syntax tree, so the whole class is printed from it
        let out = check_with(
            "class C:\n    init(let x: int):\n        \"\"\"doc\"\"\"\n        print(x)\n\n    def m(self):\n        y: typeof(self.x) = 1\n        print(y)\n",
            params_only(),
        );
        assert!(
            out.contains("TypeOf[self.x]"),
            "`typeof` no longer rewrites the tree, got:\n{out}"
        );
        assert!(
            out.contains(
                "        \"\"\"doc\"\"\"\n        _soundness_check(x, int)\n        self.x: int = x\n"
            ),
            "got:\n{out}"
        );
    }

    /// a stub is never run, so it produces no value to check
    #[test]
    fn a_stub_gets_no_checks() {
        let source = "def f(x: int) -> int:\n    return x\n";
        let config = Config {
            is_stub: true,
            soundness: crate::SoundnessPositions::all(),
            ..Config::test_default()
        };
        assert_eq!(transpile(source, &config).unwrap(), source);
    }
}
