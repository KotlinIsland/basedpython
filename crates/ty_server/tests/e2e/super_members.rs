//! `by/superMembers` and `by/documentSuperMembers` over the wire
//!
//! which members a member overrides is `ty_ide`'s and its unit tests cover it. what these cover is
//! the contract a client reads: a position on a declared name in, and out the members in MRO order,
//! each in the file that declares it with the name to land on — and `null` kept apart from an empty
//! list, which a client tells a user two different things about. and for a whole document, every
//! member that overrides something with the same members, and the name each is declared with, about
//! the text the client names

use anyhow::Result;
use lsp_types::{
    LanguageKind, LspRequestMethod, MessageDirection, Request, TextDocumentContentChangeEvent,
    TextDocumentContentChangeWholeDocument,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::notebook::NotebookBuilder;
use crate::{TestServer, TestServerBuilder, text_hash};

/// the request as a client sends it, in json throughout
enum SuperMembers {}

impl Request for SuperMembers {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/superMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// `by/documentSuperMembers`, in json throughout
enum DocumentSuperMembers {}

impl Request for DocumentSuperMembers {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/documentSuperMembers");
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
        assert_eq!(member["abstract"], false);
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

fn ask_document(server: &mut TestServer, text_hash: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({
        "textDocument": { "uri": server.file_uri(SystemPath::new("src/main.by")) },
    });
    if let Some(hash) = text_hash {
        params["textHash"] = serde_json::json!(hash);
    }
    server
        .send_request_await::<DocumentSuperMembers>(params)
        .expect("an answer for a document")
}

#[test]
fn a_document_lists_the_members_that_override_something_as_super_members_answers_each() -> Result<()>
{
    let mut server = server()?;

    let answer = ask_document(&mut server, None);
    let members = answer.as_array().expect("a list of members");
    // `listen` overrides nothing, and `free` is no member
    assert_eq!(members.len(), 1, "{answer:#}");
    let speak = &members[0];
    assert_eq!(speak["name"], "speak");
    assert_eq!(speak["containerName"], "Both");
    assert_eq!(speak["abstract"], false);
    assert_eq!(
        speak["selectionRange"],
        serde_json::json!({
            "start": { "line": 3, "character": 17 },
            "end": { "line": 3, "character": 22 },
        }),
    );
    // the very members `by/superMembers` answers at the name
    assert_eq!(
        Some(speak["superMembers"].clone()),
        ask(&mut server, 3, 17),
        "{answer:#}"
    );
    Ok(())
}

/// a protocol member, and a member two classes up: what an editor marks as implemented rather
/// than overridden, and what it goes to past a class that does not declare it
#[test]
fn a_protocol_member_is_abstract_and_a_member_two_levels_up_is_found() -> Result<()> {
    let main = "\
from typing import Protocol

class Greeter(Protocol):
    def greet(self) -> str: ...

class A:
    def f(self) -> None: ...

class B(A): ...

class C(B, Greeter):
    def f(self) -> None: ...

    def greet(self) -> str:
        return 'hi'
";
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/main.by"), main)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/main.by"), main, 1);

    let answer = ask_document(&mut server, None);
    let members = answer.as_array().expect("a list of members");
    let summary: Vec<_> = members
        .iter()
        .map(|member| {
            let overridden = &member["superMembers"][0];
            (
                member["name"].as_str().unwrap().to_string(),
                member["selectionRange"]["start"]["line"].as_u64().unwrap(),
                overridden["containerName"].as_str().unwrap().to_string(),
                overridden["selectionRange"]["start"]["line"]
                    .as_u64()
                    .unwrap(),
                overridden["abstract"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("f".to_string(), 11, "A".to_string(), 6, false),
            ("greet".to_string(), 13, "Greeter".to_string(), 3, true),
        ],
        "{answer:#}"
    );
    Ok(())
}

/// the same text-naming every document request takes: a document the client has not opened is
/// answered from the file when that is the text named, and a text the server does not hold yet is
/// waited for
#[test]
fn a_document_request_is_answered_about_the_text_it_names() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/bases.by"), BASES)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();

    // not opened: the file on disk, which is the text named
    let answer = ask_document(&mut server, Some(&text_hash(MAIN)));
    assert_eq!(
        answer[0]["selectionRange"]["start"]["line"], 3,
        "{answer:#}"
    );

    // an edit the server has not been sent moves `speak` down a line, and is waited for. the server
    // takes its messages in order, so an answer made when the request arrived would be about the
    // text before the edit, with `speak` on line 3
    server.open_text_document(SystemPath::new("src/main.by"), MAIN, 1);
    let edited = format!("\n{MAIN}");
    let id = server.send_request::<DocumentSuperMembers>(serde_json::json!({
        "textDocument": { "uri": server.file_uri(SystemPath::new("src/main.by")) },
        "textHash": text_hash(&edited),
    }));
    server.change_text_document(
        SystemPath::new("src/main.by"),
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument { text: edited },
            ),
        ],
        2,
    );
    let answer = server
        .await_response::<DocumentSuperMembers>(&id)
        .expect("an answer about the edit");
    assert_eq!(
        answer[0]["selectionRange"]["start"]["line"], 4,
        "{answer:#}"
    );
    Ok(())
}

/// `by/superMembers` names its text as the document request does: answered from the file for a
/// document the client has not opened, and held for an edit the server has not been sent
#[test]
fn a_member_request_is_answered_about_the_text_it_names() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/bases.by"), BASES)?
        .with_file(SystemPath::new("src/main.by"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();
    let uri = server.file_uri(SystemPath::new("src/main.by"));

    // not opened: the file on disk, which is the text named
    let answer = server
        .send_request_await::<SuperMembers>(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 17 },
            "textHash": text_hash(MAIN),
        }))
        .expect("the members `speak` overrides");
    assert_eq!(answer.as_array().map(Vec::len), Some(2), "{answer:#}");

    // `speak` a line down in an edit not sent yet: held until it is. the server takes its messages
    // in order, so an answer made when the request arrived would be about the text before the
    // edit, where that position is inside a method body and has no member
    server.open_text_document(SystemPath::new("src/main.by"), MAIN, 1);
    let edited = format!("\n{MAIN}");
    let id = server.send_request::<SuperMembers>(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": 4, "character": 17 },
        "textHash": text_hash(&edited),
    }));
    server.change_text_document(
        SystemPath::new("src/main.by"),
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument { text: edited },
            ),
        ],
        2,
    );
    let answer = server
        .await_response::<SuperMembers>(&id)
        .expect("the members `speak` overrides, in the edit");
    assert_eq!(answer.as_array().map(Vec::len), Some(2), "{answer:#}");
    Ok(())
}

/// a notebook's ranges are per cell and a template is not python: neither has its members answered
/// for the whole document, and a template has none at a position either, even one whose text
/// would read as python
#[test]
fn a_notebook_or_a_template_is_answered_null() -> Result<()> {
    let template = "class A:\n    x = 1\n\nclass B(A):\n    x = 2\n";
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/templates/page.html"), template)?
        .build()
        .wait_until_workspaces_are_initialized();

    let mut notebook = NotebookBuilder::virtual_file("test.ipynb");
    notebook.add_python_cell("class A:\n    def f(self) -> None: ...\n");
    let cell = notebook.add_python_cell("class B(A):\n    def f(self) -> None: ...\n");
    notebook.open(&mut server);
    server.collect_publish_diagnostic_notifications(2);
    let answer = server.send_request_await::<DocumentSuperMembers>(serde_json::json!({
        "textDocument": { "uri": cell },
    }));
    assert_eq!(answer, None);

    server.open_text_document_as(
        SystemPath::new("src/templates/page.html"),
        template,
        1,
        LanguageKind::new("django-html"),
    );
    let uri = server.file_uri(SystemPath::new("src/templates/page.html"));
    let answer = server.send_request_await::<DocumentSuperMembers>(serde_json::json!({
        "textDocument": { "uri": uri },
    }));
    assert_eq!(answer, None);
    let answer = server.send_request_await::<SuperMembers>(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": 4, "character": 4 },
    }));
    assert_eq!(answer, None);
    Ok(())
}
