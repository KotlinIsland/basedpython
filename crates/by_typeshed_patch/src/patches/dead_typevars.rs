//! removes private module-level `TypeVar`/`ParamSpec`/`TypeVarTuple`
//! declarations that nothing uses
//!
//! the pep 695 conversion drops a typevar once a class/function conversion
//! consumes its last reference, but a typevar that was *already* pep 695 in the
//! reverse-transpiled stub (so no conversion ran) can leave its legacy
//! declaration stranded — e.g. `builtins._StartT_co`/`_StopT_co`, left over from
//! `slice` once it became `slice[out StartT = Any, ...]`
//!
//! only private (`_`-prefixed) names are removed — a public typevar may be
//! re-exported and imported elsewhere. a name is kept if it is referenced
//! anywhere outside a typevar declaration; references from one dead typevar's
//! `default`/`bound` to another don't count, so a whole dead chain
//! (`_StopT_co` defaulting to `_StartT_co`) is removed together

use std::path::Path;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{Expr, ModModule, Stmt, StmtAssign};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

pub struct DeleteDeadTypevars;

impl Patch for DeleteDeadTypevars {
    fn name(&self) -> &'static str {
        "delete-dead-typevars"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let body = &parsed.syntax().body;
        let mut decls: Vec<(&str, TextRange)> = Vec::new();
        collect_decls(body, &mut decls);
        if decls.is_empty() {
            return Vec::new();
        }

        // collect every load reference to a declared name, with its position
        let names: Vec<&str> = decls.iter().map(|(n, _)| *n).collect();
        let mut refs = RefCollector {
            names: &names,
            hits: Vec::new(),
        };
        for stmt in body {
            refs.visit_stmt(stmt);
        }

        // a name is "externally used" if it has a reference outside every
        // typevar declaration statement
        let decl_ranges: Vec<TextRange> = decls.iter().map(|(_, r)| *r).collect();
        decls
            .iter()
            .filter(|(name, _)| name.starts_with('_'))
            .filter(|(name, _)| {
                !refs
                    .hits
                    .iter()
                    .any(|(n, pos)| n == name && !decl_ranges.iter().any(|r| r.contains(*pos)))
            })
            .map(|(_, range)| remove_stmt(*range, source))
            .collect()
    }
}

fn collect_decls<'a>(body: &'a [Stmt], out: &mut Vec<(&'a str, TextRange)>) {
    for stmt in body {
        match stmt {
            Stmt::Assign(assign) => {
                if let Some(name) = typevar_decl_name(assign) {
                    out.push((name, assign.range()));
                }
            }
            Stmt::If(node) => {
                collect_decls(&node.body, out);
                for clause in &node.elif_else_clauses {
                    collect_decls(&clause.body, out);
                }
            }
            Stmt::Try(node) => {
                collect_decls(&node.body, out);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_decls(&h.body, out);
                }
                collect_decls(&node.orelse, out);
                collect_decls(&node.finalbody, out);
            }
            Stmt::With(node) => collect_decls(&node.body, out),
            _ => {}
        }
    }
}

fn typevar_decl_name(assign: &StmtAssign) -> Option<&str> {
    let [Expr::Name(target)] = assign.targets.as_slice() else {
        return None;
    };
    let Expr::Call(call) = &*assign.value else {
        return None;
    };
    let callee = match &*call.func {
        Expr::Name(name) => name.id.as_str(),
        Expr::Attribute(attr) => attr.attr.as_str(),
        _ => return None,
    };
    matches!(callee, "TypeVar" | "ParamSpec" | "TypeVarTuple").then(|| target.id.as_str())
}

struct RefCollector<'a> {
    names: &'a [&'a str],
    hits: Vec<(&'a str, ruff_text_size::TextSize)>,
}

impl<'a> SourceOrderVisitor<'a> for RefCollector<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_load()
            && let Some(known) = self.names.iter().find(|n| **n == name.id.as_str())
        {
            self.hits.push((known, name.range().start()));
        }
        walk_expr(self, expr);
    }
}

/// delete the declaration's whole physical line(s), including a trailing comment
fn remove_stmt(range: TextRange, source: &str) -> Edit {
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
        let edits = DeleteDeadTypevars.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn removes_dead_chain() {
        let src = "\
_StartT_co = TypeVar(\"_StartT_co\", covariant=True, default=Any)
_StopT_co = TypeVar(\"_StopT_co\", covariant=True, default=_StartT_co)
class slice[out StartT = Any, out StopT = StartT]: ...
";
        let expected = "class slice[out StartT = Any, out StopT = StartT]: ...\n";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn keeps_used_typevar() {
        let src = "\
_T = TypeVar(\"_T\")
def f(x: _T) -> _T: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn keeps_public_typevar() {
        let src = "AnyStr = TypeVar(\"AnyStr\", str, bytes)\n";
        assert_eq!(run(src), src);
    }
}
