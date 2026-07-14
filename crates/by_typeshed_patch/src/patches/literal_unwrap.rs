//! `Literal[a, b, ...]` → `a | b | ...` in type positions
//!
//! basedpython reads a bare literal in type position as its `Literal` type, so
//! the explicit `Literal[...]` wrapper is redundant. this is the inverse of the
//! forward transpile, which re-wraps bare literals into `Literal[...]`
//!
//! bare literals include str/bytes/number/bool/None and enum members
//! (`Literal[Color.RED]` → `Color.RED` — a `Literal` argument that is a dotted
//! name is an enum member per PEP 586, which basedpython reads bare too).
//!
//! `Literal[...]` sitting in `Annotated[...]` metadata position is left alone —
//! it is a value rather than a type

use std::path::Path;

use ruff_python_ast::{
    Expr, ModModule, Parameters, Stmt, StmtAnnAssign, StmtAssign, StmtClassDef, StmtFunctionDef,
    TypeParam, UnaryOp,
};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct UnwrapLiteral;

impl Patch for UnwrapLiteral {
    fn name(&self) -> &'static str {
        "unwrap-literal"
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
                // a `# noqa: Y026` plain assignment is a type alias that couldn't
                // use `TypeAlias` (an old mypy limitation), so its value is a type
                Stmt::Assign(assign) if is_y026_alias(assign, self.source) => {
                    self.type_expr(&assign.value);
                }
                // a pep 695 `type X = V` statement: the value is a type
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
        // the value of an explicit `X: TypeAlias = <type>` is itself a type
        if matches!(assign.annotation.as_ref(), Expr::Name(n) if n.id == "TypeAlias")
            && let Some(value) = &assign.value
        {
            self.type_expr(value);
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

    /// recurse through a type expression, unwrapping any `Literal[...]`
    fn type_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Subscript(sub) => match subscript_head(&sub.value) {
                Some("Literal") => {
                    if let Some(replacement) = self.unwrap_literal(&sub.slice) {
                        self.edits.push(Edit {
                            start: expr.range().start().to_usize(),
                            end: expr.range().end().to_usize(),
                            replacement,
                        });
                    }
                    // either unwrapped wholesale, or left intact — do not descend
                }
                Some("Annotated") => {
                    // only the first argument is a type; the rest is metadata
                    if let Expr::Tuple(tuple) = &*sub.slice
                        && let Some(first) = tuple.elts.first()
                    {
                        self.type_expr(first);
                    }
                }
                _ => self.type_expr(&sub.slice),
            },
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

    /// render the `Literal[...]` argument list as `a | b | ...`, or `None` if any
    /// argument is not a bare literal
    fn unwrap_literal(&self, slice: &Expr) -> Option<String> {
        let elements: Vec<&Expr> = match slice {
            Expr::Tuple(tuple) => tuple.elts.iter().collect(),
            single => vec![single],
        };
        if elements.is_empty() || !elements.iter().all(|e| is_bare_literal(e)) {
            return None;
        }
        Some(
            elements
                .iter()
                .map(|e| &self.source[e.range()])
                .collect::<Vec<_>>()
                .join(" | "),
        )
    }
}

/// whether `expr` is a literal basedpython reads bare in type position
fn is_bare_literal(expr: &Expr) -> bool {
    match expr {
        Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_) => true,
        // signed numeric literals: `-1`, `+2`
        Expr::UnaryOp(unary) => {
            matches!(unary.op, UnaryOp::USub | UnaryOp::UAdd)
                && matches!(&*unary.operand, Expr::NumberLiteral(_))
        }
        // an enum member: a dotted name. a `Literal` argument that is a dotted
        // name is an enum member (PEP 586), which basedpython reads bare
        Expr::Attribute(attr) => is_dotted_name(&attr.value),
        _ => false,
    }
}

/// whether `expr` is a (possibly dotted) name, e.g. `E` or `pkg.mod.E`
fn is_dotted_name(expr: &Expr) -> bool {
    match expr {
        Expr::Name(_) => true,
        Expr::Attribute(attr) => is_dotted_name(&attr.value),
        _ => false,
    }
}

fn subscript_head(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        _ => None,
    }
}

/// a single-target plain assignment carrying a `# noqa: Y026` — flake8-pyi's
/// "this is a type alias that should use `TypeAlias`". the marker may sit
/// mid-statement in a parenthesised value, so scan the whole physical span
pub(crate) fn is_y026_alias(assign: &StmtAssign, source: &str) -> bool {
    if !matches!(assign.targets.as_slice(), [Expr::Name(_)]) {
        return false;
    }
    let bytes = source.as_bytes();
    let mut start = assign.range().start().to_usize();
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = assign.range().end().to_usize();
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    source[start..end].contains("noqa: Y026")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = UnwrapLiteral.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn unwraps_single_literal() {
        assert_eq!(
            run("def imag(self) -> Literal[0]\n"),
            "def imag(self) -> 0\n"
        );
    }

    #[test]
    fn unwraps_multi_literal_to_union() {
        assert_eq!(
            run("def f(self, x: Literal[\"little\", \"big\"]) -> None\n"),
            "def f(self, x: \"little\" | \"big\") -> None\n"
        );
    }

    #[test]
    fn unwraps_negative_and_bool() {
        assert_eq!(run("x: Literal[-1, True]\n"), "x: -1 | True\n");
    }

    #[test]
    fn unwraps_nested_in_generic_and_union() {
        assert_eq!(
            run("x: dict[str, Literal[1, 2]] | Literal[0]\n"),
            "x: dict[str, 1 | 2] | 0\n"
        );
    }

    #[test]
    fn unwraps_enum_member_literal() {
        assert_eq!(run("x: Literal[Color.RED]\n"), "x: Color.RED\n");
        assert_eq!(
            run("x: Literal[_MISSING_TYPE.MISSING, Color.RED]\n"),
            "x: _MISSING_TYPE.MISSING | Color.RED\n"
        );
    }

    #[test]
    fn leaves_non_dotted_subscript_literal() {
        // a subscripted value in Literal isn't a bare enum member
        assert_eq!(run("x: Literal[foo[0]]\n"), "x: Literal[foo[0]]\n");
    }

    #[test]
    fn leaves_literal_in_annotated_metadata() {
        assert_eq!(
            run("x: Annotated[int, Literal[0]]\n"),
            "x: Annotated[int, Literal[0]]\n"
        );
    }

    #[test]
    fn unwraps_type_in_annotated_but_not_metadata() {
        assert_eq!(
            run("x: Annotated[Literal[1, 2], Literal[0]]\n"),
            "x: Annotated[1 | 2, Literal[0]]\n"
        );
    }

    #[test]
    fn unwraps_in_typealias_value() {
        assert_eq!(
            run("_E: TypeAlias = Literal[\"a\", \"b\"]\n"),
            "_E: TypeAlias = \"a\" | \"b\"\n"
        );
    }

    #[test]
    fn unwraps_y026_plain_alias() {
        assert_eq!(
            run("_LiteralInteger = _Pos | _Neg | Literal[0]  # noqa: Y026\n"),
            "_LiteralInteger = _Pos | _Neg | 0  # noqa: Y026\n"
        );
    }

    #[test]
    fn leaves_plain_assign_without_y026() {
        // an ordinary assignment value is not a type position
        let src = "SENTINEL = Literal[0]\n";
        assert_eq!(run(src), src);
    }
}
