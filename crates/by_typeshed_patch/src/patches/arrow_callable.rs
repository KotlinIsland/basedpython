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
//! - `Callable[[Unpack[Ts]], R]` (a `TypeVarTuple`) → `(*Ts) -> R`
//!
//! the conversion recurses, so nested callables (in parameters, returns, unions)
//! convert too. a return type that is a union or itself an arrow is
//! parenthesised, since `->` binds tighter than `|` and is right-associative

use std::collections::HashSet;
use std::path::Path;

use ruff_python_ast::{
    Expr, ExprSubscript, ModModule, Parameters, Stmt, StmtAnnAssign, StmtClassDef, StmtFunctionDef,
    TypeParam,
};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

/// what rendering an arrow needs: the module source, plus the names the module
/// declares as `TypeVarTuple`s (see [`collect_type_var_tuples`])
struct Ctx<'a> {
    source: &'a str,
    type_var_tuples: HashSet<&'a str>,
}

pub struct ArrowCallable;

impl Patch for ArrowCallable {
    fn name(&self) -> &'static str {
        "arrow-callable"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let body = &parsed.syntax().body;
        let mut type_var_tuples = HashSet::new();
        collect_type_var_tuples(body, &mut type_var_tuples);
        let ctx = Ctx {
            source,
            type_var_tuples,
        };
        let mut edits = Vec::new();
        walk_body(body, &ctx, &mut edits);
        edits
    }
}

/// every name the module declares as a `TypeVarTuple`: a pep 695 `*Ts` type
/// parameter or a legacy `Ts = TypeVarTuple(...)` assignment.
///
/// `Unpack[N]` is written `*N` inside an arrow only for such an `N`. for
/// anything else — a `TypedDict` kwargs unpack, a tuple alias — `*N` would read
/// as a homogeneous variadic annotated `N` instead, so the `Unpack` stays
fn collect_type_var_tuples<'a>(body: &'a [Stmt], out: &mut HashSet<&'a str>) {
    for stmt in body {
        let (type_params, nested) = match stmt {
            Stmt::FunctionDef(func) => (func.type_params.as_deref(), Some(&func.body)),
            Stmt::ClassDef(class) => (class.type_params.as_deref(), Some(&class.body)),
            Stmt::TypeAlias(alias) => (alias.type_params.as_deref(), None),
            Stmt::Assign(assign) => {
                if let Expr::Call(call) = assign.value.as_ref()
                    && subscript_head(&call.func) == Some("TypeVarTuple")
                    && let [Expr::Name(target)] = assign.targets.as_slice()
                {
                    out.insert(target.id.as_str());
                }
                (None, None)
            }
            Stmt::If(node) => {
                collect_type_var_tuples(&node.body, out);
                for clause in &node.elif_else_clauses {
                    collect_type_var_tuples(&clause.body, out);
                }
                (None, None)
            }
            Stmt::Try(node) => {
                collect_type_var_tuples(&node.body, out);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_type_var_tuples(&h.body, out);
                }
                collect_type_var_tuples(&node.orelse, out);
                collect_type_var_tuples(&node.finalbody, out);
                (None, None)
            }
            Stmt::With(node) => {
                collect_type_var_tuples(&node.body, out);
                (None, None)
            }
            _ => (None, None),
        };
        if let Some(type_params) = type_params {
            for type_param in &type_params.type_params {
                if let TypeParam::TypeVarTuple(tvt) = type_param {
                    out.insert(tvt.name.id.as_str());
                }
            }
        }
        if let Some(nested) = nested {
            collect_type_var_tuples(nested, out);
        }
    }
}

fn walk_body(body: &[Stmt], ctx: &Ctx, edits: &mut Vec<Edit>) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => function(func, ctx, edits),
            Stmt::ClassDef(class) => class_def(class, ctx, edits),
            Stmt::AnnAssign(assign) => ann_assign(assign, ctx, edits),
            Stmt::TypeAlias(alias) => type_expr(&alias.value, false, ctx, edits),
            Stmt::If(node) => {
                walk_body(&node.body, ctx, edits);
                for clause in &node.elif_else_clauses {
                    walk_body(&clause.body, ctx, edits);
                }
            }
            Stmt::Try(node) => {
                walk_body(&node.body, ctx, edits);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk_body(&h.body, ctx, edits);
                }
                walk_body(&node.orelse, ctx, edits);
                walk_body(&node.finalbody, ctx, edits);
            }
            Stmt::With(node) => walk_body(&node.body, ctx, edits),
            _ => {}
        }
    }
}

fn function(func: &StmtFunctionDef, ctx: &Ctx, edits: &mut Vec<Edit>) {
    type_params(func.type_params.as_deref(), ctx, edits);
    signature(&func.parameters, ctx, edits);
    if let Some(returns) = &func.returns {
        type_expr(returns, false, ctx, edits);
    }
    walk_body(&func.body, ctx, edits);
}

fn class_def(class: &StmtClassDef, ctx: &Ctx, edits: &mut Vec<Edit>) {
    type_params(class.type_params.as_deref(), ctx, edits);
    if let Some(arguments) = &class.arguments {
        for base in &arguments.args {
            type_expr(base, false, ctx, edits);
        }
    }
    walk_body(&class.body, ctx, edits);
}

fn ann_assign(assign: &StmtAnnAssign, ctx: &Ctx, edits: &mut Vec<Edit>) {
    type_expr(&assign.annotation, false, ctx, edits);
    // the value of an `X: TypeAlias = <type>` (bare or qualified
    // `typing_extensions.TypeAlias`) is itself a type
    let is_type_alias = matches!(assign.annotation.as_ref(), Expr::Name(n) if n.id == "TypeAlias")
        || matches!(assign.annotation.as_ref(), Expr::Attribute(a) if a.attr.as_str() == "TypeAlias");
    if is_type_alias && let Some(value) = &assign.value {
        type_expr(value, false, ctx, edits);
    }
}

fn signature(params: &Parameters, ctx: &Ctx, edits: &mut Vec<Edit>) {
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
        type_expr(annotation, false, ctx, edits);
    }
}

fn type_params(
    type_params: Option<&ruff_python_ast::TypeParams>,
    ctx: &Ctx,
    edits: &mut Vec<Edit>,
) {
    let Some(type_params) = type_params else {
        return;
    };
    for type_param in &type_params.type_params {
        if let TypeParam::TypeVar(tv) = type_param {
            if let Some(bound) = &tv.bound {
                type_expr(bound, false, ctx, edits);
            }
            if let Some(default) = &tv.default {
                type_expr(default, false, ctx, edits);
            }
        }
    }
}

/// emit an edit for each outermost convertible `Callable[...]` within a type
/// expression; recurse through non-callable structure to find nested ones.
/// `wrap` is set when `expr` sits where a bare arrow would parse wrongly (a
/// `|`/`&` operand), so the arrow must be parenthesised
fn type_expr(expr: &Expr, wrap: bool, ctx: &Ctx, edits: &mut Vec<Edit>) {
    if let Expr::Subscript(sub) = expr
        && callable_parts(sub).is_some()
    {
        let arrow = render(expr, ctx);
        edits.push(Edit {
            start: expr.range().start().to_usize(),
            end: expr.range().end().to_usize(),
            replacement: if wrap { format!("({arrow})") } else { arrow },
        });
        return;
    }
    // not a convertible callable — descend into children
    match expr {
        Expr::Subscript(sub) => type_expr(&sub.slice, false, ctx, edits),
        Expr::BinOp(binop) => {
            // an arrow operand of `|`/`&` needs parentheses
            type_expr(&binop.left, true, ctx, edits);
            type_expr(&binop.right, true, ctx, edits);
        }
        Expr::Tuple(tuple) => {
            for elt in &tuple.elts {
                type_expr(elt, false, ctx, edits);
            }
        }
        Expr::List(list) => {
            for elt in &list.elts {
                type_expr(elt, false, ctx, edits);
            }
        }
        Expr::UnaryOp(unary) => type_expr(&unary.operand, true, ctx, edits),
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
    // parameters must be a list, a bare `...`, a `ParamSpec`, or a
    // `Concatenate`
    let params = match params {
        Expr::EllipsisLiteral(_) => CallableParams::Ellipsis,
        Expr::List(list) => CallableParams::List(&list.elts),
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

/// render a type expression, converting every convertible callable within it
fn render(expr: &Expr, ctx: &Ctx) -> String {
    if let Expr::Subscript(sub) = expr
        && let Some((params, ret)) = callable_parts(sub)
    {
        let params_text = match params {
            CallableParams::List(elts) => {
                let inner: Vec<String> = elts.iter().map(|e| render_parameter(e, ctx)).collect();
                format!("({})", inner.join(", "))
            }
            CallableParams::Ellipsis => "(...)".to_string(),
            // `Callable[P, R]` → `(**P)`
            CallableParams::ParamSpec(p) => format!("(**{})", render(p, ctx)),
            // `Callable[Concatenate[T1, …, P], R]` → `(T1, …, **P)`
            CallableParams::Concatenate(prefix, paramspec) => {
                let mut parts: Vec<String> = prefix.iter().map(|e| render(e, ctx)).collect();
                parts.push(format!("**{}", render(paramspec, ctx)));
                format!("({})", parts.join(", "))
            }
        };
        let mut ret_text = render(ret, ctx);
        if needs_return_parens(ret) {
            ret_text = format!("({ret_text})");
        }
        return format!("{params_text} -> {ret_text}");
    }

    // reconstruct from source, splicing any nested convertible callables
    let mut hits: Vec<(TextRange, String)> = Vec::new();
    collect_outermost(expr, false, ctx, &mut hits);
    splice(ctx.source, expr.range(), &hits)
}

/// render one element of an arrow parameter list. `Unpack[Ts]` of a known
/// `TypeVarTuple` takes the shorter `*Ts` spelling the arrow form prefers;
/// every other element renders as an ordinary type
fn render_parameter(elt: &Expr, ctx: &Ctx) -> String {
    if let Expr::Subscript(sub) = elt
        && subscript_head(&sub.value) == Some("Unpack")
        && let Expr::Name(name) = sub.slice.as_ref()
        && ctx.type_var_tuples.contains(name.id.as_str())
    {
        return format!("*{}", name.id);
    }
    render(elt, ctx)
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
fn collect_outermost(expr: &Expr, wrap: bool, ctx: &Ctx, hits: &mut Vec<(TextRange, String)>) {
    if let Expr::Subscript(sub) = expr
        && callable_parts(sub).is_some()
    {
        let arrow = render(expr, ctx);
        hits.push((
            expr.range(),
            if wrap { format!("({arrow})") } else { arrow },
        ));
        return;
    }
    match expr {
        Expr::Subscript(sub) => collect_outermost(&sub.slice, false, ctx, hits),
        Expr::BinOp(binop) => {
            collect_outermost(&binop.left, true, ctx, hits);
            collect_outermost(&binop.right, true, ctx, hits);
        }
        Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .for_each(|e| collect_outermost(e, false, ctx, hits)),
        Expr::List(list) => list
            .elts
            .iter()
            .for_each(|e| collect_outermost(e, false, ctx, hits)),
        Expr::UnaryOp(unary) => collect_outermost(&unary.operand, true, ctx, hits),
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
    fn converts_unpacked_type_var_tuple() {
        assert_eq!(
            run("def f[*Ts](cb: Callable[[Unpack[Ts]], int]) -> None: ...\n"),
            "def f[*Ts](cb: (*Ts) -> int) -> None: ...\n"
        );
    }

    #[test]
    fn converts_unpacked_type_var_tuple_after_a_prefix() {
        assert_eq!(
            run("def f[*Ts](cb: Callable[[str, Unpack[Ts]], int]) -> None: ...\n"),
            "def f[*Ts](cb: (str, *Ts) -> int) -> None: ...\n"
        );
    }

    #[test]
    fn converts_a_legacy_type_var_tuple() {
        assert_eq!(
            run("Ts = TypeVarTuple(\"Ts\")\nf: Callable[[Unpack[Ts]], int]\n"),
            "Ts = TypeVarTuple(\"Ts\")\nf: (*Ts) -> int\n"
        );
    }

    #[test]
    fn converts_a_starred_param_list() {
        assert_eq!(
            run("def f[*Ts](cb: Callable[[*Ts], int]) -> None: ...\n"),
            "def f[*Ts](cb: (*Ts) -> int) -> None: ...\n"
        );
    }

    #[test]
    fn keeps_unpack_of_a_non_type_var_tuple() {
        // `Movie` is a `TypedDict`, not a `TypeVarTuple` — `*Movie` would read
        // as a homogeneous variadic annotated `Movie`, so the `Unpack` stays
        assert_eq!(
            run("f: Callable[[Unpack[Movie]], int]\n"),
            "f: (Unpack[Movie]) -> int\n"
        );
    }

    #[test]
    fn keeps_unpack_of_a_tuple_type() {
        assert_eq!(
            run("f: Callable[[Unpack[tuple[int, str]]], bool]\n"),
            "f: (Unpack[tuple[int, str]]) -> bool\n"
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
