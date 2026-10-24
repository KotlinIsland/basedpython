//! The server learning of changes to the file system: by watching it itself, as `by server`
//! does, or through the paths it asks the client to watch.
//!
//! The tests of the server's own watcher read real file system events from the operating system.
//! Like the command line's file-watching tests, they do not work inside a sandbox that withholds
//! those events.

use std::time::Duration;

use anyhow::Result;
use insta::{assert_json_snapshot, assert_snapshot};
use lsp_types::{
    BaseUri, DocumentDiagnosticReport, FileSystemWatcher, GlobPattern, InlayHintRefreshRequest,
    Message, RegistrationRequest, TextDocumentContentChangeEvent,
    TextDocumentContentChangeWholeDocument, WorkspaceDiagnosticRequest,
};
use ruff_db::system::SystemPath;
use ruff_python_trivia::textwrap::dedent;
use ty_project::UseUv;
use ty_server::{ClientOptions, DiagnosticMode};

use crate::diagnostic_snapshots::condensed_document_diagnostic_snapshot;
use crate::pull_diagnostics::{
    assert_workspace_diagnostics_suspends_for_long_polling, send_workspace_diagnostic_request,
};
use crate::{ServerMessageError, TestServer, TestServerBuilder};

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
        .with_watched_file_support(false)
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

    // the file watcher is registered with a request of its own
    assert!(
        matches!(
            server.try_await_request::<RegistrationRequest>(Some(SETTLE)),
            Err(ServerMessageError::Timeout)
        ),
        "the server asked the client to watch files it watches itself"
    );

    Ok(())
}

/// A server that does not watch for itself asks the client to.
#[test]
fn the_client_is_asked_to_watch_when_the_server_does_not() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_watched_file_support(false)
        .enable_diagnostic_dynamic_registration(true)
        .build()
        .wait_until_workspaces_are_initialized();

    let (_, params) = server.await_request::<RegistrationRequest>();
    let methods: Vec<_> = params
        .registrations
        .iter()
        .map(|registration| registration.method.as_str())
        .collect();
    assert_eq!(methods, ["textDocument/diagnostic"]);

    // the file watcher is registered with a request of its own
    watcher_registrations(&mut server)?;

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

#[test]
fn refreshes_script_dependency_after_rewatch() -> Result<()> {
    let script = SystemPath::new("src/script.py");
    let dependency = SystemPath::new("dependencies/dependency.py");
    let source = dedent(
        r#"
        # /// script
        # [tool.ty.environment]
        # extra-paths = ["../dependencies"]
        # ///
        from dependency import value
        result: int = value
        "#,
    );

    let mut server = TestServerBuilder::new()?
        .with_workspace(
            SystemPath::new("src"),
            Some(ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace)),
        )?
        .with_file(script, &source)?
        .with_file(dependency, "value = 1")?
        .with_watched_file_support(true)
        .enable_workspace_diagnostic_refresh(true)
        .enable_inlay_hints(true)
        .enable_inlay_hint_refresh(true)
        .build()
        .wait_until_workspaces_are_initialized();

    // The initial registration covers both the project and the script's external search path.
    let (initial_id, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **",
      "file://<temp_dir>/dependencies :: **"
    ]
    "#);

    server.open_text_document(script, &source, 1);
    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(script, None)),
        @""
    );

    // Removing the extra path replaces the registration and leaves the dependency unwatched.
    let without_dependency =
        source.replace(r#"extra-paths = ["../dependencies"]"#, "extra-paths = []");
    server.change_text_document(
        script,
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: without_dependency,
                },
            ),
        ],
        2,
    );
    let (project_id, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **"
    ]
    "#);
    assert_eq!(server.acknowledge_unregistration()?, initial_id);

    // No file event reaches the server for this edit while the dependency is unwatched.
    server.write_file(dependency, "value = 'wrong'")?;
    server.change_text_document(
        script,
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: source.into(),
                },
            ),
        ],
        3,
    );
    let (request_id, _, watches) = server.watcher_registration_request()?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **",
      "file://<temp_dir>/dependencies :: **"
    ]
    "#);
    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(script, None)),
        @""
    );

    // Completing registration refreshes the known dependency and finds its missed edit.
    server.acknowledge_request(request_id);
    assert_eq!(server.acknowledge_unregistration()?, project_id);
    server.await_diagnostic_refresh();
    let (refresh_id, ()) = server.await_request::<InlayHintRefreshRequest>();
    server.acknowledge_request(refresh_id);
    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(script, None)),
        @r#"6:14..6:19[ERROR]: Object of type `Literal["wrong"]` is not assignable to `int`"#
    );
    Ok(())
}

#[test]
fn picks_up_edits_to_unwatched_pth_files() -> Result<()> {
    let main = SystemPath::new("src/main.py");
    let config = SystemPath::new("src/pyproject.toml");
    let source = dedent(
        r"
        from dependency import value
        result: int = value
        ",
    )
    .trim_start()
    .to_owned();
    let project_config = dedent(
        r#"
        [tool.ty.environment]
        python = "../venv"
        "#,
    )
    .trim_start()
    .to_owned();

    let (base_python, pth) = if cfg!(windows) {
        (
            "base/bin/python.exe",
            "venv/Lib/site-packages/dependency.pth",
        )
    } else {
        (
            "base/bin/python",
            "venv/lib/python3.12/site-packages/dependency.pth",
        )
    };

    let builder = TestServerBuilder::new()?.with_workspace(
        SystemPath::new("src"),
        Some(ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace)),
    )?;
    let python_home = builder.file_path("base/bin");
    let old_root = builder.file_path("old");

    let mut server = builder
        .with_file(base_python, "")?
        .with_file(
            "venv/pyvenv.cfg",
            dedent(&format!(
                r"
                home = {python_home}
                version = 3.12
                "
            ))
            .trim_start(),
        )?
        .with_file(pth, old_root.as_str())?
        .with_file("old/dependency.py", "value = 1")?
        .with_file("new/dependency.py", "value = 'wrong'")?
        .with_file(main, &source)?
        .with_file(config, &project_config)?
        .with_watched_file_support(true)
        .build()
        .wait_until_workspaces_are_initialized();

    let (_, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **",
      "file://<temp_dir>/venv :: **",
      "file://<temp_dir>/venv/<site-packages> :: **",
      "file://<temp_dir>/old :: **"
    ]
    "#);
    server.open_text_document(main, &source, 1);
    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(main, None)),
        @""
    );

    // Switch to the default environment so the client stops watching site-packages.
    server.write_file(config, "[tool.ty]")?;
    server.did_change_watched_files(vec![lsp_types::FileEvent {
        uri: server.file_uri(config),
        kind: lsp_types::FileChangeType::Changed,
    }]);
    let (project_only_id, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **"
    ]
    "#);
    server.acknowledge_unregistration()?;

    // The client doesn't report the `.pth` edit while site-packages is unwatched.
    let new = server.file_path("new");
    server.write_file(pth, new.as_str())?;

    // Restoring the environment initially registers paths from the cached `.pth` contents.
    server.write_file(config, project_config)?;
    server.did_change_watched_files(vec![lsp_types::FileEvent {
        uri: server.file_uri(config),
        kind: lsp_types::FileChangeType::Changed,
    }]);
    let (request_id, stale_pth_id, watches) = server.watcher_registration_request()?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **",
      "file://<temp_dir>/venv :: **",
      "file://<temp_dir>/venv/<site-packages> :: **",
      "file://<temp_dir>/old :: **"
    ]
    "#);
    server.acknowledge_request(request_id);
    assert_eq!(server.acknowledge_unregistration()?, project_only_id);

    // Completing registration refreshes `.pth` and registers its new search path.
    let (_, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **",
      "file://<temp_dir>/venv :: **",
      "file://<temp_dir>/venv/<site-packages> :: **",
      "file://<temp_dir>/new :: **"
    ]
    "#);
    assert_eq!(server.acknowledge_unregistration()?, stale_pth_id);

    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(main, None)),
        @r#"1:14..1:19[ERROR]: Object of type `Literal["wrong"]` is not assignable to `int`"#
    );
    Ok(())
}

#[test]
fn nested_search_path_picks_up_missed_dependency_edit() -> Result<()> {
    let main = SystemPath::new("src/main.py");
    let config = SystemPath::new("src/pyproject.toml");
    let dependency = SystemPath::new("dependencies/pkg/dependency.py");
    let source = dedent(
        r"
        from pkg.dependency import value
        result: int = value
        ",
    )
    .trim_start()
    .to_owned();

    let mut server = TestServerBuilder::new()?
        .with_workspace(
            SystemPath::new("src"),
            Some(ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace)),
        )?
        .with_file(main, &source)?
        .with_file(
            config,
            dedent(
                r#"
                [tool.ty.environment]
                extra-paths = ["../dependencies"]
                "#,
            )
            .trim_start(),
        )?
        .with_file(dependency, "value = 1")?
        .with_watched_file_support(true)
        .build()
        .wait_until_workspaces_are_initialized();

    let (_, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **",
      "file://<temp_dir>/dependencies :: **"
    ]
    "#);
    server.open_text_document(main, &source, 1);
    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(main, None)),
        @""
    );

    // Remove the parent search path before changing the dependency on disk.
    server.write_file(config, "[tool.ty]")?;
    server.did_change_watched_files(vec![lsp_types::FileEvent {
        uri: server.file_uri(config),
        kind: lsp_types::FileChangeType::Changed,
    }]);
    let (_, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **"
    ]
    "#);
    server.acknowledge_unregistration()?;

    // No file event reports the edit while the parent search path is unwatched.
    server.write_file(dependency, "value = 'wrong'")?;
    // Register the nested search path, which contains the previously read dependency.
    server.write_file(
        config,
        dedent(
            r#"
            [tool.ty.environment]
            extra-paths = ["../dependencies/pkg"]
            "#,
        )
        .trim_start(),
    )?;
    server.did_change_watched_files(vec![lsp_types::FileEvent {
        uri: server.file_uri(config),
        kind: lsp_types::FileChangeType::Changed,
    }]);
    let (_, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "file://<temp_dir>/src :: **",
      "file://<temp_dir>/dependencies/pkg :: **"
    ]
    "#);
    server.acknowledge_unregistration()?;

    // The new search root exposes the same file as `dependency` instead of `pkg.dependency`.
    // Reimport it to check that the edit missed while it was unwatched has been refreshed.
    server.change_text_document(
        main,
        vec![
            TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument {
                    text: dedent(
                        r"
                        from dependency import value
                        result: int = value
                        ",
                    )
                    .trim_start()
                    .to_owned(),
                },
            ),
        ],
        2,
    );
    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(main, None)),
        @r#"1:14..1:19[ERROR]: Object of type `Literal["wrong"]` is not assignable to `int`"#
    );
    Ok(())
}

#[test]
fn watches_project_without_relative_patterns() -> Result<()> {
    let main = SystemPath::new("src/main.py");
    let dependency = SystemPath::new("src/dependency.py");
    let source = dedent(
        r"
        from dependency import value
        result: int = value
        ",
    )
    .trim_start()
    .to_owned();

    let mut server = TestServerBuilder::new()?
        .with_workspace(
            SystemPath::new("src"),
            Some(ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace)),
        )?
        .with_file(main, &source)?
        .with_file(dependency, "value = 1")?
        .with_watched_file_support(false)
        .build()
        .wait_until_workspaces_are_initialized();

    // A client without relative patterns receives one workspace-wide glob.
    let (_, watches) = watcher_registrations(&mut server)?;
    assert_json_snapshot!(watcher_snapshot(&watches), @r#"
    [
      "**"
    ]
    "#);

    server.open_text_document(main, &source, 1);
    server.write_file(dependency, "value = 'wrong'")?;
    server.did_change_watched_files(vec![lsp_types::FileEvent {
        uri: server.file_uri(dependency),
        kind: lsp_types::FileChangeType::Changed,
    }]);
    assert_snapshot!(
        condensed_document_diagnostic_snapshot(server.document_diagnostic_request(main, None)),
        @r#"1:14..1:19[ERROR]: Object of type `Literal["wrong"]` is not assignable to `int`"#
    );
    Ok(())
}

fn watcher_registrations(server: &mut TestServer) -> Result<(String, Vec<FileSystemWatcher>)> {
    let (request_id, registration_id, watchers) = server.watcher_registration_request()?;
    server.acknowledge_request(request_id);
    Ok((registration_id, watchers))
}

fn watcher_snapshot(watchers: &[FileSystemWatcher]) -> Vec<String> {
    watchers
        .iter()
        .map(|watcher| {
            let pattern = match &watcher.glob_pattern {
                GlobPattern::Pattern(pattern) => pattern.clone(),
                GlobPattern::RelativePattern(relative) => match &relative.base_uri {
                    BaseUri::Uri(uri) => format!("{uri} :: {}", relative.pattern),
                    BaseUri::WorkspaceFolder(folder) => {
                        format!("workspace {} :: {}", folder.uri, relative.pattern)
                    }
                },
            };
            // The fixture's virtual environment has a different site-packages layout on Windows.
            let pattern = pattern
                .replace("/venv/Lib/site-packages", "/venv/<site-packages>")
                .replace(
                    "/venv/lib/python3.12/site-packages",
                    "/venv/<site-packages>",
                );
            if let Some(kind) = watcher.kind {
                format!("{pattern} [{kind:?}]")
            } else {
                pattern
            }
        })
        .collect()
}
