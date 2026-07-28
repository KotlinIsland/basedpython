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
    IMPLICIT_TYPING_NAMES.binary_search(&name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{IMPLICIT_TYPING_NAMES, is_implicit_typing_name};

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
}
