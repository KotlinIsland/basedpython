use crate::capabilities::SupportedCommand;
use crate::server;
use crate::server::api::LSPResult;
use crate::server::api::RequestHandler;
use crate::server::api::traits::SyncRequestHandler;
use crate::session::Session;
use crate::session::client::Client;
use lsp_server::ErrorCode;
use lsp_types::ExecuteCommandRequest;
use lsp_types::{self as types, MessageType};
use ruff_db::Db as _;
use ruff_db::system::SystemPathBuf;
use std::fmt::{self, Write};
use std::str::FromStr;
use ty_module_resolver::ModuleResolveMode;
use ty_project::{Db as _, ProjectDatabase};

pub(crate) struct ExecuteCommand;

impl RequestHandler for ExecuteCommand {
    type RequestType = ExecuteCommandRequest;
}

impl SyncRequestHandler for ExecuteCommand {
    fn run(
        session: &mut Session,
        client: &Client,
        params: types::ExecuteCommandParams,
    ) -> server::Result<Option<serde_json::Value>> {
        let command = SupportedCommand::from_str(&params.command)
            .with_failure_code(ErrorCode::InvalidParams)?;

        match command {
            SupportedCommand::Debug => Ok(Some(serde_json::Value::String(
                debug_information(session).with_failure_code(ErrorCode::InternalError)?,
            ))),
            SupportedCommand::RunManage => {
                let arguments: ManageArguments = params
                    .arguments
                    .into_iter()
                    .flatten()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("`{}` takes one argument", params.command))
                    .and_then(|argument| Ok(serde_json::from_value(argument)?))
                    .with_failure_code(ErrorCode::InvalidParams)?;

                run_manage(session, client, &arguments.arguments)
                    .with_failure_code(ErrorCode::InvalidParams)?;

                Ok(None)
            }
        }
    }
}

/// What a `ty.runManageCommand` is asked to run.
#[derive(Debug, serde::Deserialize)]
struct ManageArguments {
    /// The arguments to `manage.py`, the subcommand first.
    arguments: Vec<String>,
}

/// Django management commands that must not be run this way.
///
/// A command the server spawns has no terminal: its output is a log the client
/// appends to, there is no stdin to type into and no way to interrupt it. That is
/// fine for a command that runs and finishes, and wrong for one that expects to
/// own a terminal — a `runserver` would run until the editor was closed with no
/// way to stop it, and a `shell` would sit waiting for input that can never
/// arrive. Neither is offered by any lens; this is what keeps a client that asks
/// for one anyway from being left with a process it cannot reach. The refusal
/// hands back the command line instead, so a client that wants to open a terminal
/// of its own has what to put in it.
const TERMINAL_COMMANDS: &[&str] = &["runserver", "shell", "dbshell", "createsuperuser"];

/// The management commands a lens offers, and so the only ones this will run.
///
/// Spawning a process is the one thing the server does that reaches outside
/// itself, so what it will spawn is stated rather than inferred: a command
/// nothing offered is refused. Django's own set includes `flush` and
/// `sqlflush`, and a project can add whatever it likes to it, so "anything but
/// the few we know are wrong" is a much larger promise than this needs to make.
const OFFERED_COMMANDS: &[&str] = &["test", "makemigrations", "migrate", "sqlmigrate"];

/// Runs the project's `manage.py`, reporting through `window/logMessage`.
///
/// The process is spawned on a thread of its own so that a command that takes a
/// while — a test suite is the whole point of this — doesn't stop the server
/// answering anything else in the meantime.
fn run_manage(session: &Session, client: &Client, arguments: &[String]) -> crate::Result<()> {
    let Some(subcommand) = arguments.first() else {
        return Err(anyhow::anyhow!("no management command was named"));
    };

    // this is a refusal about the command itself, so it does not wait on finding
    // the project's `manage.py` or its interpreter first
    if TERMINAL_COMMANDS.contains(&subcommand.as_str()) {
        return Err(anyhow::anyhow!(
            "`manage.py {subcommand}` needs a terminal, which the server cannot give it. \
             Run it yourself: `manage.py {}`",
            arguments.join(" "),
        ));
    }

    if !OFFERED_COMMANDS.contains(&subcommand.as_str()) {
        return Err(anyhow::anyhow!(
            "the server only runs the management commands it offers a lens for, \
             and `{subcommand}` is not one of them. Run it yourself: `manage.py {}`",
            arguments.join(" "),
        ));
    }

    let (manage, interpreter) = session
        .project_dbs()
        .find_map(|db| {
            let manage = ty_ide::django_manage_script(db)?
                .path(db)
                .as_system_path()?
                .to_path_buf();

            Some((manage, interpreter(db)))
        })
        .ok_or_else(|| anyhow::anyhow!("this project has no `manage.py`"))?;

    let interpreter = interpreter.ok_or_else(|| {
        anyhow::anyhow!(
            "no python interpreter could be found for this project, \
             so `manage.py` cannot be run"
        )
    })?;

    // django resolves its settings, its apps and its templates relative to the
    // directory `manage.py` sits in, which is why it is run from there rather than
    // from wherever the editor happens to have been started
    let working_directory = manage
        .parent()
        .ok_or_else(|| anyhow::anyhow!("`{manage}` is not in a directory"))?
        .to_path_buf();

    let mut process = std::process::Command::new(interpreter.as_std_path());
    process
        .arg(manage.as_std_path())
        .args(arguments)
        .current_dir(working_directory.as_std_path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let line = format!("manage.py {}", arguments.join(" "));
    let client = client.clone();

    std::thread::Builder::new()
        .name("ty:manage".to_string())
        .spawn(move || {
            log(&client, MessageType::Info, format!("running {line}"));

            match process.output() {
                Ok(output) => {
                    // django's test runner and most of its commands report on
                    // stderr, so both streams are the answer rather than one
                    for stream in [&output.stdout, &output.stderr] {
                        let text = String::from_utf8_lossy(stream);
                        if !text.trim().is_empty() {
                            log(&client, MessageType::Info, text.trim_end().to_string());
                        }
                    }

                    let (kind, outcome) = if output.status.success() {
                        (MessageType::Info, "succeeded".to_string())
                    } else {
                        (MessageType::Error, format!("failed ({})", output.status))
                    };
                    log(&client, kind, format!("{line} {outcome}"));
                }
                Err(error) => log(
                    &client,
                    MessageType::Error,
                    format!("{line} could not be started: {error}"),
                ),
            }
        })?;

    Ok(())
}

fn log(client: &Client, kind: MessageType, message: String) {
    client.send_notification::<lsp_types::LogMessageNotification>(lsp_types::LogMessageParams {
        kind,
        message,
    });
}

/// The interpreter of the environment whose packages the project is checked against.
///
/// ty resolves a python environment to find the project's third-party packages, and
/// its `site-packages` is the one link back to the installation it came from — which
/// is the installation that has django in it, and so the only one able to run
/// `manage.py`. The layout is walked upwards and the candidate is *checked to
/// exist* rather than assumed, so an environment shaped like nothing here expects
/// yields no interpreter instead of an unrunnable path.
fn interpreter(db: &ProjectDatabase) -> Option<SystemPathBuf> {
    // `<prefix>/lib/python3.13/site-packages` on unix, `<prefix>/Lib/site-packages`
    // on windows: three hops up covers the deeper of the two, and every shallower
    // ancestor is tried on the way
    const MAX_PREFIX_DEPTH: usize = 3;

    const EXECUTABLES: &[&str] = &[
        "bin/python3",
        "bin/python",
        "Scripts/python.exe",
        "python.exe",
    ];

    let system = db.system();

    db.project()
        .program(db)
        .search_paths(db)
        .site_packages_paths()
        .flat_map(|site_packages| site_packages.ancestors().take(MAX_PREFIX_DEPTH + 1))
        .find_map(|prefix| {
            EXECUTABLES
                .iter()
                .map(|executable| prefix.join(executable))
                .find(|candidate| system.is_file(candidate))
        })
}

/// Returns a string with detailed memory usage.
fn debug_information(session: &Session) -> crate::Result<String> {
    let mut buffer = String::new();

    writeln!(
        buffer,
        "Client capabilities: {:#?}",
        session.client_capabilities()
    )?;
    writeln!(
        buffer,
        "Position encoding: {:#?}",
        session.position_encoding()
    )?;
    writeln!(buffer, "Global settings: {:#?}", session.global_settings())?;
    writeln!(
        buffer,
        "Open text documents: {}",
        session.text_document_handles().count()
    )?;
    writeln!(buffer)?;

    for (root, workspace) in session.workspaces() {
        writeln!(buffer, "Workspace {root} ({})", workspace.uri())?;
        writeln!(buffer, "Settings: {:#?}", workspace.settings())?;
        writeln!(buffer)?;
    }

    for db in session.project_dbs() {
        writeln!(buffer, "Project at {}", db.project().root(db))?;
        let program = db.project().program(db);
        writeln!(buffer, "Program:")?;
        writeln!(buffer, "  python-version: {}", program.python_version(db))?;
        writeln!(buffer, "  python-platform: {}", program.python_platform(db))?;
        let mut writer = IndentingWriter {
            inner: &mut buffer,
            indent: "  ",
            at_line_start: false,
        };
        writeln!(
            writer,
            "  search-paths: {:#}",
            program
                .resolver_environment(db)
                .display_search_paths(db, ModuleResolveMode::Typing)
        )?;

        writeln!(buffer, "Settings: {:#?}", db.project().settings(db))?;
        writeln!(buffer)?;
        writeln!(
            buffer,
            "Memory report:\n{}",
            db.salsa_memory_dump().display_full()
        )?;
    }
    Ok(buffer)
}

struct IndentingWriter<'a> {
    inner: &'a mut String,
    indent: &'static str,
    at_line_start: bool,
}

impl Write for IndentingWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for part in s.split_inclusive('\n') {
            if self.at_line_start {
                self.inner.write_str(self.indent)?;
            }
            self.inner.write_str(part)?;
            self.at_line_start = part.ends_with('\n');
        }

        Ok(())
    }
}
