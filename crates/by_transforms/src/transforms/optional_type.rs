//! Text-edit lowering for the postfix `?` optional-type marker.
//!
//! The parser models `T?` as an `ExprUnaryOp` carrying `UnaryOp::Optional`.
//! This pass rewrites it to the runtime-compatible union `T | None`
//! (`Optional.Some(x)` is `x`, `Optional.None_` is `None`).
//!
//! It emits narrow text edits rather than mutating the AST so it composes with
//! the value-position operator lowerings that share a statement — e.g. a
//! function whose signature has `int?` and whose body uses `??` or `?.`. A
//! whole-statement re-render would drop those sibling edits. Each edit replaces
//! a top-level optional with a codegen rendering of its fully-lowered form, so
//! nested optionals (`int??`, `list[int?]?`) lower in one edit. An optional
//! directly wrapping another optional keeps the outer layer as the runtime
//! `Optional[...]` wrapper (`int??` ⇒ `Optional[int | None]`) so its distinct
//! outer-`None` state is not collapsed into the inner one. the wrapper's runtime
//! class (see [`wrapped_runtime`](super::wrapped_runtime)) is injected when emitted.
//!
//! The result form `T ? E` (`ExprBinOp` with `Operator::Result`) and the
//! postfix `^` / `!` operators are intentionally left for a later pass — their
//! runtime representation is still being settled.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::wrapped_runtime::OPTIONAL_RUNTIME;
use crate::type_info::TypeInfo;

/// Walks the source AST and emits narrow text edits that lower each optional in
/// place — editing only the postfix `?` tokens and leaving the operand source
/// untouched, so sibling type transforms (`not`, `&`, nested constructors)
/// compose on the operand.
struct OptionalLower<'src> {
    edits: Vec<(TextRange, String)>,
    /// set when any lowered optional produced a runtime `Optional[...]` wrapper
    needs_runtime: bool,
    source: &'src str,
    /// stack of in-scope PEP 695 type-parameter names. `?` over a bare type
    /// variable lowers to the *wrapped* form (`Optional[T | None]`) — a plain
    /// union would flatten when `T` binds to an optional (mirrors ty's typing
    /// of a generic `T?`)
    typevar_scopes: Vec<Vec<String>>,
}

impl<'src> OptionalLower<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            edits: Vec::new(),
            needs_runtime: false,
            source,
            typevar_scopes: Vec::new(),
        }
    }

    fn in_scope_typevar(&self, name: &str) -> bool {
        self.typevar_scopes
            .iter()
            .any(|scope| scope.iter().any(|n| n == name))
    }
}

impl<'ast> Visitor<'ast> for OptionalLower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let type_params = match stmt {
            Stmt::FunctionDef(f) => f.type_params.as_deref(),
            Stmt::ClassDef(c) => c.type_params.as_deref(),
            _ => None,
        };
        let pushed = if let Some(tp) = type_params {
            self.typevar_scopes.push(
                tp.type_params
                    .iter()
                    .map(|p| p.name().id.as_str().to_owned())
                    .collect(),
            );
            true
        } else {
            false
        };
        walk_stmt(self, stmt);
        if pushed {
            self.typevar_scopes.pop();
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        let Expr::UnaryOp(unary) = expr else {
            walk_expr(self, expr);
            return;
        };
        if unary.op != UnaryOp::Optional {
            walk_expr(self, expr);
            return;
        }

        // peel consecutive optional layers down to the innermost operand
        let mut depth: u32 = 1;
        let mut inner: &'ast Expr = unary.operand.as_ref();
        while let Expr::UnaryOp(u) = inner {
            if u.op != UnaryOp::Optional {
                break;
            }
            depth += 1;
            inner = u.operand.as_ref();
        }

        // rewrite only the region after the operand — the `?` tokens and any
        // surrounding whitespace — and leave the operand source untouched so
        // sibling type transforms (`not`, `&`, nested constructors) still apply
        // to it. close parens are preserved; whitespace around the `?`s is
        // dropped. `X?` → `X | None`; `X??` → `Optional[X | None]`; etc.
        let node = unary.range;
        let tail = &self.source[usize::from(inner.range().end())..usize::from(node.end())];
        let close_parens: String = tail.chars().filter(|c| *c == ')').collect();
        // a bare in-scope type variable gets one extra wrapper layer: `T?` is
        // `Optional[T | None]`, matching ty's wrapped typing of a generic `?`
        let generic_operand =
            matches!(inner, Expr::Name(n) if self.in_scope_typevar(n.id.as_str()));
        let wrap_layers = (depth - 1) as usize + usize::from(generic_operand);
        if wrap_layers >= 1 {
            self.needs_runtime = true;
            self.edits.push((
                TextRange::empty(node.start()),
                "Optional[".repeat(wrap_layers),
            ));
        }
        let mut replacement = close_parens;
        replacement.push_str(" | None");
        for _ in 0..wrap_layers {
            replacement.push(']');
        }
        self.edits
            .push((TextRange::new(inner.range().end(), node.end()), replacement));

        // descend into the operand so optionals nested inside it (e.g. the inner
        // `int?` of `list[int?]?`) are lowered too
        walk_expr(self, inner);
    }
}

pub(crate) struct OptionalTypePass<'src> {
    source: &'src str,
}

impl<'src> OptionalTypePass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

/// Collect the optional-lowering text edits for a single type-expression
/// subtree. Used by the shared `just_float::rewrite_type_expr` composer so a
/// `T?` nested inside a type constructor (tuple type, kw-subscript, …) is
/// lowered when that constructor renders its nested types. The runtime
/// `Optional[...]` import for nested `T??` is handled by [`OptionalTypePass`],
/// which independently walks every type position.
pub(crate) fn collect_edits(source: &str, expr: &Expr) -> Vec<(TextRange, String)> {
    let mut lower = OptionalLower::new(source);
    lower.visit_expr(expr);
    lower.edits
}

/// Lower the optionals in a single type-expression subtree to a string, or
/// `None` if it contains no optional. Used by type constructors (tuple type,
/// kw-subscript) to lower a nested `T?` when they splice their element types.
pub(crate) fn rewrite_type_expr(source: &str, expr: &Expr) -> Option<String> {
    let mut edits = collect_edits(source, expr);
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(range, _)| range.start());
    let range = expr.range();
    let mut result = String::new();
    let mut pos = range.start();
    for (r, replacement) in &edits {
        if r.start() < pos {
            continue;
        }
        result.push_str(&source[usize::from(pos)..usize::from(r.start())]);
        result.push_str(replacement);
        pos = r.end();
    }
    result.push_str(&source[usize::from(pos)..usize::from(range.end())]);
    Some(result)
}

impl TypeAwarePass for OptionalTypePass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut lower = OptionalLower::new(self.source);
        for stmt in stmts {
            lower.visit_stmt(stmt);
        }
        if lower.needs_runtime {
            ctx.required_imports.push(OPTIONAL_RUNTIME.to_owned());
        }
        ctx.text_edits.extend(lower.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    /// the runtime `Optional` wrapper class injected ahead of any `Optional[…]`
    const RUNTIME: &str = "\
class Optional:
    def __init__(self, value):
        self.value = value

    def __class_getitem__(cls, item):
        return cls

    def __repr__(self):
        return f\"Some({self.value!r})\"

";

    /// assert a transpile that emits a wrapped optional, prepending the runtime
    fn check_wrapped(input: &str, expected_body: &str) {
        check(input, &format!("{RUNTIME}{expected_body}"));
    }

    #[test]
    fn bare_optional_annotation() {
        check("x: int?\n", "x: int | None\n");
    }

    #[test]
    fn py39_target_defers_annotation_evaluation() {
        // below 3.10 the runtime cannot evaluate the pep 604 union this very
        // lowering produces, so the future import is mandatory
        let config = crate::Config {
            min_version: crate::PythonVersion::PY39,
            ..crate::Config::test_default()
        };
        assert_eq!(
            crate::transpile("x: int? = None\n", &config).unwrap(),
            "from __future__ import annotations\nx: int | None = None\n"
        );
    }

    #[test]
    fn generic_typevar_optional_wraps() {
        // `?` over a bare in-scope type variable is the wrapped form — a plain
        // union would flatten when `T` binds to an optional. (the 3.10 generics
        // polyfill's `T` → `_T` rename composes inside the wrapper)
        check(
            indoc! {"
                def f[T](t: T) -> T?:
                    return Some(t)
            "},
            &format!(
                "from typing import TypeVar\n{RUNTIME}_T = TypeVar(\"_T\")\ndef f(t: _T) -> Optional[_T | None]:\n    return Optional(t)\n"
            ),
        );
    }

    #[test]
    fn non_typevar_name_optional_stays_union() {
        // a name that is not an in-scope type parameter lowers to the plain
        // union as before
        check(
            "def f(x: int?) -> None: ...\n",
            "def f(x: int | None) -> None: ...\n",
        );
    }

    #[test]
    fn optional_of_negation_composes() {
        // the operand source is preserved, so the `not_type` transform still
        // lowers `not A` to `Not[A]` inside the optional
        check(
            "class A: ...\nx: (not A)?\n",
            "from ty_extensions import Not\nclass A: ...\nx: (Not[A]) | None\n",
        );
    }

    #[test]
    fn optional_of_intersection_composes() {
        check(
            "class A: ...\nclass B: ...\nx: (A & B)?\n",
            "from ty_extensions import Intersection\nclass A: ...\nclass B: ...\nx: (Intersection[A, B]) | None\n",
        );
    }

    #[test]
    fn optional_return_annotation() {
        check(
            indoc! {"
                def f() -> int?:
                    return None
            "},
            indoc! {"
                def f() -> int | None:
                    return None
            "},
        );
    }

    #[test]
    fn optional_parameter_annotation() {
        check("def f(x: int?): ...\n", "def f(x: int | None): ...\n");
    }

    #[test]
    fn optional_in_subscript() {
        check("x: list[int?]\n", "x: list[int | None]\n");
    }

    #[test]
    fn optional_of_union() {
        check("x: int | str?\n", "x: int | str | None\n");
    }

    #[test]
    fn whitespace_before_marker_tolerated() {
        check("x: int ?\n", "x: int | None\n");
    }

    /// a nested optional keeps the outer layer as the runtime `Optional[...]`
    /// wrapper so the distinct outer-`None` state is preserved instead of
    /// collapsing into `int | None | None`. two `?` separated by whitespace lex
    /// as two tokens
    #[test]
    fn spaced_double_optional_wraps() {
        check_wrapped("x: int ? ?\n", "x: Optional[int | None]\n");
    }

    /// glued `??` lexes as the coalesce token, but with no right operand it can
    /// only be the double-optional type marker, so `int??` lowers the same as
    /// the spaced form
    #[test]
    fn glued_double_optional_wraps() {
        check_wrapped("x: int??\n", "x: Optional[int | None]\n");
    }

    /// the user's reported case: a glued double-optional in a return annotation
    #[test]
    fn glued_double_optional_return_annotation() {
        check_wrapped(
            "def f() -> int??: ...\n",
            "def f() -> Optional[int | None]: ...\n",
        );
    }

    /// each additional optional layer adds another `Optional[...]` wrapper
    #[test]
    fn triple_optional_nests_wrappers() {
        check_wrapped("x: int???\n", "x: Optional[Optional[int | None]]\n");
    }

    /// an optional wrapping a non-optional (here a `list`) stays a plain union,
    /// even when the list element is itself optional
    #[test]
    fn optional_of_list_of_optional_stays_union() {
        check("x: list[int?]?\n", "x: list[int | None] | None\n");
    }

    /// the edit must be narrow: an optional in a signature composes with a
    /// value-position `??` lowering in the same function body
    #[test]
    fn optional_signature_with_coalesce_body() {
        check(
            indoc! {"
                def f(x: int?):
                    return x ?? 0
            "},
            indoc! {"
                def f(x: int | None):
                    return x if x is not None else 0
            "},
        );
    }

    /// likewise with an optional-chain (`?.`) in the body
    #[test]
    fn optional_signature_with_chain_body() {
        check(
            indoc! {"
                def f(a: str?):
                    return a?.b
            "},
            indoc! {"
                def f(a: str | None):
                    return None if a is None else a.b
            "},
        );
    }

    #[test]
    fn plain_union_passthrough_in_python() {
        unchanged("x: int | None\n");
    }
}
