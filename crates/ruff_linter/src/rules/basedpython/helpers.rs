use ruff_python_ast::{self as ast, CmpOp, Expr};
use ruff_text_size::{Ranged, TextRange};

use crate::checkers::ast::Checker;

/// The operand and operator of a `<expr> is None` / `<expr> is not None` test,
/// which is the guard both of basedpython's optional operators lower to.
pub(super) fn none_test(test: &Expr) -> Option<(&Expr, CmpOp)> {
    let Expr::Compare(ast::ExprCompare {
        left,
        ops,
        comparators,
        ..
    }) = test
    else {
        return None;
    };
    let ([op], [Expr::NoneLiteral(_)]) = (&**ops, &**comparators) else {
        return None;
    };
    matches!(op, CmpOp::Is | CmpOp::IsNot).then(|| (&**left, *op))
}

/// True when an expression of comparison precedence can replace `node` inside
/// `parent` without being parenthesized.
///
/// A comparison binds looser than everything but the boolean operators, and it
/// *chains* — so `f() == x` becoming `a is b == x` would silently mean something
/// else. Anything not known to be safe therefore gets parentheses.
pub(super) fn comparison_fits(node: TextRange, parent: Option<&Expr>) -> bool {
    match parent {
        // the whole of a statement, a condition, a `return` value …
        None => true,
        // `and` / `or` bind looser than a comparison
        Some(Expr::BoolOp(_)) => true,
        // comma-delimited positions
        Some(
            Expr::Tuple(_)
            | Expr::List(_)
            | Expr::Set(_)
            | Expr::Dict(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::DictComp(_)
            | Expr::Generator(_),
        ) => true,
        // an argument is comma-delimited; the callee is not
        Some(Expr::Call(call)) => call.func.range() != node,
        // every branch of a conditional takes a full expression
        Some(Expr::If(_) | Expr::Lambda(_) | Expr::Named(_)) => true,
        _ => false,
    }
}

/// The source of `expr` as an operand of a comparison, parenthesized when it
/// binds looser than one.
pub(super) fn comparison_operand_source(checker: &Checker, expr: &Expr) -> String {
    let source = checker.locator().slice(expr.range());
    if matches!(
        expr,
        Expr::BoolOp(_)
            | Expr::Compare(_)
            | Expr::If(_)
            | Expr::Lambda(_)
            | Expr::Named(_)
            | Expr::Starred(_)
            | Expr::Tuple(_)
            | Expr::Yield(_)
            | Expr::YieldFrom(_)
    ) {
        format!("({source})")
    } else {
        source.to_string()
    }
}
