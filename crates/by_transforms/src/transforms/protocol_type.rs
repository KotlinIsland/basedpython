//! Lowers basedpython inline protocol type expressions
//! `protocol(a: int; def f(self) -> int; **Kwargs)` to a synthesized
//! `typing.Protocol` subclass.
//!
//! Each unique shape (the rendered member list) is hoisted to a single class
//! definition and reused across all occurrences, so two structurally identical
//! inline protocols in a module collapse to one class — matching ty, where an
//! inline protocol is a structural type with no identity of its own.
//!
//! Example:
//! ```by
//! def f(x: protocol(a: int; def m(self) -> str)) -> None: ...
//! ```
//!
//! Lowers to:
//! ```python
//! from typing import Protocol
//!
//! class _Protocol_<hash>(Protocol):
//!     a: "int"
//!     def m(self) -> "str": ...
//!
//! def f(x: _Protocol_<hash>) -> None: ...
//! ```
//!
//! member types are emitted as forward references for the same reason
//! `callable`'s synthesized `__call__` protocols are: the class is hoisted
//! ahead of everything the module defines, so an unquoted annotation naming a
//! later class would be evaluated at class-body time and `NameError`

use std::fmt::Write as _;
use std::hash::Hash;

use ruff_python_ast::{Expr, ExprProtocolType, Stmt};
use ruff_text_size::Ranged;

use crate::type_info::TypeInfo;

use super::ast_driver::{PassContext, TypeAwarePass};
use super::repeated_underscore::{WrittenNames, protocol_method_receiver};
use super::synthesized_types::{Namer, SynthesizedKind};

/// One inline protocol shape: the rendered class-body lines, in source order.
///
/// Rendered text rather than the AST is what identifies a shape, so two
/// occurrences that spell the same members the same way share one class.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct Shape {
    members: Vec<String>,
}

impl Shape {
    /// the name of the class declaring this shape, one the module has no other use of
    pub(crate) fn class_name(&self, written: WrittenNames) -> String {
        written.synthesized_class("_Protocol_", self)
    }

    /// the class declaring this shape as `name`, `typing.Protocol` written as `protocol`
    pub(crate) fn class_def(&self, name: &str, protocol: &str) -> String {
        let mut out = format!("class {name}({protocol}):\n");
        if self.members.is_empty() {
            out.push_str("    pass\n");
            return out;
        }
        for member in &self.members {
            let _ = writeln!(out, "    {member}");
        }
        out
    }
}

/// The shape of an inline protocol, each member's type written as python, a nested inline
/// protocol or any other type the walk named before it as its class — at any depth, since the
/// name is a substitution on the shared lowerer, whose recursion reaches every leaf.
pub(crate) fn shape(namer: &mut Namer, protocol: &ExprProtocolType) -> Shape {
    let mut members = Vec::with_capacity(protocol.members.len());
    for member in &protocol.members {
        match member {
            Expr::Named(named) => {
                let Some(name) = named.target.as_name_expr() else {
                    continue;
                };
                let ty = render_type(namer, &named.value);
                let ty = namer.quotes.quote(&ty);
                // a member that only has to be readable is a getter-only property,
                // which is how a protocol class body says read-only
                members.push(if named.is_let {
                    let property = namer.written.builtin("property");
                    format!(
                        "@{property}\n    def {name}(self) -> {ty}: ...",
                        name = name.id
                    )
                } else {
                    format!("{name}: {ty}", name = name.id)
                });
            }
            Expr::ProtocolMethod(method) => match render_method(namer, method) {
                Some(rendered) => members.push(rendered),
                None => namer.errors.push(format!(
                    "inline protocol method `{}` has no parameter list",
                    method.name.id
                )),
            },
            // `**Kwargs` contributes no members: the pack's fields are only
            // known once the enclosing generic is specialized, and python
            // erases type arguments anyway. ty splices them in statically
            Expr::Starred(_) => {}
            // the parser only ever produces the three shapes above; this
            // keeps a future one from being silently dropped
            other => namer.errors.push(format!(
                "invalid inline protocol member `{}`",
                namer.src(other.range())
            )),
        }
    }
    Shape { members }
}

/// Lower a member's type expression to Python source.
fn render_type(namer: &mut Namer, expr: &Expr) -> String {
    let rendered = namer.lower(expr);
    namer.read_type_parameters(expr.range(), &rendered)
}

/// Render a method member as a `def` line, splitting off the receiver —
/// which is a parameter name rather than a type, and which the parser
/// marked as a label to keep it out of name resolution.
fn render_method(
    namer: &mut Namer,
    method: &ruff_python_ast::ExprProtocolMethod,
) -> Option<String> {
    let Expr::CallableType(signature) = method.signature.as_ref() else {
        return None;
    };
    let receiver = protocol_method_receiver(signature).map(|name| name.id.as_str());
    let params = namer.callable.render_protocol_params(
        signature,
        true,
        receiver.unwrap_or("self"),
        // a protocol method's receiver is its `self` parameter, never an implicit one
        None,
    );
    let returns = namer.callable.render_return(signature);
    let line = format!(
        "def {name}({params}) -> {returns}: ...",
        name = method.name.id,
        returns = namer.quotes.quote(&returns),
    );
    Some(namer.read_type_parameters(method.range(), &line))
}

/// replaces each inline protocol with the `Protocol` class it names, wherever it stands
pub(crate) struct ProtocolTypePass;

impl TypeAwarePass for ProtocolTypePass {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::ProtocolType)
    }

    /// an inline protocol it replaces moves into a class, whose member types the synthesized
    /// type namer prints
    fn subsumes(&self) -> &'static [super::ast_driver::Lowering] {
        super::synthesized_types::CLASS_BODY
    }

    fn run(&self, _stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        // the class each one names, and the declarations, come from the namer's one walk over
        // the module, which named every kind of synthesized type together
        for (range, name) in ctx.synthesized.of_kind(SynthesizedKind::Protocol) {
            ctx.text_edits.push((range, name));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    fn check_py312(input: &str, expected: &str) {
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
    fn data_and_method_members() {
        check(
            indoc! {"
                def f(x: protocol(a: int; def m(self) -> str)) -> None: ...
            "},
            indoc! {r#"
                from typing import Protocol
                class _Protocol_66269af6(Protocol):
                    a: "int"
                    def m(self) -> "str": ...

                def f(x: _Protocol_66269af6) -> None: ...
            "#},
        );
    }

    /// a member's type carries a type modifier as any other type does. `final` has no python
    /// spelling and is written as the type it marks, without the blanks the keyword was replaced
    /// with in the source, which a quoted type cannot start with
    #[test]
    fn a_type_modifier_on_a_member() {
        let out = transpile(
            "def f(x: protocol(a: final int; def m(self) -> final str)) -> None: ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("    a: \"int\"\n"), "got: {out}");
        assert!(
            out.contains("    def m(self) -> \"str\": ...\n"),
            "got: {out}"
        );
    }

    /// the synthesized class is written into the preamble and names `Protocol` as it is
    /// defined, so that import stays above it even when the module imports `Protocol` itself —
    /// folding it into the module's own line would leave the class with no such name
    #[test]
    fn a_module_importing_protocol_itself_still_gets_the_name_above_the_class() {
        check(
            indoc! {"
                from typing import Protocol

                def f(x: protocol(a: int)) -> None: ...
            "},
            indoc! {r#"
                from typing import Protocol
                class _Protocol_2abdf9ec(Protocol):
                    a: "int"

                from typing import Protocol

                def f(x: _Protocol_2abdf9ec) -> None: ...
            "#},
        );
    }

    /// a member is declared as a `def` in the synthesized class, where a repeated `_` is
    /// numbered and made positional-only as it is in any other `def`, and the receiver is
    /// the first of them
    #[test]
    fn a_repeated_underscore_in_a_method_member_is_numbered() {
        let out = transpile(
            "def f(x: protocol(def m(_, _: int, _: str) -> int)) -> None: ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def m(_, _2: \"int\", _3: \"str\", /) -> \"int\": ..."),
            "got:\n{out}"
        );
    }

    #[test]
    fn identical_shapes_share_one_class() {
        check(
            indoc! {"
                a: protocol(x: int)
                b: protocol(x: int)
            "},
            indoc! {r#"
                from typing import Protocol
                class _Protocol_6a4f11ec(Protocol):
                    x: "int"

                a: _Protocol_6a4f11ec
                b: _Protocol_6a4f11ec
            "#},
        );
    }

    /// a pack contributes no members at runtime — python erases type arguments,
    /// and ty splices the fields in statically
    #[test]
    fn keyword_pack_erases_to_an_empty_protocol() {
        check_py312(
            indoc! {"
                class A[**Kwargs]:
                    def get(self) -> protocol(**Kwargs): ...
            "},
            indoc! {"
                from typing import Protocol
                class _Protocol_58c79e45(Protocol):
                    pass

                class A[**Kwargs]:
                    def get(self) -> _Protocol_58c79e45: ...
            "},
        );
    }

    #[test]
    fn nested_protocol_member_names_the_inner_class() {
        check(
            indoc! {"
                a: protocol(inner: protocol(x: int))
            "},
            indoc! {r#"
                from typing import Protocol
                class _Protocol_6a4f11ec(Protocol):
                    x: "int"

                class _Protocol_7ea2762f(Protocol):
                    inner: "_Protocol_6a4f11ec"

                a: _Protocol_7ea2762f
            "#},
        );
    }

    /// the nested protocol must resolve at any depth, not only when it is the
    /// whole member type — a leak here hides inside a forward-reference string,
    /// so the final syntax check cannot catch it
    #[test]
    fn nested_protocol_resolves_inside_a_composite_member() {
        check(
            indoc! {"
                a: protocol(x: list[protocol(b: int)])
                c: protocol(x: protocol(b: int) | None)
                d: protocol(f: (protocol(b: int)) -> str)
            "},
            indoc! {r#"
                from typing import Callable, Protocol
                class _Protocol_9ac2fcf2(Protocol):
                    b: "int"

                class _Protocol_c1aa572a(Protocol):
                    x: "list[_Protocol_9ac2fcf2]"

                class _Protocol_534e77b4(Protocol):
                    x: "_Protocol_9ac2fcf2 | None"

                class _Protocol_7729cacb(Protocol):
                    f: "Callable[[_Protocol_9ac2fcf2], str]"

                a: _Protocol_c1aa572a
                c: _Protocol_534e77b4
                d: _Protocol_7729cacb
            "#},
        );
    }

    /// the polyfill renames `T` to `_T` at module scope, and the hoisted class
    /// body sits outside the scope that declared it
    #[test]
    fn typevar_member_uses_the_mangled_name() {
        check(
            indoc! {"
                class A[T]:
                    def get(self) -> protocol(a: T; def m(self, x: T) -> T): ...
            "},
            indoc! {r#"
                from typing import Protocol, TypeVar, Generic
                class _Protocol_72797bc7(Protocol):
                    a: "_T"
                    def m(self, x: "_T") -> "_T": ...

                _T = TypeVar("_T")
                class A(Generic[_T]):
                    def get(self) -> _Protocol_72797bc7: ...
            "#},
        );
    }

    /// kept as native syntax, `T` exists only in the scope of `A`, so the hoisted class reads it
    /// off `A`. a generic no path of names reaches from module scope — local to a function —
    /// has nothing to read it through
    #[test]
    fn a_native_type_parameter_is_read_off_its_generic() {
        let out = transpile(
            indoc! {"
                class A[T]:
                    def get(self) -> protocol(a: T; def m(self, x: T) -> T): ...
                    def put[U](self, p: protocol(u: U)) -> None: ...
                def f[V](p: protocol(v: V)) -> None: ...
                def g():
                    def h[W](p: protocol(w: W)) -> None: ...
            "},
            &Config {
                min_version: crate::PythonVersion::PY313,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("    a: \"A.__type_params__[0]\"\n")
                && out.contains(
                    "    def m(self, x: \"A.__type_params__[0]\") -> \"A.__type_params__[0]\": ...\n"
                )
                && out.contains("    u: \"A.put.__type_params__[0]\"\n")
                && out.contains("    v: \"f.__type_params__[0]\"\n")
                && out.contains("    w: \"W\"\n"),
            "got:\n{out}"
        );
    }

    /// an earlier generic declaring `T` differently takes `_T`, so the member names the one
    /// the polyfill declared for this generic's `T`
    #[test]
    fn typevar_member_uses_the_name_the_polyfill_declared() {
        check(
            indoc! {"
                class A[T: int]: ...
                class B[T]:
                    def get(self) -> protocol(a: T): ...
            "},
            indoc! {r#"
                from typing import Protocol, TypeVar, Generic
                class _Protocol_fd816244(Protocol):
                    a: "_T_1"

                _T = TypeVar("_T", bound=int)
                class A(Generic[_T]): ...
                _T_1 = TypeVar("_T_1")
                class B(Generic[_T_1]):
                    def get(self) -> _Protocol_fd816244: ...
            "#},
        );
    }

    #[test]
    fn a_let_member_is_a_getter_only_property() {
        check(
            indoc! {"
                def f(x: protocol(let a: int; b: str)) -> None: ...
            "},
            indoc! {r#"
                from typing import Protocol
                class _Protocol_4e1bfe56(Protocol):
                    @property
                    def a(self) -> "int": ...
                    b: "str"

                def f(x: _Protocol_4e1bfe56) -> None: ...
            "#},
        );
    }

    /// the synthesized class is written into the preamble and names `Protocol` as it is
    /// defined, so that import stays above it even when the module imports `Protocol` itself —
    /// folding it into the module's own line would leave the class with no such name
    /// a type alias's own parameter is declared by the polyfill too
    #[test]
    fn typevar_member_of_a_generic_alias() {
        let out = transpile("type G[T] = protocol(a: T)\n", &Config::test_default()).unwrap();
        assert!(out.contains("    a: \"_T\"\n"), "got:\n{out}");
    }

    /// a defaulted list is polyfilled on 3.12, which has native syntax for one without a
    /// default
    #[test]
    fn typevar_member_of_a_defaulted_list_on_312() {
        check_py312(
            indoc! {"
                class A[T = int]:
                    def get(self) -> protocol(a: T): ...
            "},
            indoc! {r#"
                from typing import Generic, Protocol
                from typing_extensions import TypeVar
                class _Protocol_ea950896(Protocol):
                    a: "_T"

                _T = TypeVar("_T", default=int)
                class A(Generic[_T]):
                    def get(self) -> _Protocol_ea950896: ...
            "#},
        );
    }

    #[test]
    fn method_parameter_spec_survives() {
        check(
            indoc! {"
                a: protocol(def f(self, x: int, /, *args: str, **kw: int) -> str)
            "},
            indoc! {r#"
                from typing import Protocol
                class _Protocol_84b8ccdc(Protocol):
                    def f(self, x: "int", /, *args: "str", **kw: "int") -> "str": ...

                a: _Protocol_84b8ccdc
            "#},
        );
    }

    /// basedpython type sugar inside a member lowers with the member, rather
    /// than leaking surface syntax into the hoisted class body
    #[test]
    fn member_types_lower() {
        check(
            indoc! {"
                a: protocol(x: int?; y: int & str; def f(self) -> (int) -> str)
            "},
            indoc! {r#"
                from ty_extensions import Intersection
                from typing import Callable, Protocol
                class _Protocol_366f1172(Protocol):
                    x: "int | None"
                    y: "Intersection[int, str]"
                    def f(self) -> "Callable[[int], str]": ...

                a: _Protocol_366f1172
            "#},
        );
    }

    /// an AST pass rewriting a node inside the `protocol(...)` this pass replaces
    /// whole — the driver's cleanup loop catches what is left, without duplicating
    /// the class
    #[test]
    fn survives_a_statement_re_rendered_by_another_pass() {
        check(
            indoc! {"
                d: dict[str, int] = {}
                x: protocol(a: typeof(d); def m(self, n: int) -> str) = ...
            "},
            indoc! {r#"
                from ty_extensions import TypeOf
                from typing import Protocol
                class _Protocol_ade07c7b(Protocol):
                    a: "TypeOf[d]"
                    def m(self, n: "int") -> "str": ...

                d: dict[str, int] = {}
                x: _Protocol_ade07c7b = ...
            "#},
        );
    }

    #[test]
    fn protocol_call_is_left_alone() {
        unchanged(indoc! {"
            protocol = dict
            a = protocol(x=1)
        "});
    }
}
