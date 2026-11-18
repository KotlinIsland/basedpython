//! the basedpython project a directory is in, found the way `by` finds it
//!
//! whether a directory holding a basedpython module is a package without an `__init__.by` is a
//! property of the project: the type checker decides it from the project's module roots and
//! package roots, and the build writes the initializers that make it so. the linter reads them
//! from the project's own configuration, whichever configuration of its own it found
//!
//! the user-level basedpython configuration (`basedpython/basedpython.toml` or `ty/ty.toml` in the
//! user's configuration directory) is not read: the settings read here belong to a project, and a
//! user-level file that set them would make the same tree lay out differently for each user

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustc_hash::FxHashMap;
use serde::Deserialize;

use crate::fs::normalize_path_to;
use crate::packaging::{PACKAGE_INITIALIZERS, PackageFiles};

/// the files a project's options are read from, ahead of a `pyproject.toml`, in decreasing
/// precedence
const CONFIGURATION_FILES: [&str; 2] = ["basedpython.toml", "ty.toml"];

/// the package root of a project that names none
const DEFAULT_PACKAGE_ROOT: &str = "src";

/// what a basedpython project says about which of its directories are packages, its paths
/// absolute
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BasedpythonProject {
    /// the directories modules are imported from, `environment.root`
    pub(crate) module_roots: Vec<PathBuf>,
    /// the module roots below which a directory holding a basedpython module is a package,
    /// `environment.package-roots`
    pub(crate) package_roots: Vec<PathBuf>,
    /// the directories below a package root that stay namespace packages,
    /// `environment.namespace-packages`
    pub(crate) namespace_packages: Vec<PathBuf>,
}

/// the projects found while linting, kept so that each directory is looked in once however many
/// of the directories below it are asked about
#[derive(Debug, Default)]
pub(crate) struct Projects {
    /// the project configured in each directory looked in, `None` for one configuring none
    configured_in: RefCell<FxHashMap<PathBuf, Option<FoundProject>>>,
    /// the project each directory asked about is in, `None` for one in no project
    containing: RefCell<FxHashMap<PathBuf, Option<Rc<BasedpythonProject>>>>,
}

#[derive(Debug, Clone)]
struct FoundProject {
    /// whether the project configures basedpython, rather than being only a `pyproject.toml`
    configures_basedpython: bool,
    project: Rc<BasedpythonProject>,
}

impl Projects {
    /// the project `directory` is in, as `ProjectMetadata::discover` finds it: the closest
    /// directory at or above it with a `basedpython.toml`, a `ty.toml` or a `pyproject.toml` with
    /// a `[tool.basedpython]` or `[tool.ty]` table, or else the closest with any `pyproject.toml`
    ///
    /// `by` falls back to a project rooted at the directory it was started in, which is no help
    /// here: every module root of a project rooted at `directory` is `directory` or below it, so
    /// none of them makes `directory` a package. so that is `None`
    pub(crate) fn containing(
        &self,
        files: &dyn PackageFiles,
        directory: &Path,
    ) -> Option<Rc<BasedpythonProject>> {
        if let Some(project) = self.containing.borrow().get(directory) {
            return project.clone();
        }
        let mut closest = None;
        let mut project = None;
        for ancestor in directory.ancestors() {
            let Some(found) = self.configured_in(files, ancestor) else {
                continue;
            };
            if found.configures_basedpython {
                project = Some(found.project);
                break;
            }
            closest.get_or_insert(found.project);
        }
        let project = project.or(closest);
        self.containing
            .borrow_mut()
            .insert(directory.to_path_buf(), project.clone());
        project
    }

    fn configured_in(&self, files: &dyn PackageFiles, directory: &Path) -> Option<FoundProject> {
        if let Some(found) = self.configured_in.borrow().get(directory) {
            return found.clone();
        }
        let found = read_project(files, directory);
        self.configured_in
            .borrow_mut()
            .insert(directory.to_path_buf(), found.clone());
        found
    }
}

/// the project configured in `directory`, as `ProjectMetadata::discover_in` reads it
///
/// a file that can't be read is not there, as for `by`. one that can't be parsed stops `by` with
/// an error, so the project it is in is found here as one declaring nothing, rather than as the
/// project above it
fn read_project(files: &dyn PackageFiles, directory: &Path) -> Option<FoundProject> {
    let pyproject = files
        .read_to_string(&directory.join("pyproject.toml"))
        .map(|contents| Pyproject::parse(&contents));
    let configuration_file = CONFIGURATION_FILES
        .iter()
        .find_map(|name| files.read_to_string(&directory.join(name)));

    let (configures_basedpython, name, environment) = match (pyproject, configuration_file) {
        (None, None) => return None,
        (Some(Err(())), _) => (true, None, Environment::default()),
        // a configuration file takes precedence over the `pyproject.toml`, which still names the
        // project
        (pyproject, Some(contents)) => (
            true,
            pyproject
                .and_then(Result::ok)
                .and_then(|pyproject| pyproject.name),
            toml::from_str::<Options>(&contents)
                .ok()
                .and_then(|options| options.environment)
                .unwrap_or_default(),
        ),
        (Some(Ok(pyproject)), None) => (
            pyproject.configures_basedpython,
            pyproject.name,
            pyproject.environment,
        ),
    };
    let name = name.unwrap_or_else(|| {
        directory
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("root")
            .to_string()
    });
    Some(FoundProject {
        configures_basedpython,
        project: Rc::new(BasedpythonProject::new(
            files,
            directory,
            &name,
            environment,
        )),
    })
}

impl BasedpythonProject {
    /// the project rooted at `root`, named `name`, with the `environment` options it configures
    fn new(files: &dyn PackageFiles, root: &Path, name: &str, environment: Environment) -> Self {
        let resolve_all = |paths: Option<Vec<String>>| {
            paths.map(|paths| {
                paths
                    .iter()
                    .map(|path| resolve(path, root))
                    .collect::<Vec<_>>()
            })
        };
        Self {
            module_roots: resolve_all(environment.root)
                .unwrap_or_else(|| default_module_roots(files, root, name)),
            package_roots: resolve_all(environment.package_roots)
                .unwrap_or_else(|| vec![root.join(DEFAULT_PACKAGE_ROOT)]),
            namespace_packages: resolve_all(environment.namespace_packages).unwrap_or_default(),
        }
    }
}

/// the module roots of a project at `root` that configures none, as `Options::to_search_paths`
/// detects them: a `src`, `<project name>` or `python` directory that is not a package and holds
/// the project's code, then the root
fn default_module_roots(files: &dyn PackageFiles, root: &Path, name: &str) -> Vec<PathBuf> {
    let is_package = |directory: &Path| {
        PACKAGE_INITIALIZERS
            .iter()
            .any(|initializer| files.is_file(&directory.join(initializer)))
    };
    let mut roots = Vec::new();
    let src = root.join("src");
    if files.is_directory(&src) && !is_package(&src) {
        roots.push(src);
    }
    let named = root.join(name);
    if files.is_directory(&named.join(name)) && !is_package(&named) && !roots.contains(&named) {
        roots.push(named);
    }
    let python = root.join("python");
    if files.is_directory(&python) && !is_package(&python) && !roots.contains(&python) {
        roots.push(python);
    }
    roots.push(root.to_path_buf());
    roots
}

/// `path` as configured in a file in `root`, with `~` and environment variables expanded, as
/// `RelativePathBuf::absolute` resolves it
fn resolve(path: &str, root: &Path) -> PathBuf {
    let expanded = shellexpand::full(path).map_or_else(|_| path.into(), Cow::into_owned);
    normalize_path_to(Path::new(&expanded), root)
}

/// what the linter reads of a `pyproject.toml`
struct Pyproject {
    /// the normalized `project.name`
    name: Option<String>,
    /// whether it has a `[tool.basedpython]` or `[tool.ty]` table
    configures_basedpython: bool,
    /// both tables' `environment` options, a list in both joined
    environment: Environment,
}

impl Pyproject {
    fn parse(contents: &str) -> Result<Self, ()> {
        #[derive(Deserialize)]
        struct File {
            project: Option<Project>,
            tool: Option<Tools>,
        }

        #[derive(Deserialize)]
        struct Project {
            name: Option<String>,
        }

        #[derive(Deserialize)]
        struct Tools {
            basedpython: Option<Options>,
            ty: Option<Options>,
        }

        let file: File = toml::from_str(contents).map_err(|_| ())?;
        let name = file
            .project
            .and_then(|project| project.name)
            .map(|name| normalize_package_name(&name).ok_or(()))
            .transpose()?;
        let (basedpython, ty) = file
            .tool
            .map_or((None, None), |tool| (tool.basedpython, tool.ty));
        let configures_basedpython = basedpython.is_some() || ty.is_some();
        let environment = Environment::join(
            basedpython
                .and_then(|options| options.environment)
                .unwrap_or_default(),
            ty.and_then(|options| options.environment)
                .unwrap_or_default(),
        );
        Ok(Self {
            name,
            configures_basedpython,
            environment,
        })
    }
}

/// a project name as `by` reads it from `project.name`: lowercase, with each run of `-`, `_` and
/// `.` one `-`, or `None` for one that is not a valid name
fn normalize_package_name(name: &str) -> Option<String> {
    if name.is_empty()
        || name.starts_with(['-', '_', '.'])
        || name.ends_with(['-', '_', '.'])
        || !name.is_ascii()
    {
        return None;
    }
    let mut normalized = String::with_capacity(name.len());
    let mut last = None;
    for character in name.chars() {
        if matches!(character, '-' | '_' | '.') {
            if !matches!(last, Some('-' | '_' | '.')) {
                normalized.push('-');
            }
        } else {
            normalized.push(character.to_ascii_lowercase());
        }
        last = Some(character);
    }
    Some(normalized)
}

/// the part of a basedpython configuration the linter reads, every other option ignored
#[derive(Deserialize)]
struct Options {
    environment: Option<Environment>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct Environment {
    root: Option<Vec<String>>,
    package_roots: Option<Vec<String>>,
    namespace_packages: Option<Vec<String>>,
}

impl Environment {
    /// the options of a `[tool.basedpython]` and a `[tool.ty]` table, a list set in both joined
    fn join(basedpython: Self, ty: Self) -> Self {
        fn join(basedpython: Option<Vec<String>>, ty: Option<Vec<String>>) -> Option<Vec<String>> {
            match (basedpython, ty) {
                (Some(mut basedpython), Some(ty)) => {
                    basedpython.extend(ty);
                    Some(basedpython)
                }
                (basedpython, ty) => basedpython.or(ty),
            }
        }
        Self {
            root: join(basedpython.root, ty.root),
            package_roots: join(basedpython.package_roots, ty.package_roots),
            namespace_packages: join(basedpython.namespace_packages, ty.namespace_packages),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use tempfile::TempDir;

    use super::{BasedpythonProject, Projects, normalize_package_name};
    use crate::packaging::HostPackageFiles;

    /// a temporary directory holding each of `files`, with its contents
    fn tree(files: &[(&str, &str)]) -> Result<TempDir> {
        let root = tempfile::tempdir()?;
        for (path, contents) in files {
            let path = root.path().join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, contents)?;
        }
        Ok(root)
    }

    fn project_of(root: &TempDir, directory: &str) -> Option<BasedpythonProject> {
        let directory = root.path().join(directory);
        std::fs::create_dir_all(&directory).ok()?;
        Projects::default()
            .containing(&HostPackageFiles, &directory)
            .map(|project| (*project).clone())
    }

    fn paths(root: &TempDir, paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(|path| root.path().join(path)).collect()
    }

    #[test]
    fn a_basedpython_toml_marks_a_project() -> Result<()> {
        let root = tree(&[("basedpython.toml", ""), ("src/pkg/a.by", "")])?;
        let project = project_of(&root, "src/pkg").expect("a project");
        assert_eq!(project.module_roots, paths(&root, &["src", ""]));
        assert_eq!(project.package_roots, paths(&root, &["src"]));
        assert_eq!(project.namespace_packages, Vec::<PathBuf>::new());
        Ok(())
    }

    #[test]
    fn a_ty_toml_marks_a_project() -> Result<()> {
        let root = tree(&[("ty.toml", "[environment]\npackage-roots = [\".\"]\n")])?;
        let project = project_of(&root, "lib").expect("a project");
        assert_eq!(project.package_roots, paths(&root, &[""]));
        Ok(())
    }

    #[test]
    fn a_pyproject_with_either_table_marks_a_project() -> Result<()> {
        for table in ["[tool.basedpython]", "[tool.ty]"] {
            let root = tree(&[
                ("pyproject.toml", &format!("{table}\n")),
                ("inner/pyproject.toml", "[project]\nname = \"inner\"\n"),
            ])?;
            // the closer `pyproject.toml` configures no basedpython, so the one above wins
            let project = project_of(&root, "inner/src/pkg").expect("a project");
            assert_eq!(project.package_roots, paths(&root, &["src"]), "{table}");
        }
        Ok(())
    }

    #[test]
    fn a_pyproject_without_either_table_is_the_fallback() -> Result<()> {
        let root = tree(&[
            ("pyproject.toml", "[project]\nname = \"outer\"\n"),
            ("inner/pyproject.toml", "[project]\nname = \"inner\"\n"),
        ])?;
        let project = project_of(&root, "inner/lib").expect("a project");
        assert_eq!(project.package_roots, paths(&root, &["inner/src"]));
        Ok(())
    }

    #[test]
    fn no_configuration_is_no_project() -> Result<()> {
        let root = tree(&[("src/pkg/a.by", "")])?;
        // a temporary directory with a configuration above it would make this test meaningless
        if Projects::default()
            .containing(&HostPackageFiles, root.path())
            .is_none()
        {
            assert_eq!(project_of(&root, "src/pkg"), None);
        }
        Ok(())
    }

    #[test]
    fn a_configuration_file_takes_precedence_over_the_pyproject() -> Result<()> {
        let root = tree(&[
            (
                "basedpython.toml",
                "[environment]\npackage-roots = [\"a\"]\n",
            ),
            ("ty.toml", "[environment]\npackage-roots = [\"b\"]\n"),
            (
                "pyproject.toml",
                "[tool.basedpython.environment]\npackage-roots = [\"c\"]\n",
            ),
        ])?;
        assert_eq!(
            project_of(&root, "").expect("a project").package_roots,
            paths(&root, &["a"])
        );
        Ok(())
    }

    #[test]
    fn lists_in_both_pyproject_tables_are_joined() -> Result<()> {
        let root = tree(&[(
            "pyproject.toml",
            "[tool.basedpython.environment]\npackage-roots = [\".\"]\nroot = [\".\"]\n\
             [tool.ty.environment]\npackage-roots = [\"lib\"]\nroot = [\"lib\"]\n\
             namespace-packages = [\"lib/ns\"]\n",
        )])?;
        let project = project_of(&root, "").expect("a project");
        assert_eq!(project.package_roots, paths(&root, &["", "lib"]));
        assert_eq!(project.module_roots, paths(&root, &["", "lib"]));
        assert_eq!(project.namespace_packages, paths(&root, &["lib/ns"]));
        Ok(())
    }

    #[test]
    fn the_closest_project_wins() -> Result<()> {
        let root = tree(&[
            (
                "pyproject.toml",
                "[tool.basedpython.environment]\npackage-roots = [\".\"]\n",
            ),
            ("inner/basedpython.toml", ""),
        ])?;
        assert_eq!(
            project_of(&root, "inner/src")
                .expect("a project")
                .package_roots,
            paths(&root, &["inner/src"])
        );
        assert_eq!(
            project_of(&root, "outer").expect("a project").package_roots,
            paths(&root, &[""])
        );
        Ok(())
    }

    #[test]
    fn configured_paths_are_resolved_against_the_project() -> Result<()> {
        let root = tree(&[(
            "basedpython.toml",
            "[environment]\nroot = [\"./app\", \"lib/../other\", \"$HOME/x\"]\n",
        )])?;
        let project = project_of(&root, "").expect("a project");
        let mut expected = paths(&root, &["app", "other"]);
        expected.push(match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home).join("x"),
            // an unset variable leaves the path as written
            Err(_) => root.path().join("$HOME/x"),
        });
        assert_eq!(project.module_roots, expected);
        Ok(())
    }

    #[test]
    fn default_module_roots() -> Result<()> {
        let root = tree(&[
            (
                "pyproject.toml",
                "[project]\nname = \"My_App\"\n[tool.ty]\n",
            ),
            ("src/a.by", ""),
            ("my-app/my-app/a.by", ""),
            ("python/a.by", ""),
        ])?;
        assert_eq!(
            project_of(&root, "").expect("a project").module_roots,
            paths(&root, &["src", "my-app", "python", ""])
        );
        Ok(())
    }

    #[test]
    fn a_package_is_no_default_module_root() -> Result<()> {
        let root = tree(&[
            ("basedpython.toml", ""),
            ("src/__init__.by", ""),
            ("python/__init__.pyi", ""),
        ])?;
        let project = project_of(&root, "").expect("a project");
        assert_eq!(project.module_roots, paths(&root, &[""]));
        // the package root stays `src`, which is no module root, and so lays nothing out
        assert_eq!(project.package_roots, paths(&root, &["src"]));
        Ok(())
    }

    #[test]
    fn the_directory_name_names_a_project_without_one() -> Result<()> {
        let root = tree(&[("app/basedpython.toml", ""), ("app/app/app/a.by", "")])?;
        assert_eq!(
            project_of(&root, "app").expect("a project").module_roots,
            paths(&root, &["app/app", "app"])
        );
        Ok(())
    }

    #[test]
    fn an_unparsable_configuration_declares_nothing() -> Result<()> {
        for files in [
            [
                (
                    "pyproject.toml",
                    "[tool.basedpython.environment]\npackage-roots = [\".\"]\n",
                ),
                ("inner/basedpython.toml", "[environment\n"),
            ],
            [
                (
                    "pyproject.toml",
                    "[tool.basedpython.environment]\npackage-roots = [\".\"]\n",
                ),
                ("inner/pyproject.toml", "[project]\nname = \"-invalid\"\n"),
            ],
            [
                (
                    "pyproject.toml",
                    "[tool.basedpython.environment]\npackage-roots = [\".\"]\n",
                ),
                ("inner/ty.toml", "[environment]\npackage-roots = \".\"\n"),
            ],
        ] {
            let root = tree(&files)?;
            assert_eq!(
                project_of(&root, "inner").expect("a project").package_roots,
                paths(&root, &["inner/src"]),
                "{files:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn package_names_are_normalized() {
        assert_eq!(normalize_package_name("My__App.x"), Some("my-app-x".into()));
        assert_eq!(normalize_package_name("app"), Some("app".into()));
        assert_eq!(normalize_package_name("_app"), None);
        assert_eq!(normalize_package_name("app-"), None);
        assert_eq!(normalize_package_name("äpp"), None);
        assert_eq!(normalize_package_name(""), None);
    }
}
