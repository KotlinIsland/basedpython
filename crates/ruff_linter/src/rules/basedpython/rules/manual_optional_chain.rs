use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::helpers::contains_effect;
use ruff_python_ast::{self as ast, CmpOp, Expr};
use ruff_text_size::{Ranged, TextRange};

use crate::checkers::ast::Checker;
use crate::rules::basedpython::helpers::none_test;
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for conditional expressions in `.by` source that spell out an
/// optional chain, `?.`.
///
/// ## Why is this bad?
/// `a?.b` short-circuits to `None` when `a is None` and evaluates `a.b`
/// otherwise, which is what the conditional expression says the long way — it is
/// the exact form `?.` lowers to. A chain also keeps reading left to right as it
/// grows, where the conditional pushes the guard further from the access it
/// guards with every link.
///
/// ## Example
/// ```by
/// city = None if user is None else user.address.city
/// ```
///
/// Use instead:
/// ```by
/// city = user?.address.city
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the expression contains comments,
/// which the rewrite would drop. No fix is offered when the guarded access is
/// not written immediately after its receiver, since the `?` has nowhere to go.
///
/// ## References
/// - [basedpython documentation: optional chaining](https://docs.basedpython.org/features/optional-chaining)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10")]
pub(crate) struct ManualOptionalChain;

impl AlwaysFixableViolation for ManualOptionalChain {
    #[derive_message_formats]
    fn message(&self) -> String {
        "Conditional expression can be written as `?.`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `?.`".to_string()
    }
}

/// BY002
pub(crate) fn manual_optional_chain(checker: &Checker, if_exp: &ast::ExprIf) {
    if !checker.source_type.is_basedpython() {
        return;
    }

    let Some((receiver, op)) = none_test(&if_exp.test) else {
        return;
    };

    // `None if receiver is None else <chain>`, or the same written the other way
    // around as `<chain> if receiver is not None else None`
    let chain = match op {
        CmpOp::Is if if_exp.body.is_none_literal_expr() => &if_exp.orelse,
        CmpOp::IsNot if if_exp.orelse.is_none_literal_expr() => &if_exp.body,
        _ => return,
    };

    // the chain has to be rooted at the very expression the guard tests, and the
    // guard has to be the only thing evaluating it — `?.` evaluates its receiver
    // once
    let Some(root) = guarded_receiver(chain, receiver) else {
        return;
    };
    if contains_effect(receiver, |id| checker.semantic().has_builtin_binding(id)) {
        return;
    }

    // `?` goes between the receiver and the `.` that follows it, so there has to
    // be nothing between them
    let trailers = checker
        .locator()
        .slice(TextRange::new(root.end(), chain.end()));
    if !trailers.starts_with('.') {
        return;
    }

    let replacement = format!("{}?{trailers}", checker.locator().slice(root.range()));

    let applicability = if checker.comment_ranges().intersects(if_exp.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualOptionalChain, if_exp.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, if_exp.range()),
            applicability,
        ));
}

/// The occurrence of `receiver` inside `chain` that the `?` attaches to: the
/// outermost one that a plain attribute access is taken on, since that is the
/// step `a?.b` guards. `None` when the chain does not reach `receiver` that way,
/// or when it is already optional along the way.
fn guarded_receiver<'a>(chain: &'a Expr, receiver: &Expr) -> Option<&'a Expr> {
    let receiver = ComparableExpr::from(receiver);
    let mut current = chain;
    loop {
        let (inner, is_attribute) = match current {
            Expr::Attribute(attribute) if attribute.optional => return None,
            Expr::Attribute(attribute) => (&*attribute.value, true),
            Expr::Subscript(subscript) => (&*subscript.value, false),
            Expr::Call(call) => (&*call.func, false),
            _ => return None,
        };
        if is_attribute && ComparableExpr::from(inner) == receiver {
            return Some(inner);
        }
        current = inner;
    }
}
