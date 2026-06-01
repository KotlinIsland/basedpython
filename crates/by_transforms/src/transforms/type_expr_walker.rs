//! shared walker for type-position expressions across transforms
//!
//! many transforms (`not_type`, `intersection`, `literal_types`, `just_float`,
//! `annotation`, ...) all need to visit "every type expression" in a module.
//! historically each one re-implemented the statement traversal + interior
//! descent, with subtle differences:
//!
//! - some handled class bases, others didn't
//! - some descended into `|` arms, others didn't
//! - some recognised `Annotated[T, meta]` first-arg-only semantics, others
//!   blindly rewrote metadata
//! - some walked value-position type applications (`list[T]` outside an
//!   annotation), others stopped at the syntactic annotation slot
//!
//! this module centralises that traversal. transforms implement
//! [`TypeExprVisitor::visit`] and the walker takes care of getting them
//! called on every type-position node. visitor returns [`Recurse::Stop`] to
//! tell the walker not to descend into a subtree it already fully rewrote
//!
//! the walker recognises these type positions:
//!
//! 1. `AnnAssign` annotation
//! 2. function parameter annotations (regular, vararg, kwarg, lambda params)
//! 3. function return annotation
//! 4. `type X = T` RHS / `X: TypeAlias = T` (the `TypeAlias` form is reached
//!    via `AnnAssign`, no special case needed)
//! 5. `TypeParam` bound (PEP 695 + basedpython `constraints (…)`)
//! 6. `TypeParam` default
//! 7. value-position type applications (`list[T]`, `dict[K, V]` outside an
//!    annotation — gated on ty's `is_known_type_subscript`)
//! 8. `cast(T, _)` first arg
//! 9. `Annotated[T, meta…]` only the first arg
//! 10. `Callable[[P1, P2], R]` parameter list elements + return type
//! 12. class base list

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Operator, Parameters, Stmt, TypeParam, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::TypeInfo;

/// the kind of type position currently being visited. lets visitors
/// distinguish (e.g.) a syntactic annotation from an interior subtree
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypePos {
    /// top-level annotation (`a: T`, `-> T`, type-param bound/default,
    /// class base, type-alias RHS, top-level value-position type app)
    Root,
    /// nested type expression (subscript slice, binop arm, Callable return)
    Nested,
    /// first argument of `Annotated[T, meta…]` — only this slot is a type
    /// position; remaining slots are arbitrary metadata
    AnnotatedFirst,
    /// element of a `Callable[[P1, P2], R]` parameter list
    CallableParam,
}

/// whether the walker should descend into a node's sub-positions after the
/// visitor processes it. visitors that fully rewrite a subtree (e.g. wrap
/// the whole thing in `Literal[...]`) return [`Recurse::Stop`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recurse {
    Descend,
    Stop,
}

pub(crate) trait TypeExprVisitor {
    fn visit(&mut self, expr: &Expr, pos: TypePos) -> Recurse;
}

/// walk every type-position expression in `stmts`. `types` enables detection
/// of value-position type applications; pass `None` to limit traversal to
/// syntactic annotation positions only
pub(crate) fn walk_type_positions(
    stmts: &[Stmt],
    types: Option<&dyn TypeInfo>,
    visitor: &mut dyn TypeExprVisitor,
) {
    walk_type_positions_skipping(stmts, types, &[], visitor);
}

/// like [`walk_type_positions`] but skips any type expression whose range falls
/// within one of `claimed` — subtrees another pass has already resolved
/// wholesale (e.g. `symbolic_type_op` folding `1 + 1` to `Literal[2]`). this
/// keeps later passes from emitting now-stale edits or import requests for an
/// operation that no longer appears in the output
pub(crate) fn walk_type_positions_skipping(
    stmts: &[Stmt],
    types: Option<&dyn TypeInfo>,
    claimed: &[TextRange],
    visitor: &mut dyn TypeExprVisitor,
) {
    let mut walker = TypePosWalker {
        types,
        claimed,
        visitor,
    };
    for stmt in stmts {
        walker.visit_stmt(stmt);
    }
}

/// drive a [`TypeExprVisitor`] over a single root type expression. used by
/// callers that already know the expression is in a type position (e.g.
/// `generics.rs` re-rewriting the body of a polyfilled type alias)
pub(crate) fn walk_one_type_expr(expr: &Expr, visitor: &mut dyn TypeExprVisitor) {
    let mut walker = TypePosWalker {
        types: None,
        claimed: &[],
        visitor,
    };
    walker.visit_type_expr(expr, TypePos::Root);
}

struct TypePosWalker<'a> {
    types: Option<&'a dyn TypeInfo>,
    claimed: &'a [TextRange],
    visitor: &'a mut dyn TypeExprVisitor,
}

impl TypePosWalker<'_> {
    fn visit_type_expr(&mut self, expr: &Expr, pos: TypePos) {
        // a claimed subtree has already been resolved by an earlier pass; don't
        // visit or descend into it
        if self.claimed.iter().any(|c| c.contains_range(expr.range())) {
            return;
        }
        if self.visitor.visit(expr, pos) == Recurse::Stop {
            return;
        }
        self.descend_into_type_expr(expr);
    }

    /// recurse into sub-positions of a type expression. the walker knows the
    /// shape of `BinOp` (`|` / `&`), `UnaryOp` (`not`), and `Subscript`
    /// (generic + special forms `Annotated` / `Callable` / `Literal`)
    fn descend_into_type_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::BinOp(b) if matches!(b.op, Operator::BitOr | Operator::BitAnd) => {
                self.visit_type_expr(&b.left, TypePos::Nested);
                self.visit_type_expr(&b.right, TypePos::Nested);
            }
            // `not T` (negation) and `T?` (optional) both carry a nested type
            // expression in their operand, so descend so sibling transforms
            // (intersection, not, float, …) apply inside `(not A)?`, `(A & B)?`
            Expr::UnaryOp(u) if matches!(u.op, UnaryOp::Not | UnaryOp::Optional) => {
                self.visit_type_expr(&u.operand, TypePos::Nested);
            }
            Expr::Subscript(s) => {
                if name_matches(&s.value, "Literal") {
                    // `Literal[1, "a"]` arguments are value tokens, not
                    // type expressions — don't descend
                    return;
                }
                if name_matches(&s.value, "Annotated") {
                    // first slice element is a type position; rest is metadata
                    if let Expr::Tuple(t) = s.slice.as_ref() {
                        if let Some(first) = t.elts.first() {
                            self.visit_type_expr(first, TypePos::AnnotatedFirst);
                        }
                    } else {
                        self.visit_type_expr(&s.slice, TypePos::AnnotatedFirst);
                    }
                    return;
                }
                if name_matches(&s.value, "Callable") {
                    self.descend_callable(&s.slice);
                    return;
                }
                self.descend_generic_subscript_slice(&s.slice);
            }
            // parenthesized tuple in a type position (`a: (int, str)`) — owned
            // by `annotation` (tuple-literal lowering) and `anon_named_tuple`.
            // we deliberately don't descend so other transforms (not_type,
            // intersection, just_float, …) don't emit narrow edits inside the
            // tuple that would be subsumed by the outer tuple-lowering wide
            // edit (leaving the narrow transform's `needs_import` flag set
            // even though the edit was dropped)
            Expr::Tuple(_) => {}
            _ => {}
        }
    }

    fn descend_generic_subscript_slice(&mut self, slice: &Expr) {
        if let Expr::Tuple(t) = slice {
            if !t.parenthesized {
                for e in &t.elts {
                    self.visit_type_expr(e, TypePos::Nested);
                }
                return;
            }
        }
        self.visit_type_expr(slice, TypePos::Nested);
    }

    fn descend_callable(&mut self, slice: &Expr) {
        // `Callable[[P1, P2], R]` — slice is an unparenthesized 2-tuple
        // `(List[P1, P2], R)`. fall back to generic descent for any other
        // shape (e.g. `Callable[..., R]`, `Callable[P, R]` with ParamSpec)
        if let Expr::Tuple(t) = slice {
            if !t.parenthesized && t.elts.len() == 2 {
                if let Expr::List(pl) = &t.elts[0] {
                    for p in &pl.elts {
                        self.visit_type_expr(p, TypePos::CallableParam);
                    }
                } else {
                    self.visit_type_expr(&t.elts[0], TypePos::Nested);
                }
                self.visit_type_expr(&t.elts[1], TypePos::Nested);
                return;
            }
        }
        self.descend_generic_subscript_slice(slice);
    }

    fn visit_parameters(&mut self, params: &Parameters) {
        for p in params.iter_non_variadic_params() {
            if let Some(ann) = &p.parameter.annotation {
                self.visit_type_expr(ann, TypePos::Root);
            }
            if let Some(default) = &p.default {
                self.visit_expr(default);
            }
        }
        if let Some(v) = &params.vararg {
            if let Some(ann) = &v.annotation {
                self.visit_type_expr(ann, TypePos::Root);
            }
        }
        if let Some(k) = &params.kwarg {
            if let Some(ann) = &k.annotation {
                self.visit_type_expr(ann, TypePos::Root);
            }
        }
    }

    fn visit_type_param(&mut self, tp: &TypeParam) {
        match tp {
            TypeParam::TypeVar(tv) => {
                if let Some(b) = &tv.bound {
                    self.visit_type_expr(b, TypePos::Root);
                }
                if let Some(d) = &tv.default {
                    self.visit_type_expr(d, TypePos::Root);
                }
            }
            TypeParam::TypeVarTuple(tvt) => {
                if let Some(d) = &tvt.default {
                    self.visit_type_expr(d, TypePos::Root);
                }
            }
            TypeParam::ParamSpec(ps) => {
                if let Some(d) = &ps.default {
                    self.visit_type_expr(d, TypePos::Root);
                }
            }
        }
    }

    fn is_known_type_subscript(&self, value: &Expr) -> bool {
        let Some(types) = self.types else {
            return false;
        };
        match value {
            Expr::Name(n) => types.subscript_is_known_type_context(n),
            Expr::Attribute(a) => match a.value.as_ref() {
                Expr::Name(base) => types.attr_base_is_type_context(base),
                _ => false,
            },
            _ => false,
        }
    }
}

impl<'ast> Visitor<'ast> for TypePosWalker<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::AnnAssign(a) => {
                self.visit_type_expr(&a.annotation, TypePos::Root);
                if let Some(v) = &a.value {
                    self.visit_expr(v);
                }
            }
            Stmt::TypeAlias(a) => {
                self.visit_type_expr(&a.value, TypePos::Root);
                if let Some(tp) = &a.type_params {
                    for p in &tp.type_params {
                        self.visit_type_param(p);
                    }
                }
            }
            Stmt::FunctionDef(f) => {
                self.visit_parameters(&f.parameters);
                if let Some(ret) = &f.returns {
                    self.visit_type_expr(ret, TypePos::Root);
                }
                if let Some(tp) = &f.type_params {
                    for p in &tp.type_params {
                        self.visit_type_param(p);
                    }
                }
                for s in &f.body {
                    self.visit_stmt(s);
                }
            }
            Stmt::ClassDef(c) => {
                if let Some(arguments) = &c.arguments {
                    for base in &arguments.args {
                        self.visit_type_expr(base, TypePos::Root);
                    }
                    for kw in &arguments.keywords {
                        // class kwargs (e.g. `metaclass=Meta`) are runtime
                        // values, not type expressions — walk normally
                        self.visit_expr(&kw.value);
                    }
                }
                if let Some(tp) = &c.type_params {
                    for p in &tp.type_params {
                        self.visit_type_param(p);
                    }
                }
                for s in &c.body {
                    self.visit_stmt(s);
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // value-position type application: `list[T]` outside annotation
        if let Expr::Subscript(s) = expr {
            if self.is_known_type_subscript(&s.value) {
                self.visit_type_expr(expr, TypePos::Root);
                return;
            }
        }
        // `cast(T, x)` — first argument is a type position
        if let Expr::Call(c) = expr {
            if is_cast_name(&c.func) {
                if let Some(t) = c.arguments.args.first() {
                    self.visit_type_expr(t, TypePos::Root);
                }
                for arg in c.arguments.args.iter().skip(1) {
                    self.visit_expr(arg);
                }
                for kw in &c.arguments.keywords {
                    self.visit_expr(&kw.value);
                }
                return;
            }
        }
        // lambda parameter annotations (basedpython supports typed lambdas)
        if let Expr::Lambda(l) = expr {
            if let Some(params) = l.parameters.as_deref() {
                self.visit_parameters(params);
            }
            self.visit_expr(l.body.as_ref());
            return;
        }
        walk_expr(self, expr);
    }
}

fn name_matches(expr: &Expr, ident: &str) -> bool {
    match expr {
        Expr::Name(n) => n.id.as_str() == ident,
        Expr::Attribute(a) => a.attr.id.as_str() == ident,
        _ => false,
    }
}

fn is_cast_name(func: &Expr) -> bool {
    match func {
        Expr::Name(n) => n.id.as_str() == "cast",
        Expr::Attribute(a) => a.attr.id.as_str() == "cast",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_parser::{Mode, ParseOptions, parse};

    fn collect_positions(source: &str) -> Vec<(String, TypePos)> {
        struct Collector(Vec<(String, TypePos)>);
        impl TypeExprVisitor for Collector {
            fn visit(&mut self, expr: &Expr, pos: TypePos) -> Recurse {
                self.0
                    .push((format!("{expr:?}").chars().take(40).collect(), pos));
                Recurse::Descend
            }
        }

        let parsed = parse(source, ParseOptions::from(Mode::Module)).unwrap();
        let module = parsed.syntax();
        let stmts = match module {
            ruff_python_ast::Mod::Module(m) => &m.body,
            ruff_python_ast::Mod::Expression(_) => return Vec::new(),
        };
        let mut c = Collector(Vec::new());
        walk_type_positions(stmts, None, &mut c);
        c.0
    }

    #[test]
    fn ann_assign_visits_root_then_subscript() {
        let positions = collect_positions("x: list[int]\n");
        assert!(!positions.is_empty(), "expected visits, got none");
        assert_eq!(positions[0].1, TypePos::Root);
    }

    #[test]
    fn function_return_visited_at_root() {
        let positions = collect_positions("def f() -> int: pass\n");
        assert!(positions.iter().any(|(_, p)| *p == TypePos::Root));
    }

    #[test]
    fn parameter_annotation_visited_at_root() {
        let positions = collect_positions("def f(x: int): pass\n");
        assert!(positions.iter().any(|(_, p)| *p == TypePos::Root));
    }

    #[test]
    fn annotated_first_arg_is_special_pos() {
        let positions = collect_positions("x: Annotated[int, meta]\n");
        assert!(positions.iter().any(|(_, p)| *p == TypePos::AnnotatedFirst));
    }

    #[test]
    fn value_assignment_not_a_type_position() {
        let positions = collect_positions("x = 1\n");
        assert!(positions.is_empty(), "non-type stmt should yield no visits");
    }
}
