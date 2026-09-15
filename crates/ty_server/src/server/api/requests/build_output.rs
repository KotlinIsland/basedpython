//! `by/buildOutput` — where a path stands in its project's build
//!
//! an editor relates a `.by` to what it becomes in more places than one: opening the generated
//! module, going back from it, keeping the output out of its own index, telling a test runner not
//! to collect a second copy of every test. every one of those used to be the editor's own guess at
//! the layout, and the guess was wrong twice — about the directory and about the path inside it,
//! which follows the module tree rather than the directory tree. see [`by_stage::layout`]
//!
//! not a document request, because half of what it is asked about is not a document the editor
//! has open: a project root, a directory it wants excluded, a generated file it wants to go back
//! from. the path is enough to find the project that holds it

use by_stage::layout::{BuildOutput, build_output};
use lsp_types::{LspRequestMethod, MessageDirection, Request, Uri};
use ruff_db::system::SystemPath;
use ty_project::Db as _;

use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

/// the request a client sends to relate a path to the build
pub(crate) enum BuildOutputRequest {}

impl Request for BuildOutputRequest {
    type Params = BuildOutputParams;
    type Result = Option<BuildOutput>;
    // not a method LSP defines, so it goes across as a custom one, under the same `by/` prefix as
    // the rest of them
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/buildOutput");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the path to place: a source, a generated file, or any directory in a project
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BuildOutputParams {
    uri: Uri,
}

pub(crate) struct BuildOutputRequestHandler;

impl RequestHandler for BuildOutputRequestHandler {
    type RequestType = BuildOutputRequest;
}

impl BackgroundRequestHandler for BuildOutputRequestHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: BuildOutputParams,
    ) -> crate::server::Result<Option<BuildOutput>> {
        let Ok(path) = params.uri.to_file_path() else {
            // not a file on disk, so not something a build is made of or writes
            return Ok(None);
        };
        let Some(system_path) = SystemPath::from_std_path(&path) else {
            return Ok(None);
        };
        // the project whose root holds the path, the deepest one when projects nest: that is the
        // project a `by build` run there would build
        let Some(db) = snapshot
            .projects()
            .iter()
            .filter(|db| system_path.starts_with(db.project().root(*db)))
            .max_by_key(|db| db.project().root(*db).components().count())
        else {
            return Ok(None);
        };
        Ok(Some(build_output(db, &path)))
    }
}

impl RetriableRequestHandler for BuildOutputRequestHandler {}
