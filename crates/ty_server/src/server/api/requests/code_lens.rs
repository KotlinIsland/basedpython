use std::borrow::Cow;

use lsp_types::{CodeLens, CodeLensParams, CodeLensRequest, Uri};
use ruff_db::files::FileRange;
use ty_ide::{DjangoLensAction, django_python_code_lenses, django_template_code_lenses};
use ty_project::ProjectDatabase;

use crate::capabilities::SupportedCommand;
use crate::document::{FileRangeExt, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

/// The command a navigating lens asks the client to run.
///
/// LSP has no navigation command of its own, so a lens that goes somewhere has to
/// name one the client already knows. This is the identifier VS Code registers and
/// that the editors emulating its lenses — and rust-analyzer's, which is where the
/// convention comes from — recognise. Its arguments are the document the lens is
/// in, the position it sits at, and the locations to offer.
const SHOW_REFERENCES_COMMAND: &str = "editor.action.showReferences";

pub(crate) struct CodeLensRequestHandler;

impl RequestHandler for CodeLensRequestHandler {
    type RequestType = CodeLensRequest;
}

impl BackgroundDocumentRequestHandler for CodeLensRequestHandler {
    fn document_uri(params: &CodeLensParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: CodeLensParams,
    ) -> crate::server::Result<Option<Vec<CodeLens>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let lenses = if snapshot.is_django_template() {
            django_template_code_lenses(db, file)
        } else {
            django_python_code_lenses(db, file)
        };

        let uri = snapshot.uri().clone();

        let lenses: Vec<CodeLens> = lenses
            .into_iter()
            .filter_map(|lens| {
                let range = lens
                    .range
                    .to_lsp_range(db, file, snapshot.encoding())?
                    .local_range();

                let command = match lens.action {
                    DjangoLensAction::Run(arguments) => lsp_types::Command {
                        title: lens.title,
                        command: SupportedCommand::RunManage.identifier().to_string(),
                        tooltip: None,
                        arguments: Some(vec![serde_json::json!({
                            "arguments": arguments,
                        })]),
                    },
                    DjangoLensAction::Navigate(targets) => {
                        let locations: Vec<lsp_types::Location> = targets
                            .into_iter()
                            .filter_map(|target| {
                                FileRange::new(target.file, target.range)
                                    .to_lsp_range(db, snapshot.encoding())?
                                    .to_location()
                            })
                            .collect();

                        // a lens that would navigate nowhere is worse than no lens
                        if locations.is_empty() {
                            return None;
                        }

                        lsp_types::Command {
                            title: lens.title,
                            command: SHOW_REFERENCES_COMMAND.to_string(),
                            tooltip: None,
                            arguments: Some(vec![
                                serde_json::to_value(&uri).ok()?,
                                serde_json::to_value(range.start).ok()?,
                                serde_json::to_value(&locations).ok()?,
                            ]),
                        }
                    }
                };

                Some(CodeLens {
                    range,
                    command: Some(command),
                    data: None,
                })
            })
            .collect();

        if lenses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(lenses))
        }
    }
}

impl RetriableRequestHandler for CodeLensRequestHandler {}
