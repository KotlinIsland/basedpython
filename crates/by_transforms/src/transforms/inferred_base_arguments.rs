//! type-aware pass: write the type arguments a generic base takes from the class body
//! overriding its members
//!
//! ```by
//! class A[T]:
//!     x: T
//!
//! class B(A):
//!     x = 1
//! ```
//!
//! lowers `class B(A)` to `class B(A[int])`. the type checker solves the arguments from the
//! members `B` overrides; python has no such rule, so the lowering writes them where a python
//! checker reads them
//!
//! a base is evaluated as the class is made, so an argument that would not resolve there — one
//! naming a module the source never imports as it runs, the class being made, or a class the
//! module binds further down — is written as a string, which a generic class's subscript accepts
//! as a forward reference

use std::collections::{BTreeSet, HashMap};

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Stmt, StmtClassDef};
use ruff_text_size::TextRange;

use super::generics::PolyfilledTypeParams;
use super::source_util::rename_identifiers;
use crate::Config;
use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::repeated_underscore::WrittenNames;
use crate::type_info::{SpelledBaseArgument, TypeInfo};

pub(crate) struct InferredBaseArgumentsPass<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    config: Config,
}

impl<'src> InferredBaseArgumentsPass<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>, config: Config) -> Self {
        Self {
            source,
            written,
            config,
        }
    }
}

impl TypeAwarePass for InferredBaseArgumentsPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut state = State {
            types,
            source: self.source,
            written: self.written,
            min_version: self.config.min_version,
            polyfilled: PolyfilledTypeParams::decide(
                self.source,
                self.written,
                stmts,
                &self.config,
            ),
            edits: Vec::new(),
            errors: Vec::new(),
            modules: BTreeSet::new(),
            imports: BTreeSet::new(),
        };
        for stmt in stmts {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits);
        ctx.errors.extend(state.errors);
        ctx.type_only_imports.extend(state.modules);
        ctx.required_imports.extend(state.imports);
    }
}

struct State<'a> {
    types: &'a dyn TypeInfo,
    source: &'a str,
    written: WrittenNames<'a>,
    min_version: ruff_python_ast::PythonVersion,
    /// the type parameters the generics polyfill moves to module-level `TypeVar`s, under names
    /// of its own
    polyfilled: PolyfilledTypeParams,
    edits: Vec<(TextRange, String)>,
    errors: Vec<String>,
    /// the modules an argument qualifies a class with that the source never imported under their
    /// own names
    modules: BTreeSet<String>,
    /// the imports of the `typing` names an argument reads, and of `Unpack`
    imports: BTreeSet<String>,
}

impl State<'_> {
    fn process_class(&mut self, class: &StmtClassDef) {
        // a class's own type parameters, which an argument may name, are renamed by the
        // polyfill when it moves them to the module
        let renames: HashMap<String, String> = class
            .type_params
            .as_deref()
            .and_then(|type_params| self.polyfilled.renames(type_params))
            .unwrap_or_default();
        let renames: HashMap<&str, &str> = renames
            .iter()
            .map(|(from, to)| (from.as_str(), to.as_str()))
            .collect();

        for (base, arguments) in self
            .types
            .inferred_base_type_arguments(class, self.min_version)
        {
            // the checker inherits these arguments; leaving them out would give a python
            // checker a different base
            let Some(arguments) = arguments else {
                self.errors.push(format!(
                    "the type arguments `{}` inherits `{}` at cannot be written where its base is",
                    class.name, &self.source[base],
                ));
                continue;
            };
            let spelled: Vec<String> = arguments
                .iter()
                .map(|argument| match argument {
                    SpelledBaseArgument::Type(ty) => ty.annotation_in(self.written, true),
                    SpelledBaseArgument::Unpacked(ty) => {
                        self.imports
                            .insert(self.written.import_from("typing", &["Unpack"]));
                        format!(
                            "{}[{}]",
                            self.written.imported("typing", "Unpack"),
                            ty.annotation_in(self.written, true)
                        )
                    }
                })
                .map(|text| rename_identifiers(&text, &renames))
                .collect();
            self.edits.push((
                TextRange::empty(base.end()),
                format!("[{}]", spelled.join(", ")),
            ));
            for argument in &arguments {
                let (SpelledBaseArgument::Type(ty) | SpelledBaseArgument::Unpacked(ty)) = argument;
                self.modules.extend(ty.modules.iter().cloned());
                self.imports.extend(ty.typing_imports_in(self.written));
            }
        }
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.process_class(class);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    /// on 3.12, where the generic classes keep their own type parameter lists
    fn check(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn an_attribute_override_writes_the_argument() {
        check(
            indoc! {"
                class A[T]:
                    x: T

                class B(A):
                    x = 1
            "},
            indoc! {"
                class A[T]:
                    x: T

                class B(A[int]):
                    x: int = 1
            "},
        );
    }

    #[test]
    fn a_written_argument_is_left_alone() {
        check(
            indoc! {"
                class A[T]:
                    x: T

                class B(A[object]):
                    x = 1
            "},
            indoc! {"
                class A[T]:
                    x: T

                class B(A[object]):
                    x: int = 1
            "},
        );
    }

    #[test]
    fn several_arguments_are_written_in_declaration_order() {
        check(
            indoc! {"
                class C[K, V]:
                    def get(self, key: K) -> V:
                        raise NotImplementedError

                class D(C):
                    override def get(self, key: int) -> str:
                        return ''
            "},
            indoc! {"
                from typing import override
                class C[K, V]:
                    def get(self, key: K) -> V:
                        raise NotImplementedError

                class D(C[int, str]):
                    @override
                    def get(self, key: int) -> str:
                        return ''
            "},
        );
    }

    /// a `*Ts` of a fixed number of types is written as that many arguments, and one of no
    /// fixed number unpacked
    #[test]
    fn a_variadic_argument_is_written_by_its_length() {
        check(
            indoc! {"
                class F[*Ts]:
                    def f(self) -> tuple[*Ts]:
                        raise NotImplementedError

                class Two(F):
                    override def f(self) -> tuple[int, str]:
                        return (1, '')

                class Many(F):
                    override def f(self) -> tuple[int, ...]:
                        return (1,)
            "},
            indoc! {"
                from typing import Unpack, override
                class F[*Ts]:
                    def f(self) -> tuple[*Ts]:
                        raise NotImplementedError

                class Two(F[int, str]):
                    @override
                    def f(self) -> tuple[int, str]:
                        return (1, '')

                class Many(F[Unpack[tuple[int, ...]]]):
                    @override
                    def f(self) -> tuple[int, ...]:
                        return (1,)
            "},
        );
    }

    /// a base is evaluated before the class exists and before anything the module binds further
    /// down, so an argument naming either is a forward reference
    #[test]
    fn an_argument_not_yet_bound_is_a_string() {
        check(
            indoc! {"
                class A[T]:
                    x: T

                class Early: ...

                class D(A):
                    class Inner: ...
                    x: Inner

                class E(A):
                    x: Later

                class F(A):
                    x: Early

                class Later: ...
            "},
            indoc! {r#"
                class A[T]:
                    x: T

                class Early: ...

                class D(A["D.Inner"]):
                    class Inner: ...
                    x: Inner

                class E(A["Later"]):
                    x: "Later"

                class F(A[Early]):
                    x: Early

                class Later: ...
            "#},
        );
    }

    /// a generic subclass names its own type parameter, under the name the polyfill declares it
    /// by where the type parameter list is lowered
    #[test]
    fn a_generic_subclass_names_its_own_type_parameter() {
        let source = indoc! {"
            class A[T]:
                x: T

            class H[U](A):
                x: U
        "};
        check(
            source,
            indoc! {"
                class A[T]:
                    x: T

                class H[U](A[U]):
                    x: U
            "},
        );
        let lowered = transpile(source, &Config::test_default()).unwrap();
        assert!(
            lowered.contains("class H(A[_U], Generic[_U]):"),
            "{lowered}"
        );
    }

    /// the checker inherits the arguments, so a base whose arguments python cannot write where
    /// the base is — a class local to a function — is an error rather than a different base
    #[test]
    fn an_argument_python_cannot_write_is_an_error() {
        let error = transpile(
            indoc! {"
                class A[T]:
                    x: T

                def make():
                    class Local: ...

                    class B(A):
                        x: Local

                    return B
            "},
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(error.contains("cannot be written"), "{error}");
    }
}
