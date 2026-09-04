mod args;
mod by_commands;
mod by_init;
mod by_lowering;
mod by_source_encoding;
mod by_stamps;
mod by_wheels;
mod logging;
mod printer;
mod python_version;
mod rule;
mod version;

use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{ExitCode, Termination};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use anyhow::{Context, anyhow};
use clap::{CommandFactory, Parser};
use colored::Colorize;
use crossbeam::channel as crossbeam_channel;
use rayon::ThreadPoolBuilder;
use ruff_db::cancellation::{Canceled, CancellationToken, CancellationTokenSource};
use ruff_db::diagnostic::{
    Diagnostic, DiagnosticId, DisplayDiagnosticConfig, DisplayDiagnostics, Severity, UnifiedFile,
};
use ruff_db::files::File;
use ruff_db::system::{OsSystem, System, SystemPath, SystemPathBuf};
use ruff_db::{STACK_SIZE, max_parallelism};
use ruff_diagnostics::Applicability;
use salsa::Database;
use ty_project::metadata::settings::TerminalSettings;
use ty_project::watch::ProjectWatcher;
use ty_project::{CollectReporter, Db, watch};
use ty_project::{ProjectDatabase, ProjectMetadata};
use ty_python_semantic::{fix_all_diagnostics, suppress_all_diagnostics};
use ty_server::run_server;
use ty_static::EnvVars;

use crate::args::{CheckCommand, Command, ExplainCommand, TerminalColor};
use crate::logging::{VerbosityLevel, setup_tracing};
use crate::printer::Printer;
pub use args::Cli;

pub fn run() -> anyhow::Result<ExitStatus> {
    run_from_args(wild::args_os())
}

/// run ty with an explicit arg list — used by `by` to pass remapped args without a subprocess
pub fn run_from_args<I, T>(iter: I) -> anyhow::Result<ExitStatus>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    setup_rayon();
    ruff_db::set_program_version(crate::version::version().to_string()).unwrap();

    let args = iter.into_iter().map(Into::into);
    let args = argfile::expand_args_from(args, argfile::parse_fromfile, argfile::PREFIX)
        .context("Failed to read CLI arguments from file")?;
    let args = Cli::parse_from(args);

    // type inference recurses with the shape of the program it is checking, so
    // how deep a file it can survive is decided by the stack it runs on. the
    // rayon pool asks for `STACK_SIZE`, but the thread a process starts on gets a
    // platform default — 1 MiB on windows — and the commands that check on the
    // calling thread rather than through the pool (`run`, `build`, `transpile`,
    // `compile`) were overflowing it there. so the whole command runs on a thread
    // this codebase has sized for the job, wherever it is dispatched to
    std::thread::scope(|scope| {
        let command = std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, || run_command(args.command))
            .context("failed to start the worker thread")?;
        match command.join() {
            Ok(status) => status,
            // the panic has already been reported by the default hook; carrying
            // it across the join keeps the process behaving as if it never moved
            // off the starting thread
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

fn run_command(command: Command) -> anyhow::Result<ExitStatus> {
    match command {
        Command::Server => run_server().map(|()| ExitStatus::Success),
        Command::Check(check_args) => run_check(check_args),
        Command::Version { output_format } => Ok(by_commands::cmd_version_by(output_format)),
        Command::GenerateShellCompletion { shell } => {
            use std::io::stdout;

            shell.generate(&mut Cli::command(), &mut stdout());
            Ok(ExitStatus::Success)
        }
        Command::Explain { command } => match command {
            ExplainCommand::Rule {
                rule,
                output_format,
            } => {
                if let Some(name) = rule {
                    rule::rule(&name, output_format)?;
                } else {
                    rule::rules(output_format)?;
                }
                Ok(ExitStatus::Success)
            }
        },
        Command::Run {
            module,
            args,
            min_version,
            python,
            lowering,
            compiled,
        } => by_commands::cmd_run(
            module.as_deref(),
            &args,
            min_version.as_deref(),
            &lowering,
            compiled,
            python.as_deref(),
        ),
        Command::Init {
            path,
            name,
            lib,
            app: _,
            python_version,
        } => {
            let kind = if lib {
                by_init::ProjectKind::Library
            } else {
                by_init::ProjectKind::Application
            };
            let version = python_version
                .unwrap_or_else(|| by_commands::default_project_python_version().to_string());
            by_init::cmd_init(path.as_deref(), name.as_deref(), kind, &version)
        }
        Command::Restage {
            build_directory,
            file,
        } => by_commands::cmd_restage(&build_directory, &file),
        Command::Build {
            min_version,
            wheels,
            out,
            print_manifest,
            lowering,
        } => {
            if wheels {
                by_wheels::cmd_build_wheels(out.as_deref(), &lowering)
            } else {
                by_commands::cmd_build(
                    min_version.as_deref(),
                    &lowering,
                    out.as_deref().unwrap_or(Path::new("out")),
                    print_manifest,
                )
            }
        }
        Command::Compile {
            files,
            output,
            verbose,
            emit_c_only,
            no_any,
            require_native,
            annotate,
            lowering,
        } => by_commands::cmd_compile(
            &files,
            &output,
            by_commands::CompileFlags {
                verbose,
                emit_c_only,
                annotate,
                lowering,
                options: by_build::Options {
                    no_any,
                    require_native,
                    annotate,
                    fallback: None,
                    language: by_irbuild::Language::default(),
                },
            },
        ),
        Command::Transpile {
            file,
            reverse,
            min_version,
            lowering,
        } => by_commands::cmd_transpile(file.as_ref(), reverse, min_version.as_deref(), &lowering),
        Command::GenerateApiFile {
            output,
            stdout,
            project,
            python,
            python_version,
        } => run_generate_api_file(output, stdout, project, python, python_version),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "args are owned in clap; passing by value keeps the call site flat"
)]
fn run_generate_api_file(
    output: Option<std::path::PathBuf>,
    stdout: bool,
    project: Option<SystemPathBuf>,
    python: Option<SystemPathBuf>,
    python_version: Option<crate::python_version::PythonVersion>,
) -> anyhow::Result<ExitStatus> {
    use ruff_ranged_value::RangedValue;
    use ty_project::metadata::options::EnvironmentOptions;
    use ty_project::metadata::value::RelativePathBuf;
    use ty_python_semantic::ProgramEnvironment;
    use ty_python_semantic::api_lockfile::generate_api_lockfile;

    let cwd = {
        let cwd = std::env::current_dir().context("Failed to get the current working directory")?;
        SystemPathBuf::from_path_buf(cwd).map_err(|path| {
            anyhow!(
                "The current working directory `{}` contains non-Unicode characters. ty only supports Unicode paths.",
                path.display()
            )
        })?
    };

    let project_path = project
        .as_ref()
        .map(|p| {
            if p.as_std_path().is_dir() {
                Ok(SystemPath::absolute(p, &cwd))
            } else {
                Err(anyhow!("Provided project path `{p}` is not a directory"))
            }
        })
        .transpose()?
        .unwrap_or_else(|| cwd.clone());

    let system = OsSystem::new(&cwd);

    let mut project_metadata = ProjectMetadata::discover(&project_path, &system)?;
    project_metadata.apply_configuration_files(&system)?;

    let cli_options = ty_project::metadata::options::Options {
        environment: Some(EnvironmentOptions {
            python_version: python_version.map(Into::into).map(RangedValue::cli),
            python: python.map(RelativePathBuf::cli),
            ..EnvironmentOptions::default()
        }),
        ..ty_project::metadata::options::Options::default()
    };
    project_metadata.apply_override_options(cli_options);

    let db = ProjectDatabase::fallible(project_metadata, system)?;
    let project = db.project();

    // walk first-party files only. exclude transpiler build output
    // (`out/`, `build/`, `dist/`) and editor artefacts so the lockfile
    // tracks user source rather than regenerated artefacts
    let indexed = project.files(&db);
    let mut first_party_files: Vec<_> = indexed
        .iter()
        .copied()
        .filter(|file| {
            let path = file.path(&db);
            // only system paths in first-party search paths
            let ruff_db::files::FilePath::System(system_path) = path else {
                return false;
            };
            // skip any file whose path passes through a build-output dir
            let path_str = system_path.as_str();
            for excluded in ["/out/", "/build/", "/dist/"] {
                if path_str.contains(excluded) {
                    return false;
                }
            }
            if path_str.starts_with("out/")
                || path_str.starts_with("build/")
                || path_str.starts_with("dist/")
            {
                return false;
            }
            ty_module_resolver::file_to_module(
                &db,
                ty_python_semantic::Db::program_file(&db, *file).resolver_file(&db),
            )
            .and_then(|module| module.search_path(&db).cloned())
            .is_some_and(|sp| sp.is_first_party())
        })
        .collect();
    first_party_files.sort_by_key(|file| file.path(&db).as_str().to_string());

    let python_version_str = python_version
        .map(|v| v.to_string())
        .unwrap_or_else(|| "default".to_owned());
    let env = first_party_files
        .first()
        .map(|file| ProgramEnvironment::from_file(ty_python_semantic::Db::program_file(&db, *file)))
        .unwrap_or_else(|| ProgramEnvironment::from_program(db.project().program(&db)));
    let lockfile = generate_api_lockfile(&db, &env, first_party_files, &python_version_str);

    if stdout {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        out.write_all(lockfile.as_bytes())?;
    } else {
        let output_path = output.unwrap_or_else(|| std::path::PathBuf::from("api.lock"));
        std::fs::write(&output_path, lockfile)
            .with_context(|| format!("Failed to write {}", output_path.display()))?;
    }

    std::mem::forget(db);
    Ok(ExitStatus::Success)
}

fn run_check(args: CheckCommand) -> anyhow::Result<ExitStatus> {
    // Enabled ANSI colors on Windows 10.
    #[cfg(windows)]
    assert!(colored::control::set_virtual_terminal(true).is_ok());

    set_colored_override(args.color);

    let verbosity = args.verbosity.level();
    let _guard = setup_tracing(verbosity, args.color.unwrap_or_default())?;

    let printer = Printer::new(verbosity, args.no_progress);

    tracing::debug!("Version: {}", version::version());

    // The base path to which all CLI arguments are relative to.
    let cwd = {
        let cwd = std::env::current_dir().context("Failed to get the current working directory")?;
        SystemPathBuf::from_path_buf(cwd).map_err(|path| {
            anyhow!(
                "The current working directory `{}` contains non-Unicode characters. \
                ty only supports Unicode paths.",
                path.display()
            )
        })?
    };

    let project_path = args
        .project
        .as_ref()
        .map(|project| {
            if project.as_std_path().is_dir() {
                Ok(SystemPath::absolute(project, &cwd))
            } else {
                Err(anyhow!(
                    "Provided project path `{project}` is not a directory"
                ))
            }
        })
        .transpose()?
        .unwrap_or_else(|| cwd.clone());

    let check_paths: Vec<_> = args
        .paths
        .iter()
        .map(|path| SystemPath::absolute(path, &cwd))
        .collect();

    let mode = if args.fix {
        MainLoopMode::Fix(FixMode::ApplyFixes)
    } else if args.add_ignore {
        MainLoopMode::Fix(FixMode::AddIgnore)
    } else {
        MainLoopMode::Check
    };

    let system = OsSystem::new(&cwd);
    let watch = args.watch;
    let exit_zero = args.exit_zero;
    let memory_report = std::env::var(EnvVars::TY_MEMORY_REPORT).ok();
    let config_file = args
        .config_file
        .as_ref()
        .map(|path| SystemPath::absolute(path, &cwd));
    let force_exclude = args.force_exclude();

    let mut project_metadata = match &config_file {
        Some(config_file) => {
            ProjectMetadata::from_config_file(config_file.clone(), &project_path, &system)?
        }
        None if check_paths.iter().any(|path| system.is_file(path)) => {
            // `uv check --script` passes a file as its check path. Disable uv workspace metadata
            // for scripts until script integration is implemented in a follow-up.
            ProjectMetadata::discover_without_uv(&project_path, &system)?
        }
        None => ProjectMetadata::discover(&project_path, &system)?,
    };

    if watch && project_metadata.has_uv_workspace() {
        return Err(anyhow!(
            "`--watch` is not supported with uv workspace integration"
        ));
    }

    project_metadata.apply_configuration_files(&system)?;

    project_metadata.apply_override_options(args.into_options());

    let mut db = ProjectDatabase::fallible(project_metadata, system)?;

    // the project's django, which the type checker does not read: its templates are
    // checked alongside the python files, and what its routes get wrong is folded
    // into the python files' own diagnostics. a project without django registers a
    // checker that answers with nothing, and pays nothing
    db.set_checker(Arc::new(ty_ide::DjangoChecker));

    let project = db.project();

    project.set_verbose(&mut db, verbosity >= VerbosityLevel::Verbose);
    project.set_force_exclude(&mut db, force_exclude);

    if !check_paths.is_empty() {
        project.set_included_paths(&mut db, check_paths);
    }

    // Disabling LRU only assumes that the database is short-lived; unlike freezing below, it does
    // not require immutable inputs.
    if !watch {
        ruff_db::disable_lru(&mut db);
    }

    // The CLI never opens files, so this is safe even where the freeze below isn't
    db.freeze_open_files();

    // A one-shot check never mutates these heavily read inputs, so freezing them avoids recording
    // unnecessary Salsa dependencies. Watch mode updates inputs incrementally, fix modes apply
    // source-text overrides, and memory reports measure the database without this optimization, so
    // they must keep the inputs mutable.
    if !watch && matches!(mode, MainLoopMode::Check) && memory_report.is_none() {
        db.freeze();
    }

    let (main_loop, main_loop_cancellation_token) = MainLoop::new(mode, printer);

    // Listen to Ctrl+C and abort the watch mode.
    let main_loop_cancellation_token = Mutex::new(Some(main_loop_cancellation_token));
    ctrlc::set_handler(move || {
        let mut lock = main_loop_cancellation_token.lock().unwrap();

        if let Some(token) = lock.take() {
            token.stop();
        }
    })?;

    let exit_status = if watch {
        main_loop.watch(&mut db)?
    } else {
        main_loop.run(&mut db)?
    };

    let mut stdout = printer.stream_for_requested_summary().lock();
    match memory_report.as_deref() {
        Some("short") => write!(stdout, "{}", db.salsa_memory_dump().display_short())?,
        Some("full") => write!(stdout, "{}", db.salsa_memory_dump().display_full())?,
        Some("json") => writeln!(stdout, "{}", db.salsa_memory_dump().to_json())?,
        Some(other) => {
            tracing::warn!(
                "Unknown value for `TY_MEMORY_REPORT`: `{other}`. \
                Valid values are `short`, `full`, and `json`."
            );
        }
        None => {}
    }

    std::mem::forget(db);

    if matches!(exit_status, ExitStatus::Interrupted) {
        return Ok(ExitStatus::Interrupted);
    }

    if exit_zero {
        Ok(ExitStatus::Success)
    } else {
        Ok(exit_status)
    }
}

#[derive(Copy, Clone)]
pub enum ExitStatus {
    /// Checking was successful and there were no errors.
    Success = 0,

    /// Checking was successful but there were errors.
    Failure = 1,

    /// Checking failed due to an invocation error (e.g. the current directory no longer exists, incorrect CLI arguments, ...)
    Error = 2,

    /// Internal ty error (panic, or any other error that isn't due to the user using the
    /// program incorrectly or transient environment errors).
    InternalError = 101,

    /// Checking was interrupted by Ctrl+C.
    Interrupted = 130,
}

impl ExitStatus {
    const fn is_internal_error(self) -> bool {
        matches!(self, ExitStatus::InternalError)
    }
}

impl Termination for ExitStatus {
    fn report(self) -> ExitCode {
        ExitCode::from(self as u8)
    }
}

struct MainLoop {
    mode: MainLoopMode,

    /// Sender that can be used to send messages to the main loop.
    sender: crossbeam_channel::Sender<MainLoopMessage>,

    /// Receiver for the messages sent **to** the main loop.
    receiver: crossbeam_channel::Receiver<MainLoopMessage>,

    /// Capacity-one channel used to coalesce pending workspace checks.
    check_sender: crossbeam_channel::Sender<()>,
    check_receiver: crossbeam_channel::Receiver<()>,

    /// The file system watcher, if running in watch mode.
    watcher: Option<ProjectWatcher>,

    /// Interface for displaying information to the user.
    printer: Printer,

    /// Cancellation token that gets set by Ctrl+C.
    /// Used for long-running operations on the main thread. Operations on background threads
    /// use Salsa's cancellation mechanism.
    cancellation_token: CancellationToken,
}

impl MainLoop {
    fn new(mode: MainLoopMode, printer: Printer) -> (Self, MainLoopCancellationToken) {
        let (sender, receiver) = crossbeam_channel::bounded(10);
        let (check_sender, check_receiver) = crossbeam_channel::bounded(1);

        let cancellation_token_source = CancellationTokenSource::new();
        let cancellation_token = cancellation_token_source.token();

        (
            Self {
                mode,
                sender: sender.clone(),
                receiver,
                check_sender,
                check_receiver,
                watcher: None,
                printer,
                cancellation_token,
            },
            MainLoopCancellationToken {
                sender,
                source: cancellation_token_source,
            },
        )
    }

    fn watch(mut self, db: &mut ProjectDatabase) -> Result<ExitStatus> {
        tracing::debug!("Starting watch mode");
        let sender = self.sender.clone();
        let watcher = watch::directory_watcher(move |event| {
            sender.send(MainLoopMessage::ApplyChanges(event)).unwrap();
        })?;

        self.watcher = Some(ProjectWatcher::new(watcher, db));
        self.run(db)
    }

    fn run(self, db: &mut ProjectDatabase) -> Result<ExitStatus> {
        self.request_check();

        let result = self.main_loop(db);

        tracing::debug!("Exiting main loop");

        result
    }

    fn request_check(&self) {
        // A pending request already represents a check of the latest database revision.
        let _ = self.check_sender.try_send(());
    }

    fn main_loop(mut self, db: &mut ProjectDatabase) -> Result<ExitStatus> {
        tracing::debug!("Starting main loop");

        let mut revision = 0u64;

        // Apply all queued changes before starting a pending check because every applied change
        // cancels the running check.
        while let Ok(message) = crossbeam_channel::select_biased! {
            recv(self.receiver) -> message => message,
            recv(self.check_receiver) -> request => request.map(|()| MainLoopMessage::CheckWorkspace),
        } {
            match message {
                MainLoopMessage::CheckWorkspace => {
                    let db = db.clone();
                    let sender = self.sender.clone();

                    // Spawn a new task that checks the project. This needs to be done in a separate thread
                    // to prevent blocking the main loop here.
                    rayon::spawn(move || {
                        let mut reporter = IndicatifReporter::from(self.printer);
                        let bar = reporter.bar.clone();

                        match salsa::Cancelled::catch(|| {
                            db.check_with_reporter(&mut reporter);
                            reporter.bar.finish_and_clear();
                            reporter.collector.into_sorted(&db)
                        }) {
                            Ok(result) => {
                                // Send the result back to the main loop for printing.
                                sender
                                    .send(MainLoopMessage::CheckCompleted { result, revision })
                                    .unwrap();
                            }
                            Err(cancelled) => {
                                bar.finish_and_clear();
                                tracing::debug!("Check has been cancelled: {cancelled:?}");
                            }
                        }
                    });
                }

                MainLoopMessage::CheckCompleted {
                    result,
                    revision: check_revision,
                } => {
                    if check_revision != revision {
                        tracing::debug!(
                            "Discarding check result for outdated revision: \
                            current: {revision}, result revision: {check_revision}"
                        );
                        continue;
                    }

                    if db.project().files(db).is_empty() {
                        tracing::warn!("No python files found under the given path(s)");
                    }

                    let result = match self.mode {
                        MainLoopMode::Check => {
                            // TODO: We should have an official flag to silence workspace diagnostics.
                            if std::env::var("TY_MEMORY_REPORT").as_deref() == Ok("json") {
                                return Ok(ExitStatus::Success);
                            }

                            self.write_diagnostics(db, &result, None)?;

                            if self.cancellation_token.is_cancelled() {
                                Err(Canceled)
                            } else {
                                Ok(result)
                            }
                        }
                        MainLoopMode::Fix(mode) => {
                            // both of these rewrite a file through its python tokens, so a
                            // diagnostic on a file that is not python is reported unchanged
                            // rather than fixed
                            let (result, other_language) = split_other_language(db, result);

                            let result = match mode {
                                FixMode::AddIgnore => {
                                    suppress_all_diagnostics(db, result, &self.cancellation_token)
                                }
                                FixMode::ApplyFixes => fix_all_diagnostics(
                                    db,
                                    result,
                                    Applicability::Safe,
                                    &self.cancellation_token,
                                ),
                            };

                            if let Ok(mut result) = result {
                                result.diagnostics.extend(other_language);
                                result.diagnostics.sort_by(|left, right| {
                                    left.rendering_sort_key(db)
                                        .cmp(&right.rendering_sort_key(db))
                                });
                                let fixed_diagnostics = match mode {
                                    FixMode::AddIgnore => None,
                                    FixMode::ApplyFixes => Some(result.count),
                                };
                                self.write_diagnostics(db, &result.diagnostics, fixed_diagnostics)?;

                                let terminal_settings = db.project().settings(db).terminal();
                                let is_human_readable =
                                    terminal_settings.output_format.is_human_readable();

                                if is_human_readable {
                                    match mode {
                                        FixMode::AddIgnore => {
                                            writeln!(
                                                self.printer.stream_for_failure_summary(),
                                                "Added {} ignore comment{}",
                                                result.count,
                                                if result.count > 1 { "s" } else { "" }
                                            )?;
                                        }
                                        FixMode::ApplyFixes => {}
                                    }
                                }

                                Ok(result.diagnostics)
                            } else {
                                Err(Canceled)
                            }
                        }
                    };

                    let exit_status = match result.as_deref() {
                        Ok([]) => ExitStatus::Success,
                        Ok(diagnostics) => {
                            let terminal_settings = db.project().settings(db).terminal();
                            exit_status_from_diagnostics(diagnostics, terminal_settings)
                        }
                        Err(Canceled) => ExitStatus::Interrupted,
                    };

                    if exit_status.is_internal_error() {
                        tracing::warn!(
                            "A fatal error occurred while checking some files. \
                            Not all project files were analyzed. \
                            See the diagnostics list above for details."
                        );
                    }

                    if self.watcher.is_some() {
                        continue;
                    }

                    return Ok(exit_status);
                }

                MainLoopMessage::ApplyChanges(changes) => {
                    Printer::clear_screen()?;

                    revision += 1;
                    // Automatically cancels any pending queries and waits for them to complete.
                    db.apply_changes(&changes);
                    if let Some(watcher) = self.watcher.as_mut() {
                        watcher.update(db);
                    }

                    self.request_check();
                }
                MainLoopMessage::Exit => {
                    // Cancel any pending queries and wait for them to complete.
                    db.trigger_cancellation();
                    return Ok(ExitStatus::Interrupted);
                }
            }

            tracing::debug!("Waiting for next main loop message.");
        }

        Ok(ExitStatus::Success)
    }

    fn write_diagnostics(
        &self,
        db: &ProjectDatabase,
        diagnostics: &[Diagnostic],
        fixed_diagnostics: Option<usize>,
    ) -> anyhow::Result<()> {
        let terminal_settings = db.project().settings(db).terminal();
        let is_human_readable = terminal_settings.output_format.is_human_readable();

        match diagnostics {
            [] if is_human_readable && fixed_diagnostics.is_none_or(|fixed| fixed == 0) => {
                writeln!(
                    self.printer.stream_for_success_summary(),
                    "{}",
                    "All checks passed!".green().bold()
                )?;
            }
            diagnostics => {
                let diagnostics_count = diagnostics.len();

                let stdout = self.printer.stream_for_details().lock();

                // Only render diagnostics if they're going to be displayed, since doing
                // so is expensive.
                if stdout.is_enabled() {
                    let mut stdout = BufWriter::new(stdout);
                    let display_config = DisplayDiagnosticConfig::new("ty")
                        .format(terminal_settings.output_format.into())
                        .color(colored::control::SHOULD_COLORIZE.should_colorize())
                        .with_cancellation_token(Some(self.cancellation_token.clone()))
                        .context(0);

                    write!(
                        stdout,
                        "{}",
                        DisplayDiagnostics::new(db, &display_config, diagnostics)
                    )?;
                    stdout.flush()?;
                }

                if !self.cancellation_token.is_cancelled() && is_human_readable {
                    if let Some(fixed) = fixed_diagnostics {
                        let total = fixed + diagnostics_count;
                        writeln!(
                            self.printer.stream_for_failure_summary(),
                            "Found {total} diagnostic{} \
                            ({fixed} fixed, {diagnostics_count} remaining).",
                            if total == 1 { "" } else { "s" }
                        )?;
                    } else {
                        writeln!(
                            self.printer.stream_for_failure_summary(),
                            "Found {} diagnostic{}",
                            diagnostics_count,
                            if diagnostics_count > 1 { "s" } else { "" }
                        )?;
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Copy, Clone, Debug)]
enum MainLoopMode {
    Check,
    Fix(FixMode),
}

#[derive(Copy, Clone, Debug)]
enum FixMode {
    AddIgnore,
    ApplyFixes,
}

/// Split `diagnostics` into the python ones and the ones a registered
/// [`ty_project::ProjectChecker`] owns the file of.
///
/// Everything that rewrites a file — applying a fix, adding an ignore comment —
/// works through the file's python tokens, and the second group has none.
fn split_other_language(
    db: &ProjectDatabase,
    diagnostics: Vec<Diagnostic>,
) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    let Some(checker) = db.project_checker() else {
        return (diagnostics, Vec::new());
    };

    diagnostics.into_iter().partition(|diagnostic| {
        let owned = diagnostic
            .primary_span_ref()
            .and_then(|span| match span.file() {
                UnifiedFile::Ty(file) => file.path(db).as_system_path(),
                UnifiedFile::Ruff(_) => None,
            })
            .is_some_and(|path| checker.owns(db, path));

        !owned
    })
}

fn exit_status_from_diagnostics(
    diagnostics: &[Diagnostic],
    terminal_settings: &TerminalSettings,
) -> ExitStatus {
    if diagnostics.is_empty() {
        return ExitStatus::Success;
    }

    let mut max_severity = Severity::Info;
    let mut io_error = false;

    for diagnostic in diagnostics {
        max_severity = max_severity.max(diagnostic.severity());
        io_error = io_error || matches!(diagnostic.id(), DiagnosticId::Io);
    }

    if !max_severity.is_fatal() && io_error {
        return ExitStatus::Error;
    }

    match max_severity {
        Severity::Info => ExitStatus::Success,
        Severity::Warning => {
            if terminal_settings.error_on_warning {
                ExitStatus::Failure
            } else {
                ExitStatus::Success
            }
        }
        Severity::Error => ExitStatus::Failure,
        Severity::Fatal => ExitStatus::InternalError,
    }
}

/// A progress reporter for `ty check`.
struct IndicatifReporter {
    collector: CollectReporter,

    /// A reporter that is ready, containing a progress bar to report to.
    ///
    /// Initialization of the bar is deferred to [`ty_project::ProgressReporter::set_files`] so we
    /// do not initialize the bar too early as it may take a while to collect the number of files to
    /// process and we don't want to display an empty "0/0" bar.
    bar: indicatif::ProgressBar,

    printer: Printer,
}

impl From<Printer> for IndicatifReporter {
    fn from(printer: Printer) -> Self {
        Self {
            bar: indicatif::ProgressBar::hidden(),
            collector: CollectReporter::default(),
            printer,
        }
    }
}

impl ty_project::ProgressReporter for IndicatifReporter {
    fn set_files(&mut self, files: usize) {
        self.collector.set_files(files);

        self.bar.set_length(files as u64);
        self.bar.set_message("Checking");
        self.bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{msg:8.dim} {bar:60.green/dim} {pos}/{len} files",
            )
            .unwrap()
            .progress_chars("--"),
        );
        self.bar.set_draw_target(self.printer.progress_target());
    }

    fn report_checked_file(&self, db: &ProjectDatabase, file: File, diagnostics: &[Diagnostic]) {
        self.collector.report_checked_file(db, file, diagnostics);
        self.bar.inc(1);
    }

    fn report_diagnostics(&mut self, db: &ProjectDatabase, diagnostics: Vec<Diagnostic>) {
        self.collector.report_diagnostics(db, diagnostics);
    }
}

#[derive(Debug)]
struct MainLoopCancellationToken {
    sender: crossbeam_channel::Sender<MainLoopMessage>,
    source: CancellationTokenSource,
}

impl MainLoopCancellationToken {
    fn stop(self) {
        self.source.cancel();
        self.sender.send(MainLoopMessage::Exit).unwrap();
    }
}

/// Message sent from the orchestrator to the main loop.
#[derive(Debug)]
enum MainLoopMessage {
    CheckWorkspace,
    CheckCompleted {
        /// The diagnostics that were found during the check.
        result: Vec<Diagnostic>,
        revision: u64,
    },
    ApplyChanges(Vec<watch::ChangeEvent>),
    Exit,
}

fn set_colored_override(color: Option<TerminalColor>) {
    let Some(color) = color else {
        return;
    };

    match color {
        TerminalColor::Auto => {
            colored::control::unset_override();
        }
        TerminalColor::Always => {
            colored::control::set_override(true);
        }
        TerminalColor::Never => {
            colored::control::set_override(false);
        }
    }
}

/// Initializes the global rayon thread pool to never use more than `TY_MAX_PARALLELISM` threads.
fn setup_rayon() {
    ThreadPoolBuilder::default()
        .num_threads(max_parallelism().get())
        .stack_size(STACK_SIZE)
        .build_global()
        .unwrap();
}
