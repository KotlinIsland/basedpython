use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::helpers::contains_effect;
use ruff_python_ast::{self as ast, Operator};
use ruff_text_size::Ranged;

use crate::codes::Category;
use crate::checkers::ast::Checker;
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for a none-coalesce whose fallback cannot change the result.
///
/// ## Why is this bad?
/// `a ?? None` is `a` when `a` is not `None`, and `None` — which is what `a`
/// already was — otherwise. `a ?? a` says the same thing twice. Either reads as
/// a guard that is doing something, and neither is.
///
/// ## Example
/// ```by
/// value = lookup() ?? None
/// ```
///
/// Use instead:
/// ```by
/// value = lookup()
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the expression contains comments,
/// which the rewrite would drop.
///
/// ## References
/// - [basedpython documentation: none-coalesce operator](https://docs.basedpython.org/features/none-coalesce)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct RedundantNoneCoalesce;

impl AlwaysFixableViolation for RedundantNoneCoalesce {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`??` fallback cannot change the result".to_string()
    }

    fn fix_title(&self) -> String {
        "Remove the `??`".to_string()
    }
}

/// BY101
pub(crate) fn redundant_none_coalesce(checker: &Checker, binary: &ast::ExprBinOp) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    if binary.op != Operator::Coalesce {
        return;
    }

    // `a ?? a` only collapses when evaluating `a` twice has nothing to do
    let redundant = binary.right.is_none_literal_expr()
        || (ComparableExpr::from(&*binary.left) == ComparableExpr::from(&*binary.right)
            && !contains_effect(&binary.left, |id| {
                checker.semantic().has_builtin_binding(id)
            }));
    if !redundant {
        return;
    }

    let applicability = if checker.comment_ranges().intersects(binary.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(RedundantNoneCoalesce, binary.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(
                checker.locator().slice(binary.left.range()).to_string(),
                binary.range(),
            ),
            applicability,
        ));
}
