//! basedpython: the narrowing a function performs but never wrote down.
//!
//! `def is_int(a: object) -> a is int` states what a truthy result means about the argument, so
//! `if is_int(x)` narrows `x`. A `def` that leaves its return type out states the very same thing
//! by returning `a is int` — the body is simply the only place it is written. So the guards a
//! signature carries are recovered from the body alongside the return type itself, and calls
//! narrow by a recovered guard exactly as they narrow by a written one.
//!
//! What is recovered is what every `return` in the body agrees on. A place is narrowed where the
//! call is truthy only if *every* `return` that can hand back a truthy value narrows it, and the
//! guard is the union of what each of them establishes; a `return` that says nothing about the
//! place says the whole of `object` about it, which unions away to no guard at all. The falsy
//! side is the same with the returns that can hand back a falsy value, and falling off the end of
//! the body is one of those.
//!
//! A body that returns nothing makes the other kind of claim: `def check(a): assert a` tells
//! every caller that `a` is truthy once the call has returned, just as `-> asserts a` would. What
//! is recovered there is what every way out of the body agrees on — each reachable `return`, and
//! falling off the end — and a body with no way out at all asserts nothing, since no call to it
//! ever returns. The semantic index merges those ways out into one state per place, which is what
//! `recovered_assertion_guards` reads. The same reading checks a written `-> asserts`, against each
//! way out on its own, so that the `return` which fails the assertion is the one reported.
//!
//! Two things are asked of the body beyond that, and both are about the value a caller actually
//! holds: that calling it really does run the body — which rules out a coroutine and a
//! generator — and that the place the guard names still holds what the caller passed, which
//! rules out a body that rebinds it.
//!
//! See `docs/basedpython/features/type-is.md`.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::{Name, UnqualifiedName};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::TextRange;
use rustc_hash::FxHashSet;
use ty_python_core::ast_ids::try_scoped_use_id;
use ty_python_core::definition::{Definition, DefinitionKind, DefinitionState};
use ty_python_core::place::{PlaceExpr, PlaceExprRef, ScopedPlaceId};
use ty_python_core::predicate::{Predicate, PredicateNode};
use ty_python_core::scope::ScopeId;
use ty_python_core::{
    BindingWithConstraintsIterator, FileScopeId, SemanticIndex, UseDefMap, semantic_index,
};

use crate::Db;
use crate::reachability::ReachabilityConstraintsExtension;
use crate::types::function::OverloadLiteral;
use crate::types::infer::ScopeInference;
use crate::types::inferred_signature::can_implicitly_return_none;
use crate::types::narrow::{
    NarrowingConstraint, NarrowingEvaluatorExtension, infer_narrowing_constraints,
};
use crate::types::protocol_class::InlineProtocolMember;
use crate::types::signatures::{NarrowingGuard, NarrowingGuardKind};
use crate::types::{
    IntersectionBuilder, LiteralValueTypeKind, ProgramEnvironment, Type, TypeContext, UnionBuilder,
    UnionType, infer_scope_types,
};

/// Whether a `def`'s body is where its guards are stated: it wrote no return type, and calling
/// it runs the body.
fn states_guards_in_body<'db>(db: &'db dyn Db, overload: OverloadLiteral<'db>) -> bool {
    let program_file = overload.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let node = overload.node(db, overload.file(db), &module);

    // a written return type is the whole of what the function declares; a body is only read
    // where nothing was written down
    if node.returns.is_some() || node.is_asserts_return {
        return false;
    }

    // a guard is a claim about the value a call produces, and neither a generator nor a coroutine
    // produces what its `return`s say: `if is_int(x)` on an `async def` tests the coroutine
    // object, which is truthy without the body having run at all
    let index = semantic_index(db, program_file);
    !(node.is_async
        || overload
            .body_scope(db)
            .file_scope_id(db)
            .is_generator_function(index))
}

/// The predicate guards `overload`'s body establishes, for a `def` that wrote no return type.
///
/// Resolving what the body returns infers types, and inferring them can reach this very
/// function's signature — a recursive predicate is the ordinary case, not a pathological one. So
/// a cycle starts at "this function narrows nothing", the answer that assumes the least: without
/// a guard every place stays at its widest, so a provisional read can never justify a narrowing
/// the settled answer would not.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| Box::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn inferred_predicate_guards<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Box<[NarrowingGuard<'db>]> {
    if !states_guards_in_body(db, overload) {
        return Box::default();
    }

    let program_file = overload.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let node = overload.node(db, overload.file(db), &module);
    let index = semantic_index(db, program_file);
    let body_scope = overload.body_scope(db);
    let file_scope_id = body_scope.file_scope_id(db);

    // a body that hands back nothing gives a caller no value to test. what it establishes is an
    // assertion instead — see [`recovered_assertion_guards`]
    if index.use_def_map(file_scope_id).records_normal_exit() {
        return Box::default();
    }

    let mut returns = ReturnedExpressions::default();
    returns.visit_body(&node.body);
    let returns = returns.expressions;
    if returns.is_empty() {
        return Box::default();
    }

    let first_parameter = node
        .parameters
        .iter()
        .next()
        .map(ast::AnyParameterRef::name);
    let parameters: FxHashSet<&Name> = node
        .parameters
        .iter_non_variadic_params()
        .map(|parameter| &parameter.parameter.name.id)
        .collect();
    if parameters.is_empty() {
        return Box::default();
    }

    // a place is a candidate when a `return` mentions it and it is reachable from a parameter,
    // since only such a place has an argument at the call site to narrow
    let place_table = index.place_table(file_scope_id);
    let mut candidates = Vec::new();
    let mut seen = FxHashSet::default();
    for returned in &returns {
        let mut mentioned = MentionedPlaces::default();
        mentioned.visit_expr(returned);
        for (name, members) in mentioned.places {
            if !parameters.contains(&name) || !seen.insert((name.clone(), members.clone())) {
                continue;
            }
            let Some(place_expr) = PlaceExpr::from_symbol_with_members(&name, &members) else {
                continue;
            };
            let Some(place_id) = place_table.place_id(&place_expr) else {
                continue;
            };
            if !holds_the_argument(db, index, file_scope_id, &name, &members) {
                continue;
            }
            candidates.push((name, members, place_id));
        }
    }
    // everything above reads the body's syntax alone. inferring it is what a caller's own
    // signature can be waiting on, so it is not reached until a guard is actually possible
    if candidates.is_empty() {
        return Box::default();
    }

    let env = &ProgramEnvironment::from_file(program_file);
    let inference = infer_scope_types(db, body_scope, TypeContext::default());

    // only a predicate is recovered from: a `bool` result is one a caller can do nothing with
    // but test, so what the test means is the whole content of the answer. anything else is a
    // value in its own right, and reading a claim about the arguments out of its truthiness
    // would be reading far more into it than it says
    if !returns
        .iter()
        .all(|returned| is_boolean(db, inference.expression_type(*returned)))
    {
        return Box::default();
    }

    // falling off the end of the body hands back `None`, which is falsy and says nothing about
    // any place, so it rules the falsy side out for every candidate at once
    let falls_off_end = can_implicitly_return_none(db, index.use_def_map(file_scope_id));

    candidates
        .into_iter()
        .filter_map(|(name, members, place_id)| {
            let (positive, negative) =
                returned_constraints(db, env, index, inference, &returns, place_id, falls_off_end);
            let kind = NarrowingGuardKind::InferredPredicate { positive, negative };
            (positive.is_some() || negative.is_some()).then(|| NarrowingGuard {
                root_is_first_parameter: first_parameter.is_some_and(|first| first.id == name),
                name,
                members: members.into_boxed_slice(),
                kind,
            })
        })
        .collect()
}

/// Whether [`recovered_assertion_guards`] can find anything for `overload`, from what the semantic
/// index alone says: the body records a merged exit state, and a predicate in it narrows a
/// parameter or a place below one. It infers nothing, so a signature can ask it while it is being
/// built.
pub(crate) fn may_recover_assertions<'db>(db: &'db dyn Db, overload: OverloadLiteral<'db>) -> bool {
    if !states_guards_in_body(db, overload) {
        return false;
    }

    let program_file = overload.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let node = overload.node(db, overload.file(db), &module);
    let index = semantic_index(db, program_file);
    let file_scope_id = overload.body_scope(db).file_scope_id(db);

    // the index records a merged exit state for exactly the bodies an assertion is read from:
    // those that hand back nothing, and that cannot return through a `finally` suite
    let use_def = index.use_def_map(file_scope_id);
    if !use_def.records_normal_exit() {
        return false;
    }

    let place_table = index.place_table(file_scope_id);
    let targets = use_def.predicate_narrowing_targets();
    node.parameters.iter_non_variadic_params().any(|parameter| {
        let name = &parameter.parameter.name.id;
        place_table.symbol_id(name).is_some_and(|symbol| {
            std::iter::once(ScopedPlaceId::Symbol(symbol))
                .chain(place_table.members_of_symbol(name))
                .any(|place| targets.contains_place(place))
        })
    })
}

/// The assertions `overload`'s body establishes, for a `def` that wrote no return type and hands
/// back nothing.
///
/// A place is asserted to be what the merged state of every way out of the body says it is,
/// narrowing `object` so that a call site intersects the assertion with the argument it actually
/// passed. A body the index recorded no such state for — one that hands back a value, or that can
/// return through a `finally` suite — asserts nothing.
///
/// These are not part of the function's signature, which only records whose they are — see
/// [`DeferredAssertions`](crate::types::signatures::DeferredAssertions). A cycle starts at "this
/// function asserts nothing", for the reason [`inferred_predicate_guards`] gives.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| Box::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn recovered_assertion_guards<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Box<[NarrowingGuard<'db>]> {
    if !may_recover_assertions(db, overload) {
        return Box::default();
    }

    let program_file = overload.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let node = overload.node(db, overload.file(db), &module);
    let index = semantic_index(db, program_file);
    let file_scope_id = overload.body_scope(db).file_scope_id(db);
    let place_table = index.place_table(file_scope_id);
    let use_def = index.use_def_map(file_scope_id);
    let first_parameter = node
        .parameters
        .iter()
        .next()
        .map(ast::AnyParameterRef::name);
    let env = &ProgramEnvironment::from_file(program_file);

    let mut guards = Vec::new();
    for parameter in node.parameters.iter_non_variadic_params() {
        let name = &parameter.parameter.name.id;
        let Some(symbol) = place_table.symbol_id(name) else {
            continue;
        };
        let places = std::iter::once(ScopedPlaceId::Symbol(symbol))
            .chain(place_table.members_of_symbol(name));
        for place in places {
            // a place no predicate narrows is `object` wherever the body returns, which is no
            // assertion at all, and answering that costs nothing
            if !use_def.predicate_narrowing_targets().contains_place(place) {
                continue;
            }
            let Some(members) = place_table.place(place).attribute_chain() else {
                continue;
            };
            let members: Vec<Name> = members.into_iter().map(Name::new).collect();
            if !holds_the_argument(db, index, file_scope_id, name, &members) {
                continue;
            }
            let Some(bindings) = use_def.normal_exit_bindings(place) else {
                continue;
            };
            let established = narrowed_over(db, env, use_def, bindings, place, &|_| Type::object());
            let Some(ty) = agreed_constraint(db, env, vec![established]) else {
                continue;
            };
            guards.push(NarrowingGuard {
                root_is_first_parameter: first_parameter.is_some_and(|first| first.id == *name),
                name: name.clone(),
                members: members.into_boxed_slice(),
                kind: NarrowingGuardKind::InferredAssertion { ty },
            });
        }
    }
    guards.into_boxed_slice()
}

/// A way out of a function body that hands control back to the caller.
#[derive(Clone, Copy, Debug)]
pub(crate) enum NormalExit {
    /// The `return` statement at this position among those recorded for the body, and its range.
    Return { index: usize, range: TextRange },
    /// Falling off the end of the body.
    EndOfBody,
}

/// Every way out of a function body that can be reached, and whose state says what a caller sees.
///
/// A `return` a `finally` suite can run after is left out: what the caller sees is the state after
/// that suite, which this model does not have. So it is neither read as establishing something nor
/// reported as failing to.
pub(crate) fn normal_exits<'db>(db: &'db dyn Db, use_def: &UseDefMap<'db>) -> Vec<NormalExit> {
    let is_reachable = |reachability| {
        !use_def
            .reachability_constraints()
            .evaluate(db, use_def.predicates(), reachability)
            .is_always_false()
    };
    use_def
        .return_exits()
        .enumerate()
        .filter(|(_, (_, reachability, through_finally))| {
            !through_finally && is_reachable(*reachability)
        })
        .map(|(index, (range, _, _))| NormalExit::Return { index, range })
        .chain(can_implicitly_return_none(db, use_def).then_some(NormalExit::EndOfBody))
        .collect()
}

/// What `place` is where `exit` leaves the body.
///
/// Each binding that reaches the exit is narrowed by what the flow established on the way there.
/// `start` gives the type narrowing starts from for a binding: `None` for the state the place was
/// in when the body began, which the body did not put there.
pub(crate) fn narrowed_at_exit<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    use_def: &UseDefMap<'db>,
    exit: NormalExit,
    place: ScopedPlaceId,
    start: &dyn Fn(Option<Definition<'db>>) -> Type<'db>,
) -> Type<'db> {
    let narrow = |bindings| narrowed_over(db, env, use_def, bindings, place, start);
    match exit {
        // a place first named after the `return` had nothing established about it there
        NormalExit::Return { index, .. } => use_def
            .return_exit_bindings(index, place)
            .map_or_else(|| start(None), narrow),
        NormalExit::EndOfBody => narrow(use_def.end_of_scope_bindings(place)),
    }
}

/// What `bindings` leave `place` as, each narrowed by what the flow established on the way to it.
fn narrowed_over<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    use_def: &UseDefMap<'db>,
    bindings: BindingWithConstraintsIterator<'_, 'db>,
    place: ScopedPlaceId,
    start: &dyn Fn(Option<Definition<'db>>) -> Type<'db>,
) -> Type<'db> {
    let mut narrowed = UnionBuilder::new(db, env);
    for binding in bindings {
        if use_def
            .reachability_constraints()
            .evaluate(db, use_def.predicates(), binding.reachability_constraint)
            .is_always_false()
        {
            continue;
        }
        let start = match binding.binding {
            DefinitionState::Undefined => start(None),
            DefinitionState::Defined(definition) => start(Some(definition)),
            // a deleted place holds nothing a caller could read back
            DefinitionState::Deleted => continue,
        };
        narrowed.add_in_place(binding.narrowing_constraint.narrow(db, env, start, place));
    }
    narrowed.build()
}

/// Whether the place `name` and `members` describe still holds the caller's value — the argument
/// passed for a parameter, or what the place held where the guard is written — everywhere in the
/// body.
///
/// A guard names the argument a call passed, so it says nothing once the body puts something else
/// where that argument was. `def f(a): a = 1; return a is int` hands back `True` whatever it was
/// given, and reading that as a claim about the argument would narrow it to `Never`. The same
/// goes for a member below the parameter: writing to `a.b` makes what the body tested and what
/// the caller reads back afterwards two different values.
///
/// Any binding at all is enough to give up on, wherever in the body it is. Deciding which
/// bindings reach which `return` is what the flow analysis of the body is for, and this is a
/// claim about every call — one that has to hold for all of them or not be made.
pub(crate) fn holds_the_argument<'db>(
    db: &'db dyn Db,
    index: &SemanticIndex<'db>,
    file_scope_id: FileScopeId,
    name: &Name,
    members: &[Name],
) -> bool {
    let place_table = index.place_table(file_scope_id);
    let use_def = index.use_def_map(file_scope_id);

    // a name the body never mentions is one it cannot have put anything in
    let Some(symbol_id) = place_table.symbol_id(name) else {
        return true;
    };
    // a `global` or `nonlocal` name is bound where the caller reads it, so writing to it writes
    // what the caller goes on to see
    let symbol = place_table.symbol(symbol_id);
    if symbol.is_global() || symbol.is_nonlocal() {
        return true;
    }
    if !use_def
        .reachable_symbol_bindings(symbol_id)
        .all(|binding| match binding.binding {
            // the implicit unbound state is not something the body put there
            DefinitionState::Undefined => true,
            DefinitionState::Defined(definition) => matches!(
                definition.kind(db),
                DefinitionKind::Parameter(_) | DefinitionKind::LambdaParameter(_)
            ),
            DefinitionState::Deleted => false,
        })
    {
        return false;
    }

    // and each step down to the place is reached through the one above it, so a write anywhere
    // along the way leaves the guard describing a value the caller never sees
    (1..=members.len()).all(|depth| {
        let Some(prefix) = PlaceExpr::from_symbol_with_members(name, &members[..depth]) else {
            return false;
        };
        match place_table.place_id(&prefix) {
            Some(place_id) => use_def
                .reachable_bindings(place_id)
                .all(|binding| matches!(binding.binding, DefinitionState::Undefined)),
            None => true,
        }
    })
}

/// Whether `ty` is a `bool` — the class itself, or one of its two values.
fn is_boolean<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    matches!(
        ty.as_literal_value_kind(),
        Some(LiteralValueTypeKind::Bool(_))
    ) || ty.is_bool(db)
}

/// What the `return`s of a body agree `place` is, where the call is truthy and where it is falsy.
///
/// `None` on either side means they agree on nothing there, which is no guard rather than a guard
/// of `object`.
fn returned_constraints<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    index: &'db SemanticIndex<'db>,
    inference: &ScopeInference<'db>,
    returns: &[&Expr],
    place: ScopedPlaceId,
    falls_off_end: bool,
) -> (Option<Type<'db>>, Option<Type<'db>>) {
    let mut positive = Vec::new();
    let mut negative = if falls_off_end {
        vec![Type::object()]
    } else {
        Vec::new()
    };

    for returned in returns {
        let truthiness = inference.expression_type(*returned).bool(db, env);
        let (when_true, when_false) = match index.try_expression(*returned) {
            Some(expression) => infer_narrowing_constraints(
                db,
                env,
                Predicate {
                    node: PredicateNode::Expression(expression),
                    is_positive: true,
                },
                place,
            ),
            // a returned expression the index holds no standalone expression for is one whose
            // narrowing cannot be evaluated, which is the same as narrowing nothing
            None => (None, None),
        };
        if truthiness.may_be_true() {
            positive.push(constraint_type(db, env, when_true));
        }
        if !truthiness.is_always_true() {
            negative.push(constraint_type(db, env, when_false));
        }
    }

    (
        agreed_constraint(db, env, positive),
        agreed_constraint(db, env, negative),
    )
}

/// The one claim a set of `return`s all support, or `None` when they support none.
fn agreed_constraint<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    constraints: Vec<Type<'db>>,
) -> Option<Type<'db>> {
    if constraints.is_empty() {
        return None;
    }
    let agreed = UnionType::from_elements(db, env, constraints);
    // `object` is every value there is, so agreeing on it is agreeing on nothing. `Never` is no
    // value at all, which says the branch cannot be taken — true, but the reachability analysis
    // already says so, and narrowing an argument to `Never` on the strength of it would report
    // the contradiction at the call site rather than where it lives
    if agreed.is_object() || agreed.is_never() || agreed.has_typevar(db, env) {
        return None;
    }
    Some(agreed)
}

/// The type a narrowing constraint narrows `object` to.
///
/// Narrowing `object` rather than the place's own type keeps the guard to what the body
/// established, so that a call site intersects it with the argument it actually passed rather
/// than with the parameter's declared type.
fn constraint_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    constraint: Option<NarrowingConstraint<'db>>,
) -> Type<'db> {
    match constraint {
        Some(constraint) => NarrowingConstraint::intersection(Type::object())
            .merge_constraint_and(constraint)
            .evaluate_constraint_type(db, env),
        None => Type::object(),
    }
}

/// The expressions a function body hands back, skipping the bodies of the scopes nested in it.
#[derive(Default)]
struct ReturnedExpressions<'ast> {
    expressions: Vec<&'ast Expr>,
}

impl<'ast> Visitor<'ast> for ReturnedExpressions<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a nested `def` or `class` returns for itself, not for the function around it
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::Return(ast::StmtReturn {
                value: Some(value), ..
            }) => self.expressions.push(value),
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, _: &'ast Expr) {}
}

/// The place expressions an expression mentions, each as a root name and the attributes below it.
#[derive(Default)]
struct MentionedPlaces {
    places: Vec<(Name, Vec<Name>)>,
}

impl<'ast> Visitor<'ast> for MentionedPlaces {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if (expr.is_name_expr() || expr.is_attribute_expr())
            && let Some(path) = UnqualifiedName::from_expr(expr)
            && let [root, members @ ..] = path.segments()
        {
            self.places.push((
                Name::new(root),
                members.iter().copied().map(Name::new).collect(),
            ));
        }
        walk_expr(self, expr);
    }
}

/// `ty` with what narrowing established about the members of the place `returned` names folded
/// into it.
///
/// `assert a.b is int` narrows the place `a.b`, and `return a` would ordinarily leave that behind:
/// the caller receives an `A`, whose `b` is whatever `A` declares. The narrowing is still true of
/// the value being handed back, though, and a structural type can say so — `A & protocol(b: int)`
/// reads `.b` back as `int` without claiming anything else. So the recovered return type says it.
///
/// The claim is a *read-only* one. It says what reading the member gives, which is all the
/// narrowing established; requiring a mutable attribute of that type would say the value's own
/// attribute is declared that way, which it is not.
pub(crate) fn with_narrowed_members<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    returned: &Expr,
    ty: Type<'db>,
) -> Type<'db> {
    // a type still standing for something else — `Self`, a type parameter — is solved against
    // whatever the call site turns out to be, and intersecting a claim onto it leaves nothing for
    // that solve to bind: a method that returns `self` would hand its caller back an unsolved
    // `Self` wearing a protocol
    if ty.has_typevar(db, env) {
        return ty;
    }
    // a place is the only thing whose members are tracked, and only a place's use records them
    let Some(returned_place) = PlaceExpr::try_from_expr(returned) else {
        return ty;
    };
    let program_file = scope.program_file(db);
    let Some(use_id) = try_scoped_use_id(db, program_file, returned) else {
        return ty;
    };
    let index = semantic_index(db, program_file);
    let file_scope_id = scope.file_scope_id(db);
    let use_def = index.use_def_map(file_scope_id);
    let place_table = index.place_table(file_scope_id);
    // the places recorded alongside the return spell out the whole path from the root, so the
    // chain the protocol describes starts below the place being returned
    let Some(returned_segments) = PlaceExprRef::from(&returned_place)
        .attribute_chain()
        .map(|chain| chain.len())
    else {
        return ty;
    };

    let mut narrowed = Vec::new();
    for (place, bindings) in use_def.multi_bindings_at_use(use_id) {
        let Some(chain) = place_table.place(place).attribute_chain() else {
            continue;
        };
        // the chain below the place being returned, which is what the protocol describes
        let Some(chain) = chain
            .get(returned_segments..)
            .filter(|chain| !chain.is_empty())
        else {
            continue;
        };
        // narrowing `object` leaves only what the narrowing itself established; a member the
        // flow said nothing about comes back as `object`, which is no claim at all
        let mut established = UnionBuilder::new(db, env);
        let mut any = false;
        for binding in bindings {
            any = true;
            established.add_in_place(binding.narrowing_constraint.narrow(
                db,
                env,
                Type::object(),
                place,
            ));
        }
        if !any {
            continue;
        }
        let established = established.build();
        if established.is_object() || established.is_never() {
            continue;
        }
        narrowed.push((chain.iter().map(Name::new).collect::<Vec<_>>(), established));
    }

    match narrowed_members_protocol(db, env, &narrowed) {
        Some(protocol) => IntersectionBuilder::new(db, env)
            .add_positive(ty)
            .add_positive(protocol)
            .build(),
        None => ty,
    }
}

/// The protocol describing `narrowed`, whose chains are relative to the place it describes.
///
/// A chain deeper than one segment nests: `a.b.c` narrowed to `int` is `protocol(b: protocol(c:
/// int))`, which intersects with whatever `a.b` is declared to be when `.b` is read off it.
fn narrowed_members_protocol<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    narrowed: &[(Vec<Name>, Type<'db>)],
) -> Option<Type<'db>> {
    let mut members: Vec<(Name, InlineProtocolMember<'db>)> = Vec::new();
    let mut seen = FxHashSet::default();
    for (chain, _) in narrowed {
        let Some(name) = chain.first() else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        let own = narrowed
            .iter()
            .find(|(candidate, _)| candidate.len() == 1 && candidate[0] == *name)
            .map(|(_, established)| *established);
        let below: Vec<(Vec<Name>, Type<'db>)> = narrowed
            .iter()
            .filter(|(candidate, _)| candidate.len() > 1 && candidate[0] == *name)
            .map(|(candidate, established)| (candidate[1..].to_vec(), *established))
            .collect();
        let member = match (own, narrowed_members_protocol(db, env, &below)) {
            (Some(own), Some(below)) => IntersectionBuilder::new(db, env)
                .add_positive(own)
                .add_positive(below)
                .build(),
            (Some(own), None) => own,
            (None, Some(below)) => below,
            (None, None) => continue,
        };
        members.push((
            name.clone(),
            InlineProtocolMember::ReadOnlyAttribute(member),
        ));
    }

    (!members.is_empty()).then(|| Type::inline_protocol(db, env, members, Box::default()))
}
