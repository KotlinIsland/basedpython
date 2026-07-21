//! adds the `override` modifier to every basedpython typeshed method that
//! genuinely overrides a superclass member
//!
//! unlike the per-file ast rewrites in `by_typeshed_patch`, deciding whether a
//! method overrides needs the whole typeshed type-checked at once (cross-module
//! mro resolution). so this is a separate sync phase that runs ty's own analysis:
//! it type-checks every stub with `missing-override-decorator` enabled and
//! inserts `override` exactly where ty reports a missing decorator. because that
//! is the precise complement of `invalid-explicit-override`, the result never
//! trips the (error-by-default) explicit-override check
//!
//! the on-disk stubs are pointed at as ty's `custom_typeshed`, so the file being
//! checked *is* the stdlib and its `object` is the same class every mro roots on
//! — self-consistent, with no dual definitions between a checked copy and a
//! separate resolution copy. the project root is a throwaway directory disjoint
//! from the stdlib (an overlapping first-party root would shadow and disable the
//! custom stdlib search path), and each stub is opened so it passes the
//! `should_check_file` gate that otherwise suppresses stdlib diagnostics
//!
//! usage: `by_override_patch <typeshed-stdlib-dir>`

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_ranged_value::RangedValue;
use ty_project::metadata::options::{Options, Rules};
use ty_project::{Db as _, ProjectDatabase, ProjectMetadata};
use ty_python_semantic::lint::Level;
use ty_python_semantic::types::check_types;

const LINT: &str = "missing-override-decorator";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(stdlib_arg) = args.next() else {
        bail!("usage: by_override_patch <typeshed-stdlib-dir>");
    };
    if args.next().is_some() {
        bail!("usage: by_override_patch <typeshed-stdlib-dir>");
    }

    let stdlib_dir = fs::canonicalize(&stdlib_arg)
        .with_context(|| format!("resolving typeshed stdlib dir {stdlib_arg}"))?;
    if !stdlib_dir.is_dir() {
        bail!("not a directory: {}", stdlib_dir.display());
    }
    let typeshed_root = stdlib_dir
        .parent()
        .context("typeshed stdlib dir has no parent")?
        .to_path_buf();

    // marking a method can turn a sibling into a newly-detectable override, so
    // iterate a fresh pass (over updated on-disk files) until it reaches a
    // fixpoint. each pass re-checks with a clean database
    let mut total_methods = 0_usize;
    let mut total_files = 0_usize;
    loop {
        let (methods, files) = mark_pass(&stdlib_dir, &typeshed_root)?;
        total_methods += methods;
        total_files += files;
        if methods == 0 {
            break;
        }
    }

    println!("marked {total_methods} method(s) `override` across {total_files} file-pass(es)");
    Ok(())
}

/// type-check the whole typeshed once and insert `override` at every
/// `missing-override-decorator` site. returns (methods marked, files rewritten)
fn mark_pass(stdlib_dir: &Path, typeshed_root: &Path) -> Result<(usize, usize)> {
    // resolve every stdlib import to the on-disk copies we are rewriting (via
    // `custom_typeshed`), so the file being checked *is* the stdlib and its
    // `object` is the same class ty uses for every mro root — self-consistent,
    // no dual definitions. the files are system paths, so they also pass the
    // `should_check_file` gate in `report_lint` once opened
    //
    // the project root must be a directory *disjoint* from the stdlib: if the
    // first-party root overlapped the custom typeshed's `stdlib/`, it would
    // shadow and disable the stdlib search path
    let project_root = env::temp_dir().join("by-override-project");
    fs::create_dir_all(&project_root).context("creating scratch project root")?;
    let system = OsSystem::new(SystemPathBuf::from_path_buf_lossy(project_root.clone()));
    let root = SystemPathBuf::from_path_buf_lossy(project_root);
    let mut metadata = ProjectMetadata::new("typeshed-override", root);
    metadata.apply_options(Options {
        environment: Some(ty_project::metadata::options::EnvironmentOptions {
            typeshed: Some(ty_project::metadata::value::RelativePathBuf::cli(
                SystemPathBuf::from_path_buf_lossy(typeshed_root.to_path_buf()),
            )),
            ..Default::default()
        }),
        rules: Some(Rules::from_iter([(
            RangedValue::cli(LINT.to_string()),
            RangedValue::cli(Level::Error),
        )])),
        ..Options::default()
    });
    let mut db =
        ProjectDatabase::fallible(metadata, system).context("building project database")?;

    let mut byi_files: Vec<PathBuf> = walkdir::WalkDir::new(stdlib_dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "byi"))
        .map(|e| e.path().to_path_buf())
        .collect();
    byi_files.sort();

    // resolve every stub to a `File` and mark it open so `should_check_file`
    // returns true for it regardless of the project's default check mode
    let files: Vec<(PathBuf, File)> = byi_files
        .iter()
        .filter_map(|path| {
            let system_path = SystemPath::from_std_path(path)?;
            let file = system_path_to_file(&db, system_path).ok()?;
            Some((path.clone(), file))
        })
        .collect();
    let project = db.project();
    for (_, file) in &files {
        project.open_file(&mut db, *file);
    }

    let mut files_patched = 0_usize;
    let mut methods_marked = 0_usize;
    for (path, file) in files {
        // offsets of the overriding method names ty flagged as missing `override`
        let mut name_offsets: Vec<usize> = check_types(&db, file)
            .into_iter()
            .filter(|diag| diag.id().is_lint_named(LINT))
            .filter_map(|diag| Some(diag.primary_span_ref()?.range()?.start().to_usize()))
            .collect();
        name_offsets.sort_unstable();
        name_offsets.dedup();
        if name_offsets.is_empty() {
            continue;
        }

        // rewrite the exact source ty parsed (offsets are guaranteed to line up)
        let source = source_text(&db, file);
        let patched = insert_overrides(source.as_str(), &name_offsets);
        fs::write(&path, &patched).with_context(|| format!("writing {}", path.display()))?;
        files_patched += 1;
        methods_marked += name_offsets.len();
    }

    Ok((methods_marked, files_patched))
}

/// insert `override ` before the `def` / `async def` / first modifier of each
/// flagged method. `name_offsets` point at the method names; the modifier goes
/// at the first non-whitespace column of the line the name sits on (after any
/// decorator lines above it)
fn insert_overrides(source: &str, name_offsets: &[usize]) -> String {
    let bytes = source.as_bytes();
    // dedupe by keyword column (an overloaded method is flagged once, but guard
    // anyway) and apply from the end so earlier offsets stay valid
    let columns: BTreeSet<usize> = name_offsets
        .iter()
        .map(|&offset| keyword_column(bytes, offset))
        .collect();
    let mut out = source.to_string();
    for column in columns.into_iter().rev() {
        out.insert_str(column, "override ");
    }
    out
}

/// first non-whitespace byte offset of the line containing `offset`
fn keyword_column(bytes: &[u8], offset: usize) -> usize {
    let mut line_start = offset;
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let mut column = line_start;
    while column < bytes.len() && matches!(bytes[column], b' ' | b'\t') {
        column += 1;
    }
    column
}

#[cfg(test)]
mod tests {
    use super::*;

    /// offset of the method name in `src`, given a unique substring of the name
    fn name_at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    #[test]
    fn inserts_before_plain_and_modified_defs() {
        let src = "\
class C:
    def m(self) -> int
    class def cm(cls) -> int
    final def fm(self) -> int
    async def am(self) -> int
";
        let offsets = [
            name_at(src, "m(self) -> int"),
            name_at(src, "cm"),
            name_at(src, "fm"),
            name_at(src, "am"),
        ];
        let expected = "\
class C:
    override def m(self) -> int
    override class def cm(cls) -> int
    override final def fm(self) -> int
    override async def am(self) -> int
";
        assert_eq!(insert_overrides(src, &offsets), expected);
    }

    #[test]
    fn goes_after_decorator_lines() {
        // the name sits on the `def` line, so the modifier lands there, not on
        // the `@overload` decorator line above it
        let src = "class C:\n    @overload\n    def m(self) -> int\n";
        let expected = "class C:\n    @overload\n    override def m(self) -> int\n";
        assert_eq!(insert_overrides(src, &[name_at(src, "m(self)")]), expected);
    }
}
