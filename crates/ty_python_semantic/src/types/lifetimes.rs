//! basedpython: static enforcement of `local` and `once` parameters.
//!
//! [`check_local_lifetimes`] runs two intraprocedural analyses over a function
//! body:
//!
//! - **`local`** — a borrowed parameter must not outlive the call. Reports
//!   [`ESCAPING_LOCAL`] when it is returned, stored on a parameter-rooted object
//!   (`self.cb = fn`), bound to a `global` / `nonlocal` name, or handed to a
//!   parameter that is not itself a borrow.
//! - **`once`** — a callback must be called exactly once. It is a `local` borrow
//!   with that extra obligation, so it is *also* escape-checked (and may only be
//!   passed on to another `once`). Reports [`ONCE_NOT_CALLED`] when it is never
//!   called and [`ONCE_CALLED_TWICE`] on two unconditional calls or a call inside
//!   a loop.
//!
//! A parameter carries a borrow either from its own `local` / `once` prefix or,
//! for a trailing lambda block's implicit `it`, from the callee's declaration of
//! the callback's parameter ([`InheritedBorrow`]) — `(local int) -> None` makes
//! the block the implementation of a borrowed callback.
//!
//! Both are deliberately conservative — they flag only what they can see
//! directly, never guessing through opaque calls, aliasing, or closures. See
//! `docs/basedpython/features/local-lifetimes.md`.

use ruff_db::parsed::ParsedModuleRef;
use ruff_db::source::source_text;
use ruff_python_ast::helpers::parameter_modifiers;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{self as ast, Expr, ExprContext, ExprName, ParameterBorrow, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind};
use ty_python_core::{SemanticIndex, semantic_index};

use crate::Db;

use super::context::InferContext;
use super::diagnostic::{
    ESCAPING_LOCAL, ESCAPING_LOOP_VARIABLE, INVALID_ASSIGNMENT, ONCE_CALLED_TWICE, ONCE_NOT_CALLED,
    TRAILING_LAMBDA_CONTROL_FLOW,
};
use super::{Type, TypeContext, infer_expression_types};

/// basedpython: where a parameter's borrow was declared, and how the diagnostic
/// should describe that declaration.
#[derive(Clone, Copy)]
struct BorrowDeclaration {
    /// what the "declared here" annotation points at
    range: TextRange,
    /// true when the borrow comes from the *callee's* callback signature rather
    /// than from a modifier on this parameter — a trailing lambda's `it`, whose
    /// own parameter is synthetic and has an empty range, so `range` points at
    /// the callee instead
    inherited: bool,
}

impl BorrowDeclaration {
    /// declared by a modifier on the parameter itself
    fn own(range: TextRange) -> Self {
        Self {
            range,
            inherited: false,
        }
    }

    /// the "declared here" annotation for a borrow of kind `kind` on `name`
    fn describe(self, name: &str, kind: &str) -> String {
        if self.inherited {
            format!("`{name}` binds a `{kind}` parameter of this callback")
        } else {
            format!("`{name}` is declared `{kind}` here")
        }
    }
}

/// basedpython: a borrow a function's parameter carries from somewhere other
/// than its own declaration — a trailing-lambda block's `it`, which is borrowed
/// because the callee declared its callback's parameter `local` / `once`.
#[derive(Clone, Copy)]
pub(super) struct InheritedBorrow<'a, 'db, 'ast> {
    /// the parameter the borrow lands on
    pub(super) name: &'ast str,
    /// which modifier the callee declared
    pub(super) borrow: ParameterBorrow,
    /// the callee expression, which the diagnostics point at
    pub(super) declaration: TextRange,
    /// the block's own scope, and the index to resolve names against — a store
    /// in a block writes *through* to an enclosing binding, so what looks like a
    /// block-local assignment can be an escape
    pub(super) index: &'a SemanticIndex<'db>,
    pub(super) block_scope: FileScopeId,
}

impl<'ast> InheritedBorrow<'_, '_, 'ast> {
    /// Whether assigning `name` inside the block rebinds a name an enclosing
    /// scope already binds. The lowering emits a `global` / `nonlocal` for
    /// exactly those, so the assigned value outlives the call; a name only the
    /// block binds stays block-local and dies with it.
    fn writes_through(&self, name: &str) -> bool {
        self.index
            .ancestor_scopes(self.block_scope)
            .skip(1)
            .any(|(scope_id, _)| {
                let table = self.index.place_table(scope_id);
                table.symbol_id(name).is_some_and(|symbol_id| {
                    let symbol = table.symbol(symbol_id);
                    symbol.is_bound() || symbol.is_declared()
                })
            })
    }

    /// records this borrow in the two maps the checks are driven by
    fn seed(
        self,
        locals: &mut FxHashMap<&'ast str, BorrowDeclaration>,
        once: &mut FxHashMap<&'ast str, BorrowDeclaration>,
    ) {
        let declaration = BorrowDeclaration {
            range: self.declaration,
            inherited: true,
        };
        match self.borrow {
            ParameterBorrow::None => {}
            ParameterBorrow::Local => {
                locals.insert(self.name, declaration);
            }
            // `once` is a borrow with an extra obligation, so it seeds both
            ParameterBorrow::Once => {
                locals.insert(self.name, declaration);
                once.insert(self.name, declaration);
            }
        }
    }
}

/// How a `local` value left the call — feeds the diagnostic message.
#[derive(Clone, Copy)]
enum EscapeRoute {
    Returned,
    Stored,
}

impl EscapeRoute {
    fn describe(self) -> &'static str {
        match self {
            EscapeRoute::Returned => "returned from the call",
            EscapeRoute::Stored => "stored where it outlives the call",
        }
    }
}

/// the borrowed parameters of `function`: every `local` / `once` one, and the
/// `once` subset. `once` is a borrow with an extra "called exactly once"
/// obligation, so it appears in both
fn borrowed_parameters<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    function: &'ast ast::StmtFunctionDef,
    inherited: Option<InheritedBorrow<'_, 'db, 'ast>>,
) -> (
    FxHashMap<&'ast str, BorrowDeclaration>,
    FxHashMap<&'ast str, BorrowDeclaration>,
) {
    let source = source_text(context.db(), context.file());
    let mut locals: FxHashMap<&'ast str, BorrowDeclaration> = FxHashMap::default();
    let mut once: FxHashMap<&'ast str, BorrowDeclaration> = FxHashMap::default();
    for param in &function.parameters {
        let param = param.as_parameter();
        let modifiers = parameter_modifiers(&source, param);
        if modifiers.local || modifiers.once {
            locals.insert(
                param.name.as_str(),
                BorrowDeclaration::own(param.name.range()),
            );
        }
        if modifiers.once {
            once.insert(
                param.name.as_str(),
                BorrowDeclaration::own(param.name.range()),
            );
        }
    }
    if let Some(inherited) = inherited {
        inherited.seed(&mut locals, &mut once);
    }
    (locals, once)
}

/// basedpython entry point: enforce `local` (no escape) on `function`'s
/// parameters. The `once` count obligation is checked separately, by
/// [`check_once_obligations`], which needs the body's inferred types.
pub(super) fn check_local_lifetimes<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    function: &'ast ast::StmtFunctionDef,
    inherited: Option<InheritedBorrow<'_, 'db, 'ast>>,
) {
    let (locals, once) = borrowed_parameters(context, function, inherited);
    if locals.is_empty() {
        return;
    }

    // every parameter name: a parameter's attributes / items outlive the call,
    // because the caller holds the value bound to the parameter
    let params: FxHashSet<&'ast str> = function
        .parameters
        .iter()
        .map(|param| param.as_parameter().name.as_str())
        .collect();

    // names rebound to an outer scope by a `global` / `nonlocal` declaration
    let mut outer_names: FxHashSet<&'ast str> = FxHashSet::default();
    OuterNameCollector {
        names: &mut outer_names,
    }
    .visit_body(&function.body);

    EscapeChecker {
        context,
        locals: &locals,
        once: &once,
        params: &params,
        outer_names: &outer_names,
        inherited,
    }
    .visit_body(&function.body);
}

/// basedpython entry point: enforce the `once` "called exactly once"
/// obligation. Runs after the body is inferred, because a call can be spelled
/// `x.cb()` through the callback's own [implicit receiver](super::receivers),
/// and only the receiver's type says whether it is one.
pub(super) fn check_once_obligations<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    function: &'ast ast::StmtFunctionDef,
    inherited: Option<InheritedBorrow<'_, 'db, 'ast>>,
    receiver_call: impl Fn(&'ast ast::ExprAttribute) -> bool,
) {
    let (_, once) = borrowed_parameters(context, function, inherited);
    if !once.is_empty() {
        check_once_callbacks(context, &function.body, &once, &receiver_call);
    }
}

/// Collects the names declared `global` / `nonlocal` within a function body,
/// without descending into nested scopes (a nested declaration binds *its*
/// scope, not this one).
struct OuterNameCollector<'a, 'ast> {
    names: &'a mut FxHashSet<&'ast str>,
}

impl<'ast> StatementVisitor<'ast> for OuterNameCollector<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            Stmt::Global(global) => {
                self.names
                    .extend(global.names.iter().map(ast::Identifier::as_str));
            }
            Stmt::Nonlocal(nonlocal) => {
                self.names
                    .extend(nonlocal.names.iter().map(ast::Identifier::as_str));
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

struct EscapeChecker<'a, 'db, 'ast> {
    context: &'a InferContext<'db, 'ast>,
    /// every borrowed parameter (`local` or `once`) → its declaration range
    locals: &'a FxHashMap<&'ast str, BorrowDeclaration>,
    /// the subset that is `once`, so a diagnostic names the right modifier
    once: &'a FxHashMap<&'ast str, BorrowDeclaration>,
    params: &'a FxHashSet<&'ast str>,
    outer_names: &'a FxHashSet<&'ast str>,
    /// set when the body is a trailing-lambda block, whose stores may write
    /// through to an enclosing binding
    inherited: Option<InheritedBorrow<'a, 'db, 'ast>>,
}

impl<'ast> StatementVisitor<'ast> for EscapeChecker<'_, '_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a nested function or class is its own scope; the `local` parameter
            // is not in scope there (a capture is a separate, deferred concern)
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            Stmt::Return(ret) => {
                if let Some(value) = ret.value.as_deref()
                    && let Some(name) = self.surface_local(value)
                {
                    self.report(name, EscapeRoute::Returned);
                }
            }
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    self.check_store(target, &assign.value);
                }
            }
            Stmt::AnnAssign(ann) => {
                if let Some(value) = ann.value.as_deref() {
                    self.check_store(&ann.target, value);
                }
            }
            // `self.items += [fn]` mutates a parameter-rooted container in place,
            // so the local reaches storage that outlives the call
            Stmt::AugAssign(aug) => self.check_store(&aug.target, &aug.value),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

impl<'ast> EscapeChecker<'_, '_, 'ast> {
    /// If `value` hands a `local` binding straight to its consumer — the bare
    /// name, or a name held directly in a container literal — returns that name.
    /// Deliberately shallow: a value routed through a call (`g(fn)`) is not
    /// assumed to escape, since the callee may not retain it.
    fn surface_local(&self, value: &'ast Expr) -> Option<&'ast ExprName> {
        surface_local(value, self.locals)
    }

    /// Flags an assignment whose target outlives the call — a store into a
    /// parameter-rooted attribute / item, or into a `global` / `nonlocal` name —
    /// when its value carries a `local`.
    fn check_store(&mut self, target: &'ast Expr, value: &'ast Expr) {
        let outlives = match target {
            // a store into an attribute / item of a parameter, or of a
            // `global` / `nonlocal` name, reaches state that outlives the call
            Expr::Attribute(_) | Expr::Subscript(_) => root_name(target)
                .is_some_and(|root| self.params.contains(root) || self.outlives_scope(root)),
            Expr::Name(name) => self.outlives_scope(name.id.as_str()),
            _ => false,
        };
        if outlives && let Some(name) = self.surface_local(value) {
            self.report(name, EscapeRoute::Stored);
        }
    }

    /// Whether binding `name` here reaches state that outlives the call: it was
    /// declared `global` / `nonlocal`, or — in a trailing-lambda block, whose
    /// assignments write back — it resolves to an enclosing binding.
    fn outlives_scope(&self, name: &str) -> bool {
        self.outer_names.contains(name)
            || self
                .inherited
                .is_some_and(|inherited| inherited.writes_through(name))
    }

    fn report(&self, name: &ExprName, route: EscapeRoute) {
        let id = name.id.as_str();
        let kind = if self.once.contains_key(id) {
            "once"
        } else {
            "local"
        };
        let Some(builder) = self.context.report_lint(&ESCAPING_LOCAL, name) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "{kind} `{id}` cannot escape the call: it is {}",
            route.describe()
        ));
        if let Some(&decl) = self.locals.get(id) {
            diagnostic.annotate(
                self.context
                    .secondary(decl.range)
                    .message(decl.describe(id, kind)),
            );
        }
    }
}

/// The leftmost `Name` of an attribute / subscript chain: `a` for `a.b[c].d`.
fn root_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attribute) => root_name(&attribute.value),
        Expr::Subscript(subscript) => root_name(&subscript.value),
        _ => None,
    }
}

/// If `value` hands a `local` binding straight to its consumer — the bare name,
/// or a name held directly in a container literal — returns that name. Shallow
/// on purpose: a value routed through a call is handled by the caller.
fn surface_local<'ast>(
    value: &'ast Expr,
    locals: &FxHashMap<&str, BorrowDeclaration>,
) -> Option<&'ast ExprName> {
    match value {
        Expr::Name(name) if locals.contains_key(name.id.as_str()) => Some(name),
        Expr::List(list) => list.elts.iter().find_map(|e| surface_local(e, locals)),
        Expr::Tuple(tuple) => tuple.elts.iter().find_map(|e| surface_local(e, locals)),
        Expr::Set(set) => set.elts.iter().find_map(|e| surface_local(e, locals)),
        Expr::Starred(starred) => surface_local(&starred.value, locals),
        Expr::Dict(dict) => dict
            .items
            .iter()
            .find_map(|item| surface_local(&item.value, locals)),
        // both arms of a ternary hand the value straight on (`fn if c else other`)
        Expr::If(if_exp) => {
            surface_local(&if_exp.body, locals).or_else(|| surface_local(&if_exp.orelse, locals))
        }
        // `fn or fallback` / `a and fn` — any operand may be the surfaced value
        Expr::BoolOp(bool_op) => bool_op.values.iter().find_map(|v| surface_local(v, locals)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// borrow propagation: a `local` may only be passed to another borrow, a `once`
// only to another `once`.
// ---------------------------------------------------------------------------

/// The parameter an argument binds to.
#[derive(Clone, Copy)]
enum ArgTarget<'a> {
    /// the `n`-th positional argument of the call
    Positional(usize),
    /// a keyword argument `name=…`
    Keyword(&'a str),
}

/// basedpython: a borrow must not escape by being handed onward. A `local` value
/// may only be passed to another borrow (`local` or `once`); a `once` value may
/// only be passed to another `once` parameter (a plain `local` recipient could
/// call it zero or many times, breaking the exactly-once count). Report each
/// argument that violates this. The callee's signature must be resolvable (a
/// plain function or bound method); an opaque callee is left alone.
pub(super) fn check_local_argument_passing<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    function: &'ast ast::StmtFunctionDef,
    inherited: Option<InheritedBorrow<'_, 'db, 'ast>>,
    callee_type: impl Fn(&'ast Expr) -> Option<Type<'db>>,
) {
    let source = source_text(context.db(), context.file());
    let mut locals: FxHashMap<&'ast str, BorrowDeclaration> = FxHashMap::default();
    let mut once: FxHashMap<&'ast str, BorrowDeclaration> = FxHashMap::default();
    for param in &function.parameters {
        let param = param.as_parameter();
        let modifiers = parameter_modifiers(&source, param);
        if modifiers.local || modifiers.once {
            locals.insert(
                param.name.as_str(),
                BorrowDeclaration::own(param.name.range()),
            );
        }
        if modifiers.once {
            once.insert(
                param.name.as_str(),
                BorrowDeclaration::own(param.name.range()),
            );
        }
    }
    if let Some(inherited) = inherited {
        inherited.seed(&mut locals, &mut once);
    }
    if locals.is_empty() {
        return;
    }

    LocalArgChecker {
        context,
        locals: &locals,
        once: &once,
        callee_type: &callee_type,
    }
    .visit_body(&function.body);
}

struct LocalArgChecker<'a, 'db, 'ast, F> {
    context: &'a InferContext<'db, 'ast>,
    /// every borrowed parameter (`local` or `once`) → its declaration range
    locals: &'a FxHashMap<&'ast str, BorrowDeclaration>,
    /// the subset that is `once`
    once: &'a FxHashMap<&'ast str, BorrowDeclaration>,
    callee_type: &'a F,
}

impl<'db, 'ast, F> Visitor<'ast> for LocalArgChecker<'_, 'db, 'ast, F>
where
    F: Fn(&'ast Expr) -> Option<Type<'db>>,
{
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // a nested function / class is its own scope; the `local` parameter is
        // not in scope there (a capture is a separate, deferred concern)
        if matches!(stmt, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            return;
        }
        ruff_python_ast::visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            let callee = (self.callee_type)(&call.func);
            for (index, arg) in call.arguments.args.iter().enumerate() {
                self.check_argument(arg, callee, ArgTarget::Positional(index));
            }
            for keyword in &call.arguments.keywords {
                if let Some(name) = &keyword.arg {
                    self.check_argument(&keyword.value, callee, ArgTarget::Keyword(name.as_str()));
                }
            }
        }
        walk_expr(self, expr);
    }
}

impl<'db, 'ast, F> LocalArgChecker<'_, 'db, 'ast, F> {
    fn check_argument(&self, arg: &'ast Expr, callee: Option<Type<'db>>, target: ArgTarget<'_>) {
        let Some(name) = surface_local(arg, self.locals) else {
            return;
        };
        let Some(callee) = callee else {
            return;
        };
        let id = name.id.as_str();
        let source_is_once = self.once.contains_key(id);
        if !argument_escapes(self.context.db(), callee, target, source_is_once) {
            return;
        }
        let kind = if source_is_once { "once" } else { "local" };
        let Some(builder) = self.context.report_lint(&ESCAPING_LOCAL, name) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "{kind} `{id}` cannot escape the call: it is passed as a non-`{kind}` argument"
        ));
        if let Some(&decl) = self.locals.get(id) {
            diagnostic.annotate(
                self.context
                    .secondary(decl.range)
                    .message(decl.describe(id, kind)),
            );
        }
    }
}

/// Whether a borrowed argument bound to `target` of `callee` escapes. A `once`
/// source (`source_is_once`) escapes unless the target parameter is itself
/// `once`; a plain `local` source escapes unless the target is a borrow (`local`
/// or `once`). `false` (leave it alone) when the callee is not a resolvable
/// function / bound method, or the target parameter cannot be found (`*args` past
/// the declared parameters is an arity concern handled elsewhere).
fn argument_escapes<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
    target: ArgTarget<'_>,
    source_is_once: bool,
) -> bool {
    // resolve to a single function definition and the offset of bound parameters
    // (a bound method has already consumed `self`)
    let (function, bound) = match callee {
        Type::FunctionLiteral(function) => (function, 0usize),
        Type::BoundMethod(method) => (method.function(db), 1usize),
        _ => return false,
    };
    let modifiers = function
        .literal(db)
        .last_definition
        .callback_parameter_modifiers(db);

    // `Some((borrowed, once))` for the resolved parameter; `None` when the
    // argument binds to nothing declared (wrong arity, unknown keyword)
    let target: Option<(bool, bool)> = match target {
        ArgTarget::Positional(index) => {
            let index = index + bound;
            if let (Some(&borrowed), Some(&once)) = (
                modifiers.positional_borrowed.get(index),
                modifiers.positional_once.get(index),
            ) {
                Some((borrowed, once))
            } else {
                // beyond the declared positionals, the argument feeds `*args`
                modifiers.vararg_borrowed.zip(modifiers.vararg_once)
            }
        }
        ArgTarget::Keyword(name) => modifiers
            .keyword_names
            .iter()
            .position(|param| param.as_str() == name)
            .map(|position| {
                (
                    modifiers.keyword_borrowed[position],
                    modifiers.keyword_once[position],
                )
            })
            .or(modifiers.kwarg_borrowed.zip(modifiers.kwarg_once)),
    };

    // an unresolved target is not our concern; otherwise a `once` value needs a
    // `once` recipient, a plain `local` value needs any borrow recipient
    match target {
        Some((borrowed, once)) => {
            if source_is_once {
                !once
            } else {
                !borrowed
            }
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// `once`: a callback that must be called exactly once on every normal path.
// ---------------------------------------------------------------------------

/// What the body does with a single `once` callback.
#[derive(Default)]
struct OnceCallInfo {
    /// number of unconditional direct calls (reached on every path)
    unconditional: u32,
    /// range of the second unconditional call, for the `once-called-twice` span
    second_unconditional: Option<TextRange>,
    /// range of a direct call inside a loop (may run any number of times)
    in_loop: Option<TextRange>,
}

/// Report [`ONCE_NOT_CALLED`] / [`ONCE_CALLED_TWICE`] for each `once` callback.
///
/// The analysis is deliberately conservative. `once-not-called` fires only when
/// the callback is never mentioned at all — a value that is passed on (and might
/// be called elsewhere) is left alone. `once-called-twice` fires on two
/// unconditional calls or a call inside a loop; mutually-exclusive branch calls
/// (`if c: done() else: done()`) are one call on every path and are not flagged.
fn check_once_callbacks<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    body: &'ast [Stmt],
    once: &FxHashMap<&'ast str, BorrowDeclaration>,
    receiver_call: &dyn Fn(&'ast ast::ExprAttribute) -> bool,
) {
    // a callback mentioned anywhere — including a nested scope — might still be
    // called, so it is not "never called"
    let mut referenced: FxHashSet<&'ast str> = FxHashSet::default();
    RefCollector {
        once,
        referenced: &mut referenced,
        receiver_call,
    }
    .visit_body(body);

    let mut counter = OnceCounter {
        once,
        info: FxHashMap::default(),
        loop_depth: 0,
        cond_depth: 0,
        receiver_call,
    };
    counter.visit_body(body);

    // deterministic order (by declaration) so diagnostics are stable
    let mut ordered: Vec<(&'ast str, BorrowDeclaration)> =
        once.iter().map(|(&name, &decl)| (name, decl)).collect();
    ordered.sort_by_key(|(_, decl)| decl.range.start());

    for (name, decl) in ordered {
        if !referenced.contains(name) {
            if let Some(builder) = context.report_lint(&ONCE_NOT_CALLED, decl.range) {
                builder.into_diagnostic(format_args!("once callback `{name}` is never called"));
            }
            continue;
        }
        let info = counter.info.get(name);
        let twice = info
            .and_then(|info| info.in_loop)
            .or_else(|| info.and_then(|info| info.second_unconditional));
        if let Some(range) = twice
            && let Some(builder) = context.report_lint(&ONCE_CALLED_TWICE, range)
        {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "once callback `{name}` may be called more than once"
            ));
            diagnostic.annotate(
                context
                    .secondary(decl.range)
                    .message(decl.describe(name, "once")),
            );
        }
    }
}

/// Marks which `once` callbacks are referenced anywhere in the body, descending
/// into nested scopes (a capture may still call the callback later).
struct RefCollector<'a, 'ast> {
    once: &'a FxHashMap<&'ast str, BorrowDeclaration>,
    referenced: &'a mut FxHashSet<&'ast str>,
    receiver_call: &'a dyn Fn(&'ast ast::ExprAttribute) -> bool,
}

impl<'ast> Visitor<'ast> for RefCollector<'_, 'ast> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Name(name) if self.once.contains_key(name.id.as_str()) => {
                self.referenced.insert(name.id.as_str());
            }
            // a receiver callable is also spelled `x.cb` — the same callback,
            // reached through its declared receiver rather than by bare name
            Expr::Attribute(attribute) => {
                if let Some((&name, _)) = self.once.get_key_value(attribute.attr.as_str())
                    && (self.receiver_call)(attribute)
                {
                    self.referenced.insert(name);
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// Counts direct calls of each `once` callback, tracking whether each call is
/// unconditional or inside a loop. Nested scopes are skipped (a callback call
/// there is a separate concern), as are conditional expression contexts
/// (`a and done()`, `done() if c else ...`) so a call is only counted where it
/// definitely runs.
struct OnceCounter<'a, 'ast> {
    once: &'a FxHashMap<&'ast str, BorrowDeclaration>,
    info: FxHashMap<&'ast str, OnceCallInfo>,
    loop_depth: u32,
    cond_depth: u32,
    receiver_call: &'a dyn Fn(&'ast ast::ExprAttribute) -> bool,
}

impl<'ast> OnceCounter<'_, 'ast> {
    /// the `once` callback `callee` calls, whether it is spelled by bare name
    /// or through the callback's own [receiver](super::receivers) (`x.cb()`)
    fn called_once_callback(&self, callee: &'ast Expr) -> Option<&'ast str> {
        match callee {
            Expr::Name(name) => self
                .once
                .get_key_value(name.id.as_str())
                .map(|(&name, _)| name),
            Expr::Attribute(attribute) => self
                .once
                .get_key_value(attribute.attr.as_str())
                .filter(|_| (self.receiver_call)(attribute))
                .map(|(&name, _)| name),
            _ => None,
        }
    }

    fn record(&mut self, name: &'ast str, range: TextRange) {
        let info = self.info.entry(name).or_default();
        if self.loop_depth > 0 {
            info.in_loop.get_or_insert(range);
        } else if self.cond_depth == 0 {
            info.unconditional += 1;
            if info.unconditional == 2 {
                info.second_unconditional = Some(range);
            }
        }
    }

    /// Records direct `once` calls reachable within `expr` without passing
    /// through a branch, loop, lambda, or comprehension.
    fn scan(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Call(call) => {
                if let Some(name) = self.called_once_callback(call.func.as_ref()) {
                    self.record(name, call.range());
                }
                self.scan(&call.func);
                for arg in &call.arguments.args {
                    self.scan(arg);
                }
                for keyword in &call.arguments.keywords {
                    self.scan(&keyword.value);
                }
            }
            Expr::Attribute(attribute) => self.scan(&attribute.value),
            Expr::Subscript(subscript) => {
                self.scan(&subscript.value);
                self.scan(&subscript.slice);
            }
            Expr::Starred(starred) => self.scan(&starred.value),
            Expr::Await(await_) => self.scan(&await_.value),
            Expr::Tuple(tuple) => tuple.elts.iter().for_each(|e| self.scan(e)),
            Expr::List(list) => list.elts.iter().for_each(|e| self.scan(e)),
            Expr::Set(set) => set.elts.iter().for_each(|e| self.scan(e)),
            Expr::Dict(dict) => {
                for item in &dict.items {
                    if let Some(key) = &item.key {
                        self.scan(key);
                    }
                    self.scan(&item.value);
                }
            }
            // a branch / loop / nested-scope expression context is not counted
            _ => {}
        }
    }
}

impl<'ast> StatementVisitor<'ast> for OnceCounter<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::If(if_) => {
                self.scan(&if_.test);
                self.cond_depth += 1;
                self.visit_body(&if_.body);
                for clause in &if_.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.scan(test);
                    }
                    self.visit_body(&clause.body);
                }
                self.cond_depth -= 1;
            }
            Stmt::While(while_) => {
                self.scan(&while_.test);
                self.loop_depth += 1;
                self.visit_body(&while_.body);
                self.loop_depth -= 1;
                self.cond_depth += 1;
                self.visit_body(&while_.orelse);
                self.cond_depth -= 1;
            }
            Stmt::For(for_) => {
                self.scan(&for_.iter);
                self.loop_depth += 1;
                self.visit_body(&for_.body);
                self.loop_depth -= 1;
                self.cond_depth += 1;
                self.visit_body(&for_.orelse);
                self.cond_depth -= 1;
            }
            Stmt::With(with) => {
                for item in &with.items {
                    self.scan(&item.context_expr);
                }
                self.visit_body(&with.body);
            }
            Stmt::Try(try_) => {
                self.cond_depth += 1;
                self.visit_body(&try_.body);
                for handler in &try_.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    self.visit_body(&handler.body);
                }
                self.visit_body(&try_.orelse);
                self.cond_depth -= 1;
                // a `finally` block always runs
                self.visit_body(&try_.finalbody);
            }
            Stmt::Match(match_) => {
                self.scan(&match_.subject);
                self.cond_depth += 1;
                for case in &match_.cases {
                    if let Some(guard) = &case.guard {
                        self.scan(guard);
                    }
                    self.visit_body(&case.body);
                }
                self.cond_depth -= 1;
            }
            Stmt::Return(ret) => {
                if let Some(value) = ret.value.as_deref() {
                    self.scan(value);
                }
            }
            Stmt::Expr(expr) => self.scan(&expr.value),
            Stmt::Assign(assign) => self.scan(&assign.value),
            Stmt::AnnAssign(ann) => {
                if let Some(value) = ann.value.as_deref() {
                    self.scan(value);
                }
            }
            Stmt::AugAssign(aug) => self.scan(&aug.value),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// non-`once` trailing-lambda blocks: no non-local control flow.
// ---------------------------------------------------------------------------

/// basedpython: report non-local control flow in a non-`once` trailing-lambda
/// block. Only a `once` block runs exactly once (`with`-like), so only there may
/// control flow target the enclosing scope; a non-`once` block is an ordinary
/// closure that may run any number of times.
///
/// This covers only `return`: because the block is a function scope, a `break` /
/// `continue` that would leave it is already a `break`-outside-loop syntax error
/// (with the right loop-depth analysis — one inside a block-local loop is fine),
/// so re-reporting it here would only double the diagnostic.
pub(super) fn check_non_once_trailing_lambda<'ast>(
    context: &InferContext<'_, 'ast>,
    function: &'ast ast::StmtFunctionDef,
) {
    ControlFlowChecker { context }.visit_body(&function.body);
}

struct ControlFlowChecker<'a, 'db, 'ast> {
    context: &'a InferContext<'db, 'ast>,
}

impl<'ast> StatementVisitor<'ast> for ControlFlowChecker<'_, '_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a nested function / class is its own `return` target
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            Stmt::Return(ret) => {
                if let Some(builder) = self.context.report_lint(&TRAILING_LAMBDA_CONTROL_FLOW, ret)
                {
                    builder.into_diagnostic(
                        "`return` is not allowed in a non-`once` trailing-lambda block — \
                         it would leave the block, not the enclosing scope",
                    );
                }
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

// ---------------------------------------------------------------------------
// non-`once` trailing-lambda blocks: no writing an enclosing `let` / `final`.
// ---------------------------------------------------------------------------

/// basedpython: a non-`once` trailing-lambda block may run more than once, so an
/// assignment there to a name an enclosing scope declares `let` / `final` could
/// bind that `Final` repeatedly. Report each such assignment.
pub(super) fn check_non_once_trailing_lambda_final_writes<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    block_scope: FileScopeId,
    function: &'ast ast::StmtFunctionDef,
) {
    FinalWriteChecker {
        context,
        index,
        module,
        block_scope,
    }
    .visit_body(&function.body);
}

struct FinalWriteChecker<'a, 'db, 'ast> {
    context: &'a InferContext<'db, 'ast>,
    index: &'ast SemanticIndex<'db>,
    module: &'ast ParsedModuleRef,
    block_scope: FileScopeId,
}

impl<'ast> StatementVisitor<'ast> for FinalWriteChecker<'_, '_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a nested scope's assignments rebind its own names, not ours
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    self.check_target(target);
                }
            }
            Stmt::AnnAssign(ann) if ann.value.is_some() => self.check_target(&ann.target),
            Stmt::AugAssign(aug) => self.check_target(&aug.target),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

impl<'ast> FinalWriteChecker<'_, '_, 'ast> {
    fn check_target(&self, target: &'ast Expr) {
        // only a bare-name target rebinds an enclosing name
        let Expr::Name(name) = target else {
            return;
        };
        let Some(declaration) = self.enclosing_final_declaration(name.id.as_str()) else {
            return;
        };
        let Some(builder) = self.context.report_lint(&INVALID_ASSIGNMENT, name) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` is `Final`, so a non-`once` trailing-lambda block cannot assign it",
            name.id
        ));
        diagnostic.annotate(
            self.context
                .secondary(declaration)
                .message(format_args!("`{}` declared `Final` here", name.id)),
        );
    }

    /// The range of the enclosing `let` / `final` declaration `name` resolves to,
    /// if any. Mirrors the write-back's resolution: the nearest ancestor scope
    /// that binds or declares the name is the one that matters, so a nearer,
    /// non-`final` binding shadows a farther `let`.
    fn enclosing_final_declaration(&self, name: &str) -> Option<TextRange> {
        for (scope_id, scope) in self.index.ancestor_scopes(self.block_scope).skip(1) {
            let table = self.index.place_table(scope_id);
            let Some(symbol_id) = table.symbol_id(name) else {
                continue;
            };
            let symbol = table.symbol(symbol_id);
            if !(symbol.is_bound() || symbol.is_declared()) {
                continue;
            }
            // the nearest resolving scope; only a `let` / `final` there bans the write
            let body = match scope.node() {
                NodeWithScopeKind::Function(func) => &func.node(self.module).body,
                NodeWithScopeKind::Module => &self.module.syntax().body,
                _ => return None,
            };
            return final_declaration_range(body, name);
        }
        None
    }
}

/// The range of `name`'s `let` / `final` declaration in `body` (`__let__` /
/// `__final__` marker, bare or subscripted), i.e. a `Final` in a function /
/// module scope. Only the scope's own statements are inspected — a declaration
/// is always at its scope's top level.
fn final_declaration_range(body: &[ast::Stmt], name: &str) -> Option<TextRange> {
    fn marker(annotation: &ast::Expr) -> Option<&str> {
        match annotation {
            ast::Expr::Name(n) => Some(n.id.as_str()),
            ast::Expr::Subscript(s) => match s.value.as_ref() {
                ast::Expr::Name(n) => Some(n.id.as_str()),
                _ => None,
            },
            _ => None,
        }
    }
    body.iter().find_map(|stmt| {
        let ast::Stmt::AnnAssign(ann) = stmt else {
            return None;
        };
        let ast::Expr::Name(target) = ann.target.as_ref() else {
            return None;
        };
        (target.id.as_str() == name
            && matches!(marker(&ann.annotation), Some("__let__" | "__final__")))
        .then(|| target.range())
    })
}

// ---------------------------------------------------------------------------
// loop-variable capture: a trailing-lambda block that may outlive its loop.
// ---------------------------------------------------------------------------

/// basedpython: the type-aware complement to ruff's `B023`. A trailing-lambda
/// block inside a loop that captures a loop variable is a late-binding trap only
/// when the callee can defer the block past the loop; a `local` / `once` callee
/// runs it synchronously, so those are safe. Report each captured loop variable
/// when the callee resolves to a non-borrow function / bound method. Runs over a
/// module or function body (`body`), with `callee_type` resolving a callee's type.
pub(super) fn check_loop_variable_capture<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    body: &'ast [Stmt],
    callee_type: impl Fn(&'ast Expr) -> Option<Type<'db>>,
) {
    walk_for_blocks(context, body, &FxHashSet::default(), false, &callee_type);
}

/// Walks `stmts`, tracking the names assigned by every enclosing loop, and checks
/// each trailing-lambda block that appears inside one.
fn walk_for_blocks<'db, 'ast, F>(
    context: &InferContext<'db, 'ast>,
    stmts: &'ast [Stmt],
    loop_assigned: &FxHashSet<&'ast str>,
    in_loop: bool,
    callee_type: &F,
) where
    F: Fn(&'ast Expr) -> Option<Type<'db>>,
{
    for stmt in stmts {
        match stmt {
            Stmt::For(for_) => {
                let mut assigned = loop_assigned.clone();
                collect_target_names(&for_.target, &mut assigned);
                collect_loop_assignments(&for_.body, &mut assigned);
                walk_for_blocks(context, &for_.body, &assigned, true, callee_type);
                walk_for_blocks(context, &for_.orelse, loop_assigned, in_loop, callee_type);
            }
            Stmt::While(while_) => {
                let mut assigned = loop_assigned.clone();
                collect_loop_assignments(&while_.body, &mut assigned);
                walk_for_blocks(context, &while_.body, &assigned, true, callee_type);
                walk_for_blocks(context, &while_.orelse, loop_assigned, in_loop, callee_type);
            }
            // a trailing-lambda block is the case we check. its body is a scope
            // of its own — but a `once` block runs inline, exactly once, so a
            // block nested inside it sits inside the loop as much as the `once`
            // block does, and its own callee decides whether it is confined.
            // (a non-borrow block already reports every capture in its body,
            // nested blocks included, so only a `once` one is entered)
            Stmt::FunctionDef(func) if func.is_trailing_lambda => {
                if in_loop {
                    check_block_capture(context, func, loop_assigned, callee_type);
                    if block_callee_is_once(context, func, callee_type) {
                        walk_for_blocks(context, &func.body, loop_assigned, in_loop, callee_type);
                    }
                }
            }
            // an ordinary nested function / class is its own scope
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::If(if_) => {
                walk_for_blocks(context, &if_.body, loop_assigned, in_loop, callee_type);
                for clause in &if_.elif_else_clauses {
                    walk_for_blocks(context, &clause.body, loop_assigned, in_loop, callee_type);
                }
            }
            Stmt::With(with) => {
                walk_for_blocks(context, &with.body, loop_assigned, in_loop, callee_type);
            }
            Stmt::Try(try_) => {
                walk_for_blocks(context, &try_.body, loop_assigned, in_loop, callee_type);
                for handler in &try_.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    walk_for_blocks(context, &handler.body, loop_assigned, in_loop, callee_type);
                }
                walk_for_blocks(context, &try_.orelse, loop_assigned, in_loop, callee_type);
                walk_for_blocks(
                    context,
                    &try_.finalbody,
                    loop_assigned,
                    in_loop,
                    callee_type,
                );
            }
            Stmt::Match(match_) => {
                for case in &match_.cases {
                    walk_for_blocks(context, &case.body, loop_assigned, in_loop, callee_type);
                }
            }
            _ => {}
        }
    }
}

/// The type of `block`'s callee. A block directly in the body being checked has
/// it in that body's inference (`callee_type`); one nested inside a `once` block
/// has its callee in the `once` block's own scope, which that inference never
/// enters — so it is read from the callee's standalone inference instead, which
/// the semantic index registers for every trailing-lambda callee (and which the
/// block's own `it` typing already reads, so it is not a cycle).
fn block_callee_type<'db, 'ast, F>(
    context: &InferContext<'db, 'ast>,
    block: &'ast ast::StmtFunctionDef,
    callee_type: &F,
) -> Option<Type<'db>>
where
    F: Fn(&'ast Expr) -> Option<Type<'db>>,
{
    let callee = block.trailing_lambda_callee()?;
    if let Some(ty) = callee_type(callee) {
        return Some(ty);
    }
    let db = context.db();
    let expression = semantic_index(db, context.program_file()).try_expression(callee)?;
    infer_expression_types(db, expression, TypeContext::default()).try_expression_type(callee)
}

/// Whether `block`'s callee marks its callback `once` — the block then runs
/// inline, exactly once. Anything unresolvable is not `once`.
fn block_callee_is_once<'db, 'ast, F>(
    context: &InferContext<'db, 'ast>,
    block: &'ast ast::StmtFunctionDef,
    callee_type: &F,
) -> bool
where
    F: Fn(&'ast Expr) -> Option<Type<'db>>,
{
    block_callee_type(context, block, callee_type).is_some_and(|callee| {
        crate::types::trailing_lambda::callee_callback_is_once(context.db(), callee)
    })
}

/// Reports each loop variable a trailing-lambda block captures, unless its callee
/// confines the block (a `local` / `once` callee) or cannot be resolved.
fn check_block_capture<'db, 'ast, F>(
    context: &InferContext<'db, 'ast>,
    block: &'ast ast::StmtFunctionDef,
    loop_assigned: &FxHashSet<&'ast str>,
    callee_type: &F,
) where
    F: Fn(&'ast Expr) -> Option<Type<'db>>,
{
    // a `local` / `once` callee runs the block synchronously (safe); an opaque
    // callee is left alone. only a resolved non-borrow callee is a concern
    let resolved_non_borrow = block_callee_type(context, block, callee_type)
        .and_then(|callee| {
            crate::types::trailing_lambda::callee_callback_is_borrowed(context.db(), callee)
        })
        .is_some_and(|borrowed| !borrowed);
    if !resolved_non_borrow {
        return;
    }

    let mut names = CapturedNames::default();
    names.visit_body(&block.body);
    for name in &names.loaded {
        let id = name.id.as_str();
        // a name the block also binds is a block local, not a capture
        if names.stored.iter().any(|stored| stored.id == name.id) {
            continue;
        }
        if block.parameters.includes(&name.id) {
            continue;
        }
        if loop_assigned.contains(id)
            && let Some(builder) = context.report_lint(&ESCAPING_LOOP_VARIABLE, *name)
        {
            builder.into_diagnostic(format_args!(
                "trailing-lambda block captures loop variable `{id}`: its callee is not \
                 `local` / `once`, so it may run the block after the loop advances, when \
                 `{id}` holds its final value"
            ));
        }
    }
}

/// Collects the `Name` expressions loaded and stored within a body, for finding a
/// block's captured (loaded-but-not-bound) free variables.
#[derive(Default)]
struct CapturedNames<'ast> {
    loaded: Vec<&'ast ExprName>,
    stored: Vec<&'ast ExprName>,
}

impl<'ast> Visitor<'ast> for CapturedNames<'ast> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Name(name) => match name.ctx {
                ExprContext::Load => self.loaded.push(name),
                ExprContext::Store => self.stored.push(name),
                ExprContext::Invalid | ExprContext::Del => {}
            },
            _ => walk_expr(self, expr),
        }
    }
}

/// Inserts the bare `Name`s bound by an assignment target (`a`, `a, b`, `[a, *b]`).
fn collect_target_names<'ast>(target: &'ast Expr, out: &mut FxHashSet<&'ast str>) {
    match target {
        Expr::Name(name) => {
            out.insert(name.id.as_str());
        }
        Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .for_each(|elt| collect_target_names(elt, out)),
        Expr::List(list) => list
            .elts
            .iter()
            .for_each(|elt| collect_target_names(elt, out)),
        Expr::Starred(starred) => collect_target_names(&starred.value, out),
        _ => {}
    }
}

/// Collects every name assigned within a loop body — assignment targets and
/// nested `for` targets — without descending into a nested scope's own bindings.
fn collect_loop_assignments<'ast>(body: &'ast [Stmt], out: &mut FxHashSet<&'ast str>) {
    for stmt in body {
        match stmt {
            // a nested scope's assignments are its own
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    collect_target_names(target, out);
                }
            }
            Stmt::AnnAssign(ann) => collect_target_names(&ann.target, out),
            Stmt::AugAssign(aug) => collect_target_names(&aug.target, out),
            Stmt::For(for_) => {
                collect_target_names(&for_.target, out);
                collect_loop_assignments(&for_.body, out);
                collect_loop_assignments(&for_.orelse, out);
            }
            Stmt::While(while_) => {
                collect_loop_assignments(&while_.body, out);
                collect_loop_assignments(&while_.orelse, out);
            }
            Stmt::If(if_) => {
                collect_loop_assignments(&if_.body, out);
                for clause in &if_.elif_else_clauses {
                    collect_loop_assignments(&clause.body, out);
                }
            }
            Stmt::With(with) => collect_loop_assignments(&with.body, out),
            Stmt::Try(try_) => {
                collect_loop_assignments(&try_.body, out);
                for handler in &try_.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_loop_assignments(&handler.body, out);
                }
                collect_loop_assignments(&try_.orelse, out);
                collect_loop_assignments(&try_.finalbody, out);
            }
            Stmt::Match(match_) => {
                for case in &match_.cases {
                    collect_loop_assignments(&case.body, out);
                }
            }
            _ => {}
        }
    }
}
