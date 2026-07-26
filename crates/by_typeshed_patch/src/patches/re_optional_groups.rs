//! sound optional types for `re` capture groups
//!
//! upstream typeshed types every "this group may not have participated" position
//! in `re` as `AnyStr | MaybeNone` — and `MaybeNone` is `Any`, so `m.group(1)`
//! silently accepts `.upper()` on a group that is `None` at runtime. basedpython
//! spells the possibility out instead, so those positions become
//! `AnyStr | None`
//!
//! `re` is the only module where `MaybeNone` stands for a capture group, and
//! every one of its uses there is such a group (`Match.group`, `Match.groups`,
//! `Match.groupdict`, `Match.__getitem__`, and the `split` functions, whose
//! `None`s are the unmatched groups), so the whole module is rewritten. the now
//! unused import is dropped with it
//!
//! where the pattern is a literal, ty reads its capture groups and replaces
//! these types with something exact; this patch is what a pattern it cannot see
//! falls back to

use std::path::Path;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{Expr, ModModule, Stmt, StmtImportFrom};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

/// the only module whose `MaybeNone`s are all capture groups
const MODULE: &str = "re";
/// the `Any` alias upstream uses to mean "may be `None`"
const MAYBE_NONE: &str = "MaybeNone";

pub struct ReOptionalGroups;

impl Patch for ReOptionalGroups {
    fn name(&self) -> &'static str {
        "re-optional-groups"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &["re.Match", "re.Pattern", "re.split"]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if crate::module_qualname(module_path).as_deref() != Some(MODULE) {
            return Vec::new();
        }

        let mut collector = MaybeNoneReferences::default();
        let mut edits: Vec<Edit> = Vec::new();
        for stmt in &parsed.syntax().body {
            if let Stmt::ImportFrom(import) = stmt {
                edits.extend(drop_maybe_none_import(import, source));
                continue;
            }
            collector.visit_stmt(stmt);
        }

        edits.extend(collector.spans.into_iter().map(|(start, end)| Edit {
            start,
            end,
            replacement: "None".to_string(),
        }));
        edits
    }
}

/// rewrite (or delete) a `from … import …` that brings `MaybeNone` in
fn drop_maybe_none_import(import: &StmtImportFrom, source: &str) -> Option<Edit> {
    if !import
        .names
        .iter()
        .any(|alias| alias.name.as_str() == MAYBE_NONE)
    {
        return None;
    }

    let start = import.range().start().to_usize();
    let end = import.range().end().to_usize();
    let remaining: Vec<String> = import
        .names
        .iter()
        .filter(|alias| alias.name.as_str() != MAYBE_NONE)
        .map(|alias| match &alias.asname {
            Some(asname) => format!("{} as {}", alias.name, asname),
            None => alias.name.to_string(),
        })
        .collect();

    if remaining.is_empty() {
        // nothing else came from there, so the whole line goes — along with the
        // blank lines that were only separating it from what follows
        let mut end = source[end..]
            .find('\n')
            .map_or(source.len(), |offset| end + offset + 1);
        while let Some(offset) = source[end..].find('\n') {
            if !source[end..end + offset].trim().is_empty() {
                break;
            }
            end += offset + 1;
        }
        return Some(Edit {
            start,
            end,
            replacement: String::new(),
        });
    }

    let dots = ".".repeat(import.level as usize);
    let module = import.module.as_ref().map_or("", |module| module.as_str());
    Some(Edit {
        start,
        end,
        replacement: format!("from {dots}{module} import {}", remaining.join(", ")),
    })
}

/// collects the byte spans of every `MaybeNone` reference in the module
#[derive(Default)]
struct MaybeNoneReferences {
    spans: Vec<(usize, usize)>,
}

impl<'a> SourceOrderVisitor<'a> for MaybeNoneReferences {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.id.as_str() == MAYBE_NONE
        {
            self.spans
                .push((name.range.start().to_usize(), name.range.end().to_usize()));
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(path: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = ReOptionalGroups.rewrite(Path::new(path), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn rewrites_every_group_position() {
        let src = "\
from _typeshed import MaybeNone, ReadableBuffer

final class Match[AnyStr]:
    def group(self, group: str | int, /) -> AnyStr | MaybeNone
    def groups(self) -> (*: AnyStr | MaybeNone)
    def groupdict(self) -> dict[str, AnyStr | MaybeNone]
    def __getitem__(self, key: int | str, /) -> AnyStr | MaybeNone

def split(pattern: str, string: str) -> list[str | MaybeNone]
";
        let expected = "\
from _typeshed import ReadableBuffer

final class Match[AnyStr]:
    def group(self, group: str | int, /) -> AnyStr | None
    def groups(self) -> (*: AnyStr | None)
    def groupdict(self) -> dict[str, AnyStr | None]
    def __getitem__(self, key: int | str, /) -> AnyStr | None

def split(pattern: str, string: str) -> list[str | None]
";
        assert_eq!(run("re.byi", src), expected);
    }

    #[test]
    fn drops_the_import_line_when_nothing_else_came_from_it() {
        let src = "\
from _typeshed import MaybeNone

def split(pattern: str) -> list[str | MaybeNone]
";
        let expected = "\
def split(pattern: str) -> list[str | None]
";
        assert_eq!(run("re.byi", src), expected);
    }

    #[test]
    fn keeps_an_alias_intact() {
        let src = "from _typeshed import MaybeNone, ReadableBuffer as Buffer\n";
        let expected = "from _typeshed import ReadableBuffer as Buffer\n";
        assert_eq!(run("re.byi", src), expected);
    }

    #[test]
    fn idempotent_once_rewritten() {
        let src = "\
from _typeshed import ReadableBuffer

def split(pattern: str) -> list[str | None]
";
        assert_eq!(run("re.byi", src), src);
    }

    #[test]
    fn skips_other_modules() {
        let src = "\
from _typeshed import MaybeNone

def readline() -> str | MaybeNone
";
        assert_eq!(run("io.byi", src), src);
    }
}
