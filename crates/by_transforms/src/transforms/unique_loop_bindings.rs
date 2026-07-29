//! per-iteration loop bindings.
//!
//! python gives a loop exactly one binding for its target, shared by every
//! iteration, so a closure made inside the body reads whatever the variable
//! holds *later* rather than what it held when the closure was made:
//!
//! ```py
//! fns = []
//! for i in [1, 2, 3]:
//!     fns.append(lambda: print(i))
//! ```
//!
//! in python every `fn()` prints `3`. basedpython gives each iteration its own
//! binding — the same change go made in 1.22 — so the calls print `1`, `2`,
//! `3`. the same applies to a comprehension's target, which has the identical
//! one-cell-per-comprehension behaviour.
//!
//! the lowering binds the captured value at the point the closure is created.
//! an expression closure (a `lambda`, a generator expression) is applied to the
//! values through a wrapper whose parameters shadow them, so the closure body
//! is untouched and closes over the wrapper's fresh parameter cells:
//!
//! ```py
//! fns.append((lambda i: lambda: print(i))(i))
//! ```
//!
//! a `def` is a statement and cannot be wrapped that way, so it gets a
//! decorator instead: [`LOOP_BIND_RUNTIME`] rebuilds the function with fresh
//! cells for the captured names, carrying every other cell (outer locals, the
//! implicit `__class__` of a zero-argument `super()`, a reified type parameter)
//! across untouched. the decorator is inserted innermost, below any user
//! decorator, so it always receives the raw function whose closure it rebuilds.
//!
//! only names the closure actually reads *through* the loop's binding are
//! captured: a name the closure binds itself, one an intervening scope binds,
//! and one it declares `global` / `nonlocal` (a write through the loop's own
//! cell, which a fresh cell would swallow) are all left alone. ty's semantic
//! index answers each of those, so shadowing is decided by real name
//! resolution rather than a syntactic guess.
//!
//! two things stay python's: a target rebound *inside* the body after a closure
//! was made is not seen by that closure (the capture happens where the closure
//! is written, not at the end of the iteration), and a `while` loop has no
//! target to bind.

use ruff_python_ast::helpers::has_written_def_header;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{AnyParameterRef, Comprehension, Expr, ExprName, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::source_util::{line_indent, line_start};
use crate::type_info::{CaptureKind, TypeInfo};

/// rebuilds a function with fresh cells for the loop bindings it captured, so
/// the iteration that defined it keeps its own values. cells the call does not
/// name — outer locals, `__class__`, reified type parameters — are carried
/// over, as are the attributes `FunctionType` does not copy
pub(crate) const LOOP_BIND_RUNTIME: &str = "\
def _by_loop_bind(**_by_values):
    def _by_rebind(_by_fn):
        _by_code = _by_fn.__code__
        _by_bound = FunctionType(
            _by_code,
            _by_fn.__globals__,
            _by_fn.__name__,
            _by_fn.__defaults__,
            tuple(
                CellType(_by_values[_by_name]) if _by_name in _by_values else _by_cell
                for _by_name, _by_cell in zip(_by_code.co_freevars, _by_fn.__closure__ or ())
            ),
        )
        _by_bound.__kwdefaults__ = _by_fn.__kwdefaults__
        _by_bound.__qualname__ = _by_fn.__qualname__
        _by_bound.__doc__ = _by_fn.__doc__
        _by_bound.__dict__.update(_by_fn.__dict__)
        if hasattr(_by_fn, \"__annotate__\"):
            _by_bound.__annotate__ = _by_fn.__annotate__
        else:
            _by_bound.__annotations__ = _by_fn.__annotations__
        if hasattr(_by_fn, \"__type_params__\"):
            _by_bound.__type_params__ = _by_fn.__type_params__
        return _by_bound
    return _by_rebind
";

/// which of a loop's bindings a lowering can bind by value
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// a wrapper's parameter shadows the name wherever the loop's own binding
    /// lives, so both kinds are reachable
    AnyBinding,
    /// the rebind swaps a function's closure cells, so it only reaches a
    /// binding python compiled *as* a cell — a module-level loop target is read
    /// from inside a `def` as a global, with no cell to swap
    ClosureCell,
}

impl Reach {
    fn covers(self, kind: CaptureKind) -> bool {
        match self {
            Self::AnyBinding => true,
            Self::ClosureCell => kind == CaptureKind::Nonlocal,
        }
    }
}

/// whether a generator expression publishes a name into the scope around it.
/// an assignment expression inside a comprehension binds in the *containing*
/// scope, walking out through any comprehension in between, so moving the
/// generator into a wrapper would move that binding with it — the name would
/// stop arriving where it was written to arrive. a `lambda` in between stops
/// the walk: it is a function scope, and a walrus binds inside it
fn publishes_a_binding(generator: &Expr) -> bool {
    struct Walrus(bool);

    impl<'ast> Visitor<'ast> for Walrus {
        fn visit_expr(&mut self, expr: &'ast Expr) {
            match expr {
                Expr::Named(_) => self.0 = true,
                Expr::Lambda(_) => return,
                _ => {}
            }
            walk_expr(self, expr);
        }
    }

    let mut walrus = Walrus(false);
    walrus.visit_expr(generator);
    walrus.0
}

/// every name a subtree reads, and the names it declares `global` / `nonlocal`
/// anywhere inside it
#[derive(Default)]
struct References<'ast> {
    reads: Vec<&'ast ExprName>,
    declared: Vec<&'ast str>,
}

impl<'ast> Visitor<'ast> for References<'ast> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_load()
        {
            self.reads.push(name);
        }
        walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::Global(global) => self
                .declared
                .extend(global.names.iter().map(|name| name.id.as_str())),
            Stmt::Nonlocal(nonlocal) => self
                .declared
                .extend(nonlocal.names.iter().map(|name| name.id.as_str())),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

struct UniqueLoopBindings<'a, 'ast> {
    source: &'a str,
    types: &'a dyn TypeInfo,
    /// loop and comprehension targets whose bindings the statements currently
    /// being walked run inside, in binding order
    active: Vec<&'ast ExprName>,
    /// wrapper applications around expression closures
    wraps: Vec<(TextRange, Vec<Fragment>)>,
    /// `@_by_loop_bind(…)` insertions above `def` headers
    decorators: Vec<(TextRange, String)>,
    /// at least one `def` was decorated — emit the rebind runtime
    used_runtime: bool,
}

impl<'a, 'ast> UniqueLoopBindings<'a, 'ast> {
    fn new(source: &'a str, types: &'a dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            active: Vec::new(),
            wraps: Vec::new(),
            decorators: Vec::new(),
            used_runtime: false,
        }
    }

    /// the active loop bindings the subtree captures, in binding order. a name
    /// bound by two nested loops resolves to the inner one, so only the
    /// innermost target of each name is asked
    fn captured(&self, references: &References<'ast>, reach: Reach) -> Vec<String> {
        let mut innermost: Vec<&ExprName> = Vec::new();
        for target in &self.active {
            match innermost.iter_mut().find(|held| held.id == target.id) {
                Some(held) => *held = target,
                None => innermost.push(target),
            }
        }
        innermost
            .into_iter()
            .filter(|target| !references.declared.contains(&target.id.as_str()))
            .filter(|target| {
                references.reads.iter().any(|read| {
                    read.id == target.id
                        && self
                            .types
                            .reads_binding_of(read, target)
                            .is_some_and(|kind| reach.covers(kind))
                })
            })
            .map(|target| target.id.to_string())
            .collect()
    }

    fn captured_in_expr(&self, expr: &'ast Expr, reach: Reach) -> Vec<String> {
        let mut references = References::default();
        references.visit_expr(expr);
        self.captured(&references, reach)
    }

    fn captured_in_stmt(&self, stmt: &'ast Stmt, reach: Reach) -> Vec<String> {
        let mut references = References::default();
        references.visit_stmt(stmt);
        self.captured(&references, reach)
    }

    /// apply the closure to the captured values through a wrapper whose
    /// parameters shadow them: `lambda: i` → `(lambda i: lambda: i)(i)`
    fn wrap(&mut self, expr: &'ast Expr, names: &[String]) {
        let arguments = names.join(", ");
        // a generator expression written as a call's sole argument carries no
        // parentheses of its own, and `lambda i: x for x in y` is not an
        // expression — so give the passthrough its own
        let (open, close) = match expr {
            Expr::Generator(generator) if !generator.parenthesized => ("(", ")"),
            _ => ("", ""),
        };
        self.wraps.push((
            expr.range(),
            vec![
                Fragment::Lit(format!("(lambda {arguments}: {open}")),
                Fragment::Src(expr.range()),
                Fragment::Lit(format!("{close})({arguments})")),
            ],
        ));
    }

    /// rebuild the function's closure with the captured values. the decorator
    /// goes on its own line directly above the `def` / `async def` header —
    /// innermost, below any user decorator — sharing its indentation. returns
    /// whether the function was bound
    fn decorate(&mut self, function: &StmtFunctionDef, names: &[String]) -> bool {
        let name_start = function.name.range().start();
        let indent = line_indent(self.source, name_start);
        let anchor = line_start(self.source, name_start) + TextSize::of(indent);
        // a function the parser synthesized — a trailing-lambda block, a
        // property accessor — has no header in the source to decorate, and the
        // pass that owns the construct re-emits the whole statement anyway. for
        // a trailing-lambda block that is the right division: whether its
        // closure outlives the iteration is decided by the callee's `local` /
        // `once` marker, which ty checks as `escaping-loop-variable`. `B023`
        // reports what this leaves unbound, and shares the predicate
        if !has_written_def_header(self.source, function) {
            return false;
        }
        let bindings = names
            .iter()
            .map(|name| format!("{name}={name}"))
            .collect::<Vec<_>>()
            .join(", ");
        self.decorators.push((
            TextRange::empty(anchor),
            format!("@_by_loop_bind({bindings})\n{indent}"),
        ));
        self.used_runtime = true;
        true
    }

    fn push_targets(&mut self, target: &'ast Expr) {
        match target {
            Expr::Name(name) => self.active.push(name),
            Expr::Tuple(tuple) => {
                for element in &tuple.elts {
                    self.push_targets(element);
                }
            }
            Expr::List(list) => {
                for element in &list.elts {
                    self.push_targets(element);
                }
            }
            Expr::Starred(starred) => self.push_targets(&starred.value),
            // an attribute / subscript target is not a binding a closure can
            // capture — it writes through an object that outlives the loop
            _ => {}
        }
    }

    /// walk a closure's children with the bindings it just captured retired:
    /// they now name the wrapper's parameters (or the rebuilt cells), so a
    /// closure nested inside reads the frozen value already
    fn walk_captured<F>(&mut self, captured: &[String], walk: F)
    where
        F: FnOnce(&mut Self),
    {
        if captured.is_empty() {
            walk(self);
            return;
        }
        let saved = self.active.clone();
        self.active
            .retain(|target| !captured.iter().any(|name| name == target.id.as_str()));
        walk(self);
        self.active = saved;
    }

    /// the element expressions and generators of any comprehension form
    fn comprehension_parts(expr: &'ast Expr) -> Option<(&'ast [Comprehension], Vec<&'ast Expr>)> {
        match expr {
            Expr::ListComp(comp) => Some((&comp.generators, vec![&comp.elt])),
            Expr::SetComp(comp) => Some((&comp.generators, vec![&comp.elt])),
            Expr::Generator(comp) => Some((&comp.generators, vec![&comp.elt])),
            // a dict comprehension's key is optional in basedpython (`{v for …}`
            // over a mapping keeps the value alone)
            Expr::DictComp(comp) => Some((
                &comp.generators,
                comp.key
                    .as_deref()
                    .into_iter()
                    .chain(std::iter::once(&*comp.value))
                    .collect(),
            )),
            _ => None,
        }
    }

    fn walk_comprehension(&mut self, expr: &'ast Expr) {
        let Some((generators, elements)) = Self::comprehension_parts(expr) else {
            return;
        };
        let Some(first) = generators.first() else {
            return;
        };
        // the outermost iterable is evaluated in the enclosing scope, before
        // the comprehension's own bindings exist
        self.visit_expr(&first.iter);
        let depth = self.active.len();
        for generator in generators {
            self.push_targets(&generator.target);
        }
        for generator in generators.iter().skip(1) {
            self.visit_expr(&generator.iter);
        }
        for condition in generators.iter().flat_map(|generator| &generator.ifs) {
            self.visit_expr(condition);
        }
        for element in elements {
            self.visit_expr(element);
        }
        self.active.truncate(depth);
    }
}

impl<'ast> Visitor<'ast> for UniqueLoopBindings<'_, 'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::For(for_stmt) => {
                self.visit_expr(&for_stmt.iter);
                let depth = self.active.len();
                self.push_targets(&for_stmt.target);
                for body_stmt in &for_stmt.body {
                    self.visit_stmt(body_stmt);
                }
                self.active.truncate(depth);
                // the `else` clause runs once, after the target's last value —
                // there is no iteration to bind
                for orelse_stmt in &for_stmt.orelse {
                    self.visit_stmt(orelse_stmt);
                }
            }
            Stmt::FunctionDef(function) if !self.active.is_empty() => {
                let rebound = self.captured_in_stmt(stmt, Reach::ClosureCell);
                let bound = !rebound.is_empty() && self.decorate(function, &rebound);
                // decorators and annotations are evaluated where the `def`
                // runs — once per iteration, in the loop's own scope — so the
                // bindings stay live for a closure written in one of them. a
                // *default* is not: [`mutable_defaults`](super::mutable_defaults)
                // moves every non-scalar default into the body, where it is
                // re-evaluated per call against the binding rebound below
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }
                if let Some(type_params) = &function.type_params {
                    self.visit_type_params(type_params);
                }
                for annotation in function
                    .parameters
                    .iter()
                    .filter_map(AnyParameterRef::annotation)
                    .chain(function.returns.as_deref())
                {
                    self.visit_annotation(annotation);
                }
                // a binding the rebind cannot *reach* — a module-level target,
                // read as a global — is retired even though nothing was bound:
                // freezing a closure inside the body would bind it where the
                // body runs, which is neither python's reading nor an
                // iteration's. when the function was simply not ours to
                // decorate (a synthesized header), its body keeps every binding
                // live, so a closure written inside it is still bound
                let mut retired = self.captured_in_stmt(stmt, Reach::AnyBinding);
                if !bound {
                    retired.retain(|name| !rebound.contains(name));
                }
                self.walk_captured(&retired, |this| this.visit_body(&function.body));
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Lambda(_) | Expr::Generator(_) if !self.active.is_empty() => {
                let captured = self.captured_in_expr(expr, Reach::AnyBinding);
                // a generator that publishes a binding outward keeps python's
                // reading of the loop variable rather than lose the binding
                let captured = if expr.is_generator_expr() && publishes_a_binding(expr) {
                    Vec::new()
                } else {
                    captured
                };
                if !captured.is_empty() {
                    self.wrap(expr, &captured);
                }
                self.walk_captured(&captured, |this| {
                    if expr.is_generator_expr() {
                        this.walk_comprehension(expr);
                    } else {
                        walk_expr(this, expr);
                    }
                });
            }
            // a generator expression reaches here only with no binding to
            // capture; it still binds its own targets for the closures inside
            Expr::ListComp(_) | Expr::SetComp(_) | Expr::DictComp(_) | Expr::Generator(_) => {
                self.walk_comprehension(expr);
            }
            _ => walk_expr(self, expr),
        }
    }
}

pub(crate) struct UniqueLoopBindingsPass<'src> {
    source: &'src str,
    enabled: bool,
}

impl<'src> UniqueLoopBindingsPass<'src> {
    pub(crate) fn new(source: &'src str, enabled: bool) -> Self {
        Self { source, enabled }
    }
}

impl TypeAwarePass for UniqueLoopBindingsPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if !self.enabled {
            return;
        }
        let mut inner = UniqueLoopBindings::new(self.source, types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if inner.used_runtime {
            ctx.required_imports
                .push("from types import CellType, FunctionType".to_owned());
            ctx.required_imports.push(LOOP_BIND_RUNTIME.to_owned());
        }
        ctx.text_edits.extend(inner.decorators);
        ctx.template_edits.extend(inner.wraps);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// the decorated form emits the rebind runtime ahead of the body; the tests
    /// below assert the body only
    fn check_body(input: &str, expected: &str) {
        let out = transpile(input, &Config::test_default()).unwrap();
        let body = out
            .split_once("    return _by_rebind\n")
            .map(|(_, body)| body.trim_start_matches('\n').to_owned())
            .unwrap_or(out);
        assert_eq!(body, crate::python_passthrough::lazify_expected(expected));
    }

    #[test]
    fn lambda_captures_the_loop_target() {
        check(
            indoc! {"
                fns = []
                for i in [1, 2, 3]:
                    fns.append(lambda: print(i))
            "},
            indoc! {"
                fns = []
                for i in [1, 2, 3]:
                    fns.append((lambda i: lambda: print(i))(i))
            "},
        );
    }

    #[test]
    fn every_target_of_a_destructuring_loop_binds() {
        check(
            indoc! {"
                for left, right in pairs:
                    fns.append(lambda: (left, right))
            "},
            indoc! {"
                for left, right in pairs:
                    fns.append((lambda left, right: lambda: (left, right))(left, right))
            "},
        );
    }

    #[test]
    fn nested_loops_each_bind() {
        check(
            indoc! {"
                for i in rows:
                    for j in columns:
                        fns.append(lambda: (i, j))
            "},
            indoc! {"
                for i in rows:
                    for j in columns:
                        fns.append((lambda i, j: lambda: (i, j))(i, j))
            "},
        );
    }

    #[test]
    fn comprehension_target_binds_per_element() {
        check(
            "fns = [lambda: i for i in items]\n",
            "fns = [(lambda i: lambda: i)(i) for i in items]\n",
        );
    }

    #[test]
    fn a_bare_generator_argument_is_parenthesized() {
        check(
            indoc! {"
                for i in items:
                    print(sum(i * x for x in xs))
            "},
            indoc! {"
                for i in items:
                    print(sum((lambda i: (i * x for x in xs))(i)))
            "},
        );
    }

    #[test]
    fn a_parenthesized_generator_keeps_its_own_parentheses() {
        check(
            indoc! {"
                for i in items:
                    g = (i * x for x in xs)
            "},
            indoc! {"
                for i in items:
                    g = (lambda i: (i * x for x in xs))(i)
            "},
        );
    }

    #[test]
    fn a_def_is_decorated_below_its_user_decorators() {
        check_body(
            indoc! {"
                def register():
                    for i in items:
                        @app
                        def handler():
                            return i
            "},
            indoc! {"
                def register():
                    for i in items:
                        @app
                        @_by_loop_bind(i=i)
                        def handler():
                            return i
            "},
        );
    }

    #[test]
    fn a_method_of_a_class_in_a_loop_is_decorated() {
        check_body(
            indoc! {"
                def build():
                    for i in items:
                        class Holder:
                            def value(self):
                                return i
            "},
            indoc! {"
                def build():
                    for i in items:
                        class Holder:
                            @_by_loop_bind(i=i)
                            def value(self):
                                return i
            "},
        );
    }

    /// a decorator runs where the `def` does — in the loop's own scope, once
    /// per iteration — so a closure written there is wrapped, even though the
    /// body it belongs to is rebound
    #[test]
    fn a_closure_in_a_decorator_is_wrapped() {
        check_body(
            indoc! {"
                def build():
                    for i in items:
                        @wrap(lambda: i)
                        def handler():
                            return i
            "},
            indoc! {"
                def build():
                    for i in items:
                        @wrap((lambda i: lambda: i)(i))
                        @_by_loop_bind(i=i)
                        def handler():
                            return i
            "},
        );
    }

    /// a closure written as a *default* needs no wrapper: `mutable_defaults`
    /// moves it into the body, where it is re-evaluated per call against the
    /// binding the rebind froze
    #[test]
    fn a_closure_in_a_parameter_default_is_left_to_the_body_guard() {
        check_body(
            indoc! {"
                def build():
                    for i in items:
                        def handler(cb=lambda: i):
                            return cb
            "},
            indoc! {"
                def build():
                    for i in items:
                        @_by_loop_bind(i=i)
                        def handler(cb=_MISSING):
                            if cb is _MISSING:
                                cb = lambda: i
                            return cb
            "},
        );
    }

    /// a trailing-lambda block is a `def` the parser synthesized — there is no
    /// header in the source to decorate, and whether the block outlives the
    /// iteration is decided by its callee's `local` / `once` marker, which ty
    /// checks as `escaping-loop-variable`
    #[test]
    fn a_trailing_lambda_block_is_left_to_its_own_lowering() {
        let out = transpile(
            indoc! {"
                def run(fn: () -> None):
                    fn()

                def main():
                    for x in items:
                        run:
                            print(x)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("_by_loop_bind"), "got:\n{out}");
        assert!(out.contains("run(fn=_trailing_lambda_0)"), "got:\n{out}");
    }

    /// skipping a synthesized `def` must not take the binding out of its body:
    /// a closure written inside the block is still made once per iteration
    #[test]
    fn a_closure_inside_a_trailing_lambda_block_is_still_bound() {
        let out = transpile(
            indoc! {"
                def run(once fn: () -> None):
                    fn()

                def main():
                    for i in items:
                        run:
                            fns.append(lambda: i)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("_by_loop_bind"), "got:\n{out}");
        assert!(
            out.contains("fns.append((lambda i: lambda: i)(i))"),
            "got:\n{out}"
        );
    }

    /// same for a property accessor: the `def` is synthesized, and the
    /// construct that owns it emits the whole `@property` block
    #[test]
    fn a_property_accessor_is_left_to_its_own_lowering() {
        let out = transpile(
            indoc! {"
                def build():
                    for i in items:
                        class Holder:
                            var scale: int = 0
                                get() = field * i
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("_by_loop_bind"), "got:\n{out}");
        assert!(out.contains("return self.__scale * i"), "got:\n{out}");
    }

    /// the rebind swaps closure cells, and a module-level loop target is read
    /// from inside the `def` as a global — there is no cell to swap
    #[test]
    fn a_def_in_a_module_level_loop_is_left_alone() {
        unchanged(indoc! {"
            for i in items:
                def handler():
                    return i
        "});
    }

    /// an assignment expression in a generator binds in the scope *around* the
    /// generator, so wrapping it would carry that binding off with it. the loop
    /// variable keeps python's reading rather than the name go missing
    #[test]
    fn a_generator_that_publishes_a_binding_is_left_alone() {
        unchanged(indoc! {"
            for i in items:
                g = ((seen := x * i) for x in xs)
        "});
    }

    /// a walrus inside a nested lambda binds in that lambda, not around the
    /// generator, so it is no reason to leave the capture unbound. the inner
    /// lambda reads the generator's own target too, and is bound for it
    #[test]
    fn a_walrus_inside_a_nested_lambda_still_wraps() {
        check(
            indoc! {"
                for i in items:
                    g = (f(lambda: (seen := x * i)) for x in xs)
            "},
            indoc! {"
                for i in items:
                    g = (lambda i: (f((lambda x: lambda: (seen := x * i))(x)) for x in xs))(i)
            "},
        );
    }

    /// each closure in the body is treated on its own — retiring a binding for
    /// a rebound `def` must not leak into its siblings
    #[test]
    fn sibling_closures_are_bound_independently() {
        check_body(
            indoc! {"
                def build():
                    for i in items:
                        def handler():
                            return i
                        fns.append(handler)
                        fns.append(lambda: i)
            "},
            indoc! {"
                def build():
                    for i in items:
                        @_by_loop_bind(i=i)
                        def handler():
                            return i
                        fns.append(handler)
                        fns.append((lambda i: lambda: i)(i))
            "},
        );
    }

    /// name resolution skips a class body, so a method never reads a loop
    /// target bound there in the first place — binding one would invent a
    /// value python does not give it
    #[test]
    fn a_loop_in_a_class_body_binds_nothing_for_its_methods() {
        unchanged(indoc! {"
            class Holder:
                for i in items:
                    def value(self):
                        return i
        "});
    }

    /// a typed lambda's basedpython surface is removed by deleting it rather
    /// than by re-rendering the statement (see [`typed_lambda`](super::typed_lambda)),
    /// so the wrapper around the lambda survives
    #[test]
    fn a_typed_lambda_in_a_loop_is_wrapped() {
        check(
            indoc! {"
                for tag in tags:
                    fns.append(lambda (s: str): s + tag)
            "},
            indoc! {"
                for tag in tags:
                    fns.append((lambda tag: lambda s: s + tag)(tag))
            "},
        );
    }

    #[test]
    fn a_shadowing_parameter_is_left_alone() {
        unchanged(indoc! {"
            for i in items:
                fns.append(lambda i: i)
        "});
    }

    #[test]
    fn a_shadowing_comprehension_target_is_left_alone() {
        unchanged(indoc! {"
            for i in items:
                fns.append(lambda: [i for i in inner])
        "});
    }

    #[test]
    fn the_hand_written_default_idiom_is_left_alone() {
        unchanged(indoc! {"
            for i in items:
                fns.append(lambda i=i: i)
        "});
    }

    #[test]
    fn a_closure_that_captures_nothing_is_left_alone() {
        unchanged(indoc! {"
            for i in items:
                fns.append(lambda: 1)
                print(i)
        "});
    }

    /// the `else` clause runs once, after the target's last value
    #[test]
    fn the_else_clause_is_left_alone() {
        unchanged(indoc! {"
            for i in items:
                pass
            else:
                fns.append(lambda: i)
        "});
    }

    #[test]
    fn a_while_loop_has_no_target_to_bind() {
        unchanged(indoc! {"
            while n:
                fns.append(lambda: n)
        "});
    }

    /// a name a nested scope writes through is left on the loop's own cell —
    /// a fresh one would swallow the write
    #[test]
    fn a_nonlocal_write_through_is_left_alone() {
        check_body(
            indoc! {"
                def outer():
                    total = 0
                    for i in items:
                        def bump():
                            nonlocal total, i
                            total += i
            "},
            indoc! {"
                def outer():
                    total = 0
                    for i in items:
                        def bump():
                            nonlocal total, i
                            total += i
            "},
        );
    }

    #[test]
    fn disabled_leaves_every_loop_alone() {
        let source = indoc! {"
            for i in items:
                fns.append(lambda: i)
        "};
        let config = Config {
            unique_loop_bindings: false,
            ..Config::test_default()
        };
        assert_eq!(transpile(source, &config).unwrap(), source);
    }
}
