//! the basedpython names that resolve to a module member without an import
//!
//! basedpython source may write `Mapping`, `Character` or `dynamic` bare. each
//! of them means a member of a module the file never imported — the transpiler
//! emits the matching import during lowering, and the type checker resolves the
//! bare name to that same member.
//!
//! this module is the one description of that set, so the checker and every IDE
//! feature answer for the same names: name resolution reads it to type the bare
//! name, completions read it to offer the names, and goto-definition reads it to
//! navigate to the member behind one.
//!
//! it is *not* the gate for "does anything already claim this name" — that is
//! [`crate::types::name_fallback`], which answers a different question about a
//! deliberately different set
//!
//! ruff's linter has to answer the same question to keep F821 quiet about these
//! names, and cannot read this table because it does not depend on ty's module
//! resolver. its copy is `SemanticModel::is_basedpython_transpile_resolved_name`
//! in `ruff_python_semantic`, and a name added here belongs there too

use ruff_python_stdlib::basedpython::{IMPLICIT_TYPING_NAMES, implicit_typing_name};
use ty_module_resolver::KnownModule;
use ty_python_core::scope::ScopeId;

use crate::Db;
use crate::place::{PlaceAndQualifiers, core_module_scope, known_module_symbol};
use crate::types::ProgramEnvironment;

/// where a name carries its implicit meaning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImplicitNamePosition {
    /// anywhere a name may be written, as `Mapping` is
    Anywhere,
    /// only where a type is being written, as `dynamic` is: a value-position
    /// `dynamic` stays an ordinary identifier
    TypeExpression,
    /// only where a value is being written, as the return-value markers are:
    /// they are decorators, and nothing about them is a type
    ValueExpression,
}

impl ImplicitNamePosition {
    /// whether a name carrying this position means its member where it is
    /// written.
    pub(crate) const fn admits(self, in_type_expression: bool) -> bool {
        match self {
            Self::Anywhere => true,
            Self::TypeExpression => in_type_expression,
            Self::ValueExpression => !in_type_expression,
        }
    }
}

/// the module member a bare basedpython name means
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImplicitName {
    /// the modules the member is looked up in, in order.
    ///
    /// a version-gated `typing` member (`Self`, `LiteralString`) is missing from
    /// the stub for an older target version; basedpython makes it available
    /// regardless, and the transpiler emits the `typing_extensions` import for
    /// it, so the lookup falls through to there
    pub(crate) modules: &'static [KnownModule],
    /// the name of the member, which is not always the name that was written:
    /// `dynamic` is the surface spelling of `typing.Any`
    pub(crate) member: &'static str,
    pub(crate) position: ImplicitNamePosition,
    /// whether the spelling is a keyword of the language rather than the name of
    /// the member behind it.
    ///
    /// `dynamic` is basedpython's own word for `Any`, so an editor highlights it
    /// as a keyword and offers it as one. resolution still has to know which
    /// member it means, but nothing that treats a *name* as a reference to its
    /// definition should treat this one that way
    pub(crate) is_keyword: bool,
}

impl ImplicitName {
    /// The type the bare name resolves to, or [`Place::Undefined`] when no
    /// module supplies the member.
    ///
    /// [`Place::Undefined`]: crate::place::Place::Undefined
    pub(crate) fn resolve<'db>(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> PlaceAndQualifiers<'db> {
        let mut resolved = PlaceAndQualifiers::default();
        for &module in self.modules {
            resolved = resolved.or_fall_back_to(db, env, || {
                known_module_symbol(db, env, module, self.member)
            });
        }
        resolved
    }

    /// The global scope of the module [`ImplicitName::resolve`] takes the member
    /// from, so that anything reading the member itself reads the one the type
    /// checker resolved.
    pub(crate) fn resolving_module_scope<'db>(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<ScopeId<'db>> {
        self.modules
            .iter()
            .find(|&&module| {
                !known_module_symbol(db, env, module, self.member)
                    .place
                    .is_undefined()
            })
            .and_then(|&module| core_module_scope(db, env, module))
    }
}

const TYPING_MODULES: &[KnownModule] = &[KnownModule::Typing, KnownModule::TypingExtensions];

/// the implicit names that are not `typing` members.
const EXTRA_IMPLICIT_NAMES: &[&str] = &["dynamic", "Character", "Overlapping"];

/// the return-value markers, which name themselves
const RETURN_VALUE_MARKERS: &[&str] = &["ignorable_return_value", "must_use_return_value"];

/// what `name` means where nothing else claims it, if anything.
///
/// callers in a value position must check [`ImplicitName::position`]: a
/// type-expression name written elsewhere is an ordinary identifier
pub(crate) fn implicit_name(name: &str) -> Option<ImplicitName> {
    if let Some(member) = implicit_typing_name(name) {
        return Some(ImplicitName {
            modules: TYPING_MODULES,
            member,
            position: ImplicitNamePosition::Anywhere,
            is_keyword: false,
        });
    }
    // the return-value markers are decorators, so unlike the rest of this table
    // they are written where a *value* goes
    if let Some(member) = RETURN_VALUE_MARKERS
        .iter()
        .find(|marker| **marker == name)
        .copied()
    {
        return Some(ImplicitName {
            modules: &[KnownModule::TyExtensions],
            member,
            position: ImplicitNamePosition::ValueExpression,
            is_keyword: false,
        });
    }
    let (modules, member, is_keyword) = match name {
        // basedpython's own word for `typing.Any`
        "dynamic" => (TYPING_MODULES, "Any", true),
        // the single-character string type
        "Character" => (&[KnownModule::TyExtensions][..], "Character", false),
        // a `ty_extensions` special form the vendored typeshed writes
        // unqualified (`Container.__contains__` and friends)
        "Overlapping" => (&[KnownModule::TyExtensions][..], "Overlapping", false),
        _ => return None,
    };
    Some(ImplicitName {
        modules,
        member,
        // none of these is available outside a type: a value-position `dynamic`
        // or `Character` stays an ordinary identifier
        position: ImplicitNamePosition::TypeExpression,
        is_keyword,
    })
}

/// every name this module describes.
///
/// what each one means is `implicit_name`; a caller that needs the meaning as
/// well — an IDE offering the names — asks for it per name, so that only the
/// names it goes on to use cost a module lookup
pub fn implicit_names() -> impl Iterator<Item = &'static str> {
    IMPLICIT_TYPING_NAMES
        .iter()
        .copied()
        .chain(EXTRA_IMPLICIT_NAMES.iter().copied())
        .chain(RETURN_VALUE_MARKERS.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::{implicit_name, implicit_names};

    /// every name the iterator yields describes itself, and the one spelling
    /// that means something other than itself keeps meaning it
    #[test]
    fn the_table_answers_for_every_name_it_lists() {
        let names: Vec<_> = implicit_names().collect();
        assert!(names.contains(&"Mapping"));

        for name in names {
            assert!(implicit_name(name).is_some(), "{name}");
        }

        let dynamic = implicit_name("dynamic").unwrap();
        assert_eq!(dynamic.member, "Any");
        assert!(dynamic.position.admits(true));
        assert!(!dynamic.position.admits(false));
        assert!(dynamic.is_keyword);

        let ignorable = implicit_name("ignorable_return_value").unwrap();
        assert_eq!(ignorable.member, "ignorable_return_value");
        assert!(ignorable.position.admits(false));
        assert!(!ignorable.position.admits(true));
        assert!(!ignorable.is_keyword);

        let mapping = implicit_name("Mapping").unwrap();
        assert_eq!(mapping.member, "Mapping");
        assert!(mapping.position.admits(true));
        assert!(mapping.position.admits(false));
        assert!(!mapping.is_keyword);

        assert_eq!(implicit_name("nonesuch"), None);
    }
}
