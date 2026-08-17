//! `by/transpile` — the python a `.by` document lowers to, and the reverse.
//!
//! A custom request rather than a subprocess the client spawns over a path. The two differ in what
//! they are looking at: a subprocess reads the file, and an editor's copy of a file is the buffer,
//! not the bytes on disk. Asking `by transpile` about a document with unsaved edits shows the last
//! saved version of it, which is the wrong answer and a quiet one.
//!
//! It is also the wrong *tool*. The server has this project's configuration already resolved and a
//! db with its modules indexed, so it transpiles with cross-module types available; a subprocess
//! rediscovers all of that per call, by a different route, and can disagree with the diagnostics in
//! the same window about what the file means.

use std::borrow::Cow;

use by_transforms::Config;
use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier, Uri};
use ty_project::{Db as _, ProjectDatabase};

use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum TranspileRequest {}

impl Request for TranspileRequest {
    type Params = TranspileParams;
    type Result = Option<TranspileResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/transpile");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Which document, and which way.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TranspileParams {
    /// The document to transpile. Its *buffer* — what the editor holds, which is the point.
    pub(crate) text_document: TextDocumentIdentifier,

    /// When true, go the other way: python in, basedpython out.
    #[serde(default)]
    pub(crate) reverse: bool,

    /// Text to transpile instead of the document's own.
    ///
    /// For a fragment that is not a file: a selection the user asked about has no document of its
    /// own, and the alternative — the client writing it to a temp file and running the CLI over
    /// that — is the very thing this request exists to remove. `text_document` still says which
    /// document the fragment came from, because that is what routes the request to a server; the
    /// fragment is checked on its own, which is all a fragment can be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

/// What came out, or why nothing did.
///
/// A failed transpile is an answer rather than a protocol error: source that does not lower yet is
/// an ordinary state for a file being edited, and an error response would make the client render it
/// as a fault in the server.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TranspileResponse {
    /// The generated source, absent when the transpile failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,

    /// Why it failed, absent when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl TranspileResponse {
    fn generated(source: String) -> Self {
        Self {
            source: Some(source),
            error: None,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            source: None,
            error: Some(error),
        }
    }
}

pub(crate) struct TranspileRequestHandler;

impl RequestHandler for TranspileRequestHandler {
    type RequestType = TranspileRequest;
}

impl BackgroundDocumentRequestHandler for TranspileRequestHandler {
    fn document_uri(params: &TranspileParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: TranspileParams,
    ) -> crate::server::Result<Option<TranspileResponse>> {
        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let config = config_for(db, snapshot.uri());
        let document = ruff_db::source::source_text(db, file);
        let source = params.source.as_deref().unwrap_or(document.as_str());

        // Reverse always goes through the text form: the rewrite is syntactic, and there is no
        // python project db to infer against. Forward uses the project db when it is the whole
        // document that was asked about — that is the reason for answering here, since cross-module
        // types resolve — and the single-file path for a fragment, which has no module to resolve
        // against however it is transpiled.
        let response = if params.reverse {
            match by_transforms::reverse_transpile(source, &config) {
                Ok(out) => TranspileResponse::generated(out),
                Err(error) => TranspileResponse::failed(error),
            }
        } else if params.source.is_some() {
            match by_transforms::transpile(source, &config) {
                Ok(out) => TranspileResponse::generated(out),
                Err(error) => TranspileResponse::failed(error),
            }
        } else {
            match by_transforms::transpile_typed(db, file, &config, None) {
                Ok(out) => TranspileResponse::generated(out),
                Err(error) => TranspileResponse::failed(error.to_string()),
            }
        };

        Ok(Some(response))
    }
}

impl RetriableRequestHandler for TranspileRequestHandler {}

/// The transpile config for the document at [`uri`].
///
/// The minimum version is the project's own, read off the db the server is already holding —
/// rather than rediscovered from the filesystem, which is what a subprocess would have to do and
/// what lets the two disagree.
fn config_for(db: &ProjectDatabase, uri: &Uri) -> Config {
    let path = uri.path().to_string();
    let extension = path.rsplit('.').next().unwrap_or_default();
    Config {
        is_python: matches!(extension, "py" | "pyi"),
        is_stub: matches!(extension, "pyi" | "byi"),
        min_version: db
            .project()
            .program(db)
            .python_version(db)
            .to_string()
            .parse()
            .unwrap_or_else(|_| Config::default().min_version),
        ..Config::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form, exactly as a client sends it. Nothing else here exercises it, and a serde
    /// attribute that makes the typed value unreachable from json would not show up anywhere else.
    #[test]
    fn the_params_a_client_sends_parse() {
        let forward: TranspileParams =
            serde_json::from_str(r#"{"textDocument":{"uri":"file:///a.by"}}"#)
                .expect("reverse is optional");
        assert!(!forward.reverse);

        let reverse: TranspileParams =
            serde_json::from_str(r#"{"textDocument":{"uri":"file:///a.py"},"reverse":true}"#)
                .expect("both fields are accepted");
        assert!(reverse.reverse);
        assert!(reverse.source.is_none());

        let fragment: TranspileParams =
            serde_json::from_str(r#"{"textDocument":{"uri":"file:///a.by"},"source":"x = 1\n"}"#)
                .expect("a fragment carries its own text");
        assert_eq!(fragment.source.as_deref(), Some("x = 1\n"));
    }

    /// A failure travels as a result, so the client can show the reason rather than a dead request.
    #[test]
    fn a_failure_serializes_as_a_reason_and_no_source() {
        let json = serde_json::to_value(TranspileResponse::failed("nope".to_string()))
            .expect("the response is plain data");
        assert_eq!(json["error"], "nope");
        assert!(json.get("source").is_none());
    }

    #[test]
    fn output_serializes_as_source_and_no_reason() {
        let json = serde_json::to_value(TranspileResponse::generated("x = 1\n".to_string()))
            .expect("the response is plain data");
        assert_eq!(json["source"], "x = 1\n");
        assert!(json.get("error").is_none());
    }
}
