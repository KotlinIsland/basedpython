//! Lowering for context-sensitive resolution (`a: Color = Red`).
//!
//! A name that ty resolved against its expected type rather than through an
//! ordinary lookup has no runtime binding, so it is qualified with the enum it
//! was found on: `Red` → `Color.Red`. ty answers which names those are and what
//! the qualifier is (see [`context_sensitive`]), so the checker and this pass
//! never disagree about what a bare name means.
//!
//! Unlike every other type-aware pass, this one runs *before* the enum lowering
//! rewrites `enum class` to python. That is what keeps the two halves on one
//! answer: the source ty resolves here is the same source it checks, and the
//! qualified form the pass emits is exactly how the enum lowering spells its
//! members at runtime.
//!
//! [`context_sensitive`]: ty_python_semantic::types::context_sensitive

use std::borrow::Cow;

use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::TypeInfo;

struct Qualify<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for Qualify<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_load()
            && let Some(qualifier) = self.types.context_sensitive_qualifier(name)
        {
            self.edits
                .push((name.range(), format!("{qualifier}.{}", name.id)));
        }
        walk_expr(self, expr);
    }
}

/// Qualify every context-sensitively resolved name in `source`. Returns a
/// borrowed `Cow` when there are none — the overwhelmingly common case, and the
/// signal to the caller that its own view of the source is still current.
///
/// `suite` must be the parse the `types` model was built from: the resolution is
/// looked up by AST node identity.
pub(crate) fn qualify<'a>(source: &'a str, suite: &[Stmt], types: &dyn TypeInfo) -> Cow<'a, str> {
    let mut visitor = Qualify {
        types,
        edits: Vec::new(),
    };
    for stmt in suite {
        visitor.visit_stmt(stmt);
    }
    if visitor.edits.is_empty() {
        return Cow::Borrowed(source);
    }

    // one edit per name node, so the ranges are disjoint; sorting is all that is
    // needed to splice them in a single forward pass
    visitor.edits.sort_by_key(|(range, _)| range.start());
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (range, replacement) in visitor.edits {
        output.push_str(&source[cursor..range.start().into()]);
        output.push_str(&replacement);
        cursor = range.end().into();
    }
    output.push_str(&source[cursor..]);
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use crate::{Config, make_in_memory_db, transpile, transpile_typed_with_map};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    /// The `.by` line an output line came from, by content match. `None` when the
    /// map has lost the correspondence — which is the failure this guards.
    fn mapped_by_line(source: &str, needle: &str) -> Option<u32> {
        let (db, file) = make_in_memory_db(source);
        let (output, line_map) =
            transpile_typed_with_map(&db, file, &Config::test_default()).unwrap();
        let index = output
            .lines()
            .position(|line| line.trim() == needle)
            .expect("needle not found in transpiled output");
        line_map.get(index).copied().flatten()
    }

    #[test]
    fn qualifying_keeps_the_line_map() {
        // qualification edits *within* a line, so every output line still maps
        // back to its `.by` line. it must not be mistaken for the enum phase,
        // whose line map is empty when it did not fire — composing through that
        // one maps every line to nothing, and `by run` loses the whole
        // traceback rewrite for the file
        let source = indoc! {"
            from enum import Enum

            class Color(Enum):
                RED = 1

            a: Color = RED
            print(a)
        "};
        assert_eq!(mapped_by_line(source, "print(a)"), Some(6));
    }

    #[test]
    fn qualifying_keeps_the_line_map_alongside_an_enum() {
        // the same, with the enum phase *also* firing: the map has to compose
        // through the enum lowering's renumbering rather than skip it
        let source = indoc! {"
            enum class Color:
                case Red, Green

            a: Color = Red
            print(a)
        "};
        assert_eq!(mapped_by_line(source, "print(a)"), Some(4));
    }

    #[test]
    fn unit_variant_is_qualified() {
        check(
            indoc! {"
                enum class Color:
                    case Red, Green

                a: Color = Red
            "},
            indoc! {"
                from __future__ import annotations
                from enum import Enum, auto
                class Color(Enum):
                    Red = auto()
                    Green = auto()

                a: Color = Color.Red
            "},
        );
    }

    #[test]
    fn arguments_and_returns_are_qualified() {
        check(
            indoc! {"
                enum class Color:
                    case Red, Green

                def paint(c: Color) -> None: ...

                paint(Red)

                def favourite() -> Color:
                    return Green
            "},
            indoc! {"
                from __future__ import annotations
                from enum import Enum, auto
                class Color(Enum):
                    Red = auto()
                    Green = auto()

                def paint(c: Color) -> None: ...

                paint(Color.Red)

                def favourite() -> Color:
                    return Color.Green
            "},
        );
    }

    #[test]
    fn payload_variant_constructor_is_qualified() {
        check(
            indoc! {"
                enum class Shape:
                    case Circle(radius: int)
                    case Empty

                s: Shape = Circle(2)
                e: Shape = Empty
            "},
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class Shape:
                    pass

                @final
                @dataclass(frozen=True, slots=True)
                class _Shape_Circle(Shape):
                    radius: int
                _Shape_Circle.__name__ = \"Circle\"
                _Shape_Circle.__qualname__ = \"Shape.Circle\"
                Shape.Circle = _Shape_Circle

                class _Shape_Empty(Shape):
                    __slots__ = ()
                    def __repr__(self): return \"Empty\"
                _Shape_Empty.__name__ = \"Empty\"
                _Shape_Empty.__qualname__ = \"Shape.Empty\"
                Shape.Empty = _Shape_Empty()

                s: Shape = Shape.Circle(2)
                e: Shape = Shape.Empty
            "},
        );
    }

    #[test]
    fn python_enum_member_is_qualified() {
        check(
            indoc! {"
                from enum import Enum

                class Color(Enum):
                    RED = 1
                    GREEN = 2

                a: Color = RED
            "},
            indoc! {"
                from enum import Enum

                class Color(Enum):
                    RED = 1
                    GREEN = 2

                a: Color = Color.RED
            "},
        );
    }

    #[test]
    fn an_ordinary_binding_is_left_alone() {
        // the name resolves without help, so it keeps its ordinary meaning
        check(
            indoc! {"
                from enum import Enum

                class Color(Enum):
                    RED = 1

                RED = Color.RED
                a: Color = RED
            "},
            indoc! {"
                from enum import Enum

                class Color(Enum):
                    RED = 1

                RED = Color.RED
                a: Color = RED
            "},
        );
    }

    #[test]
    fn an_unresolved_name_is_left_alone() {
        // nothing to qualify it with: the transpiler emits it verbatim and the
        // checker reports the unresolved reference
        check(
            indoc! {"
                from enum import Enum

                class Color(Enum):
                    RED = 1

                a: Color = BLUE
            "},
            indoc! {"
                from enum import Enum

                class Color(Enum):
                    RED = 1

                a: Color = BLUE
            "},
        );
    }
}
