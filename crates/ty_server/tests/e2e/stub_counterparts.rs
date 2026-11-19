//! a stub and its runtime module over the wire: `by/documentCounterparts`, and the inlay hints a
//! runtime module takes from its stub
//!
//! which declarations pair, and which hints a stub gives, is `ty_ide`'s and its unit tests cover
//! it. what these cover is the contract a client reads: the request's shape both ways round, the
//! open buffers it reads, and the `data` a whole-line hint carries

use anyhow::Result;
use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentContentChangeEvent,
    TextDocumentContentChangeWholeDocument,
};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout, so the shape a client depends on is what
/// is asserted
enum DocumentCounterparts {}

impl Request for DocumentCounterparts {
    type Params = Value;
    type Result = Option<Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/documentCounterparts");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

const STUB: &str = "\
from typing import overload

class Parser:
    def parse(self, text: str) -> list[str]: ...
    def reset(self) -> None: ...

@overload
def load(path: str) -> Parser: ...
@overload
def load(path: bytes) -> Parser: ...
";

const RUNTIME: &str = "\
class Parser:
    def parse(self, text):
        return text.split()

def load(path):
    return Parser()
";

fn server() -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/parsing.pyi"), STUB)?
        .with_file(SystemPath::new("src/parsing.py"), RUNTIME)?
        .with_file(SystemPath::new("src/plain.py"), "def f(): ...\n")?
        .enable_inlay_hints(true)
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(SystemPath::new("src/parsing.py"), RUNTIME, 1);
    server.open_text_document(SystemPath::new("src/parsing.pyi"), STUB, 1);
    Ok(server)
}

fn ask(server: &mut TestServer, path: &str) -> Option<Value> {
    let uri = server.file_uri(SystemPath::new(path));
    server.send_request_await::<DocumentCounterparts>(json!({ "textDocument": { "uri": uri } }))
}

/// each declaration as `kind name line:character -> line:character, ...`, its name's start and
/// each target name's start
fn declarations(answer: &Value) -> Vec<String> {
    let at = |range: &Value| {
        format!(
            "{}:{}",
            range["start"]["line"].as_u64().unwrap_or_default(),
            range["start"]["character"].as_u64().unwrap_or_default()
        )
    };
    answer["declarations"]
        .as_array()
        .expect("declarations")
        .iter()
        .map(|declaration| {
            let targets = declaration["targets"]
                .as_array()
                .expect("targets")
                .iter()
                .map(|target| at(&target["selectionRange"]))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} {} {} -> {targets}",
                declaration["kind"].as_str().unwrap_or_default(),
                declaration["name"].as_str().unwrap_or_default(),
                at(&declaration["selectionRange"]),
            )
        })
        .collect()
}

#[test]
fn a_runtime_module_lists_its_stub_and_what_the_stub_declares() -> Result<()> {
    let mut server = server()?;
    let stub_uri = server.file_uri(SystemPath::new("src/parsing.pyi"));

    let answer = ask(&mut server, "src/parsing.py").expect("a runtime module with a stub");
    assert_eq!(answer["role"], "implementation");
    assert_eq!(answer["counterparts"], json!([{ "uri": stub_uri }]));
    assert_eq!(
        declarations(&answer),
        [
            "class Parser 0:6 -> 2:6",
            "function Parser.parse 1:8 -> 3:8",
            "function load 4:4 -> 7:4, 9:4",
        ]
    );
    // a declaration's whole range is what a caret anywhere in its body is on
    assert_eq!(
        answer["declarations"][1]["range"],
        json!({ "start": { "line": 1, "character": 4 }, "end": { "line": 2, "character": 27 } })
    );
    let load = &answer["declarations"][2]["targets"][0];
    assert_eq!(load["uri"], json!(stub_uri));
    // the whole overload, its decorator included
    assert_eq!(
        load["range"],
        json!({ "start": { "line": 6, "character": 0 }, "end": { "line": 7, "character": 34 } })
    );
    Ok(())
}

#[test]
fn a_stub_lists_its_runtime_module_and_what_both_declare() -> Result<()> {
    let mut server = server()?;
    let runtime_uri = server.file_uri(SystemPath::new("src/parsing.py"));

    let answer = ask(&mut server, "src/parsing.pyi").expect("a stub with a runtime module");
    assert_eq!(answer["role"], "stub");
    assert_eq!(answer["counterparts"], json!([{ "uri": runtime_uri }]));
    // `reset` is the stub's alone, so it is not listed
    assert_eq!(
        declarations(&answer),
        [
            "class Parser 2:6 -> 0:6",
            "function Parser.parse 3:8 -> 1:8",
            "function load 7:4 -> 4:4",
            "function load 9:4 -> 4:4",
        ]
    );
    Ok(())
}

#[test]
fn a_module_with_no_stub_has_no_counterparts() -> Result<()> {
    let mut server = server()?;
    server.open_text_document(SystemPath::new("src/plain.py"), "def f(): ...\n", 1);
    assert_eq!(ask(&mut server, "src/plain.py"), None);
    Ok(())
}

/// what the counterpart's buffer says, not what is on disk
#[test]
fn the_counterparts_open_buffer_is_read() -> Result<()> {
    let mut server = server()?;
    let edited = format!("{STUB}def added() -> None: ...\n");
    server.change_text_document(
        SystemPath::new("src/parsing.pyi"),
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument { text: edited },
            ),
        ],
        2,
    );
    let runtime = format!("{RUNTIME}\ndef added():\n    pass\n");
    server.change_text_document(
        SystemPath::new("src/parsing.py"),
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument { text: runtime },
            ),
        ],
        2,
    );

    let answer = ask(&mut server, "src/parsing.py").expect("a runtime module with a stub");
    assert_eq!(
        declarations(&answer).last().map(String::as_str),
        Some("function added 7:4 -> 10:4")
    );
    Ok(())
}

/// a member only the stub declares is a hint drawn as a line of its own, which says so in its
/// `data`, beside the kind every hint names there
#[test]
fn a_stub_only_member_is_a_whole_line_hint() -> Result<()> {
    let mut server = server()?;

    let hints = server
        .inlay_hints_request(
            SystemPath::new("src/parsing.py"),
            Range::new(Position::new(0, 0), Position::new(6, 0)),
        )
        .expect("hints");
    let hints = serde_json::to_value(hints)?;
    let stub_only: Vec<&Value> = hints
        .as_array()
        .expect("hints")
        .iter()
        .filter(|hint| hint["data"]["kind"] == "stubOnlyMembers")
        .collect();
    let [reset] = stub_only.as_slice() else {
        panic!("one stub-only member, not {stub_only:#?}");
    };
    assert_eq!(
        reset["data"],
        json!({ "kind": "stubOnlyMembers", "placement": "below" })
    );
    // below the last line of `parse`, at the class body's indentation
    assert_eq!(reset["position"], json!({ "line": 2, "character": 27 }));
    assert_eq!(
        reset["label"][0]["value"],
        "    def reset(self) -> None: ..."
    );
    assert_eq!(
        reset["label"][0]["location"]["range"]["start"],
        json!({ "line": 4, "character": 8 })
    );

    // and the stub's annotations are hints of their own kind, inline as every other hint is
    let stub_types: Vec<String> = hints
        .as_array()
        .expect("hints")
        .iter()
        .filter(|hint| hint["data"] == json!({ "kind": "stubTypes" }))
        .map(|hint| {
            hint["label"]
                .as_array()
                .expect("label parts")
                .iter()
                .map(|part| part["value"].as_str().unwrap_or_default())
                .collect()
        })
        .collect();
    // `load`'s overloads agree on what it returns and nothing else, and what they agree on is
    // the stub's own `Parser`, not the runtime module's: the checker does not read the runtime
    // `load` as returning it, so neither does a hint
    assert_eq!(stub_types, [": str", "-> list[str]"]);
    Ok(())
}

/// where a runtime module disagrees with its stub is reported in the runtime module as ordinary
/// diagnostics, and a member it lacks is offered as a quick fix that writes it in as the stub
/// declares it
#[test]
fn a_member_the_runtime_module_lacks_is_reported_and_offered() -> Result<()> {
    let mut server = server()?;
    let runtime = SystemPath::new("src/parsing.py");
    let report = server.document_diagnostic_request(runtime, None);
    let lsp_types::DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) = report
    else {
        panic!("expected a full diagnostic report");
    };
    let diagnostics = report.full_document_diagnostic_report.items;
    let missing: Vec<lsp_types::Diagnostic> = diagnostics
        .into_iter()
        .filter(|diagnostic| {
            serde_json::to_value(diagnostic).expect("diagnostic")["code"]
                == "missing-runtime-member"
        })
        .collect();
    let reported = serde_json::to_value(&missing)?;
    assert_eq!(
        reported
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|diagnostic| (diagnostic["message"].clone(), diagnostic["range"].clone()))
            .collect::<Vec<_>>(),
        // on the class's name
        [(
            json!("`reset` is declared by the stub but not defined by class `Parser`"),
            json!({ "start": { "line": 0, "character": 6 }, "end": { "line": 0, "character": 12 } }),
        )]
    );

    let range = missing[0].range;
    let actions =
        server.send_request_await::<lsp_types::CodeActionRequest>(lsp_types::CodeActionParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: server.file_uri(runtime),
            },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: missing,
                only: Some(vec![lsp_types::CodeActionKind::QuickFix]),
                trigger_kind: None,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        });
    let actions = serde_json::to_value(&actions)?;
    let added = actions
        .as_array()
        .expect("code actions")
        .iter()
        .find(|action| action["title"] == "Add `reset` from the stub")
        .expect("a quick fix adding `reset`");
    let new_texts: Vec<&str> = added["edit"]["changes"]
        .as_object()
        .expect("changes")
        .values()
        .flat_map(|edits| edits.as_array().expect("edits"))
        .map(|edit| edit["newText"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(new_texts, ["\n    def reset(self) -> None: ..."]);
    Ok(())
}
