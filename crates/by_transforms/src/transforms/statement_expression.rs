//! Lowering for statement expressions.
//!
//! A statement expression is a compound statement standing where a value is
//! expected:
//!
//! ```text
//! a = match command:
//!     case "up":
//!         1
//!     case "down":
//!         -1
//! ```
//!
//! The statement already sits at the tail of the line it appears on — the parser
//! rejects it anywhere else — so lowering is a matter of moving the assignment
//! from in front of the statement to after it, and writing each of the
//! statement's value positions into the temporary that replaces it:
//!
//! ```text
//! match command:
//!     case "up":
//!         __by_stmt_expr_0__ = 1
//!     case "down":
//!         __by_stmt_expr_0__ = -1
//! a = __by_stmt_expr_0__
//! ```
//!
//! `break <value>` becomes an assignment followed by a bare `break`; the loop's
//! `else` clause supplies the value when it completes without breaking.
//!
//! `raise` and `return` carry no suite and never produce a value, so they lower
//! to themselves. They are also allowed inside the operators that *choose*
//! between operands — `and`, `or`, `??` and the conditional expression — which
//! do need restructuring, because there the diverging branch is conditional:
//!
//! ```text
//! v = table.get(k) ?? raise KeyError(k)
//! ```
//!
//! ```text
//! __by_stmt_expr_0__ = table.get(k)
//! if __by_stmt_expr_0__ is None:
//!     raise KeyError(k)
//! v = __by_stmt_expr_0__
//! ```
//!
//! Every rewrite keeps operand source as [`Fragment::Src`] passthrough spans, so
//! lowerings nested inside a branch — and the comments in it — survive.

use ruff_python_ast::helpers::{StatementExpressionValue, statement_expression_values};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{BoolOp, Expr, ExprStatement, ModModule, Operator, Stmt, StmtBreak};
use ruff_text_size::{Ranged, TextRange};
use std::collections::HashSet;

use super::ast_driver::{AstPass, Fragment, PassContext};
use super::source_util::{
    line_indent, line_start, parenthesized_value_range, temporary_name, value_separator_start,
};

pub(crate) struct StatementExpressionPass<'src> {
    source: &'src str,
}

impl<'src> StatementExpressionPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for StatementExpressionPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut misplaced = Vec::new();
        let mut lower = Lower {
            source: self.source,
            counter: 0,
            claimed_breaks: HashSet::new(),
            misplaced: Vec::new(),
            text_edits: Vec::new(),
            template_edits: Vec::new(),
        };
        for stmt in &module.body {
            lower.visit_stmt(stmt);
        }
        // a `break <value>` whose loop is not a statement expression has nothing
        // to write the value into (ty reports it as `DiscardedBreakValue`). the
        // value still has to be evaluated, so it becomes a statement of its own
        // rather than being dropped
        let mut orphans = Vec::new();
        collect_all_value_breaks(&module.body, &mut orphans);
        for break_stmt in orphans {
            if lower.claimed_breaks.contains(&break_stmt.range) {
                continue;
            }
            lower.emit_break(break_stmt, None);
        }
        misplaced.append(&mut lower.misplaced);
        if !misplaced.is_empty() {
            // the parser rejects these, so reaching here means its rule and this
            // pass have drifted apart. refuse rather than emit broken python
            ctx.errors.extend(misplaced);
            return;
        }
        ctx.text_edits.extend(lower.text_edits);
        ctx.template_edits.extend(lower.template_edits);
    }
}

struct Lower<'src> {
    source: &'src str,
    /// monotonic across the file so sibling statement expressions get distinct
    /// temporaries — nesting means several can be live at once
    counter: usize,
    /// ranges of the `break <value>` statements a statement expression claimed
    claimed_breaks: HashSet<TextRange>,
    /// statement expressions this pass cannot place (see [`AstPass::run`])
    misplaced: Vec<String>,
    text_edits: Vec<(TextRange, String)>,
    template_edits: Vec<(TextRange, Vec<Fragment>)>,
}

impl<'ast> Visitor<'ast> for Lower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Some(tail) = tail_expression(stmt)
            && contains_statement_expression(tail)
        {
            self.lower_statement(stmt, tail);
        }

        // descend regardless: the suites of a statement expression, like every
        // other body, may hold statement expressions of their own
        walk_stmt(self, stmt);
    }
}

impl Lower<'_> {
    fn next_temp(&mut self) -> String {
        let name = temporary_name("stmt_expr", self.counter);
        self.counter += 1;
        name
    }

    /// Rewrites `stmt`, whose value expression `tail` holds a statement
    /// expression.
    fn lower_statement(&mut self, stmt: &Stmt, tail: &Expr) {
        let indent = line_indent(self.source, stmt.range().start()).to_string();

        // the statement expression's suite continues the line the statement starts
        // on, so nothing may precede it there
        if matches!(tail, Expr::Statement(_))
            && usize::from(stmt.range().start())
                != usize::from(line_start(self.source, stmt.range().start())) + indent.len()
        {
            self.misplaced.push(
                "a statement expression with a suite must be the first statement on its line"
                    .to_string(),
            );
            return;
        }

        let temp = self.next_temp();

        let mut fragments = Vec::new();
        if let Expr::Statement(statement) = tail {
            self.emit_root(statement, stmt, tail, &temp, &indent, &mut fragments);
        } else {
            Self::emit_value(tail, &temp, &indent, &mut fragments);
            // the replaced range starts after the line's existing indentation,
            // so the first emitted line must not repeat it
            if let Some(Fragment::Lit(first)) = fragments.first_mut()
                && let Some(rest) = first.strip_prefix(&indent)
            {
                *first = rest.to_string();
            }
            self.push_prefix(stmt, tail, &temp, &indent, &mut fragments);
        }
        self.template_edits.push((stmt.range(), fragments));
    }

    /// Emits a statement whose value *is* a statement expression: the statement
    /// stays where it is, its value positions are redirected into `temp`, and
    /// the assignment moves below it.
    fn emit_root(
        &mut self,
        statement: &ExprStatement,
        stmt: &Stmt,
        tail: &Expr,
        temp: &str,
        indent: &str,
        out: &mut Vec<Fragment>,
    ) {
        let values = self.redirect_values(&statement.stmt, temp);
        out.push(Fragment::Src(statement.range));

        // with no value position there is nothing to read: `raise` and `return`
        // never complete, and neither does a statement whose every branch
        // diverges. (a statement that reaches its end without a value is
        // `non-exhaustive-statement-expression`, which ty has already reported)
        if values > 0 {
            self.push_prefix(stmt, tail, temp, indent, out);
        }
    }

    /// Redirects each of `stmt`'s value positions into `temp`, as minimal edits
    /// inside the statement's own source so that they compose with whatever else
    /// lowers inside its branches.
    fn redirect_values(&mut self, stmt: &Stmt, temp: &str) -> usize {
        let values = statement_expression_values(stmt);
        for &value in &values {
            match value {
                StatementExpressionValue::Tail(tail, _) => {
                    // anchored to the statement, not the expression: an
                    // expression's range stops inside any parentheses around it,
                    // and `(temp = value)` reads as an anonymous named tuple
                    self.text_edits
                        .push((TextRange::empty(tail.range().start()), format!("{temp} = ")));
                }
                StatementExpressionValue::Break(break_stmt, _) => {
                    self.claimed_breaks.insert(break_stmt.range);
                    self.emit_break(break_stmt, Some(temp));
                }
            }
        }
        values.len()
    }

    /// Rewrites `break <value>` into the value — assigned to `temp`, or standing
    /// alone when nothing reads it — followed by a bare `break`.
    fn emit_break(&mut self, break_stmt: &StmtBreak, temp: Option<&str>) {
        let Some(value) = &break_stmt.value else {
            return;
        };
        let break_indent = line_indent(self.source, break_stmt.range.start()).to_string();
        self.text_edits.push((
            TextRange::new(break_stmt.range.start(), value.range().start()),
            temp.map(|temp| format!("{temp} = ")).unwrap_or_default(),
        ));
        self.text_edits.push((
            TextRange::empty(break_stmt.range.end()),
            format!("\n{break_indent}break"),
        ));
    }

    /// Emits, on a line of its own, everything the statement says before its
    /// value — `a = `, `let a = `, `return ` — followed by `temp`.
    ///
    /// The prefix is emitted as a passthrough span rather than copied out as
    /// text, because a pass may be rewriting the prefix itself: a `let`
    /// declaration lowers by replacing `let a =` with `a: Final =`. The driver
    /// claims every edit inside a template's range and materializes only what
    /// the template's passthrough spans cover, so a prefix copied as text takes
    /// that rewrite down with it and the `let` reaches the output.
    ///
    /// The span runs all the way to the value, so that a rewrite of the whole
    /// prefix is contained by it. What separates the two — whitespace, an
    /// opening parenthesis, a line continuation — cannot be re-emitted verbatim
    /// (see [`value_separator_start`]), so it is normalised to a single space by
    /// an edit of its own. A prefix rewrite starts earlier and therefore wins
    /// over that edit under the driver's first-wins overlap rule, which is why
    /// no other pass has to know where this boundary lies.
    fn push_prefix(
        &mut self,
        stmt: &Stmt,
        tail: &Expr,
        temp: &str,
        indent: &str,
        out: &mut Vec<Fragment>,
    ) {
        // the span stops at the value's *written* start, parentheses included:
        // a group around the value is replaced by what is emitted above, so
        // re-emitting its `(` would leave it open — the matching `)` is inside
        // the replaced statement and does not reach the output
        let value_start =
            parenthesized_value_range(self.source, tail.range(), written_before_value(stmt))
                .start();
        let prefix = TextRange::new(stmt.range().start(), value_start);
        let separator = TextRange::new(value_separator_start(self.source, prefix), prefix.end());
        self.text_edits.push((separator, " ".to_owned()));
        out.push(Fragment::Lit(format!("\n{indent}")));
        out.push(Fragment::Src(prefix));
        out.push(Fragment::Lit(temp.to_owned()));
    }

    /// Emits the block that computes `expr` into `temp` at `indent`.
    fn emit_value(expr: &Expr, temp: &str, indent: &str, out: &mut Vec<Fragment>) {
        match expr {
            // only the diverging forms reach here: a form with a suite is always
            // the whole value expression and took [`Self::emit_root`]
            Expr::Statement(statement) => {
                out.push(Fragment::Lit(indent.to_string()));
                out.push(Fragment::Src(statement.range));
            }
            Expr::BoolOp(bool_op) => {
                // `a or b` short-circuits on a truthy `a`, so the later operands
                // run under progressively deeper guards
                let guard = match bool_op.op {
                    BoolOp::Or => "if not ",
                    BoolOp::And => "if ",
                };
                let mut inner = indent.to_string();
                for (index, value) in bool_op.values.iter().enumerate() {
                    if index > 0 {
                        out.push(Fragment::Lit(format!("\n{inner}{guard}{temp}:\n")));
                        inner.push_str("    ");
                    }
                    Self::emit_value(value, temp, &inner, out);
                }
            }
            Expr::If(if_expr) => {
                let inner = format!("{indent}    ");
                out.push(Fragment::Lit(format!("{indent}if ")));
                out.push(Fragment::Src(if_expr.test.range()));
                out.push(Fragment::Lit(":\n".to_string()));
                Self::emit_value(&if_expr.body, temp, &inner, out);
                out.push(Fragment::Lit(format!("\n{indent}else:\n")));
                Self::emit_value(&if_expr.orelse, temp, &inner, out);
            }
            Expr::BinOp(bin_op) if matches!(bin_op.op, Operator::Coalesce) => {
                let inner = format!("{indent}    ");
                Self::emit_value(&bin_op.left, temp, indent, out);
                out.push(Fragment::Lit(format!("\n{indent}if {temp} is None:\n")));
                Self::emit_value(&bin_op.right, temp, &inner, out);
            }
            Expr::Named(named) => {
                Self::emit_value(&named.value, temp, indent, out);
                out.push(Fragment::Lit(format!("\n{indent}")));
                out.push(Fragment::Src(named.target.range()));
                out.push(Fragment::Lit(format!(" = {temp}")));
            }
            // an ordinary operand: evaluate it into the temporary
            _ => {
                out.push(Fragment::Lit(format!("{indent}{temp} = ")));
                out.push(Fragment::Src(expr.range()));
            }
        }
    }
}

/// Where the scan for a value's own parentheses may start: the end of the last
/// node the statement wrote before its value, or the statement's start when it
/// wrote none. See [`parenthesized_value_range`].
fn written_before_value(stmt: &Stmt) -> ruff_text_size::TextSize {
    let written = match stmt {
        Stmt::Assign(assign) => assign.targets.last().map(Ranged::end),
        Stmt::AnnAssign(assign) => Some(assign.annotation.range().end()),
        Stmt::AugAssign(assign) => Some(assign.target.range().end()),
        // `return <value>` and a bare expression statement write nothing ahead
        // of the value but the keyword, which no string can hide in
        _ => None,
    };
    written.unwrap_or_else(|| stmt.range().start())
}

/// The expression whose value the statement takes on, if it has one.
fn tail_expression(stmt: &Stmt) -> Option<&Expr> {
    match stmt {
        Stmt::Assign(assign) => Some(&assign.value),
        Stmt::AnnAssign(assign) => assign.value.as_deref(),
        Stmt::AugAssign(assign) => Some(&assign.value),
        Stmt::Return(ret) => ret.value.as_deref(),
        Stmt::Expr(expr) => Some(&expr.value),
        _ => None,
    }
}

/// Whether `expr` holds a statement expression, without descending into one.
fn contains_statement_expression(expr: &Expr) -> bool {
    struct Finder {
        found: bool,
    }

    impl<'a> Visitor<'a> for Finder {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                // a trailing lambda block needs no temporary: its value is the
                // call it stands for, which [`trailing_lambda`] emits in place
                //
                // [`trailing_lambda`]: super::trailing_lambda
                Expr::Statement(statement) if statement.is_trailing_lambda() => {}
                Expr::Statement(_) => self.found = true,
                _ if !self.found => walk_expr(self, expr),
                _ => {}
            }
        }
    }

    let mut finder = Finder { found: false };
    finder.visit_expr(expr);
    finder.found
}

/// Collects every `break <value>` anywhere in `body`, including inside nested
/// loops and nested function and class bodies.
fn collect_all_value_breaks<'a>(body: &'a [Stmt], out: &mut Vec<&'a StmtBreak>) {
    struct Finder<'a, 'b> {
        out: &'b mut Vec<&'a StmtBreak>,
    }

    impl<'a> Visitor<'a> for Finder<'a, '_> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            if let Stmt::Break(break_stmt) = stmt
                && break_stmt.value.is_some()
            {
                self.out.push(break_stmt);
            }
            walk_stmt(self, stmt);
        }
    }

    let mut finder = Finder { out };
    for stmt in body {
        finder.visit_stmt(stmt);
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
    fn match_expression() {
        let out = check(indoc! {r#"
            def f(command: str) -> int:
                direction = match command:
                    case "up":
                        1
                    case _:
                        0
                return direction
        "#});
        assert!(
            out.contains(indoc! {r#"
                def f(command: str) -> int:
                    match command:
                        case "up":
                            __by_stmt_expr_0__ = 1
                        case _:
                            __by_stmt_expr_0__ = 0
                    direction = __by_stmt_expr_0__
                    return direction
            "#}),
            "got:\n{out}"
        );
    }

    #[test]
    fn parenthesized_branch_value() {
        // the assignment goes ahead of the parentheses. inside them
        // `(__by_stmt_expr_0__ = ...)` is the anonymous named tuple value form, and
        // the branch silently lowered to a `NamedTuple` constructor instead
        let out = check(indoc! {"
            def f(c: bool) -> int:
                a = if c:
                    (1 + 2)
                else:
                    0
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(c: bool) -> int:
                    if c:
                        __by_stmt_expr_0__ = (1 + 2)
                    else:
                        __by_stmt_expr_0__ = 0
                    a = __by_stmt_expr_0__
                    return a
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn if_expression() {
        let out = check(indoc! {"
            def f(c: bool) -> int:
                a = if c:
                    1
                else:
                    2
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(c: bool) -> int:
                    if c:
                        __by_stmt_expr_0__ = 1
                    else:
                        __by_stmt_expr_0__ = 2
                    a = __by_stmt_expr_0__
                    return a
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn try_expression() {
        let out = check(indoc! {"
            def f(s: str) -> int:
                a = try:
                    s.index(\"=\")
                except ValueError:
                    0
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(s: str) -> int:
                    try:
                        __by_stmt_expr_0__ = s.index(\"=\")
                    except ValueError:
                        __by_stmt_expr_0__ = 0
                    a = __by_stmt_expr_0__
                    return a
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn try_expression_else_clause() {
        // the `else` clause runs once the `try` block has completed, so it is what
        // the statement produces there — the block's own last expression is not
        let out = check(indoc! {"
            def f(s: str) -> int:
                a = try:
                    at = s.index(\"=\")
                except ValueError:
                    0
                else:
                    at + 1
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(s: str) -> int:
                    try:
                        at = s.index(\"=\")
                    except ValueError:
                        __by_stmt_expr_0__ = 0
                    else:
                        __by_stmt_expr_0__ = at + 1
                    a = __by_stmt_expr_0__
                    return a
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn loop_break_value() {
        let out = check(indoc! {"
            def f(xs: list[int]) -> int:
                a = for x in xs:
                    if x:
                        break x
                else:
                    -1
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(xs: list[int]) -> int:
                    for x in xs:
                        if x:
                            __by_stmt_expr_0__ = x
                            break
                    else:
                        __by_stmt_expr_0__ = -1
                    a = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn return_statement_takes_the_value() {
        let out = check(indoc! {"
            def f(c: bool) -> int:
                return if c:
                    1
                else:
                    2
        "});
        assert!(
            out.contains(indoc! {"
                def f(c: bool) -> int:
                    if c:
                        __by_stmt_expr_0__ = 1
                    else:
                        __by_stmt_expr_0__ = 2
                    return __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn coalesce_with_raise() {
        let out = check(indoc! {r#"
            def f(table: dict[str, int], k: str) -> int:
                v = table.get(k) ?? raise KeyError(k)
                return v
        "#});
        assert!(
            out.contains(indoc! {"
                def f(table: dict[str, int], k: str) -> int:
                    __by_stmt_expr_0__ = table.get(k)
                    if __by_stmt_expr_0__ is None:
                        raise KeyError(k)
                    v = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn coalesce_with_continue() {
        let out = check(indoc! {r#"
            def f(items: list[int | None]) -> int:
                total = 0
                for item in items:
                    one = item ?? continue
                    total += one
                return total
        "#});
        assert!(
            out.contains(indoc! {"
                def f(items: list[int | None]) -> int:
                    total = 0
                    for item in items:
                        __by_stmt_expr_0__ = item
                        if __by_stmt_expr_0__ is None:
                            continue
                        one = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn coalesce_with_break() {
        let out = check(indoc! {r#"
            def f(items: list[int | None]) -> int:
                total = 0
                for item in items:
                    one = item ?? break
                    total += one
                return total
        "#});
        assert!(
            out.contains(indoc! {"
                def f(items: list[int | None]) -> int:
                    total = 0
                    for item in items:
                        __by_stmt_expr_0__ = item
                        if __by_stmt_expr_0__ is None:
                            break
                        one = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn conditional_expression_with_raise() {
        let out = check(indoc! {"
            def f(x: int) -> int:
                a = x if x > 0 else raise ValueError(x)
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(x: int) -> int:
                    if x > 0:
                        __by_stmt_expr_0__ = x
                    else:
                        raise ValueError(x)
                    a = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn boolean_operator_with_return() {
        let out = check(indoc! {"
            def f(x: int) -> int:
                a = x or return 0
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(x: int) -> int:
                    __by_stmt_expr_0__ = x
                    if not __by_stmt_expr_0__:
                        return 0
                    a = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn nested_statement_expressions_use_distinct_temporaries() {
        let out = check(indoc! {"
            def f(c: bool, d: bool) -> int:
                a = if c:
                    b = if d:
                        1
                    else:
                        2
                    b
                else:
                    3
                return a
        "});
        assert!(out.contains("__by_stmt_expr_0__"), "got:\n{out}");
        assert!(out.contains("__by_stmt_expr_1__"), "got:\n{out}");
    }

    #[test]
    fn a_lowering_inside_a_branch_still_applies() {
        // the branch bodies pass through as source spans, so lowerings nested in
        // them (here `?.`) compose instead of being copied verbatim
        let out = check(indoc! {"
            class A:
                x: int = 1

            def f(c: bool, a: A?) -> int?:
                v = if c:
                    a?.x
                else:
                    None
                return v
        "});
        assert!(!out.contains("?."), "got:\n{out}");
        assert!(out.contains("__by_stmt_expr_0__ = "), "got:\n{out}");
    }

    #[test]
    fn while_loop_break_value() {
        let out = check(indoc! {"
            def f() -> int:
                a = while True:
                    break 1
                else:
                    0
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f() -> int:
                    while True:
                        __by_stmt_expr_0__ = 1
                        break
                    else:
                        __by_stmt_expr_0__ = 0
                    a = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn elif_chain() {
        let out = check(indoc! {r#"
            def f(n: int) -> str:
                a = if n == 0:
                    "none"
                elif n == 1:
                    "one"
                else:
                    "many"
                return a
        "#});
        assert!(
            out.contains(indoc! {r#"
                def f(n: int) -> str:
                    if n == 0:
                        __by_stmt_expr_0__ = "none"
                    elif n == 1:
                        __by_stmt_expr_0__ = "one"
                    else:
                        __by_stmt_expr_0__ = "many"
                    a = __by_stmt_expr_0__
            "#}),
            "got:\n{out}"
        );
    }

    /// a loop in tail position supplies its branch's value, breaks included
    #[test]
    fn loop_in_tail_position_of_a_branch() {
        let out = check(indoc! {"
            def f(c: bool, xs: list[int]) -> int:
                a = if c:
                    for x in xs:
                        break x
                    else:
                        -1
                else:
                    0
                return a
        "});
        assert!(
            out.contains(indoc! {"
                def f(c: bool, xs: list[int]) -> int:
                    if c:
                        for x in xs:
                            __by_stmt_expr_0__ = x
                            break
                        else:
                            __by_stmt_expr_0__ = -1
                    else:
                        __by_stmt_expr_0__ = 0
                    a = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    /// an expression's range excludes its parentheses, so the moved assignment
    /// must not pick up the opening one
    #[test]
    fn parenthesized_tail_stays_balanced() {
        let out = check(indoc! {"
            def f(c: bool) -> int:
                v = (b := 1 if c else raise ValueError())
                return v + b
        "});
        assert!(
            out.contains(indoc! {"
                def f(c: bool) -> int:
                    if c:
                        __by_stmt_expr_0__ = 1
                    else:
                        raise ValueError()
                    b = __by_stmt_expr_0__
                    v = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    /// nothing reads the value when every branch diverges
    #[test]
    fn every_branch_diverging_reads_nothing() {
        let out = check(indoc! {"
            def f(c: bool) -> int:
                a = if c:
                    raise ValueError()
                else:
                    raise TypeError()
                return a
        "});
        assert!(!out.contains("a = __by_stmt_expr_0__"), "got:\n{out}");
        assert!(out.contains("raise TypeError()"), "got:\n{out}");
    }

    /// a suite continues the line its statement starts on, so the pass refuses
    /// rather than emitting a compound statement after a semicolon
    #[test]
    fn suite_after_a_semicolon_is_refused() {
        let err = transpile(
            indoc! {"
                def f(x: int) -> int:
                    p = 1; q = match x:
                        case _:
                            2
                    return p + q
            "},
            &Config::test_default(),
        );
        assert!(err.is_err(), "expected a transpile error, got: {err:?}");
    }

    /// the moved prefix carries the statement's own lowering with it: a
    /// declaration's keyword is rewritten by an edit on the very range the
    /// prefix occupies, and re-emitting the prefix as text would drop that edit
    /// and leak the keyword into the output
    #[test]
    fn a_declarations_lowering_survives_the_moved_prefix() {
        for (declaration, lowered) in [
            ("let a", "a: Final"),
            ("var a", "a"),
            ("let a: int", "a: Final[int]"),
            ("final a: int", "a: Final[int]"),
            ("private a: int", "a: int"),
        ] {
            let out = check(&format!(
                "b: int? = None\n{declaration} = b ?? raise ValueError()\nprint(a)\n"
            ));
            assert!(
                out.contains(&format!("{lowered} = __by_stmt_expr_0__\nprint(a)")),
                "`{declaration}`, got:\n{out}"
            );
            assert!(!out.contains(declaration), "`{declaration}`, got:\n{out}");
        }
    }

    /// the suite-bearing form moves the same prefix, so it lowers the same way
    #[test]
    fn a_declaration_takes_a_suite_bearing_value() {
        let out = check(indoc! {"
            let a = match 1:
                case 1:
                    2
                case _:
                    3
            print(a)
        "});
        assert!(
            out.contains(indoc! {"
                match 1:
                    case 1:
                        __by_stmt_expr_0__ = 2
                    case _:
                        __by_stmt_expr_0__ = 3
                a: Final = __by_stmt_expr_0__
            "}),
            "got:\n{out}"
        );
    }

    /// the prefix is re-emitted from source, so the separator before the value —
    /// which may be an unbalanced `(` — is normalised rather than carried along
    #[test]
    fn a_moved_prefix_normalises_its_separator() {
        let parenthesized = check(indoc! {"
            b: int? = None
            let a = (b ?? raise ValueError())
            print(a)
        "});
        assert!(
            parenthesized.contains("a: Final = __by_stmt_expr_0__"),
            "got:\n{parenthesized}"
        );

        let spaced = check(indoc! {"
            b: int? = None
            let a   =   b ?? raise ValueError()
            print(a)
        "});
        assert!(
            spaced.contains("a: Final = __by_stmt_expr_0__"),
            "got:\n{spaced}"
        );
    }

    #[test]
    fn comments_inside_a_branch_survive() {
        let out = check(indoc! {"
            def f(c: bool) -> int:
                a = if c:
                    # a comment
                    1
                else:
                    2
                return a
        "});
        assert!(out.contains("# a comment"), "got:\n{out}");
    }
}
