//! Lowering for the pattern-matching `if let <pattern> := <subject>:` clause.
//!
//! A clause matches when its pattern matches, binding the pattern's captures in
//! the enclosing scope — the semantics of a single `match` case. There is no
//! python spelling that keeps a `match` inside an `if` header, so the whole
//! chain is flattened onto a selector variable: each clause header is rewritten
//! to a guard that records *which* clause was taken, followed by a plain `if`
//! that runs the original body.
//!
//! ```text
//! if let int(x) := v:           __by_if_let_0__ = 0
//!     use(x)                    match v:
//! elif fallback:                    case int(x):
//!     other()                           __by_if_let_0__ = 1
//! else:                    ⇒    if __by_if_let_0__ == 1:
//!     last()                        use(x)
//!                               if __by_if_let_0__ == 0:
//!                                   if fallback:
//!                                       __by_if_let_0__ = 2
//!                               if __by_if_let_0__ == 2:
//!                                   other()
//!                               if __by_if_let_0__ == 0:
//!                                   last()
//!                               del __by_if_let_0__
//! ```
//!
//! Only the header spans (clause keyword through its colon) are replaced, so
//! every body keeps its exact source bytes — comments survive and the lowerings
//! nested inside a body compose untouched. Subject and pattern pass through as
//! [`Fragment::Src`] spans for the same reason.
//!
//! A guard for clause `k` only runs when no earlier clause was taken, so a
//! subject is evaluated exactly as lazily as it was written. The selector is
//! dropped after the chain so it never lingers as a member of the surrounding
//! namespace.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Pattern, PythonVersion, Stmt, StmtIf};
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::destructure::{NameGen, push_destructure};
use super::source_util::{line_indent, temporary_name};
use crate::type_info::TypeInfo;

/// `match` statements — which the lowering emits — are python 3.10 syntax.
const MIN_VERSION: PythonVersion = PythonVersion::PY310;

pub(crate) struct IfLetPass<'src> {
    source: &'src str,
    min_version: PythonVersion,
}

impl<'src> IfLetPass<'src> {
    pub(crate) fn new(source: &'src str, min_version: PythonVersion) -> Self {
        Self {
            source,
            min_version,
        }
    }
}

impl TypeAwarePass for IfLetPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut lower = IfLetLower {
            source: self.source,
            types,
            edits: Vec::new(),
            errors: Vec::new(),
            counter: 0,
            supported: self.min_version >= MIN_VERSION,
            names: NameGen::default(),
        };
        for stmt in stmts {
            lower.visit_stmt(stmt);
        }
        ctx.template_edits.extend(lower.edits);
        ctx.errors.extend(lower.errors);
    }
}

/// One clause of an `if` chain: its keyword start, the subject/condition, and
/// the pattern when it is an `if let` clause. An `else` clause has neither.
struct Clause<'ast> {
    start: TextSize,
    pattern: Option<&'ast Pattern>,
    test: Option<&'ast Expr>,
}

struct IfLetLower<'a, 'src> {
    source: &'src str,
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    errors: Vec<String>,
    /// monotonic across the file so sibling chains get distinct selectors
    counter: usize,
    supported: bool,
    /// names the temporaries a clause's destructuring needs
    names: NameGen,
}

impl<'ast> Visitor<'ast> for IfLetLower<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::If(if_stmt) = stmt {
            self.process(if_stmt);
        }
        walk_stmt(self, stmt);
    }
}

impl IfLetLower<'_, '_> {
    /// A selector name that is unbound at the statement's scope, so the
    /// generated assignments can't shadow anything the source binds
    fn fresh_selector(&mut self, anchor: &Expr) -> String {
        loop {
            let name = temporary_name("if_let", self.counter);
            self.counter += 1;
            if self.types.is_unbound_at(&name, anchor) {
                return name;
            }
        }
    }

    /// The end of the header colon that terminates the clause, scanning from
    /// `after` — the end of the clause's condition, or the start of a bare
    /// `else`. Comments, the `else` keyword, and the closing parentheses of a
    /// parenthesized condition are skipped
    fn colon_end(&self, after: TextSize) -> Option<TextSize> {
        SimpleTokenizer::starts_at(after, self.source)
            .skip_trivia()
            .find(|token| {
                !matches!(
                    token.kind(),
                    SimpleTokenKind::RParen | SimpleTokenKind::Else
                )
            })
            .filter(|token| token.kind() == SimpleTokenKind::Colon)
            .map(|token| token.range().end())
    }

    /// The 1-based line number of `offset`, for diagnostics.
    fn line_of(&self, offset: TextSize) -> usize {
        1 + self.source[..usize::from(offset)].matches('\n').count()
    }

    fn process(&mut self, if_stmt: &StmtIf) {
        // the overwhelming majority of `if` statements carry no pattern; check
        // before building anything
        if if_stmt.pattern.is_none()
            && if_stmt
                .elif_else_clauses
                .iter()
                .all(|clause| clause.pattern.is_none())
        {
            return;
        }

        if !self.supported {
            self.errors.push(format!(
                "`if let` needs python 3.10 or later (it lowers to a `match` statement) \
                (line {})",
                self.line_of(if_stmt.range().start()),
            ));
            return;
        }

        let mut clauses = vec![Clause {
            start: if_stmt.range().start(),
            pattern: if_stmt.pattern.as_deref(),
            test: Some(&if_stmt.test),
        }];
        clauses.extend(if_stmt.elif_else_clauses.iter().map(|clause| Clause {
            start: clause.range().start(),
            pattern: clause.pattern.as_deref(),
            test: clause.test.as_ref(),
        }));

        let indent = line_indent(self.source, if_stmt.range().start()).to_owned();
        let selector = self.fresh_selector(&if_stmt.test);

        let mut edits = Vec::with_capacity(clauses.len());
        for (index, clause) in clauses.iter().enumerate() {
            // the header ends at the colon after the condition, or after the
            // clause keyword itself for `else`
            let scan_from = clause.test.map_or(clause.start, Ranged::end);
            let Some(colon_end) = self.colon_end(scan_from) else {
                // without every header span the chain can only be rewritten in
                // part, which is worse than not at all. surface it here: left
                // alone, the clause reaches the output and the syntax check
                // reports it as basedpython-only syntax in a `.py` file, which
                // says nothing about what actually went wrong
                self.errors.push(format!(
                    "could not find the `:` ending an `if let` clause header (line {})",
                    self.line_of(clause.start),
                ));
                return;
            };
            // clause 0 is entered unconditionally, so its guard needs no
            // `selector == 0` wrapper; the selector is initialised there instead
            let mut fragments = Vec::new();
            match (index, clause.test) {
                (0, Some(test)) => {
                    fragments.push(Fragment::Lit(format!("{selector} = 0\n{indent}")));
                    if let Err(error) = push_guard(
                        &mut fragments,
                        &indent,
                        &selector,
                        1,
                        clause.pattern,
                        test,
                        &mut self.names,
                    ) {
                        self.errors
                            .push(format!("{error} (line {})", self.line_of(clause.start)));
                        return;
                    }
                    fragments.push(Fragment::Lit(format!("\n{indent}if {selector} == 1:")));
                }
                (_, Some(test)) => {
                    fragments.push(Fragment::Lit(format!("if {selector} == 0:\n{indent}    ")));
                    if let Err(error) = push_guard(
                        &mut fragments,
                        &format!("{indent}    "),
                        &selector,
                        index + 1,
                        clause.pattern,
                        test,
                        &mut self.names,
                    ) {
                        self.errors
                            .push(format!("{error} (line {})", self.line_of(clause.start)));
                        return;
                    }
                    fragments.push(Fragment::Lit(format!(
                        "\n{indent}if {selector} == {}:",
                        index + 1
                    )));
                }
                // `else`: taken exactly when no clause was
                (_, None) => {
                    fragments.push(Fragment::Lit(format!("if {selector} == 0:")));
                }
            }

            // the last clause's edit runs to the end of the statement, passing
            // its body through, so the trailing `del` sits *inside* the span
            // rather than at its boundary. an insertion at the boundary is not
            // claimed by an enclosing template (see the driver's claim pass), so
            // it would escape a construct that re-emits this statement's source
            // somewhere else — a trailing-lambda block would leave the `del`
            // behind, outside the function it belongs to
            let end = if index + 1 == clauses.len() {
                let body = TextRange::new(colon_end, if_stmt.range().end());
                fragments.push(Fragment::Src(body));
                // the selector is machinery, not a binding the source asked for;
                // left behind it becomes a member of whatever namespace the
                // statement sits in — a class attribute, or an outright bogus
                // variant in an `enum class` body
                fragments.push(Fragment::Lit(format!("\n{indent}del {selector}")));
                body.end()
            } else {
                colon_end
            };
            edits.push((TextRange::new(clause.start, end), fragments));
        }

        self.edits.extend(edits);
    }
}

/// Appends the guard that records clause `value` as taken: the destructuring a
/// pattern clause stands for, a plain `if` for a condition
fn push_guard(
    fragments: &mut Vec<Fragment>,
    indent: &str,
    selector: &str,
    value: usize,
    pattern: Option<&Pattern>,
    test: &Expr,
    names: &mut NameGen,
) -> Result<(), String> {
    match pattern {
        Some(pattern) => push_destructure(
            fragments,
            indent,
            &[Fragment::Src(test.range())],
            pattern,
            None,
            &format!("{selector} = {value}"),
            names,
        ),
        None => {
            fragments.push(Fragment::Lit("if ".to_owned()));
            fragments.push(Fragment::Src(test.range()));
            fragments.push(Fragment::Lit(format!(
                ":\n{indent}    {selector} = {value}"
            )));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::{Config, transpile};
    use ruff_python_ast::PythonVersion;

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    #[test]
    fn single_clause_lowers_to_a_one_case_match() {
        let out = check(indoc! {"
            opt: int | None = 1
            if let int(x) := opt:
                print(x)
        "});
        assert!(
            out.contains(indoc! {"
                __by_if_let_0__ = 0
                match opt:
                    case int(x):
                        __by_if_let_0__ = 1
                if __by_if_let_0__ == 1:
                    print(x)
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn else_runs_when_nothing_matched() {
        let out = check(indoc! {"
            opt: int | None = 1
            if let int(x) := opt:
                print(x)
            else:
                print('nope')
        "});
        assert!(
            out.contains(indoc! {"
                if __by_if_let_0__ == 0:
                    print('nope')
            "}),
            "got:\n{out}"
        );
    }

    /// an `elif let` subject may only be evaluated once every earlier clause has
    /// failed, so its `match` is nested under the selector check
    #[test]
    fn elif_let_subject_is_evaluated_lazily() {
        let out = check(indoc! {"
            def side_effect() -> int | None:
                return 1

            if False:
                pass
            elif let int(x) := side_effect():
                print(x)
        "});
        assert!(
            out.contains(indoc! {"
                if __by_if_let_0__ == 0:
                    match side_effect():
                        case int(x):
                            __by_if_let_0__ = 2
                if __by_if_let_0__ == 2:
                    print(x)
            "}),
            "got:\n{out}"
        );
    }

    /// bodies are never re-rendered, so a construct inside one lowers as usual
    #[test]
    fn body_lowerings_compose() {
        let out = check(indoc! {"
            opt: int | None = 1
            other: int | None = None
            if let int(x) := opt:
                y = other ?? 0
        "});
        assert!(!out.contains("??"), "got:\n{out}");
        assert!(out.contains("    y = "), "got:\n{out}");
    }

    /// the subject passes through as a source span, so its own lowerings survive
    #[test]
    fn subject_lowerings_compose() {
        let out = check(indoc! {"
            a: int | None = None
            if let int(x) := a ?? 3:
                print(x)
        "});
        assert!(!out.contains("??"), "got:\n{out}");
        assert!(
            out.contains("match a if a is not None else 3:"),
            "got:\n{out}"
        );
    }

    #[test]
    fn one_line_body_stays_inline() {
        let out = check(indoc! {"
            opt: int | None = 1
            if let int(x) := opt: print(x)
        "});
        assert!(
            out.contains("if __by_if_let_0__ == 1: print(x)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn nested_chains_get_distinct_selectors() {
        let out = check(indoc! {"
            opt: int | None = 1
            if let int(x) := opt:
                if let int(y) := opt:
                    print(x + y)
        "});
        assert!(out.contains("__by_if_let_0__ = 0"), "got:\n{out}");
        assert!(out.contains("    __by_if_let_1__ = 0"), "got:\n{out}");
    }

    #[test]
    fn plain_if_chain_is_untouched() {
        let out = check(indoc! {"
            x = 1
            if x:
                print(x)
            elif not x:
                print(0)
        "});
        assert!(!out.contains("_by_if_let"), "got:\n{out}");
    }

    /// a chain that mixes conditions and patterns keeps each clause's own kind
    /// of guard
    #[test]
    fn mixed_clauses() {
        let out = check(indoc! {"
            opt: int | None = 1
            flag = True
            if flag:
                print('flag')
            elif let int(x) := opt:
                print(x)
            else:
                print('none')
        "});
        assert!(
            out.contains(indoc! {"
                __by_if_let_0__ = 0
                if flag:
                    __by_if_let_0__ = 1
                if __by_if_let_0__ == 1:
                    print('flag')
            "}),
            "got:\n{out}"
        );
    }

    /// the selector is machinery, not a binding the source asked for — it must
    /// not survive into the namespace the statement sits in (a class body turns
    /// a stray assignment into an attribute, an `enum class` body into a variant)
    #[test]
    fn the_selector_is_dropped_after_the_chain() {
        let out = check(indoc! {"
            class C:
                v: int | str = 1
                if let int(k) := v:
                    w = k
        "});
        assert!(
            out.contains("    if __by_if_let_0__ == 1:\n        w = k\n    del __by_if_let_0__\n"),
            "got:\n{out}"
        );
    }

    /// nested chains end at the same offset, and the `del`s must dedent outward
    /// or the output is a syntax error
    #[test]
    fn nested_chains_drop_their_selectors_innermost_first() {
        let out = check(indoc! {"
            opt: int | None = 1
            if let int(x) := opt:
                if let int(y) := opt:
                    print(x + y)
        "});
        assert!(
            out.contains(indoc! {"
                        print(x + y)
                    del __by_if_let_1__
                del __by_if_let_0__
            "}),
            "got:\n{out}"
        );
    }

    /// a chain inside another construct's re-emitted body must be lowered in
    /// place — including its trailing `del`, which stays inside that body
    #[test]
    fn composes_inside_a_trailing_lambda_block() {
        let out = check(indoc! {"
            def with_resource(once fn: (int) -> None):
                fn(42)

            v: int | str = 1

            with_resource:
                if let int(n) := v:
                    print(n)
        "});
        assert!(
            out.contains(indoc! {"
                def _trailing_lambda_0(it=None):
                    __by_if_let_0__ = 0
                    match v:
                        case int(n):
                            __by_if_let_0__ = 1
                    if __by_if_let_0__ == 1:
                        print(n)
                    del __by_if_let_0__
                with_resource(fn=_trailing_lambda_0)
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn needs_python_310() {
        let config = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        let err = transpile(
            indoc! {"
                opt: int | None = 1
                if let int(x) := opt:
                    print(x)
            "},
            &config,
        )
        .unwrap_err();
        assert!(err.contains("`if let` needs python 3.10"), "got:\n{err}");
        assert!(err.contains("(line 2)"), "reports the line, got:\n{err}");
    }

    /// a chain nested in another statement is reported too — the walk descends
    /// past the statement it could not lower
    #[test]
    fn needs_python_310_when_nested() {
        let config = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        let err = transpile(
            indoc! {"
                def f(opt: int | None):
                    if opt:
                        if let int(x) := opt:
                            print(x)
            "},
            &config,
        )
        .unwrap_err();
        assert!(err.contains("`if let` needs python 3.10"), "got:\n{err}");
        assert!(err.contains("(line 3)"), "reports the line, got:\n{err}");
    }
}
