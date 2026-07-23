//! `type _X = …` → `private type X = …`
//!
//! typeshed spells a module-internal alias with a leading underscore.
//! basedpython has a keyword for that, and it reads better: the underscore is
//! applied by the lowering rather than written by hand, and importing the alias
//! from another module becomes a `private-import` error instead of merely a
//! convention someone can ignore.
//!
//! because `private type X` *binds* `X`, converting an alias renames every
//! reference to it. that is only safe when every reference lives in the
//! declaring file, so the patch takes a whole-tree scan and converts an alias
//! only when
//!
//! - every other stub mentioning the identifier `_X` declares its own `type _X`
//!   (so the mention resolves locally there and is not an import of this one),
//!   and
//! - the stripped name `X` occurs nowhere in the declaring stub (so the rename
//!   cannot capture an unrelated symbol)
//!
//! aliases imported across modules are therefore left alone: rewriting them
//! would need a coordinated edit in the importing stub, which the per-file
//! [`Patch`] contract cannot express.
//!
//! the rewrite is a whole-file identifier rename plus a `private ` insertion,
//! and an already-converted alias is no longer spelled `_X`, so re-running is a
//! no-op

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ruff_python_ast::{ModModule, PySourceType, Stmt};
use ruff_python_parser::{Parsed, parse_unchecked_source};
use ruff_text_size::Ranged;
use walkdir::WalkDir;

use crate::{Edit, Patch};

pub struct PrivateTypeAliases {
    /// stub path (relative to the stdlib root) → the alias names in it that are
    /// safe to convert
    convertible: BTreeMap<PathBuf, BTreeSet<String>>,
}

impl PrivateTypeAliases {
    /// scan the whole stub tree to decide which aliases are referenced only by
    /// their own module
    pub fn scan(root: &Path) -> Self {
        let mut sources: Vec<(PathBuf, String)> = Vec::new();
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "byi") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(path) else {
                continue;
            };
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            sources.push((rel, source));
        }

        // identifier → the stubs mentioning it, and stub → the aliases it declares
        let mut mentions: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
        let mut declared: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
        for (rel, source) in &sources {
            for name in identifiers(source) {
                mentions.entry(name).or_default().insert(rel.clone());
            }
            let parsed = parse_unchecked_source(source, PySourceType::BasedPythonStub);
            declared.insert(rel.clone(), private_alias_names(&parsed));
        }

        let mut convertible: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
        for (rel, source) in &sources {
            let idents = identifiers(source);
            let mut safe = BTreeSet::new();
            for name in declared.get(rel).into_iter().flatten() {
                // a mention in a stub that declares its own alias of the same
                // name resolves there, so it is not a reference to this one
                let leaks = mentions.get(name).is_some_and(|files| {
                    files.iter().any(|other| {
                        other != rel
                            && !declared
                                .get(other)
                                .is_some_and(|names| names.contains(name))
                    })
                });
                // the rename must not collide with a name the stub already uses
                if leaks || idents.contains(&name[1..]) {
                    continue;
                }
                safe.insert(name.clone());
            }
            if !safe.is_empty() {
                convertible.insert(rel.clone(), safe);
            }
        }

        Self { convertible }
    }
}

/// every identifier-shaped token in `source`, including ones inside strings and
/// comments — a rename has to consider forward references and `__all__` entries
fn identifiers(source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        let is_start = b.is_ascii_alphabetic() || b == b'_';
        let is_continue = is_start || b.is_ascii_digit();
        match (start, is_continue) {
            (None, _) if is_start => start = Some(i),
            (Some(s), false) => {
                out.insert(source[s..i].to_string());
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.insert(source[s..].to_string());
    }
    out
}

/// module-level `type _X = …` alias names that are not already `private`
fn private_alias_names(parsed: &Parsed<ModModule>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for stmt in &parsed.syntax().body {
        if let Stmt::TypeAlias(alias) = stmt
            && !alias.is_private
            && let ruff_python_ast::Expr::Name(name) = alias.name.as_ref()
            && name.id.starts_with('_')
            && name.id.len() > 1
            && !name.id.starts_with("__")
        {
            names.insert(name.id.to_string());
        }
    }
    names
}

impl Patch for PrivateTypeAliases {
    fn name(&self) -> &'static str {
        "private-type-aliases"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        // the patch is keyed on shape, not on named symbols; drift in any one
        // module is handled by the whole-tree scan rather than a fixed list
        &[]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let Some(convertible) = self.convertible.get(module_path) else {
            return Vec::new();
        };

        let mut converted: BTreeSet<&str> = BTreeSet::new();
        let mut edits = Vec::new();
        for stmt in &parsed.syntax().body {
            if let Stmt::TypeAlias(alias) = stmt
                && !alias.is_private
                && let ruff_python_ast::Expr::Name(name) = alias.name.as_ref()
                && convertible.contains(name.id.as_str())
            {
                converted.insert(name.id.as_str());
                let start = usize::from(alias.range().start());
                edits.push(Edit {
                    start,
                    end: start,
                    replacement: "private ".to_string(),
                });
            }
        }
        if converted.is_empty() {
            return edits;
        }

        // rename every occurrence, wherever it sits — a string annotation and a
        // doc comment are as much a reference as an expression
        for (start, ident) in identifier_spans(source) {
            if converted.contains(ident) {
                edits.push(Edit {
                    start,
                    end: start + 1,
                    replacement: String::new(),
                });
            }
        }
        edits
    }
}

/// `(offset, text)` for every identifier-shaped token in `source`
fn identifier_spans(source: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        let is_start = b.is_ascii_alphabetic() || b == b'_';
        let is_continue = is_start || b.is_ascii_digit();
        match (start, is_continue) {
            (None, _) if is_start => start = Some(i),
            (Some(s), false) => {
                out.push((s, &source[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &source[s..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_edits;

    fn patch_for(file: &str, names: &[&str]) -> PrivateTypeAliases {
        let mut convertible = BTreeMap::new();
        convertible.insert(
            PathBuf::from(file),
            names.iter().map(|n| (*n).to_string()).collect(),
        );
        PrivateTypeAliases { convertible }
    }

    fn run(patch: &PrivateTypeAliases, file: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = patch.rewrite(Path::new(file), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_and_renames_references() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "\
type _Key = str | int

def get(k: _Key) -> int: ...
def put(k: _Key, v: int) -> None: ...
";
        assert_eq!(
            run(&patch, "m.byi", src),
            "\
private type Key = str | int

def get(k: Key) -> int: ...
def put(k: Key, v: int) -> None: ...
"
        );
    }

    #[test]
    fn idempotent() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n\ndef get(k: _Key) -> int: ...\n";
        let once = run(&patch, "m.byi", src);
        assert_eq!(run(&patch, "m.byi", &once), once);
    }

    #[test]
    fn renames_inside_string_annotations() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n\ndef get(k: \"_Key\") -> int: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Key = str\n\ndef get(k: \"Key\") -> int: ...\n"
        );
    }

    #[test]
    fn leaves_unlisted_aliases_alone() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\ntype _Other = int\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Key = str\ntype _Other = int\n"
        );
    }

    #[test]
    fn ignores_files_with_nothing_to_convert() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n";
        assert_eq!(run(&patch, "other.byi", src), src);
    }

    #[test]
    fn does_not_touch_similarly_named_identifiers() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n\ndef get(k: _Key, j: _KeyPair, i: __Key) -> int: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Key = str\n\ndef get(k: Key, j: _KeyPair, i: __Key) -> int: ...\n"
        );
    }

    #[test]
    fn generic_alias() {
        let patch = patch_for("m.byi", &["_Pair"]);
        let src = "type _Pair[T] = tuple[T, T]\n\ndef f(p: _Pair[int]) -> None: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Pair[T] = tuple[T, T]\n\ndef f(p: Pair[int]) -> None: ...\n"
        );
    }

    #[test]
    fn scan_skips_aliases_referenced_by_another_stub() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("a.byi"),
            "type _Shared = str\ntype _Local = int\n",
        )
        .expect("write a");
        std::fs::write(dir.path().join("b.byi"), "from a import _Shared\n").expect("write b");

        let patch = PrivateTypeAliases::scan(dir.path());
        let safe = patch
            .convertible
            .get(Path::new("a.byi"))
            .expect("a.byi has convertible aliases");
        assert!(safe.contains("_Local"));
        assert!(!safe.contains("_Shared"));
    }

    #[test]
    fn scan_allows_the_same_alias_name_declared_in_two_stubs() {
        // each stub binds its own `_Address`, so neither mention is a reference
        // to the other and both convert independently
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("a.byi"), "type _Address = str\n").expect("write a");
        std::fs::write(dir.path().join("b.byi"), "type _Address = int\n").expect("write b");

        let patch = PrivateTypeAliases::scan(dir.path());
        for file in ["a.byi", "b.byi"] {
            assert!(
                patch
                    .convertible
                    .get(Path::new(file))
                    .is_some_and(|names| names.contains("_Address")),
                "{file} should convert"
            );
        }
    }

    #[test]
    fn scan_skips_aliases_whose_stripped_name_is_taken() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("a.byi"),
            "type _Socket = int\n\nclass Socket: ...\n",
        )
        .expect("write a");

        let patch = PrivateTypeAliases::scan(dir.path());
        assert!(!patch.convertible.contains_key(Path::new("a.byi")));
    }
}
