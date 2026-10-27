use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use by_transforms::config::{Config, PythonVersion};
use ruff_db::diagnostic::{
    Annotation, Diagnostic, DiagnosticId, DisplayDiagnosticConfig, LintName, Severity, Span,
    SubDiagnostic, SubDiagnosticSeverity,
};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_text_size::TextRange;
use ty_project::metadata::options::OutputFormat;
use ty_project::{Db, ProjectDatabase, ProjectMetadata};
use ty_site_packages::{PythonEnvironment, SysPrefixPathOrigin};
use ty_static::EnvVars;
use walkdir::WalkDir;

use crate::ExitStatus;
use crate::args::LoweringArgs;
use crate::by_lowering::SettledLowering;
use by_stage::emit::{
    CheckGate, Emit, Transpiled, is_unusable_source, transpile_failure_diagnostic,
};
use by_stage::project::{
    BY_SOURCES, COMPILABLE_SOURCES, Rebuilder, build_project_db, build_project_db_for,
    may_contain_sources, module_roots, source_files,
};
use by_stage::record::{BuildRecord, parse_soundness, stage_build_record};
use by_stage::runtime::RuntimeLayout;
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

/// How `by run` was invoked, beyond what it is running.
///
/// The choices a caller makes about the *process*: which interpreter, what
/// stands in front of it, whether its modules are compiled first, which
/// directory it runs in, and whether what it runs is measured. [`CompileFlags`] groups `by compile`'s for the same
/// reason.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RunFlags<'a> {
    /// Compile every imported module to a native extension first.
    pub(crate) compiled: bool,
    /// Run the program in the tree the run staged rather than where it was typed.
    pub(crate) in_build: bool,
    /// `--python`: the interpreter, or the environment holding it.
    pub(crate) python: Option<&'a Path>,
    /// `--launcher`: a program to start the interpreter through.
    pub(crate) launcher: Option<&'a Path>,
    /// `--coverage`: where to write an LCOV report of what ran.
    pub(crate) coverage: Option<&'a Path>,
    /// `--profile`: where to write a `pstats` snapshot of where the time went
    pub(crate) profile: Option<&'a Path>,
}

/// what `by run` and `by repl` both start from: the project checked, transpiled and staged
/// into a temporary tree, and the interpreter that runs out of it
pub(crate) struct StagedRun {
    /// where the command was invoked, which is where the program runs
    pub(crate) cwd: PathBuf,
    project: ResolvedProject,
    pub(crate) interpreter: Interpreter,
    /// what the project was transpiled for: the interpreter's version unless told otherwise
    pub(crate) config: Config,
    /// the staged tree, deleted when this is dropped
    pub(crate) tree: tempfile::TempDir,
    /// the project file behind each copy in the tree
    pub(crate) carried: BTreeMap<PathBuf, PathBuf>,
    traceback_entries: Vec<TracebackEntry>,
    /// the means to build the project's db again, unfrozen
    pub(crate) rebuilder: Rebuilder,
    /// the module `by run` runs, which a session has none of
    module: Option<String>,
}

/// what a staged tree is for
#[derive(Clone, Copy)]
pub(crate) enum RunTarget<'a> {
    /// `by run`'s program: the module named, or the project's entry point
    Module(Option<&'a str>),
    /// a `by repl` session, which runs no module of the project's
    Session,
}

/// check, transpile and stage the project around the working directory for `target`, on the
/// interpreter `python_flag` names or discovery finds. `Err` is the exit status of a project
/// that cannot run: one that does not check, having said why
#[allow(clippy::print_stderr)]
pub(crate) fn stage_run(
    min_version: Option<&str>,
    lowering: &LoweringArgs,
    python_flag: Option<&Path>,
    compiled: bool,
    run_target: RunTarget<'_>,
) -> anyhow::Result<Result<StagedRun, ExitStatus>> {
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
    let probe = probe_interpreter(&python);
    let probed = probe.as_ref().map(|probe| probe.version);
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

    // the program is checked against the packages of the python it is about to run on
    let checked_against = probe
        .filter(|_| interpreter.named_for_run)
        .map(|probe| probe.prefix);
    let (db, handles, rebuilder, root) =
        build_project_db_for(&cwd, BY_SOURCES, None, checked_against.as_deref())?;
    // a session can start in a project with nothing to stage, as python's repl starts
    // anywhere; a program has to be somewhere
    if handles.is_empty() && matches!(run_target, RunTarget::Module(_)) {
        eprintln!("no .by files found");
        return Ok(Err(ExitStatus::Failure));
    }
    let roots = module_roots(&db, &root);

    // an explicit module always wins; otherwise the project's configured entry
    // point stands in for it. resolving before the (much slower) check means a
    // project with neither fails immediately rather than after transpiling
    let module = match run_target {
        RunTarget::Module(Some(module)) => Some(module.to_owned()),
        RunTarget::Module(None) => match by_stage::run_module::configured_main(&db) {
            Some(main) => Some(main),
            None => anyhow::bail!(
                "no module given and no entry point configured — \
                 name a module (`by run main`) or set `run.main` in the project configuration"
            ),
        },
        RunTarget::Session => None,
    };

    // each generated `.py` paired with its source `.by` and the line table that
    // lifts generated line numbers back to `.by` lines (for traceback rewriting)
    let mut traceback_entries: Vec<TracebackEntry> = Vec::new();
    let mut staging = Staging::new(tmp.path());
    let mut layout = RuntimeLayout::default();
    let status = render_check_and_transpile(
        &db,
        &handles,
        OutputFormat::Full,
        &mut Emit {
            config: &config,
            gate: CheckGate::AllErrors,
            rebuilder: &rebuilder,
            requirements: &mut by_transforms::RuntimeRequirements::default(),
            runtime: Some(&mut layout),
            roots: &roots,
            root: &root,
        },
        |emitted| {
            let relative = transpiled_destination(&roots, &root, emitted.by_path);
            traceback_entries.push(stage_module(&mut staging, &relative, emitted)?);
            Ok(())
        },
    )?;
    if !matches!(status, ExitStatus::Success) {
        return Ok(Err(status));
    }
    // a program is its data as much as its modules: a `.py` module it imports, a
    // json file it opens, a template it renders. running out of a directory
    // holding only the transpiled half fails on the first of them
    stage_verbatim(&db, &root, &roots, &mut staging)?;
    stage_runtime(&mut staging, &layout)?;
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
        &BuildRecord::new(&root, &roots, module.clone(), compiled, &config),
    )?;
    // the project file behind each copy in the tree, for the runner to run it as
    // and a coverage report to name: the copy is deleted with the tree when the
    // run ends
    let carried: BTreeMap<PathBuf, PathBuf> = staging
        .entries()
        .filter_map(|(staged, source)| {
            let source = source.filter(|source| source.extension() != Some(OsStr::new("by")))?;
            Some((staged.to_path_buf(), source.to_path_buf()))
        })
        .collect();
    write_project_module(&mut staging, &root, &carried)?;
    staging.finish()?;
    Ok(Ok(StagedRun {
        cwd,
        project,
        interpreter,
        config,
        tree: tmp,
        carried,
        traceback_entries,
        rebuilder,
        module,
    }))
}

#[allow(clippy::exit, clippy::print_stderr)]
pub(crate) fn cmd_run(
    module: Option<&str>,
    args: &[String],
    min_version: Option<&str>,
    lowering: &LoweringArgs,
    flags: RunFlags<'_>,
) -> anyhow::Result<ExitStatus> {
    let RunFlags {
        compiled,
        in_build,
        python: python_flag,
        launcher,
        coverage,
        profile,
    } = flags;
    let staged = match stage_run(
        min_version,
        lowering,
        python_flag,
        compiled,
        RunTarget::Module(module),
    )? {
        Ok(staged) => staged,
        Err(status) => return Ok(status),
    };
    let StagedRun {
        cwd,
        project,
        interpreter,
        config,
        tree: tmp,
        carried,
        traceback_entries,
        rebuilder: _,
        module,
    } = staged;
    let python = interpreter.path.clone();
    let module = module.context("a program is staged for the module it runs")?;

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
        let mut options = by_build::Options {
            fallback: Some(config),
            language: by_irbuild::Language::default(),
            ..by_build::Options::default()
        };
        project.apply_compile_options(&mut options);
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
            let mut lowered = by_irbuild::module_from_source(source, name, options.lowering());
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
    // `sys.path`, so the module is still found there.
    //
    // a launcher goes in front of the interpreter rather than in place of it. a
    // debugger that has to *be* the process the program runs in used to get
    // there as `--python`, which switched this discovery off and left it to name
    // an interpreter it could only guess at — and to tell the version probe apart
    // from the program, since both were sent to it
    let mut command = match launcher {
        Some(launcher) => {
            let mut command = Command::new(launcher);
            command.arg(&python);
            command
        }
        None => Command::new(&python),
    };
    // `--in-build` runs it in that tree instead, for the programs whose
    // arguments are the project's own files: the project is python only there,
    // so `pytest tests/test_x.py` finds a file at all only from inside it — and
    // the whole project is inside it, configuration and data included, so what
    // the program makes of the tree is what it would have made of the project
    let workdir = if in_build { tmp.path() } else { cwd.as_path() };
    command.arg(tmp.path().join(BY_RUNNER_FILENAME));
    // what the runner measured, written outside the tree the program can see
    let measured = match coverage {
        Some(_) => {
            let dir = tempfile::TempDir::new().context("failed to create temp directory")?;
            let file = dir.path().join("measured.json");
            command.arg("--coverage").arg(&file);
            Some((dir, file))
        }
        None => None,
    };
    // the runner writes the snapshot itself, every function already named by its
    // project file: the program can run somewhere else, so the path is settled here
    let snapshot = match profile {
        Some(snapshot) => {
            let snapshot = cwd.join(snapshot);
            if let Some(parent) = snapshot.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("could not create {}", parent.display()))?;
            }
            // a snapshot left from an earlier run would read as this run's when
            // this one leaves none
            match fs::remove_file(&snapshot) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(error)
                        .with_context(|| format!("could not replace {}", snapshot.display()));
                }
                _ => {}
            }
            command.arg("--profile").arg(&snapshot);
            Some(snapshot)
        }
        None => None,
    };
    let status = command
        .arg(&module)
        .args(args)
        .current_dir(workdir)
        .status()
        .with_context(|| match launcher {
            Some(launcher) => format!(
                "could not run the launcher `{}` for `{python}`, the interpreter from {}",
                launcher.display(),
                interpreter.origin
            ),
            None => format!(
                "could not run `{python}`, the interpreter from {}",
                interpreter.origin
            ),
        })?;

    let mut code = status.code().unwrap_or(1);
    if let (Some(report), Some((_, file))) = (coverage, &measured) {
        if file.is_file() {
            let report = cwd.join(report);
            if let Err(error) =
                crate::by_coverage::write_report(file, &report, tmp.path(), &carried)
            {
                eprintln!("error: no coverage report was written: {error:#}");
                if code == 0 {
                    code = 1;
                }
            }
        } else if code != 2 {
            // 2 is the runner refusing to start without coverage.py, which it has
            // said already
            eprintln!(
                "note: no coverage report was written: the program ended without returning \
                 to `by run`, as `os._exit` does"
            );
        }
    }
    if let Some(snapshot) = &snapshot
        && !snapshot.is_file()
    {
        eprintln!(
            "note: no profile was written: the program ended without returning to `by run`, \
             as one that calls `os._exit`, replaces itself with `os.exec*` or is killed by a \
             signal does"
        );
    }
    drop(measured);
    // drop the temp dir explicitly: `process::exit` skips destructors, so
    // exiting while it's still in scope would leak the directory
    drop(tmp);
    std::process::exit(code);
}

/// the status `by` exits with for a program that ended with `status`: the program's own, or
/// for one a signal ended, 128 and the signal's number, as a shell reports it
pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
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

/// Write the runtime helpers into every place a staged module imports them
/// from.
///
/// One copy per package rather than one at the tree root: a distribution ships
/// packages, so a root module would either not be packaged at all or claim a
/// top-level name a second basedpython wheel overwrites on install.
/// [`by_stage::runtime`] is where that rule lives; this only puts the bytes
/// where it says.
fn stage_runtime(staging: &mut Staging, layout: &RuntimeLayout) -> anyhow::Result<()> {
    for relative in layout.files() {
        staging.write(&relative, None, by_transforms::runtime::SOURCE)?;
    }
    Ok(())
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
    probe_interpreter(&interpreter.path)
        .map_or_else(|| Config::default().min_version, |probe| probe.version)
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
pub(crate) struct Interpreter {
    pub(crate) path: String,
    pub(crate) origin: String,
    /// whether the command was told which python to run on, by `--python` or `$PYTHON`, rather
    /// than finding the project's environment: the checker finds that one itself, and an
    /// interpreter named only for the run has to be handed to it
    named_for_run: bool,
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
        return Ok(named_for_run(python, "`PYTHON`"));
    }

    Ok(discovered.unwrap_or_else(|| Interpreter {
        path: "python3".to_owned(),
        origin: "`PATH`".to_owned(),
        named_for_run: false,
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

    /// What the project's `compile` options ask of a native build.
    ///
    /// Destructured, so an option added to the group and never carried across is a
    /// compile error here rather than a setting that silently does nothing.
    fn apply_compile_options(&self, options: &mut by_build::Options) {
        let Some(ty_project::metadata::options::CompileOptions {
            follow_recursion_limit,
            bind_functions_early,
        }) = self.metadata.options().compile.as_ref()
        else {
            return;
        };
        if let Some(follow) = follow_recursion_limit {
            options.follow_recursion_limit = *follow;
        }
        if let Some(early) = bind_functions_early {
            options.bind_functions_early = *early;
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

/// what an interpreter says of itself, asked once
struct Probed {
    /// its `major.minor`, e.g. `3.9`, which `run` targets
    version: PythonVersion,
    /// its `sys.prefix`: the environment it runs in, as the checker takes one
    prefix: SystemPathBuf,
}

/// `python`'s version and the environment it runs in, or `None` when it cannot be run or what
/// it says cannot be read
///
/// the environment is the one the interpreter reports rather than one derived from where its
/// file is, so an interpreter named by path and the same one found on `PATH` are checked
/// against one environment, a launcher script that runs another included
fn probe_interpreter(python: &str) -> Option<Probed> {
    let output = Command::new(python)
        .arg("-c")
        .arg("import json, sys; print(json.dumps([sys.version_info[0], sys.version_info[1], sys.prefix]))")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let (major, minor, prefix): (u8, u8, String) = serde_json::from_slice(&output.stdout).ok()?;
    Some(Probed {
        version: PythonVersion::from((major, minor)),
        prefix: SystemPathBuf::from(prefix),
    })
}

// ── build ────────────────────────────────────────────────────────────────────

#[allow(clippy::print_stderr)]
/// Recompute the slots of a set of edited files in a build tree that already
/// exists, and print them.
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
pub(crate) fn cmd_restage(build_directory: &Path, files: &[PathBuf]) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let files: Vec<PathBuf> = files
        .iter()
        .map(|file| {
            if file.is_absolute() {
                file.clone()
            } else {
                cwd.join(file)
            }
        })
        .collect();
    let (db, _, _, _) = build_project_db(&cwd, BY_SOURCES, Some(build_directory))?;

    let restaged = by_stage::restage::restage(&db, build_directory, &files)?;
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
    out: Option<&Path>,
    print_manifest: bool,
    output_format: OutputFormat,
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let mut config = version_config(min_version, &cwd)?;
    lowering.apply_for_build(&mut config, &cwd)?;
    let target = config.min_version.to_string();
    crate::by_stamps::fill_discovered(&mut config.stamps, &cwd, Some(&target));

    let out = output_dir(&cwd, out)?;

    let (db, handles, rebuilder, root) = build_project_db(&cwd, BY_SOURCES, Some(&out))?;
    if handles.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }
    let file_count = handles.len();
    let roots = module_roots(&db, &root);
    let mut staging = Staging::new(&out);
    // `build/` outlives the build that wrote it — it is what a test runner, a
    // debugger or an editor plugin sees — so the sourcemap goes with it. this is
    // the directory where a `.by` really can be saved after the transpile, which
    // is what the digests beside the map are for
    let mut entries: Vec<TracebackEntry> = Vec::new();
    let mut requirements = by_transforms::RuntimeRequirements::default();
    let mut layout = RuntimeLayout::default();
    let status = render_check_and_transpile(
        &db,
        &handles,
        output_format,
        &mut Emit {
            config: &config,
            gate: CheckGate::ParseErrorsOnly,
            rebuilder: &rebuilder,
            requirements: &mut requirements,
            runtime: Some(&mut layout),
            roots: &roots,
            root: &root,
        },
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
    // so they are staged whether or not something was reported — a `build/` a
    // debugger cannot read is worse than one built from a partial check
    stage_verbatim(&db, &root, &roots, &mut staging)?;
    stage_runtime(&mut staging, &layout)?;
    stage_by_typed_markers(&db, &mut staging, &roots, &root)?;
    write_sourcemap_module(&mut staging, &entries)?;
    // `build/` outlives the build that wrote it and is what a debugger, a test
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

    let status = status?;
    if !matches!(status, ExitStatus::Success) {
        return Ok(status);
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

/// Where a command that writes the build tree writes it: `out` when it was given, against the
/// working directory it was typed in; otherwise the project's own build directory, wherever in the
/// project the command runs.
///
/// The default is the project's for the reason the sources are: `by build` in `tests/` builds the
/// whole project, and writing that into `tests/build` put a second tree where nothing looks for
/// it — not the language server's `by/buildOutput`, which names one build directory per project,
/// and not the next `by build` run from the root. [`by_stage::layout::default_build_directory`] is
/// the one answer both of them read.
fn output_dir(cwd: &Path, out: Option<&Path>) -> anyhow::Result<PathBuf> {
    Ok(match out {
        Some(out) => settled_output_dir(cwd, out),
        None => {
            let project = ResolvedProject::discover(cwd)?;
            settled_output_dir(
                cwd,
                &by_stage::layout::default_build_directory(project.root()),
            )
        }
    })
}

/// Where a build's output goes, settled before the project is read.
///
/// It is the one directory the project must not be read *from*: it holds a copy
/// of every source the build is about to write, so a second build that walked it
/// would take its own output for more of the project. Canonical, because the
/// paths it is compared against are.
///
/// Nothing is created here. An output directory conjured before the command has
/// anything to put in it outlives every way the command can fail early, and a
/// project that has just been told it has no sources should not be left holding
/// an empty `build/` it did not ask for. A directory that does not exist yet
/// cannot be canonicalized, so its parent is, which answers the same question:
/// two paths naming this directory compare equal.
fn settled_output_dir(cwd: &Path, out: &Path) -> PathBuf {
    let out = cwd.join(out);
    if let Ok(canonical) = fs::canonicalize(&out) {
        return canonical;
    }
    match (out.parent(), out.file_name()) {
        (Some(parent), Some(name)) => match fs::canonicalize(parent) {
            Ok(canonical) => canonical.join(name),
            Err(_) => out,
        },
        _ => out,
    }
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
    output: Option<&Path>,
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
    // the python this writes into the tree is `by build`'s python, derived the
    // way `build` derives it: from the version the *project* declares.
    //
    // it must not come from the interpreter the extensions are built against.
    // the two commands share an output directory, so the module `build` writes
    // and the module `compile` writes are the same file — and a project that
    // declares `>=3.9` would get a tree lowered for 3.9 or for 3.13 depending on
    // which command ran last. `build/` is also where an editable install points,
    // so the losing half of that is code the project says it supports and no
    // longer runs on
    let mut tree_config = version_config(None, &cwd)?;
    lowering.apply_for_build(&mut tree_config, &cwd)?;
    let target = tree_config.min_version.to_string();
    crate::by_stamps::fill_discovered(&mut tree_config.stamps, &cwd, Some(&target));
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
    project.apply_compile_options(&mut options);
    let python = discover_interpreter(None, &project)?.path;
    // see the note on the other `probe` call: its own errors are self-describing, and the
    // context that used to sit here misreported a version refusal as a missing header
    let toolchain = by_build::Toolchain::probe(&python)?;

    let out_dir = output_dir(&cwd, output)?;
    let mut compiled = 0usize;
    let mut declined_total = 0usize;
    // the output tree. `by_build` lays the artefacts out itself, so they are
    // recorded rather than written through this — but they belong in the same
    // manifest as everything else, or nothing ever takes a stale one back
    let mut staging = Staging::new(&out_dir);

    // one database for the whole project, so a type imported from a sibling module
    // resolves. lowering each file on its own is sound — an unresolved class
    // degrades to the object protocol — but it makes every imported type look
    // gradual, and `--no-any` would then fail on noise
    let (db, project, rebuilder, root) =
        build_project_db(&cwd, COMPILABLE_SOURCES, Some(&out_dir))?;

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

    // `compile` writes a superset of what `build` writes: the whole project as
    // importable python, and native extensions for the modules that were asked
    // for. two things follow from it, and neither is optional.
    //
    // the tree is a program rather than a heap of artefacts — a module nobody
    // named still imports, from the python `by build` would have written, so
    // `by compile one.by` no longer produces a tree that can import `one` and
    // nothing else written in basedpython.
    //
    // and the manifest becomes sound. `finish` deletes what the last run wrote
    // and this one did not, which is only a correct thing to do when this run
    // authored the whole tree. while `compile` wrote artefacts alone, it deleted
    // the sourcemap and build record a `by build` into the same directory had
    // left — and `by restage` then refused that tree, which took the language
    // server's re-stage down with it
    let roots = module_roots(&db, &root);
    let transpilable: Vec<(PathBuf, ruff_db::files::File)> = project
        .iter()
        .filter(|(path, _)| {
            path.extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| BY_SOURCES.contains(&extension))
        })
        .cloned()
        .collect();
    let mut entries: Vec<TracebackEntry> = Vec::new();
    let mut requirements = by_transforms::RuntimeRequirements::default();
    let mut layout = RuntimeLayout::default();
    let transpiled = render_check_and_transpile(
        &db,
        &transpilable,
        OutputFormat::Full,
        &mut Emit {
            config: &tree_config,
            gate: CheckGate::ParseErrorsOnly,
            rebuilder: &rebuilder,
            requirements: &mut requirements,
            runtime: Some(&mut layout),
            roots: &roots,
            root: &root,
        },
        |emitted| {
            let relative = transpiled_destination(&roots, &root, emitted.by_path);
            entries.push(stage_module(&mut staging, &relative, emitted)?);
            Ok(())
        },
    );

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

    // the tree is finished whatever happens in here, so a run that gives up
    // half way still leaves a manifest describing what is actually on disk. a
    // manifest left describing the run before it would have the *next* run prune
    // against a tree that no longer exists
    let compiling: anyhow::Result<()> = 'compiling: {
        // `by_build` writes each artifact before it is recorded, so the tree is marked first
        if let Err(error) = staging.mark_as_output() {
            break 'compiling Err(error);
        }
        for ((path, file), name) in planned {
            let source = match fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))
            {
                Ok(source) => source,
                Err(error) => break 'compiling Err(error),
            };

            let program_file = ty_python_semantic::Db::program_file(&*db, *file);
            let parsed =
                ruff_db::parsed::parsed_module(&*db, program_file.python_file(&*db)).load(&*db);
            let model = ty_python_semantic::LoweringModel::new(&*db, program_file);
            // a `.py` source needs no transpiling to be its own interpreted fallback
            let mut options = options.clone();
            if path.extension().is_some_and(|x| x == "py") {
                options.language = by_irbuild::Language::Python;
            }
            let mut lowered = by_irbuild::build_module(
                &*db,
                &model.program_environment(),
                &model,
                parsed.suite(),
                name,
                options.lowering(),
            );
            // the real path, so a `#line` in the generated C resolves for a debugger
            let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            lowered.lines = Some(by_ir::function::LineTable::new(
                absolute.display().to_string(),
                &source,
            ));

            let built = match if emit_c_only {
                by_build::emit_lowered(lowered, &source, Some(&toolchain), &out_dir, &options)
            } else {
                by_build::build_lowered(lowered, &source, &toolchain, &out_dir, &options).inspect(
                    |built| {
                        if let Some(extension) = &built.artifact.extension {
                            eprintln!("{} -> {}", path.display(), extension.display());
                        }
                    },
                )
            }
            .with_context(|| format!("could not compile {}", path.display()))
            {
                Ok(built) => built,
                Err(error) => break 'compiling Err(error),
            };

            // exactly what `by_build` says it wrote — no guessing at names, and no
            // asking the file system, which cannot tell an artefact this run
            // produced from one a previous run left
            for produced in [&built.artifact.source, &built.artifact.header]
                .into_iter()
                .chain(built.artifact.extension.as_ref())
                .chain(built.artifact.annotation.as_ref())
            {
                if let Ok(relative) = produced.strip_prefix(&out_dir)
                    && let Err(error) = staging.record(relative)
                {
                    break 'compiling Err(error);
                }
            }
            if let Some(annotation) = &built.artifact.annotation {
                eprintln!("  annotated {}", annotation.display());
            }
            declined_total += built.declined.len();
            if verbose {
                // a decline is the compiler's report on the code it did *not* take,
                // so it points at that code the way every other diagnostic does
                let diagnostics: Vec<Diagnostic> = built
                    .declined
                    .iter()
                    .map(|declined| declined_diagnostic(*file, declined))
                    .collect();
                if let Err(error) = render_diagnostics(&db, &diagnostics) {
                    break 'compiling Err(error);
                }
            }
            compiled += 1;
        }

        // the rest of the project: the data a module reads relative to itself,
        // the hand-written `.py` beside it, the markers and the record that let a
        // tool read the tree back. the same calls `build` makes, in the same
        // order, because this is the same tree.
        //
        // inside the block with the compile, because each of these can fail and
        // the tree has to be finished either way — `stage_verbatim` in particular
        // is where a file the project keeps on top of an artefact is reported,
        // which is a failure this change deliberately introduced
        if let Err(error) = stage_verbatim(&db, &root, &roots, &mut staging) {
            break 'compiling Err(error);
        }
        if let Err(error) = stage_runtime(&mut staging, &layout) {
            break 'compiling Err(error);
        }
        if let Err(error) = stage_by_typed_markers(&db, &mut staging, &roots, &root) {
            break 'compiling Err(error);
        }
        if let Err(error) = write_sourcemap_module(&mut staging, &entries) {
            break 'compiling Err(error);
        }
        // whether the tree actually *holds* extensions, which is the question a
        // reader of the record is asking — `--emit-c-only` writes the generated C
        // and stops, so its tree is as replaceable one file at a time as a
        // `by build`'s. the generated `.c` beside it is then a description of the
        // source at the time it was written, and a later re-stage does not
        // regenerate it; nothing loads it, so the two are allowed to drift
        let holds_extensions = !emit_c_only && compiled > 0;
        if let Err(error) = stage_build_record(
            &mut staging,
            &BuildRecord::new(&root, &roots, None, holds_extensions, &tree_config),
        ) {
            break 'compiling Err(error);
        }
        Ok(())
    };

    // whatever happened above, the manifest is brought into line with what is
    // actually on disk. a manifest left describing the run before would have the
    // *next* run prune against a tree that no longer exists, leaving behind files
    // no source produces — an importable module nobody wrote
    staging.finish()?;

    compiling?;
    // the transpile's verdict is answered after the tree is finished, the way
    // `build` answers it: a diagnostic must not cost the tree, because a `build/`
    // a debugger cannot read is worse than one built from a partial check
    let transpiled = transpiled?;
    if !matches!(transpiled, ExitStatus::Success) {
        return Ok(transpiled);
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
                diagnostics.push(transpile_failure_diagnostic(file, &e));
                render_diagnostics(&db, &diagnostics)?;
                return Ok(ExitStatus::Failure);
            }
        }
    } else {
        by_transforms::transpile_analysed_at(&source, &config, config.min_version)
            .map_err(|e| anyhow::anyhow!("{e}"))?
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
/// build`, but written in place rather than to `build/`).
#[allow(clippy::print_stderr)]
fn forward_dir(dir: &Path, config: &Config) -> anyhow::Result<ExitStatus> {
    let (db, handles, rebuilder, root) = build_project_db(dir, BY_SOURCES, None)?;
    if handles.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }

    let roots = module_roots(&db, &root);
    render_check_and_transpile(
        &db,
        &handles,
        OutputFormat::Full,
        &mut Emit {
            config,
            gate: CheckGate::ParseErrorsOnly,
            rebuilder: &rebuilder,
            requirements: &mut by_transforms::RuntimeRequirements::default(),
            // the output lands in the source tree itself, where a runtime file
            // with no `.by` beside it would read as a module the author wrote —
            // to `by check`, to the linter, to `by transpile --reverse`
            runtime: None,
            roots: &roots,
            root: &root,
        },
        |emitted| {
            let py = emitted.by_path.with_extension("py");
            fs::write(&py, emitted.python).with_context(|| format!("{}", py.display()))?;
            Ok(())
        },
    )
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
/// target module compiled under its `.by` path and lines, and, on an uncaught
/// exception, rewrites any frame still in a generated file back to its `.by`
/// source location
fn write_traceback_runtime(
    staging: &mut Staging,
    entries: &[TracebackEntry],
) -> anyhow::Result<()> {
    write_sourcemap_module(staging, entries)?;
    // through the staging, so that a project of its own with this name is
    // reported as the collision it is rather than silently overwritten by a shim
    // it knows nothing about
    staging.write(Path::new(BY_RUNNER_FILENAME), None, &runner_source()?)?;
    staging.write(
        Path::new(BY_COVERAGE_PLUGIN_FILENAME),
        None,
        BY_COVERAGE_PLUGIN_SRC,
    )?;
    staging.write(
        Path::new(BY_PYTEST_PLUGIN_FILENAME),
        None,
        BY_PYTEST_PLUGIN_SRC,
    )?;
    staging.write(Path::new(BY_REPL_FILENAME), None, BY_REPL_SRC)?;
    for (name, text) in BY_PYTEST_DISTRIBUTION {
        staging.write(
            &Path::new(BY_PYTEST_DISTRIBUTION_DIR).join(name),
            None,
            text,
        )?;
    }
    Ok(())
}

/// the python half of a `by repl` session, which `by repl` runs in the tree it staged
pub(crate) const BY_REPL_FILENAME: &str = "_by_repl.py";

const BY_REPL_SRC: &str = r#"# generated by `by repl` — the python half of a session, see `by_repl.rs`
import ast
import contextlib
import io
import json
import linecache
import os
import signal
import socket
import sys
import threading
import types

import _by_runner

# what a terminal sends around a paste, and an editor around an input it sends whole
_PASTE_START = "\x1b[200~"
_PASTE_END = "\x1b[201~"

_INTERACTIVE = sys.stdin.isatty()

# standard input as this file reads it, for the inputs: the program's `sys.stdin` can be
# another object, which says when the program reads
_STDIN = sys.stdin


@contextlib.contextmanager
def _holding_ctrl_c():
    """A ctrl-c that arrives inside the block is raised once it has finished, so a message is
    never cut short. Only the main thread receives signals, so only it has any to hold."""
    if threading.current_thread() is not threading.main_thread():
        yield
        return
    interrupted = []
    previous = signal.signal(signal.SIGINT, lambda *_: interrupted.append(True))
    try:
        yield
    finally:
        signal.signal(signal.SIGINT, previous)
    if interrupted:
        raise KeyboardInterrupt


class _Interrupted(KeyboardInterrupt):
    """A ctrl-c that arrived while `by repl` answered, with the answer it gave."""

    def __init__(self, reply):
        super().__init__()
        self.reply = reply


class _Session:
    """The connection to `by repl`, which says whether an input is whole and what it runs as."""

    def __init__(self):
        port = int(os.environ.pop("BY_REPL_PORT"))
        token = os.environ.pop("BY_REPL_TOKEN")
        # whether an editor listens for events: what standard input is read for, and when an
        # input has finished
        self.events = os.environ.pop("BY_REPL_EVENTS", None) == "1"
        self._socket = socket.create_connection(("127.0.0.1", port))
        self._reader = self._socket.makefile("r", encoding="utf-8", newline="\n")
        # a thread of the program's can read standard input while the main thread talks to
        # `by repl`, and each message goes whole
        self._lock = threading.Lock()
        # alone on its line, which is all `by repl` reads of a connection before it is the token
        with _holding_ctrl_c():
            self._socket.sendall((token + "\n").encode("ascii"))

    def _send(self, message):
        data = (json.dumps(message) + "\n").encode("utf-8")
        with self._lock:
            self._socket.sendall(data)

    def tell(self, message):
        with _holding_ctrl_c():
            self._send(message)

    def ask(self, message):
        # a ctrl-c that arrives while the answer is on its way is held until it has arrived,
        # so the next question is not answered with this one's answer
        line = ""
        try:
            with _holding_ctrl_c():
                self._send(message)
                line = self._reader.readline()
        except KeyboardInterrupt:
            if not line:
                raise EOFError from None
            raise _Interrupted(json.loads(line)) from None
        if not line:
            raise EOFError
        return json.loads(line)

    def event(self, event):
        """Tell the editor `event`, after what has been printed so far: it is in the pipes
        before the event is sent."""
        if not self.events:
            return
        sys.stdout.flush()
        sys.stderr.flush()
        self.tell({"event": event})


class _ProgramStdin(io.TextIOBase):
    """`sys.stdin` for the program while an editor listens for events: each read says first
    that the program is reading standard input, so the editor sends the next line as the
    program's rather than as an input."""

    def __init__(self, stdin, session):
        self._stdin = stdin
        self._session = session
        self._buffer = _ProgramStdinBuffer(stdin.buffer, session)

    def _reading(self):
        self._session.event({"reading": "program"})

    def read(self, size=-1):
        self._reading()
        return self._stdin.read(size)

    def readline(self, size=-1):
        self._reading()
        return self._stdin.readline(size)

    def readlines(self, hint=-1):
        self._reading()
        return self._stdin.readlines(hint)

    def readable(self):
        return True

    def fileno(self):
        return self._stdin.fileno()

    def isatty(self):
        return self._stdin.isatty()

    @property
    def buffer(self):
        return self._buffer

    @property
    def encoding(self):
        return self._stdin.encoding

    @property
    def errors(self):
        return self._stdin.errors

    @property
    def newlines(self):
        return self._stdin.newlines

    def __getattr__(self, name):
        return getattr(self._stdin, name)


class _ProgramStdinBuffer(io.BufferedIOBase):
    """`sys.stdin.buffer` for the program, saying it reads as `_ProgramStdin` does."""

    def __init__(self, buffer, session):
        self._inner = buffer
        self._session = session

    def _reading(self):
        self._session.event({"reading": "program"})

    def read(self, size=-1):
        self._reading()
        return self._inner.read(size)

    def read1(self, size=-1):
        self._reading()
        return self._inner.read1(size)

    def readinto(self, buffer):
        self._reading()
        return self._inner.readinto(buffer)

    def readinto1(self, buffer):
        self._reading()
        return self._inner.readinto1(buffer)

    def readline(self, size=-1):
        self._reading()
        return self._inner.readline(size)

    def readable(self):
        return True

    def fileno(self):
        return self._inner.fileno()

    def isatty(self):
        return self._inner.isatty()

    def __getattr__(self, name):
        return getattr(self._inner, name)


def _read(prompt):
    # what the last input printed is out before anything else is read, as it is on a terminal,
    # wherever the streams go
    sys.stdout.flush()
    sys.stderr.flush()
    if _INTERACTIVE:
        return input(prompt) + "\n"
    line = _STDIN.readline()
    if not line:
        raise EOFError
    return line


def _read_input(session):
    """The next input: lines until `by repl` says they are whole, or a framed block."""
    session.event({"reading": "input"})
    line = _read(">>> ")
    if line.startswith(_PASTE_START):
        text = line[len(_PASTE_START):]
        while _PASTE_END not in text:
            text += _read("")
        return text[: text.index(_PASTE_END)]
    text = line
    while session.ask({"status": text})["status"] == "incomplete":
        session.event({"reading": "continuation"})
        text += _read("... ")
    return text


def _codes(reply, text, filename):
    """The code objects of the python `by repl` wrote for `text`, in `reply`, which run for
    it, named `filename` and at the lines of `text`."""
    generated = reply["lines"]
    widths = [len(line.rstrip("\r\n")) for line in text.splitlines()]

    def by_line(line):
        mapped = generated[line - 1] if line is not None and 0 < line <= len(generated) else None
        return 0 if mapped is None else mapped + 1

    def width(line):
        return widths[line - 1] if 0 < line <= len(widths) else 0

    module = ast.parse(reply["python"])
    body = [module.body[index] for index in reply["statements"]]
    if not body:
        return []
    if reply["display"]:
        # compiled as python's repl compiles an input, each expression statement prints its value
        code = compile(ast.Interactive(body=body), filename, "single", dont_inherit=True)
    else:
        code = compile(ast.Module(body=body, type_ignores=[]), filename, "exec", dont_inherit=True)
    return [_by_runner._relined(code, filename, by_line, width)]


def _execute(session, namespace, number, codes):
    """Run `codes`, input `number`'s, which is part of the session from the moment it starts,
    whatever it raises."""
    try:
        session.tell({"ran": number})
        for code in codes:
            exec(code, namespace)
    except SystemExit:
        raise
    except BaseException:
        sys.stderr.write(_by_runner.format_exception(*sys.exc_info()))


def _run(session, namespace, text):
    """Run `text`, an input: the number it was given, and whether it ran, which makes it part
    of the session whatever it raised."""
    try:
        reply = session.ask({"run": text})
    except _Interrupted as interrupted:
        # as python's repl drops an input interrupted while it compiles
        sys.stderr.write("\nKeyboardInterrupt\n")
        return interrupted.reply["input"], False
    number = reply["input"]
    if "python" not in reply:
        # refused: `by repl` has said why
        return number, False
    filename = f"<by-input-{number}>"
    # what a traceback quotes for a line of the input, for as long as the session lasts
    linecache.cache[filename] = (len(text), None, text.splitlines(True), filename)
    try:
        codes = _codes(reply, text, filename)
    except Exception:
        sys.stderr.write("error: the python this input was transpiled to does not compile:\n")
        sys.stderr.write(_by_runner.format_exception(*sys.exc_info()))
        return number, False
    except KeyboardInterrupt:
        sys.stderr.write("\nKeyboardInterrupt\n")
        return number, False
    _execute(session, namespace, number, codes)
    return number, True


def _start_in(session, main_module, start):
    """Run the module the session starts in, which `start` is what `by repl` wrote for, as
    `__main__`: in the session's namespace, with the module's file, package and spec, as `by
    run` runs it, so its relative imports resolve and a spawned process finds it."""
    by_path = start["file"]
    try:
        _, spec = _by_runner._entry_spec(start["module"])
        codes = _codes(start, start["text"], by_path)
    except Exception:
        sys.stderr.write(_by_runner.format_exception(*sys.exc_info()))
        return
    sys.argv = [by_path]
    main_module.__file__ = by_path
    main_module.__loader__ = spec.loader
    main_module.__package__ = spec.parent
    main_module.__spec__ = spec
    _execute(session, main_module.__dict__, start["input"], codes)


def main():
    session = _Session()
    sys.meta_path.insert(0, _by_runner._ByFinder())
    sys.excepthook = _by_runner._excepthook
    sys.argv = [""]
    main_module = types.ModuleType("__main__")
    sys.modules["__main__"] = main_module
    namespace = main_module.__dict__
    if session.events:
        sys.stdin = _ProgramStdin(_STDIN, session)
    try:
        start = session.ask({"start": True})
        if "module" in start:
            _start_in(session, main_module, start)
    except KeyboardInterrupt:
        sys.stderr.write("\nKeyboardInterrupt\n")
    except EOFError:
        return
    if _INTERACTIVE:
        try:
            import readline  # noqa: F401 — line editing for `input`
        except ImportError:
            pass
    while True:
        try:
            text = _read_input(session)
            if not text.strip():
                continue
            number, ran = _run(session, namespace, text)
            session.event({"finished": number, "ran": ran})
        except EOFError:
            # the end of the session's input, or `by repl` has gone, and with it the session
            if _INTERACTIVE:
                sys.stdout.write("\n")
            return
        except KeyboardInterrupt:
            sys.stderr.write("\nKeyboardInterrupt\n")


if __name__ == "__main__":
    main()
"#;

/// the pytest plugin that moves a test module's functions to the `.by` it was
/// transpiled from, so that a failure is reported at the `.by` line that failed
const BY_PYTEST_PLUGIN_FILENAME: &str = "_by_pytest.py";

const BY_PYTEST_PLUGIN_SRC: &str = r###"# generated by `by run` — reports a test at the `.by` line it failed at, for pytest
import ast
import os
import pprint
import sys
import traceback
import types

import pytest
from _pytest.assertion.rewrite import AssertionRewritingHook, rewrite_asserts

import _by_runner

# set to `teamcity` by an IDE that reads the run as TeamCity service messages
_REPORTER = "BY_TEST_REPORTER"


def _rewritten(hook, source, filename):
    # a test module, or a `conftest`, as pytest's assertion rewriter compiles it, each
    # `assert` rewritten to explain how it failed
    tree = ast.parse(source, filename=filename)
    rewrite_asserts(tree, source, filename, hook.config)
    return compile(tree, filename, "exec", dont_inherit=True)


# pytest compiles a test module itself to rewrite its assertions, and installs the
# rewriter before it imports this plugin, so before any test module or `conftest`. the
# runner compiles each such module as the rewriter does and moves all of it to its
# `.by`, its top level included, as it does every other module of the project
_by_runner.compile_like(AssertionRewritingHook, _rewritten)


def pytest_pycollect_makeitem(collector):
    # called for each name of a test module or class pytest collects, once the module is
    # imported and before any of its tests runs
    _by_runner.reline_module(collector.module)


def pytest_plugin_registered(plugin):
    # a `conftest.py` is a plugin, and its fixtures run in the tests
    if isinstance(plugin, types.ModuleType):
        _by_runner.reline_module(plugin)


def pytest_exception_interact(node, call, report):
    # an error raised while a test module is imported comes from its top level. it is
    # reported as `by run` reports an error in a program, as python prints it with
    # each frame at its `.by` line
    if not isinstance(report, pytest.CollectReport) or call.excinfo is None:
        return
    error = call.excinfo.value
    if isinstance(error, pytest.Collector.CollectError):
        # pytest words the other errors it wraps, which are kept: a module that skips
        # without `allow_module_level`, or a file imported under another's name
        error = error.__cause__
        if not isinstance(error, ImportError):
            return
    if _by_runner.runs_transpiled_code(error.__traceback__):
        report.longrepr = _Traceback(error)


class _Traceback:
    """An error as `by run` prints it, standing as a report's `longrepr`, which pytest
    writes out with `toterminal` and summarises in a line by `reprcrash.message`."""

    def __init__(self, error):
        self._text = _by_runner.format_exception(type(error), error, error.__traceback__)
        self.reprcrash = types.SimpleNamespace(
            message="".join(traceback.format_exception_only(type(error), error)).strip()
        )

    def toterminal(self, out):
        out.line(self._text.rstrip("\n"))

    def __str__(self):
        return self._text


def pytest_configure(config):
    reporter = os.environ.get(_REPORTER)
    if not reporter:
        return
    if reporter != "teamcity":
        raise pytest.UsageError(f"{_REPORTER} is {reporter!r}, and the one reporter is teamcity")
    config.pluginmanager.register(_TeamCity(config), "_by_teamcity")


def _escape(value):
    # https://www.jetbrains.com/help/teamcity/service-messages.html#Escaped+Values
    return (
        value.replace("|", "||")
        .replace("'", "|'")
        .replace("\n", "|n")
        .replace("\r", "|r")
        .replace("[", "|[")
        .replace("]", "|]")
        .replace("\u0085", "|x")
        .replace(" ", "|l")
        .replace(" ", "|p")
    )


def _shown(left, right):
    # two strings are compared as their text, anything else as python writes it
    if isinstance(left, str) and isinstance(right, str):
        return left, right
    return pprint.pformat(left), pprint.pformat(right)


class _TeamCity:
    """Reports the run as the TeamCity service messages an IDE's test runner reads.

    Each test file is a suite named by the project file it was transpiled from or
    copied out of, relative to the project, and each class collected in it a suite
    inside it, so the tree is the project's rather than the staged tree's. A test
    carries its pytest node id as `metainfo`, which is how it is run again. Every
    `locationHint` is `file://` and an absolute path, with `:` and a 1-based line
    for a class or a test, of the project file.

    A failing `==` is reported with what each side was, `actual` the left and
    `expected` the right, as PyCharm reports python's.
    """

    def __init__(self, config):
        self._config = config
        self._capture = config.pluginmanager.getplugin("capturemanager")
        # the suites open, outermost first, each its collector's node id and name
        self._open = []
        # the name of the test running, and its phases, each pytest's report of it
        self._name = None
        self._phases = []
        # each `==` an assertion failing in the phase running compared, which
        # `_comparison` matches to the assertion it failed in
        self._compared = []
        # the comparison each phase failed in, by node id and phase
        self._comparisons = {}

    def _message(self, kind, **attributes):
        text = "".join(
            f" {key}='{_escape(value)}'" for key, value in attributes.items() if value is not None
        )
        # pytest captures the program's output while a test runs, and a message is
        # never written while it does: it would land in the test's output
        if self._capture is None:
            self._write(f"##teamcity[{kind}{text}]\n")
        else:
            with self._capture.global_and_fixture_disabled():
                self._write(f"##teamcity[{kind}{text}]\n")

    @staticmethod
    def _write(text):
        sys.stdout.write(text)
        sys.stdout.flush()

    def _location(self, path, line=None):
        path, line = _by_runner.project_location(os.fspath(path), line)
        url = "file://" + os.path.abspath(path)
        return url if line is None else f"{url}:{line}"

    def _file_name(self, path):
        # the project file, relative to the project when it is in it
        project_file, _ = _by_runner.project_location(os.fspath(path), None)
        relative = os.path.relpath(os.path.abspath(project_file), _by_runner.ROOT)
        if relative.split(os.sep)[0] == os.pardir:
            return project_file
        return relative.replace(os.sep, "/")

    def pytest_sessionstart(self, session):
        self._message("enteredTheMatrix")

    def pytest_collectreport(self, report):
        # a file or directory that failed to collect, or skipped as a whole, is a test
        # of its own: there is no other node to put it on
        if not (report.failed or report.skipped):
            return
        path = os.path.join(self._config.rootpath, report.fspath)
        name = self._file_name(path)
        self._message("testStarted", name=name, locationHint=self._location(path))
        if report.failed:
            self._message(
                "testFailed",
                name=name,
                message=_crash(report),
                details=str(report.longrepr),
            )
        else:
            self._message("testIgnored", name=name, message=_skip_reason(report))
        self._message("testFinished", name=name)

    def pytest_collection_finish(self, session):
        self._message("testCount", count=str(len(session.items)))

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtest_protocol(self, item, nextitem):
        # the suites are the collectors from the file down: no directory or package
        chain = item.listchain()[:-1]
        files = [index for index, node in enumerate(chain) if isinstance(node, pytest.File)]
        suites = chain[files[-1] :] if files else []
        shared = 0
        while (
            shared < len(suites)
            and shared < len(self._open)
            and suites[shared].nodeid == self._open[shared][0]
        ):
            shared += 1
        self._close(shared)
        for suite in suites[shared:]:
            if isinstance(suite, pytest.File):
                name = self._file_name(suite.path)
                location = self._location(suite.path)
            else:
                name = suite.name
                path, line, _ = suite.reportinfo()
                location = self._location(path, None if line is None else line + 1)
            self._message("testSuiteStarted", name=name, locationHint=location)
            self._open.append((suite.nodeid, name))
        path, line, _ = item.location
        self._message(
            "testStarted",
            name=item.name,
            locationHint=self._location(
                os.path.join(self._config.rootpath, path), None if line is None else line + 1
            ),
            metainfo=item.nodeid,
            # with pytest capturing it, a test's output is sent with its result
            captureStandardOutput="false" if self._captures() else "true",
        )
        self._name = item.name
        self._phases = []
        self._comparisons = {}

    def _captures(self):
        return self._capture is not None and self._config.getoption("capture") != "no"

    def _close(self, depth):
        while len(self._open) > depth:
            _, name = self._open.pop()
            self._message("testSuiteFinished", name=name)

    def pytest_assertrepr_compare(self, config, op, left, right):
        if op != "==":
            return None
        # the frame of the assertion: the first that is neither pytest's nor pluggy's
        frame = sys._getframe(1)
        while frame is not None and frame.f_globals.get("__name__", "").startswith(
            ("_pytest.", "pluggy.")
        ):
            frame = frame.f_back
        if frame is None:
            return None
        record = (frame, frame.f_lineno, frame.f_lasti, left, right)
        # an assertion explains each comparison it failed in, one call each; a
        # comparison it has explained already is the assertion failing again
        if (
            self._compared
            and self._compared[-1][0] is frame
            and self._compared[-1][1] == frame.f_lineno
            and all(previous[2] != frame.f_lasti for previous in self._compared)
        ):
            self._compared.append(record)
        else:
            self._compared = [record]
        return None

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtest_makereport(self, item, call):
        compared, self._compared = self._compared, []
        if call.excinfo is None or not isinstance(call.excinfo.value, AssertionError):
            return None
        tb = call.excinfo.value.__traceback__
        while tb is not None and tb.tb_next is not None:
            tb = tb.tb_next
        # the comparison is the assertion's that failed only when it was the one
        # comparison that assertion explained: of `a == b or c == d`, neither is
        if (
            tb is not None
            and len(compared) == 1
            and compared[0][0] is tb.tb_frame
            and compared[0][1] == tb.tb_lineno
        ):
            _, _, _, left, right = compared[0]
            self._comparisons[call.when] = _shown(left, right)
        return None

    def pytest_runtest_logreport(self, report):
        if report.when in ("setup", "call", "teardown"):
            self._phases.append(report)

    @pytest.hookimpl(trylast=True)
    def pytest_runtest_logfinish(self, nodeid, location):
        name = self._name
        for report in self._phases:
            for title, content in report.sections:
                if not content or not title.endswith(" " + report.when):
                    continue
                if "stdout" in title:
                    self._message("testStdOut", name=name, out=content)
                elif "stderr" in title:
                    self._message("testStdErr", name=name, out=content)
        failed = [report for report in self._phases if report.failed]
        skipped = [report for report in self._phases if report.skipped]
        if failed:
            first = failed[0]
            comparison = self._comparisons.get(first.when)
            message = _crash(first)
            if first.when != "call":
                message = f"error at {first.when}: {message}"
            attributes = {}
            if comparison is not None:
                actual, expected = comparison
                attributes = {"type": "comparisonFailure", "expected": expected, "actual": actual}
            self._message(
                "testFailed",
                name=name,
                message=message,
                details="\n\n".join(str(report.longrepr) for report in failed),
                **attributes,
            )
        elif skipped:
            self._message("testIgnored", name=name, message=_skip_reason(skipped[0]))
        duration = sum(report.duration for report in self._phases)
        self._message("testFinished", name=name, duration=str(int(duration * 1000)))
        self._phases = []
        self._comparisons = {}

    @pytest.hookimpl(trylast=True)
    def pytest_sessionfinish(self, session):
        self._close(0)


def _crash(report):
    # the error's own line, as pytest's summary shows it, or the last of what it wrote
    crash = getattr(report.longrepr, "reprcrash", None)
    if crash is not None:
        return crash.message
    lines = str(report.longrepr).strip().splitlines()
    return lines[-1] if lines else ""


def _skip_reason(report):
    if hasattr(report, "wasxfail"):
        return f"expected failure: {report.wasxfail}" if report.wasxfail else "expected failure"
    # pytest's skip is a (path, line, reason) triple
    if isinstance(report.longrepr, tuple) and len(report.longrepr) == 3:
        return report.longrepr[2]
    return str(report.longrepr)
"###;

/// the distribution that names `_by_pytest` as a pytest plugin. pytest loads the
/// plugins every distribution on `sys.path` declares, and the tree is at the head
/// of `sys.path` in the process `by run` starts, and in any python started in it:
/// the plugin is loaded wherever pytest can import it, and nowhere it cannot.
/// `PYTEST_DISABLE_PLUGIN_AUTOLOAD` turns it off with every other plugin, and
/// `-p _by_pytest` turns it back on
const BY_PYTEST_DISTRIBUTION_DIR: &str = "_by_pytest-0.dist-info";

const BY_PYTEST_DISTRIBUTION: [(&str, &str); 2] = [
    (
        "METADATA",
        "Metadata-Version: 2.1\nName: by-pytest\nVersion: 0\n",
    ),
    ("entry_points.txt", "[pytest11]\nby = _by_pytest\n"),
];

/// the coverage.py plugin `by run --coverage` measures with, which the runner
/// names by its module name
const BY_COVERAGE_PLUGIN_FILENAME: &str = "_by_coverage_plugin.py";

const BY_COVERAGE_PLUGIN_SRC: &str = r#"# generated by `by run` — the `.by` modules a run can leave out, for coverage.py
import os

import coverage

from _by_sourcemap import SOURCEMAP


class _ByModules(coverage.CoveragePlugin):
    """The `.by` files transpiled into the tree, as files coverage.py looks for
    under its `source` to report the ones that never ran, as it does a `.py`.

    It traces nothing: a `.by` module that runs is compiled under its `.by` path
    and lines, and measured like any python file."""

    def file_tracer(self, filename):
        return None

    def find_executable_files(self, src_dir):
        within = os.path.join(os.path.realpath(src_dir), "")
        for by_path, _ in SOURCEMAP.values():
            if os.path.realpath(by_path).startswith(within):
                yield by_path


def coverage_init(reg, options):
    reg.add_file_tracer(_ByModules())
"#;

/// the runner's record of the project the tree was staged from
const BY_PROJECT_FILENAME: &str = "_by_project.py";

/// writes `_by_project.py`: the project's root, and the project file each
/// hand-written module in the tree is a copy of, which the runner runs it as
fn write_project_module(
    staging: &mut Staging,
    root: &Path,
    carried: &BTreeMap<PathBuf, PathBuf>,
) -> anyhow::Result<()> {
    // a json string is a python string literal, escapes included
    let modules: BTreeMap<String, String> = carried
        .iter()
        .filter(|(_, source)| source.extension() == Some(OsStr::new("py")))
        .map(|(staged, source)| (staged.display().to_string(), source.display().to_string()))
        .collect();
    let text = format!(
        "# generated by `by run` — the project this tree was staged from\n\
         ROOT = {}\n\
         CARRIED = {}\n",
        serde_json::to_string(&root.display().to_string())?,
        serde_json::to_string(&modules)?,
    );
    staging.write(Path::new(BY_PROJECT_FILENAME), None, &text)
}

/// the line [`BY_RUNNER_SRC`] holds for the runtime's definitions, which [`runner_source`] fills in
const RUNTIME_LINES_PLACEHOLDER: &str = "_RUNTIME_LINES = {}\n";

/// the line [`BY_RUNNER_SRC`] holds for the files of by's own python in the tree, which
/// [`runner_source`] fills in
const MACHINERY_FILES_PLACEHOLDER: &str = "_MACHINERY_FILES = []\n";

/// the python `by` writes into the tree to run the program, none of which is the program's:
/// what a traceback and a profile leave out
const MACHINERY_FILES: [&str; 4] = [
    BY_RUNNER_FILENAME,
    BY_REPL_FILENAME,
    BY_PYTEST_PLUGIN_FILENAME,
    BY_COVERAGE_PLUGIN_FILENAME,
];

/// the runner shim, with the line of `_by_runtime.py` each of the runtime's definitions starts
/// on, by the names it binds — what a profile names the runtime's code by — and the files of
/// by's own python beside it
fn runner_source() -> anyhow::Result<String> {
    anyhow::ensure!(
        BY_RUNNER_SRC.contains(RUNTIME_LINES_PLACEHOLDER),
        "the runner has no place for the runtime's lines"
    );
    anyhow::ensure!(
        BY_RUNNER_SRC.contains(MACHINERY_FILES_PLACEHOLDER),
        "the runner has no place for the files of by's own python"
    );
    // a json object of strings and integers is a python dict literal, and a json array of
    // strings a python list literal
    let lines = serde_json::to_string(&by_transforms::runtime::definition_lines())?;
    let machinery = serde_json::to_string(&MACHINERY_FILES)?;
    Ok(BY_RUNNER_SRC
        .replacen(
            RUNTIME_LINES_PLACEHOLDER,
            &format!("_RUNTIME_LINES = {lines}\n"),
            1,
        )
        .replacen(
            MACHINERY_FILES_PLACEHOLDER,
            &format!("_MACHINERY_FILES = {machinery}\n"),
            1,
        ))
}

const BY_RUNNER_SRC: &str = r#"# generated by `by run` — runs the target module with .by-aware tracebacks
import atexit
import dis
import hashlib
import importlib.machinery
import importlib.util
import json
import linecache
import marshal
import os
import sys
import traceback
import types
import warnings

from _by_project import CARRIED, ROOT
from _by_sourcemap import DIGESTS, SOURCEMAP

# index the sourcemap by realpath so symlinked temp dirs (e.g. /tmp on macOS)
# still match the filenames python reports in frames. the key the entry was
# written under travels with it, because DIGESTS is keyed the unresolved way
_BY_MAP = {os.path.realpath(py): (py, info) for py, info in SOURCEMAP.items()}
# the .by files themselves, which a module compiled by `_by_code` names in its frames,
# with the key of the entry each was transpiled under
_BY_SOURCES = {os.path.realpath(info[0]): py for py, info in SOURCEMAP.items()}

# the line of `_by_runtime.py`, as `by` writes it, each of the runtime's definitions starts
# on, by the names it binds. `by run` fills it in
_RUNTIME_LINES = {}
# what a profile names the runtime's code by, which is no file: `<frozen runpy>` is python's
# own name for code it holds rather than reads
_RUNTIME_FILE = "<by runtime>"
# while the program is profiled, each code object `_relined` gave no `.by` line, by its id,
# with the staged module and the line of it it was compiled from
_TRANSPILER_WRITTEN = None

# this file, whose frames are never the program's
_RUNNER = os.path.realpath(__file__)
# the start of the name `by repl` compiles an input under, `<by-input-3>`
_SESSION_INPUT = "<by-input-"
# the tree the project was staged into, which this file is at the top of
_TREE = os.path.dirname(_RUNNER)
# the names of the files of by's own python at the top of the tree, this one among them.
# `by run` fills it in
_MACHINERY_FILES = []
# those files, whose frames run the program and are never its own: a traceback and a profile
# leave them out
_MACHINERY = {os.path.realpath(os.path.join(_TREE, name)) for name in _MACHINERY_FILES}
# the hand-written `.py` files carried over into the tree, each with the project
# file it is a copy of
_CARRIED = {
    os.path.realpath(os.path.join(_TREE, staged)): source for staged, source in CARRIED.items()
}

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


def _stale(key, by_path):
    _STALE.add(key)
    sys.stderr.write(
        f"note: {by_path} no longer matches what it was transpiled from, "
        "so no frame names a line of it\n"
    )


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
    _stale(key, by_path)
    return False


def _still_current(key, by_path):
    # a module `_by_code` compiled has its `.by` lines in its frames already, checked
    # when it was loaded. a `.by` saved over since then no longer has those lines, so
    # it is checked again before a frame names one
    if key in _STALE:
        return False
    digests = DIGESTS.get(key)
    if digests is not None and _digest_matches(by_path, digests["by"]):
        return True
    _stale(key, by_path)
    return False


def _rewrite(frames):
    # drop the runner/runpy bootstrap above the first user frame. an input to a `by repl`
    # session is the program's own as much as a module of the project is
    first = next(
        (
            i
            for i, f in enumerate(frames)
            if os.path.realpath(f.filename) in _BY_MAP
            or os.path.realpath(f.filename) in _BY_SOURCES
            or f.filename.startswith(_SESSION_INPUT)
        ),
        None,
    )
    if first is not None:
        # below the program's first frame, by's own are what the program called without
        # knowing it: the loader the runner puts on every staged module, and the standard
        # input `by repl` reads the program's lines through
        frames = [
            f for f in frames[first:] if os.path.realpath(f.filename) not in _MACHINERY
        ]
    out = []
    for f in frames:
        compiled_from = _BY_SOURCES.get(os.path.realpath(f.filename))
        if compiled_from is not None and not _still_current(compiled_from, f.filename):
            # its line is one of the file that was transpiled, which is gone. the
            # generated line it came from is named instead when it is the only one
            # that `.by` line was written as, and no line at all when it is not
            _, generated = SOURCEMAP[compiled_from]
            written_as = [
                index + 1
                for index, line in enumerate(generated)
                if f.lineno and line == f.lineno - 1
            ]
            if len(written_as) == 1:
                out.append(traceback.FrameSummary(compiled_from, written_as[0], f.name))
            else:
                out.append(f'  File "{f.filename}", in {f.name}\n')
            continue
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


def format_exception(etype, evalue, tb):
    """The traceback python prints for an exception, with every frame of a staged
    module at the line of the project file it stands for, and none of the frames
    that ran before the program's first."""
    frames = _rewrite(traceback.extract_tb(tb))
    if (
        isinstance(evalue, SyntaxError)
        and evalue.filename is not None
        and os.path.realpath(evalue.filename) in _BY_SOURCES
        and all(
            isinstance(frame, traceback.FrameSummary)
            and os.path.realpath(frame.filename) == _RUNNER
            for frame in frames
        )
    ):
        # the program's own module, which the runner could not compile: every frame
        # is the runner's, so it is reported as python reports a script it cannot
        # compile, by where the error is and nothing above it
        return "".join(traceback.format_exception_only(etype, evalue))
    out = ["Traceback (most recent call last):\n"]
    # a frame `_rewrite` could only name is already text. the others are formatted a
    # run at a time, so python still folds a repeated one
    run = []
    for frame in frames + [None]:
        if isinstance(frame, traceback.FrameSummary):
            run.append(frame)
            continue
        out.extend(traceback.StackSummary.from_list(run).format())
        run = []
        if frame is not None:
            out.append(frame)
    out.extend(traceback.format_exception_only(etype, evalue))
    return "".join(out)


def runs_transpiled_code(tb):
    """Whether a frame of `tb` runs a transpiled module's code: at the lines of its
    `.by`, as `_by_code` compiled it, or at those of the generated python, as a loader
    `reline_module` had yet to reach compiled it."""
    while tb is not None:
        filename = os.path.realpath(tb.tb_frame.f_code.co_filename)
        if filename in _BY_MAP or filename in _BY_SOURCES:
            return True
        tb = tb.tb_next
    return False


def _excepthook(etype, evalue, tb):
    sys.stderr.write(format_exception(etype, evalue, tb))


def _project_file_of(origin):
    # the project file a staged module stands for: the .by a staged .py was
    # transpiled from, or the hand-written .py it is a copy of. None for anything
    # else, such as a module from site-packages
    if not origin:
        return None
    real = os.path.realpath(origin)
    entry = _BY_MAP.get(real)
    return _CARRIED.get(real) if entry is None else entry[1][0]


def _project_code(origin, compile_source):
    """The staged module at `origin` compiled by `compile_source` as the project file
    it stands for, or `None` when it stands for none, which leaves it to be compiled
    where it is.

    A hand-written `.py` is a copy, so it is compiled from its text under the
    project file's path: what python derives from the code object — a traceback,
    `inspect`, what coverage.py measures — names the file the author wrote, as it
    does for a `.by`."""
    code = _by_code(origin, compile_source)
    if code is not None:
        return code
    project_file = _CARRIED.get(os.path.realpath(origin)) if origin else None
    if project_file is None:
        return None
    with open(origin, "rb") as handle:
        source = handle.read()
    return compile_source(source, project_file)


def _varint(out, value):
    while value >= 64:
        out.append(0x40 | (value & 63))
        value >>= 6
    out.append(value)


def _svarint(out, value):
    _varint(out, (-value << 1) | 1 if value < 0 else value << 1)


def _relined(code, filename, by_line, width):
    """`code` and every code object it holds, named `filename`, with each location
    moved to the line `by_line` gives, spanning the whole of that line."""
    relined = _relined_one(code, filename, by_line, width)
    if _TRANSPILER_WRITTEN is not None and relined.co_firstlineno == 0:
        # line 0 says only that the transpiler wrote it; a profile needs to know what
        _TRANSPILER_WRITTEN[id(relined)] = (relined, code.co_filename, code.co_firstlineno)
    return relined


def _relined_one(code, filename, by_line, width):
    """`_relined`, for `code` itself.

    The line table's format is private to each python version: see `Objects/locations.md`
    (3.11 on) and `Objects/lnotab_notes.txt` (3.10, and 3.9 before it) in cpython.
    """
    consts = tuple(
        _relined(const, filename, by_line, width) if isinstance(const, types.CodeType) else const
        for const in code.co_consts
    )
    # a module's code starts at its first line whatever is written there, as python's
    # own does, rather than at line 0 when the transpiler wrote that line — an import
    # it needed — on its own. a profile names the module by it, `main.by:1(<module>)`
    first = 1 if code.co_name == "<module>" else by_line(code.co_firstlineno)
    table = bytearray()
    if sys.version_info >= (3, 11):
        # an entry for each run of up to 8 code units at one line, in the long form,
        # which holds any line and column, or the form for no location
        lines = [None if line is None else by_line(line) for line, *_ in code.co_positions()]
        previous = first
        start = 0
        while start < len(lines):
            line = lines[start]
            end = start + 1
            while end < len(lines) and end - start < 8 and lines[end] == line:
                end += 1
            if line is None:
                table.append(0x80 | (15 << 3) | (end - start - 1))
            else:
                table.append(0x80 | (14 << 3) | (end - start - 1))
                _svarint(table, line - previous)
                _varint(table, 0)
                _varint(table, 1)
                _varint(table, width(line) + 1)
                previous = line
            start = end
        return code.replace(
            co_filename=filename, co_firstlineno=first, co_consts=consts, co_linetable=bytes(table)
        )
    if sys.version_info >= (3, 10):
        # a (bytecode length, line delta) pair for each range, -128 for no line
        previous = first
        for start, end, line in code.co_lines():
            size = end - start
            delta = -128
            if line is not None:
                line = by_line(line)
                delta = line - previous
                previous = line
                while delta > 127:
                    table += bytes((0, 127))
                    delta -= 127
                while delta < -127:
                    table += bytes((0, 256 - 127))
                    delta += 127
            while size > 254:
                table += bytes((254, delta & 255))
                delta = -128 if line is None else 0
                size -= 254
            table += bytes((size, delta & 255))
        return code.replace(
            co_filename=filename, co_firstlineno=first, co_consts=consts, co_linetable=bytes(table)
        )
    # a (bytecode offset delta, line delta) pair for each place the line changes
    offset, previous = 0, first
    for start, line in dis.findlinestarts(code):
        line = by_line(line)
        step, delta = start - offset, line - previous
        while step > 255:
            table += bytes((255, 0))
            step -= 255
        while delta > 127:
            table += bytes((step, 127))
            step, delta = 0, delta - 127
        while delta < -128:
            table += bytes((step, 128))
            step, delta = 0, delta + 128
        table += bytes((step, delta & 255))
        offset, previous = start, line
    return code.replace(
        co_filename=filename, co_firstlineno=first, co_consts=consts, co_lnotab=bytes(table)
    )


def _by_lines(origin):
    """The `.by` the staged `.py` at `origin` was transpiled from, as `_relined` takes
    it: its path, the `.by` line of each line of the `.py` (0 for one the transpiler
    wrote on its own), and the width of each `.by` line.

    `None` when there is no `.by` to answer with, or it has changed since it was
    transpiled."""
    entry = _BY_MAP.get(os.path.realpath(origin)) if origin else None
    if entry is None:
        return None
    key, (by_path, lines) = entry
    if not _is_current(key, by_path):
        return None
    with open(by_path, "rb") as handle:
        widths = [len(line.rstrip(b"\r\n")) for line in handle]

    def by_line(line):
        mapped = lines[line - 1] if line is not None and 0 < line <= len(lines) else None
        return 0 if mapped is None else mapped + 1

    def width(line):
        return widths[line - 1] if 0 < line <= len(widths) else 0

    return by_path, by_line, width


def project_location(path, line):
    """The project file a location in the tree stands for and the line of it, 1-based:
    a staged `.py` moves to the line of its `.by`, and a copy of a hand-written `.py`
    to the same line of the original. A location in no staged module is its own.

    `line` is `None`, and so is the answer's, for the file as a whole, and for a line
    the transpiler wrote on its own."""
    lines = _by_lines(path)
    if lines is not None:
        by_path, by_line, _ = lines
        return by_path, (by_line(line) or None) if line is not None else None
    carried = _CARRIED.get(os.path.realpath(path))
    return (path if carried is None else carried), line


def reline_module(module):
    """Every function a staged module compiled by another loader defines, moved to the
    `.by` the module was transpiled from, as `_by_code` compiles the modules no other
    loader stands behind. The other loader is an import hook that compiles a module itself
    and that `compile_like` was not told how to compile like.

    Only the code a test runs is reached: the module's functions, the methods of the
    classes it defines, and what a decorator wraps, each with the functions nested in it.
    The module's top level ran as it was imported, so an error raised while importing it
    still names the staged `.py`.
    """
    if id(module) in _RELINED_MODULES:
        return
    _RELINED_MODULES.add(id(module))
    mappings = {}
    seen = set()

    def mapping(origin):
        if origin not in mappings:
            mappings[origin] = _by_lines(origin)
        return mappings[origin]

    def visit(value):
        if id(value) in seen:
            return
        seen.add(id(value))
        if isinstance(value, types.FunctionType):
            lines = mapping(value.__code__.co_filename)
            if lines is not None:
                value.__code__ = _relined(value.__code__, *lines)
        elif isinstance(value, type):
            # a class another module defines is that module's to reline
            if value.__module__ == module.__name__:
                for member in vars(value).values():
                    visit(member)
            return
        elif isinstance(value, (staticmethod, classmethod)):
            visit(value.__func__)
        elif isinstance(value, property):
            for accessor in (value.fget, value.fset, value.fdel):
                if accessor is not None:
                    visit(accessor)
        # what `functools.wraps` wraps — a pytest fixture's function among them. read from
        # the object's own `__dict__` with `object.__getattribute__`, so no `__getattr__` or
        # `__getattribute__` of the program's runs
        try:
            namespace = object.__getattribute__(value, "__dict__")
        except AttributeError:
            return
        if type(namespace) is dict and namespace.get("__wrapped__") is not None:
            visit(namespace["__wrapped__"])

    for value in list(vars(module).values()):
        visit(value)


# the modules `reline_module` has seen, by `id`: a module stays in `sys.modules`, so
# its `id` names no other module while the run lasts
_RELINED_MODULES = set()


def _compiled(source, filename):
    # a module compiled as python's own loader compiles it
    return compile(source, filename, "exec", dont_inherit=True)


def _by_code(origin, compile_source):
    """The staged `.py` at `origin` compiled by `compile_source` — `_compiled`, or as
    the loader `compile_like` names compiles it — and moved to the `.by` it was
    transpiled from.

    A program reads its own locations back — `traceback.format_exc()`, the file
    and line `warnings` attributes a warning to, `co_filename`, `inspect` — and
    python answers each from the code object. So the code object is named after
    the `.by` path, with every line the `.by` line it came from: the answer is then
    the author's wherever it is read, and the source line python shows beside it is
    read from the `.by` too. A line the transpiler wrote on its own — a runtime
    helper — has no `.by` line, and says so as line 0. Columns are the generated
    file's, so every location spans the whole of its `.by` line instead: python
    marks no part of a line a location covers, and reads what it can from the `.by`
    text.

    The `.py` is compiled from its text, as the loader that found it would, and only
    then moved to the `.by`: python 3.9 to 3.11 refuse, at the interpreter's
    recursion limit, a syntax tree nested deeper than the source they accept. A
    warning the compiler raises is moved to its `.by` line the same way, and so is
    the `SyntaxError` it raises for one the warning filters make an error.

    `None` when there is no `.by` to answer with, or it has changed since it was
    transpiled, which leaves the module to be compiled where it is.
    """
    lines = _by_lines(origin)
    if lines is None:
        return None
    by_path, by_line, width = lines
    with open(origin, "rb") as handle:
        source = handle.read()

    with warnings.catch_warnings(record=True) as caught:
        try:
            code = compile_source(source, origin)
        except SyntaxError as error:
            # a warning the filters turn into an error (`-W error`) is raised by the
            # compiler as a `SyntaxError` at the line it would have warned about, so
            # it is moved to the `.by` like the warning. its columns are the
            # generated file's, which is why it points at no part of the line
            line = by_line(error.lineno) if error.filename == origin else 0
            if line == 0:
                raise
            text = linecache.getline(by_path, line) or None
            raise SyntaxError(error.msg, (by_path, line, None, text)) from None
    for warning in caught:
        if warning.filename == origin:
            warnings.warn_explicit(
                warning.message, warning.category, by_path, by_line(warning.lineno)
            )
        else:
            warnings.warn_explicit(
                warning.message, warning.category, warning.filename, warning.lineno
            )
    return _relined(code, by_path, by_line, width)


class _ByLoader:
    """Wraps a staged module's loader to name its project file as `__file__`.

    The program is run out of a directory of transpiled copies, so every path
    python derives from a module's origin — `__file__`, and anything a tool walks
    up from it to find — names a temporary file in a directory that is deleted
    when the run ends. The file the module was written as, a `.by` or a
    hand-written `.py`, is the answer to every question those paths are asked, so
    it is what the module reports.

    This is the loader of a module some other loader stands behind that
    `compile_like` was not told how to compile like: an import hook that compiles
    the module itself. It is left to that loader, and only its `__file__` moves; its
    frames are rewritten to `.by` lines when they reach the excepthook, and pytest's
    plugin, `_by_pytest`, moves its functions to the `.by` with `reline_module`
    before a test runs. A module python's own loader would compile gets
    `_BySourceLoader`, and one a loader `compile_like` names would, `_ByCompiledLike`.

    `__file__` is set as the module is created, and `exec_module` is not wrapped:
    the inner loader's own is the one python calls. A wrapper's frame between
    python's import machinery and the module's code is what kept python from
    removing that machinery's frames from a traceback raised while importing — a
    module that failed as it was imported showed `<frozen importlib._bootstrap>`
    three times and this file once, above the line that failed. The cost is a
    reload, which sets `__file__` back to the staged path and is not seen here.
    """

    def __init__(self, inner, by_path):
        self._by_inner = inner
        self._by_path = by_path

    def create_module(self, spec):
        # python sets `__file__` on the module only when it has none
        module = self._by_inner.create_module(spec)
        if module is None:
            module = types.ModuleType(spec.name)
        module.__file__ = self._by_path
        return module

    def __getattr__(self, name):
        return getattr(self._by_inner, name)


class _BySourceLoader(importlib.machinery.SourceFileLoader):
    """The loader of a staged module python's own loader would load.

    It compiles the module with `_project_code`, under its project file's path —
    and a `.by`'s lines — and names that file as `__file__`, like `_ByLoader`. It
    is python's own source
    loader rather than a wrapper around one, so that executing the module is
    python's own `exec_module`, whose frames python removes from a traceback raised
    while importing: what a failed import shows is the line that imported and the
    line that failed, as it is for a program python runs itself.
    """

    def __init__(self, fullname, path, by_path):
        super().__init__(fullname, path)
        self._by_path = by_path

    def get_code(self, fullname):
        # python's `exec_module` asks for the code of the module it is about to
        # run, which is in `sys.modules` by then — on an import and on a reload,
        # which sets `__file__` back to the staged path just before
        module = sys.modules.get(fullname)
        if module is not None and getattr(module, "__loader__", None) is self:
            module.__file__ = self._by_path
        code = _project_code(self.get_filename(fullname), _compiled)
        if code is not None:
            return code
        return super().get_code(fullname)


class _ByCompiledLike(importlib.machinery.SourceFileLoader):
    """The loader of a staged module a loader `compile_like` names would load, which
    compiles the module itself — pytest's assertion rewriter.

    It compiles the module with `_project_code` as that loader compiles it, so every
    code object of a `.by` — the module's top level, a class body, a comprehension, a
    lambda, as much as a function — is the `.by`'s, at its lines, from the moment it
    is compiled: a debugger stops at a breakpoint on any line of it, and python
    reports any frame of it at its `.by` line, as for a module `_BySourceLoader`
    loads. Its `__file__` is left the staged path the other loader would have given
    it: pytest checks that a test module it imported is the file it collected, and
    the staged `.py` is that file.
    """

    def __init__(self, fullname, path, compile_source):
        super().__init__(fullname, path)
        self._by_compile_source = compile_source

    def get_code(self, fullname):
        path = self.get_filename(fullname)
        code = _project_code(path, self._by_compile_source)
        if code is not None:
            return code
        # a `.by` saved over since it was transpiled: compiled where it is, still as
        # the other loader would compile it
        return self._by_compile_source(self.get_data(path), path)


class _ByFinder:
    """Puts a loader of its own on every staged module, whoever ends up finding it."""

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
            by_path = _project_file_of(getattr(spec, "origin", None))
            compile_source = _COMPILERS.get(type(spec.loader))
            if by_path is not None and type(spec.loader) is importlib.machinery.SourceFileLoader:
                spec.loader = _BySourceLoader(spec.loader.name, spec.loader.path, by_path)
            elif by_path is not None and compile_source is not None:
                loader = spec.loader
                spec.loader = _ByCompiledLike(
                    spec.name,
                    spec.origin,
                    lambda source, filename: compile_source(loader, source, filename),
                )
            elif by_path is not None and spec.loader is not None:
                spec.loader = _ByLoader(spec.loader, by_path)
            return spec
        return None


# the types of loader that compile a module themselves, each with how it compiles one,
# from `compile_like`
_COMPILERS = {}


def compile_like(loader_type, compile_source):
    """Load each staged module a loader of `loader_type` would load with
    `_ByCompiledLike`, compiled as `compile_source(loader, source, filename)` answers
    — the code object a loader of that type would execute — and then moved to the
    `.by`.

    Such a loader is an import hook that puts itself at the head of `sys.meta_path`,
    as pytest's assertion rewriter does before it loads the plugin that calls this,
    and a finder there answers ahead of `_ByFinder`. So `_ByFinder` is put back at
    the head, where it asks every other finder in turn and sees what they answer.
    """
    _COMPILERS[loader_type] = compile_source
    ours = [finder for finder in sys.meta_path if isinstance(finder, _ByFinder)]
    sys.meta_path[:] = ours + [
        finder for finder in sys.meta_path if not isinstance(finder, _ByFinder)
    ]


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


def _run_as_main(module, profiler=None):
    """Run `module` as `__main__`, with `.by` paths for `__file__` and `argv[0]`,
    measured by `profiler` from its first line when there is one.

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
    by_path = _project_file_of(spec.origin) or spec.origin
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
    if profiler is not None:
        # here rather than before the module was found, so the profile is the
        # program's and not the runner's search for it
        profiler.enable()
    exec(code, main_module.__dict__)


def _start_coverage(report):
    """Measure the program with coverage.py, and write what ran to `report` once the
    program has ended: after its other threads and its own `atexit` handlers, which
    python runs before the ones registered ahead of them."""
    try:
        import coverage
        from coverage.exceptions import CoverageWarning
    except ImportError:
        sys.stderr.write(
            "error: `by run --coverage` measures with coverage.py, which "
            f"{sys.executable} cannot import\n"
            "  install it into that environment: `uv add --dev coverage`, or "
            f"`{sys.executable} -m pip install coverage`\n"
        )
        sys.exit(2)
    # the project's coverage configuration names the project's files, which is what
    # every module runs as, so it is read, and its relative paths resolved, where
    # the project is. a program run in the tree (`--in-build`) is in the project's
    # directory that the tree's directory is a copy of
    here = os.getcwd()
    within = os.path.relpath(os.path.realpath(here), _TREE)
    project_dir = here if within.split(os.sep)[0] == os.pardir else os.path.join(ROOT, within)
    os.chdir(project_dir)
    try:
        # no data file: what was measured is read back below, and a `.coverage` left
        # in the directory the program ran in would be a second, unmapped report of it
        measurement = coverage.Coverage(data_file=None)
        measurement.set_option(
            "run:plugins", [*measurement.get_option("run:plugins"), "_by_coverage_plugin"]
        )
        with warnings.catch_warnings():
            # a core that runs no plugin's tracer says so, and the plugin traces
            # nothing: it only lists the `.by` files, which coverage.py asks it for
            # whatever the core
            warnings.filterwarnings(
                "ignore",
                message=r"Plugin file tracers \(_by_coverage_plugin\._ByModules\) aren't supported",
                category=CoverageWarning,
            )
            measurement.start()
    finally:
        os.chdir(here)
    atexit.register(_write_coverage, measurement, report)


def _analysed(measurement, filename):
    """The statements of `filename` and those of them that ran, as coverage.py counts
    them, or `None` for a file coverage.py cannot read as python."""
    try:
        _, statements, _, missing, _ = measurement.analysis2(filename)
    except Exception:
        # a file python ran that has no python source to read: code compiled from a
        # string under a made-up filename, which coverage.py's own report skips too
        return None
    return statements, sorted(set(statements) - set(missing))


def _generated_analysis(measurement, filename, lines):
    """The statements of the generated `.py` at `filename` and those of them `lines`
    ran, as coverage.py's report reads them: each line that ran is put on the
    statement it is part of. `None` when coverage.py cannot read the file."""
    from coverage.exceptions import NoSource
    from coverage.python import PythonFileReporter

    try:
        reporter = PythonFileReporter(filename, measurement)
        statements = reporter.lines()
    except NoSource:
        return None
    return statements, reporter.translate_lines(lines) & statements


def _write_coverage(measurement, report):
    """What ran, with every line of a transpiled module moved to its `.by` line.

    coverage.py records a module by the filename and lines of its code objects. one
    `_by_code` compiled is already the `.by` with its lines, and one another loader
    compiled — an import hook `compile_like` was not told of — is the staged `.py`
    with its own.
    either way the statements are the generated python's, so each module's lines are
    put back on its `.py`, analysed there, and only then moved through the sourcemap
    to the `.by`. a `.by` line is covered when any statement written for it ran.

    every other file is reported as coverage.py measured it, staged copies included:
    `by run` puts those back where they came from once the run ends.
    """
    measurement.stop()
    data = measurement.get_data()
    ran = {}
    files = {}
    for measured in data.measured_files():
        real = os.path.realpath(measured)
        lines = data.lines(measured) or []
        if real in _BY_SOURCES:
            key = _BY_SOURCES[real]
            by_lines = set(lines)
            generated = SOURCEMAP[key][1]
            ran.setdefault(key, set()).update(
                index + 1
                for index, line in enumerate(generated)
                if line is not None and line + 1 in by_lines
            )
        elif real in _BY_MAP:
            ran.setdefault(_BY_MAP[real][0], set()).update(lines)
        else:
            analysed = _analysed(measurement, measured)
            if analysed is not None:
                files[measured] = {"statements": analysed[0], "executed": analysed[1]}
    for key, lines in ran.items():
        if key in _STALE:
            # its `.by` was saved over during the run, so no line of it is the line
            # this ran as
            continue
        by_path, generated = SOURCEMAP[key]
        analysed = _generated_analysis(measurement, key, lines)
        if analysed is None:
            continue
        statements, executed = (
            {
                generated[number - 1] + 1
                for number in numbers
                if 0 < number <= len(generated) and generated[number - 1] is not None
            }
            for numbers in analysed
        )
        entry = files.setdefault(by_path, {"statements": [], "executed": []})
        entry["statements"] = sorted(statements.union(entry["statements"]))
        entry["executed"] = sorted(executed.union(entry["executed"]))
    with open(report, "w", encoding="utf-8") as handle:
        json.dump({"files": files}, handle)


def _start_profile(snapshot):
    """A profiler for the program, which writes what it measured to `snapshot` once
    the program has ended: after its other threads and its own `atexit` handlers,
    which python runs before the ones registered ahead of them."""
    import cProfile

    global _TRANSPILER_WRITTEN
    _TRANSPILER_WRITTEN = {}
    profiler = cProfile.Profile()
    atexit.register(_write_profile, profiler, snapshot)
    # registered after it, so it runs before it: python calls it, where no frame of the
    # runner's can be measured calling it, and the profile has ended by the time the runner
    # writes it
    atexit.register(profiler.disable)
    return profiler


def _summed(left, right):
    """Two measurements of one function added up, as `pstats` adds two profiles: the
    call counts and times, and for a function's own row the callers after them, each
    caller's added up the same way."""
    counts = tuple(a + b for a, b in zip(left[:4], right[:4]))
    if len(left) == 4:
        return counts
    callers = dict(left[4])
    for caller, timing in right[4].items():
        callers[caller] = _summed(callers[caller], timing) if caller in callers else timing
    return (*counts, callers)


def _apportioned(count, shares):
    """`count` calls shared out by `shares`, which add up to 1, in whole calls that add up
    to `count`: each its share rounded down, and the calls left over to the largest
    remainders."""
    exact = {key: count * share for key, share in shares.items()}
    whole = {key: int(value) for key, value in exact.items()}
    left = count - sum(whole.values())
    for key in sorted(exact, key=lambda key: exact[key] - whole[key], reverse=True)[:left]:
        whole[key] += 1
    return whole


def _without_machinery(stats, machinery):
    """`stats`, a `pstats` table, without the functions `machinery` names: by's own, which
    the program called without knowing it — the loader the runner puts on every staged
    module, and pytest's plugin.

    A function taken out is folded into the functions that called it: its own time is
    spent in them, and what it called they call — python's import machinery calls
    python's own finder, as it does under `python main.py`, where the runner's finder in
    front of it called it. `pstats` keeps one total for each caller of
    a function rather than one for each call, so a function called from two places is
    shared between them by how often each called it. The functions nearest the program go
    first, so a chain of by's functions under one of the program's folds into it whole.

    A function of by's that nothing measured called — one python called on its own, as it
    calls the excepthook — leaves with what it called: those calls are taken off each
    function it called, and a function called from nowhere else leaves with it.
    """
    rows = {key: [*row[:4], dict(row[4])] for key, row in stats.items()}
    callees = {key: set() for key in rows}
    for key, row in rows.items():
        for caller in row[4]:
            callees[caller].add(key)
    pending = [key for key in rows if key in machinery]

    def drop(key, timing):
        # the calls `timing` counts are taken off `key`, which leaves with them when they
        # were all it was called for
        row = rows[key]
        row[0] -= timing[1]
        row[1] -= timing[0]
        row[2] -= timing[2]
        row[3] -= timing[3]
        if row[1] > 0 or row[4]:
            return
        del rows[key]
        if key in pending:
            pending.remove(key)
        for callee in callees.pop(key):
            if callee in rows:
                drop(callee, rows[callee][4].pop(key))

    while pending:
        key = next(
            (
                key
                for key in pending
                if not any(caller in pending and caller != key for caller in rows[key][4])
            ),
            pending[0],
        )
        pending.remove(key)
        _, _, inline, _, callers = rows.pop(key)
        callers.pop(key, None)
        called = callees.pop(key)
        called.discard(key)
        outgoing = {callee: rows[callee][4].pop(key) for callee in called if callee in rows}
        for caller in callers:
            callees[caller].discard(key)
        if not callers:
            for callee, timing in outgoing.items():
                drop(callee, timing)
            continue
        calls = sum(timing[0] for timing in callers.values())
        shares = {
            caller: timing[0] / calls if calls else 1 / len(callers)
            for caller, timing in callers.items()
        }
        for caller, share in shares.items():
            rows[caller][2] += inline * share
        for callee, (count, primitive, inline_time, total_time) in outgoing.items():
            counts = _apportioned(count, shares)
            primitives = _apportioned(primitive, shares)
            for caller, share in shares.items():
                part = (counts[caller], primitives[caller], inline_time * share, total_time * share)
                had = rows[callee][4].get(caller)
                rows[callee][4][caller] = part if had is None else _summed(had, part)
                callees[caller].add(callee)
    return {key: (*row[:4], row[4]) for key, row in rows.items()}


def _write_profile(profiler, snapshot):
    """What `profiler` measured, as the `pstats` file `cProfile` writes, with every
    function of a staged module named by the project file and line it was written at.

    A function is the file, first line and name of its code object. One `_by_code`
    compiled is the `.by` already; one another loader compiled — an import hook
    `compile_like` was not told of — is the staged `.py`, at the generated python's
    lines, and moves to its `.by` here, as a staged copy of a hand-written `.py` moves
    to the project file. The two can then name one function, and are added up as one.
    A line the transpiler wrote on its own has no `.by` line, and is line 0.

    The runtime's own code is the exception: a helper the transpiler wrote into a
    module, and one in a copy of the runtime written beside a package's modules, are
    `<by runtime>` at the line of `_by_runtime.py` they are, under their own names, so a
    helper is one function wherever it ran from, and two helpers of one name are two.

    By's own functions are not in it: see `_without_machinery`.
    """
    located = {}
    parsed = {}

    def function(code):
        # a builtin is its name, as `cProfile` keys it
        if isinstance(code, str):
            return ("~", 0, code)
        if id(code) not in located:
            filename, line, name = code.co_filename, code.co_firstlineno, code.co_name
            written = _TRANSPILER_WRITTEN.get(id(code))
            if written is not None and written[0] is code:
                # moved to the `.by`, where it has no line: the staged module's line says
                # what it is
                _, filename, line = written
            runtime_line = _runtime_line(filename, line, name, parsed)
            real = os.path.realpath(filename)
            if runtime_line is not None:
                located[id(code)] = (_RUNTIME_FILE, runtime_line, name)
            elif real in _BY_MAP or real in _CARRIED:
                path, line = project_location(filename, line)
                located[id(code)] = (path, line or 0, name)
            else:
                located[id(code)] = (filename, line, name)
        return located[id(code)]

    # what `cProfile.Profile.create_stats` makes of the entries, keyed as above rather than
    # by each code object's own file, line and name, under which two functions can collide
    entries = profiler.getstats()
    stats = {}
    for entry in entries:
        key = function(entry.code)
        row = (
            entry.callcount - entry.reccallcount,
            entry.callcount,
            entry.inlinetime,
            entry.totaltime,
            {},
        )
        stats[key] = _summed(stats[key], row) if key in stats else row
    for entry in entries:
        caller = function(entry.code)
        for call in entry.calls or ():
            callee = function(call.code)
            if callee not in stats:
                continue
            timing = (
                call.callcount,
                call.callcount - call.reccallcount,
                call.inlinetime,
                call.totaltime,
            )
            callers = stats[callee][4]
            callers[caller] = _summed(callers[caller], timing) if caller in callers else timing
    stats = _without_machinery(
        stats, {key for key in stats if os.path.realpath(key[0]) in _MACHINERY}
    )
    with open(snapshot, "wb") as handle:
        marshal.dump(stats, handle)


def _runtime_line(path, line, name, parsed):
    """The line of `_by_runtime.py`, as `by` writes it, that the code named `name` starting
    at `line` of the staged file at `path` is, when it is the runtime's: in a copy of the
    runtime written beside a package's modules, or in one of the runtime's definitions the
    transpiler wrote into a module. `None` for any other code. `parsed` keeps each module's
    top-level statements once read."""
    import ast
    import pathlib

    real = os.path.realpath(path)
    # `is_relative_to` rather than `os.path.commonpath`, which raises for a path on another
    # drive than the tree's
    if os.path.basename(real) == "_by_runtime.py" and pathlib.PurePath(real).is_relative_to(
        _TREE
    ):
        # no project file can take this name in the tree, which the staging refuses
        return line
    entry = _BY_MAP.get(real)
    # a module's own code starts at its first line, which can be a definition of the
    # runtime's, and is the module's all the same
    if entry is None or not line or name == "<module>":
        return None
    _, (_, lines) = entry
    if real not in parsed:
        with open(real, "rb") as handle:
            parsed[real] = ast.parse(handle.read()).body
    statements = parsed[real]

    def start(stmt):
        return min([stmt.lineno, *(d.lineno for d in getattr(stmt, "decorator_list", ()))])

    def written(stmt):
        first = start(stmt)
        return not (0 < first <= len(lines) and lines[first - 1] is not None)

    def bound(stmt):
        if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            return [stmt.name]
        if isinstance(stmt, ast.Assign):
            return [target.id for target in stmt.targets if isinstance(target, ast.Name)]
        if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
            return [stmt.target.id]
        if isinstance(stmt, (ast.Import, ast.ImportFrom)):
            return [alias.asname or alias.name.split(".")[0] for alias in stmt.names]
        return []

    index = next(
        (
            index
            for index, stmt in enumerate(statements)
            if start(stmt) <= line <= stmt.end_lineno
        ),
        None,
    )
    if index is None:
        return None
    # a statement that binds nothing is part of the definition before it, as `runtime.rs`
    # slices the runtime into definitions
    while index > 0 and not bound(statements[index]) and written(statements[index]):
        index -= 1
    stmt = statements[index]
    if not written(stmt):
        return None
    for name in bound(stmt):
        if name in _RUNTIME_LINES:
            return _RUNTIME_LINES[name] + line - start(stmt)
    return None

def main():
    # the plugins `by run` puts in the tree reach this module by name; it is running
    # as `__main__`, which the program's own module takes over below
    sys.modules["_by_runner"] = sys.modules[__name__]
    sys.excepthook = _excepthook
    sys.meta_path.insert(0, _ByFinder())
    # `by run --coverage` and `--profile` name where to write what was measured in
    # front of the module, where no module name can be
    written = {}
    while sys.argv[1] in ("--coverage", "--profile"):
        written[sys.argv[1]] = sys.argv[2]
        del sys.argv[1:3]
    module = sys.argv[1]
    sys.argv = sys.argv[1:]
    if "--coverage" in written:
        _start_coverage(written["--coverage"])
    profiler = None
    if "--profile" in written:
        profiler = _start_profile(written["--profile"])
    # nothing here catches what the program raises. an exception it does not catch leaves
    # the interpreter as it leaves `python main.py`: python reports it through the
    # excepthook above and exits 1. a debugger watching for an exception the program did not
    # catch then sees the program's own, with the program's frames on top, where catching
    # it here had it see a `SystemExit` the runner raised in a frame of its own
    _run_as_main(module, profiler)


if __name__ == "__main__":
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
/// `consume` with the transpiled Python. Answers the exit status the command
/// ends with: a failure when something was reported, and an internal error when
/// that was `by` itself panicking, the way `by check` tells the two apart
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
    format: OutputFormat,
    emit: &mut Emit<'_>,
    consume: impl FnMut(&Transpiled<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<ExitStatus> {
    let emitted = by_stage::emit::check_and_transpile(db, handles, emit, consume)?;
    if !emitted.diagnostics.is_empty() {
        render_diagnostics_as(format, db, &emitted.diagnostics)?;
    }
    Ok(if emitted.ok {
        ExitStatus::Success
    } else if emitted.diagnostics.iter().any(|d| d.severity().is_fatal()) {
        ExitStatus::InternalError
    } else {
        ExitStatus::Failure
    })
}

/// [`render_diagnostics_as`] in the full format, which every command but
/// `build` prints its diagnostics in.
fn render_diagnostics(db: &ProjectDatabase, diagnostics: &[Diagnostic]) -> anyhow::Result<()> {
    render_diagnostics_as(OutputFormat::Full, db, diagnostics)
}

/// Render diagnostics to stderr in `format`, as `by check` renders it. The
/// transpiled output goes to stdout, so diagnostics belong on stderr to keep
/// the two streams separable.
///
/// The count is a sentence for a person, so a format made for a program goes
/// without it, as `by check`'s does.
#[allow(clippy::print_stderr)]
fn render_diagnostics_as(
    format: OutputFormat,
    db: &ProjectDatabase,
    diagnostics: &[Diagnostic],
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let display_config = DisplayDiagnosticConfig::new("ty")
        .color(colored::control::SHOULD_COLORIZE.should_colorize())
        .context(0);
    let mut stderr = std::io::stderr().lock();
    write!(
        stderr,
        "{}",
        format.display(db, &display_config, diagnostics)
    )?;
    if !format.is_human_readable() {
        return Ok(());
    }
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
