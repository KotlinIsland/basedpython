//! Put a list's elements each on a line of its own, or all on one line: a call's arguments, a
//! definition's parameters, the elements of a display.
//!
//! The list is the innermost one with brackets of its own around the range that the rewrite
//! applies to, so a range inside a list that is already split splits nothing and is offered the
//! list around it instead. Only the text between the brackets changes, and only the text between
//! the elements: each element is kept as it is written.
//!
//! A split list ends with a comma, which a formatter reads as the list being meant to stay split;
//! a joined one loses it, except for a one-element tuple, which needs it to be a tuple.

use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{AnyNodeRef, StringFlags};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange};

use super::{Plan, RefactorContext, Refusal};
use crate::comma_list::{CommaList, ListKind, comma_list, tokens_within};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// Each element on a line of its own.
    Split,
    /// Every element on the line the list starts on.
    Join,
}

pub(super) fn plan(
    context: &RefactorContext<'_>,
    range: TextRange,
    direction: Direction,
) -> Result<Plan, Refusal> {
    let module = context.parsed.syntax();
    if !module.range().contains_range(range) {
        return Err(Refusal::NotApplicable);
    }
    let tokens = context.parsed.tokens();
    let covering = covering_node(module.into(), range);
    let ancestors: Vec<AnyNodeRef> = covering.ancestors().collect();

    for (index, node) in ancestors.iter().enumerate() {
        let Some(list) = comma_list(*node, tokens) else {
            continue;
        };
        let (Some((open, close)), Some(inside)) = (list.brackets, list.inside()) else {
            continue;
        };
        if !TextRange::new(open.start(), close.end()).contains_range(range) {
            continue;
        }
        let gaps = gaps(&list, inside);
        let broken = gaps
            .iter()
            .filter(|gap| context.is_multiline(**gap))
            .count();
        let applies = match direction {
            Direction::Split => list.elements.len() >= 2 && broken < gaps.len(),
            Direction::Join => !list.elements.is_empty() && broken > 0,
        };
        if !applies {
            continue;
        }

        let title = title(direction, list.kind);
        if tokens_within(tokens, inside)
            .iter()
            .any(|token| token.kind() == TokenKind::Comment)
        {
            return Err(Refusal::refused(
                title,
                "the list has comments in it, which would have to move with the text around them",
            ));
        }
        let in_single_line_string = ancestors[index + 1..]
            .iter()
            .any(|ancestor| match ancestor {
                AnyNodeRef::FString(string) => !string.flags.is_triple_quoted(),
                AnyNodeRef::TString(string) => !string.flags.is_triple_quoted(),
                _ => false,
            });
        if direction == Direction::Split && in_single_line_string {
            return Err(Refusal::refused(
                title,
                "the list is inside an interpolation of a string that is written on one line",
            ));
        }

        let source = context.source();
        let text = match direction {
            Direction::Split => {
                let line_ending = context.line_ending();
                // the indentation of the line the list opens on, which its closing bracket goes
                // back to
                let line = source.line_start(open.start());
                let line_text = &source[TextRange::new(line, source.line_end(line))];
                let base = &line_text[..line_text.len() - line_text.trim_start().len()];
                let indent = format!("{base}{}", context.indent_unit());
                let mut text = String::from(line_ending);
                for element in &list.elements {
                    text.push_str(&indent);
                    text.push_str(&reindented(context, *element, &indent));
                    text.push(',');
                    text.push_str(line_ending);
                }
                text.push_str(base);
                text
            }
            Direction::Join => {
                let mut text = list
                    .elements
                    .iter()
                    .map(|element| &source[*element])
                    .collect::<Vec<_>>()
                    .join(", ");
                let lone_tuple_element =
                    matches!(node, AnyNodeRef::ExprTuple(tuple) if tuple.elts.len() == 1);
                if lone_tuple_element {
                    text.push(',');
                }
                text
            }
        };
        return Ok(Plan {
            title: title.to_string(),
            edits: vec![Edit::range_replacement(text, inside)],
            name: None,
        });
    }
    Err(Refusal::NotApplicable)
}

/// The text of `element`, an element that is moving to a line of its own indented by `indent`,
/// with each of its lines after the first moved as far as the line it started on moves: what is
/// nested in it stays indented under it. A line that starts inside a string literal is the
/// string's, and is left as it is.
fn reindented(context: &RefactorContext<'_>, element: TextRange, indent: &str) -> String {
    let source = context.source();
    let text = &source[element];
    if !context.is_multiline(element) {
        return text.to_string();
    }
    let line = source.line_start(element.start());
    let line_text = &source[TextRange::new(line, source.line_end(line))];
    let old = line_text.len() - line_text.trim_start().len();
    let new = indent.len();

    let tokens = context.parsed.tokens();
    let mut out = String::with_capacity(text.len());
    let mut start = element.start();
    for piece in text.split_inclusive('\n') {
        let piece_start = start;
        start += ruff_text_size::TextSize::of(piece);
        let index = tokens.partition_point(|token| token.end() <= piece_start);
        let in_string = tokens.get(index).is_some_and(|token| {
            token.start() < piece_start
                && matches!(
                    token.kind(),
                    TokenKind::String | TokenKind::FStringMiddle | TokenKind::TStringMiddle
                )
        });
        if piece_start == element.start() || in_string || piece.trim().is_empty() {
            out.push_str(piece);
            continue;
        }
        let leading = piece.len() - piece.trim_start().len();
        let kept = if new >= old {
            format!("{}{}", &piece[..leading], " ".repeat(new - old))
        } else {
            piece[..leading.saturating_sub(old - new)].to_string()
        };
        out.push_str(&kept);
        out.push_str(&piece[leading..]);
    }
    out
}

/// The stretches of `list` between the brackets that are not its elements: before the first,
/// between each two, and after the last.
fn gaps(list: &CommaList, inside: TextRange) -> Vec<TextRange> {
    let mut gaps = Vec::with_capacity(list.elements.len() + 1);
    let mut start = inside.start();
    for element in &list.elements {
        gaps.push(TextRange::new(start, element.start()));
        start = element.end();
    }
    gaps.push(TextRange::new(start, inside.end()));
    gaps
}

fn title(direction: Direction, kind: ListKind) -> &'static str {
    match (direction, kind) {
        (Direction::Split, ListKind::Arguments) => "Put arguments on separate lines",
        (Direction::Split, ListKind::Parameters) => "Put parameters on separate lines",
        (Direction::Split, ListKind::TypeParameters) => "Put type parameters on separate lines",
        (Direction::Split, _) => "Put elements on separate lines",
        (Direction::Join, ListKind::Arguments) => "Put arguments on one line",
        (Direction::Join, ListKind::Parameters) => "Put parameters on one line",
        (Direction::Join, ListKind::TypeParameters) => "Put type parameters on one line",
        (Direction::Join, _) => "Put elements on one line",
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    fn split(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::SplitList)
    }

    fn join(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::JoinList)
    }

    #[test]
    fn splits_a_call_s_arguments_one_per_line_with_a_trailing_comma() {
        assert_snapshot!(split(
            "
            def g():
                f(a, <CURSOR>b=1, *c, **d)
            ",
        ), @"
        Put arguments on separate lines
        ---

        def g():
            f(
                a,
                b=1,
                *c,
                **d,
            )
        ");
    }

    #[test]
    fn joins_a_split_call_and_drops_the_trailing_comma() {
        assert_snapshot!(join(
            "
            f(
                a,<CURSOR>
                b,
            )
            ",
        ), @"
        Put arguments on one line
        ---

        f(a, b)
        ");
    }

    /// A lambda's commas are its own: the call has two arguments, not three.
    #[test]
    fn an_element_s_own_commas_do_not_split_it() {
        assert_snapshot!(split(
            "
            f(<CURSOR>lambda a, b: a, (c, d))
            ",
        ), @"
        Put arguments on separate lines
        ---

        f(
            lambda a, b: a,
            (c, d),
        )
        ");
    }

    /// The `/` and bare `*` separators are parameters of their own as far as the layout goes.
    #[test]
    fn splits_parameters_with_their_separators() {
        assert_snapshot!(split(
            "
            def f(a, /, b, *, <CURSOR>c=1, **d): ...
            ",
        ), @"
        Put parameters on separate lines
        ---

        def f(
            a,
            /,
            b,
            *,
            c=1,
            **d,
        ): ...
        ");
    }

    #[test]
    fn a_dict_s_entries_and_unpackings_are_its_elements() {
        assert_snapshot!(split(
            "
            x = {<CURSOR>'a': 1, **b}
            ",
        ), @"
        Put elements on separate lines
        ---

        x = {
            'a': 1,
            **b,
        }
        ");
    }

    /// A tuple of one element needs its comma to stay a tuple.
    #[test]
    fn joining_a_one_element_tuple_keeps_its_comma() {
        assert_snapshot!(join(
            "
            x = (
                <CURSOR>a,
            )
            ",
        ), @"
        Put elements on one line
        ---

        x = (a,)
        ");
    }

    /// Inside a list that is split already there is nothing to split, so the list around it is
    /// the one offered.
    #[test]
    fn a_list_that_is_split_already_offers_the_list_around_it() {
        assert_snapshot!(split(
            "
            f(x, [
                <CURSOR>1,
                2,
            ])
            ",
        ), @"
        Put arguments on separate lines
        ---

        f(
            x,
            [
                1,
                2,
            ],
        )
        ");
    }

    #[test]
    fn a_list_on_one_line_is_not_joined() {
        assert_snapshot!(join("f(a, <CURSOR>b)"), @"not offered");
    }

    #[test]
    fn a_single_element_is_not_split() {
        assert_snapshot!(split("f(<CURSOR>a)"), @"not offered");
    }

    /// Joining would put what follows a comment inside it.
    #[test]
    fn a_list_with_a_comment_is_refused() {
        assert_snapshot!(join(
            "
            f(
                a,  # the first
                <CURSOR>b,
            )
            ",
        ), @"refused: Put arguments on one line (the list has comments in it, which would have to move with the text around them)");
    }

    /// A line break inside an interpolation of a string written on one line is only valid from
    /// Python 3.12.
    #[test]
    fn a_list_in_a_one_line_f_string_is_not_split() {
        assert_snapshot!(split(
            r#"
            x = f"{g(a, <CURSOR>b)}"
            "#,
        ), @"refused: Put arguments on separate lines (the list is inside an interpolation of a string that is written on one line)");
    }

    #[test]
    fn the_caret_on_the_callee_is_not_in_the_list() {
        assert_snapshot!(split("<CURSOR>f(a, b)"), @"not offered");
    }
}
