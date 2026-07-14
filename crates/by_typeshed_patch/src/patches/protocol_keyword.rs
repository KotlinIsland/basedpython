//! `class C[T](Base, Protocol)` → `protocol C[T](Base)`
//!
//! basedpython spells a protocol class with the `protocol` keyword instead of a
//! `Protocol` base. ty recognises the keyword's `protocol_class` marker natively
//! (see `types/class/static_literal.rs`), so the two forms are equivalent; this
//! is the inverse of the forward transpile, which re-inserts the `Protocol` base
//!
//! the rewrite replaces the `class` keyword with `protocol` and drops the
//! `Protocol` base, collapsing the base list to nothing when `Protocol` was the
//! only base

use std::path::Path;

use ruff_python_ast::{Arguments, Expr, ModModule, Stmt, StmtClassDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct ProtocolKeyword;

impl Patch for ProtocolKeyword {
    fn name(&self) -> &'static str {
        "protocol-keyword"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        walk_classes(&parsed.syntax().body, &mut |class| {
            convert(class, source, &mut edits);
        });
        edits
    }
}

fn convert(class: &StmtClassDef, source: &str, edits: &mut Vec<Edit>) {
    let Some(arguments) = &class.arguments else {
        return;
    };
    if !arguments.args.iter().any(is_protocol_base) {
        return;
    }

    // `class` → `protocol`: the keyword sits just before the class name (after
    // any decorators), separated only by whitespace. scan back over the whole
    // introducer word rather than a fixed width so the guard stays correct
    let bytes = source.as_bytes();
    let mut kw_end = class.name.range().start().to_usize();
    while kw_end > 0 && bytes[kw_end - 1].is_ascii_whitespace() {
        kw_end -= 1;
    }
    let mut kw_start = kw_end;
    while kw_start > 0
        && (bytes[kw_start - 1].is_ascii_alphanumeric() || bytes[kw_start - 1] == b'_')
    {
        kw_start -= 1;
    }
    if &source[kw_start..kw_end] != "class" {
        return;
    }
    edits.push(Edit {
        start: kw_start,
        end: kw_end,
        replacement: "protocol".to_string(),
    });

    // drop the `Protocol` base, rebuilding the remaining base list
    edits.push(rebuild_without_protocol(arguments, source));
}

fn rebuild_without_protocol(arguments: &Arguments, source: &str) -> Edit {
    let mut parts = Vec::new();
    for base in &arguments.args {
        if !is_protocol_base(base) {
            parts.push(source[base.range()].to_string());
        }
    }
    for keyword in &arguments.keywords {
        parts.push(source[keyword.range()].to_string());
    }
    let replacement = if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    };
    Edit {
        start: arguments.range().start().to_usize(),
        end: arguments.range().end().to_usize(),
        replacement,
    }
}

/// a bare `Protocol` / `typing.Protocol` base (not the generic `Protocol[...]`
/// subscript, which the pep 695 conversion has already turned into a bare base)
fn is_protocol_base(base: &Expr) -> bool {
    match base {
        Expr::Name(name) => name.id == "Protocol",
        Expr::Attribute(attr) => attr.attr.as_str() == "Protocol",
        _ => false,
    }
}

fn walk_classes<'a>(body: &'a [Stmt], f: &mut impl FnMut(&'a StmtClassDef)) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class) => {
                f(class);
                walk_classes(&class.body, f);
            }
            Stmt::If(node) => {
                walk_classes(&node.body, f);
                for clause in &node.elif_else_clauses {
                    walk_classes(&clause.body, f);
                }
            }
            Stmt::Try(node) => {
                walk_classes(&node.body, f);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk_classes(&h.body, f);
                }
                walk_classes(&node.orelse, f);
                walk_classes(&node.finalbody, f);
            }
            Stmt::With(node) => walk_classes(&node.body, f),
            _ => {}
        }
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
        let edits = ProtocolKeyword.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn sole_protocol_base_drops_parens() {
        assert_eq!(run("class C(Protocol): ...\n"), "protocol C: ...\n");
    }

    #[test]
    fn generic_protocol() {
        assert_eq!(
            run("class C[T](Protocol):\n    x: T\n"),
            "protocol C[T]:\n    x: T\n"
        );
    }

    #[test]
    fn protocol_with_extra_base() {
        assert_eq!(
            run("class C(Sized, Protocol): ...\n"),
            "protocol C(Sized): ...\n"
        );
    }

    #[test]
    fn protocol_first_then_base() {
        assert_eq!(
            run("class C(Protocol, Sized): ...\n"),
            "protocol C(Sized): ...\n"
        );
    }

    #[test]
    fn keeps_keyword_arguments() {
        assert_eq!(
            run("class C(Protocol, metaclass=ABCMeta): ...\n"),
            "protocol C(metaclass=ABCMeta): ...\n"
        );
    }

    #[test]
    fn keeps_runtime_checkable_decorator() {
        assert_eq!(
            run("@runtime_checkable\nclass C(Protocol): ...\n"),
            "@runtime_checkable\nprotocol C: ...\n"
        );
    }

    #[test]
    fn leaves_non_protocol_class() {
        let src = "class C(Sized): ...\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn idempotent_on_converted_form() {
        // a re-parsed `protocol …` has no `Protocol` base (it carries the
        // synthetic `protocol_class` marker instead), so a second pass is a no-op
        for src in [
            "protocol C: ...\n",
            "protocol C[T]:\n    x: T\n",
            "protocol C(Sized): ...\n",
            "@runtime_checkable\nprotocol C: ...\n",
        ] {
            assert_eq!(run(src), src, "not idempotent on {src:?}");
        }
    }
}
