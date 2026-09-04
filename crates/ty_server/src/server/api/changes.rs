//! Taking file system changes into the session, and telling the client what they
//! did to what it is showing.
//!
//! Changes reach the server two ways — the client reporting what its watcher saw,
//! and the server itself having run something that writes to the file system —
//! and both leave the same work behind, which is what lives here.

use lsp_types as types;
use ty_project::watch::ChangeEvent;

use crate::server::api::diagnostics::{
    publish_all_document_diagnostics, publish_settings_diagnostics,
};
use crate::session::Session;
use crate::session::client::Client;
use crate::system::AnySystemPath;

/// Applies `changes` to every project and refreshes what the client is showing.
pub(crate) fn apply(session: &mut Session, client: &Client, changes: &[ChangeEvent]) {
    if changes.is_empty() {
        return;
    }

    let client_capabilities = session.client_capabilities();
    let roots: Vec<_> = session
        .workspaces()
        .into_iter()
        .map(|(root, _)| root.clone())
        .collect();

    for root in roots {
        tracing::debug!("Applying changes to `{root}`");

        session.apply_changes(client, &AnySystemPath::System(root.clone()), changes);
        publish_settings_diagnostics(session, client, root);
    }

    if client_capabilities.supports_workspace_diagnostic_refresh() {
        client.send_request::<types::DiagnosticRefreshRequest>(session, (), |_, ()| {});
    } else {
        publish_all_document_diagnostics(session, client);
    }

    if client_capabilities.supports_inlay_hint_refresh() {
        client.send_request::<types::InlayHintRefreshRequest>(session, (), |_, ()| {});
    }
}
