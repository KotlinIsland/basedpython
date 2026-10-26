//! The parts of a file a diff may ignore when asked to ignore imports and formatting.
//!
//! Imports are every import statement, wherever it is. Formatting is the text python's tokenizer
//! discards without its being significant: whitespace between two tokens of a logical line, the
//! line breaks and indentation of a line continued inside brackets or after a backslash, and blank
//! lines. The indentation a logical line starts with is not formatting, since it says which block
//! the line is in, and nor is anything inside an f-string or a t-string, where `f"{x = }"` prints
//! the spaces around its `=`. Two versions that differ only in what is ignored here tokenize to
//! the same tokens.

use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{PySourceType, Stmt};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextRange, TextSize};

/// What a diff ignoring imports and formatting ignores in one text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffIgnoredRanges {
    /// Each import statement.
    pub imports: Vec<TextRange>,
    /// Each run of text that is only formatting, in order.
    pub formatting: Vec<TextRange>,
}

pub fn diff_ignored_ranges(text: &str, source_type: PySourceType) -> DiffIgnoredRanges {
    let parsed = parse_unchecked_source(text, source_type);

    let mut imports = Imports(Vec::new());
    imports.visit_body(parsed.suite());

    let mut formatting: Vec<TextRange> = Vec::new();
    let mut push = |range: TextRange| {
        if range.is_empty() {
            return;
        }
        match formatting.last_mut() {
            Some(last) if last.end() == range.start() => *last = last.cover(range),
            _ => formatting.push(range),
        }
    };
    let mut brackets = 0u32;
    let mut strings = 0u32;
    let mut previous: Option<(TokenKind, TextSize)> = None;
    for token in parsed.tokens() {
        let kind = token.kind();
        let gap_start = previous.map_or(TextSize::default(), |(_, end)| end);
        // the whitespace before a token starts its line's indentation where the line before it
        // ended a logical line, or the file starts
        let starts_logical_line = match previous {
            None => true,
            Some((TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent, _)) => true,
            Some((TokenKind::NonLogicalNewline, _)) => brackets == 0,
            _ => false,
        };
        if strings == 0 && !starts_logical_line {
            push(TextRange::new(gap_start, token.start()));
        }
        match kind {
            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace => brackets += 1,
            TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace => {
                brackets = brackets.saturating_sub(1);
            }
            TokenKind::FStringStart | TokenKind::TStringStart => strings += 1,
            TokenKind::FStringEnd | TokenKind::TStringEnd => strings = strings.saturating_sub(1),
            TokenKind::NonLogicalNewline if strings == 0 => {
                // a line break inside brackets, or a line with nothing on it
                let blank = matches!(
                    previous,
                    None | Some((TokenKind::Newline | TokenKind::NonLogicalNewline, _))
                );
                if brackets > 0 || blank {
                    push(token.range());
                }
            }
            _ => {}
        }
        previous = Some((kind, token.end()));
    }

    DiffIgnoredRanges {
        imports: imports.0,
        formatting,
    }
}

struct Imports(Vec<TextRange>);

impl<'a> StatementVisitor<'a> for Imports {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if statement.is_import_stmt() || statement.is_import_from_stmt() {
            self.0.push(statement.range());
        }
        walk_stmt(self, statement);
    }
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::PySourceType;

    use super::diff_ignored_ranges;

    /// `text` with what is ignored as formatting written as `~`, and imports as `I`.
    fn marked(text: &str) -> String {
        let ignored = diff_ignored_ranges(text, PySourceType::BasedPython);
        let mut out: Vec<char> = text.chars().collect();
        let index = |offset: usize| text[..offset].chars().count();
        for range in ignored.imports {
            let marked = index(range.start().to_usize())..index(range.end().to_usize());
            for character in &mut out[marked] {
                *character = 'I';
            }
        }
        for range in ignored.formatting {
            let marked = index(range.start().to_usize())..index(range.end().to_usize());
            for character in &mut out[marked] {
                *character = if *character == '\n' { '$' } else { '~' };
            }
        }
        out.into_iter().collect()
    }

    #[test]
    fn whitespace_inside_a_line_and_brackets_is_formatting_and_indentation_is_not() {
        insta::assert_snapshot!(marked("import os\nfrom a import (\n    b,\n)\n\n\ndef f( x ):\n    y = [\n        1 ,  2,\n    ]\n    return f\"{ x = }\"\n"), @r#"
        IIIIII~II
        IIII~I~IIIIII~I~~~~~II~I
        $$def~f(~x~):
            y~=~[$~~~~~~~~1~,~~2,$~~~~]
            return~f"{ x = }"
        "#);
    }
}
