//! Checking a project's sources and turning the ones that pass into python.
//!
//! The one transpile path. `by run`, `by build`, `by transpile <dir>` and the
//! server's re-stage of an edit all arrive here, which is the point: two paths
//! could disagree about what a file lowers to, and a build tree whose modules were
//! written by one and re-staged by the other is a tree the debugger will refuse —
//! or worse, accept while the line table describes something else.
//!
//! Nothing here prints. Diagnostics come back as values, because a CLI renders
//! them to a terminal and a language server hands them to an editor, and a
//! pipeline that wrote to stderr could only ever serve the first.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use by_transforms::config::Config;
use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};
use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, Severity, Span};
use ruff_db::panic::PanicActivity;
use ruff_db::source::source_text;
use ty_project::ProjectDatabase;
use ty_project::parallel::ParallelIteratorExt;

use crate::project::Rebuilder;
use crate::runtime::{RuntimeLayout, RuntimePackage};
use crate::staging::transpiled_destination;

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

/// how and where a build turns its sources into python: one value, because it
/// is one decision taken once per command and carried unchanged through the run
pub struct Emit<'a> {
    /// what the transpiler lowers for. its `runtime_module` is settled per file
    pub config: &'a Config,
    pub gate: CheckGate,
    pub rebuilder: &'a Rebuilder,
    /// what the emitted python needs that the standard library does not provide
    pub requirements: &'a mut by_transforms::RuntimeRequirements,
    /// where the runtime helpers go, and which copy each module imports. `None`
    /// pastes the definitions into every module instead
    pub runtime: Option<&'a mut RuntimeLayout>,
    /// the project's module roots, longest first, and the project root
    pub roots: &'a [PathBuf],
    pub root: &'a Path,
}

/// one module the transpile fans out over
struct Job<'a> {
    by_path: &'a Path,
    file: ruff_db::files::File,
    /// the package whose runtime copy the module imports, when the build writes
    /// one
    runtime_package: Option<RuntimePackage>,
}

/// what one module's transpile came to
enum Outcome {
    Transpiled {
        python: String,
        line_map: Vec<Option<u32>>,
        requirements: by_transforms::RuntimeRequirements,
    },
    /// the transpiler declined to emit the module, for whatever reason it gave
    Failed(by_transforms::TranspileError),
    /// the transpiler panicked, reported the way a check reports a panic
    Panicked(Diagnostic),
}

impl Outcome {
    fn is_failure(&self) -> bool {
        !matches!(self, Outcome::Transpiled { .. })
    }
}

/// Check every file, then for each non-blocked file call `consume` with the
/// transpiled Python.
pub fn check_and_transpile(
    db: &ProjectDatabase,
    handles: &[(PathBuf, ruff_db::files::File)],
    emit: &mut Emit<'_>,
    mut consume: impl FnMut(&Transpiled<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<Emitted> {
    let Emit {
        config,
        gate,
        rebuilder,
        requirements,
        runtime,
        roots,
        root,
    } = emit;
    let gate = *gate;
    let mut all_diagnostics: Vec<Diagnostic> = Vec::new();
    let mut unusable: Vec<ruff_db::files::File> = Vec::new();

    // the same fan-out `by check` uses. two separate wins, and it took measuring
    // several variants to tell them apart: the fan-out is worth about half the
    // wall time, and the rest — a 10x drop in *total* cpu — comes from
    // `map_with_db` handing each rayon job its own `Db` clone. a serial loop that
    // only recreates the handle per file gets that second win too (37.5s wall /
    // 16.6s cpu, against this version's 17.8s / 23.1s), so the saving is the fresh
    // handle rather than the parallelism.
    //
    // *why* a fresh handle is cheaper is not established. a salsa clone shares its
    // memo tables, so the only per-handle state is `ZalsaLocal`; the cost that
    // disappears is real re-inference rather than bookkeeping, and one file in the
    // project accounts for nearly all of it. what is ruled out: this is not a
    // general tax on holding a handle. `by check` over the same project on a single
    // serial handle is 9.7s of cpu for every file, with no sign of the effect, so
    // whatever this is belongs to what a *build* does — checking and transpiling
    // through the same handle while type-aware passes build databases of their own.
    //
    // deliberately without `with_min_len`, which the neighbouring `map_with_db`
    // callers use to cut database-cloning overhead: here the cloning *is* the win.
    // batching files into fewer jobs measured 4.5x slower, and cloning only every
    // 32 files gives the saving up entirely — so anything that reduces the number
    // of clones silently undoes this
    //
    // ignoring the check mode, because these files are what the output is made of:
    // a language server whose mode reports diagnostics only for open files still
    // has to refuse a closed one that does not check, before its bytes are written
    // into a tree a program is running out of
    let checked: Vec<Vec<Diagnostic>> = handles
        .par_iter()
        .map_with_db(db, |db, (_, file)| {
            ty_project::check_file_ignoring_check_mode(db, *file)
        })
        .collect();
    for ((_, file), diags) in handles.iter().zip(checked) {
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
    // the helpers a module calls come from a copy written beside it, and which
    // copy depends on where it lands, so the config is settled per file rather
    // than once for the build
    let system = ruff_db::Db::system(db);
    let runtime_wanted = runtime.is_some();
    let jobs: Vec<Job<'_>> = handles
        .iter()
        .filter(|(_, file)| !unusable.contains(file))
        .map(|(by_path, file)| Job {
            by_path,
            file: *file,
            runtime_package: if runtime_wanted {
                RuntimePackage::of(
                    &transpiled_destination(roots, root, by_path),
                    by_path,
                    system,
                )
            } else {
                None
            },
        })
        .collect();
    let finished = fan_out(jobs, gate == CheckGate::AllErrors, db.clone(), |db, job| {
        let config = Config {
            runtime_module: job.runtime_package.as_ref().map(RuntimePackage::module),
            ..config.clone()
        };
        let rebuild = || Some(rebuilder.rebuild());
        // caught on the worker that ran it, because the hook that records a
        // panic's location and backtrace is armed per thread
        let transpiled = ty_project::catch_panic(
            &*db,
            job.file,
            PanicActivity::Transpiling,
            AssertUnwindSafe(|| {
                by_transforms::transpile_typed_with_report(&*db, job.file, &config, Some(&rebuild))
            }),
        );
        match transpiled {
            Ok(Ok((python, line_map, requirements))) => Outcome::Transpiled {
                python,
                line_map,
                requirements,
            },
            Ok(Err(error)) => Outcome::Failed(error),
            Err(panic) => Outcome::Panicked(panic),
        }
    });
    for (job, outcome) in finished {
        // a job is skipped only after an earlier one failed, and the hand-over
        // has already stopped there
        let Some(outcome) = outcome else {
            break;
        };
        if let (Some(layout), Some(package)) = (runtime.as_deref_mut(), &job.runtime_package) {
            layout.record(package);
        }
        let failure = match outcome {
            Outcome::Transpiled {
                python,
                line_map,
                requirements: needed,
            } => {
                requirements.merge(needed);
                let by_source = source_text(db, job.file);
                consume(&Transpiled {
                    by_path: job.by_path,
                    by_source: by_source.as_str(),
                    python: &python,
                    line_map: &line_map,
                })?;
                continue;
            }
            Outcome::Failed(error) => transpile_failure_diagnostic(job.file, &error),
            Outcome::Panicked(diagnostic) => diagnostic,
        };
        all_diagnostics.push(failure);
        ok = false;
        if gate == CheckGate::AllErrors {
            break;
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

/// Run `transpile` over every job in parallel, and hand each job back with what
/// it came to, in the jobs' own order whatever order they finished in.
///
/// With `stop_at_first_failure`, a job is not started once a job before it has
/// failed: the caller stops at the first failure in order, so nothing after it is
/// needed. It comes back with no outcome.
///
/// Each job gets its own copy of `handle`, as the check's jobs each get their own
/// handle on the project db, and none is attached for the whole job: a type-aware
/// pass analyses rewritten source in databases of its own, and salsa refuses to
/// attach a second database on a thread that already has one attached. That holds
/// only because a rayon worker has nothing attached between jobs, so a rayon job
/// that runs attached must never block on nested parallelism, where its worker
/// could steal one of these jobs and run it under that attachment.
fn fan_out<J, H>(
    jobs: Vec<J>,
    stop_at_first_failure: bool,
    handle: H,
    transpile: impl Fn(&mut H, &J) -> Outcome + Sync + Send,
) -> Vec<(J, Option<Outcome>)>
where
    J: Send,
    H: Clone + Send,
{
    let first_failure = AtomicUsize::new(usize::MAX);
    jobs.into_par_iter()
        .enumerate()
        .map_with(handle, |handle, (index, job)| {
            if index > first_failure.load(Ordering::Relaxed) {
                return (job, None);
            }
            let outcome = transpile(handle, &job);
            if stop_at_first_failure && outcome.is_failure() {
                first_failure.fetch_min(index, Ordering::Relaxed);
            }
            (job, Some(outcome))
        })
        .collect()
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
///
/// The id is the failure's own kind, because the three read differently to whoever gets
/// them: a refusal is about the module and says what to rename, while `invalid-syntax` and
/// `internal-error` are about the transpiler and the next step is to report them.
pub fn transpile_failure_diagnostic(
    file: ruff_db::files::File,
    err: &by_transforms::TranspileError,
) -> Diagnostic {
    let id = match err.kind {
        by_transforms::TranspileErrorKind::InvalidSyntax => DiagnosticId::InvalidSyntax,
        by_transforms::TranspileErrorKind::Refused => DiagnosticId::TranspileRefused,
        by_transforms::TranspileErrorKind::Bug => DiagnosticId::InternalError,
    };
    let mut diag = Diagnostic::new(id, Severity::Error, err.message.clone());
    if let Some(range) = err.by_range {
        diag.annotate(Annotation::primary(Span::from(file).with_range(range)));
    }
    diag
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use ruff_db::diagnostic::{Diagnostic, DiagnosticId, Severity};

    use super::{Outcome, fan_out};

    fn transpiled() -> Outcome {
        Outcome::Transpiled {
            python: String::new(),
            line_map: Vec::new(),
            requirements: by_transforms::RuntimeRequirements::default(),
        }
    }

    fn panicked() -> Outcome {
        Outcome::Panicked(Diagnostic::new(
            DiagnosticId::Panic,
            Severity::Fatal,
            "the transpiler is broken",
        ))
    }

    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("a thread pool")
    }

    /// the first job cannot finish until the second has, so the order they finish in is the
    /// reverse of theirs, and the jobs still come back in their own
    #[test]
    fn jobs_come_back_in_their_own_order_whatever_order_they_finish_in() {
        let (second_finished, wait_for_second) = mpsc::channel::<()>();
        let wait_for_second = Mutex::new(wait_for_second);
        let second_finished = Mutex::new(second_finished);
        let saw_the_second_finish = AtomicUsize::new(0);

        let finished = pool(2).install(|| {
            fan_out(vec![0, 1], false, (), |(), job: &usize| {
                if *job == 0 {
                    let waited = wait_for_second
                        .lock()
                        .expect("the receiver")
                        .recv_timeout(Duration::from_secs(30));
                    if waited.is_ok() {
                        saw_the_second_finish.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    let _ = second_finished.lock().expect("the sender").send(());
                }
                transpiled()
            })
        });

        assert_eq!(
            saw_the_second_finish.load(Ordering::Relaxed),
            1,
            "the two jobs did not run at the same time"
        );
        let order: Vec<usize> = finished.iter().map(|(job, _)| *job).collect();
        assert_eq!(order, [0, 1]);
        assert!(finished.iter().all(|(_, outcome)| outcome.is_some()));
    }

    /// one thread runs the jobs in their order, so every job after the failed first one is
    /// one that had not started when it failed
    #[test]
    fn a_job_after_a_failure_is_not_started_when_the_caller_stops_there() {
        let started = AtomicUsize::new(0);
        let finished = pool(1).install(|| {
            fan_out(vec![0, 1, 2, 3], true, (), |(), job: &usize| {
                started.fetch_add(1, Ordering::Relaxed);
                if *job == 0 { panicked() } else { transpiled() }
            })
        });

        assert_eq!(started.load(Ordering::Relaxed), 1);
        let ran: Vec<bool> = finished
            .iter()
            .map(|(_, outcome)| outcome.is_some())
            .collect();
        assert_eq!(ran, [true, false, false, false]);
    }

    /// a build reports every module it could not emit, so a failure stops nothing there
    #[test]
    fn every_job_runs_when_the_caller_goes_on_past_a_failure() {
        let finished = pool(1).install(|| {
            fan_out(vec![0, 1, 2], false, (), |(), job: &usize| {
                if *job == 0 { panicked() } else { transpiled() }
            })
        });

        assert!(finished.iter().all(|(_, outcome)| outcome.is_some()));
    }
}
