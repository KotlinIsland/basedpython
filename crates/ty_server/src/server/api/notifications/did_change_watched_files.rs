use crate::document::DocumentKey;
use crate::server::Result;
use crate::server::api::changes;
use crate::server::api::traits::{NotificationHandler, SyncNotificationHandler};
use crate::session::Session;
use crate::session::client::Client;
use crate::system::AnySystemPath;
use lsp_types::FileChangeType;
use lsp_types::{self as types, DidChangeWatchedFilesNotification};
use ty_project::watch::{ChangeEvent, ChangedKind, CreatedKind, DeletedKind, ExistingPathKind};

pub(crate) struct DidChangeWatchedFiles;

impl NotificationHandler for DidChangeWatchedFiles {
    type NotificationType = DidChangeWatchedFilesNotification;
}

impl SyncNotificationHandler for DidChangeWatchedFiles {
    fn run(
        session: &mut Session,
        client: &Client,
        params: types::DidChangeWatchedFilesParams,
    ) -> Result<()> {
        let mut changes = Vec::new();
        let system = session.system();

        for change in params.changes {
            let path = DocumentKey::from_uri(&change.uri).into_file_path();

            let system_path = match path {
                AnySystemPath::System(system) => system,
                AnySystemPath::SystemVirtual(path) => {
                    tracing::debug!("Ignoring virtual path from change event: `{path}`");
                    continue;
                }
            };

            let change_event = match change.kind {
                FileChangeType::Created => ChangeEvent::Created {
                    kind: CreatedKind::from(ExistingPathKind::from_system(system, &system_path)),
                    path: system_path,
                },
                FileChangeType::Changed => {
                    // We're only interested in file content or metadata changes.
                    // Renames are modelled as create/delete events.
                    if ExistingPathKind::from_system(system, &system_path).is_file() {
                        ChangeEvent::Changed {
                            path: system_path,
                            kind: ChangedKind::Any,
                        }
                    } else {
                        continue;
                    }
                }
                FileChangeType::Deleted => ChangeEvent::Deleted {
                    path: system_path,
                    kind: DeletedKind::Any,
                },
                // Custom file change types are not supported and should be ignored.
                FileChangeType::Custom(_) => continue,
            };

            changes.push(change_event);
        }

        changes::apply(session, client, &changes);

        Ok(())
    }
}
