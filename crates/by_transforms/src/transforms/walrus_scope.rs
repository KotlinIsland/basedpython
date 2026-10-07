//! where a lowering may bind a temporary with `:=`
//!
//! a lowering that reads a value twice but may evaluate it only once — `a ?? b`,
//! `f()?.x` — binds it to a temporary with an assignment expression. python refuses
//! one:
//!
//! - anywhere inside a comprehension's iterable, at any depth, a lambda's body
//!   included: `[x for x in (t := xs)]`
//! - directly in an annotation, from 3.14: `x: (t := int) = 1`
//! - directly in the scope of a generic definition's type parameters: a bound or a
//!   default, a generic class's base, a generic function's annotation, a `type`
//!   alias's value: `class C[T](base(t := 1)): ...`
//! - in a comprehension whose nearest enclosing scope, past comprehensions and
//!   annotations, is a class body or type parameters' scope:
//!   `class C: ys = [(t := x) for x in xs]`
//!
//! a lambda's body is a function scope, where none of this applies short of a
//! comprehension iterable. where python refuses `:=` the lowering writes its temporary
//! as a comprehension variable instead, which python accepts in any expression

use std::collections::HashSet;

use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::python_scopes::{self, Scope, ScopeHooks, ScopeState};

/// the expressions of a module where python accepts an assignment expression
///
/// what is recorded is where one is accepted, so a position the walk never reached
/// — or regions nobody collected — answers that it is refused, and a lowering falls
/// back to the form python accepts everywhere
#[derive(Default)]
pub(crate) struct WalrusRegions {
    allowed: HashSet<TextRange>,
    /// a range two expressions share is refused if either refuses it
    refused: HashSet<TextRange>,
}

impl WalrusRegions {
    pub(crate) fn collect(stmts: &[Stmt]) -> Self {
        let mut regions = WalrusRegions::default();
        python_scopes::walk(stmts, &mut regions);
        regions
    }

    /// whether an assignment expression written in place of the expression at `range`
    /// is accepted
    pub(crate) fn allows(&self, range: TextRange) -> bool {
        self.allowed.contains(&range) && !self.refused.contains(&range)
    }
}

impl ScopeHooks<'_> for WalrusRegions {
    fn expr(&mut self, expr: &Expr, state: &ScopeState) {
        if walrus_allowed(state) {
            self.allowed.insert(expr.range());
        } else {
            self.refused.insert(expr.range());
        }
    }
}

fn walrus_allowed(state: &ScopeState) -> bool {
    if state.in_iterable() {
        return false;
    }
    match state.scopes().last() {
        Some(Scope::Annotation | Scope::TypeParams) => false,
        Some(Scope::Comprehension { .. }) => !matches!(
            state
                .scopes()
                .iter()
                .rev()
                .find(|scope| !matches!(scope, Scope::Comprehension { .. } | Scope::Annotation)),
            Some(Scope::Class | Scope::TypeParams)
        ),
        Some(Scope::Class | Scope::Function { .. } | Scope::Lambda) | None => true,
    }
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::visitor::{Visitor, walk_expr};
    use ruff_python_ast::{Expr, PySourceType};
    use ruff_python_parser::parse_unchecked_source;
    use ruff_text_size::{Ranged, TextRange};

    use super::WalrusRegions;

    /// each source with a name `hole`, and whether python 3.12, 3.13 and 3.14 all
    /// compile `(t := f())` written in its place — on every version that compiles the
    /// source at all. measured by compiling each one, with the walrus and with the
    /// comprehension form the lowering writes instead, which compiles in all of them
    const POSITIONS: &[(&str, bool)] = &[
        ("x = hole", true),
        ("x = [hole for a in b]", true),
        ("x = [a for a in b if hole]", true),
        ("x = [a for a in hole]", false),
        ("x = [a for c in d for a in hole]", false),
        ("x = [a for a in (lambda: hole)()]", false),
        ("x = [a for a in [hole for c in d]]", false),
        ("x = {hole: 1 for a in b}", true),
        ("x = {a: hole for a in b}", true),
        ("x = {hole for a in b}", true),
        ("x = (hole for a in b)", true),
        ("x = (a for a in hole)", false),
        ("x = lambda: hole", true),
        ("x = lambda y=hole: y", true),
        ("x = [(lambda: hole) for a in b]", true),
        ("x = [(lambda y=hole: y) for a in b]", true),
        ("def f():\n    return [hole for a in b]", true),
        ("def f():\n    return [a for a in hole]", false),
        ("def f(y=hole):\n    pass", true),
        ("def f(y: hole):\n    pass", false),
        ("def f() -> hole:\n    pass", false),
        ("def f(*y: hole):\n    pass", false),
        ("@dec(hole)\ndef f():\n    pass", true),
        ("class C:\n    x = hole", true),
        ("class C:\n    x = [hole for a in b]", false),
        ("class C:\n    x = [a for a in b if hole]", false),
        ("class C:\n    x = [a for a in hole]", false),
        ("class C:\n    x = [[hole for a in b] for c in d]", false),
        ("class C:\n    x = [(lambda: hole) for a in b]", true),
        ("class C:\n    x = [(lambda y=hole: y) for a in b]", false),
        ("class C:\n    x = lambda: hole", true),
        ("class C:\n    def f(self, y=hole):\n        pass", true),
        (
            "class C:\n    @dec(hole)\n    def f(self):\n        pass",
            true,
        ),
        ("class C:\n    x: hole = 1", false),
        ("class C:\n    x: int = hole", true),
        ("class C(hole):\n    pass", true),
        ("class C(metaclass=hole):\n    pass", true),
        ("@dec(hole)\nclass C:\n    pass", true),
        ("class C[T](hole):\n    pass", false),
        ("class C[T](metaclass=hole):\n    pass", false),
        ("class C[T](b[[hole for a in c]]):\n    pass", false),
        ("class C[T](b[lambda: hole]):\n    pass", true),
        ("class C[T: hole]:\n    pass", false),
        ("class C[T = hole]:\n    pass", false),
        ("@dec(hole)\nclass C[T]:\n    pass", true),
        ("class C[T]:\n    x = hole", true),
        ("def f[T](y=hole):\n    pass", true),
        ("def f[T](y: hole):\n    pass", false),
        ("def f[T]() -> hole:\n    pass", false),
        ("def f[T: hole]():\n    pass", false),
        ("@dec(hole)\ndef f[T]():\n    pass", true),
        ("def f[T]():\n    return hole", true),
        ("type A = hole", false),
        ("type A = b[[hole for a in c]]", false),
        ("type A = b[lambda: hole]", true),
        ("type A[T] = hole", false),
        ("type A[T: hole] = int", false),
        ("x: hole = 1", false),
        ("x: int = hole", true),
        ("x: b[[hole for a in c]] = 1", true),
        ("x: b[lambda: hole] = 1", true),
        ("x: b[[a for a in hole]] = 1", false),
        ("def f():\n    x: hole = 1", false),
        ("def f():\n    x: b[[hole for a in c]] = 1", true),
        ("def f(y: b[lambda: hole]):\n    pass", true),
        ("for a in hole:\n    pass", true),
        ("with hole as a:\n    pass", true),
        ("x = f\"{hole}\"", true),
        ("match hole:\n    case _:\n        pass", true),
        ("match a:\n    case _ if hole:\n        pass", true),
        ("assert hole", true),
        ("x = [a for a in b if (lambda: hole)()]", true),
        ("x = [[a for a in hole] for c in d]", false),
        ("async def f():\n    return [a for a in await hole]", false),
        ("def f[T](y: b[[hole for a in c]]):\n    pass", false),
        ("def f[T](y: b[lambda: hole]):\n    pass", true),
        ("def f(y: b[[hole for a in c]]):\n    pass", true),
        ("class C[T]:\n    x: hole = 1", false),
        ("class C[T]:\n    x: b[[hole for a in c]] = 1", false),
        ("class C:\n    x: b[[hole for a in c]] = 1", false),
        ("class C:\n    x = {a: hole for a in b}", false),
        ("class C:\n    x = (hole for a in b)", false),
        ("class C:\n    x = [a for a in b if (lambda: hole)()]", true),
        (
            "class C:\n    def f(self):\n        return [hole for a in b]",
            true,
        ),
        (
            "class C:\n    class D:\n        x = [hole for a in b]",
            false,
        ),
        (
            "def f():\n    class D:\n        x = [hole for a in b]",
            false,
        ),
        (
            "class C[T](b[(lambda: [hole for a in c])()]):\n    pass",
            true,
        ),
        (
            "class C[T](b[[(lambda: hole) for a in c]]):\n    pass",
            true,
        ),
        ("type A = b[[(lambda: hole) for a in c]]", true),
        ("x: b[[a for a in c if hole]] = 1", true),
        ("x: (lambda y=hole: y) = 1", false),
        ("type A = (lambda y=hole: y)", false),
        ("class C[T](b[lambda y=hole: y]):\n    pass", false),
        ("x = [lambda: [hole for a in b] for c in d]", true),
        (
            "class C:\n    x = [lambda: [hole for a in b] for c in d]",
            true,
        ),
        ("def f(y=[hole for a in b]):\n    pass", true),
        (
            "class C:\n    def f(self, y=[hole for a in b]):\n        pass",
            false,
        ),
        ("@dec([hole for a in b])\ndef f():\n    pass", true),
        (
            "class C:\n    @dec([hole for a in b])\n    def f(self):\n        pass",
            false,
        ),
        (
            "class C:\n    def f(self, y: b[[hole for a in c]]):\n        pass",
            false,
        ),
        (
            "class C:\n    def f(self) -> b[[hole for a in c]]:\n        pass",
            false,
        ),
        (
            "class C:\n    def f(self, y: b[lambda: hole]):\n        pass",
            true,
        ),
        ("def f():\n    x: b[[hole for a in c]] = 1", true),
        ("class C:\n    type A = b[lambda: hole]", true),
        ("class C[T]:\n    def f(self, y=hole):\n        pass", true),
        ("class C[T]:\n    x = [hole for a in b]", false),
        ("x = lambda: [hole for a in b]", true),
        ("class C:\n    x = lambda: [hole for a in b]", true),
    ];

    fn hole(source: &str) -> TextRange {
        struct Hole(Option<TextRange>);
        impl<'a> Visitor<'a> for Hole {
            fn visit_expr(&mut self, expr: &'a Expr) {
                if let Expr::Name(name) = expr
                    && name.id.as_str() == "hole"
                {
                    self.0 = Some(name.range());
                }
                walk_expr(self, expr);
            }
        }
        let parsed = parse_unchecked_source(source, PySourceType::Python);
        assert!(
            parsed.errors().is_empty(),
            "{source}: {:?}",
            parsed.errors()
        );
        let mut hole = Hole(None);
        hole.visit_body(&parsed.syntax().body);
        hole.0.unwrap_or_else(|| panic!("no `hole` in {source}"))
    }

    /// a position nobody recorded is refused: regions left empty, or a range the
    /// walk never reached
    #[test]
    fn an_unknown_position_is_refused() {
        let source = "x = hole\n";
        assert!(!WalrusRegions::default().allows(hole(source)));
        let parsed = parse_unchecked_source(source, PySourceType::Python);
        let regions = WalrusRegions::collect(&parsed.syntax().body);
        assert!(regions.allows(hole(source)));
        // `x =` is no expression
        assert!(!regions.allows(TextRange::new(0.into(), 3.into())));
    }

    #[test]
    fn agrees_with_python_on_every_position() {
        let mismatches: Vec<String> = POSITIONS
            .iter()
            .filter_map(|&(source, python)| {
                let parsed = parse_unchecked_source(source, PySourceType::Python);
                let ours = WalrusRegions::collect(&parsed.syntax().body).allows(hole(source));
                (ours != python).then(|| format!("python {python}, ours {ours}: {source:?}"))
            })
            .collect();
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }
}
