//! Writing a project out as python.
//!
//! `by build` and `by run` both need the same thing: the project, rendered as a
//! directory python can import. That is more than the transpiled `.by` files. A
//! project is also its hand-written `.py` modules, its `py.typed` marker, its
//! templates and json and fixture data — and a tree holding only the transpiled
//! half is not a project at all. A module that imports a `.py` sibling fails to
//! import, and anything that opens a data file relative to the working directory
//! fails to open it.
//!
//! So the output tree mirrors the project: every file is carried over to the same
//! relative place, `.by` sources being the ones that change on the way (they are
//! transpiled, and, when `build.sources` is on, carried over as well so a
//! downstream basedpython project can read them). The one rearrangement is the
//! module roots: a src-layout project's `src/pkg/a.by` is the module `pkg.a`, so
//! it lands at `pkg/a.py`, not `src/pkg/a.py`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// The name of the file that records what the last build wrote.
///
/// A build that only ever adds files leaves the output tree accumulating modules
/// that were deleted from the source months ago. They keep importing, so nothing
/// locally ever notices — until a wheel built from the same tree ships them, or
/// until one shadows a module that moved. The manifest is what makes the output a
/// mirror rather than a pile: what the previous build wrote and this one did not
/// is deleted.
pub(crate) const MANIFEST_FILENAME: &str = ".by-manifest";

/// An output tree being written.
pub(crate) struct Staging {
    out: PathBuf,
    /// relative destination -> the source it came from, for collision reporting
    written: BTreeMap<PathBuf, Option<PathBuf>>,
}

impl Staging {
    pub(crate) fn new(out: &Path) -> Self {
        Self {
            out: out.to_path_buf(),
            written: BTreeMap::new(),
        }
    }

    pub(crate) fn out(&self) -> &Path {
        &self.out
    }

    /// Every file the build read, in sorted order and without duplicates.
    ///
    /// A `.by` appears once even though it produced two outputs — the python it
    /// was transpiled into and the copy of itself carried alongside.
    pub(crate) fn inputs(&self) -> BTreeSet<&Path> {
        self.written
            .values()
            .filter_map(|source| source.as_deref())
            .collect()
    }

    /// Every written path paired with the file it came from.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&Path, Option<&Path>)> {
        self.written
            .iter()
            .map(|(destination, source)| (destination.as_path(), source.as_deref()))
    }

    /// Write `contents` to `relative`, recording it as produced from `source`.
    ///
    /// Two sources landing on one destination is an error rather than a
    /// last-writer-wins overwrite: `a.by` and a hand-written `a.py` are both the
    /// module `a`, and quietly picking one means the build disagrees with what
    /// python will import.
    pub(crate) fn write(
        &mut self,
        relative: &Path,
        source: Option<&Path>,
        contents: &str,
    ) -> anyhow::Result<()> {
        self.claim(relative, source)?;
        let destination = self.out.join(relative);
        create_parent(&destination)?;
        fs::write(&destination, contents)
            .with_context(|| format!("could not write {}", destination.display()))
    }

    /// Copy `source` to `relative` verbatim.
    pub(crate) fn copy(&mut self, relative: &Path, source: &Path) -> anyhow::Result<()> {
        self.claim(relative, Some(source))?;
        let destination = self.out.join(relative);
        create_parent(&destination)?;
        fs::copy(source, &destination).with_context(|| {
            format!(
                "could not copy {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(())
    }

    fn claim(&mut self, relative: &Path, source: Option<&Path>) -> anyhow::Result<()> {
        if let Some((previous, Some(previous_source))) = self.written.get_key_value(relative)
            && Some(previous_source.as_path()) != source
        {
            let claimant = source.map_or_else(
                || "the build".to_owned(),
                |source| format!("`{}`", source.display()),
            );
            anyhow::bail!(
                "`{}` and {claimant} both build to `{}` — \
                 they are the same module, so one of them has to be renamed",
                previous_source.display(),
                previous.display(),
            );
        }
        self.written
            .insert(relative.to_path_buf(), source.map(Path::to_path_buf));
        Ok(())
    }

    /// Delete what the previous build wrote and this one did not, then record
    /// what this one wrote.
    pub(crate) fn finish(self) -> anyhow::Result<()> {
        let manifest = self.out.join(MANIFEST_FILENAME);
        let previous = read_manifest(&manifest);
        let current: BTreeSet<&Path> = self.written.keys().map(PathBuf::as_path).collect();

        let mut emptied: BTreeSet<PathBuf> = BTreeSet::new();
        for stale in &previous {
            if current.contains(stale.as_path()) {
                continue;
            }
            let path = self.out.join(stale);
            // a file the user deleted from the output themselves is already in
            // the state we want, so a missing file is not an error
            let _ = fs::remove_file(&path);
            let mut parent = path.parent();
            while let Some(directory) = parent {
                if directory == self.out {
                    break;
                }
                emptied.insert(directory.to_path_buf());
                parent = directory.parent();
            }
        }
        // deepest first, so a directory whose only content was other now-removed
        // directories is itself removed
        for directory in emptied.iter().rev() {
            let _ = fs::remove_dir(directory);
        }

        create_parent(&manifest)?;
        let mut rendered =
            String::from("# written by `by build`; delete it and stale output stays\n");
        for path in current {
            rendered.push_str(&portable(path));
            rendered.push('\n');
        }
        fs::write(&manifest, rendered)
            .with_context(|| format!("could not write {}", manifest.display()))
    }
}

fn create_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    Ok(())
}

fn read_manifest(path: &Path) -> BTreeSet<PathBuf> {
    let Ok(contents) = fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        // the manifest is written with `/` separators so an output tree stays
        // readable on the platform that did not write it; a path built from those
        // components is native either way
        .map(|line| line.split('/').collect::<PathBuf>())
        .collect()
}

/// A relative path with `/` separators, whatever the platform.
fn portable(path: &Path) -> String {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Where `source`'s output goes, relative to the output root.
///
/// The tree mirrored is the *module* tree, not the directory tree: a src-layout
/// project's `src/pkg/main.by` is the module `pkg.main`, so it has to land at
/// `pkg/main.py`. Mirroring the directory instead emits `src/pkg/main.py`, whose
/// module is `src.pkg.main` — a name nothing imports, and one `run.main` cannot
/// sensibly be set to. A file outside every module root keeps its place relative
/// to the project.
pub(crate) fn relative_destination(roots: &[PathBuf], root: &Path, source: &Path) -> PathBuf {
    let relative = roots
        .iter()
        .find_map(|candidate| source.strip_prefix(candidate).ok())
        .or_else(|| source.strip_prefix(root).ok())
        .unwrap_or(source);
    // whatever happened above, the result has to be *relative*: joined onto the
    // output directory an absolute path replaces it outright, so every emitted
    // file would land outside the output tree. keeping only the named components
    // also drops any `..`, which would climb back out of it
    relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

/// What a transpiled source is called in the output.
///
/// A stub stays a stub: `.byi` transpiles to `.pyi`, not to `.py`. Emitting a
/// stub as a module would put a body-less definition where python expects the
/// implementation, and it would shadow the real module at runtime.
pub(crate) fn transpiled_destination(roots: &[PathBuf], root: &Path, source: &Path) -> PathBuf {
    let extension = match source.extension().and_then(std::ffi::OsStr::to_str) {
        Some("byi") => "pyi",
        _ => "py",
    };
    relative_destination(roots, root, source).with_extension(extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_module_root_is_stripped() {
        let destination = transpiled_destination(
            &roots(&["/p/src"]),
            Path::new("/p"),
            Path::new("/p/src/pkg/main.by"),
        );
        assert_eq!(destination, PathBuf::from("pkg/main.py"));
    }

    #[test]
    fn the_deepest_root_wins() {
        let destination = transpiled_destination(
            &roots(&["/p/src/inner", "/p/src"]),
            Path::new("/p"),
            Path::new("/p/src/inner/pkg/main.by"),
        );
        assert_eq!(destination, PathBuf::from("pkg/main.py"));
    }

    #[test]
    fn a_file_outside_every_root_keeps_its_place() {
        let destination = relative_destination(
            &roots(&["/p/src"]),
            Path::new("/p"),
            Path::new("/p/assets/logo.svg"),
        );
        assert_eq!(destination, PathBuf::from("assets/logo.svg"));
    }

    /// a root that shares no prefix with the file — which is what `canonicalize`
    /// and `current_dir` disagreeing produced on windows — must not leave the
    /// path absolute: joining that onto the output directory discards the output
    /// directory entirely, so every emitted file lands outside it
    #[test]
    fn a_root_that_does_not_match_still_yields_a_relative_path() {
        let destination = transpiled_destination(
            &roots(&["/other/src"]),
            Path::new("/other"),
            Path::new("/p/pkg/main.by"),
        );
        assert!(
            destination.is_relative(),
            "an absolute result escapes the output directory: {}",
            destination.display()
        );
    }

    /// a stub is not a module: transpiling `a.byi` to `a.py` would put a
    /// body-less definition where python imports the implementation
    #[test]
    fn a_stub_transpiles_to_a_stub() {
        let destination =
            transpiled_destination(&roots(&["/p"]), Path::new("/p"), Path::new("/p/a.byi"));
        assert_eq!(destination, PathBuf::from("a.pyi"));
    }

    #[test]
    fn two_sources_claiming_one_destination_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut staging = Staging::new(directory.path());
        staging
            .write(Path::new("a.py"), Some(Path::new("/p/a.by")), "x = 1\n")
            .expect("the first write succeeds");
        let error = staging
            .write(Path::new("a.py"), Some(Path::new("/p/a.py")), "x = 2\n")
            .expect_err("the second source collides");
        let message = error.to_string();
        assert!(message.contains("a.by"), "{message}");
        assert!(message.contains("a.py"), "{message}");
    }

    /// the same source rewriting its own destination is not a collision — that is
    /// just a rebuild
    #[test]
    fn one_source_may_claim_its_destination_twice() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut staging = Staging::new(directory.path());
        let source = PathBuf::from("/p/a.by");
        staging
            .write(Path::new("a.py"), Some(&source), "x = 1\n")
            .expect("the first write succeeds");
        staging
            .write(Path::new("a.py"), Some(&source), "x = 2\n")
            .expect("rewriting from the same source is fine");
    }

    #[test]
    fn what_the_previous_build_wrote_and_this_one_did_not_is_deleted() {
        let directory = tempfile::tempdir().expect("tempdir");

        let mut first = Staging::new(directory.path());
        first
            .write(Path::new("kept.py"), None, "x = 1\n")
            .expect("write");
        first
            .write(Path::new("pkg/gone.py"), None, "x = 1\n")
            .expect("write");
        first.finish().expect("finish");
        assert!(directory.path().join("pkg/gone.py").exists());

        let mut second = Staging::new(directory.path());
        second
            .write(Path::new("kept.py"), None, "x = 1\n")
            .expect("write");
        second.finish().expect("finish");

        assert!(directory.path().join("kept.py").exists());
        assert!(
            !directory.path().join("pkg/gone.py").exists(),
            "a module the source no longer has must not survive in the output"
        );
        assert!(
            !directory.path().join("pkg").exists(),
            "the directory it was the only content of goes with it"
        );
    }

    /// only what the build itself wrote is ever deleted. anything else in the
    /// output directory was put there by someone, and a build is not entitled to
    /// remove it
    #[test]
    fn a_file_the_build_never_wrote_is_left_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("theirs.txt"), "hands off").expect("write");

        let mut staging = Staging::new(directory.path());
        staging
            .write(Path::new("mine.py"), None, "x = 1\n")
            .expect("write");
        staging.finish().expect("finish");

        Staging::new(directory.path()).finish().expect("finish");

        assert!(directory.path().join("theirs.txt").exists());
        assert!(!directory.path().join("mine.py").exists());
    }

    #[test]
    fn a_manifest_round_trips_through_its_portable_form() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut staging = Staging::new(directory.path());
        staging
            .write(&PathBuf::from("pkg").join("deep").join("a.py"), None, "")
            .expect("write");
        staging.finish().expect("finish");

        let manifest = read_manifest(&directory.path().join(MANIFEST_FILENAME));
        assert!(manifest.contains(&PathBuf::from("pkg").join("deep").join("a.py")));
        let rendered = fs::read_to_string(directory.path().join(MANIFEST_FILENAME)).expect("read");
        assert!(
            rendered.contains("pkg/deep/a.py"),
            "the manifest is written with `/` separators:\n{rendered}"
        );
    }
}
