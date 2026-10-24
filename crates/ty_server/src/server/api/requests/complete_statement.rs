//! `by/completeStatement` — the statement at a position completed, and the line after it started:
//! what an editor's *Complete Current Statement* (Smart Enter) does. See
//! [`ty_ide::complete_statement`].
//!
//! LSP has no request for it. The answer is the edits and where the caret goes afterwards, which
//! is a place in the text the edits produce rather than in the text that was asked about, so it
//! is not a code action's workspace edit either.
//!
//! One position in one document; `textHash` says which revision of the document it is in.

use std::borrow::Cow;

use lsp_types::{
    FormattingOptions, LspRequestMethod, MessageDirection, Position, Request,
    TextDocumentIdentifier, TextEdit, Uri,
};
use ruff_db::source::source_text;
use ruff_source_file::LineIndex;
use ty_ide::complete_statement;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToRangeExt, text_size_to_lsp_position};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum CompleteStatementRequest {}

impl Request for CompleteStatementRequest {
    type Params = CompleteStatementParams;
    type Result = Option<CompletedStatement>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/completeStatement");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Where the statement to complete is.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CompleteStatementParams {
    text_document: TextDocumentIdentifier,
    /// The caret: the statement completed is the one on its line.
    position: Position,
    /// How the document is indented, as `textDocument/onTypeFormatting` says it: one level is
    /// `tabSize` spaces when `insertSpaces`, and a tab otherwise. Used for the line a header's
    /// suite starts on when the suite has no statement yet to copy the indentation of.
    options: FormattingOptions,
}

/// The statement completed.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletedStatement {
    /// The edits, against the document as it was asked about. None overlaps another.
    edits: Vec<TextEdit>,
    /// Where the caret goes, in the document as the edits leave it.
    caret: Position,
}

pub(crate) struct CompleteStatementRequestHandler;

impl RequestHandler for CompleteStatementRequestHandler {
    type RequestType = CompleteStatementRequest;
}

impl BackgroundDocumentRequestHandler for CompleteStatementRequestHandler {
    fn document_uri(params: &CompleteStatementParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: CompleteStatementParams,
    ) -> crate::server::Result<Option<CompletedStatement>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        if snapshot.document().is_cell_or_notebook() || snapshot.is_django_template() {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };

        let indent_unit = if params.options.insert_spaces {
            " ".repeat(usize::try_from(params.options.tab_size.max(1)).unwrap_or(4))
        } else {
            "\t".to_string()
        };
        let python_file = db.program_file(file).python_file(db);
        let completed = complete_statement(db, python_file, offset, &indent_unit);

        let mut edits = Vec::with_capacity(completed.edits.len());
        for edit in &completed.edits {
            let Some(range) = edit
                .range
                .to_lsp_range(db, file, snapshot.encoding())
                .map(|range| range.local_range())
            else {
                return Ok(None);
            };
            edits.push(TextEdit {
                range,
                new_text: edit.content.clone(),
            });
        }

        // The caret is a place in the edited text, so it is measured there.
        let mut edited = source_text(db, file).as_str().to_string();
        for edit in completed.edits.iter().rev() {
            edited.replace_range(edit.range.to_std_range(), &edit.content);
        }
        let index = LineIndex::from_source_text(&edited);
        let caret =
            text_size_to_lsp_position(completed.caret, &edited, &index, snapshot.encoding());

        Ok(Some(CompletedStatement { edits, caret }))
    }
}

impl RetriableRequestHandler for CompleteStatementRequestHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form, exactly as a client sends it.
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: CompleteStatementParams = serde_json::from_str(
            r#"{"textDocument":{"uri":"file:///main.by"},"position":{"line":0,"character":4},"options":{"tabSize":4,"insertSpaces":true}}"#,
        )
        .expect("a client sends a document, a position and the formatting options");
        assert_eq!(parsed.position.character, 4);
        assert!(parsed.options.insert_spaces);
    }
}
