//! `by/injections` — the fragments of another language a document holds.
//!
//! An editor cannot work this out for itself. Which language a string is written in is a question
//! about the program: the marker may be on the statement above it, or on the parameter it is handed
//! to, in another module, reached through a function that only passes it along. Answering it needs
//! the same resolved project the diagnostics in that window came from.
//!
//! What the editor does with the answer is the other half, and it is entirely the editor's. This
//! request says *where* a fragment is and *what* language it is in; injecting that language, and
//! lighting up whatever support the editor has for it, happens on the client. That split is what
//! lets a language `by` knows nothing about work as well as basedpython does.

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, Uri};
use ty_ide::injections;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::ToRangeExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum InjectionsRequest {}

impl Request for InjectionsRequest {
    type Params = InjectionsParams;
    type Result = Option<InjectionsResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/injections");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Which document to look in.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InjectionsParams {
    /// The document to look in — its *buffer*, so a marker typed a moment ago counts.
    pub(crate) text_document: TextDocumentIdentifier,
}

/// Every fragment in the document, in source order.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InjectionsResponse {
    /// The fragments. A client keys its own state on a fragment's position in this list, which is
    /// source order and so is stable while the fragments are.
    pub(crate) injections: Vec<InjectionFragment>,
}

/// One fragment of another language.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InjectionFragment {
    /// The language, as the marker spelled it. The server does not interpret it: matching it to a
    /// language the editor has is the client's, and an id it does not recognise is not an error.
    pub(crate) language: String,

    /// Where the fragment's text is, quotes excluded, one range per literal part.
    ///
    /// More than one means the fragment was written as several adjacent literals, and its text is
    /// their contents joined in this order.
    pub(crate) ranges: Vec<Range>,

    /// What decided the language: `comment`, `declared`, or `propagated`.
    ///
    /// A client shows this when a reader asks why a string is being treated as another language —
    /// `propagated` is the one whose reason is not visible at the string itself.
    pub(crate) origin: String,
}

pub(crate) struct InjectionsRequestHandler;

impl RequestHandler for InjectionsRequestHandler {
    type RequestType = InjectionsRequest;
}

impl BackgroundDocumentRequestHandler for InjectionsRequestHandler {
    fn document_uri(params: &InjectionsParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: InjectionsParams,
    ) -> crate::server::Result<Option<InjectionsResponse>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let found = injections(db, db.program_file(file))
            .iter()
            .filter_map(|injection| {
                // A fragment whose parts do not all map is not reportable in halves: the client
                // joins the parts to get the text, so a missing one silently shifts everything
                // after it.
                let ranges = injection
                    .ranges
                    .iter()
                    .map(|range| {
                        range
                            .to_lsp_range(db, file, snapshot.encoding())
                            .map(|range| range.local_range())
                    })
                    .collect::<Option<Vec<_>>>()?;

                Some(InjectionFragment {
                    language: injection.language.clone(),
                    ranges,
                    origin: injection.origin.as_str().to_string(),
                })
            })
            .collect();

        Ok(Some(InjectionsResponse { injections: found }))
    }
}

impl RetriableRequestHandler for InjectionsRequestHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form, exactly as a client sends it — the whole contract with a plugin written in
    /// another language, and nothing else here exercises it.
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: InjectionsParams =
            serde_json::from_str(r#"{"textDocument":{"uri":"file:///main.by"}}"#)
                .expect("a client sends a document");
        assert_eq!(parsed.text_document.uri.as_str(), "file:///main.by");
    }

    /// The shape a client reads back. A fragment in two parts is the case a client is most likely
    /// to get wrong, so it is the one written out here.
    #[test]
    fn the_response_a_client_reads_serialises() {
        let response = InjectionsResponse {
            injections: vec![InjectionFragment {
                language: "sql".to_string(),
                ranges: vec![
                    Range::new(
                        lsp_types::Position::new(1, 9),
                        lsp_types::Position::new(1, 17),
                    ),
                    Range::new(
                        lsp_types::Position::new(1, 20),
                        lsp_types::Position::new(1, 27),
                    ),
                ],
                origin: "comment".to_string(),
            }],
        };

        assert_eq!(
            serde_json::to_string(&response).expect("a response to serialise"),
            r#"{"injections":[{"language":"sql","ranges":[{"start":{"line":1,"character":9},"end":{"line":1,"character":17}},{"start":{"line":1,"character":20},"end":{"line":1,"character":27}}],"origin":"comment"}]}"#
        );
    }
}
