//! deletes specific stray comments that no longer make sense in the basedpython
//! typeshed (upstream notes about mypy/pyright quirks, placeholder musings, ...)
//!
//! each entry is a distinctive substring; any whole-line comment containing it
//! is removed. keep the substrings specific enough that they cannot match an
//! unrelated comment

use std::path::Path;

use ruff_python_ast::ModModule;
use ruff_python_ast::token::TokenKind;
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

/// substrings identifying comments to delete
const MARKERS: &[&str] = &[
    "generic more on vibes",
    "need to use Container[Any] instead of Container[_T_co]",
    // stale once `delete-dead-typevars` removes the slice typevars it labels
    "Type variables for slice",
];

pub struct DeleteStrayComments;

impl Patch for DeleteStrayComments {
    fn name(&self) -> &'static str {
        "delete-stray-comments"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        for token in parsed.tokens() {
            if token.kind() == TokenKind::Comment {
                let text = &source[token.range()];
                if MARKERS.iter().any(|m| text.contains(m)) {
                    edits.push(delete_own_line(token.range(), source));
                }
            }
        }
        edits
    }
}

/// deletion edit covering the physical line `range` sits on plus its trailing
/// newline
fn delete_own_line(range: TextRange, source: &str) -> Edit {
    let bytes = source.as_bytes();
    let mut start = range.start().to_usize();
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = range.end().to_usize();
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() {
        end += 1;
    }
    Edit {
        start,
        end,
        replacement: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = DeleteStrayComments.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn deletes_marked_comment_keeps_neighbours() {
        let src = "\
protocol Container[in ContainerT = Any]:
    # This is generic more on vibes than anything else
    abstract def __contains__(self, x: ContainerT, /) -> bool
";
        let expected = "\
protocol Container[in ContainerT = Any]:
    abstract def __contains__(self, x: ContainerT, /) -> bool
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn deletes_only_the_marked_line() {
        let src = "\
class C:
    # Note: need to use Container[Any] instead of Container[_T_co] to ensure covariance.
    # Implement Sized (but don't have it as a base class).
    abstract def __len__(self) -> int
";
        let expected = "\
class C:
    # Implement Sized (but don't have it as a base class).
    abstract def __len__(self) -> int
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn leaves_unmarked_comments() {
        let src = "# an ordinary comment\nx: int\n";
        assert_eq!(run(src), src);
    }
}
