//! The block structure and string literals of a file, as the parser sees them.
//!
//! An editor needs a handful of lexical facts to type in an indentation-delimited language: which
//! line opens a suite, where a compound statement ends, which clause keywords belong together,
//! what a string literal's escapes and interpolations are, and how much of a triple-quoted
//! string's indentation basedpython strips. Every one of those is decided by the parser, and an
//! editor that works them out again from the text gets them subtly different — a `match = 1`
//! read as a `match` statement, a `:` inside a string read as the end of a header, a margin drawn
//! where nothing is stripped. So they are read off the parse tree and its tokens here.

use ruff_db::PythonFile;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::token::{TokenKind, Tokens, parenthesized_range};
use ruff_python_ast::visitor::source_order::{
    SourceOrderVisitor, walk_expr, walk_f_string, walk_t_string,
};
use ruff_python_ast::{
    self as ast, AnyNodeRef, AnyStringFlags, Expr, InterpolatedStringElement,
    InterpolatedStringElements, Stmt, StringFlags,
};
use ruff_python_trivia::basedpython::{TripleQuotedDedent, dedent_triple_quoted_body};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::Db;

/// The block structure and string literals of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxOutline {
    /// The module's statements, in source order, each carrying the suites nested under it.
    pub statements: Vec<OutlineStatement>,
    /// Every string literal part in the file, in source order.
    pub strings: Vec<OutlineString>,
}

/// One statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineStatement {
    /// The whole statement: from its first token (a decorator, for a decorated definition) to
    /// the end of its last clause's body.
    pub range: TextRange,
    /// The clauses of a compound statement, in source order. Empty for a simple statement.
    pub clauses: Vec<OutlineClause>,
    /// The basedpython modifiers the definition was declared with (`data`, `frozen data`,
    /// `enum class`, `class` in `class def`, ...), in source order.
    pub modifiers: Vec<OutlineModifier>,
    /// For an expression statement that is a call, the call.
    pub call: Option<OutlineCall>,
}

/// One clause of a compound statement: `if`, `elif`, `else`, `case`, the `def` of a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineClause {
    /// The clause keyword. `None` for a clause that has none, which is a basedpython trailing
    /// lambda (`items.each:`).
    pub keyword: Option<TextRange>,
    /// The `:` that ends the header and opens the suite. `None` when there is none to find, which
    /// is a header the parser recovered from, or the `let` clause of a `let ... := ... else:`.
    pub colon: Option<TextRange>,
    /// From the start of the header to the end of the body — or of the header, when the body is
    /// empty.
    pub range: TextRange,
    /// The statements of the suite.
    pub body: Vec<OutlineStatement>,
}

/// A basedpython modifier keyword, as the parser read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineModifier {
    /// The keyword or keywords, without the whitespace that follows them.
    pub range: TextRange,
    /// What the parser recorded it as, e.g. `data_class`, `frozen_data_class`, `enum_def`,
    /// `classmethod`, `protocol_class`, `final`.
    pub name: String,
}

/// A call made as a statement of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineCall {
    /// The expression being called.
    pub callee: TextRange,
    /// From the start of the first argument to the end of the last, parentheses around either
    /// included. `None` for a call with no arguments.
    pub arguments: Option<TextRange>,
    /// Whether every argument is positional and none is unpacked with `*` or `**`.
    pub positional_only: bool,
}

/// One string literal part: one pair of quotes and what is between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineString {
    /// The part, prefix and quotes included.
    pub range: TextRange,
    /// How many leading characters basedpython strips from each line of this literal, when it
    /// strips any.
    ///
    /// The rule is [`dedent_triple_quoted_body`]'s, the one the transpiler applies, so this is
    /// set exactly when the program's string differs from the literal as written: a lone
    /// triple-quoted string in a basedpython file whose content starts on the line after the
    /// opening quotes and whose closing quotes sit on a line of their own.
    pub stripped_indent: Option<TextSize>,
    /// The `{...}` interpolations of an f-string or t-string, braces included.
    pub interpolations: Vec<TextRange>,
    /// The escape sequences in the literal text. A backslash Python does not recognise is still
    /// reported, as two characters: Python keeps it verbatim, and that is worth seeing.
    pub escapes: Vec<TextRange>,
}

/// The outline of `file`.
pub fn syntax_outline(db: &dyn Db, file: PythonFile<'_>) -> SyntaxOutline {
    let parsed = parsed_module(db, file).load(db);
    let source = source_text(db, file.file(db));
    let builder = OutlineBuilder {
        source: source.as_str(),
        tokens: parsed.tokens(),
    };

    let statements = builder.statements(parsed.suite());

    let mut strings = StringCollector {
        source: source.as_str(),
        dedents: file.file(db).source_type(db).is_basedpython(),
        dedentable: None,
        strings: Vec::new(),
    };
    strings.visit_body(parsed.suite());

    SyntaxOutline {
        statements,
        strings: strings.strings,
    }
}

/// The keywords that light up together with the keyword at `offset`, in source order.
///
/// That is a compound statement's clause keywords (`if`/`elif`/`else`, `try`/`except`/`finally`,
/// `match`/`case`, a loop and its `else`), plus the statements that leave a block from inside
/// its body: a `def`'s `return`s and `raise`s, a loop's `break`s and `continue`s. Each of those
/// binds to the nearest enclosing block of its kind, so a nested `def` keeps its own `return`s and
/// a nested loop its own `break`s; a `break` in a loop's `else` belongs to the loop outside it,
/// since that clause is not the loop's body.
///
/// `None` when `offset` is not on such a keyword, or the keyword has nothing to pair with.
pub(crate) fn keyword_family(
    db: &dyn Db,
    file: PythonFile<'_>,
    offset: TextSize,
) -> Option<Vec<TextRange>> {
    let parsed = parsed_module(db, file).load(db);
    let source = source_text(db, file.file(db));
    let tokens = parsed.tokens();

    let keyword = keyword_at(tokens, offset)?;
    let builder = OutlineBuilder {
        source: source.as_str(),
        tokens,
    };
    let mut path = Vec::new();
    let mut family = builder.family(parsed.suite(), keyword, &mut path)?;
    if family.len() < 2 {
        return None;
    }
    family.sort_by_key(ruff_text_size::Ranged::start);
    Some(family)
}

/// The keyword token touching `offset`, if there is one a family can start from.
fn keyword_at(tokens: &Tokens, offset: TextSize) -> Option<TextRange> {
    let is_family_keyword = |kind: TokenKind| {
        matches!(
            kind,
            TokenKind::If
                | TokenKind::Elif
                | TokenKind::Else
                | TokenKind::For
                | TokenKind::While
                | TokenKind::Try
                | TokenKind::Except
                | TokenKind::Finally
                | TokenKind::Match
                | TokenKind::Case
                | TokenKind::Def
                | TokenKind::Return
                | TokenKind::Raise
                | TokenKind::Break
                | TokenKind::Continue
        )
    };
    let index = tokens.partition_point(|token| token.end() < offset);
    tokens[index..]
        .iter()
        .take_while(|token| token.start() <= offset)
        .find(|token| is_family_keyword(token.kind()))
        .map(Ranged::range)
}

struct OutlineBuilder<'a> {
    source: &'a str,
    tokens: &'a Tokens,
}

impl OutlineBuilder<'_> {
    fn statements(&self, suite: &[Stmt]) -> Vec<OutlineStatement> {
        suite.iter().map(|stmt| self.statement(stmt)).collect()
    }

    fn statement(&self, stmt: &Stmt) -> OutlineStatement {
        OutlineStatement {
            range: stmt.range(),
            clauses: self.clauses(stmt),
            modifiers: self.modifiers(stmt),
            call: self.call(stmt),
        }
    }

    /// The clauses of `stmt`, each with its body.
    fn clauses(&self, stmt: &Stmt) -> Vec<OutlineClause> {
        self.clause_heads(stmt)
            .into_iter()
            .map(|head| {
                let body = self.statements(head.body);
                OutlineClause {
                    keyword: head.keyword,
                    colon: head.colon,
                    range: head.range,
                    body,
                }
            })
            .collect()
    }

    /// The clauses of `stmt` without their bodies' outlines, which is all a family needs.
    fn clause_heads<'s>(&self, stmt: &'s Stmt) -> Vec<ClauseHead<'s>> {
        let mut heads = Vec::new();
        match stmt {
            Stmt::If(if_stmt) => {
                let keyword = self.token_at(if_stmt.start());
                heads.push(self.head(keyword, if_stmt.start(), if_stmt.test.end(), &if_stmt.body));
                for clause in &if_stmt.elif_else_clauses {
                    let keyword = self.token_at(clause.start());
                    let hint = clause.test.as_ref().map_or_else(
                        || keyword.map_or(clause.start(), ruff_text_size::TextRange::end),
                        ruff_text_size::Ranged::end,
                    );
                    heads.push(self.head(keyword, clause.start(), hint, &clause.body));
                }
            }
            Stmt::For(for_stmt) => {
                let keyword = self.first_token(for_stmt.start(), TokenKind::For);
                heads.push(self.head(
                    keyword,
                    for_stmt.start(),
                    for_stmt.iter.end(),
                    &for_stmt.body,
                ));
                self.else_head(&mut heads, &for_stmt.orelse, TokenKind::Else);
            }
            Stmt::While(while_stmt) => {
                let keyword = self.token_at(while_stmt.start());
                heads.push(self.head(
                    keyword,
                    while_stmt.start(),
                    while_stmt.test.end(),
                    &while_stmt.body,
                ));
                self.else_head(&mut heads, &while_stmt.orelse, TokenKind::Else);
            }
            Stmt::Try(try_stmt) => {
                let keyword = self.token_at(try_stmt.start());
                let hint = keyword.map_or(try_stmt.start(), ruff_text_size::TextRange::end);
                heads.push(self.head(keyword, try_stmt.start(), hint, &try_stmt.body));
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    let keyword = self.token_at(handler.start());
                    let hint = handler
                        .name
                        .as_ref()
                        .map(ruff_text_size::Ranged::end)
                        .or_else(|| handler.type_.as_ref().map(|type_| type_.end()))
                        .unwrap_or_else(|| {
                            keyword.map_or(handler.start(), ruff_text_size::TextRange::end)
                        });
                    heads.push(self.head(keyword, handler.start(), hint, &handler.body));
                }
                self.else_head(&mut heads, &try_stmt.orelse, TokenKind::Else);
                self.else_head(&mut heads, &try_stmt.finalbody, TokenKind::Finally);
            }
            Stmt::With(with_stmt) => {
                let keyword = self.first_token(with_stmt.start(), TokenKind::With);
                let hint = with_stmt.items.last().map_or_else(
                    || keyword.map_or(with_stmt.start(), ruff_text_size::TextRange::end),
                    ruff_text_size::Ranged::end,
                );
                heads.push(self.head(keyword, with_stmt.start(), hint, &with_stmt.body));
            }
            Stmt::Match(match_stmt) => {
                // `match` is a soft keyword, and the clause keyword is simply the statement's
                // first token: the parser only makes a match statement out of one that is.
                let keyword = self.first_token_any(match_stmt.start());
                heads.push(self.head(keyword, match_stmt.start(), match_stmt.subject.end(), &[]));
                for case in &match_stmt.cases {
                    let keyword = self.first_token_any(case.start());
                    let hint = case
                        .guard
                        .as_ref()
                        .map_or_else(|| case.pattern.end(), |guard| guard.end());
                    heads.push(self.head(keyword, case.start(), hint, &case.body));
                }
            }
            Stmt::FunctionDef(function) if function.is_trailing_lambda => {
                // A statement-level `<call>:` whose suite is the lambda's body. There is no
                // keyword; the header is the call.
                let hint = function
                    .decorator_list
                    .first()
                    .map_or(function.start(), |decorator| decorator.expression.end());
                heads.push(self.head(None, function.start(), hint, &function.body));
            }
            Stmt::FunctionDef(function) => {
                let keyword = self.token_before(function.name.start());
                let hint = [
                    Some(function.parameters.end()),
                    function.returns.as_ref().map(|returns| returns.end()),
                    function.raises.as_ref().map(|raises| raises.end()),
                ]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(function.name.end());
                let start = keyword.map_or(function.start(), ruff_text_size::TextRange::start);
                heads.push(self.head(keyword, start, hint, &function.body));
            }
            Stmt::ClassDef(class) => {
                let keyword = self.token_before(class.name.start());
                let hint = [
                    Some(class.name.end()),
                    class.type_params.as_ref().map(|params| params.end()),
                    class.arguments.as_ref().map(|arguments| arguments.end()),
                ]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(class.name.end());
                let start = keyword.map_or(class.start(), ruff_text_size::TextRange::start);
                heads.push(self.head(keyword, start, hint, &class.body));
            }
            Stmt::Let(let_stmt) if !let_stmt.orelse.is_empty() => {
                // `let <pattern> := <subject> else:` — the `let` half opens no suite, and the
                // `else` is a clause of the statement like a loop's.
                let keyword = self.token_at(let_stmt.start());
                heads.push(ClauseHead {
                    keyword,
                    colon: None,
                    range: TextRange::new(let_stmt.start(), let_stmt.value.end()),
                    body: &[],
                });
                self.else_head(&mut heads, &let_stmt.orelse, TokenKind::Else);
            }
            _ => {}
        }
        heads
    }

    /// A clause that starts at `start`, whose header's last component ends at `hint`.
    fn head<'s>(
        &self,
        keyword: Option<TextRange>,
        start: TextSize,
        hint: TextSize,
        body: &'s [Stmt],
    ) -> ClauseHead<'s> {
        let colon = self.colon_after(hint);
        let header_end = colon.map_or(hint, ruff_text_size::TextRange::end);
        let end = body
            .last()
            .map_or(header_end, ruff_text_size::Ranged::end)
            .max(header_end);
        ClauseHead {
            keyword,
            colon,
            range: TextRange::new(start, end),
            body,
        }
    }

    /// The `else` or `finally` clause heading `body`, which has no node of its own: its keyword is
    /// the last one of `kind` before the body's first statement.
    fn else_head<'s>(&self, heads: &mut Vec<ClauseHead<'s>>, body: &'s [Stmt], kind: TokenKind) {
        let Some(first) = body.first() else {
            return;
        };
        let before = &self.tokens[..self
            .tokens
            .partition_point(|token| token.start() < first.start())];
        let Some(keyword) = before
            .iter()
            .rev()
            .find(|token| {
                !matches!(
                    token.kind(),
                    TokenKind::Colon
                        | TokenKind::Newline
                        | TokenKind::NonLogicalNewline
                        | TokenKind::Indent
                        | TokenKind::Dedent
                        | TokenKind::Comment
                )
            })
            .filter(|token| token.kind() == kind)
        else {
            return;
        };
        heads.push(self.head(Some(keyword.range()), keyword.start(), keyword.end(), body));
    }

    /// The header's `:` — the first one after `hint` outside brackets, before the logical line
    /// ends. Searching from the header's last component rather than from its keyword is what keeps
    /// a lambda's `:` in the header out of it.
    fn colon_after(&self, hint: TextSize) -> Option<TextRange> {
        let start = self.tokens.partition_point(|token| token.start() < hint);
        let mut depth = 0i32;
        for token in &self.tokens[start..] {
            match token.kind() {
                TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace => depth += 1,
                TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace => depth -= 1,
                TokenKind::Colon if depth <= 0 => return Some(token.range()),
                TokenKind::Newline => return None,
                _ => {}
            }
        }
        None
    }

    /// The token starting exactly at `offset`.
    fn token_at(&self, offset: TextSize) -> Option<TextRange> {
        self.first_token_any(offset)
            .filter(|range| range.start() == offset)
    }

    /// The first token starting at or after `offset`, passing over the zero-width `Dedent`s
    /// that start where a dedented clause keyword does.
    fn first_token_any(&self, offset: TextSize) -> Option<TextRange> {
        let index = self.tokens.partition_point(|token| token.start() < offset);
        self.tokens[index..]
            .iter()
            .find(|token| !token.range().is_empty())
            .map(Ranged::range)
    }

    /// The first token of `kind` at or after `offset`.
    fn first_token(&self, offset: TextSize, kind: TokenKind) -> Option<TextRange> {
        let index = self.tokens.partition_point(|token| token.start() < offset);
        self.tokens[index..]
            .iter()
            .find(|token| token.kind() == kind)
            .map(Ranged::range)
    }

    /// The last significant token ending at or before `offset`.
    fn token_before(&self, offset: TextSize) -> Option<TextRange> {
        let index = self.tokens.partition_point(|token| token.end() <= offset);
        self.tokens[..index]
            .iter()
            .rev()
            .find(|token| !token.kind().is_trivia())
            .map(Ranged::range)
    }

    /// The modifiers a definition was declared with: the decorators the parser synthesised from
    /// basedpython keywords, which unlike a written decorator do not start with `@`.
    fn modifiers(&self, stmt: &Stmt) -> Vec<OutlineModifier> {
        let decorators = match stmt {
            Stmt::FunctionDef(function) if !function.is_trailing_lambda => &function.decorator_list,
            Stmt::ClassDef(class) => &class.decorator_list,
            _ => return Vec::new(),
        };
        decorators
            .iter()
            .filter_map(|decorator| {
                let text = self.source.get(decorator.range().to_std_range())?;
                if text.starts_with('@') {
                    return None;
                }
                let Expr::Name(name) = &decorator.expression else {
                    return None;
                };
                let trimmed = TextSize::of(text.trim_end());
                if trimmed == TextSize::ZERO {
                    return None;
                }
                Some(OutlineModifier {
                    range: TextRange::at(decorator.start(), trimmed),
                    name: name.id.to_string(),
                })
            })
            .collect()
    }

    fn call(&self, stmt: &Stmt) -> Option<OutlineCall> {
        let Stmt::Expr(ast::StmtExpr { value, .. }) = stmt else {
            return None;
        };
        let Expr::Call(call) = value.as_ref() else {
            return None;
        };
        let parent = AnyNodeRef::from(&call.arguments);
        let full = |expr: &Expr| {
            parenthesized_range(expr.into(), parent, self.tokens).unwrap_or(expr.range())
        };
        let arguments = call
            .arguments
            .iter_source_order()
            .map(|argument| match argument {
                ast::ArgOrKeyword::Arg(expr) => full(expr),
                ast::ArgOrKeyword::Keyword(keyword) => keyword.range(),
            })
            .reduce(ruff_text_size::TextRange::cover);
        let positional_only = call.arguments.keywords.is_empty()
            && call
                .arguments
                .args
                .iter()
                .all(|argument| !argument.is_starred_expr());
        Some(OutlineCall {
            callee: call.func.range(),
            arguments,
            positional_only,
        })
    }
}

struct ClauseHead<'s> {
    keyword: Option<TextRange>,
    colon: Option<TextRange>,
    range: TextRange,
    body: &'s [Stmt],
}

/// A block enclosing the statement being looked at, as a `return` or a `break` sees it.
#[derive(Clone, Copy)]
struct Enclosing<'s> {
    stmt: &'s Stmt,
    /// Whether the path runs through the block's first suite — a loop's body rather than its
    /// `else`, which a `break` does not belong to.
    in_body: bool,
}

impl OutlineBuilder<'_> {
    /// The family of `keyword` among `suite` and everything nested in it. `path` holds the blocks
    /// enclosing `suite`, innermost last.
    fn family<'s>(
        &self,
        suite: &'s [Stmt],
        keyword: TextRange,
        path: &mut Vec<Enclosing<'s>>,
    ) -> Option<Vec<TextRange>> {
        for stmt in suite {
            if !stmt.range().contains_range(keyword) {
                continue;
            }
            match stmt {
                Stmt::Return(_) | Stmt::Raise(_) if stmt.start() == keyword.start() => {
                    return path.iter().rev().find_map(|block| match block.stmt {
                        Stmt::FunctionDef(_) => Some(Some(self.own_family(block.stmt))),
                        Stmt::ClassDef(_) => Some(None),
                        _ => None,
                    })?;
                }
                Stmt::Break(_) | Stmt::Continue(_) if stmt.start() == keyword.start() => {
                    return path.iter().rev().find_map(|block| match block.stmt {
                        Stmt::For(_) | Stmt::While(_) if block.in_body => {
                            Some(Some(self.own_family(block.stmt)))
                        }
                        Stmt::FunctionDef(_) | Stmt::ClassDef(_) => Some(None),
                        _ => None,
                    })?;
                }
                _ => {}
            }

            let heads = self.clause_heads(stmt);
            if heads.iter().any(|head| head.keyword == Some(keyword)) {
                return Some(self.own_family(stmt));
            }
            for (index, head) in heads.iter().enumerate() {
                path.push(Enclosing {
                    stmt,
                    in_body: index == 0,
                });
                let found = self.family(head.body, keyword, path);
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            return None;
        }
        None
    }

    /// What lights up with one of `stmt`'s own clause keywords.
    fn own_family(&self, stmt: &Stmt) -> Vec<TextRange> {
        let keywords = self
            .clause_heads(stmt)
            .into_iter()
            .filter_map(|head| head.keyword);
        match stmt {
            Stmt::FunctionDef(function) => {
                let mut family: Vec<_> = keywords.collect();
                self.exits_in(&function.body, JumpKind::Exit, &mut family);
                family
            }
            Stmt::For(ast::StmtFor { body, .. }) | Stmt::While(ast::StmtWhile { body, .. }) => {
                let mut family: Vec<_> = keywords.collect();
                self.exits_in(body, JumpKind::Jump, &mut family);
                family
            }
            _ => keywords.collect(),
        }
    }

    /// The exits of `kind` in `suite`, not counting those inside a nested block that owns them: a
    /// nested function owns its exits, and a nested loop, function or class its jumps. A nested
    /// loop's `else` is not its body, so a jump there still belongs out here.
    fn exits_in(&self, suite: &[Stmt], kind: JumpKind, found: &mut Vec<TextRange>) {
        for stmt in suite {
            match (stmt, kind) {
                (Stmt::Return(_) | Stmt::Raise(_), JumpKind::Exit)
                | (Stmt::Break(_) | Stmt::Continue(_), JumpKind::Jump) => {
                    found.extend(self.token_at(stmt.start()));
                }
                (Stmt::FunctionDef(_) | Stmt::ClassDef(_), _) => {}
                (
                    Stmt::For(ast::StmtFor { orelse, .. })
                    | Stmt::While(ast::StmtWhile { orelse, .. }),
                    JumpKind::Jump,
                ) => self.exits_in(orelse, kind, found),
                _ => {
                    for head in self.clause_heads(stmt) {
                        self.exits_in(head.body, kind, found);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JumpKind {
    /// `return` and `raise`, which leave a function.
    Exit,
    /// `break` and `continue`, which leave or restart a loop.
    Jump,
}

/// Collects every string literal part, with what is inside it.
struct StringCollector<'a> {
    source: &'a str,
    /// Whether this file's triple-quoted strings are dedented, which is basedpython's rule and not
    /// python's.
    dedents: bool,
    /// The part the expression being visited consists of, when it is the lone part of a string
    /// expression — the only shape the transpiler dedents.
    dedentable: Option<TextRange>,
    strings: Vec<OutlineString>,
}

impl StringCollector<'_> {
    fn push(
        &mut self,
        range: TextRange,
        flags: AnyStringFlags,
        interpolations: Vec<TextRange>,
        literal_runs: &[TextRange],
    ) {
        let stripped_indent = if self.dedents
            && self.dedentable == Some(range)
            && flags.is_triple_quoted()
            && !flags.is_unclosed()
            && !flags.is_byte_string()
        {
            let body = TextRange::new(
                range.start() + flags.opener_len(),
                range.end() - flags.closer_len(),
            );
            match self
                .source
                .get(body.to_std_range())
                .map(dedent_triple_quoted_body)
            {
                Some(TripleQuotedDedent::Dedents { indent, .. }) => Some(TextSize::of(indent)),
                _ => None,
            }
        } else {
            None
        };

        let mut escapes = Vec::new();
        if !flags.prefix().is_raw() {
            for run in literal_runs {
                escapes_in(self.source, *run, flags.is_byte_string(), &mut escapes);
            }
        }

        self.strings.push(OutlineString {
            range,
            stripped_indent,
            interpolations,
            escapes,
        });
    }

    /// The literal text of `elements`, and the top-level interpolations among them. A format
    /// spec's literal text is literal text too, and is where its escapes are.
    fn elements(
        elements: &InterpolatedStringElements,
        interpolations: &mut Vec<TextRange>,
        literal_runs: &mut Vec<TextRange>,
        top_level: bool,
    ) {
        for element in elements {
            match element {
                InterpolatedStringElement::Literal(literal) => literal_runs.push(literal.range()),
                InterpolatedStringElement::Interpolation(interpolation) => {
                    if top_level {
                        interpolations.push(interpolation.range());
                    }
                    if let Some(spec) = &interpolation.format_spec {
                        Self::elements(&spec.elements, interpolations, literal_runs, false);
                    }
                }
            }
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for StringCollector<'_> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        let outer = self.dedentable;
        self.dedentable = match expr {
            Expr::StringLiteral(string) => string.as_single_part_string().map(Ranged::range),
            Expr::FString(string) => string.as_single_part_fstring().map(Ranged::range),
            Expr::TString(string) => string.as_single_part_tstring().map(Ranged::range),
            _ => None,
        };
        walk_expr(self, expr);
        self.dedentable = outer;
    }

    fn visit_string_literal(&mut self, string: &'a ast::StringLiteral) {
        let content = string.content_range();
        self.push(string.range(), string.flags.into(), Vec::new(), &[content]);
    }

    fn visit_bytes_literal(&mut self, bytes: &'a ast::BytesLiteral) {
        let content = bytes.content_range();
        self.push(bytes.range(), bytes.flags.into(), Vec::new(), &[content]);
    }

    fn visit_f_string(&mut self, f_string: &'a ast::FString) {
        let mut interpolations = Vec::new();
        let mut runs = Vec::new();
        Self::elements(&f_string.elements, &mut interpolations, &mut runs, true);
        self.push(
            f_string.range(),
            f_string.flags.into(),
            interpolations,
            &runs,
        );
        // Inside an interpolation is an expression, which may hold strings of its own.
        let outer = self.dedentable.take();
        walk_f_string(self, f_string);
        self.dedentable = outer;
    }

    fn visit_t_string(&mut self, t_string: &'a ast::TString) {
        let mut interpolations = Vec::new();
        let mut runs = Vec::new();
        Self::elements(&t_string.elements, &mut interpolations, &mut runs, true);
        self.push(
            t_string.range(),
            t_string.flags.into(),
            interpolations,
            &runs,
        );
        let outer = self.dedentable.take();
        walk_t_string(self, t_string);
        self.dedentable = outer;
    }
}

/// The escape sequences in the literal text `run`, by Python's rules: `\N{...}`, `\u` and `\U`
/// are escapes in a `str` and not in `bytes`, `\x` takes two hex digits and an octal escape up to
/// three, and a line continuation is an escape of its line break.
fn escapes_in(source: &str, run: TextRange, bytes: bool, found: &mut Vec<TextRange>) {
    let Some(text) = source.get(run.to_std_range()) else {
        return;
    };
    let chars = text.as_bytes();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != b'\\' || index + 1 >= chars.len() {
            index += 1;
            continue;
        }
        let rest = &chars[index + 1..];
        let hex_run = |count: usize| {
            if rest.len() > count && rest[1..=count].iter().all(u8::is_ascii_hexdigit) {
                count + 2
            } else {
                2
            }
        };
        let len = match rest[0] {
            b'x' => hex_run(2),
            b'u' if !bytes => hex_run(4),
            b'U' if !bytes => hex_run(8),
            b'N' if !bytes && rest.get(1) == Some(&b'{') => rest
                .iter()
                .position(|&c| c == b'}')
                .map_or(2, |close| close + 2),
            b'0'..=b'7' => {
                1 + rest
                    .iter()
                    .take(3)
                    .take_while(|c| matches!(c, b'0'..=b'7'))
                    .count()
            }
            b'\r' if rest.get(1) == Some(&b'\n') => 3,
            // Any other character, a multi-byte one included: the escape is the backslash and
            // that whole character.
            _ => 1 + text[index + 1..].chars().next().map_or(1, char::len_utf8),
        };
        let start = run.start() + TextSize::try_from(index).unwrap_or_default();
        found.push(TextRange::at(
            start,
            TextSize::try_from(len).unwrap_or_default(),
        ));
        index += len;
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use ruff_text_size::TextRange;

    use super::{OutlineStatement, keyword_family, syntax_outline};
    use crate::tests::CursorTest;

    fn test(path: &str, source: &str) -> CursorTest {
        let source = if source.contains("<CURSOR>") {
            source.to_string()
        } else {
            format!("<CURSOR>{source}")
        };
        CursorTest::builder().source(path, source).build()
    }

    /// The statements of a basedpython file, one line per clause header or simple statement,
    /// indented by nesting: a header is shown up to its `:`, marked when it has no keyword or no
    /// colon, and a statement's modifiers and call follow it in brackets.
    fn statements(source: &str) -> String {
        let test = test("main.by", source);
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        let outline = syntax_outline(&test.db, file);
        let text = test.cursor.source.as_str();
        let mut out = String::new();
        render(&outline.statements, text, 0, &mut out);
        out
    }

    fn render(statements: &[OutlineStatement], text: &str, depth: usize, out: &mut String) {
        let slice = |range: TextRange| &text[range.to_std_range()];
        for statement in statements {
            let indent = "  ".repeat(depth);
            if statement.clauses.is_empty() {
                let first_line = slice(statement.range).lines().next().unwrap_or_default();
                write!(out, "{indent}{first_line}").unwrap();
            }
            for (index, clause) in statement.clauses.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                }
                let header_end = clause
                    .colon
                    .map_or(clause.range.end(), ruff_text_size::TextRange::end);
                let header = slice(TextRange::new(clause.range.start(), header_end));
                write!(out, "{indent}{}", header.replace('\n', "⏎")).unwrap();
                if clause.keyword.is_none() {
                    out.push_str(" [no keyword]");
                }
                if clause.colon.is_none() {
                    out.push_str(" [no colon]");
                }
                if index == 0 {
                    for modifier in &statement.modifiers {
                        write!(out, " [{} `{}`]", modifier.name, slice(modifier.range)).unwrap();
                    }
                }
                if !clause.body.is_empty() {
                    out.push('\n');
                    render(&clause.body, text, depth + 1, out);
                    // `render` ends every statement with a newline, and so does this loop.
                    out.pop();
                }
            }
            if let Some(call) = &statement.call {
                write!(
                    out,
                    " [call `{}` with `{}`{}]",
                    slice(call.callee),
                    call.arguments.map(slice).unwrap_or_default(),
                    if call.positional_only {
                        ", positional only"
                    } else {
                        ""
                    }
                )
                .unwrap();
            }
            out.push('\n');
        }
    }

    /// Every string part: its text, what basedpython strips, and its interpolations and escapes.
    fn strings(path: &str, source: &str) -> String {
        let test = test(path, source);
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        let outline = syntax_outline(&test.db, file);
        let text = test.cursor.source.as_str();
        let slice = |range: TextRange| text[range.to_std_range()].replace('\n', "⏎");
        let mut out = String::new();
        for string in &outline.strings {
            write!(out, "{}", slice(string.range)).unwrap();
            if let Some(indent) = string.stripped_indent {
                write!(out, " strips {}", u32::from(indent)).unwrap();
            }
            for interpolation in &string.interpolations {
                write!(out, " interpolation `{}`", slice(*interpolation)).unwrap();
            }
            for escape in &string.escapes {
                write!(out, " escape `{}`", slice(*escape)).unwrap();
            }
            out.push('\n');
        }
        out
    }

    /// The keywords that light up with the one under `<CURSOR>`, or `none`.
    fn family(source: &str) -> String {
        let test = test("main.by", source);
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        let text = test.cursor.source.as_str();
        match keyword_family(&test.db, file, test.cursor.offset) {
            None => "none".to_string(),
            Some(family) => family
                .iter()
                .map(|range| {
                    let line = text[..range.start().to_usize()].matches('\n').count() + 1;
                    format!("{}@{line}", &text[range.to_std_range()])
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    #[test]
    fn a_soft_keyword_used_as_a_name_opens_no_suite() {
        assert_eq!(
            statements("match: int = 1\ncase = match\n"),
            "match: int = 1\ncase = match\n"
        );
    }

    #[test]
    fn colons_in_strings_brackets_and_lambdas_are_not_the_header_colon() {
        assert_eq!(
            statements(
                "if d[\":\"] == {1: 2} and (lambda: 1)():  # a: comment\n    pass\nelif x:\n    y = \"a:\"\nelse: pass\n"
            ),
            "if d[\":\"] == {1: 2} and (lambda: 1)():\n  pass\nelif x:\n  y = \"a:\"\nelse:\n  pass\n"
        );
    }

    #[test]
    fn a_header_wrapped_over_lines_ends_at_its_colon() {
        assert_eq!(
            statements("def f(\n    a: int,\n) -> int:\n    return a\n"),
            "def f(⏎    a: int,⏎) -> int:\n  return a\n"
        );
    }

    #[test]
    fn the_clauses_of_every_compound_statement() {
        assert_eq!(
            statements(
                "\
try:
    pass
except (A, B) as e:
    pass
except C:
    pass
else:
    pass
finally:
    pass
for x in y:
    break
else:
    pass
while z:
    continue
else:
    pass
async def f():
    async for a in b:
        pass
    async with c as d, e:
        pass
match p:
    case [1, 2] if q:
        pass
    case _:
        pass
"
            ),
            "\
try:
  pass
except (A, B) as e:
  pass
except C:
  pass
else:
  pass
finally:
  pass
for x in y:
  break
else:
  pass
while z:
  continue
else:
  pass
def f():
  async for a in b:
    pass
  async with c as d, e:
    pass
match p:
case [1, 2] if q:
  pass
case _:
  pass
"
        );
    }

    #[test]
    fn a_header_being_typed_still_opens_its_suite() {
        assert_eq!(statements("def f():\n    if x:"), "def f():\n  if x:\n");
    }

    #[test]
    fn an_inline_suite_is_the_body_of_its_header() {
        assert_eq!(
            statements("if x: return 1\nwhile y: pass\n"),
            "if x:\n  return 1\nwhile y:\n  pass\n"
        );
    }

    #[test]
    fn a_definition_keeps_its_decorators_and_modifiers_but_its_clause_starts_at_the_keyword() {
        assert_eq!(
            statements(
                "\
@dec
frozen data class P:
    x: int
enum class Color:
    RED
class C:
    class def make(cls): ...
    static def s(): ...
"
            ),
            "\
class P: [frozen_data_class `frozen data`]
  x: int
class Color: [enum_def `enum class`]
  RED
class C:
  def make(cls): [classmethod `class`]
    ...
  def s(): [static `static`]
    ...
"
        );
    }

    #[test]
    fn a_let_with_an_else_is_a_compound_statement() {
        assert_eq!(
            statements("let A(foo) := value else:\n    return None\nlet b := 1\n"),
            "let A(foo) := value [no colon]\nelse:\n  return None\nlet b := 1\n"
        );
    }

    #[test]
    fn a_call_statement_says_what_it_calls_and_how() {
        assert_eq!(
            statements("print(a, (b + c))\nprint(x, sep=\"\")\nprint(*xs)\nprint()\nf(x)(y)\n"),
            "\
print(a, (b + c)) [call `print` with `a, (b + c)`, positional only]
print(x, sep=\"\") [call `print` with `x, sep=\"\"`]
print(*xs) [call `print` with `*xs`]
print() [call `print` with ``, positional only]
f(x)(y) [call `f(x)` with `y`, positional only]
"
        );
    }

    #[test]
    fn basedpython_strips_what_the_transpiler_strips() {
        assert_eq!(
            strings(
                "main.by",
                "\
def f():
    content_is_the_margin = \"\"\"
            eight
          six
        \"\"\"
    opening_line_has_text = \"\"\"Summary.
        more
        \"\"\"
    closing_shares_a_line = \"\"\"
        text\"\"\"
    closing_past_the_content = \"\"\"
    a
        \"\"\"
    concatenated = \"\"\"
        a
        \"\"\" \"b\"
    bytes = b\"\"\"
        a
        \"\"\"
    formatted = f\"\"\"
        {x}
        \"\"\"
"
            ),
            "\
\"\"\"⏎            eight⏎          six⏎        \"\"\" strips 10
\"\"\"Summary.⏎        more⏎        \"\"\"
\"\"\"⏎        text\"\"\"
\"\"\"⏎    a⏎        \"\"\"
\"\"\"⏎        a⏎        \"\"\"
\"b\"
b\"\"\"⏎        a⏎        \"\"\"
f\"\"\"⏎        {x}⏎        \"\"\" strips 8 interpolation `{x}`
"
        );
    }

    #[test]
    fn python_strips_nothing() {
        assert_eq!(
            strings("main.py", "x = \"\"\"\n    a\n    \"\"\"\n"),
            "\"\"\"⏎    a⏎    \"\"\"\n"
        );
    }

    #[test]
    fn escapes_follow_pythons_rules() {
        assert_eq!(
            strings(
                "main.by",
                r#"a = "\n\x41\u00e9\N{EM DASH}\101\q\"" + b"\u00e9\x41" + r"\n" + f"\t{x!r:\n>{w}}{{\n}}"
"#
            ),
            r#""\n\x41\u00e9\N{EM DASH}\101\q\"" escape `\n` escape `\x41` escape `\u00e9` escape `\N{EM DASH}` escape `\101` escape `\q` escape `\"`
b"\u00e9\x41" escape `\u` escape `\x41`
r"\n"
f"\t{x!r:\n>{w}}{{\n}}" interpolation `{x!r:\n>{w}}` escape `\t` escape `\n` escape `\n`
"#
        );
    }

    #[test]
    fn an_if_pairs_with_its_own_elif_and_else() {
        assert_eq!(
            family(
                "<CURSOR>if a:\n    x = b if c else d\nelif e:\n    pass\nelse:\n    pass\nif f:\n    pass\n"
            ),
            "if@1 elif@3 else@5"
        );
        assert_eq!(family("if a:\n    x = b if c <CURSOR>else d\n"), "none");
        assert_eq!(family("<CURSOR>if a:\n    pass\n"), "none");
    }

    #[test]
    fn a_try_pairs_with_its_handlers() {
        assert_eq!(
            family("try:\n    pass\nexcept A:\n    pass\n<CURSOR>finally:\n    pass\n"),
            "try@1 except@3 finally@5"
        );
    }

    #[test]
    fn a_match_pairs_with_its_cases_and_a_name_called_match_with_nothing() {
        assert_eq!(
            family("match x:\n    <CURSOR>case 1:\n        pass\n    case _:\n        pass\n"),
            "match@1 case@2 case@4"
        );
        assert_eq!(family("<CURSOR>match: int = 1\n"), "none");
    }

    #[test]
    fn a_def_pairs_with_its_own_exits() {
        let source = "\
def f():
    if a:
        return 1
    def g():
        return 2
    raise E
";
        assert_eq!(
            family(&source.replacen("def f", "<CURSOR>def f", 1)),
            "def@1 return@3 raise@6"
        );
        assert_eq!(
            family(&source.replacen("return 1", "<CURSOR>return 1", 1)),
            "def@1 return@3 raise@6"
        );
        assert_eq!(
            family(&source.replacen("return 2", "<CURSOR>return 2", 1)),
            "def@4 return@5"
        );
    }

    #[test]
    fn a_loop_pairs_with_its_own_jumps() {
        let source = "\
for a in b:
    while c:
        break
    for d in e:
        pass
    else:
        continue
else:
    pass
";
        assert_eq!(
            family(&source.replacen("for a", "<CURSOR>for a", 1)),
            "for@1 continue@7 else@8"
        );
        assert_eq!(
            family(&source.replacen("break", "<CURSOR>break", 1)),
            "while@2 break@3"
        );
    }
}
