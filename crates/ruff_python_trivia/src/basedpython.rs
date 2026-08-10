//! basedpython-specific trivia analysis.

/// How a triple-quoted basedpython string's body is auto-dedented.
///
/// See [`dedent_triple_quoted_body`].
#[derive(Debug, PartialEq, Eq)]
pub enum TripleQuotedDedent<'a> {
    /// The body is left exactly as written, either because it does not have the
    /// shape a dedent applies to or because its lines share no indentation.
    Unchanged,

    /// The body's content lines all start with `indent`, which is stripped from
    /// each of them. `content` is the body without its opening newline and
    /// without the whitespace-only line the closing quotes sit on.
    Dedents { content: &'a str, indent: &'a str },

    /// The closing quotes are indented past the content, so there is no
    /// consistent depth to dedent to.
    ClosingOverIndented,
}

/// Analyse the body of a triple-quoted string — the text between the quotes —
/// for basedpython's [auto-dedent](https://docs.basedpython.org/features/dedent-strings).
///
/// A body is dedented when it opens on the line after the quotes and the closing
/// quotes sit on a line of their own: together those make the closing line's
/// indentation the depth the content is written at.
pub fn dedent_triple_quoted_body(body: &str) -> TripleQuotedDedent<'_> {
    let Some(body) = body.strip_prefix('\n') else {
        return TripleQuotedDedent::Unchanged;
    };

    let Some((content, closing_indent)) = body.rsplit_once('\n') else {
        return TripleQuotedDedent::Unchanged;
    };
    if !closing_indent.chars().all(|c| c == ' ' || c == '\t') {
        return TripleQuotedDedent::Unchanged;
    }

    let lines = || content.split('\n').filter(|line| !line.trim().is_empty());
    let indent = lines()
        .map(leading_whitespace)
        .reduce(common_prefix)
        .unwrap_or_default();

    // a closing quote indented past the actual content of the string has no
    // consistent dedent interpretation — the caller refuses rather than silently
    // producing a misaligned literal
    if lines().next().is_some() && closing_indent.len() > indent.len() {
        return TripleQuotedDedent::ClosingOverIndented;
    }

    if indent.is_empty() {
        return TripleQuotedDedent::Unchanged;
    }

    TripleQuotedDedent::Dedents { content, indent }
}

fn leading_whitespace(line: &str) -> &str {
    let end = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    &line[..end]
}

fn common_prefix<'a>(a: &'a str, b: &'a str) -> &'a str {
    let end = a
        .char_indices()
        .zip(b.chars())
        .find_map(|((index, x), y)| (x != y).then_some(index))
        .unwrap_or(a.len().min(b.len()));
    &a[..end]
}

#[cfg(test)]
mod tests {
    use super::{TripleQuotedDedent, dedent_triple_quoted_body};

    #[test]
    fn dedents_indented_body() {
        assert_eq!(
            dedent_triple_quoted_body("\n    hello\n    "),
            TripleQuotedDedent::Dedents {
                content: "    hello",
                indent: "    ",
            }
        );
    }

    #[test]
    fn dedents_to_the_shallowest_line() {
        assert_eq!(
            dedent_triple_quoted_body("\n    a\n  b\n"),
            TripleQuotedDedent::Dedents {
                content: "    a\n  b",
                indent: "  ",
            }
        );
    }

    #[test]
    fn blank_lines_do_not_count_towards_the_indent() {
        assert_eq!(
            dedent_triple_quoted_body("\n    a\n\n    b\n    "),
            TripleQuotedDedent::Dedents {
                content: "    a\n\n    b",
                indent: "    ",
            }
        );
    }

    #[test]
    fn body_opening_on_the_quote_line_is_unchanged() {
        assert_eq!(
            dedent_triple_quoted_body("one line.\n    more\n    "),
            TripleQuotedDedent::Unchanged
        );
    }

    #[test]
    fn closing_quotes_sharing_a_line_are_unchanged() {
        assert_eq!(
            dedent_triple_quoted_body("\n    a\n    b"),
            TripleQuotedDedent::Unchanged
        );
    }

    #[test]
    fn unindented_body_is_unchanged() {
        assert_eq!(
            dedent_triple_quoted_body("\nhello\n"),
            TripleQuotedDedent::Unchanged
        );
    }

    #[test]
    fn whitespace_only_body_is_unchanged() {
        assert_eq!(
            dedent_triple_quoted_body("\n    "),
            TripleQuotedDedent::Unchanged
        );
        assert_eq!(
            dedent_triple_quoted_body("\n\n    "),
            TripleQuotedDedent::Unchanged
        );
    }

    #[test]
    fn closing_quotes_past_the_content() {
        assert_eq!(
            dedent_triple_quoted_body("\n  asdf\n    "),
            TripleQuotedDedent::ClosingOverIndented
        );
        assert_eq!(
            dedent_triple_quoted_body("\nasdf\n    "),
            TripleQuotedDedent::ClosingOverIndented
        );
    }
}
