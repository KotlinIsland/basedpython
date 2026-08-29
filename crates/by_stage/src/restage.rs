//! Re-staging one file into a build tree that already exists.
//!
//! A build tree is what actually runs. `by run` transpiles the project into a
//! temp directory, copies every other project file into it, and runs the program
//! out of there — so nothing the user edits is the file the process is executing:
//! a `.by` because it was transpiled, a hand-written `.py` because it was copied.
//! Replacing a function in a running program therefore means putting new bytes in
//! the tree first, and only then asking the debugger to take them.
//!
//! Rebuilding the whole tree to do that is not an option. Measured on a 97-file
//! project, `by check` is 8.5 seconds and `by build` is 24.9; one file's share of
//! that is about 165 milliseconds. A button press can afford the second number and
//! not the first, and the difference is the entire reason this operation exists.
//!
//! Two rules shape everything below.
//!
//! **It writes nothing.** The result is the bytes and where they go. The plugin
//! writes them, because the plugin is the only party that can roll that write back
//! together with the debugger request that follows it — a tree updated for a
//! replacement the debugger then refused is a tree that lies about what is
//! running.
//!
//! **It refuses rather than guesses.** Every refusal below is a case where the
//! bytes produced would not be the bytes the build would have produced, or where
//! the tree cannot be told what it is. A refusal costs the user a rebuild; a wrong
//! answer costs them a debug session that reports lines from a file that no longer
//! exists.

use std::path::{Path, PathBuf};

use ruff_db::diagnostic::{
    Diagnostic, DiagnosticFormat, DisplayDiagnosticConfig, FileResolver, Severity,
};
use ty_project::ProjectDatabase;

use crate::emit::{CheckGate, Transpiled, check_and_transpile};
use crate::project::{BY_SOURCES, Rebuilder, project_sources};
use crate::record::{BuildRecord, build_identity};
use crate::sourcemap::{
    BY_SOURCEMAP_FILENAME, content_digest, describe_module, rewrite_sourcemap_entry,
    sourcemap_key_for,
};
use crate::staging::transpiled_destination;
use crate::verbatim::verbatim_destination;

/// What one file's slot in the tree should now contain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Restaged {
    /// where the bytes go: absolute, inside the build directory
    pub generated: PathBuf,
    /// the full text to write there
    pub content: String,
    /// the full new text of `_by_sourcemap.py`, or `None` when nothing about the
    /// map changed — including for a file that has no entry in it, which is every
    /// file the build copied rather than transpiled
    pub sourcemap: Option<String>,
    /// sha-256 of the source bytes this was produced from
    pub by_digest: String,
    /// sha-256 of `content`
    pub py_digest: String,
    /// whether `content` differs from what is in the tree right now
    ///
    /// Measured against the file on disk rather than against the digest the
    /// sourcemap recorded, because the question a caller is asking is whether
    /// writing this would change the tree — and the tree is the thing that ran.
    /// Re-staging a file nobody edited answers `false`, which is what makes
    /// `true` mean "the user's edit changed something".
    pub changed: bool,
}

/// Why nothing was produced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refusal {
    /// one sentence, written for a user rather than for a log
    pub refused: String,
    /// the diagnostics behind it, when the refusal was the check gate. empty
    /// otherwise
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

/// The answer, either way.
///
/// A refusal is a *result*, not an error. Source that does not check is an
/// ordinary state for a file being edited, and a caller that met it as a protocol
/// error would have to render it as a fault in the server rather than as the
/// reason the button did nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Restage {
    Ready(Restaged),
    Refused(Refusal),
}

impl Restage {
    fn refuse(reason: impl Into<String>) -> Self {
        Self::Refused(Refusal {
            refused: reason.into(),
            diagnostics: Vec::new(),
        })
    }
}

/// Produce what `file`'s slot in `build_directory` should now contain.
///
/// `db` is the project database the file belongs to. The language server passes
/// its own, warm and already indexed, which is the whole reason this is fast
/// enough to sit behind a button; the CLI builds one first. Either way the source
/// text comes out of the db, so an editor's unsaved buffer is what gets
/// transpiled — the answer the user means by "reload this". The digest recorded
/// for the `.by` is then over that buffer, so an unsaved file's tracebacks read as
/// stale until it is saved, which is the honest report rather than a wrong line.
pub fn restage_one(
    db: &ProjectDatabase,
    build_directory: &Path,
    file: &Path,
) -> anyhow::Result<Restage> {
    // absolute from here down, whatever the caller spelled. `generated` is a path
    // the caller writes bytes to and `_by_sourcemap.py` keys its tables by the
    // generated path as the build wrote it — which is absolute — so a relative
    // build directory would produce an answer whose two halves disagreed about
    // where the file is, and a caller resolving the first against its own working
    // directory would write it somewhere the map says nothing about
    let owned;
    let build_directory = if build_directory.is_absolute() {
        build_directory
    } else {
        owned = std::env::current_dir()
            .map(|cwd| cwd.join(build_directory))
            .unwrap_or_else(|_| build_directory.to_path_buf());
        &owned
    };

    let record = match BuildRecord::read(build_directory) {
        Ok(record) => record,
        Err(error) => return Ok(Restage::refuse(error.to_string())),
    };
    // the transpiler is the thing that has to reproduce the build's bytes, and the
    // only handle on the transpiler is which build of `by` is running. a tree
    // written by another one may have been written by another transpiler
    if record.by_version != build_identity() {
        return Ok(Restage::refuse(format!(
            "`{}` was built by by {}, and this is by {} — rebuild it before reloading into it",
            build_directory.display(),
            record.by_version,
            build_identity(),
        )));
    }
    // a native extension has no `__code__` to assign, so there is no replacement
    // for new bytes to become however carefully they are produced
    if record.compiled {
        return Ok(Restage::refuse(format!(
            "`{}` was built with `--compiled`, and a native extension module cannot be replaced \
             while it is running",
            build_directory.display(),
        )));
    }
    let config = match record.config() {
        Ok(config) => config,
        Err(error) => return Ok(Restage::refuse(error.to_string())),
    };

    let is_source = file
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| BY_SOURCES.contains(&extension));
    if is_source {
        restage_transpiled(db, build_directory, &record, &config, file)
    } else {
        restage_verbatim(db, build_directory, &record, file)
    }
}

/// A `.by`: transpile it as the build would have, and move its one entry in the
/// sourcemap.
fn restage_transpiled(
    db: &ProjectDatabase,
    build_directory: &Path,
    record: &BuildRecord,
    config: &by_transforms::config::Config,
    file: &Path,
) -> anyhow::Result<Restage> {
    // the build's own file set, asked the way the build asks it — so a source the
    // project excludes is refused here rather than transpiled into a slot the
    // build never wrote
    let sources = project_sources(db, BY_SOURCES, &record.project_root, Some(build_directory));
    let wanted = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let Some(handle) = sources.into_iter().find(|(path, _)| {
        *path == wanted || std::fs::canonicalize(path).is_ok_and(|path| path == wanted)
    }) else {
        return Ok(Restage::refuse(format!(
            "`{}` is not one of the files `{}` was built from",
            file.display(),
            build_directory.display(),
        )));
    };

    // the path the *db* holds, not the caller's spelling of it, so the destination
    // is computed against roots recorded in the same form
    let relative = transpiled_destination(&record.module_roots, &record.project_root, &handle.0);

    let existing_map = match std::fs::read_to_string(build_directory.join(BY_SOURCEMAP_FILENAME)) {
        Ok(existing) => existing,
        Err(error) => {
            return Ok(Restage::refuse(format!(
                "`{}` has no {BY_SOURCEMAP_FILENAME}, so a reloaded module would have no line \
                 table: {error}",
                build_directory.display(),
            )));
        }
    };
    let Some(key) = sourcemap_key_for(&existing_map, &relative) else {
        return Ok(Restage::refuse(format!(
            "`{}` has no entry in {BY_SOURCEMAP_FILENAME} for `{}`, so it is not part of that build",
            build_directory.display(),
            file.display(),
        )));
    };

    // the generated text travels with its description, because the two are one
    // transpile: a caller that re-read either of them separately could pair a line
    // table with bytes it does not describe
    let mut produced: Option<(crate::sourcemap::TracebackEntry, String)> = None;
    let outcome = check_and_transpile(
        db,
        std::slice::from_ref(&handle),
        config,
        // the gate `by run` uses: a program that fails `by check` must not run, and
        // a module reloaded into a running one is that program continuing
        CheckGate::AllErrors,
        &Rebuilder::for_project(db),
        &mut by_transforms::RuntimeRequirements::default(),
        |emitted: &Transpiled<'_>| {
            produced = Some((describe_module(emitted), emitted.python.to_owned()));
            Ok(())
        },
    )?;

    let Some((mut entry, python)) = produced.filter(|_| outcome.ok) else {
        return Ok(Restage::Refused(Refusal {
            refused: format!(
                "`{}` does not check, so it cannot be reloaded into a running program",
                file.display(),
            ),
            diagnostics: render_diagnostics(db, &outcome.diagnostics),
        }));
    };
    // the key the map already holds, so the rewritten file differs in the two
    // lines that had to move and in nothing else
    entry.py_path = key;

    let rewritten = rewrite_sourcemap_entry(&existing_map, &entry).map_err(|refusal| {
        anyhow::anyhow!(
            "the sourcemap entry vanished between being found and being written: {refusal:?}"
        )
    })?;

    let generated = build_directory.join(&relative);
    Ok(Restage::Ready(Restaged {
        changed: differs_on_disk(&generated, &entry.py_digest),
        content: python,
        sourcemap: (rewritten != existing_map).then_some(rewritten),
        by_digest: entry.by_digest,
        py_digest: entry.py_digest,
        generated,
    }))
}

/// A hand-written `.py`, or anything else the build copies: its own bytes, at the
/// place the build copied them to.
fn restage_verbatim(
    db: &ProjectDatabase,
    build_directory: &Path,
    record: &BuildRecord,
    file: &Path,
) -> anyhow::Result<Restage> {
    let Some(relative) = verbatim_destination(
        db,
        &record.project_root,
        &record.module_roots,
        build_directory,
        file,
    ) else {
        return Ok(Restage::refuse(format!(
            "`{}` is not one of the files `{}` was built from",
            file.display(),
            build_directory.display(),
        )));
    };

    let bytes = std::fs::read(file)
        .map_err(|error| anyhow::anyhow!("could not read {}: {error}", file.display()))?;
    // the build copies bytes, and this request carries text. a source in an
    // encoding json has no way to hold is one the build can stage and this cannot,
    // and saying so is better than handing back something lossy
    let Ok(content) = String::from_utf8(bytes) else {
        return Ok(Restage::refuse(format!(
            "`{}` is not utf-8, so its bytes cannot be sent back as text — rebuild to pick it up",
            file.display(),
        )));
    };

    // one set of bytes, so one digest under both names: the file the build read
    // and the file it wrote are the same file
    let digest = content_digest(content.as_bytes());
    let generated = build_directory.join(&relative);
    Ok(Restage::Ready(Restaged {
        changed: differs_on_disk(&generated, &digest),
        content,
        // a copied file has no line table to move, and a map rewritten to say so
        // would be a map that changed for nothing
        sourcemap: None,
        by_digest: digest.clone(),
        py_digest: digest,
        generated,
    }))
}

/// Whether writing bytes of this digest to `path` would change what is there.
///
/// A path that cannot be read counts as different: a slot the tree does not have
/// yet is one the write fills.
fn differs_on_disk(path: &Path, digest: &str) -> bool {
    !std::fs::read(path).is_ok_and(|bytes| content_digest(&bytes) == digest)
}

/// The diagnostics behind a refusal, one line each.
///
/// Concise rather than the full rendering `by check` prints: these travel as an
/// array of strings to a client that will put them in a list, and a multi-line
/// entry with source carets in it reads as several broken ones.
fn render_diagnostics(db: &dyn FileResolver, diagnostics: &[Diagnostic]) -> Vec<String> {
    let config = DisplayDiagnosticConfig::new("ty")
        .format(DiagnosticFormat::Concise)
        .color(false);
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity() >= Severity::Error)
        .map(|diagnostic| diagnostic.display(db, &config).to_string())
        .map(|rendered| rendered.trim_end().to_owned())
        .collect()
}
