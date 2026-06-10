//! AST pass: auto-quotes forward self-references in class definitions
//!
//! `class A(list[A])` → `class A(list["A"])`
//!
//! the class name appearing as a subscript slice argument in base classes or
//! the class body is replaced with a string literal — a PEP 484 forward
//! reference resolvable at runtime without deferred annotation evaluation.
//!
//! fires when the name is inside a subscript slice; direct bases (`class A(A):`)
//! are left alone — that is a runtime error regardless of quoting.
//!
//! basedpython has no manual forward-reference syntax (a string in an
//! annotation is a string-literal *type*), so the transpiler is the only
//! place these self-references can be made runtime-safe. quoting is skipped
//! when it isn't needed: on python >= 3.14 annotations are deferred natively
//! (PEP 649), and a user-written or opt-in `from __future__ import annotations`
//! already defers every annotation
//!
//! per-class state (class name + PEP-695 typevar names) means each `ClassDef`
//! drives its own walk; the shared [`type_expr_walker`] traverses each
//! class's body + bases identifying type positions, and this pass's visitor
//! checks each one for self-references

use ruff_python_ast::{Expr, ModModule, PythonVersion, Stmt, StmtClassDef};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{AstPass, PassContext};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_one_type_expr};

pub(crate) struct AutoQuote<'src> {
    source: &'src str,
    min_version: PythonVersion,
    inject_future: bool,
}

impl<'src> AutoQuote<'src> {
    pub(crate) fn new(source: &'src str, min_version: PythonVersion, inject_future: bool) -> Self {
        Self {
            source,
            min_version,
            inject_future,
        }
    }
}

impl AstPass for AutoQuote<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        // annotation positions need no quoting when they won't be eagerly
        // evaluated: native deferral on 3.14+ (PEP 649), or a future import
        // that defers them all. class *bases* and value-position subscripts
        // (`class A(list[A])`, `list[A]()`) evaluate eagerly regardless, so
        // their self-references are always quoted
        let quote_annotations = !(self.min_version.defers_annotations()
            || self.inject_future
            || has_future_annotations(&module.body));
        let mut edits: Vec<(TextRange, String)> = Vec::new();
        process_stmts(&module.body, self.source, &mut edits, quote_annotations);
        ctx.text_edits.extend(edits);
    }
}

fn has_future_annotations(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| {
        matches!(s, Stmt::ImportFrom(node)
            if node.module.as_deref() == Some("__future__")
                && node.names.iter().any(|a| a.name.as_str() == "annotations"))
    })
}

fn process_stmts(
    stmts: &[Stmt],
    source: &str,
    edits: &mut Vec<(TextRange, String)>,
    quote_annotations: bool,
) {
    for stmt in stmts {
        if let Stmt::ClassDef(c) = stmt {
            process_class(c, source, edits, quote_annotations);
            // nested classes inside this one's body are recursed into by
            // process_class so the inner ClassDef walks with its own name
        } else {
            // top-level non-class statements may contain nested classes via
            // function bodies — descend
            walk_for_nested_classes(stmt, source, edits, quote_annotations);
        }
    }
}

fn walk_for_nested_classes(
    stmt: &Stmt,
    source: &str,
    edits: &mut Vec<(TextRange, String)>,
    quote_annotations: bool,
) {
    // only walk into structures that may contain nested class defs.
    // function bodies, if/while/try blocks, etc.
    match stmt {
        Stmt::FunctionDef(f) => process_stmts(&f.body, source, edits, quote_annotations),
        Stmt::If(i) => {
            process_stmts(&i.body, source, edits, quote_annotations);
            for clause in &i.elif_else_clauses {
                process_stmts(&clause.body, source, edits, quote_annotations);
            }
        }
        Stmt::While(w) => process_stmts(&w.body, source, edits, quote_annotations),
        Stmt::For(f) => {
            process_stmts(&f.body, source, edits, quote_annotations);
            process_stmts(&f.orelse, source, edits, quote_annotations);
        }
        Stmt::With(w) => process_stmts(&w.body, source, edits, quote_annotations),
        Stmt::Try(t) => {
            process_stmts(&t.body, source, edits, quote_annotations);
            for h in &t.handlers {
                let ruff_python_ast::ExceptHandler::ExceptHandler(eh) = h;
                process_stmts(&eh.body, source, edits, quote_annotations);
            }
            process_stmts(&t.orelse, source, edits, quote_annotations);
            process_stmts(&t.finalbody, source, edits, quote_annotations);
        }
        _ => {}
    }
}

fn process_class(
    class: &StmtClassDef,
    source: &str,
    edits: &mut Vec<(TextRange, String)>,
    quote_annotations: bool,
) {
    let class_name = class.name.id.as_str();
    let typevar_names: Vec<String> = class
        .type_params
        .as_deref()
        .map(|tps| {
            tps.type_params
                .iter()
                .map(|tp| tp.name().id.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default();

    let mut visitor = Visitor {
        source,
        class_name,
        typevars: &typevar_names,
        edits,
        skip_self_ref_root: false,
    };

    if let Some(args) = &class.arguments {
        for base in &args.args {
            // a direct `class A(A)` base must not be quoted — that would
            // mask a runtime error rather than fix it. for a base, the
            // root self-ref is the bare-name direct base; descend into the
            // subscript slice / union arms but skip a root self-ref name
            visitor.skip_self_ref_root = true;
            walk_one_type_expr(base, &mut visitor);
            visitor.skip_self_ref_root = false;
        }
    }

    // body: AnnAssign annotations + function annotations are type positions
    // (handled by walker), method bodies need separate descent for
    // `list[A]()` patterns
    for stmt in &class.body {
        process_class_body_stmt(stmt, &mut visitor, quote_annotations);
    }

    // recurse into nested classes inside the body so they get their own
    // class-context walk (the `visitor` borrow of `edits` ends above)
    process_stmts(&class.body, source, edits, quote_annotations);
}

fn process_class_body_stmt(stmt: &Stmt, visitor: &mut Visitor<'_>, quote_annotations: bool) {
    match stmt {
        Stmt::Expr(e) => walk_value_subscripts(e.value.as_ref(), visitor),
        Stmt::Assign(a) => walk_value_subscripts(a.value.as_ref(), visitor),
        Stmt::AnnAssign(a) => {
            if quote_annotations {
                walk_one_type_expr(a.annotation.as_ref(), visitor);
            }
            if let Some(value) = &a.value {
                walk_value_subscripts(value.as_ref(), visitor);
            }
        }
        Stmt::FunctionDef(f) => {
            if !quote_annotations {
                return;
            }
            for param in f.parameters.iter_non_variadic_params() {
                if let Some(ann) = &param.parameter.annotation {
                    walk_one_type_expr(ann, visitor);
                }
            }
            if let Some(var) = &f.parameters.vararg
                && let Some(ann) = &var.annotation
            {
                walk_one_type_expr(ann, visitor);
            }
            if let Some(kwarg) = &f.parameters.kwarg
                && let Some(ann) = &kwarg.annotation
            {
                walk_one_type_expr(ann, visitor);
            }
            if let Some(ret) = &f.returns {
                walk_one_type_expr(ret, visitor);
            }
        }
        _ => {}
    }
}

/// `list[A]()` and similar — quote a self-ref inside a value-position
/// subscript on the LHS of a Call. doesn't descend into Call args
fn walk_value_subscripts(expr: &Expr, visitor: &mut Visitor<'_>) {
    match expr {
        Expr::Subscript(s) => {
            walk_one_type_expr(s.slice.as_ref(), visitor);
            walk_value_subscripts(&s.value, visitor);
        }
        Expr::Call(c) => walk_value_subscripts(&c.func, visitor),
        Expr::Attribute(a) => walk_value_subscripts(&a.value, visitor),
        _ => {}
    }
}

struct Visitor<'a> {
    source: &'a str,
    class_name: &'a str,
    typevars: &'a [String],
    edits: &'a mut Vec<(TextRange, String)>,
    /// when walking a class base, the root expression must not be quoted
    /// even if it's a bare self-ref name — that would mask the runtime error
    skip_self_ref_root: bool,
}

impl TypeExprVisitor for Visitor<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // class base: the root expression must not be quoted even if it
        // contains a self-ref. for a bare-name direct base (`class A(A):`)
        // quoting would mask a runtime error; for a subscript/binop, the
        // self-ref lives inside and the walker descends into it
        if self.skip_self_ref_root {
            self.skip_self_ref_root = false;
            return match expr {
                Expr::Subscript(_) | Expr::BinOp(_) => Recurse::Descend,
                _ => Recurse::Stop,
            };
        }
        if !contains_self_ref(expr, self.class_name) {
            return Recurse::Stop;
        }
        match expr {
            // unparenthesized tuple in a subscript slice (the walker
            // already passes individual elts; this fires when we somehow
            // see the Tuple directly — descend)
            Expr::Tuple(_) => Recurse::Descend,
            // `A | B` arms: quote the whole union since the original
            // behaviour collapsed it into one string (`"A | None"`) to
            // avoid the runtime `str | NoneType` computation that quoting
            // only the self-ref arm would produce
            Expr::BinOp(_) => {
                self.emit_quote(expr.range());
                Recurse::Stop
            }
            // `A[T]` where A is the class name — quote the whole subscript
            // since the base name itself is a forward reference
            Expr::Subscript(s) if is_self_ref_root(&s.value, self.class_name) => {
                self.emit_quote(expr.range());
                Recurse::Stop
            }
            // generic subscript whose base isn't a self-ref: descend into
            // the slice so nested self-refs get quoted at their own level
            Expr::Subscript(_) => Recurse::Descend,
            // bare-name or any other expression containing a self-ref:
            // quote whole as a forward reference
            _ => {
                self.emit_quote(expr.range());
                Recurse::Stop
            }
        }
    }
}

impl Visitor<'_> {
    fn emit_quote(&mut self, range: TextRange) {
        let raw = &self.source[usize::from(range.start())..usize::from(range.end())];
        // basedpython renames PEP 695 typevars (`T` → `_T`) when polyfilling
        // for runtime. quoting a forward-reference verbatim from source would
        // capture the pre-rename name and leave it unresolved inside the
        // string. apply the rename here so the quoted form stays correct
        let body = if self.typevars.is_empty() {
            raw.to_owned()
        } else {
            substitute_typevars(raw, self.typevars)
        };
        self.edits.push((range, format!("\"{body}\"")));
    }
}

fn is_self_ref_root(expr: &Expr, class_name: &str) -> bool {
    matches!(expr, Expr::Name(n) if n.id.as_str() == class_name)
}

fn contains_self_ref(expr: &Expr, class_name: &str) -> bool {
    match expr {
        Expr::Name(n) => n.id.as_str() == class_name,
        Expr::Subscript(s) => {
            contains_self_ref(&s.value, class_name) || contains_self_ref(&s.slice, class_name)
        }
        Expr::BinOp(b) => {
            contains_self_ref(&b.left, class_name) || contains_self_ref(&b.right, class_name)
        }
        Expr::Tuple(t) => t.elts.iter().any(|e| contains_self_ref(e, class_name)),
        _ => false,
    }
}

/// Replace each occurrence of `name` with `_name` (the mangled form) when
/// `name` appears as an identifier token. Identifier boundaries are detected
/// against the surrounding bytes — `T` matches `T`, `[T]`, `T |`, but not
/// `Tree` or `_T`. Only matches names that are NOT already prefixed with `_`.
fn substitute_typevars(text: &str, typevars: &[String]) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + typevars.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let starts_ident = b.is_ascii_alphabetic() || b == b'_';
        let prev_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if starts_ident && !prev_ident {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let ident = &text[i..j];
            if typevars.iter().any(|tv| tv == ident) {
                out.push('_');
                out.push_str(ident);
            } else {
                out.push_str(ident);
            }
            i = j;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::config::PythonVersion;
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple_self_reference() {
        check("class A(list[A]): ...\n", "class A(list[\"A\"]): ...\n");
    }

    #[test]
    fn nested_self_reference() {
        check(
            "class Tree(Node[Tree]): ...\n",
            "class Tree(Node[\"Tree\"]): ...\n",
        );
    }

    #[test]
    fn self_ref_in_union() {
        check(
            "class A(list[A | None]): ...\n",
            "class A(list[\"A | None\"]): ...\n",
        );
    }

    #[test]
    fn self_ref_in_nested_subscript() {
        check(
            "class A(dict[str, list[A]]): ...\n",
            "class A(dict[str, list[\"A\"]]): ...\n",
        );
    }

    #[test]
    fn direct_base_not_quoted() {
        check("class A(A): ...\n", "class A(A): ...\n");
    }

    #[test]
    fn other_names_not_quoted() {
        check("class A(list[B]): ...\n", "class A(list[B]): ...\n");
    }

    #[test]
    fn multiple_occurrences() {
        check(
            "class A(Union[A, A]): ...\n",
            indoc! {"
                from typing import Union
                class A(Union[\"A\", \"A\"]): ...
            "},
        );
    }

    #[test]
    fn body_expr_stmt_call() {
        check(
            indoc! {"
                class A(list[A], dict[int]):
                    list[A]()
            "},
            indoc! {"
                class A(list[\"A\"], dict[int]):
                    list[\"A\"]()
            "},
        );
    }

    #[test]
    fn body_ann_assign() {
        check(
            indoc! {"
                class A(list[A]):
                    x: list[A] = list[A]()
            "},
            indoc! {"
                class A(list[\"A\"]):
                    x: list[\"A\"] = list[\"A\"]()
            "},
        );
    }

    #[test]
    fn body_method_annotations() {
        check(
            indoc! {"
                class A(list[A]):
                    def method(self, x: list[A]) -> list[A]: ...
            "},
            indoc! {"
                class A(list[\"A\"]):
                    def method(self, x: list[\"A\"]) -> list[\"A\"]: ...
            "},
        );
    }

    #[test]
    fn body_method_body_not_quoted() {
        check(
            indoc! {"
                class A(list[A]):
                    def method(self):
                        return list[A]()
            "},
            indoc! {"
                class A(list[\"A\"]):
                    def method(self):
                        return list[A]()
            "},
        );
    }

    #[test]
    fn nested_class_inner_quotes_own_name() {
        check(
            indoc! {"
                class Outer:
                    class Inner(list[Inner]): ...
            "},
            indoc! {"
                class Outer:
                    class Inner(list[\"Inner\"]): ...
            "},
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("class A(list[A]): ...\n");
    }

    #[test]
    fn body_field_with_generic_self_ref() {
        check(
            indoc! {"
                class Tree:
                    children: list[Tree[int]]
            "},
            indoc! {"
                class Tree:
                    children: list[\"Tree[int]\"]
            "},
        );
    }

    #[test]
    fn bare_self_ref_in_method_signature() {
        check(
            indoc! {"
                class A:
                    def f(self) -> A: ...
            "},
            indoc! {"
                class A:
                    def f(self) -> \"A\": ...
            "},
        );
    }

    fn transpile_with(input: &str, config: &Config) -> String {
        transpile(input, config).unwrap()
    }

    #[test]
    fn not_quoted_when_version_defers_annotations() {
        // 3.14+ evaluates annotations lazily (PEP 649); nothing to quote
        let config = Config {
            min_version: PythonVersion::from((3, 14)),
            ..Config::test_default()
        };
        let out = transpile_with("class A:\n    def f(self) -> A: ...\n", &config);
        assert!(
            out.contains("-> A:"),
            "should leave the self-ref bare on 3.14+, got: {out}"
        );
    }

    #[test]
    fn class_base_still_quoted_when_version_defers_annotations() {
        // pep 649 defers annotations only — a class *base* evaluates eagerly,
        // so its self-reference still needs the quote
        let config = Config {
            min_version: PythonVersion::from((3, 14)),
            ..Config::test_default()
        };
        let out = transpile_with("class A(list[A]):\n    pass\n", &config);
        assert!(
            out.contains("class A(list[\"A\"]):"),
            "base self-ref must stay quoted on 3.14+, got: {out}"
        );
    }

    #[test]
    fn not_quoted_when_source_has_future() {
        // a user-written future import already defers every annotation
        let config = Config::test_default();
        let out = transpile_with(
            "from __future__ import annotations\nclass A:\n    def f(self) -> A: ...\n",
            &config,
        );
        assert!(
            out.contains("-> A:"),
            "should leave the self-ref bare when future is present, got: {out}"
        );
    }

    #[test]
    fn not_quoted_when_inject_future_opted_in() {
        // opting into the blanket future import defers annotations, so the
        // surgical quote is skipped and the import is prepended instead
        let config = Config {
            inject_future_annotations: true,
            ..Config::test_default()
        };
        let out = transpile_with("class A:\n    def f(self) -> A: ...\n", &config);
        assert!(
            out.starts_with("from __future__ import annotations\n"),
            "should inject the future import, got: {out}"
        );
        assert!(
            out.contains("-> A:"),
            "should leave the self-ref bare when future is injected, got: {out}"
        );
    }
}
