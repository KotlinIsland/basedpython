use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast as ast;
use ruff_text_size::Ranged;

use crate::codes::Category;
use crate::checkers::ast::Checker;
use crate::{AlwaysFixableViolation, Applicability, Fix, fix};

/// ## What it does
/// Checks for `from typing import …` in `.by` source naming only members that
/// are already implicitly available.
///
/// ## Why is this bad?
/// basedpython makes the `typing` members that describe a type available without
/// an import, and emits the matching `from typing import …` itself. An import
/// written by hand says nothing the file did not already have.
///
/// ## Example
/// ```by
/// from typing import Mapping, Sequence
///
/// def f(a: Mapping[str, int]) -> Sequence[int]: ...
/// ```
///
/// Use instead:
/// ```by
/// def f(a: Mapping[str, int]) -> Sequence[int]: ...
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the statement contains comments,
/// which the rewrite would drop.
///
/// Only a statement whose names are *all* implicit is reported, and an aliased
/// or re-exported name never is: `from typing import Sequence as Seq` binds a
/// name nothing else would, and `from typing export Sequence` is a deliberate
/// part of the module's api. Names basedpython has dedicated syntax for —
/// `Callable`, `Final`, `Literal`, `Protocol`, `TypeVar` — are not implicit and
/// are left alone here.
///
/// ## References
/// - [basedpython documentation: implicit typing imports](https://docs.basedpython.org/features/implicit-typing)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct RedundantTypingImport;

impl AlwaysFixableViolation for RedundantTypingImport {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`typing` members are implicitly available".to_string()
    }

    fn fix_title(&self) -> String {
        "Remove the import".to_string()
    }
}

/// BY012
pub(crate) fn redundant_typing_import(checker: &Checker, import_from: &ast::StmtImportFrom) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    if import_from.is_export || import_from.level != 0 {
        return;
    }
    if import_from
        .module
        .as_ref()
        .is_none_or(|module| module.id != "typing")
    {
        return;
    }
    if !import_from.names.iter().all(|alias| {
        alias.asname.is_none()
            && ruff_python_stdlib::basedpython::is_implicit_typing_name(&alias.name.id)
    }) {
        return;
    }

    let applicability = if checker.comment_ranges().intersects(import_from.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    let stmt = checker.semantic().current_statement();
    let parent = checker.semantic().current_statement_parent();
    let edit = fix::edits::delete_stmt(stmt, parent, checker.locator(), checker.indexer());

    checker
        .report_diagnostic(RedundantTypingImport, import_from.range())
        .set_fix(Fix::applicable_edit(edit, applicability));
}
