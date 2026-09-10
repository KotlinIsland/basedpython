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
//! A clause may name a type parameter — `def f[T: OSError](e: T) raises T` — and
//! then what escapes a particular call is the set with what that call solved the
//! parameter to. So the parameter has to be declared an exception, since it
//! stands for one type the caller chooses.
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

use std::cell::RefCell;

use ruff_db::diagnostic::Annotation;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::helpers::is_dunder;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, PySourceType, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::Definition;
use ty_python_core::scope::{NodeWithScopeKind, ScopeId};
use ty_python_core::{ProgramFile, semantic_index};

use crate::Db;
use crate::reified::{method_receiver, reified_class_reads, reified_type_param_names};
use crate::types::ProgramEnvironment;
use crate::types::context::InferContext;
use crate::types::diagnostic::{
    INVALID_RAISES_CLAUSE, OVERRIDE_RAISE, UNDECLARED_RAISE, UNHANDLED_EXCEPTION,
};
use crate::types::function::{FunctionLiteral, FunctionType, OverloadLiteral};
use crate::types::generics::{ApplySpecialization, Specialization, enclosing_binding_contexts};
use crate::types::typevar::{BindingContext, BoundTypeVarInstance, TypeVarBoundOrConstraints};
use crate::types::visitor::any_over_type;
use crate::types::{
    ClassType, KnownClass, Type, TypeContext, TypeMapping, UnionType, definition_expression_type,
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

/// What a call solved its callee's own type parameters to, recorded by inference.
///
/// A callee's exception set is written in terms of its type parameters, so what escapes a
/// particular call depends on what that call solved them to. Only the call itself knows — a
/// solution can come from the expected type as much as from the arguments — so inference records
/// it where it binds the call, rather than the exception analysis binding the call a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum CallSolution<'db> {
    /// The call bound, solving the callee's type parameters to this.
    Solved(Specialization<'db>),
    /// The call did not bind, so nothing is known about what it solved.
    Unbound,
}

/// One call in a function body whose exceptions are not fully handled there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct CallEffect<'db> {
    /// the called function as the call sees it: its type carries whatever
    /// specializations it went through on the way, as `f[OSError]`,
    /// `Reader[KeyError].read` and a bound method all do
    callee: FunctionType<'db>,
    /// what the call itself solved the callee's own type parameters to
    solution: Option<CallSolution<'db>>,
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
fn raised_exceptions<'db>(db: &'db dyn Db, overload: OverloadLiteral<'db>) -> Type<'db> {
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
    env: &ProgramEnvironment<'db>,
    function: FunctionLiteral<'db>,
) -> Type<'db> {
    UnionType::from_elements(
        db,
        env,
        function
            .iter_overloads_and_implementation(db)
            .map(|overload| raised_exceptions(db, overload))
            .collect::<Vec<_>>(),
    )
}

/// The exceptions a call through `function` can raise: the set of the function it
/// is a type of, specialized the way that type has been.
fn function_type_raised_exceptions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    function: FunctionType<'db>,
) -> Type<'db> {
    function.applied_specializations(db).iter().fold(
        function_raised_exceptions(db, env, function.literal(db)),
        |raised, specialization| substitute_solution(db, env, raised, *specialization),
    )
}

/// `ty` with each type parameter `solution` solves replaced by what it solved it to.
///
/// The parameters are matched as parameters, not as occurrences: a call binds a fresh
/// occurrence of its callee's type parameters, so a solution is keyed on that, while the
/// callee's set is written in the source-level one. `f(KeyError())` inside
/// `def f[T](e: T)` solves the fresh `T` to `KeyError`, and that is the `T` of `f`'s clause.
fn substitute_solution<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    solution: Specialization<'db>,
) -> Type<'db> {
    let solved: Vec<(BoundTypeVarInstance<'db>, Type<'db>)> = solution
        .generic_context(db)
        .variables(db)
        .zip(solution.types(db).iter().copied())
        .collect();
    let solved_for = |bound_typevar: BoundTypeVarInstance<'db>| {
        solved
            .iter()
            .find(|(variable, _)| variable.is_occurrence_of_same_parameter(db, bound_typevar))
            .map(|(_, ty)| *ty)
    };
    widen_type_parameters(
        db,
        env,
        ty,
        |bound_typevar| solved_for(bound_typevar).is_some(),
        |bound_typevar| solved_for(bound_typevar).unwrap_or(Type::TypeVar(bound_typevar)),
    )
}

/// Whether applying `specialization` to `function` can change what a call to it raises — which
/// is when its [`FunctionType`] has to remember the specialization.
///
/// Only a function with an exception set of its own has one to change: a `.by` function, whose
/// body may raise, or a stub that declares a `raises` clause. And a specialization only reaches
/// that set when it substitutes type parameters the function can name — its own, or those of the
/// class or function it is defined in. Anything else, such as the solution of a generic call the
/// function is merely passed to, substitutes nothing the set can mention.
pub(crate) fn specialization_reaches_exception_set<'db>(
    db: &'db dyn Db,
    function: FunctionLiteral<'db>,
    specialization: Specialization<'db>,
) -> bool {
    let overload = function.last_definition;
    let has_exception_set = match overload.file(db).source_type(db) {
        PySourceType::BasedPython => true,
        PySourceType::BasedPythonStub => function
            .iter_overloads_and_implementation(db)
            .any(|overload| declares_raises(db, overload)),
        PySourceType::Python | PySourceType::Stub | PySourceType::Ipynb => false,
    };
    if !has_exception_set {
        return false;
    }

    let Some(substituted) = specialization
        .generic_context(db)
        .variables(db)
        .next()
        .map(|variable| variable.binding_context(db))
    else {
        return false;
    };
    enclosing_binding_contexts(
        semantic_index(db, overload.program_file(db)),
        overload.body_scope(db).file_scope_id(db),
    )
    .any(|visible| visible == substituted)
}

/// Whether `overload` writes a `raises` clause, read off the syntax alone.
#[salsa::tracked(returns(copy), heap_size = ruff_memory_usage::heap_size)]
fn declares_raises<'db>(db: &'db dyn Db, overload: OverloadLiteral<'db>) -> bool {
    let module = parsed_module(db, overload.python_file(db)).load(db);
    overload
        .node(db, overload.file(db), &module)
        .raises
        .is_some()
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
    let module = parsed_module(db, overload.python_file(db)).load(db);
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
    cycle_fn = |db, cycle: &salsa::Cycle, _previous: &Type<'db>, current: Type<'db>, overload: OverloadLiteral<'db>| {
        if cycle.iteration() <= crate::TAINTED_CYCLES {
            current
        } else {
            // a recursive call that solves a type parameter to something built from it
            // (`f(Wrapped(e))` inside `def f[T](e: T)`) adds to the set on every round.
            // with every type parameter at its ceiling nothing is left to substitute, so
            // the next round reproduces this one
            let env = &ProgramEnvironment::from_file(overload.program_file(db));
            widen_to_ceilings(db, env, current)
        }
    },
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn inferred_exceptions<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Type<'db> {
    let env = &ProgramEnvironment::from_file(overload.program_file(db));
    let body_scope = overload.body_scope(db);
    let visible = visible_binding_contexts(db, overload.program_file(db), body_scope);
    resolve_effects(
        db,
        env,
        body_exception_effects(db, overload),
        &CallSite {
            body_scope,
            visible: &visible,
            resolves_recursion: true,
        },
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
    let env = &ProgramEnvironment::from_file(overload.program_file(db));
    let file = overload.file(db);
    if !file.source_type(db).is_basedpython() {
        return ExceptionEffects::default();
    }
    let module = parsed_module(db, overload.python_file(db)).load(db);
    let node = overload.node(db, file, &module);
    let inference = infer_scope_types(db, overload.body_scope(db), TypeContext::default());

    collect_exception_effects(
        db,
        env,
        &node.body,
        |expr| inference.expression_type(expr),
        |call| inference.call_solution(call),
    )
}

/// Where a body's calls are made from, which decides what the type parameters in
/// their callees' sets can name.
struct CallSite<'a, 'db> {
    /// the body the calls are written in
    body_scope: ScopeId<'db>,
    /// the binding contexts whose type parameters are in scope in that body
    visible: &'a [BindingContext<'db>],
    /// whether a call back into this body's own function may be resolved, which
    /// for a function without a declared clause means computing its set from
    /// inside the computation of that same set
    resolves_recursion: bool,
}

/// The binding contexts whose type parameters name something inside `body_scope`:
/// the function's own, and those of every class and function it is nested in.
fn visible_binding_contexts<'db>(
    db: &'db dyn Db,
    program_file: ProgramFile<'db>,
    body_scope: ScopeId<'db>,
) -> Vec<BindingContext<'db>> {
    enclosing_binding_contexts(
        semantic_index(db, program_file),
        body_scope.file_scope_id(db),
    )
    .collect()
}

/// Union the exceptions escaping `effects`, following each call into its callee.
fn resolve_effects<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    effects: &ExceptionEffects<'db>,
    site: &CallSite<'_, 'db>,
) -> Type<'db> {
    UnionType::from_elements(
        db,
        env,
        escaping_sites(db, env, effects, site, &[])
            .into_iter()
            .map(|(_, raised)| raised),
    )
}

/// Each place in a body that can raise something none of `allowed` covers,
/// paired with what escapes there.
fn escaping_sites<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    effects: &ExceptionEffects<'db>,
    site: &CallSite<'_, 'db>,
    allowed: &[Type<'db>],
) -> Vec<(TextRange, Type<'db>)> {
    let direct = effects
        .direct
        .iter()
        .filter_map(|raise| Some((raise.range, escaping(db, env, raise.raised, allowed)?)));

    let from_calls = effects
        .calls
        .iter()
        .filter(|call| !skips_recursive_call(db, call, site))
        .filter_map(|call| {
            let raised = escaping(
                db,
                env,
                call_raised_exceptions(db, env, call, site),
                &call.caught,
            )?;
            Some((call.range, escaping(db, env, raised, allowed)?))
        });

    direct.chain(from_calls).collect()
}

/// Whether `call` is a call back into the function whose body `site` is, and is
/// left out of that body's set.
///
/// A function with a declared clause is resolved from the declaration, never
/// from its body, so a call back into it is resolved like any other call. Without
/// one, the call raises what the body is being computed to raise, which is where
/// two cases part:
///
/// - a call that changes nothing — no specialization on the way, and a solution
///   that substitutes nothing — contributes exactly the set being computed. It is
///   the identity of the union, and dropping it is exact
/// - a call that does change something (`f(KeyError())` inside `def f[T](e: T)`)
///   raises the set with that change, so it has to be resolved. That is a fixpoint
///   over [`inferred_exceptions`], which is only safe where that query is what is
///   running: the check on a body is inside the body's own inference, and
///   resolving there would re-enter it
fn skips_recursive_call<'db>(
    db: &'db dyn Db,
    call: &CallEffect<'db>,
    site: &CallSite<'_, 'db>,
) -> bool {
    let callee = call.callee.literal(db);
    if !callee
        .iter_overloads_and_implementation(db)
        .any(|overload| overload.body_scope(db) == site.body_scope)
    {
        return false;
    }
    if callee
        .iter_overloads_and_implementation(db)
        .any(|overload| declared_exceptions(db, overload).is_some())
    {
        return false;
    }
    let changes_nothing = call.callee.applied_specializations(db).is_empty()
        && !matches!(call.solution, Some(CallSolution::Solved(_)));
    changes_nothing || !site.resolves_recursion
}

/// The exceptions escaping one call: the callee's set, specialized the way the
/// call sees the callee and by what the call solved.
///
/// A type parameter still left in it is one the call did not solve — an explicit
/// specialization does not survive into anything a call can read, and a call that
/// fails to bind solves nothing. Whether that matters depends on where the call is
/// written. A parameter bound by something enclosing the call names a real type
/// there: a caller's own `U` passed on to `f(u)`, a closure over its enclosing
/// function's `T`, a method's `self.m()` within its class. Anything else names
/// nothing at the call, so it stands for everything it was declared to allow —
/// and a `raises` clause may only name a parameter whose ceiling is exceptions.
/// For a call that did not bind, it stands for nothing known at all.
fn call_raised_exceptions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    call: &CallEffect<'db>,
    site: &CallSite<'_, 'db>,
) -> Type<'db> {
    let raised = function_type_raised_exceptions(db, env, call.callee);
    let raised = match call.solution {
        Some(CallSolution::Solved(solution)) => substitute_solution(db, env, raised, solution),
        Some(CallSolution::Unbound) | None => raised,
    };
    widen_type_parameters(
        db,
        env,
        raised,
        |bound_typevar| !site.visible.contains(&bound_typevar.binding_context(db)),
        |bound_typevar| match call.solution {
            Some(CallSolution::Unbound) => Type::unknown(),
            Some(CallSolution::Solved(_)) | None => {
                bound_typevar.typevar(db).declared_ceiling(db, env)
            }
        },
    )
}

/// `ty` with every type parameter in it widened to its declared ceiling.
fn widen_to_ceilings<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Type<'db> {
    widen_type_parameters(
        db,
        env,
        ty,
        |_| true,
        |bound_typevar| bound_typevar.typevar(db).declared_ceiling(db, env),
    )
}

/// `ty` with every type parameter `widen` accepts replaced by `replacement` of it.
fn widen_type_parameters<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    widen: impl Fn(BoundTypeVarInstance<'db>) -> bool,
    replacement: impl Fn(BoundTypeVarInstance<'db>) -> Type<'db>,
) -> Type<'db> {
    let widened = RefCell::new(Vec::new());
    any_over_type(db, env, ty, false, |nested| {
        if let Type::TypeVar(bound_typevar) = nested
            && !widened.borrow().contains(&bound_typevar)
            && widen(bound_typevar)
        {
            widened.borrow_mut().push(bound_typevar);
        }
        false
    });

    widened
        .into_inner()
        .into_iter()
        .fold(ty, |ty, bound_typevar| {
            ty.apply_type_mapping(
                db,
                env,
                &TypeMapping::ApplySpecialization(ApplySpecialization::Single(
                    bound_typevar,
                    replacement(bound_typevar),
                )),
                TypeContext::default(),
            )
        })
}

/// The part of `raised` that no type in `caught` handles, or `None` when it is
/// caught entirely.
///
/// A union is filtered element-wise, so `except TypeError` around code raising
/// `TypeError | ValueError` leaves `ValueError` behind rather than nothing or
/// everything.
fn escaping<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    raised: Type<'db>,
    caught: &[Type<'db>],
) -> Option<Type<'db>> {
    if raised.is_never() {
        return None;
    }

    let escaped = UnionType::from_elements(
        db,
        env,
        union_elements(db, raised).into_iter().filter(|element| {
            // a dynamic member is an unknown exception, not a known one: it is
            // what `raises ...` declares, and what an unreadable `raise` leaves
            // behind. reporting it would be reporting the absence of knowledge
            !element.is_dynamic()
                && !caught
                    .iter()
                    .any(|caught| element.is_assignable_to(db, env, *caught))
        }),
    );

    (!escaped.is_never()).then_some(escaped)
}

/// The members of `ty` when it is a union, and `ty` itself otherwise.
fn union_elements<'db>(db: &'db dyn Db, ty: Type<'db>) -> Vec<Type<'db>> {
    match ty {
        Type::Union(union) => union.elements(db).to_vec(),
        _ => vec![ty],
    }
}

/// Collect the [`ExceptionEffects`] of `body`.
///
/// `expression_type` supplies inferred types for expressions in the body, and
/// `call_solution` what each call solved its callee's type parameters to. They
/// are callbacks so that the check for the function currently being inferred can
/// read that in-progress inference rather than re-entering it as a query.
fn collect_exception_effects<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    body: &[Stmt],
    expression_type: impl Fn(&Expr) -> Type<'db>,
    call_solution: impl Fn(&ast::ExprCall) -> Option<CallSolution<'db>>,
) -> ExceptionEffects<'db> {
    let mut collector = EffectsCollector {
        db,
        env: env.clone(),
        expression_type,
        call_solution,
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

struct EffectsCollector<'db, F, G> {
    db: &'db dyn Db,
    env: ProgramEnvironment<'db>,
    expression_type: F,
    call_solution: G,
    /// the exception types caught by the `except` clauses currently enclosing
    /// the node being visited, innermost last
    caught: Vec<Type<'db>>,
    /// the exception types bound by the `except` handlers whose bodies enclose
    /// the node being visited — what a bare `raise` re-raises
    handling: Vec<Type<'db>>,
    direct: Vec<RaiseEffect<'db>>,
    calls: Vec<CallEffect<'db>>,
}

impl<'db, F, G> EffectsCollector<'db, F, G>
where
    F: Fn(&Expr) -> Type<'db>,
    G: Fn(&ast::ExprCall) -> Option<CallSolution<'db>>,
{
    fn visit_try(&mut self, try_stmt: &ast::StmtTry) {
        let env = self.env.clone();
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
                    .unwrap_or_else(|| KnownClass::BaseException.to_instance(self.db, &env)),
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
        let env = self.env.clone();
        let Some(type_) = type_ else {
            return KnownClass::BaseException.to_instance(self.db, &env);
        };

        let caught = (self.expression_type)(type_);
        if let Some(tuple) = caught.tuple_instance_spec(self.db, &env) {
            return UnionType::from_elements(
                self.db,
                &env,
                tuple
                    .iter_element_types(self.db)
                    .map(|element| self.exception_instance(element))
                    .collect::<Vec<_>>(),
            );
        }

        self.exception_instance(caught)
    }

    fn record_raise(&mut self, raise: &ast::StmtRaise) {
        let env = self.env.clone();
        let range = raise.range();
        let Some(exception) = raise.exc.as_deref() else {
            // a bare `raise` re-raises what the enclosing handler caught; outside
            // any handler python raises `RuntimeError`
            let reraised = self
                .handling
                .last()
                .copied()
                .unwrap_or_else(|| KnownClass::RuntimeError.to_instance(self.db, &env));
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
        let env = self.env.clone();
        if ty.is_assignable_to(
            self.db,
            &env,
            KnownClass::BaseException.to_subclass_of(self.db, &env),
        ) {
            ty.to_instance_approximation(self.db, &env)
                .unwrap_or_else(|| KnownClass::BaseException.to_instance(self.db, &env))
        } else {
            ty
        }
    }

    /// Record `raised` as raised at `range`, minus whatever the enclosing
    /// handlers catch.
    fn record_escaping(&mut self, raised: Type<'db>, range: TextRange) {
        let env = self.env.clone();
        if let Some(escaping) = escaping(self.db, &env, raised, &self.caught) {
            self.direct.push(RaiseEffect {
                raised: escaping,
                range,
            });
        }
    }

    /// Record a call to `callee`, minus whatever the enclosing handlers catch.
    fn record_call(
        &mut self,
        callee: FunctionType<'db>,
        solution: Option<CallSolution<'db>>,
        range: TextRange,
    ) {
        self.calls.push(CallEffect {
            callee,
            solution,
            caught: self.caught.clone().into_boxed_slice(),
            range,
        });
    }
}

impl<'db, F, G> Visitor<'_> for EffectsCollector<'db, F, G>
where
    F: Fn(&Expr) -> Type<'db>,
    G: Fn(&ast::ExprCall) -> Option<CallSolution<'db>>,
{
    fn visit_stmt(&mut self, stmt: &Stmt) {
        let env = self.env.clone();
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
                    KnownClass::AssertionError.to_instance(self.db, &env),
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
                    let solution = (self.call_solution)(call);
                    self.record_call(callee, solution, call.range());
                }
            }
            _ => {}
        }

        walk_expr(self, expr);
    }
}

/// The function a call resolves to, when it is one whose body can be analysed.
///
/// The whole function is returned rather than a single overload: which overload a
/// call matched is not known here, so resolution unions over all of them. It is
/// the function's type rather than its literal, which is what carries any
/// specialization it went through on the way to the call.
///
/// Callables, unions of callables, overload sets matched by argument, and
/// constructor calls are all left alone: this analysis reports nothing rather
/// than guessing at a set it cannot see.
fn callee_function<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<FunctionType<'db>> {
    let function = match callee {
        Type::FunctionLiteral(function) => function,
        Type::BoundMethod(method) => method.function(db),
        _ => return None,
    };

    let overload = function.literal(db).last_definition;
    // a dunder is reached through implicit dispatch far more often than through a
    // written call, and this analysis only sees the written ones. reporting the
    // visible half of a set would be worse than reporting none of it
    if is_dunder(overload.name(db)) {
        return None;
    }

    Some(function)
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
    let env = context.program_environment();
    let db = context.db();
    // resolving both sets walks two call graphs, so do nothing at all unless the
    // strictness option asked for it
    if !context.is_lint_enabled(&OVERRIDE_RAISE) {
        return;
    }

    // the base method is the one the subclass inherits, specialized by whatever the
    // subclass wrote for the base's type parameters (`class FileReader(Reader[OSError])`)
    let allowed = function_type_raised_exceptions(db, env, superclass_function);
    let raised = function_type_raised_exceptions(db, env, subclass_function);
    let Some(extra) = escaping(db, env, raised, &[allowed]) else {
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
        extra.display(db, env)
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
            allowed.display(db, env)
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
    call_solution: impl Fn(&ast::ExprCall) -> Option<CallSolution<'db>>,
) {
    let env = context.program_environment();
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

    let effects =
        collect_exception_effects(db, env, &function.body, expression_type, call_solution);
    if effects.is_empty() {
        return;
    }

    let visible = visible_binding_contexts(db, context.program_file(), body_scope);
    let site = CallSite {
        body_scope,
        visible: &visible,
        // this runs inside the body's own inference, so a call back into an
        // undeclared function here cannot be resolved without re-entering it
        resolves_recursion: declared.is_some(),
    };
    for (range, escaped) in escaping_sites(db, env, &effects, &site, &allowed) {
        let name = &function.name.id;
        if declared.is_some() {
            let Some(builder) = context.report_lint(&UNDECLARED_RAISE, range) else {
                continue;
            };
            builder.into_diagnostic(format_args!(
                "`{name}` can raise `{}`, which its `raises` clause does not include",
                escaped.display(db, env)
            ));
        } else {
            let Some(builder) = context.report_lint(&UNHANDLED_EXCEPTION, range) else {
                continue;
            };
            builder.into_diagnostic(format_args!(
                "`{}` can escape `{name}`, the entry point",
                escaped.display(db, env)
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
    let env = context.program_environment();
    let db = context.db();
    if declared.is_never() || declared.is_dynamic() {
        return;
    }
    if declared.is_disjoint_from(db, env, KnownClass::BaseException.to_instance(db, env)) {
        if let Some(builder) = context.report_lint(&INVALID_RAISES_CLAUSE, clause) {
            builder.into_diagnostic(format_args!(
                "`{}` contains no exception, so nothing can satisfy this `raises` clause",
                declared.display(db, env)
            ));
        }
        return;
    }

    check_raises_clause_type_parameters(context, clause, declared);
}

/// Report a type parameter in a `raises` clause that a caller could pick a
/// non-exception for.
///
/// A type parameter stands for one type the caller chooses, so `raises T` says
/// something about exceptions only when every type `T` can be is an exception.
/// That is what its declaration says: `def f[T: OSError](...) raises T` and
/// `def f[T in (KeyError, IndexError)](...) raises T` both hold, while a
/// parameter with no bound at all can be `int` as easily as `OSError`.
///
/// Only a member of the set itself has to be an exception. A parameter inside one
/// — `raises E[X]` for `class E[X](Exception)` — is a type argument of an
/// exception, and says nothing about what is raised.
fn check_raises_clause_type_parameters<'db, 'ast>(
    context: &InferContext<'db, 'ast>,
    clause: &'ast Expr,
    declared: Type<'db>,
) {
    let env = context.program_environment();
    let db = context.db();
    let exception = KnownClass::BaseException.to_instance(db, env);

    for element in union_elements(db, declared) {
        let Type::TypeVar(bound_typevar) = element else {
            continue;
        };
        let typevar = bound_typevar.typevar(db);
        if typevar
            .declared_ceiling(db, env)
            .is_assignable_to(db, env, exception)
        {
            continue;
        }

        let range = union_operand_named(clause, typevar.name(db)).unwrap_or(clause);
        let Some(builder) = context.report_lint(&INVALID_RAISES_CLAUSE, range) else {
            continue;
        };
        let name = element.display(db, env);
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{name}` is not always an exception, so it cannot appear in a `raises` clause"
        ));
        match typevar.bound_or_constraints(db, env) {
            None => diagnostic.info(format_args!("`{name}` has no bound")),
            Some(TypeVarBoundOrConstraints::UpperBound(bound)) => diagnostic.info(format_args!(
                "`{name}` is bounded by `{}`",
                bound.display(db, env)
            )),
            Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                if let Some(constraint) = constraints
                    .elements(db)
                    .iter()
                    .find(|constraint| !constraint.is_assignable_to(db, env, exception))
                {
                    diagnostic.info(format_args!(
                        "`{name}` can be `{}`",
                        constraint.display(db, env)
                    ));
                }
            }
        }
    }
}

/// The top-level `|` operand of `clause` that is the bare name `name`, if any.
fn union_operand_named<'e>(clause: &'e Expr, name: &str) -> Option<&'e Expr> {
    match clause {
        Expr::BinOp(ast::ExprBinOp {
            left,
            op: ast::Operator::BitOr,
            right,
            ..
        }) => union_operand_named(left, name).or_else(|| union_operand_named(right, name)),
        Expr::Name(named) if named.id.as_str() == name => Some(clause),
        _ => None,
    }
}

/// The runtime test for a function's declared exception set, for a guard on the
/// lowered function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaisesRuntimeTarget {
    /// The `isinstance` target with every type parameter widened to its declared
    /// ceiling — what the guard tests when nothing says which type the caller
    /// chose.
    pub ceiling: String,
    /// How to build the exact target at the call, when the clause names a type
    /// parameter that carries a runtime value.
    pub resolved: Option<ResolvedRaisesTarget>,
}

/// The `isinstance` target for a clause naming type parameters that carry a
/// runtime value, spelled in terms of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRaisesTarget {
    /// The target, with each of those parameters spelled by its name.
    pub expression: String,
    /// The guarded function's own reified parameters, read off the specialization
    /// it is called through.
    pub own: Vec<String>,
    /// Its class's reified parameters, read off the instance the method is
    /// called on.
    pub receiver: Vec<String>,
}

/// The runtime test for `function`'s declared exception set.
///
/// `None` when there is no faithful runtime test — a gradual clause, or a set
/// whose members have no runtime spelling (a negation, a protocol). `Never`
/// becomes the empty tuple, which no exception is an instance of.
///
/// A type parameter has no runtime spelling of its own: which exception it is
/// was chosen by the caller, and the guard runs inside the callee. What the
/// declaration always states is the parameter's ceiling, and every value it can
/// take is one of those, so that is the test the guard can always make — it
/// never rejects an exception the clause allows, and still catches one it does
/// not. A reified parameter does carry its value, and where the guard can read
/// it, it tests that instead.
pub fn declared_raises_runtime_target<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ruff_db::files::File,
    function: Type<'db>,
) -> Option<RaisesRuntimeTarget> {
    let Type::FunctionLiteral(function) = function else {
        return None;
    };
    let overload = function.literal(db).last_definition;
    let declared = declared_exceptions(db, overload)?;

    if declared.is_dynamic() {
        return None;
    }
    if declared.is_never() {
        return Some(RaisesRuntimeTarget {
            ceiling: "()".to_string(),
            resolved: None,
        });
    }

    let ceiling = crate::types::soundness::runtime_check_target(
        db,
        env,
        file,
        widen_to_ceilings(db, env, declared),
    )?;

    Some(RaisesRuntimeTarget {
        ceiling,
        resolved: resolved_runtime_target(db, env, file, overload, declared),
    })
}

/// Where a reified type parameter's value can be read, from inside the guard on a
/// function.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeSource {
    /// the guarded function's own parameter: its `generic` wrapper binds it for
    /// each call
    Own,
    /// the class parameter of a method with a receiver: the instance the method is
    /// called on carries it
    Receiver,
    /// an enclosing function's parameter: the guard is evaluated inside that
    /// function's call, where the value is already bound
    Closure,
}

/// The exact `isinstance` target for `declared`, when it names type parameters
/// the guard on `overload` can read the values of.
///
/// Each parameter is looked up by what binds it rather than by its name, which an
/// inner parameter can shadow. A parameter with no readable value — a class
/// parameter in a function nested in a method, whose first argument is not a
/// receiver, or anything not reified at all — is tested at its ceiling. Asking for
/// the value of a parameter that is not reified would mean reifying it, and
/// turning a check on must not change how the program is built.
fn resolved_runtime_target<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: ruff_db::files::File,
    overload: OverloadLiteral<'db>,
    declared: Type<'db>,
) -> Option<ResolvedRaisesTarget> {
    let sources = runtime_type_parameter_sources(db, overload);
    let source_of = |bound_typevar: BoundTypeVarInstance<'db>| {
        let name = bound_typevar.typevar(db).name(db);
        sources
            .iter()
            .find(|(context, _, _)| *context == bound_typevar.binding_context(db))
            .filter(|(_, _, reified)| reified.contains(name))
            .map(|(_, source, _)| *source)
    };

    let mut own = Vec::new();
    let mut receiver = Vec::new();
    let mut reads_any = false;
    let mut parts: Vec<String> = Vec::new();
    for element in union_elements(db, declared) {
        let part = match element {
            Type::TypeVar(bound_typevar) if let Some(source) = source_of(bound_typevar) => {
                let name = bound_typevar.typevar(db).name(db).to_string();
                let names = match source {
                    RuntimeSource::Own => Some(&mut own),
                    RuntimeSource::Receiver => Some(&mut receiver),
                    RuntimeSource::Closure => None,
                };
                if let Some(names) = names
                    && !names.contains(&name)
                {
                    names.push(name.clone());
                }
                reads_any = true;
                name
            }
            element => crate::types::soundness::runtime_check_target(
                db,
                env,
                file,
                widen_to_ceilings(db, env, element),
            )?,
        };
        if !parts.contains(&part) {
            parts.push(part);
        }
    }

    if !reads_any {
        return None;
    }
    // isinstance accepts nested tuples, so a rendered union composes without
    // flattening
    let expression = match parts.len() {
        1 => parts.pop()?,
        _ => format!("({})", parts.join(", ")),
    };
    Some(ResolvedRaisesTarget {
        expression,
        own,
        receiver,
    })
}

/// The binding contexts around `overload`'s body whose reified parameters the
/// guard can read, each with where it reads them and which of its parameters are
/// reified.
///
/// The reified sets are the ones the lowering reifies by: a function is wrapped
/// in `generic` exactly when its set is not empty, and a class decorated with
/// `generic_class` likewise.
fn runtime_type_parameter_sources<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Vec<(BindingContext<'db>, RuntimeSource, Vec<Name>)> {
    let module = parsed_module(db, overload.python_file(db)).load(db);
    let index = semantic_index(db, overload.program_file(db));
    let function = overload.node(db, overload.file(db), &module);
    let mut sources = vec![(
        BindingContext::from(overload.definition(db)),
        RuntimeSource::Own,
        reified_type_param_names(PySourceType::BasedPython, function),
    )];

    // the first class or function around the body decides whether this is a
    // method. a type-parameter scope may sit in between, and carries nothing
    let mut direct = true;
    for (_, scope) in index
        .ancestor_scopes(overload.body_scope(db).file_scope_id(db))
        .skip(1)
    {
        match scope.node() {
            NodeWithScopeKind::Class(class) => {
                if direct && method_receiver(function).is_some() {
                    sources.push((
                        index.expect_single_definition(class).into(),
                        RuntimeSource::Receiver,
                        reified_class_reads(PySourceType::BasedPython, class.node(&module)).names,
                    ));
                }
                direct = false;
            }
            NodeWithScopeKind::Function(enclosing) => {
                sources.push((
                    index.expect_single_definition(enclosing).into(),
                    RuntimeSource::Closure,
                    reified_type_param_names(PySourceType::BasedPython, enclosing.node(&module)),
                ));
                direct = false;
            }
            _ => {}
        }
    }
    sources
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
