//! basedpython: static enforcement of `local` and `once` parameters.
//!
//! [`check_local_lifetimes`] runs two intraprocedural analyses over a function
//! body:
//!
//! - **`local`** — a borrowed parameter must not outlive the call. Reports
//!   [`ESCAPING_LOCAL`] when it is returned, stored on a parameter-rooted object
//!   (`self.cb = fn`), or bound to a `global` / `nonlocal` name.
//! - **`once`** — a callback must be called exactly once. Reports
//!   [`ONCE_NOT_CALLED`] when it is never called and [`ONCE_CALLED_TWICE`] on two
//!   unconditional calls or a call inside a loop.
//!
//! Both are deliberately conservative — they flag only what they can see
//! directly, never guessing through opaque calls, aliasing, or closures. See
//! `docs/basedpython/features/local-lifetimes.md`.

use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_python_ast::helpers::parameter_modifiers;
use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{self as ast, Expr, ExprName, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_python_core::SemanticIndex;
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind};

use crate::Db;

use super::Type;
use super::context::InferContext;
use super::diagnostic::{
    ESCAPING_LOCAL, INVALID_ASSIGNMENT, ONCE_CALLED_TWICE, ONCE_NOT_CALLED,
    TRAILING_LAMBDA_CONTROL_FLOW,
};

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

/// basedpython entry point: enforce `local` (no escape) and `once` (exactly one
/// call) on `function`'s parameters.
pub(super) fn check_local_lifetimes<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    function: &'ast ast::StmtFunctionDef,
) {
    let source = source_text(context.db(), context.file());

    // `local` / `once` parameter names → the range of their declaration, plus
    // the set of every parameter name. a parameter's attributes / items outlive
    // the call, because the caller holds the value bound to the parameter
    let mut locals: FxHashMap<&'ast str, TextRange> = FxHashMap::default();
    let mut once: FxHashMap<&'ast str, TextRange> = FxHashMap::default();
    let mut params: FxHashSet<&'ast str> = FxHashSet::default();
    for param in &function.parameters {
        let param = param.as_parameter();
        params.insert(param.name.as_str());
        let modifiers = parameter_modifiers(&source, param);
        if modifiers.local {
            locals.insert(param.name.as_str(), param.name.range());
        }
        if modifiers.once {
            once.insert(param.name.as_str(), param.name.range());
        }
    }

    if !locals.is_empty() {
        // names rebound to an outer scope by a `global` / `nonlocal` declaration
        let mut outer_names: FxHashSet<&'ast str> = FxHashSet::default();
        OuterNameCollector {
            names: &mut outer_names,
        }
        .visit_body(&function.body);

        EscapeChecker {
            context,
            locals: &locals,
            params: &params,
            outer_names: &outer_names,
        }
        .visit_body(&function.body);
    }

    if !once.is_empty() {
        check_once_callbacks(context, &function.body, &once);
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
    locals: &'a FxHashMap<&'ast str, TextRange>,
    params: &'a FxHashSet<&'ast str>,
    outer_names: &'a FxHashSet<&'ast str>,
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
            Expr::Attribute(_) | Expr::Subscript(_) => {
                root_name(target).is_some_and(|root| self.params.contains(root))
            }
            Expr::Name(name) => self.outer_names.contains(name.id.as_str()),
            _ => false,
        };
        if outlives && let Some(name) = self.surface_local(value) {
            self.report(name, EscapeRoute::Stored);
        }
    }

    fn report(&self, name: &ExprName, route: EscapeRoute) {
        let id = name.id.as_str();
        let Some(builder) = self.context.report_lint(&ESCAPING_LOCAL, name) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "local `{id}` cannot escape the call: it is {}",
            route.describe()
        ));
        if let Some(&decl) = self.locals.get(id) {
            diagnostic.annotate(
                self.context
                    .secondary(decl)
                    .message(format_args!("`{id}` is declared `local` here")),
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
    locals: &FxHashMap<&str, TextRange>,
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
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// `local` propagation: a borrow may only be passed to another `local` parameter.
// ---------------------------------------------------------------------------

/// The parameter an argument binds to.
#[derive(Clone, Copy)]
enum ArgTarget<'a> {
    /// the `n`-th positional argument of the call
    Positional(usize),
    /// a keyword argument `name=…`
    Keyword(&'a str),
}

/// basedpython: a `local` parameter is a borrow that must not escape, so it may
/// only be passed on to another `local` parameter. Report each argument that
/// hands a `local` to a parameter that is not itself `local`. The callee's
/// signature must be resolvable (a plain function or bound method); an opaque
/// callee is left alone, since we cannot see the parameter's declaration.
pub(super) fn check_local_argument_passing<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    function: &'ast ast::StmtFunctionDef,
    callee_type: impl Fn(&'ast Expr) -> Option<Type<'db>>,
) {
    let source = source_text(context.db(), context.file());
    let mut locals: FxHashMap<&'ast str, TextRange> = FxHashMap::default();
    for param in &function.parameters {
        let param = param.as_parameter();
        if parameter_modifiers(&source, param).local {
            locals.insert(param.name.as_str(), param.name.range());
        }
    }
    if locals.is_empty() {
        return;
    }

    LocalArgChecker {
        context,
        locals: &locals,
        callee_type: &callee_type,
    }
    .visit_body(&function.body);
}

struct LocalArgChecker<'a, 'db, 'ast, F> {
    context: &'a InferContext<'db, 'ast>,
    locals: &'a FxHashMap<&'ast str, TextRange>,
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
        if !argument_binds_to_non_local(self.context.db(), callee, target) {
            return;
        }
        let id = name.id.as_str();
        let Some(builder) = self.context.report_lint(&ESCAPING_LOCAL, name) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "local `{id}` cannot escape the call: it is passed as a non-`local` argument"
        ));
        if let Some(&decl) = self.locals.get(id) {
            diagnostic.annotate(
                self.context
                    .secondary(decl)
                    .message(format_args!("`{id}` is declared `local` here")),
            );
        }
    }
}

/// Whether an argument bound to `target` of `callee` reaches a parameter that is
/// *not* `local`. `false` (leave it alone) when the callee is not a resolvable
/// function / bound method, or the target parameter cannot be found (`*args` past
/// the declared parameters is an arity concern handled elsewhere), or that
/// parameter is itself `local`.
fn argument_binds_to_non_local<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
    target: ArgTarget<'_>,
) -> bool {
    // resolve to a single function definition and the offset of bound parameters
    // (a bound method has already consumed `self`)
    let (function, bound) = match callee {
        Type::FunctionLiteral(function) => (function, 0usize),
        Type::BoundMethod(method) => (method.function(db), 1usize),
        _ => return false,
    };
    let overload = function.literal(db).last_definition;
    let file = overload.file(db);
    let module = parsed_module(db, file).load(db);
    let parameters = &overload.node(db, file, &module).parameters;
    let source = source_text(db, file);

    let parameter: Option<&ast::Parameter> = match target {
        ArgTarget::Positional(index) => {
            let index = index + bound;
            let positional = parameters.posonlyargs.len() + parameters.args.len();
            if index < parameters.posonlyargs.len() {
                Some(&parameters.posonlyargs[index].parameter)
            } else if index < positional {
                Some(&parameters.args[index - parameters.posonlyargs.len()].parameter)
            } else {
                // beyond the declared positionals, the argument feeds `*args`
                parameters.vararg.as_deref()
            }
        }
        ArgTarget::Keyword(name) => parameters
            .args
            .iter()
            .chain(parameters.kwonlyargs.iter())
            .map(|param| &param.parameter)
            .find(|param| param.name.as_str() == name)
            .or(parameters.kwarg.as_deref()),
    };

    // an unresolved target (wrong arity, unknown keyword) is not our concern; a
    // parameter that is itself `local` correctly accepts the borrow
    parameter.is_some_and(|parameter| !parameter_modifiers(&source, parameter).local)
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
    once: &FxHashMap<&'ast str, TextRange>,
) {
    // a callback mentioned anywhere — including a nested scope — might still be
    // called, so it is not "never called"
    let mut referenced: FxHashSet<&'ast str> = FxHashSet::default();
    RefCollector {
        once,
        referenced: &mut referenced,
    }
    .visit_body(body);

    let mut counter = OnceCounter {
        once,
        info: FxHashMap::default(),
        loop_depth: 0,
        cond_depth: 0,
    };
    counter.visit_body(body);

    // deterministic order (by declaration) so diagnostics are stable
    let mut ordered: Vec<(&'ast str, TextRange)> =
        once.iter().map(|(&name, &decl)| (name, decl)).collect();
    ordered.sort_by_key(|(_, decl)| decl.start());

    for (name, decl) in ordered {
        if !referenced.contains(name) {
            if let Some(builder) = context.report_lint(&ONCE_NOT_CALLED, decl) {
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
                    .secondary(decl)
                    .message(format_args!("`{name}` is declared `once` here")),
            );
        }
    }
}

/// Marks which `once` callbacks are referenced anywhere in the body, descending
/// into nested scopes (a capture may still call the callback later).
struct RefCollector<'a, 'ast> {
    once: &'a FxHashMap<&'ast str, TextRange>,
    referenced: &'a mut FxHashSet<&'ast str>,
}

impl<'ast> Visitor<'ast> for RefCollector<'_, 'ast> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(name) = expr
            && self.once.contains_key(name.id.as_str())
        {
            self.referenced.insert(name.id.as_str());
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
    once: &'a FxHashMap<&'ast str, TextRange>,
    info: FxHashMap<&'ast str, OnceCallInfo>,
    loop_depth: u32,
    cond_depth: u32,
}

impl<'ast> OnceCounter<'_, 'ast> {
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
                if let Expr::Name(name) = call.func.as_ref()
                    && self.once.contains_key(name.id.as_str())
                {
                    self.record(name.id.as_str(), call.range());
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
