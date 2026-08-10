//! basedpython context-sensitive resolution (`a: Color = Red`)
//!
//! where an expression's expected type is known, a bare name that is otherwise
//! unresolved is looked up as a member of that type — `Red` in a `Color` context
//! means `Color.Red`, the way kotlin and swift resolve an enum entry against the
//! expected type. only *enum* members answer: a python enum's members, a based
//! enum's unit variants, and a based enum's payload variant classes (so
//! `s: Shape = Circle(2.0)` resolves the constructor too)
//!
//! it is a *last* fallback, reached only when the name resolves to nothing else,
//! so the form is purely additive — nothing that resolves today changes meaning.
//! two further rules keep the checker and the transpiler on one answer:
//!
//! - the enum must be nameable in scope under its own name, because that is the
//!   qualifier the transpiler emits (`Red` → `Color.Red`)
//! - an ambiguous expected type — two enums in a union that both declare the
//!   name — resolves to nothing rather than picking one
//! - a name the scope binds *anywhere* is left alone, even where the binding is
//!   not yet in flow, because the transpiler cannot see flow
//!
//! that last rule is [`claimed_by_lexical_scope`], the narrower of the two shared
//! [name-fallback](crate::types::name_fallback) gates: this rule runs at the very
//! end of ty's name-resolution chain, so every name resolved without a binding
//! has already claimed itself before it is reached

use std::ops::ControlFlow;

use ruff_db::files::File;
use ruff_python_ast::name::Name;
use ty_python_core::scope::ScopeId;
use ty_python_core::{place_table, semantic_index};

use crate::Db;
use crate::place::{ConsideredDefinitions, symbol};
use crate::types::class::{ClassLiteral, ClassType, based_enum_of_variant};
use crate::types::class_base::ClassBase;
use crate::types::literal::EnumLiteralType;
use crate::types::name_fallback::claimed_by_lexical_scope;
use crate::types::receivers;
use crate::types::{Type, TypeContext};

/// an enum member reached through the expected type rather than through an
/// ordinary name lookup
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextSensitiveMember<'db> {
    /// the enum the name was found on — the qualifier the transpiler emits
    enum_class: ClassLiteral<'db>,
    /// the type of `<enum_class>.<name>`
    pub(crate) ty: Type<'db>,
}

/// what the expected type has to say about `name` before the resolution rules
/// are applied
enum Search<'db> {
    Found(ContextSensitiveMember<'db>),
    /// two enums in the expected type declare the name, so neither is chosen
    Ambiguous(ClassLiteral<'db>, ClassLiteral<'db>),
    Nothing,
}

/// the enum member `name` names anywhere in the expected type
fn search<'db>(db: &'db dyn Db, target: Type<'db>, name: &str) -> Search<'db> {
    let mut found: Option<ContextSensitiveMember<'db>> = None;
    let mut ambiguous = None;
    let _ = for_each_candidate(db, target, &mut |candidate| {
        let Some(member) = member_of(db, candidate, name) else {
            return ControlFlow::Continue(());
        };
        match found {
            // every element of a union naming the same member is the ordinary
            // case (`Shape` is the union of its variants); two *different*
            // members is a genuine ambiguity
            Some(previous) if previous != member => {
                ambiguous = Some((previous.enum_class, member.enum_class));
                ControlFlow::Break(())
            }
            Some(_) => ControlFlow::Continue(()),
            None => {
                found = Some(member);
                ControlFlow::Continue(())
            }
        }
    });
    if let Some((first, second)) = ambiguous {
        return Search::Ambiguous(first, second);
    }
    found.map_or(Search::Nothing, Search::Found)
}

/// the member `name` denotes when the expected type is `tcx`, or `None` when the
/// context offers no unambiguous enum member of that name
pub(crate) fn resolve_in_context<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    tcx: TypeContext<'db>,
    name: &str,
) -> Option<ContextSensitiveMember<'db>> {
    let target = tcx.context_sensitive_target()?;
    // reaching this fallback only means the name is unbound *at this point* in
    // the flow. the transpiler cannot see flow — it qualifies a name that no
    // scope binds at all — so a name the scope binds anywhere keeps its ordinary
    // meaning here too, or `a: Color = Red` followed by `Red = 1` would check
    // clean and `NameError` at runtime
    if claimed_by_lexical_scope(db, file, scope, name) {
        return None;
    }
    let Search::Found(member) = search(db, target, name) else {
        return None;
    };
    is_nameable(db, file, scope, member.enum_class).then_some(member)
}

/// why a name the expected type *does* declare was still not resolved — the hint
/// an `unresolved-reference` carries when context-sensitive resolution came
/// close. `None` when the context has nothing to say about the name
pub(crate) enum Miss<'db> {
    /// the scope binds the name itself, so an ordinary lookup owns it — the
    /// binding is simply not in flow here (a use before its assignment)
    Shadowed(ClassLiteral<'db>),
    /// the enum is not reachable here under its own name, so there is no
    /// qualified form to lower to
    Unnameable(ClassLiteral<'db>),
    /// two enums in the expected type declare it
    Ambiguous(ClassLiteral<'db>, ClassLiteral<'db>),
}

pub(crate) fn explain_miss<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    tcx: TypeContext<'db>,
    name: &str,
) -> Option<Miss<'db>> {
    let target = tcx.context_sensitive_target()?;
    match search(db, target, name) {
        Search::Ambiguous(first, second) => Some(Miss::Ambiguous(first, second)),
        Search::Found(member) if claimed_by_lexical_scope(db, file, scope, name) => {
            Some(Miss::Shadowed(member.enum_class))
        }
        Search::Found(member) if !is_nameable(db, file, scope, member.enum_class) => {
            Some(Miss::Unnameable(member.enum_class))
        }
        Search::Found(_) | Search::Nothing => None,
    }
}

/// visit the enum classes an expected type offers members of. a union offers
/// each of its elements' (a based enum in a type expression *is* the union of
/// its variants, and an optional enum is a union with `None`)
pub(crate) fn for_each_candidate<'db>(
    db: &'db dyn Db,
    target: Type<'db>,
    visit: &mut impl FnMut(ClassType<'db>) -> ControlFlow<()>,
) -> ControlFlow<()> {
    match target {
        Type::Union(union) => {
            for element in union.elements(db) {
                for_each_candidate(db, *element, visit)?;
            }
            ControlFlow::Continue(())
        }
        Type::NominalInstance(instance) => visit(instance.class(db)),
        Type::TypeAlias(alias) => for_each_candidate(db, alias.value_type(db), visit),
        _ => match target.as_enum_literal() {
            Some(literal) => for_each_candidate(db, literal.enum_class_instance(db), visit),
            None => ControlFlow::Continue(()),
        },
    }
}

/// the enum member `name` names on `class` or anywhere in its MRO — a based-enum
/// variant reaches its enum's other variants through the enum base it subclasses
fn member_of<'db>(
    db: &'db dyn Db,
    class: ClassType<'db>,
    name: &str,
) -> Option<ContextSensitiveMember<'db>> {
    for base in class.iter_mro(db).filter_map(ClassBase::into_class) {
        let literal = base.class_literal(db);
        // a member of a python enum, or a based enum's unit variant
        if let Some(enum_class) = literal.into_enum_class(db)
            && let Some(member) = enum_class.resolve_member(db, &Name::new(name))
        {
            return Some(ContextSensitiveMember {
                enum_class: literal,
                ty: Type::enum_literal(EnumLiteralType::new(db, enum_class, member.clone())),
            });
        }
        // a based enum's payload variant, whose class is declared in the enum body
        if let Some(ty) = base
            .own_class_member(db, None, name)
            .ignore_possibly_undefined()
            && is_variant_class(db, ty)
        {
            return Some(ContextSensitiveMember {
                enum_class: literal,
                ty,
            });
        }
    }
    None
}

/// whether `ty` is a based enum's payload variant *class*
fn is_variant_class<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    ty.as_class_literal()
        .and_then(ClassLiteral::as_static)
        .is_some_and(|class| class.is_enum_variant(db))
}

/// whether the enum is reachable from `scope` under its own name. the transpiler
/// qualifies the resolved name with it (`Red` → `Color.Red`), so a spelling that
/// is not in scope has no lowering — and must therefore not resolve here either
fn is_nameable<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    enum_class: ClassLiteral<'db>,
) -> bool {
    let name = enum_class.name(db);
    let index = semantic_index(db, file);
    for (ancestor_id, _) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = ancestor_id.to_scope_id(db, file);
        let Some(place) = place_table(db, ancestor_scope).symbol_by_name(name) else {
            continue;
        };
        if !(place.is_bound() || place.is_declared()) {
            continue;
        }
        // the first scope that gives the name a value decides it, exactly as a
        // name load would
        return symbol(
            db,
            ancestor_scope,
            name,
            ConsideredDefinitions::AllReachable,
        )
        .place
        .ignore_possibly_undefined()
        .and_then(Type::as_class_literal)
            == Some(enum_class);
    }
    false
}

/// shared checker/transpiler contract: the enum a name that resolved to `ty`
/// must be qualified with, or `None` when the name needs no qualifier
///
/// the transpiler asks this of every name it finds unbound, so the answer is
/// derived from the *resolved type* rather than from the expected type that
/// produced it: a name that is unbound everywhere, spells a member of the enum
/// its type belongs to, and can name that enum here, lowers to `<enum>.<name>`
pub(crate) fn qualifier_for_unbound_name<'db>(
    db: &'db dyn Db,
    file: File,
    scope: ScopeId<'db>,
    name: &str,
    resolved_type: impl FnOnce() -> Option<Type<'db>>,
) -> Option<&'db Name> {
    // an ordinary reference to a name bound anywhere in the lexical chain, or to
    // a builtin, keeps its ordinary spelling. checked before the name's type is
    // asked for, so a file that uses no context-sensitive name is never inferred
    // on its account
    if claimed_by_lexical_scope(db, file, scope, name) {
        return None;
    }
    // a trailing lambda block's receiver member answers *before* this fallback
    // does, so a receiver whose member happens to be an enum value keeps the
    // receiver-parameter lowering the checker resolved it to
    if receivers::implicit_receiver_name(db, file, scope, name).is_some() {
        return None;
    }
    let enum_class = enum_class_of(db, resolved_type()?)?;
    // the spelling must be the enum's own member, not merely a value of that
    // enum's type reached under some other name
    if !declares_member(db, enum_class, name) {
        return None;
    }
    is_nameable(db, file, scope, enum_class).then(|| enum_class.name(db))
}

/// the enum an already-resolved member type belongs to
fn enum_class_of<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<ClassLiteral<'db>> {
    if let Some(literal) = ty.as_enum_literal() {
        return Some(literal.enum_class(db));
    }
    let variant = ty.as_class_literal()?.as_static()?;
    Some(ClassLiteral::Static(based_enum_of_variant(db, variant)?))
}

/// whether the enum declares `name` — an enum member (or one of its aliases), or
/// a payload variant class
fn declares_member<'db>(db: &'db dyn Db, enum_class: ClassLiteral<'db>, name: &str) -> bool {
    if enum_class
        .into_enum_class(db)
        .is_some_and(|enum_class| enum_class.resolve_member(db, &Name::new(name)).is_some())
    {
        return true;
    }
    enum_class
        .default_specialization(db)
        .own_class_member(db, None, name)
        .ignore_possibly_undefined()
        .is_some_and(|ty| is_variant_class(db, ty))
}
