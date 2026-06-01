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
use ruff_db::system::{OsSystem, SystemPath};
use ty_project::{Db, ProjectDatabase, ProjectMetadata};
use walkdir::WalkDir;

use crate::ExitStatus;

pub(crate) fn parse_version(s: &str) -> anyhow::Result<Config> {
    let version = s
        .parse::<PythonVersion>()
        .map_err(|_| anyhow::anyhow!("unknown Python version {s:?} — use e.g. 3.12"))?;
    Ok(Config {
        min_version: version,
        ..Config::default()
    })
}

// ── run ──────────────────────────────────────────────────────────────────────

#[allow(clippy::exit, clippy::print_stderr)]
pub(crate) fn cmd_run(module: &str, min_version: &str) -> anyhow::Result<ExitStatus> {
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
    // `run` executes on a specific interpreter, so target *its* version: the
    // emitted code (dataclass `slots=`, PEP 695 syntax, …) must match what that
    // python actually supports. fall back to the `--min-version` flag only if
    // the interpreter can't be probed
    let config = match detect_python_version(&python) {
        Some(version) => Config {
            min_version: version,
            ..Config::default()
        },
        None => parse_version(min_version)?,
    };
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let tmp = tempfile::TempDir::new().context("failed to create temp directory")?;

    let files = bpy_files(&cwd);
    if files.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Failure);
    }

    let (db, handles) = build_project_db(&cwd, &files)?;
    // each generated `.py` paired with its source `.by` and the line table that
    // lifts generated line numbers back to `.by` lines (for traceback rewriting)
    let mut traceback_entries: Vec<TracebackEntry> = Vec::new();
    let ok = render_check_and_transpile(&db, &handles, &config, |bpy, src, line_map| {
        let rel = bpy.strip_prefix(&cwd).unwrap_or(bpy);
        let py = tmp.path().join(rel).with_extension("py");
        fs::create_dir_all(py.parent().unwrap())?;
        fs::write(&py, src)?;
        traceback_entries.push(TracebackEntry {
            py_path: py,
            by_path: fs::canonicalize(bpy).unwrap_or_else(|_| bpy.to_path_buf()),
            line_map: line_map.to_vec(),
        });
        Ok(())
    })?;
    if !ok {
        return Ok(ExitStatus::Failure);
    }

    write_traceback_runtime(tmp.path(), &traceback_entries)?;

    let status = Command::new(&python)
        .arg(BY_RUNNER_FILENAME)
        .arg(module)
        .current_dir(tmp.path())
        .status()
        .with_context(|| format!("{python}: failed to execute"))?;

    let code = status.code().unwrap_or(1);
    // drop the temp dir explicitly: `process::exit` skips destructors, so
    // exiting while it's still in scope would leak the directory
    drop(tmp);
    std::process::exit(code);
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
pub(crate) fn cmd_build(min_version: &str) -> anyhow::Result<ExitStatus> {
    let config = parse_version(min_version)?;
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let out = cwd.join("out");
    let files = bpy_files(&cwd);

    if files.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }

    let (db, handles) = build_project_db(&cwd, &files)?;
    if !render_check_and_transpile(&db, &handles, &config, |bpy, src, _line_map| {
        let py = out
            .join(bpy.strip_prefix(&cwd).unwrap())
            .with_extension("py");
        fs::create_dir_all(py.parent().unwrap())?;
        fs::write(&py, src)?;
        eprintln!("{} -> {}", bpy.display(), py.display());
        Ok(())
    })? {
        return Ok(ExitStatus::Failure);
    }

    eprintln!("\nbuild complete ({} files)", files.len());
    Ok(ExitStatus::Success)
}

// ── transpile ────────────────────────────────────────────────────────────────

#[allow(clippy::print_stdout)]
pub(crate) fn cmd_transpile(
    file: Option<&PathBuf>,
    reverse: bool,
    min_version: &str,
) -> anyhow::Result<ExitStatus> {
    let config = parse_version(min_version)?;

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

        match by_transforms::transpile_typed(&db, file, &config) {
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

/// directories skipped when walking a project for reverse transpile: virtual
/// envs, caches, vcs metadata, and build outputs — none are first-party source
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
    let files = bpy_files(dir);
    if files.is_empty() {
        eprintln!("no .by files found");
        return Ok(ExitStatus::Success);
    }

    let (db, handles) = build_project_db(dir, &files)?;
    let ok = render_check_and_transpile(&db, &handles, config, |bpy, src, _line_map| {
        let py = bpy.with_extension("py");
        fs::write(&py, src).with_context(|| format!("{}", py.display()))?;
        Ok(())
    })?;
    Ok(if ok {
        ExitStatus::Success
    } else {
        ExitStatus::Failure
    })
}

/// Every first-party `.py`/`.pyi` file under `root`, skipping [`NON_SOURCE_DIRS`]
/// and symlinks.
fn py_source_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            !(e.file_type().is_dir()
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| NON_SOURCE_DIRS.contains(&n)))
        })
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

fn bpy_files(root: &Path) -> Vec<PathBuf> {
    let out = root.join("out");
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            let p = e.path();
            !p.starts_with(&out) && p.extension().is_some_and(|x| x == "by")
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Build a project db rooted at `cwd` with every `.by` file under it set as
/// an included path, returning the db alongside `(source_path, File)` pairs
/// in the same order as the input slice.
fn build_project_db(
    cwd: &Path,
    files: &[PathBuf],
) -> anyhow::Result<(ProjectDatabase, Vec<(PathBuf, ruff_db::files::File)>)> {
    let sys_cwd = SystemPath::from_std_path(cwd)
        .with_context(|| format!("non-utf8 path: {}", cwd.display()))?;
    let system = OsSystem::new(sys_cwd);
    let project_metadata = ProjectMetadata::discover(sys_cwd, &system)
        .with_context(|| format!("failed to discover project at {sys_cwd}"))?;
    let mut db = ProjectDatabase::use_defaults(project_metadata, system);

    let mut handles = Vec::with_capacity(files.len());
    let mut included = Vec::with_capacity(files.len());
    for bpy in files {
        let abs = std::fs::canonicalize(bpy).with_context(|| format!("{}", bpy.display()))?;
        let sys_path = SystemPath::from_std_path(&abs)
            .with_context(|| format!("non-utf8 path: {}", abs.display()))?;
        included.push(sys_path.to_path_buf());
        let f = system_path_to_file(&db, sys_path)
            .with_context(|| format!("file not found in db: {sys_path}"))?;
        handles.push((bpy.clone(), f));
    }
    db.project().set_included_paths(&mut db, included);
    Ok((db, handles))
}

/// Check every file, render diagnostics, then for each non-blocked file call
/// `consume` with the transpiled Python. Returns `Ok(false)` if any file had
/// a parse error or transpiler bug (caller should propagate failure).
fn render_check_and_transpile(
    db: &ProjectDatabase,
    handles: &[(PathBuf, ruff_db::files::File)],
    config: &Config,
    mut consume: impl FnMut(&Path, &str, &[Option<u32>]) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let mut all_diagnostics: Vec<Diagnostic> = Vec::new();
    let mut blocked = false;

    for (_, file) in handles {
        let diags = db.check_file(*file);
        if diags.iter().any(is_parse_error) {
            blocked = true;
        }
        all_diagnostics.extend(diags);
    }

    if blocked {
        render_diagnostics(db, &all_diagnostics)?;
        return Ok(false);
    }

    for (bpy, file) in handles {
        match by_transforms::transpile_typed_with_map(db, *file, config) {
            Ok((out, line_map)) => consume(bpy, &out, &line_map)?,
            Err(e) => {
                all_diagnostics.push(transpile_bug_diagnostic(*file, &e));
                render_diagnostics(db, &all_diagnostics)?;
                return Ok(false);
            }
        }
    }

    if !all_diagnostics.is_empty() {
        render_diagnostics(db, &all_diagnostics)?;
    }
    Ok(true)
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
