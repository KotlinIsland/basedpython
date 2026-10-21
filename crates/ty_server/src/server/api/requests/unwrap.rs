//! `by/unwrap` — what an editor's *Unwrap/Remove* offers around a range: each compound statement
//! that can be unwrapped into one of its suites, and each clause that can be removed, innermost
//! first.
//!
//! LSP has a request for rewrites, `textDocument/codeAction`, but not one an editor can show these
//! with: it lists the options in a popup and, while one is selected, marks what it deletes apart
//! from what it keeps, which a code action carries nothing to say. So each option comes with the
//! range it rewrites, the ranges of it that stay, and the edits.

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, TextEdit, Uri,
};
use ruff_db::files::File;
use ruff_text_size::{Ranged, TextRange};
use ty_ide::unwrap_options;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::PositionEncoding;
use crate::document::{RangeExt, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum UnwrapRequest {}

impl Request for UnwrapRequest {
    type Params = UnwrapParams;
    type Result = Option<UnwrapResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/unwrap");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Where to unwrap: the document, and the caret or selection.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UnwrapParams {
    text_document: TextDocumentIdentifier,
    range: Range,
}

/// The options around the range.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnwrapResponse {
    /// Innermost first. Empty when nothing around the range is a statement that unwraps.
    options: Vec<UnwrapOption>,
    /// Why there are no options at all, when that is not for want of a statement around the
    /// range: the file does not parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
}

/// One way to unwrap or remove.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnwrapOption {
    /// What to list it as: `Unwrap 'if...'`, `Remove 'else...'`.
    title: String,
    /// What it rewrites: the whole statement for an unwrap, the clause for a removal.
    range: Range,
    /// The parts of `range` that stay. Empty for a removal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    kept: Vec<Range>,
    /// The edits to the document, many small ones rather than one: the header deleted, and each
    /// line of a kept suite dedented, so that what an editor anchors in the suite stays put.
    /// Absent when the option cannot be applied, and `refused` says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edits: Option<Vec<TextEdit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
}

pub(crate) struct UnwrapRequestHandler;

impl RequestHandler for UnwrapRequestHandler {
    type RequestType = UnwrapRequest;
}

impl BackgroundDocumentRequestHandler for UnwrapRequestHandler {
    fn document_uri(params: &UnwrapParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, and for a notebook or a template, which this does
    /// not rewrite.
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: UnwrapParams,
    ) -> crate::server::Result<Option<UnwrapResponse>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        // a notebook's ranges are per cell, and a cell's suites are not rewritten across cells
        if snapshot.document().is_cell_or_notebook() || snapshot.is_django_template() {
            return Ok(None);
        }
        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(range) = params
            .range
            .to_text_range(db, file, snapshot.uri(), snapshot.encoding())
        else {
            return Ok(None);
        };

        let options = match unwrap_options(db, db.program_file(file).python_file(db), range) {
            Ok(options) => options,
            Err(reason) => {
                return Ok(Some(UnwrapResponse {
                    options: Vec::new(),
                    refused: Some(reason),
                }));
            }
        };
        let converter = Converter {
            db,
            file,
            encoding: snapshot.encoding(),
        };
        Ok(Some(UnwrapResponse {
            options: options
                .iter()
                .filter_map(|option| converter.option(option))
                .collect(),
            refused: None,
        }))
    }
}

impl RetriableRequestHandler for UnwrapRequestHandler {}

struct Converter<'a> {
    db: &'a ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
}

impl Converter<'_> {
    fn range(&self, range: TextRange) -> Option<Range> {
        range
            .to_lsp_range(self.db, self.file, self.encoding)
            .map(|range| range.local_range())
    }

    /// The option as the client reads it, or `None` when one of its ranges cannot be said in the
    /// client's terms — an option with an edit missing would apply only part of itself.
    fn option(&self, option: &ty_ide::UnwrapOption) -> Option<UnwrapOption> {
        let (edits, refused) = match &option.edits {
            Ok(edits) => {
                let edits = edits
                    .iter()
                    .map(|edit| {
                        Some(TextEdit {
                            range: self.range(edit.range())?,
                            new_text: edit.content().unwrap_or_default().to_string(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                (Some(edits), None)
            }
            Err(reason) => (None, Some(reason.clone())),
        };
        Some(UnwrapOption {
            title: option.title.clone(),
            range: self.range(option.range)?,
            kept: option
                .kept
                .iter()
                .map(|range| self.range(*range))
                .collect::<Option<Vec<_>>>()?,
            edits,
            refused,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form, exactly as a client sends it.
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: UnwrapParams = serde_json::from_str(
            r#"{"textDocument":{"uri":"file:///main.by"},"range":{"start":{"line":1,"character":4},"end":{"line":1,"character":4}}}"#,
        )
        .expect("a client sends a document and a range");
        assert_eq!(parsed.text_document.uri.as_str(), "file:///main.by");
        assert_eq!(parsed.range.start.line, 1);
    }
}
