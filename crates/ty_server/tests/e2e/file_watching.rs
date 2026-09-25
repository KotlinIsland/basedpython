//! The server watching the file system itself, as `by server` does.
//!
//! These tests read real file system events from the operating system. Like the command line's
//! file-watching tests, they do not work inside a sandbox that withholds those events.

use anyhow::Result;
use lsp_types::{DocumentDiagnosticReport, Message, RegistrationRequest};
use ruff_db::system::SystemPath;

use crate::{TestServer, TestServerBuilder};

const LIB_INT: &str = "def make() -> int:\n    return 1\n";
const LIB_STR: &str = "def make() -> str:\n    return ''\n";
const USER: &str = "from lib import make\n\nx: bytes = make()\n";

/// The messages of the diagnostics the server has for `path` now.
fn diagnostic_messages(server: &mut TestServer, path: &SystemPath) -> Vec<String> {
    match server.document_diagnostic_request(path, None) {
        DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) => report
            .full_document_diagnostic_report
            .items
            .into_iter()
            .map(|diagnostic| {
                let Message::String(message) = diagnostic.message else {
                    panic!(
                        "a diagnostic message should be a string, and was {:#?}",
                        diagnostic.message
                    )
                };
                message
            })
            .collect(),
        DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(_) => {
            panic!("asked without a previous result id")
        }
    }
}

/// Waits for the server to say something changed, then asks again, until `user`'s diagnostics
/// satisfy `done`. Nothing but the file system tells the server about the change.
fn diagnostics_after_change(
    server: &mut TestServer,
    user: &SystemPath,
    done: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    loop {
        server.await_diagnostic_refresh();
        let messages = diagnostic_messages(server, user);
        if done(&messages) {
            return messages;
        }
    }
}

/// A file that is not open, rewritten on disk, reaches the diagnostics of an open file that
/// depends on it, with no `workspace/didChangeWatchedFiles` from the client. The server hears of
/// the write from the operating system, which reports it only once it has happened.
#[test]
fn a_write_reaches_an_open_dependent_without_the_client() -> Result<()> {
    let lib = SystemPath::new("src/lib.py");
    let user = SystemPath::new("src/user.py");

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(lib, LIB_INT)?
        .with_file(user, USER)?
        .enable_pull_diagnostics(true)
        .enable_workspace_diagnostic_refresh(true)
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(user, USER, 1);
    let before = diagnostic_messages(&mut server, user);
    assert!(
        before.iter().any(|message| message.contains("`int`")),
        "{before:#?}"
    );

    server.write_file(lib, LIB_STR)?;

    let after = diagnostics_after_change(&mut server, user, |messages| {
        messages.iter().any(|message| message.contains("`str`"))
    });
    assert!(
        after.iter().all(|message| !message.contains("`int`")),
        "{after:#?}"
    );

    Ok(())
}

/// The same through a workspace folder the client names by a symbolic link. The operating system
/// reports a change at the path it resolved to; the server knows the file by the path it was
/// given.
#[cfg(unix)]
#[test]
fn a_write_reaches_an_open_dependent_through_a_linked_workspace() -> Result<()> {
    let lib = SystemPath::new("linked/lib.py");
    let user = SystemPath::new("linked/user.py");

    let builder = TestServerBuilder::new()?
        .with_file(SystemPath::new("real/lib.py"), LIB_INT)?
        .with_file(SystemPath::new("real/user.py"), USER)?;
    std::os::unix::fs::symlink(
        builder.file_path("real").as_std_path(),
        builder.file_path("linked").as_std_path(),
    )?;
    let mut server = builder
        .with_workspace(SystemPath::new("linked"), None)?
        .enable_pull_diagnostics(true)
        .enable_workspace_diagnostic_refresh(true)
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(user, USER, 1);
    let before = diagnostic_messages(&mut server, user);
    assert!(
        before.iter().any(|message| message.contains("`int`")),
        "{before:#?}"
    );

    server.write_file(lib, LIB_STR)?;

    diagnostics_after_change(&mut server, user, |messages| {
        messages.iter().any(|message| message.contains("`str`"))
    });

    Ok(())
}

/// A client that offers to watch files is not asked to while the server watches them itself.
/// Its reports would be a second pass over every change, and a client can report a change before
/// the change is on disk.
#[test]
fn the_client_is_not_asked_to_watch_what_the_server_watches() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .enable_did_change_watched_files(true)
        .enable_diagnostic_dynamic_registration(true)
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    let (_, params) = server.await_request::<RegistrationRequest>();
    let methods: Vec<_> = params
        .registrations
        .iter()
        .map(|registration| registration.method.as_str())
        .collect();
    assert_eq!(methods, ["textDocument/diagnostic"]);

    Ok(())
}

/// A server that does not watch for itself asks the client to.
#[test]
fn the_client_is_asked_to_watch_when_the_server_does_not() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .enable_did_change_watched_files(true)
        .enable_diagnostic_dynamic_registration(true)
        .build()
        .wait_until_workspaces_are_initialized();

    let (_, params) = server.await_request::<RegistrationRequest>();
    let methods: Vec<_> = params
        .registrations
        .iter()
        .map(|registration| registration.method.as_str())
        .collect();
    assert_eq!(
        methods,
        ["textDocument/diagnostic", "workspace/didChangeWatchedFiles"]
    );

    Ok(())
}
