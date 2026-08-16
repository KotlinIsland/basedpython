//! Taking file system changes into the session, and telling the client what they
//! did to what it is showing.
//!
//! Changes reach the server two ways — the client reporting what its watcher saw,
//! and the server itself having run something that writes to the file system —
//! and both leave the same work behind, which is what lives here.

use lsp_types as types;
use ty_project::Db as _;
use ty_project::watch::ChangeEvent;

use crate::server::api::diagnostics::{
    publish_diagnostics_if_needed, publish_settings_diagnostics,
};
use crate::session::Session;
use crate::session::client::Client;
use crate::system::AnySystemPath;

/// Applies `changes` to every project and refreshes what the client is showing.
pub(crate) fn apply(session: &mut Session, client: &Client, changes: &[ChangeEvent]) {
    if changes.is_empty() {
        return;
    }

    let roots: Vec<_> = session
        .project_dbs()
        .map(|db| db.project().root(db).to_owned())
        .collect();

    for root in roots {
        tracing::debug!("Applying changes to `{root}`");

        session.apply_changes(&AnySystemPath::System(root.clone()), changes);
        publish_settings_diagnostics(session, client, root);
    }

    let client_capabilities = session.client_capabilities();

    if client_capabilities.supports_workspace_diagnostic_refresh() {
        client.send_request::<types::DiagnosticRefreshRequest>(session, (), |_, ()| {});
    } else {
        for document in session.file_document_handles() {
            publish_diagnostics_if_needed(&document, session, client);
        }
    }

    if client_capabilities.supports_inlay_hint_refresh() {
        client.send_request::<types::InlayHintRefreshRequest>(session, (), |_, ()| {});
    }
}
