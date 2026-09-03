use ruff_formatter::write;
use ruff_python_ast::{Expr, StmtAnnAssign};
use ruff_text_size::Ranged;

use crate::expression::is_splittable_expression;
use crate::expression::parentheses::{NeedsParentheses, OptionalParentheses, Parentheses};
use crate::prelude::*;
use crate::statement::assignment_alignment::AssignmentPadding;
use crate::statement::stmt_assign::{
    AnyAssignmentOperator, AnyBeforeOperator, FormatStatementsLastExpression,
};
use crate::statement::stmt_class_def::FormatDecorators;
use crate::statement::trailing_semicolon;

#[derive(Default)]
pub struct FormatStmtAnnAssign;

/// ` = <value>`, the tail the basedpython surface forms share, padded so that the
/// `=` lines up with the surrounding assignments
struct AssignedValue<'a> {
    value: &'a Expr,
    padding: AssignmentPadding,
}

fn assigned_value(value: &Expr, padding: AssignmentPadding) -> AssignedValue<'_> {
    AssignedValue { value, padding }
}

impl Format<PyFormatContext<'_>> for AssignedValue<'_> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        write!(
            f,
            [
                space(),
                AnyAssignmentOperator::assign(self.padding),
                space(),
                self.value.format()
            ]
        )
    }
}

/// detect a synthetic basedpython `let` annotation: returns `None` for bare `__let__`,
/// or `Some(type_expr)` for the typed form `__let__[T]`
#[allow(clippy::option_option)]
fn synthetic_let(ann: &Expr) -> Option<Option<&Expr>> {
    match ann {
        Expr::Name(n) if n.id.as_str() == "__let__" => Some(None),
        Expr::Subscript(s) if matches!(s.value.as_ref(), Expr::Name(n) if n.id.as_str() == "__let__") => {
            Some(Some(s.slice.as_ref()))
        }
        _ => None,
    }
}

/// detect a synthetic basedpython `final` annotation (`__final__[T]`), produced
/// by the parser for `final x: T` and modifier chains like `final override x: T`
fn synthetic_final(ann: &Expr) -> Option<&Expr> {
    let Expr::Subscript(s) = ann else {
        return None;
    };
    if !matches!(s.value.as_ref(), Expr::Name(n) if n.id.as_str() == "__final__") {
        return None;
    }
    Some(s.slice.as_ref())
}

/// the keyword prefix a declaration was written with, read straight from the source
/// between the declaration's keywords and the name it binds.
///
/// Every declaration form the parser rewrites into a marker keeps its keywords there —
/// `let`, `final override`, `context let` — and there is no other record of the order they
/// were written in, so re-emitting this text verbatim is what makes the form round-trip
fn declaration_prefix<'src>(
    item: &StmtAnnAssign,
    target: &Expr,
    src: &'src str,
) -> Option<&'src str> {
    // a decorated binding's statement begins at its first decorator, and the decorators
    // are written separately above — the keywords are only what follows the last of them
    let start = item
        .decorator_list
        .last()
        .map_or(item.range().start(), Ranged::end);
    let start = u32::from(start) as usize;
    let end = u32::from(target.range().start()) as usize;
    let prefix = src.get(start..end)?;
    // a comment may sit between the decorators and the keywords, and the formatter's own
    // comment handling writes it, so the prefix starts after the last one
    let prefix = prefix
        .rfind('#')
        .and_then(|hash| prefix[hash..].find('\n').map(|end| &prefix[hash + end..]))
        .unwrap_or(prefix)
        .trim();
    (!prefix.is_empty()).then_some(prefix)
}

/// detect a synthetic basedpython annotation marker name (classvar / newtype / sentinel)
fn synthetic_marker(ann: &Expr) -> Option<&'static str> {
    if let Expr::Name(n) = ann {
        match n.id.as_str() {
            "__classvar__" => return Some("class"),
            "__newtype__" => return Some("newtype"),
            "__sentinel__" => return Some("sentinel"),
            _ => {}
        }
    }
    None
}

/// detect a synthetic basedpython no-op modifier annotation — produced by the
/// parser for `abstract a: T`, `private a: T`, `public a: T`, `export a: T`, and
/// any other modifier chain that carries no meaning to ty (`override a: T`).
/// Mirrors [`synthetic_final`]: the annotation is `Subscript(Name(<marker>), T)`,
/// the surface keyword text lives at the marker's range, and the type is the slice
fn synthetic_modifier_annot<'ast, 'src>(
    ann: &'ast Expr,
    src: &'src str,
) -> Option<(&'src str, &'ast Expr)> {
    let Expr::Subscript(s) = ann else {
        return None;
    };
    let Expr::Name(name) = s.value.as_ref() else {
        return None;
    };
    if !matches!(
        name.id.as_str(),
        "__abstract_annot__"
            | "__visibility_annot__"
            | "__private_annot__"
            | "__modifier_annot__"
            | "__classvar_annot__"
    ) {
        return None;
    }
    let start = u32::from(name.range.start()) as usize;
    let end = u32::from(name.range.end()) as usize;
    Some((src.get(start..end)?.trim(), s.slice.as_ref()))
}

/// detect the synthetic marker the parser emits for a declaration keyword on an
/// unannotated assignment — `var a = 1`, `override a = 1`, `final override a = 1`.
/// the annotation is a bare `Name("__modifier_assign__")` whose range spans the
/// surface keyword prefix, so the prefix is recovered from source like
/// [`synthetic_final`] does
fn synthetic_modifier_assign<'src>(ann: &Expr, src: &'src str) -> Option<&'src str> {
    let Expr::Name(name) = ann else {
        return None;
    };
    if name.id.as_str() != "__modifier_assign__" {
        return None;
    }
    let start = u32::from(name.range.start()) as usize;
    let end = u32::from(name.range.end()) as usize;
    Some(src.get(start..end)?.trim())
}

impl FormatNodeRule<StmtAnnAssign> for FormatStmtAnnAssign {
    fn fmt_fields(&self, item: &StmtAnnAssign, f: &mut PyFormatter) -> FormatResult<()> {
        let StmtAnnAssign {
            range: _,
            node_index: _,
            target,
            annotation,
            value,
            simple: _,
            decorator_list,
            // the `context` keyword sits in the source prefix that the marker paths below
            // re-emit verbatim, so nothing extra is written for it here
            is_context: _,
        } = item;

        // basedpython: a binding may carry decorators, which are written above it
        // exactly as they are on a `def`
        FormatDecorators {
            decorators: decorator_list,
            leading_definition_comments: &[],
        }
        .fmt(f)?;

        let padding = AssignmentPadding::of(item.start(), f.context());

        // basedpython synthetic annotations — format back to the surface syntax
        if let Some(type_ann) = synthetic_let(annotation) {
            // `[modifiers] let NAME [: T] [= v]` — the keyword prefix is rendered verbatim
            // from source, so a sibling modifier (`context let`, `private let`) survives
            let prefix = declaration_prefix(item, target, f.context().source()).unwrap_or("let");
            write!(f, [text(prefix), space(), target.format()])?;
            if let Some(t) = type_ann {
                write!(f, [token(":"), space(), t.format()])?;
            }
            if let Some(v) = value {
                assigned_value(v, padding).fmt(f)?;
            }
            return Ok(());
        }
        if f.options().is_basedpython()
            && let Some(type_ann) = synthetic_final(annotation)
            && let Some(prefix) = declaration_prefix(item, target, f.context().source())
        {
            // `[modifiers] final [modifiers] NAME: T [= v]` — the surface modifier prefix is
            // rendered verbatim from source (it may carry a stripped sibling
            // modifier, e.g. `final override`), then the recovered type
            write!(
                f,
                [
                    text(prefix),
                    space(),
                    target.format(),
                    token(":"),
                    space(),
                    type_ann.format()
                ]
            )?;
            if let Some(v) = value {
                assigned_value(v, padding).fmt(f)?;
            }
            return Ok(());
        }
        if f.options().is_basedpython()
            && let Some(prefix) = synthetic_modifier_assign(annotation, f.context().source())
        {
            // `<keyword chain> <target> = <value>` — the keyword prefix is
            // rendered verbatim from source; the statement carries no annotation
            write!(f, [text(prefix), space(), target.format()])?;
            if let Some(v) = value {
                assigned_value(v, padding).fmt(f)?;
            }
            return Ok(());
        }
        if let Some(keyword) = synthetic_marker(annotation) {
            write!(f, [text(keyword), space(), target.format()])?;
            if let Some(v) = value {
                assigned_value(v, padding).fmt(f)?;
            }
            return Ok(());
        }
        if f.options().is_basedpython()
            && let Some((modifier_kw, type_ann)) =
                synthetic_modifier_annot(annotation, f.context().source())
        {
            // `<modifier> <target>: <T> [= value]` — the surface modifier prefix is
            // rendered verbatim from source (it may be a chain, e.g. `private
            // override`), then the declared type
            write!(
                f,
                [
                    text(modifier_kw),
                    space(),
                    target.format(),
                    token(":"),
                    space(),
                    type_ann.format()
                ]
            )?;
            if let Some(v) = value {
                assigned_value(v, padding).fmt(f)?;
            }
            return Ok(());
        }

        let comments = f.context().comments().clone();
        let annotation_parentheses = annotation
            .as_ref()
            .needs_parentheses(item.into(), f.context());

        write!(f, [target.format(), token(":"), space()])?;

        if let Some(value) = value {
            if annotation_parentheses != OptionalParentheses::Always
                && is_splittable_expression(annotation, f.context())
            {
                FormatStatementsLastExpression::RightToLeft {
                    before_operator: AnyBeforeOperator::Expression(annotation),
                    operator: AnyAssignmentOperator::assign(padding),
                    value,
                    statement: item.into(),
                }
                .fmt(f)?;
            } else {
                // Remove unnecessary parentheses around the annotation if the parenthesize long type hints preview style is enabled.
                // Ensure we keep the parentheses if the annotation has any comments.
                let parentheses = if comments.has_leading(annotation.as_ref())
                    || comments.has_trailing(annotation.as_ref())
                    || annotation_parentheses == OptionalParentheses::Always
                {
                    Parentheses::Always
                } else {
                    Parentheses::Never
                };

                annotation.format().with_options(parentheses).fmt(f)?;

                write!(
                    f,
                    [
                        space(),
                        AnyAssignmentOperator::assign(padding),
                        space(),
                        FormatStatementsLastExpression::left_to_right(value, item)
                    ]
                )?;
            }
        } else if annotation_parentheses == OptionalParentheses::Always {
            annotation
                .format()
                .with_options(Parentheses::Always)
                .fmt(f)?;
        } else {
            // Parenthesize the value and inline the comment if it is a "simple" type annotation, similar
            // to what we do with the value.
            // ```python
            // class Test:
            //     safe_age: (
            //         Decimal  #  the user's age, used to determine if it's safe for them to use ruff
            //     )
            // ```
            FormatStatementsLastExpression::left_to_right(annotation, item).fmt(f)?;
        }

        if f.options().source_type().is_ipynb()
            && f.context().node_level().is_last_top_level_statement()
            && target.is_name_expr()
            && trailing_semicolon(item.into(), f.context().source()).is_some()
        {
            token(";").fmt(f)?;
        }

        Ok(())
    }
}
