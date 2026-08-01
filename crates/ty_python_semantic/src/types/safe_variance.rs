//! basedpython safe variance: a private member does not specialize.
//!
//! Privacy is what makes variance safe. A private member is invisible outside its class, so it
//! cannot be used to tell two specializations apart — which is what lets a covariant class hold a
//! mutable field of its type parameter. The flip side is that a widened view of the class learns
//! nothing about that member: through any receiver but the class's own, the member is erased to
//! what such a view actually knows.
//!
//! The class's own receiver is the implicit `self`, whatever it is annotated as. `self` is bound
//! to the receiver the call site had, so the class's type parameters are that receiver's — a
//! widening annotation on it does not make the body another view of the class.

use super::{
    BoundTypeVarInstance, ClassLiteral, MemberLookupPolicy, Type, TypeVarVariance, any_over_type,
    is_private_member,
};
use crate::Db;

/// basedpython safe variance: a private member seen through a view of its class that is
/// not the class's own.
///
/// The `attribute` is private, its declared type names one or more of the class's
/// *non-invariant* type parameters, and the receiver substitutes something else for them.
pub(super) struct PrivateMemberView<'db> {
    /// The class's own view of itself — the receiver the member is declared against.
    pub(super) own_view: Type<'db>,
    /// The member's declared type on that view, with the type parameters left intact.
    pub(super) declared_ty: Type<'db>,
    /// The non-invariant type parameters the receiver substituted something else for.
    substituted: Box<[BoundTypeVarInstance<'db>]>,
}

impl<'db> PrivateMemberView<'db> {
    /// The declared type with every substituted parameter replaced by the unknown it really is.
    ///
    /// The receiver's own type argument says nothing about what the object holds — that is what
    /// its being widened means.
    fn erased(&self, db: &'db dyn Db) -> Type<'db> {
        self.substituted
            .iter()
            .fold(self.declared_ty, |ty, typevar| {
                ty.substitute_one_typevar(db, *typevar, Type::any())
            })
    }

    /// The type a *read* through this view yields.
    ///
    /// A read observes the member at its most general, so the erasure's top materialization is
    /// everything such a view can know: `t: T` reads as `object`. The value can be treated as its
    /// bound, but it is no longer a `T`, so it can never be funnelled back into the `T`-typed
    /// storage it came from.
    pub(super) fn read_type(&self, db: &'db dyn Db) -> Type<'db> {
        self.erased(db).top_materialization(db)
    }

    /// The type a *write* through this view has to supply.
    ///
    /// Storage is invariant in its own type, so a write has to be valid for every type the member
    /// could really have — the erasure's bottom materialization. For a plain `T` that is `Never`:
    /// a view that knows nothing about a member cannot write to it, whatever it holds.
    fn write_type(&self, db: &'db dyn Db) -> Type<'db> {
        self.erased(db).bottom_materialization(db)
    }
}

/// basedpython safe variance: the type a write to `attribute` has to supply, when `object_ty` is a
/// widened view of a class that declares `attribute` privately.
///
/// `None` leaves the write to ordinary specialization.
pub(super) fn private_member_write_type<'db>(
    db: &'db dyn Db,
    object_ty: Type<'db>,
    attribute: &str,
) -> Option<Type<'db>> {
    Some(private_member_view(db, object_ty, attribute)?.write_type(db))
}

/// basedpython safe variance: a private member does not specialize.
///
/// Through any receiver but the class's own, the member keeps its declared type rather than
/// picking up the receiver's arguments, and that type is erased to what a widened view knows: a
/// read yields the parameter's bound, a write accepts nothing.
///
/// A type parameter that is *invariant* is left alone. No specialization of the class is
/// assignable to another, so every receiver's argument is exact and ordinary specialization
/// is already sound.
pub(super) fn private_member_view<'db>(
    db: &'db dyn Db,
    object_ty: Type<'db>,
    attribute: &str,
) -> Option<PrivateMemberView<'db>> {
    let instance = object_ty.as_nominal_instance()?;
    let super::ClassType::Generic(alias) = instance.class(db) else {
        return None;
    };
    let specialization = alias.specialization(db);
    let class = ClassLiteral::Static(alias.origin(db));

    // the substituted type parameters, cheapest first: a receiver that carries the class's
    // own parameters *is* the class's own view of every member, so there is nothing to do.
    // this is the whole of the hot path — `self.x` inside the class's own methods — and it
    // settles without a member lookup
    let generic_context = specialization.generic_context(db);
    let substituted = || {
        generic_context
            .variables(db)
            .zip(specialization.types(db))
            .filter(|(typevar, argument)| **argument != Type::TypeVar(*typevar))
            .map(|(typevar, _)| typevar)
    };
    substituted().next()?;

    // look the member up on the class's own identity specialization, so its declared type
    // still names the class's type parameters rather than the receiver's arguments. a
    // `__getattr__` result is not a declared member of anything, so it is never private
    // however its name is spelled
    let own_view = Type::instance(db, class.identity_specialization(db));
    let member =
        own_view.member_lookup_with_policy(db, attribute, MemberLookupPolicy::NO_GETATTR_LOOKUP);
    let declared_ty = member.place.ignore_possibly_undefined()?;
    if !is_private_member(db, attribute, member.qualifiers, declared_ty) {
        return None;
    }

    let substituted: Box<[_]> = substituted()
        .filter(|typevar| {
            let identity = typevar.identity(db);
            let mentions_typevar = any_over_type(
                db,
                declared_ty,
                false,
                |ty| matches!(ty, Type::TypeVar(other) if other.identity(db) == identity),
            );
            // an invariant parameter is checked last: inferring variance is expensive, and
            // it re-enters the class body this access may itself be inside of
            mentions_typevar && typevar.variance(db) != TypeVarVariance::Invariant
        })
        .collect();
    if substituted.is_empty() {
        return None;
    }

    Some(PrivateMemberView {
        own_view,
        declared_ty,
        substituted,
    })
}
