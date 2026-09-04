use std::fmt::{Display, Write};

use thin_vec::ThinVec;

use ruff_python_ast::helpers::{is_compound_statement, written_annotation_type};
use ruff_python_ast::name::Name;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::visitor::transformer::{self, Transformer};
use ruff_python_ast::{
    self as ast, AtomicNodeIndex, DecoratorList, ExceptHandler, Expr, ExprContext, IpyEscapeKind,
    Operator, Pattern, PythonVersion, Stmt, Suite, Variance, WithItem,
};
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::error::StarTupleKind;
use crate::parser::expression::{ArgumentsContext, EXPR_SET, ParsedExpr};
use crate::parser::progress::ParserProgress;
use crate::parser::{
    FunctionKind, IpyEscapeContext, Parser, RecoveryContext, RecoveryContextKind, WithItemKind,
    helpers,
};
use crate::token_set::TokenSet;
use crate::{Mode, ParseErrorType, UnsupportedSyntaxErrorKind};

use super::Parenthesized;
use super::expression::{ExpressionContext, starts_statement_expression};
use super::pattern::AllowSequencePattern;

/// Tokens that represent compound statements.
const COMPOUND_STMT_SET: TokenSet = TokenSet::new([
    TokenKind::Match,
    TokenKind::If,
    TokenKind::With,
    TokenKind::While,
    TokenKind::For,
    TokenKind::Try,
    TokenKind::Def,
    TokenKind::Class,
    TokenKind::Async,
    TokenKind::At,
]);

/// Tokens that represent simple statements, but doesn't include expressions.
const SIMPLE_STMT_SET: TokenSet = TokenSet::new([
    TokenKind::Pass,
    TokenKind::Return,
    TokenKind::Break,
    TokenKind::Continue,
    TokenKind::Global,
    TokenKind::Nonlocal,
    TokenKind::Assert,
    TokenKind::Yield,
    TokenKind::Del,
    TokenKind::Raise,
    TokenKind::Import,
    TokenKind::From,
    TokenKind::Type,
    TokenKind::IpyEscapeCommand,
]);

/// Tokens that represent simple statements, including expressions.
const SIMPLE_STMT_WITH_EXPR_SET: TokenSet = SIMPLE_STMT_SET.union(EXPR_SET);

/// Tokens that represents all possible statements, including simple, compound,
/// and expression statements.
const STMTS_SET: TokenSet = SIMPLE_STMT_WITH_EXPR_SET.union(COMPOUND_STMT_SET);

/// Tokens that represent operators that can be used in augmented assignments.
const AUGMENTED_ASSIGN_SET: TokenSet = TokenSet::new([
    TokenKind::PlusEqual,
    TokenKind::MinusEqual,
    TokenKind::StarEqual,
    TokenKind::DoubleStarEqual,
    TokenKind::SlashEqual,
    TokenKind::DoubleSlashEqual,
    TokenKind::PercentEqual,
    TokenKind::AtEqual,
    TokenKind::AmperEqual,
    TokenKind::VbarEqual,
    TokenKind::CircumflexEqual,
    TokenKind::LeftShiftEqual,
    TokenKind::RightShiftEqual,
]);

/// basedpython modifier keywords that may appear (in any order, any count) before
/// `def`/`class`/`let`/`name = ...`. `abstract` is also an introducer for the
/// `abstract a: T` annotation form, which is handled separately.
fn is_modifier_kw(text: &str) -> bool {
    matches!(
        text,
        "final"
            | "abstract"
            | "open"
            | "sealed"
            | "override"
            | "static"
            | "data"
            | "frozen"
            | "export"
            | "public"
            | "private"
            // basedpython: `late var x: T` defers a property's initialisation.
            // the keyword strips like any other modifier prefix; validity (only on
            // `var`, never with an initialiser) is checked where the property is lowered
            | "late"
            // basedpython: `context x = v` declares an implicit-argument candidate for
            // `context` parameters. it is a prefix on a declaration rather than a form of
            // its own, so it composes with the rest of the chain (`context let x: T = v`
            // is a `Final` one) and the declaration under it parses as it would alone. the
            // keyword is recorded on the statement as `StmtAnnAssign::is_context`
            | "context"
    )
}

/// basedpython: records a `context` keyword read off a modifier chain on the declaration
/// the rest of the chain produced.
fn mark_context(stmt: Stmt, has_context: bool) -> Stmt {
    match stmt {
        Stmt::AnnAssign(mut ann) if has_context => {
            ann.is_context = true;
            Stmt::AnnAssign(ann)
        }
        other => other,
    }
}

/// Whether `kind` can be the name a basedpython declaration declares.
///
/// A soft keyword is a keyword only in the position that introduces it, so
/// everywhere else it is an ordinary identifier and may be declared like one:
/// `let type: int` declares a field called `type`, which is what both
/// `socket.SocketType` and `asyncio.TransportSocket` have. Only the token kind
/// differs from a plain name — [`Parser::parse_identifier`] already reads either.
fn declares_a_name(kind: TokenKind) -> bool {
    kind == TokenKind::Name || kind.is_soft_keyword()
}

/// The marker a modifier keyword contributes to the `def` or `class` it
/// precedes, as the id of the synthetic decorator the `modifiers` transform
/// reads. `None` for a keyword [`is_modifier_kw`] admits that modifies no
/// definition on its own — `frozen`, which is a modifier only in the two-word
/// `frozen data`, and `late`, which only prefixes a property's `var`.
fn definition_modifier_marker(kw: &str) -> Option<&'static str> {
    Some(match kw {
        "data" => "data_class",
        "final" => "final",
        "abstract" => "abstract",
        "override" => "override",
        "open" => "open",
        "sealed" => "sealed",
        "static" => "static",
        "export" | "public" => "export",
        "private" => "private",
        _ => return None,
    })
}

/// What a modifier keyword modifies. `final`, `abstract`, `open` and the
/// visibility keywords read on both a `def` and a `class`; the rest pick one,
/// and writing them on the other decides nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModifierTarget {
    Function,
    Class,
    Either,
}

/// The definition kind a modifier marker applies to. Keyed by the marker rather
/// than the surface keyword so it stays in step with
/// [`definition_modifier_marker`], whose output it consumes.
fn definition_modifier_target(marker: &str) -> ModifierTarget {
    match marker {
        "override" | "static" | "classmethod" => ModifierTarget::Function,
        "sealed" | "data_class" | "frozen_data_class" => ModifierTarget::Class,
        _ => ModifierTarget::Either,
    }
}

/// basedpython modifier keywords that may prefix a parameter name — the
/// [`is_modifier_kw`] set plus the binding keywords `let` / `var`. inside an
/// `init(...)` shorthand these mark the parameter for auto-attribute assignment
/// (`self.<name> = <name>`); elsewhere the chain is consumed but inert. which
/// combinations are actually valid for the context is a semantic concern the
/// `init_method` lowering checks, not the parser
fn is_param_modifier_kw(text: &str) -> bool {
    // `context` prefixes a *declaration*, and on a parameter it means something else
    // entirely — the implicit argument — which the parameter parser reads for itself
    // into `Parameter::is_context`. reading it here would swallow the keyword first
    (is_modifier_kw(text) && text != "context") || matches!(text, "let" | "var")
}

/// True when a parameter's modifier prefix (the source between the parameter
/// node start and its name) declares an instance attribute — i.e. it contains
/// the binding keyword `let` or `var`.
fn param_prefix_declares_attribute(prefix: &str) -> bool {
    prefix
        .split_whitespace()
        .any(|word| matches!(word, "let" | "var"))
}

/// True when a parameter's modifier prefix carries the `private` visibility
/// keyword — the synthesised attribute is then name-mangled (`self.__name`).
fn param_prefix_is_private(prefix: &str) -> bool {
    prefix.split_whitespace().any(|word| word == "private")
}

/// The range of the `let` keyword inside a parameter's modifier prefix (the
/// source span from the parameter node start to its name). `None` for a prefix
/// that binds with `var` (or does not bind at all), whose attribute is mutable.
fn param_prefix_let_range(source: &str, prefix: TextRange) -> Option<TextRange> {
    SimpleTokenizer::new(source, prefix)
        .skip_trivia()
        .find(|token| token.kind == SimpleTokenKind::Name && &source[token.range] == "let")
        .map(|token| token.range)
}

/// A zero-width synthetic `self` parameter injected into an `init(...)` whose
/// author omitted it. Its empty range is the marker the `init_method` transform
/// keys on to emit the matching source-level `self`.
fn synth_self_parameter(at: TextSize) -> ast::ParameterWithDefault {
    let range = TextRange::empty(at);
    ast::ParameterWithDefault {
        range,
        parameter: ast::Parameter {
            range,
            name: ast::Identifier {
                id: Name::new_static("self"),
                range,
                node_index: AtomicNodeIndex::NONE,
            },
            pattern: None,
            annotation: None,
            node_index: AtomicNodeIndex::NONE,
            is_context: false,
            is_some: false,
        },
        default: None,
        node_index: AtomicNodeIndex::NONE,
    }
}

/// Builds a synthetic marker [`Decorator`](ast::Decorator) (a `Name` with
/// [`ExprContext::Invalid`]) used to tag a based-enum variant `ClassDef` with
/// its kind. The lowering phase reads the marker; it never appears in output
/// and is hidden from name resolution.
///
/// `range` spans the `case` keyword for the first variant on a line (so the
/// language server can highlight it, mirroring the `enum_def` marker) and is
/// zero-width at the variant name for the rest.
fn synthetic_variant_decorator(marker: &'static str, range: TextRange) -> ast::Decorator {
    ast::Decorator {
        expression: Expr::Name(ast::ExprName {
            id: Name::new_static(marker),
            ctx: ExprContext::Invalid,
            range,
            node_index: AtomicNodeIndex::NONE,
        }),
        range,
        node_index: AtomicNodeIndex::NONE,
    }
}

/// Rewrites bare `field` identifiers inside a property accessor body to
/// `self.<backing>` attribute accesses, so ty sees real backing storage and the
/// lowering emits `self._<name>`. Records whether any `field` was seen — an
/// accessor block that never mentions `field` allocates no backing storage (the
/// property is computed), matching Kotlin's "no backing field" rule.
struct FieldRewriter {
    backing: Name,
    seen: std::cell::Cell<bool>,
}

/// Whether an accessor body is exactly a read of the backing field — the shape an
/// implicit getter has, and the only shape for which the in-class narrow view is
/// sound: `self.<prop>` and `self._<prop>` then denote the same object, so reading
/// the property at the narrower storage type cannot disagree with the runtime.
fn is_pure_field_read(body: &[Stmt], backing: &Name) -> bool {
    let [Stmt::Return(ret)] = body else {
        return false;
    };
    let Some(Expr::Attribute(attr)) = ret.value.as_deref() else {
        return false;
    };
    attr.attr.id == *backing
        && matches!(attr.value.as_ref(), Expr::Name(name) if name.id.as_str() == "self")
}

/// What an in-class access written under a property's public name should actually
/// resolve to.
#[derive(Debug)]
pub(crate) struct PropertyRetarget {
    /// the name the author writes
    pub(crate) public: Name,
    /// a read resolves here: the backing field when the getter only reads it (so
    /// the class sees storage at its own type), otherwise the property itself
    pub(crate) read: Name,
    /// a write always resolves to the property, so a validating setter still runs
    pub(crate) write: Name,
}

/// Retargets in-class accesses written under a property's public name.
///
/// Two things need this. A property whose getter only reads its backing field
/// reads at the *storage* type inside the class (`let a: object` backed by
/// `field = 1` reads as `int`) — the point of stating the two types separately. And
/// a `private` property does not exist under its public name at all: it is `_a`,
/// so `self.a` has to be pointed at it.
///
/// Only the `id` changes — the identifier keeps its original source range, so the
/// formatter (which prints an identifier from its range) still emits what the
/// author wrote.
struct RetargetPropertyAccess<'a> {
    properties: &'a [PropertyRetarget],
    /// the enclosing method's first parameter, i.e. what `self` is called here
    receiver: &'a str,
}

impl ruff_python_ast::visitor::transformer::Transformer for RetargetPropertyAccess<'_> {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        // a nested class has its own `self`; leave its bodies alone
        if matches!(stmt, Stmt::ClassDef(_)) {
            return;
        }
        ruff_python_ast::visitor::transformer::walk_stmt(self, stmt);
    }

    fn visit_expr(&self, expr: &mut Expr) {
        if let Expr::Attribute(attr) = expr
            && matches!(attr.value.as_ref(), Expr::Name(name) if name.id.as_str() == self.receiver)
            && let Some(property) = self
                .properties
                .iter()
                .find(|property| attr.attr.id == property.public)
        {
            attr.attr.id = if attr.ctx == ExprContext::Load {
                property.read.clone()
            } else {
                property.write.clone()
            };
        }
        ruff_python_ast::visitor::transformer::walk_expr(self, expr);
    }
}

/// Applies [`RetargetPropertyAccess`] to every method in a class body.
fn narrow_property_reads(body: &mut [Stmt], properties: &[PropertyRetarget]) {
    use ruff_python_ast::visitor::transformer::Transformer;
    for member in body {
        let Stmt::FunctionDef(func) = member else {
            continue;
        };
        // the receiver is whatever the method called its first parameter; a
        // parameterless function (a staticmethod) has no `self` to narrow through
        let Some(receiver) = func
            .parameters
            .posonlyargs
            .first()
            .or_else(|| func.parameters.args.first())
            .map(|param| param.parameter.name.id.clone())
        else {
            continue;
        };
        let rewriter = RetargetPropertyAccess {
            properties,
            receiver: receiver.as_str(),
        };
        for stmt in &mut func.body {
            rewriter.visit_stmt(stmt);
        }
    }
}

/// A zero-width synthetic parameter for a synthesised accessor signature.
fn synth_property_param(
    name: &str,
    annotation: Option<Expr>,
    at: TextSize,
) -> ast::ParameterWithDefault {
    let range = TextRange::empty(at);
    ast::ParameterWithDefault {
        range,
        parameter: ast::Parameter {
            range,
            name: ast::Identifier {
                id: Name::new(name),
                range,
                node_index: AtomicNodeIndex::NONE,
            },
            pattern: None,
            annotation: annotation.map(Box::new),
            node_index: AtomicNodeIndex::NONE,
            is_context: false,
            is_some: false,
        },
        default: None,
        node_index: AtomicNodeIndex::NONE,
    }
}

/// Wraps synthesised parameters into a zero-width [`ast::Parameters`] node.
fn synth_property_parameters(
    args: Vec<ast::ParameterWithDefault>,
    at: TextSize,
) -> ast::Parameters {
    ast::Parameters {
        range: TextRange::empty(at),
        node_index: AtomicNodeIndex::NONE,
        posonlyargs: std::iter::empty().collect(),
        args: args.into_iter().collect(),
        vararg: None,
        kwonlyargs: std::iter::empty().collect(),
        kwarg: None,
    }
}

/// Builds one synthesised accessor `def`.
fn build_property_fn(
    name: ast::Identifier,
    decorators: Vec<ast::Decorator>,
    parameters: ast::Parameters,
    returns: Option<Expr>,
    body: Vec<Stmt>,
    range: TextRange,
) -> Stmt {
    Stmt::FunctionDef(ast::StmtFunctionDef {
        name,
        type_params: None,
        parameters: Box::new(parameters),
        body: body.into_iter().collect(),
        decorator_list: decorators.into(),
        is_async: false,
        returns: returns.map(Box::new),
        is_trailing_lambda: false,
        is_asserts_return: false,
        raises: None,
        range,
        node_index: AtomicNodeIndex::NONE,
    })
}

/// An attribute access on the implicit `self`, used to reach a property's
/// backing storage (`self._<name>`) from a synthesised accessor body.
fn synth_backing_attr(backing: &Name, ctx: ExprContext, at: TextSize) -> Expr {
    let range = TextRange::empty(at);
    Expr::Attribute(ast::ExprAttribute {
        value: Box::new(Expr::Name(ast::ExprName {
            id: Name::new_static("self"),
            ctx: ExprContext::Load,
            range,
            node_index: AtomicNodeIndex::NONE,
        })),
        attr: ast::Identifier {
            id: backing.clone(),
            range,
            node_index: AtomicNodeIndex::NONE,
        },
        ctx,
        range,
        node_index: AtomicNodeIndex::NONE,
        optional: false,
    })
}

/// The declared type of a property, peeled out of the synthetic `let` / `var`
/// declaration marker: `__let__[T]` / `__modifier_annot__[T]` carry the type in
/// the subscript slice. A `final` or a `private` anywhere in the modifier chain
/// swaps the marker for `__final__` / `__private_annot__`, which carry the type the
/// same way. An untyped declaration (`let x` / `var x = v`, whose marker is a bare
/// `Name`) has no declared type.
fn property_decl_type(annotation: &Expr) -> Option<Expr> {
    if let Expr::Subscript(subscript) = annotation
        && let Expr::Name(marker) = subscript.value.as_ref()
        && matches!(
            marker.id.as_str(),
            "__let__"
                | "__modifier_annot__"
                | "__private_annot__"
                | "__final__"
                | "__classvar_annot__"
        )
    {
        return Some((*subscript.slice).clone());
    }
    None
}

/// Calls `report` for every part of `expr` that cannot be assigned to.
fn walk_invalid_assignment_targets<'a>(expr: &'a Expr, report: &mut dyn FnMut(&'a Expr)) {
    match expr {
        Expr::Starred(ast::ExprStarred { value, .. }) => {
            walk_invalid_assignment_targets(value, report);
        }
        Expr::List(ast::ExprList { elts, .. }) | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
            for expr in elts {
                walk_invalid_assignment_targets(expr, report);
            }
        }
        Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_) => {}
        _ => report(expr),
    }
}

/// Whether `expr` can be assigned to. basedpython reparses a binder that cannot
/// as a destructuring pattern
fn is_assignment_target(expr: &Expr) -> bool {
    let mut assignable = true;
    walk_invalid_assignment_targets(expr, &mut |_| assignable = false);
    assignable
}

/// basedpython: whether this statement parsed as a simple statement but already consumed an
/// indented suite of its own, so no semicolon or newline terminator follows it.
///
/// Three forms do this: a trailing lambda block (`f(2):` and a suite), a match type alias
/// (`type X[...] = match S:` and its `case` blocks), and a `let ... else` destructuring.
fn consumed_own_suite(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::FunctionDef(function) => function.is_trailing_lambda,
        Stmt::TypeAlias(alias) => !alias.cases.is_empty(),
        // a destructuring `let` is a simple statement until it has an `else`
        // block, whose suite it consumes along with the newline that ends it
        Stmt::Let(let_stmt) => !let_stmt.orelse.is_empty(),
        _ => false,
    }
}

impl ruff_python_ast::visitor::transformer::Transformer for FieldRewriter {
    fn visit_expr(&self, expr: &mut Expr) {
        if let Expr::Name(name) = expr
            && name.id.as_str() == "field"
        {
            self.seen.set(true);
            let range = name.range;
            let ctx = name.ctx;
            *expr = Expr::Attribute(ast::ExprAttribute {
                value: Box::new(Expr::Name(ast::ExprName {
                    id: Name::new_static("self"),
                    ctx: ExprContext::Load,
                    range: TextRange::empty(range.start()),
                    node_index: AtomicNodeIndex::NONE,
                })),
                attr: ast::Identifier {
                    id: self.backing.clone(),
                    range,
                    node_index: AtomicNodeIndex::NONE,
                },
                ctx,
                range,
                node_index: AtomicNodeIndex::NONE,
                optional: false,
            });
            return;
        }
        ruff_python_ast::visitor::transformer::walk_expr(self, expr);
    }
}

impl<'src> Parser<'src> {
    /// Returns `true` if the current token is the start of a compound statement.
    pub(super) fn at_compound_stmt(&self) -> bool {
        self.at_ts(COMPOUND_STMT_SET)
    }

    /// Returns `true` if the current token is the start of a simple statement,
    /// including expressions.
    fn at_simple_stmt(&self) -> bool {
        self.at_ts(SIMPLE_STMT_WITH_EXPR_SET) || self.at_soft_keyword()
    }

    /// Returns `true` if the current token is the start of a simple, compound or expression
    /// statement.
    pub(super) fn at_stmt(&self) -> bool {
        self.at_ts(STMTS_SET) || self.at_soft_keyword()
    }

    /// Checks if the parser is currently positioned at the start of a type parameter.
    pub(super) fn at_type_param(&self) -> bool {
        let token = self.current_token_kind();
        matches!(
            token,
            TokenKind::Star | TokenKind::DoubleStar | TokenKind::Name
        ) || token.is_keyword()
            // basedpython: `/` divides a type parameter list the way it divides a value one
            || (self.options.is_basedpython && token == TokenKind::Slash)
    }

    /// Parses a compound or a single simple statement.
    ///
    /// See:
    /// - <https://docs.python.org/3/reference/compound_stmts.html>
    /// - <https://docs.python.org/3/reference/simple_stmts.html>
    pub(super) fn parse_statement(&mut self) -> Stmt {
        let mut stmt = self.parse_statement_impl();
        // basedpython: a compound statement has no value position of its own, so a
        // statement expression written in one — in a `for` iterable, an `if` test —
        // is out of place wherever it stands. the simple statements validate as they
        // parse, against the value each of them has
        if is_compound_statement(&stmt) {
            self.validate_statement_expressions(&mut stmt);
        }
        stmt
    }

    fn parse_statement_impl(&mut self) -> Stmt {
        let start = self.node_start();

        match self.current_token_kind() {
            TokenKind::If => Stmt::If(self.parse_if_statement()),
            TokenKind::For => Stmt::For(self.parse_for_statement(start)),
            TokenKind::While => Stmt::While(self.parse_while_statement()),
            TokenKind::Def => {
                Stmt::FunctionDef(self.parse_function_definition(DecoratorList::new(), start))
            }
            TokenKind::Class => {
                // `class def f(cls):` — classmethod shorthand
                if self.peek() == TokenKind::Def {
                    return self.parse_with_modifier(start, DecoratorList::new());
                }
                // `class a = 1` — class variable declaration
                if declares_a_name(self.peek()) && self.peek2().1 == TokenKind::Equal {
                    return self.parse_class_var_decl(start);
                }
                // `class var x: T` / `class let x: T` — the annotated class
                // variable, the declared-type counterpart of `class x = 1`.
                // `class` is unambiguous here: a class definition always follows
                // its name with `(`, `[` or `:`, so a binding keyword and a name
                // is the declaration
                if self.peek() == TokenKind::Name && declares_a_name(self.peek2().1) {
                    let keyword_range = self.peek_nth(0).1;
                    let keyword = self.src_text(keyword_range).to_owned();
                    if matches!(keyword.as_str(), "let" | "var") {
                        return self.parse_class_var_annot_decl(start, &keyword);
                    }
                    // a class definition always follows its name with `(`, `[` or
                    // `:`, so two names is a declaration whose binding keyword is
                    // not one. say so rather than read `class private var x` as a
                    // class named `private` with a one-line body
                    let name_end = self.peek_nth(1).1.end();
                    self.add_error(
                        ParseErrorType::OtherError(format!(
                            "`{keyword}` is not a binding keyword — a class variable \
                             is `class let` or `class var`"
                        )),
                        TextRange::new(start, name_end),
                    );
                }
                Stmt::ClassDef(self.parse_class_definition(DecoratorList::new(), start))
            }
            // basedpython: `type def F[X]:` is a type function — a compound
            // statement, unlike the `type X = ...` alias handled with the simple
            // statements below
            TokenKind::Type if self.peek() == TokenKind::Def => {
                self.error_if_not_basedpython(
                    "`type def` is a basedpython declaration and is not valid in .py files"
                        .to_string(),
                );
                self.parse_type_def(start)
            }
            TokenKind::Try => Stmt::Try(self.parse_try_statement()),
            TokenKind::With => Stmt::With(self.parse_with_statement(start)),
            TokenKind::At => self.parse_decorators(),
            TokenKind::Async => self.parse_async_statement(),
            token => {
                if token == TokenKind::Match {
                    // Match is considered a soft keyword, so we will treat it as an identifier if
                    // it's followed by an unexpected token.

                    match self.classify_match_token() {
                        MatchTokenKind::Keyword => {
                            return Stmt::Match(self.parse_match_statement());
                        }
                        MatchTokenKind::KeywordOrIdentifier => {
                            if let Some(match_stmt) = self.try_parse_match_statement() {
                                return Stmt::Match(match_stmt);
                            }
                        }
                        MatchTokenKind::Identifier => {}
                    }
                }

                // basedpython: `init(...)` inside a class body is shorthand
                // for `def __init__(...)`. The synthetic `__init_method__`
                // decorator carries the keyword range so the transform can
                // rewrite `init` to `def __init__` and emit self-assignments
                // for any `let` parameters.
                if self.class_body_depth > 0
                    && token == TokenKind::Name
                    && self.peek() == TokenKind::Lpar
                    && self.src_text(self.current_token_range()) == "init"
                {
                    self.error_if_not_basedpython(
                        "`init(...)` method shorthand is not valid in .py files".to_string(),
                    );
                    return Stmt::FunctionDef(self.parse_init_method(start));
                }

                // Handle basedpython modifier keywords and introducer keywords:
                //   - modifier chains (final, abstract, open, override, static, data, enum,
                //     frozen, export, public, private) before def/class/let/assignment
                //   - single-keyword introducers: let, newtype, protocol, abstract a: T
                if token == TokenKind::Name
                    && let Some(mut stmt) = self.try_parse_modifier_or_introducer(start)
                {
                    // basedpython: a class-body `var`/`let` declaration may be
                    // followed by an indented accessor block (`get`/`set`/`field`),
                    // which turns it into a python `@property` with a backing field
                    if self.class_body_depth > 0
                        && self.at(TokenKind::Indent)
                        && self.at_accessor_block_start()
                    {
                        return self.parse_property_accessors(stmt, start);
                    }
                    // a declaration's value is an expression like any other, so a
                    // statement expression standing in it is held to the same rule
                    // as on an assignment — which this path does not go through
                    self.validate_statement_expressions(&mut stmt);
                    return stmt;
                }

                self.parse_single_simple_statement()
            }
        }
    }

    /// If the current token starts a basedpython modifier chain or introducer keyword
    /// (`let`, `newtype`, `protocol`, `abstract a: T`, or any combination of modifiers
    /// before `def`/`class`/`let`/`name = ...`), parse the corresponding statement.
    /// Returns `None` when no modifier/introducer pattern matches and the caller should
    /// fall through to ordinary statement parsing.
    ///
    /// This walks forward via [`Parser::peek_nth`] over consecutive modifier-keyword
    /// `Name` tokens — there is no fixed bound on chain length.
    fn try_parse_modifier_or_introducer(&mut self, start: TextSize) -> Option<Stmt> {
        let kw = self.src_text(self.current_token_range());

        // single-keyword introducers without modifier-chain support
        if kw == "newtype"
            && self.peek() == TokenKind::Name
            && self.peek_nth(1).0 == TokenKind::Equal
        {
            self.error_if_not_basedpython(
                "`newtype` declarations are not valid in .py files".to_string(),
            );
            return Some(self.parse_newtype_decl(start));
        }
        if kw == "protocol" && self.peek() == TokenKind::Name {
            self.error_if_not_basedpython(
                "`protocol` class syntax is not valid in .py files".to_string(),
            );
            return Some(self.parse_protocol_def(start, DecoratorList::new()));
        }
        if kw == "extension" && self.peek() == TokenKind::Name {
            self.error_if_not_basedpython(
                "`extension` declarations are not valid in .py files".to_string(),
            );
            return Some(self.parse_extension_def(start));
        }
        // `build:` — the values the build stamps into the program. the colon has
        // to be followed by a newline: `build: int` is an annotated assignment of
        // an ordinary name, and only a bare `build:` opening a block is unclaimed
        // syntax today
        if kw == "build"
            && self.peek() == TokenKind::Colon
            && self.peek_nth(1).0 == TokenKind::Newline
        {
            self.error_if_not_basedpython(
                "`build` declarations are not valid in .py files".to_string(),
            );
            return Some(self.parse_build_def(start));
        }
        if kw == "implements" && self.peek() == TokenKind::Name {
            self.error_if_not_basedpython(
                "`implements` declarations are not valid in .py files".to_string(),
            );
            return Some(self.parse_implements_decl(start));
        }
        // `enum class E:` / `enum class E[T]:` — a "based enum" (an algebraic
        // sum type when its body has payload variants, an idiomatic `Enum` when
        // its variants are all unit). the `class` keyword is part of the
        // declaration
        if kw == "enum" && self.peek() == TokenKind::Class && self.peek_nth(1).0 == TokenKind::Name
        {
            self.error_if_not_basedpython(
                "`enum class` declarations are not valid in .py files".to_string(),
            );
            return Some(self.parse_enum_def(start, DecoratorList::new()));
        }
        // a bare `enum E:` (no `class`) is not valid — based enums are written
        // `enum class E:`. report it but still parse the body for recovery
        if kw == "enum" && self.peek() == TokenKind::Name {
            self.add_error(
                ParseErrorType::OtherError(
                    "based enums must be written `enum class E:`, not `enum E:`".to_string(),
                ),
                self.current_token_range(),
            );
            return Some(self.parse_enum_def(start, DecoratorList::new()));
        }
        if kw == "decorator" && self.peek() == TokenKind::Def {
            self.error_if_not_basedpython(
                "`decorator def` syntax is not valid in .py files".to_string(),
            );
            return Some(self.parse_decorator_def(start));
        }

        // `sentinel NAME` → lowered to `NAME = Sentinel("NAME")`
        if kw == "sentinel"
            && declares_a_name(self.peek())
            && matches!(
                self.peek_nth(1).0,
                TokenKind::Newline | TokenKind::Semi | TokenKind::EndOfFile
            )
        {
            self.error_if_not_basedpython(
                "`sentinel` declarations are not valid in .py files".to_string(),
            );
            return Some(self.parse_sentinel_decl(start));
        }

        // The current token must itself be a modifier or an introducer
        // (`let` / `var` and bare `abstract a: T`) for a chain or introducer to be possible.
        if !is_modifier_kw(kw) && !matches!(kw, "let" | "var") {
            return None;
        }

        // Walk forward over consecutive modifier-keyword Name tokens.
        // `idx` counts how many tokens (current + lookahead) we have classified as
        // modifiers; index 0 is the current token, index >=1 uses peek_nth(idx - 1).
        let mut idx: usize = 0;
        // basedpython: `context` anywhere in the chain marks the declaration the rest of
        // the chain produces; it decides nothing about which form that is
        let mut has_context = false;
        loop {
            let (kind, range) = if idx == 0 {
                (self.current_token_kind(), self.current_token_range())
            } else {
                self.peek_nth(idx - 1)
            };

            match kind {
                TokenKind::Def | TokenKind::Class | TokenKind::Async => {
                    // chain of `idx` modifiers followed by def / async def / class
                    if idx == 0 {
                        return None;
                    }
                    self.error_if_not_basedpython(format!(
                        "`{kw}` is a basedpython modifier and is not valid in .py files"
                    ));
                    return Some(self.parse_with_modifier(start, DecoratorList::new()));
                }
                // `private type X = V`. only `private` is accepted: a type alias
                // is public by default, so the remaining modifiers would be
                // inert, and accepting-then-dropping them would lose data on a
                // format round-trip.
                //
                // `type` is a soft keyword, so the alias shape (`NAME [` or
                // `NAME =`) is what tells this apart from `type` used as an
                // ordinary name — which is why a `type` that fails the guard
                // falls through to the name arm below rather than bailing out
                TokenKind::Type
                    if idx == 1
                        && kw == "private"
                        && declares_a_name(self.peek_nth(idx).0)
                        && matches!(
                            self.peek_nth(idx + 1).0,
                            TokenKind::Lsqb | TokenKind::Equal
                        ) =>
                {
                    self.error_if_not_basedpython(
                        "`private` type aliases are not valid in .py files".to_string(),
                    );
                    self.bump(TokenKind::Name);
                    let mut alias = self.parse_type_alias_statement();
                    alias.is_private = true;
                    alias.range = self.node_range(start);
                    // a type alias is a *simple* statement, so unlike the
                    // compound `parse_with_modifier` path it must consume its
                    // own terminator here — the caller only does that for the
                    // fallback path. a match type alias is the exception: its
                    // `case` blocks already consumed the newline and the suite
                    if alias.cases.is_empty() {
                        self.eat(TokenKind::Semi);
                        self.eat(TokenKind::Newline);
                    }
                    return Some(Stmt::TypeAlias(alias));
                }
                // a soft keyword reaching here is not introducing its own
                // construct, so it is an ordinary name and is read as one
                kind if declares_a_name(kind) => {
                    let text = self.src_text(range);
                    if text == "let" {
                        // `let` only introduces a declaration when it is shaped
                        // like `let NAME =`, `let NAME : ...`, or a bare
                        // `let NAME` (an uninitialized declaration). otherwise it
                        // is an ordinary identifier (`let = 5`, `let(x)`,
                        // `print(let)` are valid python), so don't hijack it —
                        // and don't let a tool that parses arbitrary text (e.g.
                        // ERA001 on the comment `# the OS will let us`) panic the
                        // parser
                        if !declares_a_name(self.peek_nth(idx).0)
                            || !matches!(
                                self.peek_nth(idx + 1).0,
                                TokenKind::Colon
                                    | TokenKind::Equal
                                    | TokenKind::Newline
                                    | TokenKind::Semi
                                    | TokenKind::EndOfFile
                            )
                        {
                            return None;
                        }
                        // [modifiers] let name [: T] = value
                        self.error_if_not_basedpython(
                            "`let` declarations are not valid in .py files".to_string(),
                        );
                        for _ in 0..idx {
                            self.bump(TokenKind::Name);
                        }
                        let decl = self.parse_let_decl(start);
                        return Some(mark_context(decl, has_context));
                    }
                    if text == "var" {
                        // `var` is the mutable counterpart of `let`. it declares
                        // nothing beyond what the surface already says, so it
                        // lowers through the modifier-assignment path: the
                        // keyword is stripped and `NAME [: T] = value` is left
                        // behind. shape-gated exactly like `let` so an ordinary
                        // identifier named `var` is never hijacked
                        let following = self.peek_nth(idx + 1).0;
                        if !declares_a_name(self.peek_nth(idx).0)
                            || !matches!(
                                following,
                                TokenKind::Colon
                                    | TokenKind::Equal
                                    | TokenKind::Newline
                                    | TokenKind::Semi
                                    | TokenKind::EndOfFile
                            )
                        {
                            return None;
                        }
                        self.error_if_not_basedpython(
                            "`var` declarations are not valid in .py files".to_string(),
                        );
                        if following == TokenKind::Colon {
                            let decl = self.parse_modifier_annot_decl(start, "__modifier_annot__");
                            return Some(mark_context(decl, has_context));
                        }
                        if following != TokenKind::Equal {
                            // a bare `var x` carries neither a type nor a value,
                            // so there is nothing for it to declare — unlike
                            // `let x`, which declares an uninitialized `Final`.
                            // parse it anyway so the rest of the file still
                            // parses; the error blocks any output
                            self.add_error(
                                ParseErrorType::OtherError(
                                    "`var` declaration requires a type or an initializer"
                                        .to_string(),
                                ),
                                range,
                            );
                        }
                        let decl = self.parse_modifier_assign_decl(start);
                        return Some(mark_context(decl, has_context));
                    }
                    // a modifier keyword directly in front of `=` or `:` is the name
                    // the declaration declares, not another modifier: `context data = 1`
                    // declares `data`, and nothing can follow a modifier chain there.
                    // reading it as a modifier would leave the chain with no name at all
                    let names_this_declaration =
                        matches!(self.peek_nth(idx).0, TokenKind::Equal | TokenKind::Colon);
                    if is_modifier_kw(text) && !names_this_declaration {
                        has_context |= text == "context";
                        idx += 1;
                        continue;
                    }
                    // non-modifier Name token. could be a variable name in
                    // `[modifiers] name = value` or `abstract name : T`.
                    let following = if idx == 0 {
                        self.peek()
                    } else {
                        self.peek_nth(idx).0
                    };
                    // an introducer keyword (`enum class`, `protocol`) after a
                    // modifier chain — `private enum class E:`, `export protocol P:`.
                    // dispatch through the modifier path so the chain is carried
                    // as decorators on the introduced class
                    if (text == "protocol" && following == TokenKind::Name)
                        || (text == "enum" && following == TokenKind::Class)
                    {
                        self.error_if_not_basedpython(format!(
                            "`{kw}` is a basedpython modifier and is not valid in .py files"
                        ));
                        return Some(self.parse_with_modifier(start, DecoratorList::new()));
                    }
                    return match following {
                        TokenKind::Equal if idx > 0 => {
                            self.error_if_not_basedpython(format!(
                                "`{kw}` modifier on assignments is not valid in .py files"
                            ));
                            let decl = self.parse_modifier_assign_decl(start);
                            Some(mark_context(decl, has_context))
                        }
                        TokenKind::Colon if idx == 1 && self.is_abstract_modifier_at(0) => {
                            // bare `abstract a: T` — sole `abstract` modifier ahead of name
                            self.error_if_not_basedpython(
                                "`abstract` annotations are not valid in .py files".to_string(),
                            );
                            let decl = self.parse_abstract_annot_decl(start);
                            Some(mark_context(decl, has_context))
                        }
                        TokenKind::Colon if idx == 1 && self.is_visibility_modifier_at(0) => {
                            // bare `private a: T`, `public a: T`, or `export a: T`
                            self.error_if_not_basedpython(format!(
                                "`{kw}` annotations are not valid in .py files"
                            ));
                            let decl = self.parse_visibility_annot_decl(start);
                            Some(mark_context(decl, has_context))
                        }
                        TokenKind::Colon if idx == 1 && self.is_final_modifier_at(0) => {
                            // bare `final a: T` — a `Final` declaration in every
                            // scope. unlike the generic modifier-annot path, this
                            // keeps the real type in the AST (as `__final__[T]`) so
                            // ty resolves it without a transpile step
                            self.error_if_not_basedpython(
                                "`final` annotations are not valid in .py files".to_string(),
                            );
                            let decl = self.parse_final_annot_decl(start);
                            Some(mark_context(decl, has_context))
                        }
                        TokenKind::Colon if idx >= 1 => {
                            // any other modifier chain on an annotated assignment
                            // (`override x: T`, `final override x: T`, …) — strip
                            // the prefix, keep `x: T [= v]`. mirrors the
                            // `[modifiers] name = value` form so the two are
                            // symmetric rather than annotated-only being rejected
                            self.error_if_not_basedpython(format!(
                                "`{kw}` modifier on annotated assignments is not valid in .py files"
                            ));
                            let decl = self.parse_modifier_annot_decl(start, "__modifier_annot__");
                            Some(mark_context(decl, has_context))
                        }
                        _ => None,
                    };
                }
                _ => return None,
            }
        }
    }

    /// Consumes the two adjacent `.` tokens of a bound range, returning their combined range.
    ///
    /// The caller must have checked [`Parser::at_adjacent_double_dot`].
    fn eat_double_dot(&mut self) -> TextRange {
        let first = self.current_token_range();
        self.bump(TokenKind::Dot);
        let second = self.current_token_range();
        self.bump(TokenKind::Dot);
        TextRange::new(first.start(), second.end())
    }

    /// Emits a parse error at the current token range if the parser is not in
    /// basedpython mode. Used to gate basedpython-only syntax in `.py` files.
    pub(super) fn error_if_not_basedpython(&mut self, message: String) {
        let range = self.current_token_range();
        self.error_if_not_basedpython_at(message, range);
    }

    /// Like [`Parser::error_if_not_basedpython`], but reports at `range` rather than at the
    /// current token.
    pub(super) fn error_if_not_basedpython_at(&mut self, message: String, range: TextRange) {
        if !self.options.is_basedpython {
            self.add_error(ParseErrorType::BasedPythonOnly(message), range);
        }
    }

    /// Returns whether the modifier-keyword token at chain position `idx` is `final`.
    fn is_final_modifier_at(&mut self, idx: usize) -> bool {
        let range = if idx == 0 {
            self.current_token_range()
        } else {
            self.peek_nth(idx - 1).1
        };
        self.src_text(range) == "final"
    }

    /// Returns whether the modifier-keyword token at chain position `idx` is `abstract`.
    /// Position 0 is the current token; positions >=1 use [`Parser::peek_nth`].
    fn is_abstract_modifier_at(&mut self, idx: usize) -> bool {
        let range = if idx == 0 {
            self.current_token_range()
        } else {
            self.peek_nth(idx - 1).1
        };
        self.src_text(range) == "abstract"
    }

    /// Returns whether the modifier-keyword token at chain position `idx` is a
    /// visibility keyword (`private`, `public`, or `export`).
    fn is_visibility_modifier_at(&mut self, idx: usize) -> bool {
        let range = if idx == 0 {
            self.current_token_range()
        } else {
            self.peek_nth(idx - 1).1
        };
        matches!(self.src_text(range), "private" | "public" | "export")
    }

    /// Parses a basedpython modifier keyword statement such as `final class Foo:`,
    /// `static def foo():`, or `class def f(cls):`.
    ///
    /// The modifier keyword(s) are consumed and a synthetic `Decorator` is constructed
    /// pointing at the modifier text in the source. The downstream `modifiers` transform
    /// uses the decorator to emit the appropriate `@decorator` line.
    /// Parse a basedpython modifier chain (`class def`, `static def`, `final
    /// class`, …) into a function/class def. `decorators` holds any real
    /// `@`-decorators already parsed before the modifier (e.g. the `@overload`
    /// in `@overload class def open(...)`); the synthetic modifier decorators are
    /// appended after them.
    fn parse_with_modifier(&mut self, start: TextSize, mut decorators: DecoratorList) -> Stmt {
        loop {
            let modifier_start = self.current_token_range().start();

            // Determine the logical modifier name from the keyword text(s).
            // The synthetic decorator range covers exactly the modifier text(s) + trailing
            // whitespace up to (but not including) the class/def keyword. The transform
            // uses this to replace the modifier prefix with `@decorator\n{indent}`.
            let modifier_name: &'static str = if self.at(TokenKind::Class) {
                // `class def f(cls):` → @classmethod
                self.bump(TokenKind::Class);
                "classmethod"
            } else {
                let kw = self.src_text(self.current_token_range()).to_owned();
                self.bump(TokenKind::Name);
                if kw == "frozen" {
                    // the one two-word modifier. `frozen` qualifies `data`, so it
                    // only consumes a second keyword when that keyword is there —
                    // consuming whatever follows swallowed a neighbouring modifier
                    // and ran off the end of a chain that had none
                    if self.at(TokenKind::Name)
                        && self.src_text(self.current_token_range()) == "data"
                    {
                        self.bump(TokenKind::Name);
                        "frozen_data_class"
                    } else {
                        self.add_error(
                            ParseErrorType::OtherError(
                                "`frozen` qualifies `data class`; write `frozen data class`"
                                    .to_string(),
                            ),
                            TextRange::new(modifier_start, self.current_token_range().start()),
                        );
                        ruff_python_ast::helpers::INVALID_MODIFIER_MARKER
                    }
                } else if let Some(marker) = definition_modifier_marker(&kw) {
                    marker
                } else {
                    self.add_error(
                        ParseErrorType::OtherError(format!(
                            "`{kw}` is not a modifier on a `def` or a `class`"
                        )),
                        TextRange::new(modifier_start, self.current_token_range().start()),
                    );
                    ruff_python_ast::helpers::INVALID_MODIFIER_MARKER
                }
            };

            let modifier_end = self.current_token_range().start();
            let decorator_range = TextRange::new(modifier_start, modifier_end);

            decorators.push(ast::Decorator {
                expression: Expr::Name(ast::ExprName {
                    id: Name::new_static(modifier_name),
                    // synthetic decorator: the surface keyword is *not* a
                    // reference to a runtime name, so don't expose it to
                    // name-resolution passes (pyflakes F821 in particular)
                    ctx: ExprContext::Invalid,
                    range: decorator_range,
                    node_index: AtomicNodeIndex::NONE,
                }),
                range: decorator_range,
                node_index: AtomicNodeIndex::NONE,
            });

            // stop when we reach the def / async def being modified
            if self.at(TokenKind::Def) || self.at(TokenKind::Async) {
                break;
            }
            // a `class` token is ambiguous: `class def f` is the classmethod
            // modifier (keep looping so the next iteration consumes it as such),
            // while `class Foo` is the class being modified (end the chain)
            if self.at(TokenKind::Class) {
                if matches!(self.peek(), TokenKind::Def | TokenKind::Async) {
                    continue;
                }
                // `class var x: T` declares a class *variable*, which is not a
                // definition a modifier chain can hang off — and it carries one
                // marker, which its own keyword already fills. left alone it
                // parses on as a nested class named by the binding keyword
                let binding = self.peek_nth(0).1;
                let (named, name_range) = self.peek_nth(1);
                if matches!(self.src_text(binding), "let" | "var") && declares_a_name(named) {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "a `class` variable declaration takes no modifier — write it on its own"
                                .to_string(),
                        ),
                        TextRange::new(start, name_range.end()),
                    );
                }
                break;
            }
            // another modifier keyword follows — keep looping. an `enum class` /
            // `protocol` introducer also ends the chain (handled after the loop)
            if self.at(TokenKind::Name) {
                let kw = self.src_text(self.current_token_range());
                // any modifier keyword may follow another, in any order — reuse
                // the canonical set so none is accidentally omitted (a missing
                // `final` here used to drop the chain into `parse_class_definition`
                // with the modifier still current, panicking on `bump(Class)`)
                if is_modifier_kw(kw) {
                    continue;
                }
            }
            break;
        }

        // the chain is complete, so the definition it modifies is now known: a
        // modifier that reads on the other kind decides nothing here, and was
        // being carried into a lowering that had no arm for it and left the
        // keyword in the emitted python
        let target = if self.at(TokenKind::Async) || self.at(TokenKind::Def) {
            ModifierTarget::Function
        } else {
            ModifierTarget::Class
        };
        let misplaced: Vec<TextRange> = decorators
            .iter()
            .filter(|dec| match &dec.expression {
                Expr::Name(name) => {
                    name.ctx == ExprContext::Invalid
                        && definition_modifier_target(name.id.as_str()) != ModifierTarget::Either
                        && definition_modifier_target(name.id.as_str()) != target
                }
                _ => false,
            })
            .map(Ranged::range)
            .collect();
        for range in misplaced {
            let kw = self.src_text(range).trim_end().to_owned();
            let modified = match target {
                ModifierTarget::Function => "a `def`",
                _ => "a `class`",
            };
            self.add_error(
                ParseErrorType::OtherError(format!("`{kw}` is not a modifier on {modified}")),
                range,
            );
        }

        if self.at(TokenKind::Async) {
            // `abstract async def`, `final async def`, … — the modifier applies
            // to an async function
            self.bump(TokenKind::Async);
            Stmt::FunctionDef(ast::StmtFunctionDef {
                is_async: true,
                ..self.parse_function_definition(decorators, start)
            })
        } else if self.at(TokenKind::Def) {
            Stmt::FunctionDef(self.parse_function_definition(decorators, start))
        } else if self.at(TokenKind::Name)
            && self.src_text(self.current_token_range()) == "protocol"
            && self.peek() == TokenKind::Name
        {
            // `private protocol P:` — the modifier decorators ride alongside the
            // synthetic `protocol_class` marker; both lower by disjoint edits
            self.parse_protocol_def(start, decorators)
        } else if self.at(TokenKind::Name)
            && self.src_text(self.current_token_range()) == "enum"
            && self.peek() == TokenKind::Class
        {
            // `private enum class E:` — the enum lowering reads the visibility
            // markers to prefix its synthesized `class` line
            self.parse_enum_def(start, decorators)
        } else {
            Stmt::ClassDef(self.parse_class_definition(decorators, start))
        }
    }

    /// Parses `class a = 1` → produces a synthetic `AnnAssign` that the
    /// `modifiers` transform rewrites to `a: ClassVar = 1`.
    fn parse_class_var_decl(&mut self, start: TextSize) -> Stmt {
        // consume "class"
        self.bump(TokenKind::Class);
        let name = self.parse_identifier();
        self.bump(TokenKind::Equal);
        let value = self.parse_declaration_value();
        // Synthetic annotation pointing at the "class" keyword text in the source
        // so the transform can identify this form.
        let class_range = TextRange::new(start, name.range.start());
        let target = Expr::Name(ast::ExprName {
            id: name.id.clone(),
            ctx: ExprContext::Store,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        let annotation = Expr::Name(ast::ExprName {
            id: Name::new_static("__classvar__"),
            ctx: ExprContext::Invalid,
            range: class_range,
            node_index: AtomicNodeIndex::NONE,
        });
        self.eat_declaration_terminator();
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value: Some(Box::new(value)),
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    /// basedpython: parses the initializer of a declaration form — the value in
    /// `let a = value`, `var a: T = value`, `class a = value` — as the same
    /// expression an ordinary assignment's right-hand side is, so a trailing
    /// lambda block can stand there too:
    ///
    /// ```text
    /// let a = f:
    ///     print(it)
    /// ```
    ///
    /// A declaration binds exactly one name, which is what a block's value
    /// requires, so unlike the assignment path there is no target shape to reject.
    fn parse_declaration_value(&mut self) -> Expr {
        let value = self.parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or());
        self.parse_trailing_lambda_value(value).expr
    }

    /// basedpython: parses `class var x: T [= v]` and `class let x: T [= v]` —
    /// the class variable whose type is declared rather than inferred from a
    /// value, which `class x = 1` cannot express.
    ///
    /// A `var` is an ordinary `ClassVar`; a `let` is read-only, which python
    /// spells `Final` in a class body. A read-only one written without a value
    /// has nowhere to be bound — `__init__` binds an instance — which is what
    /// ty's `final-without-value` says, in the one place that knows a stub
    /// declares types and never values.
    fn parse_class_var_annot_decl(&mut self, start: TextSize, keyword: &str) -> Stmt {
        self.error_if_not_basedpython(
            "a `class` variable declaration is not valid in .py files".to_string(),
        );
        self.bump(TokenKind::Class);
        // the declaration carries one marker, and `class var` already fills it.
        // another modifier in the chain would take the slot and the declaration
        // would silently stop being a class variable — reject it instead. the
        // name is the only thing that may follow the binding keyword, so a token
        // other than the `:` after it is a keyword that has nowhere to go
        if self.peek2().1 != TokenKind::Colon {
            let extra = self.peek_nth(0).1;
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "`class {keyword}` takes no other modifier — `{}` has nowhere to go",
                    self.src_text(extra)
                )),
                TextRange::new(start, extra.end()),
            );
        }
        let marker = if keyword == "let" {
            "__final__"
        } else {
            "__classvar_annot__"
        };
        let stmt = self.parse_modifier_annot_decl(start, marker);

        // an accessor block turns the declaration into a property, which is a
        // member of the *instance* — `class` is the class-variable modifier, and
        // the class-level property is `static`. carry on into the block anyway,
        // so the accessors are parsed rather than cascading
        if self.class_body_depth > 0 && self.at(TokenKind::Indent) && self.at_accessor_block_start()
        {
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "`class {keyword}` is not a property declaration; write `static {keyword}`"
                )),
                stmt.range(),
            );
            return self.parse_property_accessors(stmt, start);
        }

        if self.class_body_depth == 0 {
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "`class {keyword}` declares a class variable, so it belongs in a class body"
                )),
                stmt.range(),
            );
        }
        stmt
    }

    /// basedpython: consumes the `;` / newline that ends a declaration form.
    ///
    /// These forms are parsed outside [`Parser::parse_single_simple_statement`],
    /// so each terminates itself. When the value carried a suite — a statement
    /// expression such as `let a = match x:`, or a trailing lambda block — that
    /// suite has already consumed this statement's newline, and the flag saying so
    /// is cleared here rather than left to make the *next* statement skip its own
    /// terminator.
    fn eat_declaration_terminator(&mut self) {
        if std::mem::take(&mut self.expr_consumed_suite) {
            return;
        }
        self.eat(TokenKind::Semi);
        self.eat(TokenKind::Newline);
    }

    /// Parses `let x = 5` → produces a synthetic `AnnAssign` that the
    /// `modifiers` transform rewrites to `x: Final = 5`.
    fn parse_let_decl(&mut self, start: TextSize) -> Stmt {
        self.bump(TokenKind::Name); // consume "let"
        let name = self.parse_identifier();
        // the marker spans the whole keyword prefix, from the statement's start — a
        // modifier ahead of the `let` (`context let a: T`, `private let a = v`) is part of
        // what was written, and everything that re-emits or highlights the declaration
        // reads it from this range
        let let_range = TextRange::new(start, name.range.start());
        let let_name = Expr::Name(ast::ExprName {
            id: Name::new_static("__let__"),
            ctx: ExprContext::Invalid,
            range: let_range,
            node_index: AtomicNodeIndex::NONE,
        });
        // optional `: annotation` before `=`
        let typed = self.eat(TokenKind::Colon);
        let annotation = if typed {
            let type_ann = self
                .parse_conditional_expression_or_higher_impl(
                    ExpressionContext::default().with_in_type_expression(),
                )
                .expr;
            let slice_range = type_ann.range();
            Expr::Subscript(ast::ExprSubscript {
                value: Box::new(let_name),
                slice: Box::new(type_ann),
                ctx: ExprContext::Load,
                range: TextRange::new(let_range.start(), slice_range.end()),
                node_index: AtomicNodeIndex::NONE,
                is_typeof: false,
                is_type_decoration: false,
            })
        } else {
            let_name
        };
        // the initializer may be omitted: a typed `let NAME: T` declares a
        // read-only attribute (lowers to `NAME: Final[T]`) and a bare untyped
        // `let NAME` declares an uninitialized `Final`. otherwise consume the
        // `= value`
        let value = self
            .eat(TokenKind::Equal)
            .then(|| Box::new(self.parse_declaration_value()));
        let target = Expr::Name(ast::ExprName {
            id: name.id.clone(),
            ctx: ExprContext::Store,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        self.eat_declaration_terminator();
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value,
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    /// Parses `final x = 5` → produces a synthetic `AnnAssign` that the
    /// `modifiers` transform rewrites to `x: Final = 5` at module scope or `x = 5` inside a class.
    /// Parses a basedpython modifier-chain assignment such as `final a = 1`,
    /// `override a = 1`, `final override a = 1`, or the `var a = 1` declaration —
    /// any non-empty sequence of modifier keywords (see [`is_modifier_kw`])
    /// followed by `name = value`.
    ///
    /// Produces a synthetic [`AnnAssign`] whose annotation is a `Name` with id
    /// `"__modifier_assign__"` and a range covering the modifier prefix in the
    /// source. The downstream `modifiers` transform reads the modifier names
    /// directly from that source range — no per-combination sentinel needed.
    ///
    /// Caller must position the parser at the first modifier keyword in the chain.
    ///
    /// [`AnnAssign`]: ast::StmtAnnAssign
    fn parse_modifier_assign_decl(&mut self, start: TextSize) -> Stmt {
        let modifier_start = self.current_token_range().start();
        // consume modifier keywords until we reach the variable name (the Name
        // token immediately followed by `=`, or by the end of the statement for
        // the initializer-less `var x` the caller has already rejected).
        loop {
            let (next_kind, _) = self.peek_nth(0);
            if matches!(
                next_kind,
                TokenKind::Equal | TokenKind::Newline | TokenKind::Semi | TokenKind::EndOfFile
            ) {
                break;
            }
            self.bump(TokenKind::Name);
        }
        let name = self.parse_identifier();
        let value = self
            .eat(TokenKind::Equal)
            .then(|| self.parse_declaration_value());
        let target = Expr::Name(ast::ExprName {
            id: name.id.clone(),
            ctx: ExprContext::Store,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        let annotation = Expr::Name(ast::ExprName {
            id: Name::new_static("__modifier_assign__"),
            ctx: ExprContext::Invalid,
            range: TextRange::new(modifier_start, name.range.start()),
            node_index: AtomicNodeIndex::NONE,
        });
        self.eat_declaration_terminator();
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value: value.map(Box::new),
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    /// Parses `newtype Foo = int` → produces a synthetic `AnnAssign` that the
    /// `modifiers` transform rewrites to `Foo = NewType("Foo", int)`.
    fn parse_newtype_decl(&mut self, start: TextSize) -> Stmt {
        let newtype_range = self.current_token_range();
        self.bump(TokenKind::Name); // consume "newtype"
        let name = self.parse_identifier();
        self.bump(TokenKind::Equal);
        let value = self
            .parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or())
            .expr;
        let target = Expr::Name(ast::ExprName {
            id: name.id.clone(),
            ctx: ExprContext::Store,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        let annotation = Expr::Name(ast::ExprName {
            id: Name::new_static("__newtype__"),
            ctx: ExprContext::Invalid,
            range: newtype_range,
            node_index: AtomicNodeIndex::NONE,
        });
        self.eat_declaration_terminator();
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value: Some(Box::new(value)),
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    /// Parses `sentinel A` → produces a synthetic
    /// `AnnAssign { target: A, annotation: __sentinel__, value: None }`
    /// that the `sentinel` transform rewrites to `A = Sentinel("A")`.
    fn parse_sentinel_decl(&mut self, start: TextSize) -> Stmt {
        let kw_range = self.current_token_range();
        self.bump(TokenKind::Name); // consume "sentinel"
        let name = self.parse_identifier();
        let target = Expr::Name(ast::ExprName {
            id: name.id.clone(),
            ctx: ExprContext::Store,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        let annotation = Expr::Name(ast::ExprName {
            id: Name::new_static("__sentinel__"),
            ctx: ExprContext::Invalid,
            range: kw_range,
            node_index: AtomicNodeIndex::NONE,
        });
        self.eat_declaration_terminator();
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value: None,
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    /// Parses `abstract a: int` → produces a synthetic `AnnAssign` that the
    /// `modifiers` transform rewrites to `a: int` (strips the `abstract` prefix).
    fn parse_abstract_annot_decl(&mut self, start: TextSize) -> Stmt {
        self.parse_modifier_annot_decl(start, "__abstract_annot__")
    }

    /// Parses `final NAME: T [= v]` into an `AnnAssign` whose annotation is the
    /// synthetic `Subscript(Name("__final__"), T)`, so ty resolves `T` and
    /// applies `Final` while the forward transform recovers `NAME: Final[T]`.
    /// Mirrors [`Parser::parse_let_decl`]; `final` is always typed (an untyped
    /// `final x = v` is a `__modifier_assign__` instead).
    fn parse_final_annot_decl(&mut self, start: TextSize) -> Stmt {
        let final_range = self.current_token_range();
        self.bump(TokenKind::Name); // consume "final"
        let name = self.parse_identifier();
        let final_marker = Expr::Name(ast::ExprName {
            id: Name::new_static("__final__"),
            ctx: ExprContext::Invalid,
            range: final_range,
            node_index: AtomicNodeIndex::NONE,
        });
        self.bump(TokenKind::Colon); // consume ":"
        let type_ann = self
            .parse_conditional_expression_or_higher_impl(
                ExpressionContext::default().with_in_type_expression(),
            )
            .expr;
        let slice_range = type_ann.range();
        let annotation = Expr::Subscript(ast::ExprSubscript {
            value: Box::new(final_marker),
            slice: Box::new(type_ann),
            ctx: ExprContext::Load,
            range: TextRange::new(final_range.start(), slice_range.end()),
            node_index: AtomicNodeIndex::NONE,
            is_typeof: false,
            is_type_decoration: false,
        });
        let value = self
            .eat(TokenKind::Equal)
            .then(|| Box::new(self.parse_declaration_value()));
        let target = Expr::Name(ast::ExprName {
            id: name.id.clone(),
            ctx: ExprContext::Store,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        self.eat_declaration_terminator();
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value,
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    fn parse_visibility_annot_decl(&mut self, start: TextSize) -> Stmt {
        self.parse_modifier_annot_decl(start, "__visibility_annot__")
    }

    /// Parses `<modifier> name: T [= v]` and emits an `AnnAssign` whose
    /// annotation is `Subscript(Name(synthetic_id), T)` — a synthetic marker
    /// spanning the modifier prefix, keeping the declared type `T` in annotation
    /// position. The downstream transform deletes that prefix from the source
    /// text, leaving `name: T [= v]` behind.
    fn parse_modifier_annot_decl(&mut self, start: TextSize, synthetic_id: &'static str) -> Stmt {
        // consume modifier keywords until we reach the variable name (the Name
        // token immediately followed by `:`), so chains like `final override x: T`
        // strip in full — not just the first modifier. remember a `final` and a
        // `private` in the chain: unlike the other modifiers (which ty ignores)
        // `final`'s `Final` qualifier and `private`'s invisibility to a widened
        // view of the class must both survive
        let mut is_final = false;
        let mut is_private = false;
        loop {
            if self.peek() == TokenKind::Colon {
                break;
            }
            match self.src_text(self.current_token_range()) {
                "final" => is_final = true,
                "private" => is_private = true,
                _ => {}
            }
            self.bump(TokenKind::Name);
        }
        let name = self.parse_identifier();
        self.bump(TokenKind::Colon); // consume ":"
        let annotation_expr = self
            .parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or())
            .expr;
        let assigned = self
            .eat(TokenKind::Equal)
            .then(|| Box::new(self.parse_declaration_value()));
        let target = Expr::Name(ast::ExprName {
            id: name.id.clone(),
            ctx: ExprContext::Store,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        // a `final` anywhere in the chain carries the `Final` qualifier, which ty
        // must apply in every scope; a `private` carries the privacy that safe
        // variance rests on. the rest are no-ops to ty. either way the declared
        // type stays under the marker in annotation position, so `T` is the
        // declaration — stashing it in `value` instead would make
        // `override x: T = v` declare nothing and read as `x = v`.
        // `final` wins over `private`: a `Final` member is read-only, so it can
        // neither be written through a widened view nor lose its qualifier here
        // the marker spans the whole keyword prefix from the statement's start,
        // which for `class var x: T` is the `class` — the formatter re-emits the
        // prefix from this range, so anything left out of it is dropped
        let marker_range = TextRange::new(start, name.range.start());
        let marker = Expr::Name(ast::ExprName {
            id: Name::new_static(match (is_final, is_private) {
                (true, _) => "__final__",
                (false, true) => "__private_annot__",
                (false, false) => synthetic_id,
            }),
            ctx: ExprContext::Invalid,
            range: marker_range,
            node_index: AtomicNodeIndex::NONE,
        });
        let annotation = Expr::Subscript(ast::ExprSubscript {
            range: TextRange::new(marker_range.start(), annotation_expr.range().end()),
            value: Box::new(marker),
            slice: Box::new(annotation_expr),
            ctx: ExprContext::Load,
            node_index: AtomicNodeIndex::NONE,
            is_typeof: false,
            is_type_decoration: false,
        });
        let value = assigned;
        self.eat_declaration_terminator();
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value,
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    /// Parses `decorator def name(...)` — a function definition with a synthetic
    /// decorator (name `"decorator_keyword"`) that the `decorator_keyword`
    /// transform expands into overloads + a runtime dispatcher
    fn parse_decorator_def(&mut self, start: TextSize) -> Stmt {
        let kw_start = self.current_token_range().start();
        self.bump(TokenKind::Name); // consume "decorator"
        let def_start = self.current_token_range().start();
        let decorator_range = TextRange::new(kw_start, def_start);

        let decorator = ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: Name::new_static("decorator_keyword"),
                ctx: ExprContext::Invalid,
                range: decorator_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: decorator_range,
            node_index: AtomicNodeIndex::NONE,
        };

        Stmt::FunctionDef(self.parse_function_definition(vec![decorator].into(), start))
    }

    /// Parses `protocol Foo:` — a class that uses `Protocol` as its base — without
    /// requiring an explicit `class` keyword. Produces a `ClassDef` with a synthetic
    /// `Decorator` (name `"protocol_class"`) that the `modifiers` transform rewrites
    /// to `class Foo(Protocol):`.
    fn parse_protocol_def(&mut self, start: TextSize, mut decorators: DecoratorList) -> Stmt {
        let protocol_start = self.current_token_range().start();
        self.bump(TokenKind::Name); // consume "protocol"
        let class_name_start = self.current_token_range().start();
        let decorator_range = TextRange::new(protocol_start, class_name_start);

        // the synthetic `protocol_class` marker follows any modifier decorators
        // (`private protocol P:`); the `modifiers` transform consumes each by a
        // disjoint range edit, so order between them does not matter
        decorators.push(ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: Name::new_static("protocol_class"),
                ctx: ExprContext::Invalid,
                range: decorator_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: decorator_range,
            node_index: AtomicNodeIndex::NONE,
        });

        let name = self.parse_identifier();
        let type_params = self.try_parse_type_params();
        let arguments = self
            .at(TokenKind::Lpar)
            .then(|| Box::new(self.parse_arguments(ArgumentsContext::ClassDefinition)));
        let body = if self.eat(TokenKind::Colon) {
            // a protocol body is a class body: the depth-gated class-body forms
            // (`init(...)`, property accessor blocks) must fire inside it too
            self.class_body_depth += 1;
            let body = self.parse_body(Clause::Class);
            self.class_body_depth -= 1;
            body
        } else {
            self.eat(TokenKind::Newline);
            Suite::new()
        };

        Stmt::ClassDef(ast::StmtClassDef {
            range: self.node_range(start),
            decorator_list: decorators,
            name,
            type_params: type_params.map(Box::new),
            arguments,
            body,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses an `extension Name[bounds](Interfaces):` declaration — methods and
    /// computed properties added to an existing type without subclassing it,
    /// plus the interfaces the type is declared to conform to.
    ///
    /// Produces a [`ClassDef`] carrying a synthetic `extension_def` marker
    /// decorator. the class name is the *extended* type (it references an
    /// existing declaration rather than introducing a new one), any bracketed
    /// type params are constraints on that type's own parameters
    /// (`extension list[Element: int]:`) rather than fresh declarations, and the
    /// argument list holds the conformances, in the field a class's bases live in
    ///
    /// [`ClassDef`]: ast::StmtClassDef
    fn parse_extension_def(&mut self, start: TextSize) -> Stmt {
        let extension_start = self.current_token_range().start();
        self.bump(TokenKind::Name); // consume "extension"
        let name_start = self.current_token_range().start();
        let decorator_range = TextRange::new(extension_start, name_start);

        let mut decorators = DecoratorList::new();
        decorators.push(ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: Name::new_static("extension_def"),
                ctx: ExprContext::Invalid,
                range: decorator_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: decorator_range,
            node_index: AtomicNodeIndex::NONE,
        });

        let name = self.parse_identifier();
        let type_params = self.try_parse_type_params();

        // an argument list on an extension is a *conformance* list: the
        // interfaces the extended type is being declared to satisfy. they are
        // stored where a class's bases go, so the extension literal derives
        // them and `override` checking against them comes for free
        let arguments = self
            .at(TokenKind::Lpar)
            .then(|| Box::new(self.parse_arguments(ArgumentsContext::ClassDefinition)));
        // a conformance list names interfaces and nothing else: a keyword (a
        // metaclass, say) has no meaning here, and an unpacking cannot be
        // resolved to the interfaces a conformance has to register under
        if let Some(arguments) = &arguments {
            for keyword in &arguments.keywords {
                self.add_error(
                    ParseErrorType::OtherError(
                        "an `extension` conformance list takes interfaces, not keyword arguments"
                            .to_string(),
                    ),
                    keyword.range(),
                );
            }
            for argument in &arguments.args {
                if argument.is_starred_expr() {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "an `extension` conformance list cannot be unpacked".to_string(),
                        ),
                        argument.range(),
                    );
                }
            }
        }

        let body = if self.eat(TokenKind::Colon) {
            // an extension body is a class body: the depth-gated class-body forms
            // (property accessor blocks in particular) must fire inside it too
            self.class_body_depth += 1;
            let body = self.parse_body(Clause::Class);
            self.class_body_depth -= 1;
            body
        } else {
            self.add_error(
                ParseErrorType::OtherError(
                    "Expected `:` after `extension` declaration".to_string(),
                ),
                self.current_token_range(),
            );
            Suite::new()
        };

        Stmt::ClassDef(ast::StmtClassDef {
            range: self.node_range(start),
            decorator_list: decorators,
            name,
            type_params: type_params.map(Box::new),
            arguments,
            body,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses a `build:` declaration — the values the build stamps into the
    /// program, settled when the artifact was produced rather than read at
    /// startup.
    ///
    /// Produces a [`ClassDef`] named `build` carrying a synthetic `build_def`
    /// marker decorator, so a use site is ordinary class-attribute access
    /// (`build.GIT_SHA`) and needs no resolution rule of its own.
    ///
    /// The name is given an empty range at the end of the keyword. There is no
    /// identifier in the source to point at, and a name sharing the keyword's
    /// span would highlight twice and offer a rename of something nobody wrote;
    /// an empty range is skipped by everything that reports on a span.
    ///
    /// [`ClassDef`]: ast::StmtClassDef
    fn parse_build_def(&mut self, start: TextSize) -> Stmt {
        let keyword_start = self.current_token_range().start();
        self.bump(TokenKind::Name); // consume "build"
        let keyword_range = TextRange::new(keyword_start, self.current_token_range().start());

        let mut decorators = DecoratorList::new();
        decorators.push(ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: Name::new_static("build_def"),
                ctx: ExprContext::Invalid,
                range: keyword_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: keyword_range,
            node_index: AtomicNodeIndex::NONE,
        });

        let name = ast::Identifier {
            id: Name::new_static("build"),
            range: TextRange::empty(keyword_range.end()),
            node_index: AtomicNodeIndex::NONE,
        };

        // the dispatch that got here required the colon, so there is no missing
        // one to recover from
        self.bump(TokenKind::Colon);
        let body = self.parse_body(Clause::Class);

        Stmt::ClassDef(ast::StmtClassDef {
            range: self.node_range(start),
            decorator_list: decorators,
            name,
            type_params: None,
            arguments: None,
            body,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses an `implements A, B` declaration, and its optional `for` clause
    /// naming the modules the obligation is imposed on
    /// (`implements Backend for ".*", "!.base"`).
    ///
    /// Produces an expression statement calling the synthetic `__implements__`
    /// marker. The interfaces are ordinary loads, so an import that exists only
    /// for the declaration still counts as used, and a `for` clause's patterns
    /// follow them in the same argument list as string literals. The two are told
    /// apart by kind, which is why an interface here may only be a name or a
    /// dotted name
    fn parse_implements_decl(&mut self, start: TextSize) -> Stmt {
        let keyword_range = self.current_token_range();
        self.bump(TokenKind::Name); // consume "implements"

        let mut args: Vec<Expr> = Vec::new();
        loop {
            if !self.at(TokenKind::Name) {
                self.add_error(
                    ParseErrorType::OtherError("`implements` takes an interface name".to_string()),
                    self.current_token_range(),
                );
                break;
            }
            args.push(self.parse_interface_reference());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            // a comma introduces another interface. `implements A, for "…"` is a
            // list with a hole in it, not a shorter list
            if self.at(TokenKind::For) {
                self.add_error(
                    ParseErrorType::OtherError(
                        "`implements` takes an interface name after `,`".to_string(),
                    ),
                    self.current_token_range(),
                );
                break;
            }
        }

        if self.eat(TokenKind::For) {
            loop {
                if !self.at(TokenKind::String) {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "a `for` clause takes module patterns, written as strings".to_string(),
                        ),
                        self.current_token_range(),
                    );
                    break;
                }
                let pattern_range = self.current_token_range();
                let pattern = self.parse_strings();
                if pattern.is_string_literal_expr() {
                    args.push(pattern);
                } else {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "a module pattern is a plain string".to_string(),
                        ),
                        pattern_range,
                    );
                }
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }

        // nothing else belongs on the line. a subscripted interface
        // (`implements Backend[int]`) is the shape this catches: leaving the
        // bracket to be re-parsed would silently drop the specialization and turn
        // the obligation into one the author did not write
        if !self.at_declaration_end() {
            self.add_error(
                ParseErrorType::OtherError(
                    "an `implements` declaration takes interface names, and patterns after `for`"
                        .to_string(),
                ),
                self.current_token_range(),
            );
            // consume what is left, so it does not re-parse as a statement of its
            // own and report a second, more confusing error
            let mut progress = ParserProgress::default();
            while !self.at_declaration_end() {
                progress.assert_progressing(self);
                self.bump_any();
            }
        }

        let range = self.node_range(start);
        self.eat_declaration_terminator();

        let marker = Expr::Name(ast::ExprName {
            id: Name::new_static("__implements__"),
            ctx: ExprContext::Invalid,
            range: keyword_range,
            node_index: AtomicNodeIndex::NONE,
        });
        Stmt::Expr(ast::StmtExpr {
            value: Box::new(Expr::Call(ast::ExprCall {
                func: Box::new(marker),
                arguments: ast::Arguments {
                    args: args.into_boxed_slice(),
                    keywords: ThinVec::new(),
                    range: TextRange::new(keyword_range.end(), range.end()),
                    node_index: AtomicNodeIndex::NONE,
                },
                range_start: start,
                cast_kind: None,
                is_string_tag: false,
                node_index: AtomicNodeIndex::NONE,
            })),
            range,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Is the parser at something that ends a simple statement?
    fn at_declaration_end(&self) -> bool {
        matches!(
            self.current_token_kind(),
            TokenKind::Newline | TokenKind::Semi | TokenKind::EndOfFile | TokenKind::Dedent
        )
    }

    /// Parses the `A` or `pkg.A` naming an interface in an `implements`
    /// declaration, as an ordinary load expression
    fn parse_interface_reference(&mut self) -> Expr {
        let start = self.node_start();
        let name = self.parse_identifier();
        let mut expr = Expr::Name(ast::ExprName {
            id: name.id,
            ctx: ExprContext::Load,
            range: name.range,
            node_index: AtomicNodeIndex::NONE,
        });
        while self.eat(TokenKind::Dot) {
            let attr = self.parse_identifier();
            expr = Expr::Attribute(ast::ExprAttribute {
                value: Box::new(expr),
                attr,
                ctx: ExprContext::Load,
                optional: false,
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
            });
        }
        expr
    }

    /// Parses a `type def Name[X]:` declaration — a user-defined type function
    /// whose body is executed to produce a type at each application.
    ///
    /// Produces a [`FunctionDef`] carrying a synthetic `type_fn` marker
    /// decorator and an empty parameter list: the type parameters are the
    /// function's parameters, and the application `Name[int]` is its call.
    ///
    /// [`FunctionDef`]: ast::StmtFunctionDef
    fn parse_type_def(&mut self, start: TextSize) -> Stmt {
        let marker_start = self.current_token_range().start();
        self.bump(TokenKind::Type);
        let marker_range = TextRange::new(marker_start, self.current_token_range().start());
        self.bump(TokenKind::Def);

        let mut decorators = DecoratorList::new();
        decorators.push(ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: Name::new_static(ruff_python_ast::helpers::TYPE_FN_MARKER),
                ctx: ExprContext::Invalid,
                range: marker_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: marker_range,
            node_index: AtomicNodeIndex::NONE,
        });

        let name = self.parse_identifier();
        let type_params = self.try_parse_type_params();
        if type_params.is_none() {
            self.add_error(
                ParseErrorType::OtherError(
                    "`type def` requires a type parameter list, e.g. `type def F[X]:`".to_string(),
                ),
                self.current_token_range(),
            );
        }

        // the type parameters *are* the parameters; a `(...)` list would be a
        // second, meaningless signature
        if self.at(TokenKind::Lpar) {
            self.add_error(
                ParseErrorType::OtherError(
                    "`type def` takes its parameters from the type parameter list, not `(...)`"
                        .to_string(),
                ),
                self.current_token_range(),
            );
        }

        let returns = self.eat(TokenKind::Rarrow).then(|| {
            Box::new(
                self.parse_expression_list(ExpressionContext::default())
                    .expr,
            )
        });

        let body = if self.eat(TokenKind::Colon) {
            self.parse_body(Clause::FunctionDef)
        } else {
            self.add_error(
                ParseErrorType::OtherError("Expected `:` after `type def` declaration".to_string()),
                self.current_token_range(),
            );
            Suite::new()
        };

        Stmt::FunctionDef(ast::StmtFunctionDef {
            range: self.node_range(start),
            is_async: false,
            decorator_list: decorators,
            name,
            type_params: type_params.map(Box::new),
            parameters: Box::new(ast::Parameters {
                range: TextRange::empty(start),
                node_index: AtomicNodeIndex::NONE,
                posonlyargs: std::iter::empty().collect(),
                args: std::iter::empty().collect(),
                vararg: None,
                kwonlyargs: std::iter::empty().collect(),
                kwarg: None,
            }),
            returns,
            // a `type def` declares a type-level function, which cannot raise
            raises: None,
            body,
            is_trailing_lambda: false,
            is_asserts_return: false,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses a based-enum declaration `enum Name[T]:` — an algebraic sum type.
    ///
    /// Produces a [`ClassDef`] carrying a synthetic `enum_def` marker decorator.
    /// The body holds one nested [`ClassDef`] per variant (each tagged with a
    /// `variant_unit` / `variant_tuple` marker decorator and holding its fields
    /// as [`AnnAssign`]s) plus any ordinary members (methods, classmethods,
    /// constants). The `enum` lowering phase consumes this shape.
    ///
    /// [`ClassDef`]: ast::StmtClassDef
    /// [`AnnAssign`]: ast::StmtAnnAssign
    fn parse_enum_def(&mut self, start: TextSize, mut decorators: DecoratorList) -> Stmt {
        let enum_start = self.current_token_range().start();
        self.bump(TokenKind::Name); // consume "enum"
        // the canonical surface is `enum class E:`; the `class` keyword is part
        // of the declaration. (a bare `enum E:` is reported as an error by the
        // caller and recovered here by leaving `class` un-consumed.)
        self.eat(TokenKind::Class);
        let name_start = self.current_token_range().start();
        let decorator_range = TextRange::new(enum_start, name_start);

        // the synthetic `enum_def` marker follows any modifier decorators
        // (`private enum class E:`); the enum lowering re-emits the enum from
        // scratch and reads the visibility markers to prefix the synthesized
        // `class` line, so the standard `private`/`export` class path applies
        decorators.push(ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: Name::new_static("enum_def"),
                ctx: ExprContext::Invalid,
                range: decorator_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: decorator_range,
            node_index: AtomicNodeIndex::NONE,
        });

        let name = self.parse_identifier();
        let type_params = self.try_parse_type_params();

        // sealed: a based enum has no declared base classes
        if self.at(TokenKind::Lpar) {
            self.add_error(
                ParseErrorType::OtherError(
                    "`enum` declarations cannot have base classes".to_string(),
                ),
                self.current_token_range(),
            );
        }

        let body = if self.eat(TokenKind::Colon) {
            self.class_body_depth += 1;
            let body = self.parse_enum_body();
            self.class_body_depth -= 1;
            body
        } else {
            self.add_error(
                ParseErrorType::OtherError("Expected `:` after `enum` declaration".to_string()),
                self.current_token_range(),
            );
            Suite::new()
        };

        Stmt::ClassDef(ast::StmtClassDef {
            range: self.node_range(start),
            decorator_list: decorators,
            name,
            type_params: type_params.map(Box::new),
            arguments: None,
            body,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses the indented body of an `enum` declaration, dispatching each item
    /// to either a `case` variant-declaration line or ordinary statement parsing.
    fn parse_enum_body(&mut self) -> Suite {
        let newline_range = self.current_token_range();
        if self.eat(TokenKind::Newline) {
            if self.at(TokenKind::Indent) {
                self.bump(TokenKind::Indent);
                let mut statements = Suite::new();
                if self
                    .with_recursion(|parser| {
                        parser.parse_list(RecoveryContextKind::BlockStatements, |p| {
                            p.parse_enum_item_into(&mut statements);
                        });
                    })
                    .is_none()
                {
                    self.report_recursion_limit_exceeded(self.current_token_range());
                }
                statements.shrink_to_fit();
                self.expect(TokenKind::Dedent);
                return statements;
            }
            self.add_error(
                ParseErrorType::OtherError(
                    "Expected an indented block after `enum` declaration".to_string(),
                ),
                if self.current_token_range().is_empty() {
                    newline_range
                } else {
                    self.current_token_range()
                },
            );
        } else {
            self.add_error(
                ParseErrorType::OtherError(
                    "Expected an indented block after `enum` declaration".to_string(),
                ),
                self.current_token_range(),
            );
        }
        Suite::new()
    }

    /// Parses one item in an `enum` body into `statements`: a `case` line
    /// declaring one or more comma-separated variants, or an ordinary
    /// class-body statement (method, classmethod, constant, …).
    fn parse_enum_item_into(&mut self, statements: &mut Suite) {
        // `case` followed by a name declares variant(s) — `case A`,
        // `case A, B, C`, `case Circle(radius: float), Empty`. any other use of
        // `case` (`case = 1`, `case.x`, …) is an ordinary identifier statement
        if self.at(TokenKind::Case)
            && (self.peek() == TokenKind::Name || self.peek().is_soft_keyword())
        {
            self.parse_case_variants(statements);
            return;
        }
        // a bare name statement is a no-op in a class body and is almost
        // certainly a variant missing its `case` — say so rather than letting
        // it silently parse to dead code
        if self.at(TokenKind::Name) && matches!(self.peek(), TokenKind::Newline | TokenKind::Semi) {
            self.add_error(
                ParseErrorType::OtherError(
                    "enum variants must be declared with `case`, e.g. `case Red, Green`"
                        .to_string(),
                ),
                self.current_token_range(),
            );
        }
        statements.push(self.parse_statement());
    }

    /// Parses one `case` line of an `enum` body: `case` followed by one or more
    /// comma-separated variants, each a unit (`Point`) or tuple
    /// (`Circle(radius: float)`, fields optionally defaulted) form. Each
    /// variant becomes its own marked [`ClassDef`].
    ///
    /// [`ClassDef`]: ast::StmtClassDef
    fn parse_case_variants(&mut self, statements: &mut Suite) {
        let case_start = self.current_token_range().start();
        self.bump(TokenKind::Case);
        // the first variant on a `case` line owns the keyword range; the rest
        // get a zero-width marker at their name
        let mut keyword_start = Some(case_start);
        loop {
            let start = self.node_start();
            let name = self.parse_identifier();
            let marker_range = match keyword_start.take() {
                Some(kw_start) => TextRange::new(kw_start, name.range.start()),
                None => TextRange::empty(name.range.start()),
            };
            let stmt = match self.current_token_kind() {
                TokenKind::Lpar => self.parse_tuple_variant(start, name, marker_range),
                _ => {
                    // a brace payload would otherwise parse as a stray dict
                    // display after a unit variant, surfacing as a confusing
                    // unresolved-reference — reject it with the fix spelled out
                    if self.at(TokenKind::Lbrace) {
                        self.add_error(
                            ParseErrorType::OtherError(format!(
                                "variant fields are declared in parentheses, e.g. `case {}(x: int)`",
                                name.id
                            )),
                            self.current_token_range(),
                        );
                        self.skip_brace_group();
                    }
                    let decorator = synthetic_variant_decorator("variant_unit", marker_range);
                    Stmt::ClassDef(ast::StmtClassDef {
                        range: self.node_range(start),
                        decorator_list: vec![decorator].into(),
                        name,
                        type_params: None,
                        arguments: None,
                        body: Suite::new(),
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
            };
            statements.push(stmt);
            if !self.eat(TokenKind::Comma) {
                break;
            }
            // a trailing comma ends the list
            if !(self.at(TokenKind::Name) || self.current_token_kind().is_soft_keyword()) {
                break;
            }
        }
        self.eat(TokenKind::Semi);
        self.eat(TokenKind::Newline);
    }

    /// Consumes a balanced `{ … }` group for error recovery.
    fn skip_brace_group(&mut self) {
        self.bump(TokenKind::Lbrace);
        let mut depth = 1usize;
        while depth > 0 && !self.at(TokenKind::EndOfFile) {
            match self.current_token_kind() {
                TokenKind::Lbrace => depth += 1,
                TokenKind::Rbrace => depth -= 1,
                _ => {}
            }
            self.bump_any();
        }
    }

    /// Parses the payload of a tuple variant `Circle(radius: float)` /
    /// `Node(T, Tree[T])` — positional construction. Fields may be named
    /// (`radius: float`) or anonymous (`T`), in which case they take the
    /// synthetic names `_0`, `_1`, …
    fn parse_tuple_variant(
        &mut self,
        start: TextSize,
        name: ast::Identifier,
        marker_range: TextRange,
    ) -> Stmt {
        self.bump(TokenKind::Lpar);
        let mut body = Suite::new();
        let mut index = 0usize;
        while !self.at(TokenKind::Rpar) && !self.at(TokenKind::EndOfFile) {
            let field_start = self.node_start();
            let (target_id, target_range) =
                if self.at(TokenKind::Name) && self.peek() == TokenKind::Colon {
                    let ident = self.parse_identifier();
                    self.bump(TokenKind::Colon);
                    (ident.id, ident.range)
                } else {
                    (
                        Name::from(format!("_{index}").as_str()),
                        TextRange::empty(self.current_token_range().start()),
                    )
                };
            let annotation = self.parse_conditional_expression_or_higher().expr;
            let value = if self.eat(TokenKind::Equal) {
                Some(Box::new(self.parse_conditional_expression_or_higher().expr))
            } else {
                None
            };
            body.push(self.make_variant_field(
                field_start,
                target_id,
                target_range,
                annotation,
                value,
            ));
            index += 1;
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::Rpar);
        let decorator = synthetic_variant_decorator("variant_tuple", marker_range);
        Stmt::ClassDef(ast::StmtClassDef {
            range: self.node_range(start),
            decorator_list: vec![decorator].into(),
            name,
            type_params: None,
            arguments: None,
            body,
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Builds a variant field as a synthetic [`AnnAssign`]. The annotation keeps
    /// its real source range so the lowering phase can slice the original type
    /// text (preserving any basedpython type syntax it contains).
    fn make_variant_field(
        &self,
        start: TextSize,
        target_id: Name,
        target_range: TextRange,
        annotation: Expr,
        value: Option<Box<Expr>>,
    ) -> Stmt {
        let target = Expr::Name(ast::ExprName {
            id: target_id,
            ctx: ExprContext::Store,
            range: target_range,
            node_index: AtomicNodeIndex::NONE,
        });
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target: Box::new(target),
            annotation: Box::new(annotation),
            value,
            simple: true,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        })
    }

    /// basedpython: whether only whitespace precedes `offset` on its line.
    fn starts_its_line(&self, offset: TextSize) -> bool {
        let offset = usize::from(offset);
        let line_start = self.source[..offset]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        self.source[line_start..offset]
            .bytes()
            .all(|byte| byte.is_ascii_whitespace())
    }

    /// basedpython: reports any [statement expression](ast::ExprStatement) in
    /// `stmt` that stands where its value could not be computed before the rest
    /// of the statement runs.
    ///
    /// A statement expression stands for a statement, so it owns the tail of the
    /// statement it appears in: it must be the statement's value expression, or
    /// reachable from it through the operators that merely *choose* between
    /// operands (`and`, `or`, `??`, the conditional expression, and the walrus).
    /// Anywhere else the surrounding expression would have to be evaluated around
    /// it, and there is no order in which that means anything.
    ///
    /// The forms that carry a suite are held to the stricter rule of being the
    /// value expression itself: a suite cannot be nested inside the branch of a
    /// choosing operator without being re-indented.
    fn validate_statement_expressions(&mut self, stmt: &mut Stmt) {
        let tail = match &*stmt {
            Stmt::Assign(assign) => Some(&*assign.value),
            Stmt::AnnAssign(assign) => assign.value.as_deref(),
            Stmt::AugAssign(assign) => Some(&*assign.value),
            Stmt::Return(ret) => ret.value.as_deref(),
            Stmt::Expr(expr) => Some(&*expr.value),
            _ => None,
        };

        let mut allowed = Vec::new();
        if let Some(tail) = tail {
            collect_tail_positions(tail, &mut allowed);
        }

        let starts_its_line = self.starts_its_line(stmt.range().start());

        let mut found = Vec::new();
        collect_statement_expressions(stmt, &mut found);
        let mut discarded = Vec::new();
        for statement in found {
            let expr_ref: &Expr = statement.0;
            let in_tail = allowed.iter().any(|a| std::ptr::eq(*a, expr_ref));
            let is_root = tail.is_some_and(|tail| std::ptr::eq(tail, expr_ref));
            let carries_suite = !matches!(
                &*statement.1.stmt,
                Stmt::Raise(_) | Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_)
            );
            let message = if carries_suite && !is_root {
                "a statement expression with a suite must be the whole value of its statement"
            } else if carries_suite && !starts_its_line {
                // the suite continues the line the statement starts on, so anything
                // already there would end up in front of a compound statement
                "a statement expression with a suite must be the first statement on its line"
            } else if !in_tail {
                "a statement expression must be the tail of its statement"
            } else {
                continue;
            };
            self.add_error(
                ParseErrorType::OtherError(message.to_string()),
                statement.1.range,
            );
            // only a suite is dropped from the tree: what one brings with it —
            // statements that bind and declare names — has nowhere to live where
            // the rule does not hold, while a `raise` or a `return` carries an
            // expression and nothing else, which the type checker can still read
            if carries_suite {
                discarded.push(statement.1.range);
            }
        }

        discard_statement_expressions(stmt, &discarded);
    }

    /// Parses a single simple statement.
    ///
    /// This statement must be terminated by a newline or semicolon.
    ///
    /// Use [`Parser::parse_simple_statements`] to parse a sequence of simple statements.
    fn parse_single_simple_statement(&mut self) -> Stmt {
        let mut stmt = self.parse_simple_statement();
        self.validate_statement_expressions(&mut stmt);

        // basedpython: a trailing lambda block and a match type alias are compound
        // statements — they consumed their own newline, indent and dedent, so the
        // simple-statement termination handling below does not apply
        if consumed_own_suite(&stmt) {
            return stmt;
        }

        // basedpython: likewise for a statement expression whose suite already
        // swallowed this statement's newline
        if std::mem::take(&mut self.expr_consumed_suite) {
            return stmt;
        }

        // The order of the token is important here.
        let has_eaten_semicolon = self.eat(TokenKind::Semi);
        let has_eaten_newline = self.eat(TokenKind::Newline);

        if !has_eaten_newline {
            if !has_eaten_semicolon && self.at_simple_stmt() {
                // test_err simple_stmts_on_same_line
                // a b
                // a + b c + d
                // break; continue pass; continue break
                self.add_error(
                    ParseErrorType::SimpleStatementsOnSameLine,
                    self.current_token_range(),
                );
            } else if self.at_compound_stmt() {
                // test_err simple_and_compound_stmt_on_same_line
                // a; if b: pass; b
                self.add_error(
                    ParseErrorType::SimpleAndCompoundStatementOnSameLine,
                    self.current_token_range(),
                );
            }
        }

        stmt
    }

    /// Parses a sequence of simple statements.
    ///
    /// If there is more than one statement in this sequence, it is expected to be separated by a
    /// semicolon. The sequence can optionally end with a semicolon, but regardless of whether
    /// a semicolon is present or not, it is expected to end with a newline.
    ///
    /// Matches the `simple_stmts` rule in the [Python grammar].
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    fn parse_simple_statements(&mut self) -> Suite {
        let stmts_snapshot = self.stmt_scratch.snapshot();
        let mut progress = ParserProgress::default();
        let mut is_first = true;

        loop {
            progress.assert_progressing(self);

            let mut stmt = self.parse_simple_statement();
            self.validate_statement_expressions(&mut stmt);
            let statement_expression_suite = std::mem::take(&mut self.expr_consumed_suite);
            let consumed_suite = consumed_own_suite(&stmt) || statement_expression_suite;

            // basedpython: a statement expression's suite begins on the line its
            // statement does, so anything preceding it on that line would end up
            // in front of a compound statement
            if statement_expression_suite && !is_first {
                self.add_error(
                    ParseErrorType::OtherError(
                        "a statement expression with a suite must be the first statement on its line"
                            .to_string(),
                    ),
                    stmt.range(),
                );
            }
            is_first = false;

            self.stmt_scratch.push(stmt);

            // basedpython: a trailing lambda block, a match type alias or a statement
            // expression consumed its own suite — no semicolon or newline follows it
            if consumed_suite {
                return self.stmt_scratch.take_thin_vec(stmts_snapshot);
            }

            if !self.eat(TokenKind::Semi) {
                if self.at_simple_stmt() {
                    // test_err simple_stmts_on_same_line_in_block
                    // if True: break; continue pass; continue break
                    self.add_error(
                        ParseErrorType::SimpleStatementsOnSameLine,
                        self.current_token_range(),
                    );
                } else {
                    // test_ok simple_stmts_in_block
                    // if True: pass
                    // if True: pass;
                    // if True: pass; continue
                    // if True: pass; continue;
                    // x = 1
                    break;
                }
            }

            if !self.at_simple_stmt() {
                break;
            }
        }

        // Ideally, we should use `expect` here but we use `eat` for better error message. Later,
        // if the parser isn't at the start of a compound statement, we'd `expect` a newline.
        if !self.eat(TokenKind::Newline) {
            if self.at_compound_stmt() {
                // test_err simple_and_compound_stmt_on_same_line_in_block
                // if True: pass if False: pass
                // if True: pass; if False: pass
                self.add_error(
                    ParseErrorType::SimpleAndCompoundStatementOnSameLine,
                    self.current_token_range(),
                );
            } else {
                // test_err multiple_clauses_on_same_line
                // if True: pass elif False: pass else: pass
                // if True: pass; elif False: pass; else: pass
                // for x in iter: break else: pass
                // for x in iter: break; else: pass
                // try: pass except exc: pass else: pass finally: pass
                // try: pass; except exc: pass; else: pass; finally: pass
                self.add_error(
                    ParseErrorType::ExpectedToken {
                        found: self.current_token_kind(),
                        expected: TokenKind::Newline,
                    },
                    self.current_token_range(),
                );
            }
        }

        // test_ok simple_stmts_with_semicolons
        // return; import a; from x import y; z; type T = int
        self.stmt_scratch.take_thin_vec(stmts_snapshot)
    }

    /// Parses a simple statement.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html>
    fn parse_simple_statement(&mut self) -> Stmt {
        match self.current_token_kind() {
            TokenKind::Return => Stmt::Return(self.parse_return_statement()),
            TokenKind::Import => {
                let start = self.node_start();
                Stmt::Import(self.parse_import_statement(start, false))
            }
            TokenKind::From => {
                let start = self.node_start();
                Stmt::ImportFrom(self.parse_from_import_statement(start, false))
            }
            TokenKind::Pass => Stmt::Pass(self.parse_pass_statement()),
            TokenKind::Continue => Stmt::Continue(self.parse_continue_statement()),
            TokenKind::Break => Stmt::Break(self.parse_break_statement()),
            TokenKind::Raise => Stmt::Raise(self.parse_raise_statement()),
            TokenKind::Del => Stmt::Delete(self.parse_delete_statement()),
            TokenKind::Assert => Stmt::Assert(self.parse_assert_statement()),
            TokenKind::Global => Stmt::Global(self.parse_global_statement()),
            TokenKind::Nonlocal => Stmt::Nonlocal(self.parse_nonlocal_statement()),
            TokenKind::IpyEscapeCommand => {
                Stmt::IpyEscapeCommand(self.parse_ipython_escape_command_statement())
            }
            token => {
                if token == TokenKind::Lazy {
                    let start = self.node_start();
                    let lazy_range = self.current_token_range();

                    match self.peek() {
                        // test_ok lazy_import_stmt_py315
                        // # parse_options: {"target-version": "3.15"}
                        // lazy import foo
                        // lazy import foo as bar
                        // lazy from bar import baz
                        // lazy from sys import x as y
                        // lazy = 1
                        // import foo as lazy
                        // from lazy import qux

                        // test_ok lazy_import_relative_py315
                        // # parse_options: {"target-version": "3.15"}
                        // lazy from . import basic2
                        // lazy from .basic2 import x, f
                        // lazy from . import b, x

                        // test_ok lazy_import_soft_keyword_split_py315
                        // # parse_options: {"target-version": "3.15"}
                        // lazy
                        // import os
                        //
                        // lazy  # comment
                        // from sys import path

                        // test_err lazy_import_stmt_py314
                        // # parse_options: {"target-version": "3.14"}
                        // lazy import foo
                        // lazy from bar import baz
                        TokenKind::Import => {
                            self.bump(TokenKind::Lazy);
                            self.add_unsupported_syntax_error(
                                UnsupportedSyntaxErrorKind::LazyImportStatement,
                                lazy_range,
                            );
                            return Stmt::Import(self.parse_import_statement(start, true));
                        }
                        TokenKind::From => {
                            self.bump(TokenKind::Lazy);
                            self.add_unsupported_syntax_error(
                                UnsupportedSyntaxErrorKind::LazyImportStatement,
                                lazy_range,
                            );
                            return Stmt::ImportFrom(self.parse_from_import_statement(start, true));
                        }
                        _ => {}
                    }
                }

                if token == TokenKind::Type {
                    // Type is considered a soft keyword, so we will treat it as an identifier if
                    // it's followed by an unexpected token.
                    let (first, second) = self.peek2();

                    if (first == TokenKind::Name || first.is_soft_keyword())
                        && matches!(second, TokenKind::Lsqb | TokenKind::Equal)
                    {
                        return Stmt::TypeAlias(self.parse_type_alias_statement());
                    }
                }

                // basedpython: `let <pattern> := <subject>` destructures the
                // subject. Reached after `let NAME [: T] = ...` has been ruled
                // out, since that shape is a declaration rather than a match
                if token == TokenKind::Name
                    && self.src_text(self.current_token_range()) == "let"
                    && let Some(let_stmt) = self.try_parse_let_statement()
                {
                    return Stmt::Let(let_stmt);
                }

                let start = self.node_start();

                // test_err yield_after_comma
                // def f(): 1, yield 1

                // test_ok yield_after_comma_parenthesized
                // def f(): 1, (yield 1)

                // simple_stmt: `... | yield_stmt | star_expressions | ...`
                let parsed_expr =
                    self.parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or());

                if self.at(TokenKind::Equal) {
                    Stmt::Assign(self.parse_assign_statement(parsed_expr, start))
                } else if self.at(TokenKind::Colon) {
                    // basedpython: an expression followed by `:` and an indented
                    // suite is a trailing lambda block — the suite becomes a
                    // function passed as the call's last argument
                    if self.at_trailing_lambda_block() {
                        return Stmt::FunctionDef(
                            self.parse_trailing_lambda_statement(parsed_expr, start),
                        );
                    }
                    Stmt::AnnAssign(self.parse_annotated_assignment_statement(parsed_expr, start))
                } else if let Some(op) = self.current_token_kind().as_augmented_assign_operator() {
                    Stmt::AugAssign(self.parse_augmented_assignment_statement(
                        parsed_expr,
                        op,
                        start,
                    ))
                } else if self.options.mode == Mode::Ipython && self.at(TokenKind::Question) {
                    Stmt::IpyEscapeCommand(
                        self.parse_ipython_help_end_escape_command_statement(&parsed_expr),
                    )
                } else {
                    Stmt::Expr(ast::StmtExpr {
                        range: self.node_range(start),
                        value: Box::new(parsed_expr.expr),
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
            }
        }
    }

    /// Parses a delete statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `del` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-del_stmt>
    fn parse_delete_statement(&mut self) -> ast::StmtDelete {
        let start = self.node_start();
        self.bump(TokenKind::Del);

        // test_err del_incomplete_target
        // del x, y.
        // z
        // del x, y[
        // z
        let targets = self.parse_comma_separated_list_into_vec(
            RecoveryContextKind::DeleteTargets,
            |parser| {
                // Allow starred expression to raise a better error message for
                // an invalid delete target later.
                let mut target = parser.parse_conditional_expression_or_higher_impl(
                    ExpressionContext::starred_conditional(),
                );
                helpers::set_expr_ctx(&mut target.expr, ExprContext::Del);

                // test_err invalid_del_target
                // del x + 1
                // del {'x': 1}
                // del {'x', 'y'}
                // del None, True, False, 1, 1.0, "abc"
                parser.validate_delete_target(&target.expr);

                target.expr
            },
        );

        if targets.is_empty() {
            // test_err del_stmt_empty
            // del
            self.add_error(
                ParseErrorType::EmptyDeleteTargets,
                self.current_token_range(),
            );
        }

        ast::StmtDelete {
            targets,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a `return` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `return` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-return_stmt>
    fn parse_return_statement(&mut self) -> ast::StmtReturn {
        let start = self.node_start();
        self.bump(TokenKind::Return);

        // basedpython: a statement expression may supply the returned value, and
        // its keyword does not otherwise start an expression
        let at_value = self.at_expr() || starts_statement_expression(self.current_token_kind());

        // test_err return_stmt_invalid_expr
        // return *
        // return yield x
        // return yield from x
        // return x := 1
        // return *x and y
        let value = at_value.then(|| {
            let parsed_expr = self.parse_expression_list(ExpressionContext::starred_bitwise_or());

            // test_ok iter_unpack_return_py37
            // # parse_options: {"target-version": "3.7"}
            // rest = (4, 5, 6)
            // def f(): return (1, 2, 3, *rest)

            // test_ok iter_unpack_return_py38
            // # parse_options: {"target-version": "3.8"}
            // rest = (4, 5, 6)
            // def f(): return 1, 2, 3, *rest

            // test_err iter_unpack_return_py37
            // # parse_options: {"target-version": "3.7"}
            // rest = (4, 5, 6)
            // def f(): return 1, 2, 3, *rest
            self.check_tuple_unpacking(
                &parsed_expr,
                UnsupportedSyntaxErrorKind::StarTuple(StarTupleKind::Return),
            );

            // basedpython: the returned value is an expression like any other, so
            // a trailing lambda block may supply it — `return div:` followed by a
            // suite returns what the call returns
            Box::new(self.parse_trailing_lambda_value(parsed_expr).expr)
        });

        ast::StmtReturn {
            range: self.node_range(start),
            value,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Report [`UnsupportedSyntaxError`]s for each starred element in `expr` if it is an
    /// unparenthesized tuple.
    ///
    /// This method can be used to check for tuple unpacking in `return`, `yield`, and `for`
    /// statements, which are only allowed after [Python 3.8] and [Python 3.9], respectively.
    ///
    /// [Python 3.8]: https://github.com/python/cpython/issues/76298
    /// [Python 3.9]: https://github.com/python/cpython/issues/90881
    pub(super) fn check_tuple_unpacking(&mut self, expr: &Expr, kind: UnsupportedSyntaxErrorKind) {
        if kind.is_supported(self.options.target_version) {
            return;
        }

        let Expr::Tuple(ast::ExprTuple {
            elts,
            parenthesized: false,
            is_anon_named_tuple: false,
            is_anon_named_tuple_value: false,
            callable_shape: None,
            ..
        }) = expr
        else {
            return;
        };

        for elt in elts {
            if elt.is_starred_expr() {
                self.add_unsupported_syntax_error(kind, elt.range());
            }
        }
    }

    /// Parses a `raise` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `raise` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-raise_stmt>
    fn parse_raise_statement(&mut self) -> ast::StmtRaise {
        let start = self.node_start();
        self.bump(TokenKind::Raise);

        let exc = match self.current_token_kind() {
            TokenKind::Newline => None,
            TokenKind::From => {
                // test_err raise_stmt_from_without_exc
                // raise from exc
                // raise from None
                self.add_error(
                    ParseErrorType::OtherError(
                        "Exception missing in `raise` statement with cause".to_string(),
                    ),
                    self.current_token_range(),
                );
                None
            }
            _ => {
                // test_err raise_stmt_invalid_exc
                // raise *x
                // raise yield x
                // raise x := 1
                let exc = self.parse_expression_list(ExpressionContext::default());

                if let Some(ast::ExprTuple {
                    parenthesized: false,
                    is_anon_named_tuple: false,
                    is_anon_named_tuple_value: false,
                    callable_shape: None,
                    ..
                }) = exc.as_tuple_expr()
                {
                    // test_err raise_stmt_unparenthesized_tuple_exc
                    // raise x,
                    // raise x, y
                    // raise x, y from z
                    self.add_error(ParseErrorType::UnparenthesizedTupleExpression, &exc);
                }

                Some(Box::new(exc.expr))
            }
        };

        let cause = self.eat(TokenKind::From).then(|| {
            // test_err raise_stmt_invalid_cause
            // raise x from *y
            // raise x from yield y
            // raise x from y := 1
            let cause = self.parse_expression_list(ExpressionContext::default());

            if let Some(ast::ExprTuple {
                parenthesized: false,
                is_anon_named_tuple: false,
                is_anon_named_tuple_value: false,
                callable_shape: None,
                ..
            }) = cause.as_tuple_expr()
            {
                // test_err raise_stmt_unparenthesized_tuple_cause
                // raise x from y,
                // raise x from y, z
                self.add_error(ParseErrorType::UnparenthesizedTupleExpression, &cause);
            }

            Box::new(cause.expr)
        });

        ast::StmtRaise {
            range: self.node_range(start),
            exc,
            cause,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an import statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `import` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#the-import-statement>
    fn parse_import_statement(&mut self, start: TextSize, is_lazy: bool) -> ast::StmtImport {
        self.bump(TokenKind::Import);

        // test_err import_stmt_parenthesized_names
        // import (a)
        // import (a, b)

        // test_err import_stmt_star_import
        // import *
        // import x, *, y

        // test_err import_stmt_trailing_comma
        // import ,
        // import x, y,

        let names_snapshot = self.alias_scratch.snapshot();
        self.parse_comma_separated_list(RecoveryContextKind::ImportNames, |parser| {
            let alias = parser.parse_alias(ImportStyle::Import);
            parser.alias_scratch.push(alias);
        });
        let names: Vec<_> = self.alias_scratch.take(names_snapshot);

        if names.is_empty() {
            // test_err import_stmt_empty
            // import
            self.add_error(ParseErrorType::EmptyImportNames, self.current_token_range());
        }

        ast::StmtImport {
            names,
            is_lazy,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Returns whether the parser sits on basedpython's `export` keyword — the
    /// re-exporting spelling of `import` in `from x export y`.
    fn at_export_keyword(&mut self) -> bool {
        self.at(TokenKind::Name) && self.src_text(self.current_token_range()) == "export"
    }

    /// Returns whether the parser sits on the `export` keyword of a relative
    /// import that omits its module, as in `from . export y`.
    ///
    /// A relative import may drop the module, so at this position `export` could
    /// equally start a module name. Only a following `import` (`from . export
    /// import y`) or `.` (`from .export.sub import y`) makes it one; anything
    /// else — a name, `(`, `*` — starts the imported-name list, so the `export`
    /// is the keyword.
    fn at_module_less_export(&mut self, leading_dots: u32) -> bool {
        leading_dots > 0
            && self.at_export_keyword()
            && !matches!(self.peek(), TokenKind::Import | TokenKind::Dot)
    }

    /// Parses a `from` import statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `from` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-import_stmt>
    fn parse_from_import_statement(
        &mut self,
        start: TextSize,
        is_lazy: bool,
    ) -> ast::StmtImportFrom {
        self.bump(TokenKind::From);

        let mut leading_dots = 0;
        let mut progress = ParserProgress::default();

        loop {
            progress.assert_progressing(self);

            if self.eat(TokenKind::Dot) {
                leading_dots += 1;
            } else if self.eat(TokenKind::Ellipsis) {
                leading_dots += 3;
            } else {
                break;
            }
        }

        let module = if self.at_name_or_soft_keyword() && !self.at_module_less_export(leading_dots)
        {
            // test_ok from_import_soft_keyword_module_name
            // from match import pattern
            // from type import bar
            // from case import pattern
            // from lazy import qux
            // from match.type.case import foo
            Some(self.parse_dotted_name())
        } else {
            if leading_dots == 0 {
                // test_err from_import_missing_module
                // from
                // from import x
                self.add_error(
                    ParseErrorType::OtherError("Expected a module name".to_string()),
                    self.current_token_range(),
                );
            }
            None
        };

        // test_ok from_import_no_space
        // from.import x
        // from...import x

        // basedpython: `from x export y` binds `y` as an explicit re-export —
        // the Python spelling is `from x import y as y`
        let is_export = self.at_export_keyword();
        if is_export {
            self.error_if_not_basedpython(
                "`from ... export ...` is not valid in `.py` files".to_string(),
            );
            self.bump(TokenKind::Name);
        } else {
            self.expect(TokenKind::Import);
        }

        let names_start = self.node_start();
        let names_snapshot = self.alias_scratch.snapshot();
        let mut seen_star_import = false;

        let parenthesized = Parenthesized::from(self.eat(TokenKind::Lpar));

        // test_err from_import_unparenthesized_trailing_comma
        // from a import b,
        // from a import b as c,
        // from a import b, c,
        self.parse_comma_separated_list(
            RecoveryContextKind::ImportFromAsNames(parenthesized),
            |parser| {
                // test_err from_import_dotted_names
                // from x import a.
                // from x import a.b
                // from x import a, b.c, d, e.f, g
                let alias = parser.parse_alias(ImportStyle::ImportFrom);
                seen_star_import |= alias.name.id == "*";
                parser.alias_scratch.push(alias);
            },
        );
        let names: Vec<_> = self.alias_scratch.take(names_snapshot);

        if names.is_empty() {
            // test_err from_import_empty_names
            // from x import
            // from x import ()
            // from x import ,,
            self.add_error(ParseErrorType::EmptyImportNames, self.current_token_range());
        }

        if seen_star_import && parenthesized.is_yes() {
            // test_err from_import_parenthesized_star
            // from x import (*)
            self.add_error(
                ParseErrorType::OtherError("Star import cannot be parenthesized".to_string()),
                self.node_range(names_start),
            );
        }

        if seen_star_import && names.len() > 1 {
            // test_err from_import_star_with_other_names
            // from x import *, a
            // from x import a, *, b
            // from x import *, a as b
            // from x import *, *, a
            self.add_error(
                ParseErrorType::OtherError("Star import must be the only import".to_string()),
                self.node_range(names_start),
            );
        }

        if is_export {
            // `export` means "bind under this exact name", so neither a star
            // (which binds no single name) nor a rename can be expressed
            if seen_star_import {
                self.add_error(
                    ParseErrorType::OtherError(
                        "`export` cannot be used with a star import".to_string(),
                    ),
                    self.node_range(names_start),
                );
            }

            for alias in &names {
                if let Some(asname) = &alias.asname {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "`export` cannot be combined with an `as` clause; \
                             use `from ... import ... as ...` instead"
                                .to_string(),
                        ),
                        asname.range,
                    );
                }
            }
        }

        if parenthesized.is_yes() {
            // test_err from_import_missing_rpar
            // from x import (a, b
            // 1 + 1
            // from x import (a, b,
            // 2 + 2
            self.expect(TokenKind::Rpar);
        }

        ast::StmtImportFrom {
            module,
            names,
            level: leading_dots,
            is_lazy,
            is_export,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an `import` or `from` import name.
    ///
    /// See:
    /// - <https://docs.python.org/3/reference/simple_stmts.html#the-import-statement>
    /// - <https://docs.python.org/3/library/ast.html#ast.alias>
    fn parse_alias(&mut self, style: ImportStyle) -> ast::Alias {
        let start = self.node_start();
        if self.eat(TokenKind::Star) {
            let range = self.node_range(start);
            return ast::Alias {
                name: ast::Identifier {
                    id: Name::new_static("*"),
                    range,
                    node_index: AtomicNodeIndex::NONE,
                },
                asname: None,
                range,
                node_index: AtomicNodeIndex::NONE,
            };
        }

        let name = match style {
            ImportStyle::Import => self.parse_dotted_name(),
            ImportStyle::ImportFrom => self.parse_identifier(),
        };

        let asname = if self.eat(TokenKind::As) {
            if self.at_name_or_soft_keyword() {
                // test_ok import_as_name_soft_keyword
                // import foo as match
                // import bar as case
                // import baz as type
                // import qux as lazy
                Some(self.parse_identifier())
            } else {
                // test_err import_alias_missing_asname
                // import x as
                self.add_error(
                    ParseErrorType::OtherError("Expected symbol after `as`".to_string()),
                    self.current_token_range(),
                );
                None
            }
        } else {
            None
        };

        ast::Alias {
            range: self.node_range(start),
            name,
            asname,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a dotted name.
    ///
    /// A dotted name is a sequence of identifiers separated by a single dot.
    fn parse_dotted_name(&mut self) -> ast::Identifier {
        let start = self.node_start();

        let first = self.parse_identifier();
        if !self.at(TokenKind::Dot) {
            return first;
        }

        let snapshot = self.name_buffer.len();
        self.name_buffer.push_str(&first.id);
        let mut progress = ParserProgress::default();

        while self.eat(TokenKind::Dot) {
            progress.assert_progressing(self);

            // test_err dotted_name_multiple_dots
            // import a..b
            // import a...b
            self.name_buffer.push('.');
            let identifier = self.parse_identifier();
            self.name_buffer.push_str(&identifier.id);
        }

        let id = self.name_interner.intern(&self.name_buffer[snapshot..]);
        self.name_buffer.truncate(snapshot);

        // test_ok dotted_name_normalized_spaces
        // import a.b.c
        // import a .  b  . c
        ast::Identifier {
            id,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a `pass` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `pass` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-pass_stmt>
    fn parse_pass_statement(&mut self) -> ast::StmtPass {
        let start = self.node_start();
        self.bump(TokenKind::Pass);
        ast::StmtPass {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a `continue` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `continue` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-continue_stmt>
    fn parse_continue_statement(&mut self) -> ast::StmtContinue {
        let start = self.node_start();
        self.bump(TokenKind::Continue);
        ast::StmtContinue {
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a `break` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `break` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-break_stmt>
    fn parse_break_statement(&mut self) -> ast::StmtBreak {
        let start = self.node_start();
        self.bump(TokenKind::Break);

        // basedpython: `break <value>` yields a value out of a loop used as a
        // statement expression
        let value = self.at_expr().then(|| {
            self.error_if_not_basedpython(
                "`break` with a value is not valid in .py files".to_string(),
            );
            Box::new(
                self.parse_expression_list(ExpressionContext::default())
                    .expr,
            )
        });

        ast::StmtBreak {
            value,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// basedpython: parses a compound statement written where an expression is
    /// expected — a [statement expression](ast::ExprStatement).
    ///
    /// Only called in basedpython mode; in a `.py` file these keywords keep
    /// python's own error recovery.
    ///
    /// Returns `None` when the current token is the `match` soft keyword being
    /// used as an ordinary identifier, in which case nothing has been consumed.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a token that can start a statement
    /// expression.
    pub(super) fn parse_statement_expression(&mut self) -> Option<ast::ExprStatement> {
        let start = self.node_start();

        // the suite-bearing forms end with a `Dedent`, having already eaten the
        // newline that terminates the statement they are part of
        let (stmt, consumed_suite) = match self.current_token_kind() {
            TokenKind::Match => {
                let stmt = match self.classify_match_token() {
                    MatchTokenKind::Keyword => self.parse_match_statement(),
                    MatchTokenKind::KeywordOrIdentifier => self.try_parse_match_statement()?,
                    MatchTokenKind::Identifier => return None,
                };
                (Stmt::Match(stmt), true)
            }
            TokenKind::If => (Stmt::If(self.parse_if_statement()), true),
            TokenKind::For => (Stmt::For(self.parse_for_statement(start)), true),
            TokenKind::While => (Stmt::While(self.parse_while_statement()), true),
            TokenKind::Try => (Stmt::Try(self.parse_try_statement()), true),
            TokenKind::Raise => (Stmt::Raise(self.parse_raise_statement()), false),
            TokenKind::Return => (Stmt::Return(self.parse_return_statement()), false),
            // the loop escapes: `let first = next(it) ?? break` leaves the loop
            // when the call has no value to bind
            TokenKind::Break => (Stmt::Break(self.parse_break_statement()), false),
            TokenKind::Continue => (Stmt::Continue(self.parse_continue_statement()), false),
            // `parse_atom` only enters here at one of the tokens above; `match` is
            // the only one that can turn out not to start a statement expression
            _ => return None,
        };

        if consumed_suite {
            self.expr_consumed_suite = true;
        }

        Some(ast::ExprStatement {
            stmt: Box::new(stmt),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses an `assert` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `assert` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#the-assert-statement>
    fn parse_assert_statement(&mut self) -> ast::StmtAssert {
        let start = self.node_start();
        self.bump(TokenKind::Assert);

        // test_err assert_empty_test
        // assert

        // test_err assert_invalid_test_expr
        // assert *x
        // assert assert x
        // assert yield x
        // assert x := 1
        let test = self.parse_conditional_expression_or_higher();

        let msg = if self.eat(TokenKind::Comma) {
            if self.at_expr() {
                // test_err assert_invalid_msg_expr
                // assert False, *x
                // assert False, assert x
                // assert False, yield x
                // assert False, x := 1
                Some(Box::new(self.parse_conditional_expression_or_higher().expr))
            } else {
                // test_err assert_empty_msg
                // assert x,
                self.add_error(
                    ParseErrorType::ExpectedExpression,
                    self.current_token_range(),
                );
                None
            }
        } else {
            None
        };

        ast::StmtAssert {
            test: Box::new(test.expr),
            msg,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a global statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `global` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-global_stmt>
    fn parse_global_statement(&mut self) -> ast::StmtGlobal {
        let start = self.node_start();
        self.bump(TokenKind::Global);

        // test_err global_stmt_trailing_comma
        // global ,
        // global x,
        // global x, y,

        // test_err global_stmt_expression
        // global x + 1
        let names = self.parse_comma_separated_list_into_vec(
            RecoveryContextKind::Identifiers,
            Parser::parse_identifier,
        );

        if names.is_empty() {
            // test_err global_stmt_empty
            // global
            self.add_error(ParseErrorType::EmptyGlobalNames, self.current_token_range());
        }

        // test_ok global_stmt
        // global x
        // global x, y, z
        ast::StmtGlobal {
            range: self.node_range(start),
            names,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a nonlocal statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `nonlocal` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#grammar-token-python-grammar-nonlocal_stmt>
    fn parse_nonlocal_statement(&mut self) -> ast::StmtNonlocal {
        let start = self.node_start();
        self.bump(TokenKind::Nonlocal);

        // test_err nonlocal_stmt_trailing_comma
        // def _():
        //     nonlocal ,
        //     nonlocal x,
        //     nonlocal x, y,

        // test_err nonlocal_stmt_expression
        // def _():
        //     nonlocal x + 1
        let names = self.parse_comma_separated_list_into_vec(
            RecoveryContextKind::Identifiers,
            Parser::parse_identifier,
        );

        if names.is_empty() {
            // test_err nonlocal_stmt_empty
            // def _():
            //     nonlocal
            self.add_error(
                ParseErrorType::EmptyNonlocalNames,
                self.current_token_range(),
            );
        }

        // test_ok nonlocal_stmt
        // def _():
        //     nonlocal x
        //     nonlocal x, y, z
        ast::StmtNonlocal {
            range: self.node_range(start),
            names,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a type alias statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `type` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#the-type-statement>
    fn parse_type_alias_statement(&mut self) -> ast::StmtTypeAlias {
        let start = self.node_start();
        let type_range = self.current_token_range();
        self.bump(TokenKind::Type);

        // test_ok type_stmt_py312
        // # parse_options: {"target-version": "3.12"}
        // type x = int

        // test_err type_stmt_py311
        // # parse_options: {"target-version": "3.11"}
        // type x = int

        self.add_unsupported_syntax_error(
            UnsupportedSyntaxErrorKind::TypeAliasStatement,
            type_range,
        );

        let mut name = Expr::Name(self.parse_name(ExpressionContext::default()));
        helpers::set_expr_ctx(&mut name, ExprContext::Store);

        let type_params = self.try_parse_type_params();

        self.expect(TokenKind::Equal);

        // test_err type_alias_incomplete_stmt
        // type
        // type x
        // type x =

        // basedpython: `type X[...] = match S:` opens a match type — the alias's value is
        // chosen by matching `S` against the `case` patterns that follow. `match` is a soft
        // keyword, so an alias to a variable actually called `match` must keep parsing as
        // an ordinary alias; the speculative parse rewinds when no case block follows
        if self.options.is_basedpython
            && self.at(TokenKind::Match)
            && let Some((subject, cases)) = self.try_parse_type_match()
        {
            return ast::StmtTypeAlias {
                name: Box::new(name),
                type_params: type_params.map(Box::new),
                value: Box::new(subject),
                cases,
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
                is_private: false,
            };
        }

        // test_err type_alias_invalid_value_expr
        // type x = *y
        // type x = yield y
        // type x = yield from y
        // type x = x := 1
        let value = self.parse_conditional_expression_or_higher_impl(
            ExpressionContext::default().with_in_type_expression(),
        );

        ast::StmtTypeAlias {
            name: Box::new(name),
            type_params: type_params.map(Box::new),
            value: Box::new(value.expr),
            cases: Vec::new(),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            is_private: false,
        }
    }

    /// basedpython: parses the `match S:` … `case` blocks of a match type alias.
    ///
    /// Returns the subject expression and the case blocks, or `None` — with the parser
    /// rewound — when what follows `match` is not a match block after all, so the caller
    /// can parse `match` as an ordinary name.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `match` token.
    fn try_parse_type_match(&mut self) -> Option<(Expr, Vec<ast::MatchCase>)> {
        let checkpoint = self.checkpoint();

        self.bump(TokenKind::Match);
        let subject = self.parse_type_match_subject_expression();

        if !(self.at(TokenKind::Colon) && self.peek() == TokenKind::Newline) {
            self.rewind(checkpoint);
            return None;
        }

        self.bump(TokenKind::Colon);
        let cases = self.parse_match_body();

        for case in &cases {
            self.validate_type_match_case(case);
        }

        Some((subject, cases))
    }

    /// basedpython: parses the subject of a match type.
    ///
    /// Unlike a `match` statement's subject, a lone starred expression is allowed: matching
    /// over a type variable tuple — `match *Shape:` — is the whole point of the form.
    fn parse_type_match_subject_expression(&mut self) -> Expr {
        let start = self.node_start();
        let subject =
            self.parse_named_expression_or_higher(ExpressionContext::starred_bitwise_or());

        if self.at(TokenKind::Comma) {
            let tuple = self.parse_tuple_expression(subject.expr, start, Parenthesized::No, |p| {
                p.parse_named_expression_or_higher(ExpressionContext::starred_bitwise_or())
            });
            Expr::Tuple(tuple)
        } else {
            subject.expr
        }
    }

    /// basedpython: reports a match type case block that isn't a single type expression.
    ///
    /// A case body stands for the type the alias takes when the pattern matches, so it must
    /// be exactly one expression — there is nothing for a statement to do at the type level.
    fn validate_type_match_case(&mut self, case: &ast::MatchCase) {
        if let Some(guard) = &case.guard {
            self.add_error(
                ParseErrorType::OtherError(
                    "a match type case cannot have a guard; a type-level match decides on the \
                     pattern alone"
                        .to_string(),
                ),
                guard.as_ref(),
            );
        }

        match case.body.as_slice() {
            [ast::Stmt::Expr(_)] => {}
            [] => {}
            [first, rest @ ..] => {
                let range = rest
                    .last()
                    .map_or_else(|| first.range(), |last| first.range().cover(last.range()));
                self.add_error(
                    ParseErrorType::OtherError(
                        "a match type case body must be a single type expression".to_string(),
                    ),
                    range,
                );
            }
        }
    }

    /// Parses an IPython escape command at the statement level.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `IpyEscapeCommand` token.
    fn parse_ipython_escape_command_statement(&mut self) -> ast::StmtIpyEscapeCommand {
        let start = self.node_start();

        let (value, kind) = self.bump_ipython_escape_command(IpyEscapeContext::LogicalLineStart);

        let range = self.node_range(start);
        if self.options.mode != Mode::Ipython {
            self.add_error(ParseErrorType::UnexpectedIpythonEscapeCommand, range);
        }

        ast::StmtIpyEscapeCommand {
            range,
            kind,
            value,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an IPython help end escape command at the statement level.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `?` token.
    fn parse_ipython_help_end_escape_command_statement(
        &mut self,
        parsed_expr: &ParsedExpr,
    ) -> ast::StmtIpyEscapeCommand {
        // We are permissive than the original implementation because we would allow whitespace
        // between the expression and the suffix while the IPython implementation doesn't allow it.
        // For example, `foo ?` would be valid in our case but invalid for IPython.
        fn unparse_expr(parser: &mut Parser, expr: &Expr, buffer: &mut String) {
            match expr {
                Expr::Name(ast::ExprName { id, .. }) => {
                    buffer.push_str(id.as_str());
                }
                Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                    unparse_expr(parser, value, buffer);
                    buffer.push('[');

                    if let Expr::NumberLiteral(ast::ExprNumberLiteral {
                        value: ast::Number::Int(integer),
                        ..
                    }) = &**slice
                    {
                        let _ = write!(buffer, "{integer}");
                    } else {
                        parser.add_error(
                            ParseErrorType::OtherError(
                                "Only integer literals are allowed in subscript expressions \
                                    in help end escape command"
                                    .to_string(),
                            ),
                            slice.range(),
                        );
                        buffer.push_str(parser.src_text(slice.range()));
                    }

                    buffer.push(']');
                }
                Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                    unparse_expr(parser, value, buffer);
                    buffer.push('.');
                    buffer.push_str(attr.as_str());
                }
                _ => {
                    parser.add_error(
                        ParseErrorType::OtherError(
                            "Expected name, subscript or attribute expression \
                                in help end escape command"
                                .to_string(),
                        ),
                        expr,
                    );
                }
            }
        }

        let start = self.node_start();
        self.bump(TokenKind::Question);

        let kind = if self.eat(TokenKind::Question) {
            IpyEscapeKind::Help2
        } else {
            IpyEscapeKind::Help
        };

        if parsed_expr.is_parenthesized {
            let token_range = self.node_range(start);
            self.add_error(
                ParseErrorType::OtherError(
                    "Help end escape command cannot be applied on a parenthesized expression"
                        .to_string(),
                ),
                token_range,
            );
        }

        if self.at(TokenKind::Question) {
            self.add_error(
                ParseErrorType::OtherError(
                    "Maximum of 2 `?` tokens are allowed in help end escape command".to_string(),
                ),
                self.current_token_range(),
            );
        }

        let mut value = String::new();
        unparse_expr(self, &parsed_expr.expr, &mut value);

        ast::StmtIpyEscapeCommand {
            value: value.into_boxed_str(),
            kind,
            range: self.node_range(parsed_expr.start()),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parse an assignment statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `=` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#assignment-statements>
    fn parse_assign_statement(&mut self, target: ParsedExpr, start: TextSize) -> ast::StmtAssign {
        self.bump(TokenKind::Equal);

        let mut targets = vec![target.expr];

        // test_err assign_stmt_missing_rhs
        // x =
        // 1 + 1
        // x = y =
        // 2 + 2
        // x = = y
        // 3 + 3

        // test_err assign_stmt_keyword_target
        // a = pass = c
        // a + b
        // a = b = pass = c
        // a + b

        // test_err assign_stmt_invalid_value_expr
        // x = (*a and b,)
        // x = (42, *yield x)
        // x = (42, *yield from x)
        // x = (*lambda x: x,)
        // x = x := 1

        let mut value =
            self.parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or());

        if self.at(TokenKind::Equal) {
            // This path is only taken when there are more than one assignment targets.
            self.parse_list(RecoveryContextKind::AssignmentTargets, |parser| {
                parser.bump(TokenKind::Equal);

                let mut parsed_expr =
                    parser.parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or());

                std::mem::swap(&mut value, &mut parsed_expr);

                targets.push(parsed_expr.expr);
            });
        }

        // basedpython: `a = f:` + suite — the block is the assignment's value, so
        // what the target binds is the call the block stands for
        value = self.try_parse_trailing_lambda_value(value, &targets);

        for target in &mut targets {
            helpers::set_expr_ctx(target, ExprContext::Store);
            // test_err assign_stmt_invalid_target
            // 1 = 1
            // x = 1 = 2
            // x = 1 = y = 2 = z
            // ["a", "b"] = ["a", "b"]
            self.validate_assignment_target(target);
        }

        ast::StmtAssign {
            targets,
            value: Box::new(value.expr),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        }
    }

    /// Parses an annotated assignment statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `:` token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#annotated-assignment-statements>
    fn parse_annotated_assignment_statement(
        &mut self,
        mut target: ParsedExpr,
        start: TextSize,
    ) -> ast::StmtAnnAssign {
        self.bump(TokenKind::Colon);

        // test_err ann_assign_stmt_invalid_target
        // "abc": str = "def"
        // call(): str = "no"
        // *x: int = 1, 2
        // # Tuple assignment
        // x,: int = 1
        // x, y: int = 1, 2
        // (x, y): int = 1, 2
        // # List assignment
        // [x]: int = 1
        // [x, y]: int = 1, 2
        self.validate_annotated_assignment_target(&target.expr);

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

        // test_ok ann_assign_stmt_simple_target
        // a: int  # simple
        // (a): int
        // a.b: int
        // a[0]: int
        let simple = target.is_name_expr() && !target.is_parenthesized;

        // test_err ann_assign_stmt_invalid_annotation
        // x: *int = 1
        // x: yield a = 1
        // x: yield from b = 1
        // x: y := int = 1

        // test_err ann_assign_stmt_type_alias_annotation
        // a: type X = int
        // lambda: type X = int
        let annotation = self.parse_conditional_expression_or_higher_impl(
            ExpressionContext::default().with_in_type_expression(),
        );

        let value = if self.eat(TokenKind::Equal) {
            if self.at_expr() {
                // test_err ann_assign_stmt_invalid_value
                // x: Any = *a and b
                // x: Any = x := 1
                // x: list = [x, *a | b, *a or b]
                let value =
                    self.parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or());
                // basedpython: `a: T = f:` + suite, as for a plain assignment
                Some(Box::new(
                    self.try_parse_trailing_lambda_value(value, std::slice::from_ref(&target.expr))
                        .expr,
                ))
            } else {
                // test_err ann_assign_stmt_missing_rhs
                // x: int =
                self.add_error(
                    ParseErrorType::ExpectedExpression,
                    self.current_token_range(),
                );
                None
            }
        } else {
            None
        };

        ast::StmtAnnAssign {
            target: Box::new(target.expr),
            annotation: Box::new(annotation.expr),
            value,
            simple,
            is_context: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
            decorator_list: DecoratorList::new(),
        }
    }

    /// Parses an augmented assignment statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an augmented assignment token.
    ///
    /// See: <https://docs.python.org/3/reference/simple_stmts.html#augmented-assignment-statements>
    fn parse_augmented_assignment_statement(
        &mut self,
        mut target: ParsedExpr,
        op: Operator,
        start: TextSize,
    ) -> ast::StmtAugAssign {
        // Consume the operator
        self.bump_ts(AUGMENTED_ASSIGN_SET);

        if !matches!(
            &target.expr,
            Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_)
        ) {
            // test_err aug_assign_stmt_invalid_target
            // 1 += 1
            // "a" += "b"
            // *x += 1
            // pass += 1
            // x += pass
            // (x + y) += 1
            self.add_error(ParseErrorType::InvalidAugmentedAssignmentTarget, &target);
        }

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

        // test_err aug_assign_stmt_missing_rhs
        // x +=
        // 1 + 1
        // x += y +=
        // 2 + 2

        // test_err aug_assign_stmt_invalid_value
        // x += *a and b
        // x += *yield x
        // x += *yield from x
        // x += *lambda x: x
        // x += y := 1
        let value = self.parse_expression_list(ExpressionContext::yield_or_starred_bitwise_or());

        ast::StmtAugAssign {
            target: Box::new(target.expr),
            op,
            value: Box::new(value.expr),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an `if` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `if` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-if-statement>
    fn parse_if_statement(&mut self) -> ast::StmtIf {
        let start = self.node_start();
        self.bump(TokenKind::If);

        // test_err if_stmt_invalid_test_expr
        // if *x: ...
        // if yield x: ...
        // if yield from x: ...

        // test_err if_stmt_missing_test
        // if : ...
        let pattern = self.parse_if_let_pattern();
        let test = self.parse_named_expression_or_higher(ExpressionContext::default());

        // test_err if_stmt_missing_colon
        // if x
        // if x
        //     pass
        // a = 1
        self.expect(TokenKind::Colon);

        // test_err if_stmt_empty_body
        // if True:
        // 1 + 1
        let body = self.parse_body(Clause::If);

        // test_err if_stmt_misspelled_elif
        // if True:
        //     pass
        // elf:
        //     pass
        // else:
        //     pass
        let elif_else_snapshot = self.elif_else_scratch.snapshot();
        self.parse_clauses(Clause::ElIf, |parser| {
            let clause = parser.parse_elif_or_else_clause(ElifOrElse::Elif);
            parser.elif_else_scratch.push(clause);
        });

        if self.at(TokenKind::Else) {
            let clause = self.parse_elif_or_else_clause(ElifOrElse::Else);
            self.elif_else_scratch.push(clause);
        }

        ast::StmtIf {
            pattern: pattern.map(Box::new),
            test: Box::new(test.expr),
            body,
            elif_else_clauses: self.elif_else_scratch.take(elif_else_snapshot),
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// basedpython: reports `let <pattern> = <subject>` — the binding operator
    /// written as `=`.
    ///
    /// `if let P = v` is how Rust spells this, so it is the first thing a reader
    /// coming from there types. Without this the parse just falls apart at the
    /// `=` and reports three unrelated things, none of them the actual mistake
    fn error_if_let_uses_plain_equals(&mut self, pattern_end: TextSize) -> bool {
        if !self.at(TokenKind::Equal) {
            return false;
        }
        self.add_error(
            ParseErrorType::OtherError(
                "A destructuring `let` binds with `:=`, not `=`".to_string(),
            ),
            TextRange::new(pattern_end, self.current_token_range().end()),
        );
        true
    }

    /// Parses the `let <pattern> :=` prefix of a basedpython pattern-matching
    /// `if` / `elif` clause, leaving the parser positioned at the subject
    /// expression. Returns `None` — with the parser rewound — when the clause is
    /// an ordinary condition.
    ///
    /// `let` is an ordinary identifier in python (`if let := f():` is valid), so
    /// the form is only committed to once a complete pattern followed by `:=` has
    /// been parsed. No such sequence is a valid python condition, which is why
    /// the basedpython gate below can be applied after the fact
    fn parse_if_let_pattern(&mut self) -> Option<Pattern> {
        if !(self.at(TokenKind::Name) && self.src_text(self.current_token_range()) == "let") {
            return None;
        }

        let checkpoint = self.checkpoint();
        let let_range = self.current_token_range();
        self.bump(TokenKind::Name);

        if !self.at_pattern_start() {
            self.rewind(checkpoint);
            return None;
        }

        let pattern = self.parse_match_patterns();

        if !self.eat(TokenKind::ColonEqual) {
            if self.options.is_basedpython && self.error_if_let_uses_plain_equals(pattern.end()) {
                self.bump(TokenKind::Equal);
                return Some(pattern);
            }
            self.rewind(checkpoint);
            return None;
        }

        // test_err if_let_in_python_file
        // if let Some(x) := opt:
        //     pass
        self.error_if_not_basedpython_at(
            "pattern-matching `if let` is not valid in .py files".to_string(),
            TextRange::new(let_range.start(), pattern.end()),
        );

        Some(pattern)
    }

    /// basedpython: the synthetic binder holding the value `pattern`
    /// destructures, as an expression.
    ///
    /// It is zero-width at the pattern's start: the pattern is what the source
    /// wrote, and what every tool that reads ranges should see there.
    fn destructure_binder(&mut self, pattern: &Pattern, ctx: ExprContext) -> Expr {
        Expr::Name(ast::ExprName {
            id: self.next_destructure_binder_name(),
            ctx,
            range: TextRange::empty(pattern.start()),
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// [`Parser::destructure_binder`] as the identifier naming a parameter.
    fn destructure_binder_identifier(&mut self, pattern: &Pattern) -> ast::Identifier {
        ast::Identifier {
            id: self.next_destructure_binder_name(),
            range: TextRange::empty(pattern.start()),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Names the next binder. Counting them in source order — rather than
    /// deriving the name from an offset — keeps a reformatted file parsing to
    /// the same tree, and the count is rewound along with everything else when a
    /// speculative parse is abandoned
    fn next_destructure_binder_name(&mut self) -> Name {
        let index = self.destructure_binders;
        self.destructure_binders += 1;
        ast::destructure_binder_name(index)
    }

    /// Parses a basedpython destructuring statement,
    /// `let <pattern> := <subject>`, with an optional `else` block.
    ///
    /// Like the `if let` clause this is only committed to once a whole pattern
    /// followed by `:=` has been parsed: `let` stays an ordinary identifier
    /// everywhere else, and `let NAME [: T] = value` is the unrelated declaration
    /// form handled by [`Parser::try_parse_modifier_or_introducer`]. Returns
    /// `None` — with the parser rewound — when this is neither.
    fn try_parse_let_statement(&mut self) -> Option<ast::StmtLet> {
        let start = self.node_start();
        let checkpoint = self.checkpoint();
        self.bump(TokenKind::Name);

        if !self.at_pattern_start() {
            self.rewind(checkpoint);
            return None;
        }

        let pattern = self.parse_match_patterns();

        if !self.eat(TokenKind::ColonEqual) {
            if self.options.is_basedpython && self.error_if_let_uses_plain_equals(pattern.end()) {
                self.bump(TokenKind::Equal);
            } else {
                self.rewind(checkpoint);
                return None;
            }
        }

        // test_err let_stmt_in_python_file
        // let Point(x, y) := origin
        self.error_if_not_basedpython_at(
            "a destructuring `let` is not valid in .py files".to_string(),
            TextRange::new(start, pattern.end()),
        );

        let value = self.parse_expression_list(ExpressionContext::default());

        // test_err let_stmt_else_missing_colon
        // let Point(x, y) := origin else
        //     return
        let orelse = if self.eat(TokenKind::Else) {
            self.expect(TokenKind::Colon);
            self.parse_body(Clause::Else)
        } else {
            Suite::new()
        };

        Some(ast::StmtLet {
            pattern: Box::new(pattern),
            value: Box::new(value.expr),
            orelse,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        })
    }

    /// Parses an `elif` or `else` clause.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `elif` or `else` token.
    fn parse_elif_or_else_clause(&mut self, kind: ElifOrElse) -> ast::ElifElseClause {
        let start = self.node_start();
        self.bump(kind.as_token_kind());

        let (pattern, test) = if kind.is_elif() {
            // test_err if_stmt_invalid_elif_test_expr
            // if x:
            //     pass
            // elif *x:
            //     pass
            // elif yield x:
            //     pass
            let pattern = self.parse_if_let_pattern();
            (
                pattern,
                Some(
                    self.parse_named_expression_or_higher(ExpressionContext::default())
                        .expr,
                ),
            )
        } else {
            (None, None)
        };

        // test_err if_stmt_elif_missing_colon
        // if x:
        //     pass
        // elif y
        //     pass
        // else:
        //     pass
        self.expect(TokenKind::Colon);

        let body = self.parse_body(kind.as_clause());

        ast::ElifElseClause {
            pattern: pattern.map(Box::new),
            test,
            body,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a `try` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `try` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-try-statement>
    fn parse_try_statement(&mut self) -> ast::StmtTry {
        let try_start = self.node_start();
        self.bump(TokenKind::Try);
        self.expect(TokenKind::Colon);

        let mut is_star: Option<bool> = None;

        let try_body = self.parse_body(Clause::Try);

        let has_except = self.at(TokenKind::Except);

        // test_err try_stmt_mixed_except_kind
        // try:
        //     pass
        // except:
        //     pass
        // except* ExceptionGroup:
        //     pass
        // try:
        //     pass
        // except* ExceptionGroup:
        //     pass
        // except:
        //     pass
        // try:
        //     pass
        // except:
        //     pass
        // except:
        //     pass
        // except* ExceptionGroup:
        //     pass
        // except* ExceptionGroup:
        //     pass
        let mut mixed_except_ranges = Vec::new();
        let mut handlers = Vec::new();
        self.parse_clauses(Clause::Except, |p| {
            let (handler, kind) = p.parse_except_clause();
            if let ExceptClauseKind::Star(range) = kind {
                p.add_unsupported_syntax_error(UnsupportedSyntaxErrorKind::ExceptStar, range);
            }
            if is_star.is_none() {
                is_star = Some(kind.is_star());
            } else if is_star != Some(kind.is_star()) {
                mixed_except_ranges.push(handler.range());
            }
            if handlers.is_empty() {
                handlers.reserve_exact(1);
            }
            handlers.push(handler);
        });
        handlers.shrink_to_fit();

        // Empty handler has `is_star` false.
        let is_star = is_star.unwrap_or_default();
        for handler_err_range in mixed_except_ranges {
            self.add_error(
                ParseErrorType::OtherError(
                    "Cannot have both 'except' and 'except*' on the same 'try'".to_string(),
                ),
                handler_err_range,
            );
        }

        // test_err try_stmt_misspelled_except
        // try:
        //     pass
        // exept:  # spellchecker:disable-line
        //     pass
        // finally:
        //     pass
        // a = 1
        // try:
        //     pass
        // except:
        //     pass
        // exept:  # spellchecker:disable-line
        //     pass
        // b = 1

        let orelse = if self.eat(TokenKind::Else) {
            self.expect(TokenKind::Colon);
            self.parse_body(Clause::Else)
        } else {
            Suite::new()
        };

        let (finalbody, has_finally) = if self.eat(TokenKind::Finally) {
            self.expect(TokenKind::Colon);
            (self.parse_body(Clause::Finally), true)
        } else {
            (Suite::new(), false)
        };

        if !has_except && !has_finally {
            // test_err try_stmt_missing_except_finally
            // try:
            //     pass
            // try:
            //     pass
            // else:
            //     pass
            self.add_error(
                ParseErrorType::OtherError(
                    "Expected `except` or `finally` after `try` block".to_string(),
                ),
                self.current_token_range(),
            );
        }

        if has_finally && self.at(TokenKind::Else) {
            // test_err try_stmt_invalid_order
            // try:
            //     pass
            // finally:
            //     pass
            // else:
            //     pass
            self.add_error(
                ParseErrorType::OtherError(
                    "`else` block must come before `finally` block".to_string(),
                ),
                self.current_token_range(),
            );
        }

        // test_ok except_star_py311
        // # parse_options: {"target-version": "3.11"}
        // try: ...
        // except* ValueError: ...

        // test_err except_star_py310
        // # parse_options: {"target-version": "3.10"}
        // try: ...
        // except* ValueError: ...
        // except* KeyError: ...
        // except    *     Error: ...

        ast::StmtTry {
            body: try_body,
            handlers,
            orelse,
            finalbody,
            is_star,
            range: self.node_range(try_start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses an `except` clause of a `try` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `except` token.
    fn parse_except_clause(&mut self) -> (ExceptHandler, ExceptClauseKind) {
        let start = self.node_start();
        self.bump(TokenKind::Except);

        let star_token_range = self.current_token_range();
        let block_kind = if self.eat(TokenKind::Star) {
            ExceptClauseKind::Star(star_token_range)
        } else {
            ExceptClauseKind::Normal
        };

        let type_ = if self.at_expr() {
            // test_err except_stmt_invalid_expression
            // try:
            //     pass
            // except yield x:
            //     pass
            // try:
            //     pass
            // except* *x:
            //     pass
            let parsed_expr = self.parse_expression_list(ExpressionContext::default());
            if matches!(
                parsed_expr.expr,
                Expr::Tuple(ast::ExprTuple {
                    parenthesized: false,
                    is_anon_named_tuple: false,
                    is_anon_named_tuple_value: false,
                    callable_shape: None,
                    ..
                })
            ) {
                if self.at(TokenKind::As) {
                    // test_err except_stmt_unparenthesized_tuple_as
                    // try:
                    //     pass
                    // except x, y as exc:
                    //     pass
                    // try:
                    //     pass
                    // except* x, y as eg:
                    //     pass
                    self.add_error(
                        ParseErrorType::OtherError(
                            "Multiple exception types must be parenthesized when using `as`"
                                .to_string(),
                        ),
                        &parsed_expr,
                    );
                } else {
                    // test_err except_stmt_unparenthesized_tuple_no_as_py313
                    // # parse_options: {"target-version": "3.13"}
                    // try:
                    //     pass
                    // except x, y:
                    //     pass
                    // try:
                    //     pass
                    // except* x, y:
                    //     pass

                    // test_ok except_stmt_unparenthesized_tuple_no_as_py314
                    // # parse_options: {"target-version": "3.14"}
                    // try:
                    //     pass
                    // except x, y:
                    //     pass
                    // try:
                    //     pass
                    // except* x, y:
                    //     pass
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::UnparenthesizedExceptionTypes,
                        parsed_expr.range(),
                    );
                }
            }
            Some(Box::new(parsed_expr.expr))
        } else {
            if block_kind.is_star() || self.at(TokenKind::As) {
                // test_err except_stmt_missing_exception
                // try:
                //     pass
                // except as exc:
                //     pass
                // # If a '*' is present then exception type is required
                // try:
                //     pass
                // except*:
                //     pass
                // except*
                //     pass
                // except* as exc:
                //     pass
                self.add_error(
                    ParseErrorType::OtherError("Expected one or more exception types".to_string()),
                    self.current_token_range(),
                );
            }
            None
        };

        let name = if self.eat(TokenKind::As) {
            if self.at_name_or_soft_keyword() {
                // test_ok except_stmt_as_name_soft_keyword
                // try: ...
                // except Exception as match: ...
                // except Exception as case: ...
                // except Exception as type: ...
                Some(self.parse_identifier())
            } else {
                // test_err except_stmt_missing_as_name
                // try:
                //     pass
                // except Exception as:
                //     pass
                // except Exception as
                //     pass
                self.add_error(
                    ParseErrorType::OtherError("Expected name after `as`".to_string()),
                    self.current_token_range(),
                );
                None
            }
        } else {
            None
        };

        // test_err except_stmt_missing_exception_and_as_name
        // try:
        //     pass
        // except as:
        //     pass

        self.expect(TokenKind::Colon);

        let except_body = self.parse_body(Clause::Except);

        (
            ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                type_,
                name,
                body: except_body,
                range: self.node_range(start),
                node_index: AtomicNodeIndex::NONE,
            }),
            block_kind,
        )
    }

    /// Parses a `for` statement.
    ///
    /// The given `start` offset is the start of either the `for` token or the
    /// `async` token if it's an async for statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `for` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-for-statement>
    fn parse_for_statement(&mut self, start: TextSize) -> ast::StmtFor {
        self.bump(TokenKind::For);

        // test_err for_stmt_missing_target
        // for in x: ...

        // test_ok for_in_target_valid_expr
        // for d[x in y] in target: ...
        // for (x in y)[0] in iter: ...
        // for (x in y).attr in iter: ...

        // test_err for_stmt_invalid_target_in_keyword
        // for d(x in y) in target: ...
        // for (x in y)() in iter: ...
        // for (x in y) in iter: ...
        // for (x in y, z) in iter: ...
        // for [x in y, z] in iter: ...
        // for {x in y, z} in iter: ...

        // test_err for_stmt_invalid_target_binary_expr
        // for x not in y in z: ...
        // for x == y in z: ...
        // for x or y in z: ...
        // for -x in y: ...
        // for not x in y: ...
        // for x | y in z: ...
        let target_checkpoint = self.checkpoint();
        let mut target =
            self.parse_expression_list(ExpressionContext::starred_conditional().with_in_excluded());

        // basedpython: a loop target that cannot be assigned to may be a
        // destructuring pattern instead. The ordinary parse runs first and wins
        // whenever it produced something assignable, so no loop that python
        // accepts changes meaning here
        let pattern = if self.options.is_basedpython && !is_assignment_target(&target.expr) {
            self.rewind(target_checkpoint);
            let pattern = self.parse_destructure_pattern(
                AllowSequencePattern::Yes,
                TokenSet::new([TokenKind::In]),
            );
            match pattern {
                Some(pattern) => {
                    // test_err for_stmt_destructure_in_python_file
                    // for Point(x, y) in points: ...
                    self.error_if_not_basedpython_at(
                        "a destructuring `for` target is not valid in .py files".to_string(),
                        pattern.range(),
                    );
                    target.expr = self.destructure_binder(&pattern, ExprContext::Store);
                    Some(Box::new(pattern))
                }
                None => {
                    // not a pattern either: replay the target parse so its own
                    // errors are the ones reported. A checkpoint can only be
                    // rewound to, so the attempt has to be paid for twice
                    target = self.parse_expression_list(
                        ExpressionContext::starred_conditional().with_in_excluded(),
                    );
                    None
                }
            }
        } else {
            None
        };

        if pattern.is_none() {
            helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

            // test_err for_stmt_invalid_target
            // for 1 in x: ...
            // for "a" in x: ...
            // for *x and y in z: ...
            // for *x | y in z: ...
            // for await x in z: ...
            // for yield x in y: ...
            // for [x, 1, y, *["a"]] in z: ...
            self.validate_assignment_target(&target.expr);
        }

        // test_err for_stmt_missing_in_keyword
        // for a b: ...
        // for a: ...
        self.expect(TokenKind::In);

        // test_err for_stmt_missing_iter
        // for x in:
        //     a = 1

        // test_err for_stmt_invalid_iter_expr
        // for x in *a and b: ...
        // for x in yield a: ...
        // for target in x := 1: ...
        let iter = self.parse_expression_list(ExpressionContext::starred_bitwise_or());

        // test_ok for_iter_unpack_py39
        // # parse_options: {"target-version": "3.9"}
        // for x in *a,  b: ...
        // for x in  a, *b: ...
        // for x in *a, *b: ...

        // test_ok for_iter_unpack_py38
        // # parse_options: {"target-version": "3.8"}
        // for x in (*a,  b): ...
        // for x in ( a, *b): ...
        // for x in (*a, *b): ...

        // test_err for_iter_unpack_py38
        // # parse_options: {"target-version": "3.8"}
        // for x in *a,  b: ...
        // for x in  a, *b: ...
        // for x in *a, *b: ...
        self.check_tuple_unpacking(
            &iter,
            UnsupportedSyntaxErrorKind::UnparenthesizedUnpackInFor,
        );

        self.expect(TokenKind::Colon);

        let body = self.parse_body(Clause::For);

        let orelse = if self.eat(TokenKind::Else) {
            self.expect(TokenKind::Colon);
            self.parse_body(Clause::Else)
        } else {
            Suite::new()
        };

        ast::StmtFor {
            target: Box::new(target.expr),
            pattern,
            iter: Box::new(iter.expr),
            is_async: false,
            body,
            orelse,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a `while` statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `while` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-while-statement>
    fn parse_while_statement(&mut self) -> ast::StmtWhile {
        let start = self.node_start();
        self.bump(TokenKind::While);

        // test_err while_stmt_missing_test
        // while : ...
        // while :
        //     a = 1

        // test_err while_stmt_invalid_test_expr
        // while *x: ...
        // while yield x: ...
        // while a, b: ...
        // while a := 1, b: ...
        let test = self.parse_named_expression_or_higher(ExpressionContext::default());

        // test_err while_stmt_missing_colon
        // while (
        //     a < 30 # comment
        // )
        //     pass
        self.expect(TokenKind::Colon);

        let body = self.parse_body(Clause::While);

        let orelse = if self.eat(TokenKind::Else) {
            self.expect(TokenKind::Colon);
            self.parse_body(Clause::Else)
        } else {
            Suite::new()
        };

        ast::StmtWhile {
            test: Box::new(test.expr),
            body,
            orelse,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a function definition.
    ///
    /// The given `start` offset is the start of either of the following:
    /// - `def` token
    /// - `async` token if it's an asynchronous function definition with no decorators
    /// - `@` token if the function definition has decorators
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `def` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#function-definitions>
    fn parse_function_definition(
        &mut self,
        decorator_list: DecoratorList,
        start: TextSize,
    ) -> ast::StmtFunctionDef {
        self.bump(TokenKind::Def);

        // test_err function_def_missing_identifier
        // def (): ...
        // def () -> int: ...
        let name = self.parse_identifier();

        // test_err function_def_unclosed_type_param_list
        // def foo[T1, *T2(a, b):
        //     return a + b
        // x = 10
        let type_params = self.try_parse_type_params();

        // test_ok function_type_params_py312
        // # parse_options: {"target-version": "3.12"}
        // def foo[T](): ...

        // test_err function_type_params_py311
        // # parse_options: {"target-version": "3.11"}
        // def foo[T](): ...
        // def foo[](): ...
        if let Some(ast::TypeParams { range, .. }) = &type_params {
            self.add_unsupported_syntax_error(
                UnsupportedSyntaxErrorKind::TypeParameterList,
                *range,
            );
        }

        // test_ok function_def_parameter_range
        // def foo(
        //     first: int,
        //     second: int,
        // ) -> int: ...

        // test_err function_def_unclosed_parameter_list
        // def foo(a: int, b:
        // def foo():
        //     return 42
        // def foo(a: int, b: str
        // x = 10
        let mut parameters = self.parse_parameters(FunctionKind::FunctionDef);
        // basedpython: each `some T` parameter contributes an anonymous type parameter named after
        // it. synthesizing a real `TypeParamTypeVar` here means the hole is scoped, resolved,
        // solved, and lowered by the machinery a written `[...]` entry already goes through
        let type_params = Self::with_some_holes(type_params, &mut parameters);

        let mut is_asserts_return = false;
        let returns = if self.eat(TokenKind::Rarrow) {
            // basedpython `def f(x) -> asserts x` / `-> asserts not x`: the return
            // annotation is an assertion guard, not a type. the keyword is consumed
            // here and recorded on the function; the asserted expression takes the
            // place of the annotation. a bare `-> asserts` is still the type `asserts`
            if self.at(TokenKind::Name)
                && self.src_text(self.current_token_range()) == "asserts"
                && (EXPR_SET.contains(self.peek()) || self.peek().is_soft_keyword())
            {
                self.error_if_not_basedpython(
                    "`asserts` return annotations are not valid in .py files".to_string(),
                );
                self.bump(TokenKind::Name);
                is_asserts_return = true;
            }

            if self.at_expr() {
                // test_ok function_def_valid_return_expr
                // def foo() -> int | str: ...
                // def foo() -> lambda x: x: ...
                // def foo() -> int if True else str: ...

                // test_err function_def_invalid_return_expr
                // def foo() -> *int: ...
                // def foo() -> (*int): ...
                // def foo() -> yield x: ...
                let returns = self
                    .parse_expression_list(ExpressionContext::default().with_in_type_expression());

                if matches!(
                    returns.expr,
                    Expr::Tuple(ast::ExprTuple {
                        parenthesized: false,
                        is_anon_named_tuple: false,
                        is_anon_named_tuple_value: false,
                        callable_shape: None,
                        ..
                    })
                ) {
                    // test_ok function_def_parenthesized_return_types
                    // def foo() -> (int,): ...
                    // def foo() -> (int, str): ...

                    // test_err function_def_unparenthesized_return_types
                    // def foo() -> int,: ...
                    // def foo() -> int, str: ...
                    self.add_error(
                        ParseErrorType::OtherError(
                            "Multiple return types must be parenthesized".to_string(),
                        ),
                        returns.range(),
                    );
                }

                Some(Box::new(returns.expr))
            } else {
                // test_err function_def_missing_return_type
                // def foo() -> : ...
                self.add_error(
                    ParseErrorType::ExpectedExpression,
                    self.current_token_range(),
                );

                None
            }
        } else {
            None
        };

        // basedpython `def f() -> int raises TypeError`: the declared exception
        // set. an ordinary type expression — `Never` cannot raise, `...` opts out
        // of tracking, `A | B` is a union — so no tuple form is accepted here
        let raises =
            if self.at(TokenKind::Name) && self.src_text(self.current_token_range()) == "raises" {
                self.error_if_not_basedpython(
                    "`raises` clauses are not valid in .py files".to_string(),
                );
                self.bump(TokenKind::Name);

                if self.at_expr() {
                    Some(Box::new(self.parse_conditional_expression_or_higher().expr))
                } else {
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );

                    None
                }
            } else {
                None
            };

        // basedpython: `def f(a: int) -> int` (no colon, no body) is permitted as
        // a bodyless overload declaration. The `overload` transform adds `@overload`
        // to consecutive bodyless defs with the same name and a `: ...` stub body.
        // recovery parity with upstream ruff: if the colon is missing but the next
        // line is indented, parse it as the body so references inside resolve to
        // parameters (matches `def f(x\n    return x`-style truncated headers)
        let body = if self.eat(TokenKind::Colon) {
            // test_err function_def_empty_body
            // def foo():
            // def foo() -> int:
            // x = 42
            self.parse_body(Clause::FunctionDef)
        } else if self.at(TokenKind::Newline) && self.peek() == TokenKind::Indent {
            self.add_error(
                ParseErrorType::OtherError("Expected `:` after function header".to_string()),
                self.current_token_range(),
            );
            self.parse_body(Clause::FunctionDef)
        } else {
            self.error_if_not_basedpython(
                "function declarations without a body are not valid in .py files".to_string(),
            );
            self.eat(TokenKind::Newline);
            Suite::new()
        };

        ast::StmtFunctionDef {
            name,
            type_params: type_params.map(Box::new),
            parameters: Box::new(parameters),
            body,
            decorator_list,
            is_async: false,
            returns,
            raises,
            is_trailing_lambda: false,
            is_asserts_return,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses `init(...)` — basedpython shorthand for `def __init__(...)` inside a
    /// class body. The keyword text is captured by a synthetic `__init_method__`
    /// decorator; the `init_method` transform replaces it with `def __init__` and
    /// promotes any `let` parameters into `self.<name>: <ann> = <name>` body lines.
    ///
    /// The function is named `__init__` directly so ty's semantic analysis (which
    /// scans `__init__` body for `self.X = ...` assignments) sees the synthesised
    /// body statements created from each `let` parameter
    fn parse_init_method(&mut self, start: TextSize) -> ast::StmtFunctionDef {
        let init_range = self.current_token_range();
        self.bump(TokenKind::Name); // consume "init"

        let decorator = ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: Name::new_static("__init_method__"),
                ctx: ExprContext::Invalid,
                range: init_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: init_range,
            node_index: AtomicNodeIndex::NONE,
        };

        let name = ast::Identifier {
            id: Name::new_static("__init__"),
            range: init_range,
            node_index: AtomicNodeIndex::NONE,
        };

        let mut parameters = self.parse_parameters(FunctionKind::FunctionDef);
        let params_end = parameters.range().end();

        // basedpython: every parameter of an `init(...)` becomes a field of the
        // same name, and a pattern has no name to make one of. It also has no
        // body of its own to destructure into
        for parameter in parameters
            .iter()
            .map(ast::AnyParameterRef::as_parameter)
            .filter(|parameter| parameter.pattern.is_some())
        {
            self.add_error(
                ParseErrorType::OtherError(
                    "A destructuring parameter is not valid in an `init(...)` shorthand: \
                     it has no name to make a field of"
                        .to_string(),
                ),
                parameter.range(),
            );
        }

        // `init(...)` implies `self` as the first parameter. inject a synthetic
        // (zero-width) `self` into the AST when the author omitted it, so ty
        // resolves `self` in the synthesised `self.<name> = <name>` assignments
        // and in the body. the `init_method` transform detects the same omission
        // from the source and performs the matching source-level insertion
        let has_self = parameters
            .posonlyargs
            .first()
            .map(|p| &p.parameter)
            .or_else(|| parameters.args.first().map(|p| &p.parameter))
            .is_some_and(|p| p.name.as_str() == "self");
        if !has_self {
            let self_param = synth_self_parameter(parameters.range.start() + TextSize::from(1u32));
            if parameters.posonlyargs.is_empty() {
                parameters.args.insert(0, self_param);
            } else {
                parameters.posonlyargs.insert(0, self_param);
            }
        }

        // synthesise `self.<name>: <ann> = <name>` for every attribute parameter
        // (`let` / `var`) and prepend to the body so ty's instance-attribute
        // analysis picks them up
        let synthetic_body = self.synthesize_let_assignments(&parameters);

        // bodyless form is permitted: `init(self, let a: int)` with no `:`
        let user_body = if self.eat(TokenKind::Colon) {
            self.parse_body(Clause::FunctionDef)
        } else {
            self.eat(TokenKind::Newline);
            Suite::new()
        };

        let body: Suite = synthetic_body.into_iter().chain(user_body).collect();

        ast::StmtFunctionDef {
            name,
            type_params: None,
            parameters: Box::new(parameters),
            body,
            decorator_list: vec![decorator].into(),
            is_async: false,
            // `init(...)` is a `__init__`, which returns `None`. synthesise the
            // annotation (zero-width, after the parameter list) so ty sees
            // `-> None` without it appearing in the source
            returns: Some(Box::new(Expr::NoneLiteral(ast::ExprNoneLiteral {
                range: TextRange::empty(params_end),
                node_index: AtomicNodeIndex::NONE,
            }))),
            is_trailing_lambda: false,
            is_asserts_return: false,
            raises: None,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Build the synthetic `self.<name>: <ann> = <name>` statements for each
    /// attribute-declaring parameter (one prefixed with `let` or `var`). The
    /// prefix is read back from the source span between the parameter node start
    /// and its name — the parser consumes the modifier keywords but does not
    /// record them on the `Parameter` node. A `private` prefix name-mangles the
    /// synthesised attribute to `self.__name`
    fn synthesize_let_assignments(&self, params: &ast::Parameters) -> Vec<Stmt> {
        let mut out = Vec::new();
        for p in &params.posonlyargs {
            self.maybe_synth_let_assign(&p.parameter, &mut out);
        }
        for p in &params.args {
            self.maybe_synth_let_assign(&p.parameter, &mut out);
        }
        if let Some(v) = &params.vararg {
            self.maybe_synth_let_assign(v, &mut out);
        }
        for p in &params.kwonlyargs {
            self.maybe_synth_let_assign(&p.parameter, &mut out);
        }
        if let Some(k) = &params.kwarg {
            self.maybe_synth_let_assign(k, &mut out);
        }
        out
    }

    fn maybe_synth_let_assign(&self, param: &ast::Parameter, out: &mut Vec<Stmt>) {
        let prefix_start = usize::from(param.range.start());
        let prefix_end = usize::from(param.name.range.start());
        let prefix = &self.source[prefix_start..prefix_end];
        if !param_prefix_declares_attribute(prefix) {
            return;
        }
        let name_range = param.name.range;
        let name_id = param.name.id.clone();
        // a `private` attribute is name-mangled (`self.__name`); the parameter
        // itself keeps its declared name, so the value read stays `name_id`
        let attr_id = if param_prefix_is_private(prefix) {
            Name::new(format!("__{name_id}"))
        } else {
            name_id.clone()
        };
        let self_expr = Expr::Name(ast::ExprName {
            id: Name::new_static("self"),
            ctx: ExprContext::Load,
            range: param.range,
            node_index: AtomicNodeIndex::NONE,
        });
        let attr_target = Expr::Attribute(ast::ExprAttribute {
            value: Box::new(self_expr),
            attr: ast::Identifier {
                id: attr_id,
                range: name_range,
                node_index: AtomicNodeIndex::NONE,
            },
            ctx: ExprContext::Store,
            range: param.range,
            node_index: AtomicNodeIndex::NONE,
            optional: false,
        });
        let value_expr = Expr::Name(ast::ExprName {
            id: name_id,
            ctx: ExprContext::Load,
            range: name_range,
            node_index: AtomicNodeIndex::NONE,
        });
        // `let` binds read-only state, exactly like a class-body `let x: T`, so the
        // synthesised declaration carries the same `__let__` marker. That is what
        // makes a class covariant in a type parameter it only stores: without it
        // the attribute reads as writable and pins the parameter invariant
        let let_range = param_prefix_let_range(
            self.source,
            TextRange::new(param.range.start(), param.name.range.start()),
        );
        let let_marker = |range| {
            Expr::Name(ast::ExprName {
                id: Name::new_static("__let__"),
                ctx: ExprContext::Invalid,
                range,
                node_index: AtomicNodeIndex::NONE,
            })
        };
        let annotation = match (&param.annotation, let_range) {
            // `let a: T` — the declared type rides in the marker's slice, as it
            // does for the class-body form
            (Some(ann), Some(let_range)) => Some(Box::new(Expr::Subscript(ast::ExprSubscript {
                range: TextRange::new(let_range.start(), ann.range().end()),
                value: Box::new(let_marker(let_range)),
                slice: ann.clone(),
                ctx: ExprContext::Load,
                node_index: AtomicNodeIndex::NONE,
                is_typeof: false,
                is_type_decoration: false,
            }))),
            // `let a` — a bare marker: read-only, with the type left to the value
            (None, Some(let_range)) => Some(Box::new(let_marker(let_range))),
            // `var a: T` — an ordinary, writable declaration
            (Some(ann), None) => Some(ann.clone()),
            // `var a` — no declaration at all
            (None, None) => None,
        };
        let stmt = match annotation {
            Some(annotation) => Stmt::AnnAssign(ast::StmtAnnAssign {
                target: Box::new(attr_target),
                annotation,
                value: Some(Box::new(value_expr)),
                simple: false,
                is_context: false,
                range: param.range,
                node_index: AtomicNodeIndex::NONE,
                decorator_list: DecoratorList::new(),
            }),
            None => Stmt::Assign(ast::StmtAssign {
                targets: vec![attr_target],
                value: Box::new(value_expr),
                range: param.range,
                node_index: AtomicNodeIndex::NONE,
                decorator_list: DecoratorList::new(),
            }),
        };
        out.push(stmt);
    }

    /// True when the token after the current `Indent` opens a property accessor
    /// block — one of the `get` / `set` / `field` entry keywords. An indent after
    /// a completed simple statement is otherwise always an error, so this is an
    /// unambiguous signal inside a class body.
    fn at_accessor_block_start(&mut self) -> bool {
        let (kind, range) = self.peek_nth(0);
        kind == TokenKind::Name
            && matches!(
                self.src_text(range),
                // `late` may lead the block as a prefix on `field`
                "get" | "set" | "field" | "late"
            )
    }

    /// Parses a basedpython property accessor block and lowers the whole
    /// construct to standard python `@property` members.
    ///
    /// The caller has parsed the `var` / `let` declaration (`decl`) and is
    /// positioned at the `Indent` that opens the accessor suite:
    ///
    /// ```text
    /// var age: int = 0
    ///     get() = field
    ///     set(value):
    ///         assert value >= 0
    ///         field = value
    /// ```
    ///
    /// Emits, in class-body order, a backing-field declaration (`_age: int = 0`,
    /// only when an accessor mentions `field`), a getter carrying the synthetic
    /// `__property__` marker whose range spans the whole construct, and — for a
    /// mutable `var` — a setter decorated `@<name>.setter`. `field` is rewritten
    /// to `self._<name>` so ty sees real backing storage and needs no special
    /// rule for the keyword. The declaration statement itself is dropped: the
    /// returned statement is the first synthesised member and the rest are
    /// drained by [`Parser::parse_block`].
    fn parse_property_accessors(&mut self, decl: Stmt, start: TextSize) -> Stmt {
        self.error_if_not_basedpython(
            "property accessor blocks are not valid in .py files".to_string(),
        );

        let Stmt::AnnAssign(ann) = &decl else {
            self.add_error(
                ParseErrorType::OtherError(
                    "a property accessor block must follow a `var` or `let` declaration"
                        .to_string(),
                ),
                self.current_token_range(),
            );
            return decl;
        };
        let Expr::Name(target) = ann.target.as_ref() else {
            self.add_error(
                ParseErrorType::OtherError(
                    "a property accessor block must follow a simple `var` or `let` name"
                        .to_string(),
                ),
                self.current_token_range(),
            );
            return decl;
        };
        let public_name = target.id.clone();
        let prop_name_range = target.range;
        let prop_type = property_decl_type(&ann.annotation);
        let prop_init = ann.value.as_deref().cloned();

        // the modifier prefix ahead of the name carries `let` / `var` and any
        // modifier keywords; the parser consumed them without recording them
        let prefix = &self.source[usize::from(start)..usize::from(prop_name_range.start())];
        let is_let = prefix.split_whitespace().any(|word| word == "let");
        let is_var = prefix.split_whitespace().any(|word| word == "var");
        let is_late = prefix.split_whitespace().any(|word| word == "late");
        // `static let x: T` + `get()` is a *class-level* computed property. python
        // has no such thing (chaining `classmethod` onto `property` was removed in
        // 3.13), so it lowers to a small descriptor instead of `property`. that
        // descriptor can only implement `__get__`: assigning through `A.x = v`
        // bypasses any `__set__`, which is why the mutable forms are rejected below
        let is_static = prefix.split_whitespace().any(|word| word == "static");
        if !is_let && !is_var {
            self.add_error(
                ParseErrorType::OtherError(
                    "a property accessor block requires a `var` or `let` declaration".to_string(),
                ),
                prop_name_range,
            );
        }
        if is_late && is_let {
            self.add_error(
                ParseErrorType::OtherError("`late` requires `var`".to_string()),
                prop_name_range,
            );
        }

        // `private` shifts the whole construct one level of underscore deeper: the
        // property becomes `_x` and its storage `__x`. that is self-enforcing —
        // the property simply does not exist under its public name, so an access
        // from outside the class is an unresolved attribute rather than something
        // needing its own check
        let is_private = prefix.split_whitespace().any(|word| word == "private");
        let prop_name = if is_private {
            Name::new(format!("_{public_name}"))
        } else {
            public_name.clone()
        };
        // storage is an implementation detail, so it gets a dunder name and python's
        // name mangling hides it: `self.__a` inside the class body resolves to
        // `_A__a`, and there is no `_a` for anything outside to reach. derived from
        // the *public* name so a `private` property (already `_x`) gets `__x` rather
        // than a third underscore
        let backing = Name::new(format!("__{public_name}"));

        // modifier keywords ty must see on the accessors themselves (`override`
        // checked against the base, `final`, `abstract`). they are appended *after*
        // the property marker so they decorate the accessor function and the
        // `property` wrapper stays outermost — all three are identity-returning, so
        // the result is still a property. each keeps its real source range, so the
        // `modifiers` pass's own narrow edit for it lands inside the span this
        // construct's lowering claims and is superseded there
        let modifier_markers: Vec<ast::Decorator> = {
            let base = usize::from(start);
            let mut markers = Vec::new();
            let mut offset = 0usize;
            for word in prefix.split_whitespace() {
                let Some(relative) = prefix[offset..].find(word) else {
                    continue;
                };
                let word_start = offset + relative;
                offset = word_start + word.len();
                if !matches!(word, "override" | "final" | "abstract") {
                    continue;
                }
                let (Ok(from), Ok(to)) = (
                    TextSize::try_from(base + word_start),
                    TextSize::try_from(base + offset),
                ) else {
                    continue;
                };
                let range = TextRange::new(from, to);
                markers.push(ast::Decorator {
                    expression: Expr::Name(ast::ExprName {
                        id: Name::new(word),
                        ctx: ExprContext::Invalid,
                        range,
                        node_index: AtomicNodeIndex::NONE,
                    }),
                    range,
                    node_index: AtomicNodeIndex::NONE,
                });
            }
            markers
        };

        // ---- parse the accessor suite -------------------------------------
        self.bump(TokenKind::Indent);

        let mut getter: Option<(Vec<Stmt>, TextRange)> = None;
        let mut setter: Option<(Vec<Stmt>, Option<ast::Identifier>, TextRange)> = None;
        let mut field_decl: Option<(Option<Expr>, Option<Expr>, TextRange)> = None;
        // `late field: T` declares storage with no initialiser at all — it must
        // not fall back to the property's own initialiser either
        let mut field_is_late = false;

        let mut progress = ParserProgress::default();
        while !self.at(TokenKind::Dedent) && !self.at(TokenKind::EndOfFile) {
            progress.assert_progressing(self);
            let mut kw_range = self.current_token_range();
            // `late field: T` defers the backing field's initialisation
            let field_late = self.at(TokenKind::Name) && self.src_text(kw_range) == "late";
            if field_late {
                self.bump(TokenKind::Name);
                kw_range = self.current_token_range();
            }
            let entry = if self.at(TokenKind::Name) {
                self.src_text(kw_range)
            } else {
                ""
            };
            if field_late && entry != "field" {
                self.add_error(
                    ParseErrorType::OtherError(
                        "`late` may only precede `field` in a property accessor block".to_string(),
                    ),
                    kw_range,
                );
            }
            match entry {
                "get" | "set" => {
                    let is_get = entry == "get";
                    self.bump(TokenKind::Name);
                    self.expect(TokenKind::Lpar);
                    let param = if self.at(TokenKind::Rpar) {
                        None
                    } else {
                        Some(self.parse_identifier())
                    };
                    self.expect(TokenKind::Rpar);

                    let body: Vec<Stmt> = if self.eat(TokenKind::Equal) {
                        // single-expression accessor: `get() = field * 2`
                        let expr = self.parse_conditional_expression_or_higher().expr;
                        let expr_range = expr.range();
                        self.eat(TokenKind::Semi);
                        self.eat(TokenKind::Newline);
                        if is_get {
                            vec![Stmt::Return(ast::StmtReturn {
                                value: Some(Box::new(expr)),
                                range: expr_range,
                                node_index: AtomicNodeIndex::NONE,
                            })]
                        } else {
                            vec![Stmt::Expr(ast::StmtExpr {
                                value: Box::new(expr),
                                range: expr_range,
                                node_index: AtomicNodeIndex::NONE,
                            })]
                        }
                    } else if self.eat(TokenKind::Colon) {
                        // multi-statement accessor: `set(value):` + block
                        self.parse_body(Clause::FunctionDef).into_iter().collect()
                    } else {
                        self.add_error(
                            ParseErrorType::OtherError(format!(
                                "expected `=` or `:` after `{entry}(...)` in a property accessor block"
                            )),
                            self.current_token_range(),
                        );
                        Vec::new()
                    };

                    let acc_range = TextRange::new(kw_range.start(), self.prev_token_end);
                    if is_get {
                        if getter.is_some() {
                            self.add_error(
                                ParseErrorType::OtherError(
                                    "duplicate `get` in a property accessor block".to_string(),
                                ),
                                kw_range,
                            );
                        }
                        getter = Some((body, acc_range));
                    } else {
                        if setter.is_some() {
                            self.add_error(
                                ParseErrorType::OtherError(
                                    "duplicate `set` in a property accessor block".to_string(),
                                ),
                                kw_range,
                            );
                        }
                        setter = Some((body, param, acc_range));
                    }
                }
                "field" => {
                    self.bump(TokenKind::Name);
                    let annotation = if self.eat(TokenKind::Colon) {
                        Some(self.parse_conditional_expression_or_higher().expr)
                    } else {
                        None
                    };
                    let init = if self.eat(TokenKind::Equal) {
                        Some(
                            self.parse_expression_list(
                                ExpressionContext::yield_or_starred_bitwise_or(),
                            )
                            .expr,
                        )
                    } else {
                        None
                    };
                    self.eat(TokenKind::Semi);
                    self.eat(TokenKind::Newline);
                    let decl_range = TextRange::new(kw_range.start(), self.prev_token_end);
                    if field_decl.is_some() {
                        self.add_error(
                            ParseErrorType::OtherError(
                                "duplicate `field` in a property accessor block".to_string(),
                            ),
                            kw_range,
                        );
                    }
                    if field_late && init.is_some() {
                        self.add_error(
                            ParseErrorType::OtherError(
                                "`late` cannot be combined with an initialiser".to_string(),
                            ),
                            decl_range,
                        );
                    }
                    field_is_late = field_late;
                    field_decl = Some((annotation, init, decl_range));
                }
                _ => {
                    self.add_error(
                        ParseErrorType::OtherError(
                            "expected `get`, `set`, or `field` in a property accessor block"
                                .to_string(),
                        ),
                        kw_range,
                    );
                    self.bump_any();
                }
            }
        }
        self.expect(TokenKind::Dedent);

        let construct_range = self.node_range(start);

        // ---- rewrite `field` and validate --------------------------------
        let rewriter = FieldRewriter {
            backing: backing.clone(),
            seen: std::cell::Cell::new(false),
        };
        {
            use ruff_python_ast::visitor::transformer::Transformer;
            if let Some((body, _)) = getter.as_mut() {
                for stmt in body.iter_mut() {
                    rewriter.visit_stmt(stmt);
                }
            }
            if let Some((body, _, _)) = setter.as_mut() {
                for stmt in body.iter_mut() {
                    rewriter.visit_stmt(stmt);
                }
            }
        }
        let references_field = rewriter.seen.get();

        if is_let && setter.is_some() {
            self.add_error(
                ParseErrorType::OtherError("read-only property cannot define a setter".to_string()),
                construct_range,
            );
        }
        // an explicit `field` declaration is a complete property on its own — the
        // getter is implicit, which is the whole point of stating storage
        // separately from the public type
        if getter.is_none() && setter.is_none() && field_decl.is_none() {
            self.add_error(
                ParseErrorType::OtherError(
                    "a property accessor block must define `get`, `set`, or `field`".to_string(),
                ),
                construct_range,
            );
        }
        if let Some((_, field_init, field_range)) = field_decl.as_ref() {
            // only an accessor that was actually written can fail to use the
            // storage it declared; an implicit getter always reads it
            if !references_field && (getter.is_some() || setter.is_some()) {
                self.add_error(
                    ParseErrorType::OtherError(
                        "explicit `field` declaration is never referenced by an accessor"
                            .to_string(),
                    ),
                    *field_range,
                );
            }
            if field_init.is_some() && prop_init.is_some() {
                self.add_error(
                    ParseErrorType::OtherError(
                        "an explicit `field` initialiser cannot be combined with the property's own initialiser"
                            .to_string(),
                    ),
                    *field_range,
                );
            }
        }
        if is_late && prop_init.is_some() {
            self.add_error(
                ParseErrorType::OtherError(
                    "`late` cannot be combined with an initialiser".to_string(),
                ),
                construct_range,
            );
        }
        if is_static {
            // a class-level property is read-only and purely computed: there is no
            // per-instance slot to store in, and a descriptor cannot intercept
            // `A.x = v`. each of these would need a metaclass to be honest about
            if is_var {
                self.add_error(
                    ParseErrorType::OtherError(
                        "a `static` property is read-only; use `static let`".to_string(),
                    ),
                    construct_range,
                );
            }
            if setter.is_some() {
                self.add_error(
                    ParseErrorType::OtherError(
                        "a `static` property cannot define a setter".to_string(),
                    ),
                    construct_range,
                );
            }
            if field_decl.is_some() || references_field {
                self.add_error(
                    ParseErrorType::OtherError(
                        "a `static` property has no backing `field`".to_string(),
                    ),
                    construct_range,
                );
            }
            if prop_init.is_some() {
                self.add_error(
                    ParseErrorType::OtherError(
                        "a `static` property is computed by `get`; it takes no initialiser"
                            .to_string(),
                    ),
                    construct_range,
                );
            }
            if getter.is_none() {
                self.add_error(
                    ParseErrorType::OtherError("a `static` property must define `get`".to_string()),
                    construct_range,
                );
            }
        }

        // ---- synthesise the python members -------------------------------
        // a static property's rejected extras are dropped rather than synthesized:
        // a backing `self.__x` in a `cls`-receiver getter, or a `@x.setter` on the
        // descriptor, would each add a second round of errors about the first one
        let has_backing = !is_static && (references_field || field_decl.is_some());

        // a getter that only reads the field lets the class see storage at its own
        // type; one with real logic must keep being called, so it stays public-typed
        let getter_reads_field_only = match &getter {
            None => true,
            Some((body, _)) => is_pure_field_read(body, &backing),
        };
        // in-class accesses are written under the public name, so record what each
        // one should resolve to. a read may reach storage directly (narrowing); a
        // write must reach the property so its setter still runs
        let read_target = if has_backing && getter_reads_field_only {
            backing.clone()
        } else {
            prop_name.clone()
        };
        if read_target != public_name || prop_name != public_name {
            self.pending_narrow_props.push(PropertyRetarget {
                public: public_name,
                read: read_target,
                write: prop_name.clone(),
            });
        }
        let mut members: Vec<Stmt> = Vec::new();

        // the getter leads the group: it carries the marker spanning the whole
        // construct, and the formatter emits that span verbatim while swallowing
        // the members that follow. the backing field's position is immaterial to
        // ty (its name is distinct), and the setter must still trail the getter so
        // `<name>.setter` resolves
        let mut backing_stmt: Option<Stmt> = None;
        if has_backing {
            let (field_ann, field_init, field_range) = match field_decl {
                Some((annotation, init, range)) => (annotation, init, range),
                None => (None, None, TextRange::empty(start)),
            };
            // an explicit `field` declaration states the storage type itself: when
            // it carries no annotation the type comes from its initialiser, *not*
            // from the property's public type — the two being different is the
            // reason to declare storage separately at all
            let field_declares = field_ann.is_some() || field_init.is_some();
            let backing_type = if field_declares {
                field_ann
            } else {
                prop_type.clone()
            };
            let backing_init = if field_is_late {
                None
            } else {
                field_init.or(prop_init)
            };
            // zero-width, like the other synthesized nodes: a real range invites a
            // sibling pass to attach an edit to this name (`inferred_annotation`
            // appends `: <type>` to a bare class-body assignment), which would then
            // be stranded wherever the property lowering moved the assignment to
            let backing_target = Expr::Name(ast::ExprName {
                id: backing.clone(),
                ctx: ExprContext::Store,
                range: TextRange::empty(field_range.start()),
                node_index: AtomicNodeIndex::NONE,
            });
            match (backing_type, backing_init) {
                (Some(annotation), value) => {
                    backing_stmt = Some(Stmt::AnnAssign(ast::StmtAnnAssign {
                        target: Box::new(backing_target),
                        annotation: Box::new(annotation),
                        value: value.map(Box::new),
                        simple: true,
                        is_context: false,
                        range: field_range,
                        node_index: AtomicNodeIndex::NONE,
                        decorator_list: DecoratorList::new(),
                    }));
                }
                (None, Some(value)) => {
                    // no declared storage type. when the property has one, carry it
                    // as an inference-context marker so the initialiser is solved
                    // against it (`field = []` under `Sequence[int]` declares
                    // `list[int]`) without the property's type *becoming* the
                    // storage type
                    backing_stmt = Some(match prop_type.clone() {
                        Some(context) => Stmt::AnnAssign(ast::StmtAnnAssign {
                            target: Box::new(backing_target),
                            annotation: Box::new(Expr::Subscript(ast::ExprSubscript {
                                value: Box::new(Expr::Name(ast::ExprName {
                                    id: Name::new_static("__field__"),
                                    ctx: ExprContext::Invalid,
                                    range: TextRange::empty(field_range.start()),
                                    node_index: AtomicNodeIndex::NONE,
                                })),
                                slice: Box::new(context),
                                ctx: ExprContext::Load,
                                range: TextRange::empty(field_range.start()),
                                node_index: AtomicNodeIndex::NONE,
                                is_typeof: false,
                                is_type_decoration: false,
                            })),
                            value: Some(Box::new(value)),
                            simple: true,
                            is_context: false,
                            range: field_range,
                            node_index: AtomicNodeIndex::NONE,
                            decorator_list: DecoratorList::new(),
                        }),
                        None => Stmt::Assign(ast::StmtAssign {
                            targets: vec![backing_target],
                            value: Box::new(value),
                            range: field_range,
                            node_index: AtomicNodeIndex::NONE,
                            decorator_list: DecoratorList::new(),
                        }),
                    });
                }
                // nothing to declare: storage springs into existence on first write
                (None, None) => {}
            }
        }

        // the getter carries the property marker, whose range spans the whole
        // construct so the lowering knows exactly what source to replace. the
        // `static` variant resolves to the class-level descriptor rather than to
        // `builtins.property`
        let marker = ast::Decorator {
            expression: Expr::Name(ast::ExprName {
                id: if is_static {
                    Name::new_static("__static_property__")
                } else {
                    Name::new_static("__property__")
                },
                ctx: ExprContext::Invalid,
                range: construct_range,
                node_index: AtomicNodeIndex::NONE,
            }),
            range: construct_range,
            node_index: AtomicNodeIndex::NONE,
        };
        let (getter_body, getter_range) = match getter {
            Some((body, range)) => (body, range),
            // `var` with only a setter gets a pass-through getter
            None => (
                vec![Stmt::Return(ast::StmtReturn {
                    value: Some(Box::new(synth_backing_attr(
                        &backing,
                        ExprContext::Load,
                        start,
                    ))),
                    range: TextRange::empty(start),
                    node_index: AtomicNodeIndex::NONE,
                })],
                TextRange::empty(start),
            ),
        };
        // a class-level getter receives the owning class, not an instance
        let getter_receiver = if is_static { "cls" } else { "self" };
        let getter_params = synth_property_parameters(
            vec![synth_property_param(
                getter_receiver,
                None,
                getter_range.start(),
            )],
            getter_range.start(),
        );
        members.push(build_property_fn(
            ast::Identifier {
                id: prop_name.clone(),
                range: prop_name_range,
                node_index: AtomicNodeIndex::NONE,
            },
            std::iter::once(marker)
                .chain(modifier_markers.iter().cloned())
                .collect(),
            getter_params,
            prop_type.clone(),
            getter_body,
            getter_range,
        ));
        members.extend(backing_stmt);

        // a mutable property gets a setter: the author's, or a pass-through when
        // only `get` was written. a computed property (no backing storage) has
        // nothing to write, so it stays read-only
        let wants_setter = !is_static && is_var && (setter.is_some() || has_backing);
        if wants_setter {
            let (setter_body, setter_param, setter_range) = match setter {
                Some((body, param, range)) => (body, param, range),
                None => {
                    let target =
                        synth_backing_attr(&backing, ExprContext::Store, construct_range.end());
                    (
                        vec![Stmt::Assign(ast::StmtAssign {
                            targets: vec![target],
                            value: Box::new(Expr::Name(ast::ExprName {
                                id: Name::new_static("value"),
                                ctx: ExprContext::Load,
                                range: TextRange::empty(construct_range.end()),
                                node_index: AtomicNodeIndex::NONE,
                            })),
                            range: TextRange::empty(construct_range.end()),
                            node_index: AtomicNodeIndex::NONE,
                            decorator_list: DecoratorList::new(),
                        })],
                        None,
                        TextRange::empty(construct_range.end()),
                    )
                }
            };
            let param_name = setter_param
                .as_ref()
                .map_or_else(|| Name::new_static("value"), |ident| ident.id.clone());
            let setter_decorator = ast::Decorator {
                expression: Expr::Attribute(ast::ExprAttribute {
                    value: Box::new(Expr::Name(ast::ExprName {
                        id: prop_name.clone(),
                        ctx: ExprContext::Load,
                        range: TextRange::empty(setter_range.start()),
                        node_index: AtomicNodeIndex::NONE,
                    })),
                    attr: ast::Identifier {
                        id: Name::new_static("setter"),
                        range: TextRange::empty(setter_range.start()),
                        node_index: AtomicNodeIndex::NONE,
                    },
                    ctx: ExprContext::Load,
                    range: TextRange::empty(setter_range.start()),
                    node_index: AtomicNodeIndex::NONE,
                    optional: false,
                }),
                range: TextRange::empty(setter_range.start()),
                node_index: AtomicNodeIndex::NONE,
            };
            let setter_params = synth_property_parameters(
                vec![
                    synth_property_param("self", None, setter_range.start()),
                    synth_property_param(param_name.as_str(), prop_type, setter_range.start()),
                ],
                setter_range.start(),
            );
            members.push(build_property_fn(
                ast::Identifier {
                    id: prop_name,
                    range: prop_name_range,
                    node_index: AtomicNodeIndex::NONE,
                },
                std::iter::once(setter_decorator)
                    .chain(modifier_markers)
                    .collect(),
                setter_params,
                Some(Expr::NoneLiteral(ast::ExprNoneLiteral {
                    range: TextRange::empty(setter_range.start()),
                    node_index: AtomicNodeIndex::NONE,
                })),
                setter_body,
                setter_range,
            ));
        }

        // hand back the first member; `parse_block` drains the rest so they all
        // land as siblings in the class body
        let mut members = members.into_iter();
        let first = members.next().unwrap_or(decl);
        self.pending_members.extend(members);
        first
    }

    /// basedpython: whether the parser is at the `:` that opens a trailing
    /// lambda block's suite.
    ///
    /// An annotation can never start with a newline, so this shape and an
    /// annotated target are disjoint. Gated on basedpython mode rather than
    /// merely reported: the shape also shows up in `.py` error recovery, where
    /// consuming the suite would replace upstream's diagnostics.
    pub(super) fn at_trailing_lambda_block(&mut self) -> bool {
        self.options.is_basedpython
            && self.at(TokenKind::Colon)
            && self.peek2() == (TokenKind::Newline, TokenKind::Indent)
    }

    /// basedpython: wraps `value` as a trailing lambda block when a `:` and an
    /// indented suite follow it, so an assignment can take the block's call as
    /// its value:
    ///
    /// ```text
    /// a = f(2):
    ///     print(it)
    /// ```
    ///
    /// The block is the same synthetic [`StmtFunctionDef`] a statement-level
    /// block produces, wrapped in an [`ExprStatement`] — the node that stands for
    /// a compound statement written where a value is expected. Its value is the
    /// call, not the tail expressions the other statement expressions produce.
    ///
    /// The block's value is inferred together with the definition its target
    /// makes: the suite defines a function, and a standalone inference of the
    /// value — what every other assignment shape is driven from — could not own
    /// that definition. So `targets` has to be a single name; any other shape is
    /// reported and the block is not consumed.
    ///
    /// Returns `value` untouched when no block follows.
    ///
    /// [`StmtFunctionDef`]: ast::StmtFunctionDef
    /// [`ExprStatement`]: ast::ExprStatement
    fn try_parse_trailing_lambda_value(
        &mut self,
        value: ParsedExpr,
        targets: &[Expr],
    ) -> ParsedExpr {
        if self.at_trailing_lambda_block() && !matches!(targets, [Expr::Name(_)]) {
            self.add_error(
                ParseErrorType::OtherError(
                    "a trailing lambda block's value binds a single name".to_string(),
                ),
                self.current_token_range(),
            );
            return value;
        }

        self.parse_trailing_lambda_value(value)
    }

    /// basedpython: wraps `value` as a trailing lambda block when a `:` and an
    /// indented suite follow it, and returns `value` untouched when none does.
    ///
    /// The caller has already established that a block may bind here — that the
    /// value belongs to a single name. See [`Parser::try_parse_trailing_lambda_value`]
    /// for what the wrapper is and why the target has to be one name.
    fn parse_trailing_lambda_value(&mut self, value: ParsedExpr) -> ParsedExpr {
        if !self.at_trailing_lambda_block() {
            return value;
        }

        let start = value.expr.range().start();
        let function = self.parse_trailing_lambda_statement(value, start);
        self.expr_consumed_suite = true;

        Expr::Statement(ast::ExprStatement {
            range: function.range,
            stmt: Box::new(Stmt::FunctionDef(function)),
            node_index: AtomicNodeIndex::NONE,
        })
        .into()
    }

    /// Parses a statement-level trailing lambda block — an expression followed
    /// by `:` and an indented suite (basedpython only):
    ///
    /// ```text
    /// f(2):
    ///     print(it)
    /// ```
    ///
    /// Produces a synthetic [`StmtFunctionDef`] with `is_trailing_lambda` set:
    /// the suite is the function body, `parameters` holds the single implicit
    /// parameter `it`, and the called expression is carried by a synthetic
    /// decorator whose range is the expression's own (no `@` in the source).
    /// The lowering emits a `def` and re-emits the call with the function
    /// appended as its last argument.
    ///
    /// The caller has already parsed the expression, checked basedpython mode
    /// (`.py` files keep upstream's annotated-assignment errors for this
    /// shape), and is positioned at the `:` token.
    ///
    /// [`StmtFunctionDef`]: ast::StmtFunctionDef
    fn parse_trailing_lambda_statement(
        &mut self,
        callee: ParsedExpr,
        start: TextSize,
    ) -> ast::StmtFunctionDef {
        let colon_start = self.current_token_range().start();
        self.bump(TokenKind::Colon);
        let body = self.parse_body(Clause::FunctionDef);

        let callee_range = callee.expr.range();
        let decorator = ast::Decorator {
            expression: callee.expr,
            range: callee_range,
            node_index: AtomicNodeIndex::NONE,
        };

        // synthetic identifiers anchor zero-width on the `:` so IDE token
        // walks stay ordered relative to the callee (before) and body (after)
        let synthetic = TextRange::empty(colon_start);
        let it = ast::ParameterWithDefault {
            parameter: ast::Parameter {
                name: ast::Identifier {
                    id: Name::new_static("it"),
                    range: synthetic,
                    node_index: AtomicNodeIndex::NONE,
                },
                pattern: None,
                annotation: None,
                is_context: false,
                is_some: false,
                range: synthetic,
                node_index: AtomicNodeIndex::NONE,
            },
            default: None,
            range: synthetic,
            node_index: AtomicNodeIndex::NONE,
        };
        let parameters = ast::Parameters {
            args: std::iter::once(it).collect(),
            range: synthetic,
            ..ast::Parameters::default()
        };

        ast::StmtFunctionDef {
            name: ast::Identifier {
                id: Name::new_static("__trailing_lambda__"),
                range: synthetic,
                node_index: AtomicNodeIndex::NONE,
            },
            type_params: None,
            parameters: Box::new(parameters),
            is_async: body_awaits(&body),
            body,
            decorator_list: vec![decorator].into(),
            returns: None,
            raises: None,
            is_trailing_lambda: true,
            is_asserts_return: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a class definition.
    ///
    /// The given `start` offset is the start of either the `def` token or the
    /// `@` token if the class definition has decorators.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `class` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#grammar-token-python-grammar-classdef>
    fn parse_class_definition(
        &mut self,
        decorator_list: DecoratorList,
        start: TextSize,
    ) -> ast::StmtClassDef {
        self.bump(TokenKind::Class);

        // test_err class_def_missing_name
        // class : ...
        // class (): ...
        // class (metaclass=ABC): ...
        let name = self.parse_identifier();

        // test_err class_def_unclosed_type_param_list
        // class Foo[T1, *T2(a, b):
        //     pass
        // x = 10
        let type_params = self.try_parse_type_params();

        // test_ok class_type_params_py312
        // # parse_options: {"target-version": "3.12"}
        // class Foo[S: (str, bytes), T: float, *Ts, **P]: ...

        // test_err class_type_params_py311
        // # parse_options: {"target-version": "3.11"}
        // class Foo[S: (str, bytes), T: float, *Ts, **P]: ...
        // class Foo[]: ...
        if let Some(ast::TypeParams { range, .. }) = &type_params {
            self.add_unsupported_syntax_error(
                UnsupportedSyntaxErrorKind::TypeParameterList,
                *range,
            );
        }

        // test_ok class_def_arguments
        // class Foo: ...
        // class Foo(): ...
        // class Foo((base for base in bases)): ...
        // class Foo(*(base for base in bases)): ...

        // test_err class_def_unparenthesized_generator_argument
        // class Foo(base for base in bases): ...
        let arguments = self
            .at(TokenKind::Lpar)
            .then(|| Box::new(self.parse_arguments(ArgumentsContext::ClassDefinition)));

        // basedpython: `class Foo` (no colon, no body) is permitted as an
        // empty declaration. the AST records `body: vec![]`, which the
        // `empty_declarations` transform expands to `: ...`. upstream Python
        // would error here; if the colon is present we still require a body
        let body = if self.eat(TokenKind::Colon) {
            // test_err class_def_empty_body
            // class Foo:
            // class Foo():
            // x = 42
            self.class_body_depth += 1;
            let body = self.parse_body(Clause::Class);
            self.class_body_depth -= 1;
            body
        } else {
            if !self.options.is_basedpython {
                self.add_error(
                    ParseErrorType::OtherError(
                        "class without body requires `: ...` in .py files".to_string(),
                    ),
                    self.node_range(start),
                );
            }
            // a non-body class consumes its own statement terminator since
            // there's no `parse_body` to do it for us
            self.eat(TokenKind::Newline);
            Suite::new()
        };

        ast::StmtClassDef {
            range: self.node_range(start),
            decorator_list,
            name,
            type_params: type_params.map(Box::new),
            arguments,
            body,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a `with` statement
    ///
    /// The given `start` offset is the start of either the `with` token or the
    /// `async` token if it's an async with statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `with` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-with-statement>
    fn parse_with_statement(&mut self, start: TextSize) -> ast::StmtWith {
        self.bump(TokenKind::With);

        let mut items = self.parse_with_items();
        items.shrink_to_fit();

        self.expect(TokenKind::Colon);

        let body = self.parse_body(Clause::With);

        ast::StmtWith {
            items,
            body,
            is_async: false,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a list of with items.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-with-statement>
    fn parse_with_items(&mut self) -> Vec<WithItem> {
        if !self.at_expr() {
            self.add_error(
                ParseErrorType::OtherError(
                    "Expected the start of an expression after `with` keyword".to_string(),
                ),
                self.current_token_range(),
            );
            return vec![];
        }

        let open_paren_range = self.current_token_range();

        if self.at(TokenKind::Lpar) {
            if let (Some(items), has_trailing_comma) = self.try_parse_parenthesized_with_items() {
                // test_ok tuple_context_manager_py38
                // # parse_options: {"target-version": "3.8"}
                // with (
                //   foo,
                //   bar,
                //   baz,
                // ) as tup: ...

                // test_ok single_parenthesized_item_context_manager_py38
                // # parse_options: {"target-version": "3.8"}
                // with (
                //   open('foo.txt')) as foo: ...
                // with (
                //   open('foo.txt')): ...

                // test_err tuple_context_manager_py38
                // # parse_options: {"target-version": "3.8"}
                // # these cases are _syntactically_ valid before Python 3.9 because the `with` item
                // # is parsed as a tuple, but this will always cause a runtime error, so we flag it
                // # anyway
                // with (foo, bar): ...
                // with (
                //   foo,
                //   bar,
                //   baz,
                // ): ...
                // with (foo,): ...

                // test_ok parenthesized_context_manager_py39
                // # parse_options: {"target-version": "3.9"}
                // with (foo as x, bar as y): ...
                // with (foo, bar as y): ...
                // with (foo as x, bar): ...

                // test_err parenthesized_context_manager_py38
                // # parse_options: {"target-version": "3.8"}
                // with (foo as x, bar as y): ...
                // with (foo, bar as y): ...
                // with (foo as x, bar): ...
                if items.len() > 1 || has_trailing_comma {
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::ParenthesizedContextManager,
                        open_paren_range,
                    );
                }

                self.expect(TokenKind::Rpar);
                items
            } else {
                // test_ok ambiguous_lpar_with_items_if_expr
                // with (x) if True else y: ...
                // with (x for x in iter) if True else y: ...
                // with (x async for x in iter) if True else y: ...
                // with (x)[0] if True else y: ...

                // test_ok ambiguous_lpar_with_items_binary_expr
                // # It doesn't matter what's inside the parentheses, these tests need to make sure
                // # all binary expressions parses correctly.
                // with (a) and b: ...
                // with (a) is not b: ...
                // # Make sure precedence works
                // with (a) or b and c: ...
                // with (a) and b or c: ...
                // with (a | b) << c | d: ...
                // # Postfix should still be parsed first
                // with (a)[0] + b * c: ...
                self.parse_comma_separated_list_into_vec_with_capacity(
                    RecoveryContextKind::WithItems(WithItemKind::ParenthesizedExpression),
                    |p| p.parse_with_item(WithItemParsingState::Regular).item,
                    1,
                )
            }
        } else {
            self.parse_comma_separated_list_into_vec_with_capacity(
                RecoveryContextKind::WithItems(WithItemKind::Unparenthesized),
                |p| p.parse_with_item(WithItemParsingState::Regular).item,
                1,
            )
        }
    }

    /// Try parsing with-items coming after an ambiguous `(` token.
    ///
    /// To understand the ambiguity, consider the following example:
    ///
    /// ```python
    /// with (item1, item2): ...       # Parenthesized with items
    /// with (item1, item2) as f: ...  # Parenthesized expression
    /// ```
    ///
    /// When the parser is at the `(` token after the `with` keyword, it doesn't know if `(` is
    /// used to parenthesize the with items or if it's part of a parenthesized expression of the
    /// first with item. The challenge here is that until the parser sees the matching `)` token,
    /// it can't resolve the ambiguity.
    ///
    /// This method resolves the ambiguity using speculative parsing. It starts with an assumption
    /// that it's a parenthesized with items. Then, once it finds the matching `)`, it checks if
    /// the assumption still holds true. If the initial assumption was correct, this will return
    /// the parsed with items. Otherwise, rewind the parser back to the starting `(` token,
    /// returning [`None`].
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `(` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#grammar-token-python-grammar-with_stmt_contents>
    fn try_parse_parenthesized_with_items(&mut self) -> (Option<Vec<WithItem>>, bool) {
        let checkpoint = self.checkpoint();

        // We'll start with the assumption that the with items are parenthesized.
        let mut with_item_kind = WithItemKind::Parenthesized;

        self.bump(TokenKind::Lpar);

        let mut parsed_with_items = Vec::with_capacity(1);
        let mut has_optional_vars = false;

        // test_err with_items_parenthesized_missing_comma
        // with (item1 item2): ...
        // with (item1 as f1 item2): ...
        // with (item1, item2 item3, item4): ...
        // with (item1, item2 as f1 item3, item4): ...
        // with (item1, item2: ...
        let has_trailing_comma =
            self.parse_comma_separated_list(RecoveryContextKind::WithItems(with_item_kind), |p| {
                let parsed_with_item = p.parse_with_item(WithItemParsingState::Speculative);
                has_optional_vars |= parsed_with_item.item.optional_vars.is_some();
                parsed_with_items.push(parsed_with_item);
            });

        // Check if our assumption is incorrect and it's actually a parenthesized expression.
        if has_optional_vars {
            // If any of the with item has optional variables, then our assumption is correct
            // and it is a parenthesized with items. Now, we need to restrict the grammar for a
            // with item's context expression which is:
            //
            //     with_item: expression ...
            //
            // So, named, starred and yield expressions not allowed.
            for parsed_with_item in &parsed_with_items {
                if parsed_with_item.is_parenthesized {
                    // Parentheses resets the precedence.
                    continue;
                }
                let error = match parsed_with_item.item.context_expr {
                    Expr::Named(_) => ParseErrorType::UnparenthesizedNamedExpression,
                    Expr::Starred(_) => ParseErrorType::InvalidStarredExpressionUsage,
                    Expr::Yield(_) | Expr::YieldFrom(_) => {
                        ParseErrorType::InvalidYieldExpressionUsage
                    }
                    _ => continue,
                };
                self.add_error(error, &parsed_with_item.item.context_expr);
            }
        } else if self.at(TokenKind::Rpar)
            && (
                // test_err with_items_parenthesized_missing_colon
                // # `)` followed by a newline
                // with (item1, item2)
                //     pass
                matches!(self.peek(), TokenKind::Colon | TokenKind::Newline)
            )
        {
            if parsed_with_items.is_empty() {
                // No with items, treat it as a parenthesized expression to create an empty
                // tuple expression.
                with_item_kind = WithItemKind::ParenthesizedExpression;
            } else {
                // These expressions, if unparenthesized, are only allowed if it's a
                // parenthesized expression and none of the with items have an optional
                // variable.
                if parsed_with_items.iter().any(|parsed_with_item| {
                    !parsed_with_item.is_parenthesized
                        && matches!(
                            parsed_with_item.item.context_expr,
                            Expr::Named(_) | Expr::Starred(_) | Expr::Yield(_) | Expr::YieldFrom(_)
                        )
                }) {
                    with_item_kind = WithItemKind::ParenthesizedExpression;
                }
            }
        } else {
            // For any other token followed by `)`, if any of the items has an optional
            // variables (`as ...`), then our assumption is correct. Otherwise, treat
            // it as a parenthesized expression. For example:
            //
            // ```python
            // with (item1, item2 as f): ...
            // ```
            //
            // This also helps in raising the correct syntax error for the following
            // case:
            // ```python
            // with (item1, item2 as f) as x: ...
            // #                        ^^
            // #                        Expecting `:` but got `as`
            // ```
            with_item_kind = WithItemKind::ParenthesizedExpression;
        }

        let with_items = if with_item_kind.is_parenthesized() {
            let with_items: Vec<_> = parsed_with_items
                .into_iter()
                .map(|parsed_with_item| parsed_with_item.item)
                .collect();

            Some(with_items)
        } else {
            self.rewind(checkpoint);

            None
        };

        (with_items, has_trailing_comma)
    }

    /// Parses a single `with` item.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#grammar-token-python-grammar-with_item>
    fn parse_with_item(&mut self, state: WithItemParsingState) -> ParsedWithItem {
        let start = self.node_start();

        // The grammar for the context expression of a with item depends on the state
        // of with item parsing.
        let context_expr = match state {
            WithItemParsingState::Speculative => {
                // If it's in a speculative state, the parenthesis (`(`) could be part of any of the
                // following expression:
                //
                // Tuple expression          -  star_named_expression
                // Generator expression      -  named_expression
                // Parenthesized expression  -  (yield_expr | named_expression)
                // Parenthesized with items  -  expression
                //
                // Here, the right side specifies the grammar for an element corresponding to the
                // expression mentioned in the left side.
                //
                // So, the grammar used should be able to parse an element belonging to any of the
                // above expression. At a later point, once the parser understands where the
                // parenthesis belongs to, it'll validate and report errors for any invalid expression
                // usage.
                //
                // Thus, we can conclude that the grammar used should be:
                //      (yield_expr | star_named_expression)
                self.parse_named_expression_or_higher(
                    ExpressionContext::yield_or_starred_bitwise_or(),
                )
            }
            WithItemParsingState::Regular => self.parse_conditional_expression_or_higher(),
        };

        let (optional_vars, pattern) = if self.at(TokenKind::As) {
            let (target, pattern) = self.parse_with_item_optional_vars();
            (Some(Box::new(target)), pattern)
        } else {
            (None, None)
        };

        ParsedWithItem {
            is_parenthesized: context_expr.is_parenthesized,
            item: ast::WithItem {
                range: self.node_range(start),
                context_expr: context_expr.expr,
                optional_vars,
                pattern,
                node_index: AtomicNodeIndex::NONE,
            },
        }
    }

    /// Parses the optional variables in a `with` item.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `as` token.
    ///
    /// basedpython: returns the item's pattern too when the target destructures
    /// the bound value, `with open(path) as File(handle):`.
    fn parse_with_item_optional_vars(&mut self) -> (Expr, Option<Box<Pattern>>) {
        self.bump(TokenKind::As);

        let target_checkpoint = self.checkpoint();
        let mut target = self
            .parse_conditional_expression_or_higher_impl(ExpressionContext::starred_conditional());

        // basedpython: like a `for` target, a bound value that cannot be assigned
        // to may be destructured by a pattern instead. The item ends at the `:`
        // of the `with`, at the `,` before the next item, or at the `)` closing a
        // parenthesized item list
        if self.options.is_basedpython && !is_assignment_target(&target.expr) {
            self.rewind(target_checkpoint);
            let pattern = self.parse_destructure_pattern(
                AllowSequencePattern::No,
                TokenSet::new([TokenKind::Colon, TokenKind::Comma, TokenKind::Rpar]),
            );
            match pattern {
                Some(pattern) => {
                    // test_err with_item_destructure_in_python_file
                    // with ctx() as Point(x, y): ...
                    self.error_if_not_basedpython_at(
                        "a destructuring `with` target is not valid in .py files".to_string(),
                        pattern.range(),
                    );
                    let binder = self.destructure_binder(&pattern, ExprContext::Store);
                    return (binder, Some(Box::new(pattern)));
                }
                // not a pattern either: replay the target parse so its own errors
                // are the ones reported
                None => {
                    target = self.parse_conditional_expression_or_higher_impl(
                        ExpressionContext::starred_conditional(),
                    );
                }
            }
        }

        // This has the same semantics as an assignment target.
        self.validate_assignment_target(&target.expr);

        helpers::set_expr_ctx(&mut target.expr, ExprContext::Store);

        (target.expr, None)
    }

    /// Try parsing a `match` statement.
    ///
    /// This uses speculative parsing to remove the ambiguity of whether the `match` token is used
    /// as a keyword or an identifier. This ambiguity arises only in if the `match` token is
    /// followed by certain tokens. For example, if `match` is followed by `[`, we can't know if
    /// it's used in the context of a subscript expression or as a list expression:
    ///
    /// ```python
    /// # Subscript expression; `match` is an identifier
    /// match[x]
    ///
    /// # List expression; `match` is a keyword
    /// match [x, y]:
    ///     case [1, 2]:
    ///         pass
    /// ```
    ///
    /// This is done by parsing the subject expression considering `match` as a keyword token.
    /// Then, based on certain heuristics we'll determine if our assumption is true. If so, we'll
    /// continue parsing the entire match statement. Otherwise, return `None`.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `match` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-match-statement>
    fn try_parse_match_statement(&mut self) -> Option<ast::StmtMatch> {
        let checkpoint = self.checkpoint();

        let start = self.node_start();
        self.bump(TokenKind::Match);

        let subject = self.parse_match_subject_expression();

        match self.current_token_kind() {
            // test_ok match_annotated_assignment
            // match[0]: int
            // match [x, y, z]: dict
            TokenKind::Colon if self.peek() == TokenKind::Newline => {
                // `match` is a keyword — colon followed by newline confirms
                // this is a match statement, not an annotated assignment like
                // `match [x, y, z]: {dict}` or `match[0]: int`.
                self.bump(TokenKind::Colon);

                let cases = self.parse_match_body();

                Some(ast::StmtMatch {
                    subject: Box::new(subject),
                    cases,
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            TokenKind::Newline if matches!(self.peek2(), (TokenKind::Indent, TokenKind::Case)) => {
                // `match` is a keyword

                // test_err match_expected_colon
                // match [1, 2]
                //     case _: ...
                self.add_error(
                    ParseErrorType::ExpectedToken {
                        found: self.current_token_kind(),
                        expected: TokenKind::Colon,
                    },
                    self.current_token_range(),
                );

                let cases = self.parse_match_body();

                Some(ast::StmtMatch {
                    subject: Box::new(subject),
                    cases,
                    range: self.node_range(start),
                    node_index: AtomicNodeIndex::NONE,
                })
            }
            _ => {
                // `match` is an identifier
                self.rewind(checkpoint);

                None
            }
        }
    }

    /// Parses a match statement.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `match` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#the-match-statement>
    fn parse_match_statement(&mut self) -> ast::StmtMatch {
        let start = self.node_start();
        self.bump(TokenKind::Match);

        let match_range = self.node_range(start);

        let subject = self.parse_match_subject_expression();
        self.expect(TokenKind::Colon);

        let cases = self.parse_match_body();

        // test_err match_before_py310
        // # parse_options: { "target-version": "3.9" }
        // match 2:
        //     case 1:
        //         pass

        // test_ok match_after_py310
        // # parse_options: { "target-version": "3.10" }
        // match 2:
        //     case 1:
        //         pass

        self.add_unsupported_syntax_error(UnsupportedSyntaxErrorKind::Match, match_range);

        ast::StmtMatch {
            subject: Box::new(subject),
            cases,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses the subject expression for a `match` statement.
    fn parse_match_subject_expression(&mut self) -> Expr {
        let start = self.node_start();

        // Subject expression grammar is:
        //
        //     subject_expr:
        //         | star_named_expression ',' star_named_expressions?
        //         | named_expression
        //
        // First try with `star_named_expression`, then if there's no comma,
        // we'll restrict it to `named_expression`.
        let subject =
            self.parse_named_expression_or_higher(ExpressionContext::starred_bitwise_or());

        // test_ok match_stmt_subject_expr
        // match x := 1:
        //     case _: ...
        // match (x := 1):
        //     case _: ...
        // # Starred expressions are only allowed in tuple expression
        // match *x | y, z:
        //     case _: ...
        // match await x:
        //     case _: ...

        // test_err match_stmt_invalid_subject_expr
        // match (*x):
        //     case _: ...
        // # Starred expression precedence test
        // match *x and y, z:
        //     case _: ...
        // match yield x:
        //     case _: ...
        if self.at(TokenKind::Comma) {
            let tuple = self.parse_tuple_expression(subject.expr, start, Parenthesized::No, |p| {
                p.parse_named_expression_or_higher(ExpressionContext::starred_bitwise_or())
            });

            Expr::Tuple(tuple)
        } else {
            if subject.is_unparenthesized_starred_expr() {
                // test_err match_stmt_single_starred_subject
                // match *foo:
                //     case _: ...
                self.add_error(ParseErrorType::InvalidStarredExpressionUsage, &subject);
            }
            subject.expr
        }
    }

    /// Parses the body of a `match` statement.
    ///
    /// This method expects that the parser is positioned at a `Newline` token. If not, it adds a
    /// syntax error and continues parsing.
    fn parse_match_body(&mut self) -> Vec<ast::MatchCase> {
        // test_err match_stmt_no_newline_before_case
        // match foo: case _: ...
        self.expect(TokenKind::Newline);

        // Use `eat` instead of `expect` for better error message.
        if !self.eat(TokenKind::Indent) {
            // test_err match_stmt_expect_indented_block
            // match foo:
            // case _: ...
            self.add_error(
                ParseErrorType::OtherError(
                    "Expected an indented block after `match` statement".to_string(),
                ),
                self.current_token_range(),
            );
        }

        let cases = self.parse_match_case_blocks();

        // TODO(dhruvmanila): Should we expect `Dedent` only if there was an `Indent` present?
        self.expect(TokenKind::Dedent);

        cases
    }

    /// Parses a list of match case blocks.
    fn parse_match_case_blocks(&mut self) -> Vec<ast::MatchCase> {
        let mut cases = vec![];

        if !self.at(TokenKind::Case) {
            // test_err match_stmt_expected_case_block
            // match x:
            //     x = 1
            // match x:
            //     match y:
            //         case _: ...
            self.add_error(
                ParseErrorType::OtherError("Expected `case` block".to_string()),
                self.current_token_range(),
            );
            return cases;
        }

        let mut progress = ParserProgress::default();

        while self.at(TokenKind::Case) {
            progress.assert_progressing(self);
            cases.push(self.parse_match_case());
        }

        cases
    }

    /// Parses a single match case block.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `case` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#grammar-token-python-grammar-case_block>
    fn parse_match_case(&mut self) -> ast::MatchCase {
        let start = self.node_start();
        self.bump(TokenKind::Case);

        // test_err match_stmt_missing_pattern
        // match x:
        //     case : ...
        let pattern = self.parse_match_patterns();

        let guard = if self.eat(TokenKind::If) {
            if self.at_expr() {
                // test_ok match_stmt_valid_guard_expr
                // match x:
                //     case y if a := 1: ...
                // match x:
                //     case y if a if True else b: ...
                // match x:
                //     case y if lambda a: b: ...
                // match x:
                //     case y if (yield x): ...

                // test_err match_stmt_invalid_guard_expr
                // match x:
                //     case y if *a: ...
                // match x:
                //     case y if (*a): ...
                // match x:
                //     case y if yield x: ...
                Some(Box::new(
                    self.parse_named_expression_or_higher(ExpressionContext::default())
                        .expr,
                ))
            } else {
                // test_err match_stmt_missing_guard_expr
                // match x:
                //     case y if: ...
                self.add_error(
                    ParseErrorType::ExpectedExpression,
                    self.current_token_range(),
                );
                None
            }
        } else {
            None
        };

        self.expect(TokenKind::Colon);

        // test_err case_expect_indented_block
        // match subject:
        //     case 1:
        //     case 2: ...
        let body = self.parse_body(Clause::Case);

        ast::MatchCase {
            pattern,
            guard,
            body,
            range: self.node_range(start),
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a statement that is valid after an `async` token.
    ///
    /// If the statement is not a valid `async` statement, an error will be reported
    /// and it will be parsed as a statement.
    ///
    /// See:
    /// - <https://docs.python.org/3/reference/compound_stmts.html#the-async-with-statement>
    /// - <https://docs.python.org/3/reference/compound_stmts.html#the-async-for-statement>
    /// - <https://docs.python.org/3/reference/compound_stmts.html#coroutine-function-definition>
    fn parse_async_statement(&mut self) -> Stmt {
        let async_start = self.node_start();
        self.bump(TokenKind::Async);

        match self.current_token_kind() {
            // test_ok async_function_definition
            // async def foo(): ...
            TokenKind::Def => Stmt::FunctionDef(ast::StmtFunctionDef {
                is_async: true,
                ..self.parse_function_definition(DecoratorList::new(), async_start)
            }),

            // test_ok async_with_statement
            // async with item: ...
            TokenKind::With => Stmt::With(ast::StmtWith {
                is_async: true,
                ..self.parse_with_statement(async_start)
            }),

            // test_ok async_for_statement
            // async for target in iter: ...
            TokenKind::For => Stmt::For(ast::StmtFor {
                is_async: true,
                ..self.parse_for_statement(async_start)
            }),

            kind => {
                // test_err async_unexpected_token
                // async class Foo: ...
                // async while test: ...
                // async x = 1
                // async async def foo(): ...
                // async match test:
                //     case _: ...
                self.add_error(
                    ParseErrorType::UnexpectedTokenAfterAsync(kind),
                    self.current_token_range(),
                );

                // Although this statement is not a valid `async` statement,
                // we still parse it. Guard the recursive recovery path so
                // `async async async ...` cannot overflow the parser stack.
                if let Some(stmt) = self.with_recursion(Self::parse_statement) {
                    stmt
                } else {
                    let range = self.node_range(async_start);
                    self.add_error(ParseErrorType::RecursionLimitExceeded, range);
                    Stmt::Expr(ast::StmtExpr {
                        range,
                        value: Box::new(Expr::Name(ast::ExprName {
                            range,
                            id: Name::new_static("async"),
                            ctx: ExprContext::Invalid,
                            node_index: AtomicNodeIndex::NONE,
                        })),
                        node_index: AtomicNodeIndex::NONE,
                    })
                }
            }
        }
    }

    /// Parses a decorator list followed by a class, function or async function definition.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#grammar-token-python-grammar-decorators>
    fn parse_decorators(&mut self) -> Stmt {
        let start = self.node_start();

        let mut decorators = DecoratorList::new();
        let mut progress = ParserProgress::default();

        // test_err decorator_missing_expression
        // @def foo(): ...
        // @
        // def foo(): ...
        // @@
        // def foo(): ...
        // @test
        // @
        // class Test
        while self.at(TokenKind::At) {
            progress.assert_progressing(self);

            let decorator_start = self.node_start();
            self.bump(TokenKind::At);

            let parsed_expr = if self.at(TokenKind::Def) || self.at(TokenKind::Class) {
                Expr::Name(self.parse_missing_name()).into()
            } else {
                self.parse_named_expression_or_higher(ExpressionContext::default())
            };

            if self.options.target_version < PythonVersion::PY39 {
                // test_ok decorator_expression_dotted_ident_py38
                // # parse_options: { "target-version": "3.8" }
                // @buttons.clicked.connect
                // def spam(): ...

                // test_ok decorator_expression_identity_hack_py38
                // # parse_options: { "target-version": "3.8" }
                // def _(x): return x
                // @_(buttons[0].clicked.connect)
                // def spam(): ...

                // test_ok decorator_expression_eval_hack_py38
                // # parse_options: { "target-version": "3.8" }
                // @eval("buttons[0].clicked.connect")
                // def spam(): ...

                // test_ok decorator_expression_py39
                // # parse_options: { "target-version": "3.9" }
                // @buttons[0].clicked.connect
                // def spam(): ...
                // @(x := lambda x: x)(foo)
                // def bar(): ...

                // test_err decorator_expression_py38
                // # parse_options: { "target-version": "3.8" }
                // @buttons[0].clicked.connect
                // def spam(): ...

                // test_err decorator_named_expression_py37
                // # parse_options: { "target-version": "3.7" }
                // @(x := lambda x: x)(foo)
                // def bar(): ...

                // test_err decorator_dict_literal_py38
                // # parse_options: { "target-version": "3.8" }
                // @{3: 3}
                // def bar(): ...

                // test_err decorator_float_literal_py38
                // # parse_options: { "target-version": "3.8" }
                // @3.14
                // def bar(): ...

                // test_ok decorator_await_expression_py39
                // # parse_options: { "target-version": "3.9" }
                // async def foo():
                //     @await bar
                //     def baz(): ...

                // test_err decorator_await_expression_py38
                // # parse_options: { "target-version": "3.8" }
                // async def foo():
                //     @await bar
                //     def baz(): ...

                // test_err decorator_non_toplevel_call_expression_py38
                // # parse_options: { "target-version": "3.8" }
                // @foo().bar()
                // def baz(): ...

                let relaxed_decorator_error = match &parsed_expr.expr {
                    Expr::Call(expr_call) => {
                        helpers::detect_invalid_pre_py39_decorator_node(&expr_call.func)
                    }
                    expr => helpers::detect_invalid_pre_py39_decorator_node(expr),
                };

                if let Some((error, range)) = relaxed_decorator_error {
                    self.add_unsupported_syntax_error(
                        UnsupportedSyntaxErrorKind::RelaxedDecorator(error),
                        range,
                    );
                }
            }

            // test_err decorator_invalid_expression
            // @*x
            // @(*x)
            // @((*x))
            // @yield x
            // @yield from x
            // def foo(): ...

            decorators.push(ast::Decorator {
                expression: parsed_expr.expr,
                range: self.node_range(decorator_start),
                node_index: AtomicNodeIndex::NONE,
            });

            // test_err decorator_missing_newline
            // @x def foo(): ...
            // @x async def foo(): ...
            // @x class Foo: ...
            self.expect(TokenKind::Newline);
        }

        decorators.shrink_to_fit();

        // basedpython: the `protocol` introducer may follow decorators, e.g.
        // `@runtime_checkable protocol P:`. carry the decorators into
        // `parse_protocol_def` so the synthetic `protocol_class` marker is
        // appended after them.
        if self.at(TokenKind::Name)
            && self.src_text(self.current_token_range()) == "protocol"
            && self.peek() == TokenKind::Name
        {
            self.error_if_not_basedpython(
                "`protocol` class syntax is not valid in .py files".to_string(),
            );
            return self.parse_protocol_def(start, decorators);
        }

        // basedpython: a modifier keyword may follow the decorators, e.g.
        // `@overload class def open(...)` or `@final static def helper(...)`.
        // route to the modifier parser, carrying the decorators we just parsed so
        // the synthetic modifier decorator is appended after them. a bare `class`
        // (not `class def`) stays a normal decorated class definition below.
        let at_modifier = (self.at(TokenKind::Class) && self.peek() == TokenKind::Def)
            || (self.at(TokenKind::Name)
                && is_modifier_kw(self.src_text(self.current_token_range())));
        // basedpython: a modifier chain also prefixes a *binding* — `export let
        // a = 1` — which is not what `parse_with_modifier` reads. When the chain
        // does not end at a definition the statement is parsed as the decorated
        // binding it is instead
        if at_modifier && (!self.options.is_basedpython || self.modifier_chain_ends_at_definition())
        {
            return self.parse_with_modifier(start, decorators);
        }

        match self.current_token_kind() {
            TokenKind::Def => Stmt::FunctionDef(self.parse_function_definition(decorators, start)),
            TokenKind::Class => Stmt::ClassDef(self.parse_class_definition(decorators, start)),
            TokenKind::Async if self.peek() == TokenKind::Def => {
                self.bump(TokenKind::Async);

                // test_ok decorator_async_function
                // @decorator
                // async def foo(): ...
                Stmt::FunctionDef(ast::StmtFunctionDef {
                    is_async: true,
                    ..self.parse_function_definition(decorators, start)
                })
            }
            // basedpython: a decorator may also be written above a binding —
            // `@foo` then `let x = 1`. Gated on the source type so a `.py` file
            // keeps python's own recovery, which leaves the offending statement
            // unconsumed for the caller to parse as a statement of its own
            _ if self.options.is_basedpython => self.parse_decorated_binding(decorators, start),
            _ => {
                // test_err decorator_unexpected_token
                // @foo
                // async with x: ...
                // @foo
                // x = 1
                self.add_error(
                    ParseErrorType::OtherError(
                        "Expected class, function definition or async function definition \
                            after decorator"
                            .to_string(),
                    ),
                    self.current_token_range(),
                );

                let range = self.node_range(start);

                ast::StmtFunctionDef {
                    node_index: AtomicNodeIndex::default(),
                    range,
                    is_async: false,
                    is_trailing_lambda: false,
                    is_asserts_return: false,
                    raises: None,
                    decorator_list: decorators,
                    name: ast::Identifier {
                        id: Name::empty(),
                        range: self.missing_node_range(),
                        node_index: AtomicNodeIndex::NONE,
                    },
                    type_params: None,
                    parameters: Box::new(ast::Parameters {
                        range: self.missing_node_range(),
                        ..ast::Parameters::default()
                    }),
                    returns: None,
                    body: Suite::new(),
                }
                .into()
            }
        }
    }

    /// basedpython: whether the modifier chain starting at the current token ends
    /// at a definition — a `def`, an `async def`, a `class`, or the `protocol` /
    /// `enum class` introducers — rather than at a binding.
    ///
    /// [`Parser::parse_with_modifier`] reads a definition and nothing else, so a
    /// chain that ends at `let a = 1` has to be routed elsewhere. Walks the same
    /// consecutive modifier keywords that parser does, without consuming any.
    fn modifier_chain_ends_at_definition(&mut self) -> bool {
        // index 0 is the current token; index >= 1 is `peek_nth(idx - 1)`
        let mut idx: usize = 0;
        loop {
            let (kind, range) = if idx == 0 {
                (self.current_token_kind(), self.current_token_range())
            } else {
                self.peek_nth(idx - 1)
            };
            match kind {
                TokenKind::Def | TokenKind::Async => return true,
                // `class def f` continues the chain as the classmethod modifier;
                // `class Foo` is the definition, and `class let x` is a binding
                TokenKind::Class => {
                    let (next, next_range) = self.peek_nth(idx);
                    if matches!(next, TokenKind::Def | TokenKind::Async) {
                        idx += 1;
                        continue;
                    }
                    // `class let x` / `class var x` declares a class variable
                    return !(next == TokenKind::Name
                        && matches!(self.src_text(next_range), "let" | "var"));
                }
                TokenKind::Name => {
                    let text = self.src_text(range);
                    // `protocol P:` and `enum class E:` introduce a definition
                    if text == "protocol" && declares_a_name(self.peek_nth(idx).0) {
                        return true;
                    }
                    if text == "enum" && self.peek_nth(idx).0 == TokenKind::Class {
                        return true;
                    }
                    if is_modifier_kw(text) {
                        idx += 1;
                        continue;
                    }
                    return false;
                }
                _ => return false,
            }
        }
    }

    /// basedpython: parses the binding a decorator was written above.
    ///
    /// ```by
    /// @foo
    /// let a = 1
    /// ```
    ///
    /// Python allows a decorator only on a `def` or a `class`; here it may also
    /// go above a binding, where it attaches metadata to the binding's type —
    /// `@Field` over `let a: int = 1` declares `a` as `Annotated[int, Field]`,
    /// exactly as writing the decorator in the type position does.
    ///
    /// So a binding needs both halves to carry one: a value to be a binding at all,
    /// and a written type for the metadata to attach to. `@Field a = 1` has no type
    /// to annotate, and inferring one would make what the decorator lands on depend
    /// on what the value happened to be.
    fn parse_decorated_binding(&mut self, decorators: DecoratorList, start: TextSize) -> Stmt {
        let mut stmt = self.parse_statement();
        let range = TextRange::new(start, stmt.range().end());
        match &mut stmt {
            Stmt::AnnAssign(declaration)
                if declaration.value.is_some()
                    && written_annotation_type(&declaration.annotation).is_some() =>
            {
                declaration.decorator_list = decorators;
                declaration.range = range;
            }
            Stmt::AnnAssign(declaration) if declaration.value.is_none() => {
                let _ = declaration;
                self.add_error(
                    ParseErrorType::OtherError(
                        "a declaration with no value binds nothing for a decorator to annotate"
                            .to_string(),
                    ),
                    range,
                );
            }
            Stmt::Assign(_) | Stmt::AnnAssign(_) => {
                self.add_error(
                    ParseErrorType::OtherError(
                        "a decorator on a binding annotates its type, so the binding needs one"
                            .to_string(),
                    ),
                    range,
                );
            }
            _ => {
                self.add_error(
                    ParseErrorType::OtherError(
                        "Expected a definition or a binding after decorator".to_string(),
                    ),
                    range,
                );
            }
        }
        stmt
    }

    /// Parses the body of the given [`Clause`].
    ///
    /// This could either be a single statement that's on the same line as the
    /// clause header or an indented block.
    fn parse_body(&mut self, parent_clause: Clause) -> Suite {
        // a function body is not "directly in a class suite". reset the
        // class-body depth while parsing it so a plain `init(...)` *call* inside
        // a method isn't mistaken for the basedpython init-method shorthand,
        // which is only recognised directly in a class body. nested classes
        // inside the body re-establish their own depth
        if matches!(parent_clause, Clause::FunctionDef) {
            let saved = std::mem::take(&mut self.class_body_depth);
            let body = self.parse_body_inner(parent_clause);
            self.class_body_depth = saved;
            body
        } else if matches!(parent_clause, Clause::Class) {
            // properties are collected while the body is parsed and applied once it
            // is complete, so a method written above the declaration is rewritten
            // too. saving the outer list keeps a nested class from narrowing
            // through its parent's properties
            let saved = std::mem::take(&mut self.pending_narrow_props);
            let mut body = self.parse_body_inner(parent_clause);
            let properties = std::mem::replace(&mut self.pending_narrow_props, saved);
            if !properties.is_empty() {
                narrow_property_reads(&mut body, &properties);
            }
            body
        } else {
            self.parse_body_inner(parent_clause)
        }
    }

    fn parse_body_inner(&mut self, parent_clause: Clause) -> Suite {
        // Note: The test cases in this method chooses a clause at random to test
        // the error logic.

        let newline_range = self.current_token_range();
        if self.eat(TokenKind::Newline) {
            if self.at(TokenKind::Indent) {
                return self.parse_block();
            }
            // test_err clause_expect_indented_block
            // # Here, the error is highlighted at the `pass` token
            // if True:
            // pass
            // # The parser is at the end of the program, so let's highlight
            // # at the newline token after `:`
            // if True:
            self.add_error(
                ParseErrorType::OtherError(format!(
                    "Expected an indented block after {parent_clause}"
                )),
                if self.current_token_range().is_empty() {
                    newline_range
                } else {
                    self.current_token_range()
                },
            );
        } else {
            if self.at_simple_stmt() {
                return self.parse_simple_statements();
            }
            // test_err clause_expect_single_statement
            // if True: if True: pass
            self.add_error(
                ParseErrorType::OtherError("Expected a simple statement".to_string()),
                self.current_token_range(),
            );
        }

        Suite::new()
    }

    /// Parses a block of statements.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at an `Indent` token.
    fn parse_block(&mut self) -> Suite {
        self.bump(TokenKind::Indent);

        let statements = if let Some(statements) = self.with_recursion(|parser| {
            let snapshot = parser.stmt_scratch.snapshot();
            parser.parse_list(RecoveryContextKind::BlockStatements, |parser| {
                let statement = parser.parse_statement();
                parser.stmt_scratch.push(statement);
                // basedpython: a property accessor block lowers one `var`/`let`
                // declaration to several class-body members; the extras follow
                // the returned declaration statement here, in class-body order
                if !parser.pending_members.is_empty() {
                    for member in std::mem::take(&mut parser.pending_members) {
                        parser.stmt_scratch.push(member);
                    }
                }
            });

            parser.stmt_scratch.take_thin_vec(snapshot)
        }) {
            statements
        } else {
            self.report_recursion_limit_exceeded(self.current_token_range());
            Suite::new()
        };

        self.expect(TokenKind::Dedent);

        statements
    }

    /// Parses a single parameter for the given function kind.
    ///
    /// Matches either the `param_no_default_star_annotation` or `param_no_default`
    /// rule in the [Python grammar] depending on whether star annotation is allowed
    /// or not.
    ///
    /// Use [`Parser::parse_parameter_with_default`] to allow parameter with default
    /// values.
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    fn parse_parameter(
        &mut self,
        start: TextSize,
        function_kind: FunctionKind,
        allow_star_annotation: AllowStarAnnotation,
        allow_context: AllowContextModifier,
    ) -> ast::Parameter {
        // basedpython: a chain of modifier keywords (`let`, `var`, `private`,
        // `public`, ...) may prefix a parameter name. inside an `init(...)`
        // shorthand these mark the parameter for auto-attribute-assignment; the
        // chain is detected from the source span between `start` and the
        // parameter name by the `init_method` transform and by
        // `synthesize_let_assignments` — no AST field needed. the parser is
        // permissive here; the `init_method` lowering rejects combinations that
        // are invalid for this context
        while self.at(TokenKind::Name)
            && self.peek() == TokenKind::Name
            && is_param_modifier_kw(self.src_text(self.current_token_range()))
        {
            let kw = self.src_text(self.current_token_range()).to_string();
            self.error_if_not_basedpython(format!(
                "`{kw}` parameter modifier is not valid in .py files"
            ));
            self.bump(TokenKind::Name);
        }
        // basedpython: optional `context` prefix marks the parameter as
        // implicitly fillable from `context` declarations in scope at the
        // call site. not meaningful on `*args` / `**kwargs`
        let mut is_context = false;
        if matches!(allow_context, AllowContextModifier::Yes)
            && self.at(TokenKind::Name)
            && self.src_text(self.current_token_range()) == "context"
            && self.peek() == TokenKind::Name
        {
            self.error_if_not_basedpython(
                "`context` parameter modifier is not valid in .py files".to_string(),
            );
            self.bump(TokenKind::Name);
            is_context = true;
        }

        // basedpython: `local` / `once` lifetime modifiers on a parameter.
        // `local` marks a non-escaping (borrowed) parameter; `once` marks a
        // callback that must be called exactly once. Both may appear, in any
        // order (`once local fn`). Like `let`, the keywords carry no AST field:
        // the `Parameter.range` covers them (it starts before this run) and the
        // strip transform + ty analysis detect them from the source span. The
        // trailing `Name` guard keeps a parameter literally named `local`/`once`
        // (`def f(local)`) from being read as a modifier.
        while self.at(TokenKind::Name)
            && matches!(self.src_text(self.current_token_range()), "local" | "once")
            && self.peek() == TokenKind::Name
        {
            self.error_if_not_basedpython(
                "`local` / `once` parameter modifiers are not valid in .py files".to_string(),
            );
            self.bump(TokenKind::Name);
        }
        // basedpython: a parameter may destructure its argument,
        // `def foo(Point(x, y): Point)`. Only attempted for a parameter that does
        // not start with a plain name, so an ordinary parameter — including one
        // named with a soft keyword, `def foo(match: int)` — is never reparsed
        let pattern = (self.options.is_basedpython
            && matches!(function_kind, FunctionKind::FunctionDef)
            && !(self.at_name_or_soft_keyword()
                && matches!(
                    self.peek(),
                    TokenKind::Colon | TokenKind::Comma | TokenKind::Equal | TokenKind::Rpar
                )))
        .then(|| {
            // `,` and `)` end a parameter with no annotation, which a
            // destructuring parameter needs — accepting them here is what makes
            // that the error reported rather than a stray-token one
            self.parse_destructure_pattern(
                AllowSequencePattern::No,
                TokenSet::new([TokenKind::Colon, TokenKind::Comma, TokenKind::Rpar]),
            )
        })
        .flatten();

        let name = match &pattern {
            Some(pattern) => {
                // test_err param_destructure_in_python_file
                // def foo(Point(x, y): Point): ...
                self.error_if_not_basedpython_at(
                    "a destructuring parameter is not valid in .py files".to_string(),
                    pattern.range(),
                );
                self.destructure_binder_identifier(pattern)
            }
            None => self.parse_identifier(),
        };
        let pattern = pattern.map(Box::new);

        // Annotations are only allowed for function definition. For lambda expression,
        // the `:` token would indicate its body.
        // basedpython: `some T` makes the parameter's type an anonymous type parameter bounded by
        // `T`. the annotation keeps the bound so the surface form round-trips; the matching type
        // parameter is synthesized once the whole list is parsed
        let mut is_some = false;
        let annotation = match function_kind {
            FunctionKind::FunctionDef if self.eat(TokenKind::Colon) => {
                if self.at(TokenKind::Name)
                    && self.src_text(self.current_token_range()) == "some"
                    && (EXPR_SET.contains(self.peek()) || self.peek().is_soft_keyword())
                {
                    // test_err param_some_annotation_py
                    // def f(s: some str) -> str: ...
                    self.error_if_not_basedpython(
                        "`some` parameter annotations are not valid in .py files".to_string(),
                    );
                    self.bump(TokenKind::Name);
                    is_some = true;
                }
                if self.at_expr() {
                    let parsed_expr = match allow_star_annotation {
                        AllowStarAnnotation::Yes => {
                            // test_ok param_with_star_annotation
                            // def foo(*args: *int | str): ...
                            // def foo(*args: *(int or str)): ...

                            // test_err param_with_invalid_star_annotation
                            // def foo(*args: *): ...
                            // def foo(*args: (*tuple[int])): ...
                            // def foo(*args: *int or str): ...
                            // def foo(*args: *yield x): ...
                            // # def foo(*args: **int): ...
                            let parsed_expr = self.parse_conditional_expression_or_higher_impl(
                                ExpressionContext::starred_bitwise_or().with_in_type_expression(),
                            );

                            // test_ok param_with_star_annotation_py311
                            // # parse_options: {"target-version": "3.11"}
                            // def foo(*args: *Ts): ...

                            // test_ok param_with_star_annotation_py310
                            // # parse_options: {"target-version": "3.10"}
                            // # regression tests for https://github.com/astral-sh/ruff/issues/16874
                            // # starred parameters are fine, just not the annotation
                            // from typing import Annotated, Literal
                            // def foo(*args: Ts): ...
                            // def foo(*x: Literal["this should allow arbitrary strings"]): ...
                            // def foo(*x: Annotated[str, "this should allow arbitrary strings"]): ...
                            // def foo(*args: str, **kwds: int): ...
                            // def union(*x: A | B): ...

                            // test_err param_with_star_annotation_py310
                            // # parse_options: {"target-version": "3.10"}
                            // def foo(*args: *Ts): ...
                            if parsed_expr.is_starred_expr() {
                                self.add_unsupported_syntax_error(
                                    UnsupportedSyntaxErrorKind::StarAnnotation,
                                    parsed_expr.range(),
                                );
                            }

                            parsed_expr
                        }
                        AllowStarAnnotation::KeywordPackOnly => {
                            // basedpython: `**kwargs: **Kwargs` unpacks a keyword-variadic pack.
                            // the star count matches the pack's declaration (`[**Kwargs]`), the
                            // way `*args: *Ts` matches `[*Ts]`
                            if self.at(TokenKind::DoubleStar) {
                                self.error_if_not_basedpython(
                                    "keyword-pack unpacking `**kwargs: **Pack` is not valid in \
                                     .py files"
                                        .to_string(),
                                );
                                ParsedExpr {
                                    expr: self.parse_double_starred_type_expression(),
                                    is_parenthesized: false,
                                    parameter_borrow: ast::ParameterBorrow::None,
                                }
                            } else {
                                self.parse_conditional_expression_or_higher_impl(
                                    ExpressionContext::default().with_in_type_expression(),
                                )
                            }
                        }
                        AllowStarAnnotation::No => {
                            // test_ok param_with_annotation
                            // def foo(arg: int): ...
                            // def foo(arg: lambda x: x): ...

                            // test_err param_with_invalid_annotation
                            // def foo(arg: *int): ...
                            // def foo(arg: yield int): ...
                            // def foo(arg: x := int): ...
                            self.parse_conditional_expression_or_higher_impl(
                                ExpressionContext::default().with_in_type_expression(),
                            )
                        }
                    };
                    Some(Box::new(parsed_expr.expr))
                } else {
                    // test_err param_missing_annotation
                    // def foo(x:): ...
                    // def foo(x:,): ...
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    None
                }
            }
            _ => None,
        };

        if pattern.is_some() && annotation.is_none() {
            self.add_error(
                ParseErrorType::OtherError(
                    "A destructuring parameter needs an annotation: there is nothing else to say \
                     what it destructures"
                        .to_string(),
                ),
                self.node_range(start),
            );
        }

        ast::Parameter {
            range: self.node_range(start),
            name,
            pattern,
            annotation,
            node_index: AtomicNodeIndex::NONE,
            is_context,
            is_some,
        }
    }

    /// Parses a parameter with an optional default expression.
    ///
    /// Matches the `param_maybe_default` rule in the [Python grammar].
    ///
    /// This method doesn't allow star annotation. Use [`Parser::parse_parameter`]
    /// instead.
    ///
    /// [Python grammar]: https://docs.python.org/3/reference/grammar.html
    fn parse_parameter_with_default(
        &mut self,
        start: TextSize,
        function_kind: FunctionKind,
    ) -> ast::ParameterWithDefault {
        let parameter = self.parse_parameter(
            start,
            function_kind,
            AllowStarAnnotation::No,
            AllowContextModifier::Yes,
        );

        let default = if self.eat(TokenKind::Equal) {
            if self.at_expr() {
                // test_ok param_with_default
                // def foo(x=lambda y: y): ...
                // def foo(x=1 if True else 2): ...
                // def foo(x=await y): ...
                // def foo(x=(yield y)): ...

                // test_err param_with_invalid_default
                // def foo(x=*int): ...
                // def foo(x=(*int)): ...
                // def foo(x=yield y): ...
                Some(Box::new(self.parse_conditional_expression_or_higher().expr))
            } else {
                // test_err param_missing_default
                // def foo(x=): ...
                // def foo(x: int = ): ...
                self.add_error(
                    ParseErrorType::ExpectedExpression,
                    self.current_token_range(),
                );
                None
            }
        } else {
            None
        };

        ast::ParameterWithDefault {
            range: self.node_range(start),
            parameter,
            default,
            node_index: AtomicNodeIndex::NONE,
        }
    }

    /// Parses a parameter list for the given function kind.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#grammar-token-python-grammar-parameter_list>
    pub(super) fn parse_parameters(&mut self, function_kind: FunctionKind) -> ast::Parameters {
        let start = self.node_start();

        if matches!(function_kind, FunctionKind::FunctionDef) {
            self.expect(TokenKind::Lpar);
        }

        // TODO(dhruvmanila): This has the same problem as `parse_match_pattern_mapping`
        // has where if there are multiple kwarg or vararg, the last one will win and
        // the parser will drop the previous ones. Another thing is the vararg and kwarg
        // uses `Parameter` (not `ParameterWithDefault`) which means that the parser cannot
        // recover well from `*args=(1, 2)`.
        let mut parameters = ast::Parameters::default();
        let parameters_snapshot = self.parameter_scratch.snapshot();
        let mut args_snapshot = None;
        let mut kwonlyargs_snapshot = None;

        let mut seen_default_param = false; // `a=10`
        let mut seen_positional_only_separator = false; // `/`
        let mut seen_keyword_only_separator = false; // `*`
        let mut seen_keyword_only_param_after_separator = false;

        // Range of the keyword only separator if it's the last parameter in the list.
        let mut last_keyword_only_separator_range = None;

        self.parse_comma_separated_list(RecoveryContextKind::Parameters(function_kind), |parser| {
            let param_start = parser.node_start();

            if parameters.kwarg.is_some() {
                // test_err params_follows_var_keyword_param
                // def foo(**kwargs, a, /, b=10, *, *args): ...
                parser.add_error(
                    ParseErrorType::ParamAfterVarKeywordParam,
                    parser.current_token_range(),
                );
            }

            match parser.current_token_kind() {
                TokenKind::Star => {
                    let star_range = parser.current_token_range();
                    parser.bump(TokenKind::Star);

                    kwonlyargs_snapshot.get_or_insert_with(|| parser.parameter_scratch.snapshot());

                    if parser.at_name_or_soft_keyword() {
                        let param = parser.parse_parameter(
                            param_start,
                            function_kind,
                            AllowStarAnnotation::Yes,
                            AllowContextModifier::No,
                        );
                        let param_star_range = parser.node_range(star_range.start());

                        if parser.at(TokenKind::Equal) {
                            // test_err params_var_positional_with_default
                            // def foo(a, *args=(1, 2)): ...
                            parser.add_error(
                                ParseErrorType::VarParameterWithDefault,
                                parser.current_token_range(),
                            );
                        }

                        if seen_keyword_only_separator || parameters.vararg.is_some() {
                            // test_err params_multiple_varargs
                            // def foo(a, *, *args, b): ...
                            // # def foo(a, *, b, c, *args): ...
                            // def foo(a, *args1, *args2, b): ...
                            // def foo(a, *args1, b, c, *args2): ...
                            parser.add_error(
                                ParseErrorType::OtherError(
                                    "Only one '*' parameter allowed".to_string(),
                                ),
                                param_star_range,
                            );
                        }

                        // TODO(dhruvmanila): The AST doesn't allow multiple `vararg`, so let's
                        // choose to keep the first one so that the parameters remain in preorder.
                        if parameters.vararg.is_none() {
                            parameters.vararg = Some(Box::new(param));
                        }

                        last_keyword_only_separator_range = None;
                    } else {
                        if seen_keyword_only_separator {
                            // test_err params_multiple_star_separator
                            // def foo(a, *, *, b): ...
                            // def foo(a, *, b, c, *): ...
                            parser.add_error(
                                ParseErrorType::OtherError(
                                    "Only one '*' separator allowed".to_string(),
                                ),
                                star_range,
                            );
                        }

                        if parameters.vararg.is_some() {
                            // test_err params_star_separator_after_star_param
                            // def foo(a, *args, *, b): ...
                            // def foo(a, *args, b, c, *): ...
                            parser.add_error(
                                ParseErrorType::OtherError(
                                    "Keyword-only parameter separator not allowed \
                                        after '*' parameter"
                                        .to_string(),
                                ),
                                star_range,
                            );
                        }

                        seen_keyword_only_separator = true;
                        last_keyword_only_separator_range = Some(star_range);
                    }
                }
                TokenKind::DoubleStar => {
                    let double_star_range = parser.current_token_range();
                    parser.bump(TokenKind::DoubleStar);

                    let param = parser.parse_parameter(
                        param_start,
                        function_kind,
                        AllowStarAnnotation::KeywordPackOnly,
                        AllowContextModifier::No,
                    );
                    let param_double_star_range = parser.node_range(double_star_range.start());

                    if parameters.kwarg.is_some() {
                        // test_err params_multiple_kwargs
                        // def foo(a, **kwargs1, **kwargs2): ...
                        parser.add_error(
                            ParseErrorType::OtherError(
                                "Only one '**' parameter allowed".to_string(),
                            ),
                            param_double_star_range,
                        );
                    }

                    if parser.at(TokenKind::Equal) {
                        // test_err params_var_keyword_with_default
                        // def foo(a, **kwargs={'b': 1, 'c': 2}): ...
                        parser.add_error(
                            ParseErrorType::VarParameterWithDefault,
                            parser.current_token_range(),
                        );
                    }

                    if seen_keyword_only_separator && !seen_keyword_only_param_after_separator {
                        // test_ok params_seen_keyword_only_param_after_star
                        // def foo(*, a, **kwargs): ...
                        // def foo(*, a=10, **kwargs): ...

                        // test_err params_kwarg_after_star_separator
                        // def foo(*, **kwargs): ...
                        parser.add_error(
                            ParseErrorType::ExpectedKeywordParam,
                            param_double_star_range,
                        );
                    }

                    parameters.kwarg = Some(Box::new(param));
                    last_keyword_only_separator_range = None;
                }
                TokenKind::Slash => {
                    let slash_range = parser.current_token_range();
                    parser.bump(TokenKind::Slash);

                    if parser.parameter_scratch.is_empty(&parameters_snapshot)
                        && parameters.vararg.is_none()
                        && parameters.kwarg.is_none()
                    {
                        // test_err params_no_arg_before_slash
                        // def foo(/): ...
                        // def foo(/, a): ...
                        parser.add_error(
                            ParseErrorType::OtherError(
                                "Position-only parameter separator not allowed as first parameter"
                                    .to_string(),
                            ),
                            slash_range,
                        );
                    }

                    if seen_positional_only_separator {
                        // test_err params_multiple_slash_separator
                        // def foo(a, /, /, b): ...
                        // def foo(a, /, b, c, /): ...
                        parser.add_error(
                            ParseErrorType::OtherError(
                                "Only one '/' separator allowed".to_string(),
                            ),
                            slash_range,
                        );
                    }

                    if seen_keyword_only_separator || parameters.vararg.is_some() {
                        // test_err params_star_after_slash
                        // def foo(*a, /): ...
                        // def foo(a, *args, b, /): ...
                        // def foo(a, *, /, b): ...
                        // def foo(a, *, b, c, /, d): ...
                        parser.add_error(
                            ParseErrorType::OtherError(
                                "'/' parameter must appear before '*' parameter".to_string(),
                            ),
                            slash_range,
                        );
                    }

                    if !seen_positional_only_separator {
                        // We should only split if we're seeing the separator for the
                        // first time, otherwise it's a user error.
                        if kwonlyargs_snapshot.is_none() {
                            args_snapshot = Some(parser.parameter_scratch.snapshot());
                        }
                        seen_positional_only_separator = true;

                        // test_ok pos_only_py38
                        // # parse_options: {"target-version": "3.8"}
                        // def foo(a, /): ...

                        // test_err pos_only_py37
                        // # parse_options: {"target-version": "3.7"}
                        // def foo(a, /): ...
                        // def foo(a, /, b, /): ...
                        // def foo(a, *args, /, b): ...
                        // def foo(a, //): ...
                        parser.add_unsupported_syntax_error(
                            UnsupportedSyntaxErrorKind::PositionalOnlyParameter,
                            slash_range,
                        );
                    }

                    last_keyword_only_separator_range = None;
                }
                _ if parser.at_name_or_soft_keyword() => {
                    let param = parser.parse_parameter_with_default(param_start, function_kind);

                    // TODO(dhruvmanila): Pyright seems to only highlight the first non-default argument
                    // https://github.com/microsoft/pyright/blob/3b70417dd549f6663b8f86a76f75d8dfd450f4a8/packages/pyright-internal/src/parser/parser.ts#L2038-L2042
                    //
                    // basedpython relaxes this for `def` (not `lambda`): a required
                    // parameter may follow a defaulted one — the lowering gives it a
                    // `_MISSING` sentinel default plus a body guard that raises. this
                    // is what lets a trailing lambda bind the last parameter while
                    // earlier defaulted parameters keep their defaults
                    if param.default.is_none()
                        && seen_default_param
                        && !seen_keyword_only_separator
                        && parameters.vararg.is_none()
                        && !(parser.options.is_basedpython
                            && function_kind == FunctionKind::FunctionDef)
                    {
                        // test_ok params_non_default_after_star
                        // def foo(a=10, *, b, c=11, d): ...
                        // def foo(a=10, *args, b, c=11, d): ...

                        // test_err params_non_default_after_default
                        // def foo(a=10, b, c: int): ...
                        parser.add_error(ParseErrorType::NonDefaultParamAfterDefaultParam, &param);
                    }

                    seen_default_param |= param.default.is_some();

                    if seen_keyword_only_separator {
                        seen_keyword_only_param_after_separator = true;
                    }

                    parser.parameter_scratch.push(param);
                    last_keyword_only_separator_range = None;
                }
                _ => {
                    // This corresponds to the expected token kinds for `is_list_element`.
                    unreachable!("Expected Name, '*', '**', or '/'");
                }
            }
        });

        if let Some(star_range) = last_keyword_only_separator_range {
            // test_err params_expected_after_star_separator
            // def foo(*): ...
            // def foo(*,): ...
            // def foo(a, *): ...
            // def foo(a, *,): ...
            // def foo(*, **kwargs): ...
            self.add_error(ParseErrorType::ExpectedKeywordParam, star_range);
        }

        if matches!(function_kind, FunctionKind::FunctionDef) {
            self.expect(TokenKind::Rpar);
        }

        if let Some(kwonlyargs_snapshot) = kwonlyargs_snapshot {
            parameters.kwonlyargs = self.parameter_scratch.take_thin_vec(kwonlyargs_snapshot);
        }
        if let Some(args_snapshot) = args_snapshot {
            parameters.args = self.parameter_scratch.take_thin_vec(args_snapshot);
            parameters.posonlyargs = self.parameter_scratch.take_thin_vec(parameters_snapshot);
        } else if seen_positional_only_separator {
            parameters.posonlyargs = self.parameter_scratch.take_thin_vec(parameters_snapshot);
        } else {
            parameters.args = self.parameter_scratch.take_thin_vec(parameters_snapshot);
        }

        // basedpython: a `context` parameter receives its implicit argument by
        // keyword, so it must not sit where an explicit positional argument
        // could land on it (or slide past it) at a call site
        for param in &parameters.posonlyargs {
            if param.parameter.is_context {
                self.add_error(
                    ParseErrorType::OtherError(
                        "a positional-only parameter cannot be a `context` parameter".to_string(),
                    ),
                    &param.parameter,
                );
            }
        }
        let mut seen_context_param = false;
        for param in &parameters.args {
            if param.parameter.is_context {
                seen_context_param = true;
            } else if seen_context_param {
                self.add_error(
                    ParseErrorType::OtherError(
                        "parameter after a `context` parameter must also be `context`".to_string(),
                    ),
                    &param.parameter,
                );
            }
        }
        if seen_context_param && let Some(vararg) = &parameters.vararg {
            self.add_error(
                ParseErrorType::OtherError(
                    "`*` parameter cannot follow a `context` parameter".to_string(),
                ),
                vararg.as_ref(),
            );
        }

        parameters.range = self.node_range(start);

        parameters
    }

    /// Try to parse a type parameter list. If the parser is not at the start of a
    /// type parameter list, return `None`.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#type-parameter-lists>
    fn try_parse_type_params(&mut self) -> Option<ast::TypeParams> {
        self.at(TokenKind::Lsqb).then(|| self.parse_type_params())
    }

    /// Parses a type parameter list.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `[` token.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#type-parameter-lists>
    fn parse_type_params(&mut self) -> ast::TypeParams {
        let start = self.node_start();
        self.bump(TokenKind::Lsqb);

        let mut type_params = Vec::new();
        let mut separators = ast::TypeParamSeparators::default();
        // basedpython: `/` and a bare `*` divide the list the way they divide a value parameter
        // list. their positions are validated once the whole list is known, so a malformed list
        // still reports every parameter it contains
        let mut slash_range: Option<TextRange> = None;
        let mut star_range: Option<TextRange> = None;

        self.parse_comma_separated_list(RecoveryContextKind::TypeParams, |parser| {
            if parser.options.is_basedpython && parser.at(TokenKind::Slash) {
                let range = parser.current_token_range();
                parser.bump(TokenKind::Slash);
                if slash_range.is_none() {
                    slash_range = Some(range);
                    separators.positional_only_count =
                        Some(u32::try_from(type_params.len()).unwrap_or(u32::MAX));
                    separators.slash_range = Some(range);
                } else {
                    parser.add_error(ParseErrorType::DuplicateTypeParamSeparator("/"), range);
                }
                return;
            }
            if parser.options.is_basedpython
                && parser.at(TokenKind::Star)
                && matches!(
                    parser.peek(),
                    TokenKind::Comma | TokenKind::Rsqb | TokenKind::Newline
                )
            {
                let range = parser.current_token_range();
                parser.bump(TokenKind::Star);
                if star_range.is_none() {
                    star_range = Some(range);
                    separators.keyword_only_start =
                        Some(u32::try_from(type_params.len()).unwrap_or(u32::MAX));
                    separators.star_range = Some(range);
                } else {
                    parser.add_error(ParseErrorType::DuplicateTypeParamSeparator("*"), range);
                }
                return;
            }
            type_params.push(parser.parse_type_param());
        });

        if type_params.is_empty() {
            // test_err type_params_empty
            // def foo[]():
            //     pass
            // type ListOrSet[] = list | set
            self.add_error(ParseErrorType::EmptyTypeParams, self.current_token_range());
        }

        self.validate_type_param_separators(&type_params, &mut separators, slash_range, star_range);

        self.expect(TokenKind::Rsqb);

        ast::TypeParams {
            range: self.node_range(start),
            type_params,
            node_index: AtomicNodeIndex::NONE,
            separators,
        }
    }

    /// basedpython: reject separator placements a value parameter list would also reject.
    ///
    /// A rejected separator is dropped rather than kept, so downstream consumers only ever see a
    /// well-formed split.
    fn validate_type_param_separators(
        &mut self,
        type_params: &[ast::TypeParam],
        separators: &mut ast::TypeParamSeparators,
        slash_range: Option<TextRange>,
        star_range: Option<TextRange>,
    ) {
        if let Some(range) = slash_range {
            if separators.positional_only_count == Some(0) {
                self.add_error(
                    ParseErrorType::OtherError(
                        "At least one type parameter must precede `/`".to_string(),
                    ),
                    range,
                );
                separators.positional_only_count = None;
                separators.slash_range = None;
            } else if star_range.is_some_and(|star| star.start() < range.start()) {
                self.add_error(
                    ParseErrorType::OtherError("`/` must precede `*`".to_string()),
                    range,
                );
                separators.positional_only_count = None;
                separators.slash_range = None;
            }
        }

        if let Some(range) = star_range
            && separators.keyword_only_start
                == Some(u32::try_from(type_params.len()).unwrap_or(u32::MAX))
        {
            self.add_error(
                ParseErrorType::OtherError(
                    "At least one type parameter must follow `*`".to_string(),
                ),
                range,
            );
            separators.keyword_only_start = None;
            separators.star_range = None;
        }
    }

    /// Parses a type parameter.
    ///
    /// See: <https://docs.python.org/3/reference/compound_stmts.html#grammar-token-python-grammar-type_param>
    /// basedpython: parses a `**`-prefixed type expression, encoded as `Starred(Starred(_))`.
    ///
    /// That is the shape a keyword-variadic pack takes everywhere it is unpacked — the callable
    /// arrow `(**P) -> R`, the parameter annotation `**kwargs: **Kwargs`, and a pack's own bound.
    /// Python has no `**` expression form, so a second [`ast::ExprStarred`] stands in for the
    /// second star.
    ///
    /// The caller is responsible for having reported [`Parser::error_if_not_basedpython`], since
    /// the message naming the construct differs per position.
    fn parse_double_starred_type_expression(&mut self) -> Expr {
        let star_start = self.node_start();
        self.bump(TokenKind::DoubleStar);
        let inner = self.parse_conditional_expression_or_higher().expr;
        let inner_range = inner.range();
        Expr::Starred(ast::ExprStarred {
            value: Box::new(Expr::Starred(ast::ExprStarred {
                value: Box::new(inner),
                ctx: ExprContext::Load,
                range: inner_range,
                node_index: AtomicNodeIndex::NONE,
            })),
            ctx: ExprContext::Load,
            range: self.node_range(star_start),
            node_index: AtomicNodeIndex::NONE,
        })
    }

    fn parse_type_param(&mut self) -> ast::TypeParam {
        let start = self.node_start();

        let is_reified = self.eat_reified_modifier();

        // test_ok type_param_type_var_tuple
        // type X[*Ts] = int
        // type X[*Ts = int] = int
        // type X[*Ts = *int] = int
        // type X[T, *Ts] = int
        // type X[T, *Ts = int] = int
        if self.eat(TokenKind::Star) {
            let name = self.parse_identifier();

            // basedpython: `*Ts: int` bounds every element of the pack, and the starred
            // `*Ts: *(int, str)` bounds the pack as a whole — the star count follows the
            // pack's declaration, the way `*args: *Ts` does. CPython rejects a bound on a
            // `TypeVarTuple`, so either is an error in `.py` files
            let bound = if self.eat(TokenKind::Colon) {
                // test_err type_param_type_var_tuple_bound
                // type X[*T: int] = int
                // type X[*T: *(int, str)] = int
                self.error_if_not_basedpython(
                    "a bound on a `TypeVarTuple` is a basedpython feature and is not valid in \
                     .py files"
                        .to_string(),
                );
                if self.at_expr() {
                    Some(Box::new(
                        self.parse_conditional_expression_or_higher_impl(
                            ExpressionContext::starred_bitwise_or(),
                        )
                        .expr,
                    ))
                } else {
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    None
                }
            } else {
                None
            };

            let default = if self.eat(TokenKind::Equal) {
                if self.at_expr() {
                    // test_err type_param_type_var_tuple_invalid_default_expr
                    // type X[*Ts = *int] = int
                    // type X[*Ts = *int or str] = int
                    // type X[*Ts = yield x] = int
                    // type X[*Ts = yield from x] = int
                    // type X[*Ts = x := int] = int
                    Some(Box::new(
                        self.parse_conditional_expression_or_higher_impl(
                            ExpressionContext::starred_bitwise_or(),
                        )
                        .expr,
                    ))
                } else {
                    // test_err type_param_type_var_tuple_missing_default
                    // type X[*Ts =] = int
                    // type X[*Ts =, T2] = int
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    None
                }
            } else {
                None
            };

            ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple {
                range: self.node_range(start),
                name,
                bound,
                default,
                is_reified,
                node_index: AtomicNodeIndex::NONE,
            })

        // test_ok type_param_param_spec
        // type X[**P] = int
        // type X[**P = int] = int
        // type X[T, **P] = int
        // type X[T, **P = int] = int
        } else if self.eat(TokenKind::DoubleStar) {
            let name = self.parse_identifier();

            // basedpython: `**Kwargs: int` bounds every field of the keyword-variadic pack, and
            // the double-starred `**Kwargs: **{"a": int}` bounds the pack as a whole — the star
            // count follows the pack's declaration, the way `**kwargs: **Kwargs` does. CPython
            // rejects a bound on a `ParamSpec`, so either is an error in `.py` files
            let bound = if self.eat(TokenKind::Colon) {
                // test_err type_param_param_spec_bound
                // type X[**T: int] = int
                // type X[**T: **{"a": int}] = int
                self.error_if_not_basedpython(
                    "a bound on a keyword-variadic pack is a basedpython feature and is not \
                     valid in .py files"
                        .to_string(),
                );
                if self.at(TokenKind::DoubleStar) {
                    Some(Box::new(self.parse_double_starred_type_expression()))
                } else if self.at_expr() {
                    Some(Box::new(self.parse_conditional_expression_or_higher().expr))
                } else {
                    // test_err type_param_param_spec_missing_bound
                    // type X[**T:] = int
                    // type X[**T:, T2] = int
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    None
                }
            } else {
                None
            };

            let default = if self.eat(TokenKind::Equal) {
                if self.at_expr() {
                    // test_err type_param_param_spec_invalid_default_expr
                    // type X[**P = *int] = int
                    // type X[**P = yield x] = int
                    // type X[**P = yield from x] = int
                    // type X[**P = x := int] = int
                    // type X[**P = *int] = int
                    Some(Box::new(self.parse_conditional_expression_or_higher().expr))
                } else {
                    // test_err type_param_param_spec_missing_default
                    // type X[**P =] = int
                    // type X[**P =, T2] = int
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    None
                }
            } else {
                None
            };

            ast::TypeParam::ParamSpec(ast::TypeParamParamSpec {
                range: self.node_range(start),
                name,
                bound,
                default,
                is_reified,
                node_index: AtomicNodeIndex::NONE,
            })
            // test_ok type_param_type_var
            // type X[T] = int
            // type X[T = int] = int
            // type X[T: int = int] = int
            // type X[T: (int, int) = int] = int
            // type X[T: int = int, U: (int, int) = int] = int
        } else {
            // basedpython variance keywords: `out T`, `in T`, `in out T`
            let variance = if self.eat(TokenKind::In) {
                if self.at(TokenKind::Name) && self.src_text(self.current_token_range()) == "out" {
                    self.bump(TokenKind::Name);
                    Some(Variance::Invariant)
                } else {
                    Some(Variance::Contravariant)
                }
            } else if self.at(TokenKind::Name) && self.src_text(self.current_token_range()) == "out"
            {
                self.bump(TokenKind::Name);
                Some(Variance::Covariant)
            } else {
                None
            };

            let name = self.parse_identifier();

            // basedpython: `T in (int, str)` ranges the parameter over a type mapping, where
            // `T: (int, str)` bounds it by the tuple type. the mapping is an arbitrary type
            // expression, so `in` is what tells the two apart, not the shape of what follows
            let is_type_mapping = self.at(TokenKind::In);
            let (lower_bound, bound) = if is_type_mapping {
                // test_err type_param_type_mapping_py
                // type X[T in (int, str)] = int
                // def f[T in (int, str)](): ...
                // class C[T in (int, str)]: ...
                self.error_if_not_basedpython(
                    "type mappings are a basedpython feature and are not valid in `.py` files"
                        .to_string(),
                );
                self.bump(TokenKind::In);
                if self.at_expr() {
                    (
                        None,
                        Some(Box::new(
                            self.parse_conditional_expression_or_higher_impl(
                                ExpressionContext::default().with_in_type_expression(),
                            )
                            .expr,
                        )),
                    )
                } else {
                    // test_err type_param_missing_type_mapping
                    // type X[T in ] = int
                    // type X[T1 in , T2] = int
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    (None, None)
                }
            } else if self.eat(TokenKind::Colon) {
                // the lower end is parsed in a context that leaves a following `..` alone, so it
                // separates the two ends instead of being eaten as a malformed attribute access
                if self.at_expr() {
                    // test_err type_param_invalid_bound_expr
                    // type X[T: *int] = int
                    // type X[T: yield x] = int
                    // type X[T: yield from x] = int
                    // type X[T: x := int] = int
                    let first = Box::new(
                        self.parse_conditional_expression_or_higher_impl(
                            ExpressionContext::default()
                                .with_in_type_param_bound()
                                .with_in_type_expression(),
                        )
                        .expr,
                    );
                    if self.at_adjacent_double_dot() {
                        self.parse_bound_range_upper(first)
                    } else {
                        (None, Some(first))
                    }
                } else if self.at_adjacent_double_dot() {
                    // a range needs both ends, so the lower half cannot be elided; `T: ..int`
                    // is spelled `T: int`
                    self.error_if_not_basedpython_at(
                        "type parameter bound ranges are not valid in `.py` files".to_string(),
                        self.current_token_range(),
                    );
                    let dots = self.eat_double_dot();
                    self.add_error(ParseErrorType::IncompleteTypeParamBoundRange, dots);
                    let upper = self.at_expr().then(|| {
                        Box::new(
                            self.parse_conditional_expression_or_higher_impl(
                                ExpressionContext::default().with_in_type_expression(),
                            )
                            .expr,
                        )
                    });
                    (None, upper)
                } else {
                    // test_err type_param_missing_bound
                    // type X[T: ] = int
                    // type X[T1: , T2] = int
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    (None, None)
                }
            } else {
                (None, None)
            };

            let equal_token_start = self.node_start();
            let default = if self.eat(TokenKind::Equal) {
                if self.at_expr() {
                    // test_err type_param_type_var_invalid_default_expr
                    // type X[T = *int] = int
                    // type X[T = yield x] = int
                    // type X[T = (yield x)] = int
                    // type X[T = yield from x] = int
                    // type X[T = x := int] = int
                    // type X[T: int = *int] = int
                    Some(Box::new(
                        self.parse_conditional_expression_or_higher_impl(
                            ExpressionContext::default().with_in_type_expression(),
                        )
                        .expr,
                    ))
                } else {
                    // test_err type_param_type_var_missing_default
                    // type X[T =] = int
                    // type X[T: int =] = int
                    // type X[T1 =, T2] = int
                    self.add_error(
                        ParseErrorType::ExpectedExpression,
                        self.current_token_range(),
                    );
                    None
                }
            } else {
                None
            };

            // test_ok type_param_default_py313
            // # parse_options: {"target-version": "3.13"}
            // type X[T = int] = int
            // def f[T = int](): ...
            // class C[T = int](): ...

            // test_err type_param_default_py312
            // # parse_options: {"target-version": "3.12"}
            // type X[T = int] = int
            // def f[T = int](): ...
            // class C[T = int](): ...
            // class D[S, T = int, U = uint](): ...

            if default.is_some() {
                self.add_unsupported_syntax_error(
                    UnsupportedSyntaxErrorKind::TypeParamDefault,
                    self.node_range(equal_token_start),
                );
            }

            ast::TypeParam::TypeVar(ast::TypeParamTypeVar {
                range: self.node_range(start),
                name,
                lower_bound,
                bound,
                is_type_mapping,
                default,
                variance,
                is_reified,
                is_some_hole: false,
                node_index: AtomicNodeIndex::NONE,
            })
        }
    }

    /// basedpython: consumes a leading `reified` modifier on a type parameter, reporting an error
    /// in `.py` files.
    ///
    /// `reified` is a soft keyword: it only modifies the parameter when something that can open
    /// one follows it — a name, a `*` / `**` pack, or a variance keyword. A list that ends right
    /// after it (`[reified]`, `[reified: int]`, `[reified = int]`) declares a parameter *named*
    /// `reified`.
    fn eat_reified_modifier(&mut self) -> bool {
        if !self.at(TokenKind::Name) || self.src_text(self.current_token_range()) != "reified" {
            return false;
        }
        if !matches!(
            self.peek(),
            TokenKind::Name | TokenKind::Star | TokenKind::DoubleStar | TokenKind::In
        ) {
            return false;
        }

        // test_err type_param_reified_py
        // def f[reified T](): ...
        // class C[reified T]: ...
        // type X[reified T] = int
        self.error_if_not_basedpython(
            "reified type parameters are not valid in `.py` files".to_string(),
        );
        self.bump(TokenKind::Name);
        true
    }

    /// basedpython: append one synthesized type parameter per `some T` parameter.
    ///
    /// The hole takes the parameter's name, so a later annotation can refer to it, and is marked
    /// `is_some_hole` so the formatter hides it again and re-emits `some` on the parameter. The
    /// range is the annotation's, which keeps every synthesized node inside the signature it came
    /// from.
    fn with_some_holes(
        type_params: Option<ast::TypeParams>,
        parameters: &mut ast::Parameters,
    ) -> Option<ast::TypeParams> {
        let ast::Parameters {
            posonlyargs,
            args,
            vararg,
            kwonlyargs,
            kwarg,
            ..
        } = parameters;
        let holes: Vec<ast::TypeParam> = posonlyargs
            .iter_mut()
            .chain(args.iter_mut())
            .chain(kwonlyargs.iter_mut())
            .map(|parameter| &mut parameter.parameter)
            .chain(vararg.iter_mut().map(Box::as_mut))
            .chain(kwarg.iter_mut().map(Box::as_mut))
            .filter(|parameter| parameter.is_some)
            .filter_map(|parameter| {
                // the annotation becomes a reference to the hole, which is what makes the
                // parameter's declared type the hole rather than its bound. the name keeps the
                // bound's range so the formatter can read the written text back out of the source
                let bound =
                    parameter
                        .annotation
                        .replace(Box::new(ast::Expr::Name(ast::ExprName {
                            range: parameter.annotation.as_ref()?.range(),
                            node_index: AtomicNodeIndex::NONE,
                            id: parameter.name.id.clone(),
                            ctx: ast::ExprContext::Load,
                        })))?;
                Some(ast::TypeParam::TypeVar(ast::TypeParamTypeVar {
                    // must enclose both children: the name lives in the parameter list and the
                    // bound after the colon
                    range: TextRange::new(parameter.name.range().start(), bound.range().end()),
                    node_index: AtomicNodeIndex::NONE,
                    name: parameter.name.clone(),
                    lower_bound: None,
                    bound: Some(bound),
                    // `some T` writes an upper bound; a hole has no `in` spelling
                    is_type_mapping: false,
                    default: None,
                    variance: None,
                    // a hole is a type-level construct with no runtime form to reify
                    is_reified: false,
                    is_some_hole: true,
                }))
            })
            .collect();

        if holes.is_empty() {
            return type_params;
        }
        match type_params {
            Some(mut type_params) => {
                type_params.type_params.extend(holes);
                Some(type_params)
            }
            None => Some(ast::TypeParams {
                range: parameters.range(),
                node_index: AtomicNodeIndex::NONE,
                type_params: holes,
                separators: ast::TypeParamSeparators::default(),
            }),
        }
    }

    /// Parses the upper half of a basedpython bound range, with the parser sitting on the `..`
    /// and `lower` already parsed.
    ///
    /// Returns the `(lower_bound, bound)` pair to store on the type parameter. A range that is
    /// rejected — because an end is missing, or because this is a `.py` file — degrades to a plain
    /// upper bound so that downstream consumers only ever see a well-formed bound.
    fn parse_bound_range_upper(
        &mut self,
        lower: Box<Expr>,
    ) -> (Option<Box<Expr>>, Option<Box<Expr>>) {
        // test_err type_param_bound_range_py
        // type X[T: int..object] = int
        let dots = self.eat_double_dot();
        self.error_if_not_basedpython_at(
            "type parameter bound ranges are not valid in `.py` files".to_string(),
            dots,
        );

        let Some(upper) = self.at_expr().then(|| {
            Box::new(
                self.parse_conditional_expression_or_higher_impl(
                    ExpressionContext::default().with_in_type_expression(),
                )
                .expr,
            )
        }) else {
            self.add_error(ParseErrorType::IncompleteTypeParamBoundRange, dots);
            return (None, Some(lower));
        };

        if self.options.is_basedpython {
            (Some(lower), Some(upper))
        } else {
            (None, Some(upper))
        }
    }

    /// Validate that the given expression is a valid assignment target.
    ///
    /// If the expression is a list or tuple, then validate each element in the list.
    /// If it's a starred expression, then validate the value of the starred expression.
    ///
    /// Report an error for each invalid assignment expression found.
    pub(super) fn validate_assignment_target(&mut self, expr: &Expr) {
        let mut invalid = Vec::new();
        walk_invalid_assignment_targets(expr, &mut |expr| invalid.push(expr.range()));
        for range in invalid {
            self.add_error(ParseErrorType::InvalidAssignmentTarget, range);
        }
    }

    /// Validate that the given expression is a valid annotated assignment target.
    ///
    /// Unlike [`Parser::validate_assignment_target`], starred, list and tuple
    /// expressions aren't allowed here.
    fn validate_annotated_assignment_target(&mut self, expr: &Expr) {
        match expr {
            Expr::List(_) => self.add_error(
                ParseErrorType::OtherError(
                    "Only single target (not list) can be annotated".to_string(),
                ),
                expr,
            ),
            Expr::Tuple(_) => self.add_error(
                ParseErrorType::OtherError(
                    "Only single target (not tuple) can be annotated".to_string(),
                ),
                expr,
            ),
            Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_) => {}
            _ => self.add_error(ParseErrorType::InvalidAnnotatedAssignmentTarget, expr),
        }
    }

    /// Validate that the given expression is a valid delete target.
    ///
    /// If the expression is a list or tuple, then validate each element in the list.
    ///
    /// See: <https://github.com/python/cpython/blob/d864b0094f9875c5613cbb0b7f7f3ca8f1c6b606/Parser/action_helpers.c#L1150-L1180>
    fn validate_delete_target(&mut self, expr: &Expr) {
        match expr {
            Expr::List(ast::ExprList { elts, .. }) | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                for expr in elts {
                    self.validate_delete_target(expr);
                }
            }
            Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_) => {}
            _ => self.add_error(ParseErrorType::InvalidDeleteTarget, expr),
        }
    }

    /// Classify the `match` soft keyword token.
    ///
    /// # Panics
    ///
    /// If the parser isn't positioned at a `match` token.
    fn classify_match_token(&mut self) -> MatchTokenKind {
        assert_eq!(self.current_token_kind(), TokenKind::Match);

        let (first, second) = self.peek2();

        match first {
            // test_ok match_classify_as_identifier_1
            // match not in case
            TokenKind::Not if second == TokenKind::In => MatchTokenKind::Identifier,

            // test_ok match_classify_as_keyword_1
            // match foo:
            //     case _: ...
            // match 1:
            //     case _: ...
            // match 1.0:
            //     case _: ...
            // match 1j:
            //     case _: ...
            // match "foo":
            //     case _: ...
            // match f"foo {x}":
            //     case _: ...
            // match {1, 2}:
            //     case _: ...
            // match ~foo:
            //     case _: ...
            // match ...:
            //     case _: ...
            // match not foo:
            //     case _: ...
            // match await foo():
            //     case _: ...
            // match lambda foo: foo:
            //     case _: ...

            // test_err match_classify_as_keyword
            // match yield foo:
            //     case _: ...
            TokenKind::Name
            | TokenKind::Int
            | TokenKind::Float
            | TokenKind::Complex
            | TokenKind::String
            | TokenKind::FStringStart
            | TokenKind::TStringStart
            | TokenKind::Lbrace
            | TokenKind::Tilde
            | TokenKind::Ellipsis
            | TokenKind::Not
            | TokenKind::Await
            | TokenKind::Yield
            | TokenKind::Lambda => MatchTokenKind::Keyword,

            // test_ok match_classify_as_keyword_or_identifier
            // match (1, 2)  # Identifier
            // match (1, 2):  # Keyword
            //     case _: ...
            // match [1:]  # Identifier
            // match [1, 2]:  # Keyword
            //     case _: ...
            // match * foo  # Identifier
            // match - foo  # Identifier
            // match -foo:  # Keyword
            //     case _: ...

            // test_err match_classify_as_keyword_or_identifier
            // match *foo:  # Keyword
            //     case _: ...
            TokenKind::Lpar
            | TokenKind::Lsqb
            | TokenKind::Star
            | TokenKind::Plus
            | TokenKind::Minus => MatchTokenKind::KeywordOrIdentifier,

            _ => {
                if first.is_soft_keyword() || first.is_singleton() {
                    // test_ok match_classify_as_keyword_2
                    // match match:
                    //     case _: ...
                    // match case:
                    //     case _: ...
                    // match type:
                    //     case _: ...
                    // match None:
                    //     case _: ...
                    // match True:
                    //     case _: ...
                    // match False:
                    //     case _: ...
                    MatchTokenKind::Keyword
                } else {
                    // test_ok match_classify_as_identifier_2
                    // match
                    // match != foo
                    // (foo, match)
                    // [foo, match]
                    // {foo, match}
                    // match;
                    // match: int
                    // match,
                    // match.foo
                    // match / foo
                    // match << foo
                    // match and foo
                    // match is not foo
                    MatchTokenKind::Identifier
                }
            }
        }
    }

    /// Parses a sequence of clauses.
    ///
    /// The parser only continues for as long as it sees the token indicating the start of the
    /// specific clause. Unlike [`Parser::parse_list`], this method does not perform error recovery
    /// when the next token is not a list terminator or the start of a list element.
    ///
    /// The special method is necessary because Python uses indentation over explicit delimiters to
    /// indicate the end of a clause.
    ///
    /// ```python
    /// if True: ...
    /// elif False: ...
    /// elf x: ....
    /// else: ...
    /// ```
    ///
    /// It would be nice if the above example would recover and either skip over the `elf x: ...`
    /// or parse it as a nested statement so that the parser recognises the `else` clause. But
    /// Python makes this hard (without writing custom error recovery logic) because `elf x: `
    /// could also be an annotated assignment that went wrong ;)
    ///
    /// For now, don't recover when parsing clause headers, but add the terminator tokens (e.g.
    /// `Else`) to the recovery context so that expression recovery stops when it encounters an
    /// `else` token.
    fn parse_clauses(&mut self, clause: Clause, mut parse_clause: impl FnMut(&mut Parser<'src>)) {
        let mut progress = ParserProgress::default();

        let recovery_kind = match clause {
            Clause::ElIf => RecoveryContextKind::Elif,
            Clause::Except => RecoveryContextKind::Except,
            _ => unreachable!("Clause is not supported"),
        };

        let saved_context = self.recovery_context;
        self.recovery_context = self
            .recovery_context
            .union(RecoveryContext::from_kind(recovery_kind));

        while recovery_kind.is_list_element(self) {
            progress.assert_progressing(self);

            parse_clause(self);
        }

        self.recovery_context = saved_context;
    }
}

#[derive(Copy, Clone)]
enum Clause {
    If,
    Else,
    ElIf,
    For,
    With,
    Class,
    While,
    FunctionDef,
    Case,
    Try,
    Except,
    Finally,
}

impl Display for Clause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Clause::If => write!(f, "`if` statement"),
            Clause::Else => write!(f, "`else` clause"),
            Clause::ElIf => write!(f, "`elif` clause"),
            Clause::For => write!(f, "`for` statement"),
            Clause::With => write!(f, "`with` statement"),
            Clause::Class => write!(f, "`class` definition"),
            Clause::While => write!(f, "`while` statement"),
            Clause::FunctionDef => write!(f, "function definition"),
            Clause::Case => write!(f, "`case` block"),
            Clause::Try => write!(f, "`try` statement"),
            Clause::Except => write!(f, "`except` clause"),
            Clause::Finally => write!(f, "`finally` clause"),
        }
    }
}

/// The classification of the `match` token.
///
/// The `match` token is a soft keyword which means, depending on the context, it can be used as a
/// keyword or an identifier.
#[derive(Debug, Clone, Copy)]
enum MatchTokenKind {
    /// The `match` token is used as a keyword.
    ///
    /// For example:
    /// ```python
    /// match foo:
    ///     case _:
    ///         pass
    /// ```
    Keyword,

    /// The `match` token is used as an identifier.
    ///
    /// For example:
    /// ```python
    /// match.values()
    /// match is None
    /// ````
    Identifier,

    /// The `match` token is used as either a keyword or an identifier.
    ///
    /// For example:
    /// ```python
    /// # Used as a keyword
    /// match [x, y]:
    ///     case [1, 2]:
    ///         pass
    ///
    /// # Used as an identifier
    /// match[x]
    /// ```
    KeywordOrIdentifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WithItemParsingState {
    /// Parsing the with items without any ambiguity.
    Regular,

    /// Parsing the with items in a speculative mode.
    Speculative,
}

#[derive(Debug)]
struct ParsedWithItem {
    /// The contained with item.
    item: WithItem,
    /// If the context expression of the item is parenthesized.
    is_parenthesized: bool,
}

#[derive(Debug, Copy, Clone)]
enum ElifOrElse {
    Elif,
    Else,
}

impl ElifOrElse {
    const fn is_elif(self) -> bool {
        matches!(self, ElifOrElse::Elif)
    }

    const fn as_token_kind(self) -> TokenKind {
        match self {
            ElifOrElse::Elif => TokenKind::Elif,
            ElifOrElse::Else => TokenKind::Else,
        }
    }

    const fn as_clause(self) -> Clause {
        match self {
            ElifOrElse::Elif => Clause::ElIf,
            ElifOrElse::Else => Clause::Else,
        }
    }
}

/// The kind of the except clause.
#[derive(Debug, Copy, Clone)]
enum ExceptClauseKind {
    /// A normal except clause e.g., `except Exception as e: ...`.
    Normal,
    /// An except clause with a star e.g., `except *: ...`.
    ///
    /// Contains the star's [`TextRange`] for error reporting.
    Star(TextRange),
}

impl ExceptClauseKind {
    const fn is_star(self) -> bool {
        matches!(self, ExceptClauseKind::Star(..))
    }
}

#[derive(Debug, Copy, Clone)]
enum AllowStarAnnotation {
    Yes,
    No,
    /// basedpython: `**kwargs: *Kwargs` unpacks a keyword-variadic pack into this parameter,
    /// mirroring how `*args: *Ts` unpacks a `TypeVarTuple`. Rejected in `.py` files
    KeywordPackOnly,
}

/// basedpython: whether `body` needs `await` to run — the block it belongs to is
/// then an async one, and the callee it is handed to has to await the call.
///
/// A trailing lambda block is a function of its own, so `await` in it is a
/// statement about *it* rather than about the `def` it was written inside. Only
/// the block's own statements are looked at: a nested `def` or `lambda` is a
/// separate function and its `await` is its own business.
fn body_awaits(body: &[Stmt]) -> bool {
    struct Finder {
        found: bool,
    }

    impl<'a> ruff_python_ast::visitor::Visitor<'a> for Finder {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                // a nested definition's *body* is its own scope; its header runs
                // here — decorators (a nested block's callee among them),
                // parameter defaults and annotations, the return annotation, the
                // type-parameter bounds, and a class's bases
                Stmt::FunctionDef(function) => {
                    for decorator in &function.decorator_list {
                        self.visit_expr(&decorator.expression);
                    }
                    if let Some(type_params) = &function.type_params {
                        self.visit_type_params(type_params);
                    }
                    self.visit_parameters(&function.parameters);
                    if let Some(returns) = &function.returns {
                        self.visit_annotation(returns);
                    }
                }
                Stmt::ClassDef(class) => {
                    for decorator in &class.decorator_list {
                        self.visit_expr(&decorator.expression);
                    }
                    if let Some(type_params) = &class.type_params {
                        self.visit_type_params(type_params);
                    }
                    if let Some(arguments) = &class.arguments {
                        self.visit_arguments(arguments);
                    }
                }
                Stmt::For(ast::StmtFor { is_async: true, .. })
                | Stmt::With(ast::StmtWith { is_async: true, .. }) => self.found = true,
                _ if !self.found => ruff_python_ast::visitor::walk_stmt(self, stmt),
                _ => {}
            }
        }

        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                // a lambda's body is its own scope; its parameter defaults are
                // evaluated where the lambda is written
                Expr::Lambda(lambda) => {
                    if let Some(parameters) = &lambda.parameters {
                        self.visit_parameters(parameters);
                    }
                }
                Expr::Await(_) => self.found = true,
                // a comprehension's `async for` is an await of its own, and it
                // has no statement to carry the flag
                Expr::ListComp(ast::ExprListComp { generators, .. })
                | Expr::SetComp(ast::ExprSetComp { generators, .. })
                | Expr::Generator(ast::ExprGenerator { generators, .. })
                    if generators.iter().any(|generator| generator.is_async) =>
                {
                    self.found = true;
                }
                Expr::DictComp(ast::ExprDictComp { generators, .. })
                    if generators.iter().any(|generator| generator.is_async) =>
                {
                    self.found = true;
                }
                _ if !self.found => ruff_python_ast::visitor::walk_expr(self, expr),
                _ => {}
            }
        }
    }

    let mut finder = Finder { found: false };
    for stmt in body {
        ruff_python_ast::visitor::Visitor::visit_stmt(&mut finder, stmt);
    }
    finder.found
}

/// basedpython: collects the expressions that hold the value of `expr` — `expr`
/// itself, and, for operators that choose between operands rather than combining
/// them, the operands they may choose.
///
/// These are the positions a [statement expression](ast::ExprStatement) may
/// occupy; see [`Parser::validate_statement_expressions`].
fn collect_tail_positions<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    out.push(expr);
    match expr {
        Expr::BoolOp(bool_op) => {
            for value in &bool_op.values {
                collect_tail_positions(value, out);
            }
        }
        Expr::If(if_expr) => {
            collect_tail_positions(&if_expr.body, out);
            collect_tail_positions(&if_expr.orelse, out);
        }
        Expr::Named(named) => collect_tail_positions(&named.value, out),
        // `??` chooses its right operand only when the left one is `None`
        Expr::BinOp(bin_op) if matches!(bin_op.op, ast::Operator::Coalesce) => {
            collect_tail_positions(&bin_op.right, out);
        }
        _ => {}
    }
}

/// basedpython: collects every statement expression directly in `stmt`, paired
/// with the [`Expr`] node wrapping it.
///
/// No suite is descended into — neither a statement expression's own, nor the
/// one a trailing-lambda block hands its synthesized function: the statements
/// in it are validated when they are parsed, against *their* value position.
fn collect_statement_expressions<'a>(
    stmt: &'a Stmt,
    out: &mut Vec<(&'a Expr, &'a ast::ExprStatement)>,
) {
    struct Finder<'a, 'b> {
        out: &'b mut Vec<(&'a Expr, &'a ast::ExprStatement)>,
    }

    impl<'a> ruff_python_ast::visitor::Visitor<'a> for Finder<'a, '_> {
        fn visit_stmt(&mut self, _stmt: &'a Stmt) {}

        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Statement(statement) = expr {
                self.out.push((expr, statement));
                return;
            }
            ruff_python_ast::visitor::walk_expr(self, expr);
        }
    }

    let mut finder = Finder { out };
    ruff_python_ast::visitor::walk_stmt(&mut finder, stmt);
}

/// basedpython: replaces every statement expression at one of `discarded`'s
/// ranges with the parser's inert recovery expression.
///
/// A statement expression that the position rule rejects is a parse error, and
/// nothing downstream can make sense of it: it stands where the statement it
/// wraps has no place to be lowered to, and the rest of the compiler reads the
/// statement's contents — the names it declares, above all — as belonging to the
/// scope the statement expression is written in. That only holds where the rule
/// does, so a rejected one keeps its source range and nothing else.
fn discard_statement_expressions(stmt: &mut Stmt, discarded: &[TextRange]) {
    struct Discarder<'a> {
        discarded: &'a [TextRange],
    }

    impl Transformer for Discarder<'_> {
        fn visit_expr(&self, expr: &mut Expr) {
            if let Expr::Statement(statement) = expr
                && self.discarded.contains(&statement.range)
            {
                *expr = Expr::Name(ast::ExprName {
                    range: statement.range,
                    id: Name::empty(),
                    ctx: ExprContext::Invalid,
                    node_index: AtomicNodeIndex::NONE,
                });
                return;
            }
            transformer::walk_expr(self, expr);
        }
    }

    if discarded.is_empty() {
        return;
    }

    Discarder { discarded }.visit_stmt(stmt);
}

/// basedpython: whether a `context` prefix may mark this parameter. `No` for
/// `*args` / `**kwargs`, where an implicit keyword argument cannot land
#[derive(Debug, Copy, Clone)]
enum AllowContextModifier {
    Yes,
    No,
}

#[derive(Debug, Copy, Clone)]
enum ImportStyle {
    /// E.g., `import foo, bar`
    Import,
    /// E.g., `from foo import bar, baz`
    ImportFrom,
}

#[cfg(test)]
mod property_tests {
    use crate::parse_unchecked_source;
    use ruff_python_ast::{Expr, PySourceType, Stmt};

    /// The class body members a property construct lowered to, plus any parse errors.
    fn class_body(source: &str, source_type: PySourceType) -> (Vec<Stmt>, Vec<String>) {
        let parsed = parse_unchecked_source(source, source_type);
        let errors = parsed.errors().iter().map(ToString::to_string).collect();
        let body = parsed
            .syntax()
            .body
            .iter()
            .find_map(|stmt| match stmt {
                Stmt::ClassDef(class) => Some(class.body.iter().cloned().collect()),
                _ => None,
            })
            .unwrap_or_default();
        (body, errors)
    }

    fn parse_by(source: &str) -> (Vec<Stmt>, Vec<String>) {
        class_body(source, PySourceType::BasedPython)
    }

    /// name of a class-body member, for shape assertions
    fn member_name(stmt: &Stmt) -> String {
        match stmt {
            Stmt::FunctionDef(f) => format!("def {}", f.name),
            Stmt::AnnAssign(a) => match a.target.as_ref() {
                ruff_python_ast::Expr::Name(n) => format!("annassign {}", n.id),
                _ => "annassign ?".to_string(),
            },
            Stmt::Assign(a) => match a.targets.first() {
                Some(ruff_python_ast::Expr::Name(n)) => format!("assign {}", n.id),
                _ => "assign ?".to_string(),
            },
            _ => "other".to_string(),
        }
    }

    /// the doc's motivating example: a stored `var` property with both accessors
    /// lowers to a backing field, a `@property` getter, and a setter
    #[test]
    fn stored_var_property() {
        let (body, errors) = parse_by(
            "class Person:\n    var age: int = 0\n        get() = field\n        set(value):\n            assert value >= 0\n            field = value\n",
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(
            names,
            vec!["def age", "annassign __age", "def age"],
            "members: {names:?}"
        );
    }

    /// an accessor that never mentions `field` allocates no backing storage
    #[test]
    fn computed_property_has_no_backing_field() {
        let (body, errors) = parse_by(
            "class Rect:\n    var w: int = 0\n    let area: int\n        get() = self.w * self.w\n",
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        // `var w` stays a plain declaration; `area` becomes a getter only
        assert_eq!(names, vec!["annassign w", "def area"], "members: {names:?}");
    }

    /// an explicit `field:` declaration sets the backing type and initialiser
    #[test]
    fn explicit_backing_field() {
        let (body, errors) = parse_by(
            "class Bag:\n    let items: Sequence[int]\n        field: list[int] = []\n        get() = field\n",
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(
            names,
            vec!["def items", "annassign __items"],
            "members: {names:?}"
        );
        // the backing field carries the explicit type, not the property's
        let Stmt::AnnAssign(backing) = &body[1] else {
            panic!("expected backing AnnAssign, got {:?}", body[1]);
        };
        assert!(
            backing.value.is_some(),
            "backing field keeps its initialiser"
        );
    }

    /// `var` with only a getter gains a pass-through setter
    #[test]
    fn var_get_only_gains_passthrough_setter() {
        let (body, errors) = parse_by("class A:\n    var x: int = 0\n        get() = field\n");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(
            names,
            vec!["def x", "annassign __x", "def x"],
            "members: {names:?}"
        );
    }

    /// a read-only `let` property may not define a setter
    #[test]
    fn let_with_setter_is_rejected() {
        let (_, errors) = parse_by(
            "class A:\n    let x: int = 0\n        get() = field\n        set(value):\n            field = value\n",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("read-only property cannot define a setter")),
            "got: {errors:?}"
        );
    }

    #[test]
    fn duplicate_get_is_rejected() {
        let (_, errors) = parse_by(
            "class A:\n    var x: int = 0\n        get() = field\n        get() = field\n",
        );
        assert!(
            errors.iter().any(|e| e.contains("duplicate `get`")),
            "got: {errors:?}"
        );
    }

    /// an explicit `field` nobody references is a mistake
    #[test]
    fn unreferenced_explicit_field_is_rejected() {
        let (_, errors) = parse_by(
            "class A:\n    let x: int\n        field: int = 0\n        get() = self.other\n",
        );
        assert!(
            errors.iter().any(|e| e.contains("never referenced")),
            "got: {errors:?}"
        );
    }

    /// two initialiser sites for one piece of storage is ambiguous
    #[test]
    fn field_and_property_initialiser_conflict_is_rejected() {
        let (_, errors) = parse_by(
            "class A:\n    var x: int = 1\n        field: int = 2\n        get() = field\n",
        );
        assert!(
            errors.iter().any(|e| e.contains("cannot be combined")),
            "got: {errors:?}"
        );
    }

    /// `late` defers initialisation, so pairing it with one is contradictory
    #[test]
    fn late_field_with_initialiser_is_rejected() {
        let (_, errors) = parse_by(
            "class A:\n    let x: int\n        late field: int = 0\n        get() = field\n",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot be combined with an initialiser")),
            "got: {errors:?}"
        );
    }

    /// `late` is only meaningful on the backing field declaration
    #[test]
    fn late_before_get_is_rejected() {
        let (_, errors) = parse_by("class A:\n    var x: int = 0\n        late get() = field\n");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("may only precede `field`")),
            "got: {errors:?}"
        );
    }

    /// a `late field` block still parses to backing storage plus a getter
    #[test]
    fn late_field_parses() {
        let (body, errors) = parse_by(
            "class Bag:\n    let items: list[int]\n        late field: list[int]\n        get() = field\n",
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(
            names,
            vec!["def items", "annassign __items"],
            "members: {names:?}"
        );
    }

    /// an explicit `field` declaration is a complete property: the getter is
    /// implicit, which is the point of stating storage separately from the public
    /// type (kotlin's explicit-backing-field shape)
    #[test]
    fn field_without_accessors_is_a_property() {
        let (body, errors) =
            parse_by("class A:\n    let a: Sequence[int]\n        field: list[int] = []\n");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(names, vec!["def a", "annassign __a"], "members: {names:?}");
    }

    /// an accessor block with neither storage nor an accessor declares nothing
    #[test]
    fn empty_accessor_block_is_rejected() {
        let (_, errors) = parse_by("class A:\n    var x: int = 0\n        get\n");
        assert!(!errors.is_empty(), "expected a rejection");
    }

    /// an unannotated `field = v` takes its type from the initialiser, not from the
    /// property's (wider) public type. the property's type is still carried as an
    /// `__field__[T]` *inference context* so an uninformative initialiser like `[]`
    /// can be solved against it
    #[test]
    fn unannotated_field_infers_from_initialiser() {
        let (body, errors) =
            parse_by("class A:\n    let a: object\n        field = 1\n        get() = field\n");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(names, vec!["def a", "annassign __a"], "members: {names:?}");
        let Stmt::AnnAssign(backing) = &body[1] else {
            panic!("expected backing AnnAssign, got {:?}", body[1]);
        };
        // the annotation is the context marker, not a declared storage type
        assert!(
            matches!(backing.annotation.as_ref(), Expr::Subscript(s)
                if matches!(s.value.as_ref(), Expr::Name(n) if n.id.as_str() == "__field__")),
            "expected an `__field__[T]` marker, got {:?}",
            backing.annotation
        );
    }

    /// an untyped property has no context to offer, so the backing stays a bare
    /// assignment
    #[test]
    fn unannotated_field_on_untyped_property() {
        let (body, errors) = parse_by("class A:\n    let a\n        field = 2\n");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(names, vec!["def a", "assign __a"], "members: {names:?}");
    }

    /// accessor blocks are basedpython-only syntax
    #[test]
    fn accessor_block_rejected_in_python_file() {
        let (_, errors) = class_body(
            "class A:\n    var x: int = 0\n        get() = field\n",
            PySourceType::Python,
        );
        assert!(!errors.is_empty(), "expected a .py rejection");
    }

    /// a module-level parse, for the statement-expression tests below
    fn parse_module(source: &str, source_type: PySourceType) -> (Vec<Stmt>, Vec<String>) {
        let parsed = parse_unchecked_source(source, source_type);
        let errors = parsed.errors().iter().map(ToString::to_string).collect();
        (parsed.syntax().body.iter().cloned().collect(), errors)
    }

    /// the assignment's value is an `ExprStatement` wrapping the `match`
    #[test]
    fn statement_expression_wraps_the_statement() {
        let (body, errors) = parse_module(
            "a = match x:\n    case 1:\n        2\n",
            PySourceType::BasedPython,
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let Some(Stmt::Assign(assign)) = body.first() else {
            panic!("expected an assignment, got: {body:?}")
        };
        let ruff_python_ast::Expr::Statement(statement) = assign.value.as_ref() else {
            panic!("expected a statement expression, got: {:?}", assign.value)
        };
        assert!(statement.stmt.is_match_stmt(), "got: {:?}", statement.stmt);
    }

    /// the suite swallows the statement's newline, so nothing follows it
    #[test]
    fn statement_expression_consumes_its_own_terminator() {
        let (body, errors) = parse_module(
            "a = if c:\n    1\nelse:\n    2\nb = 3\n",
            PySourceType::BasedPython,
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(body.len(), 2, "got: {body:?}");
    }

    /// `match` used as an ordinary name is still an ordinary name
    #[test]
    fn match_as_an_identifier_is_not_a_statement_expression() {
        let (body, errors) = parse_module("a = match\n", PySourceType::BasedPython);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let Some(Stmt::Assign(assign)) = body.first() else {
            panic!("expected an assignment, got: {body:?}")
        };
        assert!(assign.value.is_name_expr(), "got: {:?}", assign.value);
    }

    /// `break` may carry a value, which a loop expression reads
    #[test]
    fn break_carries_a_value() {
        let (body, errors) = parse_module(
            "a = for i in xs:\n    break i\nelse:\n    0\n",
            PySourceType::BasedPython,
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(body.len(), 1, "got: {body:?}");
    }

    /// a form with a suite has to be the whole value of its statement
    #[test]
    fn suite_form_off_the_tail_is_rejected() {
        let (_, errors) = parse_module(
            "a = 1 + if c:\n    1\nelse:\n    2\n",
            PySourceType::BasedPython,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("whole value of its statement")),
            "errors: {errors:?}"
        );
    }

    /// a diverging form still has to be in tail position
    #[test]
    fn diverging_form_off_the_tail_is_rejected() {
        let (_, errors) = parse_module("a = [raise ValueError()]\n", PySourceType::BasedPython);
        assert!(
            errors.iter().any(|e| e.contains("tail of its statement")),
            "errors: {errors:?}"
        );
    }

    /// a diverging form is allowed under the operators that choose an operand
    #[test]
    fn diverging_form_under_a_choosing_operator() {
        for source in [
            "a = b or raise ValueError()\n",
            "a = b ?? raise ValueError()\n",
            "a = b if c else raise ValueError()\n",
            "a = b ?? return None\n",
        ] {
            let (_, errors) = parse_module(source, PySourceType::BasedPython);
            assert!(errors.is_empty(), "{source:?} gave errors: {errors:?}");
        }
    }

    /// a suite continues the line its statement starts on, so nothing may precede it
    #[test]
    fn suite_form_after_another_statement_on_the_line_is_rejected() {
        let (_, errors) = parse_module(
            "p = 1; q = match x:\n    case _:\n        2\n",
            PySourceType::BasedPython,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("first statement on its line")),
            "errors: {errors:?}"
        );
    }

    /// a one-line suite is the same rule
    #[test]
    fn suite_form_after_another_statement_in_a_block_is_rejected() {
        let (_, errors) = parse_module(
            "if c: p = 1; q = match x:\n    case _:\n        2\n",
            PySourceType::BasedPython,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("first statement on its line")),
            "errors: {errors:?}"
        );
    }

    /// a diverging form has no suite, so it is unaffected by the line rule
    #[test]
    fn diverging_form_after_another_statement_on_the_line_is_allowed() {
        let (_, errors) = parse_module(
            "p = 1; q = b or raise ValueError()\n",
            PySourceType::BasedPython,
        );
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// statement expressions are basedpython-only syntax
    #[test]
    fn statement_expression_rejected_in_python_file() {
        let (_, errors) = parse_module(
            "a = match x:\n    case 1:\n        2\n",
            PySourceType::Python,
        );
        assert!(!errors.is_empty(), "expected a .py rejection");
    }

    /// a plain declaration with no accessor block is untouched
    #[test]
    fn plain_declaration_is_not_a_property() {
        let (body, errors) = parse_by("class A:\n    var x: int = 0\n    let y: int = 1\n");
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<String> = body.iter().map(member_name).collect();
        assert_eq!(
            names,
            vec!["annassign x", "annassign y"],
            "members: {names:?}"
        );
    }
}
