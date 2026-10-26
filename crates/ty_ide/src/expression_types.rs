//! the expressions a range is in, each with its type: what an editor's *type info* offers
//!
//! ```py
//! total = total_area(shapes) * 2
//! ```
//!
//! on `shapes` the answer is `shapes`, `total_area(shapes)` and `total_area(shapes) * 2`, from the
//! innermost out, each with the type ty inferred for it: `list[Shape]`, `float`, `float`. on a
//! selection of `total_area(shapes)` it starts from that call. On `kind` in
//! `kind, _, size = spec.partition(":")` it is `kind` and the tuple of targets around it, typed as
//! the tuple of what each target is bound to.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::find_node::covering_node;
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::ProgramFile;
use ty_python_semantic::types::ide_support::unpacking_target_type;
use ty_python_semantic::{HasType, SemanticModel, with_display_for_file};

use crate::Db;

/// an expression and its type
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionType {
    pub range: TextRange,
    /// the type as the file would spell it: `1` in a basedpython file, `Literal[1]` in python
    pub ty: String,
}

/// the expressions of the statement `range` is in that contain all of it, innermost first, each
/// with its type. an expression ty has no type for, such as one inside a string annotation, is
/// left out, and so is everything outside the statement: a statement has no type, and what
/// contains it is not an expression
pub fn expression_types(
    db: &dyn Db,
    file: ProgramFile<'_>,
    range: TextRange,
) -> Vec<ExpressionType> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let model = SemanticModel::new(db, file);
    let env = model.program_environment();
    let covering = covering_node(module.syntax().into(), range);

    with_display_for_file(db, file.file(db), || {
        let mut found: Vec<ExpressionType> = Vec::new();
        for node in covering.ancestors() {
            if node.is_statement() {
                break;
            }
            let Some(expression) = node.as_expr_ref() else {
                continue;
            };
            if found
                .last()
                .is_some_and(|inner| inner.range == expression.range())
            {
                continue;
            }
            // a tuple of targets has no type of its own, but is the tuple of its targets'
            let Some(ty) = expression
                .inferred_type(&model)
                .or_else(|| unpacking_target_type(&model, expression))
            else {
                continue;
            };
            found.push(ExpressionType {
                range: expression.range(),
                ty: ty.display(db, &env).to_string(),
            });
        }
        found
    })
}

#[cfg(test)]
mod tests {
    use super::expression_types;
    use crate::tests::CursorTest;
    use insta::assert_snapshot;
    use ruff_text_size::TextRange;

    fn types(test: &CursorTest, range: TextRange) -> String {
        let source = ruff_db::source::source_text(&test.db, test.cursor.file);
        expression_types(&test.db, test.program_file(test.cursor.file), range)
            .iter()
            .map(|found| format!("{} : {}", &source.as_str()[found.range], found.ty))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_caret_answers_every_expression_around_it() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
def total_area(shapes: list[int]) -> float:
    return 1.0

def main(shapes: list[int]) -> None:
    total = total_area(sha<CURSOR>pes) * 2
",
            )
            .build();

        assert_snapshot!(types(&test, TextRange::empty(test.cursor.offset)), @"
        shapes : list[int]
        total_area(shapes) : int | float
        total_area(shapes) * 2 : int | float
        ");
    }

    #[test]
    fn a_tuple_of_targets_is_the_tuple_of_what_they_are_bound_to() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
def main(text: str, numbers: list[int]) -> None:
    (kind, _, si<CURSOR>ze), (first, *rest) = text.partition(':'), numbers
",
            )
            .build();

        assert_snapshot!(types(&test, TextRange::empty(test.cursor.offset)), @"
        size : str
        (kind, _, size) : tuple[str, str, str]
        (kind, _, size), (first, *rest) : tuple[tuple[str, str, str], tuple[int, *tuple[int, ...]]]
        ");
    }

    #[test]
    fn a_selection_starts_from_the_expression_it_spans() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
def main(text: str) -> None:
    head, _, tail = <CURSOR>text.partition(':')
",
            )
            .build();
        let start = test.cursor.offset;
        let range = TextRange::new(start, start + ruff_text_size::TextSize::from(19));

        assert_snapshot!(types(&test, range), @"text.partition(':') : tuple[str, str, str]");
    }
}
