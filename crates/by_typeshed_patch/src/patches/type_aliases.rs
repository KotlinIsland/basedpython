//! `X: TypeAlias = V` → `type X = V` (pep 695 type-alias statement)
//!
//! only *non-generic*, module-level aliases are converted. these are left in
//! legacy form:
//!
//! - an alias whose value references a module `TypeVar`/`ParamSpec`/
//!   `TypeVarTuple` is implicitly generic over that variable; the pep 695 form
//!   would need an explicit `type X[T] = ...` parameter list to preserve that
//! - an alias used as a class base (`class C(Alias)`): a pep 695 `type` alias is
//!   a `TypeAliasType` object, which cannot be subclassed, so the class's MRO
//!   would break
//! - anything nested inside a class, which could also close over class type
//!   parameters
//!
//! self-referential (recursive) aliases like
//! `_ClassInfo = type | tuple[_ClassInfo, ...]` *are* converted — pep 695 type
//! aliases support recursion natively
//!
//! the conversion is a faithful inverse of the forward transpile, which lowers
//! `type X = V` back to `X: TypeAlias = V`

use std::collections::HashSet;
use std::path::Path;

use ruff_python_ast::token::TokenKind;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, Expr, ModModule, Stmt, StmtAnnAssign, StmtAssign};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::patches::literal_unwrap::is_y026_alias;
use crate::{Edit, Patch};

pub struct TypeAliasStatements;

impl Patch for TypeAliasStatements {
    fn name(&self) -> &'static str {
        "type-alias-statements"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let body = &parsed.syntax().body;
        let typevars = collect_typevar_names(body);
        let mut base_names = HashSet::new();
        collect_base_names(body, &mut base_names);

        let mut edits = Vec::new();
        convert_body(body, &typevars, &base_names, parsed, source, &mut edits);
        edits
    }
}

/// convert every convertible alias in `body`, descending into class bodies and
/// version guards (nested aliases live in class bodies). the exclusion sets are
/// computed once over the whole module, so a nested alias that references any
/// type parameter or is used as a base anywhere is conservatively left alone
fn convert_body(
    body: &[Stmt],
    typevars: &HashSet<&str>,
    base_names: &HashSet<&str>,
    parsed: &Parsed<ModModule>,
    source: &str,
    edits: &mut Vec<Edit>,
) {
    for stmt in body {
        match stmt {
            Stmt::AnnAssign(assign) if is_convertible_alias(assign, typevars, base_names) => {
                let target_start = assign.target.range().start().to_usize();
                let target_end = assign.target.range().end().to_usize();
                let annotation_end = assign.annotation.range().end().to_usize();
                // `NAME: TypeAlias = V` → `type NAME = V`
                edits.push(Edit {
                    start: target_start,
                    end: target_start,
                    replacement: "type ".to_string(),
                });
                edits.push(Edit {
                    start: target_end,
                    end: annotation_end,
                    replacement: String::new(),
                });
            }
            // a `# noqa: Y026` plain assignment is a type alias upstream could
            // not spell with `TypeAlias` (an old mypy bug); basedpython can —
            // `NAME = V  # noqa: Y026` → `type NAME = V`, comment dropped
            Stmt::Assign(assign) if is_y026_alias(assign, source) => {
                let Some(target) = convertible_plain_alias_target(assign, typevars, base_names)
                else {
                    continue;
                };
                let target_start = target.range().start().to_usize();
                edits.push(Edit {
                    start: target_start,
                    end: target_start,
                    replacement: "type ".to_string(),
                });
                if let Some(comment) = delete_y026_comment(assign.range(), parsed, source) {
                    edits.push(comment);
                }
            }
            Stmt::ClassDef(class) => {
                convert_body(&class.body, typevars, base_names, parsed, source, edits);
            }
            Stmt::If(node) => {
                convert_body(&node.body, typevars, base_names, parsed, source, edits);
                for clause in &node.elif_else_clauses {
                    convert_body(&clause.body, typevars, base_names, parsed, source, edits);
                }
            }
            Stmt::Try(node) => {
                convert_body(&node.body, typevars, base_names, parsed, source, edits);
                for handler in &node.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    convert_body(&h.body, typevars, base_names, parsed, source, edits);
                }
                convert_body(&node.orelse, typevars, base_names, parsed, source, edits);
                convert_body(&node.finalbody, typevars, base_names, parsed, source, edits);
            }
            Stmt::With(node) => {
                convert_body(&node.body, typevars, base_names, parsed, source, edits);
            }
            _ => {}
        }
    }
}

/// a `TypeAlias` annotation, whether bare or qualified (`typing.TypeAlias` /
/// `typing_extensions.TypeAlias`)
fn is_type_alias_annotation(expr: &Expr) -> bool {
    match expr {
        Expr::Name(n) => n.id == "TypeAlias",
        Expr::Attribute(attr) => attr.attr.as_str() == "TypeAlias",
        _ => false,
    }
}

/// the target of a `# noqa: Y026` plain alias `NAME = V` when it is convertible
/// on the same terms as a `TypeAlias` one (single name target, non-generic, not a
/// base, not self-referential); `None` otherwise
fn convertible_plain_alias_target<'a>(
    assign: &'a StmtAssign,
    typevars: &HashSet<&str>,
    base_names: &HashSet<&str>,
) -> Option<&'a ast::ExprName> {
    let [Expr::Name(target)] = assign.targets.as_slice() else {
        return None;
    };
    if base_names.contains(target.id.as_str()) || references_any(&assign.value, typevars) {
        return None;
    }
    Some(target)
}

/// deletion edit for the `# noqa: Y026 ...` comment on any physical line the
/// statement `range` spans (the comment may trail the value or sit mid-statement
/// in a parenthesised value), plus the whitespace before it
fn delete_y026_comment(range: TextRange, parsed: &Parsed<ModModule>, source: &str) -> Option<Edit> {
    let bytes = source.as_bytes();
    let mut span_start = range.start().to_usize();
    while span_start > 0 && bytes[span_start - 1] != b'\n' {
        span_start -= 1;
    }
    let mut span_end = range.end().to_usize();
    while span_end < bytes.len() && bytes[span_end] != b'\n' {
        span_end += 1;
    }
    for token in parsed.tokens() {
        let t = token.range();
        if token.kind() == TokenKind::Comment
            && t.start().to_usize() >= span_start
            && t.end().to_usize() <= span_end
            && source[t].contains("noqa: Y026")
        {
            let mut start = t.start().to_usize();
            while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
                start -= 1;
            }
            return Some(Edit {
                start,
                end: t.end().to_usize(),
                replacement: String::new(),
            });
        }
    }
    None
}

/// a top-level `NAME: TypeAlias = V` that is not generic over any module typevar
/// and is not used as a class base
fn is_convertible_alias(
    assign: &StmtAnnAssign,
    typevars: &HashSet<&str>,
    base_names: &HashSet<&str>,
) -> bool {
    let Some(value) = &assign.value else {
        return false;
    };
    let Some(target) = assign.target.as_name_expr() else {
        return false;
    };
    if !is_type_alias_annotation(&assign.annotation) {
        return false;
    }
    // recursive aliases (`_ClassInfo = type | tuple[_ClassInfo, ...]`) are fine —
    // pep 695 supports recursion — so only generic and base-used aliases are left
    !base_names.contains(target.id.as_str()) && !references_any(value, typevars)
}

/// names used as the head of a class base (`class C(Alias)` or `class C(Alias[T])`),
/// which therefore cannot become a pep 695 `type` alias
fn collect_base_names<'a>(body: &'a [Stmt], out: &mut HashSet<&'a str>) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class) => {
                if let Some(arguments) = &class.arguments {
                    for base in &arguments.args {
                        if let Some(name) = base_head_name(base) {
                            out.insert(name);
                        }
                    }
                }
                collect_base_names(&class.body, out);
            }
            Stmt::If(node) => {
                collect_base_names(&node.body, out);
                for clause in &node.elif_else_clauses {
                    collect_base_names(&clause.body, out);
                }
            }
            Stmt::Try(node) => {
                collect_base_names(&node.body, out);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_base_names(&h.body, out);
                }
                collect_base_names(&node.orelse, out);
                collect_base_names(&node.finalbody, out);
            }
            Stmt::With(node) => collect_base_names(&node.body, out),
            _ => {}
        }
    }
}

/// the head name of a base expression: `A` for `A`, `A` for `A[T]`
fn base_head_name(base: &Expr) -> Option<&str> {
    match base {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Subscript(sub) => base_head_name(&sub.value),
        _ => None,
    }
}

/// every name that could be a type parameter in scope: module-level
/// `TypeVar`/`ParamSpec`/`TypeVarTuple` assignments plus pep 695 type-parameter
/// names on any class/def header. an alias whose value references one of these is
/// generic and must not collapse to a bare `type X = ...`. descends through
/// version guards and into class/def bodies (conservatively — an extra name only
/// ever spares an alias from conversion)
fn collect_typevar_names(body: &[Stmt]) -> HashSet<&str> {
    let mut names = HashSet::new();
    collect_typevars_into(body, &mut names);
    names
}

fn collect_typevars_into<'a>(body: &'a [Stmt], names: &mut HashSet<&'a str>) {
    for stmt in body {
        match stmt {
            Stmt::Assign(assign) => {
                if let [Expr::Name(target)] = assign.targets.as_slice()
                    && let Expr::Call(call) = &*assign.value
                    && matches!(
                        callee_name(&call.func),
                        Some("TypeVar" | "ParamSpec" | "TypeVarTuple")
                    )
                {
                    names.insert(target.id.as_str());
                }
            }
            Stmt::ClassDef(class) => {
                collect_type_params(class.type_params.as_deref(), names);
                collect_typevars_into(&class.body, names);
            }
            Stmt::FunctionDef(func) => {
                collect_type_params(func.type_params.as_deref(), names);
                collect_typevars_into(&func.body, names);
            }
            Stmt::If(node) => {
                collect_typevars_into(&node.body, names);
                for clause in &node.elif_else_clauses {
                    collect_typevars_into(&clause.body, names);
                }
            }
            Stmt::Try(node) => {
                collect_typevars_into(&node.body, names);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_typevars_into(&h.body, names);
                }
                collect_typevars_into(&node.orelse, names);
                collect_typevars_into(&node.finalbody, names);
            }
            Stmt::With(node) => collect_typevars_into(&node.body, names),
            _ => {}
        }
    }
}

fn collect_type_params<'a>(type_params: Option<&'a ast::TypeParams>, names: &mut HashSet<&'a str>) {
    if let Some(type_params) = type_params {
        for tp in type_params {
            names.insert(tp.name().as_str());
        }
    }
}

fn callee_name(func: &Expr) -> Option<&str> {
    match func {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        _ => None,
    }
}

/// whether `expr` loads any name in `names`
fn references_any(expr: &Expr, names: &HashSet<&str>) -> bool {
    struct Refs<'a> {
        names: &'a HashSet<&'a str>,
        hit: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Refs<'_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.ctx.is_load()
                && self.names.contains(name.id.as_str())
            {
                self.hit = true;
            }
            if !self.hit {
                walk_expr(self, expr);
            }
        }
    }
    let mut refs = Refs { names, hit: false };
    refs.visit_expr(expr);
    refs.hit
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = TypeAliasStatements.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_non_generic_alias() {
        assert_eq!(
            run("_VersionInfo: TypeAlias = (int, int, int, str, int)\n"),
            "type _VersionInfo = (int, int, int, str, int)\n"
        );
    }

    #[test]
    fn converts_union_alias() {
        assert_eq!(
            run("_TaskYieldType: TypeAlias = Future[object] | None\n"),
            "type _TaskYieldType = Future[object] | None\n"
        );
    }

    #[test]
    fn leaves_generic_alias_untouched() {
        let src = "\
_T_co = TypeVar(\"_T_co\", covariant=True)
_Coro: TypeAlias = Coroutine[Any, Any, _T_co]
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_plain_assignment_untouched() {
        let src = "_LiteralInteger = _PositiveInteger | 0\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_alias_over_pep695_header_typevar() {
        // `T` is a type parameter on a class header; an alias mentioning it is
        // generic and must not collapse to a bare `type X = ...` with a free `T`
        let src = "\
class C[T]:
    x: T
_Elems: TypeAlias = list[T]
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_alias_used_as_base_class() {
        // a `type` alias is a TypeAliasType object and cannot be subclassed
        let src = "\
_Base: TypeAlias = structseq[Any]
final class V(_Base, tuple[int, int]): ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn converts_recursive_alias() {
        // pep 695 type aliases support recursion natively
        assert_eq!(
            run("_ClassInfo: TypeAlias = type | tuple[_ClassInfo, ...]\n"),
            "type _ClassInfo = type | tuple[_ClassInfo, ...]\n"
        );
    }

    #[test]
    fn converts_y026_plain_alias_and_drops_comment() {
        assert_eq!(
            run("_LiteralInteger = _Pos | _Neg | 0  # noqa: Y026  # TODO: fix\n"),
            "type _LiteralInteger = _Pos | _Neg | 0\n"
        );
    }

    #[test]
    fn leaves_plain_assign_without_y026() {
        let src = "SENTINEL = object()\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_generic_base_alias() {
        let src = "\
_Base: TypeAlias = Mapping[str, int]
class C(_Base[str]): ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn idempotent() {
        let src = "type _VersionInfo = (int, int)\n";
        assert_eq!(run(src), src);
    }
}
