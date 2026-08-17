use lsp_types::{RenameFilesParams, TextEdit, Uri, WillRenameFilesRequest, WorkspaceEdit};
use ruff_db::files::FileRange;
use ruff_db::system::SystemPathBuf;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;
use ty_ide::{FileMove, module_rename_edits};

use crate::document::FileRangeExt;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

/// `workspace/willRenameFiles` — the edits that keep imports working across a rename the editor is
/// about to perform.
///
/// The client asks *before* it moves anything, applies whatever comes back, and only then does the
/// move. That ordering is what makes the answer computable at all: the old path still holds the
/// file, so the module it is today can be resolved, while the new path is just a path — see
/// [`ty_module_resolver::path_to_module_name`].
///
/// A rename that changes no module's name — a `README.md`, a directory no search path covers —
/// answers `None` rather than an empty edit, which is what tells the client to get on with the move
/// without showing the user an empty preview.
pub(crate) struct WillRenameFilesRequestHandler;

impl RequestHandler for WillRenameFilesRequestHandler {
    type RequestType = WillRenameFilesRequest;
}

impl BackgroundRequestHandler for WillRenameFilesRequestHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: RenameFilesParams,
    ) -> crate::server::Result<Option<WorkspaceEdit>> {
        let moves: Vec<FileMove> = params
            .files
            .iter()
            .filter_map(|rename| {
                Some(FileMove {
                    old_path: system_path(&rename.old_uri)?,
                    new_path: system_path(&rename.new_uri)?,
                })
            })
            .collect();

        if moves.is_empty() {
            return Ok(None);
        }

        let mut changes: FxHashMap<Uri, Vec<TextEdit>> = FxHashMap::default();

        // Every project, because a rename in one workspace folder can move a module that another
        // folder imports; a project the paths have nothing to do with contributes nothing, since
        // the paths resolve to no module of its.
        for db in snapshot.projects() {
            let result = module_rename_edits(db, &moves);

            for skipped in &result.skipped {
                // Not an error: the rename can still go ahead, and this is the one import the user
                // will have to look at themselves. Logged with its location so that "which line?"
                // has an answer that does not involve searching the project.
                tracing::info!(
                    "willRenameFiles: leaving an import in {} alone ({:?}); it would need a \
                     different statement to name the module's new home",
                    skipped.file.path(db),
                    skipped.reason,
                );
            }

            for file_edit in result.edits {
                let range = FileRange::new(file_edit.file, file_edit.edit.range());
                let Some(location) = range
                    .to_lsp_range(db, snapshot.position_encoding())
                    .and_then(|range| range.to_location())
                else {
                    continue;
                };
                changes.entry(location.uri).or_default().push(TextEdit {
                    range: location.range,
                    new_text: file_edit.edit.content().unwrap_or_default().to_string(),
                });
            }
        }

        if changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes.into_iter().collect()),
            document_changes: None,
            change_annotations: None,
        }))
    }
}

/// The path a `file:` URI names, or nothing for a URI that is not one.
///
/// A client may send `untitled:` for a buffer that has never been saved, which cannot be a module
/// and cannot be moved; those are dropped rather than refused, so a mixed rename still gets the
/// edits for the files that do exist.
fn system_path(uri: &Uri) -> Option<SystemPathBuf> {
    SystemPathBuf::from_path_buf(uri.to_file_path().ok()?).ok()
}

impl RetriableRequestHandler for WillRenameFilesRequestHandler {
    /// A rename is a one-shot gesture the user is waiting on, and the client is holding the file
    /// move until it answers. Retrying on a database change is the right trade here for the same
    /// reason it is for the other whole-project requests: the alternative is telling the editor to
    /// go ahead with a rename this never got to check.
    const RETRY_ON_CANCELLATION: bool = true;
}
