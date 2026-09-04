//! whole-tree scan shared by the `_X` → `private X` conversions
//!
//! typeshed spells a module-internal declaration with a leading underscore.
//! basedpython has a keyword for that, and it reads better: the underscore is
//! applied by the lowering rather than written by hand, and importing the name
//! from another module becomes a `private-import` error instead of merely a
//! convention someone can ignore.
//!
//! because `private X` *binds* `X`, converting a declaration renames every
//! reference to it. that is only safe when every reference lives in the
//! declaring file, which the per-file [`Patch`](crate::Patch) contract cannot
//! see, so the decision is taken here, once, over the whole tree. a declaration
//! converts only when
//!
//! - every other stub mentioning the identifier `_X` declares its own `_X` (so
//!   the mention resolves locally there and is not an import of this one), and
//! - the stripped name `X` occurs nowhere in the declaring stub (so the rename
//!   cannot capture an unrelated symbol)
//!
//! declarations imported across modules are therefore left alone: rewriting one
//! would need a coordinated edit in the importing stub, which a per-file patch
//! cannot express

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ruff_python_ast::{Decorator, Expr, ModModule, PySourceType, Stmt, StmtClassDef};
use ruff_python_parser::{Parsed, parse_unchecked_source};
use ruff_text_size::Ranged;
use walkdir::WalkDir;

use crate::Edit;

/// stub path (relative to the stdlib root) → the names in it that are safe to
/// convert
pub(crate) type Convertible = BTreeMap<PathBuf, BTreeSet<String>>;

/// the convertible declarations of each kind, keyed by stub
pub(crate) struct PrivateNames {
    pub(crate) aliases: Convertible,
    pub(crate) protocols: Convertible,
}

/// scan the whole stub tree to decide which underscore-prefixed declarations
/// are referenced only by their own module
pub(crate) fn scan(root: &Path) -> PrivateNames {
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

    // identifier → the stubs mentioning it, and stub → what it declares. the
    // leak check only asks whether a mention binds locally, so `declared` pools
    // both kinds while `candidates` keeps them apart for the two patches
    let mut mentions: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut declared: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut candidates: BTreeMap<PathBuf, [BTreeSet<String>; 2]> = BTreeMap::new();
    for (rel, source) in &sources {
        for name in identifiers(source) {
            mentions.entry(name).or_default().insert(rel.clone());
        }
        let parsed = parse_unchecked_source(source, PySourceType::BasedPythonStub);
        let kinds = [alias_names(&parsed), protocol_names(&parsed, source)];
        declared.insert(rel.clone(), kinds.iter().flatten().cloned().collect());
        candidates.insert(rel.clone(), kinds);
    }

    let mut out = PrivateNames {
        aliases: Convertible::new(),
        protocols: Convertible::new(),
    };
    for (rel, source) in &sources {
        let idents = identifiers(source);
        let Some(kinds) = candidates.get(rel) else {
            continue;
        };
        for (names, dest) in kinds.iter().zip([&mut out.aliases, &mut out.protocols]) {
            let safe: BTreeSet<String> = names
                .iter()
                .filter(|name| {
                    // a mention in a stub that declares its own name of the
                    // same spelling resolves there, so it is not a reference to
                    // this one
                    let leaks = mentions.get(*name).is_some_and(|files| {
                        files.iter().any(|other| {
                            other != rel
                                && !declared
                                    .get(other)
                                    .is_some_and(|names| names.contains(*name))
                        })
                    });
                    // the rename must not collide with a name the stub already uses
                    !leaks && !idents.contains(&name[1..])
                })
                .cloned()
                .collect();
            if !safe.is_empty() {
                dest.insert(rel.clone(), safe);
            }
        }
    }

    out
}

/// an identifier typeshed hides by convention: exactly one leading underscore,
/// and something after it
fn is_underscore_private(name: &str) -> bool {
    name.len() > 1 && name.starts_with('_') && !name.starts_with("__")
}

/// module-level `type _X = …` alias names that are not already `private`
fn alias_names(parsed: &Parsed<ModModule>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for stmt in &parsed.syntax().body {
        if let Stmt::TypeAlias(alias) = stmt
            && !alias.is_private
            && let Expr::Name(name) = alias.name.as_ref()
            && is_underscore_private(&name.id)
        {
            names.insert(name.id.to_string());
        }
    }
    names
}

/// `protocol _X` declaration names that are not already `private`
fn protocol_names(parsed: &Parsed<ModModule>, source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    each_private_protocol(&parsed.syntax().body, source, &mut |class, _| {
        names.insert(class.name.to_string());
    });
    names
}

/// visit every `protocol _X` declaration that is not already `private`, along
/// with the offset of its `protocol` keyword.
///
/// a class body is not entered: `private` on a nested class name-mangles rather
/// than renames, so only module-level protocols are candidates. a version guard
/// or a `try` block is entered, since a protocol declared there still binds at
/// module level
pub(crate) fn each_private_protocol<'a>(
    body: &'a [Stmt],
    source: &str,
    f: &mut impl FnMut(&'a StmtClassDef, usize),
) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class) => {
                if is_underscore_private(&class.name)
                    && !has_marker(class, source, "private")
                    && let Some(marker) = marker(class, source, "protocol_class")
                {
                    f(class, marker.range().start().to_usize());
                }
            }
            Stmt::If(node) => {
                each_private_protocol(&node.body, source, f);
                for clause in &node.elif_else_clauses {
                    each_private_protocol(&clause.body, source, f);
                }
            }
            Stmt::Try(node) => {
                each_private_protocol(&node.body, source, f);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    each_private_protocol(&handler.body, source, f);
                }
                each_private_protocol(&node.orelse, source, f);
                each_private_protocol(&node.finalbody, source, f);
            }
            Stmt::With(node) => each_private_protocol(&node.body, source, f),
            _ => {}
        }
    }
}

/// the synthetic modifier decorator named `name`, if the class carries it.
///
/// the parser models a modifier keyword as a decorator whose source range does
/// not start with `@`; a real `@protocol_class` decorator is an ordinary
/// decorator and must not be mistaken for the modifier
fn marker<'a>(class: &'a StmtClassDef, source: &str, name: &str) -> Option<&'a Decorator> {
    class.decorator_list.iter().find(|decorator| {
        matches!(&decorator.expression, Expr::Name(id) if id.id.as_str() == name)
            && source
                .as_bytes()
                .get(decorator.range().start().to_usize())
                .copied()
                != Some(b'@')
    })
}

fn has_marker(class: &StmtClassDef, source: &str, name: &str) -> bool {
    marker(class, source, name).is_some()
}

/// every identifier-shaped token in `source`, including ones inside strings and
/// comments — a rename has to consider forward references and `__all__` entries
fn identifiers(source: &str) -> BTreeSet<String> {
    identifier_spans(source)
        .into_iter()
        .map(|(_, ident)| ident.to_string())
        .collect()
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

/// deletion edits stripping the leading underscore from every occurrence of a
/// converted name, wherever it sits — a string annotation and a doc comment are
/// as much a reference as an expression
pub(crate) fn strip_underscore_edits(source: &str, converted: &BTreeSet<&str>) -> Vec<Edit> {
    identifier_spans(source)
        .into_iter()
        .filter(|(_, ident)| converted.contains(ident))
        .map(|(start, _)| Edit {
            start,
            end: start + 1,
            replacement: String::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, source: &str) {
        std::fs::write(dir.join(name), source).expect("write stub");
    }

    #[test]
    fn skips_names_referenced_by_another_stub() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(
            dir.path(),
            "a.byi",
            "type _Shared = str\ntype _Local = int\nprotocol _Reader:\n    def read(self) -> str: ...\nprotocol _Writer:\n    def write(self) -> None: ...\n",
        );
        write(
            dir.path(),
            "b.byi",
            "from a import _Shared, _Reader\n\ndef f(x: _Shared, y: _Reader) -> None: ...\n",
        );

        let scan = scan(dir.path());
        let aliases = scan.aliases.get(Path::new("a.byi")).expect("aliases");
        assert!(aliases.contains("_Local"));
        assert!(!aliases.contains("_Shared"));
        let protocols = scan.protocols.get(Path::new("a.byi")).expect("protocols");
        assert!(protocols.contains("_Writer"));
        assert!(!protocols.contains("_Reader"));
    }

    #[test]
    fn allows_the_same_name_declared_in_two_stubs() {
        // each stub binds its own `_Address`, so neither mention is a reference
        // to the other and both convert independently
        let dir = tempfile::tempdir().expect("temp dir");
        write(dir.path(), "a.byi", "type _Address = str\n");
        write(dir.path(), "b.byi", "protocol _Address: ...\n");

        let scan = scan(dir.path());
        assert!(
            scan.aliases
                .get(Path::new("a.byi"))
                .is_some_and(|names| names.contains("_Address"))
        );
        assert!(
            scan.protocols
                .get(Path::new("b.byi"))
                .is_some_and(|names| names.contains("_Address"))
        );
    }

    #[test]
    fn skips_names_whose_stripped_spelling_is_taken() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(
            dir.path(),
            "a.byi",
            "type _Socket = int\nprotocol _Reader: ...\n\nclass Socket: ...\nclass Reader: ...\n",
        );

        let scan = scan(dir.path());
        assert!(!scan.aliases.contains_key(Path::new("a.byi")));
        assert!(!scan.protocols.contains_key(Path::new("a.byi")));
    }

    #[test]
    fn skips_protocols_nested_in_a_class_and_already_private_ones() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(
            dir.path(),
            "a.byi",
            "private protocol _Done: ...\n\nclass Outer:\n    protocol _Inner: ...\n",
        );

        let scan = scan(dir.path());
        assert!(!scan.protocols.contains_key(Path::new("a.byi")));
    }

    #[test]
    fn finds_protocols_under_a_version_guard() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(
            dir.path(),
            "a.byi",
            "import sys\n\nif sys.version_info >= (3, 12):\n    protocol _Guarded: ...\n",
        );

        let scan = scan(dir.path());
        assert!(
            scan.protocols
                .get(Path::new("a.byi"))
                .is_some_and(|names| names.contains("_Guarded"))
        );
    }

    #[test]
    fn dunder_names_are_not_candidates() {
        assert!(is_underscore_private("_X"));
        assert!(!is_underscore_private("_"));
        assert!(!is_underscore_private("__X"));
        assert!(!is_underscore_private("X"));
    }
}
