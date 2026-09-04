//! Everything the transpiler did not produce.
//!
//! This is what makes the output a project rather than a heap of transpiled
//! modules: a hand-written `.py` sibling, a `py.typed`, a template, a fixture —
//! all of it lands in the same relative place, so the output imports and reads
//! data exactly the way the source tree does.
//!
//! The walk is a single implementation because two callers need to agree about
//! its result. The build copies what it finds; the language server's single-file
//! re-stage asks whether one path is in it, and where that path landed. A
//! re-stage that answered from a predicate written a second time could say yes
//! about a file the build never copied, and the plugin would then write a module
//! into the tree that the build's manifest does not know about — one the next
//! build silently leaves behind.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ruff_db::system::SystemPath;
use ty_project::{Db, ProjectDatabase};
use walkdir::WalkDir;

use crate::project::may_hold_build_content;
use crate::staging::{Staging, relative_destination};

/// One file carried into the output unchanged.
struct Verbatim {
    /// where it lands, relative to the output root
    relative: PathBuf,
    /// the file it is copied from
    source: PathBuf,
}

/// Every file the build carries over untouched, in walk order.
///
/// `out` is where the tree is being written, so that an output directory inside
/// the project is not an input to itself. (`by run` stages into a temp directory,
/// where the question never arises.)
fn verbatim_files(
    db: &ProjectDatabase,
    root: &Path,
    roots: &[PathBuf],
    out: &Path,
) -> Vec<Verbatim> {
    let settings = db.project().settings(db);
    let build = settings.build();
    // a file the project excludes from itself is not part of what it ships, so
    // `src.exclude` bounds the build and `build.exclude` narrows it further
    let src = settings.src();

    let mut found = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            // the output tree is not an input to itself, wherever `--out` put it
            if entry.path() == out {
                return false;
            }
            if !may_hold_build_content(entry) {
                return false;
            }
            !entry.file_type().is_dir()
                || SystemPath::from_std_path(entry.path()).is_none_or(|path| {
                    build.is_directory_included(path) && src.is_directory_included(path)
                })
        })
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if entry.path_is_symlink() || !entry.file_type().is_file() {
            continue;
        }
        let extension = path.extension().and_then(OsStr::to_str);
        // a `.by` is an input, and it is carried over only to be read by a
        // downstream basedpython project — never for python to import
        if matches!(extension, Some("by" | "byi")) && !build.sources() {
            continue;
        }
        let Some(system_path) = SystemPath::from_std_path(path) else {
            continue;
        };
        if !build.is_file_included(system_path) || !src.is_file_included(system_path) {
            continue;
        }
        found.push(Verbatim {
            relative: relative_destination(roots, root, path),
            source: path.to_path_buf(),
        });
    }
    found
}

/// Carry every file the transpiler did not produce into the output tree.
pub fn stage_verbatim(
    db: &ProjectDatabase,
    root: &Path,
    roots: &[PathBuf],
    staging: &mut Staging,
) -> anyhow::Result<()> {
    let out = staging.out().to_path_buf();
    for file in verbatim_files(db, root, roots, &out) {
        staging.copy(&file.relative, &file.source)?;
    }
    Ok(())
}

/// Where `path` lands in the output, if the build carries it over unchanged.
///
/// `None` when the build does not stage this file at all — it is excluded, it is
/// a `.by` in a build that does not ship sources, it is outside the project. A
/// caller asking about a file it means to re-stage has to treat that as a
/// refusal: a path the build never wrote is not a slot in the tree.
pub(crate) fn verbatim_destination(
    db: &ProjectDatabase,
    root: &Path,
    roots: &[PathBuf],
    out: &Path,
    path: &Path,
) -> Option<PathBuf> {
    let wanted = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    verbatim_files(db, root, roots, out)
        .into_iter()
        .find(|file| {
            file.source == wanted
                || std::fs::canonicalize(&file.source).is_ok_and(|source| source == wanted)
        })
        .map(|file| file.relative)
}
