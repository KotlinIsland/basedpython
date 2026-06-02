//! reified type parameters (basedpython).
//!
//! a pep 695 type parameter referenced in a value position — anywhere other
//! than a type annotation — becomes a real runtime value. the function is
//! wrapped in the `generic` polyfill and its specialized call sites
//! (`f[int](…)`) route through `generic.__getitem__` instead of being stripped
//! by [`generic_call`](super::generic_call).
//!
//! ```by
//! def f[T](t: object):
//!     return isinstance(t, T)
//!
//! f[int](1)
//! ```
//!
//! →
//!
//! ```python
//! @generic
//! def f[T](t: object):
//!     return isinstance(t, T)
//!
//! f[int](1)
//! ```
//!
//! the wrapper rebuilds the function with a closure whose cells hold the type
//! arguments (pep 695 already compiles the type-parameter list as the
//! function's `co_freevars`), so the body sees `T is int`. reification reuses
//! cpython's closure machinery — no bytecode is rewritten — which means the
//! lowered `def` must keep its native `[T]` syntax. that is only available on
//! python 3.12+, so reification is gated on `min_version >= 3.12`; below that
//! the pass reports a hard error rather than emit code that cannot run.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{PythonVersion, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::reified::reified_type_param_names;

use super::ast_driver::{PassContext, TypeAwarePass};
use super::source_util::line_indent;
use crate::type_info::TypeInfo;

/// the `generic` wrapper, injected into the preamble when any function reifies.
///
/// `f[int]` produces a specialized `generic` carrying `args=(int,)`; calling it
/// rebuilds the function with a closure whose cells hold the type arguments.
/// an omitted reified slot is filled from its pep 696 default, read off the
/// function's `__type_params__`, so `f()` works when every reified parameter
/// defaults. the wrapper is also a descriptor: `__get__` captures the receiver
/// so a reified *method* (`obj.m[int]()`) binds `self` like an ordinary method
pub(crate) const GENERIC_RUNTIME: &str = "\
@dataclass
class generic:
    fn: object
    args: object = None
    instance: object = None

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return generic(self.fn, self.args, obj)

    def __getitem__(self, item):
        if self.args is not None:
            raise TypeError(\"type arguments already specified\")
        if not isinstance(item, tuple):
            item = (item,)
        return generic(self.fn, item, self.instance)

    def __call__(self, *args, **kwargs):
        freevars = self.fn.__code__.co_freevars
        values = list(self.args) if self.args is not None else []
        params = self.fn.__type_params__
        while len(values) < len(freevars):
            values.append(params[len(values)].__default__)
        temp_fn = FunctionType(
            self.fn.__code__,
            self.fn.__globals__,
            self.fn.__name__,
            None,
            tuple(CellType(value) for value in values),
        )
        if self.instance is not None:
            return temp_fn(self.instance, *args, **kwargs)
        return temp_fn(*args, **kwargs)
";

/// marker comment appended to the synthesized `@generic` decorator line. the
/// reverse transpiler keys on it to re-sugar the wrapper back to a bare `def`;
/// a hand-written `@generic` (without the marker) is left untouched. this
/// stands in for the `ReifiedGeneric` `LoweringMap` provenance the design doc
/// describes — the current pipeline carries provenance as an emitted token
pub(crate) const REIFIED_MARKER: &str = "  # basedpython: reified";

struct ReifiedGeneric<'src> {
    source: &'src str,
    /// zero-width insertions of the `@generic` decorator line
    edits: Vec<(TextRange, String)>,
    /// at least one function reified — emit the polyfill + its imports
    used: bool,
    /// a reified function was found but the target is below 3.12
    below_312: Vec<String>,
    supports_native_generics: bool,
}

impl<'src> ReifiedGeneric<'src> {
    fn new(source: &'src str, min_version: PythonVersion) -> Self {
        Self {
            source,
            edits: Vec::new(),
            used: false,
            below_312: Vec::new(),
            supports_native_generics: min_version >= PythonVersion::PY312,
        }
    }

    fn wrap(&mut self, function: &StmtFunctionDef) {
        if reified_type_param_names(function).is_empty() {
            return;
        }
        if !self.supports_native_generics {
            self.below_312.push(function.name.id.to_string());
            return;
        }
        // the decorator goes on its own line directly above the `def`, sharing
        // its indentation. the first decorator's range start (or the `def`
        // keyword when undecorated) is the insertion point
        let anchor = function
            .decorator_list
            .first()
            .map_or_else(|| function.range().start(), |d| d.range().start());
        let indent = line_indent(self.source, anchor);
        self.edits.push((
            TextRange::empty(anchor),
            format!("@generic{REIFIED_MARKER}\n{indent}"),
        ));
        self.used = true;
    }
}

impl<'ast> Visitor<'ast> for ReifiedGeneric<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            self.wrap(function);
        }
        walk_stmt(self, stmt);
    }
}

pub(crate) struct ReifiedGenericPass<'src> {
    source: &'src str,
    min_version: PythonVersion,
}

impl<'src> ReifiedGenericPass<'src> {
    pub(crate) fn new(source: &'src str, min_version: PythonVersion) -> Self {
        Self {
            source,
            min_version,
        }
    }
}

impl TypeAwarePass for ReifiedGenericPass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ReifiedGeneric::new(self.source, self.min_version);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if let Some(name) = inner.below_312.first() {
            ctx.errors.push(format!(
                "reified generic function `{name}` requires python 3.12 or newer: \
                 reification reuses pep 695 closure cells, which need native \
                 type-parameter syntax in the generated python"
            ));
            return;
        }
        if inner.used {
            ctx.required_imports
                .push("from dataclasses import dataclass".to_owned());
            ctx.required_imports
                .push("from types import CellType, FunctionType".to_owned());
            ctx.required_imports.push(GENERIC_RUNTIME.to_owned());
        }
        ctx.text_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn check_at(input: &str, expected: &str, version: PythonVersion) {
        let config = Config {
            min_version: version,
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
    }

    #[test]
    fn value_position_use_wraps_with_generic() {
        check_at(
            indoc! {"
                def f[T](t: object):
                    return isinstance(t, T)
                f[int](1)
            "},
            indoc! {"
                from dataclasses import dataclass
                from types import CellType, FunctionType
                @dataclass
                class generic:
                    fn: object
                    args: object = None
                    instance: object = None

                    def __get__(self, obj, objtype=None):
                        if obj is None:
                            return self
                        return generic(self.fn, self.args, obj)

                    def __getitem__(self, item):
                        if self.args is not None:
                            raise TypeError(\"type arguments already specified\")
                        if not isinstance(item, tuple):
                            item = (item,)
                        return generic(self.fn, item, self.instance)

                    def __call__(self, *args, **kwargs):
                        freevars = self.fn.__code__.co_freevars
                        values = list(self.args) if self.args is not None else []
                        params = self.fn.__type_params__
                        while len(values) < len(freevars):
                            values.append(params[len(values)].__default__)
                        temp_fn = FunctionType(
                            self.fn.__code__,
                            self.fn.__globals__,
                            self.fn.__name__,
                            None,
                            tuple(CellType(value) for value in values),
                        )
                        if self.instance is not None:
                            return temp_fn(self.instance, *args, **kwargs)
                        return temp_fn(*args, **kwargs)

                @generic  # basedpython: reified
                def f[T](t: object):
                    return isinstance(t, T)
                f[int](1)
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn annotation_only_use_is_not_reified() {
        // T appears only in annotations — erased generic, call site stripped,
        // no @generic wrapper
        check_at(
            indoc! {"
                def f[T](x: T) -> T:
                    return x
                f[int](1)
            "},
            indoc! {"
                def f[T](x: T) -> T:
                    return x
                f(1)
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn reified_call_site_not_stripped() {
        // the specialized call must route through generic.__getitem__, so the
        // `[int]` is preserved (not stripped to `f(1)`)
        let out = transpile(
            indoc! {"
                def f[T]():
                    print(T)
                f[int]()
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("f[int]()"),
            "call site should keep [int]: {out}"
        );
        assert!(
            out.contains("@generic  # basedpython: reified"),
            "function should be wrapped: {out}"
        );
    }

    #[test]
    fn print_value_position_wraps() {
        let out = transpile(
            indoc! {"
                def f[T]():
                    print(T)
                f[int]()
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(out.contains("@generic"), "should wrap: {out}");
    }

    #[test]
    fn below_312_is_an_error() {
        let err = transpile(
            indoc! {"
                def f[T]():
                    print(T)
            "},
            &Config {
                min_version: PythonVersion::PY311,
                ..Config::test_default()
            },
        )
        .unwrap_err();
        assert!(
            err.contains("3.12"),
            "expected a 3.12 requirement error, got: {err}"
        );
    }

    #[test]
    fn non_generic_function_untouched() {
        unchanged("def f(x):\n    return x\n");
    }

    #[test]
    fn handwritten_generic_decorator_preserved() {
        // a user-written `@generic` (no reified marker) is left as-is — only
        // the synthesized wrapper carries the marker
        let out = transpile(
            "@generic\ndef f(x):\n    return x\n",
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            !out.contains("basedpython: reified"),
            "should not add the marker to a hand-written decorator: {out}"
        );
    }
}
