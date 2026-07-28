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

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use std::collections::HashMap;

use indexmap::IndexMap;
use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, ExprProtocolType, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::TypeInfo;

use super::ast_driver::{PassContext, TypeAwarePass};
use super::callable::CallableSyntax;
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions_skipping};

/// One inline protocol shape: the rendered class-body lines, in source order.
///
/// Rendered text rather than the AST is what identifies a shape, so two
/// occurrences that spell the same members the same way share one class.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct Shape {
    members: Vec<String>,
}

impl Shape {
    fn class_name(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        #[expect(clippy::cast_possible_truncation)]
        let truncated = hasher.finish() as u32;
        format!("_Protocol_{truncated:08x}")
    }

    fn class_def(&self, name: &str) -> String {
        let mut out = format!("class {name}(Protocol):\n");
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

pub(crate) struct ProtocolTypePass<'src> {
    source: &'src str,
    config: crate::Config,
}

impl<'src> ProtocolTypePass<'src> {
    pub(crate) fn new(source: &'src str, config: crate::Config) -> Self {
        Self { source, config }
    }
}

struct ProtocolTypeLowering<'src> {
    source: &'src str,
    /// One lowerer drives every member type in the run, so its imports and the
    /// `_Callable_*` classes it synthesizes can be collected at the end. The
    /// `callable` pass deliberately does not descend into an inline protocol,
    /// so nothing else emits them
    callable: CallableSyntax<'src>,
    edits: Vec<Fix>,
    errors: Vec<String>,
    /// Insertion-ordered so a class is emitted before any later class whose
    /// members mention it (the visitor rewrites nested protocols first)
    shapes: IndexMap<Shape, String>,
    /// Typevar renames in effect at each generic scope. Python < 3.12 has no
    /// native PEP 695, so the generics polyfill renames `T` to `_T` at module
    /// scope — a hoisted class body naming the original would resolve to nothing
    typevar_scopes: Vec<(TextRange, HashMap<String, String>)>,
    needs_import: bool,
}

impl<'src> ProtocolTypeLowering<'src> {
    fn new(source: &'src str, types: &'src dyn TypeInfo, claimed: &'src [TextRange]) -> Self {
        Self {
            source,
            callable: CallableSyntax::new(source)
                .with_types(types)
                .with_claimed_ranges(claimed),
            edits: Vec::new(),
            errors: Vec::new(),
            shapes: IndexMap::new(),
            typevar_scopes: Vec::new(),
            needs_import: false,
        }
    }

    fn class_defs(&self) -> String {
        let mut out = String::new();
        for (shape, name) in &self.shapes {
            out.push_str(&shape.class_def(name));
            out.push('\n');
        }
        out
    }

    /// Lower a member's type expression to Python source.
    ///
    /// A nested inline protocol resolves to the class it was assigned — at any
    /// depth, since the substitution is registered on the shared lowerer, whose
    /// recursion reaches every leaf.
    fn render_type(&mut self, expr: &Expr) -> String {
        let rendered = self.callable.lower_type_expr(expr);
        self.apply_typevar_renames(expr.range(), &rendered)
    }

    /// Substitute every whole-word typevar reference in `rendered` with the
    /// mangled module-scope name the generics polyfill gives it, for the scopes
    /// enclosing `range`.
    fn apply_typevar_renames(&self, range: TextRange, rendered: &str) -> String {
        let mut renames: HashMap<&str, &str> = HashMap::new();
        for (scope, frame) in &self.typevar_scopes {
            if scope.contains_range(range) {
                renames.extend(frame.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            }
        }
        if renames.is_empty() {
            return rendered.to_owned();
        }
        super::source_util::rename_identifiers(rendered, &renames)
    }

    /// Render a method member as a `def` line, splitting off the receiver —
    /// which is a parameter name rather than a type, and which the parser
    /// marked as a label to keep it out of name resolution.
    fn render_method(&mut self, method: &ruff_python_ast::ExprProtocolMethod) -> Option<String> {
        let Expr::CallableType(signature) = method.signature.as_ref() else {
            return None;
        };
        let receiver = signature.args.first().and_then(|first| match first {
            Expr::Name(name) if name.ctx.is_invalid() => Some(name.id.as_str()),
            _ => None,
        });
        let offset = usize::from(receiver.is_some());
        let shift = |index: Option<u32>| index.map(|i| (i as usize).saturating_sub(offset));

        let params = self.callable.render_protocol_params(
            &signature.args[offset..],
            shift(signature.parameter_slash()),
            shift(signature.parameter_star()),
            receiver.unwrap_or("self"),
            // a protocol method's receiver is its `self` parameter, never an implicit one
            None,
        );
        let returns = self.callable.lower_type_expr(&signature.returns);
        let line = format!(
            "def {name}({params}) -> {returns}: ...",
            name = method.name.id,
            returns = quote_forward_ref(&returns),
        );
        Some(self.apply_typevar_renames(method.range(), &line))
    }

    fn extract_shape(&mut self, protocol: &ExprProtocolType) -> Shape {
        let mut members = Vec::with_capacity(protocol.members.len());
        for member in &protocol.members {
            match member {
                Expr::Named(named) => {
                    let Some(name) = named.target.as_name_expr() else {
                        continue;
                    };
                    let ty = self.render_type(&named.value);
                    members.push(format!(
                        "{name}: {ty}",
                        name = name.id,
                        ty = quote_forward_ref(&ty)
                    ));
                }
                Expr::ProtocolMethod(method) => match self.render_method(method) {
                    Some(rendered) => members.push(rendered),
                    None => self.errors.push(format!(
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
                other => self.errors.push(format!(
                    "invalid inline protocol member `{}`",
                    self.source
                        .get(std::ops::Range::<usize>::from(other.range()))
                        .unwrap_or("<unknown>")
                )),
            }
        }
        Shape { members }
    }

    fn rewrite(&mut self, protocol: &ExprProtocolType) {
        let shape = self.extract_shape(protocol);
        let name = if let Some(existing) = self.shapes.get(&shape) {
            existing.clone()
        } else {
            let name = shape.class_name();
            self.shapes.insert(shape, name.clone());
            name
        };
        // register on the shared lowerer so an enclosing member type resolves the
        // nested protocol wherever it sits — inside a subscript, a union, a
        // callable arrow — not only when it is the whole member type
        self.callable
            .add_substitution(protocol.range(), name.clone());
        self.needs_import = true;
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            name,
            protocol.range(),
        )));
    }

    /// Rewrite every inline protocol inside `expr`, innermost first, so an
    /// enclosing protocol's members can name the class a nested one produced.
    fn visit_nested(&mut self, expr: &Expr) {
        match expr {
            Expr::ProtocolType(protocol) => {
                for member in &protocol.members {
                    self.visit_nested(member);
                }
                self.rewrite(protocol);
            }
            Expr::ProtocolMethod(method) => self.visit_nested(&method.signature),
            Expr::CallableType(callable) => {
                for arg in &callable.args {
                    self.visit_nested(arg);
                }
                self.visit_nested(&callable.returns);
            }
            Expr::Named(named) => self.visit_nested(&named.value),
            Expr::BinOp(binop) => {
                self.visit_nested(&binop.left);
                self.visit_nested(&binop.right);
            }
            Expr::BoolOp(boolop) => {
                for value in &boolop.values {
                    self.visit_nested(value);
                }
            }
            Expr::UnaryOp(unary) => self.visit_nested(&unary.operand),
            Expr::Subscript(subscript) => self.visit_nested(&subscript.slice),
            Expr::Starred(starred) => self.visit_nested(&starred.value),
            Expr::Tuple(tuple) => {
                for elt in &tuple.elts {
                    self.visit_nested(elt);
                }
            }
            Expr::List(list) => {
                for elt in &list.elts {
                    self.visit_nested(elt);
                }
            }
            _ => {}
        }
    }
}

impl TypeExprVisitor for ProtocolTypeLowering<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // `visit_nested` is a deep rewriter that already knows every container
        // an inline protocol can hide in, so the walker must not descend and
        // process the same node twice
        self.visit_nested(expr);
        Recurse::Stop
    }
}

/// Wrap a rendered member type in a forward-reference string.
fn quote_forward_ref(ty: &str) -> String {
    format!("\"{}\"", ty.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Lowers inline protocols that appear in a value position (`x = protocol(a:
/// int)`). `protocol(...)` is never valid python wherever it appears, so it has
/// to be replaced everywhere, not just in the annotation slots the type-position
/// walker visits.
struct ValueProtocolWalker<'a, 'src> {
    inner: &'a mut ProtocolTypeLowering<'src>,
}

impl<'ast> ruff_python_ast::visitor::Visitor<'ast> for ValueProtocolWalker<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if matches!(expr, Expr::ProtocolType(_)) {
            self.inner.visit_nested(expr);
            return;
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

/// Collects the typevar renames the PEP 695 polyfill will apply, keyed by the
/// range of the generic scope that declares them.
struct TypevarScopeWalker<'a> {
    config: crate::Config,
    scopes: &'a mut Vec<(TextRange, HashMap<String, String>)>,
}

impl TypevarScopeWalker<'_> {
    fn record(&mut self, range: TextRange, type_params: Option<&ruff_python_ast::TypeParams>) {
        // python 3.12+ keeps PEP 695 native, so nothing is renamed
        if self.config.min_version >= ruff_python_ast::PythonVersion::PY312 {
            return;
        }
        let Some(type_params) = type_params else {
            return;
        };
        let frame: HashMap<String, String> = type_params
            .type_params
            .iter()
            .map(|param| {
                let name = param.name().id.as_str();
                (name.to_owned(), super::generics::mangle(name))
            })
            .collect();
        if !frame.is_empty() {
            self.scopes.push((range, frame));
        }
    }
}

impl<'ast> ruff_python_ast::visitor::Visitor<'ast> for TypevarScopeWalker<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ClassDef(class) => self.record(class.range(), class.type_params.as_deref()),
            Stmt::FunctionDef(function) => {
                self.record(function.range(), function.type_params.as_deref());
            }
            _ => {}
        }
        ruff_python_ast::visitor::walk_stmt(self, stmt);
    }
}

/// Drive the lowering over `stmts`, visiting both the type positions the walker
/// knows about and any inline protocol left in a value position.
fn lower<'src>(
    source: &'src str,
    types: &'src dyn TypeInfo,
    claimed: &'src [TextRange],
    stmts: &[Stmt],
    config: &crate::Config,
) -> ProtocolTypeLowering<'src> {
    let mut inner = ProtocolTypeLowering::new(source, types, claimed);
    {
        let mut walker = TypevarScopeWalker {
            config: config.clone(),
            scopes: &mut inner.typevar_scopes,
        };
        for stmt in stmts {
            ruff_python_ast::visitor::Visitor::visit_stmt(&mut walker, stmt);
        }
    }
    walk_type_positions_skipping(stmts, Some(types), claimed, &mut inner);
    let mut walker = ValueProtocolWalker { inner: &mut inner };
    for stmt in stmts {
        ruff_python_ast::visitor::Visitor::visit_stmt(&mut walker, stmt);
    }
    inner
}

/// The preamble and edits needed to lower every inline protocol still present in
/// post-transform output.
///
/// A pass that re-renders a whole statement from the AST re-emits the surface
/// `protocol(...)` syntax, after this pass's own text edits were computed — so
/// the driver runs the lowering again over the spliced output to catch them.
/// Returns `None` when nothing is left to lower.
pub(crate) fn cleanup(
    source: &str,
    types: &dyn TypeInfo,
    stmts: &[Stmt],
    config: &crate::Config,
) -> Result<Option<(Vec<Fix>, String)>, String> {
    let mut inner = lower(source, types, &[], stmts, config);
    if let Some(error) = inner.errors.first() {
        return Err(error.clone());
    }
    if !inner.needs_import {
        return Ok(None);
    }
    // the earlier run over the pre-splice source already emitted a class (and
    // its imports) for the protocol whose edit was then dropped, so only what
    // the spliced output is actually missing gets prepended
    let mut preamble = String::new();
    let source_lines: Vec<&str> = source.lines().collect();
    // matched as a run of whole lines: a bare `contains` would let
    // `from typing import ProtocolFoo` suppress the `Protocol` import we need
    let push_missing = |preamble: &mut String, entry: &str| {
        let entry_lines: Vec<&str> = entry.lines().collect();
        let present = !entry_lines.is_empty()
            && source_lines
                .windows(entry_lines.len())
                .any(|window| window == entry_lines.as_slice());
        if !present {
            preamble.push_str(entry);
            preamble.push('\n');
        }
    };
    push_missing(&mut preamble, "from typing import Protocol");
    for line in inner.callable.take_import_lines() {
        push_missing(&mut preamble, &line);
    }
    for defs in [inner.callable.class_defs().to_owned(), inner.class_defs()] {
        for class_def in defs.split_inclusive("\n\n") {
            push_missing(&mut preamble, class_def.trim_end_matches('\n'));
        }
    }
    Ok(Some((inner.edits, preamble)))
}

impl TypeAwarePass for ProtocolTypePass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let claimed = ctx.claimed_type_op_ranges.clone();
        let mut inner = lower(self.source, types, &claimed, stmts, &self.config);

        if inner.needs_import {
            ctx.required_imports
                .push("from typing import Protocol".to_owned());
            // synthesized class defs are raw multi-line python source; push
            // them as a single non-`from` line so `merge_from_imports` leaves
            // them untouched and they land in the preamble verbatim. the
            // member lowerer's own classes come first: an inline protocol's
            // member may name one
            let defs = format!("{}{}", inner.callable.class_defs(), inner.class_defs());
            if !defs.is_empty() {
                ctx.required_imports
                    .push(defs.trim_end_matches('\n').to_owned());
            }
        }
        ctx.required_imports
            .extend(inner.callable.take_import_lines());
        for fix in inner.edits {
            for edit in fix.edits() {
                ctx.text_edits
                    .push((edit.range(), edit.content().unwrap_or_default().to_owned()));
            }
        }
        ctx.errors.extend(inner.errors);
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

    /// a pass that re-renders a whole statement from the AST re-emits the
    /// surface `protocol(...)` syntax after this pass computed its edits — the
    /// driver's cleanup loop catches what is left, without duplicating the class
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
