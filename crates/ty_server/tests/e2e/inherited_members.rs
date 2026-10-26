//! `by/documentInheritedMembers` over the wire
//!
//! which members a class inherits is `ty_ide`'s and its unit tests cover it. what these cover is
//! the contract a client reads: each class by its name's range in the document, and each member
//! as its superclass's outline has it, with where it is

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum DocumentInheritedMembers {}

impl Request for DocumentInheritedMembers {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> =
        LspRequestMethod::Custom("by/documentInheritedMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const SHAPES: &str = "\
class Shape:
    def __init__(self, name: str) -> None:
        self.name = name

    def describe(self) -> str:
        return self.name
";

const MAIN: &str = "\
from shapes import Shape

class Circle(Shape):
    def __init__(self) -> None:
        super().__init__('circle')
";

#[test]
fn each_class_comes_with_its_superclasses_members() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/shapes.by"), SHAPES)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.by"), MAIN, 1);
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    let shapes = server.file_uri(SystemPath::new("src/shapes.by"));

    let answer = server
        .send_request_await::<DocumentInheritedMembers>(serde_json::json!({
            "textDocument": { "uri": uri },
        }))
        .expect("the classes of a document");
    let classes = answer.as_array().expect("a list");
    assert_eq!(classes.len(), 1);
    assert_eq!(
        classes[0]["selectionRange"],
        serde_json::json!({
            "start": { "line": 2, "character": 6 },
            "end": { "line": 2, "character": 12 },
        })
    );
    let members = classes[0]["members"].as_array().expect("members");
    // `Shape`'s own, then `object`'s; `__init__` is the class's own
    let shape_members: Vec<_> = members
        .iter()
        .filter(|member| member["containerName"] == "Shape")
        .collect();
    assert_eq!(
        shape_members,
        [
            &serde_json::json!({
                "name": "describe",
                "kind": 6,
                "detail": "(self) -> str",
                "containerName": "Shape",
                "containerModule": "shapes",
                "uri": shapes,
                "range": {
                    "start": { "line": 4, "character": 4 },
                    "end": { "line": 5, "character": 24 },
                },
                "selectionRange": {
                    "start": { "line": 4, "character": 8 },
                    "end": { "line": 4, "character": 16 },
                },
            }),
            &serde_json::json!({
                "name": "name",
                "kind": 8,
                "containerName": "Shape",
                "containerModule": "shapes",
                "uri": shapes,
                "range": {
                    "start": { "line": 2, "character": 8 },
                    "end": { "line": 2, "character": 24 },
                },
                "selectionRange": {
                    "start": { "line": 2, "character": 13 },
                    "end": { "line": 2, "character": 17 },
                },
            }),
        ]
    );
    assert!(
        members
            .iter()
            .skip(shape_members.len())
            .all(|member| member["containerName"] == "object"
                && member["containerModule"] == "builtins"
                && member["name"] != "__init__"),
        "{members:#?}"
    );
    Ok(())
}
