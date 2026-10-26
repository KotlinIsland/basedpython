//! How an import sorter orders the imports of a file, for an importer to add an import where the
//! sorter would put it.

use std::cmp::Ordering;

/// One import statement, as an import sorter reads it to sort it.
#[derive(Debug, Clone, Copy)]
pub enum SortedImport<'a> {
    /// `import name` or `import name as asname`. isort sorts a statement importing several
    /// modules by its first.
    Straight {
        name: &'a str,
        asname: Option<&'a str>,
        is_lazy: bool,
    },
    /// `from module import member`, with `level` leading dots. isort sorts it by its module and
    /// then by its first member.
    From {
        module: Option<&'a str>,
        level: u32,
        first_member: Option<(&'a str, Option<&'a str>)>,
        is_lazy: bool,
    },
}

impl SortedImport<'_> {
    pub fn is_lazy(&self) -> bool {
        match self {
            Self::Straight { is_lazy, .. } | Self::From { is_lazy, .. } => *is_lazy,
        }
    }
}

/// The order a sorter puts the imports of one file in: which section each import belongs to,
/// where it sorts within it, and the blank lines and headings written between them.
pub trait ImportOrdering {
    /// Where an import sorts in its block. Positions compare as the sorter orders the
    /// statements.
    type Position: Ord;

    /// Where `import` sorts.
    fn position(&self, import: &SortedImport<'_>) -> Self::Position;

    /// The index of the section an import at `position` belongs to, in the sorter's section
    /// order.
    fn section(&self, position: &Self::Position) -> usize;

    /// How many blank lines the sorter writes between a statement at `before` and the statement
    /// at `after` that follows it.
    fn blank_lines_between(&self, before: &Self::Position, after: &Self::Position) -> usize;

    /// The comment the sorter writes above the first import of the section at `position`, if one
    /// is configured.
    fn section_heading(&self, position: &Self::Position) -> Option<&str>;

    /// How the sorter orders two members of one `from` import, each a name and its alias.
    fn compare_members(&self, a: (&str, Option<&str>), b: (&str, Option<&str>)) -> Ordering;

    /// Whether the sorter writes `member` (imported `as asname`, if given) from `module` in a
    /// `from` import of its own rather than among the members of an existing one.
    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool;
}
