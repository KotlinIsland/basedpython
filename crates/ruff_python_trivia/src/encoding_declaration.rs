//! [PEP 263](https://peps.python.org/pep-0263/) encoding declarations
//!
//! read over bytes rather than text, since a source that declares its encoding may not be utf-8
//! yet. the syntax is ascii, so a byte scan reads it exactly

use ruff_text_size::{TextRange, TextSize};

/// the encoding a python source declares, as python's tokenizer reads it: a comment that is the
/// whole of its line and holds `coding[:=][ \t]*[-\w.]+`, on the first line or on the second when
/// the first could not be code, which is to say it is blank or a comment. a utf-8 byte order mark
/// ahead of the first line is not part of it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodingDeclaration<'a> {
    /// the encoding the source names, as written
    pub name: &'a str,
    /// where `name` is written
    pub name_range: TextRange,
    /// the line the declaration is on, its line ending included
    pub line: TextRange,
}

/// the encoding declaration of `source`, if it has one
pub fn encoding_declaration(source: &[u8]) -> Option<EncodingDeclaration<'_>> {
    let start = if source.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    let first = line_at(source, start);
    if let Some(declaration) = declaration_in(source, first) {
        return Some(declaration);
    }
    let could_be_code = source[first.start..first.content_end]
        .iter()
        .find(|byte| !is_blank(**byte))
        .is_some_and(|byte| *byte != b'#');
    if could_be_code || first.end == first.content_end {
        return None;
    }
    declaration_in(source, line_at(source, first.end))
}

#[derive(Clone, Copy)]
struct Line {
    start: usize,
    /// where the line's ending starts
    content_end: usize,
    /// past the line's ending
    end: usize,
}

/// the line of `source` starting at `start`, ended by `\n`, `\r\n` or `\r`, as python reads a
/// source
fn line_at(source: &[u8], start: usize) -> Line {
    let rest = &source[start..];
    let Some(at) = rest.iter().position(|byte| matches!(byte, b'\n' | b'\r')) else {
        return Line {
            start,
            content_end: source.len(),
            end: source.len(),
        };
    };
    let content_end = start + at;
    let ending = if rest[at..].starts_with(b"\r\n") {
        2
    } else {
        1
    };
    Line {
        start,
        content_end,
        end: content_end + ending,
    }
}

fn declaration_in(source: &[u8], line: Line) -> Option<EncodingDeclaration<'_>> {
    let text = &source[line.start..line.content_end];
    let hash = text.iter().position(|byte| !is_blank(*byte))?;
    if text[hash] != b'#' {
        return None;
    }
    let mut from = hash + 1;
    while let Some(found) = find(&text[from..], b"coding") {
        let after = from + found + b"coding".len();
        from += found + 1;
        if !matches!(text.get(after), Some(b':' | b'=')) {
            continue;
        }
        let name_start = after
            + 1
            + text[after + 1..]
                .iter()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
        let name_len = text[name_start..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            .count();
        if name_len == 0 {
            continue;
        }
        let name = std::str::from_utf8(&text[name_start..name_start + name_len]).ok()?;
        let name_start = line.start + name_start;
        return Some(EncodingDeclaration {
            name,
            name_range: TextRange::new(offset(name_start)?, offset(name_start + name_len)?),
            line: TextRange::new(offset(line.start)?, offset(line.end)?),
        });
    }
    None
}

/// the whitespace python skips ahead of a comment
fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\x0c')
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn offset(at: usize) -> Option<TextSize> {
    TextSize::try_from(at).ok()
}

#[cfg(test)]
mod tests {
    use super::encoding_declaration;

    fn name(source: &str) -> Option<&str> {
        encoding_declaration(source.as_bytes()).map(|declaration| declaration.name)
    }

    #[test]
    fn on_the_first_line() {
        assert_eq!(name("# -*- coding: latin-1 -*-\nx = 1\n"), Some("latin-1"));
        assert_eq!(name("# vim: set fileencoding=utf-8 :\n"), Some("utf-8"));
        assert_eq!(name("\x0c  # coding:\tascii"), Some("ascii"));
    }

    #[test]
    fn on_the_second_line_only_below_a_blank_line_or_a_comment() {
        assert_eq!(
            name("#!/usr/bin/env python\n# coding=cp1252\n"),
            Some("cp1252")
        );
        assert_eq!(name("\r\n# coding=cp1252\n"), Some("cp1252"));
        assert_eq!(name("x = 1\n# coding: latin-1\n"), None);
        assert_eq!(name("#\n#\n# coding: latin-1\n"), None);
    }

    #[test]
    fn only_in_a_comment_that_is_the_whole_line() {
        assert_eq!(name("x = 1  # coding: latin-1\n"), None);
        assert_eq!(name("'# coding: latin-1'\n"), None);
    }

    #[test]
    fn any_coding_the_comment_spells_out() {
        assert_eq!(name("# the coding, coding: latin-1\n"), Some("latin-1"));
        assert_eq!(name("# coding: \n"), None);
        assert_eq!(name("# coding latin-1\n"), None);
    }

    #[test]
    fn after_a_byte_order_mark() {
        let declaration = encoding_declaration("\u{feff}# coding: utf-8\r\nx\n".as_bytes());
        assert_eq!(
            declaration.map(|declaration| (declaration.name, declaration.line)),
            Some(("utf-8", ruff_text_size::TextRange::new(3.into(), 20.into())))
        );
    }

    #[test]
    fn where_the_name_is() {
        let source = "#!python\n# coding=latin-1\n";
        let declaration = encoding_declaration(source.as_bytes());
        assert_eq!(
            declaration.map(|declaration| &source[declaration.name_range]),
            Some("latin-1")
        );
    }
}
