//! `_by_sourcemap.py` — which `.by` line a generated python frame came from, and
//! whether that answer is still true.
//!
//! Two tables, keyed alike by the generated path exactly as it is spelled in the
//! file. `SOURCEMAP` maps generated line to `.by` line; `DIGESTS` carries the
//! sha-256 of both files the entry describes, because `SOURCEMAP` alone cannot be
//! checked — a `.by` edited since the transpile leaves it describing a pair of
//! files that no longer exists, and every line it then reports is wrong with total
//! confidence.
//!
//! a module's line table and its columns are each packed into one string, which a
//! reader unpacks the first time it is asked about that module ([`UNPACK_SRC`]).
//! `by run` imports the map in a tree it has just staged, so no bytecode of it is
//! ever reused, and python compiling a literal list for every generated line of a
//! large project took longer than the program it was about to run
//!
//! It is written whole by a build and edited one entry at a time by a re-stage.
//! The re-stage is a line substitution rather than a re-render, and deliberately:
//! the contract with the debugger is that every *other* entry comes out
//! byte-identical, and the only way to promise that without qualification is never
//! to have regenerated them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use by_transforms::columns::{ColumnOrigin, Columns};

use crate::emit::Transpiled;
use crate::record::build_identity;
use crate::staging::Staging;

/// python module the runner shim imports to translate generated frames back to
/// `.by`
pub(crate) const BY_SOURCEMAP_FILENAME: &str = "_by_sourcemap.py";

/// a generated `.py` file paired with the `.by` it came from and the line table
/// mapping generated lines (0-indexed) back to `.by` lines
pub struct TracebackEntry {
    pub py_path: PathBuf,
    pub by_path: PathBuf,
    pub line_map: Vec<Option<u32>>,
    /// for each generated line, the runs of it that are the author's text, with where in the
    /// `.by` each starts, and the qualifiers the transpiler wrote in front of the author's names
    pub columns: Columns,
    /// the `.by` text the transpile ran on, kept so nothing downstream has to
    /// read the file a second time and risk reading a different one
    pub by_source: String,
    /// digest of the `.by` bytes the transpiler read
    pub by_digest: String,
    /// digest of the generated python bytes it wrote
    pub py_digest: String,
}

/// stage one emitted module and describe it, in that order: the digests are over
/// the bytes that just landed on disk
///
/// it goes through the staging rather than straight to the path, so that a module
/// two sources both claim is reported, and so that the file is one the manifest
/// knows about and a later build can clean up
pub fn stage_module(
    staging: &mut Staging,
    relative: &Path,
    emitted: &Transpiled<'_>,
) -> anyhow::Result<TracebackEntry> {
    staging.write(relative, Some(emitted.by_path), emitted.python)?;
    Ok(TracebackEntry {
        py_path: staging.out().join(relative),
        ..describe_module(emitted)
    })
}

/// The same description [`stage_module`] produces, for a caller that is not going
/// to write the file itself.
///
/// The language server's re-stage of an edit is that caller: it returns the bytes
/// and lets the plugin write them, because only the plugin can roll the write back
/// together with the debugger request that follows it. Its `py_path` is filled in
/// by the caller, which is the one thing a description without a staging cannot
/// know.
pub(crate) fn describe_module(emitted: &Transpiled<'_>) -> TracebackEntry {
    TracebackEntry {
        py_path: PathBuf::new(),
        by_path: dunce::canonicalize(emitted.by_path)
            .unwrap_or_else(|_| emitted.by_path.to_path_buf()),
        line_map: emitted.line_map.to_vec(),
        columns: emitted.columns.to_vec(),
        by_source: emitted.by_source.to_owned(),
        by_digest: content_digest(emitted.by_source.as_bytes()),
        py_digest: content_digest(emitted.python.as_bytes()),
    }
}

/// a content digest as `_by_sourcemap.py` spells it: `sha256:` then lowercase
/// hex
///
/// the algorithm is named in the value so it can be changed later without
/// breaking readers — a reader that meets an algorithm it does not know refuses
/// the entry, instead of comparing a hex it could never have produced
pub(crate) fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// what in a tree is `by`'s own python rather than the program's, which the map declares
/// beside its tables: a debugger looks for the map where the program is, and finds this with
/// it
#[derive(Debug, Default)]
pub struct OwnCode {
    /// the script, relative to the tree, that a program in it is started through, which
    /// makes the program's entry module `__main__` and runs it. `None` for a tree no program
    /// is started from
    pub launcher: Option<PathBuf>,
    /// every file of `by`'s own python in the tree, relative to it, the launcher and the map
    /// itself among them. a debugger that steps through only the program's own code leaves
    /// these out, as it leaves out the standard library
    pub files: Vec<PathBuf>,
}

/// write the sourcemap module beside the generated python it describes
///
/// both tables are keyed by the generated path exactly as written here. a
/// consumer that normalises those keys — the runner shim resolves symlinks, for
/// one — has to keep the original key to reach the entry's digests
pub fn write_sourcemap_module(
    staging: &mut Staging,
    entries: &[TracebackEntry],
    own: &OwnCode,
) -> anyhow::Result<()> {
    let build = build_identity().map_err(|reason| {
        anyhow::anyhow!(
            "could not name the build of `by` writing `{}`: {reason}",
            staging.out().join(BY_SOURCEMAP_FILENAME).display()
        )
    })?;
    let rendered = render_sourcemap_module(entries, staging.out(), own, build);
    staging.write(Path::new(BY_SOURCEMAP_FILENAME), None, &rendered)
}

/// The text of `_by_sourcemap.py` for these entries, in the tree at `tree`, written by the build
/// of `by` named `build`.
pub(crate) fn render_sourcemap_module(
    entries: &[TracebackEntry],
    tree: &Path,
    own: &OwnCode,
    build: &str,
) -> String {
    let mut map_src = String::from(
        "# generated by basedpython — maps transpiled python frames to .by source\n\
         # the tables share their keys: the generated path, spelled as it is here\n\
         # each entry is (.by path, line table). the line table is one string: the zero-based\n\
         # .by line of each generated line, or `-` for one the transpiler wrote, joined by `,`\n\
         SOURCEMAP = {\n",
    );
    for e in entries {
        map_src.push_str(&sourcemap_line(e));
    }
    map_src.push_str("}\n\n");

    // `SOURCEMAP` alone cannot be checked: a `.by` edited since the transpile
    // leaves it describing a pair of files that no longer exists, and every line
    // it then reports is wrong with total confidence. the digests are what a
    // consumer recomputes from disk before trusting a mapped line. a separate
    // table rather than a wider tuple, so a reader that predates it is unaffected
    map_src.push_str(
        "# sha-256 of the two files each SOURCEMAP entry describes, over the bytes\n\
         # the transpiler read and wrote. recompute both from disk before trusting a\n\
         # mapped line: a mismatch means the file is no longer the one mapped\n\
         DIGESTS = {\n",
    );
    for e in entries {
        map_src.push_str(&digests_line(e));
    }
    map_src.push_str("}\n");

    // a third table keyed as the two are, read only where a generated location has columns
    map_src.push_str(
        "\n# for each generated line, the runs of its byte columns that are text the .by\n\
         # spells, as one string: each line's runs joined by `,` and ended by `;`. a run is\n\
         # `start end by_line by_column`, zero-based, or `start end` for a qualifier written\n\
         # in front of the author's name. every other column is the transpiler's\n\
         COLUMNS = {\n",
    );
    for e in entries {
        map_src.push_str(&columns_line(e));
    }
    map_src.push_str("}\n");

    // after the tables rather than before them: a re-stage finds an entry by the lines
    // above `DIGESTS`, and rewrites the ones that start with its key, which no line below
    // does. a path is spelled as the tables spell theirs
    map_src.push_str(
        "\n# the script a program in this tree is started through, which runs its entry\n\
         # module as `__main__`: none of its frames is the program's. None for a tree no\n\
         # program is started from\n",
    );
    let _ = writeln!(
        map_src,
        "LAUNCHER = {}",
        own.launcher.as_ref().map_or_else(
            || "None".to_owned(),
            |launcher| py_str_literal(&tree.join(launcher).to_string_lossy())
        )
    );

    map_src.push_str(
        "\n# every file of by's own python in this tree, none of which is the program's code:\n\
         # the launcher, the runtime's helpers, this map\n\
         BY_CODE = [\n",
    );
    let mut files: Vec<PathBuf> = std::iter::once(PathBuf::from(BY_SOURCEMAP_FILENAME))
        .chain(own.launcher.iter().cloned())
        .chain(own.files.iter().cloned())
        .collect();
    files.sort();
    files.dedup();
    for file in files {
        let _ = writeln!(
            map_src,
            "    {},",
            py_str_literal(&tree.join(file).to_string_lossy())
        );
    }
    map_src.push_str("]\n");

    // a re-stage moves entries in this map only when the build that wrote it is the one
    // `_by_build.json` names, so a tree holding the map of one build and the record of
    // another is refused rather than edited
    let _ = writeln!(
        map_src,
        "\n# the build of by that wrote this map, as _by_build.json names it\nBY_BUILD = {}",
        py_str_literal(build)
    );

    // `by run` carries what a program wrote into its tree back to the project, and a re-stage
    // writes into that tree while the program runs: this is how the two are told apart
    map_src.push_str(
        "\n# the files a re-stage wrote into this tree after it was built, relative to it, each\n\
         # with the digest of the bytes it wrote: a file at that digest is by's, not the program's.\n\
         # this map is one of them once a re-stage writes it, at the digest of its text with this\n\
         # table empty\n",
    );
    let _ = writeln!(map_src, "{RESTAGED_PREFIX}{{}}");
    map_src
}

/// the start of the map's line listing what a re-stage wrote, a json object on one line, which
/// is a python dict literal too
const RESTAGED_PREFIX: &str = "RESTAGED = ";

/// what an existing map says a re-stage wrote into its tree: each file relative to the tree with
/// `/` separators, and the digest of the bytes it was written with. `None` for a map without
/// the table
pub(crate) fn restaged_files(existing: &str) -> Option<BTreeMap<String, String>> {
    let table = existing
        .lines()
        .find_map(|line| line.strip_prefix(RESTAGED_PREFIX))?;
    serde_json::from_str(table).ok()
}

/// `existing` with its table of what a re-stage wrote holding `files`, and every other byte as
/// it was. `None` for a map without the table
pub(crate) fn with_restaged_files(
    existing: &str,
    files: &BTreeMap<String, String>,
) -> Option<String> {
    let table = serde_json::to_string(files).ok()?;
    let mut found = false;
    let mut out = String::with_capacity(existing.len() + table.len());
    for line in existing.split_inclusive('\n') {
        if !found && line.starts_with(RESTAGED_PREFIX) {
            found = true;
            out.push_str(RESTAGED_PREFIX);
            out.push_str(&table);
            out.push('\n');
        } else {
            out.push_str(line);
        }
    }
    found.then_some(out)
}

/// the digest a map is listed at in its own table of what a re-stage wrote: of its text with
/// that table empty, which a digest of the map can be listed in without changing what it is over
pub(crate) fn map_digest(existing: &str) -> Option<String> {
    with_restaged_files(existing, &BTreeMap::new()).map(|blank| content_digest(blank.as_bytes()))
}

/// One entry's line in the `SOURCEMAP` table, newline included.
fn sourcemap_line(entry: &TracebackEntry) -> String {
    format!(
        "    {}: ({}, \"{}\"),\n",
        py_str_literal(&entry.py_path.to_string_lossy()),
        py_str_literal(&entry.by_path.to_string_lossy()),
        pack_line_map(&entry.line_map),
    )
}

/// One entry's line in the `COLUMNS` table, newline included.
fn columns_line(entry: &TracebackEntry) -> String {
    format!(
        "    {}: \"{}\",\n",
        py_str_literal(&entry.py_path.to_string_lossy()),
        pack_columns(&entry.columns)
    )
}

/// a line table as `SOURCEMAP` packs it: the zero-based `.by` line of each generated line, or
/// `-` for one the transpiler wrote, joined by `,`. every element is written, so a table of no
/// lines is the one packed as the empty string
///
/// nothing it writes needs escaping in a string literal
fn pack_line_map(line_map: &[Option<u32>]) -> String {
    let mut packed = String::new();
    for (index, line) in line_map.iter().enumerate() {
        if index > 0 {
            packed.push(',');
        }
        match line {
            Some(line) => {
                let _ = write!(packed, "{line}");
            }
            None => packed.push('-'),
        }
    }
    packed
}

/// columns as `COLUMNS` packs them: each generated line's runs joined by `,` and ended by `;`,
/// a run `start end by_line by_column`, or `start end` for a qualifier. ended rather than
/// joined, so a line with no runs is still one line: `;;` is two of them
///
/// nothing it writes needs escaping in a string literal
fn pack_columns(columns: &Columns) -> String {
    let mut packed = String::new();
    for line in columns {
        for (index, segment) in line.iter().enumerate() {
            if index > 0 {
                packed.push(',');
            }
            let _ = write!(packed, "{} {}", segment.start, segment.end);
            if let ColumnOrigin::Copied { line, column } = segment.origin {
                let _ = write!(packed, " {line} {column}");
            }
        }
        packed.push(';');
    }
    packed
}

/// the python that unpacks a module's line table and columns from the strings `SOURCEMAP` and
/// `COLUMNS` pack them into, which a reader of the map runs on a module's entry the first time
/// it is asked about that module
///
/// `_unpack_lines` answers a list holding each generated line's zero-based `.by` line or
/// `None`, and `_unpack_columns` a list holding each generated line's runs as tuples
/// `(start, end, by_line, by_column)`, `by_line` and `by_column` `None` for a qualifier
///
/// it reads exactly what `by` writes and refuses the rest with a `ValueError`, as the debugger
/// does: a number with a sign, padding, an underscore, a leading zero or a non-ascii digit,
/// which python's `int` would read, a number past 32 bits, a run that covers no column or
/// starts before the one ahead of it ends, and more lines of columns than the line table has
pub const UNPACK_SRC: &str = r#"import re

_NUMBER = r"(?:0|[1-9][0-9]*)"
_RUN = rf"{_NUMBER} {_NUMBER}(?: {_NUMBER} {_NUMBER})?"
_PACKED_LINES = re.compile(rf"(?:(?:-|{_NUMBER})(?:,(?:-|{_NUMBER}))*)?")
_PACKED_COLUMNS = re.compile(rf"(?:(?:{_RUN}(?:,{_RUN})*)?;)*")


def _packed_number(text):
    number = int(text)
    if number >= 1 << 32:
        raise ValueError(f"{text} is past the 32 bits a sourcemap's numbers fit in")
    return number


def _unpack_lines(packed):
    if _PACKED_LINES.fullmatch(packed) is None:
        raise ValueError(f"not a line table `by` packs: {packed!r}")
    if not packed:
        return []
    return [None if line == "-" else _packed_number(line) for line in packed.split(",")]


def _unpack_columns(packed, line_count):
    if _PACKED_COLUMNS.fullmatch(packed) is None:
        raise ValueError(f"not columns `by` packs: {packed!r}")
    unpacked = []
    for line in packed.split(";")[:-1]:
        runs = []
        previous_end = 0
        for run in line.split(",") if line else ():
            start, end, *origin = map(_packed_number, run.split(" "))
            if start >= end or start < previous_end:
                raise ValueError(f"a run `{run}` out of order or covering nothing: {packed!r}")
            previous_end = end
            by_line, by_column = origin or (None, None)
            runs.append((start, end, by_line, by_column))
        unpacked.append(runs)
    if len(unpacked) > line_count:
        raise ValueError(
            f"columns of {len(unpacked)} lines, where the line table has {line_count}: {packed!r}"
        )
    return unpacked
"#;

/// One entry's line in the `DIGESTS` table, newline included.
fn digests_line(entry: &TracebackEntry) -> String {
    let mut line = String::new();
    let _ = writeln!(
        line,
        "    {}: {{\"by\": {}, \"py\": {}}},",
        py_str_literal(&entry.py_path.to_string_lossy()),
        py_str_literal(&entry.by_digest),
        py_str_literal(&entry.py_digest),
    );
    line
}

/// The key an existing map holds for the module that lands at `relative`.
///
/// The map is keyed by the *absolute* generated path, spelled the way the build
/// that wrote it spelled the output directory — and a caller re-staging into that
/// tree may well spell the same directory differently. A temp directory is
/// `/var/folders/…` to the process that created it and `/private/var/folders/…`
/// once anything canonicalizes it; both name the same file and neither is wrong.
///
/// So the entry is found by the one thing that cannot vary — where the module
/// sits inside the tree — and the key that was found is then reused verbatim, so
/// the rewritten map differs from the old one in exactly the two lines it had to.
///
/// `None` when no entry lands there, which means this file is not part of the
/// build the map describes.
pub(crate) fn sourcemap_key_for(existing: &str, relative: &Path) -> Option<PathBuf> {
    existing
        .lines()
        .take_while(|line| !line.starts_with("DIGESTS = {"))
        .filter_map(entry_key)
        .find(|key| key.ends_with(relative))
}

/// the build of `by` an existing map says wrote it, or `None` for a map that names none
pub(crate) fn sourcemap_build(existing: &str) -> Option<String> {
    existing
        .lines()
        .find_map(|line| line.strip_prefix("BY_BUILD = \""))
        .and_then(read_str_literal)
}

/// The key of a table line, or `None` for anything that is not one.
fn entry_key(line: &str) -> Option<PathBuf> {
    read_str_literal(line.strip_prefix("    \"")?).map(PathBuf::from)
}

/// a string literal as [`py_str_literal`] writes it, from just after its opening quote
fn read_str_literal(rest: &str) -> Option<String> {
    let mut key = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(key),
            // the escapes [`py_str_literal`] writes, and only those: a literal
            // holding anything else was not written here, and guessing at it
            // would invent a path
            '\\' => match chars.next()? {
                '\\' => key.push('\\'),
                '"' => key.push('"'),
                'n' => key.push('\n'),
                'r' => key.push('\r'),
                _ => return None,
            },
            _ => key.push(ch),
        }
    }
    None
}

/// Why one entry could not be replaced in an existing `_by_sourcemap.py`.
#[derive(Debug, PartialEq, Eq)]
pub enum RewriteRefusal {
    /// the generated path has no entry in one or both tables, so this file is not
    /// part of the build the map describes
    NotInTheMap,
}

/// Replace one entry in an existing `_by_sourcemap.py`, leaving every other byte
/// of the file exactly as it was found.
///
/// A substitution rather than a re-render of the whole map, and that is the whole
/// design. The other entries describe modules the debugger has already accepted
/// and is holding line tables for; a re-render that changed so much as the order
/// of a key would invalidate them all, and would do it silently. Substituting the
/// one line that has to change makes "every other entry is untouched" a property
/// of the operation rather than a hope about the renderer.
///
/// The entry is matched on its key — the generated path, spelled exactly as the
/// build spelled it — in each of the three tables. A key missing from any of them
/// is [`RewriteRefusal::NotInTheMap`] just as a key missing from all is: a map
/// with a line table and no digests beside it describes a pair of files nothing
/// can verify, and one without its columns would leave a module's code to be
/// compiled against columns of the module it replaced.
pub(crate) fn rewrite_sourcemap_entry(
    existing: &str,
    entry: &TracebackEntry,
) -> Result<String, RewriteRefusal> {
    let key = py_str_literal(&entry.py_path.to_string_lossy());
    let prefix = format!("    {key}: ");

    let mut replaced = [false; 3];
    let mut out = String::with_capacity(existing.len());
    // the tables are told apart by which header was last seen, because the key
    // is the same in each and a line matched without knowing its table would
    // write a line table where the digests belong
    let mut table = None;
    for line in existing.split_inclusive('\n') {
        if let Some(header) = ["SOURCEMAP = {", "DIGESTS = {", "COLUMNS = {"]
            .iter()
            .position(|header| line.starts_with(header))
        {
            table = Some(header);
        } else if !line.starts_with("    ") {
            table = None;
        }
        if let Some(index) = table
            && line.starts_with(&prefix)
        {
            out.push_str(&match index {
                0 => sourcemap_line(entry),
                1 => digests_line(entry),
                _ => columns_line(entry),
            });
            replaced[index] = true;
            continue;
        }
        out.push_str(line);
    }

    if replaced.iter().all(|replaced| *replaced) {
        Ok(out)
    } else {
        Err(RewriteRefusal::NotInTheMap)
    }
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

#[cfg(test)]
mod tests {
    use super::{
        BY_SOURCEMAP_FILENAME, OwnCode, RewriteRefusal, TracebackEntry, UNPACK_SRC, content_digest,
        describe_module, pack_columns, pack_line_map, py_str_literal, render_sourcemap_module,
        rewrite_sourcemap_entry, write_sourcemap_module,
    };
    use crate::emit::Transpiled;
    use by_transforms::columns::{ColumnOrigin, Columns, Segment};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// `relative` in the tree at `/build`, as the map spells a path of the tree: joined with the
    /// host's separator, which is `\` on windows
    fn in_tree(relative: &Path) -> String {
        py_str_literal(&Path::new("/build").join(relative).to_string_lossy())
    }

    fn entry(py: &str, by: &str, source: &str, python: &str) -> TracebackEntry {
        TracebackEntry {
            py_path: PathBuf::from(py),
            by_path: PathBuf::from(by),
            line_map: vec![None, Some(0)],
            columns: vec![Vec::new(), Vec::new()],
            by_source: source.to_owned(),
            by_digest: content_digest(source.as_bytes()),
            py_digest: content_digest(python.as_bytes()),
        }
    }

    /// the digest carries the algorithm that produced it, so a reader that does
    /// not know one can refuse the entry rather than compare hex it could never
    /// have produced
    #[test]
    fn a_digest_names_the_algorithm_before_the_hex() {
        assert_eq!(
            content_digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// a traceback, and a debugger matching a breakpoint, see the `.by` under the spelling a
    /// user types. on windows `std::fs::canonicalize` answers with the verbatim `\\?\C:\…`
    /// form, which is what this hands in there
    #[test]
    fn a_described_module_names_its_source_without_the_verbatim_prefix() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let source = dir.path().join("demo.by");
        std::fs::write(&source, "x = 1\n")?;
        let resolved = std::fs::canonicalize(&source)?;

        let described = describe_module(&Transpiled {
            by_path: &resolved,
            by_source: "x = 1\n",
            python: "x = 1\n",
            line_map: &[Some(0)],
            columns: &[Vec::new()],
        });

        assert_eq!(described.by_path, dunce::simplified(&resolved));
        assert!(
            !described.by_path.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            described.by_path.display()
        );
        Ok(())
    }

    /// `DIGESTS` is a second table keyed exactly as `SOURCEMAP` is — additive,
    /// so a consumer that only knows the tuple reads the same file unchanged
    #[test]
    fn the_sourcemap_module_digests_both_files_of_every_entry() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        let entries = vec![entry(
            "/build/demo.py",
            "/src/demo.by",
            "the .by source",
            "the generated python",
        )];

        let mut staging = crate::staging::Staging::new(dir.path());
        write_sourcemap_module(&mut staging, &entries, &OwnCode::default())?;
        let emitted = std::fs::read_to_string(dir.path().join(BY_SOURCEMAP_FILENAME))?;

        assert!(
            emitted.contains(r#"    "/build/demo.py": ("/src/demo.by", "-,0"),"#),
            "the line table is packed beside the `.by` path:\n{emitted}"
        );
        assert!(
            emitted.contains(&format!(
                r#"    "/build/demo.py": {{"by": "{}", "py": "{}"}},"#,
                content_digest(b"the .by source"),
                content_digest(b"the generated python"),
            )),
            "both digests are keyed by the generated path:\n{emitted}"
        );
        Ok(())
    }

    /// the property the debugger rests on: one entry moves, and the rest of the
    /// file is the same bytes it was
    #[test]
    fn rewriting_one_entry_leaves_every_other_byte_alone() {
        let others = [
            entry("/build/a.py", "/src/a.by", "a", "A"),
            entry("/build/b.py", "/src/b.by", "b", "B"),
            entry("/build/c.py", "/src/c.by", "c", "C"),
        ];
        let existing =
            render_sourcemap_module(&others, Path::new("/build"), &OwnCode::default(), "a-build");

        let mut edited = entry("/build/b.py", "/src/b.by", "b edited", "B EDITED");
        edited.line_map = vec![Some(3), Some(4), None];
        edited.columns = vec![
            vec![
                Segment {
                    start: 0,
                    end: 9,
                    origin: ColumnOrigin::Qualifier,
                },
                Segment {
                    start: 9,
                    end: 14,
                    origin: ColumnOrigin::Copied { line: 3, column: 4 },
                },
            ],
            Vec::new(),
            Vec::new(),
        ];
        let rewritten = rewrite_sourcemap_entry(&existing, &edited).expect("b is in the map");

        // what the whole-file renderer would have produced for the same set, so
        // the substitution and the render cannot drift apart
        let expected = render_sourcemap_module(
            &[
                entry("/build/a.py", "/src/a.by", "a", "A"),
                edited,
                entry("/build/c.py", "/src/c.by", "c", "C"),
            ],
            Path::new("/build"),
            &OwnCode::default(),
            "a-build",
        );
        assert_eq!(rewritten, expected);

        // and, said the other way, so a failure names which half moved
        for untouched in [
            r#"    "/build/a.py": ("/src/a.by", "-,0"),"#,
            r#"    "/build/c.py": ("/src/c.by", "-,0"),"#,
        ] {
            assert!(rewritten.contains(untouched), "{rewritten}");
        }
        assert!(
            rewritten.contains(r#"    "/build/b.py": ("/src/b.by", "3,4,-"),"#),
            "{rewritten}"
        );
        assert!(
            rewritten.contains(r#"    "/build/b.py": "0 9,9 14 3 4;;;","#),
            "the columns move with the entry:\n{rewritten}"
        );
        assert!(
            rewritten.contains(&format!(
                r#"    "/build/b.py": {{"by": "{}", "py": "{}"}},"#,
                content_digest(b"b edited"),
                content_digest(b"B EDITED"),
            )),
            "{rewritten}"
        );
    }

    /// the launcher is found where the map is, spelled as the tables spell their paths, and a
    /// re-stage, which rewrites one entry's lines, leaves it as it was
    #[test]
    fn the_launcher_is_declared_after_the_tables() {
        let own = OwnCode {
            launcher: Some(PathBuf::from("_by_runner.py")),
            files: Vec::new(),
        };
        let existing = render_sourcemap_module(
            &[entry("/build/a.py", "/src/a.by", "a", "A")],
            Path::new("/build"),
            &own,
            "a-build",
        );
        let launcher = format!("\nLAUNCHER = {}\n", in_tree(Path::new("_by_runner.py")));
        assert!(existing.contains(&launcher), "{existing}");
        assert_eq!(
            super::sourcemap_key_for(&existing, Path::new("_by_runner.py")),
            None,
            "the launcher is no entry of the tables"
        );
        let rewritten =
            rewrite_sourcemap_entry(&existing, &entry("/build/a.py", "/src/a.by", "a2", "A2"))
                .expect("a is in the map");
        assert!(rewritten.contains(&launcher), "{rewritten}");
    }

    /// by's own files are listed whole, the launcher and the map among them, and a re-stage
    /// leaves the list as it was: none of its lines is an entry's
    #[test]
    fn by_s_own_files_are_declared_after_the_tables() {
        let runtime = Path::new("pkg").join("_by_runtime.py");
        let own = OwnCode {
            launcher: Some(PathBuf::from("_by_runner.py")),
            files: vec![runtime.clone(), PathBuf::from("_by_repl.py")],
        };
        let existing = render_sourcemap_module(
            &[entry("/build/a.py", "/src/a.by", "a", "A")],
            Path::new("/build"),
            &own,
            "a-build",
        );
        let listed = format!(
            "BY_CODE = [\n    {},\n    {},\n    {},\n    {},\n]\n",
            in_tree(Path::new("_by_repl.py")),
            in_tree(Path::new("_by_runner.py")),
            in_tree(Path::new("_by_sourcemap.py")),
            in_tree(&runtime),
        );
        assert!(existing.contains(&listed), "{existing}");
        for file in [
            Path::new("_by_repl.py"),
            Path::new("_by_runner.py"),
            &runtime,
        ] {
            assert_eq!(
                super::sourcemap_key_for(&existing, file),
                None,
                "{} is no entry of the tables",
                file.display()
            );
        }
        let rewritten =
            rewrite_sourcemap_entry(&existing, &entry("/build/a.py", "/src/a.by", "a2", "A2"))
                .expect("a is in the map");
        assert!(rewritten.contains(&listed), "{rewritten}");
    }

    /// the map names the build that wrote it, below every table, so a re-stage that moves an
    /// entry leaves the name as it was
    #[test]
    fn the_map_names_the_build_that_wrote_it() {
        let existing = render_sourcemap_module(
            &[entry("/build/a.py", "/src/a.by", "a", "A")],
            Path::new("/build"),
            &OwnCode::default(),
            "a-build",
        );
        assert_eq!(
            super::sourcemap_build(&existing).as_deref(),
            Some("a-build"),
            "{existing}"
        );
        let rewritten =
            rewrite_sourcemap_entry(&existing, &entry("/build/a.py", "/src/a.by", "a2", "A2"))
                .expect("a is in the map");
        assert_eq!(
            super::sourcemap_build(&rewritten),
            super::sourcemap_build(&existing),
            "{rewritten}"
        );
    }

    /// a tree no program is started from says so, rather than leaving a reader to tell a map
    /// without a launcher from one written before launchers were declared
    #[test]
    fn a_tree_nothing_is_started_from_has_no_launcher() {
        let existing = render_sourcemap_module(
            &[entry("/build/a.py", "/src/a.by", "a", "A")],
            Path::new("/build"),
            &OwnCode::default(),
            "a-build",
        );
        assert!(existing.contains("\nLAUNCHER = None\n"), "{existing}");
    }

    /// a file the map has never heard of is not part of this build, and saying so
    /// is the only safe answer — appending an entry would claim a module the
    /// build never wrote
    #[test]
    fn an_entry_the_map_does_not_hold_is_refused() {
        let existing = render_sourcemap_module(
            &[entry("/build/a.py", "/src/a.by", "a", "A")],
            Path::new("/build"),
            &OwnCode::default(),
            "a-build",
        );
        let stranger = entry("/build/z.py", "/src/z.by", "z", "Z");
        assert_eq!(
            rewrite_sourcemap_entry(&existing, &stranger),
            Err(RewriteRefusal::NotInTheMap)
        );
    }

    /// the tree's own spelling of the output directory is the one the map keeps,
    /// whatever the caller re-staging into it calls that directory
    #[test]
    fn an_entry_is_found_by_where_it_sits_in_the_tree() {
        let existing = render_sourcemap_module(
            &[
                entry("/var/folders/t/build/pkg/a.py", "/src/pkg/a.by", "a", "A"),
                entry("/var/folders/t/build/b.py", "/src/b.by", "b", "B"),
            ],
            Path::new("/build"),
            &OwnCode::default(),
            "a-build",
        );

        assert_eq!(
            super::sourcemap_key_for(&existing, Path::new("pkg/a.py")),
            Some(PathBuf::from("/var/folders/t/build/pkg/a.py"))
        );
        assert_eq!(
            super::sourcemap_key_for(&existing, Path::new("nothing/here.py")),
            None
        );
        // a partial component is not a match: `a.py` alone must not answer for
        // `pkg/a.py`, and nor may `kg/a.py`
        assert_eq!(
            super::sourcemap_key_for(&existing, Path::new("kg/a.py")),
            None
        );
    }

    /// a line table with no digests beside it describes a pair of files nothing
    /// can verify, so half a match is no match
    #[test]
    fn an_entry_present_in_only_one_table_is_refused() {
        let existing = render_sourcemap_module(
            &[entry("/build/a.py", "/src/a.by", "a", "A")],
            Path::new("/build"),
            &OwnCode::default(),
            "a-build",
        );
        let half: String = existing
            .split_inclusive('\n')
            .filter(|line| !line.contains(r#""by":"#))
            .collect();
        let a = entry("/build/a.py", "/src/a.by", "a2", "A2");
        assert_eq!(
            rewrite_sourcemap_entry(&half, &a),
            Err(RewriteRefusal::NotInTheMap)
        );
    }

    fn copied(start: u32, end: u32, line: u32, column: u32) -> Segment {
        Segment {
            start,
            end,
            origin: ColumnOrigin::Copied { line, column },
        }
    }

    fn qualifier(start: u32, end: u32) -> Segment {
        Segment {
            start,
            end,
            origin: ColumnOrigin::Qualifier,
        }
    }

    /// what [`UNPACK_SRC`] prints for `script`, given `input` as json in `sys.argv[1]`, run by
    /// the interpreter `PYTHON` names, or `python3`
    fn run_unpacking(script: &str, input: &serde_json::Value) -> serde_json::Value {
        let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
        let output = Command::new(&python)
            .args([
                "-I",
                "-c",
                &format!("{UNPACK_SRC}\nimport json, sys\n{script}"),
            ])
            .arg(input.to_string())
            .output()
            .unwrap_or_else(|error| panic!("could not start `{python}`: {error}"));
        assert!(
            output.status.success(),
            "the unpacking failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("the unpacking prints json")
    }

    /// what [`UNPACK_SRC`] answers for each packed line table, and for each packed set of
    /// columns with the number of lines its line table has
    fn unpacked_by_python(lines: &[String], columns: &[(String, usize)]) -> serde_json::Value {
        run_unpacking(
            "lines, columns = json.loads(sys.argv[1])\n\
             print(json.dumps([[_unpack_lines(packed) for packed in lines], \
             [_unpack_columns(packed, count) for packed, count in columns]]))\n",
            &serde_json::json!([lines, columns]),
        )
    }

    /// columns as [`UNPACK_SRC`] answers them, in json: a qualifier's `.by` line and column are
    /// `None`
    fn columns_as_json(columns: &Columns) -> serde_json::Value {
        columns
            .iter()
            .map(|line| {
                line.iter()
                    .map(|segment| match segment.origin {
                        ColumnOrigin::Copied { line, column } => {
                            serde_json::json!([segment.start, segment.end, line, column])
                        }
                        ColumnOrigin::Qualifier => {
                            serde_json::json!([segment.start, segment.end, null, null])
                        }
                    })
                    .collect::<serde_json::Value>()
            })
            .collect()
    }

    /// what a module's entry says is what the runner reads back out of it, for every shape a
    /// table can take: no lines at all, lines the transpiler wrote, lines with no runs at the
    /// end, qualifiers beside copied text, and numbers as large as a table holds
    #[test]
    fn a_packed_entry_unpacks_to_the_tables_it_was_packed_from() {
        let line_maps: Vec<Vec<Option<u32>>> = vec![
            Vec::new(),
            vec![None],
            vec![Some(0)],
            vec![None, None, Some(0), Some(1), None, Some(1), Some(u32::MAX)],
        ];
        let columns: Vec<Columns> = vec![
            Vec::new(),
            vec![Vec::new()],
            vec![Vec::new(), Vec::new(), Vec::new()],
            vec![vec![copied(0, 13, 0, 0)]],
            vec![
                Vec::new(),
                vec![
                    copied(0, 12, 7, 0),
                    qualifier(12, 21),
                    copied(21, 32, 7, 12),
                ],
                vec![qualifier(0, 9)],
                vec![copied(4, u32::MAX, u32::MAX, u32::MAX)],
                Vec::new(),
            ],
        ];

        let packed_lines: Vec<String> = line_maps.iter().map(|map| pack_line_map(map)).collect();
        let packed_columns: Vec<(String, usize)> = columns
            .iter()
            .map(|columns| (pack_columns(columns), columns.len()))
            .collect();
        let unpacked = unpacked_by_python(&packed_lines, &packed_columns);

        assert_eq!(
            unpacked[0],
            serde_json::json!(line_maps),
            "{packed_lines:?}"
        );
        let expected: Vec<serde_json::Value> = columns.iter().map(columns_as_json).collect();
        assert_eq!(
            unpacked[1],
            serde_json::json!(expected),
            "{packed_columns:?}"
        );
    }

    /// the packing is pinned as well as round tripped, since the debugger reads it with a parser
    /// of its own
    #[test]
    fn an_entry_is_packed_as_the_map_s_comments_describe() {
        assert_eq!(pack_line_map(&[]), "");
        assert_eq!(pack_line_map(&[None]), "-");
        assert_eq!(pack_line_map(&[None, Some(0), Some(12)]), "-,0,12");
        assert_eq!(pack_columns(&Vec::new()), "");
        assert_eq!(pack_columns(&vec![Vec::new()]), ";");
        assert_eq!(
            pack_columns(&vec![
                Vec::new(),
                vec![copied(0, 4, 1, 0), qualifier(4, 13), copied(13, 20, 1, 4)],
            ]),
            ";0 4 1 0,4 13,13 20 1 4;"
        );
    }

    /// a string `by` could not have written is refused by the reader rather than read as some
    /// other table, in either table: python's `int` reads a sign, padding, an underscore and a
    /// non-ascii digit, and the reader has to read only what the debugger reads
    #[test]
    fn a_string_the_packing_cannot_write_is_refused_by_the_unpacking() {
        let lines = [
            " 7 ",
            "007",
            "-1",
            "+1",
            "1_0",
            "٤",
            "0,,1",
            "0, 1",
            "None",
            "4294967296",
            ",",
            "0,",
            "--",
        ];
        // each against a line table of three lines
        let columns = [
            "0 4 1 0",
            "0 4 1;",
            "0 4 1 0 2;",
            "0 x;",
            "0 4 -1 0;",
            "0 4 +1 0;",
            "0 4_0;",
            "4 2;",
            "0 0;",
            "0 ٤;",
            "00 4;",
            "0  4;",
            " 0 4;",
            "4 8 0 4,0 4 0 0;",
            "0 4,;",
            "0 4294967296;",
            ";;;;",
        ];
        let accepted = run_unpacking(
            "lines, columns = json.loads(sys.argv[1])\n\
             accepted = []\n\
             for packed in lines:\n    \
                 try:\n        _unpack_lines(packed)\n    except ValueError:\n        continue\n    \
                 accepted.append(packed)\n\
             for packed in columns:\n    \
                 try:\n        _unpack_columns(packed, 3)\n    except ValueError:\n        continue\n    \
                 accepted.append(packed)\n\
             print(json.dumps(accepted))\n",
            &serde_json::json!([lines, columns]),
        );
        assert_eq!(accepted, serde_json::json!([]));
    }
}
