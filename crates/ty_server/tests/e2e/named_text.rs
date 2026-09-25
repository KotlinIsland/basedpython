//! A document request that names the text it is about, over the wire.
//!
//! A client that keeps an answer against its own revision of a document needs the answer to be
//! about that revision. It says which text it means with `textHash`, and the server answers about
//! that text: at once when it holds it — the open buffer, or the file on disk for a document the
//! client has not opened — and otherwise once it does.

use std::time::Duration;

use anyhow::Result;
use lsp_server::ErrorCode;
use lsp_types::{
    LspRequestMethod, MessageDirection, Request, TextDocumentContentChangeEvent,
    TextDocumentContentChangeWholeDocument,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::notebook::NotebookBuilder;
use crate::{AwaitResponseError, TestServer, TestServerBuilder};

/// `by/syntaxOutline`, in json throughout, as a client sends it.
enum SyntaxOutline {}

impl Request for SyntaxOutline {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/syntaxOutline");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `textDocument/semanticTokens/full`, in json, so that the named text can ride along.
enum SemanticTokens {}

impl Request for SemanticTokens {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> =
        LspRequestMethod::Custom("textDocument/semanticTokens/full");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `textDocument/hover`, in json: a request whose handler answers only an open document.
enum Hover {}

impl Request for Hover {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("textDocument/hover");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// The text hash as a client computes it, written out again here rather than borrowed from the
/// server, so that the two are held to the definition rather than to each other: FNV-1a, 64
/// bits, over UTF-16 code units, every line ending counted as one `\n`.
fn text_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    for unit in normalised.encode_utf16() {
        for byte in unit.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

const MAIN: &str = "src/main.by";

/// On disk: one statement.
const SAVED: &str = "x = 1\n";

/// In the client's editor, not yet sent: a line above it, so every position moves.
const EDITED: &str = "y = \"s\"\nx = 1\n";

fn server() -> Result<TestServer> {
    server_with(SAVED)
}

/// A server whose `MAIN` holds `saved` on disk, and which the client has not opened.
fn server_with(saved: &str) -> Result<TestServer> {
    Ok(TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new(MAIN), saved)?
        .build()
        .wait_until_workspaces_are_initialized())
}

fn outline_params(server: &TestServer, hash: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({
        "textDocument": { "uri": server.file_uri(MAIN) },
    });
    if let Some(hash) = hash {
        params["textHash"] = serde_json::json!(hash);
    }
    params
}

/// The line each top-level statement starts on.
fn statement_lines(outline: &serde_json::Value) -> Vec<u64> {
    outline["statements"]
        .as_array()
        .expect("statements")
        .iter()
        .map(|statement| {
            statement["range"]["start"]["line"]
                .as_u64()
                .expect("a line")
        })
        .collect()
}

/// A short wait, for a response that should not come.
const NOT_YET: Option<Duration> = Some(Duration::from_millis(500));

#[test]
fn a_closed_document_is_answered_about_the_file_when_that_is_the_text_named() -> Result<()> {
    let mut server = server()?;
    let params = outline_params(&server, Some(&text_hash(SAVED)));
    let outline = server
        .send_request_await::<SyntaxOutline>(params)
        .expect("an outline of the file on disk");
    assert_eq!(statement_lines(&outline), vec![0]);
    Ok(())
}

#[test]
fn a_file_with_windows_line_endings_is_the_text_an_editor_holds_with_unix_ones() -> Result<()> {
    let mut server = server_with("y = 1\r\nx = 1\r\n")?;
    let params = outline_params(&server, Some(&text_hash("y = 1\nx = 1\n")));
    let outline = server
        .send_request_await::<SyntaxOutline>(params)
        .expect("the same positions, so the same text");
    assert_eq!(statement_lines(&outline), vec![0, 1]);
    Ok(())
}

#[test]
fn a_closed_document_is_still_refused_when_no_text_is_named() -> Result<()> {
    let mut server = server()?;
    let id = server.send_request::<SyntaxOutline>(outline_params(&server, None));
    match server.try_await_response::<SyntaxOutline>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::InvalidParams as i32);
            assert!(error.message.contains("is not open"), "{}", error.message);
        }
        other => panic!("expected the refusal a closed document always had, got {other:?}"),
    }
    Ok(())
}

/// The case the named text exists for: an edit to a file no editor shows, asked about before the
/// client's `didOpen` for it has arrived. Answering from disk would be answering about the wrong
/// text — every line one out — and a client keeping that against its revision would keep it.
#[test]
fn a_request_for_unsent_text_waits_for_the_did_open_that_brings_it() -> Result<()> {
    let mut server = server()?;
    let id =
        server.send_request::<SyntaxOutline>(outline_params(&server, Some(&text_hash(EDITED))));
    assert!(
        matches!(
            server.try_await_response::<SyntaxOutline>(&id, NOT_YET),
            Err(AwaitResponseError::Timeout)
        ),
        "the file on disk is not the text asked about"
    );

    server.open_text_document(MAIN, EDITED, 0);
    let outline = server
        .await_response::<SyntaxOutline>(&id)
        .expect("an outline of the text asked about");
    assert_eq!(statement_lines(&outline), vec![0, 1]);
    Ok(())
}

#[test]
fn a_request_for_an_edit_waits_for_the_did_change_that_brings_it() -> Result<()> {
    let mut server = server()?;
    server.open_text_document(MAIN, SAVED, 0);
    let id =
        server.send_request::<SyntaxOutline>(outline_params(&server, Some(&text_hash(EDITED))));
    assert!(matches!(
        server.try_await_response::<SyntaxOutline>(&id, NOT_YET),
        Err(AwaitResponseError::Timeout)
    ));

    server.change_text_document(
        MAIN,
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: EDITED.to_string(),
                },
            ),
        ],
        1,
    );
    let outline = server
        .await_response::<SyntaxOutline>(&id)
        .expect("an outline of the edit");
    assert_eq!(statement_lines(&outline), vec![0, 1]);
    Ok(())
}

/// A request for a text that has come and gone is no better answered than one for a text that
/// never came: the client moved on, and cancels.
#[test]
fn a_held_request_the_client_cancels_is_answered_once_as_cancelled() -> Result<()> {
    let mut server = server()?;
    let id =
        server.send_request::<SyntaxOutline>(outline_params(&server, Some(&text_hash(EDITED))));
    assert!(matches!(
        server.try_await_response::<SyntaxOutline>(&id, NOT_YET),
        Err(AwaitResponseError::Timeout)
    ));
    server.cancel(&id);
    match server.try_await_response::<SyntaxOutline>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::RequestCanceled as i32);
        }
        other => panic!("expected the cancellation, got {other:?}"),
    }

    // the text arriving later does not bring a second answer
    server.open_text_document(MAIN, EDITED, 0);
    assert!(matches!(
        server.try_await_response::<SyntaxOutline>(&id, NOT_YET),
        Err(AwaitResponseError::Timeout)
    ));
    Ok(())
}

#[test]
fn a_standard_request_names_its_text_the_same_way() -> Result<()> {
    let mut server = server()?;
    let params = serde_json::json!({
        "textDocument": { "uri": server.file_uri(MAIN) },
        "textHash": text_hash(SAVED),
    });
    let tokens = server
        .send_request_await::<SemanticTokens>(params)
        .expect("semantic tokens for the file on disk");
    assert!(
        !tokens["data"].as_array().expect("token data").is_empty(),
        "{tokens:#?}"
    );
    Ok(())
}

/// A notebook cell is a document of its own, so the text a request about a cell names is the
/// cell's, not the notebook's cells joined together.
#[test]
fn a_request_about_a_cell_names_the_cell_text() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .enable_pull_diagnostics(true)
        .build()
        .wait_until_workspaces_are_initialized();
    let mut notebook = NotebookBuilder::virtual_file("test.ipynb");
    notebook.add_python_cell("x = 1\n");
    let cell = notebook.add_python_cell("y = x\n");
    notebook.open(&mut server);
    server.collect_publish_diagnostic_notifications(2);

    let id = server.send_request::<SemanticTokens>(serde_json::json!({
        "textDocument": { "uri": cell },
        "textHash": text_hash("y = x\n"),
    }));
    let tokens = server
        .try_await_response::<SemanticTokens>(&id, Some(Duration::from_secs(5)))
        .unwrap_or_else(|error| panic!("the cell was not answered: {error}"))
        .expect("semantic tokens for the cell");
    assert!(
        !tokens["data"].as_array().expect("token data").is_empty(),
        "{tokens:#?}"
    );
    Ok(())
}

#[test]
fn each_method_that_reads_a_closed_document_answers_one() -> Result<()> {
    let mut server = server()?;
    for method in [
        "by/syntaxOutline",
        "by/injections",
        "textDocument/semanticTokens/full",
        "textDocument/documentSymbol",
    ] {
        let params = serde_json::json!({
            "textDocument": { "uri": server.file_uri(MAIN) },
            "textHash": text_hash(SAVED),
        });
        let id = server.send_request_named(method, params);
        let answer = server.await_response::<SyntaxOutline>(&id);
        assert!(answer.is_some(), "{method} answered nothing");
    }
    Ok(())
}

/// The file on disk moving ahead of the server: the client has read the new text and asks about
/// it before the server's own watcher has reported the write. The request waits for the watcher
/// rather than being answered about the text the server still has.
#[test]
fn a_request_for_a_written_file_waits_for_the_server_to_see_the_write() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new(MAIN), SAVED)?
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    // the server has read the file as it was
    let before = server
        .send_request_await::<SyntaxOutline>(outline_params(&server, Some(&text_hash(SAVED))))
        .expect("an outline of the file as it was");
    assert_eq!(statement_lines(&before), vec![0]);

    let id =
        server.send_request::<SyntaxOutline>(outline_params(&server, Some(&text_hash(EDITED))));
    server.write_file(MAIN, EDITED)?;
    let after = server
        .await_response::<SyntaxOutline>(&id)
        .expect("an outline of the file as written");
    assert_eq!(statement_lines(&after), vec![0, 1]);
    Ok(())
}

/// A handler that reads only open documents does not start reading the file on disk because the
/// client named its text; the request waits for the client to open it.
#[test]
fn a_handler_that_answers_only_open_documents_waits_for_the_open() -> Result<()> {
    let mut server = server()?;
    let params = serde_json::json!({
        "textDocument": { "uri": server.file_uri(MAIN) },
        "position": { "line": 0, "character": 0 },
        "textHash": text_hash(SAVED),
    });
    let id = server.send_request::<Hover>(params);
    assert!(matches!(
        server.try_await_response::<Hover>(&id, NOT_YET),
        Err(AwaitResponseError::Timeout)
    ));
    server.open_text_document(MAIN, SAVED, 0);
    let hover = server.await_response::<Hover>(&id).expect("a hover on `x`");
    assert!(hover.to_string().contains("int"), "{hover}");
    Ok(())
}

#[test]
fn a_text_hash_that_is_not_one_is_refused() -> Result<()> {
    let mut server = server()?;
    let id = server.send_request::<SyntaxOutline>(outline_params(&server, Some("not a hash")));
    match server.try_await_response::<SyntaxOutline>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::InvalidParams as i32);
        }
        other => panic!("expected invalid params, got {other:?}"),
    }
    Ok(())
}

/// Nothing a client can do makes the server keep a request forever: one whose text never comes
/// is answered once it has waited as long as it may.
#[test]
fn a_request_whose_text_never_comes_is_answered_in_the_end() -> Result<()> {
    let mut server = server()?;
    let id =
        server.send_request::<SyntaxOutline>(outline_params(&server, Some(&text_hash(EDITED))));
    match server.try_await_response::<SyntaxOutline>(&id, Some(Duration::from_secs(30))) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, ErrorCode::ServerCancelled as i32);
        }
        other => panic!("expected the server to give up, got {other:?}"),
    }
    Ok(())
}
