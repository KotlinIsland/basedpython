//! Lowers the basedpython `raises` clause.
//!
//! `def f() -> int raises TypeError:` declares the exceptions that can escape a
//! call to `f`. The declaration is checked statically by ty, and has no python
//! spelling, so lowering deletes it: [`RaisesStripPass`] removes the clause and
//! nothing else.
//!
//! When `runtime_raises_checks` is on, [`RaisesGuardPass`] additionally wraps
//! each declared function in a guard that fails at runtime if it raises outside
//! its clause, defending the contract against callers the checker never saw
//! (untyped or third-party code). The guard is a decorator inserted directly
//! above the `def` — an insertion at the start of a line, so it never disturbs
//! the ranges the other passes read, unlike wrapping the body in a `try`.
//!
//! Only a clause with a faithful runtime test is guarded. The `isinstance`
//! target comes from ty, which yields
//! `None` for a gradual `raises ...` and for any set with no runtime spelling,
//! and the empty tuple for `raises Never` — which nothing is an instance of, so
//! the guard rejects every exception. Stub bodies are skipped: there is nothing
//! to guard.

use ruff_python_ast::helpers::raises_clause_spans;
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{ModModule, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};
use crate::type_info::TypeInfo;

/// Fails when a guarded function raises outside its declared set.
///
/// The wrapper shape is chosen at decoration time rather than by the transform:
/// a coroutine must be awaited, and a generator or async generator iterated,
/// before the body runs at all — wrapping any of them with a plain call would
/// catch nothing. An async generator is checked first because it answers `False`
/// to both `iscoroutinefunction` and `isgeneratorfunction`.
const RAISES_RUNTIME: &str = r#"def _by_raises(_allowed, _name):
    import functools
    import inspect

    def _check(_exc):
        if not isinstance(_exc, _allowed):
            raise AssertionError(
                f"{_name} raised {type(_exc).__name__}, which its `raises` clause does not include"
            ) from _exc

    def _decorate(_fn):
        if inspect.isasyncgenfunction(_fn):
            @functools.wraps(_fn)
            async def _wrapper(*_args, **_kwargs):
                try:
                    async for _item in _fn(*_args, **_kwargs):
                        yield _item
                except BaseException as _exc:
                    _check(_exc)
                    raise
        elif inspect.iscoroutinefunction(_fn):
            @functools.wraps(_fn)
            async def _wrapper(*_args, **_kwargs):
                try:
                    return await _fn(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc)
                    raise
        elif inspect.isgeneratorfunction(_fn):
            @functools.wraps(_fn)
            def _wrapper(*_args, **_kwargs):
                try:
                    yield from _fn(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc)
                    raise
        else:
            @functools.wraps(_fn)
            def _wrapper(*_args, **_kwargs):
                try:
                    return _fn(*_args, **_kwargs)
                except BaseException as _exc:
                    _check(_exc)
                    raise
        return _wrapper

    return _decorate"#;

/// Deletes every `raises` clause.
pub(crate) struct RaisesStripPass<'src> {
    source: &'src str,
}

impl<'src> RaisesStripPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for RaisesStripPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut inner = ClauseVisitor {
            source: self.source,
            edits: Vec::new(),
        };
        for stmt in &module.body {
            inner.visit_stmt(stmt);
        }
        ctx.text_edits.extend(inner.edits);
    }
}

struct ClauseVisitor<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for ClauseVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt
            && let Some(range) = clause_range(self.source, function)
        {
            self.edits.push((range, String::new()));
        }
        walk_stmt(self, stmt);
    }
}

/// The source the `raises` clause occupies, plus the whitespace before it.
///
/// The clause itself — keyword through closing parenthesis — comes from
/// [`raises_clause_spans`], which tokenizes rather than searching for text, so
/// neither a comment nor a parenthesized type displaces the range.
pub(crate) fn clause_range(source: &str, function: &StmtFunctionDef) -> Option<TextRange> {
    let spans = raises_clause_spans(source, function)?;
    let before = source[..usize::from(spans.clause.start())].trim_end_matches([' ', '\t']);

    Some(TextRange::new(
        TextSize::try_from(before.len()).ok()?,
        spans.clause.end(),
    ))
}

/// Wraps each function with a runtime-testable `raises` clause in a guard.
pub(crate) struct RaisesGuardPass<'src> {
    source: &'src str,
    enabled: bool,
}

impl<'src> RaisesGuardPass<'src> {
    pub(crate) fn new(source: &'src str, enabled: bool) -> Self {
        Self { source, enabled }
    }
}

impl super::ast_driver::TypeAwarePass for RaisesGuardPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if !self.enabled {
            return;
        }

        let mut guards = Vec::new();
        let mut conflicts = Vec::new();
        for (index, stmt) in stmts.iter().enumerate() {
            // an AST pass that re-rendered this top-level statement rebuilds it
            // from the AST, which discards any insertion inside its range — and
            // the guard sits on the `def` line, inside it whenever the function
            // is decorated. a runtime check that silently disappears is worse
            // than one that refuses to build
            let rerendered = ctx.changed.contains(&index);
            let mut collector = GuardCollector {
                source: self.source,
                types,
                rerendered,
                guards: &mut guards,
                conflicts: &mut conflicts,
            };
            collector.visit_stmt(stmt);
        }

        ctx.errors.extend(conflicts);
        if guards.is_empty() {
            return;
        }

        ctx.required_imports.push(RAISES_RUNTIME.to_owned());
        ctx.text_edits.extend(guards);
    }
}

/// Finds every function needing a guard, at any nesting depth.
///
/// Walking rather than matching on `def` / `class` is what reaches a function
/// declared inside `if` / `try` / `with` / `for` — a conditional definition is
/// exactly as much in need of its contract as a top-level one.
struct GuardCollector<'a> {
    source: &'a str,
    types: &'a dyn TypeInfo,
    rerendered: bool,
    guards: &'a mut Vec<(TextRange, String)>,
    conflicts: &'a mut Vec<String>,
}

impl<'ast> Visitor<'ast> for GuardCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt
            && let Some(guard) = guard_for(self.source, function, self.types)
        {
            if self.rerendered && !function.decorator_list.is_empty() {
                self.conflicts.push(format!(
                    "`{}` declares `raises`, but another lowering re-rendered its \
                     statement, which would drop the runtime guard",
                    function.name
                ));
            } else {
                self.guards.push(guard);
            }
        }

        walk_stmt(self, stmt);
    }
}

/// The decorator insertion guarding `function`, when its clause has a runtime test.
fn guard_for(
    source: &str,
    function: &StmtFunctionDef,
    types: &dyn TypeInfo,
) -> Option<(TextRange, String)> {
    function.raises.as_ref()?;
    if is_stub_body(function) {
        return None;
    }

    let allowed = types.declared_raises_runtime_target(function)?;
    let (offset, indent) = def_line_start(source, function)?;
    let name = function.name.as_str();

    Some((
        TextRange::empty(offset),
        format!("{indent}@_by_raises({allowed}, \"{name}\")\n"),
    ))
}

/// A body that is exactly `...` declares a signature and runs nothing.
fn is_stub_body(function: &StmtFunctionDef) -> bool {
    match function.body.as_slice() {
        [Stmt::Expr(expr)] => expr.value.is_ellipsis_literal_expr(),
        [] => true,
        _ => false,
    }
}

/// The offset of the `def` keyword and the indentation of its line.
///
/// The statement's own range starts at the first decorator, so the guard — which
/// must be the innermost wrapper — is placed by finding the `def` itself.
fn def_line_start<'src>(
    source: &'src str,
    function: &StmtFunctionDef,
) -> Option<(TextSize, &'src str)> {
    let name = usize::from(function.name.start());
    let mut keyword = source[..name].rfind("def")?;

    // `async def` — the construct starts at the `async`
    let before = source[..keyword].trim_end_matches([' ', '\t']);
    if before.ends_with("async") {
        keyword = before.len() - "async".len();
    }

    let line = source[..keyword].rfind('\n').map_or(0, |index| index + 1);

    // anything else on the line before the `def` (a one-line `class C: def ...`
    // recovery, a decorator sharing the line) would misplace the guard
    let indent = &source[line..keyword];
    if !indent.trim_start_matches([' ', '\t']).is_empty() {
        return None;
    }

    Some((TextSize::try_from(line).ok()?, indent))
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    fn guarded() -> Config {
        Config {
            runtime_raises_checks: true,
            ..Config::test_default()
        }
    }

    #[test]
    fn clause_stripped() {
        check(
            "def f() raises TypeError:\n    raise TypeError\n",
            indoc! {"
                def f():
                    raise TypeError
            "},
        );
    }

    #[test]
    fn clause_stripped_after_return_annotation() {
        check(
            "def f() -> int raises TypeError | ValueError:\n    raise TypeError\n",
            indoc! {"
                def f() -> int:
                    raise TypeError
            "},
        );
    }

    #[test]
    fn never_and_gradual_clauses_are_stripped() {
        check(
            "def a() raises Never:\n    return\n\ndef b() raises ...:\n    return\n",
            indoc! {"
                def a():
                    return

                def b():
                    return
            "},
        );
    }

    #[test]
    fn clause_survives_body_rerender() {
        // a body construct that forces the whole statement to be re-rendered
        // must still drop the clause — codegen emits python, which has no
        // spelling for it
        let out = transpile(
            "def f(x: int | None) -> int raises TypeError:\n    return x ?? 0\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("raises"), "clause leaked:\n{out}");
        assert!(out.contains("def f(x: int | None) -> int:"), "got:\n{out}");
    }

    #[test]
    fn clause_survives_a_whole_statement_rerender() {
        // `typeof` in a parameter annotation makes an AST-mutation pass re-render
        // the whole `def`, dropping this pass's deletion. the generator emits
        // python, so the clause has to be erased there too — otherwise the
        // construct leaks and the pipeline reports a transform conflict
        let out = transpile(
            "def f(x: typeof(1)) raises TypeError:\n    raise TypeError\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("raises"), "clause leaked:\n{out}");
        assert!(out.contains("def f(x: TypeOf[1]):"), "got:\n{out}");
    }

    #[test]
    fn parenthesized_clause_is_stripped_whole() {
        // the declared type's range excludes the parentheses around it, so a
        // deletion that stopped at the type would orphan the closing paren and
        // produce invalid python. the formatter emits this shape for long clauses
        check(
            "def f() -> int raises (\n    TypeError\n):\n    raise TypeError\n",
            indoc! {"
                def f() -> int:
                    raise TypeError
            "},
        );
    }

    #[test]
    fn a_comment_holding_the_keyword_does_not_move_the_range() {
        check(
            "def f() -> int raises (  # raises here\n    TypeError\n):\n    raise TypeError\n",
            indoc! {"
                def f() -> int:
                    raise TypeError
            "},
        );
    }

    #[test]
    fn guard_reaches_a_function_nested_in_control_flow() {
        // a conditional definition needs its contract as much as a top-level one
        let out = transpile(
            "if True:\n    def f() raises ValueError:\n        raise ValueError\n",
            &guarded(),
        )
        .unwrap();
        assert!(
            out.contains("    @_by_raises(ValueError, \"f\")\n    def f():"),
            "guard missing:\n{out}"
        );
    }

    #[test]
    fn guard_that_would_be_dropped_is_an_error() {
        // `typeof` makes an AST pass re-render the whole statement, discarding an
        // insertion inside its range — and a decorated `def` puts the guard there.
        // a runtime check that silently vanishes is worse than one that refuses
        let error = transpile(
            "class C:\n    @staticmethod\n    def m(x: typeof(1)) raises TypeError:\n        raise TypeError\n",
            &guarded(),
        )
        .unwrap_err();
        assert!(
            error.contains("would drop the runtime guard"),
            "got: {error}"
        );
    }

    #[test]
    fn guard_wraps_a_declared_function() {
        let out = transpile(
            "def f() raises TypeError:\n    raise TypeError\n",
            &guarded(),
        )
        .unwrap();
        assert!(out.contains("def _by_raises("), "helper missing:\n{out}");
        assert!(
            out.contains("@_by_raises(TypeError, \"f\")\ndef f():"),
            "guard missing:\n{out}"
        );
    }

    #[test]
    fn guard_covers_a_union_and_never() {
        let out = transpile(
            "def f() raises TypeError | ValueError:\n    raise TypeError\n\ndef g() raises Never:\n    return\n",
            &guarded(),
        )
        .unwrap();
        assert!(
            out.contains("@_by_raises((TypeError, ValueError), \"f\")"),
            "union target wrong:\n{out}"
        );
        assert!(
            out.contains("@_by_raises((), \"g\")"),
            "`Never` target wrong:\n{out}"
        );
    }

    #[test]
    fn guard_is_the_innermost_decorator_and_keeps_indentation() {
        let out = transpile(
            indoc! {"
                class C:
                    @staticmethod
                    def m() raises TypeError:
                        raise TypeError
            "},
            &guarded(),
        )
        .unwrap();
        assert!(
            out.contains("    @staticmethod\n    @_by_raises(TypeError, \"m\")\n    def m():"),
            "guard misplaced:\n{out}"
        );
    }

    #[test]
    fn guard_handles_async() {
        let out = transpile(
            "async def f() raises TypeError:\n    raise TypeError\n",
            &guarded(),
        )
        .unwrap();
        assert!(
            out.contains("@_by_raises(TypeError, \"f\")\nasync def f():"),
            "guard misplaced:\n{out}"
        );
    }

    #[test]
    fn no_guard_without_a_runtime_test_or_a_body() {
        // a gradual clause has nothing to test against, and a stub body runs
        // nothing worth guarding
        let out = transpile(
            "def f() raises ...:\n    raise TypeError\n\ndef g() raises TypeError: ...\n",
            &guarded(),
        )
        .unwrap();
        assert!(!out.contains("_by_raises"), "unexpected guard:\n{out}");
    }

    #[test]
    fn no_guard_when_the_option_is_off() {
        let out = transpile(
            "def f() raises TypeError:\n    raise TypeError\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("_by_raises"), "unexpected guard:\n{out}");
    }
}
