//! the interpreter `by run` runs a program on
//!
//! here rather than in `by` because the language server starts programs too: a `manage.py` lens
//! runs on python when `by run` would refuse its project, and it has to be the python `by run`
//! would have picked, found the same way

use std::path::{Path, PathBuf};

use ruff_db::system::{OsSystem, SystemPath};
use ty_project::ProjectMetadata;
use ty_python_semantic::{PythonEnvironment, SysPrefixPathOrigin};
use ty_static::EnvVars;

/// The interpreter `by run` executes on, and how it was chosen.
#[derive(Clone)]
pub struct Interpreter {
    pub path: String,
    pub origin: String,
    /// whether the command was told which python to run on, by `--python` or `$PYTHON`, rather
    /// than finding the project's environment: the checker finds that one itself, and an
    /// interpreter named only for the run has to be handed to it
    pub named_for_run: bool,
    /// whether this is the bare `python3` off `PATH` — the last resort, and the
    /// only origin that says nothing about what the project targets
    pub is_from_path: bool,
}

/// Find the interpreter to run the program on.
///
/// The project environment comes first, because that is the environment the
/// project *is*: `by check` resolved this project's imports against it, so
/// running against a different python answers a question nobody asked. That has
/// to mean the same environment the checker used, resolved the same way — the
/// `environment.python` the project configures, then an activated virtual
/// environment, a conda environment, or a `.venv` beside the project's
/// `pyproject.toml`.
///
/// The project root is what all of that is relative to, not the working
/// directory: `by run` from a subdirectory is still this project, and its `.venv`
/// is still the one at the top.
///
/// `--python` overrides everything, for one run. `$PYTHON` is below discovery
/// because it names an interpreter rather than an environment, so it stands in
/// only where there is no project environment to prefer — but it still beats the
/// bare `python3` that discovery falls back to.
pub fn discover_interpreter(
    flag: Option<&Path>,
    project: &ProjectMetadata,
) -> anyhow::Result<Interpreter> {
    let named = |path: String, origin: &str| Interpreter {
        path,
        origin: origin.to_owned(),
        named_for_run: false,
        is_from_path: false,
    };
    let named_for_run = |path: String, origin: &str| Interpreter {
        named_for_run: true,
        ..named(path, origin)
    };

    if let Some(flag) = flag {
        // a `--python` may name the interpreter itself or the environment it
        // lives in, the same way `by check --python` does. failing both, it is
        // run as it is written, which finds a bare name like `python3.14` on
        // `PATH`, as `$PYTHON` and the `python3` fallback are found
        if flag.is_file() {
            return Ok(named_for_run(flag.display().to_string(), "`--python`"));
        }
        if let Some(interpreter) =
            interpreter_in_environment(flag, SysPrefixPathOrigin::PythonCliFlag)
        {
            return Ok(named_for_run(interpreter, "`--python`"));
        }
        return Ok(named_for_run(flag.display().to_string(), "`--python`"));
    }

    // what the project says its environment is, which is what the checker used.
    // a configured environment that cannot be resolved is an error rather than
    // something to fall past: `by check` refuses it outright, and running on a
    // different python than the one just type-checked against — reporting it as
    // a version mismatch, which names the wrong cause — is how the two commands
    // came to disagree in the first place
    if let Some(configured) = configured_environment(project) {
        let Some(interpreter) =
            interpreter_in_environment(&configured, SysPrefixPathOrigin::PythonCliFlag)
        else {
            anyhow::bail!(
                "`environment.python` is `{}`, which is not a python environment — \
                 the same setting `by check` reads, so neither command can use it",
                configured.display()
            );
        };
        return Ok(named(interpreter, "`environment.python`"));
    }

    let discovered = discovered_environment(project.root().as_std_path());
    if let Some(found) = &discovered
        && !found.is_from_path
    {
        return Ok(found.clone());
    }

    if let Ok(python) = std::env::var(EnvVars::PYTHON) {
        return Ok(named_for_run(python, "`PYTHON`"));
    }

    Ok(discovered.unwrap_or_else(|| Interpreter {
        path: "python3".to_owned(),
        origin: "`PATH`".to_owned(),
        named_for_run: false,
        is_from_path: true,
    }))
}

/// The environment the project configures, as an absolute path.
fn configured_environment(project: &ProjectMetadata) -> Option<PathBuf> {
    let root = project.root();
    let system = OsSystem::new(root);
    let configured = project.options().environment.as_ref()?.python.as_ref()?;
    Some(PathBuf::from(configured.absolute(root, &system).as_str()))
}

/// The environment discovery finds for this project: an activated virtual
/// environment, a conda environment, or a `.venv` at the project root — and,
/// failing all of those, whatever python is on `PATH`.
fn discovered_environment(root: &Path) -> Option<Interpreter> {
    let sys_root = SystemPath::from_std_path(root)?;
    let system = OsSystem::new(sys_root);
    let environment = PythonEnvironment::discover(Some(sys_root), &system).ok()??;
    let interpreter = environment.interpreter(&system)?;
    // discovery ends by falling back to whatever python is on `PATH`, which is
    // an interpreter but not a *project* environment — the difference is what
    // decides whether it outranks `$PYTHON`, and what a new project targets
    let is_from_path = matches!(
        environment.origin(),
        SysPrefixPathOrigin::PythonBinary | SysPrefixPathOrigin::SelfEnvironment
    );
    Some(Interpreter {
        path: interpreter.to_string(),
        origin: environment.origin().to_string(),
        named_for_run: false,
        is_from_path,
    })
}

/// The interpreter inside the environment rooted at `path`, if there is one. a
/// relative `path` is taken from the working directory
fn interpreter_in_environment(path: &Path, origin: SysPrefixPathOrigin) -> Option<String> {
    let path = std::path::absolute(path).ok()?;
    let sys_path = SystemPath::from_std_path(&path)?;
    let system = OsSystem::new(sys_path);
    let environment = PythonEnvironment::new(sys_path, origin, &system).ok()?;
    environment
        .interpreter(&system)
        .map(|interpreter| interpreter.to_string())
}
