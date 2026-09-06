use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{self as ast, Expr};
use ruff_python_semantic::SemanticModel;
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::{AlwaysFixableViolation, Applicability, Edit, Fix};

/// ## What it does
/// Checks for a subscripted `tuple` in a `.by` type position, which
/// basedpython spells as a parenthesized list of element types.
///
/// ## Why is this bad?
/// `(int, str)` is basedpython's own tuple type, and reads as the value it
/// describes. `tuple[int, str]` is the python spelling of the same type, so
/// writing it only makes the annotation longer.
///
/// ## Example
/// ```by
/// point: tuple[int, int]
///
/// def head(xs: list[tuple[str, int]]) -> tuple[str]: ...
/// ```
///
/// Use instead:
/// ```by
/// point: (int, int)
///
/// def head(xs: list[(str, int)]) -> (str,): ...
/// ```
///
/// A homogeneous `tuple[T, ...]` has no parenthesized form — basedpython
/// spells it with the [variadic](https://docs.basedpython.org/features/tuple-types)
/// `(*: T)` — so this rule leaves it alone.
///
/// ## Fix safety
/// Only the `tuple[` and the closing `]` are rewritten, so the elements keep
/// their layout. The fix is marked as unsafe when the annotation contains a
/// comment: a tuple type is re-rendered when it is lowered, so the comment
/// stays in the `.by` source but no longer reaches the transpiled python.
///
/// Only the builtin `tuple` is reported. `typing.Tuple` is left to `UP006`,
/// which rewrites it to the builtin; this rule then reports what that produced.
///
/// ## References
/// - [basedpython documentation: tuple type literals](https://docs.basedpython.org/features/tuple-types)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualTupleAnnotation;

impl AlwaysFixableViolation for ManualTupleAnnotation {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`tuple[…]` can be written as `(…)`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `(…)`".to_string()
    }
}

/// BY023
pub(crate) fn manual_tuple_annotation(checker: &Checker, subscript: &ast::ExprSubscript) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    if !in_tuple_type_position(checker.semantic()) {
        return;
    }
    if !checker
        .semantic()
        .match_builtin_expr(&subscript.value, "tuple")
    {
        return;
    }
    let Some(elements) = tuple_elements(&subscript.slice) else {
        return;
    };
    let Some(last) = elements.last() else {
        return;
    };

    let Some(open_end) = open_bracket_end(checker, subscript) else {
        return;
    };
    let close_start = subscript.end() - TextSize::from(1);
    let head = TextRange::new(subscript.start(), open_end);
    let tail = TextRange::new(close_start, subscript.end());

    // `tuple[int]` → `(int,)`: a lone element needs the trailing comma to tell
    // the tuple type apart from a parenthesized expression. an unpacked element
    // (`tuple[*A]`) is unambiguous without one, but the formatter writes the
    // comma there too, so the rewrite matches what a formatted file looks like
    let closing = if elements.len() == 1
        && !has_trailing_comma(checker, TextRange::new(last.end(), close_start))
    {
        ",)"
    } else {
        ")"
    };

    // a tuple type is re-rendered when it is lowered, so a comment among the
    // elements survives in the `.by` source but not in the transpiled python
    let applicability = if checker.comment_ranges().intersects(subscript.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualTupleAnnotation, subscript.range())
        .set_fix(Fix::applicable_edits(
            Edit::range_replacement("(".to_string(), head),
            [Edit::range_replacement(closing.to_string(), tail)],
            applicability,
        ));
}

/// True in the type positions where basedpython reads a parenthesized tuple as
/// a tuple type: an annotation, either spelling of a type alias value, and a
/// PEP 695 type parameter's bound or default.
///
/// A class base is deliberately excluded. It is a runtime value position, where
/// `class C((str, int))` is a plain tuple literal that raises `TypeError`,
/// unlike `class C(tuple[str, int])`. A string annotation is excluded too: the
/// rewrite inside the quotes would never be lowered, leaving the emitted python
/// with a tuple expression where a type belongs.
fn in_tuple_type_position(semantic: &SemanticModel) -> bool {
    if semantic.in_string_type_definition() {
        return false;
    }
    semantic.in_annotation()
        || semantic.in_type_alias_value()
        || semantic.in_type_param_definition()
}

/// The elements of a `tuple[…]` slice, or `None` when there is no rewrite to
/// make — either because the subscript has no parenthesized basedpython form,
/// or because the slice is already written as a parenthesized tuple.
fn tuple_elements(slice: &Expr) -> Option<Vec<&Expr>> {
    let elements = match slice {
        // a slice already written as a parenthesized tuple: `tuple[(int, str)]`,
        // and `tuple[()]`, which is how the empty tuple is spelled. both have a
        // basedpython form, but the parentheses the rewrite would add are
        // already there, so there is nothing to report
        Expr::Tuple(tuple) if tuple.parenthesized => return None,
        Expr::Tuple(tuple) => tuple.elts.iter().collect::<Vec<_>>(),
        other => vec![other],
    };
    if elements.is_empty() {
        return None;
    }
    // a homogeneous `tuple[T, ...]` is spelled `(*: T)`, not with a
    // parenthesized list. an ellipsis anywhere else is not a type at all, so
    // neither form applies
    if elements
        .iter()
        .any(|element| matches!(element, Expr::EllipsisLiteral(_) | Expr::Slice(_)))
    {
        return None;
    }
    Some(elements)
}

/// The offset just past the `[` opening a subscript's slice.
fn open_bracket_end(checker: &Checker, subscript: &ast::ExprSubscript) -> Option<TextSize> {
    checker
        .tokens()
        .in_range(TextRange::new(subscript.value.end(), subscript.end()))
        .iter()
        .find(|token| token.kind() == TokenKind::Lsqb)
        .map(Ranged::end)
}

/// Whether a `,` already sits in `range`, which spans from the last element to
/// the subscript's `]`.
///
/// Read from the tokens rather than the text, so that a comma inside a trailing
/// comment is not mistaken for the tuple's own — appending a second one would
/// leave `(int, ,)`.
fn has_trailing_comma(checker: &Checker, range: TextRange) -> bool {
    checker
        .tokens()
        .in_range(range)
        .iter()
        .any(|token| token.kind() == TokenKind::Comma)
}
