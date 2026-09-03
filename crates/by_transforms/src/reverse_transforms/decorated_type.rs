//! reverse of `crate::transforms::decorated_type`:
//!   `Annotated[T, meta]` → `@meta T`
//!
//! only fires in annotation positions when `Annotated` resolves to `typing`, and
//! only when every metadata element is something a decorator can be written as:
//! a name, an attribute path, a call on one, or a subscript of one. `Annotated`
//! takes arbitrary metadata, and a literal or an operator expression among it
//! (`Annotated[str, "language=javascript"]`) has no decorator spelling — those
//! subscripts are left as they are, which is still valid basedpython.
//!
//! a decorated *binding* ([`crate::transforms::decorated_binding`]) has no
//! reverse. `x = foo(1)` is what one lowers to, but it is also what an ordinary
//! assignment of a call looks like, and nothing in the output says which one was
//! written — so recovering the decorator would rewrite every call assignment in
//! the file.

use std::fmt::Write;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Operator, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct DecoratedTypeReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> DecoratedTypeReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// Whether the source already wrote a group around `range`.
    ///
    /// Both sides have to match: a `(` before an arm on its own is the group around
    /// the whole union it belongs to (`(A | B)`), which holds nothing apart from
    /// what is beside this arm.
    fn is_grouped(&self, range: TextRange) -> bool {
        let before = self.source[..usize::from(range.start())].trim_end();
        let after = self.source[usize::from(range.end())..].trim_start();
        before.ends_with('(') && after.starts_with(')')
    }

    fn is_annotated_name(&self, expr: &Expr) -> bool {
        trailing_name(expr) == Some("Annotated") && self.types.subscript_is_type_context(expr)
    }

    /// Rewrite every `Annotated` in a type position reachable from `expr`.
    ///
    /// The edits are narrow on purpose: the prefix `Annotated[` becomes the
    /// decorators and the trailing `, meta]` goes, leaving the decorated type's
    /// own source untouched. Replacing the whole annotation instead would copy
    /// that source verbatim and drop whatever another reverse pass rewrote inside
    /// it — a `typing.Any` that should have become `dynamic`, say
    ///
    /// `beside_an_operand` says the type sits next to something a decoration would
    /// otherwise read as part of itself — an arm of a `|` or `&`. The brackets that
    /// held `Annotated` apart from it are about to go, so a group has to take over.
    fn rewrite_within(&mut self, expr: &Expr, beside_an_operand: bool) {
        match expr {
            Expr::Subscript(s) if self.is_annotated_name(&s.value) => {
                let Expr::Tuple(slice) = s.slice.as_ref() else {
                    return;
                };
                let [inner, metadata @ ..] = slice.elts.as_slice() else {
                    return;
                };
                // only the first element is a type position; the rest is metadata.
                // the decoration will cover the whole of it, so nothing in there is
                // beside the decoration — it is inside it
                self.rewrite_within(inner, false);
                if metadata.is_empty() || !metadata.iter().all(is_decorator_shaped) {
                    return;
                }
                // the metadata reads in the order the decorators apply, which is
                // bottom-up — so the last element is the one written first
                let mut decorators = String::new();
                for meta in metadata.iter().rev() {
                    let _ = write!(decorators, "@{} ", self.src(meta.range()));
                }
                // a decoration runs to the end of the type it is written on, so the
                // decorated type itself never needs a group — but a decoration that
                // is one arm of a wider union does, or it would swallow the rest.
                // a group the source already wrote does that job, and adding another
                // around it would add one more on every round trip
                let (open, close) = if beside_an_operand && !self.is_grouped(s.range()) {
                    ("(", ")")
                } else {
                    ("", "")
                };
                // two separate fixes, not one fix of two edits: edits within a
                // single fix are applied back to back, which would copy the type
                // between them verbatim and skip anything another pass rewrote
                // there. Separately, a rewrite of the type is free to land in the
                // gap. They cannot half-apply, because only a pass that rewrote a
                // *prefix* of this subscript could take one and leave the other,
                // and every pass here rewrites whole type expressions
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("{open}{decorators}"),
                    TextRange::new(s.range().start(), inner.range().start()),
                )));
                let tail = TextRange::new(inner.range().end(), s.range().end());
                self.edits.push(Fix::safe_edit(if close.is_empty() {
                    Edit::range_deletion(tail)
                } else {
                    Edit::range_replacement(close.to_owned(), tail)
                }));
            }
            // each arm is reached on its own, and each is beside the others
            Expr::BinOp(b) if matches!(b.op, Operator::BitOr | Operator::BitAnd) => {
                self.rewrite_within(&b.left, true);
                self.rewrite_within(&b.right, true);
            }
            // brackets and commas end a type, so a decoration written inside them
            // stops there on its own
            Expr::Subscript(s) => self.rewrite_within(&s.slice, false),
            Expr::Tuple(t) => {
                for element in &t.elts {
                    self.rewrite_within(element, false);
                }
            }
            _ => {}
        }
    }
}

/// Whether `expr` is something a decorator can be written as — a name, an
/// attribute path, or a call or subscript on one. Anything else (a literal, an
/// operator expression, a lambda) is valid `Annotated` metadata with no decorator
/// spelling, and the subscript carrying it is left alone.
fn is_decorator_shaped(expr: &Expr) -> bool {
    match expr {
        Expr::Name(_) => true,
        Expr::Attribute(attribute) => is_decorator_shaped(&attribute.value),
        Expr::Call(call) => is_decorator_shaped(&call.func),
        Expr::Subscript(subscript) => is_decorator_shaped(&subscript.value),
        _ => false,
    }
}

impl<'ast> Visitor<'ast> for DecoratedTypeReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        crate::transforms::source_util::for_each_annotation_in_stmt(stmt, |ann| {
            self.rewrite_within(ann, false);
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn annotated_annotation() {
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: Annotated[int, meta]
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: @meta int
            "},
        );
    }

    #[test]
    fn nested_in_a_type_argument() {
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: list[Annotated[int, meta]]
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: list[@meta int]
            "},
        );
    }

    #[test]
    fn a_call_as_metadata() {
        check(
            indoc! {"
                from typing import Annotated
                def field(gt: int) -> int: ...
                x: Annotated[int, field(gt=0)]
            "},
            indoc! {"
                from typing import Annotated
                def field(gt: int) -> int
                x: @field(gt=0) int
            "},
        );
    }

    #[test]
    fn several_metadata_elements_become_a_chain() {
        // the metadata reads in the order the decorators apply, so the chain is
        // written back in the reverse of that
        check(
            indoc! {"
                from typing import Annotated
                x = 1
                y = 2
                a: Annotated[int, x, y]
            "},
            indoc! {"
                from typing import Annotated
                x = 1
                y = 2
                a: @y @x int
            "},
        );
    }

    #[test]
    fn the_decorated_union_needs_no_group() {
        // a decoration runs to the end of the type, so the union is already inside it
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: Annotated[int | str, meta]
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: @meta int | str
            "},
        );
    }

    #[test]
    fn a_group_the_source_already_wrote_is_dropped() {
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: Annotated[(int | str), meta]
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: @meta int | str
            "},
        );
    }

    #[test]
    fn a_union_with_none_is_the_decorated_union() {
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: Annotated[int | None, meta]
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: @meta int | None
            "},
        );
    }

    #[test]
    fn a_decoration_that_is_one_arm_of_a_union_is_grouped() {
        // without the group the decoration would run on and take `str` with it,
        // which is the other type entirely
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: Annotated[int, meta] | str
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: (@meta int) | str
            "},
        );
    }

    #[test]
    fn a_group_the_source_already_wrote_around_an_arm_is_not_doubled() {
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: (Annotated[int, meta]) | str
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: (@meta int) | str
            "},
        );
    }

    #[test]
    fn an_arm_that_is_optional_is_grouped_too() {
        check(
            indoc! {"
                from typing import Annotated
                meta = 1
                x: Annotated[int, meta] | None
            "},
            indoc! {"
                from typing import Annotated
                meta = 1
                x: (@meta int) | None
            "},
        );
    }

    #[test]
    fn metadata_with_no_decorator_spelling_is_left_alone() {
        check(
            indoc! {r#"
                from typing import Annotated
                x: Annotated[str, "language=javascript"]
            "#},
            indoc! {r#"
                from typing import Annotated
                x: Annotated[str, "language=javascript"]
            "#},
        );
    }

    #[test]
    fn a_lone_annotated_is_left_alone() {
        check(
            indoc! {"
                from typing import Annotated
                x: Annotated[int]
            "},
            indoc! {"
                from typing import Annotated
                x: Annotated[int]
            "},
        );
    }
}
