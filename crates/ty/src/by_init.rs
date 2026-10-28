//! Starting a project.
//!
//! Everything a basedpython project needs to be installable, checkable and
//! publishable is decided in its `pyproject.toml`, and every one of those
//! decisions is one somebody has to get right before writing a line of code: the
//! build backend, the layout the module tree is read from, the python version the
//! checker and the transpiler both target. `by init` writes them consistently, so
//! that the answer to "how do I ship this" is settled at the point the project is
//! created rather than discovered afterwards.
//!
//! It also sets up git's merges of the project, for a new project or one that is
//! already there: `by merge-driver` as the driver git runs for its python and
//! basedpython files and its `api.lock`. See [`ty_project::git_merge`].

use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use ruff_db::system::{OsSystem, SystemPathBuf};
use ty_project::git_merge::{GitMergeIntegration, invoked_executable};

use crate::ExitStatus;

/// What kind of project is being started.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectKind {
    /// something to run: it gets an entry point, and `by run` alone will run it
    Application,
    /// something to import: no entry point, but the same packaging
    Library,
}

/// Whether to set up git's merges of the project.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitMerge {
    /// `--git-merge`
    Enable,
    /// `--no-git-merge`
    Skip,
    /// neither: asked on a terminal, and not set up otherwise
    Ask,
}

pub(crate) fn cmd_init(
    path: Option<&Path>,
    name: Option<&str>,
    kind: ProjectKind,
    python_version: &str,
    git_merge: GitMerge,
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let root = match path {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => cwd.join(path),
        None => cwd.clone(),
    };

    let pyproject = root.join("pyproject.toml");
    let exists = pyproject.exists();
    let system_root = SystemPathBuf::from_path_buf(root.clone())
        .map_err(|root| anyhow!("`{}` is not a unicode path", root.display()))?;
    // the directory git is asked about: the project's, or for a new one, the nearest that is
    // there, which the project is about to be made in
    let existing_ancestor = system_root
        .ancestors()
        .find(|directory| directory.as_std_path().is_dir())
        .unwrap_or(&system_root)
        .to_path_buf();
    let system = OsSystem::new(&existing_ancestor);
    let integration = GitMergeIntegration::of(&system, &existing_ancestor)?;
    let set_up_git_merge = match git_merge {
        GitMerge::Enable => {
            if !integration.repository {
                bail!(
                    "`{}` is not in a git work tree, so there is no git merge to set up: run \
                     `git init` first",
                    root.display()
                );
            }
            true
        }
        GitMerge::Skip => false,
        GitMerge::Ask => integration.repository && !integration.is_enabled() && ask_git_merge()?,
    };
    if exists && !set_up_git_merge {
        anyhow::bail!(
            "`{}` already exists — `by init` will not write over a project that is already \
             there; `by init --git-merge` sets up git's merges of it",
            pyproject.display()
        );
    }
    if !exists {
        scaffold(&root, name, kind, python_version)?;
    }
    if set_up_git_merge {
        let cwd = SystemPathBuf::from_path_buf(cwd)
            .map_err(|cwd| anyhow!("the current directory `{}` is not unicode", cwd.display()))?;
        let by = invoked_executable(&system, &cwd)?;
        let system = OsSystem::new(&system_root);
        let before = GitMergeIntegration::of(&system, &system_root)?;
        let after = GitMergeIntegration::enable(&system, &system_root, &by)?;
        report_git_merge(&before, &after);
        if !after.is_enabled() {
            bail!(
                "git's merges of `{}` are still not set up: git does not read the lines added \
                 to `{}` for every path, or does not find the `by` it was set up to run",
                root.display(),
                after.attributes_path
            );
        }
    }
    Ok(ExitStatus::Success)
}

/// asks, on a terminal, whether to set up git's merges, and answers no anywhere else: a script
/// says what it wants with `--git-merge` or `--no-git-merge`
#[expect(clippy::print_stderr)]
fn ask_git_merge() -> anyhow::Result<bool> {
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Ok(false);
    }
    eprint!(
        "set up git merges of the project with `by merge-driver`, which resolves what git \
         cannot about imports and leaves `api.lock` to be generated again? [Y/n] "
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "" | "y" | "Y" | "yes" | "Yes"))
}

#[expect(clippy::print_stderr)]
fn report_git_merge(before: &GitMergeIntegration, after: &GitMergeIntegration) {
    if before.is_enabled() {
        eprintln!("git merges already use `by merge-driver`");
        return;
    }
    if !before.missing_attributes.is_empty() {
        eprintln!(
            "added `{}` to `{}`: commit it, so that every clone merges them with the driver",
            before
                .missing_attributes
                .iter()
                .map(|pattern| format!("{pattern} merge=by"))
                .collect::<Vec<_>>()
                .join("`, `"),
            after.attributes_path
        );
    }
    if after.driver != before.driver
        && let Some(driver) = &after.driver
    {
        match &before.driver {
            Some(replaced) => eprintln!(
                "replaced `merge.by.driver` in the repository's git configuration, which ran \
                 `{replaced}`, with `{driver}`"
            ),
            None => eprintln!(
                "set `merge.by.driver` in the repository's git configuration to `{driver}`"
            ),
        }
    }
}

/// writes a new project at `root`
#[expect(clippy::print_stderr)]
fn scaffold(
    root: &Path,
    name: Option<&str>,
    kind: ProjectKind,
    python_version: &str,
) -> anyhow::Result<()> {
    let pyproject = root.join("pyproject.toml");

    let project_name = match name {
        Some(name) => name.to_owned(),
        None => directory_name(root)?,
    };
    let package = package_name(&project_name);
    let package_root = root.join("src").join(&package);
    fs::create_dir_all(&package_root)
        .with_context(|| format!("could not create {}", package_root.display()))?;

    write_new(
        &pyproject,
        &render_pyproject(&project_name, &package, kind, python_version),
    )?;
    write_new(
        &root.join(".python-version"),
        &format!("{python_version}\n"),
    )?;
    write_new(&root.join("README.md"), &format!("# {project_name}\n"))?;
    write_new(&package_root.join("__init__.by"), "")?;
    if kind == ProjectKind::Application {
        write_new(&package_root.join("main.by"), MAIN)?;
    }

    eprintln!("initialized `{project_name}` at {}", root.display());
    if kind == ProjectKind::Application {
        eprintln!("run it with `by run`");
    }
    eprintln!("build a wheel with `uv build`");
    Ok(())
}

/// The entry module of a new application.
///
/// `main` taking no arguments is the smallest thing that is still a real entry
/// point: give it parameters and they become command-line arguments.
const MAIN: &str = "\
def main():
    print(\"hello from basedpython\")


main()
";

fn render_pyproject(
    project_name: &str,
    package: &str,
    kind: ProjectKind,
    python_version: &str,
) -> String {
    // the backend a new project builds with needs a floor: without one, a future
    // release that changes how a project is built would change how *this* project
    // is built, without the project having said anything
    let backend_version = env!("CARGO_PKG_VERSION");
    let entry_point = match kind {
        ProjectKind::Application => {
            format!("\n[tool.basedpython.run]\nmain = \"{package}.main\"\n")
        }
        ProjectKind::Library => String::new(),
    };
    format!(
        "\
[build-system]
requires = [\"basedpython>={backend_version}\"]
build-backend = \"basedpython.build\"

[project]
name = \"{project_name}\"
version = \"0.1.0\"
description = \"\"
readme = \"README.md\"
requires-python = \">={python_version}\"
dependencies = []
{entry_point}"
    )
}

/// The importable name for a project called `project_name`.
///
/// A distribution name may hold `-` and `.`, which no module name can, so the
/// package directory is the normalized form — the same one every python packaging
/// tool arrives at.
fn package_name(project_name: &str) -> String {
    project_name
        .chars()
        .map(|character| match character {
            '-' | '.' => '_',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

fn directory_name(root: &Path) -> anyhow::Result<String> {
    root.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned)
        .with_context(|| {
            format!(
                "could not read a project name from `{}` — pass one with `--name`",
                root.display()
            )
        })
}

/// Write a file, leaving anything already there alone.
///
/// `by init` refuses to start on top of an existing project, but a directory can
/// still hold a `README.md` somebody wrote. Nothing here is worth overwriting it
/// for.
fn write_new(path: &Path, contents: &str) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_distribution_name_becomes_an_importable_package_name() {
        assert_eq!(package_name("my-project"), "my_project");
        assert_eq!(package_name("My.Project"), "my_project");
        assert_eq!(package_name("plain"), "plain");
    }

    #[test]
    fn an_application_gets_an_entry_point_and_a_library_does_not() {
        let application = render_pyproject("app", "app", ProjectKind::Application, "3.13");
        assert!(application.contains("[tool.basedpython.run]"));
        assert!(application.contains("main = \"app.main\""));

        let library = render_pyproject("lib", "lib", ProjectKind::Library, "3.13");
        assert!(!library.contains("[tool.basedpython.run]"));
    }

    /// what is written has to be what the packaging path reads: the backend, and
    /// a version floor the checker and the transpiler both target
    #[test]
    fn a_new_project_is_installable_as_written() {
        let rendered = render_pyproject("thing", "thing", ProjectKind::Library, "3.12");
        assert!(rendered.contains("build-backend = \"basedpython.build\""));
        // with a floor, so that a later release cannot change how a project
        // written today is built
        assert!(
            rendered.contains(&format!(
                "requires = [\"basedpython>={}\"]",
                env!("CARGO_PKG_VERSION")
            )),
            "{rendered}"
        );
        assert!(rendered.contains("requires-python = \">=3.12\""));
    }
}
