use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for a `class` or `def` in `.by` source whose whole body is `...`.
///
/// ## Why is this bad?
/// basedpython lets a declaration be written without a body and fills the `: ...`
/// in itself, so the placeholder is only there to satisfy a python grammar this
/// file is not written in. Dropping it is most of the noise in stub-heavy code.
///
/// ## Example
/// ```by
/// class Empty: ...
///
/// def stub(x: int) -> int: ...
/// ```
///
/// Use instead:
/// ```by
/// class Empty
///
/// def stub(x: int) -> int
/// ```
///
/// ## Fix safety
/// No fix is offered when the body carries a comment or a docstring, since both
/// need a body to live in.
///
/// A stub file (`.byi`) is left alone: `...` is the convention there, and a stub
/// is read as much by tools that expect the python shape as by basedpython.
///
/// ## References
/// - [basedpython documentation: empty declarations](https://docs.basedpython.org/features/empty-declarations)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct UnnecessaryStubBody;

impl AlwaysFixableViolation for UnnecessaryStubBody {
    #[derive_message_formats]
    fn message(&self) -> String {
        "Declaration body of `...` is unnecessary".to_string()
    }

    fn fix_title(&self) -> String {
        "Remove the body".to_string()
    }
}

/// BY017
pub(crate) fn unnecessary_stub_body(checker: &Checker, stmt: &Stmt) {
    if !checker.source_type.is_basedpython() || checker.source_type.is_stub() {
        return;
    }

    let body = match stmt {
        Stmt::ClassDef(class) => &class.body,
        Stmt::FunctionDef(function) => &function.body,
        _ => return,
    };
    let [Stmt::Expr(expr)] = &**body else {
        return;
    };
    if !matches!(expr.value.as_ref(), Expr::EllipsisLiteral(_)) {
        return;
    }

    // the header ends at the `:` the body hangs off; everything from there is
    // what basedpython would have written itself
    let Some(colon) = checker
        .tokens()
        .in_range(TextRange::new(stmt.start(), body[0].start()))
        .iter()
        .rfind(|token| token.kind() == TokenKind::Colon)
    else {
        return;
    };

    let removed = TextRange::new(colon.start(), stmt.end());
    if checker.comment_ranges().intersects(removed) {
        return;
    }

    checker
        .report_diagnostic(UnnecessaryStubBody, removed)
        .set_fix(Fix::safe_edit(Edit::range_deletion(removed)));
}
