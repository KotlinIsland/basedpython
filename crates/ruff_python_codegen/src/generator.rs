//! Generate Python source code from an abstract syntax tree (AST).

use std::fmt::Write;
use std::ops::Deref;

use ruff_python_ast::helpers::{type_modifier_marker, use_site_variance_marker};
use ruff_python_ast::str::Quote;
use ruff_python_ast::{
    self as ast, Alias, AnyStringFlags, ArgOrKeyword, BoolOp, BytesLiteralFlags, CmpOp,
    Comprehension, ConversionFlag, DebugText, ExceptHandler, Expr, Identifier, MatchCase, Operator,
    Parameter, Parameters, Pattern, Singleton, Stmt, StringFlags, Suite, TypeParam,
    TypeParamParamSpec, TypeParamTypeVar, TypeParamTypeVarTuple, WithItem,
};
use ruff_python_ast::{ParameterWithDefault, TypeParams};
use ruff_python_literal::escape::{AsciiEscape, Escape, UnicodeEscape};
use ruff_source_file::LineEnding;

use super::stylist::{Indentation, Stylist};

mod precedence {
    pub(crate) const MIN: u8 = 0;
    pub(crate) const NAMED_EXPR: u8 = 1;
    pub(crate) const ASSIGN: u8 = 3;
    pub(crate) const ANN_ASSIGN: u8 = 5;
    pub(crate) const AUG_ASSIGN: u8 = 5;
    pub(crate) const EXPR: u8 = 5;
    pub(crate) const YIELD: u8 = 7;
    pub(crate) const YIELD_FROM: u8 = 7;
    pub(crate) const IF: u8 = 9;
    pub(crate) const FOR: u8 = 9;
    pub(crate) const WHILE: u8 = 9;
    pub(crate) const RETURN: u8 = 11;
    pub(crate) const SLICE: u8 = 13;
    pub(crate) const SUBSCRIPT: u8 = 13;
    pub(crate) const COMPREHENSION_TARGET: u8 = 19;
    pub(crate) const TUPLE: u8 = 19;
    pub(crate) const FORMATTED_VALUE: u8 = 19;
    pub(crate) const COMMA: u8 = 21;
    pub(crate) const ASSERT: u8 = 23;
    pub(crate) const COMPREHENSION_ELEMENT: u8 = 27;
    pub(crate) const LAMBDA: u8 = 27;
    pub(crate) const IF_EXP: u8 = 27;
    pub(crate) const COMPREHENSION: u8 = 29;
    pub(crate) const OR: u8 = 31;
    pub(crate) const AND: u8 = 33;
    pub(crate) const NOT: u8 = 35;
    pub(crate) const CMP: u8 = 37;
    pub(crate) const BIT_OR: u8 = 39;
    pub(crate) const BIT_XOR: u8 = 41;
    pub(crate) const BIT_AND: u8 = 43;
    pub(crate) const LSHIFT: u8 = 45;
    pub(crate) const RSHIFT: u8 = 45;
    pub(crate) const ADD: u8 = 47;
    pub(crate) const SUB: u8 = 47;
    pub(crate) const MULT: u8 = 49;
    pub(crate) const DIV: u8 = 49;
    pub(crate) const MOD: u8 = 49;
    pub(crate) const FLOORDIV: u8 = 49;
    pub(crate) const MAT_MULT: u8 = 49;
    pub(crate) const INVERT: u8 = 53;
    pub(crate) const UADD: u8 = 53;
    pub(crate) const USUB: u8 = 53;
    pub(crate) const POW: u8 = 55;
    pub(crate) const AWAIT: u8 = 57;
    pub(crate) const MAX: u8 = 63;
}

#[derive(Default, PartialEq, Eq, Clone, Copy, Debug)]
pub enum Mode {
    /// Ruff's default unparsing behaviour.
    #[default]
    Default,
    /// Emits same output as [`ast.unparse`](https://docs.python.org/3/library/ast.html#ast.unparse).
    AstUnparse,
    /// Emit basedpython surface syntax: `?.` for optional attribute access,
    /// `(args) -> returns` for `ExprCallableType`, `(field: type, ...)`
    /// anon NT literal for `ExprTuple` with `is_anon_named_tuple`,
    /// `typeof X` for `ExprSubscript` with `is_typeof`, `out X` / `in X` /
    /// `in out X` for a use-site variance marker subscript, `<value> cast
    /// <type>` for `ExprCall` with `is_cast`, `private type X = V` for
    /// `StmtTypeAlias` with `is_private`. Modifier-keyword
    /// decorators (whose source range does not start with `@`) are
    /// preserved as-is when this mode is active. Use when re-rendering an
    /// AST that may carry basedpython-only nodes / flags
    BasedPython,
}

impl Mode {
    /// Quote style to use.
    ///
    /// - [`Default`](`Mode::Default`): Output of `[AnyStringFlags.quote_style`].
    /// - [`AstUnparse`](`Mode::AstUnparse`): Always return [`Quote::Single`].
    /// - [`BasedPython`](`Mode::BasedPython`): same as `Default`.
    #[must_use]
    fn quote_style(self, flags: impl StringFlags) -> Quote {
        match self {
            Self::Default | Self::BasedPython => flags.quote_style(),
            Self::AstUnparse => Quote::Single,
        }
    }
}

/// basedpython: does `element`, rendered as the only field of a parameter list, still read
/// back as a parameter field rather than as a parenthesized expression?
///
/// The parser switches a parenthesized list to parameter-spec parsing when it sees a field
/// label (`name:`, `*name:`, `**name:`) or a double star (`**: T`, `**P`). A field that opens
/// with neither — a bare positional type, or the anonymous variadic that renders `*T` — is
/// indistinguishable from an ordinary expression on its own
fn reparses_as_parameter_field(element: &Expr) -> bool {
    match element {
        Expr::Named(_) => true,
        Expr::Starred(starred) => starred.value.is_starred_expr(),
        _ => false,
    }
}

pub struct Generator<'a> {
    /// The indentation style to use.
    indent: &'a Indentation,
    /// The line ending to use.
    line_ending: LineEnding,
    /// Unparsed code style. See [`Mode`] for more info.
    mode: Mode,
    buffer: String,
    indent_depth: usize,
    num_newlines: usize,
    initial: bool,
    /// basedpython: when set, the next statement continues the current line
    /// instead of starting one of its own. Set while unparsing a
    /// [statement expression](ast::ExprStatement), whose wrapped statement
    /// begins where the expression does.
    inline_statement: bool,
}

impl<'a> From<&'a Stylist<'a>> for Generator<'a> {
    fn from(stylist: &'a Stylist<'a>) -> Self {
        Self {
            indent: stylist.indentation(),
            line_ending: stylist.line_ending(),
            mode: Mode::default(),
            buffer: String::new(),
            indent_depth: 0,
            num_newlines: 0,
            initial: true,
            inline_statement: false,
        }
    }
}

impl<'a> Generator<'a> {
    pub const fn new(indent: &'a Indentation, line_ending: LineEnding) -> Self {
        Self {
            // Style preferences.
            indent,
            line_ending,
            mode: Mode::Default,
            // Internal state.
            buffer: String::new(),
            indent_depth: 0,
            num_newlines: 0,
            initial: true,
            inline_statement: false,
        }
    }

    /// Sets the mode for code unparsing.
    #[must_use]
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Generate source code from a [`Stmt`].
    pub fn stmt(mut self, stmt: &Stmt) -> String {
        self.unparse_stmt(stmt);
        self.generate()
    }

    /// Generate source code from an [`Expr`].
    pub fn expr(mut self, expr: &Expr) -> String {
        self.unparse_expr(expr, 0);
        self.generate()
    }

    fn newline(&mut self) {
        if !self.initial {
            self.num_newlines = std::cmp::max(self.num_newlines, 1);
        }
    }

    fn newlines(&mut self, extra: usize) {
        if !self.initial {
            self.num_newlines = std::cmp::max(self.num_newlines, 1 + extra);
        }
    }

    fn body(&mut self, stmts: &[Stmt]) {
        self.indent_depth = self.indent_depth.saturating_add(1);
        for stmt in stmts {
            self.unparse_stmt(stmt);
        }
        self.indent_depth = self.indent_depth.saturating_sub(1);
    }

    fn p(&mut self, s: &str) {
        if self.num_newlines > 0 {
            for _ in 0..self.num_newlines {
                self.buffer += &self.line_ending;
            }
            self.num_newlines = 0;
        }
        self.buffer += s;
    }

    fn p_id(&mut self, s: &Identifier) {
        self.p(s.as_str());
    }

    fn p_bytes_repr(&mut self, s: &[u8], flags: BytesLiteralFlags) {
        // raw bytes are interpreted without escapes and should all be ascii (it's a python syntax
        // error otherwise), but if this assumption is violated, a `Utf8Error` will be returned from
        // `p_raw_bytes`, and we should fall back on the normal escaping behavior instead of
        // panicking
        if flags.prefix().is_raw() {
            if let Ok(s) = std::str::from_utf8(s) {
                write!(self.buffer, "{}", flags.display_contents(s))
                    .expect("Writing to a String buffer should never fail");
                return;
            }
        }
        let quote_style = self.mode.quote_style(flags);
        let escape = AsciiEscape::with_preferred_quote(s, quote_style);
        if let Some(len) = escape.layout().len {
            self.buffer.reserve(len);
        }
        escape
            .bytes_repr(flags.triple_quotes())
            .write(&mut self.buffer)
            .expect("Writing to a String buffer should never fail");
    }

    fn p_str_repr(&mut self, s: &str, flags: impl Into<AnyStringFlags>) {
        let flags = flags.into();
        if flags.prefix().is_raw() {
            write!(self.buffer, "{}", flags.display_contents(s))
                .expect("Writing to a String buffer should never fail");
            return;
        }
        self.p(flags.prefix().as_str());

        let quote_style = self.mode.quote_style(flags);
        let escape = UnicodeEscape::with_preferred_quote(s, quote_style);
        if let Some(len) = escape.layout().len {
            self.buffer.reserve(len);
        }
        escape
            .str_repr(flags.triple_quotes())
            .write(&mut self.buffer)
            .expect("Writing to a String buffer should never fail");
    }

    fn p_if(&mut self, cond: bool, s: &str) {
        if cond {
            self.p(s);
        }
    }

    fn p_delim(&mut self, first: &mut bool, s: &str) {
        self.p_if(!std::mem::take(first), s);
    }

    pub(crate) fn generate(self) -> String {
        self.buffer
    }

    pub fn unparse_suite(&mut self, suite: &Suite) {
        for stmt in suite {
            self.unparse_stmt(stmt);
        }
    }

    pub(crate) fn unparse_stmt(&mut self, ast: &Stmt) {
        macro_rules! statement {
            ($body:block) => {{
                if !std::mem::take(&mut self.inline_statement) {
                    self.newline();
                    self.p(&self.indent.deref().repeat(self.indent_depth));
                }
                $body
                self.initial = false;
            }};
        }

        match ast {
            // basedpython: a trailing lambda block renders as its lowered
            // python — the surface `<call>:` form would not parse downstream.
            // this path has no type info to spell the last-parameter keyword,
            // so the function is appended positionally, ahead of any keyword
            // arguments (the same best-effort the text lowering uses for
            // uninspectable signatures)
            Stmt::FunctionDef(function) if function.trailing_lambda_callee().is_some() => {
                self.newlines(if self.indent_depth == 0 { 2 } else { 1 });
                statement!({
                    self.p("def ");
                    self.p_id(&function.name);
                    self.p("(");
                    self.unparse_parameters(&function.parameters);
                    self.p("):");
                });
                self.body(&function.body);
                let marker = &function.decorator_list[0].expression;
                statement!({
                    match marker {
                        Expr::Call(call)
                            if !call.is_cast && !call.is_checked_cast && !call.is_string_tag =>
                        {
                            self.unparse_expr(&call.func, precedence::MAX);
                            self.p("(");
                            let mut first = true;
                            let mut appended = false;
                            for arg_or_keyword in call.arguments.iter_source_order() {
                                if !appended && matches!(arg_or_keyword, ArgOrKeyword::Keyword(_)) {
                                    self.p_delim(&mut first, ", ");
                                    self.p_id(&function.name);
                                    appended = true;
                                }
                                match arg_or_keyword {
                                    ArgOrKeyword::Arg(arg) => {
                                        self.p_delim(&mut first, ", ");
                                        self.unparse_expr(arg, precedence::COMMA);
                                    }
                                    ArgOrKeyword::Keyword(keyword) => {
                                        self.p_delim(&mut first, ", ");
                                        if let Some(arg) = &keyword.arg {
                                            self.p_id(arg);
                                            self.p("=");
                                            self.unparse_expr(&keyword.value, precedence::COMMA);
                                        } else {
                                            self.p("**");
                                            self.unparse_expr(&keyword.value, precedence::MAX);
                                        }
                                    }
                                }
                            }
                            if !appended {
                                self.p_delim(&mut first, ", ");
                                self.p_id(&function.name);
                            }
                            self.p(")");
                        }
                        _ => {
                            self.unparse_expr(marker, precedence::MAX);
                            self.p("(");
                            self.p_id(&function.name);
                            self.p(")");
                        }
                    }
                });
                if self.indent_depth == 0 {
                    self.newlines(2);
                }
            }
            Stmt::FunctionDef(ast::StmtFunctionDef {
                is_async,
                name,
                parameters,
                body,
                returns,
                decorator_list,
                type_params,
                is_asserts_return,
                // basedpython: `raises` falls under the `..` deliberately. the
                // clause is compile-time-only and has no python spelling, and the
                // generator emits python — including for the statements a lowering
                // pass re-renders — so it is always erased
                ..
            }) => {
                self.newlines(if self.indent_depth == 0 { 2 } else { 1 });
                for decorator in decorator_list {
                    statement!({
                        self.p("@");
                        self.unparse_expr(&decorator.expression, precedence::MAX);
                    });
                }
                statement!({
                    if *is_async {
                        self.p("async ");
                    }
                    self.p("def ");
                    self.p_id(name);
                    if let Some(type_params) = type_params {
                        self.unparse_type_params(type_params);
                    }
                    self.p("(");
                    self.unparse_parameters(parameters);
                    self.p(")");
                    if *is_asserts_return {
                        // basedpython: `-> asserts x` names a place, not a type. the
                        // generator emits python, and such a function returns `None`
                        self.p(" -> None");
                    } else if let Some(returns) = returns {
                        self.p(" -> ");
                        // render at the same precedence as a variable annotation
                        // so a union return type stays unparenthesised
                        // (`-> int | None`) while a bare tuple still groups
                        // (`-> (int, str)`)
                        self.unparse_expr(returns, precedence::COMMA);
                    }
                    self.p(":");
                });
                self.body(body);
                if self.indent_depth == 0 {
                    self.newlines(2);
                }
            }
            Stmt::ClassDef(ast::StmtClassDef {
                name,
                arguments,
                body,
                decorator_list,
                type_params,
                implementation,
                range: _,
                node_index: _,
            }) => {
                self.newlines(if self.indent_depth == 0 { 2 } else { 1 });
                for decorator in decorator_list {
                    statement!({
                        self.p("@");
                        self.unparse_expr(&decorator.expression, precedence::MAX);
                    });
                }
                // basedpython: an `implementation A for B:` block keeps its
                // surface form; there is no python spelling of the header, and
                // the witness class it lowers to is built by the transpiler
                if let Some(header) = implementation.as_deref()
                    && self.mode == Mode::BasedPython
                {
                    statement!({
                        self.p("implementation ");
                        self.unparse_expr(&header.interface, precedence::MAX);
                        self.p(" for ");
                        self.p_id(name);
                        if let Some(type_params) = type_params {
                            self.unparse_type_params(type_params);
                        }
                        if let Some(witness) = &header.witness {
                            self.p(" as ");
                            self.p_id(witness);
                        }
                        self.p(":");
                    });
                    self.body(body);
                    if self.indent_depth == 0 {
                        self.newlines(2);
                    }
                    return;
                }
                statement!({
                    self.p("class ");
                    self.p_id(name);
                    if let Some(type_params) = type_params {
                        self.unparse_type_params(type_params);
                    }
                    if let Some(arguments) = arguments {
                        self.p("(");
                        let mut first = true;
                        for arg_or_keyword in arguments.iter_source_order() {
                            match arg_or_keyword {
                                ArgOrKeyword::Arg(arg) => {
                                    self.p_delim(&mut first, ", ");
                                    self.unparse_expr(arg, precedence::MAX);
                                }
                                ArgOrKeyword::Keyword(keyword) => {
                                    self.p_delim(&mut first, ", ");
                                    if let Some(arg) = &keyword.arg {
                                        self.p_id(arg);
                                        self.p("=");
                                    } else {
                                        self.p("**");
                                    }
                                    self.unparse_expr(&keyword.value, precedence::MAX);
                                }
                            }
                        }
                        self.p(")");
                    }
                    self.p(":");
                });
                self.body(body);
                if self.indent_depth == 0 {
                    self.newlines(2);
                }
            }
            Stmt::Return(ast::StmtReturn {
                value,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    if let Some(expr) = value {
                        self.p("return ");
                        self.unparse_expr(expr, precedence::RETURN);
                    } else {
                        self.p("return");
                    }
                });
            }
            Stmt::Delete(ast::StmtDelete {
                targets,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("del ");
                    let mut first = true;
                    for expr in targets {
                        self.p_delim(&mut first, ", ");
                        self.unparse_expr(expr, precedence::COMMA);
                    }
                });
            }
            Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                statement!({
                    for target in targets {
                        self.unparse_expr(target, precedence::ASSIGN);
                        self.p(" = ");
                    }
                    self.unparse_expr(value, precedence::ASSIGN);
                });
            }
            Stmt::AugAssign(ast::StmtAugAssign {
                target,
                op,
                value,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.unparse_expr(target, precedence::AUG_ASSIGN);
                    self.p(" ");
                    self.p(match op {
                        Operator::Add => "+",
                        Operator::Sub => "-",
                        Operator::Mult => "*",
                        Operator::MatMult => "@",
                        Operator::Div => "/",
                        Operator::Mod => "%",
                        Operator::Pow => "**",
                        Operator::LShift => "<<",
                        Operator::RShift => ">>",
                        Operator::BitOr => "|",
                        Operator::BitXor => "^",
                        Operator::BitAnd => "&",
                        Operator::FloorDiv => "//",
                        Operator::Coalesce => unreachable!("??= is not valid Python"),
                        Operator::Result => unreachable!("?= is not valid Python"),
                    });
                    self.p("= ");
                    self.unparse_expr(value, precedence::AUG_ASSIGN);
                });
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value,
                simple,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    let need_parens = matches!(target.as_ref(), Expr::Name(_)) && !simple;
                    self.p_if(need_parens, "(");
                    self.unparse_expr(target, precedence::ANN_ASSIGN);
                    self.p_if(need_parens, ")");
                    self.p(": ");
                    self.unparse_expr(annotation, precedence::COMMA);
                    if let Some(value) = value {
                        self.p(" = ");
                        self.unparse_expr(value, precedence::COMMA);
                    }
                });
            }
            Stmt::For(ast::StmtFor {
                is_async,
                target,
                iter,
                body,
                orelse,
                ..
            }) => {
                statement!({
                    if *is_async {
                        self.p("async ");
                    }
                    self.p("for ");
                    self.unparse_expr(target, precedence::FOR);
                    self.p(" in ");
                    self.unparse_expr(iter, precedence::MAX);
                    self.p(":");
                });
                self.body(body);
                if !orelse.is_empty() {
                    statement!({
                        self.p("else:");
                    });
                    self.body(orelse);
                }
            }
            Stmt::While(ast::StmtWhile {
                test,
                body,
                orelse,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("while ");
                    self.unparse_expr(test, precedence::WHILE);
                    self.p(":");
                });
                self.body(body);
                if !orelse.is_empty() {
                    statement!({
                        self.p("else:");
                    });
                    self.body(orelse);
                }
            }
            Stmt::If(ast::StmtIf {
                pattern,
                test,
                body,
                elif_else_clauses,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("if ");
                    self.unparse_if_condition(pattern.as_deref(), test);
                    self.p(":");
                });
                self.body(body);

                for clause in elif_else_clauses {
                    if let Some(test) = &clause.test {
                        statement!({
                            self.p("elif ");
                            self.unparse_if_condition(clause.pattern.as_deref(), test);
                            self.p(":");
                        });
                    } else {
                        statement!({
                            self.p("else:");
                        });
                    }
                    self.body(&clause.body);
                }
            }
            Stmt::With(ast::StmtWith {
                is_async,
                items,
                body,
                ..
            }) => {
                statement!({
                    if *is_async {
                        self.p("async ");
                    }
                    self.p("with ");
                    let mut first = true;
                    for item in items {
                        self.p_delim(&mut first, ", ");
                        self.unparse_with_item(item);
                    }
                    self.p(":");
                });
                self.body(body);
            }
            Stmt::Match(ast::StmtMatch {
                subject,
                cases,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("match ");
                    self.unparse_expr(subject, precedence::MAX);
                    self.p(":");
                });
                for case in cases {
                    self.indent_depth = self.indent_depth.saturating_add(1);
                    statement!({
                        self.unparse_match_case(case);
                    });
                    self.indent_depth = self.indent_depth.saturating_sub(1);
                }
            }
            Stmt::TypeAlias(ast::StmtTypeAlias {
                name,
                range: _,
                node_index: _,
                type_params,
                value,
                cases,
                is_private,
            }) => {
                statement!({
                    if self.mode == Mode::BasedPython && *is_private {
                        self.p("private ");
                    }
                    self.p("type ");
                    self.unparse_expr(name, precedence::MAX);
                    if let Some(type_params) = type_params {
                        self.unparse_type_params(type_params);
                    }
                    self.p(" = ");
                    // basedpython: a match type's value is the `case` blocks; `value` is the
                    // subject they are matched against
                    if cases.is_empty() {
                        self.unparse_expr(value, precedence::ASSIGN);
                    } else {
                        self.p("match ");
                        self.unparse_expr(value, precedence::MAX);
                        self.p(":");
                    }
                });
                for case in cases {
                    self.indent_depth = self.indent_depth.saturating_add(1);
                    statement!({
                        self.unparse_match_case(case);
                    });
                    self.indent_depth = self.indent_depth.saturating_sub(1);
                }
            }
            Stmt::Raise(ast::StmtRaise {
                exc,
                cause,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("raise");
                    if let Some(exc) = exc {
                        self.p(" ");
                        self.unparse_expr(exc, precedence::MAX);
                    }
                    if let Some(cause) = cause {
                        self.p(" from ");
                        self.unparse_expr(cause, precedence::MAX);
                    }
                });
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                is_star,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("try:");
                });
                self.body(body);

                for handler in handlers {
                    statement!({
                        self.unparse_except_handler(handler, *is_star);
                    });
                }

                if !orelse.is_empty() {
                    statement!({
                        self.p("else:");
                    });
                    self.body(orelse);
                }
                if !finalbody.is_empty() {
                    statement!({
                        self.p("finally:");
                    });
                    self.body(finalbody);
                }
            }
            Stmt::Assert(ast::StmtAssert {
                test,
                msg,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("assert ");
                    self.unparse_expr(test, precedence::ASSERT);
                    if let Some(msg) = msg {
                        self.p(", ");
                        self.unparse_expr(msg, precedence::ASSERT);
                    }
                });
            }
            Stmt::Import(ast::StmtImport {
                names,
                is_lazy,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    if *is_lazy {
                        self.p("lazy ");
                    }
                    self.p("import ");
                    let mut first = true;
                    for alias in names {
                        self.p_delim(&mut first, ", ");
                        self.unparse_alias(alias);
                    }
                });
            }
            Stmt::ImportFrom(ast::StmtImportFrom {
                module,
                names,
                level,
                is_lazy,
                is_export,
                range: _,
                node_index: _,
            }) => {
                // `from x export y` only has a spelling in basedpython; the
                // python rendering is its meaning, `from x import y as y`
                let export = *is_export && self.mode == Mode::BasedPython;
                statement!({
                    if *is_lazy {
                        self.p("lazy ");
                    }
                    self.p("from ");
                    if *level > 0 {
                        for _ in 0..*level {
                            self.p(".");
                        }
                    }
                    if let Some(module) = module {
                        self.p_id(module);
                    }
                    self.p(if export { " export " } else { " import " });
                    let mut first = true;
                    for alias in names {
                        self.p_delim(&mut first, ", ");
                        self.unparse_alias(alias);
                        if *is_export && !export && alias.asname.is_none() {
                            self.p(" as ");
                            self.p_id(&alias.name);
                        }
                    }
                });
            }
            Stmt::Global(ast::StmtGlobal {
                names,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("global ");
                    let mut first = true;
                    for name in names {
                        self.p_delim(&mut first, ", ");
                        self.p_id(name);
                    }
                });
            }
            Stmt::Nonlocal(ast::StmtNonlocal {
                names,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.p("nonlocal ");
                    let mut first = true;
                    for name in names {
                        self.p_delim(&mut first, ", ");
                        self.p_id(name);
                    }
                });
            }
            Stmt::Expr(ast::StmtExpr {
                value,
                range: _,
                node_index: _,
            }) => {
                statement!({
                    self.unparse_expr(value, precedence::EXPR);
                });
            }
            Stmt::Pass(_) => {
                statement!({
                    self.p("pass");
                });
            }
            Stmt::Break(ast::StmtBreak { value, .. }) => {
                statement!({
                    self.p("break");
                    if let Some(value) = value {
                        self.p(" ");
                        self.unparse_expr(value, precedence::EXPR);
                    }
                });
            }
            Stmt::Continue(_) => {
                statement!({
                    self.p("continue");
                });
            }
            Stmt::IpyEscapeCommand(ast::StmtIpyEscapeCommand { kind, value, .. }) => {
                statement!({
                    self.p(&format!("{kind}{value}"));
                });
            }
        }
    }

    fn unparse_except_handler(&mut self, ast: &ExceptHandler, star: bool) {
        match ast {
            ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                type_,
                name,
                body,
                range: _,
                node_index: _,
            }) => {
                self.p("except");
                if star {
                    self.p("*");
                }
                if let Some(type_) = type_ {
                    self.p(" ");
                    self.unparse_expr(type_, precedence::MAX);
                }
                if let Some(name) = name {
                    self.p(" as ");
                    self.p_id(name);
                }
                self.p(":");
                self.body(body);
            }
        }
    }

    fn unparse_pattern(&mut self, ast: &Pattern) {
        match ast {
            Pattern::MatchValue(ast::PatternMatchValue {
                value,
                range: _,
                node_index: _,
            }) => {
                self.unparse_expr(value, precedence::MAX);
            }
            Pattern::MatchSingleton(ast::PatternMatchSingleton {
                value,
                range: _,
                node_index: _,
            }) => {
                self.unparse_singleton(*value);
            }
            Pattern::MatchSequence(ast::PatternMatchSequence {
                patterns,
                range: _,
                node_index: _,
            }) => {
                self.p("[");
                let mut first = true;
                for pattern in patterns {
                    self.p_delim(&mut first, ", ");
                    self.unparse_pattern(pattern);
                }
                self.p("]");
            }
            Pattern::MatchMapping(ast::PatternMatchMapping {
                keys,
                patterns,
                rest,
                range: _,
                node_index: _,
            }) => {
                self.p("{");
                let mut first = true;
                for (key, pattern) in keys.iter().zip(patterns) {
                    self.p_delim(&mut first, ", ");
                    self.unparse_expr(key, precedence::MAX);
                    self.p(": ");
                    self.unparse_pattern(pattern);
                }
                if let Some(rest) = rest {
                    self.p_delim(&mut first, ", ");
                    self.p("**");
                    self.p_id(rest);
                }
                self.p("}");
            }
            Pattern::MatchClass(_) => {}
            Pattern::MatchStar(ast::PatternMatchStar {
                name,
                range: _,
                node_index: _,
            }) => {
                self.p("*");
                if let Some(name) = name {
                    self.p_id(name);
                } else {
                    self.p("_");
                }
            }
            Pattern::MatchAs(ast::PatternMatchAs {
                pattern,
                name,
                range: _,
                node_index: _,
            }) => {
                if let Some(pattern) = pattern {
                    self.unparse_pattern(pattern);
                    self.p(" as ");
                }
                if let Some(name) = name {
                    self.p_id(name);
                } else {
                    self.p("_");
                }
            }
            Pattern::MatchOr(ast::PatternMatchOr {
                patterns,
                range: _,
                node_index: _,
            }) => {
                let mut first = true;
                for pattern in patterns {
                    self.p_delim(&mut first, " | ");
                    self.unparse_pattern(pattern);
                }
            }
        }
    }

    /// Unparses the header of an `if` / `elif` clause: a plain condition, or the
    /// basedpython `let <pattern> := <subject>` pattern-matching form. The
    /// pattern is emitted in either mode — dropping it would silently turn a
    /// destructuring clause into a truthiness test
    fn unparse_if_condition(&mut self, pattern: Option<&Pattern>, test: &Expr) {
        if let Some(pattern) = pattern {
            self.p("let ");
            self.unparse_pattern(pattern);
            self.p(" := ");
        }
        self.unparse_expr(test, precedence::IF);
    }

    fn unparse_match_case(&mut self, ast: &MatchCase) {
        self.p("case ");
        self.unparse_pattern(&ast.pattern);
        if let Some(guard) = &ast.guard {
            self.p(" if ");
            self.unparse_expr(guard, precedence::MAX);
        }
        self.p(":");
        self.body(&ast.body);
    }

    fn unparse_type_params(&mut self, type_params: &TypeParams) {
        self.p("[");
        let mut first = true;
        for type_param in type_params {
            self.p_delim(&mut first, ", ");
            self.unparse_type_param(type_param);
        }
        self.p("]");
    }

    pub(crate) fn unparse_type_param(&mut self, ast: &TypeParam) {
        match ast {
            TypeParam::TypeVar(TypeParamTypeVar {
                name,
                bound,
                default,
                ..
            }) => {
                self.p_id(name);
                if let Some(expr) = bound {
                    self.p(": ");
                    self.unparse_expr(expr, precedence::MAX);
                }
                if let Some(expr) = default {
                    self.p(" = ");
                    self.unparse_expr(expr, precedence::MAX);
                }
            }
            TypeParam::TypeVarTuple(TypeParamTypeVarTuple {
                name,
                bound,
                default,
                ..
            }) => {
                self.p("*");
                self.p_id(name);
                if let Some(expr) = bound {
                    self.p(": ");
                    self.unparse_expr(expr, precedence::MAX);
                }
                if let Some(expr) = default {
                    self.p(" = ");
                    self.unparse_expr(expr, precedence::MAX);
                }
            }
            TypeParam::ParamSpec(TypeParamParamSpec {
                name,
                bound,
                default,
                ..
            }) => {
                self.p("**");
                self.p_id(name);
                if let Some(expr) = bound {
                    self.p(": ");
                    self.unparse_expr(expr, precedence::MAX);
                }
                if let Some(expr) = default {
                    self.p(" = ");
                    self.unparse_expr(expr, precedence::MAX);
                }
            }
        }
    }

    pub(crate) fn unparse_expr(&mut self, ast: &Expr, level: u8) {
        macro_rules! opprec {
            ($opty:ident, $x:expr, $enu:path, $($var:ident($op:literal, $prec:ident)),*$(,)?) => {
                match $x {
                    $(<$enu>::$var => (opprec!(@space $opty, $op), precedence::$prec),)*
                }
            };
            (@space bin, $op:literal) => {
                concat!(" ", $op, " ")
            };
            (@space un, $op:literal) => {
                $op
            };
        }
        macro_rules! group_if {
            ($lvl:expr, $body:block) => {{
                let group = level > $lvl;
                self.p_if(group, "(");
                let ret = $body;
                self.p_if(group, ")");
                ret
            }};
        }
        match ast {
            Expr::BoolOp(ast::ExprBoolOp {
                op,
                values,
                range: _,
                node_index: _,
            }) => {
                let (op, prec) = opprec!(bin, op, BoolOp, And("and", AND), Or("or", OR));
                group_if!(prec, {
                    let mut first = true;
                    for val in values {
                        self.p_delim(&mut first, op);
                        self.unparse_expr(val, prec + 1);
                    }
                });
            }
            Expr::Named(
                named @ ast::ExprNamed {
                    target,
                    value,
                    range: _,
                    node_index: _,
                },
            ) => {
                // basedpython: a keyword subscript argument (`A[foo=int]`) shares the
                // `Named` encoding with the walrus, so it has to be told apart by its
                // label or it unparses as an assignment
                if let Some(label) = named.label() {
                    self.p(label.id.as_str());
                    self.p("=");
                    self.unparse_expr(value, precedence::COMMA);
                } else {
                    group_if!(precedence::NAMED_EXPR, {
                        self.unparse_expr(target, precedence::NAMED_EXPR);
                        self.p(" := ");
                        self.unparse_expr(value, precedence::NAMED_EXPR + 1);
                    });
                }
            }
            Expr::BinOp(ast::ExprBinOp {
                left,
                op,
                right,
                range: _,
                node_index: _,
            }) => {
                let rassoc = matches!(op, Operator::Pow);
                let (op, prec) = opprec!(
                    bin,
                    op,
                    Operator,
                    Add("+", ADD),
                    Sub("-", SUB),
                    Mult("*", MULT),
                    MatMult("@", MAT_MULT),
                    Div("/", DIV),
                    Mod("%", MOD),
                    Pow("**", POW),
                    LShift("<<", LSHIFT),
                    RShift(">>", RSHIFT),
                    BitOr("|", BIT_OR),
                    BitXor("^", BIT_XOR),
                    BitAnd("&", BIT_AND),
                    FloorDiv("//", FLOORDIV),
                    Coalesce("??", OR),
                    Result("?", OR),
                );
                group_if!(prec, {
                    self.unparse_expr(left, prec + u8::from(rassoc));
                    self.p(op);
                    self.unparse_expr(right, prec + u8::from(!rassoc));
                });
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op,
                operand,
                range: _,
                node_index: _,
            }) => {
                let op_is_postfix = op.is_postfix();
                let (op, prec) = opprec!(
                    un,
                    op,
                    ruff_python_ast::UnaryOp,
                    Invert("~", INVERT),
                    Not("not ", NOT),
                    UAdd("+", UADD),
                    USub("-", USUB),
                    // basedpython postfix operators render after the operand;
                    // MAX precedence so they read as trailers (`load()^.value`)
                    // and parenthesise looser operands (`(a + b)!`)
                    Optional("?", MAX),
                    Propagate("^", MAX),
                    Force("!", MAX),
                );
                group_if!(prec, {
                    if op_is_postfix {
                        self.unparse_expr(operand, prec);
                        self.p(op);
                    } else {
                        self.p(op);
                        self.unparse_expr(operand, prec);
                    }
                });
            }
            Expr::Lambda(ast::ExprLambda {
                parameters,
                returns: _,
                body,
                range: _,
                node_index: _,
            }) => {
                group_if!(precedence::LAMBDA, {
                    self.p("lambda");
                    if let Some(parameters) = parameters {
                        self.p(" ");
                        self.unparse_parameters(parameters);
                    }
                    self.p(": ");
                    self.unparse_expr(body, precedence::LAMBDA);
                });
            }
            Expr::If(ast::ExprIf {
                test,
                body,
                orelse,
                range: _,
                node_index: _,
            }) => {
                group_if!(precedence::IF_EXP, {
                    self.unparse_expr(body, precedence::IF_EXP + 1);
                    self.p(" if ");
                    self.unparse_expr(test, precedence::IF_EXP + 1);
                    self.p(" else ");
                    self.unparse_expr(orelse, precedence::IF_EXP);
                });
            }
            Expr::Dict(dict) => {
                self.p("{");
                let mut first = true;
                for ast::DictItem { key, value } in dict {
                    self.p_delim(&mut first, ", ");
                    if let Some(key) = key {
                        self.unparse_expr(key, precedence::COMMA);
                        self.p(": ");
                        self.unparse_expr(value, precedence::COMMA);
                    } else if let Expr::Starred(outer) = value
                        && let Expr::Starred(inner) = outer.value.as_ref()
                    {
                        // basedpython `**: T` extra-items marker
                        self.p("**: ");
                        self.unparse_expr(&inner.value, precedence::COMMA);
                    } else {
                        self.p("**");
                        self.unparse_expr(value, precedence::MAX);
                    }
                }
                self.p("}");
            }
            Expr::Set(set) => {
                if set.is_empty() {
                    self.p("set()");
                } else {
                    self.p("{");
                    let mut first = true;
                    for item in set {
                        self.p_delim(&mut first, ", ");
                        self.unparse_expr(item, precedence::COMMA);
                    }
                    self.p("}");
                }
            }
            Expr::ListComp(ast::ExprListComp {
                elt,
                generators,
                range: _,
                node_index: _,
            }) => {
                self.p("[");
                self.unparse_expr(elt, precedence::COMPREHENSION_ELEMENT);
                self.unparse_comp(generators);
                self.p("]");
            }
            Expr::SetComp(ast::ExprSetComp {
                elt,
                generators,
                range: _,
                node_index: _,
            }) => {
                self.p("{");
                self.unparse_expr(elt, precedence::COMPREHENSION_ELEMENT);
                self.unparse_comp(generators);
                self.p("}");
            }
            Expr::DictComp(ast::ExprDictComp {
                key,
                value,
                generators,
                range: _,
                node_index: _,
            }) => {
                self.p("{");
                if let Some(key) = key {
                    self.unparse_expr(key, precedence::COMPREHENSION_ELEMENT);
                    self.p(": ");
                } else {
                    self.p("**");
                }
                self.unparse_expr(value, precedence::COMPREHENSION_ELEMENT);
                self.unparse_comp(generators);
                self.p("}");
            }
            Expr::Generator(ast::ExprGenerator {
                elt,
                generators,
                parenthesized: _,
                range: _,
                node_index: _,
            }) => {
                self.p("(");
                self.unparse_expr(elt, precedence::COMPREHENSION_ELEMENT);
                self.unparse_comp(generators);
                self.p(")");
            }
            Expr::Await(ast::ExprAwait {
                value,
                // basedpython: a postfix `expr.await` is semantically a prefix
                // `await expr`, so it always renders in prefix form. re-rendering
                // a statement that still carries the flag therefore lowers it
                postfix: _,
                range: _,
                node_index: _,
            }) => {
                group_if!(precedence::AWAIT, {
                    self.p("await ");
                    self.unparse_expr(value, precedence::MAX);
                });
            }
            Expr::Yield(ast::ExprYield {
                value,
                range: _,
                node_index: _,
            }) => {
                group_if!(precedence::YIELD, {
                    self.p("yield");
                    if let Some(value) = value {
                        self.p(" ");
                        self.unparse_expr(value, precedence::YIELD + 1);
                    }
                });
            }
            Expr::YieldFrom(ast::ExprYieldFrom {
                value,
                range: _,
                node_index: _,
            }) => {
                group_if!(precedence::YIELD_FROM, {
                    self.p("yield from ");
                    self.unparse_expr(value, precedence::MAX);
                });
            }
            Expr::Compare(ast::ExprCompare {
                left,
                ops,
                comparators,
                range: _,
                node_index: _,
            }) => {
                group_if!(precedence::CMP, {
                    let new_lvl = precedence::CMP + 1;
                    self.unparse_expr(left, new_lvl);
                    for (op, cmp) in ops.iter().zip(comparators) {
                        let op = match op {
                            CmpOp::Eq => " == ",
                            CmpOp::NotEq => " != ",
                            CmpOp::Lt => " < ",
                            CmpOp::LtE => " <= ",
                            CmpOp::Gt => " > ",
                            CmpOp::GtE => " >= ",
                            CmpOp::Is => " is ",
                            CmpOp::IsNot => " is not ",
                            CmpOp::In => " in ",
                            CmpOp::NotIn => " not in ",
                        };
                        self.p(op);
                        self.unparse_expr(cmp, new_lvl);
                    }
                });
            }
            Expr::Call(ast::ExprCall {
                func,
                arguments,
                range: _,
                node_index: _,
                is_cast: _,
                is_checked_cast: _,
                is_string_tag: _,
            }) => {
                self.unparse_expr(func, precedence::MAX);
                self.p("(");
                if let (
                    [
                        Expr::Generator(ast::ExprGenerator {
                            elt,
                            generators,
                            range: _,
                            node_index: _,
                            parenthesized: _,
                        }),
                    ],
                    [],
                ) = (arguments.args.as_ref(), arguments.keywords.as_ref())
                {
                    // Ensure that a single generator doesn't get double-parenthesized.
                    self.unparse_expr(elt, precedence::COMMA);
                    self.unparse_comp(generators);
                } else {
                    let mut first = true;

                    for arg_or_keyword in arguments.iter_source_order() {
                        match arg_or_keyword {
                            ArgOrKeyword::Arg(arg) => {
                                self.p_delim(&mut first, ", ");
                                self.unparse_expr(arg, precedence::COMMA);
                            }
                            ArgOrKeyword::Keyword(keyword) => {
                                self.p_delim(&mut first, ", ");
                                if let Some(arg) = &keyword.arg {
                                    self.p_id(arg);
                                    self.p("=");
                                    self.unparse_expr(&keyword.value, precedence::COMMA);
                                } else {
                                    self.p("**");
                                    self.unparse_expr(&keyword.value, precedence::MAX);
                                }
                            }
                        }
                    }
                }
                self.p(")");
            }
            Expr::FString(ast::ExprFString { value, .. }) => {
                self.unparse_f_string_value(value);
            }
            Expr::TString(ast::ExprTString { value, .. }) => {
                self.unparse_t_string_value(value);
            }
            Expr::StringLiteral(ast::ExprStringLiteral { value, .. }) => {
                self.unparse_string_literal_value(value);
            }
            Expr::BytesLiteral(ast::ExprBytesLiteral { value, .. }) => {
                let mut first = true;
                for bytes_literal in value {
                    self.p_delim(&mut first, " ");
                    self.p_bytes_repr(&bytes_literal.value, bytes_literal.flags);
                }
            }
            #[expect(clippy::eq_op)]
            Expr::NumberLiteral(ast::ExprNumberLiteral { value, .. }) => {
                static INF_STR: &str = "1e309";
                assert_eq!(f64::MAX_10_EXP, 308);

                match value {
                    ast::Number::Int(i) => {
                        self.p(&format!("{i}"));
                    }
                    ast::Number::Float(fp) => {
                        if fp.is_infinite() {
                            self.p(INF_STR);
                        } else {
                            self.p(&ruff_python_literal::float::to_string(*fp));
                        }
                    }
                    ast::Number::Complex { real, imag } => {
                        let value = if *real == 0.0 {
                            format!("{imag}j")
                        } else {
                            format!("({real}{imag:+}j)")
                        };
                        if real.is_infinite() || imag.is_infinite() {
                            self.p(&value.replace("inf", INF_STR));
                        } else {
                            self.p(&value);
                        }
                    }
                }
            }
            Expr::BooleanLiteral(ast::ExprBooleanLiteral { value, .. }) => {
                self.p(if *value { "True" } else { "False" });
            }
            Expr::NoneLiteral(_) => {
                self.p("None");
            }
            Expr::EllipsisLiteral(_) => {
                self.p("...");
            }
            Expr::Attribute(ast::ExprAttribute {
                value,
                attr,
                optional,
                ..
            }) => {
                let dot = if self.mode == Mode::BasedPython && *optional {
                    "?."
                } else {
                    "."
                };
                if let Expr::NumberLiteral(ast::ExprNumberLiteral {
                    value: ast::Number::Int(_),
                    ..
                }) = value.as_ref()
                {
                    self.p("(");
                    self.unparse_expr(value, precedence::MAX);
                    self.p(")");
                    self.p(dot);
                } else {
                    self.unparse_expr(value, precedence::MAX);
                    self.p(dot);
                }
                self.p_id(attr);
            }
            Expr::Subscript(ast::ExprSubscript {
                value,
                slice,
                is_typeof,
                ..
            }) => {
                // basedpython: `typeof X` is a subscript whose `value` is a synthetic
                // ellipsis placeholder and whose brackets exist only in the AST, so
                // unparsing it generically writes `...[X]`
                if self.mode == Mode::BasedPython && *is_typeof {
                    self.p("typeof ");
                    self.unparse_expr(slice, precedence::MAX);
                    return;
                }
                // a use-site variance keyword (`out X`) and a use-site type modifier
                // (`literal X`, `final X`) are both encoded as a subscript over a
                // synthetic marker name, and likewise carry no brackets in the source
                let marker = if self.mode == Mode::BasedPython {
                    use_site_variance_marker(ast)
                        .map(|(variance, inner)| (variance.keywords(), inner))
                        .or_else(|| {
                            type_modifier_marker(ast)
                                .map(|(modifier, inner)| (modifier.keyword(), inner))
                        })
                } else {
                    None
                };
                if let Some((keyword, inner)) = marker {
                    self.p(keyword);
                    self.p(" ");
                    self.unparse_expr(inner, precedence::SUBSCRIPT);
                } else {
                    self.unparse_expr(value, precedence::MAX);
                    self.p("[");
                    self.unparse_expr(slice, precedence::SUBSCRIPT);
                    self.p("]");
                }
            }
            Expr::Starred(ast::ExprStarred { value, .. }) => {
                self.p("*");
                self.unparse_expr(value, precedence::MAX);
            }
            Expr::Name(ast::ExprName { id, .. }) => self.p(id.as_str()),
            Expr::List(list) => {
                self.p("[");
                let mut first = true;
                for item in list {
                    self.p_delim(&mut first, ", ");
                    self.unparse_expr(item, precedence::COMMA);
                }
                self.p("]");
            }
            Expr::Tuple(tuple) => {
                // basedpython: the anonymous named tuple *value* form `(name=expr)` reuses
                // the labelled-field encoding of a parameter list for a different surface
                // syntax. its parentheses are part of that syntax rather than tuple
                // grouping, so they are always emitted — and never a trailing comma, which
                // would reparse the lone field as a plain one-element tuple
                if tuple.is_anon_named_tuple_value {
                    self.p("(");
                    let mut first = true;
                    for elt in tuple {
                        self.p_delim(&mut first, ", ");
                        self.unparse_expr(elt, precedence::COMMA);
                    }
                    self.p(")");
                }
                // a parameter-shape tuple (`(int, /, name: str)`) shares its encoding with a
                // callable's parameter list, so render it the same way
                else if tuple.has_parameter_shape() {
                    self.p("(");
                    self.unparse_parameter_spec(
                        &tuple.elts,
                        tuple.parameter_slash(),
                        tuple.parameter_star(),
                    );
                    // unlike a callable's parameter list, a tuple has no `->` to mark its
                    // parentheses as a parameter list. a lone field that doesn't start with
                    // a marker leaves `(x)` looking like a parenthesized expression, so it
                    // still needs the one-element tuple's trailing comma
                    let lone_unmarked_field = tuple.parameter_slash().is_none()
                        && tuple.parameter_star().is_none()
                        && matches!(tuple.elts.as_slice(), [elt] if !reparses_as_parameter_field(elt));
                    self.p_if(lone_unmarked_field, ",");
                    self.p(")");
                } else if tuple.is_empty() {
                    self.p("()");
                } else {
                    let lvl = match self.mode {
                        Mode::Default | Mode::BasedPython => precedence::TUPLE,
                        Mode::AstUnparse => precedence::MIN,
                    };
                    group_if!(lvl, {
                        let mut first = true;
                        for item in tuple {
                            self.p_delim(&mut first, ", ");
                            self.unparse_expr(item, precedence::COMMA);
                        }
                        self.p_if(tuple.len() == 1, ",");
                    });
                }
            }
            Expr::Slice(ast::ExprSlice {
                lower,
                upper,
                step,
                range: _,
                node_index: _,
            }) => {
                if let Some(lower) = lower {
                    self.unparse_expr(lower, precedence::SLICE);
                }
                self.p(":");
                if let Some(upper) = upper {
                    self.unparse_expr(upper, precedence::SLICE);
                }
                if let Some(step) = step {
                    self.p(":");
                    self.unparse_expr(step, precedence::SLICE);
                }
            }
            Expr::IpyEscapeCommand(ast::ExprIpyEscapeCommand { kind, value, .. }) => {
                self.p(&format!("{kind}{value}"));
            }
            Expr::CallableType(callable) => {
                assert_eq!(
                    self.mode,
                    Mode::BasedPython,
                    "callable type syntax should be transpiled before codegen"
                );
                // basedpython: the implicit receiver leads the type — `int.() -> str`
                if let Some(receiver) = &callable.receiver {
                    self.unparse_expr(receiver, precedence::MAX);
                    self.p(".");
                }
                self.p("(");
                self.unparse_parameter_spec(
                    &callable.args,
                    callable.parameter_slash(),
                    callable.parameter_star(),
                );
                self.p(") -> ");
                self.unparse_expr(&callable.returns, precedence::EXPR);
            }
            Expr::ProtocolType(protocol) => {
                assert_eq!(
                    self.mode,
                    Mode::BasedPython,
                    "inline protocol type syntax should be transpiled before codegen"
                );
                self.p("protocol(");
                for (i, member) in protocol.members.iter().enumerate() {
                    if i > 0 {
                        self.p("; ");
                    }
                    // a data member is `Named`, whose default rendering is the
                    // walrus `target := value` rather than the `name: type` label form
                    if let Expr::Named(named) = member {
                        self.unparse_expr(&named.target, precedence::MAX);
                        self.p(": ");
                        self.unparse_expr(&named.value, precedence::EXPR);
                    } else {
                        self.unparse_expr(member, precedence::EXPR);
                    }
                }
                self.p(")");
            }
            Expr::ProtocolMethod(method) => {
                assert_eq!(
                    self.mode,
                    Mode::BasedPython,
                    "inline protocol type syntax should be transpiled before codegen"
                );
                self.p("def ");
                self.p_id(&method.name);
                self.unparse_expr(&method.signature, precedence::EXPR);
            }
            Expr::Statement(statement) => {
                assert_eq!(
                    self.mode,
                    Mode::BasedPython,
                    "statement expressions should be transpiled before codegen"
                );
                self.inline_statement = true;
                self.unparse_stmt(&statement.stmt);
            }
        }
    }

    /// basedpython: unparse the body of a parameter list — the fields between the parentheses
    /// of a callable type (`(int, /, name: str) -> bool`) or of a parameter-shape tuple
    /// (`(int, /, name: str)`), which share one encoding
    ///
    /// The `/` and `*` markers carry no field of their own: each is an index into `elts`
    /// naming the field it precedes, and an index of `elts.len()` puts it last
    fn unparse_parameter_spec(&mut self, elts: &[Expr], slash: Option<u32>, star: Option<u32>) {
        let slash = slash.map(|i| i as usize);
        let star = star.map(|i| i as usize);
        let mut first = true;
        for (i, elt) in elts.iter().enumerate() {
            if Some(i) == slash {
                self.p_delim(&mut first, ", ");
                self.p("/");
            }
            if Some(i) == star {
                self.p_delim(&mut first, ", ");
                self.p("*");
            }
            self.p_delim(&mut first, ", ");
            self.unparse_parameter_spec_element(elt);
        }
        if Some(elts.len()) == slash {
            self.p_delim(&mut first, ", ");
            self.p("/");
        }
        if Some(elts.len()) == star {
            self.p_delim(&mut first, ", ");
            self.p("*");
        }
    }

    /// basedpython: unparse one field of a parameter list. A labelled field is an
    /// [`Expr::Named`] whose *target* carries the name and its star count, so it prints
    /// `name: T` / `*name: T` / `**name: T` — not the walrus the generic `Named` rendering
    /// would give. Every other shape (a bare positional type, the anonymous `*: T` / `**: T`,
    /// an unpacked `*Ts`) is already its own expression
    fn unparse_parameter_spec_element(&mut self, element: &Expr) {
        let Expr::Named(named) = element else {
            self.unparse_expr(element, precedence::EXPR);
            return;
        };
        match named.target.as_ref() {
            Expr::Starred(starred) => match starred.value.as_ref() {
                Expr::Starred(inner) => {
                    self.p("**");
                    self.unparse_expr(&inner.value, precedence::EXPR);
                }
                value => {
                    self.p("*");
                    self.unparse_expr(value, precedence::EXPR);
                }
            },
            target => self.unparse_expr(target, precedence::EXPR),
        }
        self.p(": ");
        self.unparse_expr(&named.value, precedence::EXPR);
    }

    pub(crate) fn unparse_singleton(&mut self, singleton: Singleton) {
        match singleton {
            Singleton::None => self.p("None"),
            Singleton::True => self.p("True"),
            Singleton::False => self.p("False"),
        }
    }

    fn unparse_parameters(&mut self, parameters: &Parameters) {
        let mut first = true;
        for (i, parameter_with_default) in parameters
            .posonlyargs
            .iter()
            .chain(&parameters.args)
            .enumerate()
        {
            self.p_delim(&mut first, ", ");
            self.unparse_parameter_with_default(parameter_with_default);
            self.p_if(i + 1 == parameters.posonlyargs.len(), ", /");
        }
        if parameters.vararg.is_some() || !parameters.kwonlyargs.is_empty() {
            self.p_delim(&mut first, ", ");
            self.p("*");
        }
        if let Some(vararg) = &parameters.vararg {
            self.unparse_parameter(vararg);
        }
        for kwarg in &parameters.kwonlyargs {
            self.p_delim(&mut first, ", ");
            self.unparse_parameter_with_default(kwarg);
        }
        if let Some(kwarg) = &parameters.kwarg {
            self.p_delim(&mut first, ", ");
            self.p("**");
            self.unparse_parameter(kwarg);
        }
    }

    fn unparse_parameter(&mut self, parameter: &Parameter) {
        // basedpython surface syntax keeps the `context` prefix; python output
        // drops it — the lowering passes the argument explicitly instead
        if self.mode == Mode::BasedPython && parameter.is_context {
            self.p("context ");
        }
        self.p_id(&parameter.name);
        if let Some(ann) = &parameter.annotation {
            self.p(": ");
            self.unparse_expr(ann, precedence::COMMA);
        }
    }

    fn unparse_parameter_with_default(&mut self, parameter_with_default: &ParameterWithDefault) {
        self.unparse_parameter(&parameter_with_default.parameter);
        if let Some(default) = &parameter_with_default.default {
            self.p("=");
            self.unparse_expr(default, precedence::COMMA);
        }
    }

    fn unparse_comp(&mut self, generators: &[Comprehension]) {
        for comp in generators {
            self.p(if comp.is_async {
                " async for "
            } else {
                " for "
            });
            self.unparse_expr(&comp.target, precedence::COMPREHENSION_TARGET);
            self.p(" in ");
            self.unparse_expr(&comp.iter, precedence::COMPREHENSION);
            for cond in &comp.ifs {
                self.p(" if ");
                self.unparse_expr(cond, precedence::COMPREHENSION);
            }
        }
    }

    fn unparse_string_literal(&mut self, string_literal: &ast::StringLiteral) {
        let ast::StringLiteral { value, flags, .. } = string_literal;
        self.p_str_repr(value, *flags);
    }

    fn unparse_string_literal_value(&mut self, value: &ast::StringLiteralValue) {
        let mut first = true;
        for string_literal in value {
            self.p_delim(&mut first, " ");
            self.unparse_string_literal(string_literal);
        }
    }

    fn unparse_f_string_value(&mut self, value: &ast::FStringValue) {
        let mut first = true;
        for f_string_part in value {
            self.p_delim(&mut first, " ");
            match f_string_part {
                ast::FStringPart::Literal(string_literal) => {
                    self.unparse_string_literal(string_literal);
                }
                ast::FStringPart::FString(f_string) => {
                    self.unparse_interpolated_string(&f_string.elements, f_string.flags.into());
                }
            }
        }
    }

    fn unparse_interpolated_string_body(
        &mut self,
        values: &[ast::InterpolatedStringElement],
        flags: AnyStringFlags,
    ) {
        for value in values {
            self.unparse_interpolated_string_element(value, flags);
        }
    }

    fn unparse_interpolated_element(
        &mut self,
        val: &Expr,
        debug_text: Option<&DebugText>,
        conversion: ConversionFlag,
        spec: Option<&ast::InterpolatedStringFormatSpec>,
        flags: AnyStringFlags,
    ) {
        let mut generator = Generator::new(self.indent, self.line_ending);
        generator.unparse_expr(val, precedence::FORMATTED_VALUE);
        // basedpython: a value rendering with a trailing `!` (a force-unwrap)
        // would be misread as the `!` conversion-flag marker, so parenthesise it
        if generator.buffer.ends_with('!') {
            generator.buffer.insert(0, '(');
            generator.buffer.push(')');
        }
        let brace = if generator.buffer.starts_with('{') {
            // put a space to avoid escaping the bracket
            "{ "
        } else {
            "{"
        };
        self.p(brace);

        if let Some(debug_text) = debug_text {
            self.buffer += debug_text.leading();
        }

        self.buffer += &generator.buffer;

        if let Some(debug_text) = debug_text {
            self.buffer += debug_text.trailing();
        }

        if !conversion.is_none() {
            self.p("!");

            self.p(&format!("{}", conversion as u8 as char));
        }

        if let Some(spec) = spec {
            self.p(":");
            self.unparse_f_string_specifier(&spec.elements, flags);
        }

        self.p("}");
    }

    fn unparse_interpolated_string_element(
        &mut self,
        element: &ast::InterpolatedStringElement,
        flags: AnyStringFlags,
    ) {
        match element {
            ast::InterpolatedStringElement::Literal(ast::InterpolatedStringLiteralElement {
                value,
                ..
            }) => {
                self.unparse_interpolated_string_literal_element(value, flags);
            }
            ast::InterpolatedStringElement::Interpolation(ast::InterpolatedElement {
                expression,
                debug_text,
                conversion,
                format_spec,
                range: _,
                node_index: _,
            }) => self.unparse_interpolated_element(
                expression,
                debug_text.as_ref(),
                *conversion,
                format_spec.as_deref(),
                flags,
            ),
        }
    }

    fn unparse_interpolated_string_literal_element(&mut self, s: &str, flags: AnyStringFlags) {
        let s = s.replace('{', "{{").replace('}', "}}");
        if flags.prefix().is_raw() {
            self.buffer += &s;
            return;
        }

        let quote_style = self.mode.quote_style(flags);
        let escape = UnicodeEscape::with_preferred_quote(&s, quote_style);
        if let Some(len) = escape.layout().len {
            self.buffer.reserve(len);
        }
        escape
            .write_body(&mut self.buffer)
            .expect("Writing to a String buffer should never fail");
    }

    fn unparse_f_string_specifier(
        &mut self,
        values: &[ast::InterpolatedStringElement],
        flags: AnyStringFlags,
    ) {
        self.unparse_interpolated_string_body(values, flags);
    }

    /// Unparse `values` with [`Generator::unparse_f_string_body`], using `quote` as the preferred
    /// surrounding quote style.
    fn unparse_interpolated_string(
        &mut self,
        values: &[ast::InterpolatedStringElement],
        flags: AnyStringFlags,
    ) {
        self.p(flags.prefix().as_str());

        let quote_style = self.mode.quote_style(flags);
        let flags = flags.with_quote_style(quote_style);
        self.p(flags.quote_str());
        self.unparse_interpolated_string_body(values, flags);
        self.p(flags.quote_str());
    }

    fn unparse_t_string_value(&mut self, value: &ast::TStringValue) {
        let mut first = true;
        for t_string in value {
            self.p_delim(&mut first, " ");
            self.unparse_interpolated_string(&t_string.elements, t_string.flags.into());
        }
    }

    fn unparse_alias(&mut self, alias: &Alias) {
        self.p_id(&alias.name);
        if let Some(asname) = &alias.asname {
            self.p(" as ");
            self.p_id(asname);
        }
    }

    fn unparse_with_item(&mut self, with_item: &WithItem) {
        self.unparse_expr(&with_item.context_expr, precedence::MAX);
        if let Some(optional_vars) = &with_item.optional_vars {
            self.p(" as ");
            self.unparse_expr(optional_vars, precedence::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::{Mod, ModModule};
    use ruff_python_parser::{self, Mode, ParseOptions, parse_module};
    use ruff_source_file::LineEnding;

    use crate::stylist::Indentation;

    use super::{Generator, Mode as UnparseMode};

    fn round_trip(contents: &str) -> String {
        let indentation = Indentation::default();
        let line_ending = LineEnding::default();
        let module = parse_module(contents).unwrap();
        let mut generator = Generator::new(&indentation, line_ending);
        generator.unparse_suite(module.suite());
        generator.generate()
    }

    /// Like [`round_trip`] but configure the [`Generator`] with the requested
    /// `indentation`, `line_ending` and `unparse_mode` settings.
    fn round_trip_with(
        indentation: &Indentation,
        line_ending: LineEnding,
        unparse_mode: UnparseMode,
        contents: &str,
    ) -> String {
        let module = parse_module(contents).unwrap();
        let mut generator = Generator::new(indentation, line_ending).with_mode(unparse_mode);
        generator.unparse_suite(module.suite());
        generator.generate()
    }

    /// Round-trip basedpython source: parse with the basedpython grammar and
    /// re-render in [`Mode::BasedPython`] so basedpython-only surface syntax
    /// (`T?`, `T ? E`, `expr^`, `expr!`, `?.`) is preserved.
    fn based_round_trip(contents: &str) -> String {
        let indentation = Indentation::default();
        let line_ending = LineEnding::default();
        let parsed = ruff_python_parser::parse(
            contents,
            ParseOptions::from(ruff_python_ast::PySourceType::BasedPython),
        )
        .expect("basedpython source should parse without errors");
        let Mod::Module(ModModule { body, .. }) = parsed.into_syntax() else {
            panic!("source code didn't return ModModule")
        };
        let mut generator =
            Generator::new(&indentation, line_ending).with_mode(UnparseMode::BasedPython);
        generator.unparse_suite(&body);
        generator.generate()
    }

    #[test_case::test_case("f: (int, str) -> bool" ; "positional types")]
    #[test_case::test_case("a: (a: int) -> str" ; "named field")]
    #[test_case::test_case("b: (int, /, name: str) -> bool" ; "positional-only marker")]
    #[test_case::test_case("c: (int, *, name: str) -> bool" ; "keyword-only marker")]
    #[test_case::test_case("d: (int, /) -> bool" ; "trailing positional-only marker")]
    #[test_case::test_case("g: (*args: int) -> None" ; "named variadic")]
    #[test_case::test_case("k: (*args: *Ts) -> None" ; "named variadic unpacking a pack")]
    #[test_case::test_case("l: (**kwargs: str) -> None" ; "named kwargs")]
    #[test_case::test_case("i: (*Ts) -> None" ; "unpacked pack")]
    #[test_case::test_case("j: (int, *Ts) -> None" ; "unpacked pack after a prefix")]
    #[test_case::test_case("n: (**P) -> None" ; "bare paramspec")]
    fn basedpython_callable_type_round_trip(contents: &str) {
        assert_eq!(based_round_trip(contents), contents);
    }

    #[test_case::test_case("a: (a: int)" ; "named field")]
    #[test_case::test_case("b: (int, /, name: str)" ; "positional-only marker")]
    #[test_case::test_case("c: (int, *, name: str)" ; "keyword-only marker")]
    #[test_case::test_case("d: (int, /)" ; "trailing positional-only marker")]
    #[test_case::test_case("e: (int, *)" ; "trailing keyword-only marker")]
    #[test_case::test_case("g: (*args: int)" ; "named variadic")]
    #[test_case::test_case("k: (*args: *Ts)" ; "named variadic unpacking a pack")]
    #[test_case::test_case("l: (**kwargs: str)" ; "named kwargs")]
    #[test_case::test_case("m: (name: int, other: str)" ; "every field named")]
    #[test_case::test_case("o: (int, name: str)" ; "positional then named")]
    #[test_case::test_case("p: (int, str)" ; "ordinary tuple")]
    #[test_case::test_case("q: (int,)" ; "ordinary one element tuple")]
    #[test_case::test_case("r = ()" ; "empty tuple")]
    fn basedpython_parameter_shape_tuple_round_trip(contents: &str) {
        assert_eq!(based_round_trip(contents), contents);
    }

    /// The anonymous variadic (`*: T`) and the kwargs catch-all (`**: T`) share their encoding
    /// with an unpacked pack (`*Ts`) and a `ParamSpec` (`**P`), so they render as the latter
    /// spelling. The rendering is stable, and the trailing comma keeps `*T` — which on its own
    /// would read as a parenthesized starred expression — parsing as a tuple
    #[test_case::test_case("e: (*: int)", "e: (*int,)" ; "anonymous variadic")]
    #[test_case::test_case("i: (**: str)", "i: (**str)" ; "anonymous kwargs")]
    fn basedpython_parameter_shape_tuple_normalizes(contents: &str, expected: &str) {
        assert_eq!(based_round_trip(contents), expected);
        assert_eq!(based_round_trip(expected), expected);
    }

    /// `(name=expr)` is an anonymous named tuple *value*: it reuses the labelled-field encoding
    /// of a parameter field for a different surface syntax, so it must not pick up the
    /// parameter-field rendering
    #[test]
    fn basedpython_anon_named_tuple_value_is_not_a_parameter_field() {
        assert_eq!(based_round_trip("m = (name=1)"), "m = (name=1)");
        assert_eq!(
            based_round_trip("n = (name=1, other=\"a\")"),
            "n = (name=1, other=\"a\")"
        );
    }

    /// A keyword subscript argument shares the labelled-field encoding with a parameter
    /// field, and its `Named` node with the walrus, so it round-trips only when told
    /// apart from both
    #[test_case::test_case("a: A[foo=int]" ; "single field")]
    #[test_case::test_case("b: A[foo=int, bar=str]" ; "several fields")]
    #[test_case::test_case("c: Two[bytes, foo=int]" ; "positional then field")]
    #[test_case::test_case("d = f[int, foo=str]()" ; "reified generic call")]
    #[test_case::test_case("e = A[foo=int]()" ; "value position")]
    #[test_case::test_case("g: A[foo=Two[bytes, bar=int]]" ; "nested")]
    #[test_case::test_case("h: A[foo=(int, str)]" ; "tuple value keeps its parentheses")]
    fn basedpython_keyword_subscript_round_trip(contents: &str) {
        assert_eq!(based_round_trip(contents), contents);
    }

    /// A use-site variance keyword is encoded as a subscript over a synthetic
    /// marker name, so without the basedpython rendering it round-trips as
    /// `dict[__variance_out__[int]]`
    #[test_case::test_case("a: list[out int]" ; "single covariant")]
    #[test_case::test_case("b: list[in int]" ; "single contravariant")]
    #[test_case::test_case("c: list[in out int]" ; "single invariant")]
    #[test_case::test_case("d: dict[out int, out str]" ; "covariant in both elements")]
    #[test_case::test_case("e: dict[out int, in str]" ; "contravariant in a later element")]
    #[test_case::test_case("f: dict[int, in str]" ; "bare first element")]
    #[test_case::test_case("g: X[in int, in out str, out bytes, int]" ; "every form mixed")]
    #[test_case::test_case("h: list[out list[in int]]" ; "nested")]
    fn basedpython_use_site_variance_round_trip(contents: &str) {
        assert_eq!(based_round_trip(contents), contents);
    }

    /// A use-site type modifier is a marker subscript with no brackets in the
    /// source, so it round-trips only when told apart from a real subscript
    #[test_case::test_case("a: literal str" ; "annotation")]
    #[test_case::test_case("b: final int = 1" ; "annotation with value")]
    #[test_case::test_case("c: literal str | None" ; "binds tighter than union")]
    #[test_case::test_case("d: list[literal str]" ; "nested in a subscript")]
    #[test_case::test_case("e: final dict[str, int]" ; "modified subscript")]
    #[test_case::test_case("type F = literal str" ; "type alias")]
    fn basedpython_type_modifier_round_trip(contents: &str) {
        assert_eq!(based_round_trip(contents), contents);
    }

    /// `typeof X` has no brackets in the source either — its subscript `value` is
    /// a synthetic ellipsis placeholder, so the generic rendering writes `...[X]`
    #[test_case::test_case("a: typeof x" ; "annotation")]
    #[test_case::test_case("b: list[typeof x]" ; "nested in a subscript")]
    #[test_case::test_case("c: typeof x | None" ; "in a union")]
    #[test_case::test_case("d: typeof a.b" ; "over an attribute")]
    fn basedpython_typeof_round_trip(contents: &str) {
        assert_eq!(based_round_trip(contents), contents);
    }

    /// An `implementation A for B:` header has no python spelling, so it
    /// round-trips through its own surface form. Only the header is asserted:
    /// the generator's class-body layout (a blank line after the header, `@`-form
    /// modifier decorators) is its existing behaviour for every declaration
    #[test_case::test_case("implementation A for B:" ; "anonymous")]
    #[test_case::test_case("implementation A for B as BAsA:" ; "named witness")]
    #[test_case::test_case("implementation Container[int] for B:" ; "specialized interface")]
    #[test_case::test_case("implementation collections.abc.Sized for B:" ; "dotted interface")]
    #[test_case::test_case("implementation Show for list[Element: Show]:" ; "bounded target")]
    fn basedpython_implementation_round_trip(header: &str) {
        let rendered = based_round_trip(&format!("{header}\n    def f(self):\n        pass"));
        assert_eq!(rendered.lines().next(), Some(header));
    }

    /// In python mode there is no `implementation` keyword to emit; the header
    /// falls back to the ordinary `class` rendering rather than producing
    /// basedpython-only syntax in a `.py` output
    #[test]
    fn implementation_header_is_not_emitted_in_python_mode() {
        let parsed = ruff_python_parser::parse(
            "implementation A for B:\n    def f(self):\n        pass",
            ParseOptions::from(ruff_python_ast::PySourceType::BasedPython),
        )
        .expect("basedpython source should parse without errors");
        let Mod::Module(ModModule { body, .. }) = parsed.into_syntax() else {
            panic!("source code didn't return ModModule")
        };
        let indentation = Indentation::default();
        let mut generator = Generator::new(&indentation, LineEnding::default());
        generator.unparse_suite(&body);
        let out = generator.generate();
        assert!(!out.contains("implementation"), "got {out:?}");
        assert!(out.contains("class B:"), "got {out:?}");
    }

    fn jupyter_round_trip(contents: &str) -> String {
        let indentation = Indentation::default();
        let line_ending = LineEnding::default();
        let parsed =
            ruff_python_parser::parse(contents, ParseOptions::from(Mode::Ipython)).unwrap();
        let Mod::Module(ModModule { body, .. }) = parsed.into_syntax() else {
            panic!("Source code didn't return ModModule")
        };
        let [stmt] = body.as_slice() else {
            panic!("Expected only one statement in source code")
        };
        let mut generator = Generator::new(&indentation, line_ending);
        generator.unparse_stmt(stmt);
        generator.generate()
    }

    macro_rules! assert_round_trip {
        ($contents:expr) => {
            assert_eq!(
                round_trip($contents),
                $contents.replace('\n', LineEnding::default().as_str())
            );
        };
    }

    #[test]
    fn unparse_magic_commands() {
        assert_eq!(
            jupyter_round_trip("%matplotlib inline"),
            "%matplotlib inline"
        );
        assert_eq!(
            jupyter_round_trip("%matplotlib \\\n  inline"),
            "%matplotlib   inline"
        );
        assert_eq!(jupyter_round_trip("dir = !pwd"), "dir = !pwd");
    }

    #[test]
    fn unparse() {
        assert_round_trip!("{i for i in b async for i in a if await i for b in i}");
        assert_round_trip!("f(**x)");
        assert_round_trip!("{**x}");
        assert_round_trip!("f(**([] or 5))");
        assert_round_trip!(r#"my_function(*[1], *[2], **{"three": 3}, **{"four": "four"})"#);
        assert_round_trip!("{**([] or 5)}");
        assert_round_trip!("del l[0]");
        assert_round_trip!("del obj.x");
        assert_round_trip!("a @ b");
        assert_round_trip!("a @= b");
        assert_round_trip!("x.foo");
        assert_round_trip!("return await (await bar())");
        assert_round_trip!("(5).foo");
        assert_round_trip!(r#"our_dict = {"a": 1, **{"b": 2, "c": 3}}"#);
        assert_round_trip!(r"j = [1, 2, 3]");
        assert_round_trip!(
            r#"def test(a1, a2, b1=j, b2="123", b3={}, b4=[]):
    pass"#
        );
        assert_round_trip!("a @ b");
        assert_round_trip!("a @= b");
        assert_round_trip!("[1, 2, 3]");
        assert_round_trip!("foo(1)");
        assert_round_trip!("foo(1, 2)");
        assert_round_trip!("foo(x for x in y)");
        assert_round_trip!("foo([x for x in y])");
        assert_round_trip!("foo([(x := 2) for x in y])");
        assert_round_trip!("x = yield 1");
        assert_round_trip!("return (yield 1)");
        assert_round_trip!("lambda: (1, 2, 3)");
        assert_round_trip!("return 3 and 4");
        assert_round_trip!("return 3 or 4");
        assert_round_trip!("yield from some()");
        assert_round_trip!(r#"assert (1, 2, 3), "msg""#);
        assert_round_trip!("import ast");
        assert_round_trip!("import operator as op");
        assert_round_trip!("from math import floor");
        assert_round_trip!("from .. import foobar");
        assert_round_trip!("from ..aaa import foo, bar as bar2");
        assert_round_trip!(r#"return f"functools.{qualname}({', '.join(args)})""#);
        assert_round_trip!(r#"my_function(*[1], *[2], **{"three": 3}, **{"four": "four"})"#);
        assert_round_trip!(r#"our_dict = {"a": 1, **{"b": 2, "c": 3}}"#);
        assert_round_trip!("f(**x)");
        assert_round_trip!("{**x}");
        assert_round_trip!("f(**([] or 5))");
        assert_round_trip!("{**([] or 5)}");
        assert_round_trip!(r#"return f"functools.{qualname}({', '.join(args)})""#);
        assert_round_trip!(
            r#"class TreeFactory(*[FactoryMixin, TreeBase], **{"metaclass": Foo}):
    pass"#
        );
        assert_round_trip!(
            r"class Foo(Bar, object):
    pass"
        );
        assert_round_trip!(
            r"class Foo[T]:
    pass"
        );
        assert_round_trip!(
            r"class Foo[T](Bar):
    pass"
        );
        assert_round_trip!(
            r"class Foo[*Ts]:
    pass"
        );
        assert_round_trip!(
            r"class Foo[**P]:
    pass"
        );
        assert_round_trip!(
            r"class Foo[T, U, *Ts, **P]:
    pass"
        );
        assert_round_trip!(
            r"def f() -> (int, str):
    pass"
        );
        assert_round_trip!("[await x async for x in y]");
        assert_round_trip!("[await i for i in b if await c]");
        assert_round_trip!("(await x async for x in y)");
        assert_round_trip!(
            r#"async def read_data(db):
    async with connect(db) as db_cxn:
        data = await db_cxn.fetch("SELECT foo FROM bar;")
    async for datum in data:
        if quux(datum):
            return datum"#
        );
        assert_round_trip!(
            r"def f() -> (int, int):
    pass"
        );
        assert_round_trip!(
            r"def test(a, b, /, c, *, d, **kwargs):
    pass"
        );
        assert_round_trip!(
            r"def test(a=3, b=4, /, c=7):
    pass"
        );
        assert_round_trip!(
            r"def test(a, b=4, /, c=8, d=9):
    pass"
        );
        assert_round_trip!(
            r"def test[T]():
    pass"
        );
        assert_round_trip!(
            r"def test[*Ts]():
    pass"
        );
        assert_round_trip!(
            r"def test[**P]():
    pass"
        );
        assert_round_trip!(
            r"def test[T, U, *Ts, **P]():
    pass"
        );
        assert_round_trip!(
            r"def call(*popenargs, timeout=None, **kwargs):
    pass"
        );
        assert_round_trip!(
            r"@functools.lru_cache(maxsize=None)
def f(x: int, y: int) -> int:
    return x + y"
        );
        assert_round_trip!(
            r"try:
    pass
except Exception as e:
    pass"
        );
        assert_round_trip!(
            r"try:
    pass
except* Exception as e:
    pass"
        );
        assert_round_trip!(
            r"match x:
    case [1, 2, 3]:
        return 2
    case 4 as y:
        return y"
        );
        assert_round_trip!(
            r"type X = int
type Y = str"
        );
        assert_eq!(round_trip(r"x = (1, 2, 3)"), r"x = 1, 2, 3");
        assert_eq!(round_trip(r"x = (1, (2, 3))"), r"x = 1, (2, 3)");
        assert_eq!(round_trip(r"-(1) + ~(2) + +(3)"), r"-1 + ~2 + +3");
        assert_round_trip!(
            r"def f():

    def f():
        pass"
        );
        assert_round_trip!(
            r"@foo
def f():

    @foo
    def f():
        pass"
        );

        assert_round_trip!(
            r"@foo
class Foo:

    @foo
    def f():
        pass"
        );

        assert_round_trip!(r"[lambda n: n for n in range(10)]");
        assert_round_trip!(r"[n[0:2] for n in range(10)]");
        assert_round_trip!(r"[n[0] for n in range(10)]");
        assert_round_trip!(r"[(n, n * 2) for n in range(10)]");
        assert_round_trip!(r"[1 if n % 2 == 0 else 0 for n in range(10)]");
        assert_round_trip!(r"[n % 2 == 0 or 0 for n in range(10)]");
        assert_round_trip!(r"[(n := 2) for n in range(10)]");
        assert_round_trip!(r"((n := 2) for n in range(10))");
        assert_round_trip!(r"[n * 2 for n in range(10)]");
        assert_round_trip!(r"{n * 2 for n in range(10)}");
        assert_round_trip!(r"{i: n * 2 for i, n in enumerate(range(10))}");
        assert_round_trip!(
            "class SchemaItem(NamedTuple):
    fields: ((\"property_key\", str),)"
        );
        assert_round_trip!(
            "def func():
    return (i := 1)"
        );
        assert_round_trip!("yield (i := 1)");
        assert_round_trip!("x = (i := 1)");
        assert_round_trip!("x += (i := 1)");

        // Type aliases
        assert_round_trip!(r"type Foo = int | str");
        assert_round_trip!(r"type Foo[T] = list[T]");
        assert_round_trip!(r"type Foo[*Ts] = ...");
        assert_round_trip!(r"type Foo[**P] = ...");
        assert_round_trip!(r"type Foo[T = int] = list[T]");
        assert_round_trip!(r"type Foo[*Ts = int] = ...");
        assert_round_trip!(r"type Foo[*Ts = *int] = ...");
        assert_round_trip!(r"type Foo[**P = int] = ...");
        assert_round_trip!(r"type Foo[T, U, *Ts, **P] = ...");
        // https://github.com/astral-sh/ruff/issues/6498
        assert_round_trip!(r"f(a=1, *args, **kwargs)");
        assert_round_trip!(r"f(*args, a=1, **kwargs)");
        assert_round_trip!(r"f(*args, a=1, *args2, **kwargs)");
        assert_round_trip!("class A(*args, a=2, *args2, **kwargs):\n    pass");
    }

    #[test]
    fn quote() {
        assert_round_trip!(r#""hello""#);
        assert_round_trip!(r"'hello'");
        assert_round_trip!(r"u'hello'");
        assert_round_trip!(r"r'hello'");
        assert_round_trip!(r"b'hello'");
        assert_round_trip!(r#"b"hello""#);
        assert_round_trip!(r"f'hello'");
        assert_round_trip!(r#"f"hello""#);
        assert_eq!(round_trip(r#"("abc" "def" "ghi")"#), r#""abc" "def" "ghi""#);
        assert_eq!(round_trip(r#""he\"llo""#), r#"'he"llo'"#);
        assert_eq!(round_trip(r#"b"he\"llo""#), r#"b'he"llo'"#);
        assert_eq!(round_trip(r#"f"abc{'def'}{1}""#), r#"f"abc{'def'}{1}""#);
        assert_round_trip!(r#"f'abc{"def"}{1}'"#);
    }

    /// test all of the valid string literal prefix and quote combinations from
    /// <https://docs.python.org/3/reference/lexical_analysis.html#string-and-bytes-literals>
    ///
    /// Note that the numeric ids on the input/output and quote fields prevent name conflicts from
    /// the `test_matrix` but are otherwise unnecessary
    #[test_case::test_matrix(
        [
            ("r", "r", 0),
            ("u", "u", 1),
            ("R", "R", 2),
            ("U", "u", 3), // case not tracked
            ("f", "f", 4),
            ("F", "f", 5),   // f case not tracked
            ("fr", "rf", 6), // r before f
            ("Fr", "rf", 7), // f case not tracked, r before f
            ("fR", "Rf", 8), // r before f
            ("FR", "Rf", 9), // f case not tracked, r before f
            ("rf", "rf", 10),
            ("rF", "rf", 11), // f case not tracked
            ("Rf", "Rf", 12),
            ("RF", "Rf", 13), // f case not tracked
            // bytestrings
            ("b", "b", 14),
            ("B", "b", 15),   // b case
            ("br", "rb", 16), // r before b
            ("Br", "rb", 17), // b case, r before b
            ("bR", "Rb", 18), // r before b
            ("BR", "Rb", 19), // b case, r before b
            ("rb", "rb", 20),
            ("rB", "rb", 21), // b case
            ("Rb", "Rb", 22),
            ("RB", "Rb", 23), // b case
        ],
        [("\"", 0), ("'",1), ("\"\"\"", 2), ("'''", 3)],
        ["hello", "{hello} {world}"]
    )]
    fn prefix_quotes((inp, out, _id): (&str, &str, u8), (quote, _id2): (&str, u8), base: &str) {
        let input = format!("{inp}{quote}{base}{quote}");
        let output = format!("{out}{quote}{base}{quote}");
        assert_eq!(round_trip(&input), output);
    }

    #[test]
    fn raw() {
        assert_round_trip!(r#"r"a\.b""#); // https://github.com/astral-sh/ruff/issues/9663
        assert_round_trip!(r#"R"a\.b""#);
    }

    #[test]
    fn self_documenting_fstring() {
        assert_round_trip!(r#"f"{ chr(65)  =   }""#);
        assert_round_trip!(r#"f"{ chr(65)  =   !s}""#);
        assert_round_trip!(r#"f"{ chr(65)  =   !r}""#);
        assert_round_trip!(r#"f"{ chr(65)  =   :#x}""#);
        assert_round_trip!(r#"f"{  ( chr(65)  ) = }""#);
        assert_round_trip!(r#"f"{a=!r:0.05f}""#);
        // https://github.com/astral-sh/ruff/issues/18742
        assert_eq!(
            round_trip(
                r#"
f"{1=
}"
"#
            ),
            r#"
f"{1=
}"
"#
            .trim()
        );
    }

    #[test]
    fn implicit_string_concatenation() {
        assert_round_trip!(r#""first" "second" "third""#);
        assert_round_trip!(r#"b"first" b"second" b"third""#);
        assert_round_trip!(r#""first" "second" f"third {var}""#);
    }

    #[test]
    fn indent() {
        assert_eq!(
            round_trip(
                r"
if True:
  pass
"
                .trim(),
            ),
            r"
if True:
    pass
"
            .trim()
            .replace('\n', LineEnding::default().as_str())
        );
    }

    #[test]
    fn set_indent() {
        assert_eq!(
            round_trip_with(
                &Indentation::new("    ".to_string()),
                LineEnding::default(),
                UnparseMode::Default,
                r"
if True:
  pass
"
                .trim(),
            ),
            r"
if True:
    pass
"
            .trim()
            .replace('\n', LineEnding::default().as_str())
        );
        assert_eq!(
            round_trip_with(
                &Indentation::new("  ".to_string()),
                LineEnding::default(),
                UnparseMode::Default,
                r"
if True:
  pass
"
                .trim(),
            ),
            r"
if True:
  pass
"
            .trim()
            .replace('\n', LineEnding::default().as_str())
        );
        assert_eq!(
            round_trip_with(
                &Indentation::new("\t".to_string()),
                LineEnding::default(),
                UnparseMode::Default,
                r"
if True:
  pass
"
                .trim(),
            ),
            r"
if True:
	pass
"
            .trim()
            .replace('\n', LineEnding::default().as_str())
        );
    }

    #[test]
    fn set_line_ending() {
        assert_eq!(
            round_trip_with(
                &Indentation::default(),
                LineEnding::Lf,
                UnparseMode::Default,
                "if True:\n    print(42)",
            ),
            "if True:\n    print(42)",
        );

        assert_eq!(
            round_trip_with(
                &Indentation::default(),
                LineEnding::CrLf,
                UnparseMode::Default,
                "if True:\n    print(42)",
            ),
            "if True:\r\n    print(42)",
        );

        assert_eq!(
            round_trip_with(
                &Indentation::default(),
                LineEnding::Cr,
                UnparseMode::Default,
                "if True:\n    print(42)",
            ),
            "if True:\r    print(42)",
        );
    }

    #[test_case::test_case(r#""'hello'""#, r#""'hello'""# ; "basic str ignored")]
    #[test_case::test_case(r#"b"'hello'""#, r#"b"'hello'""# ; "basic bytes ignored")]
    #[test_case::test_case(r#""hello""#, "'hello'" ; "basic str single")]
    #[test_case::test_case(r#"b"hello""#, "b'hello'" ; "basic bytes single")]
    #[test_case::test_case("'hello'", "'hello'"  ; "remain str single")]
    #[test_case::test_case(r#"x: list["str"]"#, "x: list['str']" ; "type ann single")]
    #[test_case::test_case(r#"f"hello""#, "f'hello'" ; "basic fstring single")]
    fn ast_unparse_quote(inp: &str, out: &str) {
        let got = round_trip_with(
            &Indentation::default(),
            LineEnding::default(),
            UnparseMode::AstUnparse,
            inp,
        );
        assert_eq!(got, out);
    }

    #[test_case::test_case("a,", "(a,)" ; "basic single")]
    #[test_case::test_case("a, b", "(a, b)" ; "basic multi")]
    #[test_case::test_case("x = a,", "x = (a,)" ; "basic assign single")]
    #[test_case::test_case("x = a, b", "x = (a, b)" ; "basic assign multi")]
    #[test_case::test_case("a, (b, c)", "(a, (b, c))" ; "nested")]
    fn ast_tuple_parentheses(inp: &str, out: &str) {
        let got = round_trip_with(
            &Indentation::default(),
            LineEnding::default(),
            UnparseMode::AstUnparse,
            inp,
        );
        assert_eq!(got, out);
    }

    #[test_case::test_case("x: int?" ; "optional bare")]
    #[test_case::test_case("def f() -> int?:\n    return None" ; "optional return ann")]
    #[test_case::test_case("x: list[int?]" ; "optional nested in subscript")]
    #[test_case::test_case("x: int ? TypeError" ; "result")]
    #[test_case::test_case("x: int | str ? KeyError | IndexError" ; "result over unions")]
    #[test_case::test_case("x: dict[str, int]? ? ValueError" ; "result of optional value")]
    #[test_case::test_case("x = foo()^" ; "propagate")]
    #[test_case::test_case("x = load()^.value" ; "propagate then attribute")]
    #[test_case::test_case("x = foo()!" ; "force")]
    #[test_case::test_case("x = (a + b)!" ; "force parenthesises looser operand")]
    #[test_case::test_case("x = a?.b" ; "optional chain still works")]
    #[test_case::test_case("def f(a: int, context b: str):\n    ..." ; "context parameter")]
    #[test_case::test_case("def f(a: int, *, context b: str):\n    ..." ; "keyword-only context parameter")]
    #[test_case::test_case("f = lambda context b: b" ; "lambda context parameter")]
    #[test_case::test_case("f: int.() -> str" ; "receiver callable")]
    #[test_case::test_case("f: list[int].(str, bool) -> None" ; "receiver callable with parameters")]
    fn basedpython_wrapped_round_trip(contents: &str) {
        // `based_round_trip` emits the platform line ending, so normalise the
        // expected value the same way (mirrors the other round-trip tests);
        // otherwise the multi-line cases fail on windows (CRLF vs LF)
        assert_eq!(
            based_round_trip(contents),
            contents.replace('\n', LineEnding::default().as_str())
        );
    }

    /// basedpython: a match type is a clause header plus a suite of `case` blocks, and a
    /// `TypeVarTuple` may carry a bound — both have to survive a re-render, or a pass that
    /// rewrites an enclosing statement would silently drop them.
    ///
    /// A sequence pattern comes back bracketed (`case (A, B)` → `case [A, B]`), which is how
    /// the generator renders every sequence pattern; the two spell the same pattern.
    #[test]
    fn match_type_round_trips() {
        assert_eq!(
            based_round_trip(
                "type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim"
            ),
            "type NDTuple[T, *Shape: int] = match *Shape:
    case []:
        T
    case [Dim, *Rest]:
        (NDTuple[T, *Rest],) * Dim"
                .replace('\n', LineEnding::default().as_str())
        );
        basedpython_wrapped_round_trip(
            "type Pick[T] = match T:
    case int | str:
        bool
    case _:
        T",
        );
        basedpython_wrapped_round_trip("type Plain[T, *Ts: int] = tuple[T, *Ts]");
    }

    /// basedpython: the starred whole-pack bounds, and a keyword-variadic pack's bound, have to
    /// survive a re-render for the same reason the element-wise one does — the generator is what
    /// a pass rewriting an enclosing statement emits, so a bound it drops is dropped silently.
    #[test]
    fn pack_bounds_round_trip() {
        basedpython_wrapped_round_trip("type WholeTuple[*Ts: *(int, str)] = int");
        basedpython_wrapped_round_trip("type Unbounded[*Ts: *tuple[int, ...]] = int");
        basedpython_wrapped_round_trip("type EveryField[**Kwargs: int] = int");
        basedpython_wrapped_round_trip("type WholeShape[**Kwargs: **{'a': int}] = int");
        basedpython_wrapped_round_trip("type Both[*Ts: *(int, str), **Kwargs: **{'a': int}] = int");
        basedpython_wrapped_round_trip("type WithDefault[*Ts: *(int, str) = *tuple[int]] = int");
        // a class carries the same bounds, and the generator keeps the source quote style
        basedpython_wrapped_round_trip(
            "class A[*Ts: *(int, str), **Kwargs: **{\"a\": int}]:\n    ...",
        );
    }

    /// basedpython: `-> asserts x` names a place, not a type, so the generator — which
    /// emits python — renders it as the `None` such a function returns. the formatter is
    /// what preserves the surface form
    #[test]
    fn asserts_return_renders_as_none() {
        for (contents, expected) in [
            (
                "def f(x) -> asserts x:\n    ...",
                "def f(x) -> None:\n    ...",
            ),
            (
                "def f(x) -> asserts not x:\n    ...",
                "def f(x) -> None:\n    ...",
            ),
            (
                "def f(x) -> asserts x is int:\n    ...",
                "def f(x) -> None:\n    ...",
            ),
        ] {
            assert_eq!(
                based_round_trip(contents),
                expected.replace('\n', LineEnding::default().as_str())
            );
        }
    }

    /// infix bitwise-xor must NOT be swallowed by the postfix `^` operator
    #[test]
    fn caret_stays_infix_xor_when_operand_follows() {
        assert_eq!(based_round_trip("x = a ^ b"), "x = a ^ b");
        assert_eq!(based_round_trip("x = a ^ b ^ c"), "x = a ^ b ^ c");
    }

    /// a trailing `!` inside an interpolation is the conversion flag, not the
    /// force-unwrap operator; a parenthesised `(x!)` re-enables force-unwrap
    #[test]
    fn force_unwrap_yields_to_fstring_conversion() {
        assert_eq!(based_round_trip(r#"x = f"{y!r}""#), r#"x = f"{y!r}""#);
        assert_eq!(based_round_trip(r#"x = f"{(y!)}""#), r#"x = f"{(y!)}""#);
    }
}
