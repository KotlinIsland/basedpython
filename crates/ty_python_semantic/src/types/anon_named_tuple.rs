//! basedpython anonymous named tuples, `(name: T, ...)`
//!
//! each one is a synthesized `NamedTuple` class. a plain tuple display standing
//! where one is expected — `take((1, 2))` with `def take(p: (a: int, b: int))` —
//! is read as an instance of that class, and the transpiler constructs it there.
//! both sides read the decision off the display's inferred type, so the checker
//! never accepts field access on a value the emitted python leaves a plain tuple

use ruff_python_ast::name::Name;
use salsa::plumbing::AsId;

use crate::Db;
use crate::types::class::{
    ClassLiteral, DynamicNamedTupleAnchor, DynamicNamedTupleLiteral, NamedTupleSpec,
};
use crate::types::{ProgramEnvironment, Type, UnionType};

/// the prefix of every synthesized anonymous named tuple class's name
const CLASS_PREFIX: &str = "_AnonNamedTuple_";

/// the class and field specification of an anonymous named tuple `ty` is an instance
/// of, or (for a type form in a value position) the class itself
pub(crate) fn anon_named_tuple_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<(DynamicNamedTupleLiteral<'db>, NamedTupleSpec<'db>)> {
    let literal = match ty {
        Type::NominalInstance(instance) => instance.class(db, env).class_literal(db),
        Type::ClassLiteral(literal) => literal,
        _ => return None,
    };
    let ClassLiteral::DynamicNamedTuple(class) = literal else {
        return None;
    };
    if !class.name(db).as_str().starts_with(CLASS_PREFIX) {
        return None;
    }
    match class.anchor(db) {
        DynamicNamedTupleAnchor::ScopeOffset { spec, .. } => Some((class, *spec)),
        DynamicNamedTupleAnchor::CollectionsDefinition { .. }
        | DynamicNamedTupleAnchor::TypingDefinition(_) => None,
    }
}

/// every anonymous named tuple instance `expected` allows, in the order its union
/// lists them. a type alias is read through, so this follows what an annotation
/// means rather than how it is spelled
pub(crate) fn expected_anon_named_tuples<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    expected: Type<'db>,
) -> Vec<(Type<'db>, NamedTupleSpec<'db>)> {
    fn collect<'db>(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
        found: &mut Vec<(Type<'db>, NamedTupleSpec<'db>)>,
    ) {
        let ty = ty.resolve_type_alias(db);
        if let Type::Union(union) = ty {
            for member in union.elements(db) {
                collect(db, env, *member, found);
            }
            return;
        }
        if let Type::NominalInstance(_) = ty
            && let Some((_, spec)) = anon_named_tuple_class(db, env, ty)
            && !found.iter().any(|(seen, _)| *seen == ty)
        {
            found.push((ty, spec));
        }
    }
    let mut found = Vec::new();
    collect(db, env, expected, &mut found);
    found
}

/// `expected` without its anonymous named tuple arms, or `None` when nothing is left.
///
/// a plain tuple literal is an instance of an anonymous named tuple only where it is built
/// as one, so one that is not cannot use such an arm as the shape it is inferred against —
/// a `(a: list[int])` arm would make `([1, "s"],)` fail a `tuple[list[object]]` arm beside it
pub(crate) fn without_anon_named_tuples<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    expected: Type<'db>,
) -> Option<Type<'db>> {
    let resolved = expected.resolve_type_alias(db);
    let arms: Vec<Type<'db>> = match resolved {
        Type::Union(union) => union.elements(db).to_vec(),
        other => vec![other],
    };
    let kept: Vec<Type<'db>> = arms
        .into_iter()
        .filter(|arm| {
            !(matches!(arm, Type::NominalInstance(_))
                && anon_named_tuple_class(db, env, *arm).is_some())
        })
        .collect();
    (!kept.is_empty()).then(|| UnionType::from_elements(db, env, kept))
}

/// one anonymous named tuple class, as the checker identifies it. two type forms
/// with the same identity are the same type, whatever their spelling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnonNamedTupleId(salsa::Id);

/// an anonymous named tuple class the checker read an expression as
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnonNamedTupleClass<'db> {
    pub id: AnonNamedTupleId,
    /// each field's name and declared type, in order
    pub fields: Vec<(Name, Type<'db>)>,
}

impl<'db> AnonNamedTupleClass<'db> {
    /// the anonymous named tuple `ty` is an instance of, or the class itself for a type
    /// form standing in a value position
    pub fn of(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> Option<Self> {
        let (class, spec) = anon_named_tuple_class(db, env, ty)?;
        Some(Self {
            id: AnonNamedTupleId(class.as_id()),
            fields: spec
                .fields(db)
                .iter()
                .map(|field| (field.name.clone(), field.ty))
                .collect(),
        })
    }
}
