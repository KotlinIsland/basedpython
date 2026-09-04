//! `by/injections` over the wire
//!
//! the unit tests in `ty_ide` cover which fragments the analysis finds. what they cannot cover is
//! the request itself: they work in `TextRange` over one in-memory file, and the contract with a
//! client is json, in lsp line/character positions, for a document the client opened. an off-by-one
//! in that conversion puts an editor's injected fragment one character out of place, which is
//! invisible from inside the crate and total from outside it
//!
//! so the params here are written as a client would send them and never constructed from the
//! server's own types

use anyhow::Result;
use lsp_types::{
    DidOpenTextDocumentNotification, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, DocumentDiagnosticRequest, LanguageKind, LspRequestMethod, Message,
    MessageDirection, PartialResultParams, Request, TextDocumentIdentifier, TextDocumentItem, Uri,
    WorkDoneProgressParams,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum Injections {}

impl Request for Injections {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/injections");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// one fragment, flattened to what a client actually reads off it
#[derive(Debug, PartialEq, Eq)]
struct Fragment {
    language: String,
    origin: String,
    /// each range as (start line, start character, end line, end character). a fragment does span
    /// lines: a dedented triple-quoted string is reported as the run left on each of them, and a
    /// run carries the newline that ends it — except the last, whose newline is the one the closing
    /// quotes sit after and so is not part of the text
    ranges: Vec<(u64, u64, u64, u64)>,
}

fn fragments(answer: &serde_json::Value) -> Vec<Fragment> {
    answer["injections"]
        .as_array()
        .expect("the response to carry a list of injections")
        .iter()
        .map(|injection| Fragment {
            language: injection["language"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            origin: injection["origin"].as_str().unwrap_or_default().to_string(),
            ranges: injection["ranges"]
                .as_array()
                .expect("a fragment to cover something")
                .iter()
                .map(|range| {
                    (
                        range["start"]["line"].as_u64().unwrap_or_default(),
                        range["start"]["character"].as_u64().unwrap_or_default(),
                        range["end"]["line"].as_u64().unwrap_or_default(),
                        range["end"]["character"].as_u64().unwrap_or_default(),
                    )
                })
                .collect(),
        })
        .collect()
}

/// ask about a document the server has open, and flatten the answer
fn injections_in(content: &str) -> Result<Vec<Fragment>> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, content, 1);

    let answer = server
        .send_request_await::<Injections>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(main) },
        }))
        .expect("the server answers a file it is checking");

    Ok(fragments(&answer))
}

#[test]
fn a_marked_statement_reaches_the_client_as_a_fragment() -> Result<()> {
    let found = injections_in(
        "\
# language=javascript
script = \"const x = 1\"
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "javascript".to_string(),
            origin: "comment".to_string(),
            // line 1, inside the quotes
            ranges: vec![(1, 10, 1, 21)],
        }]
    );

    Ok(())
}

#[test]
fn a_language_the_server_knows_nothing_about_is_carried_through_untouched() -> Result<()> {
    // the point of the whole contract: `by` has no idea what elvish is, and says so anyway
    let found = injections_in(
        "\
# language=elvish
spell = \"mellon\"
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "elvish".to_string(),
            origin: "comment".to_string(),
            ranges: vec![(1, 9, 1, 15)],
        }]
    );

    Ok(())
}

#[test]
fn the_language_a_parameter_declares_travels_to_the_call_that_supplies_the_string() -> Result<()> {
    let found = injections_in(
        "\
from typing import Annotated

def f1(s: Annotated[str, \"language=basedpython\"]): ...

def f2(s: str):
    f1(s)

f2(\"None\")
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "basedpython".to_string(),
            origin: "propagated".to_string(),
            ranges: vec![(7, 4, 7, 8)],
        }]
    );

    Ok(())
}

#[test]
fn a_fragment_written_as_two_literals_arrives_as_two_ranges() -> Result<()> {
    let found = injections_in(
        "\
# language=sql
query = \"SELECT *\" \" FROM t\"
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "sql".to_string(),
            origin: "comment".to_string(),
            ranges: vec![(1, 9, 1, 17), (1, 20, 1, 27)],
        }]
    );

    Ok(())
}

#[test]
fn a_file_with_nothing_marked_answers_with_an_empty_list() -> Result<()> {
    // an answer, not a miss: a client that cannot tell "no fragments" from "no server" has to
    // guess whether to leave its existing injections in place
    let found = injections_in(
        "\
script = \"const x = 1\"
",
    )?;

    assert_eq!(found, Vec::new());

    Ok(())
}

/// the fragment, opened the way an editor that injects basedpython into basedpython opens it: as a
/// document of its own, named after the host it was cut from
///
/// this is the other half of injection, and the half that only works because the fragment *is* a
/// basedpython module. the server is not asked anything new here — it is asked
/// `textDocument/diagnostic` about a document, exactly as it is for a file — which is the point:
/// everything the server already answers about a `.by` file, it answers about a fragment
#[test]
fn a_fragment_opened_as_its_own_document_is_checked_like_any_other() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");
    let host = "# language=basedpython\nsnippet = \"\"\"\nx: int = \"no\"\n\"\"\"\n";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(main, host)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, host, 1);

    // the uri names the host and which of its fragments this is, and ends in `.by` because that is
    // how the server knows what it is reading
    let fragment = Uri::parse("by-injected:/src/main.by/0.by").expect("a uri a client can build");
    server.send_notification::<DidOpenTextDocumentNotification>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: fragment.clone(),
            language_id: LanguageKind::from("basedpython".to_string()),
            version: 1,
            text: "x: int = \"no\"\n".to_string(),
        },
    });

    let report = server.send_request_await::<DocumentDiagnosticRequest>(DocumentDiagnosticParams {
        text_document: TextDocumentIdentifier { uri: fragment },
        identifier: Some("ty".to_string()),
        previous_result_id: None,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    });

    let DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) = report else {
        panic!("the server should have checked the fragment, and answered with a full report");
    };
    let reported: Vec<_> = report
        .full_document_diagnostic_report
        .items
        .iter()
        .map(|diagnostic| {
            let Message::String(message) = &diagnostic.message else {
                panic!(
                    "a diagnostic message should be a string, and was {:#?}",
                    diagnostic.message
                )
            };
            (
                message.clone(),
                diagnostic.range.start.line,
                diagnostic.range.start.character,
            )
        })
        .collect();

    // in the fragment's own coordinates, which is what lets a client map them back through the
    // injection it made
    assert_eq!(
        reported.len(),
        1,
        "the fragment's error should be reported once, and was {reported:?}"
    );
    assert_eq!((reported[0].1, reported[0].2), (0, 9));
    assert!(
        reported[0].0.contains("int"),
        "the message should be about the assignment, and was {:?}",
        reported[0].0,
    );

    Ok(())
}

/// A dedented triple-quoted string is reported as the text it stands for, not as
/// the block between its quotes.
///
/// This is the shape a client has the most to get wrong: the fragment spans
/// lines, each run starts past the indentation rather than at the line's start,
/// and the newline that ends a line belongs to the run before it. Getting any of
/// that wrong puts the editor's injected fragment out of step with the file it
/// came from.
#[test]
fn a_dedented_string_reaches_the_client_as_the_text_it_stands_for() -> Result<()> {
    let found = injections_in(
        "\
def render():
    # language=html
    page = \"\"\"
    <div>
    asdf
    </div>
    \"\"\"
",
    )?;

    assert_eq!(
        found,
        vec![Fragment {
            language: "html".to_string(),
            origin: "comment".to_string(),
            // `<div>` and its newline, `asdf` and its newline, then `</div>`
            ranges: vec![(3, 4, 4, 0), (4, 4, 5, 0), (5, 4, 5, 10)],
        }]
    );

    Ok(())
}

/// An indented `basedpython` fragment, cut out by the ranges the server reported and opened as its
/// own document, is a module rather than a syntax error.
///
/// This is what the dedent is for. The fragment sits inside a function, so the block between the
/// quotes starts every line with indentation no module may start with; it is only because the
/// reported ranges begin past that indentation that what the client cuts out parses at all. The
/// error the fragment then reports is the one written in it, at the column it is written at.
#[test]
fn an_indented_basedpython_fragment_is_a_module_and_not_a_syntax_error() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");
    let host = "\
def build():
    # language=basedpython
    snippet = \"\"\"
    x: int = \"no\"
    \"\"\"
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(main, host)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, host, 1);

    let answer = server
        .send_request_await::<Injections>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(main) },
        }))
        .expect("the server answers a file it is checking");

    assert_eq!(
        fragments(&answer),
        vec![Fragment {
            language: "basedpython".to_string(),
            origin: "comment".to_string(),
            // the one line, starting past the four spaces that indent it
            ranges: vec![(3, 4, 3, 17)],
        }]
    );

    // exactly the characters those ranges name, which is what a client cuts out
    let fragment = Uri::parse("by-injected:/src/main.by/0.by").expect("a uri a client can build");
    server.send_notification::<DidOpenTextDocumentNotification>(DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: fragment.clone(),
            language_id: LanguageKind::from("basedpython".to_string()),
            version: 1,
            text: "x: int = \"no\"".to_string(),
        },
    });

    let report = server.send_request_await::<DocumentDiagnosticRequest>(DocumentDiagnosticParams {
        text_document: TextDocumentIdentifier { uri: fragment },
        identifier: Some("ty".to_string()),
        previous_result_id: None,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    });

    let DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) = report else {
        panic!("the server should have checked the fragment, and answered with a full report");
    };
    let reported: Vec<_> = report
        .full_document_diagnostic_report
        .items
        .iter()
        .map(|diagnostic| {
            let Message::String(message) = &diagnostic.message else {
                panic!(
                    "a diagnostic message should be a string, and was {:#?}",
                    diagnostic.message
                )
            };
            (
                message.clone(),
                diagnostic.range.start.line,
                diagnostic.range.start.character,
            )
        })
        .collect();

    assert_eq!(
        reported.len(),
        1,
        "the fragment's error should be reported once, and was {reported:?}"
    );
    assert_eq!((reported[0].1, reported[0].2), (0, 9));
    assert!(
        reported[0].0.contains("int"),
        "the message should be about the assignment, and was {:?}",
        reported[0].0,
    );

    Ok(())
}
