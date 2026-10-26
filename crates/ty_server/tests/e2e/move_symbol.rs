//! `by/prepareMoveSymbol` and `by/moveSymbol` over the wire
//!
//! what a move does to the project, and when it refuses, is `ty_ide`'s and its unit tests cover
//! it. what these cover is the contract a client reads: the definition a position names, the
//! workspace edit of a move, and the reason a move is refused

use anyhow::Result;
use lsp_types::{
    CreateFilesParams, DidCreateFilesNotification, FileCreate, LspRequestMethod, MessageDirection,
    Request,
};
use ruff_db::system::SystemPath;
use ruff_source_file::LineEnding;
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
    // geometry.py is empty, so what the move writes there takes the platform's line ending
    let newline = LineEnding::default().as_str();
    assert_eq!(
        changes[geometry.as_str()],
        json!([
            {
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "newText": format!("from shapes.util import RATE{newline}"),
            },
            {
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "newText": format!(
                    "{newline}{newline}def area(r: float) -> float:{newline}    return r * r * RATE{newline}"
                ),
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

/// the import the definition takes with it and the definition are removed as one edit: two
/// removals that both claim the blank lines between them would overlap, which a client rejects
#[test]
fn a_move_s_edits_to_a_file_do_not_overlap() -> Result<()> {
    const CIRCLE: &str = "import math\n\n\ndef area(r): return math.pi*r*r\n";
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes/__init__.py"), "")?
        .with_file(SystemPath::new("src/shapes/circle.py"), CIRCLE)?
        .with_file(SystemPath::new("src/shapes/geometry.py"), "")?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/shapes/circle.py"), CIRCLE, 1);
    let circle = server.file_uri(SystemPath::new("src/shapes/circle.py"));
    let answer = server
        .send_request_await::<MoveSymbol>(json!({
            "textDocument": { "uri": circle },
            "position": { "line": 3, "character": 5 },
            "target": server.file_uri(SystemPath::new("src/shapes/geometry.py")),
        }))
        .expect("an answer");
    assert_eq!(
        answer["edit"]["changes"][circle.as_str()],
        json!([{
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 4, "character": 0 } },
            "newText": "",
        }]),
        "{answer:#}"
    );
    Ok(())
}

/// `by/prepareMoveMembers`, in json throughout
enum PrepareMoveMembers {}

impl Request for PrepareMoveMembers {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/prepareMoveMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `by/pullMembersUp`, in json throughout
enum PullMembersUp {}

impl Request for PullMembersUp {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/pullMembersUp");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `by/pushMembersDown`, in json throughout
enum PushMembersDown {}

impl Request for PushMembersDown {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/pushMembersDown");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const SHAPES: &str = "\
class Shape:
    pass


class Square(Shape):
    SIDES = 4

    def describe(self) -> str:
        return 'square'
";

fn shapes_server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes.py"), SHAPES)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/shapes.py"), SHAPES, 1);
    Ok(server)
}

#[test]
fn a_class_lists_its_members_and_where_they_could_go() -> Result<()> {
    let mut server = shapes_server()?;
    let uri = server.file_uri(SystemPath::new("src/shapes.py"));
    let answer = server.send_request_await::<PrepareMoveMembers>(json!({
        "textDocument": { "uri": uri },
        "position": { "line": 4, "character": 7 },
    }));
    assert_eq!(
        answer,
        Some(json!({
            "class": {
                "name": "Square",
                "qualifiedName": "shapes.Square",
                "uri": uri,
                "range": { "start": { "line": 4, "character": 6 }, "end": { "line": 4, "character": 12 } },
            },
            "members": [
                {
                    "name": "SIDES",
                    "kind": "attribute",
                    "detail": "SIDES",
                    "range": { "start": { "line": 5, "character": 4 }, "end": { "line": 5, "character": 9 } },
                },
                {
                    "name": "describe",
                    "kind": "method",
                    "detail": "describe(self)",
                    "range": { "start": { "line": 7, "character": 8 }, "end": { "line": 7, "character": 16 } },
                },
            ],
            "superclasses": [{
                "name": "Shape",
                "qualifiedName": "shapes.Shape",
                "uri": uri,
                "range": { "start": { "line": 0, "character": 6 }, "end": { "line": 0, "character": 11 } },
            }],
            "subclasses": [],
        }))
    );
    Ok(())
}

#[test]
fn a_pull_up_answers_its_edit_and_a_push_down_with_nowhere_to_go_is_refused() -> Result<()> {
    let mut server = shapes_server()?;
    let uri = server.file_uri(SystemPath::new("src/shapes.py"));
    let pulled = server
        .send_request_await::<PullMembersUp>(json!({
            "textDocument": { "uri": uri },
            "position": { "line": 4, "character": 7 },
            "members": ["describe"],
            "target": "shapes.Shape",
        }))
        .expect("an answer");
    assert_eq!(
        pulled["edit"]["changes"][uri.as_str()],
        json!([
            {
                // the blank line above it and the method, to the end of the file
                "range": { "start": { "line": 6, "character": 0 }, "end": { "line": 9, "character": 0 } },
                "newText": "",
            },
            {
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 2, "character": 0 } },
                "newText": "    def describe(self) -> str:\n        return 'square'\n",
            },
        ]),
        "{pulled:#}"
    );

    let pushed = server.send_request_await::<PushMembersDown>(json!({
        "textDocument": { "uri": uri },
        "position": { "line": 4, "character": 7 },
        "members": ["SIDES"],
    }));
    assert_eq!(
        pushed,
        Some(json!({
            "refused": "no class of the project names Square as a base, so there is nowhere to \
                        push to",
            "warnings": [],
        }))
    );
    Ok(())
}

/// `by/extractSuperclass`, in json throughout
enum ExtractSuperclass {}

impl Request for ExtractSuperclass {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/extractSuperclass");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[test]
fn an_extracted_superclass_takes_the_class_s_bases_and_becomes_its_base() -> Result<()> {
    let mut server = shapes_server()?;
    let uri = server.file_uri(SystemPath::new("src/shapes.py"));
    let answer = server
        .send_request_await::<ExtractSuperclass>(json!({
            "textDocument": { "uri": uri },
            "position": { "line": 4, "character": 7 },
            "members": ["SIDES"],
            "name": "Polygon",
        }))
        .expect("an answer");
    assert_eq!(
        answer["edit"]["changes"][uri.as_str()],
        json!([
            {
                "range": { "start": { "line": 5, "character": 0 }, "end": { "line": 7, "character": 0 } },
                "newText": "",
            },
            {
                "range": { "start": { "line": 4, "character": 12 }, "end": { "line": 4, "character": 19 } },
                "newText": "(Polygon)",
            },
            {
                "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 4, "character": 0 } },
                "newText": "class Polygon(Shape):\n    SIDES = 4\n\n\n",
            },
        ]),
        "{answer:#}"
    );
    Ok(())
}

/// a module the client has just made is a target once the client reports it with
/// `workspace/didCreateFiles`, even where the server had found nothing at its path before
#[test]
fn a_module_the_client_created_is_a_target_once_it_says_so() -> Result<()> {
    let mut server = server()?;
    let main = server.file_uri(SystemPath::new("src/main.py"));
    let fresh = SystemPath::new("src/shapes/fresh.py");
    let move_to_fresh = |server: &mut TestServer| {
        server
            .send_request_await::<MoveSymbol>(json!({
                "textDocument": { "uri": main },
                "position": { "line": 2, "character": 7 },
                "target": server.file_uri(fresh),
            }))
            .expect("an answer")
    };
    assert_eq!(
        move_to_fresh(&mut server)["refused"],
        json!("the target is not a file that exists")
    );

    server.write_file(fresh, "")?;
    server.send_notification::<DidCreateFilesNotification>(CreateFilesParams {
        files: vec![FileCreate {
            uri: server.file_uri(fresh),
        }],
    });
    let answer = move_to_fresh(&mut server);
    assert!(answer.get("refused").is_none(), "{answer:#}");
    assert!(
        answer["edit"]["changes"]
            .get(server.file_uri(fresh).as_str())
            .is_some(),
        "{answer:#}"
    );
    Ok(())
}
