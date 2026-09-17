//! the edits that carry an AST pass's rewrite of a statement into the output
//!
//! an AST pass rewrites the syntax tree, and every other pass lowers what it
//! lowers as an edit keyed on a range of the source. printing a rewritten
//! statement from its tree would drop each of those edits inside it — a
//! conversion, an extension call, a quoted forward reference — and nothing
//! would say so wherever the construct they lowered is valid python as written.
//!
//! so no statement is printed whole. a node that kept its source range is one
//! the passes did not synthesize, and it is emitted as its source, with the
//! edits inside it applied. a node whose own text a pass changed — `typeof x`
//! become `TypeOf[x]`, a `def` with a `sentinel` in its body become an
//! assignment — is re-emitted at its range as a template that prints what the
//! pass made of it and passes each node below it that kept a range through as
//! `Src`. a node a pass made has no range, and is printed where it stands.
//!
//! whether a node's own text changed is decided by printing it twice, from the
//! rewritten tree and from the parse, with every node below it that kept a range
//! printed as a placeholder naming that range. the two agree exactly when the
//! passes left the node itself alone.
//!
//! between two nodes it passes through, a template prints tokens of its own, and
//! keeps the source's text wherever those tokens are the source's — see
//! [`Rerendering::segment`].
//!
//! a name the node spells — a parameter's, a function's, an attribute's — is
//! compared as the source spelled it, so a node a pass only renamed something in
//! is not re-emitted: the name is, at its own range. numbering a repeated `_`
//! changes nothing else about a `def`, and every edit elsewhere in its signature
//! lands in the source around the new name.
//!
//! a compound statement whose clauses a pass left alone, and changed only the
//! statements of, is not re-emitted either: each suite a pass changed is, over the
//! source its statements span. a header is where the parser stands basedpython up
//! as syntax python has no spelling for — a modifier as a decorator, `init(...)`
//! as a `def __init__` carrying a `self` and a `-> None` the source never wrote, a
//! `raises` clause — and the passes that lower it do so with edits in the header's
//! source, which printing the header again would drop. a suite written on the line
//! of its clause is re-emitted the same way, as a block below the clause: it has no
//! line of its own to keep, and what it prints need not fit on one.
//!
//! what a template prints is indented as the source around it is: by the line the
//! node starts on, and each block it opens by the step the node's own source
//! indents its blocks by, which a file need not use throughout.

use std::collections::HashMap;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::visitor::transformer::{self, Transformer};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    Alias, AnyNodeRef, AtomicNodeIndex, ExceptHandler, Expr, ExprContext, ExprName, Identifier,
    Keyword, NodeKind, Parameter, Pattern, PatternKeyword, Stmt, StmtExpr, Suite, TypeParam,
};
use ruff_python_codegen::{Generator, Indentation, Mode};
use ruff_python_trivia::{
    BackwardsTokenizer, CommentRanges, SimpleToken, SimpleTokenKind, SimpleTokenizer,
};
use ruff_source_file::LineEnding;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use super::ast_driver::Fragment;

/// the re-emission of one statement an AST pass rewrote
pub(crate) struct Rerendering<'a> {
    source: &'a str,
    comments: &'a CommentRanges,
    /// where the edits the other passes made stand
    edited: &'a [TextRange],
    indentation: &'a Indentation,
    /// what every placeholder starts with, which the source spells nowhere
    label: String,
}

/// a node below the one being printed that kept its range
enum Hole<'m> {
    Expr(&'m Expr),
    Stmt(&'m Stmt),
}

impl Hole<'_> {
    fn range(&self) -> TextRange {
        match self {
            Hole::Expr(expr) => expr.range(),
            Hole::Stmt(stmt) => stmt.range(),
        }
    }
}

/// whether a node at `range` of `kind` is one the passes kept from the parse
///
/// a node a pass synthesized carries no range. one a pass built by parsing text of its
/// own should have forgotten the ranges into that text (see [`forget_stmt_ranges`]),
/// and one that did not is still not passed through as source it never came from
fn is_kept(parsed: &Parsed<'_>, range: TextRange, kind: NodeKind) -> bool {
    range != TextRange::default() && parsed.at(range, kind).is_some()
}

fn expr_kind(expr: &Expr) -> NodeKind {
    AnyNodeRef::from(expr).kind()
}

fn stmt_kind(stmt: &Stmt) -> NodeKind {
    AnyNodeRef::from(stmt).kind()
}

impl<'a> Rerendering<'a> {
    pub(crate) fn new(
        source: &'a str,
        comments: &'a CommentRanges,
        edited: &'a [TextRange],
        indentation: &'a Indentation,
    ) -> Self {
        let mut label = "__by_src_".to_string();
        while source.contains(&label) {
            label.push('_');
        }
        Self {
            source,
            comments,
            edited,
            indentation,
            label,
        }
    }

    /// the edits that turn `original`, the statement as parsed at `range`, into
    /// `rewritten`, what the passes made of it
    pub(crate) fn edits(
        &self,
        original: &Stmt,
        rewritten: &Stmt,
        range: TextRange,
    ) -> Vec<(TextRange, Vec<Fragment>)> {
        let mut parsed = Parsed::default();
        parsed.visit_stmt(original);
        let mut edits = Vec::new();
        self.node(&parsed, &Hole::Stmt(rewritten), range, &mut edits);
        edits
    }

    /// the edits for `node`, standing at `range` in the source, and for every node
    /// below it that kept a range
    fn node(
        &self,
        parsed: &Parsed<'_>,
        node: &Hole<'_>,
        range: TextRange,
        edits: &mut Vec<(TextRange, Vec<Fragment>)>,
    ) {
        let Below { holes, names } = below(parsed, node);
        let original = parsed.at(range, kind_of(node));
        let unchanged = original.as_ref().is_some_and(|original| {
            self.print(
                parsed,
                Printed::Node(original),
                Spelling::Parsed,
                self.indentation,
            ) == self.print(
                parsed,
                Printed::Node(node),
                Spelling::Parsed,
                self.indentation,
            )
        });
        if unchanged {
            rename(parsed, &names, &[], edits);
        } else if let (Some(Hole::Stmt(original)), Hole::Stmt(rewritten)) = (&original, node)
            && let Some(suites) = self.changed_suites(parsed, original, rewritten)
        {
            let regions: Vec<TextRange> = suites.iter().map(|suite| suite.region).collect();
            rename(parsed, &names, &regions, edits);
            for suite in suites {
                if edits.iter().any(|(at, _)| *at == suite.region) {
                    continue;
                }
                let indentation = self.indentation_between(range, suite.region);
                let printed = Printed::Suite(&suite.statements);
                let text = self.print(parsed, printed, Spelling::Rewritten, &indentation);
                let holes = below_statements(parsed, &suite.statements).holes;
                edits.push((
                    suite.region,
                    self.fragments(parsed, printed, suite.region, &text, &holes, &indentation),
                ));
            }
        } else if !edits.iter().any(|(at, _)| *at == range) {
            let indentation = match &original {
                Some(Hole::Stmt(original)) => self.indentation_of(original),
                _ => self.indentation.clone(),
            };
            let printed = Printed::Node(node);
            let text = self.print(parsed, printed, Spelling::Rewritten, &indentation);
            edits.push((
                range,
                self.fragments(parsed, printed, range, &text, &holes, &indentation),
            ));
        }
        for hole in &holes {
            self.node(parsed, hole, hole.range(), edits);
        }
    }

    /// the suites a pass changed the statements of, when it changed nothing else about
    /// `rewritten`, the statement `original` was parsed as
    ///
    /// `None` when its clauses differ, or when a suite that changed cannot be re-emitted
    /// over source of its own: one the source wrote no statement in, or one the passes
    /// emptied
    fn changed_suites<'m>(
        &self,
        parsed: &Parsed<'_>,
        original: &Stmt,
        rewritten: &'m Stmt,
    ) -> Option<Vec<ChangedSuite<'m>>> {
        let original_suites = suites(original);
        let rewritten_suites = suites(rewritten);
        if original_suites.is_empty() || original_suites.len() != rewritten_suites.len() {
            return None;
        }
        let original_clauses = self.without_suites(original);
        let rewritten_clauses = self.without_suites(rewritten);
        let clauses = |stmt: &Stmt| {
            self.print(
                parsed,
                Printed::Node(&Hole::Stmt(stmt)),
                Spelling::Parsed,
                self.indentation,
            )
        };
        if clauses(&original_clauses) != clauses(&rewritten_clauses) {
            return None;
        }
        // the parser builds some statements of a suite out of its clause — the field an
        // `init(...)` parameter declares — and gives them the range of what they were
        // built from, or none. the source does not write them in the suite
        let mut clause_ranges = ClauseRanges::default();
        clause_ranges.visit_stmt(&original_clauses);
        let written = |stmt: &Stmt| {
            let range = stmt.range();
            !range.is_empty()
                && !clause_ranges
                    .ranges
                    .iter()
                    .any(|clause| clause.contains_range(range))
        };
        let mut changed = Vec::new();
        for (original_suite, rewritten_suite) in original_suites.into_iter().zip(rewritten_suites) {
            let original_statements: Vec<&Stmt> =
                original_suite.iter().filter(|stmt| written(stmt)).collect();
            let statements: Vec<&'m Stmt> = rewritten_suite
                .iter()
                .filter(|stmt| !is_kept(parsed, stmt.range(), stmt_kind(stmt)) || written(stmt))
                .collect();
            let same = original_statements.len() == statements.len()
                && original_statements
                    .iter()
                    .zip(&statements)
                    .all(|(original, stmt)| {
                        stmt.range() == original.range()
                            && is_kept(parsed, stmt.range(), stmt_kind(stmt))
                    });
            if same {
                continue;
            }
            let (Some(first), Some(last)) =
                (original_statements.first(), original_statements.last())
            else {
                return None;
            };
            if statements.is_empty() {
                return None;
            }
            let region = TextRange::new(first.start(), last.end());
            changed.push(ChangedSuite { region, statements });
        }
        Some(changed)
    }

    /// `stmt` with each of its suites a placeholder, so it prints its clauses alone
    fn without_suites(&self, stmt: &Stmt) -> Stmt {
        let mut copy = stmt.clone();
        for suite in suites_mut(&mut copy) {
            *suite = Suite::from([Stmt::Expr(StmtExpr {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                value: Box::new(Expr::Name(ExprName {
                    node_index: AtomicNodeIndex::NONE,
                    range: TextRange::default(),
                    id: Name::from(format!("{}suite", self.label)),
                    ctx: ExprContext::Load,
                })),
            })]);
        }
        copy
    }

    /// the step `stmt`'s source indents its blocks by, read off the first statement of
    /// its first suite that starts a line
    fn indentation_of(&self, stmt: &Stmt) -> Indentation {
        suites(stmt)
            .into_iter()
            .flatten()
            .find(|body| !body.range().is_empty() && self.line_prefix(body.range()).is_some())
            .map_or_else(
                || self.indentation.clone(),
                |body| self.indentation_between(stmt.range(), body.range()),
            )
    }

    /// the step from the indentation of the line `outer` starts on to that of `inner`,
    /// or the file's when that is not a step deeper
    fn indentation_between(&self, outer: TextRange, inner: TextRange) -> Indentation {
        let outer_line = &self.source[self.line_start(outer.start())..];
        let outer_prefix = &outer_line[..outer_line
            .find(|c: char| c != ' ' && c != '\t')
            .unwrap_or(outer_line.len())];
        match self.line_prefix(inner) {
            Some(inner) if inner.len() > outer_prefix.len() && inner.starts_with(outer_prefix) => {
                Indentation::new(inner[outer_prefix.len()..].to_string())
            }
            _ => self.indentation.clone(),
        }
    }

    /// the whitespace the line holding `offset` starts with
    fn line_indentation(&self, offset: TextSize) -> &str {
        let line = &self.source[self.line_start(offset)..];
        &line[..line
            .find(|c: char| c != ' ' && c != '\t')
            .unwrap_or(line.len())]
    }

    /// where the line holding `offset` starts
    fn line_start(&self, offset: TextSize) -> usize {
        self.source[..usize::from(offset)]
            .rfind('\n')
            .map_or(0, |at| at + 1)
    }

    /// `printed` with each node below it that kept a range as a placeholder
    fn print(
        &self,
        parsed: &Parsed<'_>,
        printed: Printed<'_, '_>,
        spelling: Spelling,
        indentation: &Indentation,
    ) -> String {
        let placeholders = Placeholders {
            label: &self.label,
            parsed,
            spelling,
        };
        let statement = |stmt: &Stmt| {
            let mut copy = stmt.clone();
            placeholders.stmt_names(&mut copy);
            transformer::walk_stmt(&placeholders, &mut copy);
            generator(indentation)
                .stmt(&copy)
                .trim_end_matches('\n')
                .to_string()
        };
        match printed {
            Printed::Node(Hole::Expr(expr)) => {
                let mut copy = (*expr).clone();
                placeholders.expr_names(&mut copy);
                transformer::walk_expr(&placeholders, &mut copy);
                generator(indentation).expr(&copy)
            }
            Printed::Node(Hole::Stmt(stmt)) => statement(stmt),
            // each statement is printed where a placeholder for the suite would be, so
            // a statement a pass kept is still a placeholder rather than printed
            Printed::Suite(statements) => statements
                .iter()
                .map(|stmt| {
                    if is_kept(parsed, stmt.range(), stmt_kind(stmt)) {
                        placeholder(&self.label, stmt.range())
                    } else {
                        statement(stmt)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// the template `printed` stands for: its text, with each placeholder passing its
    /// node's source through
    fn fragments(
        &self,
        parsed: &Parsed<'_>,
        node: Printed<'_, '_>,
        range: TextRange,
        printed: &str,
        holes: &[Hole<'_>],
        indentation: &Indentation,
    ) -> Vec<Fragment> {
        let by_label: HashMap<String, &Hole<'_>> = holes
            .iter()
            .map(|hole| (placeholder(&self.label, hole.range()), hole))
            .collect();
        // a statement below the first line of the source is indented by the line it
        // starts on, which the generator, printing it on its own, knows nothing of
        let indent = match node {
            Printed::Node(Hole::Stmt(_)) | Printed::Suite(_) => self.line_prefix(range),
            Printed::Node(Hole::Expr(_)) => None,
        };
        // a suite written on the line of its clause — `def f(): x` — has no line of its
        // own to take an indentation from, and what it prints does not fit on one: it is
        // written as a block below the clause instead, a step deeper than the line the
        // clause starts on, which leaves the clause and its lowerings the source they are
        // written in
        let opens_a_block = indent.is_none() && matches!(node, Printed::Suite(_));
        let indent = match (indent, opens_a_block) {
            (indent, false) => indent,
            (_, true) => Some(format!(
                "{}{}",
                self.line_indentation(range.start()),
                indentation.as_str()
            )),
        };
        let mut indented = String::new();
        if opens_a_block {
            indented.push('\n');
            indented.push_str(indent.as_deref().unwrap_or_default());
        }
        push_indented(&mut indented, printed, indent.as_deref());
        let mut fragments: Vec<Fragment> = Vec::new();
        let mut text = String::new();
        // where the source the text printed so far stands in for begins
        let mut after = range.start();
        let mut rest = indented.as_str();
        while let Some(at) = rest.find(&self.label) {
            let tail = &rest[at..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            let Some(hole) = by_label.get(&tail[..end]) else {
                text.push_str(&rest[..at + end]);
                rest = &rest[at + end..];
                continue;
            };
            text.push_str(&rest[..at]);
            let parenthesize = match hole {
                Hole::Expr(expr) => self.needs_parentheses(parsed, node, expr, indentation),
                Hole::Stmt(_) => false,
            };
            if parenthesize {
                text.push('(');
            }
            let mut segment = self.segment(std::mem::take(&mut text), after, hole.range().start());
            // the statement's own lines keep the source's indentation, so its first line
            // does as well
            if segment.tail.is_none()
                && let Hole::Stmt(stmt) = hole
                && let Some(prefix) = self.line_prefix(stmt.range())
            {
                let trimmed = segment.text.trim_end_matches([' ', '\t']).len();
                if segment.text[..trimmed].ends_with('\n') {
                    segment.text.truncate(trimmed);
                    segment.text.push_str(&prefix);
                }
            }
            segment.push(&mut fragments);
            push_source(&mut fragments, hole.range());
            if parenthesize {
                text.push(')');
            }
            after = hole.range().end();
            rest = &rest[at + end..];
        }
        text.push_str(rest);
        self.segment(text, after, range.end()).push(&mut fragments);
        fragments
    }

    /// how `text`, printed between two nodes a template passes through, stands in for
    /// the source between them, from `start` to `end`
    ///
    /// where the printed tokens begin as the source's do, the source is kept up to the
    /// last of them, and likewise where they end as the source's do. that keeps what
    /// the printer has no spelling for — the comments, the blank lines, a signature
    /// laid out over several lines — and an edit another pass made there, such as the
    /// guard a `raises` clause inserts ahead of a decorated `def`. what is left between
    /// is printed, with the comments at either end of the source it stands for
    fn segment(&self, text: String, start: TextSize, end: TextSize) -> Segment {
        if start > end {
            // the passes reordered the nodes around it, and it stands for no source
            return Segment {
                head: None,
                text,
                tail: None,
            };
        }
        let gap = TextRange::new(start, end);
        let printed = TextRange::up_to(text.text_len());
        let same = |ours: &SimpleToken, theirs: &SimpleToken| {
            ours.kind() == theirs.kind() && text[ours.range()] == self.source[theirs.range()]
        };
        let (front, whole) = forward_tokens(&text, printed);
        let (source_front, source_whole) = forward_tokens(self.source, gap);
        let mut head = front
            .iter()
            .zip(&source_front)
            .take_while(|(ours, theirs)| same(ours, theirs))
            .count();
        if whole && source_whole && head == front.len() && head == source_front.len() {
            return Segment {
                head: Some(gap),
                text: String::new(),
                tail: None,
            };
        }
        // an edit the source is split through, or an insertion where it is split, could
        // belong to either side: to the source, or to what the printer spelled in its
        // place. it is left whole on the printed side, where it is reported if it is lost
        while head > 0 && !self.splits_cleanly(source_front[head - 1].end()) {
            head -= 1;
        }
        let head_end = front[..head].last().map_or(printed.start(), Ranged::end);
        let source_head_end = source_front[..head].last().map_or(start, Ranged::end);
        let back = backward_tokens(&text, printed, &[]);
        let source_back = backward_tokens(self.source, gap, self.comments);
        let mut tail = back
            .iter()
            .zip(&source_back)
            .take_while(|(ours, theirs)| {
                ours.start() >= head_end && theirs.start() >= source_head_end && same(ours, theirs)
            })
            .count();
        while tail > 0 && !self.splits_cleanly(source_back[tail - 1].start()) {
            tail -= 1;
        }
        let tail_start = back[..tail].last().map_or(printed.end(), Ranged::start);
        let source_tail_start = source_back[..tail].last().map_or(end, Ranged::start);
        Segment {
            head: (head > 0).then(|| TextRange::new(start, source_head_end)),
            text: self.with_comments(
                &text[TextRange::new(head_end, tail_start)],
                TextRange::new(source_head_end, source_tail_start),
            ),
            tail: (tail > 0).then(|| TextRange::new(source_tail_start, end)),
        }
    }

    /// whether no edit stands across `offset`, or is inserted at it
    fn splits_cleanly(&self, offset: TextSize) -> bool {
        !self.edited.iter().any(|edit| {
            if edit.is_empty() {
                edit.start() == offset
            } else {
                edit.start() < offset && offset < edit.end()
            }
        })
    }

    /// `text`, printed where `gap` stands in the source, with the comments and the blank
    /// lines of `gap` that lie ahead of its first token and after its last
    ///
    /// a comment ahead of the first token ends a line of the source or sits on one of
    /// its own, and is kept where the printed text breaks the line there too; likewise
    /// after the last token. a comment between two tokens the printer spelled
    /// differently has no place in the printed line it would have to sit in, and is not
    /// kept
    fn with_comments(&self, text: &str, gap: TextRange) -> String {
        let core_start = text.len() - text.trim_start().len();
        let core_end = text.trim_end().len();
        if core_start >= core_end {
            return text.to_owned();
        }
        let source = &self.source[gap];
        let (lead, trail) = match self.token_bounds(gap) {
            Some((first, last)) => (
                &source[..usize::from(first - gap.start())],
                &source[usize::from(last - gap.start())..],
            ),
            // no token between two statements: what ends the line of the first belongs to
            // it, and the lines after that to the second
            None => match source.find('\n') {
                Some(newline) => (&source[..=newline], &source[newline..]),
                None => return text.to_owned(),
            },
        };
        let mut out = respaced(&text[..core_start], lead);
        out.push_str(&text[core_start..core_end]);
        out.push_str(&respaced(&text[core_end..], trail));
        out
    }

    /// where the first token in `gap` begins and the last one ends, looking past
    /// whitespace and comments. a line continuation counts as a token, so what is
    /// looked past is only ever something a line break may be put around
    fn token_bounds(&self, gap: TextRange) -> Option<(TextSize, TextSize)> {
        let comments = self.comments.comments_in_range(gap);
        let is_blank = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c');
        let mut first = gap.start();
        loop {
            if first >= gap.end() {
                return None;
            }
            if let Some(comment) = comments.iter().find(|comment| comment.start() == first) {
                first = comment.end();
                continue;
            }
            let c = self.source[usize::from(first)..].chars().next()?;
            if !is_blank(c) {
                break;
            }
            first += c.text_len();
        }
        let mut last = gap.end();
        loop {
            if let Some(comment) = comments.iter().find(|comment| comment.end() == last) {
                last = comment.start();
                continue;
            }
            let c = self.source[..usize::from(last)].chars().next_back()?;
            if !is_blank(c) {
                break;
            }
            last -= c.text_len();
        }
        Some((first, last))
    }

    /// the whitespace ahead of `range` on its line, where nothing else is
    fn line_prefix(&self, range: TextRange) -> Option<String> {
        let start = usize::from(range.start());
        let line_start = self.source[..start].rfind('\n').map_or(0, |at| at + 1);
        let prefix = &self.source[line_start..start];
        prefix
            .chars()
            .all(|c| c == ' ' || c == '\t')
            .then(|| prefix.to_string())
    }

    /// whether `hole`'s source has to be parenthesized where `node` prints it
    ///
    /// a placeholder is a name, which binds tighter than anything, so the printed
    /// text says nothing about the expression it stands for. that is asked by
    /// printing `node` again with the expression itself in the placeholder's place,
    /// its own operands as placeholders, and reading whether the generator put
    /// parentheses around it
    fn needs_parentheses(
        &self,
        parsed: &Parsed<'_>,
        node: Printed<'_, '_>,
        hole: &Expr,
        indentation: &Indentation,
    ) -> bool {
        if binds_tightest(hole) {
            return false;
        }
        let placeholders = Placeholders {
            label: &self.label,
            parsed,
            spelling: Spelling::Rewritten,
        };
        let mut operand = hole.clone();
        transformer::walk_expr(&placeholders, &mut operand);
        let alone = generator(indentation).expr(&operand);
        let probe = Probe {
            label: &self.label,
            parsed,
            range: hole.range(),
            operand: &operand,
        };
        let statement = |stmt: &Stmt| {
            let mut copy = stmt.clone();
            transformer::walk_stmt(&probe, &mut copy);
            generator(indentation).stmt(&copy)
        };
        let printed = match node {
            Printed::Node(Hole::Expr(expr)) => {
                let mut copy = (*expr).clone();
                transformer::walk_expr(&probe, &mut copy);
                generator(indentation).expr(&copy)
            }
            Printed::Node(Hole::Stmt(stmt)) => statement(stmt),
            Printed::Suite(statements) => statements
                .iter()
                .map(|stmt| statement(stmt))
                .collect::<String>(),
        };
        match printed.find(&alone) {
            Some(at) => {
                printed[..at].ends_with('(') && printed[at + alone.len()..].starts_with(')')
            }
            // printed differently in place than alone, so nothing can be read off it
            None => true,
        }
    }
}

fn generator(indentation: &Indentation) -> Generator<'_> {
    Generator::new(indentation, LineEnding::Lf).with_mode(Mode::BasedPython)
}

/// what a template prints: a node, or the statements of a suite
#[derive(Clone, Copy)]
enum Printed<'p, 'm> {
    Node(&'p Hole<'m>),
    Suite(&'p [&'m Stmt]),
}

/// a suite whose statements a pass changed, to be re-emitted over `region`, the source
/// from the first statement the source wrote in it to the last
struct ChangedSuite<'m> {
    region: TextRange,
    statements: Vec<&'m Stmt>,
}

/// the edits that re-emit each of `names` a pass renamed, at its own range, other than
/// those inside `regions`, which a template re-emits
fn rename(
    parsed: &Parsed<'_>,
    names: &[&Identifier],
    regions: &[TextRange],
    edits: &mut Vec<(TextRange, Vec<Fragment>)>,
) {
    for name in names {
        if regions
            .iter()
            .any(|region| region.contains_range(name.range))
        {
            continue;
        }
        if parsed
            .identifier(name.range)
            .is_some_and(|original| original.id != name.id)
        {
            edits.push((name.range, vec![Fragment::Lit(name.id.to_string())]));
        }
    }
}

/// the suites `stmt` holds, in source order
fn suites(stmt: &Stmt) -> Vec<&Suite> {
    match stmt {
        Stmt::FunctionDef(node) => vec![&node.body],
        Stmt::ClassDef(node) => vec![&node.body],
        Stmt::For(node) => vec![&node.body, &node.orelse],
        Stmt::While(node) => vec![&node.body, &node.orelse],
        Stmt::If(node) => std::iter::once(&node.body)
            .chain(node.elif_else_clauses.iter().map(|clause| &clause.body))
            .collect(),
        Stmt::Let(node) => vec![&node.orelse],
        Stmt::With(node) => vec![&node.body],
        Stmt::Match(node) => node.cases.iter().map(|case| &case.body).collect(),
        Stmt::Try(node) => std::iter::once(&node.body)
            .chain(
                node.handlers
                    .iter()
                    .map(|ExceptHandler::ExceptHandler(handler)| &handler.body),
            )
            .chain([&node.orelse, &node.finalbody])
            .collect(),
        Stmt::Return(_)
        | Stmt::Delete(_)
        | Stmt::TypeAlias(_)
        | Stmt::Assign(_)
        | Stmt::AugAssign(_)
        | Stmt::AnnAssign(_)
        | Stmt::Raise(_)
        | Stmt::Assert(_)
        | Stmt::Import(_)
        | Stmt::ImportFrom(_)
        | Stmt::Global(_)
        | Stmt::Nonlocal(_)
        | Stmt::Expr(_)
        | Stmt::Pass(_)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::IpyEscapeCommand(_) => Vec::new(),
    }
}

/// as [`suites`], to be replaced
fn suites_mut(stmt: &mut Stmt) -> Vec<&mut Suite> {
    match stmt {
        Stmt::FunctionDef(node) => vec![&mut node.body],
        Stmt::ClassDef(node) => vec![&mut node.body],
        Stmt::For(node) => vec![&mut node.body, &mut node.orelse],
        Stmt::While(node) => vec![&mut node.body, &mut node.orelse],
        Stmt::If(node) => std::iter::once(&mut node.body)
            .chain(
                node.elif_else_clauses
                    .iter_mut()
                    .map(|clause| &mut clause.body),
            )
            .collect(),
        Stmt::Let(node) => vec![&mut node.orelse],
        Stmt::With(node) => vec![&mut node.body],
        Stmt::Match(node) => node.cases.iter_mut().map(|case| &mut case.body).collect(),
        Stmt::Try(node) => std::iter::once(&mut node.body)
            .chain(
                node.handlers
                    .iter_mut()
                    .map(|ExceptHandler::ExceptHandler(handler)| &mut handler.body),
            )
            .chain([&mut node.orelse, &mut node.finalbody])
            .collect(),
        Stmt::Return(_)
        | Stmt::Delete(_)
        | Stmt::TypeAlias(_)
        | Stmt::Assign(_)
        | Stmt::AugAssign(_)
        | Stmt::AnnAssign(_)
        | Stmt::Raise(_)
        | Stmt::Assert(_)
        | Stmt::Import(_)
        | Stmt::ImportFrom(_)
        | Stmt::Global(_)
        | Stmt::Nonlocal(_)
        | Stmt::Expr(_)
        | Stmt::Pass(_)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::IpyEscapeCommand(_) => Vec::new(),
    }
}

/// the range of every node a statement's clauses hold, below the statement itself
#[derive(Default)]
struct ClauseRanges {
    ranges: Vec<TextRange>,
    entered: bool,
}

impl<'a> SourceOrderVisitor<'a> for ClauseRanges {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if std::mem::replace(&mut self.entered, true) && !node.range().is_empty() {
            self.ranges.push(node.range());
        }
        TraversalSignal::Traverse
    }
}

/// `stmt` as a node a pass made, rather than one read from this file's source
///
/// a pass that builds a node by parsing text of its own is handed ranges into that
/// text, which name nothing here — and a range is what says a node came from the
/// source, see the module docs
pub(crate) fn forget_stmt_ranges(stmt: &mut Stmt) {
    Forget.visit_stmt(stmt);
    ForgetNames.visit_stmt(stmt);
}

/// as [`forget_stmt_ranges`], for an expression
pub(crate) fn forget_expr_ranges(expr: &mut Expr) {
    Forget.visit_expr(expr);
    ForgetNames.visit_expr(expr);
}

/// forgets the range of every name, which [`Forget`]'s expression relocation leaves
struct ForgetNames;

impl NameTransformer for ForgetNames {
    fn name(&self, name: &mut Identifier) {
        name.range = TextRange::default();
    }
}

impl Transformer for ForgetNames {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        self.stmt_names(stmt);
        transformer::walk_stmt(self, stmt);
    }

    fn visit_expr(&self, expr: &mut Expr) {
        self.expr_names(expr);
        transformer::walk_expr(self, expr);
    }

    fn visit_parameter(&self, parameter: &mut Parameter) {
        self.name(&mut parameter.name);
        transformer::walk_parameter(self, parameter);
    }

    fn visit_keyword(&self, keyword: &mut Keyword) {
        self.keyword_names(keyword);
        transformer::walk_keyword(self, keyword);
    }

    fn visit_alias(&self, alias: &mut Alias) {
        self.alias_names(alias);
    }

    fn visit_type_param(&self, type_param: &mut TypeParam) {
        self.type_param_names(type_param);
        transformer::walk_type_param(self, type_param);
    }

    fn visit_except_handler(&self, except_handler: &mut ExceptHandler) {
        self.except_handler_names(except_handler);
        transformer::walk_except_handler(self, except_handler);
    }

    fn visit_pattern(&self, pattern: &mut Pattern) {
        self.pattern_names(pattern);
        transformer::walk_pattern(self, pattern);
    }

    fn visit_pattern_keyword(&self, pattern_keyword: &mut PatternKeyword) {
        self.name(&mut pattern_keyword.attr);
        transformer::walk_pattern_keyword(self, pattern_keyword);
    }
}

/// what is done to each name a node spells itself, as opposed to the names spelled by
/// the nodes below it
trait NameTransformer {
    fn name(&self, name: &mut Identifier);

    fn stmt_names(&self, stmt: &mut Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => self.name(&mut function.name),
            Stmt::ClassDef(class) => self.name(&mut class.name),
            Stmt::ImportFrom(import) => {
                if let Some(module) = &mut import.module {
                    self.name(module);
                }
            }
            Stmt::Global(global) => global.names.iter_mut().for_each(|name| self.name(name)),
            Stmt::Nonlocal(nonlocal) => nonlocal.names.iter_mut().for_each(|name| self.name(name)),
            _ => {}
        }
    }

    fn expr_names(&self, expr: &mut Expr) {
        match expr {
            Expr::Attribute(attribute) => self.name(&mut attribute.attr),
            Expr::ProtocolMethod(method) => self.name(&mut method.name),
            _ => {}
        }
    }

    fn keyword_names(&self, keyword: &mut Keyword) {
        if let Some(arg) = &mut keyword.arg {
            self.name(arg);
        }
    }

    fn alias_names(&self, alias: &mut Alias) {
        self.name(&mut alias.name);
        if let Some(asname) = &mut alias.asname {
            self.name(asname);
        }
    }

    fn type_param_names(&self, type_param: &mut TypeParam) {
        match type_param {
            TypeParam::TypeVar(type_var) => self.name(&mut type_var.name),
            TypeParam::TypeVarTuple(type_var_tuple) => self.name(&mut type_var_tuple.name),
            TypeParam::ParamSpec(param_spec) => self.name(&mut param_spec.name),
        }
    }

    fn except_handler_names(&self, except_handler: &mut ExceptHandler) {
        let ExceptHandler::ExceptHandler(handler) = except_handler;
        if let Some(name) = &mut handler.name {
            self.name(name);
        }
    }

    fn pattern_names(&self, pattern: &mut Pattern) {
        let name = match pattern {
            Pattern::MatchAs(pattern) => pattern.name.as_mut(),
            Pattern::MatchStar(pattern) => pattern.name.as_mut(),
            Pattern::MatchMapping(pattern) => pattern.rest.as_mut(),
            _ => None,
        };
        if let Some(name) = name {
            self.name(name);
        }
    }
}

/// the names a node spells itself, as [`NameTransformer`] reaches them
fn own_names(node: AnyNodeRef<'_>) -> Vec<&Identifier> {
    match node {
        AnyNodeRef::StmtFunctionDef(function) => vec![&function.name],
        AnyNodeRef::StmtClassDef(class) => vec![&class.name],
        AnyNodeRef::StmtImportFrom(import) => import.module.iter().collect(),
        AnyNodeRef::StmtGlobal(global) => global.names.iter().collect(),
        AnyNodeRef::StmtNonlocal(nonlocal) => nonlocal.names.iter().collect(),
        AnyNodeRef::ExprAttribute(attribute) => vec![&attribute.attr],
        AnyNodeRef::ExprProtocolMethod(method) => vec![&method.name],
        AnyNodeRef::Parameter(parameter) => vec![&parameter.name],
        AnyNodeRef::Keyword(keyword) => keyword.arg.iter().collect(),
        AnyNodeRef::Alias(alias) => std::iter::once(&alias.name).chain(&alias.asname).collect(),
        AnyNodeRef::TypeParamTypeVar(type_var) => vec![&type_var.name],
        AnyNodeRef::TypeParamTypeVarTuple(type_var_tuple) => vec![&type_var_tuple.name],
        AnyNodeRef::TypeParamParamSpec(param_spec) => vec![&param_spec.name],
        AnyNodeRef::ExceptHandlerExceptHandler(handler) => handler.name.iter().collect(),
        AnyNodeRef::PatternMatchAs(pattern) => pattern.name.iter().collect(),
        AnyNodeRef::PatternMatchStar(pattern) => pattern.name.iter().collect(),
        AnyNodeRef::PatternMatchMapping(pattern) => pattern.rest.iter().collect(),
        AnyNodeRef::PatternKeyword(keyword) => vec![&keyword.attr],
        _ => Vec::new(),
    }
}

struct Forget;

impl Transformer for Forget {
    fn visit_expr(&self, expr: &mut Expr) {
        ruff_python_ast::relocate::relocate_expr(expr, TextRange::default());
    }

    fn visit_stmt(&self, stmt: &mut Stmt) {
        let range = match stmt {
            Stmt::FunctionDef(node) => &mut node.range,
            Stmt::ClassDef(node) => &mut node.range,
            Stmt::Return(node) => &mut node.range,
            Stmt::Delete(node) => &mut node.range,
            Stmt::TypeAlias(node) => &mut node.range,
            Stmt::Assign(node) => &mut node.range,
            Stmt::AugAssign(node) => &mut node.range,
            Stmt::AnnAssign(node) => &mut node.range,
            Stmt::For(node) => &mut node.range,
            Stmt::While(node) => &mut node.range,
            Stmt::If(node) => &mut node.range,
            Stmt::Let(node) => &mut node.range,
            Stmt::With(node) => &mut node.range,
            Stmt::Match(node) => &mut node.range,
            Stmt::Raise(node) => &mut node.range,
            Stmt::Try(node) => &mut node.range,
            Stmt::Assert(node) => &mut node.range,
            Stmt::Import(node) => &mut node.range,
            Stmt::ImportFrom(node) => &mut node.range,
            Stmt::Global(node) => &mut node.range,
            Stmt::Nonlocal(node) => &mut node.range,
            Stmt::Expr(node) => &mut node.range,
            Stmt::Pass(node) => &mut node.range,
            Stmt::Break(node) => &mut node.range,
            Stmt::Continue(node) => &mut node.range,
            Stmt::IpyEscapeCommand(node) => &mut node.range,
        };
        *range = TextRange::default();
        transformer::walk_stmt(self, stmt);
    }
}

/// the source a template keeps around the text it prints between two passthroughs, and
/// that text
struct Segment {
    head: Option<TextRange>,
    text: String,
    tail: Option<TextRange>,
}

impl Segment {
    fn push(self, fragments: &mut Vec<Fragment>) {
        if let Some(head) = self.head {
            push_source(fragments, head);
        }
        if !self.text.is_empty() {
            fragments.push(Fragment::Lit(self.text));
        }
        if let Some(tail) = self.tail {
            push_source(fragments, tail);
        }
    }
}

/// the tokens of `range` in `text` from its start, up to the first the simple tokenizer
/// cannot read — a string, a number — and whether there was none
fn forward_tokens(text: &str, range: TextRange) -> (Vec<SimpleToken>, bool) {
    readable(SimpleTokenizer::new(text, range))
}

/// as [`forward_tokens`], from the end of `range`, last first
fn backward_tokens(text: &str, range: TextRange, comments: &[TextRange]) -> Vec<SimpleToken> {
    readable(BackwardsTokenizer::new(text, range, comments)).0
}

fn readable(tokens: impl Iterator<Item = SimpleToken>) -> (Vec<SimpleToken>, bool) {
    let mut read = Vec::new();
    for token in tokens {
        match token.kind() {
            SimpleTokenKind::Other | SimpleTokenKind::Bogus => return (read, false),
            kind if kind.is_trivia() => {}
            _ => read.push(token),
        }
    }
    (read, true)
}

/// `range` of the source passed through after `fragments`, as one span with a
/// passthrough of the source just before it
///
/// an edit that spans the two is inside the one span, and materializes there
fn push_source(fragments: &mut Vec<Fragment>, range: TextRange) {
    if let Some(Fragment::Src(previous)) = fragments.last_mut()
        && previous.end() == range.start()
    {
        *previous = previous.cover(range);
    } else if !range.is_empty() {
        fragments.push(Fragment::Src(range));
    }
}

/// `whitespace`, printed between two tokens, holding the lines `trivia`, the source
/// between the same two tokens, holds: its comments and blank lines, followed by the
/// indentation the printed text continues at
///
/// only a line break the printed text makes is given them — anywhere else, a comment
/// would run into the code after it
fn respaced(whitespace: &str, trivia: &str) -> String {
    match (whitespace.rfind('\n'), trivia.rfind('\n')) {
        (Some(printed), Some(source)) => {
            format!("{}{}", &trivia[..=source], &whitespace[printed + 1..])
        }
        _ => whitespace.to_owned(),
    }
}

/// `text` appended to `out`, each line after the first indented by `indent`
fn push_indented(out: &mut String, text: &str, indent: Option<&str>) {
    match indent {
        Some(indent) if !indent.is_empty() => {
            let mut lines = text.split('\n');
            if let Some(first) = lines.next() {
                out.push_str(first);
            }
            for line in lines {
                out.push('\n');
                if !line.is_empty() {
                    out.push_str(indent);
                }
                out.push_str(line);
            }
        }
        _ => out.push_str(text),
    }
}

/// an expression whose source needs no parentheses wherever an expression can stand,
/// or which cannot be given any
fn binds_tightest(expr: &Expr) -> bool {
    match expr {
        Expr::Tuple(tuple) => {
            tuple.parenthesized
                || tuple
                    .elts
                    .iter()
                    .any(|element| matches!(element, Expr::Slice(_) | Expr::Starred(_)))
        }
        Expr::Generator(generator) => generator.parenthesized,
        Expr::Name(_)
        | Expr::Attribute(_)
        | Expr::Subscript(_)
        | Expr::Call(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::FString(_)
        | Expr::TString(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_)
        | Expr::List(_)
        | Expr::Dict(_)
        | Expr::Set(_)
        | Expr::ListComp(_)
        | Expr::SetComp(_)
        | Expr::DictComp(_)
        | Expr::Starred(_)
        | Expr::Slice(_) => true,
        _ => false,
    }
}

fn placeholder(label: &str, range: TextRange) -> String {
    format!(
        "{label}{}x{}",
        u32::from(range.start()),
        u32::from(range.end())
    )
}

fn placeholder_name(label: &str, range: TextRange) -> Expr {
    Expr::Name(ExprName {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        id: Name::from(placeholder(label, range)),
        ctx: ExprContext::Load,
    })
}

/// how a printed node spells the names it kept from the source
#[derive(Clone, Copy)]
enum Spelling {
    /// as the passes left them
    Rewritten,
    /// as the source spelled them, so a node whose one change is a name prints as it
    /// was parsed
    Parsed,
}

/// replaces each node the passes kept with a placeholder naming it
struct Placeholders<'l> {
    label: &'l str,
    parsed: &'l Parsed<'l>,
    spelling: Spelling,
}

impl NameTransformer for Placeholders<'_> {
    fn name(&self, name: &mut Identifier) {
        if let Spelling::Parsed = self.spelling
            && let Some(original) = self.parsed.identifier(name.range)
        {
            name.id = original.id.clone();
        }
    }
}

impl Transformer for Placeholders<'_> {
    fn visit_expr(&self, expr: &mut Expr) {
        if is_kept(self.parsed, expr.range(), expr_kind(expr)) {
            *expr = placeholder_name(self.label, expr.range());
        } else {
            self.expr_names(expr);
            transformer::walk_expr(self, expr);
        }
    }

    fn visit_parameter(&self, parameter: &mut Parameter) {
        self.name(&mut parameter.name);
        transformer::walk_parameter(self, parameter);
    }

    fn visit_keyword(&self, keyword: &mut Keyword) {
        self.keyword_names(keyword);
        transformer::walk_keyword(self, keyword);
    }

    fn visit_alias(&self, alias: &mut Alias) {
        self.alias_names(alias);
    }

    fn visit_type_param(&self, type_param: &mut TypeParam) {
        self.type_param_names(type_param);
        transformer::walk_type_param(self, type_param);
    }

    fn visit_except_handler(&self, except_handler: &mut ExceptHandler) {
        self.except_handler_names(except_handler);
        transformer::walk_except_handler(self, except_handler);
    }

    fn visit_pattern(&self, pattern: &mut Pattern) {
        self.pattern_names(pattern);
        transformer::walk_pattern(self, pattern);
    }

    fn visit_pattern_keyword(&self, pattern_keyword: &mut PatternKeyword) {
        self.name(&mut pattern_keyword.attr);
        transformer::walk_pattern_keyword(self, pattern_keyword);
    }

    fn visit_stmt(&self, stmt: &mut Stmt) {
        if !is_kept(self.parsed, stmt.range(), stmt_kind(stmt)) {
            self.stmt_names(stmt);
            transformer::walk_stmt(self, stmt);
        } else {
            *stmt = Stmt::Expr(StmtExpr {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                value: Box::new(placeholder_name(self.label, stmt.range())),
            });
        }
    }
}

/// as [`Placeholders`], but the node at `range` becomes `operand`
struct Probe<'p> {
    label: &'p str,
    parsed: &'p Parsed<'p>,
    range: TextRange,
    operand: &'p Expr,
}

impl Transformer for Probe<'_> {
    fn visit_expr(&self, expr: &mut Expr) {
        if !is_kept(self.parsed, expr.range(), expr_kind(expr)) {
            transformer::walk_expr(self, expr);
        } else if expr.range() == self.range {
            *expr = self.operand.clone();
        } else {
            *expr = placeholder_name(self.label, expr.range());
        }
    }

    fn visit_stmt(&self, stmt: &mut Stmt) {
        if !is_kept(self.parsed, stmt.range(), stmt_kind(stmt)) {
            transformer::walk_stmt(self, stmt);
        } else {
            *stmt = Stmt::Expr(StmtExpr {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                value: Box::new(placeholder_name(self.label, stmt.range())),
            });
        }
    }
}

/// what `node` holds that it does not print itself
struct Below<'m> {
    /// the nodes directly below it, looking through what a pass synthesized, that
    /// kept their ranges
    holes: Vec<Hole<'m>>,
    /// the names it spells, its own and those of the nodes between it and its holes
    names: Vec<&'m Identifier>,
}

fn below<'m>(parsed: &Parsed<'_>, node: &Hole<'m>) -> Below<'m> {
    struct Collect<'m, 'p> {
        parsed: &'p Parsed<'p>,
        below: Below<'m>,
    }
    impl<'m> Visitor<'m> for Collect<'m, '_> {
        fn visit_expr(&mut self, expr: &'m Expr) {
            if is_kept(self.parsed, expr.range(), expr_kind(expr)) {
                self.below.holes.push(Hole::Expr(expr));
            } else {
                self.below.names.extend(own_names(expr.into()));
                visitor::walk_expr(self, expr);
            }
        }

        fn visit_stmt(&mut self, stmt: &'m Stmt) {
            if is_kept(self.parsed, stmt.range(), stmt_kind(stmt)) {
                self.below.holes.push(Hole::Stmt(stmt));
            } else {
                self.below.names.extend(own_names(stmt.into()));
                visitor::walk_stmt(self, stmt);
            }
        }

        fn visit_parameter(&mut self, parameter: &'m Parameter) {
            self.below.names.extend(own_names(parameter.into()));
            visitor::walk_parameter(self, parameter);
        }

        fn visit_keyword(&mut self, keyword: &'m Keyword) {
            self.below.names.extend(own_names(keyword.into()));
            visitor::walk_keyword(self, keyword);
        }

        fn visit_alias(&mut self, alias: &'m Alias) {
            self.below.names.extend(own_names(alias.into()));
        }

        fn visit_type_param(&mut self, type_param: &'m TypeParam) {
            self.below.names.extend(own_names(type_param.into()));
            visitor::walk_type_param(self, type_param);
        }

        fn visit_except_handler(&mut self, except_handler: &'m ExceptHandler) {
            self.below.names.extend(own_names(except_handler.into()));
            visitor::walk_except_handler(self, except_handler);
        }

        fn visit_pattern(&mut self, pattern: &'m Pattern) {
            self.below.names.extend(own_names(pattern.into()));
            visitor::walk_pattern(self, pattern);
        }

        fn visit_pattern_keyword(&mut self, pattern_keyword: &'m PatternKeyword) {
            self.below.names.extend(own_names(pattern_keyword.into()));
            visitor::walk_pattern_keyword(self, pattern_keyword);
        }
    }
    let mut collect = Collect {
        parsed,
        below: Below {
            holes: Vec::new(),
            names: Vec::new(),
        },
    };
    match node {
        Hole::Expr(expr) => {
            collect.below.names.extend(own_names((*expr).into()));
            visitor::walk_expr(&mut collect, expr);
        }
        Hole::Stmt(stmt) => {
            collect.below.names.extend(own_names((*stmt).into()));
            visitor::walk_stmt(&mut collect, stmt);
        }
    }
    collect.below
}

/// as [`below`], for the statements of a suite
fn below_statements<'m>(parsed: &Parsed<'_>, statements: &[&'m Stmt]) -> Below<'m> {
    let mut below = Below {
        holes: Vec::new(),
        names: Vec::new(),
    };
    for stmt in statements {
        if is_kept(parsed, stmt.range(), stmt_kind(stmt)) {
            below.holes.push(Hole::Stmt(stmt));
        } else {
            let inner = self::below(parsed, &Hole::Stmt(stmt));
            below.holes.extend(inner.holes);
            below.names.extend(inner.names);
        }
    }
    below
}

fn kind_of(node: &Hole<'_>) -> NodeKind {
    match node {
        Hole::Expr(expr) => expr_kind(expr),
        Hole::Stmt(stmt) => stmt_kind(stmt),
    }
}

/// every expression, statement and name of the parse, by range
#[derive(Default)]
struct Parsed<'o> {
    nodes: HashMap<TextRange, Vec<Hole<'o>>>,
    names: HashMap<TextRange, &'o Identifier>,
}

impl<'o> Parsed<'o> {
    /// the name the source spells at `range`
    ///
    /// a name a pass made carries no range, and neither does one a pass parsed out of
    /// text of its own, once [`forget_stmt_ranges`] has run over it
    fn identifier(&self, range: TextRange) -> Option<&'o Identifier> {
        if range.is_empty() {
            return None;
        }
        self.names.get(&range).copied()
    }

    fn record(&mut self, names: Vec<&'o Identifier>) {
        for name in names {
            self.names.insert(name.range, name);
        }
    }

    fn at(&self, range: TextRange, kind: NodeKind) -> Option<Hole<'o>> {
        self.nodes.get(&range)?.iter().find_map(|node| {
            (kind_of(node) == kind).then_some(match node {
                Hole::Expr(expr) => Hole::Expr(expr),
                Hole::Stmt(stmt) => Hole::Stmt(stmt),
            })
        })
    }
}

impl<'o> Visitor<'o> for Parsed<'o> {
    fn visit_expr(&mut self, expr: &'o Expr) {
        self.nodes
            .entry(expr.range())
            .or_default()
            .push(Hole::Expr(expr));
        self.record(own_names(expr.into()));
        visitor::walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'o Stmt) {
        self.nodes
            .entry(stmt.range())
            .or_default()
            .push(Hole::Stmt(stmt));
        self.record(own_names(stmt.into()));
        visitor::walk_stmt(self, stmt);
    }

    fn visit_parameter(&mut self, parameter: &'o Parameter) {
        self.record(own_names(parameter.into()));
        visitor::walk_parameter(self, parameter);
    }

    fn visit_keyword(&mut self, keyword: &'o Keyword) {
        self.record(own_names(keyword.into()));
        visitor::walk_keyword(self, keyword);
    }

    fn visit_alias(&mut self, alias: &'o Alias) {
        self.record(own_names(alias.into()));
    }

    fn visit_type_param(&mut self, type_param: &'o TypeParam) {
        self.record(own_names(type_param.into()));
        visitor::walk_type_param(self, type_param);
    }

    fn visit_except_handler(&mut self, except_handler: &'o ExceptHandler) {
        self.record(own_names(except_handler.into()));
        visitor::walk_except_handler(self, except_handler);
    }

    fn visit_pattern(&mut self, pattern: &'o Pattern) {
        self.record(own_names(pattern.into()));
        visitor::walk_pattern(self, pattern);
    }

    fn visit_pattern_keyword(&mut self, pattern_keyword: &'o PatternKeyword) {
        self.record(own_names(pattern_keyword.into()));
        visitor::walk_pattern_keyword(self, pattern_keyword);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    const TEMPERATURES: &str = indoc! {"
        class Celsius:
            def __init__(self, degrees: int):
                self.degrees = degrees

        class Fahrenheit:
            def __init__(self, degrees: int):
                self.degrees = degrees

            @classmethod
            def __from__(cls, value: Celsius) -> Self:
                return cls(value.degrees * 9 // 5 + 32)

        def report(t: Fahrenheit) -> int:
            return t.degrees

    "};

    /// numbering a repeated `_` rewrites the `def`, and the conversion its body makes
    /// is an edit the type-aware pass keyed on the body's source. a call the printer
    /// would spell differently from the source went missing without a word
    #[test]
    fn a_conversion_in_a_def_whose_parameters_were_renamed_is_kept() {
        let out = check(&format!(
            "{TEMPERATURES}def f(c: Celsius, _: int, _: int, /) -> int:\n    return report(\n        c,\n    )\n"
        ));
        assert!(
            out.contains("def f(c: Celsius, _: int, _2: int, /) -> int:"),
            "got:\n{out}"
        );
        assert!(out.contains("Fahrenheit.__from__(c)"), "got:\n{out}");
    }

    /// `typeof` is lowered in the syntax tree, and the extension call beside it is an
    /// edit — which printing the `def` whole reported as a transform conflict
    #[test]
    fn an_extension_call_beside_a_typeof_annotation_is_kept() {
        let out = check(indoc! {"
            marker: int = 1

            extension list:
                def second(self) -> Element:
                    return self[1]

            def f(xs: list[int], tag: typeof marker) -> int:
                return xs.second()
        "});
        assert!(
            out.contains("def f(xs: list[int], tag: TypeOf[marker]) -> int:"),
            "got:\n{out}"
        );
        assert!(out.contains("_by_ext__list__second(xs)"), "got:\n{out}");
    }

    /// python before 3.14 evaluates a parameter's annotation where the `def` runs, and
    /// `later` is not bound yet there. the quote around the annotation is an edit, and
    /// the `TypeOf` inside it is the syntax tree's
    #[test]
    fn a_forward_reference_through_typeof_is_quoted() {
        let out = check(indoc! {"
            def f(x: typeof later) -> typeof later:
                return x

            later: int = 1
        "});
        assert!(
            out.contains("def f(x: \"TypeOf[later]\") -> \"TypeOf[later]\":"),
            "got:\n{out}"
        );
    }

    /// only what a pass rewrote is printed from the tree: a node it left alone is the
    /// source, its comments and spelling included
    #[test]
    fn a_rewritten_statement_keeps_the_source_it_did_not_rewrite() {
        let out = check(indoc! {"
            def f(x: int | None, y: int = 3) -> int:
                sentinel A
                return (  # a comment the printer has no spelling for
                    3
                )
        "});
        assert!(
            out.contains(indoc! {"
                def f(x: int | None, y: int = 3) -> int:
                    A = Sentinel(\"A\")
                    return (  # a comment the printer has no spelling for
                        3
                    )
            "}),
            "got:\n{out}"
        );
    }

    /// the source between two nodes a rewritten statement kept is kept too, wherever the
    /// statement prints the same tokens there: a comment on the header's line, above the
    /// first statement, between two statements and at the end of one, the blank lines
    /// between them, and the layout of a signature spread over several lines
    #[test]
    fn comments_between_the_kept_nodes_of_a_rewritten_statement_are_kept() {
        let out = check(indoc! {"
            def f(
                a: int,  # after a parameter
                b: int
            ) -> int:  # on the header line
                # before the first statement
                x = a  # at the end of a statement

                # between two statements
                sentinel A
                return x
        "});
        assert!(
            out.contains(indoc! {"
                def f(
                    a: int,  # after a parameter
                    b: int
                ) -> int:  # on the header line
                    # before the first statement
                    x = a  # at the end of a statement

                    # between two statements
                    A = Sentinel(\"A\")
                    return x
            "}),
            "got:\n{out}"
        );
    }

    /// a decorator is a node the statement keeps, and the source between it and the
    /// `def` is kept with its comments
    #[test]
    fn comments_around_a_decorator_of_a_rewritten_statement_are_kept() {
        let out = check(indoc! {"
            class C:
                @staticmethod  # on the decorator line
                # between the decorator and the def
                def m() -> int:
                    sentinel A
                    return 1
        "});
        assert!(
            out.contains(indoc! {"
                class C:
                    @staticmethod  # on the decorator line
                    # between the decorator and the def
                    def m() -> int:
                        A = Sentinel(\"A\")
                        return 1
            "}),
            "got:\n{out}"
        );
    }

    /// a comment beside a statement a pass replaced has no source left to be kept in, and
    /// stays with the statements it sat between: one ending the line before it stays
    /// there, one ending its own line ends the line of what replaced it, and one on a
    /// line of its own after it stays ahead of the next statement
    #[test]
    fn comments_beside_a_replaced_statement_keep_their_place() {
        let out = check(indoc! {"
            def f() -> int:
                x = 1  # at the end of the statement before
                # before the replaced statement
                sentinel A  # at the end of the replaced statement
                # before the statement after
                return x
        "});
        assert!(
            out.contains(indoc! {"
                def f() -> int:
                    x = 1  # at the end of the statement before
                    # before the replaced statement
                    A = Sentinel(\"A\")  # at the end of the replaced statement
                    # before the statement after
                    return x
            "}),
            "got:\n{out}"
        );
    }

    /// a comment after the last statement of a block, and one on the line of the clause
    /// that follows it, lie between two statements the rewritten `if` keeps
    #[test]
    fn comments_around_a_clause_of_a_rewritten_statement_are_kept() {
        let out = check(indoc! {"
            def f(flag: bool) -> int:
                if flag:
                    sentinel A
                    return 1
                    # after the last statement of the block
                else:  # on the line of the clause
                    return 2
                # after the last statement of the function
        "});
        assert!(
            out.contains(indoc! {"
                def f(flag: bool) -> int:
                    if flag:
                        A = Sentinel(\"A\")
                        return 1
                        # after the last statement of the block
                    else:  # on the line of the clause
                        return 2
                    # after the last statement of the function
            "}),
            "got:\n{out}"
        );
    }

    /// numbering a repeated `_` changes a parameter's name and nothing else about the
    /// `def`, so the name is all that is re-emitted: the signature keeps its layout and
    /// its comments
    #[test]
    fn a_renamed_parameter_is_all_that_is_re_emitted() {
        let out = check(indoc! {"
            def f(
                _: int,  # the first
                _: int = 3,  # the second
            ) -> int:
                return 1
        "});
        assert!(
            out.contains(indoc! {"
                def f(
                    _: int,  # the first
                    _2: int = 3, /,  # the second
                ) -> int:
                    return 1
            "}),
            "got:\n{out}"
        );
    }

    /// the `context` prefix is deleted by an edit, which has to land beside the name
    /// numbering a repeated `_` rewrote. printing the `def` whole spelled the prefix
    /// again, and the transpile was refused as a conflict
    #[test]
    fn an_edit_beside_a_renamed_parameter_is_applied() {
        let out = check(indoc! {"
            def show(context _: int, context _: str) -> str:
                return \"ok\"
        "});
        assert!(
            out.contains("def show(_: int, _2: str, /) -> str:"),
            "got:\n{out}"
        );
    }

    /// the shorthand `init(...)` is lowered in its header's source, which stands for a
    /// `def __init__` with a `self` and a `-> None` the source never wrote. a pass that
    /// rewrites a statement of its body re-emits the body alone, and the header's
    /// lowering lands; printing the whole `def` spelled the header from the syntax tree,
    /// and the transpile was refused
    #[test]
    fn an_init_shorthand_whose_body_a_pass_rewrote_is_lowered() {
        let out = check(indoc! {"
            class A:
                init(let x: int, y: int):
                    sentinel S
                    self.z = x + y
        "});
        assert!(
            out.contains(indoc! {"
                class A:
                    def __init__(self, x: int, y: int):
                        self.x: int = x
                        S = Sentinel(\"S\")
                        self.z = x + y
            "}),
            "got:\n{out}"
        );
    }

    /// a modifier is a decorator in the syntax tree and a keyword in the source, which
    /// the modifiers pass lowers where it is written. printing the `def` whole spelled
    /// it `@private`, which is not python
    #[test]
    fn modifiers_of_a_def_whose_body_a_pass_rewrote_are_lowered() {
        let out = check(indoc! {"
            class A:
                private def f(self) -> int:
                    sentinel S
                    return 1

                static def g() -> int:
                    sentinel T
                    return 2
        "});
        assert!(
            out.contains(indoc! {"
                class A:
                    def __f(self) -> int:
                        S = Sentinel(\"S\")
                        return 1

                    @staticmethod
                    def g() -> int:
                        T = Sentinel(\"T\")
                        return 2
            "}),
            "got:\n{out}"
        );
    }

    /// a `raises` clause is deleted from the header's source, and the header of a `def`
    /// whose body a pass rewrote is passed through with the deletion applied
    #[test]
    fn a_raises_clause_of_a_def_whose_body_a_pass_rewrote_is_deleted() {
        let out = check(indoc! {"
            def f() -> int raises ValueError:
                sentinel S
                raise ValueError
        "});
        assert!(
            out.contains(indoc! {"
                def f() -> int:
                    S = Sentinel(\"S\")
                    raise ValueError
            "}),
            "got:\n{out}"
        );
    }

    /// a lowering that names a parameter names the one python binds, which a repeated
    /// `_` is not
    #[test]
    fn a_default_guard_names_the_renamed_parameter() {
        let out = check(indoc! {"
            def f(_: list[int] = [], _: list[int] = []) -> int:
                return len(_)
        "});
        assert!(out.contains("if _ is _MISSING:"), "got:\n{out}");
        assert!(
            out.contains("if _2 is _MISSING:\n        _2 = []"),
            "got:\n{out}"
        );
    }

    /// a suite written on the line of its clause is re-emitted like any other suite:
    /// only what changed, as a block below the clause, so the header keeps the source
    /// the `raises` clause was deleted from. printing the whole statement instead spelled
    /// the clause again and dropped that deletion, and the transpile was refused
    #[test]
    fn a_suite_on_the_line_of_its_clause_is_re_emitted_as_a_block() {
        let out = transpile(
            indoc! {"
                marker: int = 1

                def f(s: str, t: typeof marker) -> int raises ValueError: return len(s) + t
            "},
            &Config {
                soundness: crate::SoundnessPositions::all(),
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains(concat!(
                "def f(s: str, t: TypeOf[marker]) -> int: \n",
                "    _soundness_check(s, str)\n",
                "    _soundness_check(t, int)\n",
                "    return len(s) + t\n",
            )),
            "got:\n{out}"
        );
    }

    /// the block a suite of its clause's line is re-emitted as is indented below the
    /// line the clause starts on, whatever that line is indented by
    #[test]
    fn a_re_emitted_inline_suite_is_indented_below_its_clause() {
        let out = transpile(
            indoc! {"
                marker: int = 1

                class A:
                    def f(self, s: str, t: typeof marker) -> int: return len(s) + t
            "},
            &Config {
                soundness: crate::SoundnessPositions::all(),
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains(concat!(
                "class A:\n",
                "    def f(self, s: str, t: TypeOf[marker]) -> int: \n",
                "        _soundness_check(s, str)\n",
                "        _soundness_check(t, int)\n",
                "        return len(s) + t\n",
            )),
            "got:\n{out}"
        );
    }

    /// a rewritten statement below the first column keeps the indentation of the lines
    /// it passes through, whatever the source indents by
    #[test]
    fn a_rewritten_method_keeps_its_indentation() {
        let out = check(indoc! {"
            class A:
              def f(self) -> int:
                sentinel B
                if True:
                  return 1
                return 2
        "});
        assert!(
            out.contains(indoc! {"
                class A:
                  def f(self) -> int:
                    B = Sentinel(\"B\")
                    if True:
                      return 1
                    return 2
            "}),
            "got:\n{out}"
        );
    }

    /// a file may indent one block by four spaces and another by two. a `def` whose body
    /// a pass rewrote is re-emitted as its own source is indented, where printing it with
    /// the four spaces the file starts with put its statements out of step with the
    /// ones it passes through, and the output did not parse
    #[test]
    fn a_rewritten_body_is_indented_as_its_own_source_is() {
        let out = check(indoc! {"
            class A:
                def g(self) -> int:
                    return 1


            def f() -> int:
              sentinel B
              if True:
                return 1
              return 2
        "});
        assert!(
            out.contains(indoc! {"
                def f() -> int:
                  B = Sentinel(\"B\")
                  if True:
                    return 1
                  return 2
            "}),
            "got:\n{out}"
        );
    }

    /// a statement printed whole opens its blocks by the step its own source indents
    /// them by, so a block it prints holds the statements it passes through at the
    /// depth they are written at. printed by the four spaces given for the file, the
    /// `if` it adds would be deeper than the `y = 1` beside it
    #[test]
    fn a_block_a_rewritten_statement_prints_is_indented_as_its_source_is() {
        use ruff_python_ast::{Expr, ExprUnaryOp, Stmt, UnaryOp};
        use ruff_python_codegen::Indentation;
        use ruff_text_size::Ranged;

        use super::{Rerendering, forget_stmt_ranges};
        use crate::transforms::ast_driver::Fragment;

        let source = "class A:\n    def g(self):\n        return 1\n\nif x:\n  y = 1\n";
        let parsed = ruff_python_parser::parse_module(source).unwrap();
        let comments = ruff_python_trivia::CommentRanges::from(parsed.tokens());
        let parsed = parsed.into_syntax();
        let original = &parsed.body[1];
        let mut rewritten = original.clone();
        let Stmt::If(conditional) = &mut rewritten else {
            panic!("an `if`")
        };
        let test = conditional.test.clone();
        *conditional.test = Expr::UnaryOp(ExprUnaryOp {
            node_index: ruff_python_ast::AtomicNodeIndex::NONE,
            range: ruff_text_size::TextRange::default(),
            op: UnaryOp::Not,
            operand: test,
        });
        let mut added = ruff_python_parser::parse_module("if y:\n    pass\n")
            .unwrap()
            .into_syntax()
            .body
            .remove(0);
        forget_stmt_ranges(&mut added);
        conditional.body.push(added);
        let indentation = Indentation::new("    ".to_string());
        let edits = Rerendering::new(source, &comments, &[], &indentation).edits(
            original,
            &rewritten,
            original.range(),
        );
        let spelled: Vec<String> = edits
            .iter()
            .map(|(_, fragments)| {
                fragments
                    .iter()
                    .map(|fragment| match fragment {
                        Fragment::Lit(text) => text.clone(),
                        Fragment::Src(range) => source[*range].to_string(),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(spelled, ["if not x:\n  y = 1\n  if y:\n    pass"]);
    }

    /// a placeholder binds tighter than any operator, so whether its source needs
    /// parentheses is read off how the printer spells the operand in its place: `a + b`
    /// moved from a call's argument to under a `*` needs them
    #[test]
    fn a_moved_operand_is_parenthesized_where_it_has_to_be() {
        use ruff_python_ast::{Expr, ExprBinOp, ExprCall, Operator, Stmt};
        use ruff_python_codegen::Indentation;
        use ruff_text_size::{Ranged, TextRange};

        use super::Rerendering;
        use crate::transforms::ast_driver::Fragment;

        let source = "x = f(a + b, c)\n";
        let parsed = ruff_python_parser::parse_module(source).unwrap();
        let comments = ruff_python_trivia::CommentRanges::from(parsed.tokens());
        let parsed = parsed.into_syntax();
        let original = &parsed.body[0];
        let mut rewritten = original.clone();
        let Stmt::Assign(assign) = &mut rewritten else {
            panic!("an assignment")
        };
        let Expr::Call(ExprCall { arguments, .. }) = assign.value.as_ref() else {
            panic!("a call")
        };
        let (sum, c) = (arguments.args[0].clone(), arguments.args[1].clone());
        *assign.value = Expr::BinOp(ExprBinOp {
            node_index: ruff_python_ast::AtomicNodeIndex::NONE,
            range: TextRange::default(),
            left: Box::new(sum),
            op: Operator::Mult,
            right: Box::new(c),
        });
        let indentation = Indentation::default();
        let edits = Rerendering::new(source, &comments, &[], &indentation).edits(
            original,
            &rewritten,
            original.range(),
        );
        let spelled: Vec<String> = edits
            .iter()
            .map(|(_, fragments)| {
                fragments
                    .iter()
                    .map(|fragment| match fragment {
                        Fragment::Lit(text) => text.clone(),
                        Fragment::Src(range) => source[*range].to_string(),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(spelled, ["x = (a + b) * c"]);
    }
}
