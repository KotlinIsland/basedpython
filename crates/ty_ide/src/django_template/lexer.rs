//! a lexer for the django template language
//!
//! the language is not a nested grammar the way python is: a template is a flat
//! sequence of literal text and three kinds of construct — `{{ variable }}`,
//! `{% tag %}` and `{# comment #}`. block structure (`{% if %}` … `{% endif %}`)
//! is imposed *by the tags themselves*, not by the lexer, which is why this
//! module stops at a flat token stream and leaves nesting to [`super::index`].
//!
//! the lexer is written for an editor rather than for a renderer, so it is
//! deliberately tolerant: a construct whose closing delimiter the user has not
//! typed yet still lexes, so that completions fire inside it. no construct ever
//! crosses a newline — django's own lexer can't either — which is what keeps the
//! damage from a stray `{%` to the line it is on.

use std::ops::Range;

use ruff_text_size::{TextLen, TextRange, TextSize};

/// the classification the lexer gives a run of source text
///
/// tokens never overlap and are emitted in source order. everything outside a
/// construct is covered, as [`TokenKind::Text`]; inside one, only the whitespace
/// separating the tokens is left out, since nothing wants to highlight it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum TokenKind {
    /// literal text, outside any construct
    Text,
    /// `{{`, `}}`, `{%`, `%}`, `{#` or `#}`
    Delimiter,
    /// the body of a `{# … #}` or of a `{% comment %}` block
    Comment,
    /// the name directly after `{%`
    TagName,
    /// the name directly after a `|`
    FilterName,
    /// the first segment of a variable path
    Variable,
    /// a segment of a variable path after a `.`
    Attribute,
    /// the name before a `=`, i.e. a tag's keyword argument
    KeywordArgument,
    /// a contextual keyword such as `in`, `as` or `only`
    Keyword,
    /// `True`, `False` or `None`
    BuiltinConstant,
    String,
    Number,
    /// `|`, `:`, `=`, `.` and the comparison operators
    Operator,
    /// text the lexer could not classify
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) range: TextRange,
}

/// which of the three delimiter pairs a construct is written with
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum ConstructKind {
    /// `{{ … }}`
    Variable,
    /// `{% … %}`
    Tag,
    /// `{# … #}`
    Comment,
}

/// one `{{ … }}`, `{% … %}` or `{# … #}` occurrence
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Construct {
    pub(crate) kind: ConstructKind,
    /// the whole construct, delimiters included
    pub(crate) range: TextRange,
    /// the tag name, for a [`ConstructKind::Tag`] whose name the user has typed
    pub(crate) name: Option<TextRange>,
    /// the construct's tokens, as indices into [`Lexed::tokens`]
    ///
    /// the opening and closing delimiters are included, so that highlighting can
    /// work off this slice alone.
    pub(crate) tokens: Range<usize>,
    /// whether the closing delimiter was found
    pub(crate) terminated: bool,
}

impl Construct {
    /// the tag name, or `""` for a construct that has none
    #[cfg(test)]
    pub(crate) fn name<'src>(&self, source: &'src str) -> &'src str {
        self.name.map_or("", |range| &source[range])
    }
}

/// the result of lexing a whole template
#[derive(Debug, Default, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Lexed {
    tokens: Box<[Token]>,
    constructs: Box<[Construct]>,
}

impl Lexed {
    pub(crate) fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    pub(crate) fn constructs(&self) -> &[Construct] {
        &self.constructs
    }

    /// the tokens belonging to `construct`, delimiters included
    pub(crate) fn construct_tokens(&self, construct: &Construct) -> &[Token] {
        &self.tokens[construct.tokens.clone()]
    }

    /// `construct`'s tokens with its opening and closing delimiters dropped
    pub(crate) fn inner_tokens(&self, construct: &Construct) -> &[Token] {
        let tokens = self.construct_tokens(construct);

        let start = usize::from(
            tokens
                .first()
                .is_some_and(|token| token.kind == TokenKind::Delimiter),
        );
        let end = tokens
            .iter()
            .rposition(|token| token.kind != TokenKind::Delimiter)
            .map_or(start, |index| index + 1);

        tokens.get(start..end.max(start)).unwrap_or_default()
    }

    /// the construct `offset` sits in
    ///
    /// a cursor at the very end of an *unterminated* construct is inside it: the
    /// user is still typing `{% extends `. a cursor at the end of a terminated
    /// one is past its closing delimiter and so back in the markup, which is why
    /// `{{ book }}<CURSOR>` offers nothing.
    pub(crate) fn construct_at(&self, offset: TextSize) -> Option<&Construct> {
        let mut index = self
            .constructs
            .partition_point(|construct| construct.range.end() < offset);

        if self
            .constructs
            .get(index)
            .is_some_and(|construct| construct.terminated && construct.range.end() == offset)
        {
            index += 1;
        }

        let construct = self.constructs.get(index)?;
        (construct.range.start() <= offset).then_some(construct)
    }
}

/// the contents of a string literal, its quotes excluded
///
/// a literal whose closing quote the user has not typed yet keeps everything
/// after the opening one, so that it still names whatever it is naming.
pub(crate) fn string_contents(source: &str, range: TextRange) -> TextRange {
    let text = &source[range];
    let Some(quote) = text.chars().next() else {
        return range;
    };
    if !matches!(quote, '"' | '\'') {
        return range;
    }

    let start = range.start() + TextSize::from(1);
    let end = if text.len() > 1 && text.ends_with(quote) {
        range.end() - TextSize::from(1)
    } else {
        range.end()
    };

    TextRange::new(start, end.max(start))
}

/// lex `source` as a django template
pub(crate) fn lex(source: &str) -> Lexed {
    Lexer::new(source).lex()
}

/// the tags whose body is not template source and so is lexed verbatim
///
/// each entry pairs the opening tag with the closing tag that ends the raw run.
/// `comment` bodies are highlighted as comments; `verbatim` bodies are literal
/// output and highlighted as text.
const RAW_BODY_TAGS: &[(&str, &str, TokenKind)] = &[
    ("comment", "endcomment", TokenKind::Comment),
    ("verbatim", "endverbatim", TokenKind::Text),
];

/// names the template language gives a meaning of their own wherever they appear
/// in a tag's arguments
///
/// django has no keyword list — every tag parses its own arguments — so this is
/// the union over the builtin tags of the words that are syntax rather than
/// values. highlighting a word from this list inside a tag that doesn't use it
/// is the one cost, and it is much cheaper than teaching the lexer every tag's
/// individual grammar.
///
/// words django does use as syntax but that read as ordinary variable names —
/// `{% blocktranslate %}`'s `count` and `context`, `{% ifchanged %}`'s `silent` —
/// are deliberately left out: wrongly highlighting `{{ count }}` in every template
/// that has one costs far more than highlighting one argument of one rare tag.
const KEYWORDS: &[&str] = &[
    "and", "as", "asvar", "by", "from", "in", "inline", "is", "not", "off", "on", "only", "or",
    "reversed", "trimmed", "with",
];

struct Lexer<'src> {
    source: &'src str,
    offset: usize,
    tokens: Vec<Token>,
    constructs: Vec<Construct>,
}

impl<'src> Lexer<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            offset: 0,
            tokens: Vec::new(),
            constructs: Vec::new(),
        }
    }

    fn lex(mut self) -> Lexed {
        while self.offset < self.source.len() {
            let Some(open) = self.next_construct_start() else {
                self.push_text_to(self.source.len());
                break;
            };

            self.push_text_to(open);
            self.offset = open;
            self.lex_construct();
        }

        Lexed {
            tokens: self.tokens.into_boxed_slice(),
            constructs: self.constructs.into_boxed_slice(),
        }
    }

    /// the offset of the next `{{`, `{%` or `{#`, searching from the cursor
    fn next_construct_start(&self) -> Option<usize> {
        let rest = &self.source[self.offset..];
        memchr::memchr_iter(b'{', rest.as_bytes())
            .find(|&candidate| {
                matches!(rest.as_bytes().get(candidate + 1), Some(b'{' | b'%' | b'#'))
            })
            .map(|candidate| self.offset + candidate)
    }

    fn lex_construct(&mut self) {
        let start = self.offset;
        let (kind, close) = match self.source.as_bytes()[start + 1] {
            b'{' => (ConstructKind::Variable, "}}"),
            b'%' => (ConstructKind::Tag, "%}"),
            _ => (ConstructKind::Comment, "#}"),
        };

        let first_token = self.tokens.len();
        self.push_token(TokenKind::Delimiter, start, start + 2);
        self.offset = start + 2;

        let (body_end, terminated) = self.find_close(close);

        let name = match kind {
            ConstructKind::Comment => {
                self.push_token(TokenKind::Comment, self.offset, body_end);
                None
            }
            ConstructKind::Variable => {
                self.lex_expression(body_end, false);
                None
            }
            ConstructKind::Tag => self.lex_expression(body_end, true),
        };

        self.offset = body_end;
        if terminated {
            self.push_token(TokenKind::Delimiter, body_end, body_end + close.len());
            self.offset = body_end + close.len();
        }

        self.constructs.push(Construct {
            kind,
            range: self.range(start, self.offset),
            name,
            tokens: first_token..self.tokens.len(),
            terminated,
        });

        if kind == ConstructKind::Tag {
            self.lex_raw_body(name);
        }
    }

    /// the offset of `close` searching from the cursor, and whether it was found
    ///
    /// the search never leaves the line. django's own lexer matches a construct
    /// with a pattern that cannot cross a newline, so a construct that spans one
    /// is not a construct — and searching on regardless would let a stray `{%`
    /// swallow the next tag's `%}` and, with it, that tag.
    fn find_close(&self, close: &str) -> (usize, bool) {
        let line_end = self.line_end();
        match self.source[self.offset..line_end].find(close) {
            Some(index) => (self.offset + index, true),
            None => (line_end, false),
        }
    }

    /// the end of the line the cursor is on, its terminator excluded
    fn line_end(&self) -> usize {
        let rest = &self.source[self.offset..];
        rest.find('\n').map_or(self.source.len(), |index| {
            let end = self.offset + index;
            // keep a `\r\n` terminator out of the construct
            if self.source[..end].ends_with('\r') {
                end - 1
            } else {
                end
            }
        })
    }

    /// lex the inside of a `{{ … }}` or `{% … %}`, up to `end`
    ///
    /// returns the range of the tag name when `expect_tag_name` is set and a name
    /// is actually there.
    fn lex_expression(&mut self, end: usize, expect_tag_name: bool) -> Option<TextRange> {
        let mut tag_name = None;
        // the previous token is re-classified when what follows reveals its role:
        // a `|` makes the next name a filter, a `.` makes it an attribute, and a
        // `=` makes the *preceding* name a keyword argument.
        let mut after_pipe = false;
        let mut after_dot = false;

        while self.offset < end {
            let byte = self.source.as_bytes()[self.offset];

            if byte.is_ascii_whitespace() {
                self.offset += 1;
                after_pipe = false;
                after_dot = false;
                continue;
            }

            let start = self.offset;

            match byte {
                b'"' | b'\'' => {
                    self.lex_string(byte, end);
                    self.push_token(TokenKind::String, start, self.offset);
                }
                b'|' => {
                    self.offset += 1;
                    self.push_token(TokenKind::Operator, start, self.offset);
                    after_pipe = true;
                    continue;
                }
                b'.' => {
                    self.offset += 1;
                    self.push_token(TokenKind::Operator, start, self.offset);
                    after_dot = true;
                    continue;
                }
                b':' | b',' => {
                    self.offset += 1;
                    self.push_token(TokenKind::Operator, start, self.offset);
                }
                b'=' | b'!' | b'<' | b'>' => {
                    self.offset += 1;
                    let two_byte_operator = self.source.as_bytes().get(self.offset) == Some(&b'=')
                        || (byte == b'<' && self.source.as_bytes().get(self.offset) == Some(&b'>'));
                    if two_byte_operator && self.offset < end {
                        self.offset += 1;
                    } else if byte == b'=' {
                        // a lone `=` binds the name before it as a keyword argument
                        self.reclassify_last(TokenKind::Variable, TokenKind::KeywordArgument);
                    }
                    self.push_token(TokenKind::Operator, start, self.offset);
                }
                b'-' | b'0'..=b'9' if !after_dot => {
                    if self.lex_number(end) {
                        self.push_token(TokenKind::Number, start, self.offset);
                    } else {
                        self.offset += 1;
                        self.push_token(TokenKind::Unknown, start, self.offset);
                    }
                }
                _ => {
                    let is_name = self.lex_name(end, after_dot);
                    if !is_name {
                        self.bump_char();
                        self.push_token(TokenKind::Unknown, start, self.offset);
                        after_pipe = false;
                        after_dot = false;
                        continue;
                    }

                    let text = &self.source[start..self.offset];
                    let kind = if expect_tag_name && tag_name.is_none() {
                        tag_name = Some(self.range(start, self.offset));
                        TokenKind::TagName
                    } else if after_pipe {
                        TokenKind::FilterName
                    } else if after_dot {
                        TokenKind::Attribute
                    } else if matches!(text, "True" | "False" | "None") {
                        TokenKind::BuiltinConstant
                    } else if KEYWORDS.contains(&text) {
                        TokenKind::Keyword
                    } else {
                        TokenKind::Variable
                    };
                    self.push_token(kind, start, self.offset);
                }
            }

            after_pipe = false;
            after_dot = false;
        }

        // a construct the user is still typing can leave the cursor past `end`
        self.offset = end;
        tag_name
    }

    /// consume a quoted string, stopping at the closing quote or at `end`
    fn lex_string(&mut self, quote: u8, end: usize) {
        self.offset += 1;
        while self.offset < end {
            match self.source.as_bytes()[self.offset] {
                b'\\' if self.offset + 1 < end => self.offset += 2,
                byte if byte == quote => {
                    self.offset += 1;
                    return;
                }
                _ => self.bump_char(),
            }
        }
    }

    /// consume a numeric literal, returning whether one was actually there
    ///
    /// a `-` that isn't followed by a digit is not a number: django has no
    /// arithmetic, so it is a stray character.
    fn lex_number(&mut self, end: usize) -> bool {
        let start = self.offset;
        if self.source.as_bytes()[self.offset] == b'-' {
            self.offset += 1;
        }

        let digits = self.consume_while(end, u8::is_ascii_digit);
        if !digits {
            self.offset = start;
            return false;
        }

        if self.source.as_bytes().get(self.offset) == Some(&b'.')
            && self
                .source
                .as_bytes()
                .get(self.offset + 1)
                .is_some_and(u8::is_ascii_digit)
        {
            self.offset += 1;
            self.consume_while(end, u8::is_ascii_digit);
        }

        if matches!(self.source.as_bytes().get(self.offset), Some(b'e' | b'E')) {
            let exponent = self.offset;
            self.offset += 1;
            if matches!(self.source.as_bytes().get(self.offset), Some(b'+' | b'-')) {
                self.offset += 1;
            }
            if !self.consume_while(end, u8::is_ascii_digit) {
                self.offset = exponent;
            }
        }

        true
    }

    /// consume a name, returning whether one was actually there
    ///
    /// a name after a `.` may start with a digit, because `{{ row.0 }}` indexes a
    /// sequence. `-` is a name character but never a leading one, so that the
    /// dashed names partials are conventionally given lex as one token.
    fn lex_name(&mut self, end: usize, after_dot: bool) -> bool {
        let start = self.offset;
        let first = self.source[self.offset..end].chars().next();
        let is_start = first.is_some_and(|char| {
            char.is_alphabetic() || char == '_' || (after_dot && char.is_numeric())
        });
        if !is_start {
            return false;
        }

        while self.offset < end {
            let Some(char) = self.source[self.offset..end].chars().next() else {
                break;
            };
            if char.is_alphanumeric() || char == '_' || char == '-' {
                self.offset += char.len_utf8();
            } else {
                break;
            }
        }

        // a trailing `-` belongs to whatever follows, not to the name
        while self.source[start..self.offset].ends_with('-') {
            self.offset -= 1;
        }

        true
    }

    /// lex the verbatim body a `{% comment %}` or `{% verbatim %}` tag opens
    fn lex_raw_body(&mut self, name: Option<TextRange>) {
        let Some(name) = name else { return };
        let Some(&(_, end_tag, body_kind)) = RAW_BODY_TAGS
            .iter()
            .find(|(open, _, _)| *open == &self.source[name])
        else {
            return;
        };

        let body_start = self.offset;
        let Some(body_end) = self.find_end_tag(end_tag) else {
            // an unterminated raw body is *not* swallowed to the end of the file:
            // leaving the rest of the template to lex normally is far more useful
            // while the user is still typing the closing tag.
            return;
        };

        if body_end > body_start {
            self.push_token(body_kind, body_start, body_end);
        }
        self.offset = body_end;
    }

    /// the offset of the `{%` opening the next `{% end… %}` tag named `end_tag`
    fn find_end_tag(&self, end_tag: &str) -> Option<usize> {
        let mut search = self.offset;
        while let Some(index) = self.source[search..].find("{%") {
            let open = search + index;
            let close = self.source[open..].find("%}").map(|index| open + index)?;
            if self.source[open + 2..close].split_whitespace().next() == Some(end_tag) {
                return Some(open);
            }
            search = close + 2;
        }
        None
    }

    fn consume_while(&mut self, end: usize, predicate: impl Fn(&u8) -> bool) -> bool {
        let start = self.offset;
        while self.offset < end && predicate(&self.source.as_bytes()[self.offset]) {
            self.offset += 1;
        }
        self.offset > start
    }

    /// advance one character, so that the cursor never lands inside a code point
    fn bump_char(&mut self) {
        let width = self.source[self.offset..]
            .chars()
            .next()
            .map_or(1, char::len_utf8);
        self.offset += width;
    }

    fn push_text_to(&mut self, end: usize) {
        if end > self.offset {
            self.push_token(TokenKind::Text, self.offset, end);
            self.offset = end;
        }
    }

    fn push_token(&mut self, kind: TokenKind, start: usize, end: usize) {
        if end > start {
            self.tokens.push(Token {
                kind,
                range: self.range(start, end),
            });
        }
    }

    /// re-classify the token just pushed, when a later character reveals its role
    fn reclassify_last(&mut self, from: TokenKind, to: TokenKind) {
        if let Some(last) = self.tokens.last_mut()
            && last.kind == from
        {
            last.kind = to;
        }
    }

    fn range(&self, start: usize, end: usize) -> TextRange {
        debug_assert!(end <= self.source.len());
        TextRange::new(
            TextSize::try_from(start).unwrap_or_else(|_| self.source.text_len()),
            TextSize::try_from(end).unwrap_or_else(|_| self.source.text_len()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ConstructKind, Lexed, TokenKind, lex};

    /// render the token stream as `kind:text` pairs, so a test reads as the
    /// classification of the source rather than as a pile of offsets
    fn classify(source: &str) -> Vec<String> {
        lex(source)
            .tokens()
            .iter()
            .map(|token| format!("{:?}:{}", token.kind, &source[token.range]))
            .collect()
    }

    fn constructs(lexed: &Lexed, source: &str) -> Vec<String> {
        lexed
            .constructs()
            .iter()
            .map(|construct| {
                format!(
                    "{:?}({}):{}",
                    construct.kind,
                    construct.name(source),
                    &source[construct.range]
                )
            })
            .collect()
    }

    #[test]
    fn plain_text_is_one_token() {
        assert_eq!(classify("<p>hello</p>"), ["Text:<p>hello</p>"]);
    }

    #[test]
    fn variable_with_attribute_and_filter() {
        assert_eq!(
            classify("{{ book.author.name|upper }}"),
            [
                "Delimiter:{{",
                "Variable:book",
                "Operator:.",
                "Attribute:author",
                "Operator:.",
                "Attribute:name",
                "Operator:|",
                "FilterName:upper",
                "Delimiter:}}",
            ]
        );
    }

    #[test]
    fn filter_argument_is_a_string() {
        assert_eq!(
            classify(r#"{{ value|default:"none" }}"#),
            [
                "Delimiter:{{",
                "Variable:value",
                "Operator:|",
                "FilterName:default",
                "Operator::",
                r#"String:"none""#,
                "Delimiter:}}",
            ]
        );
    }

    #[test]
    fn numeric_index_after_dot() {
        assert_eq!(
            classify("{{ row.0 }}"),
            [
                "Delimiter:{{",
                "Variable:row",
                "Operator:.",
                "Attribute:0",
                "Delimiter:}}",
            ]
        );
    }

    #[test]
    fn tag_name_is_distinguished_from_its_arguments() {
        assert_eq!(
            classify("{% for book in books reversed %}"),
            [
                "Delimiter:{%",
                "TagName:for",
                "Variable:book",
                "Keyword:in",
                "Variable:books",
                "Keyword:reversed",
                "Delimiter:%}",
            ]
        );
    }

    #[test]
    fn keyword_argument_is_reclassified_by_the_equals_that_follows() {
        assert_eq!(
            classify("{% include 'card.html' with title=book.title only %}"),
            [
                "Delimiter:{%",
                "TagName:include",
                "String:'card.html'",
                "Keyword:with",
                "KeywordArgument:title",
                "Operator:=",
                "Variable:book",
                "Operator:.",
                "Attribute:title",
                "Keyword:only",
                "Delimiter:%}",
            ]
        );
    }

    #[test]
    fn comparison_operators_are_not_keyword_arguments() {
        assert_eq!(
            classify("{% if count >= 10 and other != 3 %}"),
            [
                "Delimiter:{%",
                "TagName:if",
                "Variable:count",
                "Operator:>=",
                "Number:10",
                "Keyword:and",
                "Variable:other",
                "Operator:!=",
                "Number:3",
                "Delimiter:%}",
            ]
        );
    }

    #[test]
    fn builtin_constants_are_their_own_kind() {
        assert_eq!(
            classify("{% if x is not None %}"),
            [
                "Delimiter:{%",
                "TagName:if",
                "Variable:x",
                "Keyword:is",
                "Keyword:not",
                "BuiltinConstant:None",
                "Delimiter:%}",
            ]
        );
    }

    #[test]
    fn negative_and_float_numbers() {
        assert_eq!(
            classify("{{ x|floatformat:-2 }}{{ y|add:1.5 }}"),
            [
                "Delimiter:{{",
                "Variable:x",
                "Operator:|",
                "FilterName:floatformat",
                "Operator::",
                "Number:-2",
                "Delimiter:}}",
                "Delimiter:{{",
                "Variable:y",
                "Operator:|",
                "FilterName:add",
                "Operator::",
                "Number:1.5",
                "Delimiter:}}",
            ]
        );
    }

    #[test]
    fn dashed_names_lex_as_one_token() {
        assert_eq!(
            classify("{% partial comment-item %}"),
            [
                "Delimiter:{%",
                "TagName:partial",
                "Variable:comment-item",
                "Delimiter:%}",
            ]
        );
    }

    #[test]
    fn hash_comment() {
        assert_eq!(
            classify("{# todo: drop this #}"),
            ["Delimiter:{#", "Comment: todo: drop this ", "Delimiter:#}"]
        );
    }

    #[test]
    fn comment_block_body_is_not_template_source() {
        assert_eq!(
            classify("{% comment %}{{ x }}{% endcomment %}"),
            [
                "Delimiter:{%",
                "TagName:comment",
                "Delimiter:%}",
                "Comment:{{ x }}",
                "Delimiter:{%",
                "TagName:endcomment",
                "Delimiter:%}",
            ]
        );
    }

    #[test]
    fn verbatim_block_body_is_literal_text() {
        assert_eq!(
            classify("{% verbatim %}{{ x }}{% endverbatim %}"),
            [
                "Delimiter:{%",
                "TagName:verbatim",
                "Delimiter:%}",
                "Text:{{ x }}",
                "Delimiter:{%",
                "TagName:endverbatim",
                "Delimiter:%}",
            ]
        );
    }

    #[test]
    fn unterminated_construct_stops_at_the_end_of_its_line() {
        let source = "{% extends\n<p>text</p>";
        assert_eq!(
            classify(source),
            ["Delimiter:{%", "TagName:extends", "Text:\n<p>text</p>",]
        );

        let lexed = lex(source);
        assert_eq!(constructs(&lexed, source), ["Tag(extends):{% extends"]);
        assert!(!lexed.constructs()[0].terminated);
    }

    #[test]
    fn an_unterminated_construct_does_not_reach_a_later_constructs_delimiter() {
        // the `%}` below belongs to the `{% block %}`, and taking it would erase
        // that tag from the template — which is what the user sees the moment
        // they type `{%` on a line above one
        let source = "{% extends 'base.html'\n<p>hi</p>\n{% block content %}{% endblock %}";
        let lexed = lex(source);

        assert_eq!(
            constructs(&lexed, source),
            [
                "Tag(extends):{% extends 'base.html'",
                "Tag(block):{% block content %}",
                "Tag(endblock):{% endblock %}",
            ]
        );
        assert!(!lexed.constructs()[0].terminated);
    }

    #[test]
    fn a_construct_never_crosses_a_newline() {
        // django's own lexer matches with a pattern that cannot cross one, so a
        // `{{` and a `}}` on different lines are two runs of literal text
        let source = "{{ book\n.title }}";
        let lexed = lex(source);

        assert_eq!(lexed.constructs().len(), 1);
        assert!(!lexed.constructs()[0].terminated);
        assert_eq!(&source[lexed.constructs()[0].range], "{{ book");
    }

    #[test]
    fn a_carriage_return_is_not_part_of_an_unterminated_construct() {
        let source = "{% extends\r\n<p>text</p>";
        let lexed = lex(source);
        assert_eq!(&source[lexed.constructs()[0].range], "{% extends");
    }

    #[test]
    fn unterminated_raw_body_does_not_swallow_the_file() {
        // the closing tag is missing, so the body must keep lexing as template
        // source — otherwise everything the user types after `{% comment %}`
        // would go dark until they type `{% endcomment %}`
        assert_eq!(
            classify("{% comment %}{{ x }}"),
            [
                "Delimiter:{%",
                "TagName:comment",
                "Delimiter:%}",
                "Delimiter:{{",
                "Variable:x",
                "Delimiter:}}",
            ]
        );
    }

    #[test]
    fn constructs_record_their_kind_and_name() {
        let source = "a{{ x }}b{% if y %}c{# d #}";
        let lexed = lex(source);
        assert_eq!(
            constructs(&lexed, source),
            [
                "Variable():{{ x }}",
                "Tag(if):{% if y %}",
                "Comment():{# d #}"
            ]
        );
    }

    #[test]
    fn construct_at_finds_the_construct_around_an_offset() {
        //                0        1
        //                12345678901
        let source = "a{{ x }}bb{% if y %}";
        let lexed = lex(source);

        let at = |offset: usize| {
            lexed
                .construct_at(ruff_text_size::TextSize::try_from(offset).unwrap())
                .map(|construct| construct.name(source).to_string())
        };

        assert_eq!(at(0), None, "before any construct");
        assert_eq!(at(1), Some(String::new()), "the `{{` itself");
        assert_eq!(at(7), Some(String::new()), "inside the closing braces");
        assert_eq!(at(8), None, "past the closing braces");
        assert_eq!(at(9), None, "the literal text between them");
        assert_eq!(at(14), Some("if".to_string()));
        assert_eq!(
            at(19),
            Some("if".to_string()),
            "inside the closing delimiter"
        );
        assert_eq!(at(20), None, "past the closing delimiter");
    }

    #[test]
    fn the_end_of_an_unterminated_construct_is_still_inside_it() {
        // the user is mid-way through typing it, and this is where completions
        // have to fire
        let source = "{% extends ";
        let lexed = lex(source);
        let end = ruff_text_size::TextSize::try_from(source.len()).unwrap();

        assert_eq!(
            lexed
                .construct_at(end)
                .map(|construct| construct.name(source)),
            Some("extends")
        );
    }

    #[test]
    fn adjacent_constructs_do_not_overlap_at_their_boundary() {
        let source = "{{ a }}{{ b }}";
        let lexed = lex(source);

        let at = |offset: usize| {
            lexed
                .construct_at(ruff_text_size::TextSize::try_from(offset).unwrap())
                .map(|construct| &source[construct.range])
        };

        assert_eq!(at(7), Some("{{ b }}"), "the second one, not the first");
    }

    #[test]
    fn a_lone_brace_is_text() {
        assert_eq!(classify("{ x }"), ["Text:{ x }"]);
    }

    #[test]
    fn non_ascii_text_keeps_char_boundaries() {
        // the lexer indexes by byte but must never split a code point
        assert_eq!(
            classify("héllo {{ wörld }}"),
            [
                "Text:héllo ",
                "Delimiter:{{",
                "Variable:wörld",
                "Delimiter:}}",
            ]
        );
    }

    #[test]
    fn tokens_are_ordered_and_never_overlap() {
        let source = "a{{ x.y|f:'s' }}b{% if 1 %}{# c #}{% verbatim %}v{% endverbatim %}";
        let lexed = lex(source);

        let mut covered = 0usize;
        for token in lexed.tokens() {
            assert!(
                usize::from(token.range.start()) >= covered,
                "{token:?} overlaps the token before it"
            );
            assert!(token.range.end() > token.range.start(), "empty {token:?}");
            covered = usize::from(token.range.end());
        }
        assert_eq!(covered, source.len(), "the tail of the source is uncovered");
    }

    #[test]
    fn only_whitespace_inside_a_construct_is_left_uncovered() {
        let source = "a{{ x }}b";
        let lexed = lex(source);

        let mut covered = 0usize;
        for token in lexed.tokens() {
            let gap = &source[covered..usize::from(token.range.start())];
            assert!(gap.trim().is_empty(), "`{gap}` belongs to no token");
            covered = usize::from(token.range.end());
        }
    }

    #[test]
    fn construct_kinds_round_trip() {
        let lexed = lex("{{ a }}");
        assert_eq!(lexed.constructs()[0].kind, ConstructKind::Variable);
        assert!(lexed.constructs()[0].terminated);
        assert!(
            lexed
                .construct_tokens(&lexed.constructs()[0])
                .iter()
                .any(|token| token.kind == TokenKind::Variable)
        );
    }
}
