//! the `collections.abc` ABCs live where python defines them
//!
//! upstream typeshed declares `Mapping`, `Iterator`, `Sequence` and the rest of
//! the `collections.abc` ABCs in `typing.pyi`, and has `_collections_abc.pyi`
//! import them straight back (`from typing import Mapping as Mapping`). that is
//! the opposite of what happens at runtime: `_collections_abc.py` defines the
//! classes, `collections/abc.py` is nothing but `from _collections_abc import *`,
//! and `typing` re-exports the same objects under its deprecated aliases. a
//! reader following `collections.abc.Mapping` to its definition ends up in
//! `typing`, and a diagnostic naming the defining module says `typing.Mapping`
//! for a class nobody thinks of as living there
//!
//! this patch turns the relationship around. the class statements move from
//! `typing` into `_collections_abc`, and `typing` gains the re-export import
//! that `_collections_abc` used to hold. every existing spelling keeps working —
//! `typing.Mapping`, `collections.abc.Mapping` and the bare `Mapping` that
//! basedpython makes implicitly available all resolve to the one class
//!
//! `Callable` is the one name that is not moved but rewritten. typeshed spells
//! it `Callable: _SpecialForm`, which is not a definition of anything; the
//! runtime has a real ABC with an abstract `__call__`, so that is what this
//! patch writes into `_collections_abc`. ty still reads a subscripted `Callable`
//! as its own callable type rather than as an instance of that class, exactly as
//! it does for the `class Any` that typeshed already writes out in full
//!
//! the moved statements travel as maximal *runs* of consecutive top-level
//! statements rather than one at a time, so the comments and blank lines
//! between them come along instead of being stranded
//!
//! the `_collections_abc` import block is replaced wholesale rather than edited,
//! because what the moved code needs is decided here and not by what upstream
//! happened to import for its re-export. an upstream sync that adds an import to
//! that module will lose it; `target_symbols` is what flags this patch for review
//! when that happens

use std::path::Path;

use ruff_python_ast::{Alias, Expr, ModModule, PySourceType, Stmt, StmtImportFrom};
use ruff_python_parser::{Parsed, parse_unchecked_source};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::{Edit, Patch, delete_with_leading_comments, module_qualname};

/// the `collections.abc` ABCs, under the names typeshed gives their class
/// statements. `Callable` is absent: it has no class statement to move, and is
/// written fresh into `_collections_abc` instead
///
/// `AbstractSet` keeps its typeshed name rather than the runtime's `Set`, so
/// that diagnostics go on saying `AbstractSet` and the plain `set` alias in
/// `typing` stays unambiguous. `_collections_abc` gets `Set` as an alias of it
const MOVED: &[&str] = &[
    "AbstractSet",
    "AsyncGenerator",
    "AsyncIterable",
    "AsyncIterator",
    "Awaitable",
    "Collection",
    "Container",
    "Coroutine",
    "Generator",
    "Hashable",
    "ItemsView",
    "Iterable",
    "Iterator",
    "KeysView",
    "Mapping",
    "MappingView",
    "MutableMapping",
    "MutableSequence",
    "MutableSet",
    "Reversible",
    "Sequence",
    "Sized",
    "ValuesView",
];

/// what `_collections_abc` imports once it owns the ABCs. `sys` and
/// `MappingProxyType` are upstream's; the rest are what the moved code needs.
/// `Overlapping` and `dynamic` are basedpython spellings that need no import
const IMPORTS: &str = "\
import sys
import typing_extensions
from _typeshed import SupportsGetItem, SupportsGetItemViewable, SupportsKeysAndGetItem, Viewable
from abc import ABCMeta
from types import MappingProxyType, TracebackType
from typing import ByteString as ByteString, ClassVar, runtime_checkable  # noqa: Y022,Y038,UP035,Y057";

/// the real `Callable`, replacing typeshed's `Callable: _SpecialForm`. the
/// parameters bound is the top-parameters form, so `Parameters` ranges over
/// every parameter list
const CALLABLE: &str = r#"@runtime_checkable
protocol Callable[in Parameters: (*: *, **: *), out Return](metaclass=ABCMeta):
    """A callable is anything that can be applied to an argument list.

    `Callable[[int], str]` describes a callable taking a single `int` and
    returning a `str`; basedpython spells the same type `(int) -> str`.
    """

    __slots__ = ()
    abstract def __call__(self, *args: *Parameters, **kwargs: **Parameters) -> Return
"#;

/// the runtime name of `AbstractSet`, which `collections.abc.Set` is
const SET_ALIAS: &str = "\
# `collections.abc` calls this class `Set`; `typing` calls it `AbstractSet`, to
# leave the `typing.Set` name free for the deprecated alias of `builtins.set`
Set = AbstractSet
";

/// what the patch takes out of `typing`, decided once over that module so both
/// halves of the move agree on it
pub struct CollectionsAbcHome {
    relocation: Option<Relocation>,
}

struct Relocation {
    /// source text of each run of moved statements, in `typing` order
    definitions: Vec<String>,
    /// byte ranges to cut from `typing`: the moved runs, plus the `Callable`
    /// declaration that is rewritten rather than moved
    cuts: Vec<(usize, usize)>,
}

/// read `typing.byi` and work out what moves. returns `None` when there is
/// nothing to do — which is the case on every run after the first, because the
/// classes are then declared in `_collections_abc` and `typing` only imports
/// them
pub(crate) fn scan(root: &Path) -> CollectionsAbcHome {
    let relocation = std::fs::read_to_string(root.join("typing.byi"))
        .ok()
        // the classes travel as source text, so they have to be read *after* the pep 695
        // conversion has rewritten them — a class moved in the same run that converts
        // `typing` arrives in its legacy form, and the `TypeVar` declarations it refers to
        // stay behind in `typing` and are deleted with the conversion, leaving it broken.
        // the sync script runs this binary to a fixed point, so declining here just moves
        // the relocation into the next run, by which time `typing` is converted
        .filter(|source| !declares_legacy_typevars(source))
        .and_then(|source| {
            let parsed = parse_unchecked_source(&source, PySourceType::BasedPythonStub);
            relocation(&parsed, &source)
        });
    CollectionsAbcHome { relocation }
}

/// whether `typing` still declares the module-level legacy `TypeVar`s that the pep 695
/// conversion turns into type parameters.
///
/// `AnyStr` is deliberately left in legacy form — it is a constrained typevar the stdlib
/// exports by name — so it does not count as unconverted.
fn declares_legacy_typevars(source: &str) -> bool {
    let parsed = parse_unchecked_source(source, PySourceType::BasedPythonStub);
    parsed.syntax().body.iter().any(|stmt| {
        let Stmt::Assign(assign) = stmt else {
            return false;
        };
        let [Expr::Name(target)] = assign.targets.as_slice() else {
            return false;
        };
        if target.id.as_str() == "AnyStr" {
            return false;
        }
        assign
            .value
            .as_call_expr()
            .and_then(|call| call.func.as_name_expr())
            .is_some_and(|func| func.id.as_str() == "TypeVar")
    })
}

impl Patch for CollectionsAbcHome {
    fn name(&self) -> &'static str {
        "collections-abc-home"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[
            "typing.Callable",
            "typing.Mapping",
            "typing.Sequence",
            "typing.Iterator",
            "_collections_abc.Callable",
            "_collections_abc.Mapping",
        ]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let Some(relocation) = &self.relocation else {
            return Vec::new();
        };
        match module_qualname(module_path).as_deref() {
            Some("typing") => rewrite_typing(relocation, parsed, source),
            Some("_collections_abc") => rewrite_collections_abc(relocation, parsed, source),
            _ => Vec::new(),
        }
    }
}

/// what happens to a top-level statement of `typing`
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// stays where it is
    Keep,
    /// moves to `_collections_abc` verbatim
    Move,
    /// goes away, with `_collections_abc` gaining a rewritten form
    Drop,
}

fn relocation(parsed: &Parsed<ModModule>, source: &str) -> Option<Relocation> {
    let body = &parsed.syntax().body;
    let mut fates = Vec::with_capacity(body.len());
    for stmt in body {
        // a bare string literal is the docstring of the statement above it, so
        // it shares that statement's fate. this is what carries the prose under
        // `Callable: _SpecialForm` out along with the declaration
        let fate = if is_docstring(stmt) {
            fates.last().copied().unwrap_or(Fate::Keep)
        } else if stmt
            .as_class_def_stmt()
            .is_some_and(|class| MOVED.contains(&class.name.as_str()))
        {
            Fate::Move
        } else if is_callable_special_form(stmt) {
            Fate::Drop
        } else {
            Fate::Keep
        };
        fates.push(fate);
    }

    let mut definitions = Vec::new();
    let mut cuts = Vec::new();
    let mut index = 0;
    while index < body.len() {
        let fate = fates[index];
        if fate == Fate::Keep {
            index += 1;
            continue;
        }
        // extend over every following statement sharing this fate, so the run
        // is cut as one span and the comments inside it travel with the code
        let start = index;
        while index + 1 < body.len() && fates[index + 1] == fate {
            index += 1;
        }
        let span = span_of(&body[start], &body[index], source);
        if fate == Fate::Move {
            definitions.push(source[span.0..span.1].to_string());
        }
        cuts.push(span);
        index += 1;
    }

    (!definitions.is_empty()).then_some(Relocation { definitions, cuts })
}

fn is_docstring(stmt: &Stmt) -> bool {
    stmt.as_expr_stmt()
        .is_some_and(|expr| expr.value.is_string_literal_expr())
}

/// typeshed's `Callable: _SpecialForm`
fn is_callable_special_form(stmt: &Stmt) -> bool {
    let Stmt::AnnAssign(ann_assign) = stmt else {
        return false;
    };
    matches!(&*ann_assign.target, Expr::Name(name) if name.id == "Callable")
        && matches!(&*ann_assign.annotation, Expr::Name(name) if name.id == "_SpecialForm")
}

/// the span a run of statements occupies: from the comment lines that abut the
/// first through the blank lines that follow the last, so that cutting it leaves
/// exactly the separation the surrounding statements already had
fn span_of(first: &Stmt, last: &Stmt, source: &str) -> (usize, usize) {
    let start = decorated_start(first);
    let bounds = delete_with_leading_comments(TextRange::new(start, last.range().end()), source);
    let bytes = source.as_bytes();
    let mut end = bounds.end;
    while end < bytes.len() && bytes[end] == b'\n' {
        end += 1;
    }
    (bounds.start, end)
}

/// where a statement begins on the page, which for a decorated class or function
/// is its first decorator
fn decorated_start(stmt: &Stmt) -> TextSize {
    let decorators = match stmt {
        Stmt::ClassDef(class) => class.decorator_list.first(),
        Stmt::FunctionDef(function) => function.decorator_list.first(),
        _ => None,
    };
    decorators.map_or(stmt.range().start(), |decorator| {
        decorator.range().start().min(stmt.range().start())
    })
}

fn rewrite_typing(relocation: &Relocation, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
    let mut edits: Vec<Edit> = relocation
        .cuts
        .iter()
        .map(|&(start, end)| Edit {
            start,
            end,
            replacement: String::new(),
        })
        .collect();

    // `typing` takes over the re-export import that `_collections_abc` held
    if let Some(import) = import_from(parsed, "_collections_abc") {
        let mut re_exports: Vec<String> = MOVED
            .iter()
            .chain(std::iter::once(&"Callable"))
            .map(|name| format!("    {name} as {name},"))
            .collect();
        re_exports.sort();
        // whatever else `typing` already took from the module — the private dict
        // views — keeps its plain, non-re-exporting import
        for alias in &import.names {
            if !MOVED.contains(&alias.name.as_str()) && alias.name.as_str() != "Callable" {
                re_exports.push(format!("    {},", render_alias(alias)));
            }
        }
        edits.push(Edit {
            start: import.range().start().to_usize(),
            end: import.range().end().to_usize(),
            replacement: format!(
                "from _collections_abc import (\n{}\n)",
                re_exports.join("\n")
            ),
        });
    }

    // the moved code took the only uses of some `_typeshed` helpers with it
    if let Some(import) = import_from(parsed, "_typeshed") {
        let mut remaining = source.to_string();
        let import_range = (
            import.range().start().to_usize(),
            import.range().end().to_usize(),
        );
        for &(start, end) in relocation.cuts.iter().chain(std::iter::once(&import_range)) {
            blank_out(&mut remaining, start, end);
        }
        let kept: Vec<&Alias> = import
            .names
            .iter()
            .filter(|alias| references(&remaining, alias.asname.as_ref().unwrap_or(&alias.name)))
            .collect();
        if kept.is_empty() {
            edits.push(delete_with_leading_comments(import.range(), source));
        } else if kept.len() < import.names.len() {
            let names = kept
                .iter()
                .map(|alias| render_alias(alias))
                .collect::<Vec<_>>()
                .join(", ");
            edits.push(Edit {
                start: import_range.0,
                end: import_range.1,
                replacement: format!("from _typeshed import {names}"),
            });
        }
    }

    edits
}

fn rewrite_collections_abc(
    relocation: &Relocation,
    parsed: &Parsed<ModModule>,
    source: &str,
) -> Vec<Edit> {
    let body = &parsed.syntax().body;
    let imports: Vec<&Stmt> = body
        .iter()
        .filter(|stmt| matches!(stmt, Stmt::Import(_) | Stmt::ImportFrom(_)))
        .collect();
    // the classes go after `__all__` and before the dict views, which is where
    // the module's first class statement already begins
    let anchor = body.iter().find(|stmt| stmt.is_class_def_stmt());
    let (Some(first), Some(last), Some(anchor)) = (imports.first(), imports.last(), anchor) else {
        return Vec::new();
    };

    // the whole import block is replaced: what the module imported was in
    // service of the re-export it no longer performs
    let (start, end) = span_of(first, last, source);
    let mut edits = vec![Edit {
        start,
        end,
        replacement: format!("{IMPORTS}\n\n"),
    }];

    let mut relocated = format!("{CALLABLE}\n");
    for definition in &relocation.definitions {
        relocated.push_str(definition.trim_end());
        relocated.push_str("\n\n");
    }
    relocated.push_str(SET_ALIAS);
    relocated.push('\n');
    let at = delete_with_leading_comments(anchor.range(), source).start;
    edits.push(Edit {
        start: at,
        end: at,
        replacement: relocated,
    });

    edits
}

/// the module's `from <module> import …` statement, if it has one
fn import_from<'a>(parsed: &'a Parsed<ModModule>, module: &str) -> Option<&'a StmtImportFrom> {
    parsed
        .syntax()
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::ImportFrom(import) => Some(import),
            _ => None,
        })
        .find(|import| import.module.as_deref() == Some(module))
}

fn render_alias(alias: &Alias) -> String {
    match &alias.asname {
        Some(asname) => format!("{} as {asname}", alias.name),
        None => alias.name.to_string(),
    }
}

/// overwrite `start..end` with spaces, keeping every other byte offset valid
fn blank_out(source: &mut String, start: usize, end: usize) {
    let blanked: String = source[start..end]
        .chars()
        .map(|c| if c == '\n' { '\n' } else { ' ' })
        .collect();
    source.replace_range(start..end, &blanked);
}

/// whether `source` mentions the identifier `name`, as a whole word
fn references(source: &str, name: &str) -> bool {
    let is_part = |c: char| c.is_alphanumeric() || c == '_';
    source.match_indices(name).any(|(at, _)| {
        !source[..at].chars().next_back().is_some_and(is_part)
            && !source[at + name.len()..]
                .chars()
                .next()
                .is_some_and(is_part)
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use super::{CollectionsAbcHome, references, relocation};
    use crate::{Patch, apply_edits};

    const TYPING: &str = r#"import sys
from _collections_abc import dict_items, dict_keys, dict_values
from _typeshed import IdentityFunction, Viewable

Callable: _SpecialForm
"""Deprecated alias to collections.abc.Callable."""

Type: _SpecialForm
"""Deprecated alias to builtins.type."""

@runtime_checkable
protocol Sized(metaclass=ABCMeta):
    abstract def __len__(self) -> int

# a comment between two moved classes
class KeysView[out Key](Viewable[Key]):
    override def __iter__(self) -> Iterator[Key]

def identity() -> IdentityFunction
Text = str
"#;

    const COLLECTIONS_ABC: &str = r#""""Abstract Base Classes."""

import sys
from types import MappingProxyType
from typing import KeysView as KeysView, Sized as Sized

__all__ = ["Sized", "KeysView"]

final class dict_keys[out Key, out Value](KeysView[Key]):  # undocumented
    let mapping: MappingProxyType[Key, Value]
"#;

    fn patch() -> CollectionsAbcHome {
        let parsed = parse_unchecked_source(TYPING, PySourceType::BasedPythonStub);
        CollectionsAbcHome {
            relocation: relocation(&parsed, TYPING),
        }
    }

    fn rewrite(patch: &CollectionsAbcHome, path: &str, source: &str) -> String {
        let parsed = parse_unchecked_source(source, PySourceType::BasedPythonStub);
        apply_edits(source, patch.rewrite(Path::new(path), &parsed, source))
    }

    #[test]
    fn typing_keeps_only_the_re_export() {
        let out = rewrite(&patch(), "typing.byi", TYPING);
        assert!(!out.contains("protocol Sized"), "{out}");
        assert!(!out.contains("Callable: _SpecialForm"), "{out}");
        assert!(
            !out.contains("Deprecated alias to collections.abc"),
            "{out}"
        );
        assert!(out.contains("    Sized as Sized,"), "{out}");
        assert!(out.contains("    Callable as Callable,"), "{out}");
        assert!(out.contains("    dict_items,"), "{out}");
        // the sibling special form and its docstring are untouched
        assert!(out.contains("Type: _SpecialForm"), "{out}");
        assert!(out.contains("Deprecated alias to builtins.type"), "{out}");
    }

    #[test]
    fn typing_drops_the_imports_the_moved_code_took_with_it() {
        let out = rewrite(&patch(), "typing.byi", TYPING);
        assert!(
            out.contains("from _typeshed import IdentityFunction\n"),
            "{out}"
        );
    }

    #[test]
    fn collections_abc_gains_the_definitions() {
        let out = rewrite(&patch(), "_collections_abc.byi", COLLECTIONS_ABC);
        assert!(out.contains("protocol Sized"), "{out}");
        assert!(out.contains("protocol Callable["), "{out}");
        assert!(out.contains("Set = AbstractSet"), "{out}");
        // the comment between two moved classes travels with them
        assert!(
            out.contains("# a comment between two moved classes"),
            "{out}"
        );
        // the re-export import is gone, and the class it fed is still declared below
        assert!(!out.contains("KeysView as KeysView"), "{out}");
        assert!(out.contains("final class dict_keys"), "{out}");
    }

    #[test]
    fn re_running_the_patch_changes_nothing() {
        let moved = rewrite(&patch(), "_collections_abc.byi", COLLECTIONS_ABC);
        let typing = rewrite(&patch(), "typing.byi", TYPING);

        // a second sync scans the already-moved `typing`, and finds no classes
        let parsed = parse_unchecked_source(&typing, PySourceType::BasedPythonStub);
        let again = CollectionsAbcHome {
            relocation: relocation(&parsed, &typing),
        };
        assert_eq!(rewrite(&again, "typing.byi", &typing), typing);
        assert_eq!(rewrite(&again, "_collections_abc.byi", &moved), moved);
    }

    #[test]
    fn references_matches_whole_identifiers_only() {
        assert!(references("x: SupportsGetItem[int]", "SupportsGetItem"));
        assert!(!references(
            "x: SupportsGetItemViewable[int]",
            "SupportsGetItem"
        ));
        assert!(!references("x: _SupportsGetItem", "SupportsGetItem"));
    }
}
