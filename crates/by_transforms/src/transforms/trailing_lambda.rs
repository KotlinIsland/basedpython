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
use ruff_python_ast::{ArgOrKeyword, Expr, ExprContext, Stmt, StmtFunctionDef, StmtReturn};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::source_util::line_indent;
use crate::type_info::{CaptureKind, TypeInfo};

/// The parameter a block binds its callback's implicit receiver to. The body
/// spells it `self`, which [`implicit_receiver`] rewrites to this name — a name
/// the source cannot produce, so a method's own `self` is never shadowed
///
/// [`implicit_receiver`]: super::implicit_receiver
pub(crate) const RECEIVER_PARAMETER: &str = "_by_self";

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

/// Collects the `return` statements a `once` block propagates to the enclosing
/// function — those directly in the block, in source order. Nested functions
/// and classes are their own `return` target and are not descended into.
struct BlockReturns<'ast> {
    returns: Vec<&'ast StmtReturn>,
}

impl<'ast> Visitor<'ast> for BlockReturns<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            Stmt::Return(ret) => self.returns.push(ret),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

impl TrailingLambdaLower<'_, '_> {
    /// a function name that is unbound at the statement's scope and unused by
    /// earlier lowerings in this run. The derived `{name}_return` cell (used to
    /// carry a `once` block's return value) must be free too, so both are checked
    fn fresh_name(&mut self, anchor: &Expr) -> String {
        loop {
            let name = format!("_trailing_lambda_{}", self.counter);
            self.counter += 1;
            if self.types.is_unbound_at(&name, anchor)
                && self.types.is_unbound_at(&format!("{name}_return"), anchor)
            {
                return name;
            }
        }
    }

    /// The `global` / `nonlocal` lines a block needs so its assignments write
    /// through to the enclosing scope instead of shadowing it with a fresh
    /// local, plus the names to pre-initialize in the enclosing scope (a fresh
    /// binding a `once` block leaks needs a prior enclosing binding for its
    /// `nonlocal` to be valid). Returns `(lines to splice after `def …(it):`,
    /// names to pre-init before the `def`)`. This is what lets `f:\n    a = 2`
    /// update an enclosing `a` without a manual `nonlocal a`.
    fn capture_declarations(&self, function: &StmtFunctionDef) -> (Option<String>, Vec<String>) {
        let mut collector = BlockAssignments { names: Vec::new() };
        for stmt in &function.body {
            collector.visit_stmt(stmt);
        }

        // a fresh binding survives only from a `once` block that unconditionally
        // binds it, matching the type checker's write-back
        let is_once = function
            .trailing_lambda_callee()
            .is_some_and(|callee| self.types.trailing_lambda_callee_is_once(callee));

        let mut seen: Vec<&str> = Vec::new();
        let mut globals: Vec<&str> = Vec::new();
        let mut nonlocals: Vec<&str> = Vec::new();
        let mut preinits: Vec<String> = Vec::new();
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
                // a name bound in no enclosing scope is a fresh binding; it
                // survives a `once` block that unconditionally binds it
                None if is_once && definitely_assigns(&function.body, id) => {
                    match self.types.trailing_block_fresh_capture(name_expr) {
                        Some(CaptureKind::Global) => globals.push(id),
                        Some(CaptureKind::Nonlocal) => {
                            nonlocals.push(id);
                            preinits.push(id.to_owned());
                        }
                        None => {}
                    }
                }
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
            return (None, preinits);
        }
        let indent = line_indent(
            self.source,
            function
                .body
                .first()
                .map_or(function.range().start(), |stmt| stmt.range().start()),
        );
        let mut out = String::new();
        for line in &lines {
            out.push('\n');
            out.push_str(indent);
            out.push_str(line);
        }
        (Some(out), preinits)
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

        // basedpython: a `once` block runs exactly once, so its `return` targets
        // the enclosing function. collect the block's returns; when present, a
        // list cell captures the returned value (see `push_body_capturing_returns`)
        // and the enclosing function returns it after the call — an empty cell
        // meaning the block never returned
        let returns = if self.types.trailing_lambda_callee_is_once(signature_callee) {
            let mut collector = BlockReturns {
                returns: Vec::new(),
            };
            for stmt in &function.body {
                collector.visit_stmt(stmt);
            }
            collector.returns
        } else {
            Vec::new()
        };
        let ret_cell = format!("{name}_return");

        // write-through declarations (spliced after `def …:`) and the fresh
        // surviving bindings to pre-init before it
        let (declarations, preinits) = self.capture_declarations(function);

        // a receiver callback is called with its receiver first, so the block
        // declares a parameter for it ahead of `it`. both default to `None` so a
        // callback whose type takes fewer arguments (`() -> None`, invoked as
        // `fn()`) can still call the block, which always declares them
        let parameters = if self
            .types
            .trailing_lambda_callback_has_receiver(signature_callee)
        {
            format!("{RECEIVER_PARAMETER}=None, it=None")
        } else {
            "it=None".to_owned()
        };
        let mut fragments = Vec::new();
        if !returns.is_empty() {
            fragments.push(Fragment::Lit(format!("{ret_cell} = []\n{indent}")));
        }
        // a fresh binding a `once` block leaks needs a prior enclosing binding so
        // its in-block `nonlocal` is valid; the block overwrites this on its run
        for preinit in &preinits {
            fragments.push(Fragment::Lit(format!("{preinit} = None\n{indent}")));
        }
        fragments.push(Fragment::Lit(format!("def {name}({parameters}):")));
        // write-through declarations so block assignments update enclosing
        // bindings; spliced ahead of the suite so they precede every use
        if let Some(declarations) = declarations {
            fragments.push(Fragment::Lit(declarations));
        }
        let body = TextRange::new(colon + TextSize::from(1), stmt_range.end());
        if returns.is_empty() {
            fragments.push(Fragment::Src(body));
        } else {
            push_body_capturing_returns(&mut fragments, body, &returns, &ret_cell);
        }
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

        // basedpython: once the `once` block's call has run, return its captured
        // value to the enclosing function (an empty cell means it never returned)
        if !returns.is_empty() {
            let body_indent = function
                .body
                .first()
                .map(|stmt| line_indent(self.source, stmt.range().start()))
                .unwrap_or(indent);
            fragments.push(Fragment::Lit(format!(
                "\n{indent}if {ret_cell}:\n{body_indent}return {ret_cell}[0]"
            )));
        }

        self.edits.push((stmt_range, fragments));
    }
}

/// Splits the block body around each `return`, rewriting `return <expr>` to
/// `<cell>.append(<expr>); return` (and a bare `return` to `.append(None)`) so
/// the value lands in the enclosing function's cell while the block still exits
/// at that point. `returns` is in source order, so the cursor only moves forward.
/// Whether `body` binds `name` on every path — a top-level `name = …` (annotated
/// or augmented), a `with` body, or an `if` chain whose main body and every
/// branch (including a final `else`) bind it. Mirrors the type checker's
/// write-back so a fresh binding is captured exactly when it survives definitely.
fn definitely_assigns(body: &[Stmt], name: &str) -> bool {
    fn is_target(expr: &Expr, name: &str) -> bool {
        matches!(expr, Expr::Name(n) if n.id.as_str() == name)
    }
    body.iter().any(|stmt| match stmt {
        Stmt::Assign(assign) => assign.targets.iter().any(|t| is_target(t, name)),
        Stmt::AnnAssign(ann) => ann.value.is_some() && is_target(&ann.target, name),
        Stmt::AugAssign(aug) => is_target(&aug.target, name),
        Stmt::With(with) => definitely_assigns(&with.body, name),
        Stmt::If(if_stmt) => {
            if_stmt
                .elif_else_clauses
                .iter()
                .any(|clause| clause.test.is_none())
                && definitely_assigns(&if_stmt.body, name)
                && if_stmt
                    .elif_else_clauses
                    .iter()
                    .all(|clause| definitely_assigns(&clause.body, name))
        }
        _ => false,
    })
}

fn push_body_capturing_returns(
    fragments: &mut Vec<Fragment>,
    body: TextRange,
    returns: &[&StmtReturn],
    cell: &str,
) {
    let mut cursor = body.start();
    for ret in returns {
        fragments.push(Fragment::Src(TextRange::new(cursor, ret.range().start())));
        match &ret.value {
            Some(value) => {
                fragments.push(Fragment::Lit(format!("{cell}.append(")));
                fragments.push(Fragment::Src(value.range()));
                fragments.push(Fragment::Lit("); return".to_owned()));
            }
            None => fragments.push(Fragment::Lit(format!("{cell}.append(None); return"))),
        }
        cursor = ret.range().end();
    }
    fragments.push(Fragment::Src(TextRange::new(cursor, body.end())));
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
        // `typeof` is rewritten in the AST, so the driver re-renders the whole
        // statement through the generator, which has no type info — the
        // rendered lowering appends the function positionally
        let out = check(indoc! {"
            def f(a: (int) -> None):
                a(1)

            f:
                x: typeof(1) = 1
                print(x, it)
        "});
        assert!(out.contains("def __trailing_lambda__(it):"), "got:\n{out}");
        assert!(out.contains("f(__trailing_lambda__)"), "got:\n{out}");
    }

    /// a typed lambda used to force that fallback too. it lowers by deleting
    /// its basedpython surface now (see [`typed_lambda`](super::typed_lambda)),
    /// leaving the statement un-re-rendered, so the block keeps the template
    /// lowering and its keyword binding
    #[test]
    fn typed_lambda_inside_body_keeps_the_template_lowering() {
        let out = check(indoc! {"
            def f(a: (int) -> None):
                a(1)

            f:
                g = lambda (x: int): x
                print(g(it))
        "});
        assert!(
            out.contains("def _trailing_lambda_0(it=None):"),
            "got:\n{out}"
        );
        assert!(out.contains("f(a=_trailing_lambda_0)"), "got:\n{out}");
        assert!(out.contains("g = lambda x: x"), "got:\n{out}");
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

    #[test]
    fn once_block_return_targets_enclosing_function() {
        // a `once` block runs exactly once, so its `return` propagates to the
        // enclosing function via a list cell filled inside the block
        let out = check(indoc! {"
            def each(items: list[int], once fn: (int) -> None):
                fn(items[0])

            def find(items: list[int]) -> int:
                each(items):
                    return it
                return -1
        "});
        assert!(
            out.contains("_trailing_lambda_0_return = []"),
            "got:\n{out}"
        );
        assert!(
            out.contains("_trailing_lambda_0_return.append(it); return"),
            "got:\n{out}"
        );
        assert!(
            out.contains(
                "    if _trailing_lambda_0_return:\n        return _trailing_lambda_0_return[0]"
            ),
            "got:\n{out}"
        );
    }

    #[test]
    fn once_block_without_return_is_unchanged() {
        // no `return` in the block means no cell / propagation
        let out = check(indoc! {"
            def run(once fn: () -> None):
                fn()

            x: int = 1
            run:
                x = 2
        "});
        assert!(!out.contains("_return = []"), "got:\n{out}");
        assert!(!out.contains(".append("), "got:\n{out}");
        assert!(
            out.contains("def _trailing_lambda_0(it=None):"),
            "got:\n{out}"
        );
    }

    #[test]
    fn once_block_fresh_binding_survives_via_nonlocal_preinit() {
        // a `once` block's new binding survives; inside a function it becomes a
        // `nonlocal` with a pre-init so the declaration is valid
        let out = check(indoc! {"
            def run(once fn: () -> None):
                fn()

            def main():
                run:
                    fresh = 9
        "});
        assert!(out.contains("    fresh = None\n"), "got:\n{out}");
        assert!(out.contains("        nonlocal fresh\n"), "got:\n{out}");
    }

    #[test]
    fn once_block_fresh_binding_at_module_uses_global() {
        // at module scope the fresh binding is a `global` — no pre-init needed
        let out = check(indoc! {"
            def run(once fn: () -> None):
                fn()

            run:
                top = 7
        "});
        assert!(out.contains("global top"), "got:\n{out}");
        assert!(!out.contains("top = None"), "got:\n{out}");
    }

    #[test]
    fn non_once_block_fresh_binding_is_not_captured() {
        // a non-`once` block's new binding is not (yet) leaked, so no capture
        let out = check(indoc! {"
            def run(fn: () -> None):
                fn()

            def main():
                run:
                    fresh = 9
        "});
        assert!(!out.contains("nonlocal fresh"), "got:\n{out}");
        assert!(!out.contains("fresh = None"), "got:\n{out}");
    }
}
