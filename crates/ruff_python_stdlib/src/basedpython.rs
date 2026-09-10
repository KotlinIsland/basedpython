//! names that basedpython resolves at transpile time rather than at runtime

/// `typing` members that are implicitly available when referenced in basedpython
/// source. limited to names whose role is to construct a type or describe a
/// structural protocol — names with dedicated basedpython syntax (`Final`,
/// `Literal`, `Protocol`, etc.) and runtime helpers (`cast`, `get_type_hints`,
/// `overload`, etc.) are excluded
///
/// this is the single source of truth for the set. the transpiler inserts the
/// matching `from typing import …` during lowering, the type checker resolves
/// the bare name to the same member, the typeshed patcher strips the redundant
/// imports, and ruff's semantic model treats the name as bound
///
/// must stay sorted: [`is_implicit_typing_name`] binary searches it
pub const IMPLICIT_TYPING_NAMES: &[&str] = &[
    "AbstractSet",
    "Annotated",
    "Any",
    "AnyStr",
    "AsyncContextManager",
    "AsyncGenerator",
    "AsyncIterable",
    "AsyncIterator",
    "Awaitable",
    "BinaryIO",
    "ByteString",
    "Callable",
    "ChainMap",
    "Collection",
    "Concatenate",
    "Container",
    "ContextManager",
    "Coroutine",
    "Counter",
    "DefaultDict",
    "Deque",
    "Dict",
    "FrozenSet",
    "Generator",
    "Hashable",
    "IO",
    "ItemsView",
    "Iterable",
    "Iterator",
    "KeysView",
    "List",
    "LiteralString",
    "Mapping",
    "MappingView",
    "Match",
    "MutableMapping",
    "MutableSequence",
    "MutableSet",
    "Never",
    "NoReturn",
    "NotRequired",
    "Optional",
    "OrderedDict",
    "Pattern",
    "ReadOnly",
    "Required",
    "Reversible",
    "Self",
    "Sequence",
    "Set",
    "Sized",
    "SupportsAbs",
    "SupportsBytes",
    "SupportsComplex",
    "SupportsFloat",
    "SupportsIndex",
    "SupportsInt",
    "SupportsRound",
    "Text",
    "TextIO",
    "Tuple",
    "Type",
    "TypeGuard",
    "Union",
    "ValuesView",
];

/// whether `name` is a `typing` member implicitly available in basedpython source
pub fn is_implicit_typing_name(name: &str) -> bool {
    implicit_typing_name(name).is_some()
}

/// the [`IMPLICIT_TYPING_NAMES`] entry `name` matches
///
/// the entry outlives the borrowed name, so a caller that has to keep hold of
/// the member it resolved — to look it up in the `typing` module later — can do
/// so without copying it
pub fn implicit_typing_name(name: &str) -> Option<&'static str> {
    IMPLICIT_TYPING_NAMES
        .binary_search(&name)
        .ok()
        .map(|index| IMPLICIT_TYPING_NAMES[index])
}

/// whether `private` on a class member named `name` actually hides it — that is,
/// whether python would name-mangle the `__{name}` the lowering renames it to
///
/// python's rule is two or more leading underscores and at most one trailing
/// one, so a name that already ends in `__` is looked up verbatim. every name of
/// two or more underscores already ends in `__`; the one that does not is `_`,
/// which prefixes to `___` and is looked up verbatim just the same
///
/// this is the single source of truth for the rule. the transpiler skips the
/// rename where it answers `false`, and the type checker reports the modifier as
/// having no effect at exactly the same names
pub fn private_mangles(name: &str) -> bool {
    !name.ends_with("__") && name != "_"
}

/// the name a class member is emitted under when a visibility keyword spells
/// itself with `prefix` — `__` for `private`, `_` for `protected`, and the empty
/// string for a member that is neither
///
/// `None` when the member keeps the name it was written with: a name python
/// looks up verbatim ([`private_mangles`]) cannot be hidden by renaming it, and
/// a name that already carries the prefix is already spelled that way. the
/// second case matters most for `protected`, where a further underscore would
/// not make the member more protected but private
pub fn visibility_rename(name: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() || !private_mangles(name) || name.starts_with(prefix) {
        return None;
    }
    Some(format!("{prefix}{name}"))
}

#[cfg(test)]
mod tests {
    use super::{
        IMPLICIT_TYPING_NAMES, implicit_typing_name, is_implicit_typing_name, private_mangles,
    };

    #[test]
    fn implicit_typing_names_sorted() {
        let mut sorted = IMPLICIT_TYPING_NAMES.to_vec();
        sorted.sort_unstable();
        assert_eq!(IMPLICIT_TYPING_NAMES, sorted.as_slice());
    }

    #[test]
    fn implicit_typing_name_lookup() {
        assert!(is_implicit_typing_name("Optional"));
        assert!(is_implicit_typing_name("AbstractSet"));
        assert!(is_implicit_typing_name("ValuesView"));
        assert!(!is_implicit_typing_name("Protocol"));
        assert!(!is_implicit_typing_name("cast"));
    }

    /// the entry a name matches is the one in the table, so a caller may hold on
    /// to it after the name it looked up is gone
    #[test]
    fn implicit_typing_name_returns_the_entry() {
        let entry: Option<&'static str> = implicit_typing_name(&String::from("Mapping"));
        assert_eq!(entry, Some("Mapping"));
        assert_eq!(implicit_typing_name("cast"), None);
    }

    #[test]
    fn a_name_python_looks_up_verbatim_is_not_mangled() {
        assert!(private_mangles("helper"));
        assert!(private_mangles("trailing_"));
        // `__` + the name is what python is asked about, so a name that is only
        // underscores lands on the same rule a dunder does
        assert!(!private_mangles("_"));
        assert!(!private_mangles("__"));
        assert!(!private_mangles("__init__"));
        assert!(!private_mangles("__repr__"));
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::visibility_rename;
    #[test]
    fn a_visibility_prefix_is_applied_once() {
        assert_eq!(
            visibility_rename("helper", "__").as_deref(),
            Some("__helper")
        );
        assert_eq!(visibility_rename("helper", "_").as_deref(), Some("_helper"));
        assert_eq!(visibility_rename("helper", ""), None);
    }

    /// a `protected` member written `_x` is already spelled the way `protected`
    /// spells it; prefixing again would make it `__x`, which python mangles
    #[test]
    fn a_name_already_carrying_the_prefix_is_left_alone() {
        assert_eq!(visibility_rename("_x", "_"), None);
        assert_eq!(visibility_rename("__x", "__"), None);
        assert_eq!(visibility_rename("_x", "__").as_deref(), Some("___x"));
    }

    #[test]
    fn a_name_python_looks_up_verbatim_is_left_alone() {
        assert_eq!(visibility_rename("__repr__", "__"), None);
        assert_eq!(visibility_rename("__repr__", "_"), None);
    }
}
