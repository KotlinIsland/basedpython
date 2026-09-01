//! The `by/transpile` and `by/transpileForBuild` requests, exercised through the
//! server rather than through the library they call.
//!
//! That distinction is the whole reason these exist. The transpiler builds a
//! database of its own whenever it lowers rewritten source, and a request handler
//! runs with the project database already attached — so the nested one is a second
//! attachment on the same thread, which salsa refuses with "Cannot change database
//! mid-query". Nothing about it reproduces from the command line, where nothing is
//! attached, and the `crates/ty` tests that cover the same operations therefore
//! passed throughout. Only a request does it.

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier, Uri};
use ruff_db::system::SystemPath;

use crate::TestServerBuilder;

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TranspileParams {
    text_document: TextDocumentIdentifier,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    reverse: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TranspileResponse {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

enum Transpile {}

impl Request for Transpile {
    type Params = TranspileParams;
    type Result = Option<TranspileResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/transpile");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

fn params(uri: &Uri, source: Option<&str>) -> TranspileParams {
    TranspileParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        reverse: false,
        source: source.map(str::to_owned),
    }
}

/// The fragment path: `source` is supplied, so the whole transpile runs over text
/// rather than the project, and it builds an in-memory database to check that text
/// against. That database is queried while the project's is attached.
#[test]
fn transpiling_a_fragment_answers_rather_than_panicking() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");
    let content = "def f(a: int?) -> int:\n    return a ?? 0\n";

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, content, 1);
    let uri = server.file_uri(main);

    let response = server
        .send_request_await::<Transpile>(params(&uri, Some(content)))
        .expect("the server answered nothing at all");

    assert!(
        response.error.is_none(),
        "transpile failed: {:?}",
        response.error
    );
    let generated = response.source.expect("no generated source");
    assert!(
        generated.contains("def f("),
        "unexpected output:\n{generated}"
    );
    Ok(())
}

/// The whole-document path, which goes through the project database — and then
/// still builds one of its own, because a pre-pass rewrote the source it lowers.
/// `enum class` is such a pre-pass.
#[test]
fn transpiling_a_document_answers_rather_than_panicking() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");
    let content = "enum class Colour:\n    case Red\n    case Green\n\nlet chosen: Colour = Red\n";

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, content, 1);
    let uri = server.file_uri(main);

    let response = server
        .send_request_await::<Transpile>(params(&uri, None))
        .expect("the server answered nothing at all");

    assert!(
        response.error.is_none(),
        "transpile failed: {:?}",
        response.error
    );
    let generated = response.source.expect("no generated source");
    assert!(
        generated.contains("Colour"),
        "unexpected output:\n{generated}"
    );
    Ok(())
}
