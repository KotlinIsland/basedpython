//! auto-import `typing` members referenced without an explicit import.
//!
//! treats every member of `typing` whose primary role is to construct a
//! type (ABCs, generics, type aliases) as implicitly available — referring
//! to `Sequence`, `Optional`, `Mapping`, etc. emits a matching
//! `from typing import ...` in the preamble. names whose role is already
//! covered by basedpython syntax (`Callable`, `Final`, `ClassVar`,
//! `Literal`, `TypeIs`, `Protocol`, `Generic`, `NewType`, `TypeVar`,
//! `ParamSpec`, `TypeVarTuple`, `Unpack`, `NamedTuple`, `TypedDict`, …)
//! and runtime-only helpers (`cast`, `get_type_hints`, `overload`, ...)
//! are intentionally not on this list — see `IMPLICIT_TYPING_NAMES`.
//!
//! a name is imported only when it isn't already bound at module scope, so
//! existing `from typing import X` and user-defined `X = ...` win.

use std::collections::BTreeSet;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprName, Stmt};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// `typing` members that are implicitly available when referenced in `.by`
/// source. limited to names whose role is to construct a type or describe
/// a structural protocol — names with dedicated basedpython syntax
/// (`Callable`, `Final`, `Literal`, `Protocol`, etc.) and runtime helpers
/// (`cast`, `get_type_hints`, `overload`, etc.) are excluded
pub(crate) const IMPLICIT_TYPING_NAMES: &[&str] = &[
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

fn canonical_implicit_name(s: &str) -> Option<&'static str> {
    IMPLICIT_TYPING_NAMES.iter().copied().find(|n| *n == s)
}

pub(crate) struct ImplicitTyping<'a, T: TypeInfo + ?Sized> {
    types: &'a T,
    pub(crate) needed: BTreeSet<&'static str>,
}

impl<'a, T: TypeInfo + ?Sized> ImplicitTyping<'a, T> {
    pub(crate) fn new(types: &'a T) -> Self {
        Self {
            types,
            needed: BTreeSet::new(),
        }
    }

    fn check(&mut self, name: &ExprName) {
        let id = name.id.as_str();
        let Some(canonical) = canonical_implicit_name(id) else {
            return;
        };
        if self.types.is_bound_globally(id) {
            return;
        }
        self.needed.insert(canonical);
    }
}

pub(crate) struct ImplicitTypingPass;

impl ImplicitTypingPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for ImplicitTypingPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner: ImplicitTyping<'_, dyn TypeInfo> = ImplicitTyping::new(types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        for name in inner.needed {
            ctx.required_imports
                .push(format!("from typing import {name}"));
        }
    }
}

impl<'ast, T: TypeInfo + ?Sized> Visitor<'ast> for ImplicitTyping<'_, T> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(n) = expr {
            self.check(n);
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    // the implicit-typing name set is duplicated across three crates (this list,
    // ty's `BASEDPYTHON_IMPLICIT_TYPING_NAMES`, and by_typeshed_patch's copy). if
    // they drift, the typeshed transform strips imports ty won't provide (or keeps
    // ones it would). this locks the two lists this crate can see together
    #[test]
    fn implicit_typing_names_match_ty() {
        assert_eq!(
            super::IMPLICIT_TYPING_NAMES,
            ty_python_semantic::BASEDPYTHON_IMPLICIT_TYPING_NAMES,
        );
    }

    #[test]
    fn implicit_typing_names_sorted() {
        // ty resolves the names with a binary search, so the list must stay sorted
        let mut sorted = ty_python_semantic::BASEDPYTHON_IMPLICIT_TYPING_NAMES.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            ty_python_semantic::BASEDPYTHON_IMPLICIT_TYPING_NAMES,
            sorted.as_slice(),
        );
    }

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn auto_imports_sequence() {
        check(
            "a: Sequence[int]\n",
            indoc! {"
                from typing import Sequence
                a: Sequence[int]
            "},
        );
    }

    #[test]
    fn auto_imports_optional_and_mapping() {
        check(
            "def f(x: Optional[int], y: Mapping[str, int]): ...\n",
            indoc! {"
                from typing import Mapping, Optional
                def f(x: Optional[int], y: Mapping[str, int]): ...
            "},
        );
    }

    #[test]
    fn skips_when_already_imported() {
        check(
            indoc! {"
                from typing import Sequence
                a: Sequence[int]
            "},
            indoc! {"
                from typing import Sequence
                a: Sequence[int]
            "},
        );
    }

    #[test]
    fn skips_when_imported_from_collections_abc() {
        check(
            indoc! {"
                from collections.abc import Sequence
                a: Sequence[int]
            "},
            indoc! {"
                from collections.abc import Sequence
                a: Sequence[int]
            "},
        );
    }

    #[test]
    fn skips_when_user_defined() {
        check(
            indoc! {"
                Sequence = 5
                a = Sequence
            "},
            indoc! {"
                Sequence = 5
                a = Sequence
            "},
        );
    }

    #[test]
    fn syntax_covered_names_not_imported() {
        // `Protocol`, `Generic`, `NewType`, `TypeVar` etc. have dedicated
        // basedpython syntax; referencing them by name should not
        // auto-import — they remain a NameError at runtime if the user
        // really wrote them
        unchanged("x = cast\ny = Protocol\nz = NewType\nw = Generic\n");
    }

    #[test]
    fn callable_auto_imported() {
        check(
            "x: Callable[[int], int] = lambda x: x\n",
            indoc! {"
                from typing import Callable
                x: Callable[[int], int] = lambda x: x
            "},
        );
    }

    #[test]
    fn self_redirected_to_typing_extensions() {
        // implicit `Self` round-trips through typing_redirect (added 3.11)
        check(
            indoc! {"
                class C:
                    def f(self) -> Self: ...
            "},
            indoc! {"
                from typing_extensions import Self
                class C:
                    def f(self) -> Self: ...
            "},
        );
    }

    #[test]
    fn multiple_names_sorted() {
        check(
            indoc! {"
                a: Sequence[int]
                b: Optional[int]
                c: Mapping[str, int]
            "},
            indoc! {"
                from typing import Mapping, Optional, Sequence
                a: Sequence[int]
                b: Optional[int]
                c: Mapping[str, int]
            "},
        );
    }
}
