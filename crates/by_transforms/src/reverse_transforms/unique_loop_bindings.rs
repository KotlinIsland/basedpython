//! reverse of `crate::transforms::unique_loop_bindings`:
//!   `(lambda i: <closure>)(i)` → `<closure>`, and a `@_by_loop_bind(i=i)`
//!   decorator line → gone.
//!
//! both shapes are the *hand-written* python idiom as much as they are the
//! forward pass's output — a wrapper applied to the loop's own names, binding
//! each to a parameter of the same name, is how python code freezes a loop
//! variable today. basedpython binds it for you, so the wrapper is what the
//! `.by` source no longer needs.
//!
//! the unwrap only fires **inside the bindings it names**: outside a loop the
//! same shape is an ordinary application, and dropping it there would change
//! which value the closure reads. the wrapper is removed as two deletions —
//! its head and its tail — so the closure keeps its own source bytes and the
//! reverse rewrites inside it still apply.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Comprehension, Expr, ExprCall, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::transforms::source_util::line_start;

pub(crate) struct UniqueLoopBindingsReverse<'src> {
    source: &'src str,
    /// loop and comprehension target names the walked statements run inside
    active: Vec<String>,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> UniqueLoopBindingsReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            active: Vec::new(),
            edits: Vec::new(),
        }
    }

    /// whether the call is a wrapper application: a bare lambda applied to
    /// names that match its own parameters one for one, all of them bindings of
    /// a loop the call sits inside
    fn is_wrapper_application(&self, call: &ExprCall) -> bool {
        let Expr::Lambda(lambda) = call.func.as_ref() else {
            return false;
        };
        let Some(parameters) = lambda.parameters.as_deref() else {
            return false;
        };
        if parameters.vararg.is_some()
            || parameters.kwarg.is_some()
            || !parameters.posonlyargs.is_empty()
            || !parameters.kwonlyargs.is_empty()
            || parameters.args.iter().any(|arg| arg.default.is_some())
        {
            return false;
        }
        if !call.arguments.keywords.is_empty()
            || call.arguments.args.len() != parameters.args.len()
            || parameters.args.is_empty()
        {
            return false;
        }
        parameters.args.iter().zip(&*call.arguments.args).all(
            |(parameter, argument)| match argument {
                Expr::Name(name) => {
                    name.ctx.is_load()
                        && name.id == parameter.parameter.name.id
                        && self.active.iter().any(|target| *target == name.id)
                }
                _ => false,
            },
        )
    }

    /// delete the wrapper's head (`(lambda i: `) and tail (`)(i)`), keeping the
    /// closure between them exactly as written
    fn unwrap(&mut self, call: &ExprCall) {
        let Expr::Lambda(lambda) = call.func.as_ref() else {
            return;
        };
        // the tail deletion takes the lambda's closing parenthesis with it, so
        // the head has to take the opening one — without it the output is
        // unbalanced. ruff leaves parentheses out of a node's range, so it is
        // found in the source; when it cannot be (a comment sits between it and
        // the `lambda`), leave the wrapper alone rather than emit broken source
        let Some(head_start) = self.opening_parenthesis(lambda.range().start()) else {
            return;
        };
        self.edits.push(Fix::safe_edits(
            Edit::range_deletion(TextRange::new(head_start, lambda.body.range().start())),
            [Edit::range_deletion(TextRange::new(
                lambda.body.range().end(),
                call.range().end(),
            ))],
        ));
    }

    /// the offset of the `(` that opens the parenthesized expression starting
    /// at `start`, looking past whitespace of any kind — the wrapper may be
    /// written across lines
    fn opening_parenthesis(&self, start: TextSize) -> Option<TextSize> {
        let before = &self.source[..usize::from(start)];
        let rest = before.trim_end().strip_suffix('(')?;
        TextSize::try_from(rest.len()).ok()
    }

    /// drop a `@_by_loop_bind(…)` decorator, and its line with it
    fn undecorate(&mut self, function: &StmtFunctionDef) {
        for (index, decorator) in function.decorator_list.iter().enumerate() {
            let Expr::Call(call) = &decorator.expression else {
                continue;
            };
            if !matches!(call.func.as_ref(), Expr::Name(name) if name.id == "_by_loop_bind") {
                continue;
            }
            let next_start = match function.decorator_list.get(index + 1) {
                Some(next) => line_start(self.source, next.range().start()),
                None => line_start(self.source, function.name.range().start()),
            };
            let start = line_start(self.source, decorator.range().start());
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    start, next_start,
                ))));
        }
    }

    fn walk_comprehension(&mut self, generators: &[Comprehension], elements: &[&Expr]) {
        let Some(first) = generators.first() else {
            return;
        };
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

    fn push_targets(&mut self, target: &Expr) {
        match target {
            Expr::Name(name) => self.active.push(name.id.to_string()),
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
            _ => {}
        }
    }
}

impl<'ast> Visitor<'ast> for UniqueLoopBindingsReverse<'_> {
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
                for orelse_stmt in &for_stmt.orelse {
                    self.visit_stmt(orelse_stmt);
                }
            }
            Stmt::FunctionDef(function) => {
                self.undecorate(function);
                walk_stmt(self, stmt);
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Call(call) if self.is_wrapper_application(call) => {
                self.unwrap(call);
                walk_expr(self, expr);
            }
            Expr::ListComp(comp) => self.walk_comprehension(&comp.generators, &[&comp.elt]),
            Expr::SetComp(comp) => self.walk_comprehension(&comp.generators, &[&comp.elt]),
            Expr::Generator(comp) => self.walk_comprehension(&comp.generators, &[&comp.elt]),
            Expr::DictComp(comp) => {
                let elements: Vec<&Expr> = comp
                    .key
                    .as_deref()
                    .into_iter()
                    .chain(std::iter::once(&*comp.value))
                    .collect();
                self.walk_comprehension(&comp.generators, &elements);
            }
            _ => walk_expr(self, expr),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile, transpile};
    use indoc::indoc;

    fn rev(source: &str) -> String {
        reverse_transpile(source, &Config::test_default()).unwrap()
    }

    #[test]
    fn wrapper_application_is_unwrapped() {
        let out = rev(indoc! {"
            for i in items:
                fns.append((lambda i: lambda: print(i))(i))
        "});
        assert_eq!(
            out,
            indoc! {"
                for i in items:
                    fns.append(lambda: print(i))
            "}
        );
    }

    #[test]
    fn a_multi_name_wrapper_is_unwrapped() {
        let out = rev(indoc! {"
            for i in rows:
                for j in columns:
                    fns.append((lambda i, j: lambda: (i, j))(i, j))
        "});
        assert!(
            !out.contains("lambda i, j"),
            "wrapper should be gone: {out}"
        );
        assert!(out.contains("fns.append(lambda: (i, j))"), "got: {out}");
    }

    #[test]
    fn a_comprehension_wrapper_is_unwrapped() {
        let out = rev("fns = [(lambda i: lambda: i)(i) for i in items]\n");
        assert_eq!(out, "fns = [lambda: i for i in items]\n");
    }

    #[test]
    fn the_rebind_decorator_is_dropped() {
        let out = rev(indoc! {"
            def register():
                for i in items:
                    @app
                    @_by_loop_bind(i=i)
                    def handler():
                        return i
        "});
        assert!(!out.contains("_by_loop_bind"), "got: {out}");
        assert!(out.contains("    @app\n"), "user decorator stays: {out}");
        assert!(out.contains("def handler():"), "got: {out}");
    }

    /// outside the loop the same shape is an ordinary application — dropping it
    /// would change which value the closure reads
    #[test]
    fn an_application_outside_a_loop_is_preserved() {
        let out = rev("fns.append((lambda i: lambda: i)(i))\n");
        assert_eq!(out, "fns.append((lambda i: lambda: i)(i))\n");
    }

    /// the wrapper may be written across lines — the head deletion has to
    /// reach the parenthesis that opened it, or the output loses a bracket
    #[test]
    fn a_wrapper_split_across_lines_is_unwrapped_whole() {
        let out = rev(indoc! {"
            for i in items:
                fns.append((
                    lambda i: lambda: i
                )(i))
        "});
        assert_eq!(
            out,
            indoc! {"
                for i in items:
                    fns.append(lambda: i)
            "}
        );
    }

    /// and when the opening parenthesis cannot be identified — a comment sits
    /// between it and the `lambda` — the wrapper is left alone rather than
    /// half-deleted into unbalanced source
    #[test]
    fn a_wrapper_behind_a_comment_is_left_alone() {
        let source = indoc! {"
            for i in items:
                fns.append((  # note
                    lambda i: lambda: i
                )(i))
        "};
        assert_eq!(rev(source), source);
    }

    #[test]
    fn an_application_that_renames_is_preserved() {
        let out = rev(indoc! {"
            for i in items:
                fns.append((lambda n: lambda: n)(i))
        "});
        assert!(out.contains("(lambda n: lambda: n)(i)"), "got: {out}");
    }

    #[test]
    fn round_trip_rewraps() {
        let source = indoc! {"
            for i in items:
                fns.append(lambda: print(i))
        "};
        let lowered = transpile(source, &Config::test_default()).unwrap();
        assert_eq!(rev(&lowered), source);
    }
}
