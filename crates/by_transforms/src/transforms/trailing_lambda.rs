//! Lowering for statement-level trailing lambda blocks.
//!
//! The parser turns
//!
//! ```text
//! f(2):
//!     print(it)
//! ```
//!
//! into a synthetic `StmtFunctionDef` (`is_trailing_lambda`) whose single
//! decorator carries the called expression and whose body is the suite. The
//! lowering re-emits it as a named function followed by the call with the
//! function appended as its last argument:
//!
//! ```text
//! def _trailing_lambda_0(it=None):
//!     print(it)
//! f(2, a=_trailing_lambda_0)
//! ```
//!
//! The argument is passed by keyword — the callee's last declared parameter,
//! read from ty ([`TypeInfo::trailing_lambda_keyword`]) — so `f:` binds the
//! lambda to the last parameter even when earlier parameters are defaulted.
//! When the callee's signature is not inspectable the lambda is appended
//! positionally instead.
//!
//! The whole statement is one template edit: the suite and the called
//! expression pass through as [`Fragment::Src`] spans, so comments survive
//! and lowerings nested inside them (a `??` argument, a `cast` callee, a
//! nested trailing lambda in the body) still compose.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{ArgOrKeyword, Expr, ExprContext, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::source_util::line_indent;
use crate::type_info::{CaptureKind, TypeInfo};

struct TrailingLambdaLower<'a, 'src> {
    source: &'src str,
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    /// monotonic across the file so sibling lambdas get distinct names
    counter: usize,
}

/// Collects the `Name` targets *assigned* directly in a block — every rebinding,
/// via `=`, `for`, `with as`, `:=`, or augmented / annotated assignment (ruff
/// marks them all [`ExprContext::Store`]). Attribute / subscript targets
/// (`a.b = …`) don't rebind a name, so their `Load`-context root is skipped.
/// Nested functions, classes, lambdas, and comprehensions are their own scope
/// and are not descended into.
struct BlockAssignments<'ast> {
    names: Vec<&'ast Expr>,
}

impl<'ast> Visitor<'ast> for BlockAssignments<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if matches!(stmt, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            return;
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Name(name) if matches!(name.ctx, ExprContext::Store) => self.names.push(expr),
            Expr::Lambda(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::DictComp(_)
            | Expr::Generator(_) => {}
            _ => walk_expr(self, expr),
        }
    }
}

impl TrailingLambdaLower<'_, '_> {
    /// a function name that is unbound at the statement's scope and unused by
    /// earlier lowerings in this run
    fn fresh_name(&mut self, anchor: &Expr) -> String {
        loop {
            let name = format!("_trailing_lambda_{}", self.counter);
            self.counter += 1;
            if self.types.is_unbound_at(&name, anchor) {
                return name;
            }
        }
    }

    /// The `global` / `nonlocal` lines a block needs so its assignments write
    /// through to the enclosing scope instead of shadowing it with a fresh
    /// local. Returns the text to splice right after `def …(it):` — each
    /// declaration on its own indented line — or `None` when nothing is
    /// captured. This is what lets `f:\n    a = 2` update an enclosing `a`
    /// without a manual `nonlocal a`.
    fn capture_declarations(&self, function: &StmtFunctionDef) -> Option<String> {
        let mut collector = BlockAssignments { names: Vec::new() };
        for stmt in &function.body {
            collector.visit_stmt(stmt);
        }

        let mut seen: Vec<&str> = Vec::new();
        let mut globals: Vec<&str> = Vec::new();
        let mut nonlocals: Vec<&str> = Vec::new();
        for name_expr in &collector.names {
            let Expr::Name(name) = name_expr else {
                continue;
            };
            let id = name.id.as_str();
            // `it` is the block's own parameter, never a capture
            if id == "it" || seen.contains(&id) {
                continue;
            }
            seen.push(id);
            match self.types.trailing_block_capture(id, name_expr) {
                Some(CaptureKind::Global) => globals.push(id),
                Some(CaptureKind::Nonlocal) => nonlocals.push(id),
                None => {}
            }
        }

        let mut lines: Vec<String> = Vec::new();
        if !globals.is_empty() {
            lines.push(format!("global {}", globals.join(", ")));
        }
        if !nonlocals.is_empty() {
            lines.push(format!("nonlocal {}", nonlocals.join(", ")));
        }
        if lines.is_empty() {
            return None;
        }
        let indent = line_indent(self.source, function.body.first()?.range().start());
        let mut out = String::new();
        for line in &lines {
            out.push('\n');
            out.push_str(indent);
            out.push_str(line);
        }
        Some(out)
    }

    fn process(&mut self, function: &StmtFunctionDef) {
        let [decorator] = function.decorator_list.as_slice() else {
            return;
        };
        let callee = &decorator.expression;
        let stmt_range = function.range();

        // the header colon sits between the called expression and the suite,
        // with only whitespace before it
        let Some(colon) = self.source
            [usize::from(callee.range().end())..usize::from(stmt_range.end())]
            .find(':')
            .map(|i| callee.range().end() + TextSize::try_from(i).expect("offset fits u32"))
        else {
            return;
        };

        let name = self.fresh_name(callee);
        let indent = line_indent(self.source, stmt_range.start());

        // a `cast` / string-tag call has no parentheses in the source, so its
        // arguments can't be extended in place — treat it like a plain
        // expression and wrap it instead. `trailing_lambda_callee` makes the
        // same distinction; the pointer identity check keeps the two in sync
        let Some(signature_callee) = function.trailing_lambda_callee() else {
            return;
        };
        let plain_call = match callee {
            Expr::Call(call) if std::ptr::eq(signature_callee, call.func.as_ref()) => Some(call),
            _ => None,
        };
        let keyword = self.types.trailing_lambda_keyword(signature_callee);
        let trailing_argument = match &keyword {
            Some(keyword) => format!("{keyword}={name}"),
            None => name.clone(),
        };

        // `it` defaults to `None` so a callback whose type takes no argument
        // (`() -> None`, invoked as `fn()`) can still call the block, which
        // always declares the single implicit `it` parameter
        let mut fragments = vec![Fragment::Lit(format!("def {name}(it=None):"))];
        // write-through declarations so block assignments update enclosing
        // bindings; spliced ahead of the suite so they precede every use
        if let Some(declarations) = self.capture_declarations(function) {
            fragments.push(Fragment::Lit(declarations));
        }
        fragments.push(Fragment::Src(TextRange::new(
            colon + TextSize::from(1),
            stmt_range.end(),
        )));
        fragments.push(Fragment::Lit(format!("\n{indent}")));
        match plain_call {
            Some(call) => {
                let arguments = &call.arguments;
                let rparen = arguments.range().end() - TextSize::from(1);
                // a positional trailing argument must precede any keyword
                // argument to stay valid python; a keyword one appends freely
                let first_keyword_start = if keyword.is_none() {
                    arguments.iter_source_order().find_map(|arg| {
                        matches!(arg, ArgOrKeyword::Keyword(_)).then(|| arg.range().start())
                    })
                } else {
                    None
                };
                if let Some(keyword_start) = first_keyword_start {
                    fragments.push(Fragment::Src(TextRange::new(
                        stmt_range.start(),
                        keyword_start,
                    )));
                    fragments.push(Fragment::Lit(format!("{trailing_argument}, ")));
                    fragments.push(Fragment::Src(TextRange::new(
                        keyword_start,
                        arguments.range().end(),
                    )));
                } else {
                    // splice the trailing argument in before the closing paren
                    fragments.push(Fragment::Src(TextRange::new(stmt_range.start(), rparen)));
                    let last_argument_end = arguments
                        .iter_source_order()
                        .map(|arg| arg.range().end())
                        .max();
                    let separator = match last_argument_end {
                        None => "",
                        // a trailing comma in the source already separates
                        Some(end)
                            if self.source[usize::from(end)..usize::from(rparen)].contains(',') =>
                        {
                            " "
                        }
                        Some(_) => ", ",
                    };
                    fragments.push(Fragment::Lit(format!("{separator}{trailing_argument})")));
                }
            }
            None => {
                // call the expression itself; parenthesize anything that isn't
                // already a postfix-primary so precedence can't rebind the call
                let bare = matches!(callee, Expr::Name(_) | Expr::Attribute(_));
                if bare {
                    fragments.push(Fragment::Src(callee.range()));
                    fragments.push(Fragment::Lit(format!("({trailing_argument})")));
                } else {
                    fragments.push(Fragment::Lit("(".to_owned()));
                    fragments.push(Fragment::Src(callee.range()));
                    fragments.push(Fragment::Lit(format!(")({trailing_argument})")));
                }
            }
        }
        self.edits.push((stmt_range, fragments));
    }
}

impl<'ast> Visitor<'ast> for TrailingLambdaLower<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt
            && function.is_trailing_lambda
        {
            self.process(function);
        }
        walk_stmt(self, stmt);
    }
}

pub(crate) struct TrailingLambdaPass<'src> {
    source: &'src str,
}

impl<'src> TrailingLambdaPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for TrailingLambdaPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = TrailingLambdaLower {
            source: self.source,
            types,
            edits: Vec::new(),
            counter: 0,
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::{Config, transpile};

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    #[test]
    fn no_argument_call() {
        let out = check(indoc! {"
            def f(a: (int) -> None):
                a(1)

            f:
                print(it)
        "});
        assert!(
            out.contains("def _trailing_lambda_0(it=None):\n    print(it)"),
            "got:\n{out}"
        );
        assert!(out.contains("f(a=_trailing_lambda_0)"), "got:\n{out}");
    }

    #[test]
    fn bare_name_callee_without_parens() {
        let out = check(indoc! {"
            def f(a: (int) -> None):
                a(1)

            f:
                print(it)
        "});
        assert!(out.contains("\nf(a=_trailing_lambda_0)"), "got:\n{out}");
    }

    #[test]
    fn existing_arguments_are_kept() {
        let out = check(indoc! {"
            def f(x: int, a: (int) -> None):
                a(x)

            f(2):
                print(it)
        "});
        assert!(out.contains("f(2, a=_trailing_lambda_0)"), "got:\n{out}");
    }

    #[test]
    fn defaulted_parameter_before_lambda() {
        // the user-facing shape from the docs: `x` keeps its default when the
        // trailing lambda binds the last parameter by keyword
        let out = check(indoc! {"
            def f(x: int = 1, a: (int) -> str):
                a(x)

            f:
                print(it)

            f(2):
                print(it)
        "});
        assert!(
            out.contains("def f(x: int = 1, a: Callable[[int], str] = _MISSING):"),
            "got:\n{out}"
        );
        assert!(out.contains("f(a=_trailing_lambda_0)"), "got:\n{out}");
        assert!(out.contains("f(2, a=_trailing_lambda_1)"), "got:\n{out}");
    }

    #[test]
    fn unknown_callee_appends_positionally() {
        let out = check(indoc! {"
            from somewhere import f

            f(2):
                print(it)
        "});
        assert!(out.contains("f(2, _trailing_lambda_0)"), "got:\n{out}");
    }

    #[test]
    fn positional_fallback_precedes_keywords() {
        // with keyword arguments in the call, a positional append after them
        // would be invalid python — the lambda goes before the first keyword
        let out = check(indoc! {"
            from somewhere import f

            f(1, x=2):
                print(it)
        "});
        assert!(out.contains("f(1, _trailing_lambda_0, x=2)"), "got:\n{out}");
    }

    #[test]
    fn ast_pass_inside_body_falls_back_to_rendered_lowering() {
        // a typed lambda in the body makes the driver re-render the whole
        // statement through the generator, which has no type info — the
        // rendered lowering appends the function positionally
        let out = check(indoc! {"
            def f(a: (int) -> None):
                a(1)

            f:
                g = lambda (x: int): x
                print(g(it))
        "});
        assert!(out.contains("def __trailing_lambda__(it):"), "got:\n{out}");
        assert!(out.contains("f(__trailing_lambda__)"), "got:\n{out}");
    }

    #[test]
    fn trailing_comma_call() {
        let out = check(indoc! {"
            def f(x: int, a: (int) -> None):
                a(x)

            f(2,):
                print(it)
        "});
        assert!(out.contains("f(2, a=_trailing_lambda_0)"), "got:\n{out}");
    }

    #[test]
    fn nested_in_function_body() {
        let out = check(indoc! {"
            def g(a: (int) -> None):
                a(0)

            def outer():
                g:
                    print(it)
        "});
        assert!(
            out.contains(
                "    def _trailing_lambda_0(it=None):\n        print(it)\n    g(a=_trailing_lambda_0)"
            ),
            "got:\n{out}"
        );
    }

    #[test]
    fn nested_trailing_lambdas_compose() {
        let out = check(indoc! {"
            def g(a: (int) -> None):
                a(0)

            g:
                g:
                    print(it)
        "});
        assert!(
            out.contains("def _trailing_lambda_1(it=None):\n        print(it)"),
            "got:\n{out}"
        );
        assert!(out.contains("    g(a=_trailing_lambda_1)"), "got:\n{out}");
        assert!(out.contains("\ng(a=_trailing_lambda_0)"), "got:\n{out}");
    }

    #[test]
    fn comments_survive() {
        let out = check(indoc! {"
            def f(a: (int) -> None):
                a(1)

            f:  # header note
                # body note
                print(it)
        "});
        assert!(
            out.contains("def _trailing_lambda_0(it=None):  # header note"),
            "got:\n{out}"
        );
        assert!(out.contains("    # body note"), "got:\n{out}");
    }

    #[test]
    fn method_callee() {
        let out = check(indoc! {"
            class C:
                def run(self, a: (int) -> None):
                    a(1)

            C().run:
                print(it)
        "});
        assert!(out.contains("C().run(a=_trailing_lambda_0)"), "got:\n{out}");
    }

    #[test]
    fn user_binding_collision_avoided() {
        let out = check(indoc! {"
            _trailing_lambda_0 = 1

            def f(a: (int) -> None):
                a(1)

            f:
                print(it)
        "});
        assert!(
            out.contains("def _trailing_lambda_1(it=None):"),
            "got:\n{out}"
        );
        assert!(out.contains("f(a=_trailing_lambda_1)"), "got:\n{out}");
    }

    #[test]
    fn assignment_captures_module_binding_as_global() {
        // a block assignment to a module-level name writes through via `global`
        let out = check(indoc! {"
            def f(fn: () -> None):
                fn()

            a: int = 1
            f:
                a = 2
        "});
        assert!(
            out.contains("def _trailing_lambda_0(it=None):\n    global a\n    a = 2"),
            "got:\n{out}"
        );
    }

    #[test]
    fn assignment_captures_enclosing_function_as_nonlocal() {
        // a block assignment to an enclosing function's local writes through via
        // `nonlocal`
        let out = check(indoc! {"
            def f(fn: () -> None):
                fn()

            def outer():
                b = 1
                f:
                    b = 2
        "});
        assert!(
            out.contains("    def _trailing_lambda_0(it=None):\n        nonlocal b\n        b = 2"),
            "got:\n{out}"
        );
    }

    #[test]
    fn new_local_in_block_is_not_captured() {
        // a name bound in no enclosing scope stays a plain block local
        let out = check(indoc! {"
            def f(fn: () -> None):
                fn()

            a: int = 1
            f:
                fresh = a
        "});
        assert!(!out.contains("global fresh"), "got:\n{out}");
        assert!(!out.contains("nonlocal fresh"), "got:\n{out}");
    }

    #[test]
    fn attribute_target_does_not_capture_root() {
        // `obj.x = …` rebinds no name, so the root `obj` gets no declaration
        let out = check(indoc! {"
            def f(fn: () -> None):
                fn()

            class C:
                x: int = 0

            obj: C = C()
            f:
                obj.x = 1
        "});
        assert!(!out.contains("global obj"), "got:\n{out}");
        assert!(!out.contains("nonlocal"), "got:\n{out}");
    }

    #[test]
    fn coalesce_inside_arguments_composes() {
        let out = check(indoc! {"
            def f(x: int | None, a: (int | None) -> None):
                a(x)

            y: int? = None
            f(y ?? 3):
                print(it)
        "});
        assert!(
            out.contains("f(y if y is not None else 3, a=_trailing_lambda_0)"),
            "got:\n{out}"
        );
    }
}
