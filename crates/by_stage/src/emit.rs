//! Checking a project's sources and turning the ones that pass into python.
//!
//! The one transpile path. `by run`, `by build`, `by transpile <dir>` and the
//! server's single-file re-stage all arrive here, which is the point: two paths
//! could disagree about what a file lowers to, and a build tree whose modules were
//! written by one and re-staged by the other is a tree the debugger will refuse —
//! or worse, accept while the line table describes something else.
//!
//! Nothing here prints. Diagnostics come back as values, because a CLI renders
//! them to a terminal and a language server hands them to an editor, and a
//! pipeline that wrote to stderr could only ever serve the first.

use std::path::{Path, PathBuf};

use by_transforms::config::Config;
use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, Severity, Span};
use ruff_db::source::source_text;
use ty_project::ProjectDatabase;

use crate::project::Rebuilder;

/// How much of the check outcome blocks emitting output.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckGate {
    /// only parse errors block — type diagnostics are advisory. right for
    /// artifact-producing commands (`build`, `transpile`), where partially
    /// ill-typed code is still worth emitting
    ParseErrorsOnly,
    /// any error-severity diagnostic blocks. right for `run`: a program that
    /// fails `by check` must not execute — the checker's verdict and the
    /// runtime behaviour would otherwise diverge
    AllErrors,
}

/// one transpiled module, as [`check_and_transpile`] hands it over
///
/// the two texts are named rather than positional because a caller that mixes
/// them up — hashing the python as if it were the `.by`, say — would still
/// compile
pub struct Transpiled<'a> {
    pub by_path: &'a Path,
    /// the `.by` text this transpile ran on. it is the same read, not a fresh
    /// one: [`source_text()`] is memoized, so a digest taken here is over the
    /// bytes that actually produced `python`
    pub by_source: &'a str,
    /// the generated python, exactly as the caller is expected to write it out
    pub python: &'a str,
    /// generated line (0-indexed) → the `.by` line it came from
    pub line_map: &'a [Option<u32>],
}

/// What a run of [`check_and_transpile`] found.
///
/// `ok` and the diagnostics answer different questions, and the caller needs
/// both. The artifacts are emitted for everything that could be emitted, so a
/// file mid-edit does not take the rest of the build down; `ok` says whether
/// anything was reported, which is what a `&&` in a script reads. A command that
/// prints `error[...]` and then succeeds is one nothing can be chained onto.
pub struct Emitted {
    /// whether everything asked for was emitted and nothing was reported
    pub ok: bool,
    /// every diagnostic the check and the transpile produced, in the order they
    /// were found. the caller renders them
    pub diagnostics: Vec<Diagnostic>,
    /// whether the gate stopped the transpile before anything was emitted
    pub blocked: bool,
}

/// Check every file, then for each non-blocked file call `consume` with the
/// transpiled Python.
pub fn check_and_transpile(
    db: &ProjectDatabase,
    handles: &[(PathBuf, ruff_db::files::File)],
    config: &Config,
    gate: CheckGate,
    rebuilder: &Rebuilder,
    requirements: &mut by_transforms::RuntimeRequirements,
    mut consume: impl FnMut(&Transpiled<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<Emitted> {
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
        return Ok(Emitted {
            ok: false,
            diagnostics: all_diagnostics,
            blocked: true,
        });
    }

    let mut ok = unusable.is_empty();
    let rebuild = || Some(rebuilder.rebuild());
    for (bpy, file) in handles {
        if unusable.contains(file) {
            continue;
        }
        match by_transforms::transpile_typed_with_report(db, *file, config, Some(&rebuild)) {
            Ok((out, line_map, needed)) => {
                requirements.merge(needed);
                let by_source = source_text(db, *file);
                consume(&Transpiled {
                    by_path: bpy,
                    by_source: by_source.as_str(),
                    python: &out,
                    line_map: &line_map,
                })?;
            }
            Err(e) => {
                all_diagnostics.push(transpile_bug_diagnostic(*file, &e));
                ok = false;
                if gate == CheckGate::AllErrors {
                    break;
                }
            }
        }
    }

    let reported_an_error = all_diagnostics
        .iter()
        .any(|d| d.severity() >= Severity::Error);
    Ok(Emitted {
        ok: ok && !reported_an_error,
        diagnostics: all_diagnostics,
        blocked: false,
    })
}

/// Whether this diagnostic says the file has no source to transpile: it could
/// not be parsed, or it could not be read at all (an encoding ty does not speak,
/// a permission error). Either way the transpiler sees an empty module, so
/// emitting for it would write an empty file over the real one.
pub fn is_unusable_source(d: &Diagnostic) -> bool {
    matches!(d.id(), DiagnosticId::InvalidSyntax | DiagnosticId::Io)
        && d.severity() >= Severity::Error
}

/// Build a diagnostic for a transpile failure, annotated against the `.by`
/// source. When the failure maps back to a `.by` range, attach it so the
/// diagnostic renders with `--> file:line:col` and a source caret like any
/// other; otherwise fall back to a bare message.
pub fn transpile_bug_diagnostic(
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
