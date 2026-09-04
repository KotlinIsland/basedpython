use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use by_transforms::config::{Config, PythonVersion};
use ruff_db::diagnostic::{
    Annotation, Diagnostic, DiagnosticId, DisplayDiagnosticConfig, DisplayDiagnostics, LintName,
    Severity, Span, SubDiagnostic, SubDiagnosticSeverity,
};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{OsSystem, SystemPath};
use ruff_text_size::TextRange;
use ty_project::{Db, ProjectDatabase, ProjectMetadata};
use ty_site_packages::{PythonEnvironment, SysPrefixPathOrigin};
use ty_static::EnvVars;
use walkdir::WalkDir;

use crate::ExitStatus;
use crate::args::LoweringArgs;
use crate::by_lowering::SettledLowering;
use by_stage::emit::{CheckGate, Transpiled, is_unusable_source, transpile_bug_diagnostic};
use by_stage::project::{
    BY_SOURCES, COMPILABLE_SOURCES, Rebuilder, build_project_db, may_contain_sources, module_roots,
    source_files,
};
use by_stage::record::{BuildRecord, parse_soundness, stage_build_record};
use by_stage::sourcemap::{TracebackEntry, stage_module, write_sourcemap_module};
use by_stage::staging::{Staging, transpiled_destination};
use by_stage::verbatim::stage_verbatim;

/// The transpile config for a command whose `--min-version` is optional.
///
/// Without the flag the target is the version the project configures, so that the
/// checker and the emitter agree about which python this project is for. Outside a
/// project — `by transpile` reading a lone file — the transpiler's own default
/// stands in.
fn version_config(min_version: Option<&str>, cwd: &Path) -> anyhow::Result<Config> {
    match min_version {
        Some(spelled) => parse_version(spelled),
        None => Ok(Config {
            min_version: ResolvedProject::discover(cwd).map_or_else(
                |_| Config::default().min_version,
                |project| project.python_version(),
            ),
            ..Config::default()
        }),
    }
}

fn parse_version(s: &str) -> anyhow::Result<Config> {
    let version = s
        .parse::<PythonVersion>()
        .map_err(|_| anyhow::anyhow!("unknown Python version {s:?} — use e.g. 3.12"))?;
    Ok(Config {
        min_version: version,
        ..Config::default()
    })
}

impl LoweringArgs {
    /// Fold the lowering options this command was given into a config already
    /// carrying its target version.
    fn apply(&self, config: &mut Config, cwd: &Path) -> anyhow::Result<()> {
        self.fold(&SettledLowering::from(self), config, cwd)
    }

    /// [`apply`](Self::apply) for `by build`, which is the only command a
    /// `--wheels` release runs inside itself.
    ///
    /// A release settles these once and hands them down, so a build that is one
    /// of several inside one lowers the way the release asked rather than the
    /// way its own command line reads — the backend passes no lowering options
    /// on, so that command line is not the user's anyway.
    ///
    /// Every other command keeps [`apply`](Self::apply). These options change
    /// what the emitted python does, and a variable left in the environment
    /// must not silently re-lower a `by transpile` that was given options of its
    /// own.
    fn apply_for_build(&self, config: &mut Config, cwd: &Path) -> anyhow::Result<()> {
        let settled = crate::by_lowering::settled_by_the_release()
            .unwrap_or_else(|| SettledLowering::from(self));
        self.fold(&settled, config, cwd)
    }

    /// Fold settled lowering options into a config already carrying its target
    /// version.
    ///
    /// `cwd` is where the project is looked for: the lowerings a project
    /// configures belong to every command that emits python, so they are read
    /// here rather than at each command's own config. Outside a project the
    /// transpiler's own defaults stand.
    fn fold(
        &self,
        lowering: &SettledLowering,
        config: &mut Config,
        cwd: &Path,
    ) -> anyhow::Result<()> {
        // destructured for the same reason the arguments are on the way in: an
        // option carried across but never applied is dropped just as quietly as
        // one never carried
        let SettledLowering {
            soundness,
            runtime_raises_checks,
            no_unique_loop_bindings,
        } = lowering;
        config.soundness = parse_soundness(soundness)?;
        config.runtime_raises_checks = *runtime_raises_checks;
        config.unique_loop_bindings = !*no_unique_loop_bindings;
        if let Ok(project) = ResolvedProject::discover(cwd) {
            config.float_literals = project.float_literals();
        }
        // only what was asked for explicitly. what the build can work out for
        // itself needs the project root, which is settled after this
        config.stamps = crate::by_stamps::parse_explicit(&self.stamps)?;
        Ok(())
    }
}

// ── run ──────────────────────────────────────────────────────────────────────

#[allow(clippy::exit, clippy::print_stderr)]
pub(crate) fn cmd_run(
    module: Option<&str>,
    args: &[String],
    min_version: Option<&str>,
    lowering: &LoweringArgs,
    compiled: bool,
    python_flag: Option<&Path>,
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    // one resolution of the project, for the environment and the target version
    // both: they are two readings of the same configuration and must not be able
    // to disagree
    let project = ResolvedProject::discover(&cwd)?;
    let interpreter = discover_interpreter(python_flag, &project)?;
    let python = interpreter.path.clone();
    // `run` executes on a specific interpreter, so by default target *its*
    // version: the emitted code (dataclass `slots=`, PEP 695 syntax, …) must
    // match what that python actually supports. an explicit `--min-version`
    // wins, but it cannot exceed the interpreter — that would emit code the
    // interpreter cannot parse
    let probed = detect_python_version(&python);
    let mut config = match (min_version, probed) {
        (Some(flag), probed) => {
            let config = parse_version(flag)?;
            if let Some(found) = probed
                && config.min_version > found
            {
                anyhow::bail!(
                    "--min-version {flag} is newer than the interpreter this would run on: \
                     `{python}` is {found}, from {}",
                    interpreter.origin
                );
            }
            config
        }
        (None, Some(version)) => Config {
            min_version: version,
            ..Config::default()
        },
        (None, None) => Config::default(),
    };
    // a program written for the python the project declares cannot be run by an
    // older one: the source itself may use syntax that python has no lowering
    // for (`match`, for one), and the failure lands as a `SyntaxError` inside
    // generated code rather than as anything the author can act on
    if let Some(found) = probed
        && let Some(configured) = project.declared_python_version()
    {
        if min_version.is_none() && found < configured {
            anyhow::bail!(
                "this project targets python {configured}, but the interpreter this would run on \
                 is {found}: `{python}`, from {}\n       \
                 use an interpreter that is {configured} or newer, or pass \
                 `--min-version {found}` to build for this one",
                interpreter.origin
            );
        }
    }
    lowering.apply(&mut config, &cwd)?;
    let target = config.min_version.to_string();
    crate::by_stamps::fill_discovered(&mut config.stamps, project.root(), Some(&target));
    let tmp = tempfile::TempDir::new().context("failed to create temp directory")?;

    let (db, handles, rebuilder, root) = build_project_db(&cwd, BY_SOURCES, None)?;
    if handles.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Failure);
    }
    let roots = module_roots(&db, &root);

    // an explicit module always wins; otherwise the project's configured entry
    // point stands in for it. resolving before the (much slower) check means a
    // project with neither fails immediately rather than after transpiling
    let module = match module {
        Some(module) => module.to_owned(),
        None => match configured_main(&db) {
            Some(main) => main,
            None => anyhow::bail!(
                "no module given and no entry point configured — \
                 name a module (`by run main`) or set `run.main` in the project configuration"
            ),
        },
    };

    // each generated `.py` paired with its source `.by` and the line table that
    // lifts generated line numbers back to `.by` lines (for traceback rewriting)
    let mut traceback_entries: Vec<TracebackEntry> = Vec::new();
    let mut staging = Staging::new(tmp.path());
    let ok = render_check_and_transpile(
        &db,
        &handles,
        &config,
        CheckGate::AllErrors,
        &rebuilder,
        &mut by_transforms::RuntimeRequirements::default(),
        |emitted| {
            let relative = transpiled_destination(&roots, &root, emitted.by_path);
            traceback_entries.push(stage_module(&mut staging, &relative, emitted)?);
            Ok(())
        },
    )?;
    if !ok {
        return Ok(ExitStatus::Failure);
    }
    // a program is its data as much as its modules: a `.py` module it imports, a
    // json file it opens, a template it renders. running out of a directory
    // holding only the transpiled half fails on the first of them
    stage_verbatim(&db, &root, &roots, &mut staging)?;
    stage_by_typed_markers(&db, &mut staging, &roots, &root)?;
    write_traceback_runtime(&mut staging, &traceback_entries)?;
    // what this build *was*, written into the build itself. a tree that is going
    // to be staged again one file at a time — which is what a debugger's reload
    // does — has to be able to say which transpiler and which configuration wrote
    // it: `run` takes its target version from the interpreter it probed while
    // `build` takes it from the project, so a later re-stage that re-derived the
    // configuration would emit different code in exactly the case that matters
    stage_build_record(
        &mut staging,
        &BuildRecord::new(&root, &roots, Some(module.clone()), compiled, &config),
    )?;
    staging.finish()?;

    if compiled {
        // the extension lands beside the generated `.py`, and python's finder
        // prefers an extension to source — so an `import` picks the compiled
        // module up with no path juggling. a declined function still runs, from
        // the source embedded in the extension, which is why this is a speed
        // switch and not a behaviour switch.
        //
        // the entry module is the exception, and it has to be: `runpy` needs a
        // code object to run something as `__main__`, and an extension has none.
        // so the entry stays interpreted and everything it imports is native
        let options = by_build::Options {
            fallback: Some(config),
            language: by_irbuild::Language::default(),
            ..by_build::Options::default()
        };
        // no added context: every way `probe` fails already names the interpreter and says
        // what about it was refused, and a blanket "could not read its build configuration"
        // on top of them told a user to go and install headers when the real answer was that
        // their python was too old
        let toolchain = by_build::Toolchain::probe(&python)?;
        let mut built = 0usize;
        for entry in &traceback_entries {
            // the text the transpile ran on, not a fresh read of the file: a
            // `.by` saved between the two would give this native module a
            // different source than the sourcemap and its digest describe
            let source = &entry.by_source;
            // the generated tree *is* the module tree, so the dotted name is the
            // path within it — and it has to be dotted, because a class's
            // `__module__` is read off the front of its type's `tp_name`. a file
            // the tree gives no name to is left interpreted rather than compiled
            // under a guessed one
            let relative = entry
                .py_path
                .strip_prefix(tmp.path())
                .unwrap_or(&entry.py_path);
            let Some(name) = dotted_module_name(relative) else {
                continue;
            };
            if name.dotted() == module {
                continue;
            }
            let mut lowered = by_irbuild::module_from_source(source, name, options.language);
            lowered.lines = Some(by_ir::function::LineTable::new(
                entry.by_path.display().to_string(),
                source,
            ));
            // the root of the generated tree, not the directory the `.py` landed
            // in: the build lays the extension out at its module's own place, and
            // handing it the leaf directory would nest the tree inside itself
            by_build::build_lowered(lowered, source, &toolchain, tmp.path(), &options)
                .with_context(|| format!("could not compile {}", entry.by_path.display()))?;
            built += 1;
        }
        if built == 0 {
            eprintln!(
                "note: `{module}` is the entry module, which has to run as `__main__` \
                 from source — nothing else to compile"
            );
        }
    }

    // the program runs where the user invoked it, the way `python -m` does: a
    // relative path on its command line, and anything it reads or writes beside
    // the project, resolve against the directory they were written for. python
    // puts the runner's own directory — the generated tree — at the head of
    // `sys.path`, so the module is still found there
    let status = Command::new(&python)
        .arg(tmp.path().join(BY_RUNNER_FILENAME))
        .arg(&module)
        .args(args)
        .current_dir(&cwd)
        .status()
        .with_context(|| {
            format!(
                "could not run `{python}`, the interpreter from {}",
                interpreter.origin
            )
        })?;

    let code = status.code().unwrap_or(1);
    // drop the temp dir explicitly: `process::exit` skips destructors, so
    // exiting while it's still in scope would leak the directory
    drop(tmp);
    std::process::exit(code);
}

/// The source roots a distribution's packages come from.
///
/// The project root is always a module root — it is what lets `tests/` and a
/// script beside it resolve their imports — but for a src-layout project it is
/// not where the *distribution* lives. `src/app` is a package of this project;
/// `tests` beside it is not something anyone installs. So when the project
/// declares somewhere for its modules to live, that is where they live, and the
/// root counts only when it is the only answer.
fn packaging_roots(roots: &[PathBuf], root: &Path) -> Vec<PathBuf> {
    let declared: Vec<PathBuf> = roots
        .iter()
        .filter(|candidate| candidate.as_path() != root)
        .cloned()
        .collect();
    if declared.is_empty() {
        vec![root.to_path_buf()]
    } else {
        declared
    }
}

/// The top-level packages the build produced, as a distribution would ship them.
fn staged_packages(staging: &Staging, roots: &[PathBuf], root: &Path) -> Vec<String> {
    let packaging = packaging_roots(roots, root);
    let mut packages: Vec<String> = staging
        .entries()
        .filter(|(_, source)| {
            source.is_some_and(|source| {
                packaging
                    .iter()
                    .any(|candidate| source.starts_with(candidate))
            })
        })
        .filter_map(|(destination, _)| {
            let mut components = destination.components();
            let package = components.next()?;
            let rest: PathBuf = components.collect();
            matches!(
                rest.to_str(),
                Some("__init__.py" | "__init__.pyi" | "__init__.by" | "__init__.byi")
            )
            .then(|| package.as_os_str().to_str().map(str::to_owned))
            .flatten()
        })
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

/// Write the `by.typed` marker into every package the build ships.
///
/// The marker says two things to a project that installs this one, and both are
/// things nothing else can tell it. Its presence says the `.by` beside a module is
/// the authoritative surface, to be read in preference to the python it was
/// transpiled into — the same bargain `py.typed` strikes for inline annotations.
/// Its contents say which of this project's dependencies are part of its own
/// interface, which a `pyproject.toml` cannot, because nothing installs one.
///
/// Only the packages the project *ships* are marked, and they are read off what
/// was written rather than off the source layout — a `tests` package beside `src`
/// is neither shipped nor anybody else's business.
#[allow(clippy::print_stderr)]
fn stage_by_typed_markers(
    db: &ProjectDatabase,
    staging: &mut Staging,
    roots: &[PathBuf],
    root: &Path,
) -> anyhow::Result<()> {
    let exported = db
        .project()
        .settings(db)
        .analysis()
        .exported_dependencies
        .clone()
        .unwrap_or_default();
    // written whether or not the `.by` sources went with it: the precedence claim
    // is vacuous without them — nothing to prefer — but the export declaration is
    // not, and a python-only build still has dependencies it hands out on purpose
    let marker = ty_module_resolver::Marker::render(&exported);

    for package in staged_packages(staging, roots, root) {
        staging.write(
            &Path::new(&package).join(ty_module_resolver::BY_TYPED),
            None,
            &marker,
        )?;
    }

    if !exported.is_empty() {
        eprintln!("exporting {}", exported.join(", "));
    }
    Ok(())
}

/// The dotted module name a file laid out at `relative` will be imported under.
///
/// The tree the generated python is written into *is* the module tree — every
/// file lands at [`transpiled_destination`] — so the name is that path with its
/// separators turned into dots. `pkg/__init__.py` is the package `pkg` itself,
/// which is the name a class defined in it reports as its `__module__`.
///
/// `None` when the path names no module: nothing but `__init__.py` at the root,
/// or a component that is not plain text. A caller with no name has nothing to
/// compile, and guessing one would be worse than saying so.
fn dotted_module_name(relative: &Path) -> Option<by_ir::ModuleName> {
    let mut components: Vec<&str> = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return None;
        };
        components.push(name.to_str()?);
    }
    let last = components.pop()?;
    let stem = last.strip_suffix(".py").unwrap_or(last);
    // a package's `__init__` is not a module beside the package, it *is* the
    // package — so a class written in it belongs to the package's own name
    let is_package = stem == "__init__";
    if !is_package {
        components.push(stem);
    }
    if components.is_empty() {
        return None;
    }
    let dotted = components.join(".");
    Some(if is_package {
        by_ir::ModuleName::package(dotted)
    } else {
        by_ir::ModuleName::new(dotted)
    })
}

/// The dotted module name `file` is imported under, as the project resolves it.
///
/// The resolver is what knows this, and nothing simpler will do: it accounts for
/// the search paths (`src/pkg/m.py` in a src-layout project is `pkg.m`, not
/// `src.pkg.m`), for a `pkg/__init__.py` whose module is `pkg`, and for a
/// namespace package, which a walk looking for `__init__.py` would stop short of.
/// It also refuses a directory that merely holds `.py` files — cpython's own
/// `config-3.13-darwin` is not a package, and its name is not even an identifier.
///
/// `None` when no search path reaches the file. Such a file has no dotted name to
/// be had: the only way to import it is from its own directory, under its stem.
fn resolved_module_name(db: &ProjectDatabase, file: ruff_db::files::File) -> Option<String> {
    let program_file = ty_python_semantic::Db::program_file(db, file);
    let module = ty_module_resolver::file_to_module(db, program_file.resolver_file(db))?;
    Some(module.name(db).to_string())
}

/// The name `path` is compiled under — dotted, and knowing whether it is a
/// package's.
///
/// The *dotted* name because it is what the emitted types carry: cpython reads a
/// class's `__module__` off the front of its `tp_name`, so a class in
/// `tkinter/m.py` compiled as plain `m` reports a module nothing can look up —
/// and `dataclasses` does exactly that lookup.
///
/// Package or not because the two are written to different files. The resolver
/// names a package after its directory, so an `__init__.py` it resolved is that
/// package, and its artefact is the `__init__` inside the directory. A file the
/// resolver could not reach has no dotted name at all: it falls back to its stem,
/// which is the only name it could be imported under, from its own directory.
///
/// `None` where even that fallback names nothing the source meant. An
/// `__init__.py` is the body of the package its directory names, and a directory
/// the resolver could not reach names no package — `a-one/__init__.py` is the
/// plainest case, since `a-one` is not an identifier and nothing can import it.
/// Compiling such a file under its stem produces a module called `__init__`: it
/// loads, it answers `__name__ == "__init__"`, its relative imports have no
/// package to be relative to, and its submodules are bound to nothing. Two of
/// them in different directories then claim one artefact, which is how the clash
/// error was first proven reachable. A source whose own identity the artefact
/// cannot carry is declined rather than half-built.
fn compiled_module_name(
    db: &ProjectDatabase,
    path: &Path,
    file: ruff_db::files::File,
) -> anyhow::Result<Option<by_ir::ModuleName>> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("a source file has no usable module name")?;
    Ok(match resolved_module_name(db, file) {
        Some(resolved) if stem == "__init__" => Some(by_ir::ModuleName::package(resolved)),
        Some(resolved) => Some(by_ir::ModuleName::new(resolved)),
        None if stem == "__init__" => None,
        None => Some(by_ir::ModuleName::new(stem)),
    })
}

/// The project's `run.main` entry point, if one is configured.
fn configured_main(db: &ProjectDatabase) -> Option<String> {
    let options = db.project().metadata(db).options();
    let main = options.run.as_ref()?.main.as_ref()?;
    Some((**main).clone())
}

/// The python version a brand new project should target.
///
/// The environment it will be developed in is the right answer when there is
/// one: the version the checker targets, the version the transpiler emits for,
/// and the version that runs the result should be one version rather than three.
/// A project being created usually has no environment yet, though, and the bare
/// `python3` that turns up on `PATH` instead is whatever the operating system
/// shipped years ago. Pinning a new project to that is how a project ends up
/// targeting 3.9 for its whole life without anyone choosing to.
pub(crate) fn default_project_python_version() -> PythonVersion {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // `by init` runs before there is a project to resolve, so a failure here is
    // the ordinary case rather than a problem
    let Ok(project) = ResolvedProject::discover(&cwd) else {
        return latest_python_version();
    };
    let Ok(interpreter) = discover_interpreter(None, &project) else {
        return latest_python_version();
    };
    if interpreter.is_from_path {
        return latest_python_version();
    }
    detect_python_version(&interpreter.path).unwrap_or_else(|| Config::default().min_version)
}

/// The newest python this release can emit for.
fn latest_python_version() -> PythonVersion {
    ruff_python_ast::PythonVersion::latest()
        .to_string()
        .parse()
        .unwrap_or_else(|_| Config::default().min_version)
}

/// The interpreter `by run` executes on, and how it was chosen.
#[derive(Clone)]
struct Interpreter {
    path: String,
    origin: String,
    /// whether this is the bare `python3` off `PATH` — the last resort, and the
    /// only origin that says nothing about what the project targets
    is_from_path: bool,
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
fn discover_interpreter(
    flag: Option<&Path>,
    project: &ResolvedProject,
) -> anyhow::Result<Interpreter> {
    let named = |path: String, origin: &str| Interpreter {
        path,
        origin: origin.to_owned(),
        is_from_path: false,
    };

    if let Some(flag) = flag {
        // a `--python` may name the interpreter itself or the environment it
        // lives in, the same way `by check --python` does
        if flag.is_file() {
            return Ok(named(flag.display().to_string(), "`--python`"));
        }
        if let Some(interpreter) =
            interpreter_in_environment(flag, SysPrefixPathOrigin::PythonCliFlag)
        {
            return Ok(named(interpreter, "`--python`"));
        }
        return Ok(named(flag.display().to_string(), "`--python`"));
    }

    // what the project says its environment is, which is what the checker used.
    // a configured environment that cannot be resolved is an error rather than
    // something to fall past: `by check` refuses it outright, and running on a
    // different python than the one just type-checked against — reporting it as
    // a version mismatch, which names the wrong cause — is how the two commands
    // came to disagree in the first place
    if let Some(configured) = project.configured_environment() {
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

    let discovered = discovered_environment(project.root());
    if let Some(found) = &discovered
        && !found.is_from_path
    {
        return Ok(found.clone());
    }

    if let Ok(python) = std::env::var(EnvVars::PYTHON) {
        return Ok(named(python, "`PYTHON`"));
    }

    Ok(discovered.unwrap_or_else(|| Interpreter {
        path: "python3".to_owned(),
        origin: "`PATH`".to_owned(),
        is_from_path: true,
    }))
}

/// A project, resolved once.
///
/// Discovery walks up from the working directory reading configuration, and
/// every answer taken from it — where the root is, which environment the project
/// declares, which python it targets — has to be the same answer. Resolving it
/// per question was not only repeated work: the copies disagreed about failure,
/// one falling back to the working directory where another gave up.
struct ResolvedProject {
    root: PathBuf,
    metadata: ProjectMetadata,
}

impl ResolvedProject {
    fn discover(cwd: &Path) -> anyhow::Result<Self> {
        let sys_cwd = SystemPath::from_std_path(cwd)
            .with_context(|| format!("non-utf8 path: {}", cwd.display()))?;
        let system = OsSystem::new(sys_cwd);
        let metadata = ProjectMetadata::discover(sys_cwd, &system)
            .with_context(|| format!("failed to discover project at {sys_cwd}"))?;
        let root = PathBuf::from(metadata.root().as_str());
        Ok(Self { root, metadata })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    /// How this project spells a float or complex literal type in the python it
    /// emits.
    fn float_literals(&self) -> by_transforms::FloatLiteralLowering {
        match self
            .metadata
            .options()
            .lowering
            .as_ref()
            .and_then(|lowering| lowering.float_literals.as_deref())
        {
            Some(ty_project::metadata::options::FloatLiteralLowering::Literal) => {
                by_transforms::FloatLiteralLowering::Literal
            }
            Some(ty_project::metadata::options::FloatLiteralLowering::Nominal) | None => {
                by_transforms::FloatLiteralLowering::Nominal
            }
        }
    }

    /// The environment the project configures, as an absolute path.
    fn configured_environment(&self) -> Option<PathBuf> {
        let sys_root = SystemPath::from_std_path(&self.root)?;
        let system = OsSystem::new(sys_root);
        let configured = self
            .metadata
            .options()
            .environment
            .as_ref()?
            .python
            .as_ref()?;
        Some(PathBuf::from(
            configured.absolute(sys_root, &system).as_str(),
        ))
    }

    /// The python version the emitted code must run on: the one the project
    /// configures (`environment.python-version`, else the `requires-python` lower
    /// bound), so the checker and the emitter agree about which python this
    /// project targets.
    /// The python version the project *declares* it targets, if it declares one.
    ///
    /// Distinct from [`Self::python_version`], which fills in a default when the
    /// project says nothing. A default is not a declaration, and treating it as
    /// one refuses to run every project without a `requires-python` on anything
    /// but the newest python there is — a thing nobody asked for, and a thing the
    /// author cannot act on.
    fn declared_python_version(&self) -> Option<PythonVersion> {
        let declared = self
            .metadata
            .options()
            .environment
            .as_ref()?
            .python_version
            .as_ref()?;
        declared.to_string().parse().ok()
    }

    fn python_version(&self) -> PythonVersion {
        let Some(sys_root) = SystemPath::from_std_path(&self.root) else {
            return Config::default().min_version;
        };
        let system = OsSystem::new(sys_root);
        let db = ProjectDatabase::use_defaults(self.metadata.clone(), system);
        db.project()
            .program(&db)
            .python_version(&db)
            .to_string()
            .parse()
            .unwrap_or_else(|_| Config::default().min_version)
    }
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
        is_from_path,
    })
}

/// The interpreter inside the environment rooted at `path`, if there is one.
fn interpreter_in_environment(path: &Path, origin: SysPrefixPathOrigin) -> Option<String> {
    let sys_path = SystemPath::from_std_path(path)?;
    let system = OsSystem::new(sys_path);
    let environment = PythonEnvironment::new(sys_path, origin, &system).ok()?;
    environment
        .interpreter(&system)
        .map(|interpreter| interpreter.to_string())
}

/// Probe `python`'s `major.minor` version (e.g. `3.9`) so `run` can target the
/// interpreter it will execute on. Returns `None` if the interpreter can't be
/// run or its output can't be parsed.
fn detect_python_version(python: &str) -> Option<PythonVersion> {
    let output = Command::new(python)
        .arg("-c")
        .arg("import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

// ── build ────────────────────────────────────────────────────────────────────

#[allow(clippy::print_stderr)]
/// Recompute one file's slot in a build tree that already exists, and print it.
///
/// The command-line half of `by/transpileForBuild`, over the same implementation:
/// the language server answers this out of a database that is already warm,
/// which is what makes it fast enough for an editor, and this builds one first.
/// Two front ends, one operation — the bytes either produces have to be the bytes
/// the build itself would have written, and two implementations could not
/// promise that.
///
/// Prints JSON on stdout and writes nothing. A refusal is JSON too, and exits
/// non-zero: a caller in a script should be able to read `$?` rather than parse
/// to find out whether it got bytes.
#[allow(clippy::print_stdout)]
pub(crate) fn cmd_restage(build_directory: &Path, file: &Path) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        cwd.join(file)
    };
    let (db, _, _, _) = build_project_db(&cwd, BY_SOURCES, Some(build_directory))?;

    let restaged = by_stage::restage::restage_one(&db, build_directory, &file)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&restaged)
            .context("could not render what the re-stage produced")?
    );
    Ok(match restaged {
        by_stage::restage::Restage::Ready(_) => ExitStatus::Success,
        by_stage::restage::Restage::Refused(_) => ExitStatus::Failure,
    })
}

#[expect(clippy::print_stderr)]
pub(crate) fn cmd_build(
    min_version: Option<&str>,
    lowering: &LoweringArgs,
    out: &Path,
    print_manifest: bool,
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let mut config = version_config(min_version, &cwd)?;
    lowering.apply_for_build(&mut config, &cwd)?;
    let target = config.min_version.to_string();
    crate::by_stamps::fill_discovered(&mut config.stamps, &cwd, Some(&target));

    // the output directory is settled before the project is read, because it is
    // the one directory the project must not be read *from*: it holds a copy of
    // every source this build is about to write. canonical, because that is what
    // the paths it is compared against are — creating it first is what makes
    // canonicalizing it possible
    let out = cwd.join(out);
    fs::create_dir_all(&out).with_context(|| format!("could not create {}", out.display()))?;
    let out = fs::canonicalize(&out).unwrap_or(out);

    let (db, handles, rebuilder, root) = build_project_db(&cwd, BY_SOURCES, Some(&out))?;
    if handles.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }
    let file_count = handles.len();
    let roots = module_roots(&db, &root);
    let mut staging = Staging::new(&out);
    // `out/` outlives the build that wrote it — it is what a test runner, a
    // debugger or an editor plugin sees — so the sourcemap goes with it. this is
    // the directory where a `.by` really can be saved after the transpile, which
    // is what the digests beside the map are for
    let mut entries: Vec<TracebackEntry> = Vec::new();
    let mut requirements = by_transforms::RuntimeRequirements::default();
    let ok = render_check_and_transpile(
        &db,
        &handles,
        &config,
        CheckGate::ParseErrorsOnly,
        &rebuilder,
        &mut requirements,
        |emitted| {
            let relative = transpiled_destination(&roots, &root, emitted.by_path);
            let entry = stage_module(&mut staging, &relative, emitted)?;
            eprintln!(
                "{} -> {}",
                emitted.by_path.display(),
                entry.py_path.display()
            );
            entries.push(entry);
            Ok(())
        },
    );
    // the sourcemap and the package markers describe the tree that was written,
    // so they are staged whether or not something was reported — an `out/` a
    // debugger cannot read is worse than one built from a partial check
    stage_verbatim(&db, &root, &roots, &mut staging)?;
    stage_by_typed_markers(&db, &mut staging, &roots, &root)?;
    write_sourcemap_module(&mut staging, &entries)?;
    // `out/` outlives the build that wrote it and is what a debugger, a test
    // runner or an editor plugin later reads, so it carries the same record a
    // `run` tree does. no entry module: a build is not pointed at one
    stage_build_record(
        &mut staging,
        &BuildRecord::new(&root, &roots, None, false, &config),
    )?;
    if print_manifest {
        print_build_manifest(&staging, &roots, &root, requirements)?;
    }
    staging.finish()?;

    if !ok? {
        return Ok(ExitStatus::Failure);
    }
    eprintln!("\nbuild complete ({file_count} files)");
    Ok(ExitStatus::Success)
}

/// Report what the build read and what it produced, as `<kind> <value>` lines.
///
/// Two questions, both of which only the build can answer: which files this
/// project is made of — a source distribution has to carry exactly those, since
/// they are what rebuilds into the same wheel — and which top-level packages came
/// out. Answering them here rather than in the packaging layer keeps one answer
/// to "what is this project", instead of a second one that has to be kept in step.
#[allow(clippy::print_stdout)]
fn print_build_manifest(
    staging: &Staging,
    roots: &[PathBuf],
    root: &Path,
    requirements: by_transforms::RuntimeRequirements,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut stdout = io::stdout().lock();
    for input in staging.inputs() {
        let relative = input.strip_prefix(root).unwrap_or(input);
        writeln!(stdout, "input {}", relative.display())?;
    }
    for package in staged_packages(staging, roots, root) {
        writeln!(stdout, "package {package}")?;
    }
    // lowering for an older python can put a name in the output that only
    // `typing_extensions` has there. nothing in the source says so, so nothing
    // but the build can
    for specifier in requirements.specifiers() {
        writeln!(stdout, "requires {specifier}")?;
    }
    Ok(())
}

// ── compile ─────────────────────────────────────────────────────────────────

/// How `by compile` was invoked.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompileFlags {
    pub(crate) verbose: bool,
    pub(crate) emit_c_only: bool,
    pub(crate) annotate: bool,
    pub(crate) lowering: LoweringArgs,
    pub(crate) options: by_build::Options,
}

#[allow(clippy::print_stderr)]
/// Compile `.by` and `.py` files to native extension modules.
///
/// Every function the compiler declines is reported rather than silently
/// dropped: the count of declines is the honest measure of how much of a project
/// actually runs natively, so it is never hidden.
pub(crate) fn cmd_compile(
    files: &[PathBuf],
    output: &Path,
    flags: CompileFlags,
) -> anyhow::Result<ExitStatus> {
    let CompileFlags {
        verbose,
        emit_c_only,
        annotate,
        lowering,
        mut options,
    } = flags;
    options.annotate = options.annotate || annotate;
    // a declined function runs from the transpiled fallback, so it has to be
    // transpiled with the same options a `transpile` of this module would use —
    // otherwise the compiled half and the interpreted half check different things
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let mut fallback = Config::default();
    lowering.apply(&mut fallback, &cwd)?;
    // no target python: `by_build` transpiles the interpreted twin for whatever
    // interpreter the toolchain turns out to be, replacing this config's version
    // as it goes — so naming one here would stamp a python the output was not
    // lowered for. a program that wants `PYTHON_VERSION` under `by compile` has
    // to say which, and gets told so rather than told the wrong one
    crate::by_stamps::fill_discovered(&mut fallback.stamps, &cwd, None);
    options.fallback = Some(fallback);
    let sources: Vec<PathBuf> = if files.is_empty() {
        compilable_files(&cwd)
    } else {
        files.to_vec()
    };
    if sources.is_empty() {
        eprintln!("no .by or .py files found");
        return Ok(ExitStatus::Success);
    }

    // the same interpreter `by run` picks, resolved the same way: an extension
    // built against one abi is unimportable by another, and `by run --compiled`
    // imports what this wrote
    let project = ResolvedProject::discover(&cwd)?;
    let python = discover_interpreter(None, &project)?.path;
    // see the note on the other `probe` call: its own errors are self-describing, and the
    // context that used to sit here misreported a version refusal as a missing header
    let toolchain = by_build::Toolchain::probe(&python)?;

    let out_dir = cwd.join(output);
    let mut compiled = 0usize;
    let mut declined_total = 0usize;

    // one database for the whole project, so a type imported from a sibling module
    // resolves. lowering each file on its own is sound — an unresolved class
    // degrades to the object protocol — but it makes every imported type look
    // gradual, and `--no-any` would then fail on noise
    // `compile` embeds fallback source produced by the untyped transpile, which
    // takes no db, so the rebuilder the other commands thread through is unused here
    let (db, project, _rebuilder, _root) = build_project_db(&cwd, COMPILABLE_SOURCES, None)?;

    // the database holds the whole project so a type imported from a sibling
    // resolves, but only the files that were *asked for* are checked and emitted.
    // compiling the project regardless of the arguments is not a harmless
    // superset: it costs every other module's build time, it fails the command
    // for a diagnostic in a file nobody named, and — because the argument is
    // ignored rather than rejected — it silently compiles a file beside the one
    // under test, which has already invalidated one delta-debugging run here
    let requested: Vec<PathBuf> = sources
        .iter()
        .map(|source| {
            if source.is_absolute() {
                source.clone()
            } else {
                cwd.join(source)
            }
        })
        .map(|source| source.canonicalize().unwrap_or(source))
        .collect();
    let handles: Vec<_> = project
        .iter()
        .filter(|(path, _)| {
            files.is_empty()
                || requested.iter().any(|wanted| {
                    *wanted == **path || path.canonicalize().is_ok_and(|p| p == *wanted)
                })
        })
        .cloned()
        .collect();
    if handles.is_empty() {
        eprintln!("no .by or .py files found");
        return Ok(ExitStatus::Success);
    }

    // a source with nothing to lower blocks, the way it does for `build` and
    // `transpile` — it could not be parsed, or could not be read at all. a *type*
    // diagnostic is advisory: many valid basedpython type forms still read as errors
    // to ty, and the compiler degrades to the object protocol rather than being
    // wrong about them
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut blocked = false;
    for (_, file) in &handles {
        let found = db.check_file(*file);
        if found.iter().any(is_unusable_source) {
            blocked = true;
        }
        diagnostics.extend(found);
    }
    if blocked {
        render_diagnostics(&db, &diagnostics)?;
        return Ok(ExitStatus::Failure);
    }
    if !diagnostics.is_empty() {
        render_diagnostics(&db, &diagnostics)?;
    }

    // what each source will be compiled as, worked out before anything is written:
    // two sources that land on the same artefact used to leave only the second, and
    // nothing said so
    let mut planned: Vec<(&(PathBuf, ruff_db::files::File), by_ir::ModuleName)> =
        Vec::with_capacity(handles.len());
    let mut claimed: HashMap<PathBuf, &Path> = HashMap::new();
    for handle in &handles {
        let (path, file) = handle;
        let Some(name) = compiled_module_name(&db, path, *file)? else {
            eprintln!(
                "skipping {}: it is the body of the package `{}` names, \
                 and no import path reaches that directory",
                path.display(),
                path.parent().unwrap_or(path).display()
            );
            continue;
        };
        // keyed on the artefact rather than on the name, because the artefact is
        // what would be overwritten — and a file the resolver cannot name falls
        // back to its stem, which two directories can share
        let artifact = name.relative_path("");
        if let Some(first) = claimed.insert(artifact, path.as_path()) {
            anyhow::bail!(
                "`{}` and `{}` would both be compiled as the module `{}`, \
                 and the second would replace the first's artifact",
                first.display(),
                path.display(),
                name.dotted()
            );
        }
        planned.push((handle, name));
    }

    for ((path, file), name) in planned {
        let source = fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;

        let program_file = ty_python_semantic::Db::program_file(&db, *file);
        let parsed = ruff_db::parsed::parsed_module(&db, program_file.python_file(&db)).load(&db);
        let model = ty_python_semantic::SemanticModel::new(&db, program_file);
        // a `.py` source needs no transpiling to be its own interpreted fallback
        let mut options = options.clone();
        if path.extension().is_some_and(|x| x == "py") {
            options.language = by_irbuild::Language::Python;
        }
        let mut lowered = by_irbuild::build_module(
            &db,
            &model.program_environment(),
            &model,
            parsed.suite(),
            name,
            options.language,
        );
        // the real path, so a `#line` in the generated C resolves for a debugger
        let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        lowered.lines = Some(by_ir::function::LineTable::new(
            absolute.display().to_string(),
            &source,
        ));

        let built = if emit_c_only {
            by_build::emit_lowered(lowered, &source, Some(&toolchain), &out_dir, &options)
        } else {
            by_build::build_lowered(lowered, &source, &toolchain, &out_dir, &options).inspect(
                |built| {
                    eprintln!(
                        "{} -> {}",
                        path.display(),
                        built.artifact.extension.display()
                    );
                },
            )
        }
        .with_context(|| format!("could not compile {}", path.display()))?;

        if let Some(annotation) = &built.artifact.annotation {
            eprintln!("  annotated {}", annotation.display());
        }
        declined_total += built.declined.len();
        if verbose {
            // a decline is the compiler's report on the code it did *not* take, so
            // it points at that code the way every other diagnostic does
            let diagnostics: Vec<Diagnostic> = built
                .declined
                .iter()
                .map(|declined| declined_diagnostic(*file, declined))
                .collect();
            render_diagnostics(&db, &diagnostics)?;
        }
        compiled += 1;
    }

    eprintln!("\ncompiled {compiled} module(s)");
    if declined_total > 0 {
        let hint = if verbose {
            ""
        } else {
            " (--verbose to list them)"
        };
        eprintln!("{declined_total} function(s) left to the interpreted definition{hint}");
    }
    Ok(ExitStatus::Success)
}

// ── transpile ────────────────────────────────────────────────────────────────

#[allow(clippy::print_stdout)]
pub(crate) fn cmd_transpile(
    file: Option<&PathBuf>,
    reverse: bool,
    min_version: Option<&str>,
    lowering: &LoweringArgs,
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let mut config = version_config(min_version, &cwd)?;
    lowering.apply(&mut config, &cwd)?;
    let target = config.min_version.to_string();
    crate::by_stamps::fill_discovered(&mut config.stamps, &cwd, Some(&target));

    // a directory argument transpiles the whole tree in place: forward turns
    // every `.by` into a `.py` (type-aware, one shared project db); reverse
    // turns every `.py` into a `.by`. this is the project-level counterpart to
    // the single-file/stdin path below
    if let Some(p) = file {
        if p.is_dir() {
            return cmd_transpile_dir(p, reverse, &config);
        }
    }

    let (source, path) = match file {
        Some(p) => {
            // a python source may declare its own encoding, which is decoded on
            // the way in — everything downstream speaks utf-8
            let bytes = fs::read(p).with_context(|| format!("{}", p.display()))?;
            let decoded = crate::by_source_encoding::decode(&bytes)
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("{}", p.display()))?;
            (decoded.text, Some(p.as_path()))
        }
        None => {
            let mut s = String::new();
            io::stdin()
                .read_to_string(&mut s)
                .context("failed to read stdin")?;
            (s, None)
        }
    };

    let is_python = path
        .map(|p| {
            p.extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|e| matches!(e, "py" | "pyi"))
        })
        .unwrap_or(false);
    let is_stub = path
        .map(|p| {
            p.extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|e| matches!(e, "pyi" | "byi"))
        })
        .unwrap_or(false);
    let config = Config {
        is_python,
        is_stub,
        ..config
    };

    let output = if reverse {
        by_transforms::reverse_transpile(&source, &config).map_err(|e| anyhow::anyhow!("{e}"))?
    } else if let Some(p) = path.filter(|_| !config.is_python) {
        // run ty's full check on the file so that diagnostics (parse
        // errors, type errors, etc.) render in the same form as
        // `by check`. parse errors abort transpile; other diagnostics
        // are displayed but non-fatal — many basedpython type forms
        // (literal-type promotion, `&` intersection, etc.) look like
        // type errors to ty but are valid in `.by` source
        let abs = std::fs::canonicalize(p).with_context(|| format!("{}", p.display()))?;
        let sys_path = SystemPath::from_std_path(&abs)
            .with_context(|| format!("non-utf8 path: {}", abs.display()))?;
        let project_root = sys_path.parent().unwrap_or(sys_path);
        let system = OsSystem::new(project_root);
        let project_metadata = ProjectMetadata::discover(project_root, &system)
            .with_context(|| format!("failed to discover project at {project_root}"))?;
        let rebuilder = Rebuilder::for_sources(
            project_metadata.clone(),
            project_root.to_path_buf(),
            vec![sys_path.to_path_buf()],
        );
        let mut db = ProjectDatabase::use_defaults(project_metadata, system);
        let file = system_path_to_file(&db, sys_path)
            .with_context(|| format!("file not found in db: {sys_path}"))?;

        // mirror `by check <path>`: explicitly include the target so
        // it's always checked regardless of the project's include
        // configuration
        db.project()
            .set_included_paths(&mut db, vec![sys_path.to_path_buf()]);

        let mut diagnostics = db.check_file(file);
        let has_parse_error = diagnostics.iter().any(|d| {
            matches!(d.id(), DiagnosticId::InvalidSyntax) && d.severity() >= Severity::Error
        });

        if has_parse_error {
            render_diagnostics(&db, &diagnostics)?;
            return Ok(ExitStatus::Failure);
        }

        let rebuild = || Some(rebuilder.rebuild());
        match by_transforms::transpile_typed(&db, file, &config, Some(&rebuild)) {
            Ok(out) => {
                if !diagnostics.is_empty() {
                    render_diagnostics(&db, &diagnostics)?;
                }
                out
            }
            Err(e) => {
                diagnostics.push(transpile_bug_diagnostic(file, &e));
                render_diagnostics(&db, &diagnostics)?;
                return Ok(ExitStatus::Failure);
            }
        }
    } else {
        by_transforms::transpile(&source, &config).map_err(|e| anyhow::anyhow!("{e}"))?
    };

    print!("{output}");
    Ok(ExitStatus::Success)
}

// ── directory transpile ───────────────────────────────────────────────────────

fn cmd_transpile_dir(dir: &Path, reverse: bool, config: &Config) -> anyhow::Result<ExitStatus> {
    if reverse {
        reverse_dir(dir, config)
    } else {
        forward_dir(dir, config)
    }
}

/// Reverse every `.py`/`.pyi` under `dir` into a `.by`/`.byi` in place,
/// deleting the original. Reverse transforms are single-file, so no project db
/// is needed.
///
/// A source the transpiler cannot read, cannot convert, or that panics the
/// checker underneath it is reported and skipped rather than taking the project
/// down with it — the same way ruff reports an unreadable file as an `IOError`
/// diagnostic and lints the rest, and the same way `by check` turns a panic
/// while checking one file into a diagnostic against that file. Converting a
/// tree is not all-or-nothing: one file the transpiler chokes on must not cost
/// the caller every other file's conversion. Each file's original is removed
/// only once its replacement is on disk, so nothing is ever left both deleted
/// and unconverted.
fn reverse_dir(dir: &Path, config: &Config) -> anyhow::Result<ExitStatus> {
    reverse_dir_converting(dir, config, by_transforms::reverse_transpile)
}

/// [`reverse_dir`], with the conversion of a single source given by the caller so
/// a test can supply one that fails or panics on demand.
#[allow(clippy::print_stderr)]
fn reverse_dir_converting(
    dir: &Path,
    config: &Config,
    convert: impl Fn(&str, &Config) -> Result<String, String> + std::panic::RefUnwindSafe,
) -> anyhow::Result<ExitStatus> {
    let files = py_source_files(dir);
    if files.is_empty() {
        eprintln!("no .py files found");
        return Ok(ExitStatus::Success);
    }

    let mut count = 0usize;
    let mut skipped: Vec<(&Path, String)> = Vec::new();
    let mut recoded: Vec<(&Path, String)> = Vec::new();
    for py in &files {
        // a source that is not valid utf-8 is still valid python — PEP 263 lets
        // a file declare its own encoding — so it is decoded here rather than
        // skipped. everything downstream speaks utf-8, so the declaration the
        // file carried stops being true of it and is rewritten to say so
        let bytes = match fs::read(py) {
            Ok(bytes) => bytes,
            Err(e) => {
                skipped.push((py.as_path(), e.to_string()));
                continue;
            }
        };
        let source = match crate::by_source_encoding::decode(&bytes) {
            Ok(decoded) => {
                if let Some(from) = decoded.recoded_from {
                    recoded.push((py.as_path(), from));
                }
                decoded.text
            }
            Err(e) => {
                skipped.push((py.as_path(), e));
                continue;
            }
        };
        let is_stub = py.extension().and_then(OsStr::to_str) == Some("pyi");
        let file_config = Config {
            is_python: true,
            is_stub,
            ..config.clone()
        };
        let converted = ruff_db::panic::catch_unwind(|| convert(&source, &file_config));
        let output = match converted {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                skipped.push((py.as_path(), e));
                continue;
            }
            Err(panic) => {
                skipped.push((py.as_path(), panic.to_string()));
                continue;
            }
        };
        let by = py.with_extension(if is_stub { "byi" } else { "by" });
        fs::write(&by, output).with_context(|| format!("{}", by.display()))?;
        fs::remove_file(py).with_context(|| format!("{}", py.display()))?;
        count += 1;
    }

    for (py, from) in &recoded {
        eprintln!("re-encoded {} from {from} to utf-8", py.display());
    }
    for (py, message) in &skipped {
        eprintln!("skipped {}: {message}", py.display());
    }
    eprintln!("reversed {count} file(s) to basedpython");
    if skipped.is_empty() {
        return Ok(ExitStatus::Success);
    }
    eprintln!("skipped {} file(s)", skipped.len());
    Ok(ExitStatus::Failure)
}

/// Forward-transpile every `.by` under `dir` into a `.py` next to it, using one
/// shared project db so cross-module types resolve (the same path as `by
/// build`, but written in place rather than to `out/`).
#[allow(clippy::print_stderr)]
fn forward_dir(dir: &Path, config: &Config) -> anyhow::Result<ExitStatus> {
    let (db, handles, rebuilder, _root) = build_project_db(dir, BY_SOURCES, None)?;
    if handles.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }

    let ok = render_check_and_transpile(
        &db,
        &handles,
        config,
        CheckGate::ParseErrorsOnly,
        &rebuilder,
        &mut by_transforms::RuntimeRequirements::default(),
        |emitted| {
            let py = emitted.by_path.with_extension("py");
            fs::write(&py, emitted.python).with_context(|| format!("{}", py.display()))?;
            Ok(())
        },
    )?;
    Ok(if ok {
        ExitStatus::Success
    } else {
        ExitStatus::Failure
    })
}

/// Every first-party `.py`/`.pyi` file under `root`, skipping non-source
/// directories and symlinks.
fn py_source_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(may_contain_sources)
        .filter_map(Result::ok)
        .filter(|e| {
            let p = e.path();
            !e.path_is_symlink()
                && p.extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|x| matches!(x, "py" | "pyi"))
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

// ── traceback rewriting ────────────────────────────────────────────────────────

/// filename of the python entry-point shim `by run` writes into the build dir
const BY_RUNNER_FILENAME: &str = "_by_runner.py";

/// write the sourcemap module + runner shim into the run dir. the shim runs the
/// target module and, on an uncaught exception, rewrites traceback frames in
/// generated files back to their `.by` source location
fn write_traceback_runtime(
    staging: &mut Staging,
    entries: &[TracebackEntry],
) -> anyhow::Result<()> {
    write_sourcemap_module(staging, entries)?;
    // through the staging, so that a project of its own with this name is
    // reported as the collision it is rather than silently overwritten by a shim
    // it knows nothing about
    staging.write(Path::new(BY_RUNNER_FILENAME), None, BY_RUNNER_SRC)
}

const BY_RUNNER_SRC: &str = r#"# generated by `by run` — runs the target module with .by-aware tracebacks
import hashlib
import importlib.util
import linecache
import os
import sys
import traceback
import types

from _by_sourcemap import DIGESTS, SOURCEMAP

# index the sourcemap by realpath so symlinked temp dirs (e.g. /tmp on macOS)
# still match the filenames python reports in frames. the key the entry was
# written under travels with it, because DIGESTS is keyed the unresolved way
_BY_MAP = {os.path.realpath(py): (py, info) for py, info in SOURCEMAP.items()}

# entries already checked against their digests, so each file is read at most
# once no matter how many frames it contributes
_CURRENT = set()
_STALE = set()


def _digest_matches(path, spec):
    # the algorithm is named in the value, so one this reader does not know is a
    # refusal rather than hex it could never have produced
    algorithm, _, expected = spec.partition(":")
    try:
        digest = hashlib.new(algorithm)
    except ValueError:
        return False
    try:
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(65536), b""):
                digest.update(chunk)
    except OSError:
        return False
    return digest.hexdigest() == expected


def _is_current(key, by_path):
    # the map describes a *pair* of files, and a .by line read out of it is only
    # true while both are still the ones it was built from. a source saved since
    # the transpile would otherwise be quoted at line numbers belonging to the
    # file it replaced — a wrong answer, where leaving the frame generated is
    # merely a worse one
    if key in _CURRENT:
        return True
    if key in _STALE:
        return False
    digests = DIGESTS.get(key)
    if (
        digests is not None
        and _digest_matches(by_path, digests["by"])
        and _digest_matches(key, digests["py"])
    ):
        _CURRENT.add(key)
        return True
    _STALE.add(key)
    sys.stderr.write(
        f"note: {by_path} no longer matches what it was transpiled from — "
        "its frames are left as generated python\n"
    )
    return False


def _rewrite(frames):
    # drop the runner/runpy bootstrap above the first user frame
    first = next((i for i, f in enumerate(frames) if os.path.realpath(f.filename) in _BY_MAP), None)
    frames = frames[first:] if first is not None else frames
    out = []
    for f in frames:
        entry = _BY_MAP.get(os.path.realpath(f.filename))
        if entry is not None and f.lineno is not None:
            key, (by_path, lines) = entry
            idx = f.lineno - 1
            mapped = lines[idx] if 0 <= idx < len(lines) else None
            if mapped is not None and _is_current(key, by_path):
                by_lineno = mapped + 1
                text = linecache.getline(by_path, by_lineno).strip() or f.line
                out.append(traceback.FrameSummary(by_path, by_lineno, f.name, line=text))
                continue
        out.append(f)
    return out


def _excepthook(etype, evalue, tb):
    frames = _rewrite(traceback.extract_tb(tb))
    sys.stderr.write("Traceback (most recent call last):\n")
    sys.stderr.write("".join(traceback.StackSummary.from_list(frames).format()))
    sys.stderr.write("".join(traceback.format_exception_only(etype, evalue)))


def _by_source_of(origin):
    # the .by file a staged .py was transpiled from, or None for anything else —
    # a hand-written .py carried over verbatim, or a module from site-packages
    if not origin:
        return None
    entry = _BY_MAP.get(os.path.realpath(origin))
    return None if entry is None else entry[1][0]


class _ByLoader:
    """Wraps a staged module's loader to name its `.by` source as `__file__`.

    The program is run out of a directory of transpiled copies, so every path
    python derives from a module's origin — `__file__`, and anything a tool walks
    up from it to find — names a temporary file in a directory that is deleted
    when the run ends. The `.by` the module was written as is the answer to every
    question those paths are asked, so it is what the module reports.

    Only `__file__` moves. The code object still comes from the `.py`, so the
    filename in a traceback frame is still the one `SOURCEMAP` is keyed by and
    frames are still rewritten to `.by` lines the same way.
    """

    def __init__(self, inner, by_path):
        self._by_inner = inner
        self._by_path = by_path

    def create_module(self, spec):
        return self._by_inner.create_module(spec)

    def exec_module(self, module):
        module.__file__ = self._by_path
        self._by_inner.exec_module(module)

    def __getattr__(self, name):
        return getattr(self._by_inner, name)


class _ByFinder:
    """Puts a `_ByLoader` on every staged module, whoever ends up finding it."""

    def find_spec(self, fullname, path=None, target=None):
        for finder in sys.meta_path:
            if finder is self:
                continue
            find = getattr(finder, "find_spec", None)
            if find is None:
                continue
            spec = find(fullname, path, target)
            if spec is None:
                continue
            by_path = _by_source_of(getattr(spec, "origin", None))
            if by_path is not None and spec.loader is not None:
                spec.loader = _ByLoader(spec.loader, by_path)
            return spec
        return None


def _entry_spec(module):
    """The spec whose code `by run <module>` should execute.

    A package is run through its `__main__` submodule, the way `python -m` does:
    the package's own `__init__` is what an *import* of it runs, and running that
    instead would execute the wrong file and never reach the program.
    """
    spec = importlib.util.find_spec(module)
    if spec is None:
        raise ImportError("No module named " + repr(module), name=module)
    if spec.submodule_search_locations is not None:
        module = module + ".__main__"
        try:
            spec = importlib.util.find_spec(module)
        except ImportError as exc:
            raise ImportError(
                repr(module) + " is a package and cannot be directly executed", name=module
            ) from exc
        if spec is None:
            raise ImportError(
                repr(module) + " is a package and cannot be directly executed", name=module
            )
    if spec.loader is None:
        raise ImportError("module " + repr(module) + " has no loader", name=module)
    return module, spec


def _run_as_main(module):
    """Run `module` as `__main__`, with `.by` paths for `__file__` and `argv[0]`.

    `runpy.run_module` fetches the entry module's code straight from its loader
    rather than executing it through one, so the loader wrapper that fixes every
    imported module never sees this one. Its few lines are spelled out here
    instead, which is also the only place `sys.argv[0]` can be set: `alter_sys`
    would otherwise overwrite it with the staged path on the way in.
    """
    module, spec = _entry_spec(module)
    code = spec.loader.get_code(module)
    if code is None:
        raise ImportError("module " + repr(module) + " has no code to run", name=module)
    by_path = _by_source_of(spec.origin) or spec.origin
    sys.argv[0] = by_path
    main_module = types.ModuleType("__main__")
    main_module.__file__ = by_path
    main_module.__loader__ = spec.loader
    main_module.__package__ = spec.parent
    # the real spec, which is what `runpy` puts here too. it has to name the
    # module rather than be left unset: `multiprocessing`'s spawn start method —
    # the default on macos and windows — reads `__spec__.name` to tell the child
    # what to re-import, and falls back to running `__file__` as a *path* when
    # there is none. that path is now the `.by`, which python cannot compile, so
    # leaving this unset broke every spawned child
    main_module.__spec__ = spec
    sys.modules["__main__"] = main_module
    exec(code, main_module.__dict__)


def main():
    sys.excepthook = _excepthook
    sys.meta_path.insert(0, _ByFinder())
    module = sys.argv[1]
    sys.argv = sys.argv[1:]
    try:
        _run_as_main(module)
    except SystemExit:
        raise
    except BaseException:
        sys.excepthook(*sys.exc_info())
        sys.exit(1)


main()
"#;

// ── helpers ──────────────────────────────────────────────────────────────────

/// every source under `root` the compiler can lower
///
/// it lowers the `.by` *and* the `.py` ast — one lowering, told apart by
/// [`by_irbuild::Language`]. only the commands that *write beside* a source
/// ([`cmd_transpile`]) are `.by`-only, because a `.py` there would be its own
/// output
fn compilable_files(root: &Path) -> Vec<PathBuf> {
    source_files(root, &["by", "py"])
}

/// Check every file, render diagnostics, then for each non-blocked file call
/// `consume` with the transpiled Python. Returns `Ok(false)` if the check
/// outcome blocks per `gate`, or a transpiler bug occurred (caller should
/// propagate failure).
///
/// The deciding is [`by_stage::emit::check_and_transpile`], and only the
/// **rendering** is here. That is the whole difference between the two callers
/// of it: a command prints diagnostics for a person to read, and the language
/// server hands them back as data for an editor to draw. Two copies of the
/// deciding would be two answers to "what does this project transpile to", and
/// the one thing a re-stage has to promise is that its bytes are the bytes the
/// build wrote.
fn render_check_and_transpile(
    db: &ProjectDatabase,
    handles: &[(PathBuf, ruff_db::files::File)],
    config: &Config,
    gate: CheckGate,
    rebuilder: &Rebuilder,
    requirements: &mut by_transforms::RuntimeRequirements,
    consume: impl FnMut(&Transpiled<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let emitted = by_stage::emit::check_and_transpile(
        db,
        handles,
        config,
        gate,
        rebuilder,
        requirements,
        consume,
    )?;
    if !emitted.diagnostics.is_empty() {
        render_diagnostics(db, &emitted.diagnostics)?;
    }
    Ok(emitted.ok)
}

/// Render diagnostics to stderr in the same format as `by check`. The
/// transpiled output goes to stdout, so diagnostics belong on stderr to keep
/// the two streams separable.
#[allow(clippy::print_stderr)]
fn render_diagnostics(db: &ProjectDatabase, diagnostics: &[Diagnostic]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let display_config = DisplayDiagnosticConfig::new("ty")
        .color(colored::control::SHOULD_COLORIZE.should_colorize())
        .context(0);
    let mut stderr = std::io::stderr().lock();
    write!(
        stderr,
        "{}",
        DisplayDiagnostics::new(db, &display_config, diagnostics)
    )?;
    let n = diagnostics.len();
    writeln!(
        stderr,
        "Found {n} diagnostic{}",
        if n == 1 { "" } else { "s" }
    )?;
    Ok(())
}

/// Build a diagnostic for a transpile failure, annotated against the `.by`
/// source. When the failure maps back to a `.by` range, attach it so the
/// diagnostic renders with `--> file:line:col` and a source caret like any
/// other; otherwise fall back to a bare message.
/// a function left to its interpreted definition, as a diagnostic
fn declined_diagnostic(
    file: ruff_db::files::File,
    declined: &by_ir::function::Declined,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::new(
        DiagnosticId::Lint(LintName::of("declined")),
        Severity::Info,
        format!("`{}` was left to its interpreted definition", declined.name),
    );
    if let Some((start, end)) = declined.range {
        diagnostic.annotate(
            Annotation::primary(
                Span::from(file).with_range(TextRange::new(start.into(), end.into())),
            )
            .message(declined.reason.clone()),
        );
    } else {
        diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            declined.reason.clone(),
        ));
    }
    diagnostic
}

// ── version ──────────────────────────────────────────────────────────────────

#[allow(clippy::print_stdout)]
pub(crate) fn cmd_version_by(output_format: crate::args::HelpFormat) -> ExitStatus {
    let version = env!("CARGO_PKG_VERSION");
    match output_format {
        crate::args::HelpFormat::Text => println!("by {version}"),
        crate::args::HelpFormat::Json => println!("{{\"version\":\"{version}\"}}"),
    }
    ExitStatus::Success
}

#[cfg(test)]
mod tests {
    use super::{dotted_module_name, reverse_dir, reverse_dir_converting};
    // the staging half of what these used to exercise lives in `by_stage` now,
    // because the language server needs the same answers and cannot depend on this
    // crate — and so do the tests for it
    use crate::ExitStatus;
    use by_transforms::config::Config;
    use std::path::{Path, PathBuf};

    /// a compiled module carries its name into every type it emits, and cpython
    /// reads a class's `__module__` off the front of that — so the name of a file
    /// inside the tree is the whole path to it, not the file's own stem
    #[test]
    fn a_file_inside_the_tree_is_named_for_its_whole_path() {
        assert_eq!(
            dotted_module_name(Path::new("pkg/sub/main.py")),
            Some(by_ir::ModuleName::new("pkg.sub.main"))
        );
        assert_eq!(
            dotted_module_name(Path::new("top.py")),
            Some(by_ir::ModuleName::new("top"))
        );
    }

    /// a package's `__init__` is not a module beside the package, it *is* the
    /// package — which is the module a class written in it belongs to
    #[test]
    fn a_packages_init_is_named_for_the_package() {
        let name = dotted_module_name(Path::new("pkg/__init__.py"));
        assert_eq!(name, Some(by_ir::ModuleName::package("pkg")));
        // and its artefact is the `__init__` inside the package, not a file
        // called `pkg` beside it — that is the only place cpython's finder looks
        assert_eq!(
            name.map(|name| name.relative_path(".so")),
            Some(PathBuf::from("pkg/__init__.so"))
        );
        // and at the root there is no package for it to be, so there is no name
        assert_eq!(dotted_module_name(Path::new("__init__.py")), None);
    }

    /// a source that declares its own encoding converts like any other, and the
    /// declaration is rewritten to name what the converted file actually holds
    #[test]
    fn a_source_that_declares_its_encoding_converts() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        // PEP 263: a declared latin-1 encoding, and a byte no utf-8 decoder accepts
        let latin1 = dir.path().join("latin_1.py");
        std::fs::write(&latin1, b"# -*- coding: latin-1 -*-\ns = '\xdf'\n")?;

        let status = reverse_dir(dir.path(), &Config::default())?;

        assert!(matches!(status, ExitStatus::Success));
        assert!(!latin1.exists());
        let converted = std::fs::read_to_string(dir.path().join("latin_1.by"))?;
        assert!(
            converted.contains("coding: utf-8"),
            "the declaration still names an encoding the file no longer has: {converted}"
        );
        assert!(
            converted.contains('\u{df}'),
            "the text did not survive decoding: {converted}"
        );
        Ok(())
    }

    /// a source this build cannot decode must not take the project down with it:
    /// everything else still converts, the undecodable file is left exactly as it
    /// was found, and the exit status still says something did not convert
    #[test]
    fn a_source_that_cannot_be_decoded_is_skipped_not_fatal() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let good = dir.path().join("good.py");
        std::fs::write(&good, "x = 1\n")?;
        // an encoding with no built-in decoder, and bytes no utf-8 decoder accepts
        let jis = dir.path().join("jis.py");
        std::fs::write(&jis, b"# coding: shift_jis\ns = '\x82\xa0'\n")?;

        let status = reverse_dir(dir.path(), &Config::default())?;

        assert!(matches!(status, ExitStatus::Failure));
        assert!(dir.path().join("good.by").is_file());
        assert!(!good.exists());
        // untouched: neither converted nor deleted
        assert!(jis.is_file());
        assert!(!dir.path().join("jis.by").exists());
        Ok(())
    }

    /// a source whose conversion the transpiler gives up on is skipped the same
    /// way an unreadable one is, rather than costing every other file in the tree
    /// its conversion
    #[test]
    fn a_source_the_transpiler_rejects_is_skipped_not_fatal() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        std::fs::write(dir.path().join("good.py"), "x = 1\n")?;
        let bad = dir.path().join("bad.py");
        std::fs::write(&bad, "y = 2\n")?;

        let status = reverse_dir_converting(dir.path(), &Config::default(), |source, _| {
            if source.starts_with('y') {
                Err("nope".to_string())
            } else {
                Ok(source.to_string())
            }
        })?;

        assert!(matches!(status, ExitStatus::Failure));
        assert!(dir.path().join("good.by").is_file());
        assert!(bad.is_file());
        assert!(!dir.path().join("bad.by").exists());
        Ok(())
    }

    /// the checker underneath the transpiler can panic on one file — a salsa cycle
    /// that will not converge, say. that must cost the caller that file, not the
    /// whole tree, exactly as `by check` reports a panic against the file it was
    /// checking and carries on
    #[test]
    fn a_source_that_panics_the_checker_is_skipped_not_fatal() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        std::fs::write(dir.path().join("good.py"), "x = 1\n")?;
        let exploding = dir.path().join("exploding.py");
        std::fs::write(&exploding, "y = 2\n")?;

        let status = reverse_dir_converting(dir.path(), &Config::default(), |source, _| {
            assert!(!source.starts_with('y'), "boom");
            Ok(source.to_string())
        })?;

        assert!(matches!(status, ExitStatus::Failure));
        assert!(dir.path().join("good.by").is_file());
        // left exactly as it was found: neither converted nor deleted
        assert!(exploding.is_file());
        assert!(!dir.path().join("exploding.by").exists());
        Ok(())
    }
}
