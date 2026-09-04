use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::Ranged;

use crate::codes::Category;
use crate::checkers::ast::Checker;
use crate::{AlwaysFixableViolation, Applicability, Edit, Fix};

/// ## What it does
/// Checks for `TypeOf[…]` in a `.by` type position, which basedpython spells
/// `typeof`.
///
/// ## Why is this bad?
/// `typeof x` is the keyword the subscript exists to stand in for, and it needs
/// no import from `ty_extensions`.
///
/// ## Example
/// ```by
/// from ty_extensions import TypeOf
///
/// b: int = 1
/// a: TypeOf[b] = 1
/// ```
///
/// Use instead:
/// ```by
/// b: int = 1
/// a: typeof b = 1
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the annotation contains comments,
/// which the rewrite would drop.
///
/// The fix does not remove the now-unused `from ty_extensions import TypeOf`;
/// `F401` reports it once the last use is gone.
///
/// ## References
/// - [basedpython documentation: typeof](https://docs.basedpython.org/features/typeof)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualTypeofAnnotation;

impl AlwaysFixableViolation for ManualTypeofAnnotation {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`TypeOf[…]` can be written as `typeof`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `typeof`".to_string()
    }
}

/// BY010
pub(crate) fn manual_typeof_annotation(checker: &Checker, subscript: &ast::ExprSubscript) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    if !checker.semantic().in_type_definition() {
        return;
    }
    if !checker
        .semantic()
        .resolve_qualified_name(&subscript.value)
        .is_some_and(|qualified_name| {
            matches!(qualified_name.segments(), ["ty_extensions", "TypeOf"])
        })
    {
        return;
    }
    if matches!(subscript.slice.as_ref(), Expr::Tuple(_) | Expr::Slice(_)) {
        return;
    }

    let applicability = if checker.comment_ranges().intersects(subscript.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    let replacement = format!(
        "typeof {}",
        checker.locator().slice(subscript.slice.range())
    );

    checker
        .report_diagnostic(ManualTypeofAnnotation, subscript.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, subscript.range()),
            applicability,
        ));
}
