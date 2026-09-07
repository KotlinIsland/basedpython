//! The side channel a `by` command line reaches a running server on.
//!
//! Exercised through a real server, because everything worth testing about it is about the
//! session: which database answers, what the editor's open buffers do to the answer, and
//! whether the configuration and environment the caller resolved are the ones the server did.
//! None of that exists in the library the handler calls.
//!
//! Requests are built out of a real cold [`ProjectDatabase`], the way the `by` command line
//! builds them, rather than by hand. That is the whole point of [`answers_what_a_cold_check
//!_would_have`]: a request assembled to suit the server would agree with the server about
//! anything, including the things it gets wrong.
//!
//! The server under test publishes itself into a temporary directory rather than the user's
//! own, so that a `by check` running elsewhere on this machine never finds it.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ruff_db::diagnostic::{DisplayDiagnosticConfig, DisplayDiagnostics};
use ruff_db::system::{OsSystem, SystemPath, TestSystem};
use ruff_ranged_value::{ValueSource, ValueSourceGuard};
use tempfile::TempDir;
use ty_project::{Db as _, ProjectDatabase, ProjectMetadata};
use ty_server::project_server::protocol;
use ty_server::project_server::protocol::{
    Answer, Build, CheckRequest, PROTOCOL, Payload, Refusal, Request, Response,
};
use ty_server::project_server::{discovery, environment};
use ty_server::{ClientOptions, DiagnosticMode};

use crate::{TestServer, TestServerBuilder};

/// A file whose one error is easy to recognise in rendered output.
const MAIN: &str = "def f() -> str:\n    return 42\n";

/// How long a test will wait for an answer.
///
/// Bounded so that a change which stops the server answering — a dropped request, a wedged
/// main loop — fails the suite rather than hanging it.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Where a test server publishes itself: never the directory a real `by check` reads.
fn published(directory: &TempDir) -> &SystemPath {
    SystemPath::from_std_path(directory.path()).expect("a temporary directory to be utf-8")
}

/// A server holding `src`, reachable by a command line, checking the whole project.
///
/// Workspace diagnostics because that is the precondition: a server diagnosing only what is
/// open has not checked the project and refuses outright — see
/// [`refuses_while_checking_only_open_files`].
fn server(directory: &TempDir, files: &[(&SystemPath, &str)]) -> Result<TestServer> {
    let mut builder = TestServerBuilder::new()?
        .with_project_server(published(directory))
        .with_workspace(
            SystemPath::new("src"),
            Some(ClientOptions::default().with_diagnostic_mode(DiagnosticMode::Workspace)),
        )?;
    for (path, content) in files {
        builder = builder.with_file(path, content)?;
    }
    Ok(builder.build().wait_until_workspaces_are_initialized())
}

/// The database a `by check` in `project_root` would build for itself.
fn cold(project_root: &SystemPath) -> Result<ProjectDatabase> {
    cold_with(project_root, None)
}

/// The same, for a `by check` that was given flags.
///
/// The flags arrive as a layer over the configuration file, the way `--python-version` and
/// the rest do, rather than by editing the file: the layering is exactly what this has to
/// exercise.
fn cold_with(project_root: &SystemPath, overrides: Option<&str>) -> Result<ProjectDatabase> {
    // the same environment the server under test was given. an interpreter discovered out of
    // one process's environment and not the other's is a real disagreement, and one this
    // refuses over — see [`refuses_an_environment_it_does_not_share`] — so a test that wants
    // an answer has to stand where the server stands
    let system = TestSystem::new(OsSystem::new(project_root));
    for name in TestServerBuilder::CLEARED_ENV_VARS {
        system.remove_env_var(*name);
    }

    let mut metadata = ProjectMetadata::discover(project_root, &system)?;
    metadata.apply_configuration_files(&system)?;
    if let Some(overrides) = overrides {
        // the same source a flag's value carries, which is what decides whether a range is
        // expected alongside it
        let _guard = ValueSourceGuard::new(ValueSource::Cli, false);
        metadata.apply_override_options(serde_json::from_str(overrides)?);
    }

    let mut db = ProjectDatabase::fallible(metadata, system)?;
    db.set_checker(Arc::new(ty_ide::DjangoChecker));
    Ok(db)
}

/// The request that command line would send, resolved from its own database.
fn request_for(db: &ProjectDatabase) -> Result<CheckRequest> {
    let project = db.project();
    Ok(CheckRequest {
        project_root: project.root(db).to_path_buf(),
        working_directory: project.root(db).to_path_buf(),
        options: protocol::configuration(project.metadata(db).to_merged_options().options())?,
        environment: environment(db),
        force_exclude: project.force_exclude(db),
        verbose: project.verbose(db),
        color: false,
    })
}

/// One round trip on the side channel, bypassing the client so that a test can send something
/// a client never would.
fn ask(
    directory: &TempDir,
    project_root: &SystemPath,
    request: impl FnOnce(&discovery::Record) -> Result<Request>,
) -> Result<Option<Response>> {
    let candidates = discovery::candidates(published(directory), project_root);
    let Some(record) = candidates.first().map(|candidate| &candidate.record) else {
        anyhow::bail!("the server published no record covering `{project_root}`");
    };

    let mut connection = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, record.port)))?;
    connection.set_read_timeout(Some(TIMEOUT))?;
    connection.set_write_timeout(Some(TIMEOUT))?;

    let mut line = serde_json::to_vec(&request(record)?)?;
    line.push(b'\n');
    connection.write_all(&line)?;
    connection.flush()?;

    let mut response = String::new();
    BufReader::new(&connection).read_line(&mut response)?;
    if response.is_empty() {
        return Ok(None);
    }

    let answer: Answer = serde_json::from_str(&response)?;
    // the caller has no way to know it is talking to the server unless the server proves it
    // read the record, so every test asserts it along the way
    assert_eq!(answer.token, record.token);
    Ok(Some(answer.response))
}

/// A well-formed request, which the tests below then spoil one field at a time.
fn check_request(record: &discovery::Record, db: &ProjectDatabase) -> Result<Request> {
    Ok(Request {
        protocol: PROTOCOL,
        token: record.token.clone(),
        client: record.build.clone(),
        payload: serde_json::to_value(Payload::Check(request_for(db)?))?,
    })
}

/// Asks the usual way, from a database built the way the command line builds one.
fn check(directory: &TempDir, db: &ProjectDatabase) -> Result<Option<Response>> {
    ask(directory, db.project().root(db), |record| {
        check_request(record, db)
    })
}

/// The claim the whole design rests on: what the server answers is what this process would
/// have produced on its own, character for character.
#[test]
fn answers_what_a_cold_check_would_have() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(
        &directory,
        &[
            (SystemPath::new("src/main.py"), MAIN),
            (
                SystemPath::new("src/other.py"),
                "import collections\n\nx: int = collections\n",
            ),
        ],
    )?;

    let db = cold(&server.file_path(SystemPath::new("src")))?;
    let Some(Response::Check(response)) = check(&directory, &db)? else {
        panic!("the server did not answer");
    };

    let diagnostics = db.check();
    let config = DisplayDiagnosticConfig::new("ty")
        .format(db.project().settings(&db).terminal().output_format.into())
        .color(false)
        .context(0);
    let expected = DisplayDiagnostics::new(&db, &config, &diagnostics).to_string();

    assert_eq!(response.rendered, expected);
    assert_eq!(response.diagnostics, diagnostics.len());
    assert!(response.human_readable);
    assert!(!response.empty_project);

    Ok(())
}

/// A change made behind the server's back is a change the answer has to include. The editor's
/// watcher is what feeds this session, and nothing tells it about a file written by a `git
/// checkout`, a code generator, or this test.
#[test]
fn answers_from_the_file_system_rather_than_from_the_watcher() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let project_root = server.file_path(SystemPath::new("src"));

    // no `didChangeWatchedFiles`, because the test client is not watching anything
    std::fs::write(project_root.join("added.py").as_std_path(), "y: str = 1\n")?;

    let db = cold(&project_root)?;
    let Some(Response::Check(response)) = check(&directory, &db)? else {
        panic!("the server did not answer");
    };

    assert!(
        response.rendered.contains("added.py"),
        "the answer did not see a file written behind the server's back:\n{}",
        response.rendered
    );
    assert_eq!(response.diagnostics, db.check().len());

    Ok(())
}

/// A flag on the command line lands in the merged configuration, and moves what the check
/// reports. Comparing the configuration file alone would compare the one thing that could
/// never have differed.
#[test]
fn refuses_a_flag_that_changes_the_answer() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;

    let db = cold_with(
        &server.file_path(SystemPath::new("src")),
        Some(r#"{"rules": {"invalid-return-type": "ignore"}}"#),
    )?;

    let response = check(&directory, &db)?;
    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::Options { .. }
            })
        ),
        "expected a refusal over the configuration, got {response:?}"
    );

    Ok(())
}

/// Two processes that agree about every option can still be checking against different
/// site-packages: an environment is discovered as much as configured, and a server discovers
/// it from the editor's environment rather than the caller's shell.
#[test]
fn refuses_an_environment_it_does_not_share() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let db = cold(&server.file_path(SystemPath::new("src")))?;

    let response = ask(&directory, db.project().root(&db), |record| {
        let mut request = request_for(&db)?;
        request.environment.push_str(" (somewhere else)");
        Ok(Request {
            protocol: PROTOCOL,
            token: record.token.clone(),
            client: record.build.clone(),
            payload: serde_json::to_value(Payload::Check(request))?,
        })
    })?;

    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::Environment { .. }
            })
        ),
        "expected a refusal over the environment, got {response:?}"
    );

    Ok(())
}

/// A verbose check adds a note to every diagnostic saying where its rule was turned on, and a
/// server's database never does. Refused rather than declined to ask, so that the reason is
/// in the log where somebody looking for it can find it.
#[test]
fn refuses_a_verbose_check() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let db = cold(&server.file_path(SystemPath::new("src")))?;

    let response = ask(&directory, db.project().root(&db), |record| {
        let mut request = request_for(&db)?;
        request.verbose = true;
        Ok(Request {
            protocol: PROTOCOL,
            token: record.token.clone(),
            client: record.build.clone(),
            payload: serde_json::to_value(Payload::Check(request))?,
        })
    })?;

    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::Verbose
            })
        ),
        "expected a refusal over verbosity, got {response:?}"
    );

    Ok(())
}

/// The editor is holding text that is not in the file, so the server is checking a program
/// the caller cannot see.
#[test]
fn refuses_while_a_buffer_is_unsaved() -> Result<()> {
    let directory = TempDir::new()?;
    let main = SystemPath::new("src/main.py");
    let mut server = server(&directory, &[(main, MAIN)])?;

    server.open_text_document(main, "x: int = 1\n", 1);

    let db = cold(&server.file_path(SystemPath::new("src")))?;
    match check(&directory, &db)? {
        Some(Response::Refused {
            reason: Refusal::Unsaved { files },
        }) => assert_eq!(files, vec!["main.py".to_string()]),
        other => panic!("expected a refusal over the open buffer, got {other:?}"),
    }

    Ok(())
}

/// A notebook the editor is holding carries no text to compare against the file, because what
/// is on disk is a serialization whose formatting is the writer's choice. Refused, rather than
/// skipped for want of a way to compare it.
#[test]
fn refuses_while_a_notebook_is_open() -> Result<()> {
    let directory = TempDir::new()?;
    let notebook = SystemPath::new("src/notes.ipynb");
    let mut server = server(
        &directory,
        &[
            (SystemPath::new("src/main.py"), MAIN),
            (notebook, EMPTY_NOTEBOOK),
        ],
    )?;

    server.send_notification::<lsp_types::DidOpenNotebookDocumentNotification>(
        lsp_types::DidOpenNotebookDocumentParams {
            notebook_document: lsp_types::NotebookDocument {
                uri: server.file_uri(notebook),
                notebook_type: "jupyter-notebook".to_string(),
                version: 0,
                metadata: None,
                cells: Vec::new(),
            },
            cell_text_documents: Vec::new(),
        },
    );

    let db = cold(&server.file_path(SystemPath::new("src")))?;
    match check(&directory, &db)? {
        Some(Response::Refused {
            reason: Refusal::Unsaved { files },
        }) => assert_eq!(files, vec!["notes.ipynb".to_string()]),
        other => panic!("expected a refusal over the open notebook, got {other:?}"),
    }

    Ok(())
}

/// An open document that still matches its file is not a reason to refuse. Otherwise the
/// feature would be unavailable to anyone who had the project open, which is everyone it is
/// for.
#[test]
fn answers_while_a_buffer_is_open_and_saved() -> Result<()> {
    let directory = TempDir::new()?;
    let main = SystemPath::new("src/main.py");
    let mut server = server(&directory, &[(main, MAIN)])?;

    server.open_text_document(main, MAIN, 1);

    let db = cold(&server.file_path(SystemPath::new("src")))?;
    let response = check(&directory, &db)?;
    assert!(
        matches!(response, Some(Response::Check(_))),
        "an open, saved buffer should not stop the server answering, got {response:?}\nclient: {}",
        environment(&db)
    );

    Ok(())
}

/// The default editor setting diagnoses only what is open. A server in that mode has not
/// checked the project and must not answer as though it had.
#[test]
fn refuses_while_checking_only_open_files() -> Result<()> {
    let directory = TempDir::new()?;
    let server = TestServerBuilder::new()?
        .with_project_server(published(&directory))
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/main.py"), MAIN)?
        .build()
        .wait_until_workspaces_are_initialized();

    let db = cold(&server.file_path(SystemPath::new("src")))?;
    let response = check(&directory, &db)?;
    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::CheckMode
            })
        ),
        "expected a refusal over the check mode, got {response:?}"
    );

    Ok(())
}

/// A server holds a database per workspace, and is asked about a project rather than about a
/// workspace. One it does not hold, it does not answer for.
#[test]
fn refuses_a_project_it_does_not_hold() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let db = cold(&server.file_path(SystemPath::new("src")))?;

    let response = ask(&directory, db.project().root(&db), |record| {
        let mut request = request_for(&db)?;
        request.project_root = request.project_root.join("nested");
        Ok(Request {
            protocol: PROTOCOL,
            token: record.token.clone(),
            client: record.build.clone(),
            payload: serde_json::to_value(Payload::Check(request))?,
        })
    })?;

    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::UnknownProject
            })
        ),
        "expected a refusal over the project, got {response:?}"
    );

    Ok(())
}

/// Two builds of `by` disagree about a project's diagnostics as readily as they disagree
/// about anything else, and the caller asked for its own build's answer.
#[test]
fn refuses_a_different_build() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let db = cold(&server.file_path(SystemPath::new("src")))?;

    let response = ask(&directory, db.project().root(&db), |record| {
        let mut request = check_request(record, &db)?;
        request.client.version = "0.0.0-not-this-one".to_owned();
        Ok(request)
    })?;

    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::Version { .. }
            })
        ),
        "expected a refusal over the build, got {response:?}"
    );

    Ok(())
}

/// Two builds of the same commit report the same version — which is the ordinary state of an
/// editor holding a server while its `by` is rebuilt — so the executable settles it.
#[test]
fn refuses_a_rebuild_of_the_same_version() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let db = cold(&server.file_path(SystemPath::new("src")))?;

    let response = ask(&directory, db.project().root(&db), |record| {
        let mut request = check_request(record, &db)?;
        if let Some(executable) = request.client.executable.as_mut() {
            executable.modified += 1;
        }
        Ok(request)
    })?;

    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::Version { .. }
            })
        ),
        "expected a refusal over the executable, got {response:?}"
    );

    Ok(())
}

/// A protocol the server does not speak is said so, rather than left to fail to parse.
#[test]
fn refuses_a_protocol_it_does_not_speak() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let db = cold(&server.file_path(SystemPath::new("src")))?;

    let response = ask(&directory, db.project().root(&db), |record| {
        let mut request = check_request(record, &db)?;
        request.protocol = PROTOCOL + 1;
        // a later protocol's payload is not this one's, and must not have to be
        request.payload = serde_json::json!({ "kind": "SomethingLater" });
        Ok(request)
    })?;

    assert!(
        matches!(
            response,
            Some(Response::Refused {
                reason: Refusal::Protocol { .. }
            })
        ),
        "expected a refusal over the protocol, got {response:?}"
    );

    Ok(())
}

/// The port is on loopback, which every user on this machine can reach; the token is what
/// makes reaching it depend on being able to read a file that only this user can.
#[test]
fn says_nothing_without_the_token() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let db = cold(&server.file_path(SystemPath::new("src")))?;

    let response = ask(&directory, db.project().root(&db), |record| {
        let mut request = check_request(record, &db)?;
        request.token = "not the token".to_owned();
        Ok(request)
    })?;

    assert!(
        response.is_none(),
        "a request without the token was answered with {response:?}"
    );

    Ok(())
}

/// A record only rules a server *in*. Whether the database is really rooted there is the
/// server's to answer, but a server for an unrelated tree is not worth connecting to at all.
#[test]
fn a_record_is_found_only_from_inside_its_roots() -> Result<()> {
    let directory = TempDir::new()?;
    let server = server(&directory, &[(SystemPath::new("src/main.py"), MAIN)])?;
    let published = published(&directory);
    let project_root = server.file_path(SystemPath::new("src"));

    assert_eq!(discovery::candidates(published, &project_root).len(), 1);
    assert_eq!(
        discovery::candidates(published, &project_root.join("nested")).len(),
        1,
        "a directory inside a root is inside it"
    );
    assert_eq!(
        discovery::candidates(published, SystemPath::new("/somewhere/else")).len(),
        0
    );

    Ok(())
}

/// A record whose server has gone is taken away by whoever discovers that, or it would cost
/// every later `by check` a connection attempt for the life of the machine.
///
/// No server here at all: a record is a file, and this is about what happens to a file whose
/// process is not there any more.
#[test]
fn a_record_outlives_a_server_only_until_somebody_tries_it() -> Result<()> {
    let project = TempDir::new()?;
    let project_root = SystemPath::from_std_path(project.path())
        .unwrap()
        .to_path_buf();
    std::fs::write(project_root.join("main.py").as_std_path(), MAIN)?;

    let directory = TempDir::new()?;
    let published = published(&directory);
    let build = Build::current(ruff_db::program_version().unwrap_or("test"));

    // a port nothing is listening on: bound to learn a free number, then let go
    let port = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?
        .local_addr()?
        .port();
    std::fs::write(
        directory.path().join("stale.json"),
        serde_json::to_vec(&discovery::Record {
            protocol: PROTOCOL,
            build: build.clone(),
            port,
            token: "irrelevant".to_owned(),
            roots: vec![project_root.clone()],
        })?,
    )?;

    let db = cold(&project_root)?;
    assert_eq!(discovery::candidates(published, &project_root).len(), 1);
    assert!(
        ty_server::project_server::client::check(published, request_for(&db)?, &build).is_none(),
        "a record whose server is gone should not produce an answer"
    );
    assert_eq!(
        discovery::candidates(published, &project_root).len(),
        0,
        "the stale record should have been taken away"
    );

    Ok(())
}

/// The smallest notebook the server will accept as one.
const EMPTY_NOTEBOOK: &str = r#"{
 "cells": [],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 5
}"#;
