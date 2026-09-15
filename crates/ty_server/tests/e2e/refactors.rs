use crate::{AwaitResponseError, TestServer, TestServerBuilder};
use anyhow::Result;
use lsp_types::{
    CodeAction, CodeActionContext, CodeActionKind, CodeActionParams, CodeActionRequest,
    CodeActionResolveRequest, CodeActionResponse, CodeActionTriggerKind, Position, Range,
    TextDocumentContentChangeEvent, TextDocumentContentChangeWholeDocument, TextDocumentIdentifier,
};
use ruff_db::system::SystemPath;

const SOURCE: &str = "\
def f():
    x = compute()
    return x

def compute() -> int:
    return 1
";

fn caret(line: u32, character: u32) -> Range {
    Range::new(
        Position::new(line, character),
        Position::new(line, character),
    )
}

fn request(
    server: &mut TestServer,
    file: &SystemPath,
    range: Range,
    only: Option<Vec<CodeActionKind>>,
    trigger_kind: Option<CodeActionTriggerKind>,
) -> Vec<CodeAction> {
    let params = CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: server.file_uri(file),
        },
        range,
        context: CodeActionContext {
            diagnostics: Vec::new(),
            only,
            trigger_kind,
        },
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        partial_result_params: lsp_types::PartialResultParams::default(),
    };
    server
        .send_request_await::<CodeActionRequest>(params)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|action| match action {
            CodeActionResponse::CodeAction(action) => Some(action),
            CodeActionResponse::Command(_) => None,
        })
        .collect()
}

fn server(resolve: bool) -> Result<(TestServer, &'static SystemPath)> {
    let workspace_root = SystemPath::new("src");
    let file = SystemPath::new("src/main.py");
    let mut builder = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(file, SOURCE)?;
    if resolve {
        builder = builder.enable_code_action_edit_resolve();
    }
    let mut server = builder.build().wait_until_workspaces_are_initialized();
    server.open_text_document(file, SOURCE, 1);
    Ok((server, file))
}

/// A client that resolves edits is offered the refactoring without one, and gets
/// the edit when it resolves the action.
#[test]
fn refactoring_edit_is_computed_on_resolve() -> Result<()> {
    let (mut server, file) = server(true)?;

    let actions = request(&mut server, file, caret(1, 4), None, None);
    let [action] = actions.as_slice() else {
        panic!("expected exactly the inline refactoring, got {actions:#?}");
    };
    assert_eq!(
        action.kind,
        Some(CodeActionKind::new("refactor.inline.variable"))
    );
    assert_eq!(action.edit, None);
    assert!(action.data.is_some());

    let resolved = server.send_request_await::<CodeActionResolveRequest>(action.clone());
    insta::assert_json_snapshot!(resolved);

    Ok(())
}

/// A client that cannot resolve edits gets the edit with the action.
#[test]
fn refactoring_edit_is_sent_to_a_client_that_cannot_resolve() -> Result<()> {
    let (mut server, file) = server(false)?;

    let actions = request(&mut server, file, caret(2, 11), None, None);
    let [action] = actions.as_slice() else {
        panic!("expected exactly the inline refactoring, got {actions:#?}");
    };
    assert_eq!(action.data, None);
    insta::assert_json_snapshot!(action.edit);

    Ok(())
}

/// A refactoring that cannot be applied is shown, with its reason, only to a
/// user who explicitly asked for refactorings.
#[test]
fn refused_refactoring_is_disabled_only_when_invoked() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let file = SystemPath::new("src/main.py");
    let source = "\
def f(flag):
    if flag:
        x = 1
    return x
";
    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(file, source)?
        .enable_code_action_edit_resolve()
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(file, source, 1);

    let automatic = request(
        &mut server,
        file,
        caret(3, 11),
        None,
        Some(CodeActionTriggerKind::Automatic),
    );
    assert_eq!(automatic, Vec::new());

    let invoked = request(
        &mut server,
        file,
        caret(3, 11),
        Some(vec![CodeActionKind::RefactorInline]),
        Some(CodeActionTriggerKind::Invoked),
    );
    insta::assert_json_snapshot!(invoked);

    Ok(())
}

/// Asking only for another kind of action leaves the refactorings out.
#[test]
fn refactoring_is_left_out_when_another_kind_is_requested() -> Result<()> {
    let (mut server, file) = server(true)?;

    let actions = request(
        &mut server,
        file,
        caret(1, 4),
        Some(vec![CodeActionKind::QuickFix]),
        None,
    );
    assert_eq!(actions, Vec::new());

    Ok(())
}

/// An action offered against an older version of the document is not resolved
/// against the newer one.
#[test]
fn resolve_after_an_edit_is_content_modified() -> Result<()> {
    let (mut server, file) = server(true)?;

    let actions = request(&mut server, file, caret(1, 4), None, None);
    let [action] = actions.as_slice() else {
        panic!("expected exactly the inline refactoring, got {actions:#?}");
    };

    server.change_text_document(
        file,
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: format!("\n{SOURCE}"),
                },
            ),
        ],
        2,
    );

    let id = server.send_request::<CodeActionResolveRequest>(action.clone());
    match server.try_await_response::<CodeActionResolveRequest>(&id, None) {
        Err(AwaitResponseError::RequestFailed(error)) => {
            assert_eq!(error.code, lsp_server::ErrorCode::ContentModified as i32);
        }
        other => panic!("expected the resolve to fail as content modified, got {other:?}"),
    }

    Ok(())
}
