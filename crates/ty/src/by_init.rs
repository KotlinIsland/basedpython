//! Starting a project.
//!
//! Everything a basedpython project needs to be installable, checkable and
//! publishable is decided in its `pyproject.toml`, and every one of those
//! decisions is one somebody has to get right before writing a line of code: the
//! build backend, the layout the module tree is read from, the python version the
//! checker and the transpiler both target. `by init` writes them consistently, so
//! that the answer to "how do I ship this" is settled at the point the project is
//! created rather than discovered afterwards.

use std::fs;
use std::path::Path;

use anyhow::Context;

use crate::ExitStatus;

/// What kind of project is being started.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectKind {
    /// something to run: it gets an entry point, and `by run` alone will run it
    Application,
    /// something to import: no entry point, but the same packaging
    Library,
}

#[allow(clippy::print_stderr)]
pub(crate) fn cmd_init(
    path: Option<&Path>,
    name: Option<&str>,
    kind: ProjectKind,
    python_version: &str,
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let root = match path {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => cwd.join(path),
        None => cwd,
    };

    let project_name = match name {
        Some(name) => name.to_owned(),
        None => directory_name(&root)?,
    };
    let package = package_name(&project_name);

    let pyproject = root.join("pyproject.toml");
    if pyproject.exists() {
        anyhow::bail!(
            "`{}` already exists — `by init` will not write over a project that is already there",
            pyproject.display()
        );
    }

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
    Ok(ExitStatus::Success)
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
