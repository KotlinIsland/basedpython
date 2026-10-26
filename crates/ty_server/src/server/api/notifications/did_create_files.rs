//! `workspace/didCreateFiles`: the client created files, through a refactoring or its own user
//! interface, and says so once they are on disk
//!
//! the session's own watcher sees a write after it happens, but not before the client goes on to
//! ask about what it created: a module made to move a definition into is asked about at once. the
//! server answers from what it has read of the file system, so a path it had found nothing at is
//! still nothing until a change reaches it. this notification is that change, in order with the
//! requests after it

use lsp_types::{self as types, DidCreateFilesNotification};
use ty_project::watch::{ChangeEvent, CreatedKind, ExistingPathKind};

use crate::document::DocumentKey;
use crate::server::Result;
use crate::server::api::changes;
use crate::server::api::traits::{NotificationHandler, SyncNotificationHandler};
use crate::session::Session;
use crate::session::client::Client;
use crate::system::AnySystemPath;

pub(crate) struct DidCreateFiles;

impl NotificationHandler for DidCreateFiles {
    type NotificationType = DidCreateFilesNotification;
}

impl SyncNotificationHandler for DidCreateFiles {
    fn run(session: &mut Session, client: &Client, params: types::CreateFilesParams) -> Result<()> {
        let system = session.system();
        let changes: Vec<_> = params
            .files
            .iter()
            .filter_map(
                |created| match DocumentKey::from_uri(&created.uri).into_file_path() {
                    AnySystemPath::System(path) => Some(ChangeEvent::Created {
                        kind: CreatedKind::from(ExistingPathKind::from_system(system, &path)),
                        path,
                    }),
                    AnySystemPath::SystemVirtual(path) => {
                        tracing::debug!("Ignoring a created virtual path: `{path}`");
                        None
                    }
                },
            )
            .collect();

        changes::apply(session, client, &changes);

        Ok(())
    }
}
