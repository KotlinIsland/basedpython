//! `Callable[[A, B], R]` → `(A, B) -> R`
//!
//! basedpython's arrow callable syntax is the denotable form of
//! `typing.Callable`. only the denotable shapes are converted:
//!
//! - `Callable[[A, B], R]` → `(A, B) -> R`
//! - `Callable[[], R]` → `() -> R`
//! - `Callable[..., R]` → `(...) -> R`
//! - `Callable[P, R]` (a `ParamSpec`) → `(**P) -> R`
//! - `Callable[Concatenate[T1, …, P], R]` → `(T1, …, **P) -> R`
//!
//! a `Callable[[Unpack[Ts]], R]` (a `TypeVarTuple`) has no denotable arrow form
//! and is left as `Callable`. the conversion recurses, so nested callables (in
//! parameters, returns, unions) convert too. a return type that is a union or
//! itself an arrow is parenthesised, since `->` binds tighter than `|` and is
//! right-associative

use std::path::Path;

use ruff_python_ast::{
    Expr, ExprSubscript, ModModule, Parameters, Stmt, StmtAnnAssign, StmtClassDef, StmtFunctionDef,
    TypeParam,
};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

pub struct ArrowCallable;

impl Patch for ArrowCallable {
    fn name(&self) -> &'static str {
        "arrow-callable"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        walk_body(&parsed.syntax().body, source, &mut edits);
        edits
    }
}

fn walk_body(body: &[Stmt], source: &str, edits: &mut Vec<Edit>) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => function(func, source, edits),
            Stmt::ClassDef(class) => class_def(class, source, edits),
            Stmt::AnnAssign(assign) => ann_assign(assign, source, edits),
            Stmt::TypeAlias(alias) => type_expr(&alias.value, false, source, edits),
            Stmt::If(node) => {
                walk_body(&node.body, source, edits);
                for clause in &node.elif_else_clauses {
                    walk_body(&clause.body, source, edits);
                }
            }
            Stmt::Try(node) => {
                walk_body(&node.body, source, edits);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk_body(&h.body, source, edits);
                }
                walk_body(&node.orelse, source, edits);
                walk_body(&node.finalbody, source, edits);
            }
            Stmt::With(node) => walk_body(&node.body, source, edits),
            _ => {}
        }
    }
}

fn function(func: &StmtFunctionDef, source: &str, edits: &mut Vec<Edit>) {
    type_params(func.type_params.as_deref(), source, edits);
    signature(&func.parameters, source, edits);
    if let Some(returns) = &func.returns {
        type_expr(returns, false, source, edits);
    }
    walk_body(&func.body, source, edits);
}

fn class_def(class: &StmtClassDef, source: &str, edits: &mut Vec<Edit>) {
    type_params(class.type_params.as_deref(), source, edits);
    if let Some(arguments) = &class.arguments {
        for base in &arguments.args {
            type_expr(base, false, source, edits);
        }
    }
    walk_body(&class.body, source, edits);
}

fn ann_assign(assign: &StmtAnnAssign, source: &str, edits: &mut Vec<Edit>) {
    type_expr(&assign.annotation, false, source, edits);
    // the value of an `X: TypeAlias = <type>` (bare or qualified
    // `typing_extensions.TypeAlias`) is itself a type
    let is_type_alias = matches!(assign.annotation.as_ref(), Expr::Name(n) if n.id == "TypeAlias")
        || matches!(assign.annotation.as_ref(), Expr::Attribute(a) if a.attr.as_str() == "TypeAlias");
    if is_type_alias && let Some(value) = &assign.value {
        type_expr(value, false, source, edits);
    }
}

fn signature(params: &Parameters, source: &str, edits: &mut Vec<Edit>) {
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
        type_expr(annotation, false, source, edits);
    }
}

fn type_params(
    type_params: Option<&ruff_python_ast::TypeParams>,
    source: &str,
    edits: &mut Vec<Edit>,
) {
    let Some(type_params) = type_params else {
        return;
    };
    for type_param in &type_params.type_params {
        if let TypeParam::TypeVar(tv) = type_param {
            if let Some(bound) = &tv.bound {
                type_expr(bound, false, source, edits);
            }
            if let Some(default) = &tv.default {
                type_expr(default, false, source, edits);
            }
        }
    }
}

/// emit an edit for each outermost convertible `Callable[...]` within a type
/// expression; recurse through non-callable structure to find nested ones.
/// `wrap` is set when `expr` sits where a bare arrow would parse wrongly (a
/// `|`/`&` operand), so the arrow must be parenthesised
fn type_expr(expr: &Expr, wrap: bool, source: &str, edits: &mut Vec<Edit>) {
    if let Expr::Subscript(sub) = expr
        && callable_parts(sub).is_some()
    {
        let arrow = render(expr, source);
        edits.push(Edit {
            start: expr.range().start().to_usize(),
            end: expr.range().end().to_usize(),
            replacement: if wrap { format!("({arrow})") } else { arrow },
        });
        return;
    }
    // not a convertible callable — descend into children
    match expr {
        Expr::Subscript(sub) => type_expr(&sub.slice, false, source, edits),
        Expr::BinOp(binop) => {
            // an arrow operand of `|`/`&` needs parentheses
            type_expr(&binop.left, true, source, edits);
            type_expr(&binop.right, true, source, edits);
        }
        Expr::Tuple(tuple) => {
            for elt in &tuple.elts {
                type_expr(elt, false, source, edits);
            }
        }
        Expr::List(list) => {
            for elt in &list.elts {
                type_expr(elt, false, source, edits);
            }
        }
        Expr::UnaryOp(unary) => type_expr(&unary.operand, true, source, edits),
        _ => {}
    }
}

/// the parameter shape of a convertible `Callable[...]`, narrowed so callers
/// never have to re-match an already-validated form
enum CallableParams<'a> {
    /// `Callable[..., R]`
    Ellipsis,
    /// `Callable[[A, B], R]` — the denotable element list
    List(&'a [Expr]),
    /// `Callable[P, R]` — a bare `ParamSpec` → `(**P)`
    ParamSpec(&'a Expr),
    /// `Callable[Concatenate[T1, …, P], R]` → `(T1, …, **P)`: the prefix types
    /// and the trailing `ParamSpec`, split so `render` needs no re-matching
    Concatenate(&'a [Expr], &'a Expr),
}

/// `(params, return)` if `sub` is a convertible `Callable[...]`
fn callable_parts(sub: &ExprSubscript) -> Option<(CallableParams<'_>, &Expr)> {
    if subscript_head(&sub.value) != Some("Callable") {
        return None;
    }
    let Expr::Tuple(tuple) = &*sub.slice else {
        return None;
    };
    let [params, ret] = tuple.elts.as_slice() else {
        return None;
    };
    // parameters must be a denotable list, a bare `...`, a `ParamSpec`, or a
    // `Concatenate`. a list containing a variadic unpack (`Unpack[Ts]` / `*Ts`)
    // is NOT denotable as a plain arrow parameter list — `(Unpack[Ts])` would
    // read as one positional parameter, not a variadic — so those stay `Callable`
    let params = match params {
        Expr::EllipsisLiteral(_) => CallableParams::Ellipsis,
        Expr::List(list) if !list.elts.iter().any(is_variadic) => CallableParams::List(&list.elts),
        // a bare name in first position is a `ParamSpec`
        Expr::Name(_) => CallableParams::ParamSpec(params),
        Expr::Subscript(concat) if subscript_head(&concat.value) == Some("Concatenate") => {
            let Expr::Tuple(args) = concat.slice.as_ref() else {
                return None;
            };
            // the last argument must be a bare `ParamSpec` name; a gradual
            // `Concatenate[T, ...]` has no denotable arrow form
            match args.elts.split_last() {
                Some((last @ Expr::Name(_), prefix)) if !prefix.is_empty() => {
                    CallableParams::Concatenate(prefix, last)
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some((params, ret))
}

/// `Unpack[...]` or a starred `*Ts` element
fn is_variadic(elt: &Expr) -> bool {
    match elt {
        Expr::Starred(_) => true,
        Expr::Subscript(sub) => subscript_head(&sub.value) == Some("Unpack"),
        _ => false,
    }
}

/// render a type expression, converting every convertible callable within it
fn render(expr: &Expr, source: &str) -> String {
    if let Expr::Subscript(sub) = expr
        && let Some((params, ret)) = callable_parts(sub)
    {
        let params_text = match params {
            CallableParams::List(elts) => {
                let inner: Vec<String> = elts.iter().map(|e| render(e, source)).collect();
                format!("({})", inner.join(", "))
            }
            CallableParams::Ellipsis => "(...)".to_string(),
            // `Callable[P, R]` → `(**P)`
            CallableParams::ParamSpec(p) => format!("(**{})", render(p, source)),
            // `Callable[Concatenate[T1, …, P], R]` → `(T1, …, **P)`
            CallableParams::Concatenate(prefix, paramspec) => {
                let mut parts: Vec<String> = prefix.iter().map(|e| render(e, source)).collect();
                parts.push(format!("**{}", render(paramspec, source)));
                format!("({})", parts.join(", "))
            }
        };
        let mut ret_text = render(ret, source);
        if needs_return_parens(ret) {
            ret_text = format!("({ret_text})");
        }
        return format!("{params_text} -> {ret_text}");
    }

    // reconstruct from source, splicing any nested convertible callables
    let mut hits: Vec<(TextRange, String)> = Vec::new();
    collect_outermost(expr, false, source, &mut hits);
    splice(source, expr.range(), &hits)
}

/// a return type that is a union/intersection or itself a callable-arrow must be
/// parenthesised (`->` binds tighter than `|` and is right-associative)
fn needs_return_parens(ret: &Expr) -> bool {
    match ret {
        Expr::BinOp(_) => true,
        Expr::Subscript(sub) => callable_parts(sub).is_some(),
        _ => false,
    }
}

/// collect the convertible callables inside `expr`, each paired with its
/// rendered arrow (parenthesised when it is a `|`/`&` operand). mirrors the
/// `wrap` logic of [`type_expr`]
fn collect_outermost(expr: &Expr, wrap: bool, source: &str, hits: &mut Vec<(TextRange, String)>) {
    if let Expr::Subscript(sub) = expr
        && callable_parts(sub).is_some()
    {
        let arrow = render(expr, source);
        hits.push((
            expr.range(),
            if wrap { format!("({arrow})") } else { arrow },
        ));
        return;
    }
    match expr {
        Expr::Subscript(sub) => collect_outermost(&sub.slice, false, source, hits),
        Expr::BinOp(binop) => {
            collect_outermost(&binop.left, true, source, hits);
            collect_outermost(&binop.right, true, source, hits);
        }
        Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .for_each(|e| collect_outermost(e, false, source, hits)),
        Expr::List(list) => list
            .elts
            .iter()
            .for_each(|e| collect_outermost(e, false, source, hits)),
        Expr::UnaryOp(unary) => collect_outermost(&unary.operand, true, source, hits),
        _ => {}
    }
}

/// splice `hits` (disjoint sub-ranges → replacements) into `source[range]`
fn splice(source: &str, range: TextRange, hits: &[(TextRange, String)]) -> String {
    let base = range.start().to_usize();
    let slice = &source[range];
    let mut sorted: Vec<&(TextRange, String)> = hits.iter().collect();
    sorted.sort_by_key(|(r, _)| r.start());
    let mut out = String::new();
    let mut cursor = 0;
    for (r, replacement) in sorted {
        let start = r.start().to_usize() - base;
        let end = r.end().to_usize() - base;
        out.push_str(&slice[cursor..start]);
        out.push_str(replacement);
        cursor = end;
    }
    out.push_str(&slice[cursor..]);
    out
}

fn subscript_head(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
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
        let edits = ArrowCallable.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_simple() {
        assert_eq!(
            run("f: Callable[[int, str], bool]\n"),
            "f: (int, str) -> bool\n"
        );
    }

    #[test]
    fn converts_no_params() {
        assert_eq!(run("f: Callable[[], None]\n"), "f: () -> None\n");
    }

    #[test]
    fn converts_ellipsis() {
        assert_eq!(run("f: Callable[..., int]\n"), "f: (...) -> int\n");
    }

    #[test]
    fn parenthesises_union_return() {
        assert_eq!(
            run("f: Callable[[int], str | None]\n"),
            "f: (int) -> (str | None)\n"
        );
    }

    #[test]
    fn converts_nested_in_union() {
        // the arrow is a `|` operand, so it must be parenthesised
        assert_eq!(
            run("f: Callable[[int], str] | None\n"),
            "f: ((int) -> str) | None\n"
        );
    }

    #[test]
    fn converts_nested_callable_param() {
        assert_eq!(
            run("f: Callable[[Callable[[int], str]], bool]\n"),
            "f: ((int) -> str) -> bool\n"
        );
    }

    #[test]
    fn converts_nested_callable_return() {
        assert_eq!(
            run("f: Callable[[int], Callable[[str], bool]]\n"),
            "f: (int) -> ((str) -> bool)\n"
        );
    }

    #[test]
    fn converts_inside_generic() {
        assert_eq!(
            run("f: dict[str, Callable[[int], str]]\n"),
            "f: dict[str, (int) -> str]\n"
        );
    }

    #[test]
    fn converts_paramspec_callable() {
        assert_eq!(run("f: Callable[P, int]\n"), "f: (**P) -> int\n");
    }

    #[test]
    fn converts_concatenate_callable() {
        assert_eq!(
            run("f: Callable[Concatenate[int, str, P], bool]\n"),
            "f: (int, str, **P) -> bool\n"
        );
    }

    #[test]
    fn leaves_gradual_concatenate() {
        // `Concatenate[T, ...]` has no denotable arrow form (the tail is `...`)
        let src = "f: Callable[Concatenate[int, ...], str]\n";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_variadic_param_list() {
        // `[Unpack[Ts]]` is variadic; `(Unpack[Ts])` would read as one parameter
        assert_eq!(
            run("f: Callable[[Unpack[Ts]], int]\n"),
            "f: Callable[[Unpack[Ts]], int]\n"
        );
    }

    #[test]
    fn converts_in_signature() {
        let src = "def f(cb: Callable[[int], None]) -> Callable[[], int]: ...\n";
        let expected = "def f(cb: (int) -> None) -> () -> int: ...\n";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn idempotent_on_arrow_form() {
        // an already-converted arrow is an `ExprCallableType`, not a
        // `Callable[...]` subscript, so a second pass leaves it untouched
        let src = "def f(cb: (int) -> None) -> () -> int: ...\n";
        assert_eq!(run(src), src);
    }
}
