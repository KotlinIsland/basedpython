//! `by/transpileForBuild` — one file's slot in a running build's tree, recomputed
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
//! server has already paid both — it holds the project database, warm — so what is left is one
//! file's emit, which is the entire reason this is a request and not a subcommand.
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

use std::borrow::Cow;

use by_stage::restage::{Restage, restage_one};
use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier, Uri};
use ty_project::ProjectDatabase;

use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

/// the request a client sends while its debuggee is running
pub(crate) enum TranspileForBuildRequest {}

impl Request for TranspileForBuildRequest {
    type Params = TranspileForBuildParams;
    type Result = Option<Restage>;
    // not a method LSP defines, so it goes across as a custom one. the `by/` prefix is what keeps
    // it from ever colliding with something the protocol grows later — the same reason
    // `by/dataFlowAt` carries it
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/transpileForBuild");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// what the client knows: which file it edited, and which tree is running
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TranspileForBuildParams {
    /// the file the user edited, in the project
    ///
    /// a document rather than a path, so the source that is transpiled is the one the editor holds.
    /// A client that has not saved yet gets the buffer transpiled and the `.by` digest taken over
    /// that same buffer, which is coherent — though a client doing this for a debugger should save
    /// first anyway, because the traceback rewriter reads the file from disk.
    pub(crate) text_document: TextDocumentIdentifier,

    /// the build tree the program is running out of
    ///
    /// the client knows this and the server cannot: `by run` chooses a temp directory, and the only
    /// thing that sees the name is whatever started the program. It is not trusted on the strength
    /// of being sent — `_by_build.json` in it has to say it was written by this same `by`, or the
    /// answer is a refusal.
    pub(crate) build_directory: std::path::PathBuf,
}

pub(crate) struct TranspileForBuildRequestHandler;

impl RequestHandler for TranspileForBuildRequestHandler {
    type RequestType = TranspileForBuildRequest;
}

impl BackgroundDocumentRequestHandler for TranspileForBuildRequestHandler {
    fn document_uri(params: &TranspileForBuildParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: TranspileForBuildParams,
    ) -> crate::server::Result<Option<Restage>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(path) = file.path(db).as_system_path() else {
            // a file with no path on the filesystem — an untitled buffer, a notebook cell. the
            // build was made of files, so there is no slot in the tree for one of these
            return Ok(None);
        };

        match restage_one(db, &params.build_directory, path.as_std_path()) {
            Ok(restage) => Ok(Some(restage)),
            // an error here is the operation failing rather than refusing — the tree could not be
            // read, the file could not be written out. it is reported as a refusal so that a client
            // has one shape to read, and the reason is the error's own sentence
            Err(error) => Ok(Some(Restage::Refused(by_stage::restage::Refusal {
                refused: format!("{error:#}"),
                diagnostics: Vec::new(),
            }))),
        }
    }
}

impl RetriableRequestHandler for TranspileForBuildRequestHandler {}
