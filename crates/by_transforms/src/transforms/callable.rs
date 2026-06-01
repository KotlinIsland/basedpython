//! rewrites callable type syntax in annotation positions
//!
//! denotable callable types lower to `typing.Callable`:
//!
//! `(int) -> int`             → `Callable[[int], int]`
//! `(int, str) -> bool`       → `Callable[[int, str], bool]`
//! `() -> None`               → `Callable[[], None]`
//! `(int) -> (str) -> bool`   → `Callable[[int], Callable[[str], bool]]`
//!
//! non-denotable callable types — those with named parameters, `/` /
//! `*` markers, variadic `*args: T`, or kwargs `**kwargs: T` — synthesize
//! a `typing.Protocol` subclass with a `__call__` method. The protocol
//! class is hoisted to module scope and the annotation site is replaced
//! with the protocol's name:
//!
//! `(a: int) -> str`          → `class _Callable_<hash>(Protocol):
//!                                  def __call__(self, a: int, /) -> str: ...`
//!                              and the annotation becomes `_Callable_<hash>`

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, ExprCallableType, Operator, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::wrapped_runtime::OPTIONAL_RUNTIME;
use crate::type_info::TypeInfo;

#[expect(
    clippy::struct_excessive_bools,
    reason = "transform flags toggled across visit"
)]
pub(crate) struct CallableSyntax<'src> {
    source: &'src str,
    types: Option<&'src (dyn TypeInfo + 'src)>,
    pub(crate) edits: Vec<Fix>,
    pub(crate) needs_import: bool,
    pub(crate) needs_protocol_import: bool,
    pub(crate) needs_intersection_import: bool,
    pub(crate) needs_typeof_import: bool,
    pub(crate) needs_not_import: bool,
    pub(crate) needs_optional_runtime: bool,
    /// shape → synthesized class name. used to dedupe identical
    /// non-denotable callable shapes
    protocol_shapes: HashMap<ProtocolShape, String>,
    /// emitted class definitions in declaration order
    protocol_class_defs: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct ProtocolShape {
    /// rendered `def __call__(self, ...) -> R:` parameter list (everything
    /// between the surrounding `(self, ` and `) -> R:`)
    params: String,
    returns: String,
}

impl<'src> CallableSyntax<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            types: None,
            edits: Vec::new(),
            needs_import: false,
            needs_protocol_import: false,
            needs_intersection_import: false,
            needs_typeof_import: false,
            needs_not_import: false,
            needs_optional_runtime: false,
            protocol_shapes: HashMap::new(),
            protocol_class_defs: String::new(),
        }
    }

    pub(crate) fn with_types(mut self, types: &'src dyn TypeInfo) -> Self {
        self.types = Some(types);
        self
    }

    pub(crate) fn class_defs(&self) -> &str {
        &self.protocol_class_defs
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// True iff the callable signature can't be expressed by `Callable[[T,
    /// ...], R]` and needs a `Protocol.__call__` synthesis: any named
    /// parameter, marker, variadic, or kwargs catch-all
    #[expect(clippy::unused_self, reason = "kept as method for grouping")]
    fn is_non_denotable(&self, ct: &ExprCallableType) -> bool {
        if ct.parameter_slash.is_some() || ct.parameter_star.is_some() {
            return true;
        }
        ct.args
            .iter()
            .any(|a| matches!(a, Expr::Named(_) | Expr::Starred(_)))
    }

    /// Render a callable's parameter list as a `def __call__(self, ...) ->
    /// R:` parameter string. Markers and variadic forms map to the
    /// corresponding Python parameter syntax
    fn render_protocol_params(&mut self, ct: &ExprCallableType) -> String {
        let mut parts: Vec<String> = vec!["self".to_owned()];
        let explicit_slash = ct.parameter_slash.map(|i| i as usize);
        let star = ct.parameter_star.map(|i| i as usize);
        // implicit `/` after the last bare positional (no label) when followed
        // by a named/labelled parameter. bare positionals are positional-only
        let implicit_slash: Option<usize> = if explicit_slash.is_some() {
            None
        } else {
            let last_bare = ct.args.iter().enumerate().rev().find_map(|(i, a)| {
                let is_bare = !matches!(a, Expr::Named(_) | Expr::Starred(_));
                is_bare.then_some(i)
            });
            last_bare.and_then(|li| {
                if ct.args.get(li + 1).is_some_and(
                    |a| matches!(a, Expr::Named(n) if matches!(n.target.as_ref(), Expr::Name(_))),
                ) {
                    Some(li + 1)
                } else {
                    None
                }
            })
        };
        let slash = explicit_slash.or(implicit_slash);
        let mut star_emitted = false;
        for (i, arg) in ct.args.iter().enumerate() {
            if Some(i) == slash {
                parts.push("/".to_owned());
            }
            if Some(i) == star && !star_emitted {
                let consumed = matches!(arg, Expr::Starred(_))
                    || matches!(arg, Expr::Named(n) if matches!(n.target.as_ref(), Expr::Starred(_)));
                if !consumed {
                    parts.push("*".to_owned());
                    star_emitted = true;
                }
            }
            match arg {
                Expr::Named(named) => {
                    let name = match named.target.as_ref() {
                        Expr::Name(n) => n.id.as_str().to_owned(),
                        Expr::Starred(s) => match s.value.as_ref() {
                            Expr::Starred(inner_inner) => {
                                let n = inner_inner
                                    .value
                                    .as_name_expr()
                                    .map(|n| n.id.as_str())
                                    .unwrap_or("kwargs");
                                format!("**{n}")
                            }
                            _ => {
                                let n = s
                                    .value
                                    .as_name_expr()
                                    .map(|n| n.id.as_str())
                                    .unwrap_or("args");
                                star_emitted = true;
                                format!("*{n}")
                            }
                        },
                        _ => "_".to_owned(),
                    };
                    let ty = self.src(named.value.range()).to_owned();
                    parts.push(format!("{name}: {ty}"));
                }
                Expr::Starred(s) => match s.value.as_ref() {
                    Expr::Starred(inner) => {
                        let ty = self.src(inner.value.range()).to_owned();
                        parts.push(format!("**kwargs: {ty}"));
                    }
                    _ => {
                        star_emitted = true;
                        let ty = self.src(s.value.range()).to_owned();
                        parts.push(format!("*args: {ty}"));
                    }
                },
                _ => {
                    // bare positional type — Protocol's `__call__` needs a
                    // parameter NAME, so we synthesize one. Use an
                    // unused-prefixed name so static checkers don't flag
                    // it as a missing arg
                    let ty = self
                        .rewrite(arg)
                        .unwrap_or_else(|| self.src(arg.range()).to_owned());
                    parts.push(format!("_{i}: {ty}"));
                }
            }
        }
        // markers at the very end (slash/star at args.len)
        let after_last = ct.args.len();
        if Some(after_last) == slash {
            parts.push("/".to_owned());
        }
        if Some(after_last) == star && !star_emitted {
            parts.push("*".to_owned());
        }
        parts.join(", ")
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "shape ownership stays at call site for clarity"
    )]
    fn class_name_for(&mut self, shape: ProtocolShape) -> String {
        if let Some(name) = self.protocol_shapes.get(&shape) {
            return name.clone();
        }
        let mut hasher = DefaultHasher::new();
        shape.hash(&mut hasher);
        #[expect(clippy::cast_possible_truncation)]
        let truncated = hasher.finish() as u32;
        let name = format!("_Callable_{truncated:08x}");
        self.protocol_shapes.insert(shape.clone(), name.clone());
        let _ = writeln!(
            self.protocol_class_defs,
            "class {name}(Protocol):\n    def __call__({params}) -> {ret}: ...\n",
            params = shape.params,
            ret = shape.returns,
        );
        name
    }

    pub(crate) fn rewrite(&mut self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::CallableType(ct) if self.is_non_denotable(ct) => {
                self.needs_protocol_import = true;
                let params = self.render_protocol_params(ct);
                let returns = self
                    .rewrite(&ct.returns)
                    .unwrap_or_else(|| self.src(ct.returns.range()).to_owned());
                let shape = ProtocolShape { params, returns };
                Some(self.class_name_for(shape))
            }
            // `(...) -> R` — a single bare ellipsis parameter list is python's
            // "any arguments" callable: `Callable[..., R]`, not the
            // single-`...`-argument `Callable[[...], R]`
            Expr::CallableType(ExprCallableType { args, returns, .. })
                if matches!(args.as_slice(), [Expr::EllipsisLiteral(_)]) =>
            {
                self.needs_import = true;
                let ret_str = self
                    .rewrite(returns)
                    .unwrap_or_else(|| self.src(returns.range()).to_owned());
                Some(format!("Callable[..., {ret_str}]"))
            }

            Expr::CallableType(ExprCallableType { args, returns, .. }) => {
                self.needs_import = true;
                let args_str = args
                    .iter()
                    .map(|a| {
                        self.rewrite(a)
                            .unwrap_or_else(|| self.src(a.range()).to_owned())
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret_str = self
                    .rewrite(returns)
                    .unwrap_or_else(|| self.src(returns.range()).to_owned());
                Some(format!("Callable[[{args_str}], {ret_str}]"))
            }

            Expr::BinOp(b) if matches!(b.op, Operator::BitAnd) => {
                // intersection: `A & B & C` → `Intersection[A, B, C]`. recurse
                // through left-associative chain so nested forms (callable,
                // tuple, typeof) inside the intersection participants are also
                // lowered
                self.needs_intersection_import = true;
                let parts = flatten_bitand(expr);
                let rendered: Vec<String> = parts
                    .iter()
                    .map(|p| {
                        self.rewrite(p)
                            .unwrap_or_else(|| self.src(p.range()).to_owned())
                    })
                    .collect();
                Some(format!("Intersection[{}]", rendered.join(", ")))
            }

            // `not T` → `Not[T]`
            Expr::UnaryOp(u) if matches!(u.op, UnaryOp::Not) => {
                self.needs_not_import = true;
                let inner = self
                    .rewrite(&u.operand)
                    .unwrap_or_else(|| self.src(u.operand.range()).to_owned());
                Some(format!("Not[{inner}]"))
            }

            // `T?` → `T | None` (and nested `T??` → `Optional[T | None]`), so the
            // optional composes when it sits inside a callable-arrow arg/return
            Expr::UnaryOp(u) if matches!(u.op, UnaryOp::Optional) => {
                let mut depth: usize = 1;
                let mut inner: &Expr = u.operand.as_ref();
                while let Expr::UnaryOp(u2) = inner {
                    if u2.op != UnaryOp::Optional {
                        break;
                    }
                    depth += 1;
                    inner = u2.operand.as_ref();
                }
                let inner_str = self
                    .rewrite(inner)
                    .unwrap_or_else(|| self.src(inner.range()).to_owned());
                if depth >= 2 {
                    self.needs_optional_runtime = true;
                }
                Some(format!(
                    "{}{inner_str} | None{}",
                    "Optional[".repeat(depth - 1),
                    "]".repeat(depth - 1)
                ))
            }

            Expr::BinOp(b) => {
                let l = self.rewrite(&b.left);
                let r = self.rewrite(&b.right);
                if l.is_some() || r.is_some() {
                    let op = b.op.as_str();
                    let ls = l.unwrap_or_else(|| self.src(b.left.range()).to_owned());
                    let rs = r.unwrap_or_else(|| self.src(b.right.range()).to_owned());
                    Some(format!("{ls} {op} {rs}"))
                } else {
                    None
                }
            }

            // `typeof X` → `TypeOf[X]` (parser tags such subscripts with `is_typeof`)
            Expr::Subscript(s) if s.is_typeof => {
                self.needs_typeof_import = true;
                let inner = self
                    .rewrite(&s.slice)
                    .unwrap_or_else(|| self.src(s.slice.range()).to_owned());
                Some(format!("TypeOf[{inner}]"))
            }

            Expr::Subscript(s) => {
                let slice_rewrite = match s.slice.as_ref() {
                    Expr::Tuple(t) if !t.parenthesized => {
                        let rewrites: Vec<Option<String>> =
                            t.elts.iter().map(|e| self.rewrite(e)).collect();
                        if rewrites.iter().any(std::option::Option::is_some) {
                            let parts: Vec<String> = rewrites
                                .into_iter()
                                .zip(t.elts.iter())
                                .map(|(r, e)| r.unwrap_or_else(|| self.src(e.range()).to_owned()))
                                .collect();
                            Some(parts.join(", "))
                        } else {
                            None
                        }
                    }
                    slice => self.rewrite(slice),
                };
                slice_rewrite.map(|s_text| format!("{}[{s_text}]", self.src(s.value.range())))
            }

            // list literal inside a subscript slice (e.g. `Callable[[A, B], R]`'s
            // parameter list). recurse into elts so intersections / nested
            // callable arrows inside the list are lowered by the same wide edit
            // the outer Subscript emits — otherwise intersection.rs's narrow
            // edits would be dropped by ast_driver's first-wins overlap rule
            Expr::List(l) => {
                let rewrites: Vec<Option<String>> =
                    l.elts.iter().map(|e| self.rewrite(e)).collect();
                if rewrites.iter().any(std::option::Option::is_some) {
                    let parts: Vec<String> = rewrites
                        .into_iter()
                        .zip(l.elts.iter())
                        .map(|(r, e)| r.unwrap_or_else(|| self.src(e.range()).to_owned()))
                        .collect();
                    Some(format!("[{}]", parts.join(", ")))
                } else {
                    None
                }
            }

            // parenthesized tuple type literal: `(int, str)` → `tuple[int, str]`.
            // a tuple with any named field is an anonymous named tuple, owned
            // by `anon_named_tuple` — don't touch it here
            Expr::Tuple(t)
                if t.parenthesized
                    && !t.elts.is_empty()
                    && !t.elts.iter().any(|e| matches!(e, Expr::Named(_))) =>
            {
                let rewrites: Vec<Option<String>> =
                    t.elts.iter().map(|e| self.rewrite(e)).collect();
                let parts: Vec<String> = rewrites
                    .into_iter()
                    .zip(t.elts.iter())
                    .map(|(r, e)| r.unwrap_or_else(|| self.src(e.range()).to_owned()))
                    .collect();
                Some(format!("tuple[{}]", parts.join(", ")))
            }

            _ => None,
        }
    }
}

/// flatten a left-associative `&` chain (`A & B & C`) into individual operands
fn flatten_bitand(expr: &Expr) -> Vec<&Expr> {
    fn walk<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match expr {
            Expr::BinOp(b) if matches!(b.op, Operator::BitAnd) => {
                walk(&b.left, out);
                walk(&b.right, out);
            }
            _ => out.push(expr),
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

/// if `expr` is `Subscript(Name("__let__"|"__classvar__"), slice)`, returns the slice
pub(crate) fn synthetic_let_slice(expr: &Expr) -> Option<&Expr> {
    if let Expr::Subscript(s) = expr {
        if let Expr::Name(n) = s.value.as_ref() {
            if matches!(n.id.as_str(), "__let__" | "__classvar__") {
                return Some(s.slice.as_ref());
            }
        }
    }
    None
}

impl crate::transforms::type_expr_walker::TypeExprVisitor for CallableSyntax<'_> {
    fn visit(
        &mut self,
        expr: &Expr,
        _pos: crate::transforms::type_expr_walker::TypePos,
    ) -> crate::transforms::type_expr_walker::Recurse {
        // ParamSpec-targeted subscripts (`A[(int, str)]` where `class
        // A[P: Parameters]`): the tuple slice is a parameter list lowered
        // by `generics.rs` to `[int, str]`, not a tuple-type. don't fire
        // here — callable's tuple-literal handling would otherwise emit
        // `A[tuple[int, str]]` that subsumes generics' polyfill edit
        if let Expr::Subscript(s) = expr
            && self
                .types
                .as_ref()
                .is_some_and(|t| t.class_first_typevar_is_paramspec(&s.value))
        {
            return crate::transforms::type_expr_walker::Recurse::Stop;
        }
        // `__let__[T]` / `__classvar__[T]` are modifier markers wrapping a
        // type expression. `modifiers` owns the outer wrapper; we only want
        // to rewrite the inner T. tell the walker to descend so it visits T
        // at the next level
        if synthetic_let_slice(expr).is_some() {
            return crate::transforms::type_expr_walker::Recurse::Descend;
        }
        // a bare top-level optional (`int?`, `int??`) is owned by
        // `optional_type`, which emits narrow edits (a zero-width `Optional[`
        // insertion for nested layers). our whole-range rewrite would collide
        // with that insertion at the shared start offset. descend instead so a
        // callable nested inside the operand is still lowered, while the `?`
        // layers stay with their dedicated pass. (an optional *inside* a
        // callable arg/return is handled by `rewrite`'s recursion, where the
        // callable's wider edit cleanly subsumes the optional's narrow ones.)
        if matches!(expr, Expr::UnaryOp(u) if u.op == UnaryOp::Optional) {
            return crate::transforms::type_expr_walker::Recurse::Descend;
        }
        // `rewrite` is a deep recursive rewriter that produces a single
        // replacement for the whole expression. emit the edit and stop
        if let Some(rewrite) = self.rewrite(expr) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                rewrite,
                expr.range(),
            )));
        }
        crate::transforms::type_expr_walker::Recurse::Stop
    }
}

pub(crate) struct CallableSyntaxPass<'src> {
    source: &'src str,
}

impl<'src> CallableSyntaxPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

/// Walks value positions and lowers any `(...) -> ...` callable type found
/// there (e.g. `print(() -> int)`). The type-position walker only visits
/// annotation contexts; a `CallableType` node is always invalid python wherever
/// it appears, so it must be lowered everywhere. Fires *only* on `CallableType`
/// — never on value-position `&` or tuples, which have real runtime meaning.
struct ValueCallableWalker<'a, 'src> {
    inner: &'a mut CallableSyntax<'src>,
}

impl<'ast> ruff_python_ast::visitor::Visitor<'ast> for ValueCallableWalker<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if matches!(expr, Expr::CallableType(_)) {
            if let Some(repl) = self.inner.rewrite(expr) {
                self.inner
                    .edits
                    .push(Fix::safe_edit(Edit::range_replacement(repl, expr.range())));
            }
            // `rewrite` already lowered any nested callables/types; don't recurse
            return;
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

impl TypeAwarePass for CallableSyntaxPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = CallableSyntax::new(self.source).with_types(types);
        crate::transforms::type_expr_walker::walk_type_positions_skipping(
            stmts,
            Some(types),
            &ctx.claimed_type_op_ranges,
            &mut inner,
        );
        // also lower callable types appearing in value positions; duplicate
        // edits over type-position callables dedup in the splice
        {
            let mut walker = ValueCallableWalker { inner: &mut inner };
            for stmt in stmts {
                ruff_python_ast::visitor::Visitor::visit_stmt(&mut walker, stmt);
            }
        }
        if inner.needs_import {
            ctx.required_imports
                .push("from typing import Callable".to_owned());
        }
        if inner.needs_protocol_import {
            ctx.required_imports
                .push("from typing import Protocol".to_owned());
        }
        if inner.needs_intersection_import {
            ctx.required_imports
                .push("from ty_extensions import Intersection".to_owned());
        }
        if inner.needs_typeof_import {
            ctx.required_imports
                .push("from ty_extensions import TypeOf".to_owned());
        }
        if inner.needs_not_import {
            ctx.required_imports
                .push("from ty_extensions import Not".to_owned());
        }
        if inner.needs_optional_runtime {
            ctx.required_imports.push(OPTIONAL_RUNTIME.to_owned());
        }
        let defs = inner.class_defs().to_owned();
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
        if !defs.is_empty() {
            // preserve one trailing newline so the blank line between the
            // synthesized class defs and the rest of the file survives
            // (driver's required_imports loop appends one `\n` per entry)
            let trimmed = defs.trim_end_matches('\n');
            ctx.required_imports.push(format!("{trimmed}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple_callable() {
        check(
            "a: (int) -> int\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int], int]
            "},
        );
    }

    #[test]
    fn no_args() {
        check(
            "a: () -> None\n",
            indoc! {"
                from typing import Callable
                a: Callable[[], None]
            "},
        );
    }

    #[test]
    fn ellipsis_args() {
        // `(...) -> R` is the "any arguments" callable: `Callable[..., R]`,
        // not the single-`...`-argument `Callable[[...], R]`
        check(
            "a: (...) -> int\n",
            indoc! {"
                from typing import Callable
                a: Callable[..., int]
            "},
        );
    }

    #[test]
    fn ellipsis_args_nested_return() {
        check(
            "a: (...) -> (int) -> str\n",
            indoc! {"
                from typing import Callable
                a: Callable[..., Callable[[int], str]]
            "},
        );
    }

    #[test]
    fn multi_args() {
        check(
            "a: (int, str) -> bool\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int, str], bool]
            "},
        );
    }

    #[test]
    fn callable_in_union() {
        check(
            "a: (int) -> int | None\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int], int] | None
            "},
        );
    }

    /// an optional `?` on a callable arg lowers inside the `Callable[...]`
    /// rendering — the callable's whole-range edit subsumes `optional_type`'s
    #[test]
    fn callable_arg_optional() {
        check(
            "a: (int?) -> int\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int | None], int]
            "},
        );
    }

    #[test]
    fn callable_as_return_type() {
        check(
            indoc! {"
                def f(x: (int) -> bool) -> (str) -> None:
                    pass
            "},
            indoc! {"
                from typing import Callable
                def f(x: Callable[[int], bool]) -> Callable[[str], None]:
                    pass
            "},
        );
    }

    #[test]
    fn nested_callable() {
        check(
            "a: (int) -> (str) -> bool\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int], Callable[[str], bool]]
            "},
        );
    }

    #[test]
    fn callable_inside_subscript() {
        check(
            "a: list[(int) -> int]\n",
            indoc! {"
                from typing import Callable
                a: list[Callable[[int], int]]
            "},
        );
    }

    #[test]
    fn value_context_not_rewritten() {
        check("x = (int)\n", "x = (int)\n");
    }

    #[test]
    fn non_denotable_named_param() {
        check(
            "a: (a: int) -> str\n",
            indoc! {"
                from typing import Protocol
                class _Callable_3ffa14a8(Protocol):
                    def __call__(self, a: int) -> str: ...

                a: _Callable_3ffa14a8
            "},
        );
    }

    #[test]
    fn non_denotable_full_param_form() {
        // `(int, /, a: str, *args: int, **kwargs: str) -> None`
        let out = transpile(
            "f: (int, /, a: str, *args: int, **kwargs: str) -> None\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _Callable_"), "got: {out}");
        assert!(
            out.contains(
                "def __call__(self, _0: int, /, a: str, *args: int, **kwargs: str) -> None: ..."
            ),
            "got: {out}"
        );
        assert!(
            out.starts_with("from typing import Protocol\n"),
            "got: {out}"
        );
    }

    #[test]
    fn duplicate_non_denotable_dedupes() {
        // identical shapes share a single Protocol class
        let out = transpile(
            "a: (n: int) -> str\nb: (n: int) -> str\n",
            &Config::test_default(),
        )
        .unwrap();
        let count = out.matches("class _Callable_").count();
        assert_eq!(count, 1, "got: {out}");
    }

    #[test]
    fn callable_in_call_argument() {
        // value position: a bare callable type passed as an argument
        check(
            "print(() -> int)\n",
            indoc! {"
                from typing import Callable
                print(Callable[[], int])
            "},
        );
    }

    #[test]
    fn callable_in_assignment_value() {
        check(
            "x = (int, str) -> bool\n",
            indoc! {"
                from typing import Callable
                x = Callable[[int, str], bool]
            "},
        );
    }

    #[test]
    fn nested_callable_in_value_position() {
        check(
            "y = (int) -> (str) -> None\n",
            indoc! {"
                from typing import Callable
                y = Callable[[int], Callable[[str], None]]
            "},
        );
    }

    #[test]
    fn non_denotable_callable_in_value_position() {
        // named-param callable in value position synthesizes a Protocol class
        let out = transpile("x = (n: int) -> str\n", &Config::test_default()).unwrap();
        assert!(out.contains("class _Callable_"), "got: {out}");
        assert!(
            out.contains("x = _Callable_"),
            "value site should reference the protocol name, got: {out}"
        );
    }
}
