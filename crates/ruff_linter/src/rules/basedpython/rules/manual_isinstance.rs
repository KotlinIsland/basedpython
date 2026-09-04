use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::{self as ast, Expr, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::rules::basedpython::helpers::{comparison_fits, comparison_operand_source};
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for `isinstance` calls in `.by` source, which basedpython spells with
/// the `is` keyword.
///
/// ## Why is this bad?
/// basedpython promotes the common check to a keyword: `is` is an instance test
/// and `===` is identity, the reverse of python. Writing the call keeps the
/// python reading of `is` in a file where it does not have one.
///
/// ## Example
/// ```by
/// if isinstance(x, int):
///     ...
/// if not isinstance(x, str):
///     ...
/// ```
///
/// Use instead:
/// ```by
/// if x is int:
///     ...
/// if x is not str:
///     ...
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the call contains comments, which
/// the rewrite would drop.
///
/// A call whose second argument is a tuple of classes is reported without a fix:
/// the keyword accepts one, but a tuple written where a type is expected reads
/// as a [tuple type](https://docs.basedpython.org/features/tuple-types) rather
/// than as several classes.
///
/// ## References
/// - [basedpython documentation: identity and isinstance](https://docs.basedpython.org/features/identity-swap)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualIsinstance;

impl AlwaysFixableViolation for ManualIsinstance {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`isinstance` call can be written as `is`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `is`".to_string()
    }
}

/// BY003
pub(crate) fn manual_isinstance(checker: &Checker, call: &ast::ExprCall) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    if !checker
        .semantic()
        .match_builtin_expr(&call.func, "isinstance")
    {
        return;
    }
    let [value, class] = &*call.arguments.args else {
        return;
    };
    if !call.arguments.keywords.is_empty() {
        return;
    }
    // a tuple in the class position reads as a tuple type, not as a choice of
    // classes
    if class.is_tuple_expr() {
        return;
    }

    let value = comparison_operand_source(checker, value);
    let class = comparison_operand_source(checker, class);

    // `not isinstance(x, T)` is the `is not` form, and replacing the whole
    // `not` expression is always safe: a comparison binds tighter than `not`
    let parent = checker.semantic().current_expression_parent();
    let (range, replacement) = match parent {
        Some(Expr::UnaryOp(unary)) if unary.op == UnaryOp::Not => {
            (unary.range(), format!("{value} is not {class}"))
        }
        parent if comparison_fits(call.range(), parent) => {
            (call.range(), format!("{value} is {class}"))
        }
        _ => (call.range(), format!("({value} is {class})")),
    };

    report(checker, call.range(), range, replacement);
}

fn report(checker: &Checker, call: TextRange, range: TextRange, replacement: String) {
    let applicability = if checker.comment_ranges().intersects(range) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualIsinstance, call)
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, range),
            applicability,
        ));
}
