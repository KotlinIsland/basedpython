//! AST rewrite for the `??` (none-coalesce) operator.
//!
//! Replaces the text-edit pass in `crate::transforms::coalesce` for the
//! cases where embedding the LHS source verbatim caused inner basedpython
//! constructs to leak.
//!
//! Rewrites:
//!
//! - `a ?? b`   →  `a if a is not None else b` (when `a` is side-effect-free)
//! - `a ?? b`   →  `_t if (_t := a) is not None else b` (otherwise)
//! - non-`None` literal LHS short-circuits to the LHS alone (no comparison)
//!
//! Post-order: child `??` operands are rewritten first, so chained
//! `x ?? x ?? x ?? "fallback"` is fully expanded.

use std::cell::Cell;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::transformer::{Transformer, walk_expr};
use ruff_python_ast::{
    AtomicNodeIndex, CmpOp, Expr, ExprCompare, ExprContext, ExprIf, ExprName, ExprNamed,
    ExprNoneLiteral, Operator, Stmt,
};
use ruff_text_size::TextRange;

pub(crate) struct CoalesceFold {
    changed: Cell<bool>,
    ever_changed: Cell<bool>,
    engaged: Cell<bool>,
}

impl CoalesceFold {
    pub(crate) fn new() -> Self {
        Self {
            changed: Cell::new(false),
            ever_changed: Cell::new(false),
            engaged: Cell::new(false),
        }
    }

    pub(crate) fn changed_cell(&self) -> &Cell<bool> {
        &self.changed
    }

    #[allow(dead_code)]
    pub(crate) fn ever_changed(&self) -> bool {
        self.ever_changed.get()
    }
}

impl Transformer for CoalesceFold {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        let prev = self.engaged.get();
        self.engaged.set(statement_needs_ast_rewrite(stmt));
        ruff_python_ast::visitor::transformer::walk_stmt(self, stmt);
        self.engaged.set(prev);
    }

    fn visit_expr(&self, expr: &mut Expr) {
        walk_expr(self, expr);

        if !self.engaged.get() {
            return;
        }

        let Expr::BinOp(b) = expr else { return };
        if !matches!(b.op, Operator::Coalesce) {
            return;
        }
        if contains_optional_attribute(&b.left) {
            return;
        }

        let placeholder = || {
            Expr::NoneLiteral(ExprNoneLiteral {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
            })
        };
        let left = std::mem::replace(b.left.as_mut(), placeholder());
        let right = std::mem::replace(b.right.as_mut(), placeholder());

        let new_expr = build_coalesce(left, right);
        *expr = new_expr;
        self.changed.set(true);
        self.ever_changed.set(true);
    }
}

/// Return true when this statement contains a `??` configuration that the
/// legacy text-edit `coalesce` transform can't lower correctly. Specifically:
/// - chained `??` (`a ?? b ?? c`) — text edit emits LHS source verbatim,
///   stranding the inner `??`
/// - `??` inside a function parameter default — text-edit visitor doesn't
///   descend into Parameters
///
/// When this returns false, the AST pass is a no-op for the statement and
/// the text-edit transform owns the rewrite. This preserves the legacy
/// `?.` + `??` interaction (`transforms::coalesce::expand_none_chain`)
/// which the AST pass can't currently reproduce (ruff's [`Generator`]
/// doesn't emit the basedpython `?.` form)
fn statement_needs_ast_rewrite(stmt: &Stmt) -> bool {
    let mut state = State {
        in_parameter_default: false,
        needs: false,
    };
    scan_stmt(stmt, &mut state);
    state.needs
}

struct State {
    in_parameter_default: bool,
    needs: bool,
}

fn scan_stmt(stmt: &Stmt, state: &mut State) {
    if state.needs {
        return;
    }
    match stmt {
        Stmt::FunctionDef(f) => {
            for p in f
                .parameters
                .posonlyargs
                .iter()
                .chain(f.parameters.args.iter())
                .chain(f.parameters.kwonlyargs.iter())
            {
                if let Some(d) = p.default.as_deref() {
                    let prev = state.in_parameter_default;
                    state.in_parameter_default = true;
                    scan_expr(d, state);
                    state.in_parameter_default = prev;
                }
                if let Some(ann) = p.parameter.annotation.as_deref() {
                    scan_expr(ann, state);
                }
            }
            if let Some(ret) = f.returns.as_deref() {
                scan_expr(ret, state);
            }
            for s in &f.body {
                scan_stmt(s, state);
            }
            for dec in &f.decorator_list {
                scan_expr(&dec.expression, state);
            }
        }
        Stmt::ClassDef(c) => {
            if let Some(args) = c.arguments.as_deref() {
                for a in &args.args {
                    scan_expr(a, state);
                }
            }
            for s in &c.body {
                scan_stmt(s, state);
            }
        }
        Stmt::Expr(e) => scan_expr(&e.value, state),
        Stmt::Assign(a) => {
            for t in &a.targets {
                scan_expr(t, state);
            }
            scan_expr(&a.value, state);
        }
        Stmt::AnnAssign(a) => {
            scan_expr(&a.target, state);
            scan_expr(&a.annotation, state);
            if let Some(v) = a.value.as_deref() {
                scan_expr(v, state);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = r.value.as_deref() {
                scan_expr(v, state);
            }
        }
        Stmt::If(i) => {
            scan_expr(&i.test, state);
            for s in &i.body {
                scan_stmt(s, state);
            }
            for e in &i.elif_else_clauses {
                if let Some(t) = e.test.as_ref() {
                    scan_expr(t, state);
                }
                for s in &e.body {
                    scan_stmt(s, state);
                }
            }
        }
        Stmt::For(f) => {
            scan_expr(&f.iter, state);
            for s in &f.body {
                scan_stmt(s, state);
            }
            for s in &f.orelse {
                scan_stmt(s, state);
            }
        }
        Stmt::While(w) => {
            scan_expr(&w.test, state);
            for s in &w.body {
                scan_stmt(s, state);
            }
            for s in &w.orelse {
                scan_stmt(s, state);
            }
        }
        Stmt::With(w) => {
            for item in &w.items {
                scan_expr(&item.context_expr, state);
            }
            for s in &w.body {
                scan_stmt(s, state);
            }
        }
        Stmt::Try(t) => {
            for s in &t.body {
                scan_stmt(s, state);
            }
            for s in &t.orelse {
                scan_stmt(s, state);
            }
            for s in &t.finalbody {
                scan_stmt(s, state);
            }
        }
        _ => {}
    }
}

fn scan_expr(expr: &Expr, state: &mut State) {
    if state.needs {
        return;
    }
    // only `??` inside a function parameter default needs the AST rewrite: the
    // text-edit `coalesce` transform's visitor doesn't descend into Parameters.
    // chained `??` in statement bodies is handled by that text-edit pass
    // (recursively), which — unlike a whole-statement AST re-render — composes
    // with sibling text-lowered syntax (`int?` annotations, `?.`, …) in the
    // same statement
    if let Expr::BinOp(b) = expr
        && matches!(b.op, Operator::Coalesce)
        && !contains_optional_attribute(&b.left)
        && state.in_parameter_default
    {
        state.needs = true;
        return;
    }
    match expr {
        Expr::BinOp(b) => {
            scan_expr(&b.left, state);
            scan_expr(&b.right, state);
        }
        Expr::UnaryOp(u) => scan_expr(&u.operand, state),
        Expr::If(i) => {
            scan_expr(&i.test, state);
            scan_expr(&i.body, state);
            scan_expr(&i.orelse, state);
        }
        Expr::Call(c) => {
            scan_expr(&c.func, state);
            for a in &c.arguments.args {
                scan_expr(a, state);
            }
            for kw in &c.arguments.keywords {
                scan_expr(&kw.value, state);
            }
        }
        Expr::Subscript(s) => {
            scan_expr(&s.value, state);
            scan_expr(&s.slice, state);
        }
        Expr::Attribute(a) => scan_expr(&a.value, state),
        Expr::Tuple(t) => {
            for e in &t.elts {
                scan_expr(e, state);
            }
        }
        Expr::List(l) => {
            for e in &l.elts {
                scan_expr(e, state);
            }
        }
        Expr::Set(s) => {
            for e in &s.elts {
                scan_expr(e, state);
            }
        }
        Expr::BoolOp(b) => {
            for e in &b.values {
                scan_expr(e, state);
            }
        }
        Expr::Lambda(l) => scan_expr(&l.body, state),
        Expr::Named(n) => scan_expr(&n.value, state),
        Expr::Compare(c) => {
            scan_expr(&c.left, state);
            for comp in &c.comparators {
                scan_expr(comp, state);
            }
        }
        _ => {}
    }
}

fn contains_optional_attribute(expr: &Expr) -> bool {
    match expr {
        Expr::Attribute(a) => a.optional || contains_optional_attribute(&a.value),
        Expr::Subscript(s) => {
            contains_optional_attribute(&s.value) || contains_optional_attribute(&s.slice)
        }
        Expr::BinOp(b) => {
            contains_optional_attribute(&b.left) || contains_optional_attribute(&b.right)
        }
        Expr::UnaryOp(u) => contains_optional_attribute(&u.operand),
        Expr::Call(c) => {
            contains_optional_attribute(&c.func)
                || c.arguments.args.iter().any(contains_optional_attribute)
        }
        _ => false,
    }
}

/// Construct the `if`/`else` expression that replaces `lhs ?? rhs`.
fn build_coalesce(lhs: Expr, rhs: Expr) -> Expr {
    // literal LHS that is statically non-None collapses to the LHS alone —
    // matches the existing text-edit behaviour and avoids CPython's
    // `1 is not None` SyntaxWarning
    if is_non_none_literal(&lhs) {
        return lhs;
    }

    if is_side_effect_free(&lhs) {
        // safe to repeat: `lhs if lhs is not None else rhs`
        let cmp = compare_is_not_none(lhs.clone());
        return Expr::If(ExprIf {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            test: Box::new(cmp),
            body: Box::new(lhs),
            orelse: Box::new(rhs),
        });
    }

    // otherwise walrus into `_t`: `_t if (_t := lhs) is not None else rhs`.
    // NB: name choice mirrors the legacy text-edit transform; the surrounding
    // scope cannot have a pre-existing `_t` because the legacy pass already
    // tags those calls. AST-level scope analysis is future work
    let temp_name = "_t";
    let target = Expr::Name(ExprName {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        id: Name::from(temp_name),
        ctx: ExprContext::Store,
    });
    let read_target = Expr::Name(ExprName {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        id: Name::from(temp_name),
        ctx: ExprContext::Load,
    });
    let walrus = Expr::Named(ExprNamed {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        target: Box::new(target),
        value: Box::new(lhs),
    });
    let cmp = compare_is_not_none(walrus);
    Expr::If(ExprIf {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        test: Box::new(cmp),
        body: Box::new(read_target),
        orelse: Box::new(rhs),
    })
}

fn compare_is_not_none(left: Expr) -> Expr {
    Expr::Compare(ExprCompare {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        left: Box::new(left),
        ops: Box::new([CmpOp::IsNot]),
        comparators: Box::new([Expr::NoneLiteral(ExprNoneLiteral {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
        })]),
    })
}

/// Literals whose value cannot be `None`. `??` on these collapses to the LHS.
fn is_non_none_literal(expr: &Expr) -> bool {
    match expr {
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::FString(_) => true,
        Expr::UnaryOp(u) => is_non_none_literal(&u.operand),
        _ => false,
    }
}

/// Conservative side-effect check. A Name read or member access against a
/// Name is safe to evaluate twice; anything that could call user code (Call,
/// Subscript, Yield, ...) goes through the walrus form
fn is_side_effect_free(expr: &Expr) -> bool {
    match expr {
        Expr::Name(_) | Expr::NoneLiteral(_) | Expr::EllipsisLiteral(_) => true,
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::BooleanLiteral(_) => true,
        Expr::Attribute(a) => is_side_effect_free(&a.value),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transforms::ast_driver::render_stmt;
    use ruff_python_ast::PySourceType;
    use ruff_python_ast::visitor::transformer::Transformer;
    use ruff_python_parser::parse_unchecked_source;

    fn rewrite(source: &str) -> String {
        let parsed = parse_unchecked_source(source, PySourceType::BasedPython);
        let mut module = parsed.into_syntax();
        let pass = CoalesceFold::new();
        let mut any_changed = false;
        for stmt in &mut module.body {
            pass.changed_cell().set(false);
            pass.visit_stmt(stmt);
            if pass.changed_cell().get() {
                any_changed = true;
            }
        }
        if !any_changed {
            return source.to_owned();
        }
        module.body.iter().map(render_stmt).collect::<String>()
    }

    // simple `??` (non-chained, not in parameter default) deliberately
    // falls through to the text-edit transform — gated by
    // `statement_needs_ast_rewrite`. The AST pass tests here cover the
    // cases where the gate fires (chained, default-arg)

    // chained `??` in a statement body is now owned by the text-edit `coalesce`
    // transform (it composes with sibling text-lowered syntax in the same
    // statement, which a whole-statement AST re-render cannot). these go through
    // the full pipeline; the AST `CoalesceFold` only fires for parameter defaults
    #[test]
    fn chained_safe_lhs_end_to_end() {
        let out = crate::transpile("x = a ?? b ?? c\n", &crate::Config::test_default()).unwrap();
        assert!(!out.contains("??"), "still has ??: {out}");
    }

    #[test]
    fn chained_coalesce_fully_expanded_end_to_end() {
        let out = crate::transpile(
            "x = a ?? b ?? c ?? \"fallback\"\n",
            &crate::Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("??"), "leftover ??: {out}");
    }

    #[test]
    fn coalesce_in_default_value() {
        let out = rewrite("def f(a = x ?? 0):\n    return a\n");
        assert!(!out.contains("??"), "leftover ??: {out}");
    }

    #[test]
    fn no_coalesce_unchanged() {
        let out = rewrite("x = a + b\n");
        assert_eq!(out, "x = a + b\n");
    }
}
