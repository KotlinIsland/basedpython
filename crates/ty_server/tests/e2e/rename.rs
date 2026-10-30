use std::time::Duration;

use lsp_types::ShowMessageNotification;

use crate::notebook::NotebookBuilder;
use crate::{AwaitResponseError, TestServer, TestServerBuilder};
use insta::assert_json_snapshot;
use ruff_db::system::SystemPath;
use ty_server::ClientOptions;

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

/// a client that renames without asking first is refused just the same, rather
/// than handed an edit renaming every use of `print` in the project
#[test]
fn a_rename_of_a_builtin_is_refused_without_asking_first() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("foo.py", "")?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document("foo.py", "print(1)\n", 1);

    let response = rename_at(
        &mut server,
        "foo.py",
        lsp_types::Position::new(0, 2),
        "show",
    );

    assert_eq!(
        refusal(response),
        "`print` is declared outside this project"
    );

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

/// the quotes are part of what is replaced, so the argument is left written
/// the plain way, matching the parameter
#[test]
fn a_quoted_keyword_argument_is_renamed_to_a_bare_name() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("foo.by", QUOTED_KEYWORD)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document("foo.by", QUOTED_KEYWORD, 1);

    let edit = rename_at(
        &mut server,
        "foo.by",
        lsp_types::Position::new(2, 4),
        "retries",
    )
    .expect("the rename is carried out")
    .expect("an edit");

    let edits = edit
        .changes
        .expect("edits by file")
        .remove(&server.file_uri("foo.by"))
        .expect("edits to foo.by");

    assert_eq!(
        apply(QUOTED_KEYWORD, edits),
        "def f(retries: int): ...\n\nf(retries=1)\n"
    );

    Ok(())
}

/// a new name that is not a name, or is a keyword, would leave the code unable
/// to parse, so the rename is refused rather than written
#[test]
fn a_new_name_that_is_not_a_name_is_refused() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("foo.by", QUOTED_KEYWORD)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document("foo.by", QUOTED_KEYWORD, 1);

    let quoted = rename_at(
        &mut server,
        "foo.by",
        lsp_types::Position::new(2, 4),
        "\"retries\"",
    );
    assert_eq!(refusal(quoted), "`\"retries\"` is not a valid name");

    let keyword = rename_at(
        &mut server,
        "foo.by",
        lsp_types::Position::new(2, 4),
        "class",
    );
    assert_eq!(refusal(keyword), "`class` is a keyword");

    Ok(())
}

fn rename_at(
    server: &mut TestServer,
    path: &str,
    position: lsp_types::Position,
    new_name: &str,
) -> Result<Option<lsp_types::WorkspaceEdit>, AwaitResponseError> {
    let id = server.send_request::<lsp_types::RenameRequest>(lsp_types::RenameParams {
        text_document_position_params: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: server.file_uri(path),
            },
            position,
        },
        new_name: new_name.to_string(),
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
    });
    server.try_await_response::<lsp_types::RenameRequest>(&id, None)
}

/// the reason a request was refused, asserting that it was
#[track_caller]
fn refusal<T: std::fmt::Debug>(response: Result<T, AwaitResponseError>) -> String {
    let Err(AwaitResponseError::RequestFailed(failure)) = response else {
        panic!("expected a refusal, got {response:?}");
    };
    assert_eq!(failure.code, lsp_server::ErrorCode::RequestFailed as i32);
    failure.message
}

/// `source` with `edits` applied, for edits that each stay on one line of
/// ascii text
fn apply(source: &str, mut edits: Vec<lsp_types::TextEdit>) -> String {
    let mut lines: Vec<String> = source.split_inclusive('\n').map(str::to_string).collect();
    edits
        .sort_by_key(|edit| std::cmp::Reverse((edit.range.start.line, edit.range.start.character)));
    for edit in edits {
        assert_eq!(edit.range.start.line, edit.range.end.line);
        let line = &mut lines[edit.range.start.line as usize];
        line.replace_range(
            edit.range.start.character as usize..edit.range.end.character as usize,
            &edit.new_text,
        );
    }
    lines.concat()
}

/// `textDocument/rename`, answered as the json the server sent
///
/// an annotated text edit reads as a plain one through `lsp_types::Edit`, which tries the plain
/// shape first, so the annotations are read off the json instead
enum RawRename {}

impl lsp_types::Request for RawRename {
    type Params = lsp_types::RenameParams;
    type Result = serde_json::Value;
    const METHOD: lsp_types::LspRequestMethod<'static> =
        lsp_types::LspRequestMethod::TextDocumentRename;
    const MESSAGE_DIRECTION: lsp_types::MessageDirection =
        lsp_types::MessageDirection::ClientToServer;
}

/// rename what is at `line` and `character` of `animal.by` to `new_name`, and summarize the
/// edit one line per text edit — the file, where, the new text, and the change annotation it
/// carries — then each file renamed and each annotation
fn rename_summary(server: &mut TestServer, line: u32, character: u32, new_name: &str) -> String {
    use std::fmt::Write as _;

    let edit = server.send_request_await::<RawRename>(lsp_types::RenameParams {
        text_document_position_params: lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: server.file_uri("animal.by"),
            },
            position: lsp_types::Position::new(line, character),
        },
        new_name: new_name.to_string(),
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
    });
    let name = |uri: &serde_json::Value| {
        uri.as_str()
            .and_then(|uri| uri.rsplit('/').next())
            .unwrap_or_default()
            .to_owned()
    };
    let mut summary = String::new();
    for change in edit["documentChanges"].as_array().into_iter().flatten() {
        if change["kind"] == "rename" {
            let _ = writeln!(
                summary,
                "rename {} -> {} {}",
                name(&change["oldUri"]),
                name(&change["newUri"]),
                change["annotationId"].as_str().unwrap_or("-")
            );
            continue;
        }
        let mut edits: Vec<&serde_json::Value> =
            change["edits"].as_array().into_iter().flatten().collect();
        edits.sort_by_key(|edit| {
            (
                edit["range"]["start"]["line"].as_u64(),
                edit["range"]["start"]["character"].as_u64(),
            )
        });
        for edit in edits {
            let start = &edit["range"]["start"];
            let _ = writeln!(
                summary,
                "{} {}:{} {} {}",
                name(&change["textDocument"]["uri"]),
                start["line"].as_u64().unwrap_or_default() + 1,
                start["character"].as_u64().unwrap_or_default() + 1,
                edit["newText"],
                edit["annotationId"].as_str().unwrap_or("-")
            );
        }
    }
    let mut annotations: Vec<(&String, &serde_json::Value)> = edit["changeAnnotations"]
        .as_object()
        .into_iter()
        .flatten()
        .collect();
    annotations.sort_by_key(|(id, _)| id.as_str());
    for (id, annotation) in annotations {
        let _ = writeln!(
            summary,
            "{id}: {} / {} / confirm {}",
            annotation["label"], annotation["description"], annotation["needsConfirmation"]
        );
    }
    summary
}

const ANIMAL: &str = "\
class Animal:
    def speak(self, loud: bool) -> str:
        return \"\"
";

const DOG: &str = "\
from animal import Animal


class Dog(Animal):
    override def speak(self, loud: bool) -> str:
        return \"woof\" if loud else \"w\"


def f(d: Dog, a: Animal) -> None:
    print(d.speak(loud=True))
    print(a.speak(loud=False))
";

fn animals(annotated: bool) -> anyhow::Result<TestServer> {
    let builder = TestServerBuilder::new()?
        .with_file("animal.by", ANIMAL)?
        .with_file("dog.by", DOG)?;
    let builder = if annotated {
        builder.enable_annotated_workspace_edits()
    } else {
        builder
    };
    let mut server = builder.build().wait_until_workspaces_are_initialized();
    server.open_text_document("animal.by", ANIMAL, 1);
    Ok(server)
}

/// renaming a method offers the methods that override it, which then keep overriding it, as
/// edits the user confirms: they are `Dog.speak`'s own uses, which the rename of `Animal.speak`
/// does not reach
#[test]
fn a_method_rename_offers_its_overrides() -> anyhow::Result<()> {
    let mut server = animals(true)?;
    insta::assert_snapshot!(rename_summary(&mut server, 1, 9, "talk"), @r#"
    animal.by 2:9 "talk" -
    dog.by 5:18 "talk" override:0
    dog.by 10:13 "talk" override:0
    dog.by 11:13 "talk" -
    override:0: "Rename overriding methods" / "`dog.Dog.speak`" / confirm true
    "#);
    Ok(())
}

/// a parameter of a method is offered renamed in the methods that override it, with its uses
/// there, keyword arguments included
#[test]
fn a_parameter_rename_offers_the_overrides_parameters() -> anyhow::Result<()> {
    let mut server = animals(true)?;
    insta::assert_snapshot!(rename_summary(&mut server, 1, 21, "shout"), @r#"
    animal.by 2:21 "shout" -
    dog.by 5:30 "shout" parameter:0
    dog.by 6:26 "shout" parameter:0
    dog.by 10:19 "shout" parameter:0
    dog.by 11:19 "shout" -
    parameter:0: "Rename parameters of overriding methods" / "`loud` of `dog.Dog.speak`" / confirm true
    "#);
    Ok(())
}

/// a class rename offers to move the module named after it, with the imports of it
#[test]
fn a_class_rename_offers_the_module_named_after_it() -> anyhow::Result<()> {
    let mut server = animals(true)?;
    insta::assert_snapshot!(rename_summary(&mut server, 0, 7, "Beast"), @r#"
    animal.by 1:7 "Beast" -
    dog.by 1:6 "beast" module:0
    dog.by 1:20 "Beast" -
    dog.by 4:11 "Beast" -
    dog.by 9:18 "Beast" -
    rename animal.by -> beast.by module:0
    module:0: "Rename module" / "`animal.by` to `beast.by`" / confirm true
    "#);
    Ok(())
}

/// a client that cannot be asked to confirm part of an edit gets the rename alone, as plain
/// `changes`
#[test]
fn a_client_without_change_annotations_gets_the_rename_alone() -> anyhow::Result<()> {
    let mut server = animals(false)?;
    let edit = server
        .rename(
            &server.file_uri("animal.by"),
            lsp_types::Position::new(1, 9),
            "talk",
        )
        .expect("`speak` can be renamed")
        .expect("and is");
    assert!(edit.document_changes.is_none());
    assert!(edit.change_annotations.is_none());
    let edited: usize = edit
        .changes
        .iter()
        .flatten()
        .map(|(_, edits)| edits.len())
        .sum();
    assert_eq!(edited, 2, "{edit:#?}");
    Ok(())
}

const NAMED: &str = "\
class HttpRequest: ...


class User: ...


def get_user() -> User:
    return User()


r = HttpRequest()
x = get_user()
found: list[User] = []
print(found)
nothing = None
print(nothing)
";

fn validate(
    server: &mut TestServer,
    line: u32,
    character: u32,
    new_name: &str,
) -> serde_json::Value {
    let id = server.send_request_named(
        "by/validateRenameName",
        serde_json::json!({
            "textDocument": { "uri": server.file_uri("named.by") },
            "position": { "line": line, "character": character },
            "newName": new_name,
        }),
    );
    server.await_response::<RawResponse>(&id)
}

fn suggestions(server: &mut TestServer, line: u32, character: u32) -> serde_json::Value {
    let id = server.send_request_named(
        "by/renameNameSuggestions",
        serde_json::json!({
            "textDocument": { "uri": server.file_uri("named.by") },
            "position": { "line": line, "character": character },
        }),
    );
    server.await_response::<RawResponse>(&id)
}

/// any request, answered as the json the server sent
pub(crate) enum RawResponse {}

impl lsp_types::Request for RawResponse {
    type Params = serde_json::Value;
    type Result = serde_json::Value;
    const METHOD: lsp_types::LspRequestMethod<'static> =
        lsp_types::LspRequestMethod::Custom("by/raw");
    const MESSAGE_DIRECTION: lsp_types::MessageDirection =
        lsp_types::MessageDirection::ClientToServer;
}

/// a new name is judged by the rule the rename refuses it by, before the rename is asked for
#[test]
fn a_new_name_is_validated_by_the_renames_rule() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("named.by", NAMED)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document("named.by", NAMED, 1);

    assert_eq!(
        validate(&mut server, 10, 0, "request"),
        serde_json::json!({ "valid": true })
    );
    assert_eq!(
        validate(&mut server, 10, 0, "class"),
        serde_json::json!({ "valid": false, "message": "`class` is a keyword" })
    );
    assert_eq!(
        validate(&mut server, 10, 0, "two words"),
        serde_json::json!({ "valid": false, "message": "`two words` is not a valid name" })
    );
    // a builtin cannot be renamed at all, whatever its new name
    assert_eq!(validate(&mut server, 13, 0, "show")["valid"], false);
    // and there is nothing to rename in whitespace
    assert_eq!(validate(&mut server, 9, 0, "y"), serde_json::Value::Null);
    Ok(())
}

/// names a symbol could take come from the class of what it holds and what computes it
#[test]
fn a_rename_suggests_names_from_the_value() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("named.by", NAMED)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document("named.by", NAMED, 1);

    let found = [10, 11, 12, 14].map(|line| suggestions(&mut server, line, 0));
    assert_eq!(
        found,
        [
            serde_json::json!(["http_request", "request"]),
            serde_json::json!(["user"]),
            serde_json::json!(["users", "items"]),
            // `None` is a `types.NoneType`, which says nothing of what the value is for
            serde_json::json!(["value"]),
        ]
    );
    Ok(())
}

/// the requests about a rename answer nothing where the rename itself does
#[test]
fn rename_requests_answer_nothing_with_language_services_disabled() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(
            SystemPath::new("src"),
            Some(ClientOptions::default().with_disable_language_services(true)),
        )?
        .with_file("src/animal.by", ANIMAL)?
        .with_file("src/dog.by", DOG)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document("src/animal.by", ANIMAL, 1);
    let uri = server.file_uri("src/animal.by");
    let position = serde_json::json!({ "line": 1, "character": 9 });
    let at = serde_json::json!({ "textDocument": { "uri": uri }, "position": position });
    let named = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": position,
        "newName": "talk",
    });

    let id = server.send_request_named("by/validateRenameName", named.clone());
    assert_eq!(
        server.await_response::<RawResponse>(&id),
        serde_json::Value::Null
    );
    let id = server.send_request_named("by/renameNameSuggestions", at);
    assert_eq!(
        server.await_response::<RawResponse>(&id),
        serde_json::json!([])
    );
    let id = server.send_request_named("by/renameOffers", named);
    assert_eq!(
        server.await_response::<RawResponse>(&id),
        serde_json::json!([])
    );
    Ok(())
}

/// `by/renameOffers` for renaming what is at `line` and `character` of `animal.by`, one line per
/// offer and then one per edit it makes
fn offers_summary(server: &mut TestServer, line: u32, character: u32, new_name: &str) -> String {
    use std::fmt::Write as _;

    let id = server.send_request_named(
        "by/renameOffers",
        serde_json::json!({
            "textDocument": { "uri": server.file_uri("animal.by") },
            "position": { "line": line, "character": character },
            "newName": new_name,
        }),
    );
    let offers = server.await_response::<RawResponse>(&id);
    let name = |uri: &serde_json::Value| {
        uri.as_str()
            .and_then(|uri| uri.rsplit('/').next())
            .unwrap_or_default()
            .to_owned()
    };
    let edit_line = |file: &str, edit: &serde_json::Value| {
        let start = &edit["range"]["start"];
        format!(
            "  {file} {}:{} {}",
            start["line"].as_u64().unwrap_or_default() + 1,
            start["character"].as_u64().unwrap_or_default() + 1,
            edit["newText"]
        )
    };
    let mut summary = String::new();
    for offer in offers.as_array().into_iter().flatten() {
        let _ = writeln!(
            summary,
            "{}: {} / {}",
            offer["id"].as_str().unwrap_or_default(),
            offer["label"],
            offer["description"]
        );
        let edit = &offer["edit"];
        let mut lines = Vec::new();
        for (uri, edits) in edit["changes"].as_object().into_iter().flatten() {
            let file = name(&serde_json::Value::String(uri.clone()));
            lines.extend(
                edits
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|edit| edit_line(&file, edit)),
            );
        }
        for change in edit["documentChanges"].as_array().into_iter().flatten() {
            if change["kind"] == "rename" {
                lines.push(format!(
                    "  rename {} -> {}",
                    name(&change["oldUri"]),
                    name(&change["newUri"])
                ));
                continue;
            }
            let file = name(&change["textDocument"]["uri"]);
            for edit in change["edits"].as_array().into_iter().flatten() {
                assert!(edit.get("annotationId").is_none(), "{edit}");
                lines.push(edit_line(&file, edit));
            }
        }
        lines.sort();
        for line in lines {
            let _ = writeln!(summary, "{line}");
        }
    }
    summary
}

/// the offers a rename comes with, each as a workspace edit of its own, for a client whose
/// workspace edits carry no change annotations: the same offers, with the same identifiers, as
/// the annotated rename
#[test]
fn rename_offers_are_answered_one_edit_each() -> anyhow::Result<()> {
    let mut server = animals(false)?;
    insta::assert_snapshot!(offers_summary(&mut server, 1, 9, "talk"), @r#"
    override:0: "Rename overriding methods" / "`dog.Dog.speak`"
      dog.by 10:13 "talk"
      dog.by 5:18 "talk"
    "#);
    insta::assert_snapshot!(offers_summary(&mut server, 1, 21, "shout"), @r#"
    parameter:0: "Rename parameters of overriding methods" / "`loud` of `dog.Dog.speak`"
      dog.by 10:19 "shout"
      dog.by 5:30 "shout"
      dog.by 6:26 "shout"
    "#);
    insta::assert_snapshot!(offers_summary(&mut server, 0, 7, "Beast"), @r#"
    module:0: "Rename module" / "`animal.by` to `beast.by`"
      dog.by 1:6 "beast"
      rename animal.by -> beast.by
    "#);
    Ok(())
}

/// a new name the rename would refuse is refused with its reason, and nothing to rename has
/// nothing to offer
#[test]
fn rename_offers_refuse_what_the_rename_refuses() -> anyhow::Result<()> {
    let mut server = animals(false)?;
    let id = server.send_request_named(
        "by/renameOffers",
        serde_json::json!({
            "textDocument": { "uri": server.file_uri("animal.by") },
            "position": { "line": 1, "character": 9 },
            "newName": "class",
        }),
    );
    let refused = server
        .try_await_response::<RawResponse>(&id, None)
        .expect_err("a keyword is no new name");
    assert!(
        format!("{refused}").contains("`class` is a keyword"),
        "{refused}"
    );

    let id = server.send_request_named(
        "by/renameOffers",
        serde_json::json!({
            "textDocument": { "uri": server.file_uri("animal.by") },
            "position": { "line": 0, "character": 0 },
            "newName": "anything",
        }),
    );
    assert_eq!(
        server.await_response::<RawResponse>(&id),
        serde_json::json!([])
    );
    Ok(())
}
