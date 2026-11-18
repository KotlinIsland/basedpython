//! what converting python to basedpython does to a project's packages
//!
//! a python directory without an `__init__.py` is a namespace package, while a basedpython
//! directory below a package root holding a module is a regular package whether or not it has
//! an `__init__.by`. so a conversion that left the directories as they are would change what
//! they are. it declares each namespace package it would otherwise turn into a regular one in
//! `environment.namespace-packages`, and it leaves out an empty `__init__.py` from a directory
//! that is a package without it

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use by_stage::project::{build_project_db, module_roots};
use ruff_db::system::SystemPath;
use ruff_python_stdlib::identifiers::is_identifier;
use ty_module_resolver::PACKAGE_INITIALIZERS;
use ty_project::{Db as _, ProjectDatabase};

/// the configuration files a project's options are read from, in the order they are looked for
const CONFIGURATION_FILES: [&str; 2] = ["basedpython.toml", "ty.toml"];

/// the namespace packages a conversion has to declare to keep them namespace packages
pub(crate) struct NamespacePackages {
    /// the root of the project the declarations go into
    project_root: PathBuf,
    /// their paths relative to the project root, as a configuration file spells them, in order
    paths: BTreeSet<String>,
}

impl NamespacePackages {
    /// the namespace packages of the project around `directory` that converting `files`, the
    /// python sources under it, would turn into regular packages
    pub(crate) fn of(directory: &Path, files: &[PathBuf]) -> anyhow::Result<Self> {
        let (db, _, _, project_root) = build_project_db(directory, &["py", "pyi"], None)?;
        let roots = module_roots(&db, &project_root);
        let environment = db.project().program(&*db).resolver_environment(&*db);
        let search_paths = environment.search_paths(&*db);
        let declared = search_paths.namespace_packages();
        let mut paths = BTreeSet::new();
        for file in files {
            let file = dunce::canonicalize(file).unwrap_or_else(|_| file.clone());
            // a directory is a package without an initializer only below a package root, so
            // anywhere else the conversion leaves it what it was
            let Some(root) = roots.iter().find(|root| file.starts_with(root)) else {
                continue;
            };
            if !search_paths
                .package_roots()
                .iter()
                .any(|package_root| package_root.as_std_path() == root.as_path())
            {
                continue;
            }
            // a module python cannot import by a dotted name holds no package up: one whose own
            // name is not an identifier, and one in a directory whose name is not
            let Ok(relative) = file
                .with_extension("")
                .strip_prefix(root)
                .map(Path::to_path_buf)
            else {
                continue;
            };
            if !components(&relative).is_some_and(|components| {
                components.iter().all(|component| is_identifier(component))
            }) {
                continue;
            }
            for package in file.ancestors().skip(1) {
                if package == root.as_path() || !package.starts_with(root) {
                    break;
                }
                let is_declared = SystemPath::from_std_path(package).is_some_and(|package| {
                    declared
                        .iter()
                        .any(|declared| declared.as_path() == package)
                });
                if !is_declared
                    && is_namespace_package(&db, package)
                    && let Some(path) = package
                        .strip_prefix(&project_root)
                        .ok()
                        .and_then(components)
                {
                    paths.insert(path.join("/"));
                }
            }
        }
        Ok(Self {
            project_root,
            paths,
        })
    }

    /// add the packages to the project's `environment.namespace-packages`, in the file its
    /// options are read from, or a new `basedpython.toml` when it has none. returns the file
    pub(crate) fn declare(&self) -> anyhow::Result<Option<PathBuf>> {
        if self.paths.is_empty() {
            return Ok(None);
        }
        let configuration = CONFIGURATION_FILES
            .iter()
            .map(|name| self.project_root.join(name))
            .find(|path| path.is_file());
        let (path, prefix): (PathBuf, &[&str]) = match configuration {
            Some(path) => (path, &[]),
            None => {
                let pyproject = self.project_root.join("pyproject.toml");
                if pyproject.is_file() {
                    (pyproject, &["tool", "basedpython"])
                } else {
                    (self.project_root.join(CONFIGURATION_FILES[0]), &[])
                }
            }
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        let mut document: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("could not parse {}", path.display()))?;

        // a table on the way may be written inline, `environment = { python = ".venv" }`, as
        // well as with a header of its own
        let mut table: &mut dyn toml_edit::TableLike = document.as_table_mut();
        for key in prefix.iter().chain(&["environment"]) {
            table = table
                .entry(key)
                .or_insert_with(|| toml_edit::Item::Table(implicit_table()))
                .as_table_like_mut()
                .with_context(|| format!("`{key}` in {} is not a table", path.display()))?;
        }
        let declared = table
            .entry("namespace-packages")
            .or_insert_with(|| toml_edit::value(toml_edit::Array::new()))
            .as_array_mut()
            .with_context(|| {
                format!(
                    "`environment.namespace-packages` in {} is not a list",
                    path.display()
                )
            })?;
        let already: BTreeSet<String> = declared
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        for path in self.paths.difference(&already) {
            declared.push(path.as_str());
        }

        fs::write(&path, document.to_string())
            .with_context(|| format!("could not write {}", path.display()))?;
        Ok(Some(path))
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(String::as_str)
    }
}

/// the converted package initializers among `converted`, the basedpython files a conversion
/// wrote, that are empty in a directory that is a package without them
///
/// asked of the project as the conversion left it, so a namespace package the conversion
/// declared is one already, and every module it converted counts. only an `__init__.by` is ever
/// left out: a stub-only `__init__.byi` is what says the directory has no initializer at runtime
pub(crate) fn redundant_initializers(
    directory: &Path,
    converted: &[PathBuf],
) -> anyhow::Result<Vec<PathBuf>> {
    let candidates: Vec<&PathBuf> = converted
        .iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| name == "__init__.by")
                && fs::read_to_string(path).is_ok_and(|text| text.trim().is_empty())
        })
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let (db, ..) = build_project_db(directory, &["by", "byi"], None)?;
    let environment = db.project().program(&*db).resolver_environment(&*db);
    Ok(candidates
        .into_iter()
        .filter(|initializer| {
            let Some(package) = initializer.parent() else {
                return false;
            };
            // a directory with a second initializer, a stub beside the source, keeps its
            // initializers as they are
            let initializers = PACKAGE_INITIALIZERS
                .iter()
                .filter(|name| package.join(name).is_file())
                .count();
            let package = dunce::canonicalize(package).unwrap_or_else(|_| package.to_path_buf());
            initializers == 1
                && SystemPath::from_std_path(&package).is_some_and(|package| {
                    ty_module_resolver::is_package_without_initializer_at(
                        &*db,
                        environment,
                        package,
                    )
                })
        })
        .cloned()
        .collect())
}

/// whether `directory` is a namespace package at runtime in the project `db` reads: python runs
/// no initializer of it, and nothing basedpython in it makes it a regular package already. a
/// stub-only `__init__.pyi` makes it a regular package to a type checker alone
fn is_namespace_package(db: &ProjectDatabase, directory: &Path) -> bool {
    if ["__init__.py", "__init__.by"]
        .iter()
        .any(|initializer| directory.join(initializer).is_file())
    {
        return false;
    }
    let Some(directory) = SystemPath::from_std_path(directory) else {
        return false;
    };
    !ty_module_resolver::is_directory_package_at(
        db,
        db.project().program(db).resolver_environment(db),
        directory,
    )
}

/// the names of the components of `relative`
fn components(relative: &Path) -> Option<Vec<&str>> {
    relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect()
}

/// a table written only as the parent of the one below it, as `[tool.basedpython.environment]`
/// is without a `[tool]` header
fn implicit_table() -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    table
}
