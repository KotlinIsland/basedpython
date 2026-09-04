//! Which files a build is made of, and the db it reads them through.
//!
//! One answer to "what is this project", shared by every command that writes a
//! tree. A build that disagreed with `by check` about which files are in the
//! project reports errors for files the project deliberately excludes — and, when
//! this was first written, wrote nothing for them either. So the file set is the
//! project's own (`Project::files`), narrowed only by things a *build* has to
//! exclude and a check does not: a hidden directory holding copies, and the last
//! build's own output.
//!
//! It lives here rather than in the `by` binary because the language server needs
//! the same answers. A single file re-staged into a build tree has to land where
//! the whole-tree build would have put it and be transpiled the way the whole-tree
//! build would have transpiled it, and the only way to guarantee that is for there
//! to be one implementation of each.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ty_project::{Db, ProjectDatabase, ProjectMetadata};

/// Everything needed to build a project db a second time.
///
/// The transpiler asks for one when a pre-pass rewrites the source it hands to
/// phase 0: it then serves the rewritten file out of that db, keeping the
/// project's metadata, search paths and sibling files. The rebuilt db must be
/// independent of the one this command uses — see
/// [`by_transforms::RebuildProject`].
pub struct Rebuilder {
    metadata: ProjectMetadata,
    root: SystemPathBuf,
    included: Vec<SystemPathBuf>,
}

impl Rebuilder {
    /// A rebuilder for the project `db` holds, able to serve any file the
    /// project claims.
    ///
    /// The whole-tree commands narrow `included` to the sources they are about to
    /// emit, because that is also the set they check. A caller that arrived here
    /// holding only a db — the language server, re-staging one file — has no such
    /// list, and an empty one is exactly right: the project reads it as "the root",
    /// which is the whole project. The rebuilt db exists to resolve the file's
    /// *imports*, and narrowing it to the one file under the transpiler would
    /// resolve none of them.
    /// A rebuilder for a project read from `metadata` at `root`, narrowed to the
    /// sources a command is about to emit.
    ///
    /// What the whole-tree commands hold: they resolved the project themselves
    /// and know which files they are checking, so `included` is that set rather
    /// than the whole project. Beside `for_project` rather than in place
    /// of it, because the two callers know genuinely different things — one has a
    /// db and nothing else, the other has the list and has not built a db yet.
    pub fn for_sources(
        metadata: ProjectMetadata,
        root: SystemPathBuf,
        included: Vec<SystemPathBuf>,
    ) -> Self {
        Self {
            metadata,
            root,
            included,
        }
    }

    pub(crate) fn for_project(db: &ProjectDatabase) -> Self {
        Self {
            metadata: db.project().metadata(db).clone(),
            root: db.project().root(db).to_path_buf(),
            included: Vec::new(),
        }
    }

    /// A rebuilder for a project assembled by hand.
    ///
    /// `by transpile <file>` is the caller: it discovers a project around one file
    /// and narrows it to that file, which is neither what [`build_project_db`]
    /// produces nor what `for_project` does. The three ways of arriving at
    /// a rebuilder stay one type, so a transpile reached from any of them resolves
    /// its imports the same way.
    pub fn new(
        metadata: ProjectMetadata,
        root: SystemPathBuf,
        included: Vec<SystemPathBuf>,
    ) -> Self {
        Self {
            metadata,
            root,
            included,
        }
    }

    pub fn rebuild(&self) -> Box<dyn ty_python_semantic::Db> {
        let mut db =
            ProjectDatabase::use_defaults(self.metadata.clone(), OsSystem::new(&self.root));
        db.project()
            .set_included_paths(&mut db, self.included.clone());
        Box::new(db)
    }
}

/// A project db, the `(source_path, File)` pairs for the `.by` files it was
/// built for, the canonical project root every one of those paths is rooted at,
/// and the means to build the same project again.
///
/// The root is handed back rather than re-derived by each caller: `canonicalize`
/// and `current_dir` do not agree on every platform — on windows the first
/// returns the `\\?\` verbatim form and the second does not — so a caller that
/// re-derived it would find none of the db's paths under it.
pub type ProjectBuild = (
    ProjectDatabase,
    Vec<(PathBuf, ruff_db::files::File)>,
    Rebuilder,
    PathBuf,
);

/// the sources `build`, `run` and `transpile` claim: a `.py` beside a `.by`
/// is that file's own output, so writing beside it again would be circular
pub const BY_SOURCES: &[&str] = &["by", "byi"];

/// what `compile` claims. it lowers the `.by` *and* the `.py` ast — one
/// lowering, told apart by [`by_irbuild::Language`] — and emits into an
/// output directory rather than beside the source, so a `.py` is an input
///
/// [`by_irbuild::Language`]: https://docs.rs/by_irbuild
pub const COMPILABLE_SOURCES: &[&str] = &["by", "byi", "py"];

/// non-hidden directories skipped when walking a project (see
/// [`may_contain_sources`]): virtual envs, caches, and build outputs — none
/// are first-party source. hidden directories are skipped wholesale
const NON_SOURCE_DIRS: &[&str] = &[
    ".venv",
    "venv",
    "env",
    ".env",
    "site-packages",
    "__pycache__",
    ".git",
    ".tox",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    "build",
    "dist",
    "node_modules",
    "out",
    // rust's build directory, which a basedpython project has whenever it also
    // has an extension crate — and which the build would otherwise copy in full.
    // a project that really does have a package called `target` can take it back
    // with `exclude = ["!target"]`
    "target",
];

/// The [`NON_SOURCE_DIRS`] entries that ty's own `src.exclude` defaults don't already drop.
///
/// [`is_hidden_within`] runs over files that have *already* passed the project's file filter,
/// so for a name ty excludes by default — `venv`, `dist`, `node_modules`, `.tox`, … — a file
/// can only have reached it because the configuration deliberately re-included the directory
/// with a negated pattern, which `src.exclude` documents as the way to override a default.
/// Re-dropping such a file here would quietly undo that, and it's why a project could not
/// compile a module of its own that happens to live in a directory named `venv`.
///
/// What's left are the names ty has no default opinion about, where this walk is the only
/// thing keeping a dependency tree or a build output out of the emitted set. The unfiltered
/// [`NON_SOURCE_DIRS`] still applies to [`may_contain_sources`], which walks the file system
/// directly and never sees the project configuration at all.
const NON_SOURCE_DIRS_TY_ALLOWS: &[&str] = &[
    "env",
    ".env",
    "site-packages",
    "__pycache__",
    ".pytest_cache",
    "build",
    "out",
    "target",
];

/// Whether `path` sits inside a hidden or build-output directory under `root`.
pub(crate) fn is_hidden_within(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .any(|name| name.starts_with('.') || NON_SOURCE_DIRS_TY_ALLOWS.contains(&name))
}

/// Whether the build walk may descend into this entry.
///
/// Narrower than [`may_contain_sources`] on purpose, for the same reason
/// [`is_hidden_within`] is: this walk applies the project's own `src` and `build`
/// filters as it goes, so everything ty's `src.exclude` defaults already drop is
/// covered — and re-dropping it here would take back a file that a negated
/// exclude deliberately re-included. Only the directories ty's defaults *leave*
/// (and hidden ones) still have to be turned away.
pub(crate) fn may_hold_build_content(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| !name.starts_with('.') && !NON_SOURCE_DIRS_TY_ALLOWS.contains(&name))
}

/// Whether a project walk may descend into this entry: hidden directories
/// (`.claude`, `.git`, `.venv`, …) and `NON_SOURCE_DIRS` never hold
/// first-party source. The walk root itself is always entered, even when the
/// project directory happens to be hidden.
pub fn may_contain_sources(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| !name.starts_with('.') && !NON_SOURCE_DIRS.contains(&name))
}

/// Every file under `root` whose extension is one of `extensions`, skipping
/// non-source directories and symlinks.
pub fn source_files(root: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(may_contain_sources)
        .filter_map(Result::ok)
        .filter(|e| {
            !e.path_is_symlink()
                && e.path()
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|x| extensions.contains(&x))
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Build a project db rooted at `cwd`, returning it alongside the
/// `(source_path, File)` pair for every source the *project* claims whose
/// extension is in `extensions`
/// — the same set `by check` walks, so `src.exclude` and the ignore files it
/// honours apply here too — and the means to build the same project again.
pub fn build_project_db(
    cwd: &Path,
    extensions: &[&str],
    output: Option<&Path>,
) -> anyhow::Result<ProjectBuild> {
    // the project root must be canonicalized the same way the included files
    // are (below) so it stays a path *prefix* of them: otherwise a file's
    // search path isn't recognized as first-party and boundary diagnostics
    // (e.g. `subclass-of-sealed-class`) misfire. this bites on windows, where
    // `canonicalize` rewrites files to the `\\?\` long-path form while an
    // un-canonicalized root keeps its short (`RUNNER~1`) components
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let sys_cwd = SystemPath::from_std_path(&canonical_cwd)
        .with_context(|| format!("non-utf8 path: {}", canonical_cwd.display()))?;
    let system = OsSystem::new(sys_cwd);
    let project_metadata = ProjectMetadata::discover(sys_cwd, &system)
        .with_context(|| format!("failed to discover project at {sys_cwd}"))?;

    // the project is the project wherever the command was run from. rooting this
    // at the working directory instead means `by run` inside `tests/` transpiles
    // `tests/` and nothing else, and then cannot find the module it was asked to
    // run — the same mistake as looking for `.venv` beside the caller rather than
    // beside the project
    let canonical_root = std::fs::canonicalize(project_metadata.root().as_std_path())
        .unwrap_or_else(|_| PathBuf::from(project_metadata.root().as_str()));
    let sys_root = SystemPath::from_std_path(&canonical_root)
        .with_context(|| format!("non-utf8 path: {}", canonical_root.display()))?;

    let metadata = project_metadata.clone();
    let db = ProjectDatabase::use_defaults(project_metadata, system);

    let mut sources = project_sources(&db, extensions, &canonical_root, output);
    // the walk is over a hash set, so order is arbitrary; emit deterministically
    sources.sort_by(|(a, _), (b, _)| a.cmp(b));

    let included: Vec<SystemPathBuf> = sources
        .iter()
        .filter_map(|(path, _)| SystemPath::from_std_path(path).map(SystemPath::to_path_buf))
        .collect();
    let rebuilder = Rebuilder {
        metadata,
        root: sys_root.to_path_buf(),
        included,
    };
    Ok((db, sources, rebuilder, canonical_root))
}

/// The sources a build claims out of a db that already exists.
///
/// The project's own file set — the one `by check` walks, so `src.exclude` and
/// the ignore files it honours apply here too. A build that disagreed with the
/// check about which files are in the project reports errors for files the
/// project deliberately excludes, and (before this) wrote nothing.
///
/// Separate from [`build_project_db`] so that a caller holding a db it did not
/// build — the language server's, which is warm and must not be rebuilt — asks
/// the same question and gets the same answer.
pub(crate) fn project_sources(
    db: &ProjectDatabase,
    extensions: &[&str],
    root: &Path,
    output: Option<&Path>,
) -> Vec<(PathBuf, ruff_db::files::File)> {
    db.project()
        .files(db)
        .into_iter()
        .filter(|file| {
            file.path(db)
                .extension()
                .is_some_and(|x| extensions.contains(&x))
        })
        .filter_map(|file| {
            let path = file.path(db).as_system_path()?;
            Some((path.as_std_path().to_path_buf(), file))
        })
        // a hidden directory (`.claude/worktrees`, `.venv`, …) holds copies and
        // dependencies, not this project's sources — emitting them would write
        // a parallel tree nobody asked for
        .filter(|(path, _)| !is_hidden_within(path, root))
        // nor is the last build's output. it holds a copy of every `.by` source
        // this build is about to read, and reading those instead would build the
        // project into itself, one directory deeper each time
        .filter(|(path, _)| output.is_none_or(|output| !path.starts_with(output)))
        .collect()
}

/// The project's first-party module roots, longest first, as absolute paths.
///
/// These are the directories a module name is resolved against — for a
/// src-layout project, `src/` before the project root. Only roots inside the
/// project are kept: an emitted tree can only mirror what is being built.
pub fn module_roots(db: &ProjectDatabase, cwd: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ty_module_resolver::system_module_search_paths(
        db,
        db.project().program(db).resolver_environment(db),
    )
    .map(|path| PathBuf::from(path.as_str()))
    .filter(|path| path.starts_with(cwd))
    .collect();
    // a nested root shadows the one containing it, so the deepest match wins
    roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    roots
}

#[cfg(test)]
mod tests {
    use super::is_hidden_within;
    use std::path::Path;

    #[test]
    fn a_hidden_directory_is_not_project_source() {
        let root = Path::new("/p");
        assert!(is_hidden_within(
            Path::new("/p/.claude/worktrees/x/junk.by"),
            root
        ));
        assert!(is_hidden_within(Path::new("/p/out/main.by"), root));
        assert!(!is_hidden_within(Path::new("/p/src/pkg/main.by"), root));
        // the file's own name is not a directory component
        assert!(!is_hidden_within(Path::new("/p/.hidden.by"), root));
        // a name ty excludes by default is left to the project filter, so that a
        // negated `src.exclude` pattern re-including it isn't quietly undone here
        assert!(!is_hidden_within(Path::new("/p/venv/__init__.by"), root));
        assert!(!is_hidden_within(Path::new("/p/dist/main.by"), root));
    }
}
