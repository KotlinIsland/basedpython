//! reified class type parameters (basedpython).
//!
//! a pep 695 type parameter of a class declared `reified`, or read by the class
//! in a value position — anywhere other than a type annotation — becomes a real
//! runtime value: the type argument the instance was constructed with.
//!
//! ```by
//! class A[T]:
//!     def f(self):
//!         print(T)
//!
//! a: A[int] = A()
//! a.f()
//! ```
//!
//! →
//!
//! ```python
//! @generic_class  # basedpython: reified
//! class A[T]:
//!     def f(self):
//!         T = _type_argument(self, "T")
//!         print(T)
//!
//! a: A[int] = A[int]()
//! a.f()
//! ```
//!
//! a *function's* type argument belongs to the call, so the `generic` wrapper
//! can rebuild the closure the body reads it from. a class's belongs to the
//! **instance**: two instances of the same class can carry different arguments,
//! and one function object serves both. so the lowering goes the other way
//! round — `A[int]` builds a subclass that records the arguments, and each
//! method reads its own from the receiver it already has.
//!
//! that makes the receiver the one thing a read needs. every function nested
//! inside a method closes over the binding the prologue writes, so a read at
//! any depth inside a method is answered; a read in the class body itself, in a
//! method's decorators or parameter defaults, or inside a class nested directly
//! in the class body happens where no instance exists, and is a hard error
//! rather than a value invented from nothing. a `staticmethod` is the same case
//! with a nicer name: it is called without a receiver, so it has nothing to ask.
//!
//! `A[int]` must reach the specializer, so the lowered `class` keeps its native
//! pep 695 header — the erased `Generic[T]` form renames the parameters and
//! drops `__type_params__` with them. that is python 3.12+ syntax, so
//! reification is gated on `min_version >= 3.12` and pep 696 defaults on 3.13,
//! exactly as a reified function is; below that the pass reports a hard error
//! rather than emit code that cannot run.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{PySourceType, PythonVersion, Stmt, StmtClassDef};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::reified::{UnansweredReason, reified_class_reads};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::reified_generic::REIFIED_MARKER;
use super::source_util::{PrologueStatement, body_prologue, line_indent, line_start};
use crate::type_info::TypeInfo;

/// the `generic_class` decorator, injected into the preamble when any class
/// reifies.
///
/// it replaces the class's `__class_getitem__`, so `A[int]` no longer builds a
/// `typing` alias but a memoized subclass of `A` carrying the type arguments —
/// which is what makes them readable from `__new__` and `__init__` onwards,
/// where an `__orig_class__` stamp applied after construction is not yet there.
/// being a real subclass also keeps `isinstance(a, A)` and `class B(A[int])`
/// working, neither of which survives an alias standing in for a class; the
/// specialization declares an empty `__slots__` so a slotted class stays slotted,
/// and `__init_subclass__` is held back for it, since it is the same class with
/// its arguments fixed rather than a subclass the program wrote.
///
/// each specialization composes what it binds with what its bases already bound
/// and resolves the chain, so `class B[U](A[U])` specialized as `B[int]` answers
/// `T` with `int` and not with `U`. `__orig_class__` is carried as a class
/// attribute, which is where the alias would have put it, so every reader of a
/// runtime specialization — `_parametric_is` included — sees the same thing it
/// saw before.
///
/// `_type_argument` answers one read. it takes the receiver rather than the
/// class so a `classmethod` can pass `cls` and everything else `self`, and it
/// raises rather than returning the `TypeVar` object the parameter would
/// otherwise still name — whether because nothing specialized the class or
/// because a base's argument was never filled in
pub(crate) const GENERIC_CLASS_RUNTIME: &str = "\
def generic_class(cls):
    cls.__class_getitem__ = classmethod(_specialize)
    return cls


def _specialize(cls, item):
    args = item if isinstance(item, tuple) else (item,)
    if \"__by_type_arguments__\" in cls.__dict__:
        raise TypeError(f\"{cls.__name__} is already specialized\")
    cache = cls.__dict__.get(\"__by_specializations__\")
    if cache is None:
        cache = {}
        cls.__by_specializations__ = cache
    try:
        made = cache.get(args)
    except TypeError:
        raise TypeError(
            f\"a type argument to {cls.__name__} is not hashable, so the \"
            f\"specialization it names cannot be built\"
        ) from None
    if made is not None:
        return made
    params = cls.__type_params__
    bound = {}
    for base in reversed(cls.__mro__):
        bound.update(base.__dict__.get(\"__by_type_arguments__\") or {})
    bound.update(_bind_type_params(params, args, {}, cls.__name__))
    for param in params:
        if param.__name__ not in bound:
            raise TypeError(
                f\"too few type arguments for {cls.__name__}: \"
                f\"no argument for {param.__name__!r}\"
            )
    for name, value in bound.items():
        seen = {name}
        while isinstance(value, (TypeVar, TypeVarTuple)) and value.__name__ in bound:
            if value.__name__ in seen:
                break
            seen.add(value.__name__)
            value = bound[value.__name__]
        bound[name] = value
    namespace = {
        \"__by_type_arguments__\": bound,
        \"__orig_class__\": GenericAlias(cls, args),
        # the specialization declares nothing of its own, so a slotted class
        # stays slotted instead of gaining a `__dict__` here
        \"__slots__\": (),
    }
    # a specialization is the same class with its arguments fixed, not a
    # subclass the program wrote, so the hook that greets a subclass must not
    # run for it: it would be handed neither the class keywords the definition
    # was given nor a class anybody declared
    saved = cls.__dict__.get(\"__init_subclass__\", _by_absent)
    cls.__init_subclass__ = classmethod(lambda cls, **kwargs: None)
    try:
        made = type(cls)(cls.__name__, (cls,), namespace)
    except TypeError as exc:
        # a metaclass that takes class-creation keywords cannot be given them
        # again: nothing records what the definition was written with
        raise TypeError(
            f\"cannot build a specialization of {cls.__name__}: {exc}\"
        ) from exc
    finally:
        if saved is _by_absent:
            del cls.__init_subclass__
        else:
            cls.__init_subclass__ = saved
    made.__module__ = cls.__module__
    made.__qualname__ = cls.__qualname__
    cache[args] = made
    return made


def _type_argument(owner, name):
    cls = owner if isinstance(owner, type) else type(owner)
    bound = getattr(cls, \"__by_type_arguments__\", None)
    value = _by_absent if bound is None else bound.get(name, _by_absent)
    # a value still standing as a type parameter is a base's argument that
    # nothing filled in, which means the instance came from the bare class
    if value is _by_absent or isinstance(value, (TypeVar, TypeVarTuple)):
        raise TypeError(
            f\"{cls.__name__} has no type argument for {name!r}: it was not \"
            f\"constructed from a specialization\"
        )
    return value
";

/// one reified type parameter bound from the receiver at the top of a method
struct TypeArgumentBinding {
    name: String,
    receiver: String,
}

impl PrologueStatement for TypeArgumentBinding {
    fn push(&self, frags: &mut Vec<Fragment>, _indent: &str) {
        let Self { name, receiver } = self;
        frags.push(Fragment::Lit(format!(
            "{name} = _type_argument({receiver}, \"{name}\")"
        )));
    }
}

struct ReifiedClass<'src> {
    source: &'src str,
    /// zero-width insertions of the `@generic_class` decorator line
    edits: Vec<(TextRange, String)>,
    /// the parameter bindings, anchored at the body statement they precede
    prologues: Vec<(TextSize, Vec<Fragment>)>,
    /// at least one class reified — emit the polyfill and its imports
    used: bool,
    /// a reified class was found but the target is below 3.12
    below_312: Vec<String>,
    /// a reified class has a pep 696 default but the target is below 3.13
    defaulted_below_313: Vec<String>,
    /// `(class, parameter, why)` for a read nothing can answer
    unanswerable: Vec<(String, String, UnansweredReason)>,
    /// a reified class that answers `[...]` itself
    own_class_getitem: Vec<String>,
    /// methods whose body starts with parser-synthesized statements, so a
    /// binding has no source position to anchor to
    unanchored: Vec<String>,
    supports_native_generics: bool,
    supports_param_defaults: bool,
}

impl<'src> ReifiedClass<'src> {
    fn new(source: &'src str, min_version: PythonVersion) -> Self {
        Self {
            source,
            edits: Vec::new(),
            prologues: Vec::new(),
            used: false,
            below_312: Vec::new(),
            defaulted_below_313: Vec::new(),
            unanswerable: Vec::new(),
            own_class_getitem: Vec::new(),
            unanchored: Vec::new(),
            supports_native_generics: min_version >= PythonVersion::PY312,
            supports_param_defaults: min_version >= PythonVersion::PY313,
        }
    }

    fn specialize(&mut self, class: &StmtClassDef) {
        let reads = reified_class_reads(self.source, PySourceType::BasedPython, class);
        if reads.names.is_empty() {
            return;
        }
        let name = class.name.id.as_str();
        if !self.supports_native_generics {
            self.below_312.push(name.to_owned());
            return;
        }
        // any default in the list makes the native header 3.13-only syntax,
        // even on parameters that don't themselves reify
        if !self.supports_param_defaults
            && class
                .type_params
                .as_deref()
                .is_some_and(|tp| tp.type_params.iter().any(|p| p.default().is_some()))
        {
            self.defaulted_below_313.push(name.to_owned());
            return;
        }
        if reads.own_class_getitem.is_some() {
            self.own_class_getitem.push(name.to_owned());
            return;
        }
        if !reads.unanswerable.is_empty() {
            for read in &reads.unanswerable {
                self.unanswerable
                    .push((name.to_owned(), read.name.to_string(), read.reason));
            }
            return;
        }

        for method in &reads.methods {
            let bindings: Vec<TypeArgumentBinding> = method
                .names
                .iter()
                .map(|parameter| TypeArgumentBinding {
                    name: parameter.to_string(),
                    receiver: method.receiver.to_owned(),
                })
                .collect();
            match body_prologue(self.source, method.function, &bindings) {
                Some(anchored) => self.prologues.push(anchored),
                None => self.unanchored.push(method.function.name.id.to_string()),
            }
        }

        // the decorator goes on its own line directly above the `class` header
        // — innermost, below any user decorators — sharing its indentation.
        // anchored via the class name, whose line is the header
        let name_start = class.name.range().start();
        let indent = line_indent(self.source, name_start);
        let anchor = line_start(self.source, name_start) + TextSize::of(indent);
        self.edits.push((
            TextRange::empty(anchor),
            format!("@generic_class{REIFIED_MARKER}\n{indent}"),
        ));
        self.used = true;
    }
}

impl<'ast> Visitor<'ast> for ReifiedClass<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.specialize(class);
        }
        walk_stmt(self, stmt);
    }
}

pub(crate) struct ReifiedClassPass<'src> {
    source: &'src str,
    min_version: PythonVersion,
    is_stub: bool,
}

impl<'src> ReifiedClassPass<'src> {
    pub(crate) fn new(source: &'src str, min_version: PythonVersion, is_stub: bool) -> Self {
        Self {
            source,
            min_version,
            is_stub,
        }
    }
}

impl TypeAwarePass for ReifiedClassPass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        // a stub describes a runtime that lives elsewhere; there is no
        // specialization to build here, and the decorator would name a
        // polyfill the stub never carries
        if self.is_stub {
            return;
        }
        let mut inner = ReifiedClass::new(self.source, self.min_version);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if let Some(name) = inner.below_312.first() {
            ctx.errors.push(format!(
                "reified generic class `{name}` requires python 3.12 or newer: a \
                 specialization is built from the class's own type-parameter list, \
                 which needs native type-parameter syntax in the generated python"
            ));
            return;
        }
        if let Some(name) = inner.defaulted_below_313.first() {
            ctx.errors.push(format!(
                "reified generic class `{name}` has a type-parameter default, which \
                 requires python 3.13 or newer: reification keeps the native pep 695 \
                 parameter list in the generated python, and pep 696 defaults are not \
                 valid syntax before 3.13"
            ));
            return;
        }
        if let Some(class) = inner.own_class_getitem.first() {
            ctx.errors.push(format!(
                "reified generic class `{class}` cannot define `__class_getitem__`: \
                 `{class}[...]` is what records the type arguments an instance carries, \
                 so the class cannot answer that subscript itself"
            ));
            return;
        }
        if let Some((class, parameter, reason)) = inner.unanswerable.first() {
            let why = match reason {
                UnansweredReason::OutsideMethod => {
                    "it is read outside any method, where no instance exists yet"
                }
                UnansweredReason::WithoutReceiver => {
                    "the method that reads it is called without a receiver"
                }
            };
            ctx.errors.push(format!(
                "type parameter `{parameter}` of class `{class}` cannot be read: {why}. \
                 a class's type argument belongs to the instance that carries it, so a \
                 method reads it through its receiver"
            ));
            return;
        }
        if let Some(name) = inner.unanchored.first() {
            ctx.errors.push(format!(
                "`{name}` has a body nothing in the source anchors, so a type-argument \
                 binding has nowhere to go"
            ));
            return;
        }
        if inner.used {
            ctx.required_imports
                .push("from types import GenericAlias".to_owned());
            ctx.required_imports
                .push("_by_absent = object()".to_owned());
            ctx.required_imports
                .push("from typing import ParamSpec, TypeVar, TypeVarTuple".to_owned());
            ctx.required_imports
                .push(super::reified_generic::BIND_TYPE_PARAMS_RUNTIME.to_owned());
            ctx.required_imports.push(GENERIC_CLASS_RUNTIME.to_owned());
        }
        ctx.text_edits.extend(inner.edits);
        ctx.statement_inserts.extend(inner.prologues);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn at(version: PythonVersion, input: &str) -> String {
        transpile(
            input,
            &Config {
                min_version: version,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    fn lowered(input: &str) -> String {
        at(PythonVersion::PY312, input)
    }

    fn error(input: &str) -> String {
        transpile(
            input,
            &Config {
                min_version: PythonVersion::PY312,
                ..Config::test_default()
            },
        )
        .unwrap_err()
    }

    #[test]
    fn value_position_read_binds_from_the_receiver() {
        let out = lowered(indoc! {"
            class A[T]:
                def f(self):
                    print(T)
        "});
        assert!(
            out.contains("@generic_class  # basedpython: reified\nclass A[T]:"),
            "the class should be decorated: {out}"
        );
        assert!(
            out.contains(
                "    def f(self):\n        T = _type_argument(self, \"T\")\n        print(T)"
            ),
            "the read should be bound from the receiver: {out}"
        );
    }

    #[test]
    fn a_single_line_body_gets_its_own_line() {
        let out = lowered(indoc! {"
            class A[T]:
                def f(self): return T
        "});
        assert!(
            out.contains(
                "    def f(self): \n        T = _type_argument(self, \"T\")\n        return T"
            ),
            "the body should move below the binding: {out}"
        );
    }

    #[test]
    fn a_docstring_keeps_its_place() {
        let out = lowered(indoc! {r#"
            class A[T]:
                def f(self):
                    "what f does"
                    return T
        "#});
        assert!(
            out.contains(
                "        \"what f does\"\n        T = _type_argument(self, \"T\")\n        return T"
            ),
            "the binding should follow the docstring: {out}"
        );
    }

    #[test]
    fn a_classmethod_binds_from_cls() {
        let out = lowered(indoc! {"
            class A[T]:
                @classmethod
                def f(cls):
                    return T
        "});
        assert!(
            out.contains("T = _type_argument(cls, \"T\")"),
            "a classmethod reads through its class: {out}"
        );
    }

    #[test]
    fn every_parameter_a_method_reads_is_bound() {
        let out = lowered(indoc! {"
            class A[T, U]:
                def f(self):
                    return (U, T)
        "});
        assert!(
            out.contains(
                "        T = _type_argument(self, \"T\")\n        U = _type_argument(self, \"U\")\n"
            ),
            "both parameters bind, in declaration order: {out}"
        );
    }

    #[test]
    fn a_read_only_a_nested_function_makes_binds_in_the_method() {
        // the nested function closes over what the method bound, so the binding
        // belongs to the method that has the receiver
        let out = lowered(indoc! {"
            class A[T]:
                def f(self):
                    def inner():
                        return T
                    return inner()
        "});
        assert!(
            out.contains(
                "    def f(self):\n        T = _type_argument(self, \"T\")\n        def inner():"
            ),
            "the binding goes in the method, not the nested function: {out}"
        );
        assert_eq!(out.matches("= _type_argument(").count(), 1, "{out}");
    }

    #[test]
    fn an_annotation_only_parameter_is_left_alone() {
        unchanged(indoc! {"
            class A[T]:
                value: T

                def get(self) -> T:
                    return self.value
        "});
    }

    #[test]
    fn a_declared_parameter_wraps_with_no_binding_to_make() {
        let out = lowered(indoc! {"
            class A[reified T]:
                pass
        "});
        assert!(
            out.contains("@generic_class  # basedpython: reified"),
            "a declared parameter reifies the class on its own: {out}"
        );
        assert!(
            !out.contains("= _type_argument("),
            "nothing reads it, so nothing binds it: {out}"
        );
    }

    #[test]
    fn a_specialized_construction_reaches_the_specializer() {
        let out = lowered(indoc! {"
            class A[T]:
                def f(self):
                    return T

            a: A[int] = A()
        "});
        assert!(
            out.contains("a: A[int] = A[int]()"),
            "the construction should name the specialization: {out}"
        );
    }

    #[test]
    fn a_read_in_the_class_body_is_an_error() {
        assert!(
            error(indoc! {"
                class A[T]:
                    kind = T
            "})
            .contains("cannot be read: it is read outside any method"),
            "the class body has no instance to read from"
        );
    }

    #[test]
    fn a_static_method_read_is_an_error() {
        assert!(
            error(indoc! {"
                class A[T]:
                    @staticmethod
                    def f():
                        return T
            "})
            .contains("the method that reads it is called without a receiver"),
            "a static method is handed no receiver"
        );
    }

    #[test]
    fn a_method_written_inside_a_block_keeps_its_receiver() {
        let out = lowered(indoc! {"
            import sys

            class A[T]:
                if sys.version_info >= (3, 8):
                    def f(self):
                        return T
        "});
        assert!(
            out.contains("            T = _type_argument(self, \"T\")"),
            "a guarded method binds like any other: {out}"
        );
    }

    #[test]
    fn a_global_declaration_is_not_a_read() {
        // the name belongs to the module for the whole body, and python rejects
        // a binding written above the declaration — which is where one would go
        unchanged(indoc! {"
            T = 1

            class A[T]:
                def f(self):
                    global T
                    return T
        "});
    }

    #[test]
    fn a_class_that_answers_its_own_subscript_is_an_error() {
        assert!(
            error(indoc! {"
                class A[T]:
                    def __class_getitem__(cls, item):
                        return cls

                    def f(self):
                        return T
            "})
            .contains("cannot define `__class_getitem__`"),
            "the specialization has nowhere to record its arguments"
        );
    }

    #[test]
    fn below_312_is_an_error() {
        let message = transpile(
            indoc! {"
                class A[T]:
                    def f(self):
                        return T
            "},
            &Config {
                min_version: PythonVersion::PY311,
                ..Config::test_default()
            },
        )
        .unwrap_err();
        assert!(
            message.contains("requires python 3.12 or newer"),
            "the specializer reads the native parameter list: {message}"
        );
    }

    #[test]
    fn a_defaulted_parameter_below_313_is_an_error() {
        assert!(
            error(indoc! {"
                class A[T = int]:
                    def f(self):
                        return T
            "})
            .contains("requires python 3.13 or newer"),
            "pep 696 defaults are not valid syntax before 3.13"
        );
    }

    #[test]
    fn a_defaulted_parameter_on_313_lowers() {
        let out = at(
            PythonVersion::PY313,
            indoc! {"
                class A[T = int]:
                    def f(self):
                        return T
            "},
        );
        assert!(
            out.contains("class A[T = int]:"),
            "the native parameter list survives: {out}"
        );
    }
}
