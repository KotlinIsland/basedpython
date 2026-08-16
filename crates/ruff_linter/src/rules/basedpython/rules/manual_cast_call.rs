use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast as ast;
use ruff_text_size::Ranged;

use crate::checkers::ast::Checker;
use crate::rules::basedpython::helpers::{comparison_fits, comparison_operand_source};
use crate::{AlwaysFixableViolation, Applicability, Edit, Fix};

/// ## What it does
/// Checks for `typing.cast` calls in `.by` source, which basedpython spells with
/// the `cast` infix keyword.
///
/// ## Why is this bad?
/// `x cast T` reads in the order it happens — the value, then the type it is
/// being taken as — where the call puts the type first and buries the value.
///
/// ## Example
/// ```by
/// from typing import cast
///
/// n = cast(int, value)
/// ```
///
/// Use instead:
/// ```by
/// n = value cast! int
/// ```
///
/// ## Fix safety
/// This rule's fix is always marked as unsafe, because the two do not do the
/// same thing at runtime. `typing.cast` returns its argument untouched, while
/// `cast!` is [checked](https://docs.basedpython.org/features/checked-cast) and
/// raises on a value that is not of the named type. That is the stronger
/// guarantee, but it is a new way for the program to fail — code that relied on
/// the cast being a lie will now raise. `cast?` is the variant that yields
/// `None` instead.
///
/// The fix writes `cast!` rather than the plain `cast` because a `typing.cast`
/// is nearly always a downcast, which the unsuffixed keyword rejects.
///
/// The fix does not remove the now-unused `from typing import cast`; `F401`
/// reports it.
///
/// ## References
/// - [basedpython documentation: `cast` keyword](https://docs.basedpython.org/features/cast)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10")]
pub(crate) struct ManualCastCall;

impl AlwaysFixableViolation for ManualCastCall {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`cast` call can be written as the `cast!` keyword".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `cast!`".to_string()
    }
}

/// BY020
pub(crate) fn manual_cast_call(checker: &Checker, call: &ast::ExprCall) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    // the keyword parses to a call of its own, so it would otherwise report the
    // rewrite it just made
    if call.cast_kind.is_some() {
        return;
    }
    if !checker.semantic().match_typing_expr(&call.func, "cast") {
        return;
    }
    let [class, value] = &*call.arguments.args else {
        return;
    };
    if !call.arguments.keywords.is_empty() {
        return;
    }

    // the keyword's operands sit at the same precedence as a comparison's
    let replacement = format!(
        "{} cast! {}",
        comparison_operand_source(checker, value),
        comparison_operand_source(checker, class),
    );
    let replacement =
        if comparison_fits(call.range(), checker.semantic().current_expression_parent()) {
            replacement
        } else {
            format!("({replacement})")
        };

    checker
        .report_diagnostic(ManualCastCall, call.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, call.range()),
            Applicability::Unsafe,
        ));
}
