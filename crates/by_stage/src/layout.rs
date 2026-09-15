//! Where a project's build writes, and which file in it came from which source.
//!
//! An editor needs this for everything that relates a `.by` to what it becomes: opening the
//! generated module, going back from a generated module to its source, keeping the output out of
//! the editor's own index, telling a test runner not to collect a second copy of every test. Each of
//! those used to be an editor's guess — `out/<path relative to the project>.py` — and every part of
//! that guess went wrong: the directory is `build`, and the path inside it follows the *module*
//! tree, not the directory tree (see [`crate::staging::transpiled_destination`]). The build is the
//! only thing that knows, so this is the build's answer, asked of the same database and the same
//! file set a build reads.

use std::path::{Path, PathBuf};

use ty_project::{Db as _, ProjectDatabase};

use crate::project::{BY_SOURCES, module_roots, project_sources};
use crate::staging::transpiled_destination;

/// The directory `by build` and `by compile` write to when they are not given `--out`, relative
/// to the project root — see [`default_build_directory`].
const DEFAULT_OUTPUT_DIRECTORY: &str = "build";

/// Where the project rooted at `project_root` is built when no `--out` names somewhere else.
///
/// Relative to the project, not to wherever a command happens to run: the output belongs to the
/// project the way its sources and its `.venv` do, so `by build` in `tests/` writes the same tree
/// as `by build` at the root, and the one place an editor is told the build writes is the place
/// it does. An explicit `--out` is the caller's own path and stays relative to where they typed it.
pub fn default_build_directory(project_root: &Path) -> PathBuf {
    project_root.join(DEFAULT_OUTPUT_DIRECTORY)
}

/// A path's place in its project's build.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildOutput {
    /// The project root: the project a `by build` run anywhere inside it builds.
    project_root: PathBuf,

    /// Where `by build`, run anywhere in the project with no `--out`, writes the project.
    build_directory: PathBuf,

    /// When the path is a `.by` or `.byi` the build is made of, the file it is written to.
    generated: Option<PathBuf>,

    /// When the path is a file in [`Self::build_directory`] that a source is written to, that
    /// source.
    source: Option<PathBuf>,
}

/// Where `path` stands in the build of the project `db` holds.
///
/// Answers for any path, including one that does not exist yet — a generated file before the first
/// build is still the place that build will write. Neither side of the mapping is checked against
/// the disk.
pub fn build_output(db: &ProjectDatabase, path: &Path) -> BuildOutput {
    let declared_root = db.project().root(db).as_std_path();
    let root = canonical(declared_root);
    let build_directory = default_build_directory(&root);
    let roots = module_roots(db, &root);
    let sources = project_sources(db, BY_SOURCES, &root, Some(&build_directory));
    let wanted = canonical(path);

    // resolved once for the whole sweep. both lookups below compare against every source, and
    // `canonical` walks a path making a syscall per level, so resolving inside them would ask
    // the file system the same questions once per source per lookup
    let resolved: Vec<(PathBuf, PathBuf)> = sources
        .iter()
        .map(|(source, _)| {
            let source = canonical(source);
            let destination = transpiled_destination(&roots, &root, &source);
            (source, destination)
        })
        .collect();

    let generated = resolved
        .iter()
        .find(|(source, _)| *source == wanted)
        .map(|(_, destination)| build_directory.join(destination));

    let source = wanted
        .strip_prefix(&build_directory)
        .ok()
        .and_then(|relative| {
            resolved
                .iter()
                .find(|(_, destination)| destination == relative)
                .map(|(source, _)| source.clone())
        });

    BuildOutput {
        project_root: root,
        build_directory,
        generated,
        source,
    }
}

/// `path` with its links resolved, or as given when it cannot be — a path that does not exist yet is
/// resolved through its nearest parent that does, so it still compares equal to the resolved form.
///
/// On Windows resolving answers with a verbatim `\\?\C:\…` path, which is dropped back to the
/// `C:\…` spelling a client sends and a client can open.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return dunce::simplified(&resolved).to_path_buf();
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}
