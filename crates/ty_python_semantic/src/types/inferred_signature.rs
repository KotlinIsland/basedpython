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
use ruff_text_size::{Ranged, TextRange};
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
use crate::types::inferred_narrowing::with_narrowed_members;
use crate::types::narrow::{NarrowingConstraint, infer_narrowing_constraints};
use crate::types::protocol_class::InlineProtocolMember;
use crate::types::signatures::{Parameter, ParameterKind, Parameters, Signature};
use crate::types::typevar::{
    TypeVarBoundOrConstraintsEvaluation, TypeVarDefaultEvaluation, TypeVarIdentity,
    TypeVarInstance, TypeVarKind,
};
use crate::types::{
    IntersectionBuilder, KnownClass, Type, TypeContext, UnionType, infer_function_default_types,
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
    let module = parsed_module(db, overload.python_file(db)).load(db);
    let node = overload.node(db, file, &module);
    let body_scope = overload.body_scope(db);
    let index = semantic_index(db, db.program_file(file));
    let file_scope_id = body_scope.file_scope_id(db);
    let inference = infer_scope_types(db, body_scope, TypeContext::default());

    return_type_from_body(
        db,
        env,
        body_scope,
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
    body_scope: ScopeId<'db>,
    node: &ast::StmtFunctionDef,
    is_generator: bool,
    can_implicitly_return_none: bool,
    expression_type: impl Fn(&Expr) -> Type<'db>,
) -> Type<'db> {
    let mut collector = BodyValueCollector {
        db,
        env: env.clone(),
        body_scope,
        // a `type def` hands back a type rather than a value, so there is no value whose members
        // a caller could read and nothing for a structural claim about them to describe
        carries_member_narrowing: !ast::helpers::is_type_def(node),
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
fn inferred_parameter_typevar<'db>(
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
fn parameter_function_definition<'db>(
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

    // a default is inferred in a region of its own, separate from the rest of the signature,
    // so that changing one does not invalidate everything that reads the signature
    Some(
        infer_function_default_types(db, function)
            .expression_type(default)
            .replace_parameter_defaults(db, env),
    )
}

/// The part of a parameter's bound the *source states*, as opposed to the part its body
/// samples.
///
/// A default is a written type. `def f(safe='/')` says `safe` is a `str` as plainly as an
/// annotation would — if something else belonged there, something else would be written —
/// which is why its requirement is [`WhenContradicted::Stands`] and survives a body that
/// contradicts it. Everything else in the bound is recovered from how the body happens to
/// use the parameter, and a recovered requirement can be withdrawn.
///
/// `None` where the source states nothing: no default at all, or the `None` sentinel every
/// optional parameter is spelled with — that says the argument may be left out, not that
/// `None` is the kind of thing that belongs there. Bounding by it would reject every call
/// that supplies one, which is what `def f(x=None)` exists for.
///
/// The literal is promoted, so `safe='/'` states `str` rather than `Literal['/']`.
pub(crate) fn stated_parameter_bound<'db>(
    db: &'db dyn Db,
    parameter: Definition<'db>,
) -> Option<Type<'db>> {
    let env = &ProgramEnvironment::from_definition(parameter);
    inferred_parameter_default(db, parameter)
        .filter(|default| !default.is_none(db))
        .map(|default| default.promote(db, env))
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
    let from_default = stated_parameter_bound(db, parameter);
    let from_body = parameter_function_definition(db, parameter)
        .map(|function| body_parameter_constraints(db, function).get(parameter))
        .unwrap_or_default();

    let constraints: Vec<Requirement<'db>> = from_default
        .map(|default| Requirement {
            ty: default,
            when_contradicted: WhenContradicted::Stands,
        })
        .into_iter()
        .chain(from_body)
        .collect();
    if constraints.is_empty() {
        return Type::unknown();
    }

    let mut bound = IntersectionBuilder::new(db, env);
    for constraint in settled_requirements(db, env, &constraints) {
        bound = bound.add_positive(constraint);
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

/// One thing a parameter's bound has to say, and what becomes of it when something the program
/// states rules it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct Requirement<'db> {
    ty: Type<'db>,
    when_contradicted: WhenContradicted<'db>,
}

/// What becomes of a requirement that something the program states rules out.
///
/// Only a requirement the body reached through *syntax* — `x - 1`, `x[k]`, `for _ in x` — gives
/// way. Such an operation used to be read against whatever else bounded the parameter, and
/// reported there when it did not fit: `def g(x=0): x + "foo"` is an error in `g`'s own body.
/// Recording the operation as a requirement instead moves that error to every call site —
/// including `g(0)`, which passes the very default the bound came from — and leaves the mistake
/// itself unreported. So where something the program states rules the operation out, the
/// statement wins and the operation is checked against it as before.
///
/// Another *recovered* requirement never rules one out this way. Two of those are two things the
/// same body asked for, and neither was ever going to imply the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum WhenContradicted<'db> {
    /// it stands, and the argument has to meet it as well. `def f(x="s"): x.extra()` asks for a
    /// `str` that also has an `extra`, which is a `str` subclass and is what the body plainly
    /// means
    Stands,
    /// it falls back to what the body asked for by *name*, which is everything it asked for
    /// except the operations
    Reduces(Type<'db>),
    /// it goes entirely, because the operations were all it asked for
    Goes,
}

/// The requirements in `constraints` that still say something.
///
/// A requirement some other requirement implies adds nothing to the bound, and leaving it in
/// costs precision rather than buying any: `def twice(n=1): return n * 2` learns `int` from the
/// default and `__mul__` from the body, and every `int` has that `__mul__`, but an intersection
/// of the two is a type nothing resolves an operator through, so the body stops reading `int`
/// back out of `n * 2`.
///
/// Implication is *assignability* and not subtyping, because a requirement this analysis wrote
/// down for an operator returns `Unknown` — a gradual type is assignable from anything and a
/// subtype of nothing, so subtyping would find no redundancy at all here.
///
/// Two requirements that imply each other are the same requirement twice, and the first of them
/// is the one that stays. A requirement something stated rules out is first cut down to what
/// [`WhenContradicted`] says is left of it.
fn settled_requirements<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    constraints: &[Requirement<'db>],
) -> Vec<Type<'db>> {
    let contradicted = |requirement: &Requirement<'db>| {
        constraints.iter().any(|other| {
            !other.ty.mentions_recovered_protocol(db, env)
                && !other.ty.is_assignable_to(db, env, requirement.ty)
        })
    };

    let kept: Vec<Type<'db>> = constraints
        .iter()
        .filter_map(|requirement| match requirement.when_contradicted {
            WhenContradicted::Stands => Some(requirement.ty),
            _ if !contradicted(requirement) => Some(requirement.ty),
            WhenContradicted::Reduces(reduced) => Some(reduced),
            WhenContradicted::Goes => None,
        })
        .collect();

    kept.iter()
        .enumerate()
        .filter(|(index, constraint)| {
            !kept
                .iter()
                .enumerate()
                .any(|(other_index, other)| match other_index.cmp(index) {
                    std::cmp::Ordering::Equal => false,
                    std::cmp::Ordering::Less => other.is_assignable_to(db, env, **constraint),
                    // a later requirement only absorbs an earlier one it is strictly stronger
                    // than, so two that imply each other do not absorb each other away
                    std::cmp::Ordering::Greater => {
                        other.is_assignable_to(db, env, **constraint)
                            && !constraint.is_assignable_to(db, env, *other)
                    }
                })
        })
        .map(|(_, constraint)| *constraint)
        .collect()
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
    // the definition's own program, not the one `db.program_file(file)` answers for the
    // file: those part company for a vendored stub reached from a pep 723 script
    let program_file = function.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let index = semantic_index(db, program_file);
    let DefinitionKind::Function(function_kind) = function.kind(db) else {
        return ParameterConstraints::default();
    };
    let node = function_kind.node(&module);
    let Some(body_scope) = index
        .try_node_scope(NodeWithScopeRef::Function(node))
        .map(|scope| scope.to_scope_id(db, program_file))
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
    let place_table = index.place_table(body_scope.file_scope_id(db));
    let single_bindings: FxHashSet<Name> = place_table
        .symbols()
        .filter(|symbol| symbol.is_bound() && !symbol.is_reassigned())
        .map(|symbol| symbol.name().clone())
        .collect();

    let parameters: BTreeMap<Name, Definition<'db>> = node
        .parameters
        .iter_non_variadic_params()
        .filter(|parameter| parameter.parameter.annotation().is_none())
        .map(|parameter| {
            (
                parameter.parameter.name.id.clone(),
                index.expect_single_definition(&parameter.parameter),
            )
        })
        .collect();

    let mut collector = UseCollector {
        db,
        env: env.clone(),
        file,
        use_def: use_def_map(db, body_scope),
        expression_type: |expr: &Expr| inference.expression_type(expr),
        uses: FxHashMap::default(),
        sinks: FxHashMap::default(),
        declared_return,
        parameters,
        locals: BTreeMap::default(),
        narrowed: FxHashSet::default(),
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

    let mut unstatable = Vec::new();
    let mut entries = path_bounds(db, env, uses, &mut unstatable);

    // a use through a name a test narrowed is not a use of the argument, and no requirement was
    // built from it — but the narrowed name still carries the hole, so the bound goes on being
    // checked against it. that is the same thing a use nothing can be said about does, reached by
    // a route the walk does not see, so it costs the same thing.
    //
    // it costs it only where there is a *recovered* bound to lose. the bound and the body are
    // settled by running them against each other, and a round that has read nothing off the body
    // yet has nothing for such a use to fail against — while answering otherwise would take a
    // bound away on the strength of that round and never give it back, since a parameter never
    // becomes statable again. it is also what leaves an `assert` readable from inside its own
    // test: `assert isinstance(x, int) and x <= 5` narrows `x` for its second arm, and the round
    // that reads that arm is the round `int` is already `x`'s bound, where the narrowing leaves
    // the hole itself and the use is recorded like any other
    unstatable.extend(
        entries
            .iter()
            .map(|(parameter, _)| *parameter)
            .filter(|parameter| collector.narrowed.contains(parameter)),
    );

    entries.extend(asserted_parameter_types(db, env, index, body_scope, node));

    // a parameter a nested scope captured keeps nothing: that body is checked against this
    // bound, and this walk never saw what it does with the name.
    //
    // a parameter its own body rebinds keeps nothing either, for the same reason a rebound
    // local does. after
    //
    //     while tb.tb_next:
    //         tb = tb.tb_next
    //
    // the name stands for whatever the rebinding produced, not for what the caller passed, so
    // the uses below it are no requirement on the argument — and bounding the argument by them
    // anyway makes the body fail against its own bound, because the rebinding lands on the
    // member type the bound itself invented
    entries.retain(|(parameter, _)| {
        parameter_definition_name(db, *parameter).is_none_or(|name| {
            !collector.captured.contains(&name)
                && place_table
                    .symbol_id(name.as_str())
                    .is_some_and(|symbol| !place_table.symbol(symbol).is_reassigned())
        })
    });

    entries.sort_by_key(|(parameter, _)| *parameter);
    entries.retain(|(parameter, _)| !unstatable.contains(parameter));
    unstatable.sort();
    unstatable.dedup();

    ParameterConstraints {
        entries: entries.into_boxed_slice(),
        unstatable: unstatable.into_boxed_slice(),
    }
}

/// The bound each parameter's body contributes, keyed by the parameter's definition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct ParameterConstraints<'db> {
    entries: Box<[(Definition<'db>, Requirement<'db>)]>,
    /// the parameters the body used in a way no requirement can state, which therefore keep no
    /// bound however much else the body said about them
    unstatable: Box<[Definition<'db>]>,
}

impl<'db> ParameterConstraints<'db> {
    /// Every bound recorded for `parameter`. A parameter can appear more than once — an
    /// `assert` and a use are separate requirements that both have to hold.
    fn get(&self, parameter: Definition<'db>) -> Vec<Requirement<'db>> {
        if self.unstatable.contains(&parameter) {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|(key, _)| *key == parameter)
            .map(|(_, requirement)| *requirement)
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
    ///
    /// A parameter found to be unstatable is the one thing that goes the other way, and it has to:
    /// a use this analysis cannot state is a fact about the body in exactly the way a requirement
    /// is, and the rule above would otherwise resurrect the very bound that use rules out. So the
    /// unstatable parameters accumulate too, and take their requirements with them — which is
    /// still a monotone iteration, because a parameter never becomes statable again.
    fn keeping_requirements_seen(self, previous: &Self) -> Self {
        let mut unstatable: Vec<Definition<'db>> = self
            .unstatable
            .iter()
            .chain(&previous.unstatable)
            .copied()
            .collect();
        unstatable.sort();
        unstatable.dedup();

        let mut entries: Vec<_> = self
            .entries
            .iter()
            .chain(previous.entries.iter().filter(|(parameter, _)| {
                !self
                    .entries
                    .iter()
                    .any(|(answered, _)| answered == parameter)
            }))
            .filter(|(parameter, _)| !unstatable.contains(parameter))
            .copied()
            .collect();
        entries.sort_by_key(|(parameter, _)| *parameter);

        Self {
            entries: entries.into_boxed_slice(),
            unstatable: unstatable.into_boxed_slice(),
        }
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
) -> Vec<(Definition<'db>, Requirement<'db>)> {
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
                asserted.push((
                    *definition,
                    Requirement {
                        ty: narrowed,
                        when_contradicted: WhenContradicted::Stands,
                    },
                ));
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
    /// members read off it, and what the body did with each
    members: BTreeMap<Name, MemberUses<'db>>,
    /// declared types the value itself had to fit: a place it was read into, or a parameter it
    /// was forwarded to
    value: Vec<Type<'db>>,
    /// whether the body did something with this value that no requirement can state
    unstatable: bool,
}

/// What one member of the value at a path was used for.
#[derive(Default)]
struct MemberUses<'db> {
    /// the parameters of every call the body made through this member. a member nothing called
    /// has none, and is asked for as a plain attribute
    calls: Vec<Parameters<'db>>,
    /// how the body got to the member, which decides what its value reads back as when nothing
    /// in the body says what that value is
    reach: MemberReach,
}

/// How a body reached a member: by writing its name, or through the syntax the member is the
/// meaning of.
///
/// This decides only what the member's value reads back as when nothing else says. A member the
/// body *named* reads back as `object`: the requirement is that it exist, and `object` is what a
/// value nothing was required of has to be.
///
/// A member reached through syntax reads back as `Unknown` instead. Recording a requirement is
/// about what the *call site* has to supply, and it should not change what the body itself reads.
/// `x[k]`, `x - 1` and the element of `for _ in x` all read as `Unknown` before any of this, and
/// answering `object` there hands a body that used to check a run of errors about a value the
/// analysis never learned anything about.
#[derive(Copy, Clone, Default, PartialEq, Eq)]
enum MemberReach {
    /// `x.foo`, `x.foo()`
    ByName,
    /// `x[k]`, `x - 1`, `-x`, `x(1)`, `for _ in x`
    #[default]
    ThroughSyntax,
}

/// The one call shape a member has to fit, given every call the body made through it.
///
/// Each call is a separate requirement and all of them have to hold, so the member has to accept
/// every argument any of them passed: a parameter is contravariant, so the shapes combine by
/// unioning position by position. `m.group("a")` and `m.group("b")` ask for a `group` that takes
/// `"a" | "b"`, which is what a real `group` accepts and what pinning the member to the first
/// call site rejected.
///
/// Calls that disagree about their *shape* — a different arity, a different keyword — cannot be
/// written as one signature at all; that needs an overload, which nothing here can spell. Such a
/// member degrades to the gradual form, which keeps the honest half of the requirement (the
/// member exists and is callable) without inventing a shape no argument could match.
fn merged_call_shape<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    shapes: &[Parameters<'db>],
) -> Parameters<'db> {
    let mut merged: Option<Parameters<'db>> = None;
    for shape in shapes {
        merged = Some(match merged {
            None => shape.clone(),
            Some(merged) => match merged_parameters(db, env, &merged, shape) {
                Some(merged) => merged,
                None => return callable_any_way(),
            },
        });
    }
    merged.unwrap_or_else(callable_any_way)
}

/// The shape of a member that only has to be callable — `(self, *args, **kwargs)`.
fn callable_any_way<'db>() -> Parameters<'db> {
    Parameters::standard([
        Parameter::positional_only(Some(Name::new_static("self"))),
        Parameter::variadic(Name::new_static("args")).with_annotated_type(Type::any()),
        Parameter::keyword_variadic(Name::new_static("kwargs")).with_annotated_type(Type::any()),
    ])
}

/// Two call shapes combined, or `None` when they do not line up.
fn merged_parameters<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    left: &Parameters<'db>,
    right: &Parameters<'db>,
) -> Option<Parameters<'db>> {
    if left.len() != right.len() {
        return None;
    }
    let mut merged = Vec::with_capacity(left.len());
    for (left, right) in left.iter().zip(right.iter()) {
        let combined = match (left.kind(), right.kind()) {
            (ParameterKind::PositionalOnly { name, .. }, ParameterKind::PositionalOnly { .. }) => {
                Parameter::positional_only(name.clone())
            }
            (
                ParameterKind::KeywordOnly {
                    name: left_name, ..
                },
                ParameterKind::KeywordOnly {
                    name: right_name, ..
                },
            ) if left_name == right_name => Parameter::keyword_only(left_name.clone()),
            _ => return None,
        };
        // an argument whose type could not be written down leaves its position unannotated,
        // which already accepts anything the other call passed there
        merged.push(
            if left.should_annotation_be_displayed() && right.should_annotation_be_displayed() {
                combined.with_annotated_type(UnionType::from_elements(
                    db,
                    env,
                    [left.annotated_type(), right.annotated_type()],
                ))
            } else {
                combined
            },
        );
    }
    Some(Parameters::standard(merged))
}

/// The bound each parameter's uses add up to.
///
/// The deepest paths are folded first, so that everything the body required of a member's value
/// is already a type by the time the member that produced it is written down. A path with both a
/// value requirement and members of its own intersects them, the same way the parameter's own
/// bound intersects its protocol with the types it was forwarded into.
///
/// A path the body used in a way no requirement can state resolves to the gradual type instead of
/// to what its other uses added up to. That is what keeps a recovered signature able to type the
/// body it was recovered from: a bound is checked against *every* use, including the ones it could
/// not be built from, and a gradual type is the only one all of them are guaranteed to pass. For
/// the parameter's own path that means it keeps no bound at all; for a member's it means the member
/// still has to exist and be callable the way the body called it, but nothing is claimed about the
/// value it hands back.
fn path_bounds<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    uses: FxHashMap<MemberPath<'db>, PathUses<'db>>,
    unstatable: &mut Vec<Definition<'db>>,
) -> Vec<(Definition<'db>, Requirement<'db>)> {
    let mut uses: Vec<_> = uses.into_iter().collect();
    uses.sort_by_key(|(path, _)| std::cmp::Reverse(path.members.len()));

    let mut resolved: FxHashMap<MemberPath<'db>, Type<'db>> = FxHashMap::default();
    let mut entries = Vec::new();
    for (path, path_uses) in uses {
        if path_uses.unstatable {
            if path.members.is_empty() {
                unstatable.push(path.parameter);
            }
            resolved.insert(path, Type::unknown());
            continue;
        }

        // the members the body named and the members it reached through syntax go into one
        // protocol, but they are kept apart on the way there: what becomes of the requirement
        // when the rest of the bound rules it out depends on which of the two it is
        let mut named = Vec::new();
        let mut through_syntax = Vec::new();
        for (name, uses) in path_uses.members {
            // a member the body never constrained is one nothing is known about, and the
            // way to say that is a gradual type. `object` would be a stronger claim than
            // the source made — it does not describe the value, it forbids every use of
            // it, and that travels: the member's type becomes the function's own return
            // type, so an unannotated helper's result reached callers unusable. reading
            // `yaml.safe_load(fp)` as `object` made `config["plugins"]` an error, and no
            // annotation could take it back — `object` is not assignable to `dict`
            let value = resolved
                .get(&path.member(&name))
                .copied()
                .unwrap_or_else(Type::unknown);
            let member = if uses.calls.is_empty() {
                InlineProtocolMember::ReadOnlyAttribute(value)
            } else {
                InlineProtocolMember::Method(CallableType::function_like(
                    db,
                    Signature::new(merged_call_shape(db, env, &uses.calls), value),
                ))
            };
            match uses.reach {
                MemberReach::ByName => named.push((name, member)),
                MemberReach::ThroughSyntax => through_syntax.push((name, member)),
            }
        }

        // a path with both a value requirement and members of its own intersects them, the same
        // way the parameter's own bound intersects its protocol with the types it was forwarded
        // into
        let asking_for = |members: Vec<(Name, InlineProtocolMember<'db>)>| {
            if members.is_empty() && path_uses.value.is_empty() {
                return None;
            }
            let mut bound = IntersectionBuilder::new(db, env);
            if !members.is_empty() {
                bound = bound.add_positive(Type::recovered_protocol(db, env, members));
            }
            for value in &path_uses.value {
                bound = bound.add_positive(*value);
            }
            Some(bound.build())
        };

        let when_contradicted = if through_syntax.is_empty() {
            WhenContradicted::Stands
        } else {
            match asking_for(named.clone()) {
                Some(named_only) => WhenContradicted::Reduces(named_only),
                None => WhenContradicted::Goes,
            }
        };
        let Some(bound) = asking_for(named.into_iter().chain(through_syntax).collect()) else {
            continue;
        };

        if path.members.is_empty() {
            entries.push((
                path.parameter,
                Requirement {
                    ty: bound,
                    when_contradicted,
                },
            ));
        }
        resolved.insert(path, bound);
    }
    entries
}

/// The `for` clauses of a comprehension.
fn comprehension_generators(expr: &Expr) -> &[ast::Comprehension] {
    match expr {
        Expr::ListComp(comprehension) => &comprehension.generators,
        Expr::SetComp(comprehension) => &comprehension.generators,
        Expr::DictComp(comprehension) => &comprehension.generators,
        Expr::Generator(comprehension) => &comprehension.generators,
        _ => &[],
    }
}

/// The dunder a binary operator dispatches to, for the operators that exist at runtime.
///
/// basedpython's own `??` and `?` are written as binary operators but are not dispatched through
/// a method, so a body that uses one requires nothing of its operands.
fn runtime_dunder(op: ast::Operator) -> Option<&'static str> {
    match op {
        ast::Operator::Coalesce | ast::Operator::Result => None,
        op => Some(op.dunder()),
    }
}

/// The dunder a unary operator dispatches to.
///
/// `not x` is missing because it dispatches to `__bool__`, which every object has, so it is no
/// requirement at all. basedpython's postfix operators are not dispatched through a method.
fn unary_dunder(op: ast::UnaryOp) -> Option<&'static str> {
    match op {
        ast::UnaryOp::Invert => Some("__invert__"),
        ast::UnaryOp::UAdd => Some("__pos__"),
        ast::UnaryOp::USub => Some("__neg__"),
        ast::UnaryOp::Not
        | ast::UnaryOp::Optional
        | ast::UnaryOp::Propagate
        | ast::UnaryOp::Force => None,
    }
}

/// The dunder a comparison dispatches to, for the comparisons that require anything.
///
/// `==`, `!=` and the identity tests are answered by `object` itself, so they are no requirement.
/// `in` is left out for the opposite reason: it can run through `__contains__`, `__iter__` *or*
/// `__getitem__` on the right operand, and asking for any one of the three would demand something
/// the body never needed. That disjunction is not a shape a protocol member can state.
fn comparison_dunder(op: ast::CmpOp) -> Option<&'static str> {
    match op {
        ast::CmpOp::Lt => Some("__lt__"),
        ast::CmpOp::LtE => Some("__le__"),
        ast::CmpOp::Gt => Some("__gt__"),
        ast::CmpOp::GtE => Some("__ge__"),
        ast::CmpOp::Eq
        | ast::CmpOp::NotEq
        | ast::CmpOp::Is
        | ast::CmpOp::IsNot
        | ast::CmpOp::In
        | ast::CmpOp::NotIn => None,
    }
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

/// What the place a value is being read into says the value has to be.
///
/// Every position a tracked value can be written in gets one of these from the construct that
/// wrote it there, and a position that gets none is one this analysis did not account for. That is
/// the whole of how the walk stays honest: it is [`UseCollector::visit_expr`]'s *default* to treat
/// a tracked value as used in a way it cannot state, so a construct nobody has taught it about
/// costs a bound rather than inventing a requirement that does not hold.
#[derive(Debug, Clone, Copy)]
enum Sink<'db> {
    /// the place takes whatever it is handed — an expression statement's value, a `bool` test, an
    /// argument to a parameter typed `object` — so nothing about it has to be recorded for the
    /// body to go on checking
    Anything,
    /// the place says what it holds, and says it in something a call site could be asked for
    Required(Type<'db>),
}

/// Reads a body for what it does with each parameter hole, and with each value reached from one.
///
/// A use of the parameter itself is recognised by the *type* of the expression it is made on, not
/// by the name written there: a parameter that was reassigned, or narrowed, no longer has the
/// hole's type, and nothing it is then used for is a requirement on the argument. A local that a
/// value was assigned to has no such type of its own, so the same two questions — is this still
/// that value, and is it narrowed — are asked of its bindings instead.
///
/// The walk is *fail-closed*. Each construct declares, for every sub-expression it is made of, what
/// that position asks of the value written there — a [`Sink`] — and a sub-expression left without
/// one is taken to be a use that cannot be stated. So the requirements a bound is built from are
/// exactly the uses the walk understood, and every use it did not understand takes the bound away
/// instead of being silently passed over. That is what makes the bound one the body itself still
/// type-checks against, which is the only kind of bound worth recovering.
struct UseCollector<'db, F> {
    db: &'db dyn Db,
    env: ProgramEnvironment<'db>,
    file: File,
    use_def: &'db UseDefMap<'db>,
    expression_type: F,
    uses: FxHashMap<MemberPath<'db>, PathUses<'db>>,
    /// what each sub-expression the walk has not reached yet is being read into, keyed by its
    /// range. a construct fills this in for its own parts before walking them
    sinks: FxHashMap<TextRange, Sink<'db>>,
    /// the function's declared return type, when it wrote one down. an *inferred* return
    /// type is no constraint on the body — it is read off it
    declared_return: Option<Type<'db>>,
    /// the function's own unannotated parameters, by the name the body writes them under. a name
    /// that is one of these is the only one a *narrowing* of the argument can be written on
    parameters: BTreeMap<Name, Definition<'db>>,
    /// locals a value at some path was assigned to, so that what is done with the local — and a
    /// later `assert` about it — reads back as a requirement on that value
    locals: BTreeMap<Name, MemberPath<'db>>,
    /// the parameters the body used through a name a test had narrowed, which is a use no
    /// requirement was built from and the bound is checked against anyway
    narrowed: FxHashSet<Definition<'db>>,
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

    /// Whether `expr` reads `parameter`'s hole *narrowed*: something narrower than the argument,
    /// but still described in terms of it.
    ///
    /// A name a test narrowed stands for something narrower than the argument, so nothing done
    /// with it is a requirement on the argument and [`Self::hole`] rightly does not answer for it.
    /// It is still the same argument underneath, though, and the type it now has still mentions
    /// the hole — so the body goes on being checked against whatever bound the hole ends up with,
    /// through a use no requirement was ever built from. That is the one way a use reaches the
    /// bound without this walk seeing it, and it is closed the way every unreadable use is: the
    /// parameter keeps no bound.
    ///
    /// A narrowing the bound already implies is not one of these, and needs no special case to say
    /// so. `assert isinstance(x, int)` puts `int` into the bound, and from there narrowing `x` to
    /// an `int` leaves the hole itself — which is the one case where recording the use is safe,
    /// because what is recorded is then checked against the very bound it went into. Neither is a
    /// narrowing that left a type of its own, with the hole nowhere in it: the use is then checked
    /// against that type, and the bound has nothing to do with it.
    fn narrows_hole(&self, expr: &Expr, parameter: Definition<'db>, name: &Name) -> bool {
        let db = self.db;
        let ty = (self.expression_type)(expr);
        // the argument itself rather than a narrowing of it
        if ty.is_inferred_parameter_hole(db) {
            return false;
        }
        let env = self.env.clone();
        ty.references_typevar(
            db,
            &env,
            inferred_parameter_typevar(db, name, parameter).identity(db),
        )
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
    ///
    /// `Never` is ruled out too. Nothing is of that type, so it is never a requirement anybody
    /// could meet — and it is what an empty collection the body is still filling reads back as, so
    /// taking it at its word would ask a member to hand back a value that cannot exist.
    ///
    /// A use-site modifier — `final T`, `literal T` — is ruled out on the same grounds. It says how
    /// the value is used at the one place it is written, not what kind of thing the value is, so it
    /// is not something a call site could be asked to supply.
    fn portable(&self, ty: Type<'db>) -> Option<Type<'db>> {
        let env = self.env.clone();
        (!ty.has_typevar_or_typevar_instance(self.db, &env)
            && !ty.is_dynamic()
            && !ty.is_object()
            && !ty.is_never()
            && !matches!(ty, Type::Restricted(_))
            && !ty.mentions_recovered_protocol(self.db, &env))
        .then_some(ty)
    }

    /// The type an argument contributes to the shape of the member it was passed to.
    ///
    /// An argument is a *sample* of what the member has to accept rather than the only thing it
    /// will ever be handed, so it is promoted first — the same reading a parameter's default
    /// value gets. Without that, `i + 1` asks for an `__add__` that takes `Literal[1]`, and a
    /// second function whose own body writes `i + 3` cannot pass its parameter to the first:
    /// two requirements that any `int` meets would fail against each other.
    ///
    /// Only a literal is widened that way. Anything else an argument already is says what it is,
    /// and a shape the *program* stated — the intersection a `match` pattern or a `hasattr`
    /// established — is a requirement in its own right; widening it would throw away the part of
    /// the shape that was stated rather than sampled.
    fn argument_type(&self, expr: &Expr) -> Option<Type<'db>> {
        let env = self.env.clone();
        let argument = (self.expression_type)(expr);
        let sample = if argument.is_literal_or_union_of_literals(self.db, &env) {
            argument.promote(self.db, &env)
        } else {
            argument
        };
        self.portable(sample)
    }

    /// Record that the value at `path` has to have a member called `name`, shaped like
    /// `called` when it was called at all, and reached the way `reach` says.
    fn record_member(
        &mut self,
        path: &MemberPath<'db>,
        name: &Name,
        called: Option<Parameters<'db>>,
        reach: MemberReach,
    ) {
        let member = self
            .uses
            .entry(path.clone())
            .or_default()
            .members
            .entry(name.clone())
            .or_default();
        if let Some(called) = called {
            member.calls.push(called);
        }
        // a member the body reached by name as well as through syntax — `d[k]` and
        // `d.__getitem__(k)` — was named
        if reach == MemberReach::ByName {
            member.reach = reach;
        }
    }

    /// The parameters a member has to take to be callable the way the body called it: a receiver,
    /// then one positional per operand, typed as far as the operand's own type can be written
    /// down.
    ///
    /// The operators, subscripting and iteration all reach their member this way — Python spells
    /// them as syntax, but each one is a call on an ordinary member, and so is an ordinary
    /// requirement on the value it was written against.
    /// An operand written as `None` stands for a value the syntax passes but does not name — the
    /// value `x[k] += 1` stores back — and leaves its position unannotated, which is what says
    /// the member has to take one without saying what.
    fn operand_parameters(&self, operands: &[Option<&Expr>]) -> Parameters<'db> {
        let mut parameters = vec![Parameter::positional_only(Some(Name::new_static("self")))];
        for operand in operands {
            let mut parameter = Parameter::positional_only(None);
            if let Some(ty) = operand.and_then(|operand| self.argument_type(operand)) {
                parameter = parameter.with_annotated_type(ty);
            }
            parameters.push(parameter);
        }
        Parameters::standard(parameters)
    }

    /// Record a dunder the body reached through syntax — `x[k]`, `x - 1`, `-x` — as a member the
    /// value at `path` has to have.
    fn record_operator(
        &mut self,
        path: &MemberPath<'db>,
        dunder: &'static str,
        operands: &[Option<&Expr>],
        sink: Option<Type<'db>>,
    ) {
        let dunder = Name::new_static(dunder);
        let parameters = self.operand_parameters(operands);
        self.record_member(path, &dunder, Some(parameters), MemberReach::ThroughSyntax);
        if let Some(sink) = sink {
            self.record_value(path.member(&dunder), sink);
        }
    }

    /// Record that the value at `path` has to be iterable, and answer the path its elements are
    /// reached by.
    ///
    /// Iteration is two members deep: `__iter__` hands back an iterator, and that iterator's
    /// `__next__` hands back an element. Recording both is what keeps the elements a value in
    /// their own right, so that what the loop body does with the loop variable is a requirement
    /// on what the argument yields rather than on the argument itself.
    fn record_iteration(&mut self, path: &MemberPath<'db>) -> MemberPath<'db> {
        let iter = Name::new_static("__iter__");
        let next = Name::new_static("__next__");
        let receiver_only = self.operand_parameters(&[]);
        self.record_member(
            path,
            &iter,
            Some(receiver_only.clone()),
            MemberReach::ThroughSyntax,
        );
        let iterator = path.member(&iter);
        self.record_member(
            &iterator,
            &next,
            Some(receiver_only),
            MemberReach::ThroughSyntax,
        );
        iterator.member(&next)
    }

    /// Record that `target` names the value at `path`, when it is a name that stands for one
    /// value.
    /// Answers whether it did: a name bound more than once cannot stand for one value, so the
    /// uses below it are not uses of the value that was assigned to it and this walk cannot see
    /// them at all.
    fn bind_local(&mut self, target: &Expr, path: MemberPath<'db>) -> bool {
        if let Expr::Name(target) = target
            && self.single_bindings.contains(&target.id)
        {
            self.locals.insert(target.id.clone(), path);
            return true;
        }
        false
    }

    /// Say what `expr`'s position asks of the value written there, for the walk to read when it
    /// reaches it.
    fn reads_into(&mut self, expr: &Expr, sink: Sink<'db>) {
        self.sinks.insert(expr.range(), sink);
    }

    /// Say that `expr`'s position asks nothing of the value written there.
    fn accounted_for(&mut self, expr: &Expr) {
        self.reads_into(expr, Sink::Anything);
    }

    /// Say that each of `exprs` sits in a position that asks nothing.
    fn all_accounted_for<'e>(&mut self, exprs: impl IntoIterator<Item = &'e Expr>) {
        for expr in exprs {
            self.accounted_for(expr);
        }
    }

    /// Whether `call` still binds when the argument written at `range` is `object`.
    ///
    /// This is the question a position that could not be read any other way is settled by. A
    /// position that accepts `object` accepts every type this analysis could recover, because a
    /// recovered bound is an ordinary type and `object` is above all of them — so such a position
    /// asks nothing that has to be written down, and the body goes on checking whatever the bound
    /// turns out to be.
    ///
    /// A position that does *not* accept `object` may still accept some particular bound, but
    /// which one is not a thing a requirement can state: the checker has to pick an overload, or
    /// a union element, and the argument is what decides which. So the value written there is
    /// left unstatable rather than guessed at.
    fn call_accepts_anything(&self, call: &ast::ExprCall, range: TextRange) -> bool {
        let db = self.db;
        let env = self.env.clone();
        let callee = (self.expression_type)(&call.func);
        let arguments = CallArguments::from_arguments_typed(&call.arguments, |expr| {
            if expr.range() == range {
                Type::object()
            } else {
                (self.expression_type)(expr)
            }
        });
        callee
            .bindings(db, &env)
            .match_parameters(db, &env, &arguments)
            .check_types(
                db,
                &env,
                &ConstraintSetBuilder::new(),
                &arguments,
                TypeContext::default(),
                &[],
            )
            .is_ok()
    }

    /// Whether `receiver`'s `dunder` takes `object`, which is what the syntax that dispatches to
    /// it asks of the operand it passes there.
    ///
    /// The same reading as [`Self::call_accepts_anything`], for the operators. It is a *sufficient*
    /// test and not an exact one: `left + right` can also succeed through `right`'s reflected
    /// dunder, so an operand this says nothing about may still be fine. What it does establish is
    /// that the operand is fine whatever it turns out to be, which is all that is being asked.
    /// It is only asked where the answer can change one, because asking is a call of its own: a
    /// query run from inside the fixed point that settles a parameter's bound joins that fixed
    /// point, and one asked about an expression holding no tracked value would join it for nothing.
    fn dunder_accepts_anything(&self, operand: &Expr, receiver: &Expr, dunder: &str) -> bool {
        if !self.mentions_tracked_value(operand) {
            return false;
        }
        let env = self.env.clone();
        (self.expression_type)(receiver)
            .try_call_dunder(
                self.db,
                &env,
                dunder,
                CallArguments::positional([Type::object()]),
                TypeContext::default(),
            )
            .is_ok()
    }

    /// What a place declaring `declared` asks of the value read into it, or `None` when what it
    /// asks is something this analysis cannot write down.
    fn declared_sink(&self, declared: Type<'db>) -> Option<Sink<'db>> {
        if let Some(required) = self.portable(declared) {
            return Some(Sink::Required(required));
        }
        // a gradual place, or one typed `object`, accepts whatever it is handed, so a value read
        // into it needs nothing recorded for the body to keep checking
        (declared.is_dynamic() || declared.is_object()).then_some(Sink::Anything)
    }

    /// Say that `expr` is read into a place declaring `declared`.
    fn reads_into_declared(&mut self, expr: &Expr, declared: Type<'db>) {
        if let Some(sink) = self.declared_sink(declared) {
            self.reads_into(expr, sink);
        }
    }

    /// Record that the value at `path` was used in a way no requirement can state.
    fn mark_unstatable(&mut self, path: &MemberPath<'db>) {
        self.uses.entry(path.clone()).or_default().unstatable = true;
    }

    /// Say that each argument of a call whose shape was recorded is accounted for.
    ///
    /// The shape was written down *from these very arguments* — each position takes what the
    /// argument there already is, or takes anything where its type could not be written down — so
    /// every one of them fits it by construction.
    fn account_for_recorded_call_shape(&mut self, call: &ast::ExprCall) {
        for argument in call.arguments.iter_source_order() {
            match argument {
                ast::ArgOrKeyword::Arg(argument) => self.accounted_for(argument),
                ast::ArgOrKeyword::Keyword(keyword) => self.accounted_for(&keyword.value),
            }
        }
    }

    /// Say that `expr` is read into wherever the expression it is a possible value of was read
    /// into.
    ///
    /// `x or y`, `x if c else y` and `(a := x)` all hand on whatever they are given, so an arm of
    /// one sits in the position the whole expression sits in — including, when that position was
    /// one this analysis did not account for, in having no position at all.
    fn hands_on(&mut self, expr: &Expr, sink: Option<Sink<'db>>) {
        if let Some(sink) = sink {
            self.reads_into(expr, sink);
        }
    }

    /// Say that the parts of a display or a slice are accounted for, when the display or slice
    /// itself was asked nothing.
    ///
    /// A display builds a container this analysis does not describe, so it can say nothing about
    /// what a place expecting a particular container asks of the elements. Where the whole was
    /// asked nothing, though, neither is any part of it.
    fn parts_accounted_for<'e>(
        &mut self,
        parts: impl IntoIterator<Item = &'e Expr>,
        sink: Option<Sink<'db>>,
    ) {
        if matches!(sink, Some(Sink::Anything)) {
            self.all_accounted_for(parts);
        }
    }

    /// Record that whatever tracked value `expr` names was used in a way no requirement can state.
    ///
    /// A name a test narrowed counts too, though not as the same thing: nothing here can say what
    /// the argument would have to be for such a use to pass either, but what the use is really
    /// about is the narrower value, so it is recorded separately.
    fn cannot_state(&mut self, expr: &Expr) {
        match self.path(expr) {
            Some(path) => self.mark_unstatable(&path),
            None => self.mark_narrowed(expr),
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
        self.record_member(path, name, None, MemberReach::ByName);
        if let Some(sink) = sink {
            self.record_value(path.member(name), sink);
        }
    }

    /// Record `x.name(...)` as a method the value at `path` has to have, shaped like the call and
    /// returning something that fits wherever the result was read into.
    ///
    /// Answers whether the call was one that could be written down as a shape at all. A splatted
    /// argument is not: `x.foo(*args)` passes however many elements `args` turns out to have, and
    /// no fixed parameter list says that. Nothing is recorded then — not even that `foo` exists —
    /// because the caller marks the whole value unstatable instead, and a member requirement built
    /// from the *rest* of the body would be checked against this call and fail against it.
    fn record_call(
        &mut self,
        path: &MemberPath<'db>,
        name: &Name,
        call: &ast::ExprCall,
        sink: Option<Type<'db>>,
        reach: MemberReach,
    ) -> bool {
        let mut parameters = vec![Parameter::positional_only(Some(Name::new_static("self")))];
        for argument in &call.arguments.args {
            if argument.is_starred_expr() {
                return false;
            }
            let mut parameter = Parameter::positional_only(None);
            if let Some(ty) = self.argument_type(argument) {
                parameter = parameter.with_annotated_type(ty);
            }
            parameters.push(parameter);
        }
        for keyword in &call.arguments.keywords {
            let Some(argument_name) = keyword.arg.as_ref() else {
                return false;
            };
            let mut parameter = Parameter::keyword_only(argument_name.id.clone());
            if let Some(ty) = self.argument_type(&keyword.value) {
                parameter = parameter.with_annotated_type(ty);
            }
            parameters.push(parameter);
        }

        self.record_member(path, name, Some(Parameters::standard(parameters)), reach);
        if let Some(sink) = sink {
            self.record_value(path.member(name), sink);
        }
        true
    }

    /// The value a *name* stands for: the parameter's own hole, or a local a value at some path
    /// was assigned to.
    fn name_path(&self, expr: &Expr) -> Option<MemberPath<'db>> {
        let db = self.db;
        // a name being written to is not a read of the value it is about to hold
        if let Expr::Name(name) = expr
            && !name.ctx.is_load()
        {
            return None;
        }
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

    /// The parameter `expr` is a *narrowing* of, when a test narrowed the name written there.
    ///
    /// Only a parameter's own name is answered for. A narrowing applies to a place, and the other
    /// places it can apply to — `x.foo`, `x[0]` — are ones [`Self::path`] already answers for
    /// whether or not they were narrowed, so their uses are recorded rather than lost. A *local*
    /// stands for a value reached through the parameter rather than for the parameter, so a
    /// narrowing of one says nothing about the argument even where the local's type is written in
    /// terms of it.
    fn narrowed_parameter(&self, expr: &Expr) -> Option<Definition<'db>> {
        let name = expr.as_name_expr()?;
        if !name.ctx.is_load() {
            return None;
        }
        let parameter = *self.parameters.get(&name.id)?;
        // the name still stands for the argument itself, so its uses are recorded as usual
        if self.name_path(expr).is_some() {
            return None;
        }
        self.narrows_hole(expr, parameter, &name.id)
            .then_some(parameter)
    }

    /// Record that the body did something with `parameter` narrowed.
    fn mark_narrowed(&mut self, expr: &Expr) {
        if let Some(parameter) = self.narrowed_parameter(expr) {
            self.narrowed.insert(parameter);
        }
    }

    /// The value `expr` produces, when the body says which one it is: a name for one, or a
    /// member read, method call or subscript on one — the `x.foo()` of `a = x.foo()`.
    fn path(&self, expr: &Expr) -> Option<MemberPath<'db>> {
        let attribute = match expr {
            Expr::Call(call) => call.func.as_attribute_expr(),
            Expr::Attribute(attribute) if attribute.ctx.is_load() => Some(attribute),
            Expr::Attribute(_) => return None,
            _ => None,
        };
        if let Some(attribute) = attribute {
            return Some(self.path(&attribute.value)?.member(&attribute.attr.id));
        }
        // the value of `x[k]` is what `x`'s `__getitem__` handed back, and the value of `f(...)`
        // where `f` is itself a hole is what its `__call__` did, in exactly the way the value of
        // `x.foo()` is what `foo` did
        match expr {
            Expr::Subscript(subscript) if subscript.ctx.is_load() => Some(
                self.path(&subscript.value)?
                    .member(&Name::new_static("__getitem__")),
            ),
            Expr::Call(call) => self
                .name_path(expr)
                .or_else(|| Some(self.path(&call.func)?.member(&Name::new_static("__call__")))),
            _ => self.name_path(expr),
        }
    }

    /// Say what each of `call`'s arguments is read into: the parameter it was matched to.
    ///
    /// That parameter type serves twice: an argument that *is* a hole has to fit it, and an
    /// argument that reads a member off a hole makes that member's value have to fit it.
    ///
    /// An argument whose parameter could not be worked out — the callee is a union, or overloaded,
    /// or the call does not match its signature at all — gets no sink, and a tracked value written
    /// there is one this cannot state. The parameter is a real requirement whether or not this
    /// analysis can read it, and a bound built as though the argument were not there would be
    /// checked against this very call.
    fn account_for_call_arguments(&mut self, call: &ast::ExprCall) {
        let env = self.env.clone();
        // a splatted argument hands the callee its *elements*, or its values under their own
        // names, so the parameter it lands on says nothing about the argument itself. taking
        // that parameter as a requirement would bound a hole by what it is expected to contain
        // — `def f(it): C(*it)` would require `it` to *be* an `int` rather than to yield them.
        // what `*it` does ask for — that it be iterable — is recorded where the splat is written
        let arguments: Vec<(&Expr, bool)> = call
            .arguments
            .iter_source_order()
            .map(|argument| match argument {
                ast::ArgOrKeyword::Arg(argument) => (argument, argument.is_starred_expr()),
                ast::ArgOrKeyword::Keyword(keyword) => (&keyword.value, keyword.arg.is_none()),
            })
            .collect();

        // binding a call is the expensive half of this analysis, and most calls in a body have
        // nothing to do with any hole. only an argument with a value reached from one anywhere
        // inside it has anything to learn from the parameter it was matched to
        let tracked: Vec<bool> = arguments
            .iter()
            .map(|(argument, splatted)| !splatted && self.mentions_tracked_value(argument))
            .collect();
        if !tracked.iter().any(|tracked| *tracked) {
            // no argument of this call reads a value the walk tracks, so no position in it needs
            // accounting for and the expensive half below is skipped
            for (argument, splatted) in &arguments {
                if *splatted && argument.is_starred_expr() {
                    self.accounted_for(argument);
                }
            }
            return;
        }

        let callee = (self.expression_type)(&call.func);
        let parameter_types =
            call_parameter_types(self.db, &env, callee, &call.arguments, |expr| {
                (self.expression_type)(expr)
            });

        for (index, (argument, splatted)) in arguments.into_iter().enumerate() {
            if splatted {
                // `*xs` asks that `xs` be iterable, which the splat itself records; `**xs` asks
                // that it be a mapping, which nothing here can state
                if argument.is_starred_expr() {
                    self.accounted_for(argument);
                }
                continue;
            }
            let read_into = match parameter_types.as_ref().and_then(|types| types.get(index)) {
                // an unannotated parameter accepts anything
                Some(None) => Some(Sink::Anything),
                Some(Some(declared)) => self.declared_sink(*declared),
                None => None,
            };
            match read_into {
                Some(sink) => self.reads_into(argument, sink),
                // no parameter type was readable, or the one that was is not something this can
                // write down. the position is still accounted for if it would take `object`
                None if !tracked[index] || self.call_accepts_anything(call, argument.range()) => {
                    self.accounted_for(argument);
                }
                None => {}
            }
        }
    }

    /// Whether `expr` reads a value this walk tracks, anywhere inside it.
    ///
    /// A name a test narrowed counts. What the position asks of it is not a requirement on the
    /// argument, but it still has to be *read*: a position that takes anything takes a narrowed
    /// value too, and skipping it would leave a `print(x)` under an `if x:` looking like a use
    /// nothing accounted for.
    fn mentions_tracked_value(&self, expr: &Expr) -> bool {
        let tracked =
            |expr: &Expr| self.path(expr).is_some() || self.narrowed_parameter(expr).is_some();
        if tracked(expr) {
            return true;
        }
        let mut search = TrackedValueSearch {
            found: false,
            is_tracked: &tracked,
        };
        walk_expr(&mut search, expr);
        search.found
    }
}

/// Looks for a value a walk tracks, anywhere inside a subtree.
struct TrackedValueSearch<'a> {
    found: bool,
    is_tracked: &'a dyn Fn(&Expr) -> bool,
}

impl Visitor<'_> for TrackedValueSearch<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if self.found {
            return;
        }
        if (self.is_tracked)(expr) {
            self.found = true;
            return;
        }
        walk_expr(self, expr);
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

            // the value of an expression statement is thrown away, so the statement asks nothing
            // of it
            Stmt::Expr(expr) => {
                self.accounted_for(&expr.value);
                walk_stmt(self, stmt);
            }

            // `a = x.foo()` gives a name to the value at a path, so what the body goes on to do
            // with that name is a requirement on that value. the value itself is recorded by
            // the walk below as usual
            Stmt::Assign(assign) => {
                if let [target] = assign.targets.as_slice()
                    && let Some(path) = self.path(&assign.value)
                    && self.bind_local(target, path)
                {
                    // the name carries every use below it, so the assignment itself asks nothing
                    self.accounted_for(&assign.value);
                }
                for target in &assign.targets {
                    match target {
                        // `x[k] = v` is a `__setitem__` call, whose second argument is the
                        // value being assigned rather than anything inside the subscript
                        Expr::Subscript(subscript) => {
                            if let Some(path) = self.path(&subscript.value) {
                                self.record_operator(
                                    &path,
                                    "__setitem__",
                                    &[Some(&*subscript.slice), Some(&*assign.value)],
                                    None,
                                );
                                self.accounted_for(&subscript.value);
                                self.accounted_for(&subscript.slice);
                                self.accounted_for(&assign.value);
                            }
                        }
                        // `a, b = xs` takes `xs` apart by iterating it, the same way a `for`
                        // does
                        Expr::Tuple(_) | Expr::List(_) => {
                            if let Some(path) = self.path(&assign.value) {
                                self.record_iteration(&path);
                                self.accounted_for(&assign.value);
                            }
                        }
                        _ => {}
                    }
                }
                walk_stmt(self, stmt);
            }

            // `x[k] += 1` reads `x[k]`, operates on it and stores it back, so it asks for all
            // three. the operator asked for is the plain one rather than the in-place one:
            // Python falls back from `__iadd__` to `__add__`, and a type that defines only the
            // in-place form is far rarer than one — `int`, `str`, `tuple` — that defines only
            // the plain form
            Stmt::AugAssign(assign) => {
                if let Some(dunder) = runtime_dunder(assign.op) {
                    match &*assign.target {
                        Expr::Subscript(subscript) => {
                            if let Some(path) = self.path(&subscript.value) {
                                let key = Some(&*subscript.slice);
                                self.record_operator(&path, "__getitem__", &[key], None);
                                self.record_operator(&path, "__setitem__", &[key, None], None);
                                let element = path.member(&Name::new_static("__getitem__"));
                                self.record_operator(
                                    &element,
                                    dunder,
                                    &[Some(&*assign.value)],
                                    None,
                                );
                                self.accounted_for(&subscript.value);
                                self.accounted_for(&subscript.slice);
                                self.accounted_for(&assign.value);
                            }
                        }
                        Expr::Attribute(attribute) if attribute.ctx.is_store() => {
                            if let Some(path) = self.path(&attribute.value) {
                                self.record_read(&path, &attribute.attr.id, None);
                                self.record_operator(
                                    &path.member(&attribute.attr.id),
                                    dunder,
                                    &[Some(&*assign.value)],
                                    None,
                                );
                                self.accounted_for(&attribute.value);
                                self.accounted_for(&assign.value);
                            }
                        }
                        _ => {}
                    }
                }
                walk_stmt(self, stmt);
            }

            // `for x in xs` iterates `xs`, and the loop variable names an element of it.
            // `async for` asks instead for an `__aiter__` whose `__anext__` hands back something
            // awaitable, which is not a shape this can write down
            Stmt::For(for_stmt) => {
                if !for_stmt.is_async
                    && let Some(path) = self.path(&for_stmt.iter)
                {
                    let element = self.record_iteration(&path);
                    self.accounted_for(&for_stmt.iter);
                    self.bind_local(&for_stmt.target, element);
                }
                walk_stmt(self, stmt);
            }

            // `a: T = value` reads the value into a place that says what it has to be
            Stmt::AnnAssign(assign) => {
                self.visit_expr(&assign.target);
                if let Some(value) = assign.value.as_deref() {
                    let declared = (self.expression_type)(&assign.annotation);
                    self.reads_into_declared(value, declared);
                    self.visit_expr(value);
                }
            }

            Stmt::Return(ret) => {
                if let Some(value) = ret.value.as_deref() {
                    match self.declared_return {
                        // an *inferred* return type is read off the body, so it asks nothing of it
                        None => self.accounted_for(value),
                        Some(declared) => self.reads_into_declared(value, declared),
                    }
                    self.visit_expr(value);
                }
            }

            // a test is read for its truth, which every object answers
            Stmt::If(if_stmt) => {
                self.accounted_for(&if_stmt.test);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.accounted_for(test);
                    }
                }
                walk_stmt(self, stmt);
            }

            Stmt::While(while_stmt) => {
                self.accounted_for(&while_stmt.test);
                walk_stmt(self, stmt);
            }

            Stmt::Assert(assert) => {
                self.accounted_for(&assert.test);
                if let Some(message) = assert.msg.as_deref() {
                    self.accounted_for(message);
                }
                walk_stmt(self, stmt);
            }

            // everything left says nothing about the values written in it, which is what makes
            // those values ones this cannot state. `raise x` asks for a `BaseException`, `with x`
            // for a pair of context-manager methods, `del x.a` for an attribute nothing recorded,
            // `match x` for whatever its patterns take apart — each a requirement this analysis
            // has no way to write down, and each one a bound built without it would be checked
            // against
            _ => walk_stmt(self, stmt),
        }
    }

    // an interpolation is handed to `format`, which every object answers, so it asks nothing of
    // the value written there
    fn visit_interpolated_string_element(&mut self, element: &ast::InterpolatedStringElement) {
        if let ast::InterpolatedStringElement::Interpolation(interpolation) = element {
            self.accounted_for(&interpolation.expression);
        }
        ruff_python_ast::visitor::walk_interpolated_string_element(self, element);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        // a sink belongs to the expression it was set for; whatever is nested inside it is
        // read into somewhere else, or nowhere
        let sink = self.sinks.remove(&expr.range());
        match sink {
            Some(Sink::Required(required)) => {
                // a name that stands for a tracked value carries the requirement itself. a value
                // reached through a member carries it on that member instead, which the arms
                // below record
                if let Some(path) = self.name_path(expr) {
                    self.record_value(path, required);
                } else {
                    // what the place asks of something narrower than the argument is not what it
                    // asks of the argument: a value the test ruled out never reaches here, and
                    // holding the argument to this would reject the very calls the test was
                    // written for
                    self.mark_narrowed(expr);
                }
            }
            Some(Sink::Anything) => {}
            // nothing above accounted for this expression, so whatever tracked value it reads was
            // used in a way this analysis cannot state
            None => self.cannot_state(expr),
        }
        let required = match sink {
            Some(Sink::Required(required)) => Some(required),
            _ => None,
        };

        match expr {
            Expr::Lambda(lambda) => {
                record_captured_names_in_expr(&lambda.body, &mut self.captured);
                return;
            }

            // a comprehension binds its own names in a scope of its own, so a name written
            // inside one is not the local this scope bound. the iterable of its first `for`
            // clause is the exception: that one is evaluated where the comprehension is written
            Expr::ListComp(_) | Expr::SetComp(_) | Expr::DictComp(_) | Expr::Generator(_) => {
                if let Some(first) = comprehension_generators(expr).first()
                    && !first.is_async
                    && let Some(path) = self.path(&first.iter)
                {
                    self.record_iteration(&path);
                    self.accounted_for(&first.iter);
                }
                let outer = std::mem::replace(&mut self.in_nested_scope, true);
                walk_expr(self, expr);
                self.in_nested_scope = outer;
                return;
            }

            Expr::Call(call) => {
                // what the callee's own parameters ask of each argument, which is what an
                // argument that is not part of a shape recorded below is held to
                self.account_for_call_arguments(call);

                if let Expr::Attribute(method) = &*call.func
                    && let Some(path) = self.path(&method.value)
                {
                    if self.record_call(&path, &method.attr.id, call, required, MemberReach::ByName)
                    {
                        self.accounted_for(&method.value);
                        self.account_for_recorded_call_shape(call);
                    } else {
                        self.mark_unstatable(&path);
                    }
                    // the callee is this call, not a member read in its own right
                    self.visit_expr(&method.value);
                } else {
                    // calling the parameter itself is a `__call__` on it, and the call decides
                    // that member's shape the same way a method call does
                    if let Some(path) = self.path(&call.func) {
                        if self.record_call(
                            &path,
                            &Name::new_static("__call__"),
                            call,
                            required,
                            MemberReach::ThroughSyntax,
                        ) {
                            self.accounted_for(&call.func);
                            self.account_for_recorded_call_shape(call);
                        } else {
                            self.mark_unstatable(&path);
                        }
                    }
                    self.visit_expr(&call.func);
                }

                for argument in call.arguments.iter_source_order() {
                    match argument {
                        ast::ArgOrKeyword::Arg(argument) => self.visit_expr(argument),
                        ast::ArgOrKeyword::Keyword(keyword) => self.visit_expr(&keyword.value),
                    }
                }
                return;
            }

            // a member being *written* — `x.a = 1`, `del x.a` — asks for one this analysis does
            // not record, so the value it is written on is left unaccounted for
            Expr::Attribute(attribute) => {
                if attribute.ctx.is_load()
                    && let Some(path) = self.path(&attribute.value)
                {
                    self.record_read(&path, &attribute.attr.id, required);
                    self.accounted_for(&attribute.value);
                }
            }

            Expr::Subscript(subscript) => {
                if subscript.ctx.is_load() {
                    if let Some(path) = self.path(&subscript.value) {
                        self.record_operator(
                            &path,
                            "__getitem__",
                            &[Some(&*subscript.slice)],
                            required,
                        );
                        self.accounted_for(&subscript.value);
                        self.accounted_for(&subscript.slice);
                    } else if self.dunder_accepts_anything(
                        &subscript.slice,
                        &subscript.value,
                        "__getitem__",
                    ) {
                        self.accounted_for(&subscript.slice);
                    }
                }
            }

            // only the *left* operand carries the requirement. a tracked value on the right is
            // left unaccounted for on purpose: python reaches the right operand's reflected
            // dunder only when the left one returns `NotImplemented`, so which of the two routes
            // the operation takes is decided by the argument, and neither route is a requirement
            // on its own — `"%s" % attr` runs entirely through `str.__mod__`, and `str` has no
            // `__rmod__` for `attr` to be required to have
            Expr::BinOp(binary) => {
                if let Some(dunder) = runtime_dunder(binary.op) {
                    if let Some(path) = self.path(&binary.left) {
                        self.record_operator(&path, dunder, &[Some(&*binary.right)], required);
                        self.accounted_for(&binary.left);
                        self.accounted_for(&binary.right);
                    } else if self.dunder_accepts_anything(&binary.right, &binary.left, dunder) {
                        // `"%s" % attr` runs entirely through `str.__mod__`, which takes anything
                        self.accounted_for(&binary.right);
                    }
                }
            }

            Expr::UnaryOp(unary) => {
                if let Some(dunder) = unary_dunder(unary.op)
                    && let Some(path) = self.path(&unary.operand)
                {
                    self.record_operator(&path, dunder, &[], required);
                    self.accounted_for(&unary.operand);
                } else if matches!(unary.op, ast::UnaryOp::Not) {
                    // `not x` reads `x` for its truth, which every object answers
                    self.accounted_for(&unary.operand);
                }
            }

            // `a < b < c` is two comparisons on the same operands read pairwise, so an operand in
            // the middle of a chain is only accounted for when both of them account for it
            Expr::Compare(compare) => {
                let operands: Vec<&Expr> = std::iter::once(&*compare.left)
                    .chain(&compare.comparators)
                    .collect();
                let mut accounted = vec![true; operands.len()];
                for (index, op) in compare.ops.iter().enumerate() {
                    let (Some(left), Some(right)) = (operands.get(index), operands.get(index + 1))
                    else {
                        continue;
                    };
                    match op {
                        // `==`, `!=` and the identity tests are answered by `object` itself
                        ast::CmpOp::Eq | ast::CmpOp::NotEq | ast::CmpOp::Is | ast::CmpOp::IsNot => {
                        }
                        // `in` can run through `__contains__`, `__iter__` *or* `__getitem__` on
                        // the right operand, and asking for any one of the three would demand
                        // something the body never needed. that disjunction is not a shape a
                        // protocol member can state, and neither is what whichever of them runs
                        // then asks of the left operand
                        ast::CmpOp::In | ast::CmpOp::NotIn => {
                            // the container it runs through still decides what it takes, and
                            // most of them take anything
                            if !self.dunder_accepts_anything(left, right, "__contains__") {
                                accounted[index] = false;
                            }
                            accounted[index + 1] = false;
                        }
                        _ => match (comparison_dunder(*op), self.path(left)) {
                            (Some(dunder), Some(path)) => {
                                self.record_operator(&path, dunder, &[Some(right)], None);
                            }
                            (Some(dunder), None)
                                if self.dunder_accepts_anything(right, left, dunder) => {}
                            _ => {
                                accounted[index] = false;
                                accounted[index + 1] = false;
                            }
                        },
                    }
                }
                for (operand, accounted) in operands.iter().zip(accounted) {
                    if accounted {
                        self.accounted_for(operand);
                    }
                }
            }

            // `f(*xs)` hands the callee `xs`'s elements, which means iterating it
            Expr::Starred(starred) => {
                if starred.ctx.is_load()
                    && let Some(path) = self.path(&starred.value)
                {
                    self.record_iteration(&path);
                    self.accounted_for(&starred.value);
                }
            }

            Expr::BoolOp(bool_op) => {
                for value in &bool_op.values {
                    self.hands_on(value, sink);
                }
            }

            Expr::If(ternary) => {
                self.accounted_for(&ternary.test);
                self.hands_on(&ternary.body, sink);
                self.hands_on(&ternary.orelse, sink);
            }

            // `(a := x)` is `x` under another name and `x` again as the value of the expression,
            // so the name has to be one that can carry the uses below it for either to be read
            Expr::Named(named) => {
                if let Some(path) = self.path(&named.value)
                    && self.bind_local(&named.target, path)
                {
                    self.hands_on(&named.value, sink);
                }
            }

            Expr::Tuple(tuple) => self.parts_accounted_for(&tuple.elts, sink),
            Expr::List(list) => self.parts_accounted_for(&list.elts, sink),
            Expr::Set(set) => self.parts_accounted_for(&set.elts, sink),
            Expr::Dict(dict) => self.parts_accounted_for(
                dict.items
                    .iter()
                    .flat_map(|item| item.key.iter().chain(std::iter::once(&item.value))),
                sink,
            ),

            // the parts of a slice are handed to whatever the subscript resolved to, and where
            // that was a `__getitem__` this analysis recorded, its shape was written down from
            // this very slice
            Expr::Slice(slice) => self.parts_accounted_for(
                [
                    slice.lower.as_deref(),
                    slice.upper.as_deref(),
                    slice.step.as_deref(),
                ]
                .into_iter()
                .flatten(),
                sink,
            ),

            // what a generator yields becomes the yield type of the generator it returns, which
            // is read off the body rather than asked of it. a declared return type does ask, and
            // is not a shape this takes apart
            Expr::Yield(yielded) => {
                if self.declared_return.is_none()
                    && let Some(value) = yielded.value.as_deref()
                {
                    self.accounted_for(value);
                }
            }

            // everything left is a position this analysis does not read: `await x` asks for an
            // `__await__` whose iterator yields the awaited value, `yield from x` for an
            // iterable. neither is written down, so a tracked value in one of them is left
            // unaccounted for
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
    body_scope: ScopeId<'db>,
    carries_member_narrowing: bool,
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
                        // what the body established about the members of a returned place is
                        // part of what it hands back, so it travels with it
                        let returned = (self.expression_type)(value);
                        if self.carries_member_narrowing {
                            with_narrowed_members(self.db, &env, self.body_scope, value, returned)
                        } else {
                            returned
                        }
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
