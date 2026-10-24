//! `by/evaluatableExpression`: the expression a debugger evaluates for a position
//!
//! what a debugger's value tooltip, *Evaluate Expression* and *Quick Evaluate* start from when
//! nothing is selected: on `d` in `a.b[c].d`, all of `a.b[c].d`. LSP has no request for it — the
//! closest is `textDocument/inlineValue`, which answers for a range of lines at a stop rather than
//! for a position — and a client guessing from the text cannot tell where a reference starts. see
//! [`ty_ide::evaluatable_expression`] for what is answered

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ty_ide::evaluatable_expression;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{PositionExt, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum EvaluatableExpressionRequest {}

impl Request for EvaluatableExpressionRequest {
    type Params = EvaluatableExpressionParams;
    type Result = Option<EvaluatableExpression>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/evaluatableExpression");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the position to answer for, and whether the expression may call anything
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EvaluatableExpressionParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    /// whether the expression may call something: a debugger asking because the user asked for
    /// an evaluation says yes, and one asking for a value tooltip says no. on `f` in `f(x)` that is
    /// the difference between `f(x)` and `f`
    calls: bool,
}

/// the expression, as where it is in the document
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvaluatableExpression {
    range: Range,
}

pub(crate) struct EvaluatableExpressionRequestHandler;

impl RequestHandler for EvaluatableExpressionRequestHandler {
    type RequestType = EvaluatableExpressionRequest;
}

impl BackgroundDocumentRequestHandler for EvaluatableExpressionRequestHandler {
    fn document_uri(params: &EvaluatableExpressionParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, for a template, and when the position is not on a
    /// name an expression ends with
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: EvaluatableExpressionParams,
    ) -> crate::server::Result<Option<EvaluatableExpression>> {
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
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };

        let python_file = db.program_file(file).python_file(db);
        let Some(range) = evaluatable_expression(db, python_file, offset, params.calls) else {
            return Ok(None);
        };
        let Some(range) = range
            .to_lsp_range(db, file, snapshot.encoding())
            .map(|range| range.local_range())
        else {
            return Ok(None);
        };
        Ok(Some(EvaluatableExpression { range }))
    }
}

impl RetriableRequestHandler for EvaluatableExpressionRequestHandler {}
