use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::Ranged;

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for zero-argument `super()` calls in `.by` source, which basedpython
/// spells as a bare name.
///
/// ## Why is this bad?
/// `super.m()` says what `super().m()` says with one call fewer to read. The
/// parentheses carry no information — python needs them only because `super` is
/// a class rather than a keyword.
///
/// ## Example
/// ```by
/// class B(A):
///     def f(self):
///         super().greet()
/// ```
///
/// Use instead:
/// ```by
/// class B(A):
///     def f(self):
///         super.greet()
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the call contains comments, which
/// the rewrite would drop.
///
/// Only a `super()` an attribute is taken on is rewritten. A bare `super()` —
/// one bound to a name, or passed as an argument — has no keyword spelling,
/// since `super` on its own names the class.
///
/// ## References
/// - [basedpython documentation: `super` keyword](https://docs.basedpython.org/features/super)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualSuperCall;

impl AlwaysFixableViolation for ManualSuperCall {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`super()` call can be written as `super`".to_string()
    }

    fn fix_title(&self) -> String {
        "Remove the parentheses".to_string()
    }
}

/// BY004
pub(crate) fn manual_super_call(checker: &Checker, attribute: &ast::ExprAttribute) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    let Expr::Call(call) = attribute.value.as_ref() else {
        return;
    };
    if !call.arguments.is_empty() {
        return;
    }
    if !checker.semantic().match_builtin_expr(&call.func, "super") {
        return;
    }

    let applicability = if checker.comment_ranges().intersects(call.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualSuperCall, call.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement("super".to_string(), call.range()),
            applicability,
        ));
}
