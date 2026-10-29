//! `by/expressionTypes`: the expressions a range is in, each with its type
//!
//! what an editor's *type info* offers: on `shapes` in `total_area(shapes) * 2` the argument, the
//! call and the product, so that a reader can pick the one they meant. LSP has no request for it:
//! `textDocument/hover` answers for the name under the cursor, never for the call or the
//! expression around it. see [`ty_ide::expression_types`] for what is answered

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ruff_text_size::TextRange;
use ty_ide::expression_types;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum ExpressionTypesRequest {}

impl Request for ExpressionTypesRequest {
    type Params = ExpressionTypesParams;
    type Result = Option<Vec<ExpressionType>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/expressionTypes");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the range to answer for: the selection, or an empty range at the caret
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExpressionTypesParams {
    text_document: TextDocumentIdentifier,
    range: Range,
}

/// an expression that contains the whole range, and its type
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExpressionType {
    range: Range,
    /// the type as the document's language spells it, on one line
    #[serde(rename = "type")]
    ty: String,
    /// the same type written for a reader, as hover writes it: a function's parameters one to a
    /// line, an overloaded function's signatures one after another
    pretty: PrettyType,
}

/// code to show in a code block, with the language to highlight it as: LSP's `MarkedString` in
/// its `{ language, value }` form
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct PrettyType {
    /// `python`, or `xml` for a special type such as `<special-form 'typing.Any'>`, which python
    /// highlighting renders poorly
    language: String,
    value: String,
}

pub(crate) struct ExpressionTypesRequestHandler;

impl RequestHandler for ExpressionTypesRequestHandler {
    type RequestType = ExpressionTypesRequest;
}

impl BackgroundDocumentRequestHandler for ExpressionTypesRequestHandler {
    fn document_uri(params: &ExpressionTypesParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off and for a template; otherwise the expressions,
    /// innermost first, an empty list where the range is in none
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: ExpressionTypesParams,
    ) -> crate::server::Result<Option<Vec<ExpressionType>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        if snapshot.is_django_template() {
            return Ok(None);
        }

        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let to_offset = |position: Position| {
            position.to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        };
        let (Some(start), Some(end)) = (to_offset(params.range.start), to_offset(params.range.end))
        else {
            return Ok(None);
        };
        // a range the client sent end first is the same selection
        let range = TextRange::new(start.min(end), start.max(end));

        Ok(Some(
            expression_types(db, db.program_file(file), range)
                .into_iter()
                .filter_map(|found| {
                    Some(ExpressionType {
                        range: found
                            .range
                            .to_lsp_range(db, file, snapshot.encoding())?
                            .local_range(),
                        ty: found.ty,
                        pretty: PrettyType {
                            language: found.pretty_language.to_owned(),
                            value: found.pretty,
                        },
                    })
                })
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for ExpressionTypesRequestHandler {}
