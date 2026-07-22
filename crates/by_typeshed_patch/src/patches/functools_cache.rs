//! precise typing for `functools.cache` / `functools.lru_cache`
//!
//! upstream parametrizes `_lru_cache_wrapper` by the return type only, so the
//! cached callable loses its parameter list — `@cache def f(x: int)` accepts
//! `f(1, 2, 3)`. this rewrites the wrapper to capture the *whole* wrapped
//! callable (`_lru_cache_wrapper[Fn]`) and recover the signature via generic
//! self-binding:
//!
//! - `__call__` becomes a generic method whose `self` decomposes `Fn` into a
//!   `**P` / `R` and forwards the arguments, so calls are checked against the
//!   real signature
//! - a `__get__` overload strips the leading `self` when the wrapper decorates a
//!   method, so `a.m(...)` is checked too
//!
//! this needs no `ParamSpec` or `Concatenate` spelling — a callable-type bound
//! (`Fn: (...) -> object`) plus generic-self decomposition is enough. the edits
//! are literal, unique fragments that vanish after substitution, so the patch is
//! idempotent and scoped to `functools`

use std::path::Path;

use ruff_python_ast::ModModule;
use ruff_python_parser::Parsed;

use crate::{Edit, Patch};

pub struct FunctoolsCache;

/// (exact source fragment, replacement). each fragment is unique within
/// `functools.byi`; a missing fragment (upstream drift, or an already-converted
/// tree) is skipped rather than erroring
/// our own `__get__` overload marker (distinct from functools' other `__get__`s),
/// used to skip re-inserting on an already-converted tree
const GET_MARKER: &str = "def __get__[S, **P, R](self: _lru_cache_wrapper[(S, **P) -> R]";

/// the member the `__get__` overloads are inserted before (unique to `_lru_cache_wrapper`)
const GET_ANCHOR: &str = "    def cache_info(self) -> _CacheInfo:";

const REPLACEMENTS: &[(&str, &str)] = &[
    // the wrapper is generic over the whole wrapped callable, not its return type
    (
        "_lru_cache_wrapper[out Element]",
        "_lru_cache_wrapper[Fn: (...) -> object]",
    ),
    ("__wrapped__: (...) -> Element", "__wrapped__: Fn"),
    // decompose `Fn` back into params + return via a generic `self`
    (
        "def __call__(self, *args: Hashable, **kwargs: Hashable) -> Element:",
        "def __call__[**P, R](self: _lru_cache_wrapper[(**P) -> R], *args: P.args, **kwargs: P.kwargs) -> R:",
    ),
    (
        "    def __copy__(self) -> _lru_cache_wrapper[Element]",
        "    def __copy__(self) -> Self",
    ),
    (
        "    def __deepcopy__(self, memo: dynamic, /) -> _lru_cache_wrapper[Element]",
        "    def __deepcopy__(self, memo: dynamic, /) -> Self",
    ),
    (
        "def cache[Element](user_function: (...) -> Element, /) -> _lru_cache_wrapper[Element]:",
        "def cache[Fn: (...) -> object](user_function: Fn, /) -> _lru_cache_wrapper[Fn]:",
    ),
    (
        "def lru_cache[Element](maxsize: int | None = 128, typed: bool = False) -> ((...) -> Element) -> _lru_cache_wrapper[Element]:",
        "def lru_cache[Fn: (...) -> object](maxsize: int | None = 128, typed: bool = False) -> (Fn) -> _lru_cache_wrapper[Fn]:",
    ),
    (
        "def lru_cache[Element](maxsize: (...) -> Element, typed: bool = False) -> _lru_cache_wrapper[Element]",
        "def lru_cache[Fn: (...) -> object](maxsize: Fn, typed: bool = False) -> _lru_cache_wrapper[Fn]",
    ),
];

impl Patch for FunctoolsCache {
    fn name(&self) -> &'static str {
        "functools-cache"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[
            "functools._lru_cache_wrapper",
            "functools.cache",
            "functools.lru_cache",
        ]
    }

    fn rewrite(&self, module_path: &Path, _parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if crate::module_qualname(module_path).as_deref() != Some("functools") {
            return Vec::new();
        }
        let mut edits = Vec::new();
        for (old, new) in REPLACEMENTS {
            if let Some(start) = source.find(old) {
                edits.push(Edit {
                    start,
                    end: start + old.len(),
                    replacement: (*new).to_string(),
                });
            }
        }
        // strip the receiver when the wrapper decorates a method (descriptor
        // access); the `instance: None` overload keeps class-level access
        // returning the wrapper. anchored before `cache_info` (unique to
        // `_lru_cache_wrapper`), guarded on our own marker so a re-run doesn't
        // stack a second pair (functools has unrelated `__get__`s elsewhere)
        if !source.contains(GET_MARKER) {
            if let Some(start) = source.find(GET_ANCHOR) {
                edits.push(Edit {
                    start,
                    end: start,
                    replacement: "    def __get__(self, instance: None, owner: type | None = None) -> Self\n    \
def __get__[S, **P, R](self: _lru_cache_wrapper[(S, **P) -> R], instance: S, owner: type | None = None) -> _lru_cache_wrapper[(**P) -> R]\n\n"
                        .to_string(),
                });
            }
        }
        edits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = FunctoolsCache.rewrite(Path::new("functools.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn rewrites_wrapper_cache_and_lru_cache() {
        let src = "\
final class _lru_cache_wrapper[out Element]:
    __wrapped__: (...) -> Element
    def __call__(self, *args: Hashable, **kwargs: Hashable) -> Element:
        \"\"\"Call self as a function.\"\"\"

    def cache_info(self) -> _CacheInfo:
        \"\"\"Report cache statistics\"\"\"

    def cache_parameters(self) -> _CacheParameters
    def __copy__(self) -> _lru_cache_wrapper[Element]
    def __deepcopy__(self, memo: dynamic, /) -> _lru_cache_wrapper[Element]

class cached_property[out Element]:
    def __get__(self, instance: object, owner: type | None = None) -> Element

def lru_cache[Element](maxsize: int | None = 128, typed: bool = False) -> ((...) -> Element) -> _lru_cache_wrapper[Element]:
    \"\"\"doc\"\"\"
def lru_cache[Element](maxsize: (...) -> Element, typed: bool = False) -> _lru_cache_wrapper[Element]
def cache[Element](user_function: (...) -> Element, /) -> _lru_cache_wrapper[Element]:
    \"\"\"doc\"\"\"
";
        let expected = "\
final class _lru_cache_wrapper[Fn: (...) -> object]:
    __wrapped__: Fn
    def __call__[**P, R](self: _lru_cache_wrapper[(**P) -> R], *args: P.args, **kwargs: P.kwargs) -> R:
        \"\"\"Call self as a function.\"\"\"

    def __get__(self, instance: None, owner: type | None = None) -> Self
    def __get__[S, **P, R](self: _lru_cache_wrapper[(S, **P) -> R], instance: S, owner: type | None = None) -> _lru_cache_wrapper[(**P) -> R]

    def cache_info(self) -> _CacheInfo:
        \"\"\"Report cache statistics\"\"\"

    def cache_parameters(self) -> _CacheParameters
    def __copy__(self) -> Self
    def __deepcopy__(self, memo: dynamic, /) -> Self

class cached_property[out Element]:
    def __get__(self, instance: object, owner: type | None = None) -> Element

def lru_cache[Fn: (...) -> object](maxsize: int | None = 128, typed: bool = False) -> (Fn) -> _lru_cache_wrapper[Fn]:
    \"\"\"doc\"\"\"
def lru_cache[Fn: (...) -> object](maxsize: Fn, typed: bool = False) -> _lru_cache_wrapper[Fn]
def cache[Fn: (...) -> object](user_function: Fn, /) -> _lru_cache_wrapper[Fn]:
    \"\"\"doc\"\"\"
";
        assert_eq!(run(src), expected);
        // idempotent: the fragments are gone after the first pass
        assert_eq!(run(expected), expected);
    }

    #[test]
    fn skips_non_functools() {
        let parsed = parse_unchecked_source(
            "final class _lru_cache_wrapper[out Element]: ...\n",
            PySourceType::BasedPythonStub,
        );
        let edits = FunctoolsCache.rewrite(Path::new("other.byi"), &parsed, "irrelevant");
        assert!(edits.is_empty());
    }
}
