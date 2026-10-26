//! a parser for python (`sre_parse`) regular-expression syntax
//!
//! we only need the *shape* of a pattern — how many capture groups it has, what
//! they are named, and which of them must have participated when the pattern as
//! a whole matched — so every atom collapses to a marker and only the grouping
//! spine is built into a tree. optionality then falls out of one traversal of
//! that tree, rather than the character-level state machine a linear scan needs
//!
//! a construct we cannot model yields [`PatternAnalysis::Unknown`], never an
//! error: reporting a valid pattern as invalid is much worse than declining to
//! refine it

use std::ops::Range;

use ruff_python_ast::PythonVersion;
use ruff_python_ast::name::Name;
use ruff_python_ast::str::TripleQuotes;
use ruff_python_literal::escape::UnicodeEscape;
use rustc_hash::FxHashMap;

use super::character_names::{ALIASED_SINCE, NAMED_SINCE};

/// the characters `sre_parse` treats as ignorable whitespace under `re.VERBOSE`
const VERBOSE_WHITESPACE: [char; 6] = [' ', '\t', '\n', '\r', '\x0b', '\x0c'];

/// how deeply groups may nest before we give up
///
/// both the parser and the optionality traversal recurse per group, so this
/// bounds the stack for a pathological pattern such as `((((((…`
const MAX_DEPTH: u32 = 100;

/// one capture group of a pattern, in group-number order
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedGroup {
    /// the `(?P<name>…)` name, if the group has one
    pub(crate) name: Option<Name>,
    /// whether the group must have participated in *every* successful match
    ///
    /// `false` means the group may be unset even when the overall match
    /// succeeds, so its value is `None` at runtime
    pub(crate) definitely_set: bool,
}

/// a pattern that `re.compile` would reject, carrying python's own message
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegexError {
    message: String,
    /// the characters of the pattern the error is about: the one at the position
    /// python names, or the whole of a construct that is wrong as a whole, a
    /// `{2,1}` or a group's name
    span: Range<usize>,
}

impl RegexError {
    fn new(message: impl std::fmt::Display, position: usize) -> Self {
        Self::spanning(message, position, position..position + 1)
    }

    fn spanning(message: impl std::fmt::Display, position: usize, span: Range<usize>) -> Self {
        Self {
            message: format!("{message} at position {position}"),
            span,
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// the characters of the pattern the error is about, which end past the
    /// pattern for an error at its end, as `unexpected end of pattern` is
    pub(crate) fn span(&self) -> Range<usize> {
        self.span.clone()
    }
}

/// what we were able to learn about a pattern
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatternAnalysis {
    /// the capture groups of a pattern we fully understood
    Groups(Vec<ParsedGroup>),
    /// the pattern does not compile
    Invalid(RegexError),
    /// the pattern is beyond what we model — no refinement, and no diagnostic
    Unknown,
}

/// which `re` reads a pattern: the python version's, for a `str` or a `bytes`
/// pattern
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dialect {
    pub(crate) version: PythonVersion,
    /// a `bytes` pattern has no `\u`, `\U` or `\N` escape
    pub(crate) bytes: bool,
}

/// analyse `pattern` as python regular-expression source
///
/// `verbose` is the `re.VERBOSE`/`re.X` flag from the call site; a pattern that
/// turns verbose mode on itself with a global `(?x)` is re-parsed from the
/// start, since the flag changes how the text before it tokenizes
pub(crate) fn analyze(pattern: &str, verbose: bool, dialect: Dialect) -> PatternAnalysis {
    match Parser::new(pattern, verbose, dialect).run() {
        Ok(groups) => PatternAnalysis::Groups(groups),
        Err(Failure::RestartVerbose) if !verbose => match Parser::new(pattern, true, dialect).run()
        {
            Ok(groups) => PatternAnalysis::Groups(groups),
            Err(Failure::Invalid(error)) => PatternAnalysis::Invalid(error),
            Err(_) => PatternAnalysis::Unknown,
        },
        Err(Failure::Invalid(error)) => PatternAnalysis::Invalid(error),
        Err(_) => PatternAnalysis::Unknown,
    }
}

/// the grouping spine of a pattern
#[derive(Debug)]
enum Node {
    /// anything that isn't a group: a literal, a character set, an escape, an
    /// anchor. it contributes no groups, so its contents are irrelevant
    Atom,
    Concat(Vec<Node>),
    /// with more than one branch, a group inside any branch is only set when
    /// that branch is the one that matched
    Alternate(Vec<Node>),
    Repeat {
        node: Box<Node>,
        /// whether the repeat may match zero times
        optional: bool,
    },
    /// a subtree whose groups can never be guaranteed: the body of a negative
    /// lookaround (which by definition did not match) or of a conditional
    Uncertain(Box<Node>),
    Group {
        /// the 1-based group number, for a capturing group
        index: Option<u32>,
        node: Box<Node>,
    },
}

enum Failure {
    Invalid(RegexError),
    /// the pattern enabled verbose mode itself; re-parse it from the start
    RestartVerbose,
    /// beyond what we model
    Unknown,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    verbose: bool,
    dialect: Dialect,
    depth: u32,
    group_count: u32,
    /// whether each group, by number less one, has been closed, which a
    /// backreference to it needs
    closed: Vec<bool>,
    /// inside a lookbehind, the number of the first group opened in it, which no
    /// backreference in it may refer to
    lookbehind_groups: Option<u32>,
    /// group name to the number it was first bound to, for redefinition checks
    names: FxHashMap<Name, u32>,
    /// group number to name, built as groups are opened
    group_names: Vec<Option<Name>>,
}

impl Parser {
    fn new(pattern: &str, verbose: bool, dialect: Dialect) -> Self {
        Self {
            chars: pattern.chars().collect(),
            pos: 0,
            verbose,
            dialect,
            depth: 0,
            group_count: 0,
            closed: Vec::new(),
            lookbehind_groups: None,
            names: FxHashMap::default(),
            group_names: Vec::new(),
        }
    }

    fn run(mut self) -> Result<Vec<ParsedGroup>, Failure> {
        let root = self.parse_alternation()?;
        if let Some(char) = self.peek() {
            // `parse_concat` only stops early on `)`; anything else means we
            // failed to consume the pattern, which is our bug rather than the
            // pattern's
            return Err(if char == ')' {
                Failure::Invalid(RegexError::new("unbalanced parenthesis", self.pos))
            } else {
                Failure::Unknown
            });
        }

        let mut definitely_set = vec![false; self.group_names.len()];
        collect_certainty(&root, true, &mut definitely_set);
        Ok(self
            .group_names
            .into_iter()
            .zip(definitely_set)
            .map(|(name, definitely_set)| ParsedGroup {
                name,
                definitely_set,
            })
            .collect())
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn eat(&mut self, char: char) -> bool {
        if self.peek() == Some(char) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// skip the whitespace and `#` comments that verbose mode ignores
    fn skip_ignorable(&mut self) {
        if !self.verbose {
            return;
        }
        while let Some(char) = self.peek() {
            if VERBOSE_WHITESPACE.contains(&char) {
                self.pos += 1;
            } else if char == '#' {
                while self.peek().is_some_and(|char| char != '\n') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn parse_alternation(&mut self) -> Result<Node, Failure> {
        let mut branches = vec![self.parse_concat()?];
        while self.eat('|') {
            branches.push(self.parse_concat()?);
        }
        Ok(Node::Alternate(branches))
    }

    fn parse_concat(&mut self) -> Result<Node, Failure> {
        let mut items: Vec<Node> = Vec::new();
        loop {
            self.skip_ignorable();
            let Some(char) = self.peek() else { break };
            match char {
                '|' | ')' => break,
                '*' | '+' | '?' => {}
                '{' if self.at_quantifier() => {}
                _ => {
                    // a comment or a global flag setting contributes no item
                    if let Some(node) = self.parse_atom()? {
                        items.push(node);
                    }
                    continue;
                }
            }

            // a quantifier binds to the last real item, seeing straight through
            // anything that contributed none
            let position = self.pos;
            let Some(previous) = items.pop() else {
                return Err(Failure::Invalid(RegexError::new(
                    "nothing to repeat",
                    position,
                )));
            };
            if matches!(previous, Node::Repeat { .. }) {
                return Err(Failure::Invalid(RegexError::new(
                    "multiple repeat",
                    position,
                )));
            }
            let optional = self.parse_quantifier()?;
            items.push(Node::Repeat {
                node: Box::new(previous),
                optional,
            });
        }
        Ok(Node::Concat(items))
    }

    /// consume the quantifier at the cursor, returning whether it can match zero
    /// times
    ///
    /// only called with the cursor already known to be on one
    fn parse_quantifier(&mut self) -> Result<bool, Failure> {
        let optional = match self.peek() {
            Some('*' | '?') => {
                self.pos += 1;
                true
            }
            Some('+') => {
                self.pos += 1;
                false
            }
            Some('{') => {
                let open = self.pos;
                let Some(quantifier) = self.braced_quantifier() else {
                    return Err(Failure::Unknown);
                };
                self.pos += quantifier.length;
                if quantifier.min > quantifier.max {
                    return Err(Failure::Invalid(RegexError::spanning(
                        "min repeat greater than max repeat",
                        open + 1,
                        open..self.pos,
                    )));
                }
                quantifier.min == 0
            }
            _ => return Err(Failure::Unknown),
        };
        // a lazy (`?`) or possessive (`+`) suffix changes only match semantics.
        // possessive quantifiers need python 3.11, so accepting one on an older
        // target means a missed error rather than a spurious one
        if matches!(self.peek(), Some('?' | '+')) {
            self.pos += 1;
        }
        Ok(optional)
    }

    /// whether a `{m,n}` quantifier starts at the cursor — otherwise `{` is an
    /// ordinary literal, as in python
    fn at_quantifier(&self) -> bool {
        self.braced_quantifier().is_some()
    }

    /// the `{m,n}` at the cursor, if the text there is a quantifier at all
    fn braced_quantifier(&self) -> Option<BracedQuantifier> {
        // `{}` is spelled out as a literal in `sre_parse` before anything else
        if self.peek_at(1) == Some('}') {
            return None;
        }

        let mut offset = 1;
        let min = self.digits_at(&mut offset);
        let (max, has_comma) = if self.peek_at(offset) == Some(',') {
            offset += 1;
            (self.digits_at(&mut offset), true)
        } else {
            (None, false)
        };
        if self.peek_at(offset) != Some('}') {
            return None;
        }
        offset += 1;

        Some(BracedQuantifier {
            length: offset,
            min: min.unwrap_or(0),
            // without a comma the bound is exact; `{m,}` and `{,n}` leave the
            // missing side unbounded
            max: if has_comma {
                max.unwrap_or(u32::MAX)
            } else {
                min.unwrap_or(0)
            },
        })
    }

    /// read the run of digits at `offset`, advancing it past them
    fn digits_at(&self, offset: &mut usize) -> Option<u32> {
        let start = *offset;
        let mut value: u32 = 0;
        while let Some(digit) = self.peek_at(*offset).and_then(|char| char.to_digit(10)) {
            value = value.saturating_mul(10).saturating_add(digit);
            *offset += 1;
        }
        (*offset > start).then_some(value)
    }

    /// consume one atom, yielding `None` for source that contributes no item at
    /// all (a `(?#…)` comment or a `(?i)` global flag setting)
    fn parse_atom(&mut self) -> Result<Option<Node>, Failure> {
        match self.peek() {
            Some('(') => self.parse_group(),
            Some('[') => {
                self.parse_character_set()?;
                Ok(Some(Node::Atom))
            }
            Some('\\') => {
                self.parse_escape(false)?;
                Ok(Some(Node::Atom))
            }
            Some(_) => {
                self.pos += 1;
                Ok(Some(Node::Atom))
            }
            None => Ok(None),
        }
    }

    /// consume an escape, `in_class` saying whether it is written in a `[…]`
    ///
    /// the rules are `sre_parse`'s `_escape` and `_class_escape`: an ASCII letter
    /// or digit that is not an escape is an error, rather than the letter, so
    /// that a later python can give it a meaning
    fn parse_escape(&mut self, in_class: bool) -> Result<(), Failure> {
        let start = self.pos;
        self.pos += 1;
        let Some(escaped) = self.peek() else {
            return Err(Failure::Invalid(RegexError::new(
                "bad escape (end of pattern)",
                start,
            )));
        };
        self.pos += 1;
        let text = !self.dialect.bytes;
        match escaped {
            // the escapes of special characters, `\b` a backspace in a set
            '\\' | 'a' | 'b' | 'f' | 'n' | 'r' | 't' | 'v' => Ok(()),
            // the character classes, which a set may hold
            'd' | 'D' | 's' | 'S' | 'w' | 'W' => Ok(()),
            // the anchors, which it may not
            'A' | 'B' | 'Z' if !in_class => Ok(()),
            'z' if !in_class && self.dialect.version >= PythonVersion::PY314 => Ok(()),
            'x' => self.hex_escape(start, 2),
            'u' if text => self.hex_escape(start, 4),
            'U' if text => {
                self.hex_escape(start, 8)?;
                // `chr` refuses a code point past unicode's last
                if u32::from_str_radix(&self.text(start + 2..self.pos), 16)
                    .is_ok_and(|code| code <= 0x0010_ffff)
                {
                    Ok(())
                } else {
                    Err(self.bad_escape(start))
                }
            }
            'N' if text && self.dialect.version >= PythonVersion::PY38 => self.named_escape(start),
            '0'..='7' if in_class => {
                self.eat_octal_digits(2);
                self.octal_in_range(start)
            }
            '0' => {
                self.eat_octal_digits(2);
                Ok(())
            }
            '1'..='9' if !in_class => self.group_reference(start),
            escaped if escaped.is_ascii_alphanumeric() => Err(self.bad_escape(start)),
            _ => Ok(()),
        }
    }

    fn bad_escape(&self, start: usize) -> Failure {
        Failure::Invalid(RegexError::spanning(
            format_args!("bad escape {}", self.text(start..self.pos)),
            start,
            start..self.pos,
        ))
    }

    fn text(&self, range: Range<usize>) -> String {
        self.chars[range].iter().collect()
    }

    /// the `digits` hex digits of a `\x`, `\u` or `\U` escape starting at `start`
    fn hex_escape(&mut self, start: usize, digits: usize) -> Result<(), Failure> {
        let first = self.pos;
        while self.pos - first < digits && self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            self.pos += 1;
        }
        if self.pos - first == digits {
            Ok(())
        } else {
            Err(Failure::Invalid(RegexError::spanning(
                format_args!("incomplete escape {}", self.text(start..self.pos)),
                start,
                start..self.pos,
            )))
        }
    }

    /// the `{NAME}` of a `\N{NAME}` escape starting at `start`, whose name has to
    /// be one the python version's unicode database has for a single character —
    /// see [`character_name_resolves`]
    fn named_escape(&mut self, start: usize) -> Result<(), Failure> {
        if !self.eat('{') {
            return Err(Failure::Invalid(RegexError::new("missing {", self.pos)));
        }
        let name = self.pos;
        loop {
            match self.peek() {
                None if self.pos == name => {
                    return Err(Failure::Invalid(RegexError::new(
                        "missing character name",
                        self.pos,
                    )));
                }
                None => {
                    return Err(Failure::Invalid(RegexError::spanning(
                        "missing }, unterminated name",
                        name,
                        name..self.pos,
                    )));
                }
                Some('}') if self.pos == name => {
                    return Err(Failure::Invalid(RegexError::new(
                        "missing character name",
                        self.pos,
                    )));
                }
                Some('}') => {
                    let text = self.text(name..self.pos);
                    self.pos += 1;
                    if character_name_resolves(&text, self.dialect.version) == Some(false) {
                        return Err(Failure::Invalid(RegexError::spanning(
                            format_args!(
                                "undefined character name {}",
                                UnicodeEscape::new_repr(&text).str_repr(TripleQuotes::No)
                            ),
                            start,
                            start..self.pos,
                        )));
                    }
                    return Ok(());
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn eat_octal_digits(&mut self, most: usize) {
        for _ in 0..most {
            if !self.peek().is_some_and(|c| c.is_digit(8)) {
                break;
            }
            self.pos += 1;
        }
    }

    /// an octal escape starting at `start` names a byte at most
    fn octal_in_range(&self, start: usize) -> Result<(), Failure> {
        let digits = self.text(start + 1..self.pos);
        if u32::from_str_radix(&digits, 8).is_ok_and(|value| value <= 0o377) {
            Ok(())
        } else {
            Err(Failure::Invalid(RegexError::spanning(
                format_args!(
                    "octal escape value {} outside of range 0-0o377",
                    self.text(start..self.pos)
                ),
                start,
                start..self.pos,
            )))
        }
    }

    /// `\1` to `\99`, a backreference, unless it is three octal digits, which
    /// is an octal escape
    fn group_reference(&mut self, start: usize) -> Result<(), Failure> {
        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
            if self.chars[start + 1..self.pos]
                .iter()
                .all(|c| c.is_digit(8))
                && self.peek().is_some_and(|c| c.is_digit(8))
            {
                self.pos += 1;
                return self.octal_in_range(start);
            }
        }
        let group = self
            .text(start + 1..self.pos)
            .parse::<u32>()
            .unwrap_or(u32::MAX);
        if group > self.group_count {
            return Err(Failure::Invalid(RegexError::spanning(
                format_args!("invalid group reference {group}"),
                start + 1,
                start + 1..self.pos,
            )));
        }
        self.check_backreference(group, start, start..self.pos)
    }

    /// what python checks of a backreference to `group`, by number or by name, whose text
    /// is `span`: that the group is closed, which python reports at `start`, and that it
    /// was not opened in the lookbehind the backreference is in
    fn check_backreference(
        &self,
        group: u32,
        start: usize,
        span: Range<usize>,
    ) -> Result<(), Failure> {
        if !self
            .closed
            .get((group as usize).saturating_sub(1))
            .copied()
            .unwrap_or_default()
        {
            return Err(Failure::Invalid(RegexError::spanning(
                "cannot refer to an open group",
                start,
                span,
            )));
        }
        if self.lookbehind_groups.is_some_and(|first| group >= first) {
            return Err(Failure::Invalid(RegexError::spanning(
                "cannot refer to group defined in the same lookbehind subpattern",
                self.pos,
                span,
            )));
        }
        Ok(())
    }

    /// consume a `[…]` character set
    ///
    /// nothing inside one can open a group, so we only need to find its end. a
    /// `]` immediately after the opening bracket (or after a leading `^`) is a
    /// literal, and verbose mode does not ignore whitespace in here
    fn parse_character_set(&mut self) -> Result<(), Failure> {
        let open = self.pos;
        self.pos += 1;
        self.eat('^');
        self.eat(']');
        loop {
            match self.peek() {
                None => {
                    return Err(Failure::Invalid(RegexError::new(
                        "unterminated character set",
                        open,
                    )));
                }
                Some(']') => {
                    self.pos += 1;
                    return Ok(());
                }
                Some('\\') => self.parse_escape(true)?,
                Some(_) => self.pos += 1,
            }
        }
    }

    fn parse_group(&mut self) -> Result<Option<Node>, Failure> {
        let open = self.pos;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Failure::Unknown);
        }
        let node = self.parse_group_inner(open);
        self.depth -= 1;
        node
    }

    fn parse_group_inner(&mut self, open: usize) -> Result<Option<Node>, Failure> {
        self.pos += 1;
        if !self.eat('?') {
            let index = self.open_capturing_group(None, open)?;
            let node = self.parse_alternation()?;
            self.close_capturing_group(open, index)?;
            return Ok(Some(Node::Group {
                index: Some(index),
                node: Box::new(node),
            }));
        }

        match self.peek() {
            Some('P') => self.parse_named_extension(open),
            Some(':') => {
                self.pos += 1;
                self.parse_plain_group(open, None).map(Some)
            }
            Some('=' | '!') => {
                let negative = self.peek() == Some('!');
                self.pos += 1;
                self.parse_plain_group(open, None)
                    .map(|node| Some(uncertain_if(node, negative)))
            }
            Some('>') => {
                self.pos += 1;
                self.parse_plain_group(open, None).map(Some)
            }
            Some('<') => match self.peek_at(1) {
                Some('=' | '!') => {
                    let negative = self.peek_at(1) == Some('!');
                    self.pos += 2;
                    // the groups a backreference inside may not refer to are the
                    // ones opened in the outermost lookbehind
                    let outer = self.lookbehind_groups;
                    self.lookbehind_groups = outer.or(Some(self.group_count + 1));
                    let node = self.parse_plain_group(open, None);
                    self.lookbehind_groups = outer;
                    node.map(|node| Some(uncertain_if(node, negative)))
                }
                Some(char) => Err(Failure::Invalid(RegexError::new(
                    format_args!("unknown extension ?<{char}"),
                    open + 1,
                ))),
                None => Err(self.unexpected_end()),
            },
            Some('#') => {
                self.pos += 1;
                loop {
                    match self.peek() {
                        None => {
                            return Err(Failure::Invalid(RegexError::new(
                                "missing ), unterminated comment",
                                open,
                            )));
                        }
                        Some(')') => {
                            self.pos += 1;
                            return Ok(None);
                        }
                        Some(_) => self.pos += 1,
                    }
                }
            }
            Some('(') => self.parse_conditional(open),
            Some(char) if char == '-' || "aiLmsux".contains(char) => self.parse_flags(open),
            Some(char) => Err(Failure::Invalid(RegexError::new(
                format_args!("unknown extension ?{char}"),
                open + 1,
            ))),
            None => Err(self.unexpected_end()),
        }
    }

    fn unexpected_end(&self) -> Failure {
        Failure::Invalid(RegexError::new(
            "unexpected end of pattern",
            self.chars.len(),
        ))
    }

    /// parse a group body and its closing paren, optionally under a scoped
    /// verbose setting from inline flags
    fn parse_plain_group(&mut self, open: usize, verbose: Option<bool>) -> Result<Node, Failure> {
        let outer = self.verbose;
        if let Some(verbose) = verbose {
            self.verbose = verbose;
        }
        let node = self.parse_alternation();
        self.verbose = outer;
        let node = node?;
        self.close_group(open)?;
        Ok(node)
    }

    fn close_group(&mut self, open: usize) -> Result<(), Failure> {
        if self.eat(')') {
            Ok(())
        } else {
            Err(Failure::Invalid(RegexError::new(
                "missing ), unterminated subpattern",
                open,
            )))
        }
    }

    fn close_capturing_group(&mut self, open: usize, index: u32) -> Result<(), Failure> {
        self.close_group(open)?;
        if let Some(closed) = self.closed.get_mut(index as usize - 1) {
            *closed = true;
        }
        Ok(())
    }

    /// `(?P<name>…)` and `(?P=name)`
    fn parse_named_extension(&mut self, open: usize) -> Result<Option<Node>, Failure> {
        self.pos += 1;
        match self.peek() {
            Some('<') => {
                self.pos += 1;
                let (name, start) = self.read_name('>')?;
                let name = Self::validate_group_name(&name, start)?;
                let index = self.open_capturing_group(Some(name), start)?;
                let node = self.parse_alternation()?;
                self.close_capturing_group(open, index)?;
                Ok(Some(Node::Group {
                    index: Some(index),
                    node: Box::new(node),
                }))
            }
            Some('=') => {
                self.pos += 1;
                let (name, start) = self.read_name(')')?;
                let name = Self::validate_group_name(&name, start)?;
                // python resolves a backreference against the groups opened so
                // far, so a forward reference is an error there too
                let Some(&group) = self.names.get(&name) else {
                    return Err(Failure::Invalid(RegexError::spanning(
                        format_args!("unknown group name '{name}'"),
                        start,
                        name_span(&name, start),
                    )));
                };
                self.check_backreference(group, start, name_span(&name, start))?;
                Ok(Some(Node::Atom))
            }
            Some(char) => Err(Failure::Invalid(RegexError::new(
                format_args!("unknown extension ?P{char}"),
                open + 1,
            ))),
            None => Err(self.unexpected_end()),
        }
    }

    /// read the text up to `terminator`, returning it with the position it started at
    fn read_name(&mut self, terminator: char) -> Result<(String, usize), Failure> {
        let start = self.pos;
        let mut name = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(Failure::Invalid(if name.is_empty() {
                        RegexError::new("missing group name", start)
                    } else {
                        RegexError::new(
                            format_args!("missing {terminator}, unterminated name"),
                            start,
                        )
                    }));
                }
                Some(char) if char == terminator => {
                    self.pos += 1;
                    break;
                }
                Some(char) => {
                    name.push(char);
                    self.pos += 1;
                }
            }
        }
        Ok((name, start))
    }

    /// python requires a group name to be a valid identifier
    fn validate_group_name(name: &str, start: usize) -> Result<Name, Failure> {
        if name.is_empty() {
            return Err(Failure::Invalid(RegexError::new(
                "missing group name",
                start,
            )));
        }
        if !is_identifier(name) {
            return Err(Failure::Invalid(RegexError::spanning(
                format_args!("bad character in group name '{name}'"),
                start,
                name_span(name, start),
            )));
        }
        Ok(Name::new(name))
    }

    /// `(?(id-or-name)yes|no)`
    fn parse_conditional(&mut self, open: usize) -> Result<Option<Node>, Failure> {
        self.pos += 1;
        let (reference, start) = self.read_name(')')?;
        if !reference.chars().all(|char| char.is_ascii_digit()) {
            let name = Self::validate_group_name(&reference, start)?;
            if !self.names.contains_key(&name) {
                return Err(Failure::Invalid(RegexError::new(
                    format_args!("unknown group name '{name}'"),
                    start,
                )));
            }
        }

        let node = self.parse_alternation()?;
        if let Node::Alternate(branches) = &node
            && branches.len() > 2
        {
            return Err(Failure::Invalid(RegexError::new(
                "conditional backref with more than two branches",
                start,
            )));
        }
        self.close_group(open)?;
        // whichever branch ran, the other one's groups are unset — and even the
        // branch that ran depends on a group we cannot evaluate statically
        Ok(Some(Node::Uncertain(Box::new(node))))
    }

    /// `(?aiLmsux)` global flags and `(?aiLmsux-imsx:…)` scoped flags
    fn parse_flags(&mut self, open: usize) -> Result<Option<Node>, Failure> {
        let mut enabled = String::new();
        let mut disabled = String::new();
        let mut negating = false;
        loop {
            match self.peek() {
                Some('-') if !negating => {
                    negating = true;
                    self.pos += 1;
                }
                Some(char) if "aiLmsux".contains(char) => {
                    if negating {
                        disabled.push(char);
                    } else {
                        enabled.push(char);
                    }
                    self.pos += 1;
                }
                Some(':') => {
                    self.pos += 1;
                    let verbose = if enabled.contains('x') {
                        Some(true)
                    } else if disabled.contains('x') {
                        Some(false)
                    } else {
                        None
                    };
                    return self.parse_plain_group(open, verbose).map(Some);
                }
                Some(')') if !negating => {
                    self.pos += 1;
                    // a global `(?x)` retroactively changes how the text before
                    // it tokenizes, so the whole pattern has to be re-read
                    return if enabled.contains('x') && !self.verbose {
                        Err(Failure::RestartVerbose)
                    } else {
                        // a global flag setting contributes no item of its own
                        Ok(None)
                    };
                }
                // a negated flag set only ever scopes a group, so it must name
                // at least one flag and then be followed by `:`, never by `)`
                _ if negating => {
                    return Err(Failure::Invalid(RegexError::new(
                        if disabled.is_empty() {
                            "missing flag"
                        } else {
                            "missing :"
                        },
                        self.pos,
                    )));
                }
                _ => {
                    return Err(Failure::Invalid(RegexError::new(
                        "missing -, : or )",
                        self.pos,
                    )));
                }
            }
        }
    }

    fn open_capturing_group(&mut self, name: Option<Name>, start: usize) -> Result<u32, Failure> {
        self.group_count += 1;
        let index = self.group_count;
        if let Some(name) = &name
            && let Some(previous) = self.names.insert(name.clone(), index)
        {
            return Err(Failure::Invalid(RegexError::spanning(
                format_args!(
                    "redefinition of group name '{name}' as group {index}; was group {previous}"
                ),
                start,
                name_span(name, start),
            )));
        }
        self.group_names.push(name);
        self.closed.push(false);
        Ok(index)
    }
}

/// a parsed `{m,n}` quantifier
struct BracedQuantifier {
    /// how many characters of source it occupies
    length: usize,
    min: u32,
    max: u32,
}

/// the characters of a group's name that starts at `start`
/// the newest python whose unicode database `unicode_names2` has every name of:
/// the crate's tables are unicode 17.0's, which python 3.15 ships
const NAMES_KNOWN_THROUGH: PythonVersion = PythonVersion::PY315;

/// whether `re` on `version` resolves `name`, the name in a `\N{...}`, to one
/// character, or `None` where we cannot tell
///
/// python looks the name up in the `unicodedata` of its own version, case
/// insensitively, among the characters' names and their aliases. a named
/// sequence resolves to several characters, which `re` refuses as it refuses a
/// name that resolves to nothing. names and aliases are never changed or
/// withdrawn, so a name python resolves is one `unicode_names2` resolves, through
/// the newest python whose unicode its tables hold, and the version decides only
/// whether the alias exists yet, which [`ALIASED_SINCE`] says, or else whether the
/// character the name is for has a name yet, which [`NAMED_SINCE`] says
fn character_name_resolves(name: &str, version: PythonVersion) -> Option<bool> {
    if version > NAMES_KNOWN_THROUGH {
        return None;
    }
    // names derived from the code point by a rule, which pythons do not all
    // implement alike: 3.8 names `CJK UNIFIED IDEOGRAPH-2EBE1`, which no unicode
    // assigns, only 3.13 and later resolve one written in lowercase or a
    // `TANGUT IDEOGRAPH-17000`, and only 3.12 and earlier one whose code point is
    // written with a leading zero
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("CJK UNIFIED IDEOGRAPH-") || upper.starts_with("TANGUT IDEOGRAPH-") {
        return None;
    }
    if let Ok(index) = ALIASED_SINCE.binary_search_by(|&(alias, _)| alias.cmp(upper.as_str()))
        && let Some(&(_, minor)) = ALIASED_SINCE.get(index)
    {
        return Some(version.minor >= minor);
    }
    let Some(character) = unicode_names2::character(name) else {
        return Some(false);
    };
    let code = u32::from(character);
    let named_since = NAMED_SINCE
        .binary_search_by(|&(first, last, _)| {
            if last < code {
                std::cmp::Ordering::Less
            } else if first > code {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()
        .and_then(|index| NAMED_SINCE.get(index))
        .map(|&(_, _, minor)| minor);
    Some(named_since.is_none_or(|minor| version.minor >= minor))
}

fn name_span(name: &str, start: usize) -> Range<usize> {
    start..start + name.chars().count()
}

fn uncertain_if(node: Node, uncertain: bool) -> Node {
    if uncertain {
        Node::Uncertain(Box::new(node))
    } else {
        node
    }
}

/// python requires a group name to be a valid identifier
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|char| char == '_' || char.is_alphabetic())
        && chars.all(|char| char == '_' || char.is_alphanumeric())
}

/// record, for each capture group, whether it is set on *every* successful match
///
/// `certain` is whether the subtree we are in runs on every successful match;
/// `out` is indexed by group number minus one
fn collect_certainty(node: &Node, certain: bool, out: &mut [bool]) {
    match node {
        Node::Atom => {}
        Node::Concat(items) => {
            for item in items {
                collect_certainty(item, certain, out);
            }
        }
        Node::Alternate(branches) => {
            // a group lives in exactly one branch, so with a real choice no
            // group inside is guaranteed
            let certain = certain && branches.len() == 1;
            for branch in branches {
                collect_certainty(branch, certain, out);
            }
        }
        Node::Repeat { node, optional } => collect_certainty(node, certain && !optional, out),
        Node::Uncertain(node) => collect_certainty(node, false, out),
        Node::Group { index, node } => {
            if let Some(index) = index
                && let Some(slot) = out.get_mut((*index as usize) - 1)
            {
                *slot = certain;
            }
            collect_certainty(node, certain, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::PythonVersion;

    use super::{Dialect, NAMES_KNOWN_THROUGH, ParsedGroup, PatternAnalysis, analyze};

    const STR: Dialect = Dialect {
        version: PythonVersion::PY313,
        bytes: false,
    };

    /// `!` marks an optional group, `?` an unnamed one, e.g. `"?, !name"`
    fn shape(pattern: &str) -> String {
        shape_in(pattern, false, STR)
    }

    fn shape_with(pattern: &str, verbose: bool) -> String {
        shape_in(pattern, verbose, STR)
    }

    fn shape_in(pattern: &str, verbose: bool, dialect: Dialect) -> String {
        match analyze(pattern, verbose, dialect) {
            PatternAnalysis::Groups(groups) => groups
                .iter()
                .map(
                    |ParsedGroup {
                         name,
                         definitely_set,
                     }| {
                        format!(
                            "{}{}",
                            if *definitely_set { "" } else { "!" },
                            name.as_ref().map_or("?", |name| name.as_str())
                        )
                    },
                )
                .collect::<Vec<_>>()
                .join(", "),
            PatternAnalysis::Invalid(error) => format!("invalid: {}", error.message()),
            PatternAnalysis::Unknown => "unknown".to_string(),
        }
    }

    #[test]
    fn no_groups() {
        assert_eq!(shape(""), "");
        assert_eq!(shape("abc"), "");
        assert_eq!(shape("a.*b$"), "");
    }

    #[test]
    fn plain_groups() {
        assert_eq!(shape("()"), "?");
        assert_eq!(shape("()()"), "?, ?");
        assert_eq!(shape("(a)(b)(c)"), "?, ?, ?");
    }

    #[test]
    fn optional_groups() {
        assert_eq!(shape("()?()"), "!?, ?");
        assert_eq!(shape("(a)*"), "!?");
        assert_eq!(shape("(a)+"), "?");
        assert_eq!(shape("(a){0,3}"), "!?");
        assert_eq!(shape("(a){2,3}"), "?");
        assert_eq!(shape("(a){0}"), "!?");
        assert_eq!(shape("(a){,3}"), "!?");
        assert_eq!(shape("(a){3}"), "?");
        assert_eq!(shape("(a){1,}"), "?");
        assert_eq!(shape("(a)*?"), "!?");
        assert_eq!(shape("(a)??"), "!?");
        // possessive quantifiers and atomic groups need python 3.11; accepting
        // them unconditionally can only ever miss an error, never invent one
        assert_eq!(shape("(a)++"), "?");
    }

    #[test]
    fn nested_optionality() {
        assert_eq!(shape("((a))"), "?, ?");
        assert_eq!(shape("((a))?"), "!?, !?");
        assert_eq!(shape("((a)?)"), "?, !?");
        assert_eq!(shape("(?:(a))"), "?");
        assert_eq!(shape("(?:(a))?"), "!?");
    }

    #[test]
    fn alternation() {
        assert_eq!(shape("(a)|(b)"), "!?, !?");
        assert_eq!(shape("(a|b)"), "?");
        assert_eq!(shape("(?:(a)|(b))(c)"), "!?, !?, ?");
        // an alternation nested inside a group only makes that group's contents
        // uncertain, not its siblings
        assert_eq!(shape("((a)|b)(c)"), "?, !?, ?");
    }

    #[test]
    fn named_groups() {
        assert_eq!(shape("(?P<a>)"), "a");
        assert_eq!(shape("(?P<a>a)(?P<b>b)?"), "a, !b");
        assert_eq!(shape("(?P<a>a)(?P=a)"), "a");
        assert_eq!(shape("(?P<a_1>x)"), "a_1");
        assert_eq!(
            shape("(?P=a)(?P<a>x)"),
            "invalid: unknown group name 'a' at position 4"
        );
        assert_eq!(
            shape("(?P<a>x)(?P<a>y)"),
            "invalid: redefinition of group name 'a' as group 2; was group 1 at position 12"
        );
    }

    /// a backreference by name is checked as one by number is
    #[test]
    fn named_backreferences() {
        assert_eq!(
            shape("(?P<a>(?P=a))"),
            "invalid: cannot refer to an open group at position 10"
        );
        assert_eq!(
            shape("(?P<b>(?P<a>x)(?P=b))"),
            "invalid: cannot refer to an open group at position 18"
        );
        assert_eq!(
            shape("(?<=(?P<a>x)(?P=a))"),
            "invalid: cannot refer to group defined in the same lookbehind subpattern at position 18"
        );
        assert_eq!(shape("(?P<a>x)(?<=(?P=a))"), "a");
    }

    #[test]
    fn lookarounds() {
        // a positive lookahead's groups do capture
        assert_eq!(shape("(?=(a))a"), "?");
        assert_eq!(shape("(?<=(a))b"), "?");
        // a negative one's never can: if the overall match succeeded, it didn't
        assert_eq!(shape("(?!(a))b"), "!?");
        assert_eq!(shape("(?<!(a))b"), "!?");
    }

    #[test]
    fn atomic_and_scoped_flags() {
        assert_eq!(shape("(?>(a))"), "?");
        assert_eq!(shape("(?i:(a))"), "?");
        assert_eq!(shape("(?i-s:(a))"), "?");
    }

    #[test]
    fn conditional() {
        // the yes-branch is in fact always taken here, since group 1 always
        // matches — we settle for the conservative answer rather than
        // evaluating the condition
        assert_eq!(shape("(a)(?(1)(b)|(c))"), "?, !?, !?");
        assert_eq!(shape("(a)(?(1)b)"), "?");
        assert_eq!(shape("(?P<a>x)(?(a)y)"), "a");
        assert_eq!(
            shape("(?(x)a)(?P<x>b)"),
            "invalid: unknown group name 'x' at position 3"
        );
        assert_eq!(
            shape("(?(1)a|b|c)"),
            "invalid: conditional backref with more than two branches at position 3"
        );
    }

    #[test]
    fn comment_group() {
        assert_eq!(shape("(?#a comment)(a)"), "?");
        // the first `)` closes the comment, whatever else is in it
        assert_eq!(shape("(?#with ( inside)(a)"), "?");
        assert_eq!(
            shape("(?#unterminated"),
            "invalid: missing ), unterminated comment at position 0"
        );
    }

    /// a `(?#…)` comment and a `(?i)` global flag setting produce no item, so a
    /// quantifier reaches straight past them to whatever came before
    #[test]
    fn quantifiers_see_through_items_that_produce_nothing() {
        assert_eq!(shape("(a)(?#c)*"), "!?");
        assert_eq!(shape("(a)(?i)*"), "!?");
        assert_eq!(shape("(?#c)(?#c)(a)*"), "!?");
        assert_eq!(shape("(?#c)*"), "invalid: nothing to repeat at position 5");
        assert_eq!(shape("(?i)*"), "invalid: nothing to repeat at position 4");
        assert_eq!(shape("a*(?#c)*"), "invalid: multiple repeat at position 7");
    }

    #[test]
    fn character_sets_do_not_open_groups() {
        assert_eq!(shape("[()](a)"), "?");
        assert_eq!(shape("[]](a)"), "?");
        assert_eq!(shape("[^]](a)"), "?");
        assert_eq!(shape(r"[\]](a)"), "?");
        assert_eq!(shape("[a[b](c)"), "?");
    }

    #[test]
    fn escapes_do_not_open_groups() {
        assert_eq!(shape(r"\((a)\)"), "?");
        assert_eq!(shape(r"\\(a)"), "?");
        assert_eq!(shape(r"\N{GREEK SMALL LETTER ALPHA}(a)"), "?");
        assert_eq!(shape(r"(a)\1"), "?");
    }

    #[test]
    fn braces_that_are_not_quantifiers_are_literal() {
        assert_eq!(shape("(a){}"), "?");
        assert_eq!(shape("(a){x}"), "?");
        assert_eq!(shape("(a){1"), "?");
        assert_eq!(shape("a{2,3}(b)"), "?");
        // `{,}` on the other hand *is* a quantifier, equivalent to `*`
        assert_eq!(shape("(a){,}"), "!?");
    }

    #[test]
    fn verbose_mode() {
        assert_eq!(shape("()#()"), "?, ?");
        assert_eq!(shape_with("()#()", true), "?");
        assert_eq!(shape_with("( a ) ( b )?", true), "?, !?");
        // a global `(?x)` turns verbose on for the whole pattern
        assert_eq!(shape("(?x)()#()"), "?");
        // ... but a scoped one only inside its group
        assert_eq!(shape("(?x:()#()\n)()"), "?, ?");
        // and inside it the comment really does swallow the closing paren
        assert_eq!(
            shape("(?x:()#())()"),
            "invalid: missing ), unterminated subpattern at position 0"
        );
        // whitespace is significant inside a character set even in verbose mode
        assert_eq!(shape_with("[ #](a)", true), "?");
    }

    #[test]
    fn verbose_whitespace_before_a_quantifier() {
        assert_eq!(shape_with("(a) ?", true), "!?");
        assert_eq!(shape_with("(a) # trailing\n ?", true), "!?");
    }

    #[test]
    fn invalid_patterns() {
        assert_eq!(
            shape("("),
            "invalid: missing ), unterminated subpattern at position 0"
        );
        assert_eq!(
            shape("a(b"),
            "invalid: missing ), unterminated subpattern at position 1"
        );
        assert_eq!(shape(")"), "invalid: unbalanced parenthesis at position 0");
        assert_eq!(shape("a)"), "invalid: unbalanced parenthesis at position 1");
        assert_eq!(shape("*a"), "invalid: nothing to repeat at position 0");
        assert_eq!(shape("(|*)"), "invalid: nothing to repeat at position 2");
        assert_eq!(shape("a**"), "invalid: multiple repeat at position 2");
        assert_eq!(
            shape("[a"),
            "invalid: unterminated character set at position 0"
        );
        assert_eq!(shape("(?_)"), "invalid: unknown extension ?_ at position 1");
        assert_eq!(shape("(?)"), "invalid: unknown extension ?) at position 1");
        assert_eq!(
            shape("(?<a>)"),
            "invalid: unknown extension ?<a at position 1"
        );
        assert_eq!(
            shape("(?P>a)"),
            "invalid: unknown extension ?P> at position 1"
        );
        assert_eq!(shape("(?P<>)"), "invalid: missing group name at position 4");
        assert_eq!(
            shape("(?P<1a>)"),
            "invalid: bad character in group name '1a' at position 4"
        );
        assert_eq!(
            shape("(?P<a"),
            "invalid: missing >, unterminated name at position 4"
        );
        assert_eq!(
            shape("(?P<a>"),
            "invalid: missing ), unterminated subpattern at position 0"
        );
        assert_eq!(shape("(?P<"), "invalid: missing group name at position 4");
        assert_eq!(
            shape("(?P"),
            "invalid: unexpected end of pattern at position 3"
        );
        assert_eq!(
            shape("(?"),
            "invalid: unexpected end of pattern at position 2"
        );
        assert_eq!(
            shape("(?<"),
            "invalid: unexpected end of pattern at position 3"
        );
        assert_eq!(shape("(?("), "invalid: missing group name at position 3");
        assert_eq!(
            shape("(?(1"),
            "invalid: missing ), unterminated name at position 3"
        );
        assert_eq!(shape("(?i"), "invalid: missing -, : or ) at position 3");
        assert_eq!(shape("(?i-"), "invalid: missing flag at position 4");
        assert_eq!(shape("(?i-)"), "invalid: missing flag at position 4");
        assert_eq!(shape("(?-i)"), "invalid: missing : at position 4");
        assert_eq!(shape("(?i-i)"), "invalid: missing : at position 5");
        assert_eq!(shape("a{1,2}*"), "invalid: multiple repeat at position 6");
        assert_eq!(shape("a{1,2}{3}"), "invalid: multiple repeat at position 6");
        assert_eq!(
            shape("a{2,1}"),
            "invalid: min repeat greater than max repeat at position 2"
        );
        assert_eq!(
            shape(r"a\"),
            "invalid: bad escape (end of pattern) at position 1"
        );
        assert_eq!(
            shape(r"\"),
            "invalid: bad escape (end of pattern) at position 0"
        );
    }

    /// what python 3.13's `re` says of each, for a `str` pattern, and of the
    /// same pattern as `bytes`
    #[test]
    fn escapes() {
        let bytes = Dialect { bytes: true, ..STR };
        let both = |pattern: &str, expected: &str| {
            assert_eq!(shape(pattern), expected, "{pattern} as str");
            assert_eq!(
                shape_in(pattern, false, bytes),
                expected,
                "{pattern} as bytes"
            );
        };
        both(r"\q", r"invalid: bad escape \q at position 0");
        both(r"a\q", r"invalid: bad escape \q at position 1");
        both(r"[\q]", r"invalid: bad escape \q at position 1");
        both(r"[\z]", r"invalid: bad escape \z at position 1");
        both(r"[\A]", r"invalid: bad escape \A at position 1");
        both(r"[\8]", r"invalid: bad escape \8 at position 1");
        both(r"\x4", r"invalid: incomplete escape \x4 at position 0");
        both(r"\xg", r"invalid: incomplete escape \x at position 0");
        both(
            r"\777",
            r"invalid: octal escape value \777 outside of range 0-0o377 at position 0",
        );
        both(
            r"[\777]",
            r"invalid: octal escape value \777 outside of range 0-0o377 at position 1",
        );
        both(r"\8", "invalid: invalid group reference 8 at position 1");
        both(r"\12", "invalid: invalid group reference 12 at position 1");
        both(r"(a)\2", "invalid: invalid group reference 2 at position 4");
        both(
            r"(a\1)",
            "invalid: cannot refer to an open group at position 2",
        );
        both(
            r"(?<=(a)\1)",
            "invalid: cannot refer to group defined in the same lookbehind subpattern at position 9",
        );
        for valid in [
            r"\A\b\B\d\D\s\S\w\W\Z",
            r"[\b\d\D\s\S\w\W]",
            r"\a\f\n\r\t\v\\",
            r"\x41",
            r"\0",
            r"\07777",
            r"\177",
            r"[\1]",
            r"\-",
            r"[\-]",
            r"\_",
            r"\ ",
            "\\\u{e9}",
        ] {
            both(valid, "");
        }
        both(r"(a)\1", "?");
        both(r"(a)(?<=\1)", "?");

        // the escapes a `bytes` pattern does not have
        assert_eq!(shape(r"\u0041\U0010ffff"), "");
        assert_eq!(shape(r"\N{EM DASH}"), "");
        assert_eq!(
            shape_in(r"\u0041", false, bytes),
            r"invalid: bad escape \u at position 0"
        );
        assert_eq!(
            shape_in(r"\U0010ffff", false, bytes),
            r"invalid: bad escape \U at position 0"
        );
        assert_eq!(
            shape_in(r"\N{EM DASH}", false, bytes),
            r"invalid: bad escape \N at position 0"
        );
        assert_eq!(
            shape(r"\u12"),
            r"invalid: incomplete escape \u12 at position 0"
        );
        assert_eq!(
            shape(r"\U0011ffff"),
            r"invalid: bad escape \U0011ffff at position 0"
        );
        assert_eq!(shape(r"\N"), "invalid: missing { at position 2");
        assert_eq!(shape(r"\Nx"), "invalid: missing { at position 2");
        assert_eq!(
            shape(r"\N{"),
            "invalid: missing character name at position 3"
        );
        assert_eq!(
            shape(r"\N{}"),
            "invalid: missing character name at position 3"
        );
        assert_eq!(
            shape(r"\N{ab"),
            "invalid: missing }, unterminated name at position 3"
        );
    }

    /// a `\N{...}` name is looked up as `unicodedata.lookup` looks it up: in any
    /// case, among the names and their aliases, and for one character
    #[test]
    fn character_names() {
        assert_eq!(shape(r"\N{em dash}\N{BYTE ORDER MARK}\N{LINE FEED}"), "");
        assert_eq!(
            shape(r"a\N{EM  DASH}(b)"),
            "invalid: undefined character name 'EM  DASH' at position 1"
        );
        // a named sequence stands for several characters
        assert_eq!(
            shape(r"\N{LATIN CAPITAL LETTER A WITH MACRON AND GRAVE}"),
            "invalid: undefined character name 'LATIN CAPITAL LETTER A WITH MACRON AND GRAVE' \
             at position 0"
        );
        // the name is quoted as python's `repr` quotes it
        assert_eq!(
            shape(r"\N{O'CLOCK}"),
            r#"invalid: undefined character name "O'CLOCK" at position 0"#
        );
        // names derived from the code point, which pythons do not all derive alike,
        // are left to python
        assert_eq!(shape(r"\N{CJK UNIFIED IDEOGRAPH-2EBE1}"), "");
        assert_eq!(shape(r"\N{TANGUT IDEOGRAPH-FFFFF}"), "");
    }

    /// a name exists from the python whose unicode assigns its character, and past
    /// the unicode `unicode_names2` holds, a name is not looked up at all
    #[test]
    fn character_names_by_version() {
        let version = |minor| Dialect {
            version: PythonVersion { major: 3, minor },
            ..STR
        };
        // unicode 14.0, python 3.11
        let arabic = r"\N{ARABIC POUND MARK ABOVE}";
        assert_eq!(
            shape_in(arabic, false, version(10)),
            "invalid: undefined character name 'ARABIC POUND MARK ABOVE' at position 0"
        );
        assert_eq!(shape_in(arabic, false, version(11)), "");
        // unicode 16.0, python 3.14
        let alien = r"\N{ALIEN MONSTER OPEN JAWS}";
        assert_eq!(
            shape_in(alien, false, version(13)),
            "invalid: undefined character name 'ALIEN MONSTER OPEN JAWS' at position 0"
        );
        assert_eq!(shape_in(alien, false, version(14)), "");
        // unicode 17.0, python 3.15
        let bell = r"\N{ALARM BELL SYMBOL}";
        assert_eq!(
            shape_in(bell, false, version(14)),
            "invalid: undefined character name 'ALARM BELL SYMBOL' at position 0"
        );
        assert_eq!(shape_in(bell, false, version(15)), "");
        assert_eq!(shape_in(r"\N{NO SUCH NAME}", false, version(16)), "");
    }

    /// an alias exists from the python whose unicode adds it, which can be long after the
    /// character it is for was first named
    #[test]
    fn character_aliases_by_version() {
        let version = |minor| Dialect {
            version: PythonVersion { major: 3, minor },
            ..STR
        };
        // `END OF MEDIUM`, named in every python, is aliased `EM` from python 3.12
        assert_eq!(
            shape_in(r"\N{em}", false, version(11)),
            "invalid: undefined character name 'em' at position 0"
        );
        assert_eq!(shape_in(r"\N{em}", false, version(12)), "");
        let bamum = r"\N{BAMUM LETTER PHASE-B NGGOM}";
        assert_eq!(
            shape_in(bamum, false, version(14)),
            "invalid: undefined character name 'BAMUM LETTER PHASE-B NGGOM' at position 0"
        );
        assert_eq!(shape_in(bamum, false, version(15)), "");
        // an alias every python resolves
        assert_eq!(shape_in(r"\N{BYTE ORDER MARK}", false, version(8)), "");
    }

    /// `NAMES_KNOWN_THROUGH` is the python whose unicode `unicode_names2` holds:
    /// the tables are built from the unicode 17.0 data vendored beside the crate,
    /// and know a name 17.0 assigned. when the data moves to a newer unicode,
    /// `NAMES_KNOWN_THROUGH` moves to the python that ships it
    #[test]
    fn unicode_names2_holds_the_unicode_of_names_known_through() {
        let aliases = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../ruff_unicode_names2/data/NameAliases.txt"
        ));
        assert_eq!(aliases.lines().next(), Some("# NameAliases-17.0.0.txt"));
        assert_eq!(NAMES_KNOWN_THROUGH, PythonVersion::PY315);
        assert!(unicode_names2::character("ALARM BELL SYMBOL").is_some());
    }

    /// `\z` is an anchor from python 3.14, and a bad escape before it
    #[test]
    fn escapes_by_version() {
        assert_eq!(shape(r"\z"), r"invalid: bad escape \z at position 0");
        let py314 = Dialect {
            version: PythonVersion::PY314,
            ..STR
        };
        assert_eq!(shape_in(r"\z", false, py314), "");
        assert_eq!(
            shape_in(r"[\z]", false, py314),
            r"invalid: bad escape \z at position 1"
        );
    }

    #[test]
    fn deeply_nested_patterns_are_unknown_not_invalid() {
        let pattern = format!("{}{}", "(".repeat(200), ")".repeat(200));
        assert_eq!(shape(&pattern), "unknown");
    }

    /// patterns of the kind that actually show up in code, with the shape
    /// cpython reports for each
    #[test]
    fn realistic_patterns() {
        assert_eq!(shape(r"^(\d{4})-(\d{2})-(\d{2})$"), "?, ?, ?");
        assert_eq!(shape(r"(?P<key>\w+)\s*=\s*(?P<value>.*)"), "key, value");
        assert_eq!(shape(r"https?://([^/]+)(/.*)?"), "?, !?");
        assert_eq!(shape(r"(\w+)@(\w+)\.(\w+)"), "?, ?, ?");
        assert_eq!(shape(r"^\s*(?:#.*)?$"), "");
        assert_eq!(
            shape(r"(?P<major>\d+)\.(?P<minor>\d+)(?:\.(?P<patch>\d+))?"),
            "major, minor, !patch"
        );
        assert_eq!(shape("((((a))))"), "?, ?, ?, ?");
        assert_eq!(shape("(a(b(c)d)e)"), "?, ?, ?");
        assert_eq!(
            shape_with(
                "\n  (?P<year>\\d{4}) -    # year\n  (?P<month>\\d{2})     # month\n",
                true
            ),
            "year, month"
        );
    }
}
