//! `tuple[X, ...]` → `(*: X)` (basedpython's variadic homogeneous-tuple syntax)
//!
//! the reverse-transpile already spells most homogeneous tuples this way, but it
//! misses a few positions (notably inside a legacy `X: TypeAlias = …` value,
//! which by the time this pass runs is a pep 695 `type X = …`). this converts
//! whatever is left, so the whole `.byi` uses the one idiom
//!
//! only the exact 2-element `tuple[X, ...]` shape is a homogeneous tuple; a
//! heterogeneous `tuple[int, str]` or the already-converted `(*: X)` is left

use std::path::Path;

use ruff_python_ast::{
    Expr, ModModule, Parameters, Stmt, StmtAnnAssign, StmtClassDef, StmtFunctionDef, TypeParam,
};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct HomogeneousTuple;

impl Patch for HomogeneousTuple {
    fn name(&self) -> &'static str {
        "homogeneous-tuple"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut rw = Rewriter {
            source,
            edits: Vec::new(),
        };
        rw.walk_body(&parsed.syntax().body);
        rw.edits
    }
}

struct Rewriter<'src> {
    source: &'src str,
    edits: Vec<Edit>,
}

impl Rewriter<'_> {
    fn walk_body(&mut self, body: &[Stmt]) {
        for stmt in body {
            match stmt {
                Stmt::FunctionDef(func) => self.function(func),
                Stmt::ClassDef(class) => self.class(class),
                Stmt::AnnAssign(assign) => self.ann_assign(assign),
                Stmt::TypeAlias(alias) => self.type_expr(&alias.value),
                Stmt::If(node) => {
                    self.walk_body(&node.body);
                    for clause in &node.elif_else_clauses {
                        self.walk_body(&clause.body);
                    }
                }
                Stmt::Try(node) => {
                    self.walk_body(&node.body);
                    for handler in &node.handlers {
                        let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                        self.walk_body(&h.body);
                    }
                    self.walk_body(&node.orelse);
                    self.walk_body(&node.finalbody);
                }
                Stmt::With(node) => self.walk_body(&node.body),
                _ => {}
            }
        }
    }

    fn function(&mut self, func: &StmtFunctionDef) {
        self.type_params(func.type_params.as_deref());
        self.signature(&func.parameters);
        if let Some(returns) = &func.returns {
            self.type_expr(returns);
        }
        self.walk_body(&func.body);
    }

    fn class(&mut self, class: &StmtClassDef) {
        self.type_params(class.type_params.as_deref());
        if let Some(arguments) = &class.arguments {
            for base in &arguments.args {
                self.type_expr(base);
            }
        }
        self.walk_body(&class.body);
    }

    fn ann_assign(&mut self, assign: &StmtAnnAssign) {
        self.type_expr(&assign.annotation);
        // the value of an `X: TypeAlias = <type>` is itself a type
        if matches!(assign.annotation.as_ref(), Expr::Name(n) if n.id == "TypeAlias")
            || matches!(assign.annotation.as_ref(), Expr::Attribute(a) if a.attr.as_str() == "TypeAlias")
        {
            if let Some(value) = &assign.value {
                self.type_expr(value);
            }
        }
    }

    fn signature(&mut self, params: &Parameters) {
        let annotations = params
            .posonlyargs
            .iter()
            .chain(&params.args)
            .chain(&params.kwonlyargs)
            .map(|p| &p.parameter)
            .chain(params.vararg.iter().map(AsRef::as_ref))
            .chain(params.kwarg.iter().map(AsRef::as_ref))
            .filter_map(|p| p.annotation.as_deref());
        for annotation in annotations {
            self.type_expr(annotation);
        }
    }

    fn type_params(&mut self, type_params: Option<&ruff_python_ast::TypeParams>) {
        let Some(type_params) = type_params else {
            return;
        };
        for type_param in &type_params.type_params {
            if let TypeParam::TypeVar(tv) = type_param {
                if let Some(bound) = &tv.bound {
                    self.type_expr(bound);
                }
                if let Some(default) = &tv.default {
                    self.type_expr(default);
                }
            }
        }
    }

    /// recurse through a type expression, converting any `tuple[X, ...]`
    fn type_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Subscript(sub) => {
                if let Some(element) = homogeneous_tuple_element(sub) {
                    // whole `tuple[X, ...]` → `(*: X)`; `X` is spliced verbatim
                    // (there are no nested homogeneous tuples in the vendored
                    // typeshed, so a whole-range edit never overlaps an inner one)
                    self.edits.push(Edit {
                        start: expr.range().start().to_usize(),
                        end: expr.range().end().to_usize(),
                        replacement: format!("(*: {})", &self.source[element.range()]),
                    });
                    return;
                }
                self.type_expr(&sub.slice);
            }
            Expr::BinOp(binop) => {
                self.type_expr(&binop.left);
                self.type_expr(&binop.right);
            }
            Expr::Tuple(tuple) => {
                for elt in &tuple.elts {
                    self.type_expr(elt);
                }
            }
            Expr::List(list) => {
                for elt in &list.elts {
                    self.type_expr(elt);
                }
            }
            Expr::UnaryOp(unary) => self.type_expr(&unary.operand),
            _ => {}
        }
    }
}

/// the element `X` of a homogeneous `tuple[X, ...]`, or `None` for any other
/// subscript (heterogeneous tuples, non-`tuple` heads, ...)
fn homogeneous_tuple_element(sub: &ruff_python_ast::ExprSubscript) -> Option<&Expr> {
    if !matches!(sub.value.as_ref(), Expr::Name(n) if n.id == "tuple") {
        return None;
    }
    let Expr::Tuple(tuple) = sub.slice.as_ref() else {
        return None;
    };
    match tuple.elts.as_slice() {
        [element, Expr::EllipsisLiteral(_)] => Some(element),
        _ => None,
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
        let edits = HomogeneousTuple.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_in_signature_and_return() {
        assert_eq!(
            run("def f(x: tuple[int, ...]) -> tuple[str, ...]\n"),
            "def f(x: (*: int)) -> (*: str)\n"
        );
    }

    #[test]
    fn converts_nested_in_union_and_generic() {
        assert_eq!(
            run("x: dict[str, tuple[int, ...]] | tuple[bytes, ...]\n"),
            "x: dict[str, (*: int)] | (*: bytes)\n"
        );
    }

    #[test]
    fn converts_in_type_alias_and_type_statement() {
        assert_eq!(
            run("type _ClassInfo = type | tuple[_ClassInfo, ...]\n"),
            "type _ClassInfo = type | (*: _ClassInfo)\n"
        );
        assert_eq!(
            run("_X: TypeAlias = tuple[int, ...]\n"),
            "_X: TypeAlias = (*: int)\n"
        );
    }

    #[test]
    fn leaves_heterogeneous_tuple() {
        let src = "x: tuple[int, str]\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_two_element_tuple_without_ellipsis() {
        let src = "x: tuple[int, int]\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn idempotent_on_converted_form() {
        let src = "x: (*: int)\n";
        assert_eq!(run(src), src);
    }
}
