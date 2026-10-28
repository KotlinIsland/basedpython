//! a file a line merge left conflict markers in, as `git merge-file` writes them, read back into
//! what each version says in each conflict and written again with conflicts replaced
//!
//! ```text
//! import os
//! <<<<<<< HEAD
//! import json
//! ||||||| 3453403
//! =======
//! import sys
//! >>>>>>> feature
//! ```
//!
//! the lines outside the conflicts are what the merge took from both sides. the three versions
//! [`ConflictedText::versions`] answers are those lines with each conflict's own section in its
//! place, so that a conflict is at known lines in each of them and everything around it is the
//! merge's: what [`crate::resolve_merge_conflicts`] takes

use ruff_text_size::{TextRange, TextSize};

use crate::ConflictLines;

/// a side of a merge's conflicts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictSide {
    Ours,
    Theirs,
}

/// a text with conflict markers in it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedText {
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// lines the merge took without a conflict
    Merged(String),
    Conflict(Conflict),
}

/// one conflict, each part with its marker line, which ends with its line ending
#[derive(Debug, Clone, PartialEq, Eq)]
struct Conflict {
    ours_marker: String,
    ours: String,
    /// the base's marker and lines, which a merge only writes in the `diff3` and `zdiff3` styles
    base: Option<(String, String)>,
    separator: String,
    theirs: String,
    theirs_marker: String,
}

/// the three versions of a [`ConflictedText`], each the merged lines with its own section of
/// each conflict, and where each conflict is in them
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictVersions {
    pub base: String,
    pub ours: String,
    pub theirs: String,
    pub conflicts: Vec<ConflictLines>,
}

impl ConflictedText {
    /// `text` read with markers of `marker_size` characters, `None` where a conflict is opened
    /// and not closed, in order, by markers of that size
    pub fn parse(text: &str, marker_size: usize) -> Option<Self> {
        let marker = |line: &str, character: char| {
            let rest = line.strip_prefix(&*character.to_string().repeat(marker_size))?;
            // a longer run is not this marker, whatever it starts with
            (rest.is_empty() || rest.starts_with([' ', '\n', '\r'])).then_some(())
        };
        let mut pieces = Vec::new();
        let mut merged = String::new();
        let mut lines = text.split_inclusive('\n');
        while let Some(line) = lines.next() {
            if marker(line, '<').is_none() {
                merged.push_str(line);
                continue;
            }
            if !merged.is_empty() {
                pieces.push(Piece::Merged(std::mem::take(&mut merged)));
            }
            let mut conflict = Conflict {
                ours_marker: line.to_string(),
                ours: String::new(),
                base: None,
                separator: String::new(),
                theirs: String::new(),
                theirs_marker: String::new(),
            };
            // 0 ours, 1 base, 2 theirs
            let mut part = 0;
            loop {
                let line = lines.next()?;
                if part == 0 && marker(line, '|').is_some() {
                    conflict.base = Some((line.to_string(), String::new()));
                    part = 1;
                } else if part < 2 && marker(line, '=').is_some() {
                    conflict.separator = line.to_string();
                    part = 2;
                } else if part == 2 && marker(line, '>').is_some() {
                    conflict.theirs_marker = line.to_string();
                    break;
                } else if marker(line, '<').is_some() {
                    return None;
                } else {
                    match (part, &mut conflict.base) {
                        (0, _) => conflict.ours.push_str(line),
                        (1, Some((_, base))) => base.push_str(line),
                        _ => conflict.theirs.push_str(line),
                    }
                }
            }
            pieces.push(Piece::Conflict(conflict));
        }
        if !merged.is_empty() {
            pieces.push(Piece::Merged(merged));
        }
        Some(Self { pieces })
    }

    /// how many conflicts there are
    pub(crate) fn conflicts(&self) -> usize {
        self.pieces
            .iter()
            .filter(|piece| matches!(piece, Piece::Conflict(_)))
            .count()
    }

    /// whether every conflict says what the base had, as the `diff3` and `zdiff3` styles write
    pub fn has_base(&self) -> bool {
        self.pieces.iter().all(|piece| match piece {
            Piece::Merged(_) => true,
            Piece::Conflict(conflict) => conflict.base.is_some(),
        })
    }

    /// the three versions, `None` where a conflict does not say what the base had
    pub fn versions(&self) -> Option<ConflictVersions> {
        let mut versions = ConflictVersions {
            base: String::new(),
            ours: String::new(),
            theirs: String::new(),
            conflicts: Vec::new(),
        };
        let lines = |text: &str| text.matches('\n').count();
        for piece in &self.pieces {
            match piece {
                Piece::Merged(text) => {
                    versions.base.push_str(text);
                    versions.ours.push_str(text);
                    versions.theirs.push_str(text);
                }
                Piece::Conflict(conflict) => {
                    let (_, base) = conflict.base.as_ref()?;
                    let start = (
                        lines(&versions.base),
                        lines(&versions.ours),
                        lines(&versions.theirs),
                    );
                    versions.base.push_str(base);
                    versions.ours.push_str(&conflict.ours);
                    versions.theirs.push_str(&conflict.theirs);
                    versions.conflicts.push(ConflictLines {
                        base: start.0..lines(&versions.base),
                        ours: start.1..lines(&versions.ours),
                        theirs: start.2..lines(&versions.theirs),
                    });
                }
            }
        }
        Some(versions)
    }

    /// the text with each conflict replaced by `side`'s section of it: what the file would be
    /// were every conflict resolved to that side
    pub(crate) fn side(&self, side: ConflictSide) -> String {
        self.pieces
            .iter()
            .map(|piece| match (piece, side) {
                (Piece::Merged(text), _) => text.as_str(),
                (Piece::Conflict(conflict), ConflictSide::Ours) => &conflict.ours,
                (Piece::Conflict(conflict), ConflictSide::Theirs) => &conflict.theirs,
            })
            .collect()
    }

    /// `range` of [`Self::side`]'s ours, as a range of this text: `None` for one that is not
    /// all in lines the merge took without a conflict, which nothing outside a conflict may
    /// change
    pub(crate) fn range_from_ours(&self, range: TextRange) -> Option<TextRange> {
        let mut ours = TextSize::default();
        let mut here = TextSize::default();
        for piece in &self.pieces {
            match piece {
                Piece::Merged(text) => {
                    let end = ours + TextSize::of(text.as_str());
                    if ours <= range.start() && range.end() <= end {
                        return Some(range - ours + here);
                    }
                    ours = end;
                    here += TextSize::of(text.as_str());
                }
                Piece::Conflict(conflict) => {
                    ours += TextSize::of(conflict.ours.as_str());
                    here += TextSize::of(Self::conflict_text(conflict).as_str());
                }
            }
        }
        None
    }

    /// `offset` of [`Self::side`]'s ours, as an offset of this text, or for one inside a
    /// conflict, the start of the conflict's first marker
    pub(crate) fn offset_from_ours(&self, offset: TextSize) -> TextSize {
        if let Some(range) = self.range_from_ours(TextRange::empty(offset)) {
            return range.start();
        }
        let mut ours = TextSize::default();
        let mut here = TextSize::default();
        for piece in &self.pieces {
            let (ours_len, here_len) = match piece {
                Piece::Merged(text) => (TextSize::of(text.as_str()), TextSize::of(text.as_str())),
                Piece::Conflict(conflict) => (
                    TextSize::of(conflict.ours.as_str()),
                    TextSize::of(Self::conflict_text(conflict).as_str()),
                ),
            };
            if matches!(piece, Piece::Conflict(_)) && offset <= ours + ours_len {
                return here;
            }
            ours += ours_len;
            here += here_len;
        }
        here
    }

    /// a conflict as it is written, markers and all
    fn conflict_text(conflict: &Conflict) -> String {
        let mut out = String::new();
        out.push_str(&conflict.ours_marker);
        out.push_str(&conflict.ours);
        if let Some((marker, base)) = &conflict.base {
            out.push_str(marker);
            out.push_str(base);
        }
        out.push_str(&conflict.separator);
        out.push_str(&conflict.theirs);
        out.push_str(&conflict.theirs_marker);
        out
    }

    /// the text with each conflict replaced by its resolution, in order, and those without one
    /// written as they were, with the base's section where `with_base` says and it has one
    pub fn write(&self, resolutions: &[Option<String>], with_base: bool) -> String {
        let mut resolutions = resolutions.iter();
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Merged(text) => out.push_str(text),
                Piece::Conflict(conflict) => {
                    if let Some(Some(resolution)) = resolutions.next() {
                        out.push_str(resolution);
                        continue;
                    }
                    if with_base {
                        out.push_str(&Self::conflict_text(conflict));
                    } else {
                        out.push_str(&conflict.ours_marker);
                        out.push_str(&conflict.ours);
                        out.push_str(&conflict.separator);
                        out.push_str(&conflict.theirs);
                        out.push_str(&conflict.theirs_marker);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use ruff_text_size::{TextRange, TextSize};

    use super::{ConflictSide, ConflictedText};
    use crate::ConflictLines;

    const CONFLICTED: &str = "\
a
<<<<<<< HEAD
B1
||||||| 3453403
b
=======
B2
B3
>>>>>>> feature
c
";

    #[test]
    fn each_version_has_its_own_section_of_each_conflict() {
        let text = ConflictedText::parse(CONFLICTED, 7).expect("well formed");
        assert_eq!(text.conflicts(), 1);
        assert!(text.has_base());
        let versions = text.versions().expect("a base section");
        assert_eq!(versions.base, "a\nb\nc\n");
        assert_eq!(versions.ours, "a\nB1\nc\n");
        assert_eq!(versions.theirs, "a\nB2\nB3\nc\n");
        assert_eq!(
            versions.conflicts,
            vec![ConflictLines {
                base: 1..2,
                ours: 1..2,
                theirs: 1..3
            }]
        );
    }

    #[test]
    fn a_conflict_is_written_back_as_it_was_or_as_resolved() {
        let text = ConflictedText::parse(CONFLICTED, 7).expect("well formed");
        assert_eq!(text.write(&[None], true), CONFLICTED);
        assert_eq!(
            text.write(&[None], false),
            "a\n<<<<<<< HEAD\nB1\n=======\nB2\nB3\n>>>>>>> feature\nc\n"
        );
        assert_eq!(text.write(&[Some("B\n".to_string())], true), "a\nB\nc\n");
    }

    #[test]
    fn a_range_outside_the_conflicts_of_one_side_is_the_same_text_here() {
        let text = ConflictedText::parse(CONFLICTED, 7).expect("well formed");
        let ours = text.side(ConflictSide::Ours);
        assert_eq!(ours, "a\nB1\nc\n");
        assert_eq!(text.side(ConflictSide::Theirs), "a\nB2\nB3\nc\n");
        let c = TextRange::new(TextSize::new(5), TextSize::new(7));
        assert_eq!(&ours[c], "c\n");
        assert_eq!(
            text.range_from_ours(c).map(|range| &CONFLICTED[range]),
            Some("c\n")
        );
        // `B1` is in the conflict, and so is a range reaching into it
        assert_eq!(
            text.range_from_ours(TextRange::new(TextSize::new(2), TextSize::new(4))),
            None
        );
        assert_eq!(
            text.range_from_ours(TextRange::new(TextSize::new(0), TextSize::new(4))),
            None
        );
        assert_eq!(text.offset_from_ours(TextSize::new(3)), TextSize::new(2));
        assert_eq!(text.offset_from_ours(TextSize::new(2)), TextSize::new(2));
    }

    #[test]
    fn markers_of_another_size_are_lines_like_any_other() {
        let text = ConflictedText::parse(CONFLICTED, 9).expect("no conflict");
        assert_eq!(text.conflicts(), 0);
        assert_eq!(text.write(&[], true), CONFLICTED);
        assert_eq!(ConflictedText::parse("<<<<<<< HEAD\nx\n", 7), None);
    }
}
