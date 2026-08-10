use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use by_transforms::config::{Config, PythonVersion};
use ruff_db::diagnostic::{
    Annotation, Diagnostic, DiagnosticId, DisplayDiagnosticConfig, DisplayDiagnostics, Severity,
    Span,
};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
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
    ruff_db::Db::python_version(&db)
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
        config.checked_cast = !self.no_checked_cast;
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

    let (db, handles, rebuilder, root) = build_project_db(&cwd)?;
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
    let mut roots: Vec<PathBuf> = ty_module_resolver::system_module_search_paths(db)
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
fn module_relative_path(roots: &[PathBuf], cwd: &Path, bpy: &Path) -> PathBuf {
    roots
        .iter()
        .find_map(|root| bpy.strip_prefix(root).ok())
        .or_else(|| bpy.strip_prefix(cwd).ok())
        .unwrap_or(bpy)
        .with_extension("py")
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

    let (db, handles, rebuilder, root) = build_project_db(&cwd)?;
    if handles.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }
    let file_count = handles.len();
    let roots = module_roots(&db, &root);
    if !render_check_and_transpile(
        &db,
        &handles,
        &config,
        CheckGate::ParseErrorsOnly,
        &rebuilder,
        |bpy, src, _line_map| {
            let py = out.join(module_relative_path(&roots, &root, bpy));
            fs::create_dir_all(py.parent().unwrap())?;
            fs::write(&py, src)?;
            eprintln!("{} -> {}", bpy.display(), py.display());
            Ok(())
        },
    )? {
        return Ok(ExitStatus::Failure);
    }

    eprintln!("\nbuild complete ({file_count} files)");
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
        Some(p) => (
            fs::read_to_string(p).with_context(|| format!("{}", p.display()))?,
            Some(p.as_path()),
        ),
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
#[allow(clippy::print_stderr)]
fn reverse_dir(dir: &Path, config: &Config) -> anyhow::Result<ExitStatus> {
    let files = py_source_files(dir);
    if files.is_empty() {
        eprintln!("no .py files found");
        return Ok(ExitStatus::Success);
    }

    let mut count = 0usize;
    for py in &files {
        let source = fs::read_to_string(py).with_context(|| format!("{}", py.display()))?;
        let is_stub = py.extension().and_then(OsStr::to_str) == Some("pyi");
        let file_config = Config {
            is_python: true,
            is_stub,
            ..config.clone()
        };
        let output = by_transforms::reverse_transpile(&source, &file_config)
            .map_err(|e| anyhow::anyhow!("{}: {e}", py.display()))?;
        let by = py.with_extension(if is_stub { "byi" } else { "by" });
        fs::write(&by, output).with_context(|| format!("{}", by.display()))?;
        fs::remove_file(py).with_context(|| format!("{}", py.display()))?;
        count += 1;
    }

    eprintln!("reversed {count} file(s) to basedpython");
    Ok(ExitStatus::Success)
}

/// Forward-transpile every `.by` under `dir` into a `.py` next to it, using one
/// shared project db so cross-module types resolve (the same path as `by
/// build`, but written in place rather than to `out/`).
#[allow(clippy::print_stderr)]
fn forward_dir(dir: &Path, config: &Config) -> anyhow::Result<ExitStatus> {
    let (db, handles, rebuilder, _root) = build_project_db(dir)?;
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
        .any(|name| name.starts_with('.') || NON_SOURCE_DIRS.contains(&name))
}

/// Build a project db rooted at `cwd`, returning it alongside the
/// `(source_path, File)` pair for every basedpython source the *project* claims
/// — the same set `by check` walks, so `src.exclude` and the ignore files it
/// honours apply here too — and the means to build the same project again.
fn build_project_db(cwd: &Path) -> anyhow::Result<ProjectBuild> {
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
        .filter(|file| matches!(file.path(&db).extension(), Some("by" | "byi")))
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
    let mut unparsable: Vec<ruff_db::files::File> = Vec::new();

    for (_, file) in handles {
        let diags = db.check_file(*file);
        if diags.iter().any(is_parse_error) {
            unparsable.push(*file);
        }
        all_diagnostics.extend(diags);
    }
    // running a program is all-or-nothing: a module that does not check would
    // be imported by one that does. producing artifacts is not — a file mid-edit
    // must not take down the build of every unrelated module, which is exactly
    // when a code generator or a test runner is reached for
    let blocked = match gate {
        CheckGate::AllErrors => {
            !unparsable.is_empty()
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

    let mut ok = unparsable.is_empty();
    let rebuild = || Some(rebuilder.rebuild());
    for (bpy, file) in handles {
        if unparsable.contains(file) {
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

fn is_parse_error(d: &Diagnostic) -> bool {
    matches!(d.id(), DiagnosticId::InvalidSyntax) && d.severity() >= Severity::Error
}

/// Render diagnostics to stderr in the same format as `by check`. The
/// transpiled output goes to stdout, so diagnostics belong on stderr to keep
/// the two streams separable.
#[allow(clippy::print_stderr)]
fn render_diagnostics(db: &ProjectDatabase, diagnostics: &[Diagnostic]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let display_config = DisplayDiagnosticConfig::new("ty")
        .color(colored::control::SHOULD_COLORIZE.should_colorize())
        .show_fix_diff(true)
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
