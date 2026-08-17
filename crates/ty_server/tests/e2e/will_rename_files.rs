use crate::TestServerBuilder;
use insta::assert_json_snapshot;
use lsp_types::{RenameFilesParams, WillRenameFilesRequest};

/// Renaming a module's file, end to end: the client asks before it moves anything and gets back the
/// edit that keeps the import naming a module that exists.
#[test]
fn renaming_a_module_rewrites_the_imports() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("alpha/__init__.py", "")?
        .with_file("alpha/util.py", "def thing(): ...\n")?
        .with_file("main.py", "from alpha.util import thing\n\nthing()\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    let edits = server.send_request_await::<WillRenameFilesRequest>(RenameFilesParams {
        files: vec![lsp_types::FileRename {
            old_uri: server.file_uri("alpha/util.py"),
            new_uri: server.file_uri("alpha/helpers.py"),
        }],
    });

    assert_json_snapshot!(edits);

    Ok(())
}

/// A file no import can be naming: the answer is nothing at all, so the editor carries on with the
/// move rather than showing a preview of an empty change.
#[test]
fn renaming_something_that_is_not_a_module_answers_nothing() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_file("main.py", "x = 1\n")?
        .with_file("notes.md", "hello\n")?
        .build()
        .wait_until_workspaces_are_initialized();

    let edits = server.send_request_await::<WillRenameFilesRequest>(RenameFilesParams {
        files: vec![lsp_types::FileRename {
            old_uri: server.file_uri("notes.md"),
            new_uri: server.file_uri("notes-old.md"),
        }],
    });

    assert!(edits.is_none(), "expected no edits, got {edits:?}");

    Ok(())
}
