//! Comma-separated lists — a call's arguments, a definition's parameters, the elements of a
//! display, the names of an import — and the elements of each, as the parser read them.
//!
//! An element is read off the parse tree, not off the text between two commas: a lambda among the
//! arguments has commas of its own (`f(lambda a, b: a, c)` has two arguments), and so does a nested
//! call, and only the tree tells those apart from the list's. What the tree has no node for is
//! found in the gaps between the nodes, where the only tokens are the list's own: the `/` and bare
//! `*` that separate parameters, which are elements of their own, and the `*`, `**` that unpack an
//! element or mark a variadic parameter, which belong to the element after them.

use ruff_python_ast::token::{Token, TokenKind, Tokens, parenthesized_range};
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr};
use ruff_text_size::{Ranged, TextRange};

/// What a list holds, which is what a client calls its elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListKind {
    /// A call's arguments, or a class's bases and keywords.
    Arguments,
    /// A `def`'s or a `lambda`'s parameters.
    Parameters,
    /// A type parameter list.
    TypeParameters,
    /// The elements of a list, tuple, set or dict display, or of a subscript's tuple.
    Elements,
    /// The names an `import`, `global` or `nonlocal` statement lists.
    Names,
    /// The targets of a `del`.
    Targets,
    /// The context managers of a `with`.
    ContextManagers,
}

/// One comma-separated list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommaList {
    pub(crate) kind: ListKind,
    /// The bracket tokens around the list, when it has brackets of its own: a call's parentheses,
    /// a display's brackets, a parenthesised tuple's parentheses. `None` for a list that runs
    /// without them, like a lambda's parameters or an import's names.
    pub(crate) brackets: Option<(TextRange, TextRange)>,
    /// The elements, in source order, each from its first token to its last: parentheses around
    /// an element, and a `*` or `**` before one, included.
    pub(crate) elements: Vec<TextRange>,
}

impl CommaList {
    /// Between the brackets, both excluded; `None` for a list without brackets of its own.
    pub(crate) fn inside(&self) -> Option<TextRange> {
        let (open, close) = self.brackets?;
        Some(TextRange::new(open.end(), close.start()))
    }
}

/// The list `node` is, if it is one.
///
/// `None` for anything else, and for a list whose elements could not all be placed — which a
/// parse that recovered from an error can leave behind.
pub(crate) fn comma_list(node: AnyNodeRef, tokens: &Tokens) -> Option<CommaList> {
    let expressions = |elements: &[Expr], parent: AnyNodeRef| -> Vec<TextRange> {
        elements
            .iter()
            .map(|element| {
                parenthesized_range(element.into(), parent, tokens).unwrap_or(element.range())
            })
            .collect()
    };

    let (kind, brackets, bounds, nodes) = match node {
        AnyNodeRef::Arguments(arguments) => {
            let nodes = arguments
                .iter_source_order()
                .map(|argument| match argument {
                    ast::ArgOrKeyword::Arg(expr) => {
                        parenthesized_range(expr.into(), node, tokens).unwrap_or(expr.range())
                    }
                    ast::ArgOrKeyword::Keyword(keyword) => keyword.range(),
                })
                .collect();
            (
                ListKind::Arguments,
                bracketed(arguments.range(), tokens, TokenKind::Lpar, TokenKind::Rpar),
                arguments.range(),
                nodes,
            )
        }
        AnyNodeRef::Parameters(parameters) => {
            let nodes = parameters
                .iter()
                .map(|parameter| parameter.range())
                .collect();
            (
                ListKind::Parameters,
                bracketed(parameters.range(), tokens, TokenKind::Lpar, TokenKind::Rpar),
                parameters.range(),
                nodes,
            )
        }
        AnyNodeRef::TypeParams(type_params) => (
            ListKind::TypeParameters,
            bracketed(
                type_params.range(),
                tokens,
                TokenKind::Lsqb,
                TokenKind::Rsqb,
            ),
            type_params.range(),
            type_params.type_params.iter().map(Ranged::range).collect(),
        ),
        AnyNodeRef::ExprList(list) => (
            ListKind::Elements,
            bracketed(list.range(), tokens, TokenKind::Lsqb, TokenKind::Rsqb),
            list.range(),
            expressions(&list.elts, node),
        ),
        AnyNodeRef::ExprSet(set) => (
            ListKind::Elements,
            bracketed(set.range(), tokens, TokenKind::Lbrace, TokenKind::Rbrace),
            set.range(),
            expressions(&set.elts, node),
        ),
        AnyNodeRef::ExprTuple(tuple) => (
            ListKind::Elements,
            if tuple.parenthesized {
                bracketed(tuple.range(), tokens, TokenKind::Lpar, TokenKind::Rpar)
            } else {
                None
            },
            tuple.range(),
            expressions(&tuple.elts, node),
        ),
        AnyNodeRef::ExprDict(dict) => {
            let nodes = dict
                .items
                .iter()
                .map(|item| {
                    let value = parenthesized_range((&item.value).into(), node, tokens)
                        .unwrap_or(item.value.range());
                    match &item.key {
                        Some(key) => parenthesized_range(key.into(), node, tokens)
                            .unwrap_or(key.range())
                            .cover(value),
                        // the `**` before it is found in the gap
                        None => value,
                    }
                })
                .collect();
            (
                ListKind::Elements,
                bracketed(dict.range(), tokens, TokenKind::Lbrace, TokenKind::Rbrace),
                dict.range(),
                nodes,
            )
        }
        AnyNodeRef::StmtImport(import) => names(
            ListKind::Names,
            import.names.iter().map(Ranged::range).collect(),
        )?,
        AnyNodeRef::StmtImportFrom(import) => names(
            ListKind::Names,
            import.names.iter().map(Ranged::range).collect(),
        )?,
        AnyNodeRef::StmtGlobal(global) => names(
            ListKind::Names,
            global.names.iter().map(Ranged::range).collect(),
        )?,
        AnyNodeRef::StmtNonlocal(nonlocal) => names(
            ListKind::Names,
            nonlocal.names.iter().map(Ranged::range).collect(),
        )?,
        AnyNodeRef::StmtDelete(delete) => {
            names(ListKind::Targets, expressions(&delete.targets, node))?
        }
        AnyNodeRef::StmtWith(with) => names(
            ListKind::ContextManagers,
            with.items.iter().map(Ranged::range).collect(),
        )?,
        _ => return None,
    };

    let bounds = match brackets {
        Some((open, close)) => TextRange::new(open.end(), close.start()),
        None => bounds,
    };
    let elements = with_gaps(bounds, &nodes, tokens)?;
    Some(CommaList {
        kind,
        brackets,
        elements,
    })
}

/// A list of `nodes` with no brackets of its own, bounded by its first and last node.
#[expect(clippy::type_complexity)]
fn names(
    kind: ListKind,
    nodes: Vec<TextRange>,
) -> Option<(
    ListKind,
    Option<(TextRange, TextRange)>,
    TextRange,
    Vec<TextRange>,
)> {
    let bounds = nodes.first()?.cover(*nodes.last()?);
    Some((kind, None, bounds, nodes))
}

/// The `open` and `close` tokens at either end of `range`, when it starts and ends with them.
fn bracketed(
    range: TextRange,
    tokens: &Tokens,
    open: TokenKind,
    close: TokenKind,
) -> Option<(TextRange, TextRange)> {
    let inner = tokens_within(tokens, range);
    let first = inner.iter().find(|token| !token.kind().is_trivia())?;
    let last = inner.iter().rev().find(|token| !token.kind().is_trivia())?;
    (first.kind() == open
        && last.kind() == close
        && first.start() == range.start()
        && last.end() == range.end())
    .then(|| (first.range(), last.range()))
}

/// The elements of a list whose nodes are `nodes`, reading what lies between them inside
/// `bounds`: a run of tokens between two commas is an element of its own (a `/` or bare `*`
/// among parameters), and a run that ends at a node is part of it (the `**` of `**kwargs`).
///
/// `None` when the nodes are not in order inside the bounds, or a run between them is not one the
/// list can hold, either of which is a parse recovered from an error.
fn with_gaps(bounds: TextRange, nodes: &[TextRange], tokens: &Tokens) -> Option<Vec<TextRange>> {
    let mut elements = Vec::with_capacity(nodes.len());
    let mut position = bounds.start();
    for index in 0..=nodes.len() {
        let next = nodes.get(index).copied();
        let gap_end = next.map_or(bounds.end(), TextRange::start);
        if gap_end < position {
            return None;
        }
        let mut run: Option<TextRange> = None;
        for token in tokens_within(tokens, TextRange::new(position, gap_end)) {
            match token.kind() {
                kind if kind.is_trivia() => {}
                TokenKind::Comma => {
                    elements.extend(run.take());
                }
                TokenKind::Star | TokenKind::DoubleStar | TokenKind::Slash => {
                    run = Some(run.map_or(token.range(), |run| run.cover(token.range())));
                }
                _ => return None,
            }
        }
        match next {
            Some(node) => {
                if node.is_empty() {
                    return None;
                }
                elements.push(run.map_or(node, |run| run.cover(node)));
                position = node.end();
            }
            None => elements.extend(run),
        }
    }
    Some(elements)
}

/// The tokens wholly inside `range`.
///
/// Unlike [`Tokens::in_range`], this does not panic on a range whose ends fall inside a token,
/// which the nodes of a parse recovered from an error can have: the outline that reads these is
/// asked about every keystroke's text, the broken ones included.
pub(crate) fn tokens_within(tokens: &Tokens, range: TextRange) -> &[Token] {
    let start = tokens.partition_point(|token| token.start() < range.start());
    let end = tokens.partition_point(|token| token.end() <= range.end());
    tokens.get(start..end.max(start)).unwrap_or_default()
}

/// Every list in `suite` and everything nested in it, in the order their nodes start.
pub(crate) fn comma_lists(suite: &[ast::Stmt], tokens: &Tokens) -> Vec<CommaList> {
    struct Collector<'t> {
        tokens: &'t Tokens,
        lists: Vec<CommaList>,
    }

    impl<'a> SourceOrderVisitor<'a> for Collector<'_> {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            self.lists.extend(comma_list(node, self.tokens));
            TraversalSignal::Traverse
        }
    }

    let mut collector = Collector {
        tokens,
        lists: Vec::new(),
    };
    collector.visit_body(suite);
    collector.lists
}
