use crate::capabilities::SupportedCommand;
use crate::server;
use crate::server::api::LSPResult;
use crate::server::api::RequestHandler;
use crate::server::api::traits::SyncRequestHandler;
use crate::session::Session;
use crate::session::client::Client;
use anyhow::Context as _;
use by_stage::run_notes::{Level, Note};
use lsp_server::ErrorCode;
use lsp_types::ExecuteCommandRequest;
use lsp_types::{self as types, MessageType};
use ruff_db::system::{Command, SystemPath, SystemPathBuf};
use std::fmt::{self, Write};
use std::io::{BufRead as _, BufReader, Read};
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};
use ty_ide::{AddDependency, DependencyTarget};
use ty_module_resolver::ModuleResolveMode;
use ty_project::{Db as _, ProjectDatabase, ProjectMetadata};
use ty_python_semantic::SysPrefixPathOrigin;

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

                run_manage(session, client, arguments.arguments)
                    .with_failure_code(ErrorCode::InternalError)?;

                Ok(None)
            }
            SupportedCommand::AddDependency => {
                let arguments: AddDependencyArguments = params
                    .arguments
                    .into_iter()
                    .flatten()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("`{}` takes one argument", params.command))
                    .and_then(|argument| Ok(serde_json::from_value(argument)?))
                    .with_failure_code(ErrorCode::InvalidParams)?;

                add_dependency(session, client, &arguments)
                    .with_failure_code(ErrorCode::InvalidParams)?;

                Ok(None)
            }
        }
    }
}

/// What a `ty.addDependency` is asked to add.
///
/// This is the wire form of [`ty_ide::AddDependency`], which the command line is
/// then built from: the arguments arrive from the client, so the server states
/// what it will run rather than being handed one to run.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct AddDependencyArguments {
    /// The project to add the dependency to, which is the directory uv is run in.
    root: String,
    /// The distribution to add.
    distribution: String,
    /// The extra it is added to, if it is added to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extra: Option<String>,
    /// The dependency group it is added to, if it is added to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
}

/// The command that asks the server to add `add`, for a code action to carry.
pub(crate) fn add_dependency_command(title: &str, add: &AddDependency) -> types::Command {
    let arguments = serde_json::to_value(AddDependencyArguments::new(add))
        .expect("add dependency arguments to be serializable");

    types::Command {
        title: title.to_string(),
        command: SupportedCommand::AddDependency.identifier().to_string(),
        arguments: Some(vec![arguments]),
        tooltip: None,
    }
}

impl AddDependencyArguments {
    fn new(add: &AddDependency) -> Self {
        let (extra, group) = match &add.target {
            DependencyTarget::Project => (None, None),
            DependencyTarget::Extra(extra) => (Some(extra.clone()), None),
            DependencyTarget::Group(group) => (None, Some(group.clone())),
        };

        Self {
            root: add.root.to_string(),
            distribution: add.distribution.clone(),
            extra,
            group,
        }
    }

    /// The dependency these name, or an error if they name none that uv could add.
    fn to_add_dependency(&self) -> crate::Result<AddDependency> {
        // a name is uv's to resolve, but it is also the one part of the command
        // line that comes from the client, and an argument that could be read as
        // a flag is not a distribution under any packaging standard
        if self.distribution.is_empty()
            || !self.distribution.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            return Err(anyhow::anyhow!(
                "`{}` is not a distribution name",
                self.distribution
            ));
        }

        let target = match (&self.extra, &self.group) {
            (Some(extra), None) => DependencyTarget::Extra(extra.clone()),
            (None, Some(group)) => DependencyTarget::Group(group.clone()),
            (None, None) => DependencyTarget::Project,
            (Some(_), Some(_)) => {
                return Err(anyhow::anyhow!(
                    "a dependency is added to an extra or to a group, not to both"
                ));
            }
        };

        Ok(AddDependency {
            root: SystemPathBuf::from(self.root.clone()),
            distribution: self.distribution.clone(),
            target,
        })
    }
}

/// Declares and installs a dependency by running `uv add`, reporting through
/// `window/logMessage`.
///
/// The install is the point: the action this comes from is offered on an import
/// that doesn't resolve, and writing the requirement into `pyproject.toml` alone
/// would leave it not resolving. Once uv is done the projects are re-read, so the
/// diagnostic goes away without the editor having to notice the environment
/// changed underneath it.
fn add_dependency(
    session: &Session,
    client: &Client,
    arguments: &AddDependencyArguments,
) -> crate::Result<()> {
    let add = arguments.to_add_dependency()?;

    // the root names a project of this session's, not any directory a client
    // cares to send: uv is being run in it
    let is_project_root = session
        .project_dbs()
        .any(|db| db.project().root(db) == &*add.root);
    if !is_project_root {
        return Err(anyhow::anyhow!("`{}` is not a project of ty's", add.root));
    }

    let system = session.system();
    let uv = ty_project::uv::executable(system)
        .map_err(|error| anyhow::anyhow!("`uv add` cannot be run: {error}"))?;
    let executor = system
        .command_executor()
        .ok_or_else(|| anyhow::anyhow!("this system cannot run commands"))?
        .dyn_clone();

    let mut command = Command::new(uv.into_string());
    command.args(add.arguments()).current_dir(&add.root);

    let line = format!("uv {}", add.arguments().join(" "));
    let client = client.clone();

    // adding a dependency resolves and downloads, so it is not something to hold
    // the server still for
    std::thread::Builder::new()
        .name("ty:uv".to_string())
        .spawn(move || {
            log(&client, MessageType::Info, format!("running {line}"));

            match executor.execute(command) {
                Ok(output) => {
                    // uv reports its progress and its failures on stderr
                    for stream in [&output.stdout, &output.stderr] {
                        let text = String::from_utf8_lossy(stream);
                        if !text.trim().is_empty() {
                            log(&client, MessageType::Info, text.trim_end().to_string());
                        }
                    }

                    if output.status.success() {
                        log(&client, MessageType::Info, format!("{line} succeeded"));
                        client.rescan_projects();
                    } else {
                        tracing::error!("{line} failed ({})", output.status);
                        client.show_error_message(format!("{line} failed ({})", output.status));
                    }
                }
                Err(error) => {
                    tracing::error!("{line} could not be started: {error}");
                    client.show_error_message(format!("{line} could not be started: {error}"));
                }
            }
        })?;

    Ok(())
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

/// runs the project's `manage.py`, through `by run` when `by run` would run it
///
/// its output goes to `window/logMessage`, all of it, and its outcome to
/// `window/showMessage` once it has finished — succeeded or failed, with the last line
/// it wrote — so that whoever clicked the lens sees what came of it. the click is answered
/// at once, and a command that is not run is refused the same way: the reason is shown and
/// logged, since an error answer to a click is one an editor need not show anyone
///
/// the process is spawned on a thread of its own so that a command that takes a while — a test
/// suite is the whole point of this — doesn't stop the server answering anything else in the
/// meantime. everything else is decided there too, refusals included: whether it runs through
/// `by run` finds `by run`'s project and every one of its sources
fn run_manage(session: &Session, client: &Client, arguments: Vec<String>) -> crate::Result<()> {
    let client = client.clone();
    let manage_run = match ManageRun::new(session, arguments) {
        Ok(manage_run) => manage_run,
        Err(refusal) => {
            refuse(&client, &refusal);
            return Ok(());
        }
    };

    std::thread::Builder::new()
        .name("ty:manage".to_string())
        .spawn(move || match manage_run.plan(&client) {
            Ok(plan) => plan.run(&client),
            Err(refusal) => refuse(&client, &refusal),
        })?;

    Ok(())
}

/// shows and logs why a management command was not run, with every cause it has
fn refuse(client: &Client, refusal: &anyhow::Error) {
    let message = format!("{refusal:#}");
    log(client, MessageType::Error, message.clone());
    client.show_error_message(message);
}

/// what a management command is run with, taken from the session before the thread that
/// decides how to run it starts
struct ManageRun {
    /// the arguments to `manage.py`, the subcommand first
    arguments: Vec<String>,
    /// the environment the editor chose for the project, which `by run` cannot find for itself
    chosen_environment: Option<SystemPathBuf>,
    /// the project of the workspace `manage.py` was found in, which is what decides how a command
    /// runs when `by run` cannot discover a project of its own
    workspace: ProjectMetadata,
    /// whether the workspace's project has a source `by run` would stage
    workspace_has_run_sources: bool,
    /// the `by` this server is, if it was started as one
    by: Option<std::path::PathBuf>,
    manage: SystemPathBuf,
    /// `manage.py`'s file name
    script: String,
    /// the directory `manage.py` is in, which the command runs in
    working_directory: SystemPathBuf,
}

impl ManageRun {
    /// what the session says about running a command through the project's `manage.py`, or the
    /// refusal when no project has one
    fn new(session: &Session, arguments: Vec<String>) -> crate::Result<Self> {
        let (db, manage) = session
            .project_dbs()
            .find_map(|db| {
                let manage = ty_ide::django_manage_script(db)?
                    .path(db)
                    .as_system_path()?
                    .to_path_buf();
                Some((db, manage))
            })
            .ok_or_else(|| anyhow::anyhow!("this project has no `manage.py`"))?;

        // django resolves its settings, its apps and its templates relative to the
        // directory `manage.py` sits in, which is why it is run from there rather than
        // from wherever the editor happens to have been started
        let working_directory = manage
            .parent()
            .ok_or_else(|| anyhow::anyhow!("`{manage}` is not in a directory"))?
            .to_path_buf();
        let script = manage.file_name().unwrap_or("manage.py").to_string();

        Ok(Self {
            arguments,
            chosen_environment: chosen_environment(db),
            workspace: db.project().metadata(db).clone(),
            // the project's files are in memory already, read when `manage.py` was looked for
            workspace_has_run_sources: by_stage::project::has_run_sources(db),
            by: session.by_executable().map(std::path::Path::to_path_buf),
            manage,
            script,
            working_directory,
        })
    }

    /// how the command runs, or the refusal to run it
    fn plan(&self, client: &Client) -> crate::Result<Plan> {
        let script = &self.script;
        let Some(subcommand) = self.arguments.first() else {
            anyhow::bail!("no management command was named");
        };

        // python never imports a `.by` module, so a project with a `.by` model run by python
        // alone has no such model: `makemigrations` finds no changes and `test` runs without
        // it. `by run` stages the project, runs the command in it and writes back what the
        // command wrote there, a migration included. it refuses a project with no `.by`
        // source, which python runs as it is, on the interpreter `by run` would have run it
        // on. the project is the one `by run` finds from the directory it is started in, which
        // need not be this workspace's: a `pyproject.toml` beside `manage.py` makes a project
        // of its own
        let chosen = self.chosen_environment.as_deref();
        let runner = match by_stage::project::build_run_project_db(
            self.working_directory.as_std_path(),
            chosen,
        ) {
            Ok((_, sources, _, _)) if !sources.is_empty() => Runner::ByRun,
            Ok((db, _, _, _)) => Runner::Python(self.python(db.project().metadata(&*db))?),
            // `by run` cannot run a project it cannot discover, and python alone would run
            // this one without its `.by` models
            Err(error) if self.workspace_has_run_sources => {
                return Err(error.context(format!(
                    "`{script}` runs through `by run` in a project with `.by` sources, \
                     and `by run` cannot find the project it is in"
                )));
            }
            Err(error) => {
                log(
                    client,
                    MessageType::Warning,
                    format!(
                        "`by run` cannot find the project `{script}` is in, and it has no `.by` \
                         source, so it runs on python: {error:#}"
                    ),
                );
                Runner::Python(self.python(&self.workspace)?)
            }
        };
        let line = runner.command_line(chosen, script, &self.arguments);

        if TERMINAL_COMMANDS.contains(&subcommand.as_str()) {
            anyhow::bail!(
                "`{script} {subcommand}` needs a terminal, which the server cannot give it. \
                 Run it yourself: `{line}`",
            );
        }
        if !OFFERED_COMMANDS.contains(&subcommand.as_str()) {
            anyhow::bail!(
                "the server only runs the management commands it offers a lens for, \
                 and `{subcommand}` is not one of them. Run it yourself: `{line}`",
            );
        }

        let (mut process, notes) = match &runner {
            Runner::ByRun => {
                let by = self.by.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "this project has `.by` sources, so `{script}` runs through `by run`, \
                         and this server was not started as a `by` it can run"
                    )
                })?;
                // what `by run` says about writing back comes after the command's own last
                // line, so it is written to a file of its own rather than to stderr
                let notes = tempfile::Builder::new().prefix("by-manage").tempdir()?;
                let mut process = std::process::Command::new(by);
                process.arg("run");
                if let Some(chosen) = chosen {
                    process.arg("--python").arg(chosen.as_std_path());
                }
                process
                    .arg("--notes")
                    .arg(notes.path().join(NOTES_FILE))
                    .arg(self.manage.as_std_path());
                (process, Some(notes))
            }
            Runner::Python(python) => {
                let mut process = std::process::Command::new(python);
                process.arg(self.manage.as_std_path());
                (process, None)
            }
        };
        process
            .args(&self.arguments)
            // python buffers what it writes to stdout when that is a pipe, so a line reaches
            // the pipe when the buffer fills or the process exits, after the stderr lines
            // written since. the summary is the last line read, so each line has to reach its
            // pipe when written
            .env("PYTHONUNBUFFERED", "1")
            // the server's own stdin is the editor's connection, which a command that asks a
            // question, as `makemigrations` does about a renamed field, would read as its
            // answer
            .stdin(std::process::Stdio::null())
            .current_dir(self.working_directory.as_std_path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        Ok(Plan {
            process,
            notes,
            line,
        })
    }

    /// the interpreter `by run` would run a program of `project` on, which is the one the editor
    /// chose when it chose one
    fn python(&self, project: &ProjectMetadata) -> crate::Result<String> {
        let chosen = self
            .chosen_environment
            .as_deref()
            .map(SystemPath::as_std_path);
        let interpreter = by_stage::interpreter::discover_interpreter(chosen, project)
            .with_context(|| format!("no python interpreter can run `{}`", self.script))?;
        Ok(interpreter.path)
    }
}

/// the environment the editor chose for the project, if it chose one: the python extension's
/// active environment, or an `environment.python` sent as an editor setting
///
/// it is the one environment the server knows of that `by run` cannot find: `by run` discovers
/// the rest from the project and the environment variables it inherits from the server, as the
/// server does
fn chosen_environment(db: &ProjectDatabase) -> Option<SystemPathBuf> {
    let Ok(Some(environment)) = &db.project().program_settings(db).python_environment else {
        return None;
    };
    matches!(environment.origin(), SysPrefixPathOrigin::Editor)
        .then(|| environment.sys_prefix().to_path_buf())
}

/// what runs a management command
enum Runner {
    /// `by run`, for a project with a source it stages
    ByRun,
    /// this interpreter, for a project `by run` refuses
    Python(String),
}

impl Runner {
    /// the command line a management command runs as, which is what a log and a refusal name it
    /// by
    fn command_line(
        &self,
        chosen: Option<&SystemPath>,
        script: &str,
        arguments: &[String],
    ) -> String {
        let arguments = arguments.join(" ");
        match (self, chosen) {
            (Self::ByRun, Some(chosen)) => format!("by run --python {chosen} {script} {arguments}"),
            (Self::ByRun, None) => format!("by run {script} {arguments}"),
            (Self::Python(_), _) => format!("{script} {arguments}"),
        }
    }
}

/// a management command, ready to run
struct Plan {
    process: std::process::Command,
    /// the directory `by run --notes` writes into, when it runs through `by run`
    notes: Option<tempfile::TempDir>,
    /// the command line it is named by
    line: String,
}

impl Plan {
    /// runs the command to its end, logging what it writes and showing what came of it
    fn run(mut self, client: &Client) {
        let line = &self.line;
        log(client, MessageType::Info, format!("running {line}"));

        match run_to_end(&mut self.process) {
            Ok((output, summary)) => {
                // django's test runner and most of its commands report on
                // stderr, so both streams are the answer rather than one
                for stream in [&output.stdout, &output.stderr] {
                    let text = String::from_utf8_lossy(stream);
                    if !text.trim().is_empty() {
                        log(client, MessageType::Info, text.trim_end().to_string());
                    }
                }

                let notes = match &self.notes {
                    Some(notes) => read_notes(&notes.path().join(NOTES_FILE)),
                    None => Ok(Vec::new()),
                };
                // read once, the directory has served its purpose, and is gone by the time
                // anyone is shown the outcome
                drop(self.notes.take());
                let notes = notes.unwrap_or_else(|error| {
                    vec![Note::new(
                        Level::Warning,
                        format!(
                            "what `by run` said once the command ended could not be read: {error}"
                        ),
                    )]
                });
                for note in &notes {
                    log(client, message_type(note.level), note.to_string());
                }

                let failed = !output.status.success();
                let (kind, outcome) = if failed {
                    (MessageType::Error, format!("failed ({})", output.status))
                } else {
                    (MessageType::Info, "succeeded".to_string())
                };
                log(client, kind, format!("{line} {outcome}"));
                client.show_message(
                    outcome_message(line, &outcome, summary.as_deref(), &notes),
                    match notes.iter().map(|note| note.level).max() {
                        Some(Level::Warning | Level::Error) if !failed => MessageType::Warning,
                        _ => kind,
                    },
                );
            }
            Err(error) => {
                let message = format!("{line} could not be started: {error}");
                log(client, MessageType::Error, message.clone());
                client.show_error_message(message);
            }
        }
    }
}

/// the name of the file `by run --notes` is given, in a directory of its own
const NOTES_FILE: &str = "notes.jsonl";

/// what `by run --notes` wrote. a run that ended before the program did, as one whose project
/// does not check does, wrote none
fn read_notes(file: &std::path::Path) -> std::io::Result<Vec<Note>> {
    match std::fs::read_to_string(file) {
        Ok(written) => Note::read_all(&written).map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn message_type(level: Level) -> MessageType {
    match level {
        Level::Note => MessageType::Info,
        Level::Warning => MessageType::Warning,
        Level::Error => MessageType::Error,
    }
}

/// what a finished command is shown as: its outcome, the last line it wrote, and the first
/// thing `by run` could not write back, which that line does not say
fn outcome_message(line: &str, outcome: &str, summary: Option<&str>, notes: &[Note]) -> String {
    let mut message = match summary {
        Some(summary) => format!("{line} {outcome}: {summary}"),
        None => format!("{line} {outcome}"),
    };
    let mut unwritten = notes.iter().filter(|note| note.level > Level::Note);
    if let Some(first) = unwritten.next() {
        let _ = write!(message, "; {first}");
        let more = unwritten.count();
        if more > 0 {
            let _ = write!(message, " (and {more} more in the log)");
        }
    }
    message
}

/// what `process` wrote, once it has finished, and the last line it wrote that says something
///
/// the two streams are read as they are written, since which of them ends on the summary is the
/// command's to decide: the test runner reports on stderr, `makemigrations` on stdout, and a
/// warning at startup lands on stderr ahead of either
fn run_to_end(
    process: &mut std::process::Command,
) -> std::io::Result<(std::process::Output, Option<String>)> {
    let mut child = process.spawn()?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let last_line = Mutex::new(None);
    let (stdout, stderr) = std::thread::scope(|scope| {
        let stdout = scope.spawn(|| stdout.map(|stream| read_lines(stream, &last_line)));
        let stderr = scope.spawn(|| stderr.map(|stream| read_lines(stream, &last_line)));
        (stdout.join(), stderr.join())
    });
    let output = std::process::Output {
        status: child.wait()?,
        stdout: stdout.ok().flatten().unwrap_or_default(),
        stderr: stderr.ok().flatten().unwrap_or_default(),
    };
    Ok((
        output,
        last_line
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner),
    ))
}

/// everything `stream` holds, keeping `last_line` up to date with each line that says something
fn read_lines(stream: impl Read, last_line: &Mutex<Option<String>>) -> Vec<u8> {
    let mut reader = BufReader::new(stream);
    let mut written = Vec::new();
    loop {
        let start = written.len();
        match reader.read_until(b'\n', &mut written) {
            Ok(0) | Err(_) => return written,
            Ok(_) => {
                let line = String::from_utf8_lossy(&written[start..]);
                let line = line.trim();
                if !line.is_empty() {
                    *last_line.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(line.to_owned());
                }
            }
        }
    }
}

fn log(client: &Client, kind: MessageType, message: String) {
    client.send_notification::<lsp_types::LogMessageNotification>(lsp_types::LogMessageParams {
        kind,
        message,
    });
}

/// Returns a string with detailed memory usage.
fn debug_information(session: &mut Session) -> crate::Result<String> {
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

    for db in session.projects_mut() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// what a management command's outcome is shown with: the last line it wrote
    #[test]
    fn a_commands_summary_is_its_last_line() {
        let last_line = Mutex::new(None);
        let written = b"Ran 3 tests in 0.010s\n\nFAILED (failures=1)\n\n";
        assert_eq!(read_lines(&written[..], &last_line), written);
        assert_eq!(
            last_line.lock().unwrap().as_deref(),
            Some("FAILED (failures=1)")
        );

        let last_line = Mutex::new(None);
        read_lines(&b"\n  \n"[..], &last_line);
        assert_eq!(last_line.lock().unwrap().as_deref(), None);
    }

    /// a warning at startup does not stand in for the line the command ends on, on either stream
    #[cfg(unix)]
    #[test]
    fn a_commands_summary_is_the_last_line_of_either_stream() {
        let mut process = std::process::Command::new("sh");
        process
            .args([
                "-c",
                "echo warning >&2; sleep 0.2; echo 'No changes detected'",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let (output, summary) = run_to_end(&mut process).expect("sh runs");
        assert!(output.status.success());
        assert_eq!(output.stderr, b"warning\n");
        assert_eq!(summary.as_deref(), Some("No changes detected"));
    }

    fn round_tripped(add: &AddDependency) -> crate::Result<AddDependency> {
        let command = add_dependency_command("some title", add);
        let argument = command
            .arguments
            .and_then(|arguments| arguments.into_iter().next())
            .expect("the command to carry its arguments");

        serde_json::from_value::<AddDependencyArguments>(argument)?.to_add_dependency()
    }

    fn added(target: DependencyTarget) -> AddDependency {
        AddDependency {
            root: SystemPathBuf::from("/app"),
            distribution: "numpy".to_string(),
            target,
        }
    }

    #[test]
    fn the_dependency_survives_the_round_trip_to_the_client() {
        for target in [
            DependencyTarget::Project,
            DependencyTarget::Extra("cli".to_string()),
            DependencyTarget::Group("dev".to_string()),
        ] {
            let add = added(target);
            assert_eq!(round_tripped(&add).unwrap(), add);
        }
    }

    #[test]
    fn a_distribution_that_reads_as_a_flag_is_refused() {
        // the name is the one part of the command line that comes from the
        // client, and `uv add` would take this one as an option of its own
        let add = AddDependency {
            distribution: "--directory=/somewhere".to_string(),
            ..added(DependencyTarget::Project)
        };

        assert!(round_tripped(&add).is_err());
    }

    #[test]
    fn a_dependency_in_both_an_extra_and_a_group_is_refused() {
        let arguments = AddDependencyArguments {
            root: "/app".to_string(),
            distribution: "numpy".to_string(),
            extra: Some("cli".to_string()),
            group: Some("dev".to_string()),
        };

        assert!(arguments.to_add_dependency().is_err());
    }
}
