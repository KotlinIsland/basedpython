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

use std::path::Path;

use ruff_db::files::File;
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
