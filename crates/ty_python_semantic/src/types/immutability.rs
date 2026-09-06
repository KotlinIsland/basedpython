//! basedpython-ui: deep immutability of a type — the checker's half of the
//! framework's *stability* notion (`docs/design.md` §4.1)
//!
//! A value is *deeply immutable* when nothing reachable from it can change
//! after it is created. The framework needs exactly that property from a value
//! held in state: a `State[T]` notifies its readers when it is *assigned*, so a
//! change made *inside* the value — `items.append(1)` on a held `list` — is one
//! no reader can observe. The same property is what lets a composable be
//! skipped: two stable arguments that compare equal describe the same ui.
//!
//! The predicate answers `true` for:
//!
//! - the scalars `int`, `float`, `bool`, `str`, `bytes`, `None`, `complex`,
//!   `range` (and every literal of them)
//! - enum members, and instances of an enum class — a basedpython `enum class`
//!   included: its unit variants are members, its payload variants frozen
//!   dataclasses, checked field by field
//! - a `tuple` or `frozenset` whose elements are deeply immutable
//! - a frozen dataclass (a basedpython `frozen data class` included) or a
//!   `NamedTuple` whose fields are deeply immutable
//! - type objects, callables, and the framework's observables (`State`,
//!   `StateList`, `StateDict`, `Derived`, `Ambient`): identity-stable handles
//!   whose mutations notify
//! - a union when every member is, an intersection when any positive member is
//! - a type variable when its bound (or every constraint) is. A type variable
//!   with no bound stands for whatever the caller passes, which is checked
//!   where the call solves it — so the generic body itself is not blamed
//! - the gradual types: nothing is known, so nothing is reported
//!
//! Everything else is mutable: `list`, `dict`, `set`, `bytearray`, `deque`, a
//! non-frozen class, a protocol, `object`. The runtime mirrors this predicate
//! in `basedpython_ui.runtime.is_stable_type`, as the defence for `.py` callers
//! and `dynamic` values the checker cannot see.

use ruff_python_ast::helpers::UseSiteVariance;

use crate::types::class::{ClassLiteral, CodeGeneratorKind};
use crate::types::dedicated::basedpython_ui::{is_observable_instance, underlying};
use crate::types::enums::is_enum_class;
use crate::types::instance::NominalInstanceType;
use crate::types::{KnownClass, ProgramEnvironment, Type, TypeVarBoundOrConstraints};
use crate::{Db, Program};

/// whether no value of `ty` can change after it is created — see the module
/// documentation for exactly what counts
pub(crate) fn is_deeply_immutable<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    is_deeply_immutable_impl(db, ty, env.program(db))
}

// a frozen dataclass may hold a field of its own type; the recursion bottoms
// out on the identity of "all fields immutable"
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _, _| true,
    heap_size = ruff_memory_usage::heap_size
)]
fn is_deeply_immutable_impl<'db>(db: &'db dyn Db, ty: Type<'db>, program: Program<'db>) -> bool {
    let env = &ProgramEnvironment::from_program(program);
    let immutable = |ty: Type<'db>| is_deeply_immutable(db, env, ty);
    match ty {
        // nothing is known about a gradual type, so nothing is reported
        Type::Dynamic(_) | Type::Divergent(_) | Type::Never => true,

        // a callable is stable by identity
        Type::FunctionLiteral(_)
        | Type::BoundMethod(_)
        | Type::KnownBoundMethod(_)
        | Type::WrapperDescriptor(_)
        | Type::DataclassDecorator(_)
        | Type::DataclassTransformer(_)
        | Type::Callable(_) => true,

        // a type object, and the objects the typing machinery builds. a slot
        // descriptor belongs here with a property: both are class-level
        // descriptor objects that never change, and neither holds the instance
        // whose attribute they mediate
        Type::ClassLiteral(_)
        | Type::GenericAlias(_)
        | Type::SubclassOf(_)
        | Type::TypeForm(_)
        | Type::SpecialForm(_)
        | Type::KnownInstance(_)
        | Type::PropertyInstance(_)
        | Type::SlotDescriptor(_) => true,

        // literals, enum members, and the `bool` narrowing types
        Type::LiteralValue(_) | Type::EnumComplement(_) | Type::TypeIs(_) | Type::TypeGuard(_) => {
            true
        }

        // a module's attributes are writable; a `super()` proxy, a truthiness
        // set and a protocol say nothing about the object behind them; a
        // `TypedDict` is a `dict`
        Type::ModuleLiteral(_)
        | Type::BoundSuper(_)
        | Type::AlwaysTruthy
        | Type::AlwaysFalsy
        | Type::ProtocolInstance(_)
        | Type::TypedDict(_) => false,

        Type::Union(union) => union.elements(db).iter().copied().all(immutable),
        Type::UnsafeUnion(union) => union.elements(db).iter().copied().all(immutable),
        Type::Intersection(intersection) => {
            intersection.positive(db).iter().copied().any(immutable)
        }

        Type::TypeVar(bound_typevar) => {
            match bound_typevar.typevar(db).bound_or_constraints(db, env) {
                None => true,
                Some(TypeVarBoundOrConstraints::UpperBound(bound)) => immutable(bound),
                Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                    constraints.elements(db).iter().copied().all(immutable)
                }
            }
        }

        Type::Overlapping(overlapping) => immutable(overlapping.value_type(db, env)),
        Type::Restricted(restricted) => immutable(restricted.value_type(db)),
        Type::Deferred(deferred) => immutable(deferred.reduced(db, env)),
        Type::TypeAlias(alias) => immutable(alias.value_type(db)),
        Type::NewTypeInstance(newtype) => immutable(newtype.concrete_base_type(db)),

        Type::NominalInstance(instance) => instance_is_deeply_immutable(db, env, instance),
    }
}

/// the nominal-instance half of [`is_deeply_immutable`]: the class decides,
/// and a container or record is only as immutable as what it holds
fn instance_is_deeply_immutable<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    instance: NominalInstanceType<'db>,
) -> bool {
    let immutable = |ty: Type<'db>| is_deeply_immutable(db, env, ty);

    if let Some(tuple) = instance.tuple_spec(db, env) {
        return tuple.iter_element_types(db).all(immutable);
    }

    let class = instance.class(db, env);
    let Some((literal, specialization)) = class.static_class_literal(db) else {
        return false;
    };

    match literal.known(db) {
        Some(
            KnownClass::Int
            | KnownClass::Float
            | KnownClass::Bool
            | KnownClass::Str
            | KnownClass::Bytes
            | KnownClass::NoneType
            | KnownClass::Complex
            | KnownClass::Range
            | KnownClass::Type
            | KnownClass::EllipsisType
            | KnownClass::NotImplementedType,
        ) => return true,
        Some(KnownClass::FrozenSet) => {
            return specialization.is_none_or(|specialization| {
                specialization.types(db).iter().copied().all(immutable)
            });
        }
        _ => {}
    }

    if is_observable_instance(db, env, Type::NominalInstance(instance)) {
        return true;
    }

    if is_enum_class(db, Type::ClassLiteral(ClassLiteral::Static(literal))) {
        return true;
    }

    // a record is immutable when it cannot be written and holds only immutable
    // fields. a based enum's payload variant is a frozen dataclass; its unit
    // variants have no storage at all
    let Some(field_policy) = CodeGeneratorKind::from_class(db, ClassLiteral::Static(literal))
    else {
        return literal.is_enum_variant(db);
    };
    let frozen = match field_policy {
        CodeGeneratorKind::NamedTuple => true,
        CodeGeneratorKind::DataclassLike(_) | CodeGeneratorKind::Pydantic(_) => {
            literal.is_frozen_dataclass(db) == Some(true)
        }
        _ => false,
    };
    frozen
        && literal
            .fields(db, specialization, field_policy)
            .values()
            .all(|field| immutable(field.declared_ty))
}

/// whether `ty` is an instance of one of the builtin mutable containers —
/// `list`, `dict`, `set`, `bytearray`, `deque`, `defaultdict` — or of a subclass
/// of one, whose mutating methods change it in place without anything
/// observing the change
pub(crate) fn is_builtin_mutable_container<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    let Type::NominalInstance(instance) = underlying(db, ty) else {
        return false;
    };
    let Some((literal, specialization)) = instance.class(db, env).static_class_literal(db) else {
        return false;
    };
    literal
        .iter_mro(db, specialization)
        .filter_map(crate::types::ClassBase::into_class)
        .any(|base| {
            matches!(
                base.known(db),
                Some(
                    KnownClass::List
                        | KnownClass::Dict
                        | KnownClass::Set
                        | KnownClass::Bytearray
                        | KnownClass::Deque
                        | KnownClass::DefaultDict
                )
            )
        })
}

/// whether `ty` is a builtin mutable container seen through a use-site `out`
/// projection (`list[out int]`): a read-only view, through which the checker
/// already rejects every write
pub(crate) fn is_write_projected<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    let Type::NominalInstance(instance) = underlying(db, ty) else {
        return false;
    };
    let Some((_, Some(specialization))) = instance.class(db, env).static_class_literal(db) else {
        return false;
    };
    specialization
        .projections(db)
        .contains(&Some(UseSiteVariance::Out))
}

/// whether `ty` is a builtin mutable container whose every type argument is
/// projected `out` and deeply immutable — `list[out int]`, `dict[out str, out
/// int]`: a read-only view of immutable elements, the recommended spelling
/// for a composable that must accept a container it will never mutate
fn is_read_only_view<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    if !is_builtin_mutable_container(db, env, ty) {
        return false;
    }
    let Type::NominalInstance(instance) = underlying(db, ty) else {
        return false;
    };
    let Some((_, Some(specialization))) = instance.class(db, env).static_class_literal(db) else {
        return false;
    };
    let projections = specialization.projections(db);
    !projections.is_empty()
        && projections
            .iter()
            .all(|projection| *projection == Some(UseSiteVariance::Out))
        && specialization
            .types(db)
            .iter()
            .all(|argument| is_deeply_immutable(db, env, *argument))
}

/// whether a composable parameter of type `ty` is *stable*: deeply immutable,
/// or a read-only view of immutable elements (`list[out int]`), which the
/// composable cannot mutate and so may accept in place of a `tuple`. A union
/// is stable when every member is.
///
/// Stability is the *skipping* question, and a read-only view answers it: at
/// recomposition the runtime compares the argument structurally, and at that
/// moment the comparison is correct — a `list[out int]` that compares equal
/// to the last one describes the same ui. *Observability* is a different
/// question, asked of what a composition reads: a view restricts only this
/// reader, not the other holders of the list, so a write made through one of
/// them between two compositions notifies nobody. That is why a read of such
/// a parameter while composing is still an `unobservable-dependency`, decided
/// by [`is_deeply_immutable`] alone
pub(crate) fn is_stable_parameter_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    // a use-site restriction or alias around the type says nothing about
    // stability: `final list[out int]` is the view inside it
    match underlying(db, ty) {
        Type::Union(union) => union
            .elements(db)
            .iter()
            .all(|element| is_stable_parameter_type(db, env, *element)),
        ty => is_deeply_immutable(db, env, ty) || is_read_only_view(db, env, ty),
    }
}
