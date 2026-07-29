//! Lines up the `=` of consecutive assignments.
//!
//! ```python
//! alpha      = 1
//! beta      += 2
//! gamma: int = 3
//! ```
//!
//! The column an `=` ends up at depends on how the formatter renders the left side
//! of every other assignment in the same run, which isn't known before those
//! statements have been formatted. So the columns are computed up front, before the
//! document is built: [`AssignmentAlignmentState::compute`] walks every body, splits
//! it into runs of adjacent assignments, and measures each of them by formatting it
//! with a marker in place of its `=`. The printed column of that marker is the column
//! the `=` would be printed at without alignment; the widest one in a run becomes the
//! column the whole run is padded out to.
//!
//! Measuring formats the statement through the same code that later writes it, so
//! the two can't drift apart, at the cost of formatting each aligned assignment
//! twice. Nothing at all happens for files that don't align their assignments.

use ruff_formatter::{FormatContext, FormatOptions, LineWidth, format};
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_body};
use ruff_python_ast::{AnyNodeRef, Stmt, StmtAnnAssign, StmtAssign, StmtAugAssign};
use ruff_python_trivia::lines_after;
use ruff_text_size::{Ranged, TextSize};
use rustc_hash::FxHashMap;

use crate::comments::has_skip_comment;
use crate::prelude::*;
use crate::statement::stmt_ann_assign::FormatStmtAnnAssign;
use crate::statement::stmt_assign::FormatStmtAssign;
use crate::statement::stmt_aug_assign::FormatStmtAugAssign;

/// Stands in for the `=` of an assignment while its column is being measured.
///
/// Python source can't contain a NUL byte, so the marker can't be confused with the
/// code printed around it.
const MARKER: &str = "\u{0}";

/// The width of the `= (` that has to fit after the aligned column. When it doesn't,
/// the printer gives up on parenthesizing the value and splits the left side instead.
const EQUALS_AND_PARENTHESIS: u32 = 3;

/// Where the `=` of an assignment statement should be printed.
#[derive(Clone, Debug, Default)]
pub(crate) enum AssignmentAlignmentState {
    /// Assignments aren't aligned. Their `=` is preceded by a single space.
    #[default]
    Disabled,

    /// The measuring pass: assignments write [`MARKER`] in front of their `=`.
    Measuring,

    /// The number of spaces to insert in front of the `=` of the assignment
    /// statement starting at the given offset. Assignments that aren't in the map
    /// are printed without padding.
    Aligned(FxHashMap<TextSize, u32>),
}

impl AssignmentAlignmentState {
    /// Measures the assignments in `root` and its descendants and returns the padding
    /// each of them needs to line up with the other assignments in its run.
    pub(crate) fn compute(root: AnyNodeRef, context: &PyFormatContext) -> Self {
        if !context.options().assignment_alignment().is_enabled() {
            return Self::Disabled;
        }

        let options = context.options();
        let mut visitor = AlignmentVisitor {
            context,
            // The body of `root` is printed at the indentation the context starts at;
            // every body nested in it adds one more indent.
            indent: u32::from(
                context
                    .indent_level()
                    .to_ascii_spaces(options.indent_width()),
            ),
            paddings: FxHashMap::default(),
        };
        root.visit_source_order(&mut visitor);

        // Measuring formats the assignments, which marks the comments they write as
        // formatted even though the measured document is thrown away.
        context.comments().mark_all_unformatted();

        Self::Aligned(visitor.paddings)
    }

    /// The padding of the assignment statement starting at `start`.
    fn padding(&self, start: TextSize) -> AssignmentPadding {
        match self {
            AssignmentAlignmentState::Disabled => AssignmentPadding::None,
            AssignmentAlignmentState::Measuring => AssignmentPadding::Measure,
            AssignmentAlignmentState::Aligned(paddings) => paddings
                .get(&start)
                .map_or(AssignmentPadding::None, |padding| {
                    AssignmentPadding::Pad(*padding)
                }),
        }
    }
}

/// The spaces an assignment inserts in front of its `=` to line up with the other
/// assignments in its run.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) enum AssignmentPadding {
    /// The assignment isn't aligned.
    #[default]
    None,

    /// Write the marker the measuring pass looks for instead of padding.
    Measure,

    /// Write `n` spaces. Never zero.
    Pad(u32),
}

impl AssignmentPadding {
    /// The padding of the assignment statement starting at `start`.
    pub(crate) fn of(start: TextSize, context: &PyFormatContext) -> Self {
        context.assignment_alignment().padding(start)
    }

    /// The marker that stands in for the `=` while measuring. It's written directly
    /// in front of the `=`, so that the measured column is the column the `=` is
    /// printed at.
    pub(crate) fn marker(self) -> Option<Text<'static>> {
        matches!(self, AssignmentPadding::Measure).then(|| text(MARKER))
    }
}

/// Writes the spaces that push the `=` out to the aligned column.
impl Format<PyFormatContext<'_>> for AssignmentPadding {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        match self {
            AssignmentPadding::None | AssignmentPadding::Measure => Ok(()),
            AssignmentPadding::Pad(padding) => text(&" ".repeat(*padding as usize)).fmt(f),
        }
    }
}

/// An assignment statement that has an `=`, and can therefore be aligned.
#[derive(Copy, Clone, Debug)]
enum Assignment<'a> {
    Assign(&'a StmtAssign),
    AugAssign(&'a StmtAugAssign),
    /// Only annotated assignments that assign a value. A bare `a: int` has no `=`.
    AnnAssign(&'a StmtAnnAssign),
}

impl<'a> Assignment<'a> {
    fn try_from_statement(statement: &'a Stmt) -> Option<Self> {
        match statement {
            Stmt::Assign(assign) => Some(Assignment::Assign(assign)),
            Stmt::AugAssign(aug_assign) => Some(Assignment::AugAssign(aug_assign)),
            Stmt::AnnAssign(ann_assign) if ann_assign.value.is_some() => {
                Some(Assignment::AnnAssign(ann_assign))
            }
            _ => None,
        }
    }
}

/// Formats an assignment without its leading and trailing comments, which the
/// enclosing suite writes rather than the statement itself.
impl Format<PyFormatContext<'_>> for Assignment<'_> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        match self {
            Assignment::Assign(assign) => FormatStmtAssign.fmt_fields(assign, f),
            Assignment::AugAssign(aug_assign) => FormatStmtAugAssign.fmt_fields(aug_assign, f),
            Assignment::AnnAssign(ann_assign) => FormatStmtAnnAssign.fmt_fields(ann_assign, f),
        }
    }
}

struct AlignmentVisitor<'a, 'b> {
    context: &'b PyFormatContext<'a>,

    /// The number of spaces the body being visited is indented by.
    indent: u32,

    paddings: FxHashMap<TextSize, u32>,
}

impl<'a> SourceOrderVisitor<'a> for AlignmentVisitor<'a, '_> {
    fn visit_body(&mut self, body: &'a [Stmt]) {
        for run in runs(body, self.context) {
            self.align(run);
        }

        let indent_width = self.context.options().indent_width().value();
        self.indent = self.indent.saturating_add(indent_width);
        walk_body(self, body);
        self.indent = self.indent.saturating_sub(indent_width);
    }
}

impl AlignmentVisitor<'_, '_> {
    /// Measures every assignment in `run` and records how much padding each of them
    /// needs to put its `=` in the run's rightmost `=` column.
    fn align(&mut self, run: &[Stmt]) {
        let line_width = u32::from(self.context.options().line_width().value());

        let columns: Vec<_> = run
            .iter()
            .map(|statement| {
                Assignment::try_from_statement(statement)
                    .and_then(|assignment| self.measure(assignment))
                    // Lining up to a column that leaves no room for `= (` would make
                    // the printer split the left side rather than parenthesize the
                    // value, and a left side that splits has nothing to line up.
                    // Leaving such an assignment out keeps the padding of the rest
                    // small enough that none of them can be pushed into splitting.
                    .filter(|column| self.indent + column + EQUALS_AND_PARENTHESIS <= line_width)
            })
            .collect();

        let Some(column) = columns.iter().flatten().copied().max() else {
            return;
        };

        for (statement, measured) in run.iter().zip(columns) {
            if let Some(measured) = measured
                && measured < column
            {
                self.paddings.insert(statement.start(), column - measured);
            }
        }
    }

    /// The column the `=` of `assignment` is printed at, or `None` if its left side
    /// doesn't stay on one line where the statement is printed.
    ///
    /// The width the statement is measured at matters: a left side the printer would
    /// split has no column to line up to, and once it's split the formatter may leave
    /// a magic trailing comma behind that splits it for good. Measuring it as if it
    /// fit would line the rest of the run up against a column that only holds until
    /// the file is formatted a second time.
    fn measure(&self, assignment: Assignment) -> Option<u32> {
        let options = self.context.options();

        // The statement is printed at `self.indent`, but measured from column zero.
        // Taking the indentation out of the line width instead puts the same amount
        // of room in front of the printer.
        let indent = u16::try_from(self.indent).unwrap_or(u16::MAX);
        let available = options.line_width().value().saturating_sub(indent).max(1);

        let mut context = PyFormatContext::new(
            options
                .clone()
                .with_line_width(LineWidth::try_from(available).unwrap_or(LineWidth::MAX)),
            self.context.source(),
            self.context.comments().clone(),
            self.context.trivia(),
            self.context.tokens(),
        );
        context.set_assignment_alignment(AssignmentAlignmentState::Measuring);

        let printed = format!(context, [assignment]).ok()?.print().ok()?;
        let code = printed.as_code();

        let mut markers = code.match_indices(MARKER);
        let (marker, _) = markers.next()?;
        if markers.next().is_some() {
            return None;
        }

        match TextWidth::from_text(&code[..marker], options.indent_width()) {
            TextWidth::Width(width) => Some(width.value()),
            TextWidth::Multiline => None,
        }
    }
}

/// Splits `body` into the runs of assignments whose `=` line up.
///
/// A run ends at any statement that isn't an assignment, at any blank line — an
/// empty line separates a group of related assignments from the next one — and at
/// any statement whose formatting is suppressed.
fn runs<'a, 'b>(
    body: &'a [Stmt],
    context: &'b PyFormatContext<'a>,
) -> impl Iterator<Item = &'a [Stmt]> + use<'a, 'b> {
    let mut rest = body;

    let starts_run = |statement: &Stmt| {
        Assignment::try_from_statement(statement).is_some()
            && !suppresses_formatting(statement, context)
    };

    std::iter::from_fn(move || {
        loop {
            let first = rest.iter().position(&starts_run)?;
            rest = &rest[first..];

            let mut end = 1;
            while let Some(following) = rest.get(end)
                && starts_run(following)
                && !separated_by_blank_line(&rest[end - 1], following, context)
            {
                end += 1;
            }

            let (run, following) = rest.split_at(end);
            rest = following;

            // A lone assignment is already lined up with itself.
            if run.len() > 1 {
                return Some(run);
            }
        }
    })
}

/// Returns `true` if `statement` carries a comment that suppresses formatting.
///
/// The formatter honours those comments by printing the statement verbatim, keeping
/// whatever spacing it's written with. A run breaks at such a statement the same way
/// it breaks at a blank line, rather than aligning the statements around it to a
/// column that isn't printed.
fn suppresses_formatting(statement: &Stmt, context: &PyFormatContext) -> bool {
    let source = context.source();
    let comments = context.comments().leading_dangling_trailing(statement);

    has_skip_comment(comments.trailing, source)
        || comments
            .leading
            .iter()
            .chain(comments.trailing)
            .any(|comment| {
                comment.is_suppression_off_comment(source)
                    || comment.is_suppression_on_comment(source)
            })
}

/// Returns `true` if the formatter prints an empty line between the two statements.
///
/// This asks what the output will look like rather than what the source looks like,
/// because the two can differ: the empty line after a statement that ends in a
/// semicolon, for instance, isn't printed. Breaking a run where the output has no
/// empty line would put the run back together the next time the file is formatted,
/// and every assignment in it would shift.
///
/// The first half of the question is the one the enclosing suite asks to decide the
/// empty lines between two statements; the second is the one the comment formatter
/// asks about the empty lines inside a block of leading comments.
fn separated_by_blank_line(preceding: &Stmt, following: &Stmt, context: &PyFormatContext) -> bool {
    let comments = context.comments();
    let source = context.source();

    let end = comments
        .trailing(preceding)
        .last()
        .map_or(preceding.end(), |comment| comment.slice().end());

    lines_after(end, source) > 1
        || comments
            .leading(following)
            .iter()
            .any(|comment| lines_after(comment.slice().end(), source) > 1)
}
