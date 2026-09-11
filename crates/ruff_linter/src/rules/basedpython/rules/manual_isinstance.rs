use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::{self as ast, Expr, Operator, UnaryOp};
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
/// This rule's fix is always marked as unsafe. `isinstance` runs its check every
/// time, while a type test the value's static type already decides is emitted as
/// its answer: on a parameter annotated `int`, `isinstance(x, int)` still rejects
/// a caller that passes a `str`, but `x is int` is `True`. The two agree only
/// where the annotations hold at runtime, which is exactly what a guard like this
/// is written to check. The rewrite also drops any comments inside the call.
///
/// A call whose second argument is a tuple of classes is reported without a fix:
/// the keyword accepts one, but a tuple written where a type is expected reads
/// as a [tuple type](https://docs.basedpython.org/features/tuple-types) rather
/// than as several classes.
///
/// ## References
/// - [basedpython documentation: type tests and identity](https://docs.basedpython.org/features/identity-swap)
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

/// whether `classinfo` is something a type expression can name — a name, a
/// dotted name, a subscript of one, or a `|` union of those.
///
/// `isinstance` takes a runtime value, and most of what can stand there is not
/// a type expression: a tuple means the tuple *type* rather than a choice of
/// classes, and `type(y)` or `registry["a"]` mean nothing there at all.
fn names_a_type(classinfo: &Expr) -> bool {
    match classinfo {
        Expr::Name(_) => true,
        Expr::Attribute(attribute) => names_a_type(&attribute.value),
        Expr::Subscript(subscript) => names_a_type(&subscript.value),
        Expr::BinOp(binop) if binop.op == Operator::BitOr => {
            names_a_type(&binop.left) && names_a_type(&binop.right)
        }
        _ => false,
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
    // `is` takes a *type expression*, so the rewrite is only available when the
    // classinfo argument is something one can say. a tuple reads as the tuple
    // type rather than as a choice of classes; a call or a subscript of a value
    // is not a type expression at all
    if !names_a_type(class) {
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
    checker
        .report_diagnostic(ManualIsinstance, call)
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, range),
            Applicability::Unsafe,
        ));
}
