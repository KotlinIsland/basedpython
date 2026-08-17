//! basedpython: the signature a function has when it was never written down.
//!
//! Python's gradual guarantee makes an unannotated `def` say nothing: its
//! parameters accept anything and it returns `Unknown`. Under
//! [`sound-types`](crate::AnalysisSettings::sound_types) that trade is refused,
//! and the signature is recovered from what the function itself already
//! determines — the body it returns from, and the uses its parameters are put
//! to.
//!
//! Only the *missing* half is recovered. An explicit annotation always wins, and
//! so does anything an overload group or an overridden base method already
//! supplies.
//!
//! See `docs/basedpython/features/sound-types.md`.

use std::collections::BTreeMap;

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_python_core::ast_ids::HasScopedUseId;
use ty_python_core::definition::{Definition, DefinitionKind, ParameterDefinitionNodeKind};
use ty_python_core::narrowing_constraints::ScopedNarrowingConstraint;
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::predicate::{Predicate, PredicateNode};
use ty_python_core::scope::{NodeWithScopeRef, ScopeId};
use ty_python_core::{UseDefMap, semantic_index, use_def_map};

use crate::Db;
use crate::reachability::ReachabilityConstraintsExtension;
use crate::types::ProgramEnvironment;
use crate::types::call::CallArguments;
use crate::types::callable::CallableType;
use crate::types::constraints::{ConstraintSetBuilder, max_constructor_and_typevar_depth};
use crate::types::function::OverloadLiteral;
use crate::types::narrow::{NarrowingConstraint, infer_narrowing_constraints};
use crate::types::protocol_class::InlineProtocolMember;
use crate::types::signatures::{Parameter, Parameters, Signature};
use crate::types::typevar::{
    TypeVarBoundOrConstraintsEvaluation, TypeVarDefaultEvaluation, TypeVarIdentity,
    TypeVarInstance, TypeVarKind,
};
use crate::types::{
    IntersectionBuilder, KnownClass, Type, TypeContext, UnionType, infer_deferred_types,
    infer_scope_types,
};

/// The return type inferred for `overload` from its body.
///
/// This is the union of every value the body can hand back: each `return`
/// expression, plus `None` when control can also reach the end of the body. A
/// body that always raises returns `Never`, and a body with nothing in it at all
/// — a stub, a protocol member, an `abstractmethod` — returns `None`, which is
/// what running it would do.
///
/// A generator returns a generator rather than what its `return` statements say,
/// so those become the generator's third type argument and the `yield`
/// expressions supply the first.
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, id, _| Type::divergent(id),
    cycle_fn = |db, cycle, previous: &Type<'db>, value: Type<'db>, overload: OverloadLiteral<'db>| {
        let env = &ProgramEnvironment::from_file(overload.program_file(db));
        divergence_bounded(db, env, value, cycle).cycle_normalized(db, env, *previous, cycle)
    },
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn inferred_return_type<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Type<'db> {
    let env = &ProgramEnvironment::from_file(overload.program_file(db));
    let file = overload.file(db);
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let node = overload.node(db, file, &module);
    let body_scope = overload.body_scope(db);
    let index = semantic_index(db, db.program_file(file));
    let file_scope_id = body_scope.file_scope_id(db);
    let inference = infer_scope_types(db, body_scope, TypeContext::default());

    return_type_from_body(
        db,
        env,
        node,
        file_scope_id.is_generator_function(index),
        can_implicitly_return_none(db, index.use_def_map(file_scope_id)),
        |expr| inference.expression_type(expr),
    )
}

/// The deepest a recovered return type may nest before the recursion that built it
/// is called what it is.
///
/// A hand-written return type is a constructor or two deep — `list[int]`,
/// `dict[str, list[int]]`. Anything far past that inside a cycle was assembled one
/// layer per iteration rather than written by anybody.
const RETURN_TYPE_NESTING_LIMIT: u16 = 8;

/// `value` with a return type that grows a constructor deeper every iteration
/// replaced by the divergence marker it already stands for.
///
/// A body that returns a call taking the function itself — `def g(n): return map(g, n)`
/// — has no return type to reach: it is `map[map[…]]` without end. Ordinarily
/// [`Type::cycle_normalized`] folds such a type back onto the marker the cycle started
/// from, but the marker only survives while the type is *built*; passing through a
/// generic call's solve leaves a concrete type behind with nothing left to fold on, and
/// the fixed point recedes by one constructor per iteration forever.
///
/// So bound the nesting rather than the iterations: past the bound the value is
/// replaced by the cycle head's own `Divergent`, which is what the marker-preserving
/// path would have produced, and the next iteration reproduces it unchanged. The bound
/// reads only the value and the cycle, so the query stays a function of its inputs.
fn divergence_bounded<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value: Type<'db>,
    cycle: &salsa::Cycle,
) -> Type<'db> {
    let (constructor_depth, _) = max_constructor_and_typevar_depth(db, env, value);
    if constructor_depth < RETURN_TYPE_NESTING_LIMIT {
        return value;
    }
    cycle.head_ids().next().map_or(value, Type::divergent)
}

/// The return type `node`'s body determines, given what its expressions were inferred as.
///
/// [`inferred_return_type`] reads those out of a completed scope inference. The
/// `redundant-return-annotation` lint reads them out of the inference in progress, because it
/// runs from inside the very scope that would be re-entered. The two share this so they cannot
/// drift: the lint advises deleting an annotation, and a body type it disagreed with would make
/// that advice silently change the function's type.
pub(crate) fn return_type_from_body<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    node: &ast::StmtFunctionDef,
    is_generator: bool,
    can_implicitly_return_none: bool,
    expression_type: impl Fn(&Expr) -> Type<'db>,
) -> Type<'db> {
    let mut collector = BodyValueCollector {
        db,
        env: env.clone(),
        expression_type,
        returns: Vec::new(),
        yields: Vec::new(),
    };
    collector.visit_body(&node.body);

    let returned = UnionType::from_elements(
        db,
        env,
        collector
            .returns
            .iter()
            .copied()
            .chain(can_implicitly_return_none.then(|| Type::none(db, env))),
    );

    if !is_generator {
        return returned;
    }

    // what a generator's caller receives is the generator, not what the body
    // returns; the send type is the one thing the body does not determine, since
    // it is what the caller passes back in
    let yielded = UnionType::from_elements(db, env, collector.yields.iter().copied());
    if node.is_async {
        KnownClass::AsyncGeneratorType.to_specialized_instance(db, env, &[yielded, Type::unknown()])
    } else {
        KnownClass::GeneratorType.to_specialized_instance(
            db,
            env,
            &[yielded, Type::unknown(), returned],
        )
    }
}

/// The anonymous type parameter an unannotated parameter opens — the `some` hole
/// nobody wrote.
///
/// A parameter with no annotation is not "some fixed type we failed to learn": it
/// is a hole the call site fills, and naming it keeps the connection between what
/// goes in and what comes out. `def f(x): return x` is the identity function, and
/// only a type parameter can say so.
///
/// Its identity is the parameter's own definition, so every site that needs it
/// builds the same one. Both its bound and its default are lazy, which is what
/// lets the bound be read out of a body that is itself typed in terms of this
/// hole.
pub(crate) fn inferred_parameter_typevar<'db>(
    db: &'db dyn Db,
    name: &Name,
    parameter: Definition<'db>,
) -> TypeVarInstance<'db> {
    TypeVarInstance::new(
        db,
        TypeVarIdentity::new(
            db,
            name.clone(),
            Some(parameter),
            TypeVarKind::InferredParameter,
        ),
        Some(TypeVarBoundOrConstraintsEvaluation::LazyUpperBound),
        None,
        None,
        Some(TypeVarDefaultEvaluation::Lazy),
    )
}

/// The gradual type an unannotated parameter's hole stands in for, when nothing in the body
/// bounded it.
///
/// Such a hole is `Unknown` wearing a name. It is opened anyway so that a return type can refer
/// back to the argument, but it says no more about the value than the gradual type it replaced.
/// Anywhere that reads a *structure* out of a type rather than relating it to another — a class to
/// subclass, a pivot for `super()` — has to see through it, or recovering the signature would
/// report what the gradual type never did.
pub(crate) fn gradual_hole<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Type<'db>> {
    let Type::TypeVar(bound_typevar) = ty else {
        return None;
    };
    let typevar = bound_typevar.typevar(db);
    if typevar.kind(db) != TypeVarKind::InferredParameter {
        return None;
    }
    typevar.upper_bound(db, env).filter(Type::is_dynamic)
}

/// The type of `parameter`'s hole, bound to the function that declares it.
pub(crate) fn inferred_parameter_type<'db>(
    db: &'db dyn Db,
    name: &Name,
    parameter: Definition<'db>,
    function: Definition<'db>,
) -> Type<'db> {
    Type::TypeVar(
        inferred_parameter_typevar(db, name, parameter).with_binding_context(db, function),
    )
}

/// The definition of the function `parameter` belongs to.
pub(crate) fn parameter_function_definition<'db>(
    db: &'db dyn Db,
    parameter: Definition<'db>,
) -> Option<Definition<'db>> {
    let scope = parameter.scope(db);
    let index = semantic_index(db, scope.program_file(db));
    let function = index.scope(scope.file_scope_id(db)).node().as_function()?;
    Some(index.expect_single_definition(function))
}

/// The type of `parameter`'s default value, which is the [PEP 696] default of the
/// hole it opens: a call that leaves the argument out still names a type.
///
/// [PEP 696]: https://peps.python.org/pep-0696/
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| None,
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn inferred_parameter_default<'db>(
    db: &'db dyn Db,
    parameter: Definition<'db>,
) -> Option<Type<'db>> {
    let env = &ProgramEnvironment::from_definition(parameter);
    let DefinitionKind::Parameter(ParameterDefinitionNodeKind::Parameter(node)) =
        parameter.kind(db)
    else {
        return None;
    };
    let module = parsed_module(db, parameter.program_file(db).python_file(db)).load(db);
    let default = node.node(&module).default.as_deref()?;
    let function = parameter_function_definition(db, parameter)?;

    // defaults are always deferred, so this goes straight to the deferred inference the
    // same way the rest of the signature does
    Some(
        infer_deferred_types(db, function)
            .expression_type(default)
            .replace_parameter_defaults(db, env),
    )
}

/// Everything the function requires of `parameter`, as the upper bound of the hole
/// it opens.
///
/// Requirements come from three places, and the bound is their intersection:
///
/// - the **default value**, which is a sample of what belongs there, so it bounds
///   the hole by its promoted type
/// - the **body's uses** of the parameter — the members it reads and calls, the
///   parameters it is forwarded into, and everything it goes on to require of what
///   those members hand back — collected by [`body_parameter_constraints`]
/// - an **`assert`** at the top level of the body, which is the author saying what
///   they were prepared to accept
///
/// With nothing to go on the bound is gradual — the same `Unknown` an unannotated
/// parameter has always had, which is what keeps a body this analysis cannot read
/// from acquiring errors it never had.
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| Type::unknown(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn inferred_parameter_bound<'db>(
    db: &'db dyn Db,
    parameter: Definition<'db>,
) -> Type<'db> {
    let env = &ProgramEnvironment::from_definition(parameter);
    // `None` is the sentinel every optional parameter is spelled with — it says the argument
    // may be left out, not that `None` is the kind of thing that belongs there. bounding by it
    // would reject every call that supplies one, which is what `def f(x=None)` exists for
    let from_default = inferred_parameter_default(db, parameter)
        .filter(|default| !default.is_none(db))
        .map(|default| default.promote(db, env));
    let from_body = parameter_function_definition(db, parameter)
        .map(|function| body_parameter_constraints(db, function).get(parameter))
        .unwrap_or_default();

    let mut bound = IntersectionBuilder::new(db, env);
    let mut constrained = false;
    for constraint in from_default.into_iter().chain(from_body) {
        constrained = true;
        bound = bound.add_positive(constraint);
    }
    if !constrained {
        return Type::unknown();
    }

    let bound = bound.build();
    // requirements that cannot all hold are the function's own problem. a bound of `Never`
    // would reject every argument, which reports the contradiction at every call site but
    // never where it lives
    if bound.is_never() {
        return Type::unknown();
    }
    bound
}

/// What each of a function's unannotated parameters must be, according to its body.
///
/// Every parameter is answered in one pass, because the expensive half — inferring
/// the body, and re-binding each call in it — is shared between them.
///
/// This reads the body, and the body is checked against the bounds this produces, so
/// the answer is reached by iterating the two to a fixed point. Each round is allowed
/// to say more about a parameter than the round before it, and to say something
/// different; the one thing it may not do is stop answering for a parameter it has
/// already answered for, which is what [`ParameterConstraints::keeping_requirements_seen`]
/// enforces.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| ParameterConstraints::default(),
    cycle_fn = |_, _, previous: &ParameterConstraints<'db>, value: ParameterConstraints<'db>, _| {
        value.keeping_requirements_seen(previous)
    },
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn body_parameter_constraints<'db>(
    db: &'db dyn Db,
    function: Definition<'db>,
) -> ParameterConstraints<'db> {
    let env = &ProgramEnvironment::from_definition(function);
    let file = function.file(db);
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let index = semantic_index(db, db.program_file(file));
    let DefinitionKind::Function(function_kind) = function.kind(db) else {
        return ParameterConstraints::default();
    };
    let node = function_kind.node(&module);
    let Some(body_scope) = index
        .try_node_scope(NodeWithScopeRef::Function(node))
        .map(|scope| scope.to_scope_id(db, db.program_file(file)))
    else {
        return ParameterConstraints::default();
    };
    let inference = infer_scope_types(db, body_scope, TypeContext::default());

    // an *inferred* return type is read off the body, so it is no constraint on it; only a
    // return type the author wrote down says what the body has to produce
    let declared_return = node
        .returns
        .as_deref()
        .map(|returns| crate::types::definition_expression_type(db, function, returns));

    // a name bound more than once cannot stand for one value: which of them a later use is
    // about is not a question this can answer
    let single_bindings = index
        .place_table(body_scope.file_scope_id(db))
        .symbols()
        .filter(|symbol| symbol.is_bound() && !symbol.is_reassigned())
        .map(|symbol| symbol.name().clone())
        .collect();

    let mut collector = UseCollector {
        db,
        env: env.clone(),
        file,
        use_def: use_def_map(db, body_scope),
        expression_type: |expr: &Expr| inference.expression_type(expr),
        uses: FxHashMap::default(),
        sink: None,
        declared_return,
        locals: BTreeMap::default(),
        captured: FxHashSet::default(),
        single_bindings,
        in_nested_scope: false,
    };
    collector.visit_body(&node.body);

    let mut uses = collector.uses;
    apply_asserted_local_types(
        db,
        env,
        index,
        body_scope,
        node,
        &collector.locals,
        &mut uses,
    );

    let mut entries = path_bounds(db, env, uses);
    entries.extend(asserted_parameter_types(db, env, index, body_scope, node));

    // a parameter a nested scope captured keeps nothing: that body is checked against this
    // bound, and this walk never saw what it does with the name
    if !collector.captured.is_empty() {
        entries.retain(|(parameter, _)| {
            parameter_definition_name(db, *parameter)
                .is_none_or(|name| !collector.captured.contains(&name))
        });
    }

    entries.sort_by_key(|(parameter, _)| *parameter);

    ParameterConstraints {
        entries: entries.into_boxed_slice(),
    }
}

/// The bound each parameter's body contributes, keyed by the parameter's definition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct ParameterConstraints<'db> {
    entries: Box<[(Definition<'db>, Type<'db>)]>,
}

impl<'db> ParameterConstraints<'db> {
    /// Every bound recorded for `parameter`. A parameter can appear more than once — an
    /// `assert` and a use are separate requirements that both have to hold.
    fn get(&self, parameter: Definition<'db>) -> Vec<Type<'db>> {
        self.entries
            .iter()
            .filter(|(key, _)| *key == parameter)
            .map(|(_, ty)| *ty)
            .collect()
    }

    /// This round's requirements, plus those of every parameter this round stopped
    /// answering for.
    ///
    /// What a body requires of a parameter is a fact about the body, so a requirement one
    /// round of the cycle found does not stop holding because a later round could not find
    /// it. A round really can lose one. `assert isinstance(x, int) and x <= 5` narrows `x`
    /// to `int` only while `isinstance(x, int)` can still come out false; the round after
    /// that narrowing has become `x`'s bound the test is statically true, and an `and` arm
    /// that is always true says nothing about which branch this is, so it is dropped —
    /// taking its narrowing with it. That puts the bound back where it started, and the
    /// round after finds the narrowing again. Neither round repeats the one before it, and
    /// the iteration has no fixed point to reach.
    ///
    /// Requirements only ever being added is what leaves it one.
    fn keeping_requirements_seen(mut self, previous: &Self) -> Self {
        let dropped: Vec<_> = previous
            .entries
            .iter()
            .filter(|(parameter, _)| {
                !self
                    .entries
                    .iter()
                    .any(|(answered, _)| answered == parameter)
            })
            .copied()
            .collect();
        if dropped.is_empty() {
            return self;
        }

        let mut entries = self.entries.into_vec();
        entries.extend(dropped);
        entries.sort_by_key(|(parameter, _)| *parameter);
        self.entries = entries.into_boxed_slice();
        self
    }
}

/// Feed each `assert` at the top level of the body back into the value it is about.
///
/// `a = x.foo()` followed by `assert a is int` says what `x.foo()` has to return just as
/// plainly as `a: int = x.foo()` does; the only difference is that the requirement arrives a
/// statement later.
fn apply_asserted_local_types<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    index: &ty_python_core::SemanticIndex<'db>,
    body_scope: ScopeId<'db>,
    node: &ast::StmtFunctionDef,
    locals: &BTreeMap<Name, MemberPath<'db>>,
    uses: &mut FxHashMap<MemberPath<'db>, PathUses<'db>>,
) {
    if locals.is_empty() {
        return;
    }
    let place_table = index.place_table(body_scope.file_scope_id(db));

    for (local, path) in locals {
        let Some(place_id) = place_table.symbol_id(local.as_str()) else {
            continue;
        };
        for asserted in asserted_types(db, env, index, node, place_id.into()) {
            uses.entry(path.clone()).or_default().value.push(asserted);
        }
    }
}

/// The types an `assert` at the top level of `node`'s body narrows `place` to.
fn asserted_types<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    index: &ty_python_core::SemanticIndex<'db>,
    node: &ast::StmtFunctionDef,
    place: ScopedPlaceId,
) -> Vec<Type<'db>> {
    let mut asserted = Vec::new();
    for assert in node.body.iter().filter_map(Stmt::as_assert_stmt) {
        let Some(expression) = index.try_expression(&*assert.test) else {
            continue;
        };
        let predicate = Predicate {
            node: PredicateNode::Expression(expression),
            is_positive: true,
        };
        let (Some(constraint), _) = infer_narrowing_constraints(db, env, predicate, place) else {
            continue;
        };
        // narrowing `object` rather than the place's own type keeps a hole out of its own bound
        let narrowed = NarrowingConstraint::intersection(Type::object())
            .merge_constraint_and(constraint)
            .evaluate_constraint_type(db, env);
        if !narrowed.is_object() && !narrowed.is_never() && !narrowed.has_typevar(db, env) {
            asserted.push(narrowed);
        }
    }
    asserted
}

/// The type each parameter is narrowed to by an `assert` at the top level of `body`.
///
/// Only the top level counts. An `assert` there holds for every call that returns
/// normally, so it is a statement about the parameter itself; the same test inside an
/// `if` is a statement about one branch, and the author plainly meant the other branch
/// to be reachable.
fn asserted_parameter_types<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    index: &ty_python_core::SemanticIndex<'db>,
    body_scope: ScopeId<'db>,
    node: &ast::StmtFunctionDef,
) -> Vec<(Definition<'db>, Type<'db>)> {
    let place_table = index.place_table(body_scope.file_scope_id(db));
    let parameters: Vec<(ScopedPlaceId, Definition<'db>)> = node
        .parameters
        .iter_non_variadic_params()
        .filter(|parameter| parameter.parameter.annotation().is_none())
        .filter_map(|parameter| {
            let place_id = place_table.symbol_id(parameter.parameter.name.as_str())?;
            Some((
                place_id.into(),
                index.expect_single_definition(&parameter.parameter),
            ))
        })
        .collect();
    if parameters.is_empty() {
        return Vec::new();
    }

    let mut asserted = Vec::new();
    for assert in node.body.iter().filter_map(Stmt::as_assert_stmt) {
        let Some(expression) = index.try_expression(&*assert.test) else {
            continue;
        };
        let predicate = Predicate {
            node: PredicateNode::Expression(expression),
            is_positive: true,
        };
        for (place_id, definition) in &parameters {
            let (Some(constraint), _) = infer_narrowing_constraints(db, env, predicate, *place_id)
            else {
                continue;
            };
            // narrowing `object` rather than the hole keeps the hole out of its own bound
            let narrowed = NarrowingConstraint::intersection(Type::object())
                .merge_constraint_and(constraint)
                .evaluate_constraint_type(db, env);
            if !narrowed.is_object() && !narrowed.is_never() {
                asserted.push((*definition, narrowed));
            }
        }
    }

    asserted
}

/// A value the body can point at: a parameter's hole, and the members read off it to reach the
/// value in question.
///
/// `x` is the parameter itself, the value of `x.foo()` is `x` and `foo`, and the value of
/// `a.foo()` where `a = x.foo()` is `x` and `foo` twice. Every requirement is recorded against
/// one of these, so what the body does with a member's value is a requirement on that member and
/// not on the parameter.
#[derive(Clone, PartialEq, Eq, Hash)]
struct MemberPath<'db> {
    parameter: Definition<'db>,
    members: Vec<Name>,
}

impl<'db> MemberPath<'db> {
    fn parameter(parameter: Definition<'db>) -> Self {
        Self {
            parameter,
            members: Vec::new(),
        }
    }

    fn member(&self, name: &Name) -> Self {
        let mut members = Vec::with_capacity(self.members.len() + 1);
        members.extend_from_slice(&self.members);
        members.push(name.clone());
        Self {
            parameter: self.parameter,
            members,
        }
    }
}

/// What the value at one path was used for.
#[derive(Default)]
struct PathUses<'db> {
    /// members read off it, and how each was called — as the parameters of the first call, since
    /// a name called two different ways would need an overloaded member, which cannot be written
    /// here. requiring only the first shape under-constrains, which is the safe direction
    members: BTreeMap<Name, Option<Parameters<'db>>>,
    /// declared types the value itself had to fit: a place it was read into, or a parameter it
    /// was forwarded to
    value: Vec<Type<'db>>,
}

/// The bound each parameter's uses add up to.
///
/// The deepest paths are folded first, so that everything the body required of a member's value
/// is already a type by the time the member that produced it is written down. A path with both a
/// value requirement and members of its own intersects them, the same way the parameter's own
/// bound intersects its protocol with the types it was forwarded into.
fn path_bounds<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    uses: FxHashMap<MemberPath<'db>, PathUses<'db>>,
) -> Vec<(Definition<'db>, Type<'db>)> {
    let mut uses: Vec<_> = uses.into_iter().collect();
    uses.sort_by_key(|(path, _)| std::cmp::Reverse(path.members.len()));

    let mut resolved: FxHashMap<MemberPath<'db>, Type<'db>> = FxHashMap::default();
    let mut entries = Vec::new();
    for (path, path_uses) in uses {
        let mut bound = IntersectionBuilder::new(db, env);
        let mut constrained = false;

        if !path_uses.members.is_empty() {
            let members: Vec<_> = path_uses
                .members
                .into_iter()
                .map(|(name, called)| {
                    // `object` is what a value nothing required anything of has to be: the
                    // member only has to exist
                    let value = resolved
                        .get(&path.member(&name))
                        .copied()
                        .unwrap_or_else(Type::object);
                    let member = match called {
                        Some(parameters) => InlineProtocolMember::Method(
                            CallableType::function_like(db, Signature::new(parameters, value)),
                        ),
                        None => InlineProtocolMember::ReadOnlyAttribute(value),
                    };
                    (name, member)
                })
                .collect();
            bound = bound.add_positive(Type::recovered_protocol(db, env, members));
            constrained = true;
        }
        for value in path_uses.value {
            bound = bound.add_positive(value);
            constrained = true;
        }
        if !constrained {
            continue;
        }

        let bound = bound.build();
        if path.members.is_empty() {
            entries.push((path.parameter, bound));
        }
        resolved.insert(path, bound);
    }
    entries
}

/// The name a parameter definition binds.
fn parameter_definition_name<'db>(db: &'db dyn Db, parameter: Definition<'db>) -> Option<Name> {
    let DefinitionKind::Parameter(ParameterDefinitionNodeKind::Parameter(node)) =
        parameter.kind(db)
    else {
        return None;
    };
    let module = parsed_module(db, parameter.program_file(db).python_file(db)).load(db);
    Some(node.node(&module).parameter.name.id.clone())
}

/// Every name read inside a nested scope, which this walk cannot otherwise see into.
struct CapturedNames<'n>(&'n mut FxHashSet<Name>);

impl Visitor<'_> for CapturedNames<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Name(name) = expr {
            self.0.insert(name.id.clone());
        }
        walk_expr(self, expr);
    }
}

fn record_captured_names(stmt: &Stmt, into: &mut FxHashSet<Name>) {
    walk_stmt(&mut CapturedNames(into), stmt);
}

fn record_captured_names_in_expr(expr: &Expr, into: &mut FxHashSet<Name>) {
    walk_expr(&mut CapturedNames(into), expr);
}

/// Reads a body for what it does with each parameter hole, and with each value reached from one.
///
/// A use of the parameter itself is recognised by the *type* of the expression it is made on, not
/// by the name written there: a parameter that was reassigned, or narrowed, no longer has the
/// hole's type, and nothing it is then used for is a requirement on the argument. A local that a
/// value was assigned to has no such type of its own, so the same two questions — is this still
/// that value, and is it narrowed — are asked of its bindings instead.
struct UseCollector<'db, F> {
    db: &'db dyn Db,
    env: ProgramEnvironment<'db>,
    file: File,
    use_def: &'db UseDefMap<'db>,
    expression_type: F,
    uses: FxHashMap<MemberPath<'db>, PathUses<'db>>,
    /// the declared type the expression about to be visited has to fit, when it is being
    /// read into a place that has one
    sink: Option<Type<'db>>,
    /// the function's declared return type, when it wrote one down. an *inferred* return
    /// type is no constraint on the body — it is read off it
    declared_return: Option<Type<'db>>,
    /// locals a value at some path was assigned to, so that what is done with the local — and a
    /// later `assert` about it — reads back as a requirement on that value
    locals: BTreeMap<Name, MemberPath<'db>>,
    /// names a nested scope reads, whose uses this walk cannot see
    captured: FxHashSet<Name>,
    /// the names this scope binds exactly once, which are the only ones that stand for one value
    single_bindings: FxHashSet<Name>,
    /// whether the walk is inside a comprehension, whose expressions belong to a scope of its own
    /// and so are none of this scope's business
    in_nested_scope: bool,
}

impl<'db, F> UseCollector<'db, F>
where
    F: Fn(&Expr) -> Type<'db>,
{
    /// The parameter `expr` names, when its type is exactly that parameter's hole.
    fn hole(&self, expr: &Expr) -> Option<Definition<'db>> {
        let Type::TypeVar(bound_typevar) = (self.expression_type)(expr) else {
            return None;
        };
        let typevar = bound_typevar.typevar(self.db);
        (typevar.kind(self.db) == TypeVarKind::InferredParameter)
            .then(|| typevar.definition(self.db))?
    }

    /// A type is only a requirement on the argument if it can be written down outside the
    /// body. Anything mentioning a type variable — another hole, or the callee's own
    /// generics — would either escape its binding context or make two holes depend on each
    /// other, so it contributes nothing and the position stays gradual.
    ///
    /// A shape this analysis itself invented is ruled out for the same reason, one step on.
    /// A hole's own bound is such a shape, so the moment the body reads a member off a hole the
    /// type variable is gone and only that shape is left. Recording it would say nothing a
    /// caller could fail — the requirement is on the very type being written — and it would
    /// never settle, since each round nests the round before it one level deeper. A protocol the
    /// *program* states, written as `protocol(...)` or established by a narrowing, is a
    /// requirement like any other and stays.
    fn portable(&self, ty: Type<'db>) -> Option<Type<'db>> {
        let env = self.env.clone();
        (!ty.has_typevar_or_typevar_instance(self.db, &env)
            && !ty.is_dynamic()
            && !ty.is_object()
            && !ty.mentions_recovered_protocol(self.db, &env))
        .then_some(ty)
    }

    /// Record that the value at `path` has to have a member called `name`, shaped like
    /// `called` when it was called at all.
    fn record_member(
        &mut self,
        path: &MemberPath<'db>,
        name: &Name,
        called: Option<Parameters<'db>>,
    ) {
        let member = self
            .uses
            .entry(path.clone())
            .or_default()
            .members
            .entry(name.clone())
            .or_default();
        if let Some(called) = called {
            member.get_or_insert(called);
        }
    }

    /// Record that the value at `path` has to fit `required`, when that is something a call site
    /// could be asked for.
    fn record_value(&mut self, path: MemberPath<'db>, required: Type<'db>) {
        let Some(required) = self.portable(required) else {
            return;
        };
        self.uses.entry(path).or_default().value.push(required);
    }

    /// Record `x.name` read as a member the value at `path` has to have, whose own value has to
    /// fit wherever it was read into.
    fn record_read(&mut self, path: &MemberPath<'db>, name: &Name, sink: Option<Type<'db>>) {
        self.record_member(path, name, None);
        if let Some(sink) = sink {
            self.record_value(path.member(name), sink);
        }
    }

    /// Record `x.name(...)` as a method the value at `path` has to have, shaped like the call and
    /// returning something that fits wherever the result was read into.
    fn record_call(
        &mut self,
        path: &MemberPath<'db>,
        name: &Name,
        call: &ast::ExprCall,
        sink: Option<Type<'db>>,
    ) {
        let mut parameters = vec![Parameter::positional_only(Some(Name::new_static("self")))];
        for argument in &call.arguments.args {
            if argument.is_starred_expr() {
                return;
            }
            let mut parameter = Parameter::positional_only(None);
            if let Some(ty) = self.portable((self.expression_type)(argument)) {
                parameter = parameter.with_annotated_type(ty);
            }
            parameters.push(parameter);
        }
        for keyword in &call.arguments.keywords {
            let Some(argument_name) = keyword.arg.as_ref() else {
                return;
            };
            let mut parameter = Parameter::keyword_only(argument_name.id.clone());
            if let Some(ty) = self.portable((self.expression_type)(&keyword.value)) {
                parameter = parameter.with_annotated_type(ty);
            }
            parameters.push(parameter);
        }

        self.record_member(path, name, Some(Parameters::standard(parameters)));
        if let Some(sink) = sink {
            self.record_value(path.member(name), sink);
        }
    }

    /// The value a *name* stands for: the parameter's own hole, or a local a value at some path
    /// was assigned to.
    fn name_path(&self, expr: &Expr) -> Option<MemberPath<'db>> {
        let db = self.db;
        if let Some(parameter) = self.hole(expr) {
            return Some(MemberPath::parameter(parameter));
        }
        if self.in_nested_scope {
            return None;
        }
        let name = expr.as_name_expr()?;
        let path = self.locals.get(&name.id)?;

        // a narrowed name is about something narrower than the value it was bound to, and
        // requires nothing of that value — the same rule that stops a narrowed parameter from
        // being the hole, read off the bindings instead of the type, since a type read out of
        // the body being inferred would change from one round of this analysis to the next
        let mut bindings = self
            .use_def
            .bindings_at_use(name.scoped_use_id(self.db, db.program_file(self.file)));
        let binding = bindings.next()?;
        (bindings.next().is_none()
            && binding.binding.definition().is_some()
            && binding.narrowing_constraint.constraint() == ScopedNarrowingConstraint::ALWAYS_TRUE)
            .then(|| path.clone())
    }

    /// The value `expr` produces, when the body says which one it is: a name for one, or a
    /// member read or method call on one — the `x.foo()` of `a = x.foo()`.
    fn path(&self, expr: &Expr) -> Option<MemberPath<'db>> {
        let attribute = match expr {
            Expr::Call(call) => call.func.as_attribute_expr(),
            Expr::Attribute(attribute) if attribute.ctx.is_load() => Some(attribute),
            Expr::Attribute(_) => return None,
            _ => None,
        };
        let Some(attribute) = attribute else {
            return self.name_path(expr);
        };
        Some(self.path(&attribute.value)?.member(&attribute.attr.id))
    }

    /// Visit `expr` knowing the declared type it is being read into.
    fn visit_into(&mut self, expr: &Expr, sink: Option<Type<'db>>) {
        let outer = std::mem::replace(&mut self.sink, sink);
        self.visit_expr(expr);
        self.sink = outer;
    }

    /// Visit `call`'s arguments, each knowing the parameter type it was matched to.
    ///
    /// That parameter type serves twice: an argument that *is* a hole has to fit it, and an
    /// argument that reads a member off a hole makes that member's value have to fit it.
    fn visit_call_arguments(&mut self, call: &ast::ExprCall) {
        let env = self.env.clone();
        // a splatted argument hands the callee its *elements*, or its values under their own
        // names, so the parameter it lands on says nothing about the argument itself. taking
        // that parameter as a requirement would bound a hole by what it is expected to contain
        // — `def f(it): C(*it)` would require `it` to *be* an `int` rather than to yield them
        let arguments: Vec<(&Expr, bool)> = call
            .arguments
            .iter_source_order()
            .map(|argument| match argument {
                ast::ArgOrKeyword::Arg(argument) => (argument, argument.is_starred_expr()),
                ast::ArgOrKeyword::Keyword(keyword) => (&keyword.value, keyword.arg.is_none()),
            })
            .collect();

        // binding a call is the expensive half of this analysis, and most calls in a body have
        // nothing to do with any hole. only an argument that names a value reached from one has
        // anything to learn from the parameter it was matched to
        let constrains_a_hole = arguments
            .iter()
            .any(|(argument, splatted)| !splatted && self.path(argument).is_some());
        let parameter_types = if constrains_a_hole {
            let callee = (self.expression_type)(&call.func);
            call_parameter_types(self.db, &env, callee, &call.arguments, |expr| {
                (self.expression_type)(expr)
            })
            .unwrap_or_default()
        } else {
            Vec::new()
        };

        for (index, (argument, splatted)) in arguments.into_iter().enumerate() {
            let matched = if splatted {
                None
            } else {
                parameter_types
                    .get(index)
                    .copied()
                    .flatten()
                    .and_then(|matched| self.portable(matched))
            };
            // a member read is constrained by the sink instead, when it is visited below
            if let (Some(path), Some(matched)) = (self.name_path(argument), matched) {
                self.record_value(path, matched);
            }
            self.visit_into(argument, matched);
        }
    }
}

impl<'db, F> Visitor<'_> for UseCollector<'db, F>
where
    F: Fn(&Expr) -> Type<'db>,
{
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            // a nested scope's uses are its own; its parameters have their own holes. what it
            // does with a name it *captures* is another matter — that body is checked against
            // this parameter's bound, but its expressions belong to another scope's inference
            // and so are invisible to this walk. a bound built from the outer uses alone would
            // then be checked against inner uses it never saw, so a captured name is recorded
            // and its parameter left gradual
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {
                record_captured_names(stmt, &mut self.captured);
            }

            // `a = x.foo()` gives a name to the value at a path, so what the body goes on to do
            // with that name is a requirement on that value. the value itself is recorded by
            // the walk below as usual
            Stmt::Assign(assign) => {
                if let [ast::Expr::Name(target)] = assign.targets.as_slice()
                    && self.single_bindings.contains(&target.id)
                    && let Some(path) = self.path(&assign.value)
                {
                    self.locals.insert(target.id.clone(), path);
                }
                walk_stmt(self, stmt);
            }

            // `a: T = value` reads the value into a place that says what it has to be
            Stmt::AnnAssign(assign) => {
                self.visit_expr(&assign.target);
                if let Some(value) = assign.value.as_deref() {
                    let declared = (self.expression_type)(&assign.annotation);
                    self.visit_into(value, self.portable(declared));
                }
            }

            Stmt::Return(ret) => {
                if let Some(value) = ret.value.as_deref() {
                    self.visit_into(value, self.declared_return);
                }
            }

            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        // a sink belongs to the expression it was set for; whatever is nested inside it is
        // read into somewhere else, or nowhere
        let sink = self.sink.take();

        match expr {
            Expr::Lambda(lambda) => {
                record_captured_names_in_expr(&lambda.body, &mut self.captured);
                return;
            }

            // a comprehension binds its own names in a scope of its own, so a name written
            // inside one is not the local this scope bound
            Expr::ListComp(_) | Expr::SetComp(_) | Expr::DictComp(_) | Expr::Generator(_) => {
                let outer = std::mem::replace(&mut self.in_nested_scope, true);
                walk_expr(self, expr);
                self.in_nested_scope = outer;
                return;
            }

            Expr::Call(call) => {
                if let Expr::Attribute(method) = &*call.func
                    && let Some(path) = self.path(&method.value)
                {
                    self.record_call(&path, &method.attr.id, call, sink);
                    // the callee is this call, not a member read in its own right
                    self.visit_expr(&method.value);
                } else {
                    self.visit_expr(&call.func);
                }
                self.visit_call_arguments(call);
                return;
            }

            Expr::Attribute(attribute) => {
                if attribute.ctx.is_load()
                    && let Some(path) = self.path(&attribute.value)
                {
                    self.record_read(&path, &attribute.attr.id, sink);
                }
            }

            _ => {}
        }

        walk_expr(self, expr);
    }
}

/// The parameter type each of `call`'s source-order arguments was matched to.
///
/// `None` when the callee is a union or overloaded, where no single parameter type per
/// argument is well-defined.
fn call_parameter_types<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    callee: Type<'db>,
    arguments: &ast::Arguments,
    expression_type: impl Fn(&Expr) -> Type<'db>,
) -> Option<Vec<Option<Type<'db>>>> {
    let call_arguments = CallArguments::from_arguments_typed(arguments, expression_type);
    let bindings = callee
        .bindings(db, env)
        .match_parameters(db, env, &call_arguments)
        .check_types(
            db,
            env,
            &ConstraintSetBuilder::new(),
            &call_arguments,
            TypeContext::default(),
            &[],
        )
        .unwrap_or_else(|error| *error.into_bindings());
    bindings.plain_callee_parameter_types(call_arguments.len())
}

/// Whether control can reach the end of a scope's body and fall off it.
pub(crate) fn can_implicitly_return_none<'db>(db: &'db dyn Db, use_def: &UseDefMap<'db>) -> bool {
    !use_def
        .reachability_constraints()
        .evaluate(
            db,
            use_def.predicates(),
            use_def.end_of_scope_reachability(),
        )
        .is_always_false()
}

/// Collects the types a function body hands back — what it returns and what it
/// yields — from the body's own inferred expression types.
///
/// A nested function or lambda has its own body scope and its own returns, so
/// this never descends into one. A class body is skipped for the same reason.
struct BodyValueCollector<'db, F> {
    db: &'db dyn Db,
    env: ProgramEnvironment<'db>,
    expression_type: F,
    returns: Vec<Type<'db>>,
    yields: Vec<Type<'db>>,
}

impl<'db, F> Visitor<'_> for BodyValueCollector<'db, F>
where
    F: Fn(&Expr) -> Type<'db>,
{
    fn visit_stmt(&mut self, stmt: &Stmt) {
        let env = self.env.clone();
        match stmt {
            // a nested scope returns and yields on its own account. its
            // decorators, defaults and bases do run here, but none of them can
            // contain a `return`
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}

            Stmt::Return(ret) => {
                let returned = match ret.value.as_deref() {
                    Some(value) => {
                        self.visit_expr(value);
                        (self.expression_type)(value)
                    }
                    None => Type::none(self.db, &env),
                };
                self.returns.push(returned);
            }

            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        let env = self.env.clone();
        match expr {
            // each of these opens a scope of its own, which cannot contain a
            // `return` and (since 3.8) cannot contain a `yield` either
            Expr::Lambda(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::DictComp(_)
            | Expr::Generator(_) => return,

            Expr::Yield(yield_expr) => {
                self.yields.push(match yield_expr.value.as_deref() {
                    Some(value) => (self.expression_type)(value),
                    None => Type::none(self.db, &env),
                });
            }

            // `yield from it` re-yields `it`'s elements one at a time
            Expr::YieldFrom(yield_from) => {
                self.yields.push(
                    (self.expression_type)(&yield_from.value)
                        .iterate(self.db, &env)
                        .homogeneous_element_type(self.db, &env),
                );
            }

            _ => {}
        }

        walk_expr(self, expr);
    }
}
