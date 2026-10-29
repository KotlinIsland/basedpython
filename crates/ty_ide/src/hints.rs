use ruff_db::files::File;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;
use ty_python_semantic::types::ide_support::{
    UnreachableKind, unreachable_ranges, unused_bindings,
};

use crate::Db;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Hint {
    pub range: TextRange,
    pub kind: HintKind,
}

impl Hint {
    pub fn message(&self) -> String {
        self.kind.message()
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum HintKind {
    UnusedBinding(Name),
    UnreachableCode(UnreachableKind),
}

impl HintKind {
    /// the code that tells this hint apart from the others on the wire: `unreachable-code` for
    /// unreachable code, and none for an unused name
    ///
    /// it names the hint and no rule. the type checker does not report unreachable code, so no
    /// configuration selects it, no `# ty: ignore[...]` comment suppresses it — one naming it
    /// is an unknown rule — and `by/explainRule` has nothing to say about it
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::UnusedBinding(_) => None,
            Self::UnreachableCode(_) => Some("unreachable-code"),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::UnusedBinding(name) => format!("`{name}` is unused"),
            Self::UnreachableCode(UnreachableKind::Unconditional) => {
                "Code is always unreachable".to_owned()
            }
            Self::UnreachableCode(UnreachableKind::CurrentAnalysis) => {
                "Code is unreachable\nThis may depend on your current environment and settings"
                    .to_owned()
            }
        }
    }
}

pub fn hints(db: &dyn Db, file: File) -> Vec<Hint> {
    if !ty_project::should_check_semantics(db, file) {
        return Vec::new();
    }

    let file = db.program_file(file);
    let unreachable = unreachable_ranges(db, file);

    let mut hints = unused_bindings(db, file)
        .iter()
        // Avoid a narrower unused-binding hint inside code that is already reported as unreachable.
        .filter(|binding| {
            unreachable.is_empty()
                || !unreachable
                    .iter()
                    .any(|range| range.range.contains_range(binding.range))
        })
        .map(|binding| Hint {
            range: binding.range,
            kind: HintKind::UnusedBinding(binding.name.clone()),
        })
        .collect::<Vec<_>>();

    hints.extend(unreachable.iter().map(|range| Hint {
        range: range.range,
        kind: HintKind::UnreachableCode(range.kind),
    }));

    hints.sort_unstable_by(|left, right| {
        (left.range.start(), left.range.end(), &left.kind).cmp(&(
            right.range.start(),
            right.range.end(),
            &right.kind,
        ))
    });

    hints
}

/// the ranges [`hints`] reports as unreachable code: the ranges an editor greys out, which is why
/// an editor that would draw a semantic token over the grey may ask for the tokens inside them to
/// be left out, or marked
#[derive(Debug, Default)]
pub struct UnreachableCode {
    /// in source order, none overlapping another
    ranges: Vec<TextRange>,
}

impl UnreachableCode {
    pub(crate) fn ranges(&self) -> &[TextRange] {
        &self.ranges
    }

    /// whether all of `range` lies inside one range of unreachable code
    pub fn contains(&self, range: TextRange) -> bool {
        let after = self
            .ranges
            .partition_point(|unreachable| unreachable.start() <= range.start());
        after
            .checked_sub(1)
            .is_some_and(|index| self.ranges[index].contains_range(range))
    }
}

/// the code [`hints`] reports as unreachable in `file`
pub fn unreachable_code(db: &dyn Db, file: File) -> UnreachableCode {
    if !ty_project::should_check_semantics(db, file) {
        return UnreachableCode::default();
    }
    UnreachableCode {
        ranges: unreachable_ranges(db, db.program_file(file))
            .iter()
            .map(|unreachable| unreachable.range)
            .collect(),
    }
}
