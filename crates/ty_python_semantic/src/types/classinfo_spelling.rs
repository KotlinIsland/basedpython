//! basedpython: where a union is written as the tuple of classes it stands for
//!
//! below 3.10 a union object is no class, and `isinstance` rejects one. the lowering writes a
//! union spelled in a call to the builtin `isinstance` or `issubclass` as the tuple it stands
//! for, so `isinstance(x, int | str)` runs as `isinstance(x, (int, str,))` — and the same for
//! an optional `int?`, and for either inside a tuple the call spells. a union reached any other
//! way, through a name for one, is the `typing.Union` the lowering writes everywhere else
//!
//! which calls and which expressions in them are read as classes is decided here once, for the
//! transpiler's rewrite and for ty's report of a union `isinstance` would reject alike. the call
//! is one whose callee is the builtin, whatever the name it is reached by: ty knows the callee
//! as the function it checks the call against, and the transpiler asks
//! [`ide_support::classinfo_argument`](crate::types::ide_support::classinfo_argument)

use ruff_python_ast as ast;
use ruff_text_size::Ranged;

/// the argument `isinstance` and `issubclass` read as classes, when `call` is a call to one of
/// them
pub(crate) fn classinfo_argument(call: &ast::ExprCall) -> Option<&ast::Expr> {
    call.arguments.args.get(1)
}

/// each expression the lowering reads as classes, `classinfo` among them: every element of a
/// tuple or list it spells, and every arm of a union and operand of an optional that
/// `is_union` says builds a union object. a `|` between two numbers is an ordinary operator,
/// and nothing inside one is read as classes. outermost first
pub fn class_positions<'a>(
    classinfo: &'a ast::Expr,
    is_union: &dyn Fn(&ast::Expr) -> bool,
) -> Vec<&'a ast::Expr> {
    let mut positions = Vec::new();
    let mut pending = vec![classinfo];
    while let Some(expr) = pending.pop() {
        positions.push(expr);
        let inner: Vec<&ast::Expr> = match expr {
            ast::Expr::Tuple(tuple) => tuple.elts.iter().collect(),
            ast::Expr::List(list) => list.elts.iter().collect(),
            ast::Expr::BinOp(binop) if binop.op == ast::Operator::BitOr && is_union(expr) => {
                union_arms(expr)
            }
            ast::Expr::UnaryOp(optional)
                if optional.op == ast::UnaryOp::Optional && is_union(expr) =>
            {
                vec![optional.operand.as_ref()]
            }
            _ => Vec::new(),
        };
        pending.extend(inner.into_iter().rev());
    }
    positions
}

/// the arms of `a | b | c`, which parses as `(a | b) | c`
pub fn union_arms(expr: &ast::Expr) -> Vec<&ast::Expr> {
    let mut arms = Vec::new();
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        match expr {
            ast::Expr::BinOp(binop) if binop.op == ast::Operator::BitOr => {
                pending.push(&binop.right);
                pending.push(&binop.left);
            }
            _ => arms.push(expr),
        }
    }
    arms
}

/// whether the lowering writes `expr`, a union object `call` — a call to the builtin
/// `isinstance` or `issubclass` — passes, as a tuple of classes: `expr` is a `|` or a `?`
/// standing where the call reads classes
///
/// ty asks this of an expression whose type is already a union object, reached from the
/// argument through the tuples it spells, so a `|` or `?` on the way is one too
pub(crate) fn spelled_as_classes(call: &ast::ExprCall, expr: &ast::Expr) -> bool {
    let builds_union = |expr: &ast::Expr| match expr {
        ast::Expr::BinOp(binop) => binop.op == ast::Operator::BitOr,
        ast::Expr::UnaryOp(optional) => optional.op == ast::UnaryOp::Optional,
        _ => false,
    };
    builds_union(expr)
        && classinfo_argument(call).is_some_and(|classinfo| {
            class_positions(classinfo, &builds_union)
                .iter()
                .any(|position| position.range() == expr.range())
        })
}
