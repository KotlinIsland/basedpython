//! `buff/formatSettings` over the wire: the settings `buff` formats a document with, resolved from
//! the configuration that governs it

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum FormatSettings {}

impl Request for FormatSettings {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("buff/formatSettings");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[test]
fn a_document_is_answered_with_the_configuration_that_governs_it() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .with_file(
            "pyproject.toml",
            "[tool.ruff]\nline-length = 100\n\n[tool.ruff.format]\nquote-style = \"single\"\n",
        )?
        .build();
    server.open_text_document_with_language_id("test.by", "basedpython", "x = 1\n", 1);

    let uri = server.file_uri("test.by");
    let id = server.send_request::<FormatSettings>(serde_json::json!({
        "textDocument": { "uri": uri },
    }));
    let answer = server
        .await_response::<FormatSettings>(&id)
        .expect("settings for an open document");

    assert_eq!(answer["lineLength"], 100);
    assert_eq!(answer["indentWidth"], 4);
    assert_eq!(answer["indentStyle"], "space");
    assert_eq!(answer["quoteStyle"], "single");
    assert_eq!(answer["lineEnding"], "auto");
    assert_eq!(
        answer["configurationFile"],
        serde_json::json!(server.file_uri("pyproject.toml"))
    );
    Ok(())
}

#[test]
fn a_document_that_is_not_open_is_answered_null() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    let uri = server.file_uri("missing.by");
    let id = server.send_request::<FormatSettings>(serde_json::json!({
        "textDocument": { "uri": uri },
    }));

    assert_eq!(server.await_response::<FormatSettings>(&id), None);
    Ok(())
}

/// a field the request does not name, such as the `workDoneToken` LSP lets any request carry, is
/// left alone, so a client that sends more than this server reads is still answered
#[test]
fn a_request_with_a_field_it_does_not_name_is_answered() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();
    server.open_text_document_with_language_id("test.by", "basedpython", "x = 1\n", 1);

    let uri = server.file_uri("test.by");
    let id = server.send_request::<FormatSettings>(serde_json::json!({
        "textDocument": { "uri": uri },
        "workDoneToken": "progress",
    }));
    let answer = server
        .await_response::<FormatSettings>(&id)
        .expect("settings for an open document");

    assert_eq!(answer["lineLength"], 88);
    Ok(())
}
