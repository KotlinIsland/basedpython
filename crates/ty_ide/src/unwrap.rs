//! Unwrapping a compound statement, or removing one of its clauses.
//!
//! Unwrapping an `if` replaces the whole statement with the suite of the clause the range is in,
//! dedented to where the statement stood; removing its `else` deletes that clause and leaves the
//! rest. These are edits the user asks for to change what a program does, so unlike the
//! refactorings they are not refused for changing it. They are refused when the result would not
//! say what the user can see it saying: a `break` in an unwrapped loop's body would leave some
//! other loop, or none, a `try` cannot be left without an `except` or a `finally`, and a
//! `finally` suite would no longer run after a `return` or a `raise` in the suites before it.
//!
//! Every statement around the range is offered, innermost first, since that is the order in
//! which they enclose it.
//!
//! The rewrite is spelled as many small edits rather than one replacement of the statement: the
//! header deleted, then each line of the suite losing one level of indentation. An editor then
//! keeps the caret, and anything else it has anchored in the suite, where it was in the text.
//! The lines of a string literal that spans several lines are left alone, because their
//! indentation is part of the string.

use ruff_db::PythonFile;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::Stmt;
use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_codegen::Stylist;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::Db;
use crate::syntax_outline::{ClauseHead, OutlineBuilder};

/// One way to unwrap or remove around a range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwrapOption {
    /// What the option does, as an editor lists it: `Unwrap 'if...'`, `Remove 'else...'`.
    pub title: String,
    /// What the option rewrites: the whole statement for an unwrap, the clause for a removal.
    pub range: TextRange,
    /// The parts of [`Self::range`] that stay, which are the suites an unwrap keeps. Empty for a
    /// removal.
    pub kept: Vec<TextRange>,
    /// The edits, or why the option cannot be applied.
    pub edits: Result<Vec<Edit>, String>,
}

/// What can be unwrapped or removed around `range` in `file`, innermost first.
///
/// `Err` with the reason when the file cannot be rewritten at all, which is when it does not
/// parse: the statements a recovered parse makes up are not ones the user wrote.
pub fn unwrap_options(
    db: &dyn Db,
    file: PythonFile<'_>,
    range: TextRange,
) -> Result<Vec<UnwrapOption>, String> {
    let parsed = parsed_module(db, file).load(db);
    if parsed.has_invalid_syntax() {
        return Err("the file has syntax errors".to_string());
    }
    let source = source_text(db, file.file(db));
    let stylist = Stylist::from_tokens(parsed.tokens(), source.as_str());
    let unwrapper = Unwrapper {
        source: source.as_str(),
        line_ending: stylist.line_ending().as_str(),
        tokens: parsed.tokens(),
        builder: OutlineBuilder {
            source: source.as_str(),
            tokens: parsed.tokens(),
        },
    };

    let mut enclosing = Vec::new();
    unwrapper.enclosing(parsed.suite(), range, &mut enclosing);

    let mut options = Vec::new();
    for stmt in enclosing.into_iter().rev() {
        unwrapper.options(stmt, range, &mut options);
    }
    Ok(options)
}

struct Unwrapper<'a> {
    source: &'a str,
    /// What the file ends its lines with, for the line breaks an unwrap writes.
    line_ending: &'static str,
    tokens: &'a Tokens,
    builder: OutlineBuilder<'a>,
}

impl Unwrapper<'_> {
    /// The statements around `range`, outermost first.
    fn enclosing<'s>(&self, suite: &'s [Stmt], range: TextRange, out: &mut Vec<&'s Stmt>) {
        let Some(stmt) = suite.iter().find(|stmt| stmt.range().contains_range(range)) else {
            return;
        };
        out.push(stmt);
        for head in self.builder.clause_heads(stmt) {
            if head.range.contains_range(range) {
                self.enclosing(head.body, range, out);
            }
        }
    }

    /// The options `stmt` offers for a range in one of its clauses.
    fn options(&self, stmt: &Stmt, range: TextRange, out: &mut Vec<UnwrapOption>) {
        let heads = self.builder.clause_heads(stmt);
        let Some(index) = heads
            .iter()
            .position(|head| head.range.contains_range(range))
        else {
            return;
        };
        let kinds: Vec<Option<TokenKind>> = heads.iter().map(|head| self.keyword(head)).collect();
        let keyword = |index: usize| -> &str {
            heads[index]
                .keyword
                .map_or("", |keyword| &self.source[keyword])
        };

        match stmt {
            Stmt::If(_) => {
                out.push(self.unwrap(stmt, &heads, &[index], keyword(index), None));
                if index > 0 {
                    out.push(self.remove(&heads[index], keyword(index), None));
                }
            }
            Stmt::For(_) | Stmt::While(_) => {
                if index == 0 {
                    let refusal = self.exit(heads[0].body, true, false).map(|exit| {
                        format!("`{exit}` in the loop's body would no longer leave this loop")
                    });
                    out.push(self.unwrap(stmt, &heads, &[0], keyword(0), refusal));
                } else {
                    out.push(self.unwrap(stmt, &heads, &[index], keyword(index), None));
                    out.push(self.remove(&heads[index], keyword(index), None));
                }
            }
            Stmt::Try(try_stmt) => match kinds[index] {
                Some(TokenKind::Try) => {
                    // the path through the statement that raises nothing: the `try` suite, then
                    // the `else` suite, then the `finally` suite
                    let kept: Vec<usize> = (0..heads.len())
                        .filter(|&clause| {
                            matches!(
                                kinds[clause],
                                Some(TokenKind::Try | TokenKind::Else | TokenKind::Finally)
                            )
                        })
                        .collect();
                    // a `finally` also runs when the suites before it are left early, which the
                    // unwrapped statements would skip it on
                    let refusal = if try_stmt.finalbody.is_empty() {
                        None
                    } else {
                        [&try_stmt.body, &try_stmt.orelse]
                            .into_iter()
                            .find_map(|body| self.exit(body, true, true))
                            .map(|exit| format!("`{exit}` would no longer run the `finally` suite"))
                    };
                    out.push(self.unwrap(stmt, &heads, &kept, keyword(0), refusal));
                }
                Some(TokenKind::Except) => {
                    let handlers = try_stmt.handlers.len();
                    let refusal = if handlers > 1 {
                        None
                    } else if !try_stmt.orelse.is_empty() {
                        Some("a `try` with an `else` needs an `except`".to_string())
                    } else if try_stmt.finalbody.is_empty() {
                        Some("a `try` needs an `except` or a `finally`".to_string())
                    } else {
                        None
                    };
                    out.push(self.remove(&heads[index], keyword(index), refusal));
                }
                Some(TokenKind::Else) => {
                    out.push(self.remove(&heads[index], keyword(index), None));
                }
                Some(TokenKind::Finally) => {
                    let refusal = try_stmt
                        .handlers
                        .is_empty()
                        .then(|| "a `try` needs an `except` or a `finally`".to_string());
                    out.push(self.remove(&heads[index], keyword(index), refusal));
                }
                _ => {}
            },
            Stmt::With(_) if index == 0 => {
                out.push(self.unwrap(stmt, &heads, &[0], keyword(0), None));
            }
            _ => {}
        }
    }

    /// The kind of `head`'s keyword token, passing over the zero-width `Dedent`s that start where
    /// a dedented clause keyword does.
    fn keyword(&self, head: &ClauseHead) -> Option<TokenKind> {
        let keyword = head.keyword?;
        let index = self
            .tokens
            .partition_point(|token| token.start() < keyword.start());
        self.tokens[index..]
            .iter()
            .find(|token| !token.range().is_empty())
            .map(ruff_python_ast::token::Token::kind)
    }

    /// The first statement in `body` that leaves it other than by running to its end, outside any
    /// function or class `body` defines: with `loop_jumps`, a `break` or `continue` not inside a
    /// nested loop's own suite, and with `returns`, a `return` or a `raise`.
    fn exit(&self, body: &[Stmt], loop_jumps: bool, returns: bool) -> Option<&'static str> {
        for stmt in body {
            match stmt {
                Stmt::Break(_) if loop_jumps => return Some("break"),
                Stmt::Continue(_) if loop_jumps => return Some("continue"),
                Stmt::Return(_) if returns => return Some("return"),
                Stmt::Raise(_) if returns => return Some("raise"),
                Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
                // a nested loop's own suite has its own `break`s; its `else` does not
                Stmt::For(_) | Stmt::While(_) => {
                    for (index, head) in self.builder.clause_heads(stmt).iter().enumerate() {
                        if let Some(exit) = self.exit(head.body, loop_jumps && index > 0, returns) {
                            return Some(exit);
                        }
                    }
                }
                _ => {
                    for head in self.builder.clause_heads(stmt) {
                        if let Some(exit) = self.exit(head.body, loop_jumps, returns) {
                            return Some(exit);
                        }
                    }
                }
            }
        }
        None
    }

    /// Replacing `stmt` with the suites of the clauses `kept` (in source order), dedented to where
    /// `stmt` starts.
    fn unwrap(
        &self,
        stmt: &Stmt,
        heads: &[ClauseHead],
        kept: &[usize],
        keyword: &str,
        refusal: Option<String>,
    ) -> UnwrapOption {
        let title = format!("Unwrap '{keyword}...'");
        let suites: Option<Vec<Suite>> = kept
            .iter()
            .map(|&index| self.suite(&heads[index]))
            .collect();
        let Some(suites) = suites else {
            return UnwrapOption {
                title,
                range: stmt.range(),
                kept: Vec::new(),
                edits: Err("a clause has no suite to keep".to_string()),
            };
        };
        let kept_ranges = suites.iter().map(|suite| suite.kept).collect();
        let edits = match refusal {
            Some(reason) => Err(reason),
            None => self.unwrap_edits(stmt, &suites),
        };
        UnwrapOption {
            title,
            range: stmt.range(),
            kept: kept_ranges,
            edits,
        }
    }

    fn unwrap_edits(&self, stmt: &Stmt, suites: &[Suite]) -> Result<Vec<Edit>, String> {
        let source = self.source;
        let line_start = source.line_start(stmt.start());
        let target = &source[TextRange::new(line_start, stmt.start())];
        if !target.chars().all(char::is_whitespace) {
            return Err("the statement does not start its line".to_string());
        }

        let mut edits = Vec::new();
        let mut previous_end: Option<TextSize> = None;
        for suite in suites {
            match previous_end {
                // the header and everything before the first suite kept
                None => edits.push(Edit::deletion(stmt.start(), suite.kept.start())),
                // the clauses between two suites kept, and the header of the second
                Some(end) => edits.push(Edit::replacement(
                    format!("{}{target}", self.line_ending),
                    source.line_end(end),
                    suite.kept.start(),
                )),
            }
            if let Some(indent) = &suite.indent {
                edits.extend(self.dedent(suite, indent, target));
            }
            previous_end = Some(suite.kept.end());
        }
        // the clauses after the last suite kept
        if let Some(end) = previous_end {
            let from = source.line_end(end);
            let to = source.line_end(stmt.end());
            if from < to {
                edits.push(Edit::deletion(from, to));
            }
        }
        Ok(edits)
    }

    /// The edits taking each line of `suite` after its first from `indent` to `target`.
    fn dedent(&self, suite: &Suite, indent: &str, target: &str) -> Vec<Edit> {
        let source = self.source;
        let mut edits = Vec::new();
        let mut line = source.full_line_end(suite.kept.start());
        while line < suite.kept.end() {
            let next = source.full_line_end(line);
            let text = &source[TextRange::new(line, next)];
            let in_string = self.inside_string(line);
            if !in_string && text.starts_with(indent) && !text.trim().is_empty() {
                let indent_end = line + indent.text_len();
                if let Some(extra) = indent.strip_prefix(target) {
                    if !extra.is_empty() {
                        edits.push(Edit::deletion(line + target.text_len(), indent_end));
                    }
                } else {
                    edits.push(Edit::replacement(target.to_string(), line, indent_end));
                }
            }
            line = next;
        }
        edits
    }

    /// Whether `offset` is inside a string literal's text, rather than between tokens.
    fn inside_string(&self, offset: TextSize) -> bool {
        let index = self.tokens.partition_point(|token| token.end() <= offset);
        self.tokens.get(index).is_some_and(|token| {
            token.start() < offset
                && matches!(
                    token.kind(),
                    TokenKind::String | TokenKind::FStringMiddle | TokenKind::TStringMiddle
                )
        })
    }

    /// The suite of `head` as an unwrap keeps it.
    fn suite(&self, head: &ClauseHead) -> Option<Suite> {
        let source = self.source;
        let first = head.body.first()?;
        let last = head.body.last()?;
        let colon = head.colon?;
        let on_header_line =
            !source[TextRange::new(colon.end(), first.start())].contains(['\n', '\r']);
        if on_header_line {
            return Some(Suite {
                kept: TextRange::new(first.start(), last.end()),
                indent: None,
            });
        }
        let indent =
            source[TextRange::new(source.line_start(first.start()), first.start())].to_string();
        // the first line with anything on it after the header: a comment above the first
        // statement is kept with it
        let mut line = source.full_line_end(colon.end());
        while line < first.start()
            && source[TextRange::new(line, source.line_end(line))]
                .trim()
                .is_empty()
        {
            line = source.full_line_end(line);
        }
        let content = line
            + source[TextRange::new(line, source.line_end(line))]
                .chars()
                .take_while(|c| c.is_whitespace())
                .map(TextLen::text_len)
                .sum::<TextSize>();
        // the comments after the last statement, at the suite's indentation or deeper, are the
        // suite's too
        let mut end = last.end();
        let mut line = source.full_line_end(last.end());
        while line < source.text_len() {
            let text = &source[TextRange::new(line, source.line_end(line))];
            if text
                .strip_prefix(indent.as_str())
                .is_some_and(|rest| rest.trim_start().starts_with('#'))
                && !self.inside_string(line)
            {
                end = source.line_end(line);
            } else if !text.trim().is_empty() {
                break;
            }
            line = source.full_line_end(line);
        }
        Some(Suite {
            kept: TextRange::new(content.min(first.start()), end),
            indent: Some(indent),
        })
    }

    /// Deleting the clause `head`, from the start of its line to the end of its suite's last.
    fn remove(&self, head: &ClauseHead, keyword: &str, refusal: Option<String>) -> UnwrapOption {
        let source = self.source;
        let title = format!("Remove '{keyword}...'");
        let start = source.line_start(head.range.start());
        let edits = match refusal {
            Some(reason) => Err(reason),
            None if !source[TextRange::new(start, head.range.start())]
                .chars()
                .all(char::is_whitespace) =>
            {
                Err("the clause does not start its line".to_string())
            }
            None => Ok(vec![Edit::deletion(
                start,
                source.full_line_end(head.range.end()),
            )]),
        };
        UnwrapOption {
            title,
            range: head.range,
            kept: Vec::new(),
            edits,
        }
    }
}

/// A suite an unwrap keeps.
struct Suite {
    /// From its first comment or statement to the end of its last statement, or of the comments
    /// after it.
    kept: TextRange,
    /// The indentation of its lines; `None` for a suite on its header's line.
    indent: Option<String>,
}

#[cfg(test)]
mod tests {
    use ruff_text_size::{Ranged, TextRange};

    use super::unwrap_options;
    use crate::tests::CursorTest;

    /// Every option at `<CURSOR>`, innermost first, each with the text it keeps and the file it
    /// leaves, or the reason it is refused.
    fn unwrap(source: &str) -> String {
        let test = CursorTest::builder().source("main.by", source).build();
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        let text = test.cursor.source.as_str();
        let options = match unwrap_options(&test.db, file, TextRange::empty(test.cursor.offset)) {
            Ok(options) => options,
            Err(reason) => return format!("refused: {reason}"),
        };
        options
            .iter()
            .map(|option| {
                let kept: Vec<&str> = option.kept.iter().map(|range| &text[*range]).collect();
                match &option.edits {
                    Ok(edits) => {
                        let mut result = text.to_string();
                        let mut edits = edits.clone();
                        edits.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
                        for edit in edits {
                            result.replace_range(
                                edit.range().to_std_range(),
                                edit.content().unwrap_or_default(),
                            );
                        }
                        format!("{} keeps {kept:?}\n{result}", option.title)
                    }
                    Err(reason) => format!("{}: refused ({reason})\n", option.title),
                }
            })
            .collect::<Vec<_>>()
            .join("---\n")
    }

    /// The suite takes the `if`'s place, one level shallower, with the comment above its first
    /// statement and the lines inside its brackets; the `else` goes with the `if`.
    #[test]
    fn unwrapping_an_if_keeps_its_suite_at_the_if_s_indentation() {
        insta::assert_snapshot!(unwrap(
            "\
def f():
    if x:
        # why
        a = 1<CURSOR>
        b(
            2,
        )
    else:
        c()
    d()
"
        ));
    }

    /// In an `else` both are offered: the statement replaced by that suite, or the clause gone.
    #[test]
    fn an_else_unwraps_or_is_removed() {
        insta::assert_snapshot!(unwrap(
            "\
if x:
    a()
elif y:
    b()
else:
    <CURSOR>c()
d()
"
        ));
    }

    /// The lines of a string that spans several lines are the string's, and stay as they are.
    #[test]
    fn a_multi_line_string_s_lines_are_left_alone() {
        insta::assert_snapshot!(unwrap(
            "\
with open(p) as f:
    text = \"\"\"
    keep
\"\"\"<CURSOR>
    g(text)
"
        ));
    }

    /// Without its loop, a `break` would leave the loop around it, or be an error.
    #[test]
    fn a_loop_whose_body_breaks_is_refused() {
        insta::assert_snapshot!(unwrap(
            "\
for a in b:
    if a:
        <CURSOR>break
"
        ));
    }

    /// A nested loop's own `break` does not stop the loop around it from unwrapping.
    #[test]
    fn a_nested_loop_s_break_is_its_own() {
        insta::assert_snapshot!(unwrap(
            "\
for a in b:
    <CURSOR>for c in d:
        break
"
        ));
    }

    /// Unwrapping a `try` keeps the path that raises nothing: the `try` suite, the `else` suite,
    /// then the `finally` suite.
    #[test]
    fn a_try_unwraps_into_the_path_that_raises_nothing() {
        insta::assert_snapshot!(unwrap(
            "\
try:
    <CURSOR>a()
except E:
    b()
else:
    c()
finally:
    d()
"
        ));
    }

    /// A `finally` runs however its `try` is left, so a `try` that can leave other than by
    /// running to its end does not unwrap: the `finally` suite would no longer run after a
    /// `return`, a `raise`, or a `break` out of the loop around it.
    #[test]
    fn a_try_that_can_leave_early_keeps_its_finally() {
        insta::assert_snapshot!(unwrap(
            "\
def f():
    try:
        if x:
            return 1
        <CURSOR>a()
    finally:
        d()
"
        ));
    }

    /// A `return` in a function the `try` suite defines does not leave the `try`.
    #[test]
    fn a_nested_function_s_return_is_its_own() {
        insta::assert_snapshot!(unwrap(
            "\
try:
    def g():
        return 1
    <CURSOR>a()
finally:
    d()
"
        ));
    }

    /// The comments after a suite's last statement, at its indentation or deeper, are the
    /// suite's, and stay with it.
    #[test]
    fn a_comment_ending_a_suite_is_kept() {
        insta::assert_snapshot!(unwrap(
            "\
if x:
    <CURSOR>a()

    # keep this
else:
    b()
c()
"
        ));
    }

    #[test]
    fn the_last_except_of_a_try_with_an_else_cannot_be_removed() {
        insta::assert_snapshot!(unwrap(
            "\
try:
    a()
except E:
    <CURSOR>b()
else:
    c()
"
        ));
    }

    /// A suite written on its header's line is kept as it is written.
    #[test]
    fn a_suite_on_the_header_line() {
        insta::assert_snapshot!(unwrap("if x: a(); <CURSOR>b()\n"));
    }

    #[test]
    fn a_file_that_does_not_parse_is_refused() {
        insta::assert_snapshot!(unwrap("if x:\n    a(<CURSOR>\n"));
    }
}
