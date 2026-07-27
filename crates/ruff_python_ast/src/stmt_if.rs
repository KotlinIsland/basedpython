use std::iter;

use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange};

use crate::{ElifElseClause, Expr, Pattern, Stmt, StmtIf};

impl StmtIf {
    /// The boolean condition this `if` tests, or `None` when it is a
    /// basedpython `if let <pattern> := <subject>:` clause — whose `test` is the
    /// subject matched against the pattern, not a condition.
    ///
    /// Anything that reads `test` *as a condition* — evaluating its truthiness,
    /// merging it with another condition, rewriting it into an expression —
    /// must go through this rather than the field, or it will silently
    /// misinterpret a pattern clause.
    pub fn condition(&self) -> Option<&Expr> {
        self.pattern.is_none().then_some(&*self.test)
    }
}

impl ElifElseClause {
    /// The boolean condition this clause tests. `None` for an `else` clause, and
    /// `None` for a basedpython `elif let <pattern> := <subject>:` clause — see
    /// [`StmtIf::condition`].
    pub fn condition(&self) -> Option<&Expr> {
        self.pattern.is_none().then_some(self.test.as_ref())?
    }
}

/// Return the `Range` of the first `Elif` or `Else` token in an `If` statement.
pub fn elif_else_range(clause: &ElifElseClause, contents: &str) -> Option<TextRange> {
    let token = SimpleTokenizer::new(contents, clause.range)
        .skip_trivia()
        .next()?;
    matches!(token.kind, SimpleTokenKind::Elif | SimpleTokenKind::Else).then_some(token.range())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BranchKind {
    If,
    Elif,
}

#[derive(Debug)]
pub struct IfElifBranch<'a> {
    pub kind: BranchKind,
    /// basedpython: the pattern of an `if let <pattern> := <subject>:` branch,
    /// whose `test` is then the subject rather than a condition. A rule that
    /// rewrites or merges conditions must leave such a branch alone
    pub pattern: Option<&'a Pattern>,
    pub test: &'a Expr,
    pub body: &'a [Stmt],
    range: TextRange,
}

impl Ranged for IfElifBranch<'_> {
    fn range(&self) -> TextRange {
        self.range
    }
}

pub fn if_elif_branches(stmt_if: &StmtIf) -> impl Iterator<Item = IfElifBranch<'_>> {
    iter::once(IfElifBranch {
        kind: BranchKind::If,
        pattern: stmt_if.pattern.as_deref(),
        test: stmt_if.test.as_ref(),
        body: stmt_if.body.as_slice(),
        range: TextRange::new(stmt_if.start(), stmt_if.body.last().unwrap().end()),
    })
    .chain(stmt_if.elif_else_clauses.iter().filter_map(|clause| {
        Some(IfElifBranch {
            kind: BranchKind::Elif,
            pattern: clause.pattern.as_deref(),
            test: clause.test.as_ref()?,
            body: clause.body.as_slice(),
            range: clause.range,
        })
    }))
}
