//! Which file `by run <module>` executes, and which module a file runs as.
//!
//! `by run` stages every first-party module into a tree mirroring the module
//! tree (see [`crate::staging::transpiled_destination`]) and hands the name to
//! `runpy` there. So a name means what the project's resolver makes of it,
//! restricted to the module roots the build stages — and an editor asking "what
//! does `by run app.cli` run" or "what do I pass `by run` to run this file" has
//! to be told that, not a walk of the directory tree that agrees with it most of
//! the time.
//!
//! One name is one file here. Two files that build to the same module — a
//! `main.by` beside a `main.py`, or `main.by` at the project root beside
//! `src/main.by` — are refused by the staging step itself, so neither runs until
//! one is renamed; until then the name is reported for the file the resolver
//! finds first, and the other runs under none.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::SystemPath;
use ruff_python_ast::PySourceType;
use ty_module_resolver::{ModuleName, file_to_module, resolve_module_confident};
use ty_project::{Db as _, ProjectDatabase};
use ty_python_semantic::Db as _;

use crate::project::module_roots;

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
    file_module_name(db, file).ok()
}

/// The name `by run` is given to execute `file`, or why no name runs it.
fn file_module_name(db: &ProjectDatabase, file: File) -> Result<String, Unnamed> {
    if !is_staged(db, file) {
        return Err(Unnamed::Unstaged);
    }
    let program_file = db.program_file(file);
    let module = file_to_module(db, program_file.resolver_file(db)).ok_or(Unnamed::Unresolved)?;
    if module.kind(db).is_package() {
        return Err(Unnamed::PackageInit(module.name(db).to_string()));
    }
    Ok(module.name(db).to_string())
}

/// The module `by run` runs for `argument`, as it was typed in `cwd`.
///
/// A path to a `.by` or `.py` file that exists runs that file, as `python path/to/file.py`
/// does, under the name the project runs it as: `manage.py` runs `manage`, and `app/cli.by`
/// runs `app.cli`, inside its package. Anything else is a module name, as `python -m` takes one,
/// and is passed on as it was given.
pub fn run_argument<'a>(
    db: &ProjectDatabase,
    cwd: &Path,
    argument: &'a str,
) -> Result<Cow<'a, str>, UnrunnableFile> {
    let path = cwd.join(argument);
    if !path.is_file() || !PySourceType::try_from_path(&path).is_some_and(PySourceType::is_py_file)
    {
        return Ok(Cow::Borrowed(argument));
    }
    let unrunnable = |reason| UnrunnableFile {
        path: PathBuf::from(argument),
        reason,
    };
    // canonical, as the project root is, so a file of the project is under its module roots
    // however the path to it was written
    let canonical = dunce::canonicalize(&path).unwrap_or(path);
    let file = SystemPath::from_std_path(&canonical)
        .and_then(|system_path| system_path_to_file(db, system_path).ok())
        .ok_or_else(|| unrunnable(Unnamed::Unstaged))?;
    file_module_name(db, file)
        .map(Cow::Owned)
        .map_err(unrunnable)
}

/// A file `by run` was given by its path that runs under no module name.
#[derive(Debug)]
pub struct UnrunnableFile {
    /// The path as it was given.
    path: PathBuf,
    reason: Unnamed,
}

impl fmt::Display for UnrunnableFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.reason {
            Unnamed::Unstaged => write!(
                f,
                "`{path}` is outside every module root of the project, so no module name runs it"
            ),
            Unnamed::PackageInit(package) => write!(
                f,
                "`{path}` is the `__init__` of the package `{package}`, and running a package \
                 runs its `__main__`, as `by run {package}` does"
            ),
            Unnamed::Unresolved => write!(
                f,
                "no module name of the project resolves to `{path}`: its name is not one a module \
                 can have, or another file builds to the same module"
            ),
        }
    }
}

impl std::error::Error for UnrunnableFile {}

/// Why no module name runs a file.
#[derive(Debug)]
enum Unnamed {
    /// It lies outside every module root the build stages.
    Unstaged,
    /// It is the `__init__` of this package, and running a package runs its `__main__`.
    PackageInit(String),
    /// The resolver finds another file for its name first, or it has no name a module can have.
    Unresolved,
}

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
