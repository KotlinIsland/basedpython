use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::{self as ast, Expr};
use ruff_text_size::Ranged;

use crate::codes::Category;
use crate::checkers::ast::Checker;
use crate::{AlwaysFixableViolation, Applicability, Edit, Fix};

/// ## What it does
/// Checks for a `Sentinel(…)` construction in `.by` source, which basedpython
/// declares with the `sentinel` keyword.
///
/// ## Why is this bad?
/// `sentinel MISSING` says what the assignment says without naming `MISSING`
/// twice — once as the binding and once as the string the sentinel reports as
/// its own name, which is a pair that can drift apart.
///
/// ## Example
/// ```by
/// from typing_extensions import Sentinel
///
/// MISSING = Sentinel("MISSING")
/// ```
///
/// Use instead:
/// ```by
/// sentinel MISSING
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the statement contains comments,
/// which the rewrite would drop.
///
/// Only a module-level assignment whose string argument matches the name it is
/// bound to is reported — `sentinel` declares the two together, so a pair that
/// already disagrees is left for a reader to resolve. The fix does not remove
/// the now-unused import; `F401` reports it.
///
/// ## References
/// - [basedpython documentation: sentinel](https://docs.basedpython.org/features/sentinel)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualSentinel;

impl AlwaysFixableViolation for ManualSentinel {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`Sentinel` assignment can be written as `sentinel`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `sentinel`".to_string()
    }
}

/// BY019
pub(crate) fn manual_sentinel(checker: &Checker, assign: &ast::StmtAssign) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    // `sentinel` declares a module-level name
    if !checker.semantic().current_scope().kind.is_module() {
        return;
    }

    let [Expr::Name(target)] = &*assign.targets else {
        return;
    };
    let Expr::Call(call) = assign.value.as_ref() else {
        return;
    };
    if !call.arguments.keywords.is_empty() {
        return;
    }
    let [Expr::StringLiteral(name)] = &*call.arguments.args else {
        return;
    };
    if name.value.to_str() != target.id.as_str() {
        return;
    }
    if !checker
        .semantic()
        .resolve_qualified_name(&call.func)
        .is_some_and(|qualified_name| {
            matches!(
                qualified_name.segments(),
                ["typing" | "typing_extensions", "Sentinel"]
            )
        })
    {
        return;
    }

    let applicability = if checker.comment_ranges().intersects(assign.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualSentinel, assign.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(format!("sentinel {}", target.id), assign.range()),
            applicability,
        ));
}
