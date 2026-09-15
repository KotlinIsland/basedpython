//! `by/transpileForBuild` — the slots of an edit's files in a running build's tree, recomputed
//!
//! ## why the language server and not the `by` binary
//!
//! Because of what it costs. A build tree is what actually runs: `by run` transpiles the project
//! into a temp directory, copies every other project file in beside it, and runs the program out of
//! there — so nothing the user is editing is the file the process is executing, a `.by` because it
//! was transpiled and a hand-written `.py` because it was copied. Reloading a function into that
//! program means putting new bytes in the tree first.
//!
//! Rebuilding the tree to get them is not affordable. Measured on a 97-file project, `by check` is
//! 8.5 seconds and `by build` is 24.9; one file's share of the latter is about 165 milliseconds.
//! A CLI would pay project discovery and the whole check again on every press of a button. The
//! server has already paid both — it holds the project database, warm — so what is left is the
//! edited files' emit, which is the entire reason this is a request and not a subcommand.
//!
//! ## one request for the whole edit
//!
//! A reload is a set of files, and the tree holds one `_by_sourcemap.py` for all of them. An answer
//! about one file can only carry the tree's map plus that file's entry, so a client writing each
//! such answer's map in turn keeps the last file's line table and silently loses the rest. The
//! request therefore names every file of the edit, and the answer carries one map with all of their
//! entries moved — see [`by_stage::restage`].
//!
//! ## it writes nothing
//!
//! The answer is the bytes and where they go. The client writes them, because the client is the
//! only party that can roll that write back together with the debugger request that follows it: a
//! tree updated for a replacement the debugger then refused is a tree that lies about what is
//! running, and a debug session reading lines out of it would be wrong with total confidence.
//!
//! ## and it refuses rather than guesses
//!
//! Every refusal in [`by_stage::restage`] is a case where the bytes would not be the bytes the
//! build itself would have written — a tree built by another `by`, a `--compiled` build whose
//! modules are native extensions with no `__code__` to assign, a file that does not check. A
//! refusal costs the user a restart. A wrong answer costs them a session that reports lines from a
//! file that no longer exists.

use by_stage::restage::{Restage, restage};

use super::detached_transpile::transpile_detached;
use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier};
use ruff_db::system::{SystemPath, SystemPathBuf};
use ty_project::{Db as _, ProjectDatabase};

use crate::document::DocumentKey;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

/// the request a client sends while its debuggee is running
pub(crate) enum TranspileForBuildRequest {}

impl Request for TranspileForBuildRequest {
    type Params = TranspileForBuildParams;
    type Result = Restage;
    // not a method LSP defines, so it goes across as a custom one. the `by/` prefix is what keeps
    // it from ever colliding with something the protocol grows later — the same reason
    // `by/dataFlowAt` carries it
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/transpileForBuild");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// what the client knows: which files it edited, and which tree is running
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TranspileForBuildParams {
    /// every file of the edit, in the project
    ///
    /// documents rather than paths, so the source that is transpiled is the one the editor holds.
    /// A client that has not saved yet gets the buffer transpiled and the `.by` digest taken over
    /// that same buffer, which is coherent — though a client doing this for a debugger should save
    /// first anyway, because the traceback rewriter reads the file from disk.
    text_documents: Vec<TextDocumentIdentifier>,

    /// the build tree the program is running out of
    ///
    /// the client knows this and the server cannot: `by run` chooses a temp directory, and the only
    /// thing that sees the name is whatever started the program. It is not trusted on the strength
    /// of being sent — `_by_build.json` in it has to say it was written by this same `by`, or the
    /// answer is a refusal.
    build_directory: std::path::PathBuf,
}

pub(crate) struct TranspileForBuildRequestHandler;

impl RequestHandler for TranspileForBuildRequestHandler {
    type RequestType = TranspileForBuildRequest;
}

// Session-wide rather than a document request, because the edit is several documents and none of
// them is the one the answer is about.
impl BackgroundRequestHandler for TranspileForBuildRequestHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: TranspileForBuildParams,
    ) -> crate::server::Result<Restage> {
        let mut files = Vec::with_capacity(params.text_documents.len());
        for document in &params.text_documents {
            let Some(path) = DocumentKey::from_uri(&document.uri).file_path().cloned() else {
                // an untitled buffer, a notebook cell. the build was made of files, so there is no
                // slot in the tree for one of these
                return Ok(Restage::refuse(format!(
                    "`{}` is not a file on disk, so no build was made from it",
                    document.uri.as_str(),
                )));
            };
            files.push(path);
        }

        // A build is made of one project, so the set is answered out of that project's database.
        // Files in different projects of this session are not an edit to one build, and either
        // database would transpile the other project's files against the wrong project.
        let Some(db) = project_of(snapshot, &files) else {
            return Ok(Restage::refuse(
                "the edited files belong to different projects, and a build is made of one",
            ));
        };

        // On a thread of its own, and that is not an optimisation — see
        // [`super::detached_transpile`] for why a transpile cannot run on a thread that
        // already has a database attached. A clone rather than a borrow, because a salsa
        // database is not `Sync`: it is cheap to clone and a clone is how every one of this
        // server's worker threads already holds one.
        let owned = db.clone();
        let directory = params.build_directory;
        let files: Vec<_> = files
            .into_iter()
            .map(SystemPathBuf::into_std_path_buf)
            .collect();
        let restaged = transpile_detached(move || restage(&owned, &directory, &files));

        // A panic rather than a failure is reported as a refusal for the reason an error is:
        // a client has one shape to read, and a debugger that got no answer at all would
        // leave the tree it is about to write into in an unknown state.
        let Ok(restaged) = restaged else {
            return Ok(Restage::refuse(
                "the transpiler panicked while re-staging these files",
            ));
        };

        match restaged {
            Ok(restage) => Ok(restage),
            // an error here is the operation failing rather than refusing — the tree could not be
            // read, a file could not be read. it is reported as a refusal so that a client has one
            // shape to read, and the reason is the error's own sentence
            Err(error) => Ok(Restage::refuse(format!("{error:#}"))),
        }
    }
}

/// The one project database every file of the set belongs to, or `None` when they belong to
/// different ones.
///
/// A file belongs to the project with the deepest root above it, which is how the session routes a
/// document. A file under no project's root goes to the first project, as the session sends it too
/// — and the re-stage then refuses it as not one of the files the build was made from.
fn project_of<'a>(
    snapshot: &'a SessionSnapshot,
    files: &[SystemPathBuf],
) -> Option<&'a ProjectDatabase> {
    let projects = snapshot.projects();
    let owner = |file: &SystemPath| {
        projects
            .iter()
            .enumerate()
            .filter(|(_, db)| file.starts_with(db.project().root(*db)))
            .max_by_key(|(_, db)| db.project().root(*db).as_str().len())
            .map_or(0, |(index, _)| index)
    };
    let mut owners = files.iter().map(|file| owner(file));
    let first = owners.next().unwrap_or(0);
    if owners.all(|other| other == first) {
        projects.get(first)
    } else {
        None
    }
}

impl RetriableRequestHandler for TranspileForBuildRequestHandler {}
