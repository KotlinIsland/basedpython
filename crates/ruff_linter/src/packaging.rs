//! Detect Python package roots and file associations.

use std::path::{Path, PathBuf};

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
}

/// Return `true` if the directory at the given `Path` appears to be a Python
/// package.
pub fn is_package(path: &Path, namespace_packages: &[PathBuf]) -> bool {
    is_package_in(&HostPackageFiles, path, namespace_packages)
}

/// whether the directory at `path` of `files` is a python package
fn is_package_in(files: &dyn PackageFiles, path: &Path, namespace_packages: &[PathBuf]) -> bool {
    namespace_packages
        .iter()
        .any(|namespace_package| path.starts_with(namespace_package))
        || ["__init__.py", "__init__.pyi", "__init__.by", "__init__.byi"]
            .into_iter()
            .any(|init| files.is_file(&path.join(init)))
}

/// Return the package root for the given path to a directory with Python file.
pub fn detect_package_root<'a>(path: &'a Path, namespace_packages: &[PathBuf]) -> Option<&'a Path> {
    detect_package_root_in(&HostPackageFiles, path, namespace_packages)
}

/// the package root of the directory at `path` of `files`, as [`detect_package_root`] finds it
pub fn detect_package_root_in<'a>(
    files: &dyn PackageFiles,
    path: &'a Path,
    namespace_packages: &[PathBuf],
) -> Option<&'a Path> {
    let mut current = None;
    for parent in path.ancestors() {
        if !is_package_in(files, parent, namespace_packages) {
            return current;
        }
        current = Some(parent);
    }
    current
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::packaging::detect_package_root;
    use crate::test::test_resource_path;

    #[test]
    fn package_detection() {
        assert_eq!(
            detect_package_root(&test_resource_path("package/src/package"), &[],),
            Some(test_resource_path("package/src/package").as_path())
        );

        assert_eq!(
            detect_package_root(&test_resource_path("project/python_modules/core/core"), &[],),
            Some(test_resource_path("project/python_modules/core/core").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/examples/docs/docs/concepts"),
                &[],
            ),
            Some(test_resource_path("project/examples/docs/docs").as_path())
        );

        assert_eq!(
            detect_package_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("setup.py")
                    .as_path(),
                &[],
            ),
            None,
        );
    }

    #[test]
    fn package_detection_with_namespace_packages() {
        assert_eq!(
            detect_package_root(&test_resource_path("project/python_modules/core/core"), &[],),
            Some(test_resource_path("project/python_modules/core/core").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &[test_resource_path("project/python_modules/core"),],
            ),
            Some(test_resource_path("project/python_modules/core").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &[
                    test_resource_path("project/python_modules/core"),
                    test_resource_path("project/python_modules"),
                ],
            ),
            Some(test_resource_path("project/python_modules").as_path())
        );

        assert_eq!(
            detect_package_root(
                &test_resource_path("project/python_modules/core/core"),
                &[test_resource_path("project/python_modules"),],
            ),
            Some(test_resource_path("project/python_modules").as_path())
        );
    }
}
