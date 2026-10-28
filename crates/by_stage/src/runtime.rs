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
//! # a tree nothing ships
//!
//! a tree `by run` stages is run and thrown away, and nothing installs it, so neither
//! reason holds there. a module in no package imports the copy at the tree's root
//! instead, which the runner's directory on `sys.path` makes importable from every
//! module of the tree. the helpers then run from a file of their own, at its lines,
//! rather than as lines of the module the transpiler wrote with no line of the `.by`
//! behind them, which a traceback and a debugger can only call line 0
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
/// recorded for every module rather than only those that turned out to call a
/// helper: `by restage` recomputes modules against a tree an earlier build
/// wrote, and an edit that newly reaches for a helper has to find the copy there
#[derive(Debug, Default)]
pub struct RuntimeLayout {
    /// each package holding a copy, the tree's root as the empty name
    packages: BTreeSet<String>,
    /// whether a module in no package imports the copy at the tree's root, as in a tree
    /// nothing ships, rather than having the definitions pasted in
    root_copy: bool,
}

impl RuntimeLayout {
    /// the layout of a tree a distribution may be made of: a copy in each package, and the
    /// definitions pasted into a module in none
    pub fn shipped() -> Self {
        Self::default()
    }

    /// the layout of a tree nothing ships, which `by run` stages: a copy in each package, and
    /// one at the tree's root for every module in none
    pub fn unshipped() -> Self {
        Self {
            packages: BTreeSet::new(),
            root_copy: true,
        }
    }

    /// whether this is the layout of a tree nothing ships
    pub(crate) fn is_unshipped(&self) -> bool {
        self.root_copy
    }

    /// the copy the module `source`, landing at `relative` in the module tree, imports the
    /// runtime from — or `None` for one that gets the definitions pasted in
    pub(crate) fn package_of(
        &self,
        relative: &Path,
        source: &Path,
        system: &dyn System,
    ) -> Option<RuntimePackage> {
        RuntimePackage::of(relative, source, system)
            .or_else(|| self.root_copy.then(|| RuntimePackage(String::new())))
    }

    /// record that a module importing `package`'s copy was transpiled, whether or
    /// not the transpile succeeded
    pub(crate) fn record(&mut self, package: &RuntimePackage) {
        self.packages.insert(package.0.clone());
    }

    /// where each copy goes within the module tree
    pub fn files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.packages
            .iter()
            .map(|package| Path::new(package).join(by_transforms::runtime::FILE_NAME))
    }
}

/// the package whose copy of the runtime a module imports
#[derive(Debug)]
pub(crate) struct RuntimePackage(String);

impl RuntimePackage {
    /// the package `source`, landing at `relative` in the module tree, imports the
    /// runtime from — or `None` for a module in no package, which gets the
    /// definitions pasted in
    pub(crate) fn of(relative: &Path, source: &Path, system: &dyn System) -> Option<Self> {
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
        Some(Self(package.to_owned()))
    }

    /// the module the copy is imported as
    pub(crate) fn module(&self) -> String {
        if self.0.is_empty() {
            by_transforms::runtime::MODULE_NAME.to_owned()
        } else {
            format!("{}.{}", self.0, by_transforms::runtime::MODULE_NAME)
        }
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

        fn record(&self, layout: &mut RuntimeLayout, relative: &str) -> Option<String> {
            let source = self
                .root
                .path()
                .join("src")
                .join(relative)
                .with_extension("by");
            let package = layout.package_of(Path::new(relative), &source, &self.system)?;
            layout.record(&package);
            Some(package.module())
        }
    }

    /// a copy at the root would be a top-level module of its own
    #[test]
    fn a_root_module_gets_the_definitions_pasted_in() {
        let project = Project::with(&["src/main.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(project.record(&mut layout, "main.py"), None);
        assert_eq!(layout.files().count(), 0);
    }

    #[test]
    fn a_module_in_a_package_imports_that_packages_copy() {
        let project = Project::with(&["src/pkg/__init__.py", "src/pkg/a.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(
            project.record(&mut layout, "pkg/a.py").as_deref(),
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
            project.record(&mut layout, "pkg/a.py").as_deref(),
            Some("pkg._by_runtime")
        );
    }

    /// `scripts/tool.py` run as a script has `scripts/` on its path and not the
    /// directory above, so `scripts._by_runtime` would not import
    #[test]
    fn a_module_in_no_package_gets_the_definitions_pasted_in() {
        let project = Project::with(&["src/scripts/tool.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(project.record(&mut layout, "scripts/tool.py"), None);
        assert_eq!(layout.files().count(), 0);
    }

    /// a subpackage ships as part of the package above it
    #[test]
    fn a_subpackage_shares_the_packages_copy() {
        let project = Project::with(&["src/pkg/__init__.py", "src/pkg/sub/deep/a.by"]);
        let mut layout = RuntimeLayout::default();
        assert_eq!(
            project.record(&mut layout, "pkg/sub/deep/a.py").as_deref(),
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
        project.record(&mut layout, "one/a.py");
        project.record(&mut layout, "two/b.py");
        project.record(&mut layout, "one/c.py");
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            vec![
                PathBuf::from("one/_by_runtime.py"),
                PathBuf::from("two/_by_runtime.py")
            ]
        );
    }

    /// in a tree nothing ships, a module at a module root imports the copy at the tree's root
    #[test]
    fn an_unshipped_root_module_imports_the_root_copy() {
        let project = Project::with(&["src/main.by"]);
        let mut layout = RuntimeLayout::unshipped();
        assert_eq!(
            project.record(&mut layout, "main.py").as_deref(),
            Some("_by_runtime")
        );
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            vec![PathBuf::from("_by_runtime.py")]
        );
    }

    /// and so does a module in a directory that is not a package, which the runner's
    /// directory on `sys.path` reaches the root copy from all the same
    #[test]
    fn an_unshipped_module_in_no_package_imports_the_root_copy() {
        let project = Project::with(&["src/scripts/tool.by"]);
        let mut layout = RuntimeLayout::unshipped();
        assert_eq!(
            project.record(&mut layout, "scripts/tool.py").as_deref(),
            Some("_by_runtime")
        );
    }

    /// a package keeps its own copy in a tree nothing ships
    #[test]
    fn an_unshipped_package_imports_its_own_copy() {
        let project = Project::with(&["src/pkg/__init__.py", "src/pkg/a.by", "src/main.by"]);
        let mut layout = RuntimeLayout::unshipped();
        project.record(&mut layout, "pkg/a.py");
        project.record(&mut layout, "main.py");
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            vec![
                PathBuf::from("_by_runtime.py"),
                PathBuf::from("pkg/_by_runtime.py")
            ]
        );
    }

    #[test]
    fn an_unrecorded_layout_writes_nothing() {
        assert_eq!(RuntimeLayout::default().files().count(), 0);
    }
}
