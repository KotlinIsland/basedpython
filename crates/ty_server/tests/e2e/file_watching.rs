//! The server watching the file system itself, as `by server` does.
//!
//! These tests read real file system events from the operating system. Like the command line's
//! file-watching tests, they do not work inside a sandbox that withholds those events.

use std::time::Duration;

use anyhow::Result;
use lsp_types::{
    DocumentDiagnosticReport, Message, RegistrationRequest, WorkspaceDiagnosticRequest,
};
use ruff_db::system::SystemPath;
use ty_project::UseUv;
use ty_server::{ClientOptions, DiagnosticMode};

use crate::pull_diagnostics::{
    assert_workspace_diagnostics_suspends_for_long_polling, send_workspace_diagnostic_request,
};
use crate::{TestServer, TestServerBuilder};

const LIB_INT: &str = "def make() -> int:\n    return 1\n";
const LIB_STR: &str = "def make() -> str:\n    return ''\n";
const USER: &str = "from lib import make\n\nx: bytes = make()\n";
const IMPORTS_OTHER: &str = "from other import make\n\nx: bytes = make()\n";
const IMPORTS_DEP: &str = "from dep import make\n\nx: bytes = make()\n";

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

/// how long a change is given to reach the server and be answered before it is taken to have
/// sent nothing: the watcher settles a batch 10 ms after its last event
const SETTLE: Duration = Duration::from_secs(1);

/// asserts that the server asks for no refresh within [`SETTLE`]
#[track_caller]
fn assert_no_refresh(server: &mut TestServer, after: &str) {
    assert!(
        !server.try_await_diagnostic_refresh(SETTLE),
        "the server asked the client to refresh after {after}"
    );
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

/// a refresh makes the client pull the diagnostics and inlay hints of everything it shows again,
/// so a write that no answer depends on is not one: not a build writing its output a file at a
/// time, nor a write into an excluded or an ignored directory. a write the project reads after
/// them is
#[test]
fn writes_the_project_does_not_read_are_not_a_refresh() -> Result<()> {
    let lib = SystemPath::new("project/src/lib.py");
    let user = SystemPath::new("project/src/user.py");

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/basedpython.toml", "[src]\nexclude = [\"runs\"]\n")?
        .with_file("project/.gitignore", "__pycache__/\n")?
        .with_file(lib, LIB_INT)?
        .with_file(user, USER)?
        // the output of an earlier build, which a rebuild writes over
        .with_file("project/build/.by-manifest", "src/lib.py\nsrc/user.py\n")?
        .with_file("project/build/lib.py", LIB_INT)?
        .with_file("project/build/user.py", USER)?
        .with_file("project/runs/0.trace", "")?
        .with_file("project/src/__pycache__/lib.cpython-314.pyc", "")?
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
    assert_no_refresh(&mut server, "starting");

    // far enough apart for each round to be a batch of its own, as a build's writes are
    for round in 0..8 {
        server.write_file("project/build/lib.py", LIB_STR)?;
        server.write_file(format!("project/build/pkg/module_{round}.py"), LIB_STR)?;
        server.write_file(format!("project/runs/{round}.trace"), "")?;
        server.write_file(
            format!("project/src/__pycache__/user.{round}.cpython-314.pyc"),
            "",
        )?;
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_no_refresh(
        &mut server,
        "writes to a build's output, an excluded directory and an ignored one",
    );

    server.write_file(lib, LIB_STR)?;
    diagnostics_after_change(&mut server, user, |messages| {
        messages.iter().any(|message| message.contains("`str`"))
    });

    Ok(())
}

/// a file appearing and going away changes what an import resolves to, and each is a refresh,
/// though no file the server had read was written
#[test]
fn a_new_file_and_a_deleted_one_are_each_a_refresh() -> Result<()> {
    let user = SystemPath::new("project/src/user.py");
    let other = SystemPath::new("project/src/other.py");
    let unresolved = |messages: &[String]| {
        messages
            .iter()
            .any(|message| message.contains("Cannot resolve imported module `other`"))
    };

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(user, IMPORTS_OTHER)?
        .enable_pull_diagnostics(true)
        .enable_workspace_diagnostic_refresh(true)
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(user, IMPORTS_OTHER, 1);
    let before = diagnostic_messages(&mut server, user);
    assert!(unresolved(&before), "{before:#?}");

    server.write_file(other, LIB_INT)?;
    diagnostics_after_change(&mut server, user, |messages| !unresolved(messages));

    std::fs::remove_file(server.file_path(other).as_std_path())?;
    diagnostics_after_change(&mut server, user, unresolved);

    Ok(())
}

/// a dependency is read from outside the project, in the environment's `site-packages`, and a
/// change to it is a refresh like a change to the project's own file
#[cfg(unix)]
#[test]
fn a_change_in_site_packages_is_a_refresh() -> Result<()> {
    let user = SystemPath::new("project/user.py");
    let dependency = SystemPath::new("venv/lib/python3.14/site-packages/dep.py");

    let builder = TestServerBuilder::new()?;
    let home = builder.file_path("base/bin");
    let mut server = builder
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(
            "project/basedpython.toml",
            "[environment]\npython = \"../venv\"\n",
        )?
        .with_file(user, IMPORTS_DEP)?
        .with_file("base/bin/python", "")?
        .with_file("venv/bin/python", "")?
        .with_file(
            "venv/pyvenv.cfg",
            format!("home = {home}\nversion_info = 3.14.0\n"),
        )?
        .with_file(dependency, LIB_INT)?
        .enable_pull_diagnostics(true)
        .enable_workspace_diagnostic_refresh(true)
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(user, IMPORTS_DEP, 1);
    let before = diagnostic_messages(&mut server, user);
    assert!(
        before.iter().any(|message| message.contains("`int`")),
        "{before:#?}"
    );

    server.write_file(dependency, LIB_STR)?;
    diagnostics_after_change(&mut server, user, |messages| {
        messages.iter().any(|message| message.contains("`str`"))
    });

    Ok(())
}

/// a configuration file changes every answer without changing a source file
#[test]
fn a_configuration_change_is_a_refresh() -> Result<()> {
    let lib = SystemPath::new("project/lib.py");
    let user = SystemPath::new("project/user.py");
    let configuration = SystemPath::new("project/basedpython.toml");

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(configuration, "")?
        .with_file(lib, LIB_INT)?
        .with_file(user, USER)?
        .with_use_uv(UseUv::Off)
        .enable_pull_diagnostics(true)
        .enable_workspace_diagnostic_refresh(true)
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(user, USER, 1);
    let before = diagnostic_messages(&mut server, user);
    assert!(!before.is_empty(), "{before:#?}");

    server.write_file(configuration, "[rules]\ninvalid-assignment = \"ignore\"\n")?;
    diagnostics_after_change(&mut server, user, <[String]>::is_empty);

    Ok(())
}

/// a workspace diagnostic request held open until the workspace's diagnostics change is answered
/// when the server's own watcher sees the change, with nothing from the client
#[test]
fn a_request_held_open_is_answered_after_a_change_the_server_saw() -> Result<()> {
    let broken = SystemPath::new("project/broken.py");

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file("project/clean.py", LIB_INT)?
        .with_initialization_options(
            &ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace),
        )
        .watch_file_system()
        .build()
        .wait_until_workspaces_are_initialized();

    // checking the one file first leaves the workspace check with nothing slow to do, so the long
    // poll is held open well within the time `assert_workspace_diagnostics_suspends_for_long_polling`
    // gives it, rather than still being checked when the file is written
    let clean = SystemPath::new("project/clean.py");
    server.open_text_document(clean, LIB_INT, 1);
    assert_eq!(
        diagnostic_messages(&mut server, clean),
        Vec::<String>::new()
    );

    let long_poll = send_workspace_diagnostic_request(&mut server);
    assert_workspace_diagnostics_suspends_for_long_polling(&mut server, &long_poll);

    server.write_file(broken, "x: int = ''\n")?;
    let report = server
        .try_await_response::<WorkspaceDiagnosticRequest>(&long_poll, Some(Duration::from_secs(10)))
        .unwrap_or_else(|error| panic!("the request held open was not answered: {error}"));
    assert_eq!(report.items.len(), 1, "{report:#?}");

    Ok(())
}
