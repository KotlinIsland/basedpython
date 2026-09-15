//! `by/syntaxOutline`, and the keyword pairs `textDocument/documentHighlight` answers, over the wire
//!
//! the unit tests in `ty_ide` cover what the outline says about a parse. what they cannot cover is
//! the contract a client reads: json, in lsp line/character positions, with the fields a client
//! keys on spelled the way it expects. a position one character out puts an editor's suite one
//! column out, and nothing inside the crate can see it
//!
//! so the params here are written as a client would send them

use anyhow::Result;
use lsp_types::{
    DocumentHighlightKind, DocumentHighlightParams, DocumentHighlightRequest, LspRequestMethod,
    MessageDirection, PartialResultParams, Position, Request, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

use crate::{TestServer, TestServerBuilder};

/// the request as a client sends it, in json throughout
enum SyntaxOutline {}

impl Request for SyntaxOutline {
    type Params = serde_json::Value;
    type Result = Option<serde_json::Value>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/syntaxOutline");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

fn server_with(content: &str) -> Result<TestServer> {
    let workspace_root = SystemPath::new("src");
    let main = SystemPath::new("src/main.by");

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(main, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(main, content, 1);
    Ok(server)
}

fn outline_of(content: &str) -> Result<serde_json::Value> {
    let mut server = server_with(content)?;
    let main = SystemPath::new("src/main.by");
    let answer = server
        .send_request_await::<SyntaxOutline>(serde_json::json!({
            "textDocument": { "uri": server.file_uri(main) },
        }))
        .expect("the server answers a file it has open");
    Ok(answer)
}

/// a range as (start line, start character, end line, end character)
fn range(value: &serde_json::Value) -> (u64, u64, u64, u64) {
    (
        value["start"]["line"].as_u64().expect("a start line"),
        value["start"]["character"]
            .as_u64()
            .expect("a start character"),
        value["end"]["line"].as_u64().expect("an end line"),
        value["end"]["character"]
            .as_u64()
            .expect("an end character"),
    )
}

#[test]
fn a_suite_reaches_the_client_with_its_keyword_colon_and_body() -> Result<()> {
    let outline = outline_of(
        "\
match: int = 1
def f(x):
    if x:  # a: note
        return \"a:b\"
    else:
        pass
",
    )?;

    let statements = outline["statements"].as_array().expect("statements");
    assert_eq!(statements.len(), 2);

    // `match: int = 1` is an annotated assignment, and a simple statement carries no clauses
    assert_eq!(range(&statements[0]["range"]), (0, 0, 0, 14));
    assert!(statements[0].get("clauses").is_none());

    let def = &statements[1]["clauses"][0];
    assert_eq!(range(&def["keyword"]), (1, 0, 1, 3));
    assert_eq!(range(&def["colon"]), (1, 8, 1, 9));

    let if_statement = &def["body"][0];
    assert_eq!(range(&if_statement["range"]), (2, 4, 5, 12));
    let clauses = if_statement["clauses"].as_array().expect("clauses");
    assert_eq!(range(&clauses[0]["keyword"]), (2, 4, 2, 6));
    // the header's colon, not the one in the comment or the string below it
    assert_eq!(range(&clauses[0]["colon"]), (2, 8, 2, 9));
    assert_eq!(range(&clauses[0]["body"][0]["range"]), (3, 8, 3, 20));
    assert_eq!(range(&clauses[1]["keyword"]), (4, 4, 4, 8));
    assert_eq!(range(&clauses[1]["range"]), (4, 4, 5, 12));

    Ok(())
}

#[test]
fn a_call_statement_and_its_arguments_reach_the_client() -> Result<()> {
    let outline = outline_of("print(a, f\"{b}\")\nprint(c, end=\"\")\n")?;
    let statements = outline["statements"].as_array().expect("statements");

    let call = &statements[0]["call"];
    assert_eq!(range(&call["callee"]), (0, 0, 0, 5));
    assert_eq!(range(&call["arguments"]), (0, 6, 0, 15));
    assert_eq!(call["positionalOnly"], serde_json::json!(true));

    assert_eq!(
        statements[1]["call"]["positionalOnly"],
        serde_json::json!(false)
    );
    Ok(())
}

#[test]
fn a_string_reaches_the_client_with_what_is_inside_it() -> Result<()> {
    let outline = outline_of(
        "\
x = f\"é\\n{y}\"
z = \"\"\"
        text
    \"\"\"
",
    )?;
    let strings = outline["strings"].as_array().expect("strings");
    assert_eq!(strings.len(), 2);

    // utf-16 positions: `é` is one code unit, and the escape after it starts at character 7
    assert_eq!(range(&strings[0]["range"]), (0, 4, 0, 13));
    assert_eq!(range(&strings[0]["escapes"][0]), (0, 7, 0, 9));
    assert_eq!(range(&strings[0]["interpolations"][0]), (0, 9, 0, 12));
    assert!(strings[0].get("strippedIndent").is_none());

    // the content's eight, not the closing quotes' four
    assert_eq!(strings[1]["strippedIndent"], serde_json::json!(8));
    assert_eq!(range(&strings[1]["range"]), (1, 4, 3, 7));
    Ok(())
}

#[test]
fn a_keyword_is_highlighted_with_the_keywords_it_belongs_with() -> Result<()> {
    let content = "\
for x in y:
    if x:
        break
else:
    pass
";
    let mut server = server_with(content)?;
    let main = SystemPath::new("src/main.by");
    let params = DocumentHighlightParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: server.file_uri(main),
            },
            // on the `break`
            position: Position::new(2, 10),
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let answer = server
        .send_request_await::<DocumentHighlightRequest>(params)
        .expect("a keyword with a family is highlighted");

    let highlights: Vec<_> = answer
        .iter()
        .map(|highlight| {
            let range = highlight.range;
            (
                (
                    u64::from(range.start.line),
                    u64::from(range.start.character),
                    u64::from(range.end.line),
                    u64::from(range.end.character),
                ),
                highlight.kind,
            )
        })
        .collect();
    let text = Some(DocumentHighlightKind::Text);
    // the loop, the `break` in its body and its `else` — not the `if` the `break` sits in
    assert_eq!(
        highlights,
        vec![
            ((0, 0, 0, 3), text),
            ((2, 8, 2, 13), text),
            ((3, 0, 3, 4), text),
        ]
    );
    Ok(())
}
