use std::time::Duration;

use lsp_types::ShowMessageNotification;

use crate::notebook::NotebookBuilder;
use crate::{AwaitResponseError, TestServerBuilder};
use insta::assert_json_snapshot;

#[test]
fn text_document() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("foo.py", "")?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(
        "foo.py",
        r#"def test(): ...

test()
"#,
        1,
    );

    let edits = server
        .rename(
            &server.file_uri("foo.py"),
            lsp_types::Position {
                line: 0,
                character: 5,
            },
            "new_name",
        )
        .expect("Can rename `test` function");

    assert_json_snapshot!(edits);

    Ok(())
}

#[test]
fn notebook() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("test.ipynb", "")?
        .build()
        .wait_until_workspaces_are_initialized();

    let mut builder = NotebookBuilder::virtual_file("test.ipynb");
    builder.add_python_cell(
        r#"from typing import Literal

type Style = Literal["italic", "bold", "underline"]"#,
    );

    let cell2 = builder.add_python_cell(
        r#"def with_style(line: str, word, style: Style) -> str:
    if style == "italic":
        return line.replace(word, f"*{word}*")
    elif style == "bold":
        return line.replace(word, f"__{word}__")

    position = line.find(word)
    output = line + "\n"
    output += " " * position
    output += "-" * len(word)
"#,
    );

    builder.open(&mut server);

    let edits = server
        .rename(
            &cell2,
            lsp_types::Position {
                line: 0,
                character: 16,
            },
            "text",
        )
        .expect("Can rename `line` parameter");

    assert_json_snapshot!(edits);

    server.collect_publish_diagnostic_notifications(2);
    Ok(())
}

/// a name that cannot be renamed is refused with the reason, which the editor
/// shows: a `null` answer gives it nothing to say, and to the user the rename
/// key then looks broken. the refusal is an answer rather than a failure of the
/// server, so no popup says the server hit a problem
#[test]
fn a_builtin_is_refused_with_the_reason() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("foo.py", "")?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document("foo.py", "print(1)\n", 1);

    let id = server.send_request::<lsp_types::PrepareRenameRequest>(prepare_rename_params(
        server.file_uri("foo.py"),
        lsp_types::Position::new(0, 2),
    ));
    let response = server.try_await_response::<lsp_types::PrepareRenameRequest>(&id, None);

    let Err(AwaitResponseError::RequestFailed(failure)) = response else {
        panic!("expected a refusal, got {response:?}");
    };
    assert_eq!(failure.code, lsp_server::ErrorCode::RequestFailed as i32);
    assert_eq!(failure.message, "`print` is declared outside this project");

    let popup =
        server.try_await_notification::<ShowMessageNotification>(Some(Duration::from_millis(100)));
    assert!(popup.is_err(), "got {popup:?}");

    Ok(())
}

/// where there is no name at all there is nothing to explain, and the answer
/// stays `null`
#[test]
fn nothing_to_rename_is_still_a_null_answer() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("foo.py", "")?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document("foo.py", "x = 1\n", 1);

    let prepared = server.send_request_await::<lsp_types::PrepareRenameRequest>(
        prepare_rename_params(server.file_uri("foo.py"), lsp_types::Position::new(0, 4)),
    );

    assert!(prepared.is_none(), "got {prepared:?}");

    Ok(())
}

/// a keyword argument a `.by` file names with a string is offered for renaming
/// by its name, so the editor does not start the new name with the quotes
#[test]
fn a_quoted_keyword_argument_is_offered_by_its_name() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("foo.by", QUOTED_KEYWORD)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document("foo.by", QUOTED_KEYWORD, 1);

    let prepared = server.send_request_await::<lsp_types::PrepareRenameRequest>(
        prepare_rename_params(server.file_uri("foo.by"), lsp_types::Position::new(2, 4)),
    );

    assert_eq!(
        prepared,
        Some(lsp_types::PrepareRenameResult::PrepareRenamePlaceholder(
            lsp_types::PrepareRenamePlaceholder::new(
                lsp_types::Range::new(
                    lsp_types::Position::new(2, 2),
                    lsp_types::Position::new(2, 11)
                ),
                "timeout".to_string(),
            )
        ))
    );

    Ok(())
}

const QUOTED_KEYWORD: &str = "def f(timeout: int): ...\n\nf(\"timeout\"=1)\n";

fn prepare_rename_params(
    uri: lsp_types::Uri,
    position: lsp_types::Position,
) -> lsp_types::PrepareRenameParams {
    lsp_types::PrepareRenameParams {
        text_document_position_params: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
    }
}
