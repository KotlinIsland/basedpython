//! Answering a command line's check out of the session the editor is already keeping warm.
//!
//! The whole of the saving is that this runs on a database somebody else built and has been
//! feeding changes into. What is left is the part a cold run cannot skip either: deciding
//! whether this database is allowed to answer for this caller, and rendering.

use std::path::{Path, PathBuf};

use ruff_db::Db as _;
use ruff_db::diagnostic::{
    DiagnosticId, DisplayDiagnosticConfig, DisplayDiagnostics, FileResolver, Input, Severity,
    UnifiedFile,
};
use ruff_db::files::File;
use ruff_db::system::SystemPath;
use ruff_notebook::NotebookIndex;
use ty_project::{CheckMode, CollectReporter, Db as _, ProjectDatabase};

use super::Outcome;
use super::protocol;
use super::protocol::{CheckRequest, CheckResponse, Refusal, Response, SeverityLevel};
use crate::session::{OpenDocument, SessionSnapshot};

/// Runs `request` against `snapshot`, or explains why it did not.
///
/// A [`Refusal`] is a normal outcome, not an error. The caller falls back to checking for
/// itself, which is slower and always correct, so every uncertainty here resolves to one.
pub(super) fn run(snapshot: &SessionSnapshot, request: &CheckRequest, rescanned: bool) -> Outcome {
    let db = match gate(snapshot, request) {
        Ok(db) => db,
        Err(refusal) => return Outcome::Answered(Response::Refused { reason: refusal }),
    };

    // everything that could have refused has agreed, so this request is going to be answered
    // — and now it is worth going to the file system, which is the one thing that decides
    // whether the answer is about the project as it is now
    if !rescanned {
        return Outcome::NeedsRescan;
    }

    Outcome::Answered(answer(db, request))
}

/// Everything that has to be true before this session may answer for `request`.
///
/// Separate from [`answer`] because it is the cheap half, and because the main loop runs it
/// first on its own: re-reading the file system is expensive and interrupts the editor, and
/// there is no reason to do either for a request that was never going to be answered.
fn gate<'a>(
    snapshot: &'a SessionSnapshot,
    request: &CheckRequest,
) -> Result<&'a ProjectDatabase, Refusal> {
    let Some(db) = project_database(snapshot, request) else {
        return Err(Refusal::UnknownProject);
    };

    // a server that has been told to diagnose only what is open checks only what is open,
    // and would answer a whole project's check with a handful of files' diagnostics. the
    // editor's `diagnostic-mode` decides this, and nothing here may quietly override it: it
    // is a salsa input on a project the editor is also using
    if db.check_mode() != CheckMode::AllFiles {
        return Err(Refusal::CheckMode);
    }

    if let Some(refusal) = disagreement(db, request) {
        return Err(refusal);
    }

    // this database reads an open file out of the editor's buffer, so for as long as a
    // buffer differs from the file, the two sides are checking different programs
    let unsaved = unsaved_documents(db, snapshot);
    if !unsaved.is_empty() {
        return Err(Refusal::Unsaved { files: unsaved });
    }

    Ok(db)
}

/// Checks the project and renders what it found.
fn answer(db: &ProjectDatabase, request: &CheckRequest) -> Response {
    let diagnostics = match salsa::Cancelled::catch(|| {
        let mut reporter = CollectReporter::default();
        db.check_with_reporter(&mut reporter);
        reporter.into_sorted(db)
    }) {
        Ok(diagnostics) => diagnostics,
        Err(cancelled) => {
            tracing::debug!("A command-line check was cancelled: {cancelled:?}");
            return Response::Refused {
                reason: Refusal::Cancelled,
            };
        }
    };

    let terminal = db.project().settings(db).terminal();
    let config = DisplayDiagnosticConfig::new("ty")
        .format(terminal.output_format.into())
        .color(request.color)
        .context(0);
    let resolver = RelativeTo {
        db,
        working_directory: working_directory(db, request),
    };

    let mut max_severity = None;
    let mut io_error = false;
    for diagnostic in &diagnostics {
        max_severity = max_severity.max(Some(diagnostic.severity()));
        io_error = io_error || matches!(diagnostic.id(), DiagnosticId::Io);
    }

    Response::Check(CheckResponse {
        rendered: DisplayDiagnostics::new(&resolver, &config, &diagnostics).to_string(),
        diagnostics: diagnostics.len(),
        human_readable: terminal.output_format.is_human_readable(),
        max_severity: max_severity.map(SeverityLevel::from),
        io_error,
        error_on_warning: terminal.error_on_warning,
        empty_project: db.project().files(db).is_empty(),
        fatal: max_severity == Some(Severity::Fatal),
    })
}

/// Where the caller is standing, spelled the way this database spells it.
///
/// Both sides reach the same tree, and neither necessarily by the same name — see
/// [`super::canonical`]. Paths are relativized by stripping a prefix, so the caller's own
/// spelling of its directory would strip nothing off the files this database holds. What
/// makes it strip is putting the caller's position *inside* the project onto the root this
/// database uses.
///
/// A caller standing outside the project — `by check --project ../elsewhere` — has no
/// position inside it, and gets its own directory back. Nothing is stripped then, which
/// leaves absolute paths, which is what a cold check prints from there too.
fn working_directory(db: &ProjectDatabase, request: &CheckRequest) -> PathBuf {
    let root = db.project().root(db);
    let relative = super::canonical(&request.working_directory)
        .strip_prefix(super::canonical(root))
        .map(SystemPath::to_path_buf);

    match relative {
        Ok(relative) => root.join(relative).as_std_path().to_path_buf(),
        Err(_) => request.working_directory.as_std_path().to_path_buf(),
    }
}

/// The database, rendering paths from where the caller is standing rather than from where the
/// server is.
///
/// Everything a rendering needs comes from the database, except the one thing that does not
/// belong to it: which directory the reader will read the answer in. A server's own is
/// wherever its editor was started, which is nowhere in particular.
struct RelativeTo<'db> {
    db: &'db ProjectDatabase,
    working_directory: PathBuf,
}

impl FileResolver for RelativeTo<'_> {
    fn path(&self, file: File) -> &str {
        self.db.path(file)
    }

    fn input(&self, file: File) -> Input {
        self.db.input(file)
    }

    fn notebook_index(&self, file: &UnifiedFile) -> Option<NotebookIndex> {
        self.db.notebook_index(file)
    }

    fn is_notebook(&self, file: &UnifiedFile) -> bool {
        self.db.is_notebook(file)
    }

    fn current_directory(&self) -> &Path {
        &self.working_directory
    }
}

impl From<Severity> for SeverityLevel {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Info => SeverityLevel::Info,
            Severity::Warning => SeverityLevel::Warning,
            Severity::Error => SeverityLevel::Error,
            Severity::Fatal => SeverityLevel::Fatal,
        }
    }
}

/// Whatever this database and the caller disagree about, if anything.
///
/// Both sides resolved the same project from the same files on disk, and they have to have
/// resolved it to the same thing before one can answer for the other. What is compared is
/// what each side actually ended up with rather than what it was told, because most of the
/// ways they can differ are not written down anywhere: a flag on the command line, a setting
/// the editor contributed, an interpreter discovered out of one process's environment and not
/// the other's.
fn disagreement(db: &ProjectDatabase, request: &CheckRequest) -> Option<Refusal> {
    let options =
        match protocol::configuration(db.project().metadata(db).to_merged_options().options()) {
            Ok(options) => options,
            Err(error) => {
                tracing::debug!("Failed to serialize the project's options: {error}");
                return Some(Refusal::UnknownProject);
            }
        };
    if options != request.options {
        return Some(Refusal::Options {
            server: Box::new(options),
        });
    }

    let environment = environment(db);
    if environment != request.environment {
        return Some(Refusal::Environment {
            server: environment,
        });
    }

    if request.verbose {
        return Some(Refusal::Verbose);
    }

    let force_exclude = db.project().force_exclude(db);
    if force_exclude != request.force_exclude {
        return Some(Refusal::ForceExclude {
            server: force_exclude,
        });
    }

    None
}

/// A project's resolved python version, platform and search paths, as one comparable string.
///
/// The `Debug` rendering rather than a hand-built one because it is already the curated set:
/// it prints the four kinds of search path and deliberately leaves out the typeshed version
/// map, which is thousands of lines and identical wherever the typeshed path is.
///
/// The project's own directory is written out of it first. Two processes reach the same tree
/// by different names — a `by check` takes its root from the working directory, which the
/// operating system hands over with symlinks resolved, while an editor sends whatever it was
/// opened with — and every first-party search path, and every path under the project such as
/// a `.venv` beside it, inherits that difference. What is left is what the two would actually
/// disagree about: a different interpreter, a different typeshed, a different version.
pub fn environment(db: &ProjectDatabase) -> String {
    let rendered = format!("{:?}", db.project().program_settings(db));
    let root = db.project().root(db);

    // both spellings, because which one this side happens to hold is the whole problem
    let canonical = super::canonical(root);
    let mut rendered = rendered.replace(root.as_str(), PROJECT_ROOT);
    if canonical.as_str() != root.as_str() {
        rendered = rendered.replace(canonical.as_str(), PROJECT_ROOT);
    }
    rendered
}

/// What the project's own directory is called in an [`environment`] rendering.
const PROJECT_ROOT: &str = "<project>";

/// The database rooted exactly at the project the caller resolved, if this session holds one.
///
/// Exactly, not enclosing: a caller inside a workspace member resolved that member as its
/// project, and a database rooted at the repository above it checks a different set of files
/// under different settings.
fn project_database<'a>(
    snapshot: &'a SessionSnapshot,
    request: &CheckRequest,
) -> Option<&'a ProjectDatabase> {
    let wanted = super::canonical(&request.project_root);

    snapshot
        .projects()
        .iter()
        .find(|db| super::canonical(db.project().root(*db)) == wanted)
}

/// The open documents this project might read whose content is not what is on disk.
///
/// `db` first because everything about the question is the database's: which paths are in
/// scope, and what the file system says a path holds.
///
/// In scope is the project's own tree, plus anything this database has already interned a
/// file for — an editable install, a stub package, a file on an extra search path. The second
/// half matters because [`LSPSystem`](crate::system::LSPSystem) lays the editor's buffers
/// over the whole file system, not over the project: a check that resolves an import into a
/// file the editor happens to be holding reads the buffer. What is deliberately *not* in
/// scope is a document from some unrelated workspace in the same editor window, which this
/// project would never read and which should not cost a caller its answer.
fn unsaved_documents(db: &ProjectDatabase, snapshot: &SessionSnapshot) -> Vec<String> {
    let root = super::canonical(db.project().root(db));

    snapshot
        .open_documents()
        .filter(|(path, _)| {
            super::canonical(path).starts_with(&root) || db.files().try_system(db, path).is_some()
        })
        .filter(|(path, document)| match *document {
            // a file the file system cannot produce at all — one the editor holds for a file
            // that has since been deleted — differs like any other
            OpenDocument::Text(contents) => {
                snapshot.read_from_disk(path).as_deref() != Some(contents)
            }
            OpenDocument::Notebook => true,
        })
        .map(|(path, _)| {
            // the prefix to strip is the canonical root, so what it is stripped from has to
            // be the canonical path — a file in scope only because the database interned it
            // may still sit outside the root, and keeps its own spelling
            let canonical = super::canonical(path);
            canonical
                .strip_prefix(&root)
                .unwrap_or(path)
                .as_str()
                .to_string()
        })
        .collect()
}
