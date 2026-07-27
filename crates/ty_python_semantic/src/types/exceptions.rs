//! basedpython: static tracking of the exceptions a function can raise.
//!
//! Every function has an *exception set* — a [`Type`] that is the union of the
//! exception instances that can escape a call to it. `Never` means the function
//! cannot raise; a dynamic type means the set is unknown and nothing is checked
//! against it.
//!
//! A function's set is its declared `raises` clause when it has one, and
//! otherwise the set inferred from its body. Because the clause is an ordinary
//! type expression, the whole feature reuses the type system rather than adding
//! a parallel algebra: `raises Never` cannot raise, `raises A | B` is a union,
//! `raises not TypeError` is everything but that, `raises ...` opts out, and
//! "does the body stay inside the declaration?" is assignability.
//!
//! Inference is deliberately limited to what is visible in the body:
//!
//! - `raise X` and `raise X(...)`, plus bare `raise` inside a handler
//! - `assert`, which raises `AssertionError`
//! - calls to functions whose body is visible, transitively
//!
//! Everything else contributes nothing. In particular a call into a stub — the
//! standard library, any third-party dependency — raises nothing as far as this
//! analysis is concerned, until that stub carries a `raises` clause of its own.
//! That is the only workable default: assuming an unannotated callee raises
//! anything would make every set `BaseException`.
//!
//! `try` narrows the set: exceptions raised in the `try` body that an `except`
//! clause catches do not escape, while the handler, `else` and `finally` bodies
//! contribute their own raises. `except*` is treated as catching nothing, since
//! what escapes it is a regrouped `ExceptionGroup`.
//!
//! Known gaps, deliberate for now: context-manager `__enter__` / `__exit__`,
//! constructor calls, operators and other implicit dunder dispatch, and a
//! `finally` block that swallows an in-flight exception by returning. The
//! `raises` clause also does not participate in callable assignability.
//!
//! See `docs/basedpython/features/exceptions.md`.

use ruff_db::diagnostic::Annotation;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::helpers::is_dunder;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::Definition;
use ty_python_core::scope::ScopeId;

use crate::Db;
use crate::types::context::InferContext;
use crate::types::diagnostic::{
    INVALID_RAISES_CLAUSE, OVERRIDE_RAISE, UNDECLARED_RAISE, UNHANDLED_EXCEPTION,
};
use crate::types::function::{FunctionLiteral, FunctionType, OverloadLiteral};
use crate::types::{
    ClassType, KnownClass, Type, TypeContext, UnionType, definition_expression_type,
    infer_scope_types,
};

/// One `raise` in a function body whose exception is not handled there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct RaiseEffect<'db> {
    /// the exception instance type that escapes
    raised: Type<'db>,
    /// the `raise` or `assert` statement it escapes from
    range: TextRange,
}

/// One call in a function body whose exceptions are not fully handled there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct CallEffect<'db> {
    /// the called function, whose own exception set is resolved separately
    callee: FunctionLiteral<'db>,
    /// the exception instance types caught by the `except` clauses around the call
    caught: Box<[Type<'db>]>,
    /// the call expression
    range: TextRange,
}

/// What a function body does that can raise, with its callees left unresolved.
///
/// Splitting the analysis here is what keeps the recursion cheap and safe:
/// collecting the effects reads the function's own inferred expression types,
/// while [`resolve_effects`] walks the call graph over effects alone and never
/// re-enters type inference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct ExceptionEffects<'db> {
    /// exceptions raised directly and not caught in this body
    direct: Box<[RaiseEffect<'db>]>,
    /// calls whose exceptions are not fully caught in this body
    calls: Box<[CallEffect<'db>]>,
}

impl ExceptionEffects<'_> {
    /// Whether this body can raise nothing at all, without resolving any callee.
    pub(crate) fn is_empty(&self) -> bool {
        self.direct.is_empty() && self.calls.is_empty()
    }
}

/// The exceptions a call to `overload` can raise: its declared `raises` clause
/// when it has one, and otherwise the set inferred from its body.
pub(crate) fn raised_exceptions<'db>(db: &'db dyn Db, overload: OverloadLiteral<'db>) -> Type<'db> {
    declared_exceptions(db, overload).unwrap_or_else(|| inferred_exceptions(db, overload))
}

/// The exceptions a call to `function` can raise.
///
/// An overloaded function contributes the union of what its overloads and its
/// implementation may raise. Which overload a given call matched is not known
/// here, so this is an upper bound — deliberately, since the safe direction for
/// an escape check is to name an exception that cannot happen rather than to
/// miss one that can.
pub(crate) fn function_raised_exceptions<'db>(
    db: &'db dyn Db,
    function: FunctionLiteral<'db>,
) -> Type<'db> {
    UnionType::from_elements(
        db,
        function
            .iter_overloads_and_implementation(db)
            .map(|overload| raised_exceptions(db, overload))
            .collect::<Vec<_>>(),
    )
}

/// The type named by `overload`'s `raises` clause, or `None` when it has none.
///
/// `raises ...` is the gradual set: it declares that the function may raise
/// anything, which is what a dynamic type already means here.
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| None,
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn declared_exceptions<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Option<Type<'db>> {
    let file = overload.file(db);
    if !file.source_type(db).is_basedpython() {
        return None;
    }
    let module = parsed_module(db, file).load(db);
    let raises = overload.node(db, file, &module).raises.as_deref()?;

    if raises.is_ellipsis_literal_expr() {
        return Some(Type::unknown());
    }

    Some(definition_expression_type(
        db,
        overload.definition(db),
        raises,
    ))
}

/// The exceptions `overload`'s body can raise, ignoring any declared clause.
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| Type::Never,
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn inferred_exceptions<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Type<'db> {
    resolve_effects(
        db,
        body_exception_effects(db, overload),
        Some(overload.body_scope(db)),
    )
}

/// [`ExceptionEffects`] for `overload`'s body, read off its own inferred types.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| ExceptionEffects::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn body_exception_effects<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> ExceptionEffects<'db> {
    let file = overload.file(db);
    if !file.source_type(db).is_basedpython() {
        return ExceptionEffects::default();
    }
    let module = parsed_module(db, file).load(db);
    let node = overload.node(db, file, &module);
    let inference = infer_scope_types(db, overload.body_scope(db), TypeContext::default());

    collect_exception_effects(db, &node.body, |expr| inference.expression_type(expr))
}

/// Union the exceptions escaping `effects`, following each call into its callee.
pub(crate) fn resolve_effects<'db>(
    db: &'db dyn Db,
    effects: &ExceptionEffects<'db>,
    self_body_scope: Option<ScopeId<'db>>,
) -> Type<'db> {
    UnionType::from_elements(
        db,
        escaping_sites(db, effects, self_body_scope, &[])
            .into_iter()
            .map(|(_, raised)| raised),
    )
}

/// Each place in a body that can raise something none of `allowed` covers,
/// paired with what escapes there.
///
/// `self_body_scope` is the scope of the function the effects belong to, when it
/// is known: a directly recursive call contributes exactly the set being
/// computed, so it is the identity of the union and can be dropped rather than
/// re-entered.
pub(crate) fn escaping_sites<'db>(
    db: &'db dyn Db,
    effects: &ExceptionEffects<'db>,
    self_body_scope: Option<ScopeId<'db>>,
    allowed: &[Type<'db>],
) -> Vec<(TextRange, Type<'db>)> {
    let direct = effects
        .direct
        .iter()
        .filter_map(|raise| Some((raise.range, escaping(db, raise.raised, allowed)?)));

    let from_calls = effects
        .calls
        .iter()
        .filter(|call| {
            !call
                .callee
                .iter_overloads_and_implementation(db)
                .any(|overload| Some(overload.body_scope(db)) == self_body_scope)
        })
        .filter_map(|call| {
            let raised = escaping(
                db,
                function_raised_exceptions(db, call.callee),
                &call.caught,
            )?;
            Some((call.range, escaping(db, raised, allowed)?))
        });

    direct.chain(from_calls).collect()
}

/// The part of `raised` that no type in `caught` handles, or `None` when it is
/// caught entirely.
///
/// A union is filtered element-wise, so `except TypeError` around code raising
/// `TypeError | ValueError` leaves `ValueError` behind rather than nothing or
/// everything.
pub(crate) fn escaping<'db>(
    db: &'db dyn Db,
    raised: Type<'db>,
    caught: &[Type<'db>],
) -> Option<Type<'db>> {
    if raised.is_never() {
        return None;
    }

    let escaped = UnionType::from_elements(
        db,
        union_elements(db, raised).into_iter().filter(|element| {
            // a dynamic member is an unknown exception, not a known one: it is
            // what `raises ...` declares, and what an unreadable `raise` leaves
            // behind. reporting it would be reporting the absence of knowledge
            !element.is_dynamic()
                && !caught
                    .iter()
                    .any(|caught| element.is_assignable_to(db, *caught))
        }),
    );

    (!escaped.is_never()).then_some(escaped)
}

/// The members of `ty` when it is a union, and `ty` itself otherwise.
pub(crate) fn union_elements<'db>(db: &'db dyn Db, ty: Type<'db>) -> Vec<Type<'db>> {
    match ty {
        Type::Union(union) => union.elements(db).to_vec(),
        _ => vec![ty],
    }
}

/// Collect the [`ExceptionEffects`] of `body`.
///
/// `expression_type` supplies inferred types for expressions in the body. It is
/// a callback so that the check for the function currently being inferred can
/// read that in-progress inference rather than re-entering it as a query.
pub(crate) fn collect_exception_effects<'db>(
    db: &'db dyn Db,
    body: &[Stmt],
    expression_type: impl Fn(&Expr) -> Type<'db>,
) -> ExceptionEffects<'db> {
    let mut collector = EffectsCollector {
        db,
        expression_type,
        caught: Vec::new(),
        handling: Vec::new(),
        direct: Vec::new(),
        calls: Vec::new(),
    };
    collector.visit_body(body);

    ExceptionEffects {
        direct: collector.direct.into_boxed_slice(),
        calls: collector.calls.into_boxed_slice(),
    }
}

struct EffectsCollector<'db, F> {
    db: &'db dyn Db,
    expression_type: F,
    /// the exception types caught by the `except` clauses currently enclosing
    /// the node being visited, innermost last
    caught: Vec<Type<'db>>,
    /// the exception types bound by the `except` handlers whose bodies enclose
    /// the node being visited — what a bare `raise` re-raises
    handling: Vec<Type<'db>>,
    direct: Vec<RaiseEffect<'db>>,
    calls: Vec<CallEffect<'db>>,
}

impl<'db, F> EffectsCollector<'db, F>
where
    F: Fn(&Expr) -> Type<'db>,
{
    fn visit_try(&mut self, try_stmt: &ast::StmtTry) {
        // an `except*` clause does not simply catch what it names — what escapes
        // it is a regrouped `ExceptionGroup` — so it is treated as catching
        // nothing rather than pretending either way
        let caught: Vec<Type<'db>> = if try_stmt.is_star {
            Vec::new()
        } else {
            try_stmt
                .handlers
                .iter()
                .map(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    self.caught_type(handler.type_.as_deref())
                })
                .collect()
        };

        let depth = self.caught.len();
        self.caught.extend(caught.iter().copied());
        self.visit_body(&try_stmt.body);
        self.caught.truncate(depth);

        for (index, handler) in try_stmt.handlers.iter().enumerate() {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            if let Some(type_) = handler.type_.as_deref() {
                self.visit_expr(type_);
            }

            // a bare `raise` in the handler re-raises what it caught; an
            // `except*` handler binds a group, which this analysis does not model
            self.handling.push(
                caught
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| KnownClass::BaseException.to_instance(self.db)),
            );
            self.visit_body(&handler.body);
            self.handling.pop();
        }

        // `else` and `finally` run outside the protection of the handlers above
        self.visit_body(&try_stmt.orelse);
        self.visit_body(&try_stmt.finalbody);
    }

    /// The exception instance type an `except` clause catches. A bare `except:`
    /// catches everything, and so does a clause this analysis cannot read.
    fn caught_type(&self, type_: Option<&Expr>) -> Type<'db> {
        let Some(type_) = type_ else {
            return KnownClass::BaseException.to_instance(self.db);
        };

        let caught = (self.expression_type)(type_);
        if let Some(tuple) = caught.tuple_instance_spec(self.db) {
            return UnionType::from_elements(
                self.db,
                tuple
                    .iter_element_types(self.db)
                    .map(|element| self.exception_instance(element))
                    .collect::<Vec<_>>(),
            );
        }

        self.exception_instance(caught)
    }

    fn record_raise(&mut self, raise: &ast::StmtRaise) {
        let range = raise.range();
        let Some(exception) = raise.exc.as_deref() else {
            // a bare `raise` re-raises what the enclosing handler caught; outside
            // any handler python raises `RuntimeError`
            let reraised = self
                .handling
                .last()
                .copied()
                .unwrap_or_else(|| KnownClass::RuntimeError.to_instance(self.db));
            self.record_escaping(reraised, range);
            return;
        };

        self.visit_expr(exception);
        let raised = self.exception_instance((self.expression_type)(exception));
        self.record_escaping(raised, range);
    }

    /// Read `ty` as the exception instance it produces: `raise TypeError` names
    /// the class, `raise TypeError(...)` and `raise err` name an instance.
    fn exception_instance(&self, ty: Type<'db>) -> Type<'db> {
        if ty.is_assignable_to(self.db, KnownClass::BaseException.to_subclass_of(self.db)) {
            ty.to_instance_approximation(self.db)
                .unwrap_or_else(|| KnownClass::BaseException.to_instance(self.db))
        } else {
            ty
        }
    }

    /// Record `raised` as raised at `range`, minus whatever the enclosing
    /// handlers catch.
    fn record_escaping(&mut self, raised: Type<'db>, range: TextRange) {
        if let Some(escaping) = escaping(self.db, raised, &self.caught) {
            self.direct.push(RaiseEffect {
                raised: escaping,
                range,
            });
        }
    }

    /// Record a call to `callee`, minus whatever the enclosing handlers catch.
    fn record_call(&mut self, callee: FunctionLiteral<'db>, range: TextRange) {
        self.calls.push(CallEffect {
            callee,
            caught: self.caught.clone().into_boxed_slice(),
            range,
        });
    }
}

impl<'db, F> Visitor<'_> for EffectsCollector<'db, F>
where
    F: Fn(&Expr) -> Type<'db>,
{
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            // a nested function does not run where it is defined; its own body is
            // analysed when something calls it. its decorators and defaults do run
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                for default in function
                    .parameters
                    .iter_non_variadic_params()
                    .filter_map(|param| param.default.as_deref())
                {
                    self.visit_expr(default);
                }
            }

            Stmt::Raise(raise) => {
                self.record_raise(raise);
                if let Some(cause) = raise.cause.as_deref() {
                    self.visit_expr(cause);
                }
            }

            Stmt::Assert(assert) => {
                walk_stmt(self, stmt);
                self.record_escaping(
                    KnownClass::AssertionError.to_instance(self.db),
                    assert.range(),
                );
            }

            Stmt::Try(try_stmt) => self.visit_try(try_stmt),

            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            // a lambda body does not run where it is written, but its parameter
            // defaults are evaluated there
            Expr::Lambda(lambda) => {
                for default in lambda
                    .parameters
                    .iter()
                    .flat_map(|parameters| parameters.iter_non_variadic_params())
                    .filter_map(|param| param.default.as_deref())
                {
                    self.visit_expr(default);
                }
                return;
            }
            Expr::Call(call) => {
                if let Some(callee) = callee_function(self.db, (self.expression_type)(&call.func)) {
                    self.record_call(callee, call.range());
                }
            }
            _ => {}
        }

        walk_expr(self, expr);
    }
}

/// The function a call resolves to, when it is one whose body can be analysed.
///
/// The whole literal is returned rather than a single overload: which overload a
/// call matched is not known here, so resolution unions over all of them.
///
/// Callables, unions of callables, overload sets matched by argument, and
/// constructor calls are all left alone: this analysis reports nothing rather
/// than guessing at a set it cannot see.
fn callee_function<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<FunctionLiteral<'db>> {
    let function = match callee {
        Type::FunctionLiteral(function) => function,
        Type::BoundMethod(method) => method.function(db),
        _ => return None,
    };

    let literal = function.literal(db);
    let overload = literal.last_definition;
    // a dunder is reached through implicit dispatch far more often than through a
    // written call, and this analysis only sees the written ones. reporting the
    // visible half of a set would be worse than reporting none of it
    if is_dunder(overload.name(db)) {
        return None;
    }

    Some(literal)
}

/// basedpython: report an override that can raise more than the method it
/// overrides.
///
/// A call is checked against the type it can see, so when a base method cannot
/// raise, nothing at a call on the base type says an exception can escape — yet
/// a subclass substituted for it can still raise. Bounding every override by its
/// base closes that hole, at the cost of making a base method's exception set
/// part of its contract, so it is off by default.
pub(super) fn check_override_raises<'db>(
    context: &InferContext<'db, '_>,
    member: &str,
    subclass_function: FunctionType<'db>,
    superclass_function: FunctionType<'db>,
    superclass: ClassType<'db>,
) {
    let db = context.db();
    // resolving both sets walks two call graphs, so do nothing at all unless the
    // strictness option asked for it
    if !context.is_lint_enabled(&OVERRIDE_RAISE) {
        return;
    }

    let allowed = function_raised_exceptions(db, superclass_function.literal(db));
    let raised = function_raised_exceptions(db, subclass_function.literal(db));
    let Some(extra) = escaping(db, raised, &[allowed]) else {
        return;
    };

    let overriding = subclass_function.literal(db).last_definition;
    let range = overriding.spans(db).signature.range().unwrap_or_else(|| {
        subclass_function
            .node(db, context.file(), context.module())
            .range
    });

    let Some(builder) = context.report_lint(&OVERRIDE_RAISE, range) else {
        return;
    };
    let mut diagnostic = builder.into_diagnostic(format_args!(
        "`{member}` can raise `{}`, which the method it overrides cannot",
        extra.display(db)
    ));
    let base = superclass.name(db);
    let annotation = Annotation::secondary(
        superclass_function
            .literal(db)
            .last_definition
            .spans(db)
            .signature,
    );
    diagnostic.annotate(if allowed.is_never() {
        annotation.message(format_args!("`{base}.{member}` cannot raise"))
    } else {
        annotation.message(format_args!(
            "`{base}.{member}` raises only `{}`",
            allowed.display(db)
        ))
    });
}

/// basedpython entry point: check `function`'s body against what it is allowed
/// to raise.
///
/// The set is bounded by the `raises` clause when there is one, and by nothing
/// at all otherwise — an undeclared function simply propagates to its callers.
/// The one exception is `main`, the program entry point, which has no caller to
/// propagate to.
pub(super) fn check_function_exceptions<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    function: &'ast ast::StmtFunctionDef,
    body_scope: ScopeId<'db>,
    definition: Definition<'db>,
    expression_type: impl Fn(&Expr) -> Type<'db>,
) {
    let db = context.db();
    if !context.file().source_type(db).is_basedpython() {
        return;
    }

    // the clause belongs to the definition, not to the body being inferred here,
    // so its type comes from that definition's own (deferred) inference
    let declared = function
        .raises
        .as_deref()
        .map(|raises| (raises, declared_clause_type(db, definition, raises)));

    if let Some((clause, declared)) = declared {
        check_raises_clause_is_exceptions(context, clause, declared);
    }

    let allowed = match declared {
        Some((_, declared)) => vec![declared],
        // an undeclared function propagates to its callers, except for `main`,
        // which has none
        None if is_entry_point(db, function, body_scope) => Vec::new(),
        None => return,
    };

    let effects = collect_exception_effects(db, &function.body, expression_type);
    if effects.is_empty() {
        return;
    }

    for (range, escaped) in escaping_sites(db, &effects, Some(body_scope), &allowed) {
        let name = &function.name.id;
        if declared.is_some() {
            let Some(builder) = context.report_lint(&UNDECLARED_RAISE, range) else {
                continue;
            };
            builder.into_diagnostic(format_args!(
                "`{name}` can raise `{}`, which its `raises` clause does not include",
                escaped.display(db)
            ));
        } else {
            let Some(builder) = context.report_lint(&UNHANDLED_EXCEPTION, range) else {
                continue;
            };
            builder.into_diagnostic(format_args!(
                "`{}` can escape `{name}`, the entry point",
                escaped.display(db)
            ));
        }
    }
}

/// The exception set a `raises` clause declares.
fn declared_clause_type<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    clause: &Expr,
) -> Type<'db> {
    if clause.is_ellipsis_literal_expr() {
        Type::unknown()
    } else {
        definition_expression_type(db, definition, clause)
    }
}

/// Report a `raises` clause that cannot describe any exception at all.
///
/// The test is overlap rather than assignability, so a negated set such as
/// `raises not TypeError` — which does contain exceptions, among other things —
/// is accepted while `raises int` is not.
fn check_raises_clause_is_exceptions<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    clause: &'ast Expr,
    declared: Type<'db>,
) {
    let db = context.db();
    if declared.is_never() || declared.is_dynamic() {
        return;
    }
    if !declared.is_disjoint_from(db, KnownClass::BaseException.to_instance(db)) {
        return;
    }

    if let Some(builder) = context.report_lint(&INVALID_RAISES_CLAUSE, clause) {
        builder.into_diagnostic(format_args!(
            "`{}` contains no exception, so nothing can satisfy this `raises` clause",
            declared.display(db)
        ));
    }
}

/// The `isinstance` target for a function's declared exception set, for a
/// runtime guard on the lowered function.
///
/// `None` when there is no faithful runtime test — a gradual clause, or a set
/// whose members have no runtime spelling (a negation, a protocol). `Never`
/// becomes the empty tuple, which no exception is an instance of.
pub fn declared_raises_runtime_target<'db>(
    db: &'db dyn Db,
    file: ruff_db::files::File,
    function: Type<'db>,
) -> Option<String> {
    let Type::FunctionLiteral(function) = function else {
        return None;
    };
    let declared = declared_exceptions(db, function.literal(db).last_definition)?;

    if declared.is_dynamic() {
        return None;
    }
    if declared.is_never() {
        return Some("()".to_string());
    }

    crate::types::soundness::runtime_check_target(db, file, declared)
}

/// Whether `function` is the module's entry point — a `main` defined directly at
/// module level, which the lowering wires up to a `__main__` guard.
fn is_entry_point<'db>(
    db: &'db dyn Db,
    function: &ast::StmtFunctionDef,
    body_scope: ScopeId<'db>,
) -> bool {
    // a function body scope always has a parent, and the entry point's is the
    // module itself
    function.name.id == "main"
        && body_scope
            .scope(db)
            .parent()
            .is_some_and(ty_python_core::FileScopeId::is_global)
}
