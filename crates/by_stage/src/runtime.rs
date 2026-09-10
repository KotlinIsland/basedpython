//! where a build puts the runtime helpers, and which copy each module imports
//!
//! # one copy per package, not one at the root
//!
//! a distribution ships packages — directories with an `__init__` — so each
//! package carries a copy of its own. a copy at the tree root would be a
//! top-level module: a distribution that ships root modules would ship it as one,
//! and a second basedpython wheel, built by another version whose helpers differ,
//! overwrites it on install
//!
//! # a module in no package
//!
//! so a module at a module root gets the definitions pasted in. so does a module
//! in a directory that is not a package — a `scripts/` folder, a namespace
//! package — which has no import that works both when it is run as a script and
//! when it is imported
//!
//! # the import is absolute
//!
//! `from pkg._by_runtime import …` rather than `from ._by_runtime import …`: a
//! relative import fails in a module run as `__main__`, and a project's entry
//! point is exactly that

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use ruff_db::system::{System, SystemPath};

/// the copies a build writes
///
/// claimed for every module rather than only those that turned out to call a
/// helper: `by restage` recomputes one module against a tree an earlier build
/// wrote, and an edit that newly reaches for a helper has to find the copy there
#[derive(Debug, Default)]
pub struct RuntimeLayout {
    /// each package holding a copy
    packages: BTreeSet<String>,
}

impl RuntimeLayout {
    /// record that `source`, landing at `relative` in the module tree, needs the
    /// runtime, and answer the module it imports it from — or `None` for a module
    /// in no package, which gets the definitions pasted in
    pub(crate) fn claim(
        &mut self,
        relative: &Path,
        source: &Path,
        system: &dyn System,
    ) -> Option<String> {
        let mut components = relative.components();
        let first = components.next()?;
        // a `relative` of one component is the module's own file name: it sits at
        // a module root, in no package
        components.next()?;
        let Component::Normal(package) = first else {
            return None;
        };
        let package = package.to_str()?;
        if !is_package(&module_tree_base(relative, source).join(package), system) {
            return None;
        }
        self.packages.insert(package.to_owned());
        Some(format!("{package}.{}", by_transforms::runtime::MODULE_NAME))
    }

    /// where each copy goes within the module tree
    pub fn files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.packages
            .iter()
            .map(|package| Path::new(package).join(by_transforms::runtime::FILE_NAME))
    }
}

/// the directory `relative` is relative to: `source` with as many trailing
/// components dropped as `relative` has. `/p/src/pkg/a.by` laid out at
/// `pkg/a.py` is rooted at `/p/src`
fn module_tree_base(relative: &Path, source: &Path) -> PathBuf {
    source
        .ancestors()
        .nth(relative.components().count())
        .unwrap_or(Path::new(""))
        .to_path_buf()
}

/// whether `directory` is a regular package, the kind a distribution ships
fn is_package(directory: &Path, system: &dyn System) -> bool {
    let Some(directory) = SystemPath::from_std_path(directory) else {
        return false;
    };
    ["__init__.py", "__init__.pyi", "__init__.by", "__init__.byi"]
        .iter()
        .any(|init| system.is_file(&directory.join(init)))
}

#[cfg(test)]
mod tests {
    use ruff_db::system::OsSystem;

    use super::*;

    struct Project {
        root: tempfile::TempDir,
        system: OsSystem,
    }

    impl Project {
        fn with(files: &[&str]) -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            for file in files {
                let path = root.path().join(file);
                std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                    .expect("create");
                std::fs::write(&path, "").expect("write");
            }
            let system =
                OsSystem::new(SystemPath::from_std_path(root.path()).expect("utf-8 tempdir"));
            Self { root, system }
        }

        fn claim(&self, layout: &mut RuntimeLayout, relative: &str) -> Option<String> {
            let source = self
                .root
                .path()
                .join("src")
                .join(relative)
                .with_extension("by");
            layout.claim(Path::new(relative), &source, &self.system)
        }
    }

    /// a copy at the root would be a top-level module of its own
    #[test]
    fn a_root_module_gets_the_definitions_pasted_in() {
        let project = Project::with(&["src/main.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(project.claim(&mut layout, "main.py"), None);
        assert_eq!(layout.files().count(), 0);
    }

    #[test]
    fn a_module_in_a_package_imports_that_packages_copy() {
        let project = Project::with(&["src/pkg/__init__.py", "src/pkg/a.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(
            project.claim(&mut layout, "pkg/a.py").as_deref(),
            Some("pkg._by_runtime")
        );
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            vec![PathBuf::from("pkg/_by_runtime.py")]
        );
    }

    /// a package whose `__init__` is itself transpiled is a package all the same
    #[test]
    fn a_by_init_makes_a_package() {
        let project = Project::with(&["src/pkg/__init__.by", "src/pkg/a.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(
            project.claim(&mut layout, "pkg/a.py").as_deref(),
            Some("pkg._by_runtime")
        );
    }

    /// `scripts/tool.py` run as a script has `scripts/` on its path and not the
    /// directory above, so `scripts._by_runtime` would not import
    #[test]
    fn a_module_in_no_package_gets_the_definitions_pasted_in() {
        let project = Project::with(&["src/scripts/tool.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(project.claim(&mut layout, "scripts/tool.py"), None);
        assert_eq!(layout.files().count(), 0);
    }

    /// a subpackage ships as part of the package above it
    #[test]
    fn a_subpackage_shares_the_packages_copy() {
        let project = Project::with(&["src/pkg/__init__.py", "src/pkg/sub/deep/a.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(
            project.claim(&mut layout, "pkg/sub/deep/a.py").as_deref(),
            Some("pkg._by_runtime")
        );
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            vec![PathBuf::from("pkg/_by_runtime.py")]
        );
    }

    #[test]
    fn each_package_carries_its_own() {
        let project = Project::with(&["src/one/__init__.py", "src/two/__init__.py"]);
        let mut layout = RuntimeLayout::default();
        project.claim(&mut layout, "one/a.py");
        project.claim(&mut layout, "two/b.py");
        project.claim(&mut layout, "one/c.py");
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            vec![
                PathBuf::from("one/_by_runtime.py"),
                PathBuf::from("two/_by_runtime.py")
            ]
        );
    }

    #[test]
    fn an_unclaimed_layout_writes_nothing() {
        assert_eq!(RuntimeLayout::default().files().count(), 0);
    }
}
