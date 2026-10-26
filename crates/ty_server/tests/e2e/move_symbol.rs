//! `by/prepareMoveSymbol` and `by/moveSymbol` over the wire
//!
//! what a move does to the project, and when it refuses, is `ty_ide`'s and its unit tests cover
//! it. what these cover is the contract a client reads: the definition a position names, the
//! workspace edit of a move, and the reason a move is refused

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::json;

use crate::{TestServer, TestServerBuilder};

/// `by/prepareMoveSymbol`, in json throughout
enum PrepareMoveSymbol {}

impl Request for PrepareMoveSymbol {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/prepareMoveSymbol");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `by/moveSymbol`, in json throughout
enum MoveSymbol {}

impl Request for MoveSymbol {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/moveSymbol");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const UTIL: &str = "\
RATE = 2


def area(r: float) -> float:
    return r * r * RATE


def _private() -> int:
    return 1


def uses_private() -> int:
    return _private()
";

const MAIN: &str = "\
from shapes.util import area

print(area(1))
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes/__init__.py"), "")?
        .with_file(SystemPath::new("src/shapes/util.py"), UTIL)?
        .with_file(SystemPath::new("src/shapes/geometry.py"), "")?
        .with_file(SystemPath::new("src/main.py"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.py"), MAIN, 1);
    Ok(server)
}

#[test]
fn a_reference_names_the_definition_it_is_to() -> Result<()> {
    let mut server = server()?;
    let answer = server.send_request_await::<PrepareMoveSymbol>(json!({
        "textDocument": { "uri": server.file_uri(SystemPath::new("src/main.py")) },
        "position": { "line": 2, "character": 7 },
    }));
    assert_eq!(
        answer,
        Some(json!({
            "name": "area",
            "kind": "function",
            "module": "shapes.util",
            "uri": server.file_uri(SystemPath::new("src/shapes/util.py")),
            "range": {
                "start": { "line": 3, "character": 4 },
                "end": { "line": 3, "character": 8 },
            },
        }))
    );
    Ok(())
}

#[test]
fn a_move_answers_the_edit_to_every_file_it_changes() -> Result<()> {
    let mut server = server()?;
    let util = server.file_uri(SystemPath::new("src/shapes/util.py"));
    let geometry = server.file_uri(SystemPath::new("src/shapes/geometry.py"));
    let main = server.file_uri(SystemPath::new("src/main.py"));
    let answer = server
        .send_request_await::<MoveSymbol>(json!({
            "textDocument": { "uri": main },
            "position": { "line": 2, "character": 7 },
            "target": geometry,
        }))
        .expect("an answer");
    assert_eq!(answer["warnings"], json!([]), "{answer:#}");
    assert!(answer.get("refused").is_none(), "{answer:#}");
    let changes = &answer["edit"]["changes"];
    assert_eq!(
        changes[main.as_str()],
        json!([{
            "range": { "start": { "line": 0, "character": 5 }, "end": { "line": 0, "character": 16 } },
            "newText": "shapes.geometry",
        }]),
        "{answer:#}"
    );
    assert_eq!(
        changes[geometry.as_str()],
        json!([
            {
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "newText": "from shapes.util import RATE\n",
            },
            {
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "newText": "\n\ndef area(r: float) -> float:\n    return r * r * RATE\n",
            },
        ]),
        "{answer:#}"
    );
    assert_eq!(
        changes[util.as_str()],
        json!([{
            "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 7, "character": 0 } },
            "newText": "",
        }]),
        "{answer:#}"
    );
    Ok(())
}

#[test]
fn a_refused_move_says_why() -> Result<()> {
    let mut server = server()?;
    server.open_text_document(SystemPath::new("src/shapes/util.py"), UTIL, 1);
    let answer = server.send_request_await::<MoveSymbol>(json!({
        "textDocument": { "uri": server.file_uri(SystemPath::new("src/shapes/util.py")) },
        "position": { "line": 11, "character": 5 },
        "target": server.file_uri(SystemPath::new("src/shapes/geometry.py")),
    }));
    assert_eq!(
        answer,
        Some(json!({
            "refused": "`uses_private` uses `_private`, which is private to shapes.util; make it \
                        public, or move it too",
            "warnings": [],
        }))
    );
    Ok(())
}
