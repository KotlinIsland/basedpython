use std::collections::{BTreeSet, HashMap};
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
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_text_size::TextRange;
use ty_project::{Db, ProjectDatabase, ProjectMetadata};
use walkdir::WalkDir;

use crate::ExitStatus;
use crate::args::LoweringArgs;

/// The python version the emitted code must run on when `--min-version` is not
/// given: the one the project configures (`environment.python-version`, else the
/// `requires-python` lower bound), so the checker and the emitter agree about
/// which python this project targets. Falls back to the transpiler's own default
/// outside a project, or when the configuration names no version.
fn configured_min_version(cwd: &Path) -> PythonVersion {
    let Some(sys_cwd) = SystemPath::from_std_path(cwd) else {
        return Config::default().min_version;
    };
    let system = OsSystem::new(sys_cwd);
    let Ok(metadata) = ProjectMetadata::discover(sys_cwd, &system) else {
        return Config::default().min_version;
    };
    let db = ProjectDatabase::use_defaults(metadata, system);
    db.project()
        .program(&db)
        .python_version(&db)
        .to_string()
        .parse()
        .unwrap_or_else(|_| Config::default().min_version)
}

/// The transpile config for a command whose `--min-version` is optional.
fn version_config(min_version: Option<&str>, cwd: &Path) -> anyhow::Result<Config> {
    match min_version {
        Some(spelled) => parse_version(spelled),
        None => Ok(Config {
            min_version: configured_min_version(cwd),
            ..Config::default()
        }),
    }
}

pub(crate) fn parse_version(s: &str) -> anyhow::Result<Config> {
    let version = s
        .parse::<PythonVersion>()
        .map_err(|_| anyhow::anyhow!("unknown Python version {s:?} — use e.g. 3.12"))?;
    Ok(Config {
        min_version: version,
        ..Config::default()
    })
}

/// Parse a `--soundness` spec: `default` (the inference-gap checks), `all`
/// (those plus the opt-in `parameters` entry checks), `none`, or a
/// comma-separated subset of the position names. Unknown names are a hard
/// error so a typo doesn't silently disable a check the user expected.
pub(crate) fn parse_soundness(spec: &str) -> anyhow::Result<by_transforms::SoundnessPositions> {
    use by_transforms::SoundnessPositions;

    match spec.trim() {
        "default" => return Ok(SoundnessPositions::defaults()),
        "all" => return Ok(SoundnessPositions::all()),
        "none" => return Ok(SoundnessPositions::none()),
        _ => {}
    }
    let mut positions = SoundnessPositions::none();
    for name in spec.split(',') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        match name {
            "generic-calls" => positions.generic_calls = true,
            "projections" => positions.projections = true,
            "iterations" => positions.iterations = true,
            "assignments" => positions.assignments = true,
            "returns" => positions.returns = true,
            "arguments" => positions.arguments = true,
            "parameters" => positions.parameters = true,
            other => anyhow::bail!(
                "unknown soundness position {other:?} — use `default`, `all`, `none`, or a \
                 comma-separated subset of: generic-calls, projections, iterations, assignments, \
                 returns, arguments, parameters"
            ),
        }
    }
    Ok(positions)
}

impl LoweringArgs {
    /// Fold the lowering options into a config already carrying its target
    /// version.
    fn apply(&self, config: &mut Config) -> anyhow::Result<()> {
        config.soundness = parse_soundness(&self.soundness)?;
        config.runtime_raises_checks = self.runtime_raises_checks;
        config.unique_loop_bindings = !self.no_unique_loop_bindings;
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
) -> anyhow::Result<ExitStatus> {
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
    // `run` executes on a specific interpreter, so by default target *its*
    // version: the emitted code (dataclass `slots=`, PEP 695 syntax, …) must
    // match what that python actually supports. an explicit `--min-version`
    // wins, but it cannot exceed the interpreter — that would emit code the
    // interpreter cannot parse
    let probed = detect_python_version(&python);
    let mut config = match (min_version, probed) {
        (Some(flag), probed) => {
            let config = parse_version(flag)?;
            if let Some(interpreter) = probed
                && config.min_version > interpreter
            {
                anyhow::bail!(
                    "--min-version {flag} is newer than `{python}` ({interpreter}), \
                     which could not run the emitted code — \
                     set PYTHON to a {flag}+ interpreter"
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
    lowering.apply(&mut config)?;
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let tmp = tempfile::TempDir::new().context("failed to create temp directory")?;

    let (db, handles, rebuilder, root) = build_project_db(&cwd, BY_SOURCES)?;
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
    let ok = render_check_and_transpile(
        &db,
        &handles,
        &config,
        CheckGate::AllErrors,
        &rebuilder,
        |bpy, src, line_map| {
            let py = tmp.path().join(module_relative_path(&roots, &root, bpy));
            fs::create_dir_all(py.parent().unwrap())?;
            fs::write(&py, src)?;
            traceback_entries.push(TracebackEntry {
                py_path: py,
                by_path: fs::canonicalize(bpy).unwrap_or_else(|_| bpy.to_path_buf()),
                line_map: line_map.to_vec(),
            });
            Ok(())
        },
    )?;
    if !ok {
        return Ok(ExitStatus::Failure);
    }

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
        let toolchain = by_build::Toolchain::probe(&python).with_context(|| {
            format!(
                "could not read `{python}`'s build configuration; \
                 --compiled needs an interpreter with development headers"
            )
        })?;
        let mut built = 0usize;
        for entry in &traceback_entries {
            let source = fs::read_to_string(&entry.by_path)
                .with_context(|| format!("could not read {}", entry.by_path.display()))?;
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
            let mut lowered = by_irbuild::module_from_source(&source, name, options.language);
            lowered.lines = Some(by_ir::function::LineTable::new(
                entry.by_path.display().to_string(),
                &source,
            ));
            // the root of the generated tree, not the directory the `.py` landed
            // in: the build lays the extension out at its module's own place, and
            // handing it the leaf directory would nest the tree inside itself
            by_build::build_lowered(lowered, &source, &toolchain, tmp.path(), &options)
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

    write_traceback_runtime(tmp.path(), &traceback_entries)?;

    let status = Command::new(&python)
        .arg(BY_RUNNER_FILENAME)
        .arg(&module)
        .args(args)
        .current_dir(tmp.path())
        .status()
        .with_context(|| format!("{python}: failed to execute"))?;

    let code = status.code().unwrap_or(1);
    // drop the temp dir explicitly: `process::exit` skips destructors, so
    // exiting while it's still in scope would leak the directory
    drop(tmp);
    std::process::exit(code);
}

/// The project's first-party module roots, longest first, as absolute paths.
///
/// These are the directories a module name is resolved against — for a
/// src-layout project, `src/` before the project root. Only roots inside the
/// project are kept: an emitted tree can only mirror what is being built.
fn module_roots(db: &ProjectDatabase, cwd: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ty_module_resolver::system_module_search_paths(
        db,
        db.project().program(db).resolver_environment(db),
    )
    .map(|path| PathBuf::from(path.as_str()))
    .filter(|path| path.starts_with(cwd))
    .collect();
    // a nested root shadows the one containing it, so the deepest match wins
    roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    roots
}

/// Where `bpy`'s transpiled python goes, relative to the output root.
///
/// The tree mirrored is the *module* tree, not the directory tree: a src-layout
/// project's `src/pkg/main.by` is the module `pkg.main`, so it has to land at
/// `pkg/main.py`. Mirroring the directory instead emits `src/pkg/main.py`,
/// whose module is `src.pkg.main` — a name nothing imports, and one `run.main`
/// cannot sensibly be set to.
fn module_relative_path(roots: &[PathBuf], root: &Path, bpy: &Path) -> PathBuf {
    let relative = roots
        .iter()
        .find_map(|candidate| bpy.strip_prefix(candidate).ok())
        .or_else(|| bpy.strip_prefix(root).ok())
        .unwrap_or(bpy);
    // whatever happened above, the result has to be *relative*: joined onto the
    // output directory an absolute path replaces it outright, so every emitted
    // file would land outside the output tree. keeping only the named components
    // also drops any `..`, which would climb back out of it
    relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<PathBuf>()
        .with_extension("py")
}

/// The dotted module name a file laid out at `relative` will be imported under.
///
/// The tree the generated python is written into *is* the module tree — every
/// file lands at [`module_relative_path`] — so the name is that path with its
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
fn compiled_module_name(
    db: &ProjectDatabase,
    path: &Path,
    file: ruff_db::files::File,
) -> anyhow::Result<by_ir::ModuleName> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("a source file has no usable module name")?;
    Ok(match resolved_module_name(db, file) {
        Some(resolved) if stem == "__init__" => by_ir::ModuleName::package(resolved),
        Some(resolved) => by_ir::ModuleName::new(resolved),
        None => by_ir::ModuleName::new(stem),
    })
}

/// The project's `run.main` entry point, if one is configured.
fn configured_main(db: &ProjectDatabase) -> Option<String> {
    let options = db.project().metadata(db).options();
    let main = options.run.as_ref()?.main.as_ref()?;
    Some((**main).clone())
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
pub(crate) fn cmd_build(
    min_version: Option<&str>,
    lowering: &LoweringArgs,
) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let mut config = version_config(min_version, &cwd)?;
    lowering.apply(&mut config)?;
    let out = cwd.join("out");

    let (db, handles, rebuilder, root) = build_project_db(&cwd, BY_SOURCES)?;
    if handles.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }
    let file_count = handles.len();
    let roots = module_roots(&db, &root);
    let mut packages: BTreeSet<PathBuf> = BTreeSet::new();
    if !render_check_and_transpile(
        &db,
        &handles,
        &config,
        CheckGate::ParseErrorsOnly,
        &rebuilder,
        |bpy, src, _line_map| {
            let relative = module_relative_path(&roots, &root, bpy);
            if relative.components().count() > 1
                && let Some(package) = relative.components().next()
            {
                packages.insert(out.join(package));
            }

            let py = out.join(relative);
            fs::create_dir_all(py.parent().unwrap())?;
            fs::write(&py, src)?;
            eprintln!("{} -> {}", bpy.display(), py.display());
            Ok(())
        },
    )? {
        return Ok(ExitStatus::Failure);
    }

    write_markers(&db, &packages)?;

    eprintln!("\nbuild complete ({file_count} files)");
    Ok(ExitStatus::Success)
}

/// Writes the `by.typed` marker into every package the build emitted.
///
/// The marker is what tells a project that installs this one that its packages
/// are basedpython's, and it carries the one thing a `pyproject.toml` cannot tell
/// them: which of this project's dependencies are part of its own interface.
/// Nothing installs a `pyproject.toml`, and this rides along inside the package.
#[allow(clippy::print_stderr)]
fn write_markers(db: &ProjectDatabase, packages: &BTreeSet<PathBuf>) -> anyhow::Result<()> {
    let exported = db
        .project()
        .settings(db)
        .analysis()
        .exported_dependencies
        .clone()
        .unwrap_or_default();
    let marker = ty_module_resolver::Marker::render(&exported);

    for package in packages {
        let path = package.join(ty_module_resolver::BY_TYPED);
        fs::create_dir_all(package)?;
        fs::write(&path, &marker)?;
    }

    if !exported.is_empty() {
        eprintln!("exporting {}", exported.join(", "));
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
    let mut fallback = Config::default();
    lowering.apply(&mut fallback)?;
    options.fallback = Some(fallback);
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let sources: Vec<PathBuf> = if files.is_empty() {
        compilable_files(&cwd)
    } else {
        files.to_vec()
    };
    if sources.is_empty() {
        eprintln!("no .by or .py files found");
        return Ok(ExitStatus::Success);
    }

    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_string());
    let toolchain = by_build::Toolchain::probe(&python).with_context(|| {
        format!("could not read `{python}`'s build configuration; set PYTHON to an interpreter with development headers")
    })?;

    let out_dir = cwd.join(output);
    let mut compiled = 0usize;
    let mut declined_total = 0usize;

    // one database for the whole project, so a type imported from a sibling module
    // resolves. lowering each file on its own is sound — an unresolved class
    // degrades to the object protocol — but it makes every imported type look
    // gradual, and `--no-any` would then fail on noise
    // `compile` embeds fallback source produced by the untyped transpile, which
    // takes no db, so the rebuilder the other commands thread through is unused here
    let (db, project, _rebuilder, _root) = build_project_db(&cwd, COMPILABLE_SOURCES)?;

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
    let mut names: Vec<by_ir::ModuleName> = Vec::with_capacity(handles.len());
    let mut claimed: HashMap<PathBuf, &Path> = HashMap::new();
    for (path, file) in &handles {
        let name = compiled_module_name(&db, path, *file)?;
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
        names.push(name);
    }

    for ((path, file), name) in handles.iter().zip(names) {
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
            options.language.unique_loop_bindings(),
        );
        // the real path, so a `#line` in the generated C resolves for a debugger
        let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        lowered.lines = Some(by_ir::function::LineTable::new(
            absolute.display().to_string(),
            &source,
        ));

        let built = if emit_c_only {
            by_build::emit_lowered(lowered, &source, &out_dir, &options)
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
    lowering.apply(&mut config)?;

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
        let rebuilder = Rebuilder {
            metadata: project_metadata.clone(),
            root: project_root.to_path_buf(),
            included: vec![sys_path.to_path_buf()],
        };
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

/// non-hidden directories skipped when walking a project (see
/// [`may_contain_sources`]): virtual envs, caches, and build outputs — none
/// are first-party source. hidden directories are skipped wholesale
const NON_SOURCE_DIRS: &[&str] = &[
    ".venv",
    "venv",
    "env",
    ".env",
    "site-packages",
    "__pycache__",
    ".git",
    ".tox",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    "build",
    "dist",
    "node_modules",
    "out",
];

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
    let (db, handles, rebuilder, _root) = build_project_db(dir, BY_SOURCES)?;
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
        |bpy, src, _line_map| {
            let py = bpy.with_extension("py");
            fs::write(&py, src).with_context(|| format!("{}", py.display()))?;
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

/// Whether a project walk may descend into this entry: hidden directories
/// (`.claude`, `.git`, `.venv`, …) and [`NON_SOURCE_DIRS`] never hold
/// first-party source. The walk root itself is always entered, even when the
/// project directory happens to be hidden.
fn may_contain_sources(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| !name.starts_with('.') && !NON_SOURCE_DIRS.contains(&name))
}

// ── traceback rewriting ────────────────────────────────────────────────────────

/// filename of the python entry-point shim `by run` writes into the build dir
const BY_RUNNER_FILENAME: &str = "_by_runner.py";

/// python module the shim imports to translate generated frames back to `.by`
const BY_SOURCEMAP_FILENAME: &str = "_by_sourcemap.py";

/// a generated `.py` file paired with the `.by` it came from and the line table
/// mapping generated lines (0-indexed) back to `.by` lines
struct TracebackEntry {
    py_path: PathBuf,
    by_path: PathBuf,
    line_map: Vec<Option<u32>>,
}

/// Write the sourcemap module + runner shim into the build dir. The shim runs
/// the target module and, on an uncaught exception, rewrites traceback frames
/// in generated files back to their `.by` source location.
fn write_traceback_runtime(dir: &Path, entries: &[TracebackEntry]) -> anyhow::Result<()> {
    use std::fmt::Write as _;

    let mut map_src = String::from(
        "# generated by `by run` — maps transpiled python frames to .by source\nSOURCEMAP = {\n",
    );
    for e in entries {
        let elems: Vec<String> = e
            .line_map
            .iter()
            .map(|m| m.map_or_else(|| "None".to_owned(), |n| n.to_string()))
            .collect();
        let _ = writeln!(
            map_src,
            "    {}: ({}, [{}]),",
            py_str_literal(&e.py_path.to_string_lossy()),
            py_str_literal(&e.by_path.to_string_lossy()),
            elems.join(", "),
        );
    }
    map_src.push_str("}\n");
    fs::write(dir.join(BY_SOURCEMAP_FILENAME), map_src)
        .with_context(|| "failed to write sourcemap module")?;
    fs::write(dir.join(BY_RUNNER_FILENAME), BY_RUNNER_SRC)
        .with_context(|| "failed to write runner shim")?;
    Ok(())
}

/// Render a string as a python string literal (double-quoted, minimal escaping).
fn py_str_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

const BY_RUNNER_SRC: &str = r#"# generated by `by run` — runs the target module with .by-aware tracebacks
import os
import runpy
import sys
import traceback

from _by_sourcemap import SOURCEMAP

# index the sourcemap by realpath so symlinked temp dirs (e.g. /tmp on macOS)
# still match the filenames python reports in frames
_BY_MAP = {os.path.realpath(py): info for py, info in SOURCEMAP.items()}


def _rewrite(frames):
    # drop the runner/runpy bootstrap above the first user frame
    first = next((i for i, f in enumerate(frames) if os.path.realpath(f.filename) in _BY_MAP), None)
    frames = frames[first:] if first is not None else frames
    out = []
    for f in frames:
        info = _BY_MAP.get(os.path.realpath(f.filename))
        if info is not None and f.lineno is not None:
            by_path, lines = info
            idx = f.lineno - 1
            mapped = lines[idx] if 0 <= idx < len(lines) else None
            if mapped is not None:
                import linecache

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


def main():
    sys.excepthook = _excepthook
    module = sys.argv[1]
    sys.argv = sys.argv[1:]
    try:
        runpy.run_module(module, run_name="__main__", alter_sys=True)
    except SystemExit:
        raise
    except BaseException:
        sys.excepthook(*sys.exc_info())
        sys.exit(1)


main()
"#;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Everything needed to build a project db a second time.
///
/// The transpiler asks for one when a pre-pass rewrites the source it hands to
/// phase 0: it then serves the rewritten file out of that db, keeping the
/// project's metadata, search paths and sibling files. The rebuilt db must be
/// independent of the one this command uses — see
/// [`by_transforms::RebuildProject`].
struct Rebuilder {
    metadata: ProjectMetadata,
    root: SystemPathBuf,
    included: Vec<SystemPathBuf>,
}

/// every source under `root` the compiler can lower
///
/// it lowers the `.by` *and* the `.py` ast — one lowering, told apart by
/// [`by_irbuild::Language`]. only the commands that *write beside* a source
/// ([`cmd_transpile`]) are `.by`-only, because a `.py` there would be its own
/// output
fn compilable_files(root: &Path) -> Vec<PathBuf> {
    source_files(root, &["by", "py"])
}

fn source_files(root: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(may_contain_sources)
        .filter_map(Result::ok)
        .filter(|e| {
            !e.path_is_symlink()
                && e.path()
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|x| extensions.contains(&x))
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

impl Rebuilder {
    fn rebuild(&self) -> Box<dyn ty_python_semantic::Db> {
        let mut db =
            ProjectDatabase::use_defaults(self.metadata.clone(), OsSystem::new(&self.root));
        db.project()
            .set_included_paths(&mut db, self.included.clone());
        Box::new(db)
    }
}

/// A project db, the `(source_path, File)` pairs for the `.by` files it was
/// built for, the canonical project root every one of those paths is rooted at,
/// and the means to build the same project again.
///
/// The root is handed back rather than re-derived by each caller: `canonicalize`
/// and `current_dir` do not agree on every platform — on windows the first
/// returns the `\\?\` verbatim form and the second does not — so a caller that
/// re-derived it would find none of the db's paths under it.
type ProjectBuild = (
    ProjectDatabase,
    Vec<(PathBuf, ruff_db::files::File)>,
    Rebuilder,
    PathBuf,
);

/// the sources `build`, `run` and `transpile` claim: a `.py` beside a `.by`
/// is that file's own output, so writing beside it again would be circular
const BY_SOURCES: &[&str] = &["by", "byi"];

/// what `compile` claims. it lowers the `.by` *and* the `.py` ast — one
/// lowering, told apart by [`by_irbuild::Language`] — and emits into an
/// output directory rather than beside the source, so a `.py` is an input
const COMPILABLE_SOURCES: &[&str] = &["by", "byi", "py"];

/// The [`NON_SOURCE_DIRS`] entries that ty's own `src.exclude` defaults don't already drop.
///
/// [`is_hidden_within`] runs over files that have *already* passed the project's file filter,
/// so for a name ty excludes by default — `venv`, `dist`, `node_modules`, `.tox`, … — a file
/// can only have reached it because the configuration deliberately re-included the directory
/// with a negated pattern, which `src.exclude` documents as the way to override a default.
/// Re-dropping such a file here would quietly undo that, and it's why a project could not
/// compile a module of its own that happens to live in a directory named `venv`.
///
/// What's left are the names ty has no default opinion about, where this walk is the only
/// thing keeping a dependency tree or a build output out of the emitted set. The unfiltered
/// [`NON_SOURCE_DIRS`] still applies to [`may_contain_sources`], which walks the file system
/// directly and never sees the project configuration at all.
const NON_SOURCE_DIRS_TY_ALLOWS: &[&str] = &[
    "env",
    ".env",
    "site-packages",
    "__pycache__",
    ".pytest_cache",
    "build",
    "out",
];

/// Whether `path` sits inside a hidden or build-output directory under `root`.
fn is_hidden_within(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .any(|name| name.starts_with('.') || NON_SOURCE_DIRS_TY_ALLOWS.contains(&name))
}

/// Build a project db rooted at `cwd`, returning it alongside the
/// `(source_path, File)` pair for every source the *project* claims whose
/// extension is in `extensions`
/// — the same set `by check` walks, so `src.exclude` and the ignore files it
/// honours apply here too — and the means to build the same project again.
fn build_project_db(cwd: &Path, extensions: &[&str]) -> anyhow::Result<ProjectBuild> {
    // the project root must be canonicalized the same way the included files
    // are (below) so it stays a path *prefix* of them: otherwise a file's
    // search path isn't recognized as first-party and boundary diagnostics
    // (e.g. `subclass-of-sealed-class`) misfire. this bites on windows, where
    // `canonicalize` rewrites files to the `\\?\` long-path form while an
    // un-canonicalized root keeps its short (`RUNNER~1`) components
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let sys_cwd = SystemPath::from_std_path(&canonical_cwd)
        .with_context(|| format!("non-utf8 path: {}", canonical_cwd.display()))?;
    let system = OsSystem::new(sys_cwd);
    let project_metadata = ProjectMetadata::discover(sys_cwd, &system)
        .with_context(|| format!("failed to discover project at {sys_cwd}"))?;
    let metadata = project_metadata.clone();
    let db = ProjectDatabase::use_defaults(project_metadata, system);

    // the project's own file set — the one `by check` walks, so `src.exclude`
    // and the ignore files it honours apply here too. a build that disagreed
    // with the check about which files are in the project reports errors for
    // files the project deliberately excludes, and (before this) wrote nothing
    let mut sources: Vec<(PathBuf, ruff_db::files::File)> = db
        .project()
        .files(&db)
        .into_iter()
        .filter(|file| {
            file.path(&db)
                .extension()
                .is_some_and(|x| extensions.contains(&x))
        })
        .filter_map(|file| {
            let path = file.path(&db).as_system_path()?;
            Some((path.as_std_path().to_path_buf(), file))
        })
        // a hidden directory (`.claude/worktrees`, `.venv`, …) holds copies and
        // dependencies, not this project's sources — emitting them would write
        // a parallel tree nobody asked for
        .filter(|(path, _)| !is_hidden_within(path, &canonical_cwd))
        .collect();
    // the walk is over a hash set, so order is arbitrary; emit deterministically
    sources.sort_by(|(a, _), (b, _)| a.cmp(b));

    let included: Vec<SystemPathBuf> = sources
        .iter()
        .filter_map(|(path, _)| SystemPath::from_std_path(path).map(SystemPath::to_path_buf))
        .collect();
    let rebuilder = Rebuilder {
        metadata,
        root: sys_cwd.to_path_buf(),
        included,
    };
    Ok((db, sources, rebuilder, canonical_cwd))
}

/// How much of the check outcome blocks emitting output.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckGate {
    /// only parse errors block — type diagnostics are advisory. right for
    /// artifact-producing commands (`build`, `transpile`), where partially
    /// ill-typed code is still worth emitting
    ParseErrorsOnly,
    /// any error-severity diagnostic blocks. right for `run`: a program that
    /// fails `by check` must not execute — the checker's verdict and the
    /// runtime behaviour would otherwise diverge
    AllErrors,
}

/// Check every file, render diagnostics, then for each non-blocked file call
/// `consume` with the transpiled Python. Returns `Ok(false)` if the check
/// outcome blocks per `gate`, or a transpiler bug occurred (caller should
/// propagate failure).
fn render_check_and_transpile(
    db: &ProjectDatabase,
    handles: &[(PathBuf, ruff_db::files::File)],
    config: &Config,
    gate: CheckGate,
    rebuilder: &Rebuilder,
    mut consume: impl FnMut(&Path, &str, &[Option<u32>]) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let mut all_diagnostics: Vec<Diagnostic> = Vec::new();
    let mut unusable: Vec<ruff_db::files::File> = Vec::new();

    for (_, file) in handles {
        let diags = db.check_file(*file);
        if diags.iter().any(is_unusable_source) {
            unusable.push(*file);
        }
        all_diagnostics.extend(diags);
    }
    // running a program is all-or-nothing: a module that does not check would
    // be imported by one that does. producing artifacts is not — a file mid-edit
    // must not take down the build of every unrelated module, which is exactly
    // when a code generator or a test runner is reached for
    let blocked = match gate {
        CheckGate::AllErrors => {
            !unusable.is_empty()
                || all_diagnostics
                    .iter()
                    .any(|d| d.severity() >= Severity::Error)
        }
        CheckGate::ParseErrorsOnly => false,
    };

    if blocked {
        render_diagnostics(db, &all_diagnostics)?;
        return Ok(false);
    }

    let mut ok = unusable.is_empty();
    let rebuild = || Some(rebuilder.rebuild());
    for (bpy, file) in handles {
        if unusable.contains(file) {
            continue;
        }
        match by_transforms::transpile_typed_with_map(db, *file, config, Some(&rebuild)) {
            Ok((out, line_map)) => consume(bpy, &out, &line_map)?,
            Err(e) => {
                all_diagnostics.push(transpile_bug_diagnostic(*file, &e));
                ok = false;
                if gate == CheckGate::AllErrors {
                    break;
                }
            }
        }
    }

    if !all_diagnostics.is_empty() {
        render_diagnostics(db, &all_diagnostics)?;
    }
    Ok(ok)
}

/// Whether this diagnostic says the file has no source to transpile: it could
/// not be parsed, or it could not be read at all (an encoding ty does not speak,
/// a permission error). Either way the transpiler sees an empty module, so
/// emitting for it would write an empty file over the real one.
fn is_unusable_source(d: &Diagnostic) -> bool {
    matches!(d.id(), DiagnosticId::InvalidSyntax | DiagnosticId::Io)
        && d.severity() >= Severity::Error
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

fn transpile_bug_diagnostic(
    file: ruff_db::files::File,
    err: &by_transforms::TranspileError,
) -> Diagnostic {
    let mut diag = Diagnostic::new(
        DiagnosticId::InvalidSyntax,
        Severity::Error,
        err.message.clone(),
    );
    if let Some(range) = err.by_range {
        diag.annotate(Annotation::primary(Span::from(file).with_range(range)));
    }
    diag
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
    use super::{
        dotted_module_name, is_hidden_within, module_relative_path, reverse_dir,
        reverse_dir_converting,
    };
    use crate::ExitStatus;
    use by_transforms::config::Config;
    use std::path::{Path, PathBuf};

    /// the output tree mirrors the module tree, so a src-layout project's source
    /// root is stripped rather than mirrored
    #[test]
    fn a_module_root_is_stripped() {
        let roots = vec![PathBuf::from("/p/src"), PathBuf::from("/p")];
        assert_eq!(
            module_relative_path(&roots, Path::new("/p"), Path::new("/p/src/pkg/main.by")),
            PathBuf::from("pkg/main.py")
        );
    }

    /// the deepest root wins: a file under `src` is `pkg.main`, not `src.pkg.main`
    #[test]
    fn the_deepest_root_wins() {
        let roots = vec![PathBuf::from("/p/src"), PathBuf::from("/p")];
        assert_eq!(
            module_relative_path(&roots, Path::new("/p"), Path::new("/p/top.by")),
            PathBuf::from("top.py")
        );
    }

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

    /// a root that shares no prefix with the file — which is what `canonicalize`
    /// and `current_dir` disagreeing produced on windows — must not leave the
    /// path absolute: joining that onto the output directory discards the output
    /// directory entirely, so every emitted file lands outside it
    #[test]
    fn a_root_that_does_not_match_still_yields_a_relative_path() {
        let unrelated = vec![PathBuf::from("/other/src")];
        let emitted =
            module_relative_path(&unrelated, Path::new("/other"), Path::new("/p/pkg/main.by"));
        assert!(
            emitted.is_relative(),
            "an absolute result escapes the output directory: {}",
            emitted.display()
        );
    }

    #[test]
    fn a_hidden_directory_is_not_project_source() {
        let root = Path::new("/p");
        assert!(is_hidden_within(
            Path::new("/p/.claude/worktrees/x/junk.by"),
            root
        ));
        assert!(is_hidden_within(Path::new("/p/out/main.by"), root));
        assert!(!is_hidden_within(Path::new("/p/src/pkg/main.by"), root));
        // the file's own name is not a directory component
        assert!(!is_hidden_within(Path::new("/p/.hidden.by"), root));
        // a name ty excludes by default is left to the project filter, so that a
        // negated `src.exclude` pattern re-including it isn't quietly undone here
        assert!(!is_hidden_within(Path::new("/p/venv/__init__.by"), root));
        assert!(!is_hidden_within(Path::new("/p/dist/main.by"), root));
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
