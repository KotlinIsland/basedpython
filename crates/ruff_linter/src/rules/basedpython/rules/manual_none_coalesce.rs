use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::helpers::contains_effect;
use ruff_python_ast::{self as ast, CmpOp, Expr};
use ruff_text_size::Ranged;

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::rules::basedpython::helpers::none_test;
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for conditional expressions in `.by` source that spell out the
/// none-coalesce operator, `??`.
///
/// ## Why is this bad?
/// `a ?? b` is `a` when `a is not None` and `b` otherwise, which is exactly what
/// the conditional expression says at three times the length. The operator also
/// evaluates its left operand once, where the conditional has to name it twice.
///
/// ## Example
/// ```by
/// name = user.display_name if user.display_name is not None else "anonymous"
/// ```
///
/// Use instead:
/// ```by
/// name = user.display_name ?? "anonymous"
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the expression contains comments,
/// which the rewrite would drop.
///
/// The rule only fires when the two occurrences of the left operand are
/// identical and free of calls, so collapsing them to one evaluation cannot
/// change what runs — unless an attribute or subscript in it is overridden to
/// have side effects.
///
/// ## References
/// - [basedpython documentation: none-coalesce operator](https://docs.basedpython.org/features/none-coalesce)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualNoneCoalesce;

impl AlwaysFixableViolation for ManualNoneCoalesce {
    #[derive_message_formats]
    fn message(&self) -> String {
        "Conditional expression can be written as `??`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `??`".to_string()
    }
}

/// BY001
pub(crate) fn manual_none_coalesce(checker: &Checker, if_exp: &ast::ExprIf) {
    if !checker.source_type.is_basedpython() {
        return;
    }

    let Some((subject, op)) = none_test(&if_exp.test) else {
        return;
    };

    // `subject if subject is not None else default`, or the same written the
    // other way around as `default if subject is None else subject`
    let subject_cmp = ComparableExpr::from(subject);
    let default = match op {
        CmpOp::IsNot if ComparableExpr::from(&*if_exp.body) == subject_cmp => &if_exp.orelse,
        CmpOp::Is if ComparableExpr::from(&*if_exp.orelse) == subject_cmp => &if_exp.body,
        _ => return,
    };

    // the operator evaluates the left operand once where the conditional
    // evaluates it twice, so the two are only interchangeable when evaluating it
    // has nothing to do
    if contains_effect(subject, |id| checker.semantic().has_builtin_binding(id)) {
        return;
    }

    let replacement = format!(
        "{} ?? {}",
        operand_source(checker, subject),
        operand_source(checker, default),
    );

    let applicability = if checker.comment_ranges().intersects(if_exp.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualNoneCoalesce, if_exp.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, if_exp.range()),
            applicability,
        ));
}

/// The source of `expr`, parenthesized when it binds looser than `??` — which
/// sits at the same precedence as the conditional expression it replaces.
fn operand_source(checker: &Checker, expr: &Expr) -> String {
    let source = checker.locator().slice(expr);
    if matches!(
        expr,
        Expr::If(_) | Expr::Lambda(_) | Expr::Named(_) | Expr::Yield(_) | Expr::YieldFrom(_)
    ) {
        format!("({source})")
    } else {
        source.to_string()
    }
}
