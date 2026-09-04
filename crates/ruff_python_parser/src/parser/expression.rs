use std::ops::Deref;

use bitflags::bitflags;
use thin_vec::ThinVec;

use ruff_python_ast::name::Name;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{
    self as ast, AnyStringFlags, AtomicNodeIndex, BoolOp, CallableParameterShape, CastKind, CmpOp,
    ConversionFlag, Expr, ExprContext, FString, InterpolatedStringElement,
    InterpolatedStringElements, IpyEscapeKind, Number, Operator, OperatorPrecedence,
    ParameterBorrow, StringFlags, TString, UnaryOp,
};
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::error::{
    ComprehensionUnpackingKind, FStringKind, StarTupleKind, UnparenthesizedNamedExprKind,
};
use crate::parser::progress::ParserProgress;
use crate::parser::{FunctionKind, IpyEscapeContext, Parser, helpers};
use crate::string::{
    InterpolatedStringKind, StringType, parse_interpolated_string_literal_element,
    parse_string_literal,
};
use crate::token_set::TokenSet;
use crate::{
    InterpolatedStringErrorType, Mode, ParseErrorType, UnsupportedSyntaxError,
    UnsupportedSyntaxErrorKind,
};

use super::{InterpolatedStringElementsKind, Parenthesized, RecoveryContextKind};

/// A token set consisting of a newline or end of file.
const NEWLINE_EOF_SET: TokenSet = TokenSet::new([TokenKind::Newline, TokenKind::EndOfFile]);

/// Tokens that represents a literal expression.
const LITERAL_SET: TokenSet = TokenSet::new([
    TokenKind::Int,
    TokenKind::Float,
    TokenKind::Complex,
    TokenKind::String,
    TokenKind::Ellipsis,
    TokenKind::True,
    TokenKind::False,
    TokenKind::None,
]);

/// Tokens that represents either an expression or the start of one.
pub(super) const EXPR_SET: TokenSet = TokenSet::new([
    TokenKind::Name,
    TokenKind::Minus,
    TokenKind::Plus,
    TokenKind::Tilde,
    TokenKind::Star,
    TokenKind::DoubleStar,
    TokenKind::Lpar,
    TokenKind::Lbrace,
    TokenKind::Lsqb,
    TokenKind::Lambda,
    TokenKind::Await,
    TokenKind::Not,
    TokenKind::Yield,
    TokenKind::FStringStart,
    TokenKind::TStringStart,
    TokenKind::IpyEscapeCommand,
])
.union(LITERAL_SET);

/// Tokens that can appear after an expression.
const END_EXPR_SET: TokenSet = TokenSet::new([
    // Ex) `expr` (without a newline)
    TokenKind::EndOfFile,
    // Ex) `expr`
    TokenKind::Newline,
    // Ex) `expr;`
    TokenKind::Semi,
    // Ex) `data[expr:]`
    // Ex) `def foo() -> expr:`
    // Ex) `{expr: expr}`
    TokenKind::Colon,
    // Ex) `{expr}`
    TokenKind::Rbrace,
    // Ex) `[expr]`
    TokenKind::Rsqb,
    // Ex) `(expr)`
    TokenKind::Rpar,
    // Ex) `expr,`
    TokenKind::Comma,
    // Ex)
    //
    // if True:
    //     expr
    //     # <- Dedent
    // x
    TokenKind::Dedent,
    // Ex) `expr if expr else expr`
    TokenKind::If,
    TokenKind::Else,
    // Ex) `with expr as target:`
    // Ex) `except expr as NAME:`
    TokenKind::As,
    // Ex) `raise expr from expr`
    TokenKind::From,
    // Ex) `[expr for expr in iter]`
    TokenKind::For,
    // Ex) `[expr async for expr in iter]`
    TokenKind::Async,
    // Ex) `expr in expr`
    TokenKind::In,
    // Ex) `name: expr = expr`
    // Ex) `f"{expr=}"`
    TokenKind::Equal,
    // Ex) `f"{expr!s}"`
    TokenKind::Exclamation,
]);

/// Tokens that can appear at the end of a sequence.
const END_SEQUENCE_SET: TokenSet = END_EXPR_SET.remove(TokenKind::Comma);

/// basedpython: whether `kind` can start a
/// [statement expression](ast::ExprStatement).
///
/// `match` is a soft keyword and is covered by the soft-keyword checks that
/// accompany every use of this predicate.
pub(super) const fn starts_statement_expression(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::If
            | TokenKind::For
            | TokenKind::While
            | TokenKind::Try
            | TokenKind::Raise
            | TokenKind::Return
            | TokenKind::Break
            | TokenKind::Continue
    )
}

impl<'src> Parser<'src> {
    /// Returns `true` if the parser is at a name or keyword (including soft keyword) token.
    pub(super) fn at_name_or_keyword(&self) -> bool {
        self.at(TokenKind::Name) || self.current_token_kind().is_keyword()
    }

    /// Returns `true` if the parser is at a name or soft keyword token.
    pub(super) fn at_name_or_soft_keyword(&self) -> bool {
        self.at(TokenKind::Name) || self.at_soft_keyword()
    }

    /// Returns `true` if the parser is at a soft keyword token.
    pub(super) fn at_soft_keyword(&self) -> bool {
        self.current_token_kind().is_soft_keyword()
    }

    /// Returns `true` if the current token is the start of an expression.
    pub(super) fn at_expr(&self) -> bool {
        self.at_ts(EXPR_SET)
            || self.at_soft_keyword()
            // basedpython: a decorated type `@meta int` starts with `@`, which is
            // otherwise only ever an infix operator or a decorator line. Admitting
            // it here is what lets one be written wherever a type expression can
            // be: a subscript element, a parameter's annotation, a return type
            || (self.options.is_basedpython && self.at(TokenKind::At))
    }

    /// basedpython: consume a use-site variance keyword prefix (`out`, `in`,
    /// or `in out`) in subscript element position and return its variance, or
    /// `None` if no variance keyword was present.
    ///
    /// Disambiguates `out` from a bare reference to a variable named `out`:
    /// the variance form requires the next token to be a *name* (`out T`).
    /// Two adjacent names are never valid Python, so `out T` is unambiguous —
    /// whereas `out[...]`, `out(...)`, `out.attr`, `out + 1` etc. are an
    /// ordinary subscript / call / attribute / arithmetic on a variable named
    /// `out` and must be left alone (real Python uses them, e.g. `xs[out[0]]`).
    fn eat_basedpython_variance_prefix(
        &mut self,
    ) -> Option<ruff_python_ast::helpers::UseSiteVariance> {
        use ruff_python_ast::helpers::UseSiteVariance;
        let next_starts_type_expr = matches!(self.peek(), TokenKind::Name);
        let variance = if self.at(TokenKind::In) {
            // `in` is a hard keyword, so its presence in subscript-start
            // position is unambiguously variance.
            self.bump(TokenKind::In);
            if self.at(TokenKind::Name) && self.src_text(self.current_token_range()) == "out" {
                self.bump(TokenKind::Name);
                Some(UseSiteVariance::InOut)
            } else {
                Some(UseSiteVariance::In)
            }
        } else if self.at(TokenKind::Name)
            && self.src_text(self.current_token_range()) == "out"
            && next_starts_type_expr
        {
            self.bump(TokenKind::Name);
            Some(UseSiteVariance::Out)
        } else {
            None
        };
        if variance.is_some() {
            self.error_if_not_basedpython(
                "use-site variance keywords in subscription are not valid in .py files".to_string(),
            );
        }
        variance
    }

    /// wraps a slice element `inner` in a use-site variance marker. The
    /// marker is `Subscript(Name(<marker-id>, ctx=Invalid), inner)`. An
    /// invalid-context name with a `__variance_*__` id is unique to parser
    /// synthesis and cannot appear from any normal parse, so downstream
    /// consumers detect this shape unambiguously.
    ///
    /// `marker_range` should cover the variance keyword tokens themselves
    /// (no trailing whitespace) so the formatter can emit the exact source
    /// text on round-trip.
    fn wrap_variance_marker(
        inner: Expr,
        variance: ruff_python_ast::helpers::UseSiteVariance,
        marker_range: TextRange,
    ) -> Expr {
        let inner_range = inner.range();
        let marker_name = Expr::Name(ast::ExprName {
            range: marker_range,
            id: Name::from(variance.marker_id()),
            ctx: ExprContext::Invalid,
            node_index: AtomicNodeIndex::NONE,
        });
        Expr::Subscript(ast::ExprSubscript {
            value: Box::new(marker_name),
            slice: Box::new(inner),
            ctx: ExprContext::Load,
            range: TextRange::new(marker_range.start(), inner_range.end()),
            node_index: AtomicNodeIndex::NONE,
            is_typeof: false,
            is_type_decoration: false,
        })
    }

    /// basedpython: consume a use-site type modifier keyword (`literal`, `final`)
    /// standing in front of a type expression, returning the modifier and the
    /// range of the keyword itself. Returns `None` if no modifier is present.
    ///
    /// Disambiguated exactly like [`Self::eat_basedpython_variance_prefix`]: the
    /// modifier form requires the next token to be a *name*, and two adjacent
    /// names are never valid Python. So `literal str` is unambiguously the
    /// modifier, while `literal`, `final[x]`, `literal.attr`, `final(x)` etc.
    /// stay ordinary references to a variable of that name. The consequence is
    /// that a type which does not start with a name — a parenthesized callable
    /// type, a string forward reference, a starred type — cannot carry a bare
    /// modifier; see the docs for the recommended spelling.
    fn eat_basedpython_type_modifier_prefix(
        &mut self,
    ) -> Option<(ruff_python_ast::helpers::TypeModifier, TextRange)> {
        use ruff_python_ast::helpers::TypeModifier;

        if !self.at(TokenKind::Name) || !matches!(self.peek(), TokenKind::Name) {
            return None;
        }
        let range = self.current_token_range();
        let modifier = TypeModifier::from_keyword(self.src_text(range))?;
        self.bump(TokenKind::Name);
        self.error_if_not_basedpython_at(
            format!(
                "the `{}` type modifier is not valid in .py files",
                modifier.keyword()
            ),
            range,
        );
        Some((modifier, range))
    }

    /// wraps a type expression `inner` in a use-site type modifier marker. The
    /// marker is `Subscript(Name(<marker-id>, ctx=Invalid), inner)`, the same
    /// shape [`Self::wrap_variance_marker`] uses — an invalid-context name with
    /// a `__modifier_*__` id is unique to parser synthesis and cannot appear
    /// from any normal parse.
    ///
    /// `marker_range` covers the keyword token only (no trailing whitespace) so
    /// the formatter can emit the exact source text on round-trip.
    fn wrap_type_modifier_marker(
        inner: Expr,
        modifier: ruff_python_ast::helpers::TypeModifier,
        marker_range: TextRange,
    ) -> Expr {
        let inner_range = inner.range();
        let marker_name = Expr::Name(ast::ExprName {
            range: marker_range,
            id: Name::from(modifier.marker_id()),
            ctx: ExprContext::Invalid,
            node_index: AtomicNodeIndex::NONE,
        });
        Expr::Subscript(ast::ExprSubscript {
            value: Box::new(marker_name),
            slice: Box::new(inner),
            ctx: ExprContext::Load,
            range: TextRange::new(marker_range.start(), inner_range.end()),
            node_index: AtomicNodeIndex::NONE,
            is_typeof: false,
            is_type_decoration: false,
        })
    }

    /// basedpython: parses a decorated type — `@meta int`, the type-position
    /// spelling of `Annotated[int, meta]`.
    ///
    /// The decorator is an ordinary value expression, so it is read as a primary:
    /// a name, an attribute path, a call, or a subscript. That is what ends it —
    /// nothing separates the decorator from the type it decorates — so `@meta int`
    /// is `meta` decorating `int`, and `@field(gt=0) int` reads the call as the
    /// decorator.
    ///
    /// What follows is a whole type expression, not the single operand the
    /// decoration precedes: `@meta int | str` decorates the union, and `@meta int?`
    /// decorates the optional. A decoration reads as a prefix on the type it is
    /// written above, the way a decorator on a `def` covers the whole definition
    /// rather than its first line — so it stops only where the type does, at the
    /// `,` or `]` or `=` that ends it.
    fn parse_decorated_type(
        &mut self,
        _left_precedence: OperatorPrecedence,
        context: ExpressionContext,
    ) -> ParsedExpr {
        let start = self.node_start();
        self.bump(TokenKind::At);
        self.error_if_not_basedpython("a decorated type is not valid in .py files".to_string());

        // the decorator is read greedily, so a `(` after it is its call arguments.
        // That leaves `@meta (int | None)` — a decorator over a parenthesized type
        // — with nothing after the call to decorate, and when that happens the
        // parenthesized group was the type all along, so the parse is taken again
        // with the decorator stopping short of it
        let checkpoint = self.checkpoint();
        let decorator = self.parse_type_decorator(PostfixCalls::Allowed);
        let decorator = if self.at_expr() {
            decorator
        } else {
            self.rewind(checkpoint);
            self.parse_type_decorator(PostfixCalls::Forbidden)
        };

        let inner = self.with_recursion(|parser| {
            parser.parse_binary_expression_or_higher(OperatorPrecedence::None, context)
        });

        ParsedExpr {
            expr: Expr::Subscript(ast::ExprSubscript {
                // the last token consumed, not the type's own end — a parenthesized
                // type ends inside its parentheses, and the closing one belongs to
                // the decoration or it is left stranded when the decoration is
                // rewritten
                range: self.node_range(start),
                value: Box::new(decorator),
                slice: Box::new(inner.expr),
                ctx: ExprContext::Load,
                node_index: AtomicNodeIndex::NONE,
                is_typeof: false,
                is_type_decoration: true,
            }),
            is_parenthesized: false,
            parameter_borrow: inner.parameter_borrow,
        }
    }

    /// basedpython: reads the decorator of a decorated type — a primary
    /// expression: a name, an attribute path, a call, or a subscript.
    fn parse_type_decorator(&mut self, calls: PostfixCalls) -> Expr {
        let start = self.node_start();
        let decorator = self.parse_atom(ExpressionContext::default()).expr;
        self.parse_postfix_expression_impl(decorator, start, ExpressionContext::default(), calls)
    }

    /// Returns `true` if the current token ends a sequence.
    pub(super) fn at_sequence_end(&self) -> bool {
        self.at_ts(END_SEQUENCE_SET)
    }

    /// Parses every Python expression.
    ///
    /// Matches the `expressions` rule in the [Python grammar]. The [`ExpressionContext`] can be
    /// used to match the `star_expressions` rule.
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    pub(super) fn parse_expression_list(&mut self, context: ExpressionContext) -> ParsedExpr {
        let start = self.node_start();
        let parsed_expr = self.parse_conditional_expression_or_higher_impl(context);

        if self.at(TokenKind::Comma) {
            let subsequent_context = context.disallow_yield_expressions();
            Expr::Tuple(self.parse_tuple_expression(
                parsed_expr.expr,
                start,
                Parenthesized::No,
                |p| p.parse_conditional_expression_or_higher_impl(subsequent_context),
            ))
            .into()
        } else {
            parsed_expr
        }
    }

    /// Parses every Python expression except unparenthesized tuple.
    ///
    /// Matches the `named_expression` rule in the [Python grammar]. The [`ExpressionContext`] can
    /// be used to match the `star_named_expression` rule.
    ///
    /// NOTE: If you have expressions separated by commas and want to parse them individually
    /// instead of as a tuple, as done by [`Parser::parse_expression_list`], use this function.
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    pub(super) fn parse_named_expression_or_higher(
        &mut self,
        context: ExpressionContext,
    ) -> ParsedExpr {
        let start = self.node_start();
        let parsed_expr = self.parse_conditional_expression_or_higher_impl(context);

        if self.at(TokenKind::ColonEqual) {
            Expr::Named(self.parse_named_expression(parsed_expr.expr, start)).into()
        } else {
            parsed_expr
        }
    }

    /// Parses every Python expression except unparenthesized tuple and named expressions.
    ///
    /// Matches the `expression` rule in the [Python grammar].
    ///
    /// This uses the default [`ExpressionContext`]. Use
    /// [`Parser::parse_conditional_expression_or_higher_impl`] if you prefer to pass in the
    /// context.
    ///
    /// NOTE: If you have expressions separated by commas and want to parse them individually
    /// instead of as a tuple, as done by [`Parser::parse_expression_list`] use this function.
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    pub(super) fn parse_conditional_expression_or_higher(&mut self) -> ParsedExpr {
        self.parse_conditional_expression_or_higher_impl(ExpressionContext::default())
    }

    pub(super) fn parse_conditional_expression_or_higher_impl(
        &mut self,
        context: ExpressionContext,
    ) -> ParsedExpr {
        if self.at(TokenKind::Lambda) {
            Expr::Lambda(self.parse_lambda_expr()).into()
        } else {
            let start = self.node_start();
            let parsed_expr = self.parse_simple_expression(context);

            if self.at(TokenKind::If) {
                Expr::If(self.parse_if_expression(parsed_expr.expr, start, context)).into()
            } else {
                parsed_expr
            }
        }
    }

    /// Parses every Python expression except unparenthesized tuples, named expressions,
    /// and `if` expression.
    ///
    /// This is a combination of the `disjunction`, `starred_expression`, `yield_expr`
    /// and `lambdef` rules of the [Python grammar].
    ///
    /// Note that this function parses lambda expression but reports an error as they're not
    /// allowed in this context. This is done for better error recovery.
    /// Use [`Parser::parse_conditional_expression_or_higher`] or any methods which calls into the
    /// specified method to allow parsing lambda expression.
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    fn parse_simple_expression(&mut self, context: ExpressionContext) -> ParsedExpr {
        self.parse_binary_expression_or_higher(OperatorPrecedence::None, context)
    }

    /// Parses a binary expression using the [Pratt parsing algorithm].
    ///
    /// [Pratt parsing algorithm]: https://matklad.github.io/2020/04/13/simple-but-powerful-pratt-parsing.html
    fn parse_binary_expression_or_higher(
        &mut self,
        left_precedence: OperatorPrecedence,
        context: ExpressionContext,
    ) -> ParsedExpr {
        self.with_recursion(|parser| {
            let start = parser.node_start();
            let lhs = parser.parse_lhs_expression(left_precedence, context);
            parser.parse_binary_expression_or_higher_recursive(lhs, left_precedence, context, start)
        })
    }

    fn parse_binary_expression_or_higher_recursive(
        &mut self,
        mut left: ParsedExpr,
        left_precedence: OperatorPrecedence,
        context: ExpressionContext,
        start: TextSize,
    ) -> ParsedExpr {
        let mut progress = ParserProgress::default();

        loop {
            progress.assert_progressing(self);

            let current_token = self.current_token_kind();

            if matches!(current_token, TokenKind::In) && context.is_in_excluded() {
                // Omit the `in` keyword when parsing the target expression in a comprehension or
                // a `for` statement.
                break;
            }

            // callable type: `(int) -> int`, `(int, str) -> bool`, `() -> None`
            // `->` binds tighter than `|` so `(a) -> int | None` → `Callable[[a], int] | None`
            let is_callable_lhs =
                left.is_parenthesized || matches!(&left.expr, Expr::Tuple(t) if t.parenthesized);
            if is_callable_lhs && current_token == TokenKind::Rarrow {
                // stop before consuming `->` if the caller expects higher-precedence operators only
                if OperatorPrecedence::BitOr < left_precedence {
                    break;
                }
                self.error_if_not_basedpython(
                    "callable type syntax `(...) -> ...` is not valid in .py files".to_string(),
                );
                left = ParsedExpr {
                    expr: self.parse_callable_type_arrow(None, left, start, context),
                    is_parenthesized: false,
                    parameter_borrow: ParameterBorrow::None,
                };
                continue;
            }

            // basedpython infix casts: `<value> cast <type>` and its two
            // runtime-verifying suffixed forms, `cast!` (raises on a mismatch)
            // and `cast?` (yields `None`). A suffix directly follows the `cast`
            // soft keyword, and neither `!` nor `?` can start an expression, so
            // a suffixed form never collides with the plain `<value> cast
            // <type>` or with `<value> cast <type>?`. All three are treated as
            // the loosest binary-like operator: only consumed at the outermost
            // expression level (where `left_precedence` is `None`). Lowered by
            // `transforms::checked_cast`.
            if current_token == TokenKind::Cast
                && let Some(cast_kind) = match self.peek() {
                    TokenKind::Exclamation => Some(CastKind::Checked),
                    TokenKind::Question => Some(CastKind::Try),
                    peeked if EXPR_SET.contains(peeked) || peeked.is_soft_keyword() => {
                        Some(CastKind::Static)
                    }
                    _ => None,
                }
            {
                if left_precedence > OperatorPrecedence::None {
                    break;
                }
                self.error_if_not_basedpython(format!(
                    "`{}` keyword is not valid in .py files",
                    cast_kind.as_str()
                ));
                let cast_keyword_range = self.current_token_range();
                self.bump(TokenKind::Cast);
                // a suffixed operator is both tokens, so its span reaches the
                // suffix — consumers that point at the keyword (semantic
                // tokens) must cover `cast!` / `cast?`, not just the `cast`
                let operator_range = match cast_kind {
                    CastKind::Static => cast_keyword_range,
                    CastKind::Checked | CastKind::Try => {
                        let suffix_range = self.current_token_range();
                        self.bump(if cast_kind == CastKind::Checked {
                            TokenKind::Exclamation
                        } else {
                            TokenKind::Question
                        });
                        TextRange::new(cast_keyword_range.start(), suffix_range.end())
                    }
                };
                // the right operand is the cast's target type, so it admits the
                // use-site type modifiers wherever the cast itself is written
                let right = self.parse_binary_expression_or_higher(
                    OperatorPrecedence::None,
                    context.with_in_type_expression(),
                );
                let value_expr = left.expr;
                let type_expr = right.expr;
                let args_range = TextRange::new(operator_range.end(), type_expr.range().end());
                let func = Expr::Name(ast::ExprName {
                    range: operator_range,
                    id: Name::new_static("cast"),
                    ctx: ExprContext::Load,
                    node_index: AtomicNodeIndex::NONE,
                });
                let arguments = ast::Arguments {
                    range: args_range,
                    node_index: AtomicNodeIndex::NONE,
                    args: Box::from([type_expr, value_expr]),
                    keywords: ThinVec::new(),
                };
                left = ParsedExpr {
                    expr: Expr::Call(ast::ExprCall {
                        func: Box::new(func),
                        arguments,
                        range_start: start,
                        node_index: AtomicNodeIndex::NONE,
                        cast_kind: Some(cast_kind),
                        is_string_tag: false,
                    }),
                    is_parenthesized: false,
                    parameter_borrow: ParameterBorrow::None,
                };
                continue;
            }

            // basedpython wrapped types: `T?` (optional) and `T ? E` (result).
            // `?` binds looser than `|` (same precedence as `??`), so the value
            // and error operands each absorb a full union. Whether an error type
            // follows the `?` distinguishes the result form from the optional
            // form. In ipython mode `?` is the help-end command, handled at the
            // statement level, so the intercept stands down there.
            if current_token == TokenKind::Question && self.options.mode != Mode::Ipython {
                if OperatorPrecedence::Or <= left_precedence {
                    break;
                }
                self.error_if_not_basedpython(
                    "`?` (optional/result type) syntax is not valid in .py files".to_string(),
                );
                self.bump(TokenKind::Question);
                left.expr = if self.at_expr() {
                    let error =
                        self.parse_binary_expression_or_higher(OperatorPrecedence::Or, context);
                    Expr::BinOp(ast::ExprBinOp {
                        left: Box::new(left.expr),
                        op: ast::Operator::Result,
                        right: Box::new(error.expr),
                        range: self.node_range(start),
                        node_index: AtomicNodeIndex::NONE,
                    })
                } else {
                    Expr::UnaryOp(ast::ExprUnaryOp {
                        op: ast::UnaryOp::Optional,
                        operand: Box::new(left.expr),
                        range: self.node_range(start),
                        node_index: AtomicNodeIndex::NONE,
                    })
                };
                left.is_parenthesized = false;
                continue;
            }

            // basedpython doubly-wrapped optional: glued `T??` lowers to
            // `(T?)?`. The `??` token is otherwise the none-coalesce operator,
            // but coalesce is binary and requires a right operand, so a `??`
            // with no following expression is unambiguously the double-optional
            // type marker. (`T?? E` with an error operand remains coalesce for
            // now — its result-of-optional runtime is still being settled.)
            if current_token == TokenKind::DoubleQuestion
                && self.options.mode != Mode::Ipython
                && !(EXPR_SET.contains(self.peek())
                    || self.peek().is_soft_keyword()
                    || starts_statement_expression(self.peek()))
            {
                if OperatorPrecedence::Or <= left_precedence {
                    break;
                }
                self.error_if_not_basedpython(
                    "`?` (optional/result type) syntax is not valid in .py files".to_string(),
                );
                self.bump(TokenKind::DoubleQuestion);
                let inner = Expr::UnaryOp(ast::ExprUnaryOp {
                    op: ast::UnaryOp::Optional,
                    operand: Box::new(left.expr),
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                });
                left.expr = Expr::UnaryOp(ast::ExprUnaryOp {
                    op: ast::UnaryOp::Optional,
                    operand: Box::new(inner),
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                });
                left.is_parenthesized = false;
                continue;
            }

            let next_token =
                matches!(current_token, TokenKind::Is | TokenKind::Not).then(|| self.peek());
            let Some(operator) = BinaryLikeOperator::try_from_tokens(current_token, next_token)
            else {
                // Not an operator.
                break;
            };

            let new_precedence = operator.precedence();

            let stop_at_current_operator = if new_precedence.is_right_associative() {
                new_precedence < left_precedence
            } else {
                new_precedence <= left_precedence
            };

            if stop_at_current_operator {
                break;
            }

            left.expr = match operator {
                BinaryLikeOperator::Boolean(bool_op) => {
                    Expr::BoolOp(self.parse_boolean_expression(left.expr, start, bool_op, context))
                }
                BinaryLikeOperator::Comparison(cmp_op) => Expr::Compare(
                    self.parse_comparison_expression(left.expr, start, cmp_op, context),
                ),
                BinaryLikeOperator::Binary(bin_op) => {
                    if matches!(bin_op, ast::Operator::Coalesce) {
                        self.error_if_not_basedpython(
                            "`??` (none-coalesce) operator is not valid in .py files".to_string(),
                        );
                    }
                    self.bump(TokenKind::from(bin_op));

                    let right = self.parse_binary_expression_or_higher(new_precedence, context);

                    Expr::BinOp(ast::ExprBinOp {
                        left: Box::new(left.expr),
                        op: bin_op,
                        right: Box::new(right.expr),
                        range: self.node_range(start),
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
            };
        }

        left
    }

    /// Parses the left-hand side of an expression.
    ///
    /// This includes prefix expressions such as unary operators, boolean `not`,
    /// `await`, `lambda`. It also parses atoms and postfix expressions.
    ///
    /// The given [`OperatorPrecedence`] is used to determine if the parsed expression
    /// is valid in that context. For example, a unary operator is not valid
    /// in an `await` expression in which case the `left_precedence` would
    /// be [`OperatorPrecedence::Await`].
    fn parse_lhs_expression(
        &mut self,
        left_precedence: OperatorPrecedence,
        context: ExpressionContext,
    ) -> ParsedExpr {
        // basedpython: a decorated type `@meta int` takes the whole type expression
        // after it — unlike the use-site modifiers below, which take one operand —
        // so `@meta int | None` is `@meta (int | None)`
        if context.is_in_type_expression() && self.at(TokenKind::At) {
            return self.parse_decorated_type(left_precedence, context);
        }

        // basedpython: a use-site type modifier (`literal T`, `final T`) binds to
        // the operand it precedes and nothing more, so `literal str | None` is
        // `(literal str) | None`. Consuming it here — at the operand level of the
        // binary-expression parse — is what gives it that precedence.
        if context.is_in_type_expression()
            && let Some((modifier, marker_range)) = self.eat_basedpython_type_modifier_prefix()
        {
            let inner =
                self.with_recursion(|parser| parser.parse_lhs_expression(left_precedence, context));
            return ParsedExpr {
                expr: Self::wrap_type_modifier_marker(inner.expr, modifier, marker_range),
                is_parenthesized: false,
                parameter_borrow: inner.parameter_borrow,
            };
        }

        let token = self.current_token_kind();
        let start = self.node_start();

        if let Some(unary_op) = token.as_unary_operator() {
            let expr = self.parse_unary_expression(unary_op, context);

            if matches!(unary_op, UnaryOp::Not) {
                if left_precedence > OperatorPrecedence::Not {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "Boolean 'not' expression cannot be used here".to_string(),
                        ),
                        &expr,
                    );
                }
            } else {
                // > The power operator `**` binds less tightly than an arithmetic
                // > or bitwise unary operator on its right, that is, 2**-1 is 0.5.
                //
                // Reference: https://docs.python.org/3/reference/expressions.html#id21
                if left_precedence > OperatorPrecedence::PosNegBitNot
                    && left_precedence != OperatorPrecedence::Exponent
                {
                    self.add_error(
                        ParseErrorType::OtherError(format!(
                            "Unary '{unary_op}' expression cannot be used here",
                        )),
                        &expr,
                    );
                }
            }

            return Expr::UnaryOp(expr).into();
        }

        match token {
            TokenKind::Star => {
                // basedpython: bare `*` inside a subscript slice, followed by a
                // slice/type terminator, is the top-star marker for `Top[...]`
                // lowering. Handles nested cases like `list[int | *]` where the
                // marker is the right operand of a type-position `|` or `&`
                if context.is_subscript_slice()
                    && matches!(
                        self.peek(),
                        TokenKind::Rsqb | TokenKind::Comma | TokenKind::Vbar | TokenKind::Amper
                    )
                {
                    self.error_if_not_basedpython(
                        "bare `*` in subscription is not valid in .py files".to_string(),
                    );
                    let star_start = self.node_start();
                    self.bump(TokenKind::Star);
                    let star_range = self.node_range(star_start);
                    let marker_name = Expr::Name(ast::ExprName {
                        range: TextRange::empty(star_range.end()),
                        id: Name::empty(),
                        ctx: ExprContext::Invalid,
                        node_index: AtomicNodeIndex::NONE,
                    });
                    return Expr::Starred(ast::ExprStarred {
                        value: Box::new(marker_name),
                        ctx: ExprContext::Load,
                        range: star_range,
                        node_index: AtomicNodeIndex::NONE,
                    })
                    .into();
                }
                let starred_expr = self.parse_starred_expression(context);

                if left_precedence > OperatorPrecedence::None
                    || !context.is_starred_expression_allowed()
                {
                    self.add_error(ParseErrorType::InvalidStarredExpressionUsage, &starred_expr);
                }

                return Expr::Starred(starred_expr).into();
            }
            TokenKind::Await => {
                let await_expr = self.parse_await_expression();

                // `await` expressions cannot be nested
                if left_precedence >= OperatorPrecedence::Await {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "Await expression cannot be used here".to_string(),
                        ),
                        &await_expr,
                    );
                }

                return Expr::Await(await_expr).into();
            }
            TokenKind::Lambda => {
                // Lambda expression isn't allowed in this context but we'll still parse it and
                // report an error for better recovery.
                let lambda_expr = self.parse_lambda_expr();
                self.add_error(ParseErrorType::InvalidLambdaExpressionUsage, &lambda_expr);
                return Expr::Lambda(lambda_expr).into();
            }
            TokenKind::Yield => {
                let expr = self.parse_yield_expression();

                if left_precedence > OperatorPrecedence::None
                    || !context.is_yield_expression_allowed()
                {
                    self.add_error(ParseErrorType::InvalidYieldExpressionUsage, &expr);
                }

                return expr.into();
            }
            _ => {}
        }

        let lhs = self.parse_atom(context);

        ParsedExpr {
            expr: self.parse_postfix_expression(lhs.expr, start, context),
            is_parenthesized: lhs.is_parenthesized,
            parameter_borrow: lhs.parameter_borrow,
        }
    }

    /// Parses an expression with a minimum precedence of bitwise `or`.
    ///
    /// This methods actually parses the expression using the `expression` rule
    /// of the [Python grammar] and then validates the parsed expression. In a
    /// sense, it matches the `bitwise_or` rule of the [Python grammar].
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    fn parse_expression_with_bitwise_or_precedence(&mut self) -> ParsedExpr {
        let parsed_expr = self.parse_conditional_expression_or_higher();

        if parsed_expr.is_parenthesized {
            // Parentheses resets the precedence, so we don't need to validate it.
            return parsed_expr;
        }

        let expr_name = match parsed_expr.expr {
            Expr::Compare(_) => "Comparison",
            Expr::BoolOp(_)
            | Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                ..
            }) => "Boolean",
            Expr::If(_) => "Conditional",
            Expr::Lambda(_) => "Lambda",
            _ => return parsed_expr,
        };

        self.add_error(
            ParseErrorType::OtherError(format!("{expr_name} expression cannot be used here")),
            &parsed_expr,
        );

        parsed_expr
    }

    /// Parses a name.
    ///
    /// For an invalid name, the `id` field will be an empty string and the `ctx`
    /// field will be [`ExprContext::Invalid`].
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#atom-identifiers>
    /// basedpython: parses the type after the `:` of an anonymous parameter field (`*: T`,
    /// `**: T`).
    ///
    /// A bare `*` here is the top type, so `(*: *, **: *)` spells the *top parameters* form that
    /// every parameter list is a subtype of. It is encoded as the same
    /// [top-star marker](ruff_python_ast::helpers::is_top_star_marker) a subscript `[*]` produces.
    fn parse_parameter_field_annotation(&mut self) -> ParsedExpr {
        if self.at(TokenKind::Star) && matches!(self.peek(), TokenKind::Comma | TokenKind::Rpar) {
            let star_start = self.node_start();
            self.bump(TokenKind::Star);
            let star_range = self.node_range(star_start);
            let marker_name = Expr::Name(ast::ExprName {
                range: TextRange::empty(star_range.end()),
                id: Name::empty(),
                ctx: ExprContext::Invalid,
                node_index: AtomicNodeIndex::NONE,
            });
            return ParsedExpr {
                expr: Expr::Starred(ast::ExprStarred {
                    value: Box::new(marker_name),
                    ctx: ExprContext::Load,
                    range: star_range,
                    node_index: AtomicNodeIndex::NONE,
                }),
                is_parenthesized: false,
                parameter_borrow: ParameterBorrow::None,
            };
        }
        // a variadic's annotation may itself be starred (`*: *Ts`), just as in a `def`
        self.parse_conditional_expression_or_higher_impl(
            ExpressionContext::starred_bitwise_or().with_in_type_expression(),
        )
    }

    pub(super) fn parse_name(&mut self, context: ExpressionContext) -> ast::ExprName {
        let identifier = self.parse_identifier_with_context(context);

        let ctx = if identifier.is_valid() {
            ExprContext::Load
        } else {
            ExprContext::Invalid
        };

        ast::ExprName {
            range: identifier.range,
            id: identifier.id,
            ctx,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    pub(super) fn parse_missing_name(&mut self) -> ast::ExprName {
        let identifier = self.parse_missing_identifier();

        ast::ExprName {
            range: identifier.range,
            id: identifier.id,
            ctx: ExprContext::Invalid,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an identifier.
    ///
    /// For an invalid identifier, the `id` field will be an empty string.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#atom-identifiers>
    pub(super) fn parse_identifier(&mut self) -> ast::Identifier {
        self.parse_identifier_with_context(ExpressionContext::default())
    }

    fn parse_identifier_with_context(&mut self, context: ExpressionContext) -> ast::Identifier {
        let range = self.current_token_range();

        if self.at(TokenKind::Name) {
            let name = self.bump_name();
            return ast::Identifier {
                id: name,
                range,
                node_index: AtomicNodeIndex::NONE,
            };
        }

        if self.current_token_kind().is_soft_keyword() {
            let text = self.src_text(range);
            let id = self.intern_name(text);
            self.bump_soft_keyword_as_name();
            return ast::Identifier {
                id,
                range,
                node_index: AtomicNodeIndex::NONE,
            };
        }

        // test_err incomplete_attribute_before_for_in_delimiter
        // [item. for item in xs]
        // [item. async for item in xs]
        // {item. for item in xs}
        // (item. for item in xs)
        // [item for item. in xs]
        // for item. in xs: ...
        if (context.is_for_excluded())
            && (self.at(TokenKind::For)
                || (self.at(TokenKind::Async) && self.peek() == TokenKind::For))
            || (context.is_in_excluded() && self.at(TokenKind::In))
        {
            return self.parse_missing_identifier();
        }

        if self.current_token_kind().is_keyword() {
            // Non-soft keyword
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "Expected an identifier, but found a keyword {} that cannot be used here",
                    self.current_token_kind()
                )),
                range,
            );

            let text = self.src_text(range);
            let id = self.intern_name(text);
            self.bump_any();
            ast::Identifier {
                id,
                range,
                node_index: AtomicNodeIndex::NONE,
            }
        } else {
            self.parse_missing_identifier()
        }
    }

    fn parse_missing_identifier(&mut self) -> ast::Identifier {
        self.add_error(
            ParseErrorType::OtherError("Expected an identifier".into()),
            self.current_token_range(),
        );

        ast::Identifier {
            id: Name::empty(),
            range: self.missing_node_range(),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an atom.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#atoms>
    fn parse_atom(&mut self, context: ExpressionContext) -> ParsedExpr {
        let start = self.node_start();

        let lhs = match self.current_token_kind() {
            TokenKind::Float => {
                let value = self.bump_float();

                Expr::NumberLiteral(ast::ExprNumberLiteral {
                    value: Number::Float(value),
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::Complex => {
                let (real, imag) = self.bump_complex();
                Expr::NumberLiteral(ast::ExprNumberLiteral {
                    value: Number::Complex { real, imag },
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::Int => {
                let value = self.bump_int();
                Expr::NumberLiteral(ast::ExprNumberLiteral {
                    value: Number::Int(value),
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::True => {
                self.bump(TokenKind::True);
                Expr::BooleanLiteral(ast::ExprBooleanLiteral {
                    value: true,
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::False => {
                self.bump(TokenKind::False);
                Expr::BooleanLiteral(ast::ExprBooleanLiteral {
                    value: false,
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::None => {
                self.bump(TokenKind::None);
                Expr::NoneLiteral(ast::ExprNoneLiteral {
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::Ellipsis => {
                self.bump(TokenKind::Ellipsis);
                Expr::EllipsisLiteral(ast::ExprEllipsisLiteral {
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::Name => {
                // basedpython `typeof <expr>` keyword: when the current
                // identifier is `typeof` and the following token starts an
                // expression, treat as `typeof X` and emit an `ExprSubscript`
                // with `is_typeof: true`. Outside basedpython mode this still
                // parses (so the rest of the expression doesn't desync) but a
                // diagnostic is emitted via `error_if_not_basedpython`.
                if self.src_text(self.current_token_range()) == "typeof"
                    && (EXPR_SET.contains(self.peek()) || self.peek().is_soft_keyword())
                {
                    self.error_if_not_basedpython(
                        "`typeof` keyword is not valid in .py files".to_string(),
                    );
                    let typeof_range = self.current_token_range();
                    self.bump(TokenKind::Name);
                    // synthetic placeholder for the subscript's `value`. we
                    // don't use `Name("typeof")` because that would be picked
                    // up by name-resolution as an unresolved reference. an
                    // ellipsis literal is inert for resolution. the formatter
                    // and downstream consumers ignore `value` when
                    // `is_typeof` is set
                    let typeof_value = Expr::EllipsisLiteral(ast::ExprEllipsisLiteral {
                        range: typeof_range,
                        node_index: AtomicNodeIndex::NONE,
                    });
                    let slice = self
                        .parse_binary_expression_or_higher(
                            OperatorPrecedence::Await,
                            ExpressionContext::default(),
                        )
                        .expr;
                    Expr::Subscript(ast::ExprSubscript {
                        value: Box::new(typeof_value),
                        slice: Box::new(slice),
                        ctx: ExprContext::Load,
                        range: self.node_range(start),
                        node_index: AtomicNodeIndex::NONE,
                        is_typeof: true,
                        is_type_decoration: false,
                    })
                } else if self.at_inline_protocol_type() {
                    self.error_if_not_basedpython(
                        "inline protocol type `protocol(...)` is not valid in .py files"
                            .to_string(),
                    );
                    Expr::ProtocolType(self.parse_inline_protocol_type(start))
                } else {
                    Expr::Name(self.parse_name(context))
                }
            }
            // basedpython statement expressions. gated on the source type rather
            // than reported through `error_if_not_basedpython`: these keywords
            // already appear in `.py` files being recovered from, and consuming a
            // whole suite there would replace python's own diagnostics.
            //
            // `for` is excluded in the contexts where it delimits a comprehension
            // clause, so a missing comprehension target still reports as such
            TokenKind::If
            | TokenKind::While
            | TokenKind::Try
            | TokenKind::Raise
            | TokenKind::Return
            | TokenKind::Break
            | TokenKind::Continue
            | TokenKind::Match
                if self.options.is_basedpython =>
            {
                match self.parse_statement_expression() {
                    Some(expr) => Expr::Statement(expr),
                    // the `match` soft keyword was an ordinary identifier
                    None => Expr::Name(self.parse_name(context)),
                }
            }
            TokenKind::For if self.options.is_basedpython && !context.is_for_excluded() => {
                match self.parse_statement_expression() {
                    Some(expr) => Expr::Statement(expr),
                    None => Expr::Name(self.parse_name(context)),
                }
            }
            TokenKind::IpyEscapeCommand => {
                Expr::IpyEscapeCommand(self.parse_ipython_escape_command_expression())
            }
            TokenKind::String | TokenKind::FStringStart | TokenKind::TStringStart => {
                self.parse_strings()
            }
            TokenKind::Lpar => {
                return self.parse_parenthesized_expression(context);
            }
            TokenKind::Lsqb => self.parse_list_like_expression(context),
            TokenKind::Lbrace => self.parse_set_or_dict_like_expression(),

            kind => {
                if kind.is_keyword() {
                    Expr::Name(self.parse_name(context))
                } else {
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    Expr::Name(ast::ExprName {
                        range: self.missing_node_range(),
                        id: Name::empty(),
                        ctx: ExprContext::Invalid,
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
            }
        };

        lhs.into()
    }

    /// Parses a postfix expression in a loop until there are no postfix expressions left to parse.
    ///
    /// For a given left-hand side, a postfix expression can begin with either `(` for a call
    /// expression, `[` for a subscript expression, or `.` for an attribute expression.
    ///
    /// This method does nothing if the current token is not a candidate for a postfix expression.
    fn parse_postfix_expression(
        &mut self,
        lhs: Expr,
        start: TextSize,
        context: ExpressionContext,
    ) -> Expr {
        self.parse_postfix_expression_impl(lhs, start, context, PostfixCalls::Allowed)
    }

    /// [`Self::parse_postfix_expression`], with a say in whether a `(` is read as
    /// a call. The basedpython decorated type `@meta (int | None)` needs the
    /// answer to be no — see [`Self::parse_decorated_type`].
    fn parse_postfix_expression_impl(
        &mut self,
        mut lhs: Expr,
        start: TextSize,
        context: ExpressionContext,
        calls: PostfixCalls,
    ) -> Expr {
        loop {
            lhs = match self.current_token_kind() {
                TokenKind::Lpar if calls == PostfixCalls::Forbidden => break lhs,
                TokenKind::Lpar => Expr::Call(self.parse_call_expression(lhs, start)),
                TokenKind::Lsqb => Expr::Subscript(self.parse_subscript_expression(lhs, start)),
                // basedpython: in a bound range `T: Lower..Upper` the `..` separates the two
                // ends, so the lower end stops rather than reading the dots as attribute access
                TokenKind::Dot
                    if context.is_in_type_param_bound() && self.second_dot_is_adjacent() =>
                {
                    break lhs;
                }
                // the same `..` anywhere else is not a range. left alone it parses
                // as two attribute accesses with an empty name between them, which
                // earns a pair of diagnostics about an attribute nobody wrote —
                // say what the reader actually did instead
                TokenKind::Dot if self.options.is_basedpython && self.second_dot_is_adjacent() => {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "a `..` bound range is only valid in a type parameter's bound"
                                .to_string(),
                        ),
                        self.current_token_range(),
                    );
                    break lhs;
                }
                // basedpython: `int.() -> str` — a callable with an implicit
                // receiver. `.` followed by `(` is never valid python, so the
                // form is unambiguous; the `->` is consumed here rather than by
                // the binary loop, which only recognises a parenthesized left
                TokenKind::Dot if self.peek() == TokenKind::Lpar => {
                    self.error_if_not_basedpython(
                        "receiver callable type `T.(...) -> ...` is not valid in .py files"
                            .to_string(),
                    );
                    self.bump(TokenKind::Dot);
                    let params = self.parse_parenthesized_expression(context);
                    break self.parse_callable_type_arrow(Some(lhs), params, start, context);
                }
                TokenKind::Dot => {
                    // basedpython: postfix `.await` is sugar for a prefix
                    // `await (expr)`. it binds as tightly as attribute access,
                    // so it chains (`g().await.bar().await`). produce a standard
                    // `Await` node tagged `postfix` so lowering can rewrite it
                    // and `.py` files reject it
                    if self.peek() == TokenKind::Await {
                        self.error_if_not_basedpython(
                            "postfix `.await` is not valid in .py files".to_string(),
                        );
                        self.bump(TokenKind::Dot);
                        self.bump(TokenKind::Await);
                        Expr::Await(ast::ExprAwait {
                            value: Box::new(lhs),
                            range: self.node_range(start),
                            node_index: AtomicNodeIndex::NONE,
                            postfix: true,
                        })
                    } else {
                        Expr::Attribute(self.parse_attribute_expression(lhs, start, context))
                    }
                }
                TokenKind::QuestionDot => {
                    self.error_if_not_basedpython(
                        "`?.` (optional-chain) operator is not valid in .py files".to_string(),
                    );
                    Expr::Attribute(self.parse_optional_attribute_expression(lhs, start))
                }
                // basedpython custom string tag: the lexer lexes a string
                // glued to an identifier as a t-string, so a `TStringStart`
                // abutting a name is `tag"..."` — a call receiving the
                // template. lowered to `tag(t"...")`
                //
                // an attribute carries a tag as well (`root.text"…"`), which is
                // the spelling that survives a local of the same name shadowing
                // the tag. the receiver has to be the thing the quote is glued
                // to, so only the trailing name of the attribute may carry it —
                // that is what the range check below says
                TokenKind::TStringStart
                    if matches!(lhs, Expr::Name(_) | Expr::Attribute(_))
                        && lhs.range().end() == self.current_token_range().start() =>
                {
                    self.error_if_not_basedpython(
                        "custom string tags are not valid in .py files".to_string(),
                    );
                    let template = self.parse_strings();
                    let arguments = ast::Arguments {
                        range: template.range(),
                        node_index: AtomicNodeIndex::NONE,
                        args: Box::from([template]),
                        keywords: ThinVec::new(),
                    };
                    Expr::Call(ast::ExprCall {
                        func: Box::new(lhs),
                        arguments,
                        range_start: start,
                        node_index: AtomicNodeIndex::NONE,
                        cast_kind: None,
                        is_string_tag: true,
                    })
                }
                // basedpython postfix `^` propagate. `^` is otherwise the infix
                // bitwise-xor operator, so it is only postfix when no operand
                // follows; an operand on the right means it is xor (left for the
                // binary loop). exception: in basedpython a `^` *glued* to its
                // operand (no whitespace, as one writes `expr^`) followed by a
                // unary-capable arithmetic token (`+ - ~ * **`) is
                // postfix-then-binary (`p(a)^ + b` → `(p(a)^) + b`), not
                // xor-of-unary. that disambiguation must not apply in `.py` mode,
                // where glued `a^-b` is plain `a ^ (-b)` — stealing it would make
                // valid python a syntax error.
                TokenKind::CircumFlex
                    if self.options.mode != Mode::Ipython
                        && (!(EXPR_SET.contains(self.peek()) || self.peek().is_soft_keyword())
                            || (self.options.is_basedpython
                                && lhs.range().end() == self.current_token_range().start()
                                && matches!(
                                    self.peek(),
                                    TokenKind::Plus
                                        | TokenKind::Minus
                                        | TokenKind::Tilde
                                        | TokenKind::Star
                                        | TokenKind::DoubleStar
                                ))) =>
                {
                    self.error_if_not_basedpython(
                        "`^` (propagate) operator is not valid in .py files".to_string(),
                    );
                    self.bump(TokenKind::CircumFlex);
                    Expr::UnaryOp(ast::ExprUnaryOp {
                        op: ast::UnaryOp::Propagate,
                        operand: Box::new(lhs),
                        range: self.node_range(start),
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
                // basedpython postfix `!` force-unwrap. Inside an interpolated
                // string replacement field a trailing `!` is the conversion flag
                // (`f"{x!r}"`), so it is suppressed there.
                TokenKind::Exclamation
                    if self.options.mode != Mode::Ipython && !context.is_in_interpolation() =>
                {
                    self.error_if_not_basedpython(
                        "`!` (force-unwrap) operator is not valid in .py files".to_string(),
                    );
                    self.bump(TokenKind::Exclamation);
                    Expr::UnaryOp(ast::ExprUnaryOp {
                        op: ast::UnaryOp::Force,
                        operand: Box::new(lhs),
                        range: self.node_range(start),
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
                TokenKind::Float => match self.parse_tuple_member_expression(lhs, start) {
                    Ok(expr) => Expr::Attribute(expr),
                    Err(prev) => break prev,
                },
                _ => break lhs,
            };
        }
    }

    /// basedpython: tuple-member access `expr.N`. The lexer eats `.N` as a
    /// single `Float` token, so postfix parsing checks for the pattern here
    /// and constructs an `ExprAttribute` whose `attr` is the decimal digits.
    /// Returns `Err(lhs)` if the float token is not a tuple-member shape
    /// (e.g. `.5e10`, `.5j`, `1.0`); caller then exits the postfix loop.
    fn parse_tuple_member_expression(
        &mut self,
        lhs: Expr,
        start: TextSize,
    ) -> Result<ast::ExprAttribute, Expr> {
        let token_range = self.current_token_range();
        let text = self.src_text(token_range);
        if !text.starts_with('.') || !text[1..].bytes().all(|b| b.is_ascii_digit()) {
            return Err(lhs);
        }
        self.error_if_not_basedpython(
            "tuple-member access `expr.N` is not valid in .py files".to_string(),
        );
        let digits = &text[1..];
        // identifier range spans the full `.N` float token so the synthetic
        // node aligns with a real lexer token boundary (validator requires
        // node starts to coincide with token starts)
        let attr = ast::Identifier::new(Name::new(digits), token_range);
        self.bump(TokenKind::Float);
        Ok(ast::ExprAttribute {
            value: Box::new(lhs),
            attr,
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            optional: false,
        })
    }

    /// Parse a call expression.
    ///
    /// The function name is parsed by the caller and passed as `func` along with
    /// the `start` position of the call expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't position at a `(` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#calls>
    fn parse_call_expression(&mut self, func: Expr, start: TextSize) -> ast::ExprCall {
        let arguments = self.parse_arguments(ArgumentsContext::Call);
        debug_assert_eq!(self.node_range(start).end(), arguments.end());

        ast::ExprCall {
            func: Box::new(func),
            arguments,
            range_start: start,
            node_index: AtomicNodeIndex::NONE,
            cast_kind: None,
            is_string_tag: false,
        }
    }

    /// Parses an argument list.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `(` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#grammar-token-python-grammar-argument_list>
    pub(super) fn parse_arguments(&mut self, context: ArgumentsContext) -> ast::Arguments {
        let start = self.node_start();
        self.bump(TokenKind::Lpar);

        if self.eat(TokenKind::Rpar) {
            return ast::Arguments {
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
                args: Box::default(),
                keywords: ThinVec::default(),
            };
        }

        let args_snapshot = self.expr_scratch.snapshot();
        let keywords_snapshot = self.keyword_scratch.snapshot();
        let mut seen_keyword_argument = false; // foo = 1
        let mut seen_keyword_unpacking = false; // **foo

        let has_trailing_comma =
            self.parse_comma_separated_list(RecoveryContextKind::Arguments, |parser| {
                let argument_start = parser.node_start();
                if parser.eat(TokenKind::DoubleStar) {
                    let value = parser.parse_conditional_expression_or_higher();

                    parser.keyword_scratch.push(ast::Keyword {
                        arg: None,
                        key: ast::KeywordKey::Bare,
                        value: value.expr,
                        range: parser.node_range(argument_start),
                        node_index: AtomicNodeIndex::NONE,
                    });

                    seen_keyword_unpacking = true;
                } else {
                    let start = parser.node_start();
                    let mut parsed_expr = parser
                        .parse_named_expression_or_higher(ExpressionContext::starred_conditional());

                    match parser.current_token_kind() {
                        TokenKind::Async | TokenKind::For => {
                            if parsed_expr.is_unparenthesized_starred_expr() {
                                parser.add_unsupported_syntax_error(
                                    UnsupportedSyntaxErrorKind::UnpackingInComprehension(
                                        ComprehensionUnpackingKind::IterableInGenerator,
                                    ),
                                    parsed_expr.range(),
                                );
                            }

                            parsed_expr = Expr::Generator(parser.parse_generator_expression(
                                parsed_expr.expr,
                                start,
                                Parenthesized::No,
                            ))
                            .into();
                        }
                        _ => {
                            if seen_keyword_unpacking
                                && parsed_expr.is_unparenthesized_starred_expr()
                            {
                                parser.add_error(
                                    ParseErrorType::InvalidArgumentUnpackingOrder,
                                    &parsed_expr,
                                );
                            }
                        }
                    }

                    let arg_range = parser.node_range(start);
                    if parser.eat(TokenKind::Equal) {
                        seen_keyword_argument = true;
                        let (arg, key) = if let ParsedExpr {
                            expr: Expr::Name(ident_expr),
                            is_parenthesized,
                            parameter_borrow: _,
                        } = parsed_expr
                        {
                            // test_ok parenthesized_kwarg_py37
                            // # parse_options: {"target-version": "3.7"}
                            // f((a)=1)

                            // test_err parenthesized_kwarg_py38
                            // # parse_options: {"target-version": "3.8"}
                            // f((a)=1)
                            // f((a) = 1)
                            // f( ( a ) = 1)

                            if is_parenthesized {
                                parser.add_unsupported_syntax_error(
                                    UnsupportedSyntaxErrorKind::ParenthesizedKeywordArgumentName,
                                    arg_range,
                                );
                            }

                            (
                                ast::Identifier {
                                    id: ident_expr.id,
                                    range: ident_expr.range,
                                    node_index: AtomicNodeIndex::NONE,
                                },
                                ast::KeywordKey::Bare,
                            )
                        } else if let Some((id, key)) = flexible_keyword_name(&parsed_expr.expr) {
                            let name_range = parsed_expr.range();
                            parser.error_if_not_basedpython_at(
                                format!(
                                    "a keyword argument named `{}` is not valid in `.py` files",
                                    parser.src_text(name_range)
                                ),
                                name_range,
                            );
                            // the name is re-rendered from this span by anything
                            // that reprints the source, and a slice spanning lines
                            // is not something the formatter can lay out
                            if parser.src_text(name_range).contains(['\n', '\r']) {
                                parser.add_error(
                                    ParseErrorType::OtherError(
                                        "A keyword argument name may not span lines".to_string(),
                                    ),
                                    name_range,
                                );
                            }
                            (
                                ast::Identifier {
                                    id,
                                    range: name_range,
                                    node_index: AtomicNodeIndex::NONE,
                                },
                                key,
                            )
                        } else {
                            // TODO(dhruvmanila): Parser shouldn't drop the `parsed_expr` if it's
                            // not a name expression. We could add the expression into `args` but
                            // that means the error is a missing comma instead.
                            parser.add_error(
                                ParseErrorType::OtherError("Expected a parameter name".to_string()),
                                &parsed_expr,
                            );
                            (
                                ast::Identifier {
                                    id: Name::empty(),
                                    range: parsed_expr.range(),
                                    node_index: AtomicNodeIndex::NONE,
                                },
                                ast::KeywordKey::Bare,
                            )
                        };

                        let value = parser.parse_conditional_expression_or_higher();

                        parser.keyword_scratch.push(ast::Keyword {
                            arg: Some(arg),
                            key,
                            value: value.expr,
                            range: parser.node_range(argument_start),
                            node_index: AtomicNodeIndex::NONE,
                        });
                    } else {
                        if !parsed_expr.is_unparenthesized_starred_expr() {
                            if seen_keyword_unpacking {
                                parser.add_error(
                                    ParseErrorType::PositionalAfterKeywordUnpacking,
                                    &parsed_expr,
                                );
                            } else if seen_keyword_argument {
                                parser.add_error(
                                    ParseErrorType::PositionalAfterKeywordArgument,
                                    &parsed_expr,
                                );
                            }
                        }
                        parser.expr_scratch.push(parsed_expr.expr);
                    }
                }
            });

        self.expect(TokenKind::Rpar);

        let keywords = self.keyword_scratch.take_thin_vec(keywords_snapshot);
        let arguments = ast::Arguments {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            args: self.expr_scratch.take(args_snapshot),
            keywords,
        };

        self.validate_arguments(&arguments, has_trailing_comma, context);

        arguments
    }

    /// Parses a subscript expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `[` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#subscriptions>
    fn parse_subscript_expression(
        &mut self,
        mut value: Expr,
        start: TextSize,
    ) -> ast::ExprSubscript {
        self.bump(TokenKind::Lsqb);

        // To prevent the `value` context from being `Del` within a `del` statement,
        // we set the context as `Load` here.
        helpers::set_expr_ctx(&mut value, ExprContext::Load);

        // Slice range doesn't include the `[` token.
        let slice_start = self.node_start();

        // Create an error when receiving an empty slice to parse, e.g. `x[]`
        if self.eat(TokenKind::Rsqb) {
            let slice_range = self.node_range(slice_start);
            self.add_error(ParseErrorType::EmptySlice, slice_range);

            return ast::ExprSubscript {
                value: Box::new(value),
                slice: Box::new(Expr::Name(ast::ExprName {
                    range: slice_range,
                    id: Name::empty(),
                    ctx: ExprContext::Invalid,
                    node_index: AtomicNodeIndex::NONE,
                })),
                ctx: ExprContext::Load,
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
                is_typeof: false,
                is_type_decoration: false,
            };
        }

        // basedpython: a bare `*` terminated by `]` or `,` is the `[*]` /
        // `[..., *, ...]` shorthand for `Top[X[Any, ...]]`. The marker is a
        // `Starred(Name(id="", ctx=Invalid))` — an empty-id Name with invalid
        // context is uniquely synthesized and cannot appear from any normal
        // parse, so downstream consumers detect this shape unambiguously.
        let bare_star_marker = if self.at(TokenKind::Star)
            && matches!(self.peek(), TokenKind::Rsqb | TokenKind::Comma)
        {
            self.error_if_not_basedpython(
                "bare `*` in subscription is not valid in .py files".to_string(),
            );
            let star_start = self.node_start();
            self.bump(TokenKind::Star);
            let star_range = self.node_range(star_start);
            let marker_name = Expr::Name(ast::ExprName {
                range: TextRange::empty(star_range.end()),
                id: Name::empty(),
                ctx: ExprContext::Invalid,
                node_index: AtomicNodeIndex::NONE,
            });
            Some(Expr::Starred(ast::ExprStarred {
                value: Box::new(marker_name),
                ctx: ExprContext::Load,
                range: star_range,
                node_index: AtomicNodeIndex::NONE,
            }))
        } else {
            None
        };

        // basedpython: use-site variance keywords (`out X`, `in X`, `in out X`)
        // in subscript element position. After parsing the inner element the
        // variance is encoded as `Subscript(Name(marker, Invalid), inner)`.
        // The keyword tokens' combined range (no trailing whitespace) becomes
        // the marker Name's range so the formatter round-trips correctly.
        let variance_prefix_start = self.node_start();
        let (variance_marker, variance_marker_range) = if bare_star_marker.is_none() {
            let v = self.eat_basedpython_variance_prefix();
            let range = if v.is_some() {
                self.node_range(variance_prefix_start)
            } else {
                TextRange::empty(variance_prefix_start)
            };
            (v, range)
        } else {
            (None, TextRange::empty(variance_prefix_start))
        };

        // basedpython: a single keyword arg in subscription `x[k=v]` —
        // detect before parse_slice (which would error on the `=`)
        let mut slice = if let Some(marker) = bare_star_marker {
            marker
        } else if self.at(TokenKind::Name) && self.peek() == TokenKind::Equal {
            self.error_if_not_basedpython(
                "keyword arguments in subscription are not valid in .py files".to_string(),
            );
            let field_start = self.node_start();
            let mut target_name = self.parse_name(ExpressionContext::default());
            target_name.ctx = ExprContext::Invalid;
            self.expect(TokenKind::Equal);
            let inner =
                self.parse_conditional_expression_or_higher_impl(ExpressionContext::default());
            let field_range = self.node_range(field_start);
            Expr::Named(ast::ExprNamed {
                target: Box::new(Expr::Name(target_name)),
                value: Box::new(inner.expr),
                range: field_range,
                node_index: AtomicNodeIndex::NONE,
            })
        } else {
            self.parse_slice()
        };

        if let Some(variance) = variance_marker {
            slice = Self::wrap_variance_marker(slice, variance, variance_marker_range);
        }

        // If there are more than one element in the slice, we need to create a tuple
        // expression to represent it.
        if self.eat(TokenKind::Comma) {
            let slices_snapshot = self.expr_scratch.snapshot();
            self.expr_scratch.push(slice);

            self.parse_comma_separated_list(RecoveryContextKind::Slices, |parser| {
                // basedpython: bare `*` element (top-star marker) terminated
                // by `,` or `]`
                if parser.at(TokenKind::Star)
                    && matches!(parser.peek(), TokenKind::Rsqb | TokenKind::Comma)
                {
                    parser.error_if_not_basedpython(
                        "bare `*` in subscription is not valid in .py files".to_string(),
                    );
                    let star_start = parser.node_start();
                    parser.bump(TokenKind::Star);
                    let star_range = parser.node_range(star_start);
                    let marker_name = Expr::Name(ast::ExprName {
                        range: TextRange::empty(star_range.end()),
                        id: Name::empty(),
                        ctx: ExprContext::Invalid,
                        node_index: AtomicNodeIndex::NONE,
                    });
                    parser.expr_scratch.push(Expr::Starred(ast::ExprStarred {
                        value: Box::new(marker_name),
                        ctx: ExprContext::Load,
                        range: star_range,
                        node_index: AtomicNodeIndex::NONE,
                    }));
                    return;
                }
                // basedpython: use-site variance keywords (`out X`, `in X`,
                // `in out X`) in subsequent slice elements
                let variance_prefix_start = parser.node_start();
                let variance_marker = parser.eat_basedpython_variance_prefix();
                let variance_marker_range = if variance_marker.is_some() {
                    parser.node_range(variance_prefix_start)
                } else {
                    TextRange::empty(variance_prefix_start)
                };

                // basedpython: keyword arg in subscription `x[a, k=v]` —
                // store as `Expr::Named(target=Name(k), value=v)` like
                // anonymous-named-tuple value form
                if parser.at(TokenKind::Name) && parser.peek() == TokenKind::Equal {
                    parser.error_if_not_basedpython(
                        "keyword arguments in subscription are not valid in .py files".to_string(),
                    );
                    let field_start = parser.node_start();
                    let mut target_name = parser.parse_name(ExpressionContext::default());
                    target_name.ctx = ExprContext::Invalid;
                    parser.expect(TokenKind::Equal);
                    let inner = parser
                        .parse_conditional_expression_or_higher_impl(ExpressionContext::default());
                    let field_range = parser.node_range(field_start);
                    parser.expr_scratch.push(Expr::Named(ast::ExprNamed {
                        target: Box::new(Expr::Name(target_name)),
                        value: Box::new(inner.expr),
                        range: field_range,
                        node_index: AtomicNodeIndex::NONE,
                    }));
                    return;
                }
                let mut element = parser.parse_slice();
                if let Some(variance) = variance_marker {
                    element = Self::wrap_variance_marker(element, variance, variance_marker_range);
                }
                parser.expr_scratch.push(element);
            });

            slice = Expr::Tuple(ast::ExprTuple {
                elts: self.expr_scratch.take(slices_snapshot),
                ctx: ExprContext::Load,
                range: self.node_range(slice_start),
                parenthesized: false,
                is_anon_named_tuple: false,
                is_anon_named_tuple_value: false,
                callable_shape: None,
                is_parameter_shape: false,
                node_index: AtomicNodeIndex::NONE,
            });
        } else if slice.is_starred_expr() && !ruff_python_ast::helpers::is_top_star_marker(&slice) {
            // If the only slice element is a starred expression, that is represented
            // using a tuple expression with a single element. This is the second case
            // in the `slices` rule in the Python grammar.
            // basedpython top-star markers are kept unwrapped — downstream consumers
            // dispatch on the marker shape directly.
            slice = Expr::Tuple(ast::ExprTuple {
                elts: vec![slice],
                ctx: ExprContext::Load,
                range: self.node_range(slice_start),
                parenthesized: false,
                is_anon_named_tuple: false,
                is_anon_named_tuple_value: false,
                callable_shape: None,
                is_parameter_shape: false,
                node_index: AtomicNodeIndex::NONE,
            });
        }

        self.expect(TokenKind::Rsqb);

        // test_ok star_index_py311
        // # parse_options: {"target-version": "3.11"}
        // lst[*index]  # simple index
        // class Array(Generic[DType, *Shape]): ...  # motivating example from the PEP
        // lst[a, *b, c]  # different positions
        // lst[a, b, *c]  # different positions
        // lst[*a, *b]  # multiple unpacks
        // array[3:5, *idxs]  # mixed with slices

        // test_err star_index_py310
        // # parse_options: {"target-version": "3.10"}
        // lst[*index]  # simple index
        // class Array(Generic[DType, *Shape]): ...  # motivating example from the PEP
        // lst[a, *b, c]  # different positions
        // lst[a, b, *c]  # different positions
        // lst[*a, *b]  # multiple unpacks
        // array[3:5, *idxs]  # mixed with slices

        // test_err star_slices
        // array[*start:*end]

        // test_ok parenthesized_star_index_py310
        // # parse_options: {"target-version": "3.10"}
        // out[(*(slice(None) for _ in range(2)), *ind)] = 1
        if let Expr::Tuple(ast::ExprTuple {
            elts,
            parenthesized: false,
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: false,
            ..
        }) = &slice
        {
            for elt in elts.iter().filter(|elt| {
                elt.is_starred_expr() && !ruff_python_ast::helpers::is_top_star_marker(elt)
            }) {
                self.add_unsupported_syntax_error(
                    UnsupportedSyntaxErrorKind::StarExpressionInIndex,
                    elt.range(),
                );
            }
        }

        ast::ExprSubscript {
            value: Box::new(value),
            slice: Box::new(slice),
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            is_typeof: false,
            is_type_decoration: false,
        }
    }

    /// Parses a slice expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#slicings>
    fn parse_slice(&mut self) -> Expr {
        const UPPER_END_SET: TokenSet =
            TokenSet::new([TokenKind::Comma, TokenKind::Colon, TokenKind::Rsqb])
                .union(NEWLINE_EOF_SET);
        const STEP_END_SET: TokenSet =
            TokenSet::new([TokenKind::Comma, TokenKind::Rsqb]).union(NEWLINE_EOF_SET);

        // test_err named_expr_slice
        // # even after 3.9, an unparenthesized named expression is not allowed in a slice
        // lst[x:=1:-1]
        // lst[1:x:=1]
        // lst[1:3:x:=1]

        // test_err named_expr_slice_parse_error
        // # parse_options: {"target-version": "3.8"}
        // # before 3.9, only emit the parse error, not the unsupported syntax error
        // lst[x:=1:-1]

        let start = self.node_start();

        let lower = if self.at_expr() {
            let lower = self.parse_named_expression_or_higher(
                ExpressionContext::starred_conditional()
                    .with_subscript_slice()
                    .with_in_type_expression(),
            );

            // This means we're in a subscript.
            if self.at_ts(NEWLINE_EOF_SET.union([TokenKind::Rsqb, TokenKind::Comma].into())) {
                // test_ok parenthesized_named_expr_index_py38
                // # parse_options: {"target-version": "3.8"}
                // lst[(x:=1)]

                // test_ok unparenthesized_named_expr_index_py39
                // # parse_options: {"target-version": "3.9"}
                // lst[x:=1]

                // test_err unparenthesized_named_expr_index_py38
                // # parse_options: {"target-version": "3.8"}
                // lst[x:=1]
                if lower.is_unparenthesized_named_expr() {
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::UnparenthesizedNamedExpr(
                            UnparenthesizedNamedExprKind::SequenceIndex,
                        ),
                        lower.range(),
                    );
                }
                return lower.expr;
            }

            // Now we know we're in a slice.
            if !lower.is_parenthesized {
                match lower.expr {
                    Expr::Starred(_) => {
                        self.add_error(ParseErrorType::InvalidStarredExpressionUsage, &lower);
                    }
                    Expr::Named(_) => {
                        self.add_error(ParseErrorType::UnparenthesizedNamedExpression, &lower);
                    }
                    _ => {}
                }
            }

            Some(lower.expr)
        } else {
            None
        };

        self.expect(TokenKind::Colon);

        let lower = lower.map(Box::new);
        let upper = if self.at_ts(UPPER_END_SET) {
            None
        } else {
            Some(Box::new(self.parse_conditional_expression_or_higher().expr))
        };

        let step = if self.eat(TokenKind::Colon) {
            if self.at_ts(STEP_END_SET) {
                None
            } else {
                Some(Box::new(self.parse_conditional_expression_or_higher().expr))
            }
        } else {
            None
        };

        Expr::Slice(ast::ExprSlice {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            lower,
            upper,
            step,
        })
    }

    /// Parses a unary expression.
    ///
    /// This includes the unary arithmetic `+` and `-`, bitwise `~`, and the
    /// boolean `not` operators.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at any of the unary operators.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#unary-arithmetic-and-bitwise-operations>
    pub(super) fn parse_unary_expression(
        &mut self,
        op: UnaryOp,
        context: ExpressionContext,
    ) -> ast::ExprUnaryOp {
        let start = self.node_start();
        self.bump(TokenKind::from(op));

        let operand = self.parse_binary_expression_or_higher(OperatorPrecedence::from(op), context);

        ast::ExprUnaryOp {
            op,
            operand: Box::new(operand.expr),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an attribute expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `.` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#attribute-references>
    pub(super) fn parse_attribute_expression(
        &mut self,
        value: Expr,
        start: TextSize,
        context: ExpressionContext,
    ) -> ast::ExprAttribute {
        self.bump(TokenKind::Dot);

        let attr = self.parse_identifier_with_context(context);

        ast::ExprAttribute {
            value: Box::new(value),
            attr,
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            optional: false,
        }
    }

    /// Parse `a?.b` — basedpython None-chaining attribute access.
    fn parse_optional_attribute_expression(
        &mut self,
        value: Expr,
        start: TextSize,
    ) -> ast::ExprAttribute {
        self.bump(TokenKind::QuestionDot);

        let attr = self.parse_identifier();

        ast::ExprAttribute {
            value: Box::new(value),
            attr,
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            optional: true,
        }
    }

    /// Parses a boolean operation expression.
    ///
    /// Note that the boolean `not` operator is parsed as a unary expression and
    /// not as a boolean expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `or` or `and` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#boolean-operations>
    fn parse_boolean_expression(
        &mut self,
        lhs: Expr,
        start: TextSize,
        op: BoolOp,
        context: ExpressionContext,
    ) -> ast::ExprBoolOp {
        self.bump(TokenKind::from(op));

        let values_snapshot = self.expr_scratch.snapshot();
        self.expr_scratch.push(lhs);
        let mut progress = ParserProgress::default();

        // Keep adding the expression to `values` until we see a different
        // token than `operator_token`.
        loop {
            progress.assert_progressing(self);

            let parsed_expr =
                self.parse_binary_expression_or_higher(OperatorPrecedence::from(op), context);
            self.expr_scratch.push(parsed_expr.expr);

            if !self.eat(TokenKind::from(op)) {
                break;
            }
        }

        ast::ExprBoolOp {
            values: self.expr_scratch.take(values_snapshot),
            op,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Bump the appropriate token(s) for the given comparison operator.
    fn bump_cmp_op(&mut self, op: CmpOp) {
        let (first, second) = match op {
            CmpOp::Eq => (TokenKind::EqEqual, None),
            CmpOp::NotEq => (TokenKind::NotEqual, None),
            CmpOp::Lt => (TokenKind::Less, None),
            CmpOp::LtE => (TokenKind::LessEqual, None),
            CmpOp::Gt => (TokenKind::Greater, None),
            CmpOp::GtE => (TokenKind::GreaterEqual, None),
            CmpOp::Is => {
                // accept either `is` or `===`; bump whichever the lexer emitted
                if self.at(TokenKind::EqEqEqual) {
                    (TokenKind::EqEqEqual, None)
                } else {
                    (TokenKind::Is, None)
                }
            }
            CmpOp::IsNot => {
                // accept either `is not` or `!==`; bump whichever the lexer emitted
                if self.at(TokenKind::BangEqEqual) {
                    (TokenKind::BangEqEqual, None)
                } else {
                    (TokenKind::Is, Some(TokenKind::Not))
                }
            }
            CmpOp::In => (TokenKind::In, None),
            CmpOp::NotIn => (TokenKind::Not, Some(TokenKind::In)),
        };

        self.bump(first);
        if let Some(second) = second {
            self.bump(second);
        }
    }

    /// Parse a comparison expression.
    ///
    /// This includes the following operators:
    /// - Value comparisons: `==`, `!=`, `<`, `<=`, `>`, and `>=`.
    /// - Membership tests: `in` and `not in`.
    /// - Identity tests: `is` and `is not`.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at any of the comparison operators.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#comparisons>
    fn parse_comparison_expression(
        &mut self,
        lhs: Expr,
        start: TextSize,
        op: CmpOp,
        context: ExpressionContext,
    ) -> ast::ExprCompare {
        self.bump_cmp_op(op);

        let comparators_snapshot = self.expr_scratch.snapshot();
        let mut operators = vec![op];

        let mut progress = ParserProgress::default();

        loop {
            progress.assert_progressing(self);

            let comparator = self
                .parse_binary_expression_or_higher(
                    OperatorPrecedence::ComparisonsMembershipIdentity,
                    context,
                )
                .expr;
            self.expr_scratch.push(comparator);

            let next_token = self.current_token_kind();
            if matches!(next_token, TokenKind::In) && context.is_in_excluded() {
                break;
            }

            let next_next_token =
                matches!(next_token, TokenKind::Is | TokenKind::Not).then(|| self.peek());
            let Some(next_op) = helpers::token_kind_to_cmp_op(next_token, next_next_token) else {
                break;
            };

            self.bump_cmp_op(next_op);
            operators.push(next_op);
        }

        ast::ExprCompare {
            left: Box::new(lhs),
            ops: operators.into_boxed_slice(),
            comparators: self.expr_scratch.take(comparators_snapshot),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses all kinds of strings and implicitly concatenated strings.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `String`, `FStringStart`, or `TStringStart` token.
    ///
    /// See: <https://docs.python.org/3/reference/grammar.html> (Search "strings:")
    pub(super) fn parse_strings(&mut self) -> Expr {
        const STRING_START_SET: TokenSet = TokenSet::new([
            TokenKind::String,
            TokenKind::FStringStart,
            TokenKind::TStringStart,
        ]);

        let start = self.node_start();
        let first = self.parse_string();

        if !self.at_ts(STRING_START_SET) {
            return first.into();
        }

        let mut strings = Vec::with_capacity(2);
        strings.push(first);

        let mut progress = ParserProgress::default();

        while self.at_ts(STRING_START_SET) {
            progress.assert_progressing(self);
            strings.push(self.parse_string());
        }

        let range = self.node_range(start);
        self.handle_implicitly_concatenated_strings(strings, range)
    }

    /// Parses a single string, byte literal, f-string, or t-string.
    fn parse_string(&mut self) -> StringType {
        if self.at(TokenKind::String) {
            self.parse_string_or_byte_literal()
        } else if self.at(TokenKind::FStringStart) {
            StringType::FString(
                self.parse_interpolated_string(InterpolatedStringKind::FString)
                    .into(),
            )
        } else if self.at(TokenKind::TStringStart) {
            // test_ok template_strings_py314
            // # parse_options: {"target-version": "3.14"}
            // t"{hey}"
            // t'{there}'
            // t"""what's
            // happening?"""

            // test_err template_strings_py313
            // # parse_options: {"target-version": "3.13"}
            // t"{hey}"
            // t'{there}'
            // t"""what's
            // happening?"""
            let string_type = StringType::TString(
                self.parse_interpolated_string(InterpolatedStringKind::TString)
                    .into(),
            );
            self.add_unsupported_syntax_error(
                UnsupportedSyntaxErrorKind::TemplateStrings,
                string_type.range(),
            );
            string_type
        } else {
            unreachable!("Expected to parse a string")
        }
    }

    /// Handles implicitly concatenated strings.
    ///
    /// # Panics
    ///
    /// If the length of `strings` is less than 2.
    fn handle_implicitly_concatenated_strings(
        &mut self,
        strings: Vec<StringType>,
        range: TextRange,
    ) -> Expr {
        assert!(strings.len() > 1);

        let mut has_fstring = false;
        let mut byte_literal_count = 0;
        let mut tstring_count = 0;
        for string in &strings {
            match string {
                StringType::FString(_) => has_fstring = true,
                StringType::TString(_) => tstring_count += 1,
                StringType::Bytes(_) => byte_literal_count += 1,
                StringType::Str(_) => {}
            }
        }
        let has_bytes = byte_literal_count > 0;
        let has_tstring = tstring_count > 0;

        if has_bytes {
            if byte_literal_count < strings.len() {
                // TODO(dhruvmanila): This is not an ideal recovery because the parser
                // replaces the byte literals with an invalid string literal node. Any
                // downstream tools can extract the raw bytes from the range.
                //
                // We could convert the node into a string and mark it as invalid
                // and would be clever to mark the type which is fewer in quantity.

                // test_err mixed_tstring_and_bytes_literals
                // t'first' b'second'
                // b'first' t'second'
                // t'first' br'second'
                // 'first' b'second' t'third'
                // b'first' 'second' t'third'
                // 'first' t'second' 'third' b'fourth'
                // b'first' t'second' f'third'

                // test_err mixed_bytes_and_non_bytes_literals
                // 'first' b'second'
                // f'first' b'second'
                // 'first' f'second' b'third'
                self.report_mixed_string_literal_error(&strings, range);
            }
            // Only construct a byte expression if all the literals are bytes
            // otherwise, we'll try either string, t-string, or f-string. This is to retain
            // as much information as possible.
            else {
                let mut values = Vec::with_capacity(strings.len());
                for string in strings {
                    values.push(match string {
                        StringType::Bytes(value) => value,
                        _ => unreachable!("Expected `StringType::Bytes`"),
                    });
                }
                return Expr::from(ast::ExprBytesLiteral {
                    value: ast::BytesLiteralValue::concatenated(values),
                    range,
                    node_index: AtomicNodeIndex::NONE,
                });
            }
        } else if has_tstring {
            if tstring_count < strings.len() {
                self.report_mixed_string_literal_error(&strings, range);
            }
            // Only construct a t-string expression if all the literals are t-strings
            // otherwise, we'll try either string or f-string. This is to retain
            // as much information as possible.
            else {
                let mut values = Vec::with_capacity(strings.len());
                for string in strings {
                    values.push(match string {
                        StringType::TString(value) => value,
                        _ => unreachable!("Expected `StringType::TString`"),
                    });
                }
                return Expr::from(ast::ExprTString {
                    value: ast::TStringValue::concatenated(values),
                    range,
                    node_index: AtomicNodeIndex::NONE,
                });
            }
        }

        // TODO(dhruvmanila): Parser drops unterminated strings here as well
        // because the lexer doesn't emit them.

        // test_err implicitly_concatenated_unterminated_string
        // 'hello' 'world
        // 1 + 1
        // 'hello' f'world {x}
        // 2 + 2

        // test_err implicitly_concatenated_unterminated_string_multiline
        // (
        //     'hello'
        //     f'world {x}
        // )
        // 1 + 1
        // (
        //     'first'
        //     'second
        //     f'third'
        // )
        // 2 + 2

        if !has_fstring && !has_tstring {
            let mut values = Vec::with_capacity(strings.len());
            for string in strings {
                values.push(match string {
                    StringType::Str(value) => value,
                    _ => ast::StringLiteral::invalid(string.range()),
                });
            }
            return Expr::from(ast::ExprStringLiteral {
                value: ast::StringLiteralValue::concatenated(values),
                range,
                node_index: AtomicNodeIndex::NONE,
            });
        }

        let mut parts = Vec::with_capacity(strings.len());
        for string in strings {
            match string {
                StringType::FString(fstring) => parts.push(ast::FStringPart::FString(fstring)),
                StringType::Str(string) => parts.push(ast::FStringPart::Literal(string)),
                // Bytes and Template strings are invalid at this point
                // and stored as invalid string literal parts in the
                // f-string
                StringType::TString(tstring) => parts.push(ast::FStringPart::Literal(
                    ast::StringLiteral::invalid(tstring.range()),
                )),
                StringType::Bytes(bytes) => parts.push(ast::FStringPart::Literal(
                    ast::StringLiteral::invalid(bytes.range()),
                )),
            }
        }

        Expr::from(ast::ExprFString {
            value: ast::FStringValue::concatenated(parts),
            range,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    fn report_mixed_string_literal_error(&mut self, strings: &[StringType], range: TextRange) {
        // CPython reports the first incompatible pair. A t-string mismatch takes
        // precedence over a bytes mismatch within that pair.
        for pair in strings.windows(2) {
            let message = match pair {
                [StringType::TString(_), StringType::TString(_)]
                | [StringType::Bytes(_), StringType::Bytes(_)] => continue,
                [StringType::TString(_), _] | [_, StringType::TString(_)] => {
                    "Cannot mix t-string literals with string or bytes literals"
                }
                [StringType::Bytes(_), _] | [_, StringType::Bytes(_)] => {
                    "Bytes literal cannot be mixed with non-bytes literals"
                }
                _ => continue,
            };
            self.add_error(ParseErrorType::OtherError(message.to_string()), range);
            break;
        }
    }

    /// Parses a single string or byte literal.
    ///
    /// This does not handle implicitly concatenated strings.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `String` token.
    ///
    /// See: <https://docs.python.org/3/reference/lexical_analysis.html#string-and-bytes-literals>
    fn parse_string_or_byte_literal(&mut self) -> StringType {
        let range = self.current_token_range();
        let flags = self.tokens.current_flags().as_any_string_flags();

        let value = self.bump_string_value();

        match parse_string_literal(value, flags, range) {
            Ok(string) => string,
            Err(error) => {
                let location = error.location();
                self.add_error(ParseErrorType::Lexical(error.into_error()), location);

                if flags.is_byte_string() {
                    // test_err invalid_byte_literal
                    // b'123a𝐁c'
                    // rb"a𝐁c123"
                    // b"""123a𝐁c"""
                    StringType::Bytes(ast::BytesLiteral {
                        value: Box::new([]),
                        range,
                        flags: ast::BytesLiteralFlags::from(flags).with_invalid(),
                        node_index: AtomicNodeIndex::NONE,
                    })
                } else {
                    // test_err invalid_string_literal
                    // 'hello \N{INVALID} world'
                    // """hello \N{INVALID} world"""
                    StringType::Str(ast::StringLiteral {
                        value: "".into(),
                        range,
                        flags: ast::StringLiteralFlags::from(flags).with_invalid(),
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
            }
        }
    }

    /// Parses an f/t-string.
    ///
    /// This does not handle implicitly concatenated strings.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `FStringStart` or
    /// `TStringStart` token.
    ///
    /// See: <https://docs.python.org/3/reference/grammar.html> (Search "fstring:" or "tstring:")
    /// See: <https://docs.python.org/3/reference/lexical_analysis.html#formatted-string-literals>
    fn parse_interpolated_string(
        &mut self,
        kind: InterpolatedStringKind,
    ) -> InterpolatedStringData {
        let start = self.node_start();
        let mut flags = self.tokens.current_flags().as_any_string_flags();

        self.bump(kind.start_token());
        let elements = self.parse_interpolated_string_elements(
            flags,
            InterpolatedStringElementsKind::Regular(kind),
            kind,
        );

        if !self.expect(kind.end_token()) {
            flags = flags.with_unclosed(true);
        }

        InterpolatedStringData {
            elements,
            range: self.node_range(start),
            flags,
        }
    }

    /// Check `range` for comment tokens, report an `UnsupportedSyntaxError` for each one found,
    /// and return whether any comments were found.
    fn check_fstring_comments(&mut self, range: TextRange) -> bool {
        let mut has_comments = false;

        self.unsupported_syntax_errors.extend(
            self.tokens
                .in_range(range)
                .iter()
                .filter(|token| token.kind().is_comment())
                .map(|token| {
                    has_comments = true;
                    UnsupportedSyntaxError {
                        kind: UnsupportedSyntaxErrorKind::Pep701FString(FStringKind::Comment),
                        range: token.range(),
                        target_version: self.options.target_version,
                    }
                }),
        );

        has_comments
    }

    /// Parses a list of f/t-string elements.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `{`, `FStringMiddle`,
    /// or `TStringMiddle` token.
    fn parse_interpolated_string_elements(
        &mut self,
        flags: ast::AnyStringFlags,
        elements_kind: InterpolatedStringElementsKind,
        string_kind: InterpolatedStringKind,
    ) -> ast::InterpolatedStringElements {
        let mut elements = vec![];
        let middle_token_kind = string_kind.middle_token();

        self.parse_list(
            RecoveryContextKind::InterpolatedStringElements(elements_kind),
            |parser| {
                let element = match parser.current_token_kind() {
                    TokenKind::Lbrace => ast::InterpolatedStringElement::from(
                        parser.parse_interpolated_element(flags, string_kind),
                    ),
                    tok if tok == middle_token_kind => {
                        let range = parser.current_token_range();
                        let value = parser.current_token_text();
                        parser.bump(middle_token_kind);
                        InterpolatedStringElement::Literal(
                            parse_interpolated_string_literal_element(value, flags, range)
                                .unwrap_or_else(|lex_error| {
                                    // test_err invalid_fstring_literal_element
                                    // f'hello \N{INVALID} world'
                                    // f"""hello \N{INVALID} world"""
                                    let location = lex_error.location();
                                    parser.add_error(
                                        ParseErrorType::Lexical(lex_error.into_error()),
                                        location,
                                    );
                                    ast::InterpolatedStringLiteralElement {
                                        value: "".into(),
                                        range,
                                        node_index: AtomicNodeIndex::NONE,
                                    }
                                }),
                        )
                    }
                    // `Invalid` tokens are created when there's a lexical error, so
                    // we ignore it here to avoid creating unexpected token errors
                    TokenKind::Unknown => {
                        parser.bump_any();
                        return;
                    }
                    tok => {
                        // This should never happen because the list parsing will only
                        // call this closure for the above token kinds which are the same
                        // as in the FIRST set.
                        unreachable!(
                            "{}: unexpected token `{tok:?}` at {:?}",
                            string_kind,
                            parser.current_token_range()
                        );
                    }
                };
                elements.push(element);
            },
        );

        ast::InterpolatedStringElements::from(elements)
    }

    /// Parses an f/t-string expression element.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `{` token.
    fn parse_interpolated_element(
        &mut self,
        flags: ast::AnyStringFlags,
        string_kind: InterpolatedStringKind,
    ) -> ast::InterpolatedElement {
        let start = self.node_start();
        self.bump(TokenKind::Lbrace);

        self.tokens
            .re_lex_string_token_in_interpolation_element(string_kind);

        // test_err f_string_empty_expression
        // f"{}"
        // f"{  }"

        // test_err t_string_empty_expression
        // # parse_options: {"target-version": "3.14"}
        // t"{}"
        // t"{  }"

        // test_err f_string_invalid_starred_expr
        // # Starred expression inside f-string has a minimum precedence of bitwise or.
        // f"{*}"
        // f"{*x and y}"
        // f"{*yield x}"

        // test_err t_string_invalid_starred_expr
        // # parse_options: {"target-version": "3.14"}
        // # Starred expression inside t-string has a minimum precedence of bitwise or.
        // t"{*}"
        // t"{*x and y}"
        // t"{*yield x}"

        let value = self.parse_expression_list(
            ExpressionContext::yield_or_starred_bitwise_or().with_in_interpolation(),
        );

        if !value.is_parenthesized && value.expr.is_lambda_expr() {
            // TODO(dhruvmanila): This requires making some changes in lambda expression
            // parsing logic to handle the emitted `FStringMiddle` token in case the
            // lambda expression is not parenthesized.

            // test_err f_string_lambda_without_parentheses
            // f"{lambda x: x}"

            // test_err t_string_lambda_without_parentheses
            // # parse_options: {"target-version": "3.14"}
            // t"{lambda x: x}"
            self.add_error(
                ParseErrorType::from_interpolated_string_error(
                    InterpolatedStringErrorType::LambdaWithoutParentheses,
                    string_kind,
                ),
                value.range(),
            );
        }
        let debug_text = if self.eat(TokenKind::Equal) {
            let leading_range = TextRange::new(start + "{".text_len(), value.start());
            let trailing_range = TextRange::new(value.end(), self.current_token_range().start());
            Some(ast::DebugText::new(
                self.src_text(leading_range),
                self.src_text(value.range()),
                self.src_text(trailing_range),
            ))
        } else {
            None
        };

        let conversion = if self.eat(TokenKind::Exclamation) {
            // Ensure that the `r` is lexed as a `r` name token instead of a raw string
            // in `f{abc!r"` (note the missing `}`).
            self.tokens.re_lex_raw_string_in_format_spec();

            let conversion_flag_range = self.current_token_range();
            if self.at(TokenKind::Name) {
                // test_err f_string_conversion_follows_exclamation
                // f"{x! s}"
                // t"{x! s}"
                // f"{x! z}"
                if self.prev_token_end != conversion_flag_range.start() {
                    self.add_error(
                        ParseErrorType::from_interpolated_string_error(
                            InterpolatedStringErrorType::ConversionFlagNotImmediatelyAfterExclamation,
                            string_kind,
                        ),
                        TextRange::new(self.prev_token_end, conversion_flag_range.start()),
                    );
                }
                let name = self.bump_name();
                match &*name {
                    "s" => ConversionFlag::Str,
                    "r" => ConversionFlag::Repr,
                    "a" => ConversionFlag::Ascii,
                    _ => {
                        // test_err f_string_invalid_conversion_flag_name_tok
                        // f"{x!z}"

                        // test_err t_string_invalid_conversion_flag_name_tok
                        // # parse_options: {"target-version": "3.14"}
                        // t"{x!z}"
                        self.add_error(
                            ParseErrorType::from_interpolated_string_error(
                                InterpolatedStringErrorType::InvalidConversionFlag,
                                string_kind,
                            ),
                            conversion_flag_range,
                        );
                        ConversionFlag::None
                    }
                }
            } else {
                // test_err f_string_invalid_conversion_flag_other_tok
                // f"{x!123}"
                // f"{x!'a'}"

                // test_err t_string_invalid_conversion_flag_other_tok
                // # parse_options: {"target-version": "3.14"}
                // t"{x!123}"
                // t"{x!'a'}"
                self.add_error(
                    ParseErrorType::from_interpolated_string_error(
                        InterpolatedStringErrorType::InvalidConversionFlag,
                        string_kind,
                    ),
                    conversion_flag_range,
                );
                // TODO(dhruvmanila): Avoid dropping this token
                self.bump_any();
                ConversionFlag::None
            }
        } else {
            ConversionFlag::None
        };

        let format_spec = if self.eat(TokenKind::Colon) {
            let spec_start = self.node_start();
            let elements = self.with_recursion(|parser| {
                parser.parse_interpolated_string_elements(
                    flags,
                    InterpolatedStringElementsKind::FormatSpec(string_kind),
                    string_kind,
                )
            });
            Some(Box::new(ast::InterpolatedStringFormatSpec {
                range: self.node_range(spec_start),
                elements,
                node_index: AtomicNodeIndex::NONE,
            }))
        } else {
            None
        };

        self.tokens
            .re_lex_string_token_in_interpolation_element(string_kind);

        // We're using `eat` here instead of `expect` to use the f-string specific error type.
        if !self.eat(TokenKind::Rbrace) {
            // TODO(dhruvmanila): This requires some changes in the lexer. One of them
            // would be to emit `FStringEnd`. Currently, the following test cases doesn't
            // really work as expected. Refer https://github.com/astral-sh/ruff/pull/10372

            // test_err f_string_unclosed_lbrace
            // f"{"
            // f"{foo!r"
            // f"{foo="
            // f"{"
            // f"""{"""

            // test_err t_string_unclosed_lbrace
            // # parse_options: {"target-version": "3.14"}
            // t"{"
            // t"{foo!r"
            // t"{foo="
            // t"{"
            // t"""{"""

            // The lexer does emit `FStringEnd` for the following test cases:

            // test_err f_string_unclosed_lbrace_in_format_spec
            // f"hello {x:"
            // f"hello {x:.3f"

            // test_err t_string_unclosed_lbrace_in_format_spec
            // # parse_options: {"target-version": "3.14"}
            // t"hello {x:"
            // t"hello {x:.3f"
            self.add_error(
                ParseErrorType::from_interpolated_string_error(
                    InterpolatedStringErrorType::UnclosedLbrace,
                    string_kind,
                ),
                self.current_token_range(),
            );
        }

        // test_ok pep701_f_string_py312
        // # parse_options: {"target-version": "3.12"}
        // f'Magic wand: { bag['wand'] }'     # nested quotes
        // f"{'\n'.join(a)}"                  # escape sequence
        // f'''A complex trick: {
        //     bag['bag']                     # comment
        // }'''
        // f"{f"{f"{f"{f"{f"{1+1}"}"}"}"}"}"  # arbitrary nesting
        // f"{f'''{"nested"} inner'''} outer" # nested (triple) quotes
        // f"{
        //     1
        // }"
        // f"test {a \
        //     } more"                        # line continuation

        // test_ok pep750_t_string_py314
        // # parse_options: {"target-version": "3.14"}
        // t'Magic wand: { bag['wand'] }'     # nested quotes
        // t"{'\n'.join(a)}"                  # escape sequence
        // t'''A complex trick: {
        //     bag['bag']                     # comment
        // }'''
        // t"{t"{t"{t"{t"{t"{1+1}"}"}"}"}"}"  # arbitrary nesting
        // t"{t'''{"nested"} inner'''} outer" # nested (triple) quotes
        // t"test {a \
        //     } more"                        # line continuation

        // test_ok pep701_f_string_py311
        // # parse_options: {"target-version": "3.11"}
        // f"outer {'# not a comment'}"
        // f'outer {x:{"# not a comment"} }'
        // f"""{f'''{f'{"# not a comment"}'}'''}"""
        // f"""{f'''# before expression {f'# aro{f"#{1+1}#"}und #'}'''} # after expression"""
        // f"""{
        //     1
        // }"""
        // f"escape outside of \t {expr}\n"
        // f"test\"abcd"
        // f"{1:\x64}"  # escapes are valid in the format spec
        // f"{1:\"d\"}"  # this also means that escaped outer quotes are valid

        // test_err pep701_f_string_py311
        // # parse_options: {"target-version": "3.11"}
        // f'Magic wand: { bag['wand'] }'     # nested quotes
        // f"{'\n'.join(a)}"                  # escape sequence
        // f'''A complex trick: {
        //     bag['bag']                     # comment
        // }'''
        // f"{f"{f"{f"{f"{f"{1+1}"}"}"}"}"}"  # arbitrary nesting
        // f"{f'''{"nested"} inner'''} outer" # nested (triple) quotes
        // f"{
        //     1
        // }"
        // f"test {a \
        //     } more"                        # line continuation
        // f"""{f"""{x}"""}"""                # mark the whole triple quote
        // f"{'\n'.join(['\t', '\v', '\r'])}"  # multiple escape sequences, multiple errors

        // test_err pep701_nested_interpolation_py311
        // # parse_options: {"target-version": "3.11"}
        // # nested interpolations also need to be checked
        // f'{1: abcd "{'aa'}" }'
        // f'{1: abcd "{"\n"}" }'

        // test_err nested_quote_in_format_spec_py312
        // # parse_options: {"target-version": "3.12"}
        // f"{1:""}"  # this is a ParseError on all versions

        // test_ok non_nested_quote_in_format_spec_py311
        // # parse_options: {"target-version": "3.11"}
        // f"{1:''}"  # but this is okay on all versions
        let range = self.node_range(start);

        if !self.options.target_version.supports_pep_701()
            && matches!(string_kind, InterpolatedStringKind::FString)
        {
            // We need to check the whole expression range, including any leading or trailing
            // debug text, but exclude the format spec, where escapes and escaped, reused quotes
            // are allowed.
            let range = format_spec
                .as_ref()
                .map(|format_spec| TextRange::new(range.start(), format_spec.start()))
                .unwrap_or(range);

            let quote_bytes = flags.quote_str().as_bytes();
            let quote_len = flags.quote_len();
            let mut has_backslash_or_comment = false;

            for slash_position in memchr::memchr_iter(b'\\', self.source[range].as_bytes()) {
                has_backslash_or_comment = true;
                let slash_position = TextSize::try_from(slash_position).unwrap();
                self.add_unsupported_syntax_error(
                    UnsupportedSyntaxErrorKind::Pep701FString(FStringKind::Backslash),
                    TextRange::at(range.start() + slash_position, '\\'.text_len()),
                );
            }

            if let Some(quote_position) =
                memchr::memmem::find(self.source[range].as_bytes(), quote_bytes)
            {
                let quote_position = TextSize::try_from(quote_position).unwrap();
                self.add_unsupported_syntax_error(
                    UnsupportedSyntaxErrorKind::Pep701FString(FStringKind::NestedQuote),
                    TextRange::at(range.start() + quote_position, quote_len),
                );
            }

            has_backslash_or_comment |= self.check_fstring_comments(range);

            // Before Python 3.12, replacement fields could only span physical lines when the
            // outer f-string was triple-quoted.
            if !flags.is_triple_quoted()
                && !has_backslash_or_comment
                && memchr::memchr2(b'\n', b'\r', self.source[range].as_bytes()).is_some()
            {
                self.add_unsupported_syntax_error(
                    UnsupportedSyntaxErrorKind::Pep701FString(FStringKind::LineBreak),
                    TextRange::at(range.start(), '{'.text_len()),
                );
            }
        }

        ast::InterpolatedElement {
            expression: Box::new(value.expr),
            debug_text,
            conversion,
            format_spec,
            range,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a list or a list comprehension expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `[` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#list-displays>
    fn parse_list_like_expression(&mut self, outer: ExpressionContext) -> Expr {
        let start = self.node_start();

        self.bump(TokenKind::Lsqb);

        // Nice error message when having a unclosed open bracket `[`
        if self.at_ts(NEWLINE_EOF_SET) {
            self.add_error(
                ParseErrorType::OtherError("Missing closing bracket `]`".to_string()),
                self.current_token_range(),
            );
        }

        // Return an empty `ListExpr` when finding a `]` right after the `[`
        if self.eat(TokenKind::Rsqb) {
            return Expr::List(ast::ExprList {
                elts: vec![],
                ctx: ExprContext::Load,
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
            });
        }

        // Parse the first element with a more general rule and limit it later.
        // basedpython: a list display inside a type expression is the parameter
        // list of `Callable[[…], R]`, so its elements are type expressions too
        let element_context = ExpressionContext::starred_bitwise_or()
            .with_for_excluded()
            .inheriting_type_expression(outer);
        let first_element = self.parse_named_expression_or_higher(element_context);

        match self.current_token_kind() {
            TokenKind::Async | TokenKind::For => {
                // Parenthesized starred expression isn't allowed either but that is
                // handled by the `parse_parenthesized_expression` method.

                // test_ok starred_list_comp_py315
                // # parse_options: {"target-version": "3.15"}
                // [*x for x in y]
                // [*factor.dims for factor in bases]

                // test_err starred_list_comp_py314
                // # parse_options: {"target-version": "3.14"}
                // [*x for x in y]
                if first_element.is_unparenthesized_starred_expr() {
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::UnpackingInComprehension(
                            ComprehensionUnpackingKind::IterableInList,
                        ),
                        first_element.range(),
                    );
                }

                Expr::ListComp(self.parse_list_comprehension_expression(first_element.expr, start))
            }
            _ => Expr::List(self.parse_list_expression(first_element.expr, start, outer)),
        }
    }

    /// Parses a set, dict, set comprehension, or dict comprehension.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `{` token.
    ///
    /// See:
    /// - <https://docs.python.org/3/reference/expressions.html#set-displays>
    /// - <https://docs.python.org/3/reference/expressions.html#dictionary-displays>
    /// - <https://docs.python.org/3/reference/expressions.html#displays-for-lists-sets-and-dictionaries>
    fn parse_set_or_dict_like_expression(&mut self) -> Expr {
        // test_ok pep_798_unpacking_comprehensions_py315
        // # parse_options: {"target-version": "3.15"}
        // [*x for x in y]
        // {*x for x in y}
        // {**x for x in y}
        // (*x for x in y)
        // f(*x for x in y)
        // [*x async for x in y]
        // {*x async for x in y}
        // {**x async for x in y}
        // (*x async for x in y)

        // test_err pep_798_unpacking_comprehensions_py314
        // # parse_options: {"target-version": "3.14"}
        // [*x for x in y]
        // {*x for x in y}
        // {**x for x in y}
        // (*x for x in y)
        // f(*x for x in y)

        // test_err pep_798_invalid_dict_unpacking_comprehensions_py315
        // # parse_options: {"target-version": "3.15"}
        // {*k: v for k, v in items}
        // {k: *v for k, v in items}
        // {**k: v for k, v in items}
        // {k: **v for k, v in items}

        let start = self.node_start();
        self.bump(TokenKind::Lbrace);

        // Nice error message when having a unclosed open brace `{`
        if self.at_ts(NEWLINE_EOF_SET) {
            self.add_error(
                ParseErrorType::OtherError("Missing closing brace `}`".to_string()),
                self.current_token_range(),
            );
        }

        // Return an empty `DictExpr` when finding a `}` right after the `{`
        if self.eat(TokenKind::Rbrace) {
            return Expr::Dict(ast::ExprDict {
                items: vec![],
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
            });
        }

        let after_brace = self.node_start();

        if self.at(TokenKind::DoubleStar) {
            self.bump(TokenKind::DoubleStar);
            // basedpython `**: T` extra-items marker in a typed-dict literal.
            // encode as `Starred(Starred(T))` so the typed-dict literal lowering
            // can distinguish it from regular `**other_dict` unpacking
            if self.at(TokenKind::Colon) {
                self.bump(TokenKind::Colon);
                let inner = self.parse_conditional_expression_or_higher().expr;
                let inner_range = inner.range();
                let outer_range = self.node_range(after_brace);
                let inner_starred = Expr::Starred(ast::ExprStarred {
                    value: Box::new(inner),
                    ctx: ExprContext::Load,
                    range: inner_range,
                    node_index: AtomicNodeIndex::NONE,
                });
                let outer_starred = Expr::Starred(ast::ExprStarred {
                    value: Box::new(inner_starred),
                    ctx: ExprContext::Load,
                    range: outer_range,
                    node_index: AtomicNodeIndex::NONE,
                });
                return Expr::Dict(self.parse_dictionary_expression(None, outer_starred, start));
            }
            // Handle dictionary unpacking. Here, the grammar is `'**' bitwise_or`
            // which requires limiting the expression.
            let value = self.parse_expression_with_bitwise_or_precedence();
            let unpack_range = TextRange::new(after_brace, value.range().end());

            if matches!(self.current_token_kind(), TokenKind::Async | TokenKind::For) {
                self.add_unsupported_syntax_error(
                    UnsupportedSyntaxErrorKind::UnpackingInComprehension(
                        ComprehensionUnpackingKind::DictInDict,
                    ),
                    unpack_range,
                );

                return Expr::DictComp(
                    self.parse_dictionary_comprehension_expression(None, value.expr, start),
                );
            }

            if self.at(TokenKind::Colon) {
                self.add_error(ParseErrorType::InvalidStarredExpressionUsage, unpack_range);

                self.bump(TokenKind::Colon);
                let dict_value = self.parse_conditional_expression_or_higher();

                if matches!(self.current_token_kind(), TokenKind::Async | TokenKind::For) {
                    return Expr::DictComp(self.parse_dictionary_comprehension_expression(
                        Some(value.expr),
                        dict_value.expr,
                        start,
                    ));
                }

                return Expr::Dict(self.parse_dictionary_expression(
                    Some(value.expr),
                    dict_value.expr,
                    start,
                ));
            }

            return Expr::Dict(self.parse_dictionary_expression(None, value.expr, start));
        }

        // For dictionary expressions, the key uses the `expression` rule while for
        // set expressions, the element uses the `star_expression` rule. So, use the
        // one that is more general and limit it later.
        let key_or_element = self.parse_named_expression_or_higher(
            ExpressionContext::starred_bitwise_or().with_for_excluded(),
        );

        match self.current_token_kind() {
            TokenKind::Async | TokenKind::For => {
                if key_or_element.is_unparenthesized_starred_expr() {
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::UnpackingInComprehension(
                            ComprehensionUnpackingKind::IterableInSet,
                        ),
                        key_or_element.range(),
                    );
                } else if key_or_element.is_unparenthesized_named_expr() {
                    // test_ok parenthesized_named_expr_py38
                    // # parse_options: {"target-version": "3.8"}
                    // {(x := 1), 2, 3}
                    // {(last := x) for x in range(3)}

                    // test_ok unparenthesized_named_expr_py39
                    // # parse_options: {"target-version": "3.9"}
                    // {x := 1, 2, 3}
                    // {last := x for x in range(3)}

                    // test_err unparenthesized_named_expr_set_comp_py38
                    // # parse_options: {"target-version": "3.8"}
                    // {last := x for x in range(3)}
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::UnparenthesizedNamedExpr(
                            UnparenthesizedNamedExprKind::SetComprehension,
                        ),
                        key_or_element.range(),
                    );
                }

                Expr::SetComp(self.parse_set_comprehension_expression(key_or_element.expr, start))
            }
            TokenKind::Colon => {
                // Now, we know that it's either a dictionary expression or a dictionary comprehension.
                // In either case, the key is limited to an `expression`.
                if !key_or_element.is_parenthesized {
                    match key_or_element.expr {
                        Expr::Starred(_) => self.add_error(
                            ParseErrorType::InvalidStarredExpressionUsage,
                            &key_or_element.expr,
                        ),
                        Expr::Named(_) => self.add_error(
                            ParseErrorType::UnparenthesizedNamedExpression,
                            &key_or_element,
                        ),
                        _ => {}
                    }
                }

                self.bump(TokenKind::Colon);
                let value = if self.at(TokenKind::DoubleStar) {
                    let unpack_start = self.node_start();
                    self.bump(TokenKind::DoubleStar);
                    let value = self.parse_expression_with_bitwise_or_precedence();
                    self.add_error(
                        ParseErrorType::InvalidStarredExpressionUsage,
                        TextRange::new(unpack_start, value.range().end()),
                    );
                    value
                } else {
                    self.parse_conditional_expression_or_higher()
                };

                if matches!(self.current_token_kind(), TokenKind::Async | TokenKind::For) {
                    Expr::DictComp(self.parse_dictionary_comprehension_expression(
                        Some(key_or_element.expr),
                        value.expr,
                        start,
                    ))
                } else {
                    Expr::Dict(self.parse_dictionary_expression(
                        Some(key_or_element.expr),
                        value.expr,
                        start,
                    ))
                }
            }
            _ => Expr::Set(self.parse_set_expression(key_or_element, start)),
        }
    }

    /// basedpython: consume a `local` / `once` modifier prefixing a callable-type
    /// parameter, e.g. the `local` in `(local int) -> None`, and report which one
    /// was written. The parsed element starts at the type, so the keyword leaves
    /// no trace in the element itself — the enclosing parameter list records the
    /// returned [`ParameterBorrow`] positionally instead, and the callable
    /// transform still sees a plain `Callable[[int], None]`.
    ///
    /// Only consumed when a bare name (a type) directly follows, so a
    /// parenthesized name (`(local)`), a call (`once(x)`), or a subscript
    /// (`local[i]`) is never mistaken for a modifier.
    ///
    /// A run may spell both (`once local fn`); `once` is the stronger of the two
    /// (it implies the borrow) and wins.
    fn skip_callable_type_modifiers(&mut self) -> ParameterBorrow {
        let mut borrow = ParameterBorrow::None;
        while self.at(TokenKind::Name)
            && matches!(self.src_text(self.current_token_range()), "local" | "once")
            && self.peek() == TokenKind::Name
            // `(local x=1)` is an anonymous named tuple *value*, which has no
            // parameters to borrow — leave the name alone so it keeps its
            // existing reading
            && self.peek2().1 != TokenKind::Equal
        {
            self.error_if_not_basedpython(
                "`local` / `once` in a callable type are not valid in .py files".to_string(),
            );
            if self.src_text(self.current_token_range()) == "once" {
                borrow = ParameterBorrow::Once;
            } else if borrow.is_none() {
                borrow = ParameterBorrow::Local;
            }
            self.bump(TokenKind::Name);
        }
        borrow
    }

    /// Returns whether the current `protocol` identifier introduces an inline protocol type
    /// rather than a call to something named `protocol`.
    ///
    /// `protocol` is a soft keyword, so `protocol(x)` must keep parsing as a call. Only a
    /// parenthesized list whose first member is unambiguously a protocol member — `def` or
    /// `name :` (which no call argument can start with, since `name :=` lexes as one token) —
    /// takes the protocol path in any file. `protocol()` is a call: an empty protocol declares
    /// nothing.
    ///
    /// A leading `**Pack` is the exception: `protocol(**kwargs)` is also an ordinary call, so it
    /// only reads as a member list in a `.by` file, where shadowing the `protocol` keyword is
    /// already discouraged.
    fn at_inline_protocol_type(&mut self) -> bool {
        if self.src_text(self.current_token_range()) != "protocol" || self.peek() != TokenKind::Lpar
        {
            return false;
        }
        match self.peek_nth(1).0 {
            TokenKind::Def => true,
            TokenKind::DoubleStar => self.options.is_basedpython,
            TokenKind::Name => self.peek_nth(2).0 == TokenKind::Colon,
            _ => false,
        }
    }

    /// Reads a parsed parenthesized expression as the parameter list of a callable arrow.
    ///
    /// `(int)` parses to a parenthesized expression and is a one-parameter list; anything else
    /// parenthesized is a tuple, whose elements carry the parameter shape (and whose
    /// `is_anon_named_tuple` flag is irrelevant here — `name: T` has the same encoding in both).
    /// The `local` / `once` modifiers ride along the same two shapes: on the single element
    /// itself, or on the tuple.
    fn callable_parameter_spec(parsed: ParsedExpr) -> (Vec<Expr>, CallableParameterShape) {
        if parsed.is_parenthesized {
            let mut borrows = Vec::new();
            record_parameter_borrow(&mut borrows, 0, parsed.parameter_borrow);
            return (
                vec![parsed.expr],
                CallableParameterShape {
                    borrows: borrows.into_boxed_slice(),
                    ..CallableParameterShape::default()
                },
            );
        }
        match parsed.expr {
            Expr::Tuple(t) => (
                t.elts,
                t.callable_shape.map(|shape| *shape).unwrap_or_default(),
            ),
            _ => (vec![], CallableParameterShape::default()),
        }
    }

    /// Parses a basedpython inline protocol type,
    /// `protocol(a: int; b: str; def f(self) -> int; **Kwargs)`.
    ///
    /// The current token is the `protocol` soft keyword and the next one is `(`; the caller has
    /// already established that the parenthesized list is a member list rather than a call
    /// argument list.
    fn parse_inline_protocol_type(&mut self, start: TextSize) -> ast::ExprProtocolType {
        self.bump(TokenKind::Name); // `protocol`
        self.bump(TokenKind::Lpar);

        let mut members: Vec<Expr> = vec![];
        // member names are labels rather than bindings, so a duplicate is never what was meant
        // and would silently drop one of the two declarations
        let mut seen: Vec<Name> = vec![];
        let mut check_duplicate = |parser: &mut Self, name: &Name, range: TextRange| {
            if seen.contains(name) {
                parser.add_error(
                    ParseErrorType::BasedPythonOnly(format!("duplicate protocol member `{name}`")),
                    range,
                );
            } else {
                seen.push(name.clone());
            }
        };

        loop {
            if self.at(TokenKind::Rpar) {
                break;
            }

            let member_start = self.node_start();
            let member = if self.at(TokenKind::Def) {
                self.bump(TokenKind::Def);
                let name = self.parse_identifier();
                check_duplicate(self, &name.id, name.range);

                let signature_start = self.node_start();
                // a method's parameter list is unambiguously a parameter spec, so it goes
                // straight to the spec parser rather than through the parenthesized-expression
                // dispatch, where `(self, x: int)` would read as an anonymous named tuple
                self.expect(TokenKind::Lpar);
                let mut parameters =
                    self.parse_parameters_spec(signature_start, None, ParameterBorrow::None);
                // the receiver is a parameter *name*, not a type — mark it a label so it neither
                // binds a name nor resolves to one, the way an anonymous-named-tuple field name
                // is. Every later bare name keeps its parameter-spec meaning of a
                // positional-only parameter's type
                if let Some(Expr::Name(receiver)) = parameters.elts.first_mut() {
                    receiver.ctx = ExprContext::Invalid;
                }
                let (args, callable_shape) = (parameters.elts, parameters.callable_shape);

                // unlike the callable arrow `(...) -> T | None`, where `->` binds tighter than
                // `|` so that the union wraps the whole callable, a method's return annotation
                // reads as it does on a `def` statement: the union is the return type
                self.expect(TokenKind::Rarrow);
                let returns = self
                    .parse_conditional_expression_or_higher_impl(
                        ExpressionContext::default().with_in_type_expression(),
                    )
                    .expr;

                Expr::ProtocolMethod(ast::ExprProtocolMethod {
                    name,
                    signature: Box::new(Expr::CallableType(ast::ExprCallableType {
                        // a protocol method's receiver is its `self` parameter, not
                        // an implicit one
                        receiver: None,
                        args,
                        returns: Box::new(returns),
                        range: self.node_range(signature_start),
                        node_index: AtomicNodeIndex::NONE,
                        callable_shape,
                    })),
                    range: self.node_range(member_start),
                    node_index: AtomicNodeIndex::NONE,
                })
            } else if self.at(TokenKind::DoubleStar) {
                // `**Kwargs` — a keyword-variadic pack, encoded as `Starred(Starred(_))` the way
                // `**` unpacks are everywhere else
                self.bump(TokenKind::DoubleStar);
                let inner_start = self.node_start();
                let value = self
                    .parse_conditional_expression_or_higher_impl(
                        ExpressionContext::default().with_in_type_expression(),
                    )
                    .expr;
                Expr::Starred(ast::ExprStarred {
                    value: Box::new(Expr::Starred(ast::ExprStarred {
                        value: Box::new(value),
                        ctx: ExprContext::Load,
                        range: self.node_range(inner_start),
                        node_index: AtomicNodeIndex::NONE,
                    })),
                    ctx: ExprContext::Load,
                    range: self.node_range(member_start),
                    node_index: AtomicNodeIndex::NONE,
                })
            } else {
                if !(self.at(TokenKind::Name) && self.peek() == TokenKind::Colon) {
                    self.add_error(
                        ParseErrorType::BasedPythonOnly(
                            "expected a protocol member: `name: T`, `def name(...) -> T`, \
                             or `**Pack`"
                                .to_string(),
                        ),
                        self.current_token_range(),
                    );
                }
                // a member name is a label: it neither binds a name nor refers to one, so it is
                // marked invalid the way anonymous-named-tuple field names are
                let mut target = self.parse_name(ExpressionContext::default());
                target.ctx = ExprContext::Invalid;
                check_duplicate(self, &target.id, target.range);
                self.expect(TokenKind::Colon);
                let value = self
                    .parse_conditional_expression_or_higher_impl(
                        ExpressionContext::default().with_in_type_expression(),
                    )
                    .expr;
                Expr::Named(ast::ExprNamed {
                    target: Box::new(Expr::Name(target)),
                    value: Box::new(value),
                    range: self.node_range(member_start),
                    node_index: AtomicNodeIndex::NONE,
                })
            };

            members.push(member);

            if self.eat(TokenKind::Semi) {
                continue;
            }
            break;
        }

        self.expect(TokenKind::Rpar);

        ast::ExprProtocolType {
            members,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// basedpython: builds a callable type from its already-parsed parameter list,
    /// with the parser positioned at the `->`. `params` is the parenthesized
    /// element list — a single parenthesized type, a parenthesized tuple, or a
    /// parameters spec — and `receiver` is the implicit receiver of the
    /// `int.() -> str` form.
    fn parse_callable_type_arrow(
        &mut self,
        receiver: Option<Expr>,
        params: ParsedExpr,
        start: TextSize,
        context: ExpressionContext,
    ) -> Expr {
        // the receiver form reaches here from the postfix loop, where the `->` is
        // only expected rather than guaranteed
        self.expect(TokenKind::Rarrow);
        // parse return type stopping before `|` so the union wraps the whole callable
        let returns = self.parse_binary_expression_or_higher(OperatorPrecedence::BitOr, context);
        let (args, shape) = Self::callable_parameter_spec(params);
        Expr::CallableType(ast::ExprCallableType {
            receiver: receiver.map(Box::new),
            args,
            returns: Box::new(returns.expr),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            callable_shape: shape.into_stored(),
        })
    }

    /// Parses an expression in parentheses, a tuple expression, or a generator expression.
    ///
    /// Matches the `(tuple | group | genexp)` rule in the [Python grammar].
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    fn parse_parenthesized_expression(&mut self, outer: ExpressionContext) -> ParsedExpr {
        let start = self.node_start();
        self.bump(TokenKind::Lpar);

        // Nice error message when having a unclosed open parenthesis `(`
        if self.at_ts(NEWLINE_EOF_SET) {
            let range = self.current_token_range();
            self.add_error(
                ParseErrorType::OtherError("Missing closing parenthesis `)`".to_string()),
                range,
            );
        }

        // Return an empty `TupleExpr` when finding a `)` right after the `(`
        if self.eat(TokenKind::Rpar) {
            return Expr::Tuple(ast::ExprTuple {
                elts: vec![],
                ctx: ExprContext::Load,
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
                parenthesized: true,
                is_anon_named_tuple: false,
                is_anon_named_tuple_value: false,
                callable_shape: None,
                is_parameter_shape: false,
            })
            .into();
        }

        // basedpython: a callable-type parameter may carry a `local` / `once`
        // modifier (`(local int) -> None`). It binds to the element that follows,
        // so it is consumed before the dispatches below read that element's own
        // leading tokens, and handed to whichever shape claims it
        let first_borrow = self.skip_callable_type_modifiers();

        // basedpython: `(name: T, ...)` — anonymous named tuple type literal.
        // Detected before parsing the first element: the very first token after
        // `(` is a `Name` followed by `:`. We don't dispatch here for single-
        // field cases (`(name: T)`) without a trailing comma — see below.
        if matches!(self.current_token_kind(), TokenKind::Name) && self.peek() == TokenKind::Colon {
            self.error_if_not_basedpython(
                "anonymous named tuple type `(name: T, ...)` is not valid in .py files".to_string(),
            );
            let tuple = self.parse_anon_named_tuple_type(start, first_borrow);
            return ParsedExpr {
                expr: Expr::Tuple(tuple),
                is_parenthesized: false,
                parameter_borrow: ParameterBorrow::None,
            };
        }

        // basedpython: `(/, ...)` or `(*, ...)` or `(**name, ...)` — Parameters
        // spec literal starting with a marker. Used as a subscript key for a
        // `Parameters`-bound type variable (e.g. `A[(int, str, /, name: str)]`)
        let starts_params_spec = match (self.current_token_kind(), self.peek()) {
            (TokenKind::Slash, TokenKind::Comma | TokenKind::Rpar) => true,
            (TokenKind::Star, TokenKind::Comma | TokenKind::Rpar) => true,
            // `(*: T, ...)` — anonymous variadic at start
            (TokenKind::Star, TokenKind::Colon) => true,
            // `(*name: T, ...)` — named variadic at start
            (TokenKind::Star, TokenKind::Name) if self.peek2().1 == TokenKind::Colon => true,
            (TokenKind::DoubleStar, TokenKind::Name | TokenKind::Colon) => true,
            _ => false,
        };
        if starts_params_spec {
            self.error_if_not_basedpython(
                "Parameters spec syntax is not valid in .py files".to_string(),
            );
            let tuple =
                self.parse_extended_tuple(start, Vec::new(), Vec::new(), None, first_borrow);
            return ParsedExpr {
                expr: Expr::Tuple(tuple),
                is_parenthesized: false,
                parameter_borrow: ParameterBorrow::None,
            };
        }

        // basedpython: `(name=expr, ...)` — anonymous named tuple value
        // construction. Same dispatch trigger as the type form, but uses `=`
        // rather than `:`.
        if matches!(self.current_token_kind(), TokenKind::Name) && self.peek() == TokenKind::Equal {
            self.error_if_not_basedpython(
                "anonymous named tuple value `(name=expr, ...)` is not valid in .py files"
                    .to_string(),
            );
            let tuple = self.parse_anon_named_tuple_value(start);
            return ParsedExpr {
                expr: Expr::Tuple(tuple),
                is_parenthesized: false,
                parameter_borrow: ParameterBorrow::None,
            };
        }

        // Use the more general rule of the three to parse the first element
        // and limit it later.
        let mut parsed_expr = self.parse_named_expression_or_higher(
            ExpressionContext::yield_or_starred_bitwise_or()
                .with_for_excluded()
                .inheriting_type_expression(outer),
        );

        // basedpython: a positional first element followed by a named field
        // (e.g. `(1, name="a")` or `(int, name: str)`) is a *mixed*
        // anonymous named tuple. Detect the transition at the first comma:
        // peek past the comma for `Name : ...` or `Name = ...`. The first
        // named field's separator (`:` or `=`) determines whether the whole
        // tuple is a type form or a value form.
        if self.at(TokenKind::Comma) {
            let (after_comma, after_name) = self.peek2();
            if after_comma == TokenKind::Name
                && matches!(after_name, TokenKind::Colon | TokenKind::Equal)
            {
                self.error_if_not_basedpython(
                    "anonymous named tuple is not valid in .py files".to_string(),
                );
                let is_type_form = after_name == TokenKind::Colon;
                let tuple = self.parse_anon_named_tuple_mixed(
                    start,
                    parsed_expr.expr,
                    first_borrow,
                    is_type_form,
                );
                return ParsedExpr {
                    expr: Expr::Tuple(tuple),
                    is_parenthesized: false,
                    parameter_borrow: ParameterBorrow::None,
                };
            }
            // basedpython: positional first element followed by `, /` or
            // `, *` (a positional-only or keyword-only marker) is a
            // Parameters spec mixed form (e.g. `(int, /, name: str)`).
            // `*name` (starred expr) and `*` followed by a name are NOT
            // markers, so we restrict the lookahead to `, /,` / `, *,` and
            // `, /)` / `, *)`
            // `, /` `, *` `, *:` `, **name :` `, **:` after first. note
            // `, *name (...)` is starred-unpack, not a marker — only
            // `*name :` (with `:` after) qualifies. `*Name` followed by `,`
            // or `)` is just a star unpack of `Name`, not a marker either,
            // so we don't dispatch on `(Star, Name)` here
            let mid_tuple_marker = matches!(
                (after_comma, after_name),
                (
                    TokenKind::Slash | TokenKind::Star,
                    TokenKind::Comma | TokenKind::Rpar
                ) | (TokenKind::Star | TokenKind::DoubleStar, TokenKind::Colon)
                    // `, **P` — a trailing bare `ParamSpec` (Concatenate form)
                    | (TokenKind::DoubleStar, TokenKind::Name)
            );
            if mid_tuple_marker {
                self.error_if_not_basedpython(
                    "Parameters spec syntax is not valid in .py files".to_string(),
                );
                let tuple = self.parse_parameters_spec(start, Some(parsed_expr.expr), first_borrow);
                return ParsedExpr {
                    expr: Expr::Tuple(tuple),
                    is_parenthesized: false,
                    parameter_borrow: ParameterBorrow::None,
                };
            }
        }

        match self.current_token_kind() {
            TokenKind::Comma => {
                // grammar: `tuple`
                // basedpython: speculatively parse as a regular tuple, but if
                // we encounter a `/` or standalone `*` marker mid-tuple,
                // abort and switch to Parameters spec parsing with the
                // elements collected so far. this lets `(int, str, /, name:
                // T)` reach the spec parser without requiring a single-token
                // dispatch
                let tuple = self.parse_tuple_or_parameters_spec(
                    parsed_expr.expr,
                    first_borrow,
                    start,
                    outer,
                );

                ParsedExpr {
                    expr: tuple.into(),
                    is_parenthesized: false,
                    parameter_borrow: ParameterBorrow::None,
                }
            }
            TokenKind::Async | TokenKind::For => {
                // grammar: `genexp`
                if parsed_expr.is_unparenthesized_starred_expr() {
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::UnpackingInComprehension(
                            ComprehensionUnpackingKind::IterableInGenerator,
                        ),
                        parsed_expr.range(),
                    );
                }

                let generator = Expr::Generator(self.parse_generator_expression(
                    parsed_expr.expr,
                    start,
                    Parenthesized::Yes,
                ));

                ParsedExpr {
                    expr: generator,
                    is_parenthesized: false,
                    parameter_borrow: ParameterBorrow::None,
                }
            }
            _ => {
                // grammar: `group`
                // basedpython: `(*Ts) -> R` is a callable whose whole parameter list is an
                // unpacked variadic type. the arrow is only seen further up in the Pratt
                // loop, so peek for it here rather than rejecting the lone starred element
                let is_callable_parameter_list =
                    self.at(TokenKind::Rpar) && self.peek() == TokenKind::Rarrow;
                if parsed_expr.expr.is_starred_expr() && !is_callable_parameter_list {
                    // basedpython: `(*A)` in a type expression is the tuple type
                    // `(*A,)` — the star already says the element splices in, so
                    // the trailing comma python needs to make a tuple is pure
                    // noise. desugar to the tuple that comma would have built, so
                    // nothing downstream has to know the difference
                    if self.options.is_basedpython && outer.is_in_type_expression() {
                        self.expect(TokenKind::Rpar);
                        return ParsedExpr {
                            expr: Expr::Tuple(ast::ExprTuple {
                                elts: vec![parsed_expr.expr],
                                ctx: ExprContext::Load,
                                range: self.node_range(start),
                                node_index: AtomicNodeIndex::NONE,
                                parenthesized: true,
                                is_anon_named_tuple: false,
                                is_anon_named_tuple_value: false,
                                callable_shape: None,
                                is_parameter_shape: false,
                            }),
                            is_parenthesized: true,
                            parameter_borrow: first_borrow,
                        };
                    }
                    self.add_error(ParseErrorType::InvalidStarredExpressionUsage, &parsed_expr);
                }

                self.expect(TokenKind::Rpar);

                parsed_expr.is_parenthesized = true;
                parsed_expr.parameter_borrow = first_borrow;
                parsed_expr
            }
        }
    }

    /// Parses multiple items separated by a comma into a tuple expression.
    ///
    /// Uses the `parse_func` to parse each item in the tuple.
    pub(super) fn parse_tuple_expression(
        &mut self,
        first_element: Expr,
        start: TextSize,
        parenthesized: Parenthesized,
        mut parse_func: impl FnMut(&mut Parser<'src>) -> ParsedExpr,
    ) -> ast::ExprTuple {
        // TODO(dhruvmanila): Can we remove `parse_func` and use `parenthesized` to
        // determine the parsing function?

        if !self.at_sequence_end() {
            self.expect(TokenKind::Comma);
        }

        let elts_snapshot = self.expr_scratch.snapshot();
        self.expr_scratch.push(first_element);

        self.parse_comma_separated_list(RecoveryContextKind::TupleElements(parenthesized), |p| {
            let element = parse_func(p).expr;
            p.expr_scratch.push(element);
        });

        if parenthesized.is_yes() {
            self.expect(TokenKind::Rpar);
        }

        ast::ExprTuple {
            elts: self.expr_scratch.take(elts_snapshot),
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            parenthesized: parenthesized.is_yes(),
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: false,
            callable_shape: None,
            is_parameter_shape: false,
        }
    }

    /// Parses a basedpython anonymous named tuple type expression
    /// `(name1: T1, ...)`, possibly with positional fields interleaved
    /// (e.g. `(int, name: str)`).
    ///
    /// The opening `(` has already been consumed; `start` points at it. The
    /// current token is the first field's `Name`, and `peek()` is `:` (the
    /// caller dispatched on this).
    fn parse_anon_named_tuple_type(
        &mut self,
        start: TextSize,
        first_borrow: ParameterBorrow,
    ) -> ast::ExprTuple {
        self.parse_anon_named_tuple_fields(
            start,
            /* is_type_form = */ true,
            None,
            first_borrow,
        )
    }

    /// Parses a basedpython anonymous named tuple value construction
    /// `(name1=expr1, ...)`, possibly with positional fields interleaved
    /// (e.g. `(1, name="a")`).
    fn parse_anon_named_tuple_value(&mut self, start: TextSize) -> ast::ExprTuple {
        self.parse_anon_named_tuple_fields(
            start,
            /* is_type_form = */ false,
            None,
            ParameterBorrow::None,
        )
    }

    /// Continues parsing an anonymous named tuple after the first element has
    /// already been consumed as a plain expression. Used when the caller saw
    /// a positional first field followed by a comma + `Name :` or `Name =`.
    fn parse_anon_named_tuple_mixed(
        &mut self,
        start: TextSize,
        first_positional: Expr,
        first_borrow: ParameterBorrow,
        is_type_form: bool,
    ) -> ast::ExprTuple {
        self.parse_anon_named_tuple_fields(
            start,
            is_type_form,
            Some((first_positional, first_borrow)),
            ParameterBorrow::None,
        )
    }

    /// Shared field-list parser for anonymous named tuples. Each field is
    /// either:
    ///   - a positional expression: a plain `expression` for the value form,
    ///     or a plain `type-expression` for the type form. Stored as a bare
    ///     `Expr` element.
    ///   - a named field: `name : type-expression` (type form) or
    ///     `name = expression` (value form). Stored as an `Expr::Named` with
    ///     `target` = the name and `value` = the type/value expression.
    ///
    /// All named fields in a single tuple share the same separator (`:` or
    /// `=`); using the wrong separator is a parse error.
    fn parse_anon_named_tuple_fields(
        &mut self,
        start: TextSize,
        is_type_form: bool,
        prefix_positional: Option<(Expr, ParameterBorrow)>,
        mut pending_borrow: ParameterBorrow,
    ) -> ast::ExprTuple {
        let separator = if is_type_form {
            TokenKind::Colon
        } else {
            TokenKind::Equal
        };
        let mut elts: Vec<Expr> = Vec::new();
        // basedpython: the type form doubles as a callable's parameter list, so
        // its fields may carry `local` / `once`. The value form never does — the
        // modifier scan does not fire on a `name = value` field
        let mut borrows: Vec<ParameterBorrow> = Vec::new();

        if let Some((first, first_borrow)) = prefix_positional {
            record_parameter_borrow(&mut borrows, elts.len(), first_borrow);
            elts.push(first);
            // The caller has already verified there's a comma here.
            self.expect(TokenKind::Comma);
        }

        let other_separator = if is_type_form {
            TokenKind::Equal
        } else {
            TokenKind::Colon
        };

        // basedpython: a type-form field's value *is* the field's type, so it
        // admits the use-site type modifiers. the value form's is a value
        let field_context = if is_type_form {
            ExpressionContext::default().with_in_type_expression()
        } else {
            ExpressionContext::default()
        };

        loop {
            // Trailing comma already absorbed up the loop with `eat`; reaching
            // `)` is a clean exit.
            if self.at(TokenKind::Rpar) {
                break;
            }

            let borrow = match std::mem::take(&mut pending_borrow) {
                ParameterBorrow::None => self.skip_callable_type_modifiers(),
                pending => pending,
            };
            let elts_before = elts.len();
            if self.at(TokenKind::Rpar) {
                break;
            }

            let field_start = self.node_start();
            // Detect a field that uses the *wrong* separator
            // (`name = v` inside a `:`-form tuple, or `name : T` inside a
            // `=`-form tuple). consume both tokens + the trailing expression
            // so the rest of the tuple parses cleanly, and emit a basedpython
            // parse error pointing the user at the consistency rule
            if self.at(TokenKind::Name) && self.peek() == other_separator {
                let expected = if is_type_form { ":" } else { "=" };
                self.add_error(
                    ParseErrorType::BasedPythonOnly(format!(
                        "anonymous named tuple mixes `:` and `=` field separators — \
                         use `{expected}` consistently across all named fields"
                    )),
                    self.current_token_range(),
                );
                let mut target_name = self.parse_name(ExpressionContext::default());
                target_name.ctx = ExprContext::Invalid;
                self.bump(other_separator);
                let inner = self.parse_conditional_expression_or_higher_impl(field_context);
                let field_range = self.node_range(field_start);
                elts.push(Expr::Named(ast::ExprNamed {
                    target: Box::new(Expr::Name(target_name)),
                    value: Box::new(inner.expr),
                    range: field_range,
                    node_index: AtomicNodeIndex::NONE,
                }));
                if self.eat(TokenKind::Comma) {
                    continue;
                }
                break;
            }
            // Decide field shape: `Name SEPARATOR ...` is named, anything else
            // is positional.
            let is_named_field = self.at(TokenKind::Name) && self.peek() == separator;

            if is_named_field {
                let mut target_name = self.parse_name(ExpressionContext::default());
                // Anonymous-named-tuple field names are *labels*, not references
                // and not bindings — they don't introduce a visible name in any
                // scope, and they don't refer to one either. Mark the inner
                // `ExprName` with `ExprContext::Invalid` so downstream
                // name-resolution passes (pyflakes F821, etc.) don't treat
                // them as undefined references.
                target_name.ctx = ExprContext::Invalid;
                self.expect(separator);
                let inner = self.parse_conditional_expression_or_higher_impl(field_context);
                let field_range = self.node_range(field_start);
                elts.push(Expr::Named(ast::ExprNamed {
                    target: Box::new(Expr::Name(target_name)),
                    value: Box::new(inner.expr),
                    range: field_range,
                    node_index: AtomicNodeIndex::NONE,
                }));
            } else {
                let inner = self.parse_conditional_expression_or_higher_impl(field_context);
                elts.push(inner.expr);
            }

            if elts.len() > elts_before {
                record_parameter_borrow(&mut borrows, elts.len() - 1, borrow);
            }

            if self.eat(TokenKind::Comma) {
                continue;
            }
            break;
        }

        self.expect(TokenKind::Rpar);

        ast::ExprTuple {
            elts,
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            parenthesized: true,
            is_anon_named_tuple: is_type_form,
            is_anon_named_tuple_value: !is_type_form,
            callable_shape: CallableParameterShape {
                borrows: borrows.into_boxed_slice(),
                ..CallableParameterShape::default()
            }
            .into_stored(),
            is_parameter_shape: false,
        }
    }

    /// Parses a parenthesized tuple, switching to Parameters spec parsing if
    /// a `/` or standalone `*` marker is encountered mid-list. The first
    /// element has already been parsed; the current token is `,`.
    ///
    /// `first_borrow` is the `local` / `once` modifier that element was written
    /// with, which the caller consumed before parsing it
    fn parse_tuple_or_parameters_spec(
        &mut self,
        first_element: Expr,
        first_borrow: ParameterBorrow,
        start: TextSize,
        outer: ExpressionContext,
    ) -> ast::ExprTuple {
        let mut elts = vec![first_element];
        let mut borrows = Vec::new();
        record_parameter_borrow(&mut borrows, 0, first_borrow);
        if !self.at_sequence_end() {
            self.expect(TokenKind::Comma);
        }

        loop {
            if self.at(TokenKind::Rpar) {
                break;
            }

            // basedpython: strip a `local` / `once` modifier on this callable
            // parameter before dispatching on markers / named fields
            let borrow = self.skip_callable_type_modifiers();
            if self.at(TokenKind::Rpar) {
                break;
            }

            // basedpython markers — switch to extended-tuple parsing.
            // `*Name (...)` (call) and `*Name` followed by `,` or `)` are
            // starred-unpack expressions, NOT spec markers. only `*:`,
            // `*,`, `*)`, `*Name :`, `**:`, `**Name :` qualify
            let is_marker = matches!(self.current_token_kind(), TokenKind::Slash)
                || (self.at(TokenKind::Star)
                    && matches!(
                        self.peek(),
                        TokenKind::Comma | TokenKind::Rpar | TokenKind::Colon
                    ))
                || (self.at(TokenKind::Star)
                    && self.peek() == TokenKind::Name
                    && self.peek2().1 == TokenKind::Colon)
                || (self.at(TokenKind::DoubleStar) && matches!(self.peek(), TokenKind::Colon))
                // `**name: T` (kwargs) or a bare `**P` (ParamSpec, Concatenate form)
                || (self.at(TokenKind::DoubleStar) && self.peek() == TokenKind::Name);
            if is_marker {
                self.error_if_not_basedpython(
                    "Parameters spec syntax is not valid in .py files".to_string(),
                );
                return self.continue_parameters_spec(start, elts, borrows, borrow);
            }

            // basedpython: a `Name : type` field also switches to spec form
            // (e.g. `(int, name: str)`). when this is the first such field,
            // the existing anon-named-tuple mixed dispatch upstream already
            // catches it; here we handle the case where it appears only
            // after several positional elements
            if self.at(TokenKind::Name) && self.peek() == TokenKind::Colon {
                self.error_if_not_basedpython(
                    "Parameters spec syntax is not valid in .py files".to_string(),
                );
                return self.continue_parameters_spec(start, elts, borrows, borrow);
            }

            // basedpython: anonymous-named-tuple value form `Name = expr`
            // appearing after several positional elements (e.g. `(1, 2, a=3)`)
            // — the upstream dispatch only catches it after the first
            // element, so detect it again mid-tuple and switch to the value
            // parser
            if self.at(TokenKind::Name) && self.peek() == TokenKind::Equal {
                self.error_if_not_basedpython(
                    "anonymous named tuple is not valid in .py files".to_string(),
                );
                return self.continue_anon_named_tuple_value(start, elts);
            }

            let parsed = self.parse_named_expression_or_higher(
                ExpressionContext::starred_bitwise_or().inheriting_type_expression(outer),
            );
            record_parameter_borrow(&mut borrows, elts.len(), borrow);
            elts.push(parsed.expr);

            if self.eat(TokenKind::Comma) {
                continue;
            }
            break;
        }

        self.expect(TokenKind::Rpar);

        ast::ExprTuple {
            elts,
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            parenthesized: true,
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: false,
            callable_shape: CallableParameterShape {
                borrows: borrows.into_boxed_slice(),
                ..CallableParameterShape::default()
            }
            .into_stored(),
            is_parameter_shape: false,
        }
    }

    /// Continues an extended-tuple parse from the given prefix of already-
    /// parsed elements. Called when `parse_tuple_or_parameters_spec` hits a
    /// marker (`/`, `*`, `**`, or `name:`). Routes to `parse_extended_tuple`
    /// Continues an anonymous-named-tuple value-form parse from the given
    /// prefix of already-parsed positional elements. Called when
    /// `parse_tuple_or_parameters_spec` encounters `Name = expr` mid-tuple
    /// after several plain positional elements (e.g. `(1, 2, a=3)`)
    fn continue_anon_named_tuple_value(
        &mut self,
        start: TextSize,
        mut elts: Vec<Expr>,
    ) -> ast::ExprTuple {
        loop {
            if self.at(TokenKind::Rpar) {
                break;
            }
            let field_start = self.node_start();
            let is_named_field = self.at(TokenKind::Name) && self.peek() == TokenKind::Equal;
            if is_named_field {
                let mut target_name = self.parse_name(ExpressionContext::default());
                target_name.ctx = ExprContext::Invalid;
                self.expect(TokenKind::Equal);
                let inner =
                    self.parse_conditional_expression_or_higher_impl(ExpressionContext::default());
                let field_range = self.node_range(field_start);
                elts.push(Expr::Named(ast::ExprNamed {
                    target: Box::new(Expr::Name(target_name)),
                    value: Box::new(inner.expr),
                    range: field_range,
                    node_index: AtomicNodeIndex::NONE,
                }));
            } else {
                let inner =
                    self.parse_conditional_expression_or_higher_impl(ExpressionContext::default());
                elts.push(inner.expr);
            }
            if self.eat(TokenKind::Comma) {
                continue;
            }
            break;
        }
        self.expect(TokenKind::Rpar);
        ast::ExprTuple {
            elts,
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            parenthesized: true,
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: true,
            callable_shape: None,
            is_parameter_shape: false,
        }
    }

    fn continue_parameters_spec(
        &mut self,
        start: TextSize,
        elts: Vec<Expr>,
        borrows: Vec<ParameterBorrow>,
        pending_borrow: ParameterBorrow,
    ) -> ast::ExprTuple {
        self.parse_extended_tuple(start, elts, borrows, None, pending_borrow)
    }

    /// Parses a basedpython extended-tuple / parameter spec like
    /// `(int, str, /, name: str)`, `(/, **kw)`, `(int, *: str)`. Markers and
    /// variadic fields are encoded in `elts` and the `parameter_slash` /
    /// `parameter_star` fields:
    ///
    /// - `int`        → bare `Expr` (positional)
    /// - `name: T`    → `Expr::Named { target = Name(name), value = T }`
    /// - `*: T`       → `Expr::Starred { value = T }`
    /// - `*name: T`   → `Expr::Named { target = Starred(Name(name)), value = T }`
    /// - `**: T`      → `Expr::Starred { value = Starred(T) }` (double-starred)
    /// - `**name: T`  → `Expr::Named { target = Starred(Starred(Name(name))), value = T }`
    /// - `/`          → tracked in `parameter_slash` (index in elts)
    /// - bare `*`     → tracked in `parameter_star` (index in elts)
    ///
    /// The opening `(` has already been consumed; `start` points at it.
    /// `prefix_borrow` is the `local` / `once` modifier `prefix_positional` was
    /// written with, when the caller consumed one before parsing it
    fn parse_parameters_spec(
        &mut self,
        start: TextSize,
        prefix_positional: Option<Expr>,
        prefix_borrow: ParameterBorrow,
    ) -> ast::ExprTuple {
        self.parse_extended_tuple(
            start,
            Vec::new(),
            Vec::new(),
            prefix_positional.map(|first| (first, prefix_borrow)),
            ParameterBorrow::None,
        )
    }

    /// `borrows` is the modifier shape recorded for `elts` so far;
    /// `pending_borrow` is one the caller already consumed for the *next*
    /// element, which this parse is resuming in the middle of
    fn parse_extended_tuple(
        &mut self,
        start: TextSize,
        mut elts: Vec<Expr>,
        mut borrows: Vec<ParameterBorrow>,
        prefix_positional: Option<(Expr, ParameterBorrow)>,
        mut pending_borrow: ParameterBorrow,
    ) -> ast::ExprTuple {
        let mut slash: Option<u32> = None;
        let mut star: Option<u32> = None;
        if let Some((first, first_borrow)) = prefix_positional {
            record_parameter_borrow(&mut borrows, elts.len(), first_borrow);
            elts.push(first);
            self.expect(TokenKind::Comma);
        }

        loop {
            if self.at(TokenKind::Rpar) {
                break;
            }

            // basedpython: strip a `local` / `once` modifier prefixing this
            // callable parameter's type. a modifier the caller already consumed
            // belongs to the first element parsed here
            let borrow = match std::mem::take(&mut pending_borrow) {
                ParameterBorrow::None => self.skip_callable_type_modifiers(),
                pending => pending,
            };
            let elts_before = elts.len();
            if self.at(TokenKind::Rpar) {
                break;
            }

            // `/` separator — positional-only marker. record its position
            // (= current count of elts) and continue
            if self.at(TokenKind::Slash) {
                self.bump(TokenKind::Slash);
                if slash.is_none() {
                    slash = Some(u32::try_from(elts.len()).unwrap_or(u32::MAX));
                }
            }
            // bare `*` separator — keyword-only marker (not `*: T` variadic)
            else if self.at(TokenKind::Star)
                && matches!(self.peek(), TokenKind::Comma | TokenKind::Rpar)
            {
                self.bump(TokenKind::Star);
                if star.is_none() {
                    star = Some(u32::try_from(elts.len()).unwrap_or(u32::MAX));
                }
            }
            // `*: T` — anonymous variadic with explicit type
            else if self.at(TokenKind::Star) && self.peek() == TokenKind::Colon {
                let starred_start = self.node_start();
                self.bump(TokenKind::Star);
                let star_range = self.node_range(starred_start);
                self.expect(TokenKind::Colon);
                let inner = self.parse_parameter_field_annotation();
                let range = self.node_range(starred_start);
                // `*: *Ts` — an anonymous variadic whose annotation is itself an
                // unpack, exactly as `*args: *Ts` is in a `def`. wrapping the
                // annotation in one more `Starred` would spell the same shape the
                // kwargs catch-all `**: T` uses, so this takes the `*name: T`
                // shape instead, with the empty name that marks an anonymous
                // field everywhere else in a parameter shape. the bare `*` type is
                // a marker rather than an unpack, so `*: *` keeps its own encoding
                if matches!(&inner.expr, Expr::Starred(_))
                    && !ruff_python_ast::helpers::is_top_star_marker(&inner.expr)
                {
                    let anonymous = Expr::Name(ast::ExprName {
                        range: TextRange::empty(star_range.end()),
                        id: Name::empty(),
                        ctx: ExprContext::Invalid,
                        node_index: AtomicNodeIndex::NONE,
                    });
                    elts.push(Expr::Named(ast::ExprNamed {
                        target: Box::new(Expr::Starred(ast::ExprStarred {
                            value: Box::new(anonymous),
                            ctx: ExprContext::Load,
                            range: star_range,
                            node_index: AtomicNodeIndex::NONE,
                        })),
                        value: Box::new(inner.expr),
                        range,
                        node_index: AtomicNodeIndex::NONE,
                    }));
                } else {
                    elts.push(Expr::Starred(ast::ExprStarred {
                        value: Box::new(inner.expr),
                        ctx: ExprContext::Load,
                        range,
                        node_index: AtomicNodeIndex::NONE,
                    }));
                }
            }
            // `*name: T` — named variadic
            else if self.at(TokenKind::Star)
                && self.peek() == TokenKind::Name
                && self.peek2().1 == TokenKind::Colon
            {
                let field_start = self.node_start();
                self.bump(TokenKind::Star);
                let mut target_name = self.parse_name(ExpressionContext::default());
                target_name.ctx = ExprContext::Invalid;
                self.expect(TokenKind::Colon);
                // a variadic's annotation may itself be starred (`*args: *Ts`), just as in
                // a `def`
                let inner = self.parse_conditional_expression_or_higher_impl(
                    ExpressionContext::starred_bitwise_or(),
                );
                let starred_range = TextRange::new(field_start, target_name.range.end());
                elts.push(Expr::Named(ast::ExprNamed {
                    target: Box::new(Expr::Starred(ast::ExprStarred {
                        value: Box::new(Expr::Name(target_name)),
                        ctx: ExprContext::Load,
                        range: starred_range,
                        node_index: AtomicNodeIndex::NONE,
                    })),
                    value: Box::new(inner.expr),
                    range: self.node_range(field_start),
                    node_index: AtomicNodeIndex::NONE,
                }));
            }
            // `**: T` — anonymous kwargs catch-all with explicit type
            else if self.at(TokenKind::DoubleStar) && self.peek() == TokenKind::Colon {
                let doublestar_start = self.node_start();
                self.bump(TokenKind::DoubleStar);
                self.expect(TokenKind::Colon);
                let inner = self.parse_parameter_field_annotation();
                let range = self.node_range(doublestar_start);
                let inner_range = inner.expr.range();
                // encode `**: T` as Starred(Starred(T)) — the double-starred
                // marker. lowering / formatting check both layers
                let inner_starred = Expr::Starred(ast::ExprStarred {
                    value: Box::new(inner.expr),
                    ctx: ExprContext::Load,
                    range: inner_range,
                    node_index: AtomicNodeIndex::NONE,
                });
                elts.push(Expr::Starred(ast::ExprStarred {
                    value: Box::new(inner_starred),
                    ctx: ExprContext::Load,
                    range,
                    node_index: AtomicNodeIndex::NONE,
                }));
            }
            // `**name`: a bare `**P` is a `ParamSpec`; `**name: T` is a kwargs
            // catch-all. both encode the double star as `Starred(Starred(...))`
            else if self.at(TokenKind::DoubleStar) {
                let doublestar_start = self.node_start();
                self.bump(TokenKind::DoubleStar);
                if self.at(TokenKind::Name) {
                    let target_name = self.parse_name(ExpressionContext::default());
                    let target_range = target_name.range;
                    // `**name: T` — kwargs catch-all: `Named(Starred(Starred(name)), T)`.
                    // the name is a bare label, so mark it `Invalid` (not a reference)
                    if self.eat(TokenKind::Colon) {
                        let mut name = target_name;
                        name.ctx = ExprContext::Invalid;
                        let inner = self.parse_conditional_expression_or_higher_impl(
                            ExpressionContext::default(),
                        );
                        let inner_starred = Expr::Starred(ast::ExprStarred {
                            value: Box::new(Expr::Name(name)),
                            ctx: ExprContext::Load,
                            range: target_range,
                            node_index: AtomicNodeIndex::NONE,
                        });
                        let outer_starred = Expr::Starred(ast::ExprStarred {
                            value: Box::new(inner_starred),
                            ctx: ExprContext::Load,
                            range: target_range,
                            node_index: AtomicNodeIndex::NONE,
                        });
                        elts.push(Expr::Named(ast::ExprNamed {
                            target: Box::new(outer_starred),
                            value: Box::new(inner.expr),
                            range: target_range,
                            node_index: AtomicNodeIndex::NONE,
                        }));
                    } else {
                        // bare `**P` — a `ParamSpec`: `Starred(Starred(name))`. the
                        // name keeps `Load` ctx so ty resolves it as a reference
                        let inner_starred = Expr::Starred(ast::ExprStarred {
                            value: Box::new(Expr::Name(target_name)),
                            ctx: ExprContext::Load,
                            range: target_range,
                            node_index: AtomicNodeIndex::NONE,
                        });
                        elts.push(Expr::Starred(ast::ExprStarred {
                            value: Box::new(inner_starred),
                            ctx: ExprContext::Load,
                            range: self.node_range(doublestar_start),
                            node_index: AtomicNodeIndex::NONE,
                        }));
                    }
                }
            } else {
                let field_start = self.node_start();
                let is_named_field = self.at(TokenKind::Name) && self.peek() == TokenKind::Colon;
                if is_named_field {
                    let mut target_name = self.parse_name(ExpressionContext::default());
                    target_name.ctx = ExprContext::Invalid;
                    self.expect(TokenKind::Colon);
                    let inner = self
                        .parse_conditional_expression_or_higher_impl(ExpressionContext::default());
                    let field_range = self.node_range(field_start);
                    elts.push(Expr::Named(ast::ExprNamed {
                        target: Box::new(Expr::Name(target_name)),
                        value: Box::new(inner.expr),
                        range: field_range,
                        node_index: AtomicNodeIndex::NONE,
                    }));
                } else {
                    let inner = self
                        .parse_conditional_expression_or_higher_impl(ExpressionContext::default());
                    elts.push(inner.expr);
                }
            }

            // every branch above pushes at most one element, and a bare `/` or
            // `*` separator pushes none — so the modifier belongs to the element
            // this iteration added, if it added one
            if elts.len() > elts_before {
                record_parameter_borrow(&mut borrows, elts.len() - 1, borrow);
            }

            if self.eat(TokenKind::Comma) {
                continue;
            }
            break;
        }

        self.expect(TokenKind::Rpar);

        // do not lift this tuple to is_anon_named_tuple even when it has
        // plain `name: T` fields — markers (`/`, `*`) signal the user
        // wants parameter-spec semantics (positional-only / keyword-only),
        // which conflicts with the implicit NamedTuple synthesis. anon-NT
        // lifting is reserved for tuples without markers (the existing
        // `(name: T, ...)` and `(int, name: T)` mixed dispatch upstream)
        let is_anon_named_tuple = false;

        ast::ExprTuple {
            elts,
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            parenthesized: true,
            is_anon_named_tuple,
            is_anon_named_tuple_value: false,
            is_parameter_shape: true,
            callable_shape: CallableParameterShape {
                slash,
                star,
                borrows: borrows.into_boxed_slice(),
            }
            .into_stored(),
        }
    }

    /// Parses a list expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#list-displays>
    fn parse_list_expression(
        &mut self,
        first_element: Expr,
        start: TextSize,
        outer: ExpressionContext,
    ) -> ast::ExprList {
        if !self.at_sequence_end() {
            self.expect(TokenKind::Comma);
        }

        let elts_snapshot = self.expr_scratch.snapshot();
        self.expr_scratch.push(first_element);

        let element_context =
            ExpressionContext::starred_bitwise_or().inheriting_type_expression(outer);
        self.parse_comma_separated_list(RecoveryContextKind::ListElements, |parser| {
            let element = parser
                .parse_named_expression_or_higher(element_context)
                .expr;
            parser.expr_scratch.push(element);
        });

        self.expect(TokenKind::Rsqb);

        ast::ExprList {
            elts: self.expr_scratch.take(elts_snapshot),
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a set expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#set-displays>
    fn parse_set_expression(&mut self, first_element: ParsedExpr, start: TextSize) -> ast::ExprSet {
        if !self.at_sequence_end() {
            self.expect(TokenKind::Comma);
        }

        // test_err unparenthesized_named_expr_set_literal_py38
        // # parse_options: {"target-version": "3.8"}
        // {x := 1, 2, 3}
        // {1, x := 2, 3}
        // {1, 2, x := 3}

        if first_element.is_unparenthesized_named_expr() {
            self.add_unsupported_syntax_error(
                UnsupportedSyntaxErrorKind::UnparenthesizedNamedExpr(
                    UnparenthesizedNamedExprKind::SetLiteral,
                ),
                first_element.range(),
            );
        }

        let elts_snapshot = self.expr_scratch.snapshot();
        self.expr_scratch.push(first_element.expr);

        self.parse_comma_separated_list(RecoveryContextKind::SetElements, |parser| {
            let parsed_expr =
                parser.parse_named_expression_or_higher(ExpressionContext::starred_bitwise_or());

            if parsed_expr.is_unparenthesized_named_expr() {
                parser.add_unsupported_syntax_error(
                    UnsupportedSyntaxErrorKind::UnparenthesizedNamedExpr(
                        UnparenthesizedNamedExprKind::SetLiteral,
                    ),
                    parsed_expr.range(),
                );
            }

            parser.expr_scratch.push(parsed_expr.expr);
        });

        self.expect(TokenKind::Rbrace);

        ast::ExprSet {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            elts: self.expr_scratch.take(elts_snapshot),
        }
    }

    /// Parses a dictionary expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#dictionary-displays>
    fn parse_dictionary_expression(
        &mut self,
        key: Option<Expr>,
        value: Expr,
        start: TextSize,
    ) -> ast::ExprDict {
        if !self.at_sequence_end() {
            self.expect(TokenKind::Comma);
        }

        let mut items = vec![ast::DictItem { key, value }];

        self.parse_comma_separated_list(RecoveryContextKind::DictElements, |parser| {
            if parser.at(TokenKind::DoubleStar) {
                let doublestar_start = parser.node_start();
                parser.bump(TokenKind::DoubleStar);
                // basedpython `**: T` extra-items marker in a typed-dict literal.
                // encode as `Starred(Starred(T))` to disambiguate from regular
                // dictionary unpacking
                if parser.at(TokenKind::Colon) {
                    parser.bump(TokenKind::Colon);
                    let inner = parser.parse_conditional_expression_or_higher().expr;
                    let inner_range = inner.range();
                    let outer_range = parser.node_range(doublestar_start);
                    let inner_starred = Expr::Starred(ast::ExprStarred {
                        value: Box::new(inner),
                        ctx: ExprContext::Load,
                        range: inner_range,
                        node_index: AtomicNodeIndex::NONE,
                    });
                    let outer_starred = Expr::Starred(ast::ExprStarred {
                        value: Box::new(inner_starred),
                        ctx: ExprContext::Load,
                        range: outer_range,
                        node_index: AtomicNodeIndex::NONE,
                    });
                    items.push(ast::DictItem {
                        key: None,
                        value: outer_starred,
                    });
                } else {
                    // Handle dictionary unpacking. Here, the grammar is `'**' bitwise_or`
                    // which requires limiting the expression.
                    items.push(ast::DictItem {
                        key: None,
                        value: parser.parse_expression_with_bitwise_or_precedence().expr,
                    });
                }
            } else {
                let key = parser.parse_conditional_expression_or_higher().expr;
                parser.expect(TokenKind::Colon);

                items.push(ast::DictItem {
                    key: Some(key),
                    value: parser.parse_conditional_expression_or_higher().expr,
                });
            }
        });

        self.expect(TokenKind::Rbrace);

        items.shrink_to_fit();

        ast::ExprDict {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            items,
        }
    }

    /// Parses a list of comprehension generators.
    ///
    /// These are the `for` and `async for` clauses in a comprehension, optionally
    /// followed by `if` clauses.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#grammar-token-python-grammar-comp_for>
    fn parse_generators(&mut self) -> Vec<ast::Comprehension> {
        const GENERATOR_SET: TokenSet = TokenSet::new([TokenKind::For, TokenKind::Async]);

        let mut generators = Vec::with_capacity(1);
        let mut progress = ParserProgress::default();

        while self.at_ts(GENERATOR_SET) {
            progress.assert_progressing(self);
            generators.push(self.parse_comprehension());
        }

        generators.shrink_to_fit();

        generators
    }

    /// Parses a comprehension.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `async` or `for` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#displays-for-lists-sets-and-dictionaries>
    fn parse_comprehension(&mut self) -> ast::Comprehension {
        let start = self.node_start();

        let is_async = self.eat(TokenKind::Async);

        if is_async {
            // test_err comprehension_missing_for_after_async
            // (async)
            // (x async x in iter)
            self.expect(TokenKind::For);
        } else {
            self.bump(TokenKind::For);
        }

        let mut target =
            self.parse_expression_list(ExpressionContext::starred_conditional().with_in_excluded());

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);
        self.validate_assignment_target(&target.expr);

        self.expect(TokenKind::In);
        let iter = self.parse_simple_expression(ExpressionContext::default());

        let ifs_snapshot = self.expr_scratch.snapshot();
        let mut progress = ParserProgress::default();

        while self.eat(TokenKind::If) {
            progress.assert_progressing(self);

            let parsed_expr = self.parse_simple_expression(ExpressionContext::default());

            self.expr_scratch.push(parsed_expr.expr);
        }

        ast::Comprehension {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            target: target.expr,
            iter: iter.expr,
            ifs: self.expr_scratch.take(ifs_snapshot),
            is_async,
        }
    }

    /// Parses a generator expression.
    ///
    /// The given `start` offset is the start of either the opening parenthesis if the generator is
    /// parenthesized or the first token of the expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#generator-expressions>
    fn parse_generator_expression(
        &mut self,
        element: Expr,
        start: TextSize,
        parenthesized: Parenthesized,
    ) -> ast::ExprGenerator {
        let generators = self.parse_generators();

        if parenthesized.is_yes() {
            self.expect(TokenKind::Rpar);
        }

        ast::ExprGenerator {
            elt: Box::new(element),
            generators,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            parenthesized: parenthesized.is_yes(),
        }
    }

    /// Parses a list comprehension expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#displays-for-lists-sets-and-dictionaries>
    fn parse_list_comprehension_expression(
        &mut self,
        element: Expr,
        start: TextSize,
    ) -> ast::ExprListComp {
        let generators = self.parse_generators();

        self.expect(TokenKind::Rsqb);

        ast::ExprListComp {
            elt: Box::new(element),
            generators,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a dictionary comprehension expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#displays-for-lists-sets-and-dictionaries>
    fn parse_dictionary_comprehension_expression(
        &mut self,
        key: Option<Expr>,
        value: Expr,
        start: TextSize,
    ) -> ast::ExprDictComp {
        let generators = self.parse_generators();

        self.expect(TokenKind::Rbrace);

        ast::ExprDictComp {
            key: key.map(Box::new),
            value: Box::new(value),
            generators,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a set comprehension expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#displays-for-lists-sets-and-dictionaries>
    fn parse_set_comprehension_expression(
        &mut self,
        element: Expr,
        start: TextSize,
    ) -> ast::ExprSetComp {
        let generators = self.parse_generators();

        self.expect(TokenKind::Rbrace);

        ast::ExprSetComp {
            elt: Box::new(element),
            generators,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a starred expression with the given precedence.
    ///
    /// The expression is parsed with the highest precedence. If the precedence
    /// of the parsed expression is lower than the given precedence, an error
    /// is reported.
    ///
    /// For example, if the given precedence is [`StarredExpressionPrecedence::BitOr`],
    /// the comparison expression is not allowed.
    ///
    /// Refer to the [Python grammar] for more information.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `*` token.
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    fn parse_starred_expression(&mut self, context: ExpressionContext) -> ast::ExprStarred {
        let start = self.node_start();
        self.bump(TokenKind::Star);

        let parsed_expr = match context.starred_expression_precedence() {
            StarredExpressionPrecedence::Conditional => self
                .parse_conditional_expression_or_higher_impl(
                    // test_err starred_starred_expression
                    // print(*
                    // *[])
                    // print(* *[])
                    context.disallow_starred_expressions(),
                ),
            StarredExpressionPrecedence::BitwiseOr => {
                self.parse_expression_with_bitwise_or_precedence()
            }
        };

        ast::ExprStarred {
            value: Box::new(parsed_expr.expr),
            ctx: ExprContext::Load,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an `await` expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `await` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#await-expression>
    fn parse_await_expression(&mut self) -> ast::ExprAwait {
        let start = self.node_start();
        self.bump(TokenKind::Await);

        let parsed_expr = self.parse_binary_expression_or_higher(
            OperatorPrecedence::Await,
            ExpressionContext::default(),
        );

        ast::ExprAwait {
            value: Box::new(parsed_expr.expr),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            postfix: false,
        }
    }

    /// Parses a `yield` expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `yield` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#yield-expressions>
    fn parse_yield_expression(&mut self) -> Expr {
        let start = self.node_start();
        self.bump(TokenKind::Yield);

        if self.eat(TokenKind::From) {
            return self.parse_yield_from_expression(start);
        }

        // basedpython: a statement expression is not a tail position of the
        // statement, so `validate_statement_expressions` rejects it — parse it
        // anyway so that is the diagnostic the user sees, rather than a cascade
        // from `yield` having been left without a value
        let at_value = self.at_expr() || starts_statement_expression(self.current_token_kind());

        let value = at_value.then(|| {
            let parsed_expr = self.parse_expression_list(ExpressionContext::starred_bitwise_or());

            // test_ok iter_unpack_yield_py37
            // # parse_options: {"target-version": "3.7"}
            // rest = (4, 5, 6)
            // def g(): yield (1, 2, 3, *rest)

            // test_ok iter_unpack_yield_py38
            // # parse_options: {"target-version": "3.8"}
            // rest = (4, 5, 6)
            // def g(): yield 1, 2, 3, *rest
            // def h(): yield 1, (yield 2, *rest), 3

            // test_err iter_unpack_yield_py37
            // # parse_options: {"target-version": "3.7"}
            // rest = (4, 5, 6)
            // def g(): yield 1, 2, 3, *rest
            // def h(): yield 1, (yield 2, *rest), 3
            self.check_tuple_unpacking(
                &parsed_expr,
                UnsupportedSyntaxErrorKind::StarTuple(StarTupleKind::Yield),
            );

            Box::new(parsed_expr.expr)
        });

        Expr::Yield(ast::ExprYield {
            value,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses a `yield from` expression.
    ///
    /// This method should not be used directly. Use [`Parser::parse_yield_expression`]
    /// even when parsing a `yield from` expression.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#yield-expressions>
    fn parse_yield_from_expression(&mut self, start: TextSize) -> Expr {
        // Grammar:
        //     'yield' 'from' expression
        //
        // Here, a tuple expression isn't allowed without the parentheses. But, we
        // allow it here to report better error message.
        //
        // Now, this also solves another problem. Take the following example:
        //
        // ```python
        // yield from x, y
        // ```
        //
        // If we didn't use the `parse_expression_list` method here, the parser
        // would have stopped at the comma. Then, the outer expression would
        // have been a tuple expression with two elements: `yield from x` and `y`.
        let expr = self
            .parse_expression_list(ExpressionContext::default())
            .expr;

        match &expr {
            Expr::Tuple(tuple) if !tuple.parenthesized => {
                self.add_error(ParseErrorType::UnparenthesizedTupleExpression, &expr);
            }
            _ => {}
        }

        Expr::YieldFrom(ast::ExprYieldFrom {
            value: Box::new(expr),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses a named expression (`:=`).
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `:=` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#assignment-expressions>
    fn parse_named_expression(&mut self, mut target: Expr, start: TextSize) -> ast::ExprNamed {
        self.bump(TokenKind::ColonEqual);

        if !target.is_name_expr() {
            self.add_error(ParseErrorType::InvalidNamedAssignmentTarget, target.range());
        }
        helpers::set_expr_ctx(&mut target, ExprContext::Store);

        let value = self.parse_conditional_expression_or_higher();

        let range = self.node_range(start);

        // test_err walrus_py37
        // # parse_options: { "target-version": "3.7" }
        // (x := 1)

        // test_ok walrus_py38
        // # parse_options: { "target-version": "3.8" }
        // (x := 1)

        self.add_unsupported_syntax_error(UnsupportedSyntaxErrorKind::Walrus, range);

        ast::ExprNamed {
            target: Box::new(target),
            value: Box::new(value.expr),
            range,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a lambda expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `lambda` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#lambda>
    fn parse_lambda_expr(&mut self) -> ast::ExprLambda {
        let start = self.node_start();
        self.bump(TokenKind::Lambda);

        // basedpython typed lambda: `lambda (a: int, b: str) -> int: body`
        // standard lambda: `lambda a, b: body` or `lambda: body`
        let (parameters, returns) = if self.at(TokenKind::Lpar) {
            self.error_if_not_basedpython(
                "typed lambda `lambda (...) -> ...:` is not valid in .py files".to_string(),
            );
            let params = self.parse_parameters(FunctionKind::FunctionDef);
            let returns = if self.eat(TokenKind::Rarrow) {
                Some(Box::new(
                    self.parse_expression_list(ExpressionContext::default())
                        .expr,
                ))
            } else {
                None
            };
            (Some(Box::new(params)), returns)
        } else if self.at(TokenKind::Colon) {
            // test_ok lambda_with_no_parameters
            // lambda: 1
            (None, None)
        } else {
            (
                Some(Box::new(self.parse_parameters(FunctionKind::Lambda))),
                None,
            )
        };

        self.expect(TokenKind::Colon);

        // test_ok lambda_with_valid_body
        // lambda x: x
        // lambda x: x if True else y
        // lambda x: await x
        // lambda x: lambda y: x + y
        // lambda x: (yield x)  # Parenthesized `yield` is fine
        // lambda x: x, *y

        // test_err lambda_body_with_starred_expr
        // lambda x: *y
        // lambda x: *y,
        // lambda x: *y, z
        // lambda x: *y and z

        // test_err lambda_body_with_yield_expr
        // lambda x: yield y
        // lambda x: yield from y

        // Lambda bodies recurse through the conditional layer without entering the binary parser.
        let body = self.with_recursion(Self::parse_conditional_expression_or_higher);

        ast::ExprLambda {
            body: Box::new(body.expr),
            parameters,
            returns,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an `if` expression.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `if` token.
    ///
    /// See: <https://docs.python.org/3/reference/expressions.html#conditional-expressions>
    fn parse_if_expression(
        &mut self,
        body: Expr,
        start: TextSize,
        context: ExpressionContext,
    ) -> ast::ExprIf {
        self.bump(TokenKind::If);

        let test = self.parse_simple_expression(ExpressionContext::default());

        self.expect(TokenKind::Else);

        // the `else` value is the tail of the conditional, so a trailing
        // interpolation conversion (`f"{a if b else c!s}"`) lands here — carry
        // only the interpolation flag so the `!` stays the conversion flag
        // rather than being eaten as a postfix force-unwrap. the other context
        // flags (excluded `in`/`for`, starred precedence, …) must not leak into
        // the `else` value, which otherwise parses as an ordinary expression.
        let orelse_context = if context.is_in_interpolation() {
            ExpressionContext::default().with_in_interpolation()
        } else {
            ExpressionContext::default()
        };
        // `a if b else a if b else ...` recurses through `orelse` at the conditional
        // layer, which the `parse_lhs_expression` guard does not cover — that scope is
        // released once each atom is parsed — so the guard belongs here. the
        // binary-expression guard has likewise already returned by this point
        let orelse =
            self.with_recursion(|p| p.parse_conditional_expression_or_higher_impl(orelse_context));

        ast::ExprIf {
            body: Box::new(body),
            test: Box::new(test.expr),
            orelse: Box::new(orelse.expr),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an IPython escape command at the expression level.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `IpyEscapeCommand` token.
    /// If the escape command kind is not `%` or `!`.
    fn parse_ipython_escape_command_expression(&mut self) -> ast::ExprIpyEscapeCommand {
        let start = self.node_start();

        let (value, kind) = self.bump_ipython_escape_command(IpyEscapeContext::Assignment);

        if !matches!(kind, IpyEscapeKind::Magic | IpyEscapeKind::Shell) {
            // This should never occur as the lexer won't allow it.
            unreachable!("IPython escape command expression is only allowed for % and !");
        }

        let command = ast::ExprIpyEscapeCommand {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            kind,
            value,
        };

        if self.options.mode != Mode::Ipython {
            self.add_error(ParseErrorType::UnexpectedIpythonEscapeCommand, &command);
        }

        command
    }

    /// Performs the following validations on the arguments:
    /// - Generator expressions are parenthesized when required by the argument context.
    fn validate_arguments(
        &mut self,
        arguments: &ast::Arguments,
        has_trailing_comma: bool,
        context: ArgumentsContext,
    ) {
        let generator_must_be_parenthesized = match context {
            ArgumentsContext::Call => has_trailing_comma || arguments.len() > 1,
            // CPython rejects an unparenthesized generator expression as a class base even though
            // this restriction isn't specified in the class definition grammar.
            ArgumentsContext::ClassDefinition => true,
        };

        if generator_must_be_parenthesized {
            for arg in &*arguments.args {
                if let Some(ast::ExprGenerator {
                    range,
                    parenthesized: false,
                    ..
                }) = arg.as_generator_expr()
                {
                    // test_ok args_unparenthesized_generator
                    // zip((x for x in range(10)), (y for y in range(10)))
                    // sum(x for x in range(10))
                    // sum((x for x in range(10)),)

                    // test_err args_unparenthesized_generator
                    // sum(x for x in range(10), 5)
                    // total(1, 2, x for x in range(5), 6)
                    // sum(x for x in range(10),)
                    self.add_error(ParseErrorType::UnparenthesizedGeneratorExpression, range);
                }
            }
        }
    }
}

/// Identifies the syntactic context for an argument list.
///
/// Unlike calls, class definitions require a sole generator expression to be parenthesized:
///
/// ```python
/// f(x for x in xs)
/// class C((x for x in xs)): ...
/// ```
#[derive(Debug, Copy, Clone)]
pub(super) enum ArgumentsContext {
    Call,
    ClassDefinition,
}

#[derive(Debug)]
pub(super) struct ParsedExpr {
    pub(super) expr: Expr,
    pub(super) is_parenthesized: bool,
    /// basedpython: the `local` / `once` modifier written before this expression
    /// when it is the sole element of a callable type's parameter list — the
    /// `local` of `(local int) -> None`. A multi-element list records its
    /// modifiers on the tuple instead
    pub(super) parameter_borrow: ParameterBorrow,
}

impl ParsedExpr {
    #[inline]
    pub(super) const fn is_unparenthesized_starred_expr(&self) -> bool {
        !self.is_parenthesized && self.expr.is_starred_expr()
    }

    #[inline]
    const fn is_unparenthesized_named_expr(&self) -> bool {
        !self.is_parenthesized && self.expr.is_named_expr()
    }
}

impl From<Expr> for ParsedExpr {
    #[inline]
    fn from(expr: Expr) -> Self {
        ParsedExpr {
            expr,
            is_parenthesized: false,
            parameter_borrow: ParameterBorrow::None,
        }
    }
}

/// basedpython: record `borrow` for the element at `index` of a callable
/// parameter list under construction.
///
/// The list stays sparse: a list where nothing was written never allocates, and
/// the leading unmodified elements are only padded once a modifier appears. The
/// result is either empty or exactly as long as the elements, which is what
/// [`ExprTuple::parameter_borrow`] and [`ExprCallableType::parameter_borrow`]
/// expect.
///
/// [`ExprTuple::parameter_borrow`]: ast::ExprTuple::parameter_borrow
/// [`ExprCallableType::parameter_borrow`]: ast::ExprCallableType::parameter_borrow
fn record_parameter_borrow(
    borrows: &mut Vec<ParameterBorrow>,
    index: usize,
    borrow: ParameterBorrow,
) {
    if !borrow.is_borrow() {
        return;
    }
    debug_assert!(borrows.len() <= index, "elements are recorded in order");
    borrows.resize(index, ParameterBorrow::None);
    borrows.push(borrow);
}

/// basedpython: the name a keyword argument written in one of the flexible
/// forms binds, together with how it was spelled.
///
/// Python names a keyword argument with a bare identifier, so `f(a.b=1)` and
/// `f("content-type"=1)` are both syntax errors there. basedpython accepts a
/// dotted path and a string literal too, taking the path's text or the string's
/// *value* as the name — which is the only way a call can reach a `**kwargs`
/// entry whose key is not spellable as a python identifier.
///
/// Returns `None` for anything else, including an f-string, a bytes literal,
/// and an implicitly concatenated string: those have no single obvious name,
/// and the caller reports them as a missing parameter name.
fn flexible_keyword_name(expr: &Expr) -> Option<(Name, ast::KeywordKey)> {
    match expr {
        Expr::Attribute(_) => {
            let mut name = String::new();
            dotted_name(expr, &mut name).then(|| (Name::new(name), ast::KeywordKey::Bare))
        }
        Expr::StringLiteral(string) => (!string.value.is_implicit_concatenated())
            .then(|| (Name::new(string.value.to_str()), ast::KeywordKey::Quoted)),
        _ => None,
    }
}

/// Writes a dotted path of plain names (`foo.bar.baz`) into `name`, reporting
/// whether `expr` was one. A partial path may have been written on failure.
fn dotted_name(expr: &Expr, name: &mut String) -> bool {
    match expr {
        Expr::Name(plain) => {
            name.push_str(&plain.id);
            true
        }
        // `a?.b` is an optional chain, which evaluates something; only a plain
        // attribute path is just text
        Expr::Attribute(attribute) if !attribute.optional => {
            dotted_name(&attribute.value, name) && {
                name.push('.');
                name.push_str(&attribute.attr);
                true
            }
        }
        _ => false,
    }
}

impl Deref for ParsedExpr {
    type Target = Expr;

    fn deref(&self) -> &Self::Target {
        &self.expr
    }
}

impl Ranged for ParsedExpr {
    #[inline]
    fn range(&self) -> TextRange {
        self.expr.range()
    }
}

#[derive(Debug)]
enum BinaryLikeOperator {
    Boolean(BoolOp),
    Comparison(CmpOp),
    Binary(Operator),
}

impl BinaryLikeOperator {
    /// Attempts to convert the token into the corresponding binary-like operator. `next` is
    /// required to distinguish `is not` and `not in` from their one-token alternatives.
    /// Returns [None] if it's not a binary-like operator.
    fn try_from_tokens(current: TokenKind, next: Option<TokenKind>) -> Option<BinaryLikeOperator> {
        if let Some(bool_op) = current.as_bool_operator() {
            Some(BinaryLikeOperator::Boolean(bool_op))
        } else if let Some(bin_op) = current.as_binary_operator() {
            Some(BinaryLikeOperator::Binary(bin_op))
        } else {
            helpers::token_kind_to_cmp_op(current, next).map(BinaryLikeOperator::Comparison)
        }
    }

    /// Returns the [`OperatorPrecedence`] for the given operator token or [None] if the token
    /// isn't an operator token.
    fn precedence(&self) -> OperatorPrecedence {
        match self {
            BinaryLikeOperator::Boolean(bool_op) => OperatorPrecedence::from(*bool_op),
            BinaryLikeOperator::Comparison(_) => OperatorPrecedence::ComparisonsMembershipIdentity,
            BinaryLikeOperator::Binary(bin_op) => OperatorPrecedence::from(*bin_op),
        }
    }
}

/// Represents the precedence used for parsing the value part of a starred expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StarredExpressionPrecedence {
    /// Matches `'*' bitwise_or` which is part of the `star_expression` rule in the
    /// [Python grammar](https://docs.python.org/3/reference/grammar.html).
    BitwiseOr,

    /// Matches `'*' expression` which is part of the `starred_expression` rule in the
    /// [Python grammar](https://docs.python.org/3/reference/grammar.html).
    Conditional,
}

/// Whether a `(` after an expression is read as a call while postfix expressions
/// are parsed. Only the basedpython decorated type has a reason to say no.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PostfixCalls {
    Allowed,
    Forbidden,
}

/// Represents the expression parsing context.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub(super) struct ExpressionContext(ExpressionContextFlags);

bitflags! {
    #[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
    struct ExpressionContextFlags: u16 {
        /// This flag is set when the `in` keyword should be excluded from a comparison expression.
        /// It is to avoid ambiguity in `for ... in ...` statements.
        const EXCLUDE_IN = 1 << 0;

        /// This flag is set when a starred expression should be allowed. This doesn't affect the
        /// parsing of a starred expression as it will be parsed nevertheless. But, if it is not
        /// allowed, an error is reported.
        const ALLOW_STARRED_EXPRESSION = 1 << 1;

        /// This flag is set when the value of a starred expression should be limited to bitwise OR
        /// precedence. Matches the `* bitwise_or` grammar rule if set.
        const STARRED_BITWISE_OR_PRECEDENCE = 1 << 2;

        /// This flag is set when a yield expression should be allowed. This doesn't affect the
        /// parsing of a yield expression as it will be parsed nevertheless. But, if it is not
        /// allowed, an error is reported.
        const ALLOW_YIELD_EXPRESSION = 1 << 3;

        /// basedpython: set while parsing a subscript slice element. Used to allow
        /// the bare `*` top-star marker in nested positions (e.g. inside `int | *`).
        const SUBSCRIPT_SLICE = 1 << 4;

        /// This flag is set when the `for` keyword, or `async` starting `async for`, should be
        /// excluded from an expression.
        const EXCLUDE_FOR = 1 << 5;

        /// basedpython: set while parsing the top-level value of an interpolated
        /// string replacement field (`f"{value!r}"`). Suppresses the postfix
        /// `!` force-unwrap operator so the trailing `!` stays available as the
        /// conversion flag. A parenthesised `(value!)` resets the context.
        const IN_INTERPOLATION = 1 << 6;

        /// basedpython: set while parsing the lower end of a type-parameter bound range
        /// (`T: Lower..Upper`). Suppresses the `..` attribute-access recovery so the `..`
        /// stays available as the range separator.
        const IN_TYPE_PARAM_BOUND = 1 << 7;

        /// basedpython: set while parsing a type expression (an annotation, a return
        /// type, a type-alias value, a subscript slice element, a callable type's
        /// parameter or return). Enables the `literal T` / `final T` use-site type
        /// modifiers, which are meaningless — and ambiguous with an ordinary
        /// reference to a variable named `literal` or `final` — anywhere else.
        const IN_TYPE_EXPRESSION = 1 << 8;
    }
}

impl ExpressionContext {
    /// Create a new context allowing starred expression at conditional precedence.
    pub(super) fn starred_conditional() -> Self {
        ExpressionContext::default()
            .with_starred_expression_allowed(StarredExpressionPrecedence::Conditional)
    }

    /// Create a new context allowing starred expression at bitwise OR precedence.
    pub(super) fn starred_bitwise_or() -> Self {
        ExpressionContext::default()
            .with_starred_expression_allowed(StarredExpressionPrecedence::BitwiseOr)
    }

    /// Create a new context allowing starred expression at bitwise OR precedence or yield
    /// expression.
    pub(super) fn yield_or_starred_bitwise_or() -> Self {
        ExpressionContext::starred_bitwise_or().with_yield_expression_allowed()
    }

    fn disallow_starred_expressions(self) -> Self {
        let flags = self.0 & !ExpressionContextFlags::ALLOW_STARRED_EXPRESSION;
        ExpressionContext(flags)
    }

    fn disallow_yield_expressions(self) -> Self {
        ExpressionContext(self.0 & !ExpressionContextFlags::ALLOW_YIELD_EXPRESSION)
    }

    /// Returns a new [`ExpressionContext`] which allows starred expression with the given
    /// precedence.
    fn with_starred_expression_allowed(self, precedence: StarredExpressionPrecedence) -> Self {
        let mut flags = self.0 | ExpressionContextFlags::ALLOW_STARRED_EXPRESSION;
        match precedence {
            StarredExpressionPrecedence::BitwiseOr => {
                flags |= ExpressionContextFlags::STARRED_BITWISE_OR_PRECEDENCE;
            }
            StarredExpressionPrecedence::Conditional => {
                flags -= ExpressionContextFlags::STARRED_BITWISE_OR_PRECEDENCE;
            }
        }
        ExpressionContext(flags)
    }

    /// Returns a new [`ExpressionContext`] which allows yield expression.
    fn with_yield_expression_allowed(self) -> Self {
        ExpressionContext(self.0 | ExpressionContextFlags::ALLOW_YIELD_EXPRESSION)
    }

    /// Returns a new [`ExpressionContext`] which excludes `in` as part of a comparison expression.
    pub(super) fn with_in_excluded(self) -> Self {
        ExpressionContext(self.0 | ExpressionContextFlags::EXCLUDE_IN)
    }

    /// Returns a new [`ExpressionContext`] which excludes `for` from an expression.
    fn with_for_excluded(self) -> Self {
        ExpressionContext(self.0 | ExpressionContextFlags::EXCLUDE_FOR)
    }

    /// Returns `true` if the `in` keyword should be excluded from a comparison expression.
    const fn is_in_excluded(self) -> bool {
        self.0.contains(ExpressionContextFlags::EXCLUDE_IN)
    }

    /// basedpython: returns a new context that marks parsing as being inside a
    /// subscript slice element, enabling bare `*` top-star markers nested
    /// inside type-position binops like `int | *`
    fn with_subscript_slice(self) -> Self {
        ExpressionContext(self.0 | ExpressionContextFlags::SUBSCRIPT_SLICE)
    }

    /// basedpython: returns `true` if currently parsing a subscript slice element
    const fn is_subscript_slice(self) -> bool {
        self.0.contains(ExpressionContextFlags::SUBSCRIPT_SLICE)
    }

    /// basedpython: returns a new context that marks parsing as being inside a type
    /// expression, enabling the `literal T` / `final T` use-site type modifiers
    pub(super) fn with_in_type_expression(self) -> Self {
        ExpressionContext(self.0 | ExpressionContextFlags::IN_TYPE_EXPRESSION)
    }

    /// basedpython: returns `true` if currently parsing a type expression
    const fn is_in_type_expression(self) -> bool {
        self.0.contains(ExpressionContextFlags::IN_TYPE_EXPRESSION)
    }

    /// basedpython: carry `outer`'s type-expression flag onto this context.
    ///
    /// Parentheses build their elements' contexts from scratch — the starred /
    /// yield / comprehension rules inside them are their own — but being in a
    /// type expression is a property of the *position*, and a parenthesis does
    /// not leave it. Without this, `(literal str)` reads `literal` as a name
    fn inheriting_type_expression(self, outer: Self) -> Self {
        if outer.is_in_type_expression() {
            self.with_in_type_expression()
        } else {
            self
        }
    }

    /// basedpython: returns a new context that marks parsing as being inside the
    /// value of an interpolated-string replacement field
    fn with_in_interpolation(self) -> Self {
        ExpressionContext(self.0 | ExpressionContextFlags::IN_INTERPOLATION)
    }

    /// basedpython: returns `true` if parsing the value of an interpolated-string
    /// replacement field, where a trailing `!` is the conversion flag rather
    /// than the postfix force-unwrap operator
    const fn is_in_interpolation(self) -> bool {
        self.0.contains(ExpressionContextFlags::IN_INTERPOLATION)
    }

    /// basedpython: returns a new context that marks parsing as being inside the lower end of a
    /// type-parameter bound range
    pub(super) fn with_in_type_param_bound(self) -> Self {
        ExpressionContext(self.0 | ExpressionContextFlags::IN_TYPE_PARAM_BOUND)
    }

    /// basedpython: returns `true` if parsing the lower end of a type-parameter bound range,
    /// where a following `..` separates the two ends rather than being a malformed attribute
    /// access
    const fn is_in_type_param_bound(self) -> bool {
        self.0.contains(ExpressionContextFlags::IN_TYPE_PARAM_BOUND)
    }

    /// Returns `true` if starred expressions are allowed.
    const fn is_starred_expression_allowed(self) -> bool {
        self.0
            .contains(ExpressionContextFlags::ALLOW_STARRED_EXPRESSION)
    }

    /// Returns `true` if yield expressions are allowed.
    const fn is_yield_expression_allowed(self) -> bool {
        self.0
            .contains(ExpressionContextFlags::ALLOW_YIELD_EXPRESSION)
    }

    /// Returns `true` if `for` should be excluded from the expression.
    const fn is_for_excluded(self) -> bool {
        self.0.contains(ExpressionContextFlags::EXCLUDE_FOR)
    }

    /// Returns the [`StarredExpressionPrecedence`] for the context, regardless of whether starred
    /// expressions are allowed or not.
    const fn starred_expression_precedence(self) -> StarredExpressionPrecedence {
        if self
            .0
            .contains(ExpressionContextFlags::STARRED_BITWISE_OR_PRECEDENCE)
        {
            StarredExpressionPrecedence::BitwiseOr
        } else {
            StarredExpressionPrecedence::Conditional
        }
    }
}

#[derive(Debug)]
struct InterpolatedStringData {
    elements: InterpolatedStringElements,
    range: TextRange,
    flags: AnyStringFlags,
}

impl From<InterpolatedStringData> for FString {
    fn from(value: InterpolatedStringData) -> Self {
        Self {
            elements: value.elements,
            range: value.range,
            flags: value.flags.into(),
            node_index: AtomicNodeIndex::NONE,
        }
    }
}

impl From<InterpolatedStringData> for TString {
    fn from(value: InterpolatedStringData) -> Self {
        Self {
            elements: value.elements,
            range: value.range,
            flags: value.flags.into(),
            node_index: AtomicNodeIndex::NONE,
        }
    }
}
