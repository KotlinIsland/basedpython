//! Locating statements in their suites and spelling the edits that move them.

use ruff_diagnostics::Edit;
use ruff_python_ast::token::parenthesized_range;
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, ExprRef, Stmt};
use ruff_python_trivia::{PythonWhitespace, indentation_at_offset};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::RefactorContext;

/// A statement together with the suite it is written in.
#[derive(Clone, Copy)]
pub(crate) struct InSuite<'a> {
    pub(crate) suite: &'a [Stmt],
    pub(crate) index: usize,
}

impl<'a> InSuite<'a> {
    fn statement(&self) -> &'a Stmt {
        &self.suite[self.index]
    }

    pub(crate) fn next(&self) -> Option<&'a Stmt> {
        self.suite.get(self.index + 1)
    }
}

/// The suite `statement` is written in, found by identity.
pub(crate) fn suite_of<'a>(body: &'a [Stmt], statement: &Stmt) -> Option<InSuite<'a>> {
    for (index, candidate) in body.iter().enumerate() {
        if std::ptr::eq(candidate, statement) {
            return Some(InSuite { suite: body, index });
        }
        if !candidate.range().contains_range(statement.range()) {
            continue;
        }
        for nested in nested_suites(candidate) {
            if let Some(found) = suite_of(nested, statement) {
                return Some(found);
            }
        }
    }
    None
}

/// The suites written directly inside `statement`.
pub(crate) fn nested_suites(statement: &Stmt) -> Vec<&[Stmt]> {
    match statement {
        Stmt::FunctionDef(stmt) => vec![&stmt.body],
        Stmt::ClassDef(stmt) => vec![&stmt.body],
        Stmt::For(stmt) => vec![&stmt.body, &stmt.orelse],
        Stmt::While(stmt) => vec![&stmt.body, &stmt.orelse],
        Stmt::If(stmt) => std::iter::once(&*stmt.body)
            .chain(stmt.elif_else_clauses.iter().map(|clause| &*clause.body))
            .collect(),
        Stmt::With(stmt) => vec![&stmt.body],
        Stmt::Match(stmt) => stmt.cases.iter().map(|case| &*case.body).collect(),
        Stmt::Try(stmt) => std::iter::once(&*stmt.body)
            .chain(stmt.handlers.iter().map(|handler| {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                &*handler.body
            }))
            .chain([&*stmt.orelse, &*stmt.finalbody])
            .collect(),
        Stmt::Let(stmt) => vec![&stmt.orelse],
        _ => Vec::new(),
    }
}

/// The innermost statement containing `range`, with its ancestors, outermost first.
pub(crate) fn statement_ancestors(body: &[Stmt], range: TextRange) -> Vec<&Stmt> {
    let mut ancestors = Vec::new();
    let mut suite = body;
    'descend: loop {
        for statement in suite {
            if statement.range().contains_range(range) {
                ancestors.push(statement);
                for nested in nested_suites(statement) {
                    if nested.iter().any(|stmt| stmt.range().contains_range(range)) {
                        suite = nested;
                        continue 'descend;
                    }
                }
                break 'descend;
            }
        }
        break;
    }
    ancestors
}

impl RefactorContext<'_> {
    /// Whether `statement` is alone on its lines: nothing but indentation before
    /// it and nothing but a comment after it.
    pub(crate) fn owns_its_lines(&self, node: impl Ranged) -> bool {
        let source = self.source();
        let range = node.range();
        indentation_at_offset(range.start(), source).is_some()
            && source[TextRange::new(range.end(), source.line_end(range.end()))]
                .trim_whitespace_start()
                .chars()
                .next()
                .is_none_or(|first| first == '#')
    }

    /// The indentation `node` is written at.
    pub(crate) fn indentation(&self, node: impl Ranged) -> &str {
        let source = self.source();
        let start = node.range().start();
        &source[TextRange::new(source.line_start(start), start)]
    }

    /// An edit that removes `statement` from `in_suite`, lines and trailing comment
    /// included, or replaces it with `pass` when it is all the suite holds.
    pub(crate) fn delete_statement(&self, in_suite: InSuite<'_>) -> Edit {
        let statement = in_suite.statement();
        if in_suite.suite.len() == 1 {
            let source = self.source();
            let end = source.line_end(statement.end());
            return Edit::replacement("pass".to_string(), statement.start(), end);
        }
        Edit::range_deletion(self.source().full_lines_range(statement.range()))
    }

    /// The source of `expr`, without any parentheses around it.
    pub(crate) fn expression_text(&self, expr: &Expr) -> &str {
        &self.source()[expr.range()]
    }

    /// The range of `expr` including any parentheses written around it.
    pub(crate) fn parenthesized_range(&self, expr: &Expr, parent: AnyNodeRef<'_>) -> TextRange {
        parenthesized_range(ExprRef::from(expr), parent, self.parsed.tokens())
            .unwrap_or_else(|| expr.range())
    }

    pub(crate) fn line_ending(&self) -> &'static str {
        self.stylist.line_ending().as_str()
    }

    /// One level of indentation, as the file writes it.
    pub(crate) fn indent_unit(&self) -> &str {
        self.stylist.indentation().as_str()
    }

    /// Whether the text at `range` spans more than one line.
    pub(crate) fn is_multiline(&self, range: TextRange) -> bool {
        self.source().contains_line_break(range)
    }
}

/// Where the comment lines directly above `offset`'s line begin, so a new
/// statement does not separate a definition from its comment.
pub(crate) fn leading_comments_start(source: &str, offset: TextSize) -> TextSize {
    let mut start = source.line_start(offset);
    while start > TextSize::default() {
        let previous = source.line_start(start - TextSize::from(1));
        let line = &source[TextRange::new(previous, start)];
        if line.trim_whitespace().starts_with('#') {
            start = previous;
        } else {
            break;
        }
    }
    start
}
