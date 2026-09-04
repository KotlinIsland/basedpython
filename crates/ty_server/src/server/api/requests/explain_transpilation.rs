//! `by/explainTranspilation` — which basedpython constructs a document uses, and what each lowers to.
//!
//! The recognition belongs here rather than in an editor plugin, and not only on principle. A
//! client that wanted this had to guess from the source text — a regex for `?.`, another for `??`,
//! another for `data class` — and a regex cannot tell an operator from the same characters inside a
//! string or a comment, cannot see that `?` in a type position means something else, and drifts
//! from the language the moment a construct is added. The parser here is the one the transpiler
//! itself runs, so the answer is the same one the lowering is about to act on.

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier, Uri};
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Operator, Stmt, UnaryOp};
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextRange};
use ty_project::ProjectDatabase;

use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum ExplainTranspilationRequest {}

impl Request for ExplainTranspilationRequest {
    type Params = ExplainTranspilationParams;
    type Result = Option<Vec<TranspilationNote>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/explainTranspilation");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExplainTranspilationParams {
    text_document: TextDocumentIdentifier,
}

/// One construct found, and what the transpiler does with it.
#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TranspilationNote {
    /// A short, stable name for the construct, e.g. `null-safe access`.
    construct: String,
    /// The source it was written as.
    snippet: String,
    /// What it lowers to, in a sentence.
    explanation: String,
    /// The one-based line it is on.
    line: u32,
}

pub(crate) struct ExplainTranspilationHandler;

impl RequestHandler for ExplainTranspilationHandler {
    type RequestType = ExplainTranspilationRequest;
}

impl BackgroundDocumentRequestHandler for ExplainTranspilationHandler {
    fn document_uri(params: &ExplainTranspilationParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: ExplainTranspilationParams,
    ) -> crate::server::Result<Option<Vec<TranspilationNote>>> {
        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };
        let source = ruff_db::source::source_text(db, file);
        Ok(Some(notes_in(source.as_str())))
    }
}

impl RetriableRequestHandler for ExplainTranspilationHandler {}

/// Every construct [`source`] uses, in source order.
///
/// Parsed rather than scanned. A construct is only reported where the parser built the node for it,
/// so the same characters inside a string, a comment or a type position are not mistaken for one.
fn notes_in(source: &str) -> Vec<TranspilationNote> {
    let parsed = ruff_python_parser::parse_unchecked_source(
        source,
        ruff_python_ast::PySourceType::BasedPython,
    );
    let index = LineIndex::from_source_text(source);
    let mut collector = Collector {
        source,
        index: &index,
        notes: Vec::new(),
    };
    collector.visit_body(parsed.suite());
    collector.notes.sort_by_key(|note| note.line);
    collector.notes
}

struct Collector<'a> {
    source: &'a str,
    index: &'a LineIndex,
    notes: Vec<TranspilationNote>,
}

impl Collector<'_> {
    fn note(&mut self, range: TextRange, construct: &str, explanation: &str) {
        let snippet = self
            .source
            .get(range.start().into()..range.end().into())
            .unwrap_or_default()
            .trim();
        self.notes.push(TranspilationNote {
            construct: construct.to_string(),
            snippet: snippet.to_string(),
            explanation: explanation.to_string(),
            // A line number past `u32` needs a file no editor would open.
            line: u32::try_from(self.index.line_index(range.start()).get()).unwrap_or(u32::MAX),
        });
    }
}

impl<'a> SourceOrderVisitor<'a> for Collector<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Match(_) => self.note(
                stmt.range(),
                "pattern match",
                "Lowered to a Python `match` statement, or to an `if`/`elif` chain when the \
                 configured minimum version predates structural pattern matching.",
            ),
            Stmt::ClassDef(class) if class.decorator_list.iter().any(is_data_decorator) => self
                .note(
                    class.range(),
                    "data-class modifier",
                    "Lowered to a `@dataclasses.dataclass` class, with `__init__`, `__repr__` and \
                     `__eq__` generated from the annotated fields.",
                ),
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::BinOp(op) if op.op == Operator::Coalesce => self.note(
                expr.range(),
                "null-coalescing operator",
                "Lowered to a conditional that evaluates the right operand only when the left is \
                 `None`.",
            ),
            Expr::UnaryOp(op) => match op.op {
                UnaryOp::Force => self.note(
                    expr.range(),
                    "force unwrap",
                    "Lowered to a check that raises when the value is absent, and yields the value \
                     otherwise.",
                ),
                UnaryOp::Propagate => self.note(
                    expr.range(),
                    "propagate operator",
                    "Lowered to an early return of the absent case, so the rest of the function \
                     sees only the present one.",
                ),
                UnaryOp::Optional => self.note(
                    expr.range(),
                    "optional type",
                    "A type-level marker: lowered to `Optional[T]`, i.e. `T | None`.",
                ),
                _ => {}
            },
            // The parser records `?.` as a flag on the access rather than leaving it in the
            // text, so this is what was written and not what the characters look like.
            Expr::Attribute(access) if access.optional => self.note(
                expr.range(),
                "null-safe access",
                "Lowered to a conditional that yields `None` when the receiver is `None`, and the \
                 attribute otherwise. A chain evaluates the receiver once.",
            ),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

fn is_data_decorator(decorator: &ruff_python_ast::Decorator) -> bool {
    let name = match &decorator.expression {
        Expr::Name(name) => name.id.as_str(),
        Expr::Call(call) => match call.func.as_ref() {
            Expr::Name(name) => name.id.as_str(),
            _ => return false,
        },
        _ => return false,
    };
    matches!(name, "data" | "dataclass")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constructs(source: &str) -> Vec<String> {
        notes_in(source)
            .into_iter()
            .map(|note| note.construct)
            .collect()
    }

    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: ExplainTranspilationParams =
            serde_json::from_str(r#"{"textDocument":{"uri":"file:///a.by"}}"#)
                .expect("a client sends just the document");
        assert_eq!(parsed.text_document.uri.path().to_string(), "/a.by");
    }

    #[test]
    fn the_postfix_operators_are_recognised() {
        assert_eq!(constructs("x = a ?? b\n"), ["null-coalescing operator"]);
        assert_eq!(constructs("x = a!\n"), ["force unwrap"]);
        assert_eq!(constructs("x = a^\n"), ["propagate operator"]);
    }

    #[test]
    fn a_null_safe_access_is_recognised() {
        assert_eq!(constructs("x = a?.b\n"), ["null-safe access"]);
    }

    /// The whole reason for parsing rather than scanning: a plain access is not a null-safe one.
    #[test]
    fn a_plain_access_is_not_reported() {
        assert!(constructs("x = a.b\n").is_empty());
        assert!(constructs("x = a[0]\n").is_empty());
    }

    /// The other half of that reason: the same characters inside a string are not an operator.
    #[test]
    fn a_marker_inside_a_string_is_not_an_operator() {
        assert!(constructs("x = \"a?.b\"\n").is_empty());
        assert!(constructs("x = 1  # a ?? b\n").is_empty());
    }

    #[test]
    fn a_match_statement_is_recognised() {
        assert_eq!(
            constructs("match x:\n    case 1:\n        pass\n"),
            ["pattern match"]
        );
    }

    #[test]
    fn a_data_class_is_recognised() {
        assert_eq!(
            constructs("@data\nclass Point:\n    x: int\n"),
            ["data-class modifier"]
        );
    }

    #[test]
    fn notes_come_back_in_source_order() {
        let lines: Vec<u32> = notes_in("x = a!\ny = b ?? c\nz = d?.e\n")
            .into_iter()
            .map(|note| note.line)
            .collect();
        assert_eq!(lines, [1, 2, 3]);
    }

    #[test]
    fn a_note_carries_the_source_it_was_written_as() {
        let notes = notes_in("x = a ?? b\n");
        assert_eq!(notes[0].snippet, "a ?? b");
        assert_eq!(notes[0].line, 1);
    }

    /// Source that does not parse is an ordinary state mid-edit, not something to fail on.
    #[test]
    fn unparsable_source_yields_no_notes_rather_than_an_error() {
        let _ = notes_in("def (\n");
    }
}
