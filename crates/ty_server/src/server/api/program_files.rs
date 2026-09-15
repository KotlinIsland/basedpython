//! The project file a request names by URI, for the requests that ask about a file whether or not
//! the editor has it open.
//!
//! An open document is found through the session's index; a file a run configuration names — the
//! module `by run` is about to execute, say — usually is not open at all, and the project database
//! answers about it all the same: an open one through its overlay, anything else from disk.

use lsp_types::Uri;
use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::SystemPathBuf;
use ty_project::{Db as _, ProjectDatabase};

use crate::session::SessionSnapshot;

/// The project `uri` belongs to — the one with the deepest root containing it — and its file.
pub(super) fn project_file<'a>(
    snapshot: &'a SessionSnapshot,
    uri: &Uri,
) -> Option<(&'a ProjectDatabase, File)> {
    let path = SystemPathBuf::from_path_buf(uri.to_file_path().ok()?).ok()?;
    let db = snapshot
        .projects()
        .iter()
        .filter(|db| path.starts_with(db.project().root(*db)))
        .max_by_key(|db| db.project().root(*db).components().count())?;
    let file = system_path_to_file(db, &path).ok()?;
    Some((db, file))
}
