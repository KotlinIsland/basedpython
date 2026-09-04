use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::Ranged;

use crate::codes::Category;
use crate::checkers::ast::Checker;
use crate::{AlwaysFixableViolation, Applicability, Edit, Fix};

/// ## What it does
/// Checks for `Unpack[…]` in a `.by` type position, which basedpython spells
/// with a star.
///
/// ## Why is this bad?
/// `*Ts` is [PEP 646][pep-646]'s own spelling. basedpython accepts it on every
/// target and emits `Unpack[Ts]` itself when the target is below python 3.11,
/// so writing the helper form only pins the source to the older shape.
///
/// ## Example
/// ```by
/// def f(*args: Unpack[tuple[int, ...]]): ...
///
/// coords: tuple[Unpack[Ts]]
/// ```
///
/// Use instead:
/// ```by
/// def f(*args: *tuple[int, ...]): ...
///
/// coords: tuple[*Ts]
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the annotation contains comments,
/// which the rewrite would drop.
///
/// [pep-646]: https://peps.python.org/pep-0646/
///
/// ## References
/// - [basedpython documentation: unpack syntax](https://docs.basedpython.org/features/unpack-syntax)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualUnpackAnnotation;

impl AlwaysFixableViolation for ManualUnpackAnnotation {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`Unpack[…]` can be written as `*`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `*`".to_string()
    }
}

/// BY009
pub(crate) fn manual_unpack_annotation(checker: &Checker, subscript: &ast::ExprSubscript) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    if !checker.semantic().in_type_definition() {
        return;
    }
    if !checker
        .semantic()
        .match_typing_expr(&subscript.value, "Unpack")
    {
        return;
    }
    // `Unpack[A, B]` is not a thing, but a slice that is a tuple would render as
    // `*A, B` and mean something else
    if matches!(subscript.slice.as_ref(), Expr::Tuple(_) | Expr::Slice(_)) {
        return;
    }
    if !star_fits(checker, subscript) {
        return;
    }

    let applicability = if checker.comment_ranges().intersects(subscript.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    let replacement = format!("*{}", checker.locator().slice(subscript.slice.range()));

    checker
        .report_diagnostic(ManualUnpackAnnotation, subscript.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, subscript.range()),
            applicability,
        ));
}

/// True where a starred type is spellable: inside a subscript slice, or as the
/// annotation of a `*args`.
///
/// `**kwargs: Unpack[Movie]` is [PEP 692][pep-692]'s keyword-argument form,
/// which has no starred spelling — a star there means the variadic *positional*
/// pack.
///
/// [pep-692]: https://peps.python.org/pep-0692/
fn star_fits(checker: &Checker, subscript: &ast::ExprSubscript) -> bool {
    match checker.semantic().current_expression_parent() {
        Some(Expr::Subscript(parent)) => parent.slice.range().contains_range(subscript.range()),
        Some(Expr::Tuple(_)) => true,
        Some(_) => false,
        None => {
            let Stmt::FunctionDef(function) = checker.semantic().current_statement() else {
                return false;
            };
            function
                .parameters
                .vararg
                .as_ref()
                .and_then(|vararg| vararg.annotation.as_ref())
                .is_some_and(|annotation| annotation.range() == subscript.range())
        }
    }
}
