use ruff_formatter::FormatResult;
use ruff_python_ast::TypeParams;
use ruff_text_size::{Ranged, TextRange};

use crate::builders::PyFormatterExtensions;
use crate::expression::parentheses::parenthesized;
use crate::prelude::*;

#[derive(Default)]
pub struct FormatTypeParams;

/// Formats a sequence of [`TypeParam`](ruff_python_ast::TypeParam) nodes.
impl FormatNodeRule<TypeParams> for FormatTypeParams {
    fn fmt_fields(&self, item: &TypeParams, f: &mut PyFormatter) -> FormatResult<()> {
        // A dangling comment indicates a comment on the same line as the opening bracket, e.g.:
        // ```python
        // type foo[  # This type parameter clause has a dangling comment.
        //     a,
        //     b,
        //     c,
        // ] = ...
        let comments = f.context().comments().clone();
        let dangling_comments = comments.dangling(item);

        let items = format_with(|f| {
            // basedpython: the `/` and bare `*` separators are not `TypeParam` nodes, so they are
            // emitted as bare labels at their declared positions, the way a value parameter list
            // interleaves them
            let separators = item.separators;
            let positional_only_count = separators.positional_only_count.map(|c| c as usize);
            let keyword_only_start = separators.keyword_only_start.map(|c| c as usize);

            // the separators at `index`, if any — a `/` written last (`[A, /]`) sits at
            // `index == len`, past every type parameter
            let separators_at = |index: usize| {
                let slash = (positional_only_count == Some(index))
                    .then_some(separators.slash_range)
                    .flatten()
                    .map(|range| SeparatorToken { range, text: "/" });
                let star = (keyword_only_start == Some(index))
                    .then_some(separators.star_range)
                    .flatten()
                    .map(|range| SeparatorToken { range, text: "*" });
                slash.into_iter().chain(star)
            };

            let mut joiner = f.join_comma_separated(item.end());
            for (index, type_param) in item.type_params.iter().enumerate() {
                // basedpython: a `some T` hole was never written in the list — the parameter it
                // came from re-emits it
                if type_param
                    .as_type_var()
                    .is_some_and(|type_var| type_var.is_some_hole)
                {
                    continue;
                }
                for sep in separators_at(index) {
                    joiner.entry(&sep, &sep);
                }
                joiner.entry(type_param, &type_param.format());
            }
            for sep in separators_at(item.type_params.len()) {
                joiner.entry(&sep, &sep);
            }
            joiner.finish()
        });

        parenthesized("[", &items, "]")
            .with_dangling_comments(dangling_comments)
            .fmt(f)
    }
}

/// basedpython: a bare `/` or `*` separator token positioned inside a type parameter list.
struct SeparatorToken {
    range: TextRange,
    text: &'static str,
}

impl Ranged for SeparatorToken {
    fn range(&self) -> TextRange {
        self.range
    }
}

impl Format<PyFormatContext<'_>> for SeparatorToken {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        token(self.text).fmt(f)
    }
}
