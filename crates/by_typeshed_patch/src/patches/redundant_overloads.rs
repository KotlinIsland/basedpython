//! deletes overloads that exist only to placate mypy/pyright
//!
//! typeshed carries a handful of overloads whose sole purpose is to nudge
//! another checker's inference; they are, by their own comments, "technically
//! covered" by a more general overload. ty doesn't need them
//!
//! - `builtins.getattr` — the `None`/`bool`/`list`/`dict` `default` overloads
//!   are subsumed by the generic `default: T` overload
//!
//! matching is structural (parameter shapes, presence of type parameters) rather
//! than comment-based, so it stays correct if the surrounding text drifts

use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Parameters, Stmt, StmtFunctionDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch, delete_with_leading_comments};

pub struct DeleteRedundantOverloads;

impl Patch for DeleteRedundantOverloads {
    fn name(&self) -> &'static str {
        "delete-redundant-overloads"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &["builtins.getattr"]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if crate::module_qualname(module_path).as_deref() != Some("builtins") {
            return Vec::new();
        }

        let mut edits = Vec::new();
        for stmt in &parsed.syntax().body {
            if let Stmt::FunctionDef(func) = stmt
                && func.name.as_str() == "getattr"
                && is_redundant_getattr(func)
            {
                edits.push(delete_with_leading_comments(func.range(), source));
            }
        }
        edits
    }
}

/// the non-`self`/`cls` parameters of a signature, in order
fn value_params(params: &Parameters) -> impl Iterator<Item = &ruff_python_ast::Parameter> {
    params
        .posonlyargs
        .iter()
        .chain(&params.args)
        .map(|p| &p.parameter)
        .filter(|p| !matches!(p.name.as_str(), "self" | "cls"))
}

fn type_param_names(func: &StmtFunctionDef) -> Vec<&str> {
    func.type_params
        .as_ref()
        .map(|tps| tps.iter().map(|tp| tp.name().as_str()).collect())
        .unwrap_or_default()
}

/// a `getattr` overload is redundant when it pins `default` to a concrete type
/// (`None`, `bool`, a container). the surviving overloads are the 2-argument
/// form (no `default`) and the generic `default: T` form
fn is_redundant_getattr(func: &StmtFunctionDef) -> bool {
    let type_params = type_param_names(func);
    let Some(default) = value_params(&func.parameters).find(|p| p.name.as_str() == "default")
    else {
        return false;
    };
    let Some(annotation) = &default.annotation else {
        return false;
    };
    // keep only the overload whose `default` is a bare method type parameter
    match annotation.as_ref() {
        Expr::Name(name) => !type_params.contains(&name.id.as_str()),
        _ => true,
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
        let edits = DeleteRedundantOverloads.rewrite(Path::new("builtins.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn drops_concrete_getattr_overloads_and_comment() {
        let src = "\
def getattr(o: object, name: str, /) -> dynamic:
    \"\"\"doc\"\"\"

# While technically covered by the last overload, spelling out the types
# helps mypy out
def getattr(o: object, name: str, default: None, /) -> dynamic | None
def getattr(o: object, name: str, default: bool, /) -> dynamic | bool
def getattr(o: object, name: str, default: list[dynamic], /) -> dynamic | list[dynamic]
def getattr(o: object, name: str, default: dict[dynamic, dynamic], /) -> dynamic | dict[dynamic, dynamic]
def getattr[Element](o: object, name: str, default: Element, /) -> dynamic | Element
";
        let expected = "\
def getattr(o: object, name: str, /) -> dynamic:
    \"\"\"doc\"\"\"

def getattr[Element](o: object, name: str, default: Element, /) -> dynamic | Element
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn keeps_two_arg_getattr() {
        // a lone 2-arg getattr with no `default` is not redundant
        let src = "def getattr(o: object, name: str, /) -> dynamic\n";
        assert_eq!(run(src), src);
    }
}
