//! how an import sorter orders the imports of a file, for an importer to add an import where the
//! sorter would put it

use std::cmp::Ordering;

use ruff_python_codegen::Stylist;

/// one import statement, as an import sorter reads it to sort it
#[derive(Debug, Clone, Copy)]
pub enum SortedImport<'a> {
    /// `import name` or `import name as asname`. isort sorts a statement importing several
    /// modules by its first
    Straight {
        name: &'a str,
        asname: Option<&'a str>,
        is_lazy: bool,
    },
    /// `from module import member`, with `level` leading dots. isort sorts it by its module and
    /// then by its first member
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

/// the order a sorter puts the imports of one file in: which section each import belongs to,
/// where it sorts within it, and the blank lines and headings written between them
pub trait ImportOrdering {
    /// where an import sorts in its block. positions compare as the sorter orders the
    /// statements
    type Position: Ord;

    /// where `import` sorts
    fn position(&self, import: &SortedImport<'_>) -> Self::Position;

    /// the index of the section an import at `position` belongs to, in the sorter's section
    /// order
    fn section(&self, position: &Self::Position) -> usize;

    /// how many blank lines the sorter writes between a statement at `before` and the statement
    /// at `after` that follows it
    fn blank_lines_between(&self, before: &Self::Position, after: &Self::Position) -> usize;

    /// the comment the sorter writes above the first import of the section at `position`, if one
    /// is configured
    fn section_heading(&self, position: &Self::Position) -> Option<&str>;

    /// Whether `comment`, a whole comment line, is a heading the sorter writes above a section.
    /// The sorter rewrites the headings directly above the first import of a block.
    fn is_heading(&self, comment: &str) -> bool;

    /// how the sorter orders two members of one `from` import, each a name and its alias
    fn compare_members(&self, a: (&str, Option<&str>), b: (&str, Option<&str>)) -> Ordering;

    /// whether the sorter writes `member` (imported `as asname`, if given) from `module` in a
    /// `from` import of its own rather than among the members of an existing one
    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool;

    /// How many blank lines the sorter writes between the last import of a block and the
    /// statement after it, a function or class definition or another statement.
    fn blank_lines_after_block(&self, before_definition: bool) -> usize;

    /// `source`, a run of import statements, as the sorter writes them in `stylist`'s style,
    /// wrapped to its line length, ending in a line ending, without the headings of their
    /// sections. `None` when the sorter can't say, as for source with a comment in it
    fn write_statements(&self, source: &str, stylist: &Stylist) -> Option<String>;
}
