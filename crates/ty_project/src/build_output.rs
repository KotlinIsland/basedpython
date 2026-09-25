//! the trees `by build` and `by compile` write, which are not part of the project they were built
//! from
//!
//! a build output holds a copy of the project: a `.py` for every `.by`, every hand-written `.py`
//! carried over verbatim, and the `.by` sources too when `build.sources` is on. taken for part of
//! the project, each of those would be checked a second time, and every diagnostic in a source
//! reported again against its copy, at a path the author never edits
//!
//! an output is recognised by the manifest the build leaves at its top, not by its name. the
//! default output is `build/` in the project root, but `--out` can name any directory, and a
//! project can have several; each one describes itself. a directory the author happens to call
//! `build` holds no manifest, so it stays part of the project wherever the build writes
//!
//! the exclusion applies where the project's files are discovered (the walk, and the file
//! watcher's changes) the way an ignore file's does, so that `by check`, the language server's
//! workspace diagnostics, and everything else that reads [`crate::Project::files`] agree. it
//! does not make the output unreadable: module resolution reads the file system, not the
//! project's file list, and a generated file an editor opens is still a file it can ask about

use ruff_db::system::{System, SystemPath, SystemPathBuf};

/// the file a build writes at the top of its output, recording what it wrote there, and, by
/// being there, marking the directory as an output
pub const BUILD_MANIFEST: &str = ".by-manifest";

/// whether `directory` is the top of a build output
pub fn is_build_output(system: &dyn System, directory: &SystemPath) -> bool {
    system.is_file(&directory.join(BUILD_MANIFEST))
}

/// whether `path` lies inside a build output below the walk root that holds it
///
/// what the walk decides for a path it reaches from one of `walk_roots` (the project root, or
/// the paths a command line named), decided for one path on its own, as the file watcher's
/// changes need. a walk root is never itself skipped: the project root is never an output,
/// whatever it holds, and a directory a command line names is checked because it was asked for,
/// output or not. only the directories between the walk root and `path` are asked
pub(crate) fn is_within_build_output(
    system: &dyn System,
    path: &SystemPath,
    walk_roots: &[SystemPathBuf],
) -> bool {
    let Some(walk_root) = innermost_walk_root(path, walk_roots) else {
        return false;
    };
    path.ancestors()
        .skip(1)
        .take_while(|directory| *directory != walk_root)
        .any(|directory| is_build_output(system, directory))
}

/// whether `path` is the manifest of a build output inside one of `walk_roots`: its appearing or
/// disappearing changes which of the files under its directory belong to the project
pub(crate) fn is_build_manifest(path: &SystemPath, walk_roots: &[SystemPathBuf]) -> bool {
    path.file_name() == Some(BUILD_MANIFEST)
        && path.parent().is_some_and(|directory| {
            innermost_walk_root(directory, walk_roots)
                .is_some_and(|walk_root| directory != walk_root)
        })
}

/// the walk root nearest above `path`, or `path` itself if it is one
///
/// the walk starts again from each of its roots, one nested in another included, so a path is
/// reached from every root above it, and from the innermost one through the fewest directories.
/// a path any of the walks keeps, that one keeps
fn innermost_walk_root<'a>(
    path: &SystemPath,
    walk_roots: &'a [SystemPathBuf],
) -> Option<&'a SystemPath> {
    walk_roots
        .iter()
        .filter(|walk_root| path.starts_with(walk_root))
        .max_by_key(|walk_root| walk_root.components().count())
        .map(SystemPathBuf::as_path)
}

#[cfg(test)]
mod tests {
    use ruff_db::system::{SystemPath, SystemPathBuf, TestSystem};

    use super::{BUILD_MANIFEST, is_build_manifest, is_within_build_output};

    fn system_with(files: &[&str]) -> TestSystem {
        let system = TestSystem::default();
        for file in files {
            system
                .memory_file_system()
                .write_file_all(file, "")
                .expect("an in-memory write succeeds");
        }
        system
    }

    fn roots(paths: &[&str]) -> Vec<SystemPathBuf> {
        paths
            .iter()
            .map(|path| SystemPathBuf::from(*path))
            .collect()
    }

    /// `--out` can name any directory, and a directory the author calls `build` is theirs until
    /// a build writes a manifest into it
    #[test]
    fn an_output_is_recognised_by_its_manifest_not_its_name() {
        let system = system_with(&["/p/anything/.by-manifest", "/p/build/tool.py"]);
        let walk_roots = roots(&["/p"]);
        assert!(is_within_build_output(
            &system,
            SystemPath::new("/p/anything/a.py"),
            &walk_roots
        ));
        assert!(
            is_within_build_output(
                &system,
                SystemPath::new("/p/anything/pkg/a.py"),
                &walk_roots
            ),
            "a file deeper inside the output is inside it too"
        );
        assert!(!is_within_build_output(
            &system,
            SystemPath::new("/p/build/tool.py"),
            &walk_roots
        ));
    }

    /// a manifest left at the root would otherwise empty the project of every file, and a
    /// directory named on the command line is checked because it was asked for
    #[test]
    fn a_walk_root_is_never_itself_an_output() {
        let system = system_with(&["/p/.by-manifest", "/p/build/.by-manifest"]);
        assert!(!is_within_build_output(
            &system,
            SystemPath::new("/p/a.py"),
            &roots(&["/p"])
        ));
        assert!(!is_within_build_output(
            &system,
            SystemPath::new("/p/build/a.py"),
            &roots(&["/p/build"])
        ));
        assert!(!is_build_manifest(
            &SystemPath::new("/p").join(BUILD_MANIFEST),
            &roots(&["/p"])
        ));
        assert!(is_build_manifest(
            &SystemPath::new("/p/build").join(BUILD_MANIFEST),
            &roots(&["/p"])
        ));

        // named inside another walk root, it is still a root of its own: the walk from it keeps
        // what the walk from the root above skips
        let nested = roots(&["/p", "/p/build"]);
        assert!(!is_within_build_output(
            &system,
            SystemPath::new("/p/build/a.py"),
            &nested
        ));
        assert!(!is_build_manifest(
            &SystemPath::new("/p/build").join(BUILD_MANIFEST),
            &nested
        ));
    }
}
