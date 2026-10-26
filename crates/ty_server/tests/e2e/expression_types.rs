//! `by/expressionTypes` over the wire
//!
//! which expressions a range is in, and their types, is `ty_ide`'s and its unit tests cover it.
//! what these cover is the contract a client reads: a range in, and out the expressions that
//! contain it, innermost first, each with its range and its type as the document spells it

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout
enum ExpressionTypes {}

impl Request for ExpressionTypes {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/expressionTypes");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const MAIN: &str = "\
def size(text: str) -> int:
    return len(text) + 1
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.by"), MAIN, 1);
    Ok(server)
}

fn ask(server: &mut TestServer, start: u32, end: u32) -> Option<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    server.send_request_await::<ExpressionTypes>(serde_json::json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 1, "character": start },
            "end": { "line": 1, "character": end },
        },
    }))
}

fn found(start: u32, end: u32, ty: &str) -> serde_json::Value {
    serde_json::json!({
        "range": {
            "start": { "line": 1, "character": start },
            "end": { "line": 1, "character": end },
        },
        "type": ty,
    })
}

#[test]
fn a_caret_answers_every_expression_around_it_innermost_first() -> Result<()> {
    let mut server = server()?;

    assert_eq!(
        ask(&mut server, 17, 17),
        Some(serde_json::json!([
            found(15, 19, "str"),
            found(11, 20, "int"),
            found(11, 24, "int"),
        ]))
    );
    Ok(())
}

#[test]
fn a_selection_starts_from_the_expression_it_spans() -> Result<()> {
    let mut server = server()?;

    assert_eq!(
        ask(&mut server, 11, 20),
        Some(serde_json::json!([
            found(11, 20, "int"),
            found(11, 24, "int")
        ]))
    );
    Ok(())
}

#[test]
fn outside_an_expression_answers_nothing() -> Result<()> {
    let mut server = server()?;

    assert_eq!(ask(&mut server, 0, 0), Some(serde_json::json!([])));
    Ok(())
}
