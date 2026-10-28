use lsp_server::ErrorCode;
use lsp_types::DidCloseTextDocumentNotification;
use lsp_types::{DidCloseTextDocumentParams, TextDocumentIdentifier};

use crate::server::Result;
use crate::server::api::LSPResult;
use crate::server::api::traits::{NotificationHandler, SyncNotificationHandler};
use crate::session::client::Client;
use crate::session::{Changed, Session};

pub(crate) struct DidCloseTextDocumentHandler;

impl NotificationHandler for DidCloseTextDocumentHandler {
    type NotificationType = DidCloseTextDocumentNotification;
}

impl SyncNotificationHandler for DidCloseTextDocumentHandler {
    fn run(
        session: &mut Session,
        client: &Client,
        params: DidCloseTextDocumentParams,
    ) -> Result<()> {
        let DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        } = params;

        let document = session
            .open_document_handle(&uri)
            .with_failure_code(ErrorCode::InternalError)?;

        let should_clear_diagnostics = document
            .close(session, client)
            .with_failure_code(ErrorCode::InternalError)?;

        // the document is read from disk again, which unsaved edits to it differed from
        if document.is_python() {
            session.diagnostics_may_have_changed(Changed::Closed);
        }

        if should_clear_diagnostics {
            session.clear_diagnostics_if_needed(&document, client);
        }

        Ok(())
    }
}
