use crate::prelude::*;
use ruff_python_ast::Parameter;
use ruff_python_ast::helpers::parameter_modifiers;
use ruff_text_size::{TextRange, TextSize};

#[derive(Default)]
pub struct FormatParameter;

impl FormatNodeRule<Parameter> for FormatParameter {
    fn fmt_fields(&self, item: &Parameter, f: &mut PyFormatter) -> FormatResult<()> {
        let Parameter {
            range: _,
            node_index: _,
            name,
            annotation,
            is_context,
        } = item;

        if *is_context {
            token("context").fmt(f)?;
            space().fmt(f)?;
        }

        // basedpython `local` / `once` lifetime modifiers carry no AST field —
        // they live in the source span between the parameter's start and its
        // name — so reconstructing the parameter from the AST alone would
        // silently drop them. re-emit them in source order
        for strip_range in &parameter_modifiers(f.context().source(), item).strip_ranges {
            // a strip range spans the keyword plus the whitespace up to the next
            // token; emit the keyword alone and normalise the separator
            let keyword_len = TextSize::of(f.context().source()[*strip_range].trim_end());
            source_text_slice(TextRange::at(strip_range.start(), keyword_len)).fmt(f)?;
            space().fmt(f)?;
        }

        name.format().fmt(f)?;

        if let Some(annotation) = annotation.as_deref() {
            token(":").fmt(f)?;

            if f.context().comments().has_leading(annotation)
                && !f.context().is_expression_parenthesized(annotation.into())
            {
                hard_line_break().fmt(f)?;
            } else {
                space().fmt(f)?;
            }

            annotation.format().fmt(f)?;
        }

        Ok(())
    }
}
