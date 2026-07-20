//! dedicated sqlalchemy support — 2.0 declarative model detection and the
//! descriptor-annotated field extraction that constructor synthesis needs
//!
//! sqlalchemy 2.0 ships inline types built on the descriptor protocol, so
//! attribute access (`user.id` → `int`, `User.id` →
//! `InstrumentedAttribute[int]`) and query typing already resolve generally.
//! what needs dedicated help is the constructor: a plain declarative model
//! inherits `__init__(self, **kw: Any)`, so every keyword is unchecked. we
//! re-derive the truthful signature from the `Mapped[T]` annotations. see
//! `docs/basedpython/frameworks/sqlalchemy.md`

use crate::Db;
use crate::types::{ClassBase, KnownClass, StaticClassLiteral, Type};

/// `class` is a sqlalchemy 2.0 declarative model: `DeclarativeBase` in its
/// mro, and not a `MappedAsDataclass` model. the dataclass path (pep 681
/// `dataclass_transform`) already handles `MappedAsDataclass` and must keep
/// winning, so it is excluded here
pub(in crate::types) fn is_declarative(db: &dyn Db, class: StaticClassLiteral<'_>) -> bool {
    let mut has_declarative_base = false;
    for base in class.iter_mro(db, None).filter_map(ClassBase::into_class) {
        if base.is_known(db, KnownClass::SqlalchemyMappedAsDataclass) {
            return false;
        }
        if base.is_known(db, KnownClass::SqlalchemyDeclarativeBase) {
            has_declarative_base = true;
        }
    }
    has_declarative_base
}

/// the `T` of a `Mapped[T]` field annotation, or `None` when the annotation
/// is not a mapped attribute (so the class-body entry is not a field).
///
/// this unwraps the descriptor marker: `id: Mapped[int]` declares a field
/// whose value type is `int`, which becomes the constructor parameter type.
/// `__tablename__`, `ClassVar`s, plain annotations and methods do not unwrap
/// and are therefore not fields
pub(in crate::types) fn mapped_field_type<'db>(
    db: &'db dyn Db,
    declared_ty: Type<'db>,
) -> Option<Type<'db>> {
    let mapped = KnownClass::SqlalchemyMapped.try_to_class_literal(db)?;
    let specialization = declared_ty.specialization_of(db, mapped)?;
    let [element] = specialization.types(db) else {
        return None;
    };
    Some(*element)
}
