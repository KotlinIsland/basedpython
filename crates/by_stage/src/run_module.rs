//! Which file `by run <module>` executes, which module a file runs as, and what `by run
//! path/to/file.py` runs.
//!
//! `by run` stages every first-party module into a tree mirroring the module
//! tree (see [`crate::staging::transpiled_destination`]) and hands the name to
//! `runpy` there. So a name means what the project's resolver makes of it,
//! restricted to the module roots the build stages — and an editor asking "what
//! does `by run app.cli` run" or "what do I pass `by run` to run this file" has
//! to be told that, not a walk of the directory tree that agrees with it most of
//! the time.
//!
//! A path to a file is not a name: `by run backend/manage.py` runs the file as python runs a
//! script, in no package and with the staged copy of its directory at the head of `sys.path`,
//! so any file the build stages runs this way, whether or not a module name reaches it. An
//! argument with a path separator in it can only be a path, since no module name has one.
//!
//! One name is one file here. Two files that build to the same module — a
//! `main.by` beside a `main.py`, or `main.by` at the project root beside
//! `src/main.by` — are refused by the staging step itself, so neither runs until
//! one is renamed; until then the name is reported for the file the resolver
//! finds first, and the other runs under none.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use ruff_db::files::File;
use ruff_python_ast::PySourceType;
use ty_module_resolver::{ModuleName, file_to_module, resolve_module_confident};
use ty_project::{Db as _, ProjectDatabase};
use ty_python_semantic::Db as _;

use crate::project::{BY_SOURCES, module_roots, project_sources};
use crate::staging::transpiled_destination;
use crate::verbatim::verbatim_destination;

/// The module `by run` executes when none is named: the project's `run.main`.
pub fn configured_main(db: &ProjectDatabase) -> Option<String> {
    let options = db.project().metadata(db).options();
    let main = options.run.as_ref()?.main.as_ref()?;
    Some((**main).clone())
}

/// The file `by run <module>` executes, or `None` when the name resolves to
/// nothing the build stages.
///
/// A package runs as its `__main__`, which is what `runpy` does with one.
pub fn module_file(db: &ProjectDatabase, module: &str) -> Option<File> {
    let name = ModuleName::new(module.trim())?;
    let environment = db.project().program(db).resolver_environment(db);
    let resolved = resolve_module_confident(db, environment, &name)?;
    let resolved = if resolved.kind(db).is_package() {
        let main = ModuleName::new(&format!("{name}.__main__"))?;
        resolve_module_confident(db, environment, &main)?
    } else {
        resolved
    };
    let file = resolved.file(db)?;
    is_staged(db, file).then_some(file)
}

/// The name `by run` is given to execute `file`, or `None` when no name runs it.
///
/// `None` for a file outside every module root the build stages, for a file
/// whose name the resolver finds another file for first, and for a package's
/// `__init__`, since running a package runs its `__main__` instead.
pub fn module_name(db: &ProjectDatabase, file: File) -> Option<String> {
    if !is_staged(db, file) {
        return None;
    }
    let program_file = db.program_file(file);
    let module = file_to_module(db, program_file.resolver_file(db))?;
    if module.kind(db).is_package() {
        return None;
    }
    Some(module.name(db).to_string())
}

/// What runs as `__main__` for an argument of `by run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A module, by its name, which runs as `python -m` runs one.
    Module(String),
    /// A file of the project, which runs as `python path/to/file.py` runs one.
    Script(Script),
}

/// A file of the project that runs as a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// The file, as the project holds it.
    pub file: PathBuf,
    /// Where the build stages it, relative to the tree.
    pub staged: PathBuf,
    /// The path to it `by run` was given, as it was typed, which python keeps for
    /// `sys.argv[0]` and, joined to the working directory, for `__file__`: through a symbolic
    /// link, it names the link rather than the file.
    pub argument: PathBuf,
}

/// Whether `argument` of `by run` is a path rather than a module name: one with a path
/// separator in it, which no module name has.
fn is_path(argument: &str) -> bool {
    argument.chars().any(std::path::is_separator)
}

/// Whether `by run` runs `argument`, typed in `cwd`, as a script rather than a module: whether
/// it is a path, or names a `.by` or `.py` file that is there (see [`run_argument`]).
fn is_script(cwd: &Path, argument: &str) -> bool {
    if is_path(argument) {
        return true;
    }
    let path = cwd.join(argument);
    path.is_file() && PySourceType::try_from_path(&path).is_some_and(PySourceType::is_py_file)
}

/// Why `by run` cannot run `argument`, typed in `cwd`, as python cannot open it, said as python
/// says it, or `None` when it runs as a module or names a file that can be opened.
///
/// This needs nothing of the project, so a path that names no file, or a file that cannot be
/// read, fails before the project is read, checked or staged.
pub fn unopenable(cwd: &Path, argument: &str) -> Option<String> {
    if !is_script(cwd, argument) {
        return None;
    }
    // joined, not resolved: python names the file as it was typed, made absolute
    let path = cwd.join(argument);
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        return Some(format!(
            "`by run` runs a file or a module, and '{}' is a directory",
            path.display()
        ));
    }
    // opened, as python opens it, since a file can be there and still not be readable
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // without waiting for a writer, which a named pipe would wait for
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    match options.open(&path) {
        Ok(_) => None,
        Err(error) => Some(format!(
            "can't open file '{}': {}",
            path.display(),
            python_os_error(&error)
        )),
    }
}

/// `error` as python prints an `OSError`: `[Errno 2] No such file or directory`.
fn python_os_error(error: &io::Error) -> String {
    let Some(code) = error.raw_os_error() else {
        return error.to_string();
    };
    let described = error.to_string();
    let described = described
        .strip_suffix(&format!(" (os error {code})"))
        .unwrap_or(&described);
    format!("[Errno {code}] {described}")
}

/// What `by run` runs for `argument`, as it was typed in `cwd`, out of a tree a project rooted
/// at `root` with the module roots `roots` is staged into at `tree`.
///
/// A path runs the file it names as `python path/to/file.py` does: as `__main__` in no package,
/// with its own directory at the head of `sys.path`, which for `by run` is the directory the
/// build stages it into. An argument is a path when it has a path separator in it, which no
/// module name has, and otherwise only when it names a `.by` or `.py` file that exists. Anything
/// else is a module name, as `python -m` takes one, and is passed on as it was given: `foo.py`
/// naming no file is the module `py` of the package `foo`, as it is to `python -m`.
pub fn run_argument(
    db: &ProjectDatabase,
    root: &Path,
    roots: &[PathBuf],
    tree: &Path,
    cwd: &Path,
    argument: &str,
) -> Result<Entry, UnrunnableFile> {
    if !is_script(cwd, argument) {
        return Ok(Entry::Module(argument.to_owned()));
    }
    let path = cwd.join(argument);
    // canonical, as the project's files are, so a file of the project is found among them
    // however the path to it was written
    let file = dunce::canonicalize(&path).unwrap_or(path);
    let staged =
        staged_destination(db, root, roots, tree, &file).ok_or_else(|| UnrunnableFile {
            path: PathBuf::from(argument),
        })?;
    Ok(Entry::Script(Script {
        file,
        staged,
        argument: PathBuf::from(argument),
    }))
}

/// Where the build stages `file`, relative to the tree: the module a `.by` source transpiles to,
/// or the copy of any other file it carries over. `None` for a file it does not stage.
fn staged_destination(
    db: &ProjectDatabase,
    root: &Path,
    roots: &[PathBuf],
    tree: &Path,
    file: &Path,
) -> Option<PathBuf> {
    let is_source = file
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| BY_SOURCES.contains(&extension));
    if !is_source {
        return verbatim_destination(db, root, roots, tree, file);
    }
    project_sources(db, BY_SOURCES, root, None)
        .iter()
        .any(|(source, _)| {
            source == file || dunce::canonicalize(source).is_ok_and(|source| source == file)
        })
        .then(|| transpiled_destination(roots, root, file))
}

/// A file `by run` was given by its path that the build does not stage, so it is not in the
/// tree the program runs out of.
#[derive(Debug)]
pub struct UnrunnableFile {
    /// The path as it was given.
    path: PathBuf,
}

impl fmt::Display for UnrunnableFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not one of the files `by run` stages from the project, so it is not in the \
             tree the program runs out of: it is outside the project, or the project excludes it",
            self.path.display()
        )
    }
}

impl std::error::Error for UnrunnableFile {}

/// Whether `file` lies under a module root `by run` stages.
fn is_staged(db: &ProjectDatabase, file: File) -> bool {
    let Some(path) = file.path(db).as_system_path() else {
        return false;
    };
    let root = Path::new(db.project().root(db).as_str());
    module_roots(db, root)
        .iter()
        .any(|candidate| Path::new(path.as_str()).starts_with(candidate))
}
