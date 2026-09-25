//! `by/superMembers` over the wire
//!
//! which members a member overrides is `ty_ide`'s and its unit tests cover it. what these cover is
//! the contract a client reads: a position on a declared name in, and out the members in MRO order,
//! each in the file that declares it with the name to land on — and `null` kept apart from an empty
//! list, which a client tells a user two different things about

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout
enum SuperMembers {}

impl Request for SuperMembers {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/superMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const BASES: &str = "\
class Root:
    def speak(self) -> str:
        return ''

class Loud(Root):
    def speak(self) -> str:
        return 'HEY'

class Quiet(Root):
    def speak(self) -> str:
        return 'hey'
";

const MAIN: &str = "\
from bases import Loud, Quiet

class Both(Loud, Quiet):
    override def speak(self) -> str:
        return 'hi'

    def listen(self) -> None: ...

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

fn ask(server: &mut TestServer, line: u32, character: u32) -> Option<serde_json::Value> {
    let uri = server.file_uri(SystemPath::new("src/main.by"));
    server.send_request_await::<SuperMembers>(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    }))
}

#[test]
fn a_method_of_a_class_with_two_bases_overrides_each_in_mro_order() -> Result<()> {
    let mut server = server()?;
    let bases = server.file_uri(SystemPath::new("src/bases.by"));

    // on `speak` in `override def speak`
    let answer = ask(&mut server, 3, 17).expect("a member is declared there");
    let members = answer.as_array().expect("a list of members");
    assert_eq!(members.len(), 2, "{answer:#}");

    for (member, (container, line)) in members.iter().zip([("Loud", 5), ("Quiet", 9)]) {
        assert_eq!(member["name"], "speak");
        assert_eq!(member["containerName"], container);
        assert_eq!(member["uri"], bases.as_str());
        assert_eq!(member["synthesized"], false);
        assert_eq!(
            member["selectionRange"],
            serde_json::json!({
                "start": { "line": line, "character": 8 },
                "end": { "line": line, "character": 13 },
            }),
        );
        assert_eq!(member["range"]["start"]["line"], line);
    }
    Ok(())
}

#[test]
fn a_member_that_overrides_nothing_is_an_empty_list_and_no_member_is_null() -> Result<()> {
    let mut server = server()?;

    // on `listen`
    assert_eq!(ask(&mut server, 6, 9), Some(serde_json::json!([])));
    // on `free`, a function no class declares
    assert_eq!(ask(&mut server, 8, 5), None);
    // inside a method body rather than on a declared name
    assert_eq!(ask(&mut server, 4, 9), None);
    Ok(())
}
