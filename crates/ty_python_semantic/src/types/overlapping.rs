//! `ty_extensions.Overlapping` — a safe-variance escape hatch for input positions
//!
//! `Overlapping[Key]` is a parameter-only special form. it lets a covariant
//! (`out Key`) class declare a method that *consumes* `Key` without giving up
//! covariance, by pulling in two opposite directions:
//!
//! - at the call site it accepts an argument iff that argument is *not disjoint*
//!   from the specialized `Key`. so for a `Mapping[int, ...]`, `1 in m` and
//!   `object() in m` are accepted (both could-be an `int`), while `"a" in m` is
//!   rejected (`str` and `int` never overlap). this is looser than a plain `Key`
//!   parameter (which would reject `object()`) but stricter than `object` (which
//!   would accept `"a"`)
//! - inside the body the parameter is seen as `Key`'s upper bound, so the
//!   consumed value can never be funnelled back into `Key`-typed covariant
//!   storage. that erasure is the soundness guard
//!
//! `Overlapping` is the loose sibling of the (documented, not-yet-built)
//! `SafeVariance`: they share the two-faced structure and differ only in the
//! call-site relation (overlap vs. subtype)

use super::variance::{VarianceInferable, VarianceTerm};
use super::{BoundTypeVarIdentity, Type, TypeVarVariance, visitor};
use crate::Db;
use crate::types::ProgramEnvironment;

#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct OverlappingType<'db> {
    #[returns(copy)]
    pub(crate) type_argument: Type<'db>,
}

pub(super) fn walk_overlapping_type<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    overlapping_type: OverlappingType<'db>,
    visitor: &V,
) {
    visitor.visit_type(db, overlapping_type.type_argument(db));
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for OverlappingType<'_> {}

impl<'db> OverlappingType<'db> {
    pub(crate) fn from_type_expression(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
        Type::Overlapping(Self::new(db, ty))
    }

    /// The type a value of an `Overlapping[Key]` parameter is seen as *inside the
    /// method body*: the upper bound of the wrapped type argument. A covariant
    /// `Key` is thereby erased to its bound (`object` when unbounded), so it can
    /// never be written back into `Key`-typed storage.
    pub(crate) fn value_type(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self.type_argument(db) {
            Type::TypeVar(typevar) => typevar
                .typevar(db)
                .bound_or_constraints(db, env)
                .map(|bound_or_constraints| bound_or_constraints.as_type(db, env))
                .unwrap_or_else(Type::object),
            other => other,
        }
    }
}

impl<'db> Type<'db> {
    /// Erase a top-level `Overlapping[Key]` marker to the type a body sees for
    /// such a parameter (`Key`'s upper bound). Any other type is returned
    /// unchanged. Used when binding a parameter's declared type inside the body,
    /// where the marker has no place — it exists only for the call binder.
    pub(crate) fn erase_overlapping(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Type<'db> {
        match self {
            Type::Overlapping(overlapping) => overlapping.value_type(db, env),
            _ => self,
        }
    }
}

impl<'db> VarianceInferable<'db> for OverlappingType<'db> {
    // `Overlapping` is bivariant in its type argument: it constrains variance in
    // neither direction, matching the escape-hatch semantics (its whole purpose
    // is to let a covariant typevar appear in an input position without forcing
    // invariance).
    fn variance_of(
        self,
        _db: &'db dyn Db,
        _env: &ProgramEnvironment<'db>,
        _typevar: BoundTypeVarIdentity<'db>,
    ) -> VarianceTerm<'db> {
        VarianceTerm::BIVARIANT
    }
}
