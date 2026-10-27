//! `by run --coverage`: what a run executed, as an LCOV report against the files
//! the project is written in.
//!
//! The measuring happens in the program's own process, where coverage.py runs:
//! the runner `by run` stages measures the program, moves each transpiled
//! module's lines to its `.by` through the sourcemap it already holds, and writes
//! what it found as JSON (see `_write_coverage` in the runner). A `.py` the
//! project carries over runs as its project file, but one an import hook other
//! than pytest's assertion rewriter compiled itself runs as the copy it compiled,
//! in a temporary directory that is deleted when the run ends. the staging knows what each copy is a copy of, so the
//! report is finished here: every staged copy is put back on the file it was
//! copied from, and the runner's own files, which belong to no project, are left
//! out.
//!
//! LCOV because it is the format the tools that read coverage share: an editor's
//! gutter, `genhtml`, a CI service. Each line is reported as run once or not at
//! all, since coverage.py records whether a line ran rather than how often.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// What the runner measured, keyed by the path it ran as.
#[derive(Debug, Deserialize)]
struct Measured {
    files: BTreeMap<String, Lines>,
}

#[derive(Debug, Default, Deserialize)]
struct Lines {
    /// every line that holds the start of a statement
    statements: Vec<u32>,
    /// the statement lines that ran
    executed: Vec<u32>,
}

/// One file's lines in the report.
#[derive(Debug, Default, PartialEq, Eq)]
struct Covered {
    statements: BTreeSet<u32>,
    executed: BTreeSet<u32>,
}

/// Turn the runner's measurement at `measured` into an LCOV report at `report`.
///
/// `tree` is the staged directory the program ran in, and `carried` maps a path
/// within it to the project file it is a copy of.
pub(crate) fn write_report(
    measured: &Path,
    report: &Path,
    tree: &Path,
    carried: &BTreeMap<PathBuf, PathBuf>,
) -> anyhow::Result<()> {
    let text = fs::read_to_string(measured)
        .with_context(|| format!("could not read {}", measured.display()))?;
    let measured: Measured = serde_json::from_str(&text)
        .with_context(|| format!("could not parse {}", measured.display()))?;
    // python reports a file under its real path, and a temporary directory is
    // reached through a symlink on macos, so the tree is recognised either way
    let trees: Vec<PathBuf> = std::iter::once(tree.to_path_buf())
        .chain(fs::canonicalize(tree).ok())
        .collect();
    let covered = project_files(measured, &trees, carried);
    if let Some(parent) = report
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(report, render_lcov(&covered))
        .with_context(|| format!("could not write {}", report.display()))
}

/// The measurement with every staged copy moved back to its project file, and
/// everything else in the staged tree dropped.
fn project_files(
    measured: Measured,
    trees: &[PathBuf],
    carried: &BTreeMap<PathBuf, PathBuf>,
) -> BTreeMap<PathBuf, Covered> {
    let mut covered: BTreeMap<PathBuf, Covered> = BTreeMap::new();
    for (path, lines) in measured.files {
        let path = PathBuf::from(path);
        let staged = trees.iter().find_map(|tree| path.strip_prefix(tree).ok());
        let file = match staged {
            // the runner, the sourcemap and the runtime helpers are staged from
            // nothing, and a transpiled module's lines were moved to its `.by`
            // already, so only a copy is put back
            Some(relative) => match carried.get(relative) {
                Some(source) => source.clone(),
                None => continue,
            },
            None => path,
        };
        let entry = covered.entry(file).or_default();
        entry.statements.extend(&lines.statements);
        entry.statements.extend(&lines.executed);
        entry.executed.extend(lines.executed);
    }
    covered
}

/// `covered` as LCOV tracefile records, one per file.
fn render_lcov(covered: &BTreeMap<PathBuf, Covered>) -> String {
    let mut out = String::new();
    for (file, lines) in covered {
        let _ = writeln!(out, "TN:");
        let _ = writeln!(out, "SF:{}", file.display());
        for line in &lines.statements {
            let hits = u32::from(lines.executed.contains(line));
            let _ = writeln!(out, "DA:{line},{hits}");
        }
        let _ = writeln!(out, "LF:{}", lines.statements.len());
        let _ = writeln!(out, "LH:{}", lines.executed.len());
        let _ = writeln!(out, "end_of_record");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(json: &str) -> Measured {
        serde_json::from_str(json).expect("valid measurement")
    }

    #[test]
    fn a_file_outside_the_tree_is_reported_where_it_ran() {
        let covered = project_files(
            measured(r#"{"files": {"/p/main.by": {"statements": [1, 2, 4], "executed": [1, 4]}}}"#),
            &[PathBuf::from("/tmp/run")],
            &BTreeMap::new(),
        );
        assert_eq!(
            render_lcov(&covered),
            "TN:\nSF:/p/main.by\nDA:1,1\nDA:2,0\nDA:4,1\nLF:3\nLH:2\nend_of_record\n"
        );
    }

    /// the copy ran out of a directory that is gone by the time anyone reads the
    /// report, so it is reported against the file it was copied from
    #[test]
    fn a_staged_copy_is_reported_against_its_project_file() {
        let carried = BTreeMap::from([(
            PathBuf::from("pkg/helper.py"),
            PathBuf::from("/p/pkg/helper.py"),
        )]);
        let covered = project_files(
            measured(
                r#"{"files": {"/private/tmp/run/pkg/helper.py": {"statements": [1], "executed": [1]}}}"#,
            ),
            &[PathBuf::from("/tmp/run"), PathBuf::from("/private/tmp/run")],
            &carried,
        );
        assert_eq!(
            covered.keys().collect::<Vec<_>>(),
            [Path::new("/p/pkg/helper.py")]
        );
    }

    /// the runner and the sourcemap are `by run`'s, not the project's
    #[test]
    fn a_file_staged_from_nothing_is_left_out() {
        let covered = project_files(
            measured(
                r#"{"files": {"/tmp/run/_by_runner.py": {"statements": [1], "executed": [1]}}}"#,
            ),
            &[PathBuf::from("/tmp/run")],
            &BTreeMap::new(),
        );
        assert!(covered.is_empty(), "{covered:?}");
    }

    /// a project's `.py` can run both from where it was written and as the copy
    /// staged beside the transpiled modules, and a line either of them ran is covered
    #[test]
    fn two_measurements_of_one_file_are_merged() {
        let carried = BTreeMap::from([(PathBuf::from("helper.py"), PathBuf::from("/p/helper.py"))]);
        let covered = project_files(
            measured(
                r#"{"files": {
                    "/p/helper.py": {"statements": [1, 2], "executed": [1]},
                    "/tmp/run/helper.py": {"statements": [1, 2], "executed": [2]}
                }}"#,
            ),
            &[PathBuf::from("/tmp/run")],
            &carried,
        );
        assert_eq!(
            render_lcov(&covered),
            "TN:\nSF:/p/helper.py\nDA:1,1\nDA:2,1\nLF:2\nLH:2\nend_of_record\n"
        );
    }
}
