//! Lowering for basedpython destructuring: the `let <pattern> := <subject>`
//! statement, patterns in binding positions (`for`, `with`, parameters), and the
//! `and` pattern, which python has no spelling for.
//!
//! Every form comes down to the same thing — bind a pattern's captures from a
//! value — so they all lower through [`push_destructure`], which emits the
//! single-case `match` that does it:
//!
//! ```text
//! let Point(x, y) := origin      ⇒    match origin:
//!                                         case Point(x, y):
//!                                             pass
//! ```
//!
//! A `match` case binds in the enclosing scope, so the captures outlive the
//! statement exactly as the source says they do.
//!
//! An `and` pattern matches a value against each of its conjuncts in turn, which
//! is a `match` per conjunct nested inside the previous one. A conjunction
//! nested inside another pattern is hoisted out: it is replaced by a binder that
//! captures that position, and the conjunction is matched against the binder
//! afterwards. So `A(foo=int() and B(y))` becomes
//!
//! ```text
//! match subject:
//!     case A(foo=__by_and_N__):
//!         match __by_and_N__:
//!             case int():
//!                 match __by_and_N__:
//!                     case B(y):
//!                         ...
//! ```
//!
//! Hoisting cannot cross a `|`, whose alternatives must all bind the same names,
//! so an `and` there is a transpile error.
//!
//! The binding positions rewrite their pattern to the binder the parser already
//! named it with, and put the destructure at the top of the body:
//!
//! ```text
//! for Point(x, y) in points:     ⇒    for __by_destructure_N__ in points:
//!     use(x, y)                           match __by_destructure_N__:
//!                                             case Point(x, y):
//!                                                 pass
//!                                         del __by_destructure_N__
//!                                         use(x, y)
//! ```
//!
//! Only the header and the point the body starts at are rewritten, so every body
//! keeps its exact source bytes and the lowerings inside it compose.
//!
//! The binder is dropped as soon as the captures are bound. Left behind it would
//! become a member of whatever namespace surrounds it — a class attribute, or a
//! bogus variant in an `enum class` body — and nothing after the destructure
//! reads it. The drop goes at the *top* of the body rather than after the
//! statement so it runs whatever the body does with the iteration, and so a
//! `for` re-binds it on the next one.

use ruff_python_ast::visitor::{Visitor, walk_pattern, walk_stmt};
use ruff_python_ast::{
    AnyParameterRef, Expr, ModModule, Parameters, Pattern, PatternMatchAnd, Stmt, StmtFor, StmtLet,
    StmtMatch, StmtWith,
};
use ruff_python_parser::semantic_errors::AND_PATTERN_IN_ALTERNATIVE;
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, Fragment, PassContext};
use super::source_util::{line_indent, line_start, temporary_name};

pub(crate) struct DestructurePass<'src> {
    source: &'src str,
}

impl<'src> DestructurePass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for DestructurePass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut lower = DestructureLower {
            source: self.source,
            edits: Vec::new(),
            errors: Vec::new(),
            names: NameGen::default(),
        };
        for stmt in &module.body {
            lower.visit_stmt(stmt);
        }
        ctx.template_edits.extend(lower.edits);
        ctx.errors.extend(lower.errors);
    }
}

struct DestructureLower<'src> {
    source: &'src str,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    errors: Vec<String>,
    names: NameGen,
}

impl<'ast> Visitor<'ast> for DestructureLower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::Let(let_stmt) => self.lower_let(let_stmt),
            Stmt::For(for_stmt) => self.lower_for(for_stmt),
            Stmt::With(with_stmt) => self.lower_with(with_stmt),
            Stmt::FunctionDef(function) => {
                self.lower_parameters(&function.parameters, &function.body);
            }
            Stmt::Match(match_stmt) => self.lower_match(match_stmt),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

impl DestructureLower<'_> {
    /// The 1-based line number of `offset`, for diagnostics.
    fn line_of(&self, offset: TextSize) -> usize {
        1 + self.source[..usize::from(offset)].matches('\n').count()
    }

    /// The end of the `:` that ends the header of the block `body` belongs to.
    ///
    /// It is the last colon before the body starts: a return annotation or an
    /// iterable can contain colons of its own, and every one of those comes
    /// before the header's
    ///
    /// `None` when `body` is empty — there is no header colon to find and nothing
    /// that could read the captures, so the caller has nothing to do
    fn header_colon_end(&self, from: TextSize, body: &[Stmt]) -> Option<TextSize> {
        let body_start = body.first()?.range().start();
        SimpleTokenizer::starts_at(from, self.source)
            .skip_trivia()
            .take_while(|token| token.range().end() <= body_start)
            .filter(|token| token.kind() == SimpleTokenKind::Colon)
            .last()
            .map(|token| token.range().end())
    }

    /// Whether the `let` is the only statement on its line, which its lowering
    /// needs: it emits a block of statements where one stood, and a block cannot
    /// share a line with anything.
    ///
    /// `let` is the only form here that can share one — every other is a compound
    /// statement, which python already refuses to write after a `;` or a one-line
    /// block header.
    ///
    /// Reported rather than worked around, because giving the block its own line
    /// means rewriting the statement that shares it — an `if` header, or the
    /// neighbour across a `;` — which this pass does not own. Left alone, the
    /// output is either invalid python (`if q: match p:`) or, for a trailing
    /// neighbour, a statement that silently became part of the `case` body and so
    /// only runs when the pattern matched
    fn alone_on_its_line(&mut self, let_stmt: &StmtLet) -> bool {
        let range = let_stmt.range();
        let before = &self.source
            [usize::from(line_start(self.source, range.start()))..usize::from(range.start())];
        let shares_line = !before.trim().is_empty()
            || SimpleTokenizer::starts_at(range.end(), self.source)
                .skip_trivia()
                .next()
                .is_some_and(|token| token.kind() == SimpleTokenKind::Semi);
        if shares_line {
            self.errors.push(format!(
                "a destructuring `let` has to be the only statement on its line \
                (it lowers to a `match` block) (line {})",
                self.line_of(range.start()),
            ));
        }
        !shares_line
    }

    /// The end of the `:` that ends a clause header, scanning from `after` —
    /// the end of the header's last expression. Comments, the `else` keyword and
    /// the closing parentheses of a parenthesized header are skipped
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

    /// basedpython `let <pattern> := <subject> [else: ...]`.
    fn lower_let(&mut self, let_stmt: &StmtLet) {
        if !self.alone_on_its_line(let_stmt) {
            return;
        }
        let indent = line_indent(self.source, let_stmt.range().start()).to_owned();
        let subject = vec![Fragment::Src(let_stmt.value.range())];

        if let_stmt.orelse.is_empty() {
            let mut fragments = Vec::new();
            if !self.push_destructure(&mut fragments, &indent, &subject, &let_stmt.pattern, "pass")
            {
                return;
            }
            self.edits.push((let_stmt.range(), fragments));
            return;
        }

        // the `else` block runs when the pattern did not match, which a `match`
        // cannot say without re-indenting the block into a `case _:` arm. A
        // selector records whether the pattern matched instead, so the block
        // keeps its own source bytes
        let Some(colon_end) = self.colon_end(let_stmt.value.range().end()) else {
            self.errors.push(format!(
                "could not find the `:` ending a `let ... else` header (line {})",
                self.line_of(let_stmt.range().start()),
            ));
            return;
        };
        let selector = temporary_name("let", u32::from(let_stmt.range().start()));

        let mut fragments = vec![Fragment::Lit(format!("{selector} = 0\n{indent}"))];
        if !self.push_destructure(
            &mut fragments,
            &indent,
            &subject,
            &let_stmt.pattern,
            &format!("{selector} = 1"),
        ) {
            return;
        }
        fragments.push(Fragment::Lit(format!("\n{indent}if {selector} == 0:")));
        // the block passes through from the colon, so it is never re-indented,
        // and the trailing `del` sits inside this span rather than at its
        // boundary — an insertion at the boundary escapes an enclosing template
        fragments.push(Fragment::Src(TextRange::new(
            colon_end,
            let_stmt.range().end(),
        )));
        fragments.push(Fragment::Lit(format!("\n{indent}del {selector}")));
        self.edits.push((let_stmt.range(), fragments));
    }

    /// `for <pattern> in <iterable>:` — the loop binds the element to the
    /// pattern's binder and destructures it at the top of the body.
    fn lower_for(&mut self, for_stmt: &StmtFor) {
        let Some(pattern) = for_stmt.pattern.as_deref() else {
            return;
        };
        let Some(colon_end) = self.header_colon_end(for_stmt.iter.range().end(), &for_stmt.body)
        else {
            // an empty body cannot read the captures, so there is nothing to bind
            if !for_stmt.body.is_empty() {
                self.errors.push(format!(
                    "could not find the `:` ending a `for` header (line {})",
                    self.line_of(for_stmt.range().start()),
                ));
            }
            return;
        };
        let Expr::Name(binder) = &*for_stmt.target else {
            return;
        };
        self.push_binder_destructure(pattern, &binder.id, colon_end, &for_stmt.body, false);
    }

    /// `with <expr> as <pattern>:` — one binder per destructuring item.
    fn lower_with(&mut self, with_stmt: &StmtWith) {
        let destructuring = with_stmt
            .items
            .iter()
            .filter_map(
                |item| match (item.pattern.as_deref(), item.optional_vars.as_deref()) {
                    (Some(pattern), Some(Expr::Name(binder))) => Some((pattern, binder)),
                    _ => None,
                },
            )
            .collect::<Vec<_>>();
        if destructuring.is_empty() {
            return;
        }

        let after_items = with_stmt
            .items
            .last()
            .map_or_else(|| with_stmt.range().end(), |last| last.range().end());
        let Some(colon_end) = self.header_colon_end(after_items, &with_stmt.body) else {
            // an empty body cannot read the captures, so there is nothing to bind
            if !with_stmt.body.is_empty() {
                self.errors.push(format!(
                    "could not find the `:` ending a `with` header (line {})",
                    self.line_of(with_stmt.range().start()),
                ));
            }
            return;
        };

        for (pattern, binder) in destructuring {
            self.push_binder_destructure(pattern, &binder.id, colon_end, &with_stmt.body, false);
        }
    }

    /// `def f(<pattern>: T)` — one binder per destructuring parameter.
    fn lower_parameters(&mut self, parameters: &Parameters, body: &[Stmt]) {
        let destructuring = parameters
            .iter()
            .map(AnyParameterRef::as_parameter)
            .filter_map(|parameter| Some((parameter.pattern.as_deref()?, parameter)))
            .collect::<Vec<_>>();
        let Some((first, _)) = destructuring.first() else {
            return;
        };

        let Some(colon_end) = self.header_colon_end(parameters.range().end(), body) else {
            // an empty body cannot read the captures, so there is nothing to bind
            if !body.is_empty() {
                self.errors.push(format!(
                    "could not find the `:` ending a `def` header (line {})",
                    self.line_of(first.range().start()),
                ));
            }
            return;
        };

        for (pattern, parameter) in destructuring {
            self.push_binder_destructure(pattern, &parameter.name.id, colon_end, body, true);
        }
    }

    /// Rewrites a binding position's pattern to its binder and puts the
    /// destructure at the top of `body`.
    ///
    /// One edit covers both, running from the pattern to the point the body
    /// starts at: the header between them passes through as source, so the
    /// lowerings inside it compose, and the body itself is never touched.
    fn push_binder_destructure(
        &mut self,
        pattern: &Pattern,
        binder: &str,
        colon_end: TextSize,
        body: &[Stmt],
        skip_docstring: bool,
    ) {
        // a docstring has to stay the first statement in the body, so the
        // destructure goes after it
        let leading_docstring = skip_docstring
            .then(|| body.first())
            .flatten()
            .filter(|stmt| matches!(stmt, Stmt::Expr(expr) if expr.value.is_string_literal_expr()));
        let anchor_stmt = match leading_docstring {
            Some(_) => body.get(1),
            None => body.first(),
        };

        // `header_end` is where the header's own source stops passing through;
        // `span_end` is where this edit stops, which is further along when the
        // whitespace between them has to be replaced by a line break
        let (header_end, span_end, body_indent) = match (leading_docstring, anchor_stmt) {
            // the block goes after the docstring, on its own line
            (Some(docstring), _) => {
                let end = docstring.range().end();
                (
                    end,
                    end,
                    line_indent(self.source, docstring.range().start()).to_owned(),
                )
            }
            (None, Some(stmt)) => {
                let stmt_start = stmt.range().start();
                let same_line =
                    !self.source[usize::from(colon_end)..usize::from(stmt_start)].contains('\n');
                if same_line {
                    // a one-line body: the whitespace after the colon is
                    // consumed, and the block's own line break replaces it, so
                    // the body ends up in a block of its own
                    let indent = format!("{}    ", line_indent(self.source, colon_end));
                    (colon_end, stmt_start, indent)
                } else {
                    // stop at the end of the header line so a comment there
                    // survives; the body's own line break and indentation follow
                    let line_end = self.source[usize::from(colon_end)..]
                        .find('\n')
                        .map_or(colon_end, |offset| {
                            colon_end + TextSize::try_from(offset).unwrap_or_default()
                        });
                    (
                        line_end,
                        line_end,
                        line_indent(self.source, stmt_start).to_owned(),
                    )
                }
            }
            // a body of nothing but a docstring, or no body at all: nothing can
            // read the captures, so there is nothing to bind
            (None, None) => return,
        };

        let mut fragments = vec![
            Fragment::Lit(binder.to_owned()),
            Fragment::Src(TextRange::new(pattern.range().end(), header_end)),
            Fragment::Lit(format!("\n{body_indent}")),
        ];
        let subject = vec![Fragment::Lit(binder.to_owned())];
        if !self.push_destructure(&mut fragments, &body_indent, &subject, pattern, "pass") {
            return;
        }
        fragments.push(Fragment::Lit(format!("\n{body_indent}del {binder}")));
        if span_end != header_end {
            fragments.push(Fragment::Lit(format!("\n{body_indent}")));
        }
        self.edits
            .push((TextRange::new(pattern.range().start(), span_end), fragments));
    }

    /// A `match` statement whose cases use `and` patterns cannot stay a `match`:
    /// a conjunction is a nested match, and a nested match cannot fall through
    /// to the next case. The whole statement flattens onto a selector instead,
    /// exactly as an `if let` chain does, so each case is tried only when no
    /// earlier one matched.
    fn lower_match(&mut self, match_stmt: &StmtMatch) {
        if !match_stmt
            .cases
            .iter()
            .any(|case| contains_and_pattern(&case.pattern))
        {
            return;
        }

        let indent = line_indent(self.source, match_stmt.range().start()).to_owned();
        let case_indent = format!("{indent}    ");
        let offset = u32::from(match_stmt.range().start());
        let subject_name = temporary_name("subject", offset);
        let selector = temporary_name("case", offset);

        let Some(header_colon) = self.colon_end(match_stmt.subject.range().end()) else {
            self.errors.push(format!(
                "could not find the `:` ending a `match` header (line {})",
                self.line_of(match_stmt.range().start()),
            ));
            return;
        };

        // the subject is evaluated once, and every case guard reads it from
        // there. `if True:` hosts the cases at the indentation their bodies
        // already have, so no body moves
        let mut edits = vec![(
            TextRange::new(match_stmt.range().start(), header_colon),
            vec![
                Fragment::Lit(format!("{subject_name} = ")),
                Fragment::Src(match_stmt.subject.range()),
                Fragment::Lit(format!("\n{indent}{selector} = 0\n{indent}if True:")),
            ],
        )];

        for (index, case) in match_stmt.cases.iter().enumerate() {
            let scan_from = case
                .guard
                .as_ref()
                .map_or_else(|| case.pattern.range().end(), |guard| guard.range().end());
            let Some(colon_end) = self.colon_end(scan_from) else {
                self.errors.push(format!(
                    "could not find the `:` ending a `case` header (line {})",
                    self.line_of(case.range().start()),
                ));
                return;
            };

            let guard_indent = format!("{case_indent}    ");
            let mut fragments = vec![Fragment::Lit(format!(
                "if {selector} == 0:\n{guard_indent}"
            ))];
            if !self.push_destructure_with_guard(
                &mut fragments,
                &guard_indent,
                &[Fragment::Lit(subject_name.clone())],
                &case.pattern,
                case.guard.as_deref(),
                &format!("{selector} = {}", index + 1),
            ) {
                return;
            }
            fragments.push(Fragment::Lit(format!(
                "\n{case_indent}if {selector} == {}:",
                index + 1
            )));

            // the last case's edit runs to the end of the statement so the
            // trailing `del`s sit inside the span rather than at its boundary
            let end = if index + 1 == match_stmt.cases.len() {
                let body = TextRange::new(colon_end, match_stmt.range().end());
                fragments.push(Fragment::Src(body));
                fragments.push(Fragment::Lit(format!(
                    "\n{indent}del {selector}\n{indent}del {subject_name}"
                )));
                body.end()
            } else {
                colon_end
            };
            edits.push((TextRange::new(case.range().start(), end), fragments));
        }

        self.edits.extend(edits);
    }

    /// [`push_destructure`], reporting a pattern that cannot be lowered against
    /// the line it is written on. Returns `false` when it could not be lowered.
    fn push_destructure(
        &mut self,
        fragments: &mut Vec<Fragment>,
        indent: &str,
        subject: &[Fragment],
        pattern: &Pattern,
        on_match: &str,
    ) -> bool {
        self.push_destructure_with_guard(fragments, indent, subject, pattern, None, on_match)
    }

    /// [`Self::push_destructure`] with a `case` guard.
    fn push_destructure_with_guard(
        &mut self,
        fragments: &mut Vec<Fragment>,
        indent: &str,
        subject: &[Fragment],
        pattern: &Pattern,
        guard: Option<&Expr>,
        on_match: &str,
    ) -> bool {
        match push_destructure(
            fragments,
            indent,
            subject,
            pattern,
            guard,
            on_match,
            &mut self.names,
        ) {
            Ok(()) => true,
            Err(error) => {
                self.errors.push(format!(
                    "{error} (line {})",
                    self.line_of(pattern.range().start())
                ));
                false
            }
        }
    }
}

/// Appends the `match` chain that binds `pattern`'s captures from `subject` and
/// then runs `on_match`, at `indent`.
///
/// `guard` is a `case` guard, which is applied once the whole pattern has
/// matched. Fails only for an `and` pattern inside a `|` alternative, which
/// cannot be hoisted out of it.
pub(crate) fn push_destructure(
    fragments: &mut Vec<Fragment>,
    indent: &str,
    subject: &[Fragment],
    pattern: &Pattern,
    guard: Option<&Expr>,
    on_match: &str,
    names: &mut NameGen,
) -> Result<(), String> {
    // every temporary this lowering makes, to be dropped once the nest is done
    let mut temporaries = Vec::new();

    // a conjunction matches the same value more than once, so the value has to
    // be reachable by name. A binding position's subject already is
    let hoisted = (matches!(pattern, Pattern::MatchAnd(_)) && !is_name_fragment(subject))
        .then(|| names.next("subject"));
    let subject: Vec<Fragment> = match &hoisted {
        Some(name) => {
            fragments.push(Fragment::Lit(format!("{name} = ")));
            fragments.extend(subject.iter().cloned());
            fragments.push(Fragment::Lit(format!("\n{indent}")));
            temporaries.push(name.clone());
            vec![Fragment::Lit(name.clone())]
        }
        None => subject.to_vec(),
    };

    let mut steps = Vec::new();
    let mut binders = Vec::new();
    collect_steps(&subject, pattern, names, &mut binders, &mut steps)?;

    // a hoisted conjunction's binder is bound by the pattern that captured it,
    // which a failed match may or may not have reached — python does not say how
    // far it binds before giving up. Binding them up front is what makes the
    // `del` below safe
    for binder in &binders {
        fragments.push(Fragment::Lit(format!("{binder} = None\n{indent}")));
    }
    temporaries.extend(binders);

    let mut step_indent = indent.to_owned();
    let last = steps.len().saturating_sub(1);
    for (index, step) in steps.iter().enumerate() {
        fragments.push(Fragment::Lit("match ".to_owned()));
        fragments.extend(step.subject.iter().cloned());
        fragments.push(Fragment::Lit(format!(":\n{step_indent}    case ")));
        fragments.extend(step.pattern.iter().cloned());
        // a guard runs once the whole pattern has matched, so it goes on the
        // innermost case
        if index == last
            && let Some(guard) = guard
        {
            fragments.push(Fragment::Lit(" if ".to_owned()));
            fragments.push(Fragment::Src(guard.range()));
        }
        step_indent = format!("{step_indent}        ");
        fragments.push(Fragment::Lit(format!(":\n{step_indent}")));
    }
    fragments.push(Fragment::Lit(on_match.to_owned()));

    // the temporaries are machinery, not bindings the source asked for. left
    // behind they become members of whatever namespace the statement sits in —
    // a class attribute, or an outright bogus variant in an `enum class` body
    if !temporaries.is_empty() {
        fragments.push(Fragment::Lit(format!(
            "\n{indent}del {}",
            temporaries.join(", ")
        )));
    }
    Ok(())
}

/// Names the temporaries a destructuring's lowering needs.
///
/// Counted rather than derived from a byte offset, so that reformatting a file
/// changes its output only where the formatting did. Two passes each holding one
/// can name the same temporary, which is harmless: a temporary never outlives
/// the `match` nest that made it — the nest drops it before any body runs.
#[derive(Default)]
pub(crate) struct NameGen(usize);

impl NameGen {
    fn next(&mut self, kind: &str) -> String {
        let index = self.0;
        self.0 += 1;
        temporary_name(kind, index)
    }
}

/// One `match <subject>: case <pattern>:` of a destructuring.
struct Step {
    subject: Vec<Fragment>,
    pattern: Vec<Fragment>,
}

/// Flattens `pattern` into the single-case matches that together bind exactly
/// its captures: one per conjunct of every `and` it contains, and one for the
/// pattern that is left once they are hoisted out.
fn collect_steps(
    subject: &[Fragment],
    pattern: &Pattern,
    names: &mut NameGen,
    binders: &mut Vec<String>,
    steps: &mut Vec<Step>,
) -> Result<(), String> {
    // every conjunct is matched against the same value, in turn
    if let Pattern::MatchAnd(and) = pattern {
        for conjunct in &and.patterns {
            collect_steps(subject, conjunct, names, binders, steps)?;
        }
        return Ok(());
    }

    let nested = nested_conjunctions(pattern)?;
    // one binder per hoisted conjunction, standing in for the position it was
    // written in
    let hoisted: Vec<String> = nested.iter().map(|_| names.next("and")).collect();
    binders.extend(hoisted.iter().cloned());

    let mut fragments = Vec::new();
    let mut cursor = pattern.range().start();
    for (and, binder) in nested.iter().zip(&hoisted) {
        fragments.push(Fragment::Src(TextRange::new(cursor, and.range().start())));
        fragments.push(Fragment::Lit(binder.clone()));
        cursor = and.range().end();
    }
    fragments.push(Fragment::Src(TextRange::new(cursor, pattern.range().end())));

    steps.push(Step {
        subject: subject.to_vec(),
        pattern: fragments,
    });

    // a hoisted conjunction is matched against the binder that captured its
    // position, after the pattern it was hoisted out of has matched
    for (and, binder) in nested.into_iter().zip(hoisted) {
        let subject = vec![Fragment::Lit(binder)];
        for conjunct in &and.patterns {
            collect_steps(&subject, conjunct, names, binders, steps)?;
        }
    }
    Ok(())
}

/// The outermost `and` patterns nested inside `pattern`, in source order.
///
/// Descent stops at each one: a conjunction is hoisted whole, and the conjuncts
/// inside it are flattened when it is.
fn nested_conjunctions(pattern: &Pattern) -> Result<Vec<&PatternMatchAnd>, String> {
    let mut collector = ConjunctionCollector {
        found: Vec::new(),
        in_alternative: false,
        error: None,
    };
    // the root goes through `visit_pattern` too: a `|` at the root is what puts
    // everything below it inside an alternative
    collector.visit_pattern(pattern);
    match collector.error {
        Some(error) => Err(error),
        None => Ok(collector.found),
    }
}

struct ConjunctionCollector<'a> {
    found: Vec<&'a PatternMatchAnd>,
    in_alternative: bool,
    error: Option<String>,
}

impl<'a> Visitor<'a> for ConjunctionCollector<'a> {
    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        match pattern {
            Pattern::MatchAnd(and) => {
                if self.in_alternative {
                    // hoisting out of an alternative would leave the binder
                    // bound by that alternative alone, which python rejects:
                    // every alternative of a `|` binds the same names
                    self.error = Some(AND_PATTERN_IN_ALTERNATIVE.to_owned());
                } else {
                    self.found.push(and);
                }
            }
            Pattern::MatchOr(_) => {
                let outer = std::mem::replace(&mut self.in_alternative, true);
                walk_pattern(self, pattern);
                self.in_alternative = outer;
            }
            _ => walk_pattern(self, pattern),
        }
    }
}

/// Whether `pattern` has an `and` anywhere inside it.
fn contains_and_pattern(pattern: &Pattern) -> bool {
    matches!(pattern, Pattern::MatchAnd(_))
        // an `and` that cannot be hoisted is still an `and`: the statement has
        // to be rewritten for the error to be reported where it belongs
        || nested_conjunctions(pattern).map_or(true, |nested| !nested.is_empty())
}

/// Whether the fragments are a single name, which a conjunction can re-read
/// without evaluating anything twice.
fn is_name_fragment(subject: &[Fragment]) -> bool {
    matches!(subject, [Fragment::Lit(_)])
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    use crate::{Config, transpile};

    fn check(input: &str) -> String {
        normalize(&transpile(input, &Config::test_default()).unwrap())
    }

    /// Replaces the offset in every generated name with `N`, so a test can read
    /// the shape of the output without pinning it to byte positions.
    fn normalize(output: &str) -> String {
        let mut normalized = String::with_capacity(output.len());
        let mut rest = output;
        while let Some(index) = rest.find("_by_") {
            let (head, tail) = rest.split_at(index);
            normalized.push_str(head);
            let end = tail
                .char_indices()
                .find(|(_, character)| !(character.is_alphanumeric() || *character == '_'))
                .map_or(tail.len(), |(index, _)| index);
            let (name, remainder) = tail.split_at(end);
            // a binder is a dunder (see `destructure_binder_name`); every other
            // generated name ends at its counter
            let (body, suffix) = match name.strip_suffix("__") {
                Some(body) => (body, "__"),
                None => (name, ""),
            };
            let stem = body.trim_end_matches(|character: char| character.is_ascii_digit());
            normalized.push_str(stem);
            if stem.len() != body.len() {
                normalized.push('N');
            }
            normalized.push_str(suffix);
            rest = remainder;
        }
        normalized.push_str(rest);
        normalized
    }

    /// A class with `__match_args__`, so a class pattern has something to take
    /// apart, written on one line so the offsets in the tests below are stable.
    const POINT: &str = "class Point:\n    __match_args__ = ('x', 'y')\n    def __init__(self, x: int, y: int):\n        self.x = x\n        self.y = y\n";

    #[test]
    fn let_lowers_to_a_one_case_match() {
        let out = check(&format!(
            "{POINT}\ndef f(p: Point):\n    let Point(x, y) := p\n    print(x, y)\n"
        ));
        assert!(
            out.contains(indoc! {"
                def f(p: Point):
                    match p:
                        case Point(x, y):
                            pass
                    print(x, y)
            "}),
            "got:\n{out}"
        );
    }

    /// the `else` block runs when the pattern did not match, and keeps its own
    /// source bytes — a `case _:` arm would have to re-indent it
    #[test]
    fn let_else_runs_when_the_pattern_did_not_match() {
        let out = check(indoc! {"
            def f(v: int | str) -> str:
                let int(n) := v else:
                    return 'no'
                return str(n)
        "});
        assert!(
            out.contains(indoc! {"
                def f(v: int | str) -> str:
                    __by_let_N__ = 0
                    match v:
                        case int(n):
                            __by_let_N__ = 1
                    if __by_let_N__ == 0:
                        return 'no'
                    del __by_let_N__
                    return str(n)
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn for_target_destructures_each_element() {
        let out = check(&format!(
            "{POINT}\ndef f(ps: list[Point]):\n    for Point(x, y) in ps:\n        print(x, y)\n"
        ));
        assert!(
            out.contains(indoc! {"
                def f(ps: list[Point]):
                    for __by_destructure_N__ in ps:
                        match __by_destructure_N__:
                            case Point(x, y):
                                pass
                        del __by_destructure_N__
                        print(x, y)
            "}),
            "got:\n{out}"
        );
    }

    /// a one-line body ends up in a block of its own, so the destructure can
    /// come before it
    #[test]
    fn for_target_with_a_one_line_body() {
        let out = check(&format!(
            "{POINT}\ndef f(ps: list[Point]):\n    for Point(x, y) in ps: print(x)\n"
        ));
        assert!(
            out.contains(indoc! {"
                def f(ps: list[Point]):
                    for __by_destructure_N__ in ps:
                        match __by_destructure_N__:
                            case Point(x, y):
                                pass
                        del __by_destructure_N__
                        print(x)
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn with_item_destructures_the_value_it_binds() {
        let out = check(&format!(
            "{POINT}\nfrom typing import ContextManager\ndef f(cm: ContextManager[Point]):\n    with cm as Point(x, y):\n        print(x, y)\n"
        ));
        assert!(
            out.contains(indoc! {"
                def f(cm: ContextManager[Point]):
                    with cm as __by_destructure_N__:
                        match __by_destructure_N__:
                            case Point(x, y):
                                pass
                        del __by_destructure_N__
                        print(x, y)
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn parameter_destructures_its_argument() {
        let out = check(&format!(
            "{POINT}\ndef f(Point(x, y): Point):\n    print(x, y)\n"
        ));
        assert!(
            out.contains(indoc! {"
                def f(__by_destructure_N__: Point):
                    match __by_destructure_N__:
                        case Point(x, y):
                            pass
                    del __by_destructure_N__
                    print(x, y)
            "}),
            "got:\n{out}"
        );
    }

    /// the binder is dropped once the captures are bound: left in a class body it
    /// would be an attribute, and in an `enum class` body a bogus variant
    #[test]
    fn a_binder_does_not_outlive_the_destructure() {
        let out = check(&format!(
            "{POINT}\nclass Holder:\n    for Point(x, y) in ps:\n        print(x, y)\n"
        ));
        assert!(
            out.contains(indoc! {"
                class Holder:
                    for __by_destructure_N__ in ps:
                        match __by_destructure_N__:
                            case Point(x, y):
                                pass
                        del __by_destructure_N__
                        print(x, y)
            "}),
            "got:\n{out}"
        );
    }

    /// a docstring stays the first statement in the body, so the destructure
    /// goes after it
    #[test]
    fn parameter_destructure_follows_a_docstring() {
        let out = check(&format!(
            "{POINT}\ndef f(Point(x, y): Point):\n    'what f does'\n    print(x, y)\n"
        ));
        assert!(
            out.contains(indoc! {"
                def f(__by_destructure_N__: Point):
                    'what f does'
                    match __by_destructure_N__:
            "}),
            "got:\n{out}"
        );
    }

    /// a conjunction is a match per conjunct, nested inside the previous one
    #[test]
    fn and_pattern_matches_every_conjunct() {
        let out = check(indoc! {"
            def f(v: object):
                let int() and 3 := v else:
                    return
                print(v)
        "});
        assert!(
            out.contains(indoc! {"
                def f(v: object):
                    __by_let_N__ = 0
                    __by_subject_N__ = v
                    match __by_subject_N__:
                        case int():
                            match __by_subject_N__:
                                case 3:
                                    __by_let_N__ = 1
            "}),
            "got:\n{out}"
        );
    }

    /// a conjunction nested inside another pattern is hoisted out: the binder
    /// captures its position, and the conjunction is matched against the binder
    #[test]
    fn nested_and_pattern_is_hoisted() {
        let out = check(&format!(
            "{POINT}\ndef f(p: Point):\n    let Point(x=int() and 1, y=y) := p\n    print(y)\n"
        ));
        assert!(
            out.contains(indoc! {"
                def f(p: Point):
                    __by_and_N__ = None
                    match p:
                        case Point(x=__by_and_N__, y=y):
                            match __by_and_N__:
                                case int():
                                    match __by_and_N__:
                                        case 1:
                                            pass
                    del __by_and_N__
                    print(y)
            "}),
            "got:\n{out}"
        );
    }

    /// a hoisted binder is bound by the pattern that captured it, which a failed
    /// match may not have reached — binding it up front is what makes dropping it
    /// safe, and dropping it is what keeps it out of the namespace the statement
    /// sits in
    #[test]
    fn and_pattern_machinery_is_dropped() {
        let out = check(&format!(
            "{POINT}\nclass Holder:\n    p: Point = Point(0, 1)\n    let Point(x, y) and object() := p\n"
        ));
        assert!(
            out.contains(indoc! {"
                class Holder:
                    p: Point = Point(0, 1)
                    __by_subject_N__ = p
            "}),
            "got:\n{out}"
        );
        assert!(
            out.contains("    del __by_subject_N__\n"),
            "the hoisted subject is dropped, got:\n{out}"
        );
    }

    /// `let` is the only form here that can share a line, and its lowering needs
    /// one of its own — the neighbour would either break the output or silently
    /// become part of the `case` body
    #[test]
    fn let_needs_a_line_of_its_own() {
        for source in [
            "def f(p: object, q: bool):\n    if q: let int(n) := p\n",
            "def f(p: object):\n    print('before'); let int(n) := p\n",
            "def f(p: object):\n    let int(n) := p; print('after')\n",
        ] {
            let err = transpile(source, &Config::test_default()).unwrap_err();
            assert!(
                err.contains("has to be the only statement on its line"),
                "expected the shared-line error for `{source}`, got:\n{err}"
            );
        }
    }

    /// two binders in one header each get their own destructure
    #[test]
    fn several_binders_in_one_header() {
        let out = check(&format!(
            "{POINT}\ndef f(Point(a, b): Point, Point(c, d): Point) -> int:\n    return a + b + c + d\n"
        ));
        for binder in [
            "__by_destructure_N__",
            "case Point(a, b)",
            "case Point(c, d)",
        ] {
            assert!(out.contains(binder), "expected `{binder}`, got:\n{out}");
        }
    }

    /// a body with nothing in it cannot read the captures, so there is nothing to
    /// bind — and no header colon to look for either. Only recovery from a broken
    /// parse gets here, which must not be handed a second, unrelated complaint
    #[test]
    fn a_body_less_binder_is_left_alone() {
        let err = transpile(
            "class Box:\n    init(let Point(x, y): Point)\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            !err.contains("could not find the `:`"),
            "the colon scan should stay quiet, got:\n{err}"
        );
    }

    /// a construct whose span covers a `return` that the trailing-lambda lowering
    /// rewrites still lowers: the block's body passes through whole, and the
    /// return is an edit of its own
    #[test]
    fn composes_inside_a_trailing_lambda_block() {
        let out = check(indoc! {"
            def with_resource(once fn: (int) -> None):
                fn(1)

            def f(p: int | str) -> None:
                with_resource:
                    let int(n) := p else:
                        return
                    print(n)
        "});
        assert!(
            !out.contains("let int(n)"),
            "the `let` lowered, got:\n{out}"
        );
        assert!(
            out.contains("        if __by_let_N__ == 0:\n            _trailing_lambda_0_return.append(None); return"),
            "got:\n{out}"
        );
    }

    /// every alternative of a `|` binds the same names, so a conjunction inside
    /// one cannot be hoisted out of it
    #[test]
    fn and_pattern_inside_an_alternative_is_rejected() {
        let err = transpile(
            indoc! {"
                def f(v: object):
                    let int() | (str() and 'x') := v else:
                        return
            "},
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            err.contains("cannot be written inside an alternative of a `|` pattern"),
            "got:\n{err}"
        );
        assert!(err.contains("(line 2)"), "reports the line, got:\n{err}");
    }

    /// a nested match cannot fall through to the next case, so a `match` with a
    /// conjunction flattens onto a selector — every case is tried only when no
    /// earlier one matched
    #[test]
    fn match_statement_with_an_and_pattern_flattens() {
        let out = check(indoc! {"
            def f(v: object):
                match v:
                    case int() and 1:
                        print('one')
                    case _:
                        print('other')
        "});
        assert!(
            out.contains(indoc! {"
                def f(v: object):
                    __by_subject_N__ = v
                    __by_case_N__ = 0
                    if True:
                        if __by_case_N__ == 0:
                            match __by_subject_N__:
                                case int():
                                    match __by_subject_N__:
                                        case 1:
                                            __by_case_N__ = 1
                        if __by_case_N__ == 1:
                            print('one')
            "}),
            "got:\n{out}"
        );
        assert!(
            out.contains("    del __by_case_N__\n    del __by_subject_N__\n"),
            "the machinery is dropped after the statement, got:\n{out}"
        );
    }

    /// a `match` without a conjunction stays exactly as it was written
    #[test]
    fn plain_match_statement_is_untouched() {
        let out = check(indoc! {"
            def f(v: object):
                match v:
                    case 1:
                        print('one')
        "});
        assert!(!out.contains("_by_case"), "got:\n{out}");
    }

    /// a case guard runs once the whole pattern has matched
    #[test]
    fn match_case_guard_moves_to_the_innermost_case() {
        let out = check(indoc! {"
            def f(v: object, flag: bool):
                match v:
                    case int() and 1 if flag:
                        print('one')
        "});
        assert!(out.contains("case 1 if flag:"), "got:\n{out}");
    }

    #[test]
    fn if_let_composes_with_an_and_pattern() {
        let out = check(indoc! {"
            def f(v: object):
                if let int() and 1 := v:
                    print(v)
        "});
        assert!(
            out.contains(indoc! {"
                def f(v: object):
                    __by_if_let_N__ = 0
                    __by_subject_N__ = v
                    match __by_subject_N__:
                        case int():
                            match __by_subject_N__:
                                case 1:
                                    __by_if_let_N__ = 1
            "}),
            "got:\n{out}"
        );
    }

    /// bodies are never re-rendered, so a construct inside one lowers as usual
    #[test]
    fn body_lowerings_compose() {
        let out = check(&format!(
            "{POINT}\ndef f(ps: list[Point], other: int | None):\n    for Point(x, y) in ps:\n        z = other ?? 0\n"
        ));
        assert!(!out.contains("??"), "got:\n{out}");
        assert!(out.contains("        z = "), "got:\n{out}");
    }

    /// the `match` a destructuring lowers to is itself lowered for a target
    /// that predates it, so the construct reaches every version the polyfill
    /// covers rather than being an error below 3.10
    #[test]
    fn lowers_below_python_310() {
        let config = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        let out = transpile("let (a, b) := (1, 2)\n", &config).unwrap();
        assert!(!out.contains("match "), "got:\n{out}");
        assert!(out.contains("a := "), "binds the captures, got:\n{out}");
        assert!(out.contains("b := "), "binds the captures, got:\n{out}");
    }
}
