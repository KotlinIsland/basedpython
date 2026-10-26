//! where the characters of a literal pattern were written
//!
//! a pattern's errors are positions in the text the program holds, which is
//! not the text of the literal that wrote it: `"\t("` holds two characters
//! written as four, and `"a(" "b"` one pattern written as two literals. to
//! point at the character an error is about, the literal is read again the way
//! python reads it, keeping where each character it holds came from

use std::ops::Range;

use ruff_python_ast as ast;
use ruff_python_ast::str::dedented_runs;
use ruff_text_size::{TextRange, TextSize};

/// where in `source` the characters `span` of `pattern` were written, when
/// `pattern` is the value of the string or bytes literal `expr`
///
/// a bytes pattern is `pattern` read as latin-1, one character a byte. `span`
/// is clamped to the pattern, so a span past its end — an error at the end —
/// is its last character. `dedents` says whether `source` is basedpython, which
/// reads a lone triple-quoted string from the text its dedent keeps. `None` for
/// an expression that is not a literal, or a literal whose value does not read
/// as `pattern`, which leaves the caller to point at the expression as a whole
pub(crate) fn pattern_range(
    expr: &ast::Expr,
    source: &str,
    dedents: bool,
    pattern: &str,
    span: Range<usize>,
) -> Option<TextRange> {
    let mut written = Vec::new();
    match expr {
        ast::Expr::StringLiteral(string) => {
            let dedented = string
                .as_single_part_string()
                .filter(|_| dedents)
                .and_then(|part| dedented_runs(source, part.range, part.flags.into()));
            for part in &string.value {
                let raw = part.flags.prefix().is_raw();
                match &dedented {
                    Some(runs) => {
                        for run in runs {
                            read_part(source, *run, raw, false, &mut written)?;
                        }
                    }
                    None => read_part(source, part.content_range(), raw, false, &mut written)?,
                }
            }
        }
        ast::Expr::BytesLiteral(bytes) => {
            for part in &bytes.value {
                read_part(
                    source,
                    part.content_range(),
                    part.flags.prefix().is_raw(),
                    true,
                    &mut written,
                )?;
            }
        }
        _ => return None,
    }

    // the literal has to read as the pattern, character for character, or the
    // reading is not the one python made, and no character of it can be trusted
    let mut held = pattern.chars();
    for character in &written {
        let held = held.next()?;
        if character.value.is_some_and(|value| value != held) {
            return None;
        }
    }
    if held.next().is_some() {
        return None;
    }

    let last = written.len().checked_sub(1)?;
    let start = span.start.min(last);
    let end = span.end.clamp(start + 1, written.len()) - 1;
    Some(TextRange::new(
        written[start].range.start(),
        written[end].range.end(),
    ))
}

/// a character a literal holds and the text that wrote it
struct Written {
    /// the character, or `None` for one written by a name, `\N{BULLET}`, which
    /// is not looked up
    value: Option<char>,
    range: TextRange,
}

/// read the text `content` of a literal as python does: the text between its
/// quotes, or one run of it its dedent keeps, which never splits an escape
fn read_part(
    source: &str,
    content: TextRange,
    raw: bool,
    bytes: bool,
    written: &mut Vec<Written>,
) -> Option<()> {
    let text = source.get(content.start().to_usize()..content.end().to_usize())?;
    let at = |index: usize| content.start() + TextSize::of(&text[..index]);
    let mut characters = text.char_indices().peekable();
    while let Some((start, character)) = characters.next() {
        if raw || character != '\\' {
            written.push(Written {
                value: Some(character),
                range: TextRange::at(at(start), TextSize::of(character)),
            });
            continue;
        }
        let &(_, escaped) = characters.peek()?;
        let value = match escaped {
            // a backslash ending a line joins it to the next, and holds nothing
            '\n' => {
                characters.next();
                continue;
            }
            '\r' => {
                characters.next();
                characters.next_if(|&(_, next)| next == '\n');
                continue;
            }
            '\\' | '\'' | '"' | 'a' | 'b' | 'f' | 'n' | 'r' | 't' | 'v' => {
                characters.next();
                Some(match escaped {
                    'a' => '\x07',
                    'b' => '\x08',
                    'f' => '\x0c',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'v' => '\x0b',
                    quoted => quoted,
                })
            }
            '0'..='7' => {
                let mut code = 0;
                while let Some((_, digit)) =
                    characters.next_if(|&(index, next)| index - start <= 3 && next.is_digit(8))
                {
                    code = code * 8 + digit.to_digit(8)?;
                }
                // a bytes literal keeps the low byte of an octal escape past 0o377
                Some(char::from_u32(if bytes { code & 0xff } else { code })?)
            }
            'x' => {
                characters.next();
                Some(hex(&mut characters, 2)?)
            }
            'u' if !bytes => {
                characters.next();
                Some(hex(&mut characters, 4)?)
            }
            'U' if !bytes => {
                characters.next();
                Some(hex(&mut characters, 8)?)
            }
            'N' if !bytes => {
                characters.next();
                characters.next_if(|&(_, next)| next == '{')?;
                characters.find(|&(_, next)| next == '}')?;
                None
            }
            // any other backslash is the character it is, and so is what follows
            _ => Some('\\'),
        };
        let end = if value == Some('\\') && escaped != '\\' {
            start + 1
        } else {
            characters.peek().map_or(text.len(), |&(index, _)| index)
        };
        written.push(Written {
            value,
            range: TextRange::new(at(start), at(end)),
        });
    }
    Some(())
}

/// the character `digits` hex digits write, consuming them
fn hex(
    characters: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    digits: usize,
) -> Option<char> {
    let mut code = 0;
    for _ in 0..digits {
        let (_, digit) = characters.next()?;
        code = code * 16 + digit.to_digit(16)?;
    }
    char::from_u32(code)
}

#[cfg(test)]
mod tests {
    use ruff_python_ast as ast;
    use ruff_python_parser::parse_expression;

    use super::pattern_range;

    /// the text of `literal` that wrote the characters `span` of the pattern it holds
    fn written(literal: &str, pattern: &str, span: std::ops::Range<usize>) -> Option<String> {
        let parsed = parse_expression(literal).expect("a literal");
        let expr: &ast::Expr = parsed.expr();
        pattern_range(expr, literal, false, pattern, span).map(|range| literal[range].to_string())
    }

    #[test]
    fn a_plain_character_is_itself() {
        assert_eq!(written(r#""ab(c""#, "ab(c", 2..3).as_deref(), Some("("));
    }

    #[test]
    fn an_escape_is_the_text_that_wrote_it() {
        assert_eq!(
            written(r#""\t\x41é\N{BULLET}(""#, "\tAé\u{2022}(", 4..5).as_deref(),
            Some("(")
        );
        assert_eq!(
            written(r#""\t\x41é\N{BULLET}(""#, "\tAé\u{2022}(", 1..4).as_deref(),
            Some(r"\x41é\N{BULLET}")
        );
        assert_eq!(written(r#""\101(""#, "A(", 0..1).as_deref(), Some(r"\101"));
    }

    #[test]
    fn a_backslash_that_escapes_nothing_is_itself() {
        assert_eq!(written(r#""\d+(""#, r"\d+(", 0..2).as_deref(), Some(r"\d"));
    }

    #[test]
    fn a_raw_literal_holds_its_text() {
        assert_eq!(written(r#"r"\t(""#, r"\t(", 2..3).as_deref(), Some("("));
    }

    #[test]
    fn concatenated_literals_are_one_pattern() {
        assert_eq!(
            written(r#"("\t" r"\d" "x{2,1}")"#, "\t\\dx{2,1}", 4..9).as_deref(),
            Some("{2,1}")
        );
    }

    /// basedpython reads a lone triple-quoted string from the text its dedent
    /// keeps, so a character of the pattern is found in that text
    #[test]
    fn a_dedented_literal_is_read_from_what_its_dedent_keeps() {
        let literal = "\"\"\"\n    a\n    (b\n    \"\"\"";
        let parsed = parse_expression(literal).expect("a literal");
        let range = pattern_range(parsed.expr(), literal, true, "a\n(b", 2..3);
        assert_eq!(range.map(|range| &literal[range]), Some("("));
        assert_eq!(range.map(|range| range.start().to_u32()), Some(14));
    }

    #[test]
    fn a_line_continuation_holds_nothing() {
        assert_eq!(written("\"a\\\n(\"", "a(", 1..2).as_deref(), Some("("));
    }

    #[test]
    fn a_bytes_literal_is_read_a_byte_a_character() {
        assert_eq!(
            written(r#"b"\xff(""#, "\u{ff}(", 1..2).as_deref(),
            Some("(")
        );
    }

    #[test]
    fn an_error_at_the_end_is_the_last_character() {
        assert_eq!(written(r#""a\\""#, r"a\", 2..3).as_deref(), Some(r"\\"));
    }

    #[test]
    fn a_literal_that_does_not_hold_the_pattern_is_not_read() {
        assert_eq!(written(r#""ab""#, "abc", 0..1), None);
        assert_eq!(written("name", "abc", 0..1), None);
    }
}
