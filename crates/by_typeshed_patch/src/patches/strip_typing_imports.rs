//! removes `from typing import ...` names that basedpython provides implicitly
//!
//! ty auto-resolves a fixed set of `typing` type-construction names in a
//! basedpython file (see [`ruff_python_stdlib::basedpython`]), so importing
//! them is redundant. this drops exactly those names from `from typing import`
//! statements (deleting the statement if nothing is left), and keeps every other
//! name — runtime helpers (`overload`, `type_check_only`), constructors
//! (`TypeVar`, `ParamSpec`), and qualifiers (`ClassVar`) still need their import
//!
//! aliased imports (`Set as AbstractSet`) are left alone: the bound name may not
//! match the implicit name's meaning, and so are re-exports (`from typing export
//! Never`) — the name being implicit *here* says nothing about the module that
//! imports it from this one

use std::path::Path;

use ruff_python_ast::{ModModule, Stmt};
use ruff_python_parser::Parsed;
use ruff_python_stdlib::basedpython::is_implicit_typing_name;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

pub struct StripTypingImports;

impl Patch for StripTypingImports {
    fn name(&self) -> &'static str {
        "strip-typing-imports"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        walk(&parsed.syntax().body, source, &mut edits);
        edits
    }
}

fn walk(body: &[Stmt], source: &str, edits: &mut Vec<Edit>) {
    for stmt in body {
        match stmt {
            Stmt::ImportFrom(import) => {
                if import
                    .module
                    .as_ref()
                    .map(ruff_python_ast::Identifier::as_str)
                    != Some("typing")
                {
                    continue;
                }
                // a re-export is what makes `from typing_extensions import Never` resolve
                // at all, so it stays even though the name is implicit here
                if import.is_export {
                    continue;
                }
                // an implicit name imported without an alias is redundant
                let (drop, keep): (Vec<_>, Vec<_>) = import.names.iter().partition(|alias| {
                    alias.asname.is_none() && is_implicit_typing_name(alias.name.as_str())
                });
                if drop.is_empty() {
                    continue;
                }
                if keep.is_empty() {
                    edits.push(delete_line(import.range(), source));
                } else {
                    let kept: Vec<&str> = keep.iter().map(|a| &source[a.range()]).collect();
                    edits.push(Edit {
                        start: import.range().start().to_usize(),
                        end: import.range().end().to_usize(),
                        replacement: format!("from typing import {}", kept.join(", ")),
                    });
                }
            }
            Stmt::If(node) => {
                walk(&node.body, source, edits);
                for clause in &node.elif_else_clauses {
                    walk(&clause.body, source, edits);
                }
            }
            Stmt::Try(node) => {
                walk(&node.body, source, edits);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk(&h.body, source, edits);
                }
                walk(&node.orelse, source, edits);
                walk(&node.finalbody, source, edits);
            }
            Stmt::With(node) => walk(&node.body, source, edits),
            _ => {}
        }
    }
}

/// deletion edit covering the whole physical span of `range` (import statements
/// may wrap across lines) plus the trailing newline
fn delete_line(range: TextRange, source: &str) -> Edit {
    let bytes = source.as_bytes();
    let mut start = range.start().to_usize();
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = range.end().to_usize();
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() {
        end += 1;
    }
    Edit {
        start,
        end,
        replacement: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = StripTypingImports.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn drops_implicit_keeps_others() {
        assert_eq!(
            run("from typing import Mapping, ClassVar, Sequence, overload\n"),
            "from typing import ClassVar, overload\n"
        );
    }

    #[test]
    fn deletes_all_implicit_import() {
        assert_eq!(run("from typing import Mapping, Sequence, Any\n"), "");
    }

    #[test]
    fn keeps_non_implicit_import_untouched() {
        let src = "from typing import ClassVar, TypeVar, overload\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn keeps_aliased_import() {
        let src = "from typing import Set as AbstractSet, ClassVar\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn ignores_other_modules() {
        let src = "from collections.abc import Mapping\n";
        assert_eq!(run(src), src);
    }
}
