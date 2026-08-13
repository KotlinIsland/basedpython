//! `ty_extensions.UnsafeUnion` — a gradual type with a finite set of materializations
//!
//! `UnsafeUnion[A, B]` fuses a union with an intersection. like `A | B`, both an `A` and
//! a `B` can be assigned *to* it. like `A & B`, a value *of* it can be used where an `A`
//! or where a `B` is wanted, and every member of either type is available on it
//!
//! that is `Any` restricted to a finite menu: the materializations of `UnsafeUnion[A, B]`
//! are `{A, B}` instead of "every type"
//!
//! - `X` is assignable to `UnsafeUnion[A, B]` iff `X` is assignable to `A | B` (the union
//!   face, in target position)
//! - `UnsafeUnion[A, B]` is assignable to `Y` iff `A` or `B` is assignable to `Y` (the
//!   intersection face, in source position)
//! - `UnsafeUnion[A, B]` is disjoint from `Y` iff both `A` and `B` are
//! - the top materialization is `A | B`; the bottom materialization is `A & B`
//!
//! unlike `Any` the menu is finite, so the type still rejects things: passing an
//! `UnsafeUnion[int, str]` where `bytes` is wanted is an error, and so is reaching for a
//! member that neither `int` nor `str` has
//!
//! ty infers this type when an overload call is ambiguous because of a gradual argument
//! (step 5 of the overload call evaluation algorithm): the surviving overloads' return
//! types are exactly the menu of possible results

use crate::Db;
use crate::place::{
    DefinedPlace, Definedness, Place, PlaceAndQualifiers, Provenance, PublicTypePolicy, TypeOrigin,
};
use crate::types::ProgramEnvironment;
use crate::types::set_theoretic::UnionType;
use crate::types::variance::VarianceInferable;
use crate::types::{
    BoundTypeVarIdentity, InstanceProjection, Type, TypeQualifiers, TypeVarVariance, visitor,
};

#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct UnsafeUnionType<'db> {
    /// The possible materializations of this type. Always at least two elements: an
    /// `UnsafeUnion` of one type is that type, and of no types is `Never`.
    #[returns(deref)]
    pub elements: Box<[Type<'db>]>,
}

pub(super) fn walk_unsafe_union<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    unsafe_union: UnsafeUnionType<'db>,
    visitor: &V,
) {
    for element in unsafe_union.elements(db) {
        visitor.visit_type(db, *element);
    }
}

// the salsa heap is tracked separately
impl get_size2::GetSize for UnsafeUnionType<'_> {}

impl<'db> UnsafeUnionType<'db> {
    /// Build the unsafe union of `elements`, simplifying it into a different variant of
    /// [`Type`] where the menu of materializations collapses.
    pub(crate) fn from_elements<I, T>(db: &'db dyn Db, elements: I) -> Type<'db>
    where
        I: IntoIterator<Item = T>,
        T: Into<Type<'db>>,
    {
        let mut collected: Vec<Type<'db>> = Vec::new();

        for element in elements {
            match element.into() {
                // a dynamic element admits every materialization, which swallows the whole
                // menu: `UnsafeUnion[int, Any]` can materialize to anything, so it *is* `Any`
                dynamic @ Type::Dynamic(_) => return dynamic,
                Type::UnsafeUnion(nested) => {
                    for nested_element in nested.elements(db) {
                        if !collected.contains(nested_element) {
                            collected.push(*nested_element);
                        }
                    }
                }
                // `Never` is uninhabited, so it contributes no runtime values to choose
                // from. keeping it would also make the whole type assignable to everything,
                // since `Never` is assignable to every type
                Type::Never => {}
                element => {
                    if !collected.contains(&element) {
                        collected.push(element);
                    }
                }
            }
        }

        match collected.as_slice() {
            [] => Type::Never,
            [single] => *single,
            _ => Type::UnsafeUnion(Self::new(db, collected.into_boxed_slice())),
        }
    }

    /// The top materialization: the union of every type this could materialize to.
    ///
    /// This is the type an `UnsafeUnion` is narrowed to when it is used *safely*, and the
    /// face it presents to operations that must hold for every possible materialization.
    pub(crate) fn to_union(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        UnionType::from_elements(db, env, self.elements(db).iter().copied())
    }

    /// Apply `transform` to every element, rebuilding (and possibly collapsing) the menu.
    pub(crate) fn map_elements(
        self,
        db: &'db dyn Db,
        transform: impl FnMut(Type<'db>) -> Type<'db>,
    ) -> Type<'db> {
        Self::from_elements(db, self.elements(db).iter().copied().map(transform))
    }

    /// A fallible version of [`UnsafeUnionType::map_elements`].
    pub(crate) fn try_map_elements(
        self,
        db: &'db dyn Db,
        transform: impl FnMut(Type<'db>) -> Option<Type<'db>>,
    ) -> Option<Type<'db>> {
        let elements: Option<Vec<_>> = self.elements(db).iter().copied().map(transform).collect();
        Some(Self::from_elements(db, elements?))
    }

    /// Project every element from a class-object type into its instance type.
    pub(crate) fn to_instance(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<InstanceProjection<Type<'db>>> {
        let mut is_exact = true;
        let instance = self.try_map_elements(db, |element| {
            let projection = element.to_instance(db, env)?;
            is_exact &= projection.is_exact();
            Some(projection.into_inner())
        })?;
        Some(InstanceProjection::new(instance, is_exact))
    }

    /// Look a member up on every element, keeping the ones that have it.
    ///
    /// This is the intersection face: the member is available as long as *some*
    /// materialization has it, and is undefined only when none does. The result is itself
    /// an unsafe union, so the imprecision keeps propagating rather than silently
    /// collapsing into a safe union.
    pub(crate) fn map_with_boundness_and_qualifiers(
        self,
        db: &'db dyn Db,
        mut transform_fn: impl FnMut(&Type<'db>) -> PlaceAndQualifiers<'db>,
    ) -> PlaceAndQualifiers<'db> {
        let mut member_types = Vec::new();
        let mut qualifiers = TypeQualifiers::empty();

        let mut any_definitely_bound = false;
        let mut origin = TypeOrigin::Declared;
        let mut provenance = Provenance::Unknown;

        for element in self.elements(db) {
            let PlaceAndQualifiers {
                place: member,
                qualifiers: member_qualifiers,
            } = transform_fn(element);
            qualifiers |= member_qualifiers;
            match member {
                Place::Undefined => {}
                Place::Defined(DefinedPlace {
                    ty: member_ty,
                    origin: member_origin,
                    definedness: member_definedness,
                    provenance: member_provenance,
                    ..
                }) => {
                    origin = origin.merge(member_origin);
                    if member_definedness == Definedness::AlwaysDefined {
                        any_definitely_bound = true;
                    }
                    provenance = provenance.or(member_provenance);
                    member_types.push(member_ty);
                }
            }
        }

        PlaceAndQualifiers {
            place: if member_types.is_empty() {
                Place::Undefined
            } else {
                Place::Defined(DefinedPlace {
                    ty: Self::from_elements(db, member_types),
                    origin,
                    definedness: if any_definitely_bound {
                        Definedness::AlwaysDefined
                    } else {
                        Definedness::PossiblyUndefined
                    },
                    public_type_policy: PublicTypePolicy::Raw,
                    provenance,
                })
            },
            qualifiers,
        }
    }
}

impl<'db> VarianceInferable<'db> for UnsafeUnionType<'db> {
    /// An `UnsafeUnion` is invariant in its elements: each one is reachable in both
    /// directions (a value can be assigned *to* the type through it, and read *out* of the
    /// type as it), so neither polarity alone describes it.
    fn variance_of(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar: BoundTypeVarIdentity<'db>,
    ) -> TypeVarVariance {
        self.elements(db)
            .iter()
            .map(|element| {
                TypeVarVariance::Invariant.compose(element.variance_of(db, env, typevar))
            })
            .collect()
    }
}
