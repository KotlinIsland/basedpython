//! python's compile-time checks on the emitted module
//!
//! a module python parses can still be one it refuses to compile: an assignment
//! expression in a comprehension's iterable, a parameter named twice, a `break` outside
//! a loop, a capture pattern that makes the remaining cases unreachable. the parser
//! reports these apart from its parse errors, through a checker that asks its caller
//! about the scope each node sits in. this answers it for the transpiled output, so a
//! module python would refuse is a transpile error instead of a `SyntaxError` at import
//!
//! the questions that need a binding analysis — what a `global` or `nonlocal` names,
//! whether a name is a parameter — are answered so that the checker reports nothing
//! about them. what those checks find is written by the author, not by a lowering, and
//! `by check` has already reported it on the source

use std::cell::RefCell;

use ruff_python_ast::{Expr, PythonVersion, Stmt};
use ruff_python_parser::semantic_errors::{
    LazyImportContext, SemanticSyntaxChecker, SemanticSyntaxContext, SemanticSyntaxError,
    SemanticSyntaxErrorKind, YieldOutsideFunctionKind,
};
use ruff_text_size::TextRange;

use crate::transforms::python_scopes::{self, Scope, ScopeHooks, ScopeState};

/// the first error python's compile-time checks find in `suite`, on any version from
/// `min_version` on. the checks changed between versions in both directions — an async
/// comprehension inside a sync one compiles from 3.11, an assignment expression in an
/// annotation stopped compiling in 3.14 — so each version is checked
pub(crate) fn first_error(
    suite: &[Stmt],
    source: &str,
    min_version: PythonVersion,
    is_stub: bool,
) -> Option<SemanticSyntaxError> {
    let future_annotations_or_stub = is_stub || has_future_annotations(suite);
    PythonVersion::iter()
        .filter(|version| *version >= min_version)
        .find_map(|python_version| {
            let mut checker = Checker {
                syntax: SemanticSyntaxChecker::new(),
                errors: RefCell::default(),
                source,
                python_version,
                future_annotations_or_stub,
            };
            python_scopes::walk(suite, &mut checker);
            checker.errors.into_inner().into_iter().next()
        })
}

fn has_future_annotations(suite: &[Stmt]) -> bool {
    suite.iter().any(|stmt| {
        matches!(stmt, Stmt::ImportFrom(import)
            if import.module.as_deref() == Some("__future__")
                && import.names.iter().any(|alias| alias.name.as_str() == "annotations"))
    })
}

struct Checker<'src> {
    syntax: SemanticSyntaxChecker,
    errors: RefCell<Vec<SemanticSyntaxError>>,
    source: &'src str,
    python_version: PythonVersion,
    future_annotations_or_stub: bool,
}

impl ScopeHooks<'_> for Checker<'_> {
    fn stmt(&mut self, stmt: &Stmt, state: &ScopeState) {
        let mut syntax = std::mem::take(&mut self.syntax);
        syntax.visit_stmt(
            stmt,
            &Context {
                checker: self,
                state,
            },
        );
        self.syntax = syntax;
    }

    fn expr(&mut self, expr: &Expr, state: &ScopeState) {
        let mut syntax = std::mem::take(&mut self.syntax);
        syntax.visit_expr(
            expr,
            &Context {
                checker: self,
                state,
            },
        );
        self.syntax = syntax;
    }
}

/// the checker's questions, answered at one node
struct Context<'a, 'src> {
    checker: &'a Checker<'src>,
    state: &'a ScopeState,
}

impl Context<'_, '_> {
    /// the scopes around the node, innermost first, past the annotation and type
    /// parameter scopes, which the checker's questions look through
    fn scopes(&self) -> impl Iterator<Item = &Scope> {
        self.state
            .scopes()
            .iter()
            .rev()
            .filter(|scope| !matches!(scope, Scope::Annotation | Scope::TypeParams))
    }
}

impl SemanticSyntaxContext for Context<'_, '_> {
    fn future_annotations_or_stub(&self) -> bool {
        self.checker.future_annotations_or_stub
    }

    fn lazy_import_context(&self) -> Option<LazyImportContext> {
        match self.scopes().next() {
            Some(Scope::Function { .. }) => return Some(LazyImportContext::Function),
            Some(Scope::Class) => return Some(LazyImportContext::Class),
            _ => {}
        }
        self.state
            .in_try()
            .then_some(LazyImportContext::TryExceptBlocks)
    }

    fn python_version(&self) -> PythonVersion {
        self.checker.python_version
    }

    fn source(&self) -> &str {
        self.checker.source
    }

    fn global(&self, _name: &str) -> Option<TextRange> {
        None
    }

    fn has_nonlocal_binding(&self, _name: &str) -> bool {
        true
    }

    fn in_async_context(&self) -> bool {
        for scope in self.scopes() {
            match scope {
                Scope::Class | Scope::Lambda => return false,
                Scope::Function { is_async } => return *is_async,
                _ => {}
            }
        }
        false
    }

    fn in_await_allowed_context(&self) -> bool {
        for scope in self.scopes() {
            match scope {
                Scope::Class => return false,
                Scope::Function { .. }
                | Scope::Lambda
                | Scope::Comprehension {
                    generator: true, ..
                } => return true,
                _ => {}
            }
        }
        false
    }

    fn in_yield_allowed_context(&self) -> bool {
        matches!(
            self.scopes().next(),
            Some(Scope::Function { .. } | Scope::Lambda)
        )
    }

    fn in_sync_comprehension(&self) -> bool {
        self.scopes().any(|scope| {
            matches!(
                scope,
                Scope::Comprehension {
                    generator: false,
                    is_async: false
                }
            )
        })
    }

    fn in_class_body_comprehension(&self) -> bool {
        for scope in self.scopes() {
            match scope {
                Scope::Comprehension { .. } => {}
                Scope::Class => return true,
                _ => return false,
            }
        }
        false
    }

    fn in_module_scope(&self) -> bool {
        self.state.scopes().is_empty()
    }

    fn in_function_scope(&self) -> bool {
        matches!(
            self.scopes().next(),
            Some(Scope::Function { .. } | Scope::Lambda)
        )
    }

    fn def_is_method(&self) -> bool {
        // a definition is checked before the scope it opens is entered
        matches!(self.state.scopes().last(), Some(Scope::Class))
    }

    fn in_generator_context(&self) -> bool {
        self.scopes().any(|scope| {
            matches!(
                scope,
                Scope::Comprehension {
                    generator: true,
                    ..
                }
            )
        })
    }

    fn in_notebook(&self) -> bool {
        false
    }

    fn report_semantic_error(&self, error: SemanticSyntaxError) {
        // an `await` outside a function is reported as outside an async function too,
        // which says the same thing
        if matches!(
            error.kind,
            SemanticSyntaxErrorKind::YieldOutsideFunction(YieldOutsideFunctionKind::Await)
        ) {
            return;
        }
        self.checker.errors.borrow_mut().push(error);
    }

    fn in_loop_context(&self) -> bool {
        self.state.in_loop()
    }

    fn is_bound_parameter(&self, _name: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::{PySourceType, PythonVersion};
    use ruff_python_parser::parse_unchecked_source;

    fn first_error(source: &str, min_version: PythonVersion) -> Option<String> {
        let parsed = parse_unchecked_source(source, PySourceType::Python);
        super::first_error(parsed.suite(), source, min_version, false)
            .map(|error| error.to_string())
    }

    /// what python refuses to compile, whatever scope it is written in
    #[test]
    fn refuses_what_python_does_not_compile() {
        for source in [
            "[x for x in (t := xs)]\n",
            "def f():\n    return [x for y in z for x in (lambda: (t := 1))()]\n",
            "class C:\n    ys = [(t := x) for x in xs]\n",
            "class C:\n    ys = [x for x in xs if [(t := 1) for _ in z]]\n",
            "def f(a, a):\n    pass\n",
            "def f():\n    return [(yield) for x in xs]\n",
            "class C:\n    yield 1\n",
            "break\n",
            "for x in xs:\n    pass\nelse:\n    continue\n",
            "while x:\n    def f():\n        break\n",
            "for x in xs:\n    class C:\n        continue\n",
            "class C[T]((t := 1)):\n    pass\n",
        ] {
            assert!(
                first_error(source, PythonVersion::PY313).is_some(),
                "accepted: {source}"
            );
        }
    }

    /// and what it compiles
    #[test]
    fn accepts_what_python_compiles() {
        for source in [
            "def f():\n    return [(t := x) for x in xs]\n",
            "class C:\n    y = (t := 1)\n    ys = [(lambda: (t := x))() for x in xs]\n",
            "class C:\n    def f(self, y=(t := 1)):\n        return [(u := x) for x in xs]\n",
            "def f():\n    yield 1\n    g = lambda: (yield)\n",
            "async def f():\n    return [await x for x in xs], (await x for x in xs)\n",
            "def f():\n    return (await x for x in xs)\n",
            "x = [x for x in ([t for t in (f(),) if t is not None] or [[]])[0]]\n",
            "for x in xs:\n    if x:\n        break\n    try:\n        continue\n    finally:\n        pass\n",
            "while x:\n    with y:\n        break\nelse:\n    pass\n",
            "class C[T](b[([t for t in (f(),) if t is not None] or [0])[0]]):\n    pass\n",
        ] {
            assert_eq!(first_error(source, PythonVersion::PY313), None, "{source}");
        }
    }

    /// a check that changed between versions applies on every version the output runs on
    #[test]
    fn checks_every_version_the_output_runs_on() {
        let source = "def f(x: (t := int)):\n    pass\n";
        assert!(first_error(source, PythonVersion::PY313).is_some());
    }
}
