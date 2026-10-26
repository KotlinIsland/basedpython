use aho_corasick::{AhoCorasick, AhoCorasickKind, Anchored, Input, MatchKind, StartKind};
use std::fmt;
use std::sync::LazyLock;

use ruff_python_trivia::basedpython::{TripleQuotedDedent, dedent_triple_quoted_body};
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::{AnyStringFlags, StringFlags};

/// basedpython: the runs of the string written at `range` of `source` that its
/// [auto-dedent](https://docs.basedpython.org/features/dedent-strings) keeps, as
/// ranges of `source` in order, or `None` when it is not dedented or cannot be
/// (see [`dedent`])
pub fn dedented_runs(
    source: &str,
    range: TextRange,
    flags: AnyStringFlags,
) -> Option<Vec<TextRange>> {
    match dedent(source, range, flags) {
        Dedent::Runs { runs, .. } => Some(runs),
        Dedent::Unchanged | Dedent::Invalid(_) => None,
    }
}

/// basedpython: how the string written at `range` of `source` is
/// [auto-dedented](https://docs.basedpython.org/features/dedent-strings), the one
/// rule the parser, the checker and the transpiler all read a dedented string by
///
/// only a lone triple-quoted literal is dedented, never one of several adjacent
/// ones, which is for the caller to know; a bytes literal never is, whatever its quotes
pub fn dedent(source: &str, range: TextRange, flags: AnyStringFlags) -> Dedent {
    if !flags.is_triple_quoted() || flags.is_unclosed() || flags.is_byte_string() {
        return Dedent::Unchanged;
    }
    let body = TextRange::new(
        range.start() + flags.opener_len(),
        range.end() - flags.closer_len(),
    );
    let Some(text) = source.get(body.start().to_usize()..body.end().to_usize()) else {
        return Dedent::Unchanged;
    };
    let dedent = dedent_triple_quoted_body(text);
    if dedent == TripleQuotedDedent::ClosingOverIndented {
        return Dedent::Invalid(DedentError {
            kind: DedentErrorKind::ClosingOverIndented {
                quote: flags.quote_str(),
            },
            range: TextRange::new(body.end(), range.end()),
        });
    }
    let TripleQuotedDedent::Dedents { indent, .. } = dedent else {
        return Dedent::Unchanged;
    };
    let indent = indent.text_len();
    let Some(runs) = dedent.runs() else {
        return Dedent::Unchanged;
    };
    let runs: Vec<TextRange> = runs.into_iter().map(|run| run + body.start()).collect();
    // a `\` ending the last line continues the string past the newline after it, which is
    // the newline the dedent drops, so there is nothing left for it to continue onto. a raw
    // string has no line continuation: its backslash is text
    if !flags.prefix().is_raw()
        && let Some(last) = runs.last()
        && let trailing = source[*last].len() - source[*last].trim_end_matches('\\').len()
        && trailing % 2 == 1
    {
        return Dedent::Invalid(DedentError {
            kind: DedentErrorKind::LineContinuationAtEnd,
            range: TextRange::at(last.end() - '\\'.text_len(), '\\'.text_len()),
        });
    }
    Dedent::Runs { runs, indent }
}

/// basedpython: how a string is [auto-dedented](dedent)
#[derive(Debug, PartialEq, Eq)]
pub enum Dedent {
    /// the string is not dedented, and is read as written
    Unchanged,
    /// the string is dedented by stripping `indent` from each of its lines
    Runs {
        /// the runs of the source the dedent keeps, in order: the text the string is read
        /// from is these runs, one after another
        runs: Vec<TextRange>,
        /// the length of the indent stripped from each line that is not blank
        indent: TextSize,
    },
    /// the string has the shape of a dedented one, but no reading of it as one
    Invalid(DedentError),
}

/// basedpython: why a string that has the shape of a dedented one cannot be read as one
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedentError {
    kind: DedentErrorKind,
    range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DedentErrorKind {
    ClosingOverIndented { quote: &'static str },
    LineContinuationAtEnd,
}

impl Ranged for DedentError {
    fn range(&self) -> TextRange {
        self.range
    }
}

impl fmt::Display for DedentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            DedentErrorKind::ClosingOverIndented { quote } => write!(
                f,
                "closing `{quote}` is indented more than the content of the triple-quoted string"
            ),
            DedentErrorKind::LineContinuationAtEnd => f.write_str(
                "a line continuation cannot end the last line of a dedented string, \
                 whose newline the dedent drops",
            ),
        }
    }
}

/// Enumeration of the two kinds of quotes that can be used
/// for Python string/f/t-string/bytestring literals
#[derive(Debug, Default, Copy, Clone, Hash, PartialEq, Eq, is_macro::Is)]
pub enum Quote {
    /// E.g. `'`
    Single,
    /// E.g. `"`
    #[default]
    Double,
}

impl Quote {
    #[inline]
    pub const fn as_char(self) -> char {
        match self {
            Self::Single => '\'',
            Self::Double => '"',
        }
    }

    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "'",
            Self::Double => "\"",
        }
    }

    #[must_use]
    #[inline]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Single => Self::Double,
            Self::Double => Self::Single,
        }
    }

    #[inline]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Single => b'\'',
            Self::Double => b'"',
        }
    }
}

impl fmt::Display for Quote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_char())
    }
}

impl TryFrom<char> for Quote {
    type Error = ();

    fn try_from(value: char) -> Result<Self, Self::Error> {
        match value {
            '\'' => Ok(Quote::Single),
            '"' => Ok(Quote::Double),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TripleQuotes {
    Yes,
    No,
}

impl TripleQuotes {
    #[must_use]
    pub const fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }

    #[must_use]
    pub const fn is_no(self) -> bool {
        matches!(self, Self::No)
    }
}

/// Includes all permutations of `r`, `u`, `f`, and `fr` (`ur` is invalid, as is `uf`). This
/// includes all possible orders, and all possible casings, for both single and triple quotes.
///
/// See: <https://docs.python.org/3/reference/lexical_analysis.html#string-and-bytes-literals>
#[rustfmt::skip]
const TRIPLE_QUOTE_STR_PREFIXES: &[&str] = &[
    "FR\"\"\"",
    "Fr\"\"\"",
    "fR\"\"\"",
    "fr\"\"\"",
    "RF\"\"\"",
    "Rf\"\"\"",
    "rF\"\"\"",
    "rf\"\"\"",
    "FR'''",
    "Fr'''",
    "fR'''",
    "fr'''",
    "RF'''",
    "Rf'''",
    "rF'''",
    "rf'''",
    "R\"\"\"",
    "r\"\"\"",
    "R'''",
    "r'''",
    "F\"\"\"",
    "f\"\"\"",
    "F'''",
    "f'''",
    "U\"\"\"",
    "u\"\"\"",
    "U'''",
    "u'''",
    "\"\"\"",
    "'''",
];

#[rustfmt::skip]
const SINGLE_QUOTE_STR_PREFIXES: &[&str] = &[
    "FR\"",
    "Fr\"",
    "fR\"",
    "fr\"",
    "RF\"",
    "Rf\"",
    "rF\"",
    "rf\"",
    "FR'",
    "Fr'",
    "fR'",
    "fr'",
    "RF'",
    "Rf'",
    "rF'",
    "rf'",
    "R\"",
    "r\"",
    "R'",
    "r'",
    "F\"",
    "f\"",
    "F'",
    "f'",
    "U\"",
    "u\"",
    "U'",
    "u'",
    "\"",
    "'",
];

/// Includes all permutations of `b` and `rb`. This includes all possible orders, and all possible
/// casings, for both single and triple quotes.
///
/// See: <https://docs.python.org/3/reference/lexical_analysis.html#string-and-bytes-literals>
#[rustfmt::skip]
pub(crate) const TRIPLE_QUOTE_BYTE_PREFIXES: &[&str] = &[
    "BR\"\"\"",
    "Br\"\"\"",
    "bR\"\"\"",
    "br\"\"\"",
    "RB\"\"\"",
    "Rb\"\"\"",
    "rB\"\"\"",
    "rb\"\"\"",
    "BR'''",
    "Br'''",
    "bR'''",
    "br'''",
    "RB'''",
    "Rb'''",
    "rB'''",
    "rb'''",
    "B\"\"\"",
    "b\"\"\"",
    "B'''",
    "b'''",
];

#[rustfmt::skip]
pub(crate) const SINGLE_QUOTE_BYTE_PREFIXES: &[&str] = &[
    "BR\"",
    "Br\"",
    "bR\"",
    "br\"",
    "RB\"",
    "Rb\"",
    "rB\"",
    "rb\"",
    "BR'",
    "Br'",
    "bR'",
    "br'",
    "RB'",
    "Rb'",
    "rB'",
    "rb'",
    "B\"",
    "b\"",
    "B'",
    "b'",
];

/// Includes all permutations of `t` and `rt`. This includes all possible orders, and all possible
/// casings, for both single and triple quotes.
///
/// See: <https://docs.python.org/3/reference/lexical_analysis.html#string-and-bytes-literals>
#[rustfmt::skip]
pub(crate) const TRIPLE_QUOTE_TEMPLATE_PREFIXES: &[&str] = &[
    "TR\"\"\"",
    "Tr\"\"\"",
    "tR\"\"\"",
    "tr\"\"\"",
    "RT\"\"\"",
    "Rt\"\"\"",
    "rT\"\"\"",
    "rt\"\"\"",
    "TR'''",
    "Tr'''",
    "tR'''",
    "tr'''",
    "RT'''",
    "Rt'''",
    "rT'''",
    "rt'''",
    "T\"\"\"",
    "t\"\"\"",
    "T'''",
    "t'''",
];

#[rustfmt::skip]
pub(crate) const SINGLE_QUOTE_TEMPLATE_PREFIXES: &[&str] = &[
    "TR\"",
    "Tr\"",
    "tR\"",
    "tr\"",
    "RT\"",
    "Rt\"",
    "rT\"",
    "rt\"",
    "TR'",
    "Tr'",
    "tR'",
    "tr'",
    "RT'",
    "Rt'",
    "rT'",
    "rt'",
    "T\"",
    "t\"",
    "T'",
    "t'",
];

/// Strip the leading and trailing quotes from a string.
/// Assumes that the string is a valid string literal, but does not verify that the string
/// is a "simple" string literal (i.e., that it does not contain any implicit concatenations).
pub fn raw_contents(contents: &str) -> Option<&str> {
    let range = raw_contents_range(contents)?;

    Some(&contents[range])
}

fn raw_contents_range(contents: &str) -> Option<TextRange> {
    let leading_quote_str = leading_quote(contents)?;
    let trailing_quote_str = trailing_quote(contents)?;

    Some(TextRange::new(
        leading_quote_str.text_len(),
        contents.text_len() - trailing_quote_str.text_len(),
    ))
}

/// An [`AhoCorasick`] matcher for string, template, and bytes literal prefixes.
static PREFIX_MATCHER: LazyLock<AhoCorasick> = LazyLock::new(|| {
    AhoCorasick::builder()
        .start_kind(StartKind::Anchored)
        .match_kind(MatchKind::LeftmostLongest)
        .kind(Some(AhoCorasickKind::DFA))
        .build(
            TRIPLE_QUOTE_STR_PREFIXES
                .iter()
                .chain(TRIPLE_QUOTE_BYTE_PREFIXES)
                .chain(TRIPLE_QUOTE_TEMPLATE_PREFIXES)
                .chain(SINGLE_QUOTE_STR_PREFIXES)
                .chain(SINGLE_QUOTE_BYTE_PREFIXES)
                .chain(SINGLE_QUOTE_TEMPLATE_PREFIXES),
        )
        .unwrap()
});

/// Return the leading quote for a string, template, or bytes literal (e.g., `"""`).
pub fn leading_quote(content: &str) -> Option<&str> {
    let mat = PREFIX_MATCHER.find(Input::new(content).anchored(Anchored::Yes))?;
    Some(&content[mat.start()..mat.end()])
}

/// Return the trailing quote string for a string, template, or bytes literal (e.g., `"""`).
pub fn trailing_quote(content: &str) -> Option<&str> {
    if content.ends_with("'''") {
        Some("'''")
    } else if content.ends_with("\"\"\"") {
        Some("\"\"\"")
    } else if content.ends_with('\'') {
        Some("'")
    } else if content.ends_with('\"') {
        Some("\"")
    } else {
        None
    }
}

/// Return `true` if the string is a triple-quote string or byte prefix.
pub fn is_triple_quote(content: &str) -> bool {
    TRIPLE_QUOTE_STR_PREFIXES.contains(&content)
        || TRIPLE_QUOTE_BYTE_PREFIXES.contains(&content)
        || TRIPLE_QUOTE_TEMPLATE_PREFIXES.contains(&content)
}

#[cfg(test)]
mod tests {
    use super::{
        SINGLE_QUOTE_BYTE_PREFIXES, SINGLE_QUOTE_STR_PREFIXES, SINGLE_QUOTE_TEMPLATE_PREFIXES,
        TRIPLE_QUOTE_BYTE_PREFIXES, TRIPLE_QUOTE_STR_PREFIXES, TRIPLE_QUOTE_TEMPLATE_PREFIXES,
    };

    #[test]
    fn prefix_uniqueness() {
        let prefixes = TRIPLE_QUOTE_STR_PREFIXES
            .iter()
            .chain(TRIPLE_QUOTE_BYTE_PREFIXES)
            .chain(TRIPLE_QUOTE_TEMPLATE_PREFIXES)
            .chain(SINGLE_QUOTE_STR_PREFIXES)
            .chain(SINGLE_QUOTE_BYTE_PREFIXES)
            .chain(SINGLE_QUOTE_TEMPLATE_PREFIXES)
            .collect::<Vec<_>>();
        for (i, prefix_i) in prefixes.iter().enumerate() {
            for (j, prefix_j) in prefixes.iter().enumerate() {
                if i > j {
                    assert!(
                        !prefix_i.starts_with(*prefix_j),
                        "Prefixes are not unique: {prefix_i} starts with {prefix_j}",
                    );
                }
            }
        }
    }
}
