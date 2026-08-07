//! `def f(...) -> None` → `def f(...)`
//!
//! a `def` that leaves out its return type returns what its body returns, and a
//! stub-shaped body returns `None` — so an explicit `-> None` on one only repeats
//! what the `def` already says. this is the `redundant-return-annotation` lint
//! applied to the vendored stubs
//!
//! deleting the annotation is only the same type where the *body* is what would
//! answer. every other source outranks it, so this refuses anything it cannot
//! read off the `def` alone: an overload group member (the implementation draws
//! its return type from the group), an `override` (the base draws it), `__new__`
//! and `__init__` (construction reads them structurally), an `async def` (whose
//! annotation is the awaited type, not the one the body hands back), a decorator
//! that could transform the signature, and a body that is not a plain stub

use std::collections::HashMap;
use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Stmt, StmtFunctionDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct RedundantNoneReturn;

impl Patch for RedundantNoneReturn {
    fn name(&self) -> &'static str {
        "redundant-none-return"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        walk_scope(&parsed.syntax().body, source, &mut edits);
        edits
    }
}

/// decorators that leave both the body and the return type alone. a decorator
/// outside this set either supplies the return type from elsewhere (`overload`,
/// the synthetic `override` marker) or wraps the function into something else
/// (`property`, `contextmanager`), so the `def`'s own body stops being the answer
const INERT_DECORATORS: &[&str] = &[
    "staticmethod",
    "classmethod",
    "abstractmethod",
    // the synthetic markers basedpython's `static def` / `abstract def` /
    // `final def` modifier keywords parse to
    "static",
    "abstract",
    "final",
];

/// a scope's function definitions, then its nested scopes. the name counts are
/// taken over the whole scope so a name defined twice — an overload group, or a
/// `sys.version_info` split — is left alone wherever its definitions sit
fn walk_scope(body: &[Stmt], source: &str, edits: &mut Vec<Edit>) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    count_definitions(body, &mut counts);
    visit(body, &counts, source, edits);
}

/// every `def` name bound directly in this scope, branch bodies included
fn count_definitions<'a>(body: &'a [Stmt], counts: &mut HashMap<&'a str, usize>) {
    for_each_branch_body(body, &mut |stmts| {
        for stmt in stmts {
            if let Stmt::FunctionDef(func) = stmt {
                *counts.entry(func.name.as_str()).or_default() += 1;
            }
        }
    });
}

fn visit(body: &[Stmt], counts: &HashMap<&str, usize>, source: &str, edits: &mut Vec<Edit>) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => {
                if counts.get(func.name.as_str()).copied() == Some(1)
                    && let Some(edit) = strip_edit(func, source)
                {
                    edits.push(edit);
                }
                walk_scope(&func.body, source, edits);
            }
            Stmt::ClassDef(class) => walk_scope(&class.body, source, edits),
            Stmt::If(node) => {
                visit(&node.body, counts, source, edits);
                for clause in &node.elif_else_clauses {
                    visit(&clause.body, counts, source, edits);
                }
            }
            Stmt::Try(node) => {
                visit(&node.body, counts, source, edits);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    visit(&h.body, counts, source, edits);
                }
                visit(&node.orelse, counts, source, edits);
                visit(&node.finalbody, counts, source, edits);
            }
            Stmt::With(node) => visit(&node.body, counts, source, edits),
            _ => {}
        }
    }
}

/// call `f` with this body and with every branch body that binds into the same
/// scope — an `if`/`try`/`with` suite, but not a nested class or function
fn for_each_branch_body<'a>(body: &'a [Stmt], f: &mut impl FnMut(&'a [Stmt])) {
    f(body);
    for stmt in body {
        match stmt {
            Stmt::If(node) => {
                for_each_branch_body(&node.body, f);
                for clause in &node.elif_else_clauses {
                    for_each_branch_body(&clause.body, f);
                }
            }
            Stmt::Try(node) => {
                for_each_branch_body(&node.body, f);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    for_each_branch_body(&h.body, f);
                }
                for_each_branch_body(&node.orelse, f);
                for_each_branch_body(&node.finalbody, f);
            }
            Stmt::With(node) => for_each_branch_body(&node.body, f),
            _ => {}
        }
    }
}

/// the deletion of ` -> None`, when this `def` would keep the same return type
/// without it
fn strip_edit(func: &StmtFunctionDef, source: &str) -> Option<Edit> {
    let returns = func.returns.as_deref()?;
    if !matches!(returns, Expr::NoneLiteral(_)) {
        return None;
    }
    // an `async def`'s declared return type is the awaited one, which the body
    // does not spell; leave the wrapping to the annotation
    if func.is_async {
        return None;
    }
    // construction reads both structurally — `__new__`'s return type decides
    // whether `__init__` runs at all — so neither is a place to let a placeholder
    // body answer. `__init__` also already has the annotation-free `init(...)`
    // spelling, so what is left here is what that conversion refused
    if matches!(func.name.as_str(), "__new__" | "__init__") {
        return None;
    }
    if !func
        .decorator_list
        .iter()
        .all(|decorator| match &decorator.expression {
            Expr::Name(name) => INERT_DECORATORS.contains(&name.id.as_str()),
            _ => false,
        })
    {
        return None;
    }
    if !is_stub_body(&func.body) {
        return None;
    }

    let start = func.parameters.range().end().to_usize();
    let end = returns.range().end().to_usize();
    // nothing but the arrow may sit in the span, or the deletion would take a
    // comment with it
    let between = source.get(start..end)?;
    if between.trim_start().strip_prefix("->")?.trim() != "None" {
        return None;
    }
    Some(Edit {
        start,
        end,
        replacement: String::new(),
    })
}

/// a body that hands back `None`: nothing at all, `...`, a docstring, or both.
/// a `raise` body returns `Never` and a `return <expr>` returns that expression,
/// so neither is the same type as the annotation
fn is_stub_body(body: &[Stmt]) -> bool {
    let is_docstring = |stmt: &Stmt| matches!(stmt, Stmt::Expr(expr) if matches!(expr.value.as_ref(), Expr::StringLiteral(_)));
    let is_ellipsis = |stmt: &Stmt| matches!(stmt, Stmt::Expr(expr) if matches!(expr.value.as_ref(), Expr::EllipsisLiteral(_)));
    match body {
        [] => true,
        [only] => is_docstring(only) || is_ellipsis(only),
        [first, second] => is_docstring(first) && is_ellipsis(second),
        _ => false,
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
        let edits = RedundantNoneReturn.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn strips_a_bodyless_def() {
        assert_eq!(run("def f(x: int) -> None\n"), "def f(x: int)\n");
    }

    #[test]
    fn strips_a_method_with_a_docstring() {
        let src = "\
class C:
    def close(self) -> None:
        \"\"\"Close it.\"\"\"
";
        let expected = "\
class C:
    def close(self):
        \"\"\"Close it.\"\"\"
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn strips_across_a_multiline_signature() {
        let src = "\
def f(
    a: int,
    b: str,
) -> None: ...
";
        let expected = "\
def f(
    a: int,
    b: str,
): ...
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn strips_under_an_inert_decorator() {
        let src = "\
class C:
    @staticmethod
    def f(x: int) -> None: ...
    static def g(x: int) -> None: ...
";
        let expected = "\
class C:
    @staticmethod
    def f(x: int): ...
    static def g(x: int): ...
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn leaves_an_override() {
        // the base method is what an unannotated override's return type comes
        // from, not the body
        let src = "\
class C(B):
    override def close(self) -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_an_overload_group() {
        // implicit overloads: consecutive bodyless `def`s with one name. the
        // implementation's return type is the group's, not its body's
        let src = "\
class C:
    def sort(self, *, key: None = None) -> None: ...
    def sort(self, *, key: (int) -> int) -> None
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_a_name_split_across_version_branches() {
        let src = "\
if sys.version_info >= (3, 12):
    def f(x: int) -> None: ...
else:
    def f(x: str) -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_an_explicit_overload() {
        let src = "\
@overload
def f(x: int) -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_a_property() {
        let src = "\
class C:
    @property
    def f(self) -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_construction_methods() {
        let src = "\
class C:
    def __new__(cls) -> None: ...
    def __init__(self) -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_an_async_def() {
        assert_eq!(
            run("async def f() -> None: ...\n"),
            "async def f() -> None: ...\n"
        );
    }

    #[test]
    fn leaves_a_non_stub_body() {
        let src = "\
def f() -> None:
    raise NotImplementedError
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_a_non_none_return() {
        assert_eq!(run("def f() -> int: ...\n"), "def f() -> int: ...\n");
    }

    #[test]
    fn leaves_a_comment_between_the_signature_and_the_arrow() {
        let src = "\
def f(x: int)  # why
    -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn idempotent_on_the_stripped_form() {
        for src in [
            "def f(x: int)\n",
            "class C:\n    def close(self):\n        \"\"\"Close it.\"\"\"\n",
            "def f(a: int): ...\n",
        ] {
            assert_eq!(run(src), src, "not idempotent on {src:?}");
        }
    }
}
