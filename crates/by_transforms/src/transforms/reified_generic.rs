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
//! the wrapper rebuilds the function with a closure in which the cells named
//! after type parameters hold the type arguments (pep 695 already compiles a
//! body-referenced type parameter into the function's `co_freevars`), so the
//! body sees `T is int`. every other cell — captured outer locals, the
//! implicit `__class__` of zero-arg `super()` — is carried over untouched.
//! reification reuses cpython's closure machinery — no bytecode is rewritten —
//! which means the lowered `def` must keep its native `[T]` syntax. that is
//! only available on python 3.12+, so reification is gated on
//! `min_version >= 3.12`; below that the pass reports a hard error rather than
//! emit code that cannot run. pep 696 defaults (`[T = int]`) raise the bar to
//! 3.13 — they are not valid syntax on 3.12, and the erased polyfill can't
//! stand in because it discards the native parameter list reification depends
//! on — so a reified function with a defaulted parameter on a 3.12 target is a
//! hard error too.
//!
//! the decorator is inserted *innermost* — directly above the `def` header,
//! below any user decorators — so the wrapper always receives the raw function
//! object whose closure it rebuilds. outer decorators then compose with the
//! wrapper the same way ty types them: `@staticmethod` and identity-typed
//! decorators pass it through, and a decorator that returns a different
//! callable makes the specialization subscript a type error at the use site.
//! the one binding that cannot compose is `classmethod` (it wraps the callable
//! in an opaque bound method with no `__getitem__`), so a reified classmethod
//! is a hard transpile error.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, PythonVersion, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::reified::reified_type_param_names;

use super::ast_driver::{PassContext, TypeAwarePass};
use super::source_util::{line_indent, line_start};
use crate::type_info::TypeInfo;

/// the `generic` wrapper, injected into the preamble when any function reifies.
///
/// `f[int]` produces a specialized `generic` carrying `args=(int,)`; calling it
/// rebuilds the function with a closure whose type-parameter cells hold the
/// type arguments, keyed by `co_freevars` name so unrelated cells (captured
/// locals, `__class__`) survive. parameter defaults, kwonly defaults and the
/// qualname carry over to the rebuilt function. an omitted reified slot is
/// filled from its pep 696 default, read off the function's `__type_params__`,
/// so `f()` works when every reified parameter defaults; a slot with no value
/// raises `TypeError`, as does over-specialization. the wrapper is also a
/// descriptor: `__get__` captures the receiver so a reified *method*
/// (`obj.m[int]()`) binds `self` like an ordinary method. attribute access
/// falls through to the wrapped function, keeping introspection
/// (`f.__name__`, `f.__doc__`) working
pub(crate) const GENERIC_RUNTIME: &str = "\
class generic:
    def __init__(self, fn, args=None, instance=None):
        self.fn = fn
        self.args = args
        self.instance = instance

    def __repr__(self):
        return f\"<generic {self.fn!r}>\"

    def __getattr__(self, name):
        if name == \"fn\":
            raise AttributeError(name)
        return getattr(self.fn, name)

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return generic(self.fn, self.args, obj)

    def __getitem__(self, item):
        if self.args is not None:
            raise TypeError(\"type arguments already specified\")
        if not isinstance(item, tuple):
            item = (item,)
        params = self.fn.__type_params__
        if len(item) > len(params):
            raise TypeError(
                f\"too many type arguments for {self.fn.__name__}: \"
                f\"expected {len(params)}, got {len(item)}\"
            )
        return generic(self.fn, item, self.instance)

    def __call__(self, *args, **kwargs):
        fn = self.fn
        code = fn.__code__
        supplied = self.args if self.args is not None else ()
        values = {}
        for index, param in enumerate(fn.__type_params__):
            name = param.__name__
            if index < len(supplied):
                values[name] = supplied[index]
                continue
            has_default = getattr(param, \"has_default\", None)
            if has_default is not None and has_default():
                values[name] = param.__default__
            elif name in code.co_freevars:
                raise TypeError(f\"{fn.__name__}() missing a type argument for {name!r}\")
        closure = tuple(
            CellType(values[name]) if name in values else cell
            for name, cell in zip(code.co_freevars, fn.__closure__ or ())
        )
        temp_fn = FunctionType(code, fn.__globals__, fn.__name__, fn.__defaults__, closure)
        temp_fn.__kwdefaults__ = fn.__kwdefaults__
        temp_fn.__qualname__ = fn.__qualname__
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
    /// a reified function has a pep 696 default but the target is below 3.13
    defaulted_below_313: Vec<String>,
    /// a reified function is classmethod-decorated — unsupported
    classmethods: Vec<String>,
    supports_native_generics: bool,
    supports_param_defaults: bool,
}

impl<'src> ReifiedGeneric<'src> {
    fn new(source: &'src str, min_version: PythonVersion) -> Self {
        Self {
            source,
            edits: Vec::new(),
            used: false,
            below_312: Vec::new(),
            defaulted_below_313: Vec::new(),
            classmethods: Vec::new(),
            supports_native_generics: min_version >= PythonVersion::PY312,
            supports_param_defaults: min_version >= PythonVersion::PY313,
        }
    }

    fn wrap(&mut self, function: &StmtFunctionDef) {
        if reified_type_param_names(self.source, function).is_empty() {
            return;
        }
        if !self.supports_native_generics {
            self.below_312.push(function.name.id.to_string());
            return;
        }
        // any default in the list makes the native header 3.13-only syntax,
        // even on parameters that don't themselves reify
        if !self.supports_param_defaults
            && function
                .type_params
                .as_deref()
                .is_some_and(|tp| tp.type_params.iter().any(|p| p.default().is_some()))
        {
            self.defaulted_below_313.push(function.name.id.to_string());
            return;
        }
        if function
            .decorator_list
            .iter()
            .any(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "classmethod"))
        {
            self.classmethods.push(function.name.id.to_string());
            return;
        }
        // the decorator goes on its own line directly above the `def`/`async`
        // header — innermost, below any user decorators — sharing the header's
        // indentation. anchored via the function name, whose line is the header
        let name_start = function.name.range().start();
        let indent = line_indent(self.source, name_start);
        let anchor = line_start(self.source, name_start) + TextSize::of(indent);
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

/// inserts the inferred `[...]` specialization at bare reified call sites
struct SpecializationInjector<'ti> {
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for SpecializationInjector<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            // an explicit `f[int](…)` already carries its specialization
            && !matches!(call.func.as_ref(), Expr::Subscript(_))
            && let Some(arguments) = self.types.reified_call_specialization(call)
        {
            self.edits.push((
                TextRange::empty(call.func.range().end()),
                format!("[{arguments}]"),
            ));
        }
        walk_expr(self, expr);
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
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ReifiedGeneric::new(self.source, self.min_version);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        // bare calls of reified generics: inject the specialization ty inferred
        // from the arguments (`f(1)` → `f[int](1)`), so the call routes through
        // `generic.__getitem__` exactly like an explicit one. ty accepts a bare
        // call only when this injection exists, so the two stay in agreement
        let mut injector = SpecializationInjector {
            types,
            edits: Vec::new(),
        };
        for stmt in stmts {
            injector.visit_stmt(stmt);
        }
        ctx.text_edits.extend(injector.edits);
        if let Some(name) = inner.below_312.first() {
            ctx.errors.push(format!(
                "reified generic function `{name}` requires python 3.12 or newer: \
                 reification reuses pep 695 closure cells, which need native \
                 type-parameter syntax in the generated python"
            ));
            return;
        }
        if let Some(name) = inner.defaulted_below_313.first() {
            ctx.errors.push(format!(
                "reified generic function `{name}` has a type-parameter default, \
                 which requires python 3.13 or newer: reification keeps the native \
                 pep 695 parameter list in the generated python, and pep 696 \
                 defaults are not valid syntax before 3.13"
            ));
            return;
        }
        if let Some(name) = inner.classmethods.first() {
            ctx.errors.push(format!(
                "classmethod `{name}` cannot have reified type parameters: the \
                 classmethod binding hides the function whose closure holds the \
                 reified cells"
            ));
            return;
        }
        if inner.used {
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
                from types import CellType, FunctionType
                class generic:
                    def __init__(self, fn, args=None, instance=None):
                        self.fn = fn
                        self.args = args
                        self.instance = instance

                    def __repr__(self):
                        return f\"<generic {self.fn!r}>\"

                    def __getattr__(self, name):
                        if name == \"fn\":
                            raise AttributeError(name)
                        return getattr(self.fn, name)

                    def __get__(self, obj, objtype=None):
                        if obj is None:
                            return self
                        return generic(self.fn, self.args, obj)

                    def __getitem__(self, item):
                        if self.args is not None:
                            raise TypeError(\"type arguments already specified\")
                        if not isinstance(item, tuple):
                            item = (item,)
                        params = self.fn.__type_params__
                        if len(item) > len(params):
                            raise TypeError(
                                f\"too many type arguments for {self.fn.__name__}: \"
                                f\"expected {len(params)}, got {len(item)}\"
                            )
                        return generic(self.fn, item, self.instance)

                    def __call__(self, *args, **kwargs):
                        fn = self.fn
                        code = fn.__code__
                        supplied = self.args if self.args is not None else ()
                        values = {}
                        for index, param in enumerate(fn.__type_params__):
                            name = param.__name__
                            if index < len(supplied):
                                values[name] = supplied[index]
                                continue
                            has_default = getattr(param, \"has_default\", None)
                            if has_default is not None and has_default():
                                values[name] = param.__default__
                            elif name in code.co_freevars:
                                raise TypeError(f\"{fn.__name__}() missing a type argument for {name!r}\")
                        closure = tuple(
                            CellType(values[name]) if name in values else cell
                            for name, cell in zip(code.co_freevars, fn.__closure__ or ())
                        )
                        temp_fn = FunctionType(code, fn.__globals__, fn.__name__, fn.__defaults__, closure)
                        temp_fn.__kwdefaults__ = fn.__kwdefaults__
                        temp_fn.__qualname__ = fn.__qualname__
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
    fn bare_call_gets_inferred_specialization() {
        let out = transpile(
            indoc! {"
                def f[T](t: T):
                    print(T)
                f(1)
                f(\"x\")
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(out.contains("f[int](1)"), "int call should inject: {out}");
        assert!(
            out.contains("f[str](\"x\")"),
            "str call should inject: {out}"
        );
    }

    #[test]
    fn bare_method_call_gets_inferred_specialization() {
        let out = transpile(
            indoc! {"
                class Box:
                    def m[T](self, t: T) -> None:
                        print(T)
                Box().m(1)
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("Box().m[int](1)"),
            "method call should inject: {out}"
        );
    }

    #[test]
    fn uninferable_bare_call_left_unchanged() {
        // `T` never appears in the signature — nothing to infer. ty reports
        // the call; the transform leaves it for the runtime backstop
        let out = transpile(
            indoc! {"
                def f[T](t: object):
                    print(T)
                f(1)
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("\nf(1)"),
            "uninferable call should stay bare: {out}"
        );
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
    fn default_on_312_is_an_error() {
        // pep 696 defaults are 3.13-only syntax, and a reified function can't
        // fall back to the erased polyfill, so 3.12 targets get a hard error
        let err = transpile(
            indoc! {"
                def f[T = int]():
                    print(T)
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap_err();
        assert!(
            err.contains("3.13"),
            "expected a 3.13 requirement error, got: {err}"
        );
    }

    #[test]
    fn default_on_313_wraps_natively() {
        let out = transpile(
            indoc! {"
                def f[T = int]():
                    print(T)
                f()
            "},
            &Config {
                min_version: PythonVersion::PY313,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("@generic  # basedpython: reified"),
            "should wrap: {out}"
        );
        assert!(
            out.contains("def f[T = int]():"),
            "native defaulted params kept: {out}"
        );
    }

    #[test]
    fn non_generic_function_untouched() {
        unchanged("def f(x):\n    return x\n");
    }

    #[test]
    fn wrapper_goes_innermost_below_user_decorators() {
        // the wrapper must receive the raw function, so it sits directly above
        // the `def`, below any user decorators
        let out = transpile(
            indoc! {"
                class C:
                    @staticmethod
                    def f[T]() -> None:
                        print(T)
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains(
                "    @staticmethod\n    @generic  # basedpython: reified\n    def f[T]() -> None:"
            ),
            "wrapper should be innermost: {out}"
        );
    }

    #[test]
    fn async_wrapper_sits_above_async_keyword() {
        let out = transpile(
            indoc! {"
                async def f[T]() -> None:
                    print(T)
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("@generic  # basedpython: reified\nasync def f[T]() -> None:"),
            "wrapper should precede the async header: {out}"
        );
    }

    #[test]
    fn classmethod_reified_is_an_error() {
        let err = transpile(
            indoc! {"
                class C:
                    @classmethod
                    def f[T](cls) -> None:
                        print(T)
            "},
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap_err();
        assert!(
            err.contains("classmethod `f` cannot have reified type parameters"),
            "expected a classmethod reification error, got: {err}"
        );
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
