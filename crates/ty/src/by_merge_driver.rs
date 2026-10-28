//! `by merge-driver`: the merge driver git runs for a python or basedpython file, and for a
//! project's `api.lock`, when both sides of a merge changed it
//!
//! git calls a merge driver for every file both sides changed, conflicting or not, and it is
//! the only point at which anything but git sees a merge git runs itself, from the command line
//! or from an editor: a clean line merge never opens an editor's merge window. so the driver
//! does what git does and then what only a language can:
//!
//! 1. git's own line merge, `git merge-file`, as `git merge` runs it: the histogram diff,
//!    the repository's `merge.conflictStyle`, which `git merge-file` reads itself, and the
//!    marker size and labels git hands the driver
//! 2. each conflict resolved where [`ty_ide::resolve_merge_conflicts`] resolves it
//! 3. the imports reconciled with the three versions by [`ty_ide::reconcile_merge_imports`]: a
//!    clean merge can still drop an import the merged code reads. where conflicts are left, by
//!    [`ty_ide::reconcile_conflicted_merge_imports`], with what either side of each needs and
//!    outside them, so that whatever is chosen in a conflict has its imports
//!
//! the result is clean only when nothing is left: a conflict nothing resolved stays as git
//! writes it, and a name the versions import from different places becomes a conflict over its
//! imports. the driver exits 0 only for a clean result, as git asks of a merge driver
//!
//! `api.lock` is generated from the source, and a merge driver runs while other files may still
//! conflict, or not yet be written: there is no source to generate it from. so where both sides
//! changed it differently, it is never clean: the driver leaves git's line merge in it, conflict
//! or not, and says how to generate it again

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow, bail};
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_python_ast::PySourceType;
use ruff_text_size::TextRange;
use ty_ide::{
    ConflictedText, MergeEdit, MergeTexts, MergeVersion, ReconciledImports, apply_merge_edits,
    file_import_sorter, reconcile_conflicted_merge_imports, reconcile_merge_imports,
    resolve_merge_conflicts,
};
use ty_project::api_lock::API_LOCK_FILE_NAME;
use ty_python_semantic::api_lockfile::ApiLockTarget;

use crate::ExitStatus;

/// what git hands a merge driver, `%O %A %B %L %P %S %X %Y`
pub(crate) struct DriverArgs {
    pub(crate) base: PathBuf,
    /// ours, and where the result is written
    pub(crate) ours: PathBuf,
    pub(crate) theirs: PathBuf,
    pub(crate) marker_size: usize,
    /// the path, relative to the top of the work tree, the result is for
    pub(crate) path: PathBuf,
    pub(crate) base_label: Option<String>,
    pub(crate) ours_label: Option<String>,
    pub(crate) theirs_label: Option<String>,
}

#[allow(clippy::print_stderr)]
pub(crate) fn cmd_merge_driver(args: &DriverArgs) -> anyhow::Result<ExitStatus> {
    let read = |path: &Path| {
        std::fs::read(path).with_context(|| format!("could not read `{}`", path.display()))
    };
    let (base, ours, theirs) = (read(&args.base)?, read(&args.ours)?, read(&args.theirs)?);
    let line_merge = git_merge_file(args, false)?;

    if args.path.file_name().and_then(|name| name.to_str()) == Some(API_LOCK_FILE_NAME) {
        if ours == theirs {
            return Ok(ExitStatus::Success);
        }
        write(&args.ours, &line_merge.text)?;
        eprintln!("{}", api_lock_advice(&args.path, &ours, &theirs));
        return Ok(ExitStatus::Failure);
    }

    let source_type = args
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(PySourceType::try_from_extension);
    let (Some(source_type), Ok(base), Ok(ours), Ok(theirs), Ok(merged)) = (
        source_type,
        String::from_utf8(base),
        String::from_utf8(ours),
        String::from_utf8(theirs),
        String::from_utf8(line_merge.text.clone()),
    ) else {
        // not a file the driver reads, or not text: git's line merge, exactly as git does it
        write(&args.ours, &line_merge.text)?;
        return Ok(line_merge.status());
    };

    let cwd = SystemPathBuf::from_path_buf(
        std::env::current_dir().context("failed to get the current directory")?,
    )
    .map_err(|path| anyhow!("the current directory `{}` is not unicode", path.display()))?;
    let path = SystemPath::from_std_path(&args.path)
        .ok_or_else(|| anyhow!("`{}` is not a unicode path", args.path.display()))?;
    // git runs a merge driver at the top of the work tree, which `%P` is relative to
    let path = cwd.join(path);
    let system = OsSystem::new(&cwd);
    let sort_imports = file_import_sorter(&system, &path, &cwd);

    // the merged text, and whether conflicts are left in it
    let (merged, conflicted) = if line_merge.conflicts == 0 {
        (merged, false)
    } else {
        let has_base = ConflictedText::parse(&merged, args.marker_size)
            .is_some_and(|conflicted| conflicted.has_base());
        // the conflicts are read with their base's lines, which the default style leaves out
        let diff3 = if has_base {
            merged.clone()
        } else {
            String::from_utf8(git_merge_file(args, true)?.text)
                .context("`git merge-file --diff3` wrote text that is not utf-8")?
        };
        let conflicted = ConflictedText::parse(&diff3, args.marker_size)
            .ok_or_else(|| anyhow!("`git merge-file` wrote conflict markers that do not close"))?;
        let versions = conflicted
            .versions()
            .ok_or_else(|| anyhow!("`git merge-file --diff3` wrote a conflict without its base"))?;
        let resolutions = resolve_merge_conflicts(
            MergeTexts {
                base: &versions.base,
                ours: &versions.ours,
                theirs: &versions.theirs,
            },
            &versions.conflicts,
            source_type,
            &sort_imports,
        );
        if resolutions.iter().all(Option::is_none) {
            // exactly what git wrote, in the repository's conflict style
            (merged, true)
        } else {
            let left = resolutions.iter().any(Option::is_none);
            (conflicted.write(&resolutions, has_base), left)
        }
    };

    let texts = MergeTexts {
        base: &base,
        ours: &ours,
        theirs: &theirs,
    };
    // with conflicts left, the imports are reconciled with what either side of each needs, and
    // only outside them: whatever is chosen in a conflict then has its imports
    let reconciled = if conflicted {
        let text = ConflictedText::parse(&merged, args.marker_size)
            .ok_or_else(|| anyhow!("the merged text has conflict markers that do not close"))?;
        reconcile_conflicted_merge_imports(texts, &text, source_type, &sort_imports)
    } else {
        reconcile_merge_imports(texts, &merged, source_type, &sort_imports)
    };
    match reconciled {
        ReconciledImports::Edits(edits) => {
            write(&args.ours, apply_merge_edits(&merged, &edits).as_bytes())?;
            Ok(if conflicted {
                ExitStatus::Failure
            } else {
                ExitStatus::Success
            })
        }
        ReconciledImports::Refused(choice) => {
            // the conflict takes the place of the merged file's imports of the name, so that
            // what is chosen in it is the one import of it
            let mut edits: Vec<MergeEdit> = choice
                .bindings
                .iter()
                .map(|range| MergeEdit {
                    range: *range,
                    replacement: String::new(),
                })
                .collect();
            edits.push(MergeEdit {
                range: TextRange::empty(choice.offset),
                replacement: import_conflict(args, &choice.imports, conflict_style_has_base()?),
            });
            write(&args.ours, apply_merge_edits(&merged, &edits).as_bytes())?;
            eprintln!(
                "by: `{}` is imported from different places by the versions merged into `{}`: \
                 the conflict among its imports is which one the code means",
                choice.name,
                args.path.display()
            );
            Ok(ExitStatus::Failure)
        }
    }
}

/// what `git merge-file` wrote, and how many conflicts it left
struct LineMerge {
    text: Vec<u8>,
    conflicts: i32,
}

impl LineMerge {
    /// what git makes of the merge without the driver: conflicted where anything conflicts
    fn status(&self) -> ExitStatus {
        if self.conflicts == 0 {
            ExitStatus::Success
        } else {
            ExitStatus::Failure
        }
    }
}

/// git's line merge of the three versions, as `git merge` merges a file: with the histogram
/// diff, which `git merge` uses where `git merge-file` defaults to myers, and in the
/// repository's `merge.conflictStyle`, or `diff3` where `diff3` says
fn git_merge_file(args: &DriverArgs, diff3: bool) -> anyhow::Result<LineMerge> {
    let mut command = Command::new("git");
    command
        .arg("merge-file")
        .arg("--stdout")
        .arg("--diff-algorithm=histogram")
        .arg(format!("--marker-size={}", args.marker_size));
    if diff3 {
        command.arg("--diff3");
    }
    // git's own labels, which it gives a driver that asks with `%S %X %Y`
    if let (Some(ours), Some(base), Some(theirs)) =
        (&args.ours_label, &args.base_label, &args.theirs_label)
    {
        command.args(["-L", ours, "-L", base, "-L", theirs]);
    }
    command.arg(&args.ours).arg(&args.base).arg(&args.theirs);
    let output = command.output().context("could not run `git merge-file`")?;
    // the number of conflicts, at most 127, or negative for an error
    match output.status.code() {
        Some(conflicts @ 0..=127) => Ok(LineMerge {
            text: output.stdout,
            conflicts,
        }),
        _ => bail!(
            "`git merge-file` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// whether the repository's `merge.conflictStyle` writes the base's lines, as `diff3` and
/// `zdiff3` do
fn conflict_style_has_base() -> anyhow::Result<bool> {
    let output = Command::new("git")
        .args(["config", "--get", "merge.conflictStyle"])
        .output()
        .context("could not run `git config`")?;
    let style = String::from_utf8_lossy(&output.stdout);
    Ok(matches!(style.trim(), "diff3" | "zdiff3"))
}

/// the conflict over a name the versions import from different places: each side's imports of
/// it, and the base's where the conflict style shows it
fn import_conflict(
    args: &DriverArgs,
    imports: &[(MergeVersion, Vec<String>)],
    with_base: bool,
) -> String {
    let marker = |character: char, label: Option<&str>| {
        let marker = character.to_string().repeat(args.marker_size);
        match label {
            Some(label) if !label.is_empty() => format!("{marker} {label}\n"),
            _ => format!("{marker}\n"),
        }
    };
    let statements = |wanted: MergeVersion| -> String {
        imports
            .iter()
            .filter(|(version, _)| *version == wanted)
            .flat_map(|(_, statements)| statements.iter().map(|statement| format!("{statement}\n")))
            .collect()
    };
    let mut out = marker('<', args.ours_label.as_deref());
    out.push_str(&statements(MergeVersion::Ours));
    if with_base {
        out.push_str(&marker('|', args.base_label.as_deref()));
        out.push_str(&statements(MergeVersion::Base));
    }
    out.push_str(&marker('=', None));
    out.push_str(&statements(MergeVersion::Theirs));
    out.push_str(&marker('>', args.theirs_label.as_deref()));
    out
}

/// what to tell someone whose merge changed `api.lock` on both sides: the command that writes
/// it for the python each side's lockfile names
fn api_lock_advice(path: &Path, ours: &[u8], theirs: &[u8]) -> String {
    let target = |lockfile: &[u8]| {
        ApiLockTarget::of_lockfile(&String::from_utf8_lossy(lockfile)).and_then(Result::ok)
    };
    let command = |target: Option<ApiLockTarget>| match target {
        Some(ApiLockTarget::Version(version)) => {
            format!("by generate-api-file --python-version {version}")
        }
        Some(ApiLockTarget::Project) | None => "by generate-api-file".to_string(),
    };
    let (ours, theirs) = (command(target(ours)), command(target(theirs)));
    let generate = if ours == theirs {
        format!("`{ours}`")
    } else {
        format!("`{ours}` (ours) or `{theirs}` (theirs)")
    };
    format!(
        "by: `{path}` is generated from the source, which may still conflict while git merges, so \
         by does not merge it: once the merge's other conflicts are resolved, run {generate} in \
         its project and `git add {path}`",
        path = path.display()
    )
}

fn write(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("could not write `{}`", path.display()))
}
