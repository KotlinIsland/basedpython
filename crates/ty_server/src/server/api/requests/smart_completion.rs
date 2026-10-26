//! `by/smartCompletion`: the completions that fit the type the position expects
//!
//! what an editor's smart, type-matching completion offers: on `draw(` the values that are the
//! `Shape` its first parameter takes, and the classes and functions whose call makes one. LSP's
//! completion request has no kind of completion to ask for, so this is a request of its own,
//! taking what `textDocument/completion` takes and answering in the same form, items for items.
//! see [`ty_ide::smart_completion`] for which positions expect a type and which completions fit

use std::borrow::Cow;

use lsp_types::{
    CompletionList, CompletionParams, LspRequestMethod, MessageDirection, Request, Uri,
};
use ty_ide::{CompletionCapabilities, smart_completion};
use ty_project::{ProjectDatabase, SemanticDb as _};
use ty_python_semantic::ProgramEnvironment;

use crate::document::PositionExt;
use crate::server::api::requests::completion::completion_list;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum SmartCompletionRequest {}

impl Request for SmartCompletionRequest {
    type Params = CompletionParams;
    type Result = Option<CompletionList>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/smartCompletion");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

pub(crate) struct SmartCompletionRequestHandler;

impl RequestHandler for SmartCompletionRequestHandler {
    type RequestType = SmartCompletionRequest;
}

impl BackgroundDocumentRequestHandler for SmartCompletionRequestHandler {
    fn document_uri(params: &CompletionParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    /// `null` when language services are off, in a template, and where the position expects no
    /// type the source states, which leaves the client to say there is nothing to match. otherwise
    /// a list, empty when nothing at hand fits
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: CompletionParams,
    ) -> crate::server::Result<Option<CompletionList>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
            || snapshot.is_django_template()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(offset) = params.text_document_position_params.position.to_text_size(
            db,
            file,
            snapshot.document().uri(),
            snapshot.encoding(),
        ) else {
            return Ok(None);
        };
        let program_file = db.program_file(file);
        let env = ProgramEnvironment::from_file(program_file);
        let Some(completions) = smart_completion(
            db,
            &env,
            snapshot.workspace_settings().completions(),
            CompletionCapabilities::default().snippets(
                snapshot
                    .resolved_client_capabilities()
                    .supports_completion_item_snippets(),
            ),
            program_file,
            offset,
        ) else {
            return Ok(None);
        };
        let mut list = completion_list(db, snapshot, file, &env, completions);
        // the list is whole: typing on narrows it the way the client narrows any list
        list.is_incomplete = false;
        Ok(Some(list))
    }
}

impl RetriableRequestHandler for SmartCompletionRequestHandler {}
