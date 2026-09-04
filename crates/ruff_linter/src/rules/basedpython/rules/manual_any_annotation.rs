use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::Expr;
use ruff_text_size::Ranged;

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::{AlwaysFixableViolation, Applicability, Edit, Fix};

/// ## What it does
/// Checks for `typing.Any` in a `.by` type position, which basedpython spells
/// `dynamic`.
///
/// ## Why is this bad?
/// `dynamic` is the surface spelling of `Any` and needs no import — the
/// transpiler emits `from typing import Any` for you. Writing `Any` means
/// importing a name to say what a keyword already says.
///
/// ## Example
/// ```by
/// from typing import Any
///
/// def handle(payload: Any) -> Any: ...
/// ```
///
/// Use instead:
/// ```by
/// def handle(payload: dynamic) -> dynamic: ...
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the annotation contains comments,
/// which the rewrite would drop.
///
/// The fix does not remove the now-unused `from typing import Any`; `F401`
/// reports it once the last use is gone.
///
/// ## References
/// - [basedpython documentation: dynamic](https://docs.basedpython.org/features/dynamic)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualAnyAnnotation;

impl AlwaysFixableViolation for ManualAnyAnnotation {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`Any` can be written as `dynamic`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `dynamic`".to_string()
    }
}

/// BY007
pub(crate) fn manual_any_annotation(checker: &Checker, expr: &Expr) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    // `dynamic` is a type-position keyword; in a value position `Any` is an
    // ordinary name with no keyword spelling
    if !checker.semantic().in_type_definition() {
        return;
    }
    if !checker.semantic().match_typing_expr(expr, "Any") {
        return;
    }

    let applicability = if checker.comment_ranges().intersects(expr.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualAnyAnnotation, expr.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement("dynamic".to_string(), expr.range()),
            applicability,
        ));
}
