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

/// applies `changes` to every project and, if that changed anything, refreshes what the client is
/// showing
///
/// only then: a batch of changes is often a write to something no answer depends on (a build's
/// output, an ignored directory, a file nothing imports), and a refresh makes the client ask again
/// for the diagnostics and inlay hints of everything it shows, and poll the workspace's
/// diagnostics again
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

    let mut changed = false;
    for root in roots {
        tracing::debug!("Applying changes to `{root}`");

        let result = session.apply_changes(client, &AnySystemPath::System(root.clone()), changes);
        if result.database_changed() {
            changed = true;
            publish_settings_diagnostics(session, client, root);
        }
    }

    // a change can move what there is to watch: a new virtual environment, a configuration
    // naming other search paths
    session.update_file_watcher(client);

    if !changed {
        tracing::debug!("The changes changed nothing the session answers from");
        return;
    }

    // a workspace diagnostic request waiting for the session to change is answered here rather
    // than by a notification's handler: most changes come from the session's own watcher
    session.resume_suspended_workspace_diagnostic_requests(client);

    if client_capabilities.supports_workspace_diagnostic_refresh() {
        client.send_request::<types::DiagnosticRefreshRequest>(session, (), |_, ()| {});
    } else {
        publish_all_document_diagnostics(session, client);
    }

    if client_capabilities.supports_inlay_hint_refresh() {
        client.send_request::<types::InlayHintRefreshRequest>(session, (), |_, ()| {});
    }
}
