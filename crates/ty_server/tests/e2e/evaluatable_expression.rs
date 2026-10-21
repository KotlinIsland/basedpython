//! `by/evaluatableExpression` over the wire
//!
//! which expression a position is on is `ty_ide`'s and its unit tests cover it. what these cover is
//! the contract a client reads: a position and whether the expression may call anything in, and out
//! the range of the expression in the document, or `null` where there is nothing to evaluate

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder, text_hash};

/// the request as a client sends it, in json throughout
enum EvaluatableExpression {}

impl Request for EvaluatableExpression {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/evaluatableExpression");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const MAIN: &str = "\
def main(a: object, c: str) -> None:
    total = a.b[c].d + compute(len(c))
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

fn ask(server: &mut TestServer, character: u32, calls: bool) -> Option<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    server.send_request_await::<EvaluatableExpression>(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": 1, "character": character },
        "calls": calls,
        "textHash": text_hash(MAIN),
    }))
}

fn range(start: u32, end: u32) -> serde_json::Value {
    serde_json::json!({
        "range": {
            "start": { "line": 1, "character": start },
            "end": { "line": 1, "character": end },
        }
    })
}

#[test]
fn the_last_name_of_a_reference_answers_the_whole_reference() -> Result<()> {
    let mut server = server()?;

    // on `d`, the end of `a.b[c].d`
    assert_eq!(ask(&mut server, 19, false), Some(range(12, 20)));
    Ok(())
}

#[test]
fn a_callee_is_its_call_only_when_calls_are_allowed() -> Result<()> {
    let mut server = server()?;

    // on `compute`
    assert_eq!(ask(&mut server, 24, false), Some(range(23, 30)));
    assert_eq!(ask(&mut server, 24, true), Some(range(23, 38)));
    Ok(())
}

#[test]
fn an_operator_answers_null() -> Result<()> {
    let mut server = server()?;

    // on `+`
    assert_eq!(ask(&mut server, 21, true), None);
    Ok(())
}
