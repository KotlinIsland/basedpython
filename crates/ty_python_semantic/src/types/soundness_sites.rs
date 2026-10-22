//! basedpython: where the transpiled program checks a value at runtime against the type
//! the checker gave it
//!
//! the transpiler writes a check at each site this decides on, and a native build of the
//! same module makes the same checks, so neither decides for itself. the checker asks too:
//! a check names its type's classes as the module binds them, and the import binding one is
//! used by the check although the source never spells the read

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Comprehension, Expr, ExprCall, Parameter, Stmt, StmtFunctionDef, UnaryOp};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::scope::ScopeKind;
use ty_python_core::semantic_index;

use crate::Db;
use crate::place::{RuntimeBinding, RuntimeRead, binds_one_class_wherever_bound, runtime_binding};
use crate::semantic_model::{HasType, LoweringModel};
use crate::types::check_order::InferredInCheckOrder;
use crate::types::runtime_spelling::{RuntimeSite, RuntimeSpelling};
use crate::types::soundness::{
    ArgSelector, CheckKind, CheckTarget, call_result_is_typevar_derived,
    is_specialized_generic_instance, parameter_check_plan, parameter_runtime_check_plan,
    runtime_check_plan,
};
use crate::types::{DynamicType, KnownInstanceType, ProgramEnvironment, Type};

/// every runtime soundness check `suite` makes under `positions`, in the order the
/// checks are found
pub fn soundness_sites<'ast>(
    model: &LoweringModel<'_>,
    suite: &'ast [Stmt],
    positions: SoundnessPositions,
) -> Vec<SoundnessSite<'ast>> {
    if !positions.any() {
        return Vec::new();
    }
    let mut walker = Soundness::new(model, positions);
    walker.visit_body(suite);
    walker.sites
}

/// Per-position toggles for the runtime type-soundness checks. Each field
/// enables validation at one syntactic position where ty accepts a value on
/// an annotation-level claim it can't verify at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-position toggles, not a state machine"
)]
pub struct SoundnessPositions {
    /// results of calls whose type is typevar-derived — a generic function's
    /// return (`t[T]() -> T`) or a method bound to a specialized generic
    /// instance (`dict[str, int].get`)
    pub generic_calls: bool,
    /// element reads out of a specialized container (`a[0]` on `list[str]`)
    pub projections: bool,
    /// loop / comprehension elements drawn from a specialized iterable
    pub iterations: bool,
    /// explicit `Any` (or a context-solved generic result) flowing into an
    /// annotated assignment target (`a: str = any_val`)
    pub assignments: bool,
    /// a returned value validated against the enclosing function's declared
    /// return type (`def g() -> str: return any_val`)
    pub returns: bool,
    /// a call argument validated against its matched parameter's annotation
    /// (`takes(any_val)` where `takes(s: str)`)
    pub arguments: bool,
    /// a function's own parameters validated against their annotations at
    /// entry, defending its contract against callers the checker never saw
    /// (untyped or third-party code). off in the default set — it runs on
    /// every call, so it's opt-in — but included in [`Self::all`]
    pub parameters: bool,
}

impl SoundnessPositions {
    /// every position enabled, including the opt-in defensive `parameters`
    /// entry checks (the CLI `--soundness all`)
    pub fn all() -> Self {
        Self {
            parameters: true,
            ..Self::defaults()
        }
    }

    /// the positions enabled without an explicit request: every inference-gap
    /// check, but *not* the defensive `parameters` entry checks (those add a
    /// check on every call, so they stay opt-in)
    pub fn defaults() -> Self {
        Self {
            generic_calls: true,
            projections: true,
            iterations: true,
            assignments: true,
            returns: true,
            arguments: true,
            parameters: false,
        }
    }

    /// no position enabled — the pass is a no-op
    pub fn none() -> Self {
        Self {
            generic_calls: false,
            projections: false,
            iterations: false,
            assignments: false,
            returns: false,
            arguments: false,
            parameters: false,
        }
    }

    /// whether any position is enabled
    pub fn any(self) -> bool {
        self.generic_calls
            || self.projections
            || self.iterations
            || self.assignments
            || self.returns
            || self.arguments
            || self.parameters
    }
}

/// which parameter an argument binds to, for the `arguments` gate
enum ArgSlot<'a> {
    Positional(usize),
    Keyword(&'a str),
}

/// one runtime soundness check the transpiled program makes, at the syntax node it is
/// made at
#[derive(Debug, Clone)]
pub enum SoundnessSite<'ast> {
    /// the value `expr` produces is checked as it is produced
    Value { expr: &'ast Expr, check: SiteCheck },
    /// each element drawn from `iterable` is checked as it is drawn
    Elements {
        iterable: &'ast Expr,
        is_async: bool,
        check: SiteCheck,
    },
    /// `function`'s own parameters are checked where its body begins, each in the order
    /// it is written
    Parameters {
        function: &'ast StmtFunctionDef,
        guards: Vec<(&'ast Parameter, SiteCheck)>,
    },
}

impl SoundnessSite<'_> {
    /// every check the site makes
    pub(crate) fn plans(&self) -> Vec<&CheckKind> {
        match self {
            Self::Value { check, .. } | Self::Elements { check, .. } => vec![&check.plan],
            Self::Parameters { guards, .. } => {
                guards.iter().map(|(_, check)| &check.plan).collect()
            }
        }
    }
}

/// a check a site makes, and the module-level names it reads that the program may not have
/// bound yet when it runs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteCheck {
    pub plan: CheckKind,
    /// each name the check's target reads that is bound only further down the module, or
    /// only on some paths, as a spelling of its own. every binding the module makes of one
    /// binds the class the checker resolved, so once all of them are bound the target is the
    /// one the checker named; the check is made only then, and the value passes unchecked
    /// before. empty for a check whose names are bound wherever it runs
    pub once_bound: Vec<RuntimeSpelling>,
}

/// where the checks go, decided once over a syntax tree, and what is not a place
/// for one
struct Soundness<'a, 'db, 'ast> {
    model: &'a LoweringModel<'db>,
    /// what a name the checks are written with holds is asked once the file is inferred
    inferred: InferredInCheckOrder<'db>,
    positions: SoundnessPositions,
    sites: Vec<SoundnessSite<'ast>>,
    /// gated expressions already covered by a wrap on an enclosing `!`
    /// force-unwrap — wrapping them directly would splice the check's second
    /// argument into `_force_unwrap`'s call parens
    consumed: Vec<TextRange>,
    /// declared-return-type check plans of the enclosing functions (innermost
    /// last); `None` when a function has no annotation or an uncheckable one
    return_targets: Vec<Option<CheckKind>>,
}

impl<'a, 'db, 'ast> Soundness<'a, 'db, 'ast> {
    fn new(model: &'a LoweringModel<'db>, positions: SoundnessPositions) -> Self {
        Self {
            model,
            inferred: model.inferred_in_check_order(),
            positions,
            sites: Vec::new(),
            consumed: Vec::new(),
            return_targets: Vec::new(),
        }
    }

    fn db(&self) -> &'db dyn Db {
        self.model.db()
    }

    fn env(&self) -> ProgramEnvironment<'db> {
        self.model.program_environment()
    }

    fn call_result_is_typevar_derived(&self, callee: &Expr) -> bool {
        callee
            .inferred_type(self.model)
            .is_some_and(|ty| call_result_is_typevar_derived(self.db(), &self.env(), ty))
    }

    fn is_specialized_generic_instance(&self, expr: &Expr) -> bool {
        expr.inferred_type(self.model)
            .is_some_and(|ty| is_specialized_generic_instance(self.db(), &self.env(), ty))
    }

    fn is_any(&self, expr: &Expr) -> bool {
        expr.inferred_type(self.model)
            .is_some_and(|ty| matches!(ty, Type::Dynamic(DynamicType::Any)))
    }

    fn is_field_specifier(&self, expr: &Expr) -> bool {
        matches!(
            expr.inferred_type(self.model),
            Some(Type::KnownInstance(KnownInstanceType::Field(_)))
        )
    }

    /// the check for the parameter `selector` binds to in a call through `callee`
    fn parameter_check_plan_for(
        &self,
        callee: &Expr,
        selector: ArgSelector<'_>,
    ) -> Option<CheckKind> {
        let ty = callee.inferred_type(self.model)?;
        parameter_check_plan(self.db(), &self.env(), self.model.file(), ty, selector)
    }

    /// whether `expr` is a value this pass classifies as resting on an
    /// unverified claim: a call whose result type came from a typevar
    /// solution, or an element read out of a specialized container. this is
    /// the position-agnostic *classification*; the direct-wrap sites also
    /// require the corresponding position to be enabled ([`Self::gated_enabled`])
    fn gated(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call(call) => self.call_result_is_typevar_derived(&call.func),
            Expr::Subscript(subscript) => {
                subscript.ctx.is_load() && self.is_specialized_generic_instance(&subscript.value)
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
        self.is_any(value) || (self.gated(value) && self.check_plan(value).is_none())
    }

    /// the check plan for `expr`'s inferred type, with the pure-`None`
    /// isinstance noise case dropped (validating a `None` result guards nothing)
    fn check_plan(&self, expr: &Expr) -> Option<CheckKind> {
        let ty = expr.inferred_type(self.model)?;
        runtime_check_plan(self.db(), &self.env(), self.model.file(), ty)
            .filter(|plan| !matches!(plan, CheckKind::Isinstance(CheckTarget::NoneType)))
    }

    /// [`Self::check_plan`] for a parameter, read off the parameter rather than off
    /// its annotation — which is the only way to see a type the source stated with a
    /// default instead of with an annotation
    fn parameter_plan(&self, parameter: &Parameter) -> Option<CheckKind> {
        let ty = parameter.inferred_type(self.model)?;
        parameter_runtime_check_plan(self.db(), &self.env(), self.model.file(), ty)
            .filter(|plan| !matches!(plan, CheckKind::Isinstance(CheckTarget::NoneType)))
    }

    /// the check `plan` makes at `site`, if it can make one there: every name its target
    /// reads must hold, where `site` runs, the binding the checker resolved. a check that would
    /// read one that does not — a class imported only under `if TYPE_CHECKING:`, or declared
    /// further on — is left out, as a check with no faithful target is.
    ///
    /// code in a function runs whenever the function is called, which can be before or after
    /// the module binds a class declared further down. there a name the module binds only to
    /// one class, and never to anything else, is looked up as the check runs instead
    /// ([`SiteCheck::once_bound`]), and the check is made once it is bound
    fn site_check(
        &self,
        plan: CheckKind,
        site: RuntimeSite<'db>,
        runs_later: bool,
    ) -> Option<SiteCheck> {
        let db = self.db();
        let file = self.model.file();
        let program_file = db.program_file(file);
        let mut once_bound = Vec::new();
        for (name, read) in plan.spelling().global_read_spellings() {
            match runtime_binding(
                db,
                &self.env(),
                RuntimeRead::Global {
                    name,
                    range: site.range(),
                },
                site.inferred(),
            ) {
                RuntimeBinding::Bound => {}
                RuntimeBinding::Unbound | RuntimeBinding::NotTheCheckers
                    if runs_later && binds_one_class_wherever_bound(db, program_file, name) =>
                {
                    once_bound.push(read);
                }
                RuntimeBinding::Unbound | RuntimeBinding::NotTheCheckers => return None,
            }
        }
        Some(SiteCheck { plan, once_bound })
    }

    /// whether `expr` runs only once a function or a lambda holding it is called, rather than
    /// as the module or a class body reaches it
    fn runs_later(&self, expr: &Expr) -> bool {
        let index = semantic_index(self.db(), self.db().program_file(self.model.file()));
        index.try_expression_scope_id(expr).is_some_and(|scope| {
            index
                .ancestor_scopes(scope)
                .any(|(_, scope)| matches!(scope.kind(), ScopeKind::Function | ScopeKind::Lambda))
        })
    }

    /// check the value `expr` produces against `plan`
    fn check_value(&mut self, expr: &'ast Expr, plan: CheckKind) {
        let Some(check) = self.site_check(
            plan,
            RuntimeSite::at(expr, self.inferred),
            self.runs_later(expr),
        ) else {
            return;
        };
        self.sites.push(SoundnessSite::Value { expr, check });
    }

    /// check each element drawn from the iterable of a `for` / comprehension
    /// clause when the iterable carries a generic specialization and the element
    /// (loop target) type is checkable
    fn check_iteration(&mut self, iterable: &'ast Expr, target_expr: &Expr, is_async: bool) {
        if !self.is_specialized_generic_instance(iterable) {
            return;
        }
        let Some(plan) = self.check_plan(target_expr) else {
            return;
        };
        let Some(check) = self.site_check(
            plan,
            RuntimeSite::at(iterable, self.inferred),
            self.runs_later(iterable),
        ) else {
            return;
        };
        self.sites.push(SoundnessSite::Elements {
            iterable,
            is_async,
            check,
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
            ArgSlot::Positional(index) => {
                self.parameter_check_plan_for(callee, ArgSelector::Positional(index))
            }
            ArgSlot::Keyword(name) => {
                self.parameter_check_plan_for(callee, ArgSelector::Keyword(name))
            }
        };
        if let Some(plan) = plan {
            self.check_value(arg, plan);
        }
    }

    /// the plan to validate a returned `value` against — the enclosing
    /// function's declared return plan — but only when `value`'s own type
    /// is unhelpful (else `generic_calls`/`projections` already covers it)
    fn return_plan(&self, value: &Expr) -> Option<CheckKind> {
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
            // the guards run where the function's body begins, when it is called
            if let Some(plan) = self.parameter_plan(parameter)
                && let Some(check) =
                    self.site_check(plan, RuntimeSite::at(func, self.inferred), true)
            {
                guards.push((parameter, check));
            }
        }
        if !guards.is_empty() {
            self.sites.push(SoundnessSite::Parameters {
                function: func,
                guards,
            });
        }
    }
}

impl<'ast> Visitor<'ast> for Soundness<'_, '_, 'ast> {
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
                    && !self.is_field_specifier(value)
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
