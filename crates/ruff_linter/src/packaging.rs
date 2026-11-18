//! Detect Python package roots and file associations.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use ruff_python_stdlib::identifiers::is_identifier;
use rustc_hash::FxHashMap;

use crate::basedpython_project::{BasedpythonProject, Projects};
use crate::settings::LinterSettings;

// If we have a Python package layout like:
// - root/
//   - foo/
//     - __init__.py
//     - bar.py
//     - baz/
//       - __init__.py
//       - qux.py
//
// Then today, if you run with defaults (`src = ["."]`) from `root`, we'll
// detect that `foo.bar`, `foo.baz`, and `foo.baz.qux` are first-party modules
// (since, if you're in `root`, you can see `foo`).
//
// However, we'd also like it to be the case that, even if you run this command
// from `foo`, we still consider `foo.baz.qux` to be first-party when linting
// `foo/bar.py`. More specifically, for each Python file, we should find the
// root of the current package.
//
// Thus, for each file, we iterate up its ancestors, returning the last
// directory containing an `__init__.py`.

/// the file system packages and modules are looked up in: the host's, or the view of it another
/// program keeps, as a language server keeps one of its workspace
pub trait PackageFiles {
    /// whether `path` is a file
    fn is_file(&self, path: &Path) -> bool;

    /// whether `path` is a directory
    fn is_directory(&self, path: &Path) -> bool;

    /// the entries of the directory at `path`, or none when it is not a directory that can be
    /// read. an entry whose name is not UTF-8, which no module can have, may be left out
    fn read_directory(&self, path: &Path) -> Vec<DirectoryEntry>;

    /// the contents of the file at `path`, or `None` when it is not a UTF-8 file that can be read
    fn read_to_string(&self, path: &Path) -> Option<String>;
}

/// an entry of a directory, as [`PackageFiles::read_directory`] lists it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: EntryKind,
}

/// what a directory entry is, a symlink not followed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// the host's file system
#[derive(Debug, Clone, Copy, Default)]
pub struct HostPackageFiles;

impl PackageFiles for HostPackageFiles {
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn is_directory(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn read_directory(&self, path: &Path) -> Vec<DirectoryEntry> {
        let Ok(entries) = std::fs::read_dir(path) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                let kind = if file_type.is_symlink() {
                    EntryKind::Symlink
                } else if file_type.is_dir() {
                    EntryKind::Directory
                } else if file_type.is_file() {
                    EntryKind::File
                } else {
                    return None;
                };
                Some(DirectoryEntry {
                    name: entry.file_name().into_string().ok()?,
                    kind,
                })
            })
            .collect()
    }

    fn read_to_string(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

/// the names a package's initializer is looked for under
pub(crate) const PACKAGE_INITIALIZERS: [&str; 4] =
    ["__init__.py", "__init__.pyi", "__init__.by", "__init__.byi"];

/// the file a basedpython build writes at the top of its output, marking the directory as one
const BUILD_MANIFEST: &str = ".by-manifest";

/// what was found out about the directories asked about: whether each holds a basedpython module,
/// and the basedpython project each is in, kept so that a tree is listed, and each project in it
/// read, once however many of the directories in it are asked about
#[derive(Debug, Default)]
pub struct PackageLookups {
    listings: RefCell<FxHashMap<PathBuf, bool>>,
    projects: Projects,
}

/// Return `true` if the directory at the given `Path` appears to be a Python
/// package.
pub fn is_package(path: &Path, settings: &LinterSettings, lookups: &PackageLookups) -> bool {
    is_package_in(&HostPackageFiles, path, settings, lookups)
}

/// whether the directory at `path` of `files` is a python package
///
/// a directory the basedpython project declares a namespace package counts as one, as a
/// directory in the linter's own `namespace-packages` does
fn is_package_in(
    files: &dyn PackageFiles,
    path: &Path,
    settings: &LinterSettings,
    lookups: &PackageLookups,
) -> bool {
    settings
        .namespace_packages
        .iter()
        .any(|namespace_package| path.starts_with(namespace_package))
        || PACKAGE_INITIALIZERS
            .into_iter()
            .any(|init| files.is_file(&path.join(init)))
        || lookups
            .projects
            .containing(files, path)
            .is_some_and(|project| {
                project
                    .namespace_packages
                    .iter()
                    .any(|namespace_package| path.starts_with(namespace_package))
                    || is_package_without_initializer(files, path, &project, lookups)
            })
}

/// whether `directory`, below a package root of the basedpython `project` it is in, is a package
/// as the basedpython build makes it, writing an `__init__.py` into each directory there that
/// holds a basedpython module
///
/// only a directory every module in it can be imported through is one: each directory from the
/// root down to it has an identifier for a name
fn is_package_without_initializer(
    files: &dyn PackageFiles,
    directory: &Path,
    project: &BasedpythonProject,
    lookups: &PackageLookups,
) -> bool {
    // the build lays a directory out below the deepest module root holding it, so that root has
    // to be a package root. a root is never a package itself: the deepest root holding it is
    // itself
    let Some(root) = project
        .module_roots
        .iter()
        .filter(|root| directory.starts_with(root))
        .max_by_key(|root| root.components().count())
    else {
        return false;
    };
    if root == directory || !project.package_roots.contains(root) {
        return false;
    }
    let Ok(relative) = directory.strip_prefix(root) else {
        return false;
    };
    relative.components().all(|component| {
        matches!(component, Component::Normal(name) if name.to_str().is_some_and(is_identifier))
    }) && holds_module(files, directory, Initializer::Uncounted, lookups)
}

/// whether a directory's own `__init__.by` counts as one of the modules it holds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Initializer {
    Counted,
    Uncounted,
}

/// whether `directory` holds a basedpython module at any depth
///
/// only directories with an identifier for a name are looked into, as a module in any other
/// can't be imported through this one. a build's output, which holds a copy of every source it
/// built, is not looked into, and neither is a symlinked directory, which can point back up the
/// tree
fn holds_module(
    files: &dyn PackageFiles,
    directory: &Path,
    initializer: Initializer,
    lookups: &PackageLookups,
) -> bool {
    let entries = files.read_directory(directory);
    if entries.iter().any(|entry| entry.name == BUILD_MANIFEST) {
        return false;
    }
    let mut subdirectories = Vec::new();
    for entry in &entries {
        match entry.kind {
            EntryKind::File | EntryKind::Symlink => {
                let name = Path::new(&entry.name);
                if name
                    .extension()
                    .is_some_and(|extension| extension == "by" || extension == "byi")
                    && name
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .is_some_and(|stem| {
                            is_identifier(stem)
                                && (initializer == Initializer::Counted || stem != "__init__")
                        })
                    && (entry.kind == EntryKind::File
                        || files.is_file(&directory.join(&entry.name)))
                {
                    return true;
                }
            }
            EntryKind::Directory => {
                if is_identifier(&entry.name) {
                    subdirectories.push(directory.join(&entry.name));
                }
            }
        }
    }
    subdirectories
        .iter()
        .any(|subdirectory| holds_any_module(files, subdirectory, lookups))
}

/// [`holds_module`] counting every module, which below the directory asked about includes a
/// subpackage's initializer, remembered in `lookups`
fn holds_any_module(files: &dyn PackageFiles, directory: &Path, lookups: &PackageLookups) -> bool {
    if let Some(&holds) = lookups.listings.borrow().get(directory) {
        return holds;
    }
    let holds = holds_module(files, directory, Initializer::Counted, lookups);
    lookups
        .listings
        .borrow_mut()
        .insert(directory.to_path_buf(), holds);
    holds
}

/// Return the package root for the given path to a directory with Python file.
pub fn detect_package_root<'a>(path: &'a Path, settings: &LinterSettings) -> Option<&'a Path> {
    detect_package_root_in(&HostPackageFiles, path, settings)
}

/// the package root of the directory at `path` of `files`, as [`detect_package_root`] finds it
pub fn detect_package_root_in<'a>(
    files: &dyn PackageFiles,
    path: &'a Path,
    settings: &LinterSettings,
) -> Option<&'a Path> {
    let lookups = PackageLookups::default();
    let mut current = None;
    for parent in path.ancestors() {
        if !is_package_in(files, parent, settings, &lookups) {
            return current;
        }
        current = Some(parent);
    }
    current
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use tempfile::TempDir;

    use crate::packaging::{PackageLookups, detect_package_root, is_package};
    use crate::settings::LinterSettings;
    use crate::test::test_resource_path;

    fn settings(namespace_packages: &[PathBuf]) -> LinterSettings {
        LinterSettings {
            namespace_packages: namespace_packages.to_vec(),
            ..LinterSettings::default()
        }
    }

    /// a basedpython project in a temporary directory, configured by `configuration` and holding
    /// each of `files`, empty. it has the default module roots, `src` and the project root, and
    /// package root, `src`, unless `configuration` says otherwise
    fn project_with(configuration: &str, files: &[&str]) -> Result<TempDir> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("basedpython.toml"), configuration)?;
        for file in files {
            let path = root.path().join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, "")?;
        }
        Ok(root)
    }

    fn tree(files: &[&str]) -> Result<TempDir> {
        project_with("", files)
    }

    fn is_package_at(root: &TempDir, directory: &str) -> bool {
        is_package(
            &root.path().join(directory),
            &LinterSettings::default(),
            &PackageLookups::default(),
        )
    }

    #[test]
    fn package_detection() {
        assert_eq!(
            detect_package_root(&test_resource_path("package/src/package"), &settings(&[])),
            Some(test_resource_path("package/src/package").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &settings(&[])
            ),
            Some(test_resource_path("project/python_modules/core/core").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/examples/docs/docs/concepts"),
                &settings(&[]),
            ),
            Some(test_resource_path("project/examples/docs/docs").as_path())
        );

        assert_eq!(
            detect_package_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("setup.py")
                    .as_path(),
                &settings(&[]),
            ),
            None,
        );
    }

    #[test]
    fn package_detection_with_namespace_packages() {
        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &settings(&[])
            ),
            Some(test_resource_path("project/python_modules/core/core").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &settings(&[test_resource_path("project/python_modules/core"),]),
            ),
            Some(test_resource_path("project/python_modules/core").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &settings(&[
                    test_resource_path("project/python_modules/core"),
                    test_resource_path("project/python_modules"),
                ]),
            ),
            Some(test_resource_path("project/python_modules").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &settings(&[test_resource_path("project/python_modules"),]),
            ),
            Some(test_resource_path("project/python_modules").as_path())
        );
    }

    #[test]
    fn package_without_initializer() -> Result<()> {
        let root = tree(&["src/pkg/a.by", "src/pkg/sub/b.byi"])?;
        assert_eq!(
            detect_package_root(&root.path().join("src/pkg/sub"), &settings(&[])),
            Some(root.path().join("src/pkg").as_path())
        );
        assert!(!is_package_at(&root, "src"));
        Ok(())
    }

    #[test]
    fn package_without_initializer_holding_modules_only_in_a_subdirectory() -> Result<()> {
        let root = tree(&["src/pkg/sub/b.by"])?;
        assert!(is_package_at(&root, "src/pkg"));
        Ok(())
    }

    #[test]
    fn package_without_initializer_holding_a_subpackage_initializer() -> Result<()> {
        let root = tree(&["src/pkg/sub/__init__.by"])?;
        assert!(is_package_at(&root, "src/pkg"));
        Ok(())
    }

    #[test]
    fn initializer_stub_only() -> Result<()> {
        let root = tree(&["src/pkg/__init__.byi"])?;
        assert!(is_package_at(&root, "src/pkg"));
        Ok(())
    }

    #[test]
    fn package_without_initializer_outside_package_roots() -> Result<()> {
        let root = tree(&["tools/a.by"])?;
        assert_eq!(
            detect_package_root(&root.path().join("tools"), &settings(&[])),
            None
        );
        Ok(())
    }

    #[test]
    fn python_module_without_initializer() -> Result<()> {
        let root = tree(&["src/pkg/a.py"])?;
        assert!(!is_package_at(&root, "src/pkg"));
        Ok(())
    }

    #[test]
    fn non_identifier_names_without_initializer() -> Result<()> {
        let root = tree(&[
            "src/my-pkg/a.by",
            "src/my-pkg/pkg/a.by",
            "src/module/my-module.by",
            "src/directory/my-directory/a.by",
        ])?;
        assert!(!is_package_at(&root, "src/my-pkg"));
        assert!(!is_package_at(&root, "src/my-pkg/pkg"));
        assert!(!is_package_at(&root, "src/module"));
        assert!(!is_package_at(&root, "src/directory"));
        Ok(())
    }

    #[test]
    fn build_output_without_initializer() -> Result<()> {
        let root = tree(&[
            "src/output/.by-manifest",
            "src/output/a.by",
            "src/pkg/build/.by-manifest",
            "src/pkg/build/a.by",
        ])?;
        assert!(!is_package_at(&root, "src/output"));
        assert!(!is_package_at(&root, "src/pkg"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_without_initializer() -> Result<()> {
        let root = tree(&["other/a.by", "src/directory/.keep", "src/file/.keep"])?;
        std::os::unix::fs::symlink(
            root.path().join("other"),
            root.path().join("src/directory/link"),
        )?;
        std::os::unix::fs::symlink(
            root.path().join("other/a.by"),
            root.path().join("src/file/a.by"),
        )?;
        assert!(!is_package_at(&root, "src/directory"));
        assert!(is_package_at(&root, "src/file"));
        Ok(())
    }

    #[test]
    fn nested_package_roots_without_initializer() -> Result<()> {
        let root = project_with(
            "[environment]\nroot = [\"src\", \"src/lib\"]\npackage-roots = [\"src\", \"src/lib\"]\n",
            &["src/lib/pkg/a.by"],
        )?;
        assert_eq!(
            detect_package_root(&root.path().join("src/lib/pkg"), &settings(&[])),
            Some(root.path().join("src/lib/pkg").as_path())
        );
        Ok(())
    }

    #[test]
    fn package_without_initializer_below_a_configured_module_root() -> Result<()> {
        let root = project_with(
            "[environment]\nroot = [\"app\"]\npackage-roots = [\"app\"]\n",
            &["app/pkg/a.by", "src/pkg/a.by"],
        )?;
        assert!(is_package_at(&root, "app/pkg"));
        assert!(!is_package_at(&root, "app"));
        // `src` is no module root of this project
        assert!(!is_package_at(&root, "src/pkg"));
        Ok(())
    }

    #[test]
    fn package_root_that_is_no_module_root() -> Result<()> {
        // `src` is a package, and so no module root: below the project root, the module root
        // holding it, no directory is a package without an initializer
        let root = tree(&["src/__init__.by", "src/pkg/a.by"])?;
        assert!(!is_package_at(&root, "src/pkg"));
        Ok(())
    }

    #[test]
    fn declared_namespace_package() -> Result<()> {
        let root = project_with(
            "[environment]\nnamespace-packages = [\"src/ns\"]\n",
            &["src/ns/a.py", "src/ns/sub/b.py"],
        )?;
        // a namespace package the project declares is an intended one, as one in the linter's
        // own `namespace-packages` is
        assert!(is_package_at(&root, "src/ns"));
        assert!(is_package_at(&root, "src/ns/sub"));
        assert_eq!(
            detect_package_root(&root.path().join("src/ns/sub"), &settings(&[])),
            Some(root.path().join("src/ns").as_path())
        );
        Ok(())
    }
}
