use ruff_formatter::{FormatRuleWithOptions, format_args};
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::helpers::is_top_star_marker;
use ruff_python_ast::{Expr, ExprTuple};
use ruff_text_size::{Ranged, TextRange};

use crate::builders::parenthesize_if_expands;
use crate::expression::parentheses::{
    NeedsParentheses, OptionalParentheses, empty_parenthesized, optional_parentheses, parenthesized,
};
use crate::other::commas::has_trailing_comma;
use crate::prelude::*;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum TupleParentheses {
    /// By default tuples with a single element will include parentheses. Tuples with multiple elements
    /// will parenthesize if the expression expands. This means that tuples will often *preserve*
    /// their parentheses, but this differs from `Preserve` in that we may also *introduce*
    /// parentheses as well.
    #[default]
    Default,

    /// Handle special cases where parentheses are to be preserved.
    ///
    /// Black omits parentheses for tuples inside subscripts except if the tuple is already
    /// parenthesized in the source code.
    /// ```python
    /// x[a, :]
    /// x[a, b:]
    /// x[(a, b):]
    /// ```
    Preserve,

    /// The same as [`Self::Default`] except that it uses [`optional_parentheses`] rather than
    /// [`parenthesize_if_expands`]. This avoids adding parentheses if breaking any containing parenthesized
    /// expression makes the tuple fit.
    ///
    /// Avoids adding parentheses around the tuple because breaking the `sum` call expression is sufficient
    /// to make it fit.
    ///
    /// ```python
    /// return len(self.nodeseeeeeeeee), sum(
    ///     len(node.parents) for node in self.node_map.values()
    /// )
    /// ```
    OptionalParentheses,

    /// Handle the special cases where we don't include parentheses at all.
    ///
    /// Black never formats tuple targets of for loops with parentheses if inside a comprehension.
    /// For example, tuple targets will always be formatted on the same line, except when an element supports
    /// line-breaking in an un-parenthesized context.
    /// ```python
    /// # Input
    /// {k: v for x, (k, v) in this_is_a_very_long_variable_which_will_cause_a_trailing_comma_which_breaks_the_comprehension}
    ///
    /// # Black
    /// {
    ///     k: v
    ///     for x, (
    ///         k,
    ///         v,
    ///     ) in this_is_a_very_long_variable_which_will_cause_a_trailing_comma_which_breaks_the_comprehension
    /// }
    /// ```
    Never,

    /// Handle the special cases where we don't include parentheses if they are not required.
    ///
    /// Normally, black keeps parentheses, but in the case of for loops it formats
    /// ```python
    /// for (a, b) in x:
    ///     pass
    /// ```
    /// to
    /// ```python
    /// for a, b in x:
    ///     pass
    /// ```
    /// Black still does use parentheses in these positions if the group breaks or magic trailing
    /// comma is used.
    ///
    /// Additional examples:
    /// ```python
    /// for (a,) in []:
    /// pass
    /// for a, b in []:
    ///     pass
    /// for a, b in []:  # Strips parentheses
    ///     pass
    /// for (
    ///     aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,
    ///     b,
    /// ) in []:
    ///     pass
    /// ```
    NeverPreserve,
}

#[derive(Default)]
pub struct FormatExprTuple {
    parentheses: TupleParentheses,
}

impl FormatRuleWithOptions<ExprTuple, PyFormatContext<'_>> for FormatExprTuple {
    type Options = TupleParentheses;

    fn with_options(mut self, options: Self::Options) -> Self {
        self.parentheses = options;
        self
    }
}

impl FormatNodeRule<ExprTuple> for FormatExprTuple {
    fn fmt_fields(&self, item: &ExprTuple, f: &mut PyFormatter) -> FormatResult<()> {
        let ExprTuple {
            elts,
            ctx: _,
            range: _,
            node_index: _,
            parenthesized: is_parenthesized,
            is_anon_named_tuple: _,
            is_anon_named_tuple_value: _,
            callable_shape: _,
            is_parameter_shape: _,
        } = item;

        // basedpython: anonymous named tuple type — `(name: T, name: T, ...)`.
        // Always parenthesized; format each `ExprNamed` element as `name: type`.
        if item.is_anon_named_tuple {
            return parenthesized("(", &AnonNamedTupleFields::new(item, ":"), ")").fmt(f);
        }
        // basedpython: anonymous named tuple value — `(name=expr, ...)`.
        if item.is_anon_named_tuple_value {
            return parenthesized("(", &AnonNamedTupleFields::new(item, "="), ")").fmt(f);
        }
        // basedpython: Parameters spec — `(int, str, /, name: T)`,
        // `(*: T)`, `(**: T)`, etc. dedicated formatter handles slash/star
        // markers + variadic / kwargs encodings
        if item.has_parameter_shape() {
            return parenthesized("(", &ParameterShapeFields::new(item), ")").fmt(f);
        }

        let comments = f.context().comments().clone();
        let dangling = comments.dangling(item);

        // Handle the edge cases of an empty tuple and a tuple with one element
        //
        // there can be dangling comments, and they can be in two
        // positions:
        // ```python
        // a3 = (  # end-of-line
        //     # own line
        // )
        // ```
        // In all other cases comments get assigned to a list element
        match elts.as_slice() {
            [] => empty_parenthesized("(", dangling, ")").fmt(f),
            [single] => match self.parentheses {
                TupleParentheses::Preserve if !is_parenthesized => {
                    single.format().fmt(f)?;
                    // The `TupleParentheses::Preserve` is only set by subscript expression
                    // formatting. With PEP 646, a single element starred expression in the slice
                    // position of a subscript expression is actually a tuple expression. For
                    // example:
                    //
                    // ```python
                    // data[*x]
                    // #    ^^ single element tuple expression without a trailing comma
                    //
                    // data[*x,]
                    // #    ^^^ single element tuple expression with a trailing comma
                    // ```
                    //
                    //
                    // This means that the formatter should only add a trailing comma if there is
                    // one already.
                    if has_trailing_comma(TextRange::new(single.end(), item.end()), f.context()) {
                        token(",").fmt(f)?;
                    }
                    Ok(())
                }
                _ =>
                // A single element tuple always needs parentheses and a trailing comma, except when inside of a subscript
                {
                    parenthesized("(", &format_args![single.format(), token(",")], ")")
                        .with_dangling_comments(dangling)
                        .fmt(f)
                }
            },
            // If the tuple has parentheses, we generally want to keep them. The exception are for
            // loops, see `TupleParentheses::NeverPreserve` doc comment.
            //
            // Unlike other expression parentheses, tuple parentheses are part of the range of the
            // tuple itself.
            _ if *is_parenthesized
                && !(self.parentheses == TupleParentheses::NeverPreserve
                    && dangling.is_empty()) =>
            {
                parenthesized("(", &ExprSequence::new(item), ")")
                    .with_dangling_comments(dangling)
                    .fmt(f)
            }
            _ => match self.parentheses {
                TupleParentheses::Never => {
                    let separator =
                        format_with(|f| group(&format_args![token(","), space()]).fmt(f));
                    f.join_with(separator)
                        .entries(elts.iter().formatted())
                        .finish()
                }
                TupleParentheses::Preserve => group(&ExprSequence::new(item)).fmt(f),
                TupleParentheses::NeverPreserve => {
                    optional_parentheses(&ExprSequence::new(item)).fmt(f)
                }
                TupleParentheses::OptionalParentheses if item.len() == 2 => {
                    optional_parentheses(&ExprSequence::new(item)).fmt(f)
                }
                TupleParentheses::Default | TupleParentheses::OptionalParentheses => {
                    parenthesize_if_expands(&ExprSequence::new(item)).fmt(f)
                }
            },
        }
    }
}

#[derive(Debug)]
struct ExprSequence<'a> {
    tuple: &'a ExprTuple,
}

impl<'a> ExprSequence<'a> {
    const fn new(expr: &'a ExprTuple) -> Self {
        Self { tuple: expr }
    }
}

impl Format<PyFormatContext<'_>> for ExprSequence<'_> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        f.join_comma_separated(self.tuple.end())
            .nodes(&self.tuple.elts)
            .finish()
    }
}

#[derive(Debug)]
struct AnonNamedTupleFields<'a> {
    tuple: &'a ExprTuple,
    /// Operator between field name and field expression: `":"` for the
    /// type form (`name: T`), `"="` for the value form (`name=v`).
    sep: &'static str,
}

impl<'a> AnonNamedTupleFields<'a> {
    const fn new(tuple: &'a ExprTuple, sep: &'static str) -> Self {
        Self { tuple, sep }
    }
}

impl Format<PyFormatContext<'_>> for AnonNamedTupleFields<'_> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        let mut joiner = f.join_comma_separated(self.tuple.end());
        let sep_token = self.sep;
        // Type form gets a trailing space after `:` (PEP 8); value form does
        // not insert space around `=` (Python keyword-argument convention).
        let needs_space_after = sep_token == ":";
        for elt in &self.tuple.elts {
            // Mixed positional + named: a bare expression is a positional
            // field and renders verbatim. An `Expr::Named` node carries the
            // field name and value with the form-appropriate separator.
            if let Expr::Named(named) = elt {
                joiner.entry(
                    elt,
                    &format_with(|f| {
                        if needs_space_after {
                            ruff_formatter::write!(
                                f,
                                [
                                    named.target.format(),
                                    token(sep_token),
                                    space(),
                                    named.value.format()
                                ]
                            )
                        } else {
                            ruff_formatter::write!(
                                f,
                                [
                                    named.target.format(),
                                    token(sep_token),
                                    named.value.format()
                                ]
                            )
                        }
                    }),
                );
            } else {
                joiner.entry(elt, &elt.format());
            }
        }
        joiner.finish()
    }
}

/// Formatter for parameter-shape tuples. Handles markers (`/`, `*`),
/// variadic (`*: T` / `*name: T`) encoded as `Expr::Starred` /
/// `Expr::Named(target=Starred(...), value=T)`, and kwargs catch-all
/// (`**: T` / `**name: T`) encoded as `Expr::Starred(Starred(...))` /
/// `Expr::Named(target=Starred(Starred(...)), value=T)`. Markers are
/// re-inserted at their `parameter_slash` / `parameter_star` indices
struct ParameterShapeFields<'a> {
    tuple: &'a ExprTuple,
}

impl<'a> ParameterShapeFields<'a> {
    const fn new(tuple: &'a ExprTuple) -> Self {
        Self { tuple }
    }
}

impl Format<PyFormatContext<'_>> for ParameterShapeFields<'_> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        let slash = self.tuple.parameter_slash().map(|i| i as usize);
        let star = self.tuple.parameter_star().map(|i| i as usize);
        let mut joiner = f.join_comma_separated(self.tuple.end());
        for (i, elt) in self.tuple.elts.iter().enumerate() {
            if Some(i) == slash {
                joiner.entry(elt, &format_with(|f| token("/").fmt(f)));
            }
            if Some(i) == star
                && !matches!(elt, Expr::Starred(_))
                && !matches!(elt, Expr::Named(n) if matches!(n.target.as_ref(), Expr::Starred(_)))
            {
                joiner.entry(elt, &format_with(|f| token("*").fmt(f)));
            }
            match elt {
                Expr::Named(named) => match named.target.as_ref() {
                    Expr::Starred(starred) => match starred.value.as_ref() {
                        // `**name: T`
                        Expr::Starred(inner_inner) => {
                            joiner.entry(
                                elt,
                                &format_with(|f| {
                                    token("**").fmt(f)?;
                                    inner_inner.value.format().fmt(f)?;
                                    token(":").fmt(f)?;
                                    space().fmt(f)?;
                                    named.value.format().fmt(f)
                                }),
                            );
                        }
                        // `*name: T`, and the anonymous `*: *Ts` — whose name is
                        // the empty marker, so there is nothing to write between
                        // the star and the colon
                        _ => {
                            let anonymous = matches!(
                                starred.value.as_ref(),
                                Expr::Name(name)
                                    if name.id.is_empty() && name.ctx.is_invalid()
                            );
                            joiner.entry(
                                elt,
                                &format_with(|f| {
                                    token("*").fmt(f)?;
                                    if !anonymous {
                                        starred.value.format().fmt(f)?;
                                    }
                                    token(":").fmt(f)?;
                                    space().fmt(f)?;
                                    named.value.format().fmt(f)
                                }),
                            );
                        }
                    },
                    // `name: T`
                    _ => {
                        joiner.entry(
                            elt,
                            &format_with(|f| {
                                named.target.format().fmt(f)?;
                                token(":").fmt(f)?;
                                space().fmt(f)?;
                                named.value.format().fmt(f)
                            }),
                        );
                    }
                },
                // an anonymous variadic whose annotation is the bare star
                // (`*: *`) encodes as `Starred(Starred(Name("")))` — the same
                // outer shape as the kwargs catch-all `**: T`. the empty
                // `Invalid`-context name is unique to the bare-star synthesis,
                // so test for it before reading the shape as a double star
                Expr::Starred(s) if is_top_star_marker(&s.value) => {
                    joiner.entry(
                        elt,
                        &format_with(|f| {
                            token("*").fmt(f)?;
                            token(":").fmt(f)?;
                            space().fmt(f)?;
                            token("*").fmt(f)
                        }),
                    );
                }
                Expr::Starred(s) => match s.value.as_ref() {
                    // `**: T`
                    Expr::Starred(inner) => {
                        joiner.entry(
                            elt,
                            &format_with(|f| {
                                token("**").fmt(f)?;
                                token(":").fmt(f)?;
                                space().fmt(f)?;
                                inner.value.format().fmt(f)
                            }),
                        );
                    }
                    // `*: T`
                    _ => {
                        joiner.entry(
                            elt,
                            &format_with(|f| {
                                token("*").fmt(f)?;
                                token(":").fmt(f)?;
                                space().fmt(f)?;
                                s.value.format().fmt(f)
                            }),
                        );
                    }
                },
                _ => {
                    joiner.entry(elt, &elt.format());
                }
            }
        }
        // markers can also appear at the very end (after last elt)
        let after_last = self.tuple.elts.len();
        if Some(after_last) == slash {
            joiner.entry(self.tuple, &format_with(|f| token("/").fmt(f)));
        }
        if Some(after_last) == star {
            joiner.entry(self.tuple, &format_with(|f| token("*").fmt(f)));
        }
        joiner.finish()
    }
}

impl NeedsParentheses for ExprTuple {
    fn needs_parentheses(
        &self,
        _parent: AnyNodeRef,
        _context: &PyFormatContext,
    ) -> OptionalParentheses {
        OptionalParentheses::Never
    }
}
