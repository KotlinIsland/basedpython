//! Re-staging a set of edited files into a build tree that already exists.
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
//! Three rules shape everything below.
//!
//! **It writes nothing.** The result is the bytes and where they go. The plugin
//! writes them, because the plugin is the only party that can roll that write back
//! together with the debugger request that follows it — a tree updated for a
//! replacement the debugger then refused is a tree that lies about what is
//! running.
//!
//! **It answers for the whole set at once.** `_by_sourcemap.py` is one file
//! holding an entry for every transpiled module, so a set of edits has one map,
//! not one per file. Answering each file on its own would hand back a map per
//! file, each holding the tree's map plus that one file's entry — and whichever a
//! caller wrote last would silently drop every other edit's line table. So the
//! set goes in as a set and comes back with one map that carries all of it.
//!
//! **It refuses rather than guesses.** Every refusal below is a case where the
//! bytes produced would not be the bytes the build would have produced, or where
//! the tree cannot be told what it is. A refusal costs the user a rebuild; a wrong
//! answer costs them a debug session that reports lines from a file that no longer
//! exists. One file refused refuses the set, for the reason the debugger applies a
//! replacement that way: a tree holding half of an edit describes a program that
//! never existed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ruff_db::diagnostic::{
    Diagnostic, DiagnosticFormat, DisplayDiagnosticConfig, FileResolver, Severity,
};
use ty_project::ProjectDatabase;

use crate::emit::{CheckGate, Emit, Transpiled, check_and_transpile};
use crate::project::{BY_SOURCES, Rebuilder, project_sources};
use crate::record::{BuildRecord, build_identity};
use crate::runtime::RuntimeLayout;
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
    /// the file in the project this was produced from, as it was asked about
    source: PathBuf,
    /// where the bytes go: absolute, inside the build directory
    generated: PathBuf,
    /// the full text to write there
    content: String,
    /// sha-256 of the source bytes this was produced from
    by_digest: String,
    /// sha-256 of `content`
    py_digest: String,
    /// whether `content` differs from what is in the tree right now
    ///
    /// Measured against the file on disk rather than against the digest the
    /// sourcemap recorded, because the question a caller is asking is whether
    /// writing this would change the tree — and the tree is the thing that ran.
    /// Re-staging a file nobody edited answers `false`, which is what makes
    /// `true` mean "the user's edit changed something".
    changed: bool,
}

/// Every slot of the set, and the one map that describes all of them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestagedSet {
    /// one per distinct file asked about, in the order they were asked
    files: Vec<Restaged>,
    /// the full new text of `_by_sourcemap.py` with the entry of every
    /// transpiled file of the set moved in it, or `None` when nothing about the
    /// map changed — which includes a set of nothing but files the build copied
    /// rather than transpiled, since none of those has an entry
    sourcemap: Option<String>,
}

/// Why nothing was produced for one file, or for the tree as a whole.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refusal {
    /// the file this is about, or `None` when it is about the tree — a record
    /// that cannot be read, a tree another `by` built — and so about every file
    file: Option<PathBuf>,
    /// one sentence, written for a user rather than for a log
    refused: String,
    /// the diagnostics behind it, when the refusal was the check gate. empty
    /// otherwise
    #[serde(default)]
    diagnostics: Vec<String>,
}

/// Every reason the set was refused.
///
/// All of them rather than the first: a user who fixes one file and presses
/// reload only to be told about the next has been made to find them one at a
/// time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refused {
    refusals: Vec<Refusal>,
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
    Ready(RestagedSet),
    Refused(Refused),
}

impl Restage {
    /// The whole set refused, for a reason that is not about any one file.
    pub fn refuse(reason: impl Into<String>) -> Self {
        Self::Refused(Refused {
            refusals: vec![Refusal {
                file: None,
                refused: reason.into(),
                diagnostics: Vec::new(),
            }],
        })
    }
}

/// What the tree and the project say, read once for the whole set.
struct Tree<'a> {
    db: &'a ProjectDatabase,
    directory: &'a Path,
    record: BuildRecord,
    config: by_transforms::config::Config,
    rebuilder: Rebuilder,
    /// the build's own source files, asked the way the build asks, each beside the path with
    /// its links resolved — worked out once, because every file of the set is looked up against
    /// all of them and resolving a path costs a syscall
    sources: Vec<(PathBuf, ruff_db::files::File, PathBuf)>,
}

/// Produce what each of `files`' slots in `build_directory` should now contain,
/// and the one `_by_sourcemap.py` that describes them together.
///
/// `db` is the project database the files belong to. The language server passes
/// its own, warm and already indexed, which is the whole reason this is fast
/// enough to sit behind a button; the CLI builds one first. Either way the source
/// text comes out of the db, so an editor's unsaved buffer is what gets
/// transpiled — the answer the user means by "reload this". The digest recorded
/// for the `.by` is then over that buffer, so an unsaved file's tracebacks read as
/// stale until it is saved, which is the honest report rather than a wrong line.
pub fn restage(
    db: &ProjectDatabase,
    build_directory: &Path,
    files: &[PathBuf],
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

    if files.is_empty() {
        return Ok(Restage::refuse(
            "no files were named, and an empty set has nothing in it to reload",
        ));
    }

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

    // one file named twice — or under two spellings of one path — is one slot,
    // and answering for it twice would hand a caller two writes to one file
    let mut seen = HashSet::new();
    let files: Vec<&Path> = files
        .iter()
        .filter(|file| seen.insert(std::fs::canonicalize(file).unwrap_or_else(|_| (*file).clone())))
        .map(PathBuf::as_path)
        .collect();

    // read once for the set, and only when the set holds something the map
    // describes: a set of copied files has no line table to move
    let existing_map = if files.iter().any(|file| is_source(file)) {
        match std::fs::read_to_string(build_directory.join(BY_SOURCEMAP_FILENAME)) {
            Ok(existing) => Some(existing),
            Err(error) => {
                return Ok(Restage::refuse(format!(
                    "`{}` has no {BY_SOURCEMAP_FILENAME}, so a reloaded module would have no \
                     line table: {error}",
                    build_directory.display(),
                )));
            }
        }
    } else {
        None
    };

    let tree = Tree {
        db,
        directory: build_directory,
        sources: project_sources(db, BY_SOURCES, &record.project_root, Some(build_directory))
            .into_iter()
            .map(|(path, file)| {
                let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                (path, file, resolved)
            })
            .collect(),
        rebuilder: Rebuilder::for_project(db),
        record,
        config,
    };

    // every entry of the set moved in one text, one after another. each move
    // substitutes that entry's two lines and nothing else, so the order they are
    // applied in cannot change the result
    let mut merged = existing_map.clone();
    let mut restaged = Vec::with_capacity(files.len());
    let mut refusals = Vec::new();
    for file in files {
        // which of the two a file goes through is decided by the file, never by whether a map
        // happened to be read: a `.by` staged verbatim would copy basedpython into the tree
        let outcome = if is_source(file) {
            let Some(map) = merged.as_mut() else {
                return Ok(Restage::refuse(format!(
                    "`{}` is a source file and the set was read without a {BY_SOURCEMAP_FILENAME}",
                    file.display(),
                )));
            };
            restage_transpiled(&tree, file, map)?
        } else {
            restage_verbatim(&tree, file)?
        };
        match outcome {
            Ok(one) => restaged.push(one),
            Err(refusal) => refusals.push(refusal),
        }
    }

    if !refusals.is_empty() {
        return Ok(Restage::Refused(Refused { refusals }));
    }
    Ok(Restage::Ready(RestagedSet {
        files: restaged,
        sourcemap: merged.filter(|merged| Some(merged) != existing_map.as_ref()),
    }))
}

/// Whether the build transpiled `file` rather than copying it.
fn is_source(file: &Path) -> bool {
    file.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| BY_SOURCES.contains(&extension))
}

/// A refusal about one file of the set.
fn refuse_file(file: &Path, refused: String, diagnostics: Vec<String>) -> Refusal {
    Refusal {
        file: Some(file.to_path_buf()),
        refused,
        diagnostics,
    }
}

/// A `.by`: transpile it as the build would have, and move its one entry in
/// `map`, the set's sourcemap so far.
fn restage_transpiled(
    tree: &Tree<'_>,
    file: &Path,
    map: &mut String,
) -> anyhow::Result<Result<Restaged, Refusal>> {
    let db = tree.db;
    // the build's own file set, so a source the project excludes is refused here
    // rather than transpiled into a slot the build never wrote
    let wanted = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let Some((source, source_file, _)) = tree
        .sources
        .iter()
        .find(|(path, _, resolved)| *path == wanted || *resolved == wanted)
    else {
        return Ok(Err(refuse_file(
            file,
            format!(
                "`{}` is not one of the files `{}` was built from",
                file.display(),
                tree.directory.display(),
            ),
            Vec::new(),
        )));
    };

    // the path the *db* holds, not the caller's spelling of it, so the destination
    // is computed against roots recorded in the same form
    let relative =
        transpiled_destination(&tree.record.module_roots, &tree.record.project_root, source);

    let Some(key) = sourcemap_key_for(map, &relative) else {
        return Ok(Err(refuse_file(
            file,
            format!(
                "`{}` has no entry in {BY_SOURCEMAP_FILENAME} for `{}`, so it is not part of that \
                 build",
                tree.directory.display(),
                file.display(),
            ),
            Vec::new(),
        )));
    };

    // the generated text travels with its description, because the two are one
    // transpile: a caller that re-read either of them separately could pair a line
    // table with bytes it does not describe
    let mut produced: Option<(crate::sourcemap::TracebackEntry, String)> = None;
    let outcome = check_and_transpile(
        db,
        std::slice::from_ref(&(source.clone(), *source_file)),
        &mut Emit {
            config: &tree.config,
            // the gate `by run` uses: a program that fails `by check` must not
            // run, and a module reloaded into a running one is that program
            // continuing
            gate: CheckGate::AllErrors,
            rebuilder: &tree.rebuilder,
            requirements: &mut by_transforms::RuntimeRequirements::default(),
            // a re-stage writes modules back into a tree the build already laid
            // out, so what it claims is thrown away: whatever copy of the runtime
            // a module imports is already sitting where the build put it
            runtime: Some(&mut RuntimeLayout::default()),
            roots: &tree.record.module_roots,
            root: &tree.record.project_root,
        },
        |emitted: &Transpiled<'_>| {
            produced = Some((describe_module(emitted), emitted.python.to_owned()));
            Ok(())
        },
    )?;

    let Some((mut entry, python)) = produced.filter(|_| outcome.ok) else {
        return Ok(Err(refuse_file(
            file,
            format!(
                "`{}` does not check, so it cannot be reloaded into a running program",
                file.display(),
            ),
            render_diagnostics(db, &outcome.diagnostics),
        )));
    };
    // the key the map already holds, so the rewritten file differs in the two
    // lines that had to move and in nothing else
    entry.py_path = key;

    *map = rewrite_sourcemap_entry(map, &entry).map_err(|refusal| {
        anyhow::anyhow!(
            "the sourcemap entry vanished between being found and being written: {refusal:?}"
        )
    })?;

    let generated = tree.directory.join(&relative);
    Ok(Ok(Restaged {
        source: file.to_path_buf(),
        changed: differs_on_disk(&generated, &entry.py_digest),
        content: python,
        by_digest: entry.by_digest,
        py_digest: entry.py_digest,
        generated,
    }))
}

/// A hand-written `.py`, or anything else the build copies: its own bytes, at the
/// place the build copied them to.
fn restage_verbatim(tree: &Tree<'_>, file: &Path) -> anyhow::Result<Result<Restaged, Refusal>> {
    let Some(relative) = verbatim_destination(
        tree.db,
        &tree.record.project_root,
        &tree.record.module_roots,
        tree.directory,
        file,
    ) else {
        return Ok(Err(refuse_file(
            file,
            format!(
                "`{}` is not one of the files `{}` was built from",
                file.display(),
                tree.directory.display(),
            ),
            Vec::new(),
        )));
    };

    let bytes = std::fs::read(file)
        .map_err(|error| anyhow::anyhow!("could not read {}: {error}", file.display()))?;
    // the build copies bytes, and this request carries text. a source in an
    // encoding json has no way to hold is one the build can stage and this cannot,
    // and saying so is better than handing back something lossy
    let Ok(content) = String::from_utf8(bytes) else {
        return Ok(Err(refuse_file(
            file,
            format!(
                "`{}` is not utf-8, so its bytes cannot be sent back as text — rebuild to pick \
                 it up",
                file.display(),
            ),
            Vec::new(),
        )));
    };

    // one set of bytes, so one digest under both names: the file the build read
    // and the file it wrote are the same file
    let digest = content_digest(content.as_bytes());
    let generated = tree.directory.join(&relative);
    Ok(Ok(Restaged {
        source: file.to_path_buf(),
        changed: differs_on_disk(&generated, &digest),
        content,
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
