//! `by/overridableMembers` and `by/overrideMembers` over the wire
//!
//! which members a class could override, and how a declaration of each is written, is `ty_ide`'s
//! and its unit tests cover it. what these cover is the contract a client reads: a position in a
//! class in, out the members with the superclass and module each comes from, and then for the
//! names picked the edits that write them, imports included, with the first one's body as a range
//! in the text the edits leave behind — and `null` where there is no class

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// `by/overridableMembers`, in json throughout
enum OverridableMembers {}

impl Request for OverridableMembers {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/overridableMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `by/overrideMembers`, in json throughout
enum OverrideMembers {}

impl Request for OverrideMembers {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/overrideMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const BASES: &str = "\
from abc import ABC, abstractmethod
from pathlib import Path

class Shape(ABC):
    @abstractmethod
    def area(self) -> float: ...

    def save(self, to: Path) -> None: ...
";

const MAIN: &str = "\
from bases import Shape

class Square(Shape):
    side = 1.0

def free() -> None: ...
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/bases.by"), BASES)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.by"), MAIN, 1);
    Ok(server)
}

fn list(server: &mut TestServer, line: u32, character: u32) -> Option<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    server.send_request_await::<OverridableMembers>(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    }))
}

#[test]
fn a_class_lists_what_it_inherits_with_where_each_comes_from() -> Result<()> {
    let mut server = server()?;

    // in the body of `Square`
    let answer = list(&mut server, 3, 4).expect("a class is there");
    assert_eq!(answer["className"], "Square");
    let members = answer["members"].as_array().expect("a list of members");
    assert_eq!(
        members[0],
        serde_json::json!({
            "name": "area",
            "containerName": "Shape",
            "containerModule": "bases",
            "abstract": true,
            "kind": "method",
            "signature": "area(self) -> float",
        }),
    );
    assert_eq!(members[1]["name"], "save");
    assert_eq!(members[1]["abstract"], false);
    assert_eq!(members[1]["signature"], "save(self, to: Path)");
    // what every class inherits comes last, from `object`
    assert!(
        members[2..]
            .iter()
            .all(|member| member["containerName"] == "object"),
        "{answer:#}"
    );
    Ok(())
}

#[test]
fn no_class_at_the_position_is_null() -> Result<()> {
    let mut server = server()?;
    // in `free`
    assert_eq!(list(&mut server, 5, 5), None);
    Ok(())
}

#[test]
fn the_members_picked_are_written_with_their_imports_and_the_first_body_selected() -> Result<()> {
    let mut server = server()?;
    let uri = server.file_uri(SystemPath::new("src/main.by"));

    let answer = server
        .send_request_await::<OverrideMembers>(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 14 },
            "members": ["area", "save"],
        }))
        .expect("the members are written");

    let edits = answer["edits"].as_array().expect("a list of edits");
    assert_eq!(
        edits[0],
        serde_json::json!({
            "range": {
                "start": { "line": 3, "character": 14 },
                "end": { "line": 3, "character": 14 },
            },
            "newText": "\n\n    override def area(self) -> float:\n        ...\n\n    override def save(self, to: Path):\n        super.save(to)",
        }),
        "{answer:#}"
    );
    assert_eq!(edits.len(), 2, "{answer:#}");
    assert_eq!(edits[1]["newText"], "from pathlib import Path\n");
    // the `...` of `area`, once the import has moved it down a line
    assert_eq!(
        answer["selection"],
        serde_json::json!({
            "start": { "line": 7, "character": 8 },
            "end": { "line": 7, "character": 11 },
        }),
    );

    // a name the class could not override is not written
    let none = server.send_request_await::<OverrideMembers>(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": 3, "character": 14 },
        "members": ["side"],
    }));
    assert_eq!(none, None);
    Ok(())
}
