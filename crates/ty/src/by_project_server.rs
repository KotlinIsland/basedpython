//! Checking a project by asking the language server that is already holding it.
//!
//! `by check` builds a project database, resolves an environment, and parses and infers
//! every file in the project — and then exits, and throws all of it away. An editor with the
//! language server running has done that work already and has been keeping it current ever
//! since. So before doing it again, ask.
//!
//! What comes back has to be what this process would have produced on its own, which is why
//! most of what is here is the deciding rather than the asking. The server refuses whenever
//! it might answer differently — a different build, a different configuration, an unsaved
//! buffer — and this side refuses for every invocation whose answer is not simply "the
//! diagnostics for this project": a fix, a watch, a subset of paths.
//!
//! The fallback is not a failure mode, it is the normal path. Nothing here is required to
//! work, and a caller that gets nothing goes on to check for itself.

use std::io::Write;

use ruff_db::Db as _;
use ruff_db::diagnostic::Severity;
use ruff_db::system::SystemPath;
use ty_project::{Db as _, ProjectDatabase};
use ty_server::project_server;
use ty_server::project_server::client;
use ty_server::project_server::protocol;
use ty_server::project_server::protocol::{Build, CheckRequest, CheckResponse, SeverityLevel};

use crate::printer::Printer;
use crate::{ExitStatus, exit_status_from_summary, write_summary};

/// Why one `by check` is not a check a server may answer.
///
/// A server answers one question — the diagnostics for a whole project, as configured — so
/// anything that asks a narrower or a different one is ruled out here. Every one of these
/// would otherwise be a silent change in what the command does.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Ineligible {
    /// The caller passed `--no-server`.
    ///
    /// `BY_NO_PROJECT_SERVER` rules it out too, but later and separately: it is read through
    /// the same [`System`](ruff_db::system::System) the server reads it through, which this
    /// side does not have until it has a database.
    Disabled,

    /// The check re-runs on every change, so everything after the first run is warm already.
    Watch,

    /// The check rewrites the files it reports on, which a server does not do.
    Fixing,

    /// The check was pointed at part of the project rather than all of it.
    Paths,

    /// The caller asked about a database that this path never builds.
    MemoryReport,
}

impl std::fmt::Display for Ineligible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Ineligible::Disabled => "the project server is disabled",
            Ineligible::Watch => "the check is in watch mode",
            Ineligible::Fixing => "the check rewrites the files it reports on",
            Ineligible::Paths => "the check was given paths rather than the whole project",
            Ineligible::MemoryReport => "a memory report was requested",
        })
    }
}

/// Asks a running server to check the project `db` was built for, and prints what it says.
///
/// `None` when there was no answer, for any reason at all. The caller then checks the project
/// itself, which is what it would have done had none of this existed.
///
/// `db` rather than the metadata it came from because the comparison that decides whether the
/// server may answer is over what this process *resolved* — its search paths, its interpreter,
/// its merged configuration — and resolving that is the database's job. Building one is cheap
/// next to checking with it, which is the part this exists to skip.
pub(crate) fn check(
    db: &ProjectDatabase,
    working_directory: &SystemPath,
    printer: Printer,
    ineligible: Option<Ineligible>,
) -> Option<ExitStatus> {
    if let Some(ineligible) = ineligible {
        tracing::debug!("Not asking a project server: {ineligible}");
        return None;
    }

    if project_server::disabled(db.system()) {
        tracing::debug!("Not asking a project server: disabled by the environment");
        return None;
    }

    // a platform with nowhere for a server to have announced itself
    let directory = project_server::discovery::default_directory(db.system())?;

    let project = db.project();
    let options = protocol::configuration(project.metadata(db).to_merged_options().options())
        .inspect_err(|error| tracing::debug!("Failed to serialize the resolved options: {error}"))
        .ok()?;

    let response = client::check(
        &directory,
        CheckRequest {
            project_root: project.root(db).to_path_buf(),
            working_directory: working_directory.to_path_buf(),
            options,
            environment: project_server::environment(db),
            force_exclude: project.force_exclude(db),
            verbose: project.verbose(db),
            color: colored::control::SHOULD_COLORIZE.should_colorize(),
        },
        &Build::current(ruff_db::program_version()?),
    )?;

    Some(report(&response, printer))
}

/// Prints an answer, and says what the command should exit with.
///
/// Separate from asking for it because from here on there is no falling back: the answer has
/// reached the caller's terminal, and checking again would print it twice. A stream that
/// cannot be written to is the same failure it is on the cold path, and is reported the same
/// way rather than being turned into "no server answered".
fn report(response: &CheckResponse, printer: Printer) -> ExitStatus {
    // said before the diagnostics, and on stderr: a reader piping `--output-format=json`
    // somewhere is still owed the explanation of why the answer arrived so fast, and is not
    // owed it in the middle of their json
    if printer.shows_general_messages() {
        writeln!(std::io::stderr(), "using project server information").ok();
    }

    let written = write_answer(response, printer);

    // the project has already been checked and the answer is already partly on its way out,
    // so a broken stream changes what the caller sees but not what the check found. the cold
    // path reports the same failure the same way and returns the same status
    if let Err(error) = written {
        tracing::warn!("Failed to write the diagnostics: {error}");
    }

    exit_status_from_summary(
        response.max_severity.map(severity),
        response.io_error,
        response.error_on_warning,
    )
}

fn write_answer(response: &CheckResponse, printer: Printer) -> std::io::Result<()> {
    {
        let stdout = printer.stream_for_details().lock();
        if stdout.is_enabled() {
            let mut stdout = std::io::BufWriter::new(stdout);
            write!(stdout, "{}", response.rendered)?;
            stdout.flush()?;
        }
    }

    // the cold path warns about both of these, and a caller that cannot tell which path
    // answered it is a caller for whom the two paths are the same command
    if response.empty_project {
        tracing::warn!("No python files found under the given path(s)");
    }
    if response.fatal {
        tracing::warn!(
            "A fatal error occurred while checking some files. \
            Not all project files were analyzed. \
            See the diagnostics list above for details."
        );
    }

    // never cancelled: a check the server could not finish comes back as a refusal, and a
    // refusal never reaches this far
    write_summary(
        printer,
        response.diagnostics,
        response.human_readable,
        None,
        false,
    )
    .map_err(std::io::Error::other)
}

fn severity(level: SeverityLevel) -> Severity {
    match level {
        SeverityLevel::Info => Severity::Info,
        SeverityLevel::Warning => Severity::Warning,
        SeverityLevel::Error => Severity::Error,
        SeverityLevel::Fatal => Severity::Fatal,
    }
}
