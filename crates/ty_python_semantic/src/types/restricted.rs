//! basedpython use-site type modifiers — `literal T` and `final T`
//!
//! Both are written as a keyword in front of a type expression and narrow the
//! set of values the annotated place accepts, without changing what the value
//! *is*. They are a use-site counterpart to the declaration-site modifiers:
//! `@final` on a class says "nobody may subclass me", `final T` at a use site
//! says "only exactly a `T` fits here", and neither has a runtime artefact.
//!
//! - `literal T` accepts a value whose type is a [literal type](Type::is_literal_type)
//!   assignable to `T`. `literal str` is exactly `LiteralString`, and the
//!   constructor reduces it to that type.
//! - `final T` accepts a value whose runtime class is exactly `T`'s, so a
//!   proper subtype is rejected: `b: final int = True` is an error because
//!   `bool` is a *sub*class of `int`.
//!
//! The restriction applies in *target* position only. In source position a
//! restricted type behaves as the type it wraps — a `final A` value is an `A`
//! and offers `A`'s members — which is what makes the whole rest of the type
//! system able to ignore this variant and delegate to [`RestrictedType::value_type`].

use ruff_python_ast::helpers::TypeModifier;

use super::class::ClassType;
use super::variance::{VarianceInferable, VarianceTerm};
use super::{BoundTypeVarIdentity, KnownClass, Type, visitor};
use crate::Db;
use crate::types::ProgramEnvironment;

#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct RestrictedType<'db> {
    /// which keyword wrote this restriction
    #[returns(copy)]
    pub(crate) modifier: TypeModifier,
    /// the type being restricted — `str` in `literal str`
    #[returns(copy)]
    pub(crate) type_argument: Type<'db>,
}

pub(super) fn walk_restricted_type<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    restricted: RestrictedType<'db>,
    visitor: &V,
) {
    visitor.visit_type(db, restricted.type_argument(db));
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for RestrictedType<'_> {}

impl<'db> RestrictedType<'db> {
    /// Build the type `<modifier> <ty>` denotes, reducing it to an ordinary type
    /// whenever the restriction adds nothing:
    ///
    /// - `literal str` *is* `LiteralString`, the stdlib spelling of the same set
    /// - a restriction every inhabitant of `ty` already satisfies (`literal
    ///   Literal[1]`, `final` on a `@final` class, anything on `Never` or a
    ///   dynamic type) leaves `ty` alone
    /// - stacking the same modifier twice is idempotent
    pub(crate) fn from_type_expression(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        modifier: TypeModifier,
        ty: Type<'db>,
    ) -> Type<'db> {
        // `literal str` and `LiteralString` denote the same set of values, so
        // there is no reason to carry a second spelling of it around
        if modifier == TypeModifier::Literal && ty == KnownClass::Str.to_instance(db, env) {
            return Type::literal_string();
        }
        if restriction_holds(db, env, modifier, ty) {
            return ty;
        }
        Type::Restricted(Self::new(db, modifier, ty))
    }

    /// The type a value of this restricted type has once the restriction has
    /// done its work at the assignment or call site: the type it wraps. Member
    /// lookup, iteration, truthiness and every other value-level question
    /// delegate here, so a `final A` behaves as an `A` in a body.
    pub(crate) fn value_type(self, db: &'db dyn Db) -> Type<'db> {
        self.type_argument(db)
    }
}

impl<'db> Type<'db> {
    /// Erase a top-level use-site modifier, leaving the type it wraps. Any other
    /// type is returned unchanged.
    #[must_use]
    pub fn erase_restriction(self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Type::Restricted(restricted) => restricted.value_type(db),
            _ => self,
        }
    }

    /// basedpython: whether this type is a *literal type* — one whose values can
    /// only be written down literally in source.
    ///
    /// The literal value types (`Literal[1]`, `Literal["a"]`, `LiteralString`,
    /// enum members, …) are the base case. `Never` is vacuously literal: it has
    /// no inhabitant to write. A specialized generic is literal when every type
    /// argument is, which is what makes `list[Never]` — the type of `[]`, whose
    /// only inhabitant is the empty list display — literal while `list[int]` is
    /// not. `None` and `...` are literal because that is how they are spelled.
    ///
    /// A dynamic type is literal, matching the way gradual types are admissible
    /// against every other restriction in the type system.
    fn is_literal_type(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> bool {
        match self {
            Type::LiteralValue(_) => true,
            Type::Dynamic(_) | Type::Divergent(_) | Type::Never => true,

            Type::Restricted(restricted) => {
                restricted.modifier(db) == TypeModifier::Literal
                    || restricted.value_type(db).is_literal_type(db, env)
            }

            Type::TypeAlias(alias) => alias.value_type(db).is_literal_type(db, env),
            Type::Deferred(deferred) => deferred.reduced(db, env).is_literal_type(db, env),

            // a union is literal when every member is; an intersection when any
            // positive member is (its values are drawn from that member)
            Type::Union(union) => union
                .elements(db)
                .iter()
                .all(|element| element.is_literal_type(db, env)),
            Type::Intersection(intersection) => intersection
                .positive(db)
                .iter()
                .any(|element| element.is_literal_type(db, env)),

            Type::TypeVar(bound_typevar) => bound_typevar
                .typevar(db)
                .bound_or_constraints(db, env)
                .is_some_and(|bound| bound.as_type(db, env).is_literal_type(db, env)),

            Type::NominalInstance(nominal) => {
                // `None` and `...` are singletons written as literals
                self.is_singleton(db, env)
                    || class_arguments_are_literal(db, env, nominal.class(db, env))
            }

            _ => false,
        }
    }
}

/// Whether every type argument of `class` is literal, and there is at least one.
/// A bare class has no arguments to make literal, so it is not literal itself —
/// only its literal value types are.
fn class_arguments_are_literal<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: ClassType<'db>,
) -> bool {
    let (_, Some(specialization)) = class.class_literal_and_specialization(db) else {
        return false;
    };
    let arguments = specialization.types(db);
    !arguments.is_empty()
        && arguments
            .iter()
            .all(|argument| argument.is_literal_type(db, env))
}

/// Whether a value of type `source` is admissible where `<modifier> <inner>` is
/// expected. This is the whole meaning of a use-site modifier; the enclosing
/// relation separately checks that `source` is assignable to `inner`.
pub(crate) fn restriction_admits<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    modifier: TypeModifier,
    inner: Type<'db>,
    source: Type<'db>,
) -> bool {
    // a gradual or empty source is admissible against every restriction, the
    // same way it is assignable to every type. an unannotated parameter's hole
    // that nothing bounded is one of those wearing a name
    if matches!(source, Type::Dynamic(_) | Type::Divergent(_) | Type::Never)
        || crate::types::inferred_signature::gradual_hole(db, env, source).is_some()
    {
        return true;
    }

    match source {
        Type::TypeAlias(alias) => {
            return restriction_admits(db, env, modifier, inner, alias.value_type(db));
        }
        Type::Deferred(deferred) => {
            return restriction_admits(db, env, modifier, inner, deferred.reduced(db, env));
        }
        // every member of a union has to fit, since the value may be any of them
        Type::Union(union) => {
            return union
                .elements(db)
                .iter()
                .all(|element| restriction_admits(db, env, modifier, inner, *element));
        }
        // an intersection's values are drawn from every positive member, so one
        // admissible member is enough
        Type::Intersection(intersection) => {
            let positive = intersection.positive(db);
            return !positive.is_empty()
                && positive
                    .iter()
                    .any(|element| restriction_admits(db, env, modifier, inner, *element));
        }
        _ => {}
    }

    match modifier {
        TypeModifier::Literal => source.is_literal_type(db, env),
        TypeModifier::Final => {
            // "the runtime class is exactly `inner`'s": promote a literal to the
            // class it is an instance of (`Literal[1]` → `int`, `True` → `bool`)
            // and require the result to be that very class. `True` therefore
            // fails against `final int`, which is the point of the modifier.
            //
            // only the class is asked here. whether the type *arguments* fit is
            // the enclosing relation's question, and it answers it gradually —
            // asking it a second time as an equivalence would reject
            // `list[Unknown]` against `final list[int]`, which every other
            // relation in the system admits
            let promoted = source
                .erase_restriction(db)
                .literal_fallback_instance(db, env)
                .unwrap_or_else(|| source.erase_restriction(db));
            let inner = inner.erase_restriction(db);
            match (promoted, inner) {
                (Type::NominalInstance(source), Type::NominalInstance(inner)) => {
                    source.class(db, env).class_literal(db)
                        == inner.class(db, env).class_literal(db)
                }
                // a type with no class behind it — a callable, a protocol —
                // degenerates to plain type equality
                _ => promoted.is_equivalent_to(db, env, inner),
            }
        }
    }
}

/// Whether every inhabitant of `ty` already satisfies `modifier`, making the
/// restriction a no-op.
///
/// This is deliberately stricter than [`restriction_admits`]: that relation lets
/// a gradual type through, as every relation in the type system does, but a
/// gradual type is not *known* to satisfy the restriction, so dropping the
/// modifier from `literal list[*]` would silently discard the check for every
/// later assignment.
fn restriction_holds<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    modifier: TypeModifier,
    ty: Type<'db>,
) -> bool {
    match ty {
        Type::Never => return true,
        Type::Dynamic(_) | Type::Divergent(_) => return false,
        _ => {}
    }
    match modifier {
        TypeModifier::Literal => matches!(ty, Type::LiteralValue(_)),
        // a `@final` class has no subclasses, so every one of its instances is
        // already exactly it
        TypeModifier::Final => matches!(ty, Type::NominalInstance(nominal)
            if nominal.class(db, env).class_literal(db).is_final(db)),
    }
}

impl<'db> VarianceInferable<'db> for RestrictedType<'db> {
    // a restriction narrows the set of values without reordering it, so it
    // inherits the variance of the type it wraps
    fn variance_of(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar: BoundTypeVarIdentity<'db>,
    ) -> VarianceTerm<'db> {
        self.type_argument(db).variance_of(db, env, typevar)
    }
}
