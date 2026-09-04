use itertools::Itertools;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast as ast;
use ruff_text_size::Ranged;

use crate::codes::Category;
use crate::checkers::ast::Checker;
use crate::{AlwaysFixableViolation, Applicability, Edit, Fix};

/// ## What it does
/// Checks for the `import name as name` re-export convention in `.by` source,
/// which basedpython spells `export`.
///
/// ## Why is this bad?
/// `from x import y as y` is python's way of saying that `y` is deliberately
/// part of this module's public api rather than something it happens to need.
/// `from x export y` says it without writing the name twice, and reads as the
/// declaration it is.
///
/// ## Example
/// ```by
/// from .models import Widget as Widget
/// ```
///
/// Use instead:
/// ```by
/// from .models export Widget
/// ```
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when the statement contains comments,
/// which the rewrite drops along with the line structure of a parenthesized
/// import.
///
/// `export` applies to the whole statement, so only an import whose names are
/// *all* self-aliased is reported.
///
/// ## References
/// - [basedpython documentation: export imports](https://docs.basedpython.org/features/export-imports)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualReExport;

impl AlwaysFixableViolation for ManualReExport {
    #[derive_message_formats]
    fn message(&self) -> String {
        "`import name as name` can be written as `export`".to_string()
    }

    fn fix_title(&self) -> String {
        "Replace with `export`".to_string()
    }
}

/// BY011
pub(crate) fn manual_re_export(checker: &Checker, import_from: &ast::StmtImportFrom) {
    if !checker.source_type.is_basedpython() {
        return;
    }
    if import_from.is_export {
        return;
    }
    // a `*` import binds no names to re-export, and an empty list cannot happen
    if import_from.names.is_empty() {
        return;
    }
    if !import_from.names.iter().all(|alias| {
        alias
            .asname
            .as_ref()
            .is_some_and(|asname| asname.id == alias.name.id)
    }) {
        return;
    }

    let dots = ".".repeat(import_from.level as usize);
    let module = import_from
        .module
        .as_ref()
        .map_or("", ast::Identifier::as_str);
    let names = import_from
        .names
        .iter()
        .map(|alias| &alias.name.id)
        .join(", ");
    let replacement = format!("from {dots}{module} export {names}");

    let applicability = if checker.comment_ranges().intersects(import_from.range()) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(ManualReExport, import_from.range())
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(replacement, import_from.range()),
            applicability,
        ));
}
