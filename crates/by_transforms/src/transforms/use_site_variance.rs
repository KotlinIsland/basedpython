//! Pre-source text-edit that blanks basedpython's keyword-prefix type markers
//! out of the source string before the main lowering pipeline begins: the
//! use-site variance markers (`out T`, `in T`, `in out T`) and the use-site
//! type modifiers (`literal T`, `final T`). Both are written as a bare keyword
//! in front of a type expression, both are compile-time-only, and both are
//! erased by exactly the same mechanism, so one pass handles them.
//!
//! Unlike the other passes in `ast_pass`, this one does NOT mutate the AST
//! and re-render through [`Generator`]; it scans the AST for markers,
//! gathers their source ranges, and overwrites them in the source
//! string directly. The result is a basedpython source file with no
//! marker keywords — downstream transforms (callable arrow lowering,
//! intersection lowering) can then copy operand source verbatim without
//! capturing keywords that would later leak, and AST passes can
//! re-render a statement without the [`Generator`] meeting a marker node it
//! has no spelling for
//!
//! The keyword bytes are replaced with spaces rather than deleted, which is
//! what lets the *type checker* keep reading the markers: blanking preserves
//! every byte position, so the db can hold the original source — markers
//! intact — while every pass reads and splices the blanked copy at exactly
//! the same ranges. A pass that asks ty about `x is A[out int]` therefore
//! sees the projection, and still renders `A[int]`. Deleting the bytes
//! instead would shift every following position out of alignment and hide
//! the projection from ty entirely
//!
//! The padding this leaves behind (`A[    int]`) is valid Python but ugly, so
//! the driver also emits each blanked range as a deletion edit. Those apply
//! wherever no wider edit claims the region, which is the common case; inside
//! a wider *plain* text edit the padding survives, harmlessly.

use ruff_python_ast::PySourceType;
use ruff_python_ast::helpers::{type_modifier_marker, use_site_variance_marker};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextRange};

/// A source with every use-site variance marker blanked out, plus the ranges
/// that were blanked.
pub(crate) struct Blanked<'a> {
    pub(crate) source: std::borrow::Cow<'a, str>,
    /// the blanked ranges, ascending. Positions are valid in both the
    /// original and the blanked source — blanking is length-preserving
    pub(crate) ranges: Vec<TextRange>,
}

/// What a blanked range should collapse to: nothing, except any newlines it
/// spanned. Keeping those means the marker never costs a line, so output line
/// numbers still match the input's — which the driver's identity line table
/// relies on. A marker written on one line, the overwhelmingly common case,
/// collapses to the empty string
pub(crate) fn collapsed_to(source: &str, range: TextRange) -> String {
    source[usize::from(range.start())..usize::from(range.end())]
        .matches('\n')
        .collect()
}

impl Blanked<'_> {
    /// `original` with the marker keywords collapsed away, rather than blanked
    /// to spaces. This is what the driver's edits produce for an unclaimed
    /// range; the early-return paths, where no pass ran to emit them, use it to
    /// get the same output directly
    pub(crate) fn stripped<'s>(&self, original: &'s str) -> std::borrow::Cow<'s, str> {
        if self.ranges.is_empty() {
            return std::borrow::Cow::Borrowed(original);
        }
        let mut out = original.to_owned();
        for range in self.ranges.iter().rev() {
            out.replace_range(
                usize::from(range.start())..usize::from(range.end()),
                &collapsed_to(original, *range),
            );
        }
        std::borrow::Cow::Owned(out)
    }
}

/// Blank every keyword-prefix type marker — the use-site variance keywords
/// (`out T`, `in T`, `in out T`) and the use-site type modifiers (`literal T`,
/// `final T`) — out of `source`, replacing the keyword bytes with spaces so
/// byte positions are preserved. Newlines are kept as-is, so line structure survives a marker
/// that spans a line break. If parsing fails or no markers are present,
/// returns `source` unchanged with no ranges.
pub(crate) fn blank(source: &str) -> Blanked<'_> {
    let unchanged = || Blanked {
        source: std::borrow::Cow::Borrowed(source),
        ranges: Vec::new(),
    };
    let parsed = parse_unchecked_source(source, PySourceType::BasedPython);
    if !parsed.errors().is_empty() {
        return unchanged();
    }
    let module = parsed.into_syntax();

    let mut collector = MarkerCollector { ranges: Vec::new() };
    for stmt in &module.body {
        collector.visit_stmt(stmt);
    }
    if collector.ranges.is_empty() {
        return unchanged();
    }
    let mut ranges = collector.ranges;
    ranges.sort_by_key(Ranged::start);

    let mut out = source.to_owned();
    for range in &ranges {
        let span = &source[usize::from(range.start())..usize::from(range.end())];
        // keep newlines so the blanked copy has the same line structure; every
        // other byte becomes a space. mapping *bytes* (not chars) is what
        // makes this length-preserving even if the span holds a comment with
        // multi-byte text, and the all-ASCII result is still valid UTF-8
        let blanked: String = span
            .bytes()
            .map(|b| if b == b'\n' { '\n' } else { ' ' })
            .collect();
        out.replace_range(
            usize::from(range.start())..usize::from(range.end()),
            &blanked,
        );
    }
    Blanked {
        source: std::borrow::Cow::Owned(out),
        ranges,
    }
}

struct MarkerCollector {
    ranges: Vec<TextRange>,
}

impl<'ast> Visitor<'ast> for MarkerCollector {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // both marker shapes span from the keyword to the end of the type they
        // precede, so the keyword bytes are exactly what lies before the inner
        // expression's start
        if let Some(inner) = use_site_variance_marker(expr)
            .map(|(_, inner)| inner)
            .or_else(|| type_modifier_marker(expr).map(|(_, inner)| inner))
        {
            let start = expr.range().start();
            let inner_start = inner.range().start();
            if start < inner_start {
                self.ranges.push(TextRange::new(start, inner_start));
            }
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    #[test]
    fn def_site_covariant_stripped() {
        check(
            "class Box[out T]: ...\n",
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", covariant=True)
                class Box(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn def_site_contravariant_stripped() {
        check(
            "class Sink[in T]: ...\n",
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", contravariant=True)
                class Sink(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn def_site_bivariant_stripped() {
        check(
            "class Box[in out T]: ...\n",
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Box(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn use_site_out_strips_keyword() {
        check(
            "def f(data: list[out int]) -> int: ...\n",
            "def f(data: list[int]) -> int: ...\n",
        );
    }

    #[test]
    fn use_site_in_strips_keyword() {
        check(
            "def f(data: list[in int]) -> None: ...\n",
            "def f(data: list[int]) -> None: ...\n",
        );
    }

    #[test]
    fn use_site_in_out_strips_keyword() {
        check(
            "def f(data: list[in out int]) -> int: ...\n",
            "def f(data: list[int]) -> int: ...\n",
        );
    }

    #[test]
    fn use_site_does_not_fire_on_plain_subscript() {
        unchanged("a: list[int]\n");
    }

    #[test]
    fn use_site_does_not_fire_on_bare_out_identifier() {
        unchanged("x = out\n");
    }

    #[test]
    fn use_site_does_not_fire_on_arithmetic_continuation() {
        unchanged("y = a[out + 1]\n");
    }

    #[test]
    fn use_site_complex_inner_strips_keyword() {
        check("x: list[out int | str]\n", "x: list[int | str]\n");
    }

    #[test]
    fn use_site_multi_arg_strips_each_marked_element() {
        check(
            "def f(data: dict[str, out int]) -> int: ...\n",
            "def f(data: dict[str, int]) -> int: ...\n",
        );
    }

    #[test]
    fn use_site_nested_inside_other_subscript() {
        check("x: tuple[list[out int]]\n", "x: tuple[list[int]]\n");
    }

    #[test]
    fn blanks_use_site_variance() {
        let out = blank("def f(x: list[out int]) -> None: ...\n");
        assert_eq!(out.source, "def f(x: list[    int]) -> None: ...\n");
    }

    #[test]
    fn blanks_inside_callable_arrow() {
        let out = blank("fn: (list[out int]) -> None\n");
        assert_eq!(out.source, "fn: (list[    int]) -> None\n");
    }

    #[test]
    fn blanks_inside_intersection() {
        let out = blank("def h(x: list[out int] & list[out str]) -> None: pass\n");
        assert_eq!(
            out.source,
            "def h(x: list[    int] & list[    str]) -> None: pass\n"
        );
    }

    /// blanking must not move a single byte: the driver relies on this to hold
    /// the original source (markers intact) in the db while every pass reads
    /// and splices the blanked copy at the same ranges
    #[test]
    fn blanking_preserves_every_byte_position() {
        let src = "def f(x: dict[str, out int], y: list[in out str]) -> None: ...\n";
        let out = blank(src);
        assert_eq!(out.source.len(), src.len());
        for range in &out.ranges {
            assert!(
                out.source[usize::from(range.start())..usize::from(range.end())]
                    .bytes()
                    .all(|b| b == b' '),
                "blanked range should be all spaces"
            );
        }
    }

    /// a marker spanning a line break keeps its newline, so line numbers in
    /// the blanked copy still match the original
    #[test]
    fn blanking_preserves_line_structure() {
        let src = "x: list[out\n    int]\n";
        let out = blank(src);
        assert_eq!(out.source, "x: list[   \n    int]\n");
        assert_eq!(
            out.source.lines().count(),
            src.lines().count(),
            "line count must survive"
        );
    }

    #[test]
    fn stripped_collapses_what_blank_padded() {
        let src = "def f(x: list[out int]) -> None: ...\n";
        assert_eq!(
            blank(src).stripped(src),
            "def f(x: list[int]) -> None: ...\n"
        );
    }

    /// a marker spanning a line break keeps its newline through the collapse
    /// too, so output line numbers still match the input's — the driver's
    /// identity line table depends on it. the run of indentation up to the
    /// inner expression is part of the marker's range and goes with it
    #[test]
    fn collapsing_a_multiline_marker_costs_no_line() {
        let src = "x: list[out\n    int]\ny = 1\n";
        let stripped = blank(src).stripped(src);
        assert_eq!(stripped, "x: list[\nint]\ny = 1\n");
        assert_eq!(stripped.lines().count(), src.lines().count());
    }

    /// the whole point of the multi-line case: it still transpiles, and `y`
    /// stays on the line it started on
    #[test]
    fn multiline_marker_transpiles_without_shifting_lines() {
        check("x: list[out\n    int]\ny = 1\n", "x: list[\nint]\ny = 1\n");
    }

    #[test]
    fn no_markers_borrows_input() {
        let src = "def f(x: list[int]) -> None: ...\n";
        let out = blank(src);
        assert!(matches!(out.source, std::borrow::Cow::Borrowed(_)));
        assert!(out.ranges.is_empty());
        assert_eq!(out.source, src);
    }

    // the `literal T` / `final T` use-site type modifiers ride the same
    // blanking pass: both are compile-time-only and erase to the type they
    // precede

    #[test]
    fn type_modifiers_are_erased() {
        check("a: literal str = \"x\"\n", "a: str = \"x\"\n");
        check("b: final int = 1\n", "b: int = 1\n");
    }

    #[test]
    fn type_modifiers_erased_in_nested_positions() {
        check("a: list[literal str] = []\n", "a: list[str] = []\n");
        check("b: literal str | None = None\n", "b: str | None = None\n");
        check(
            "def f(x: literal int, y: final str) -> final bool: ...\n",
            "def f(x: int, y: str) -> bool: ...\n",
        );
    }

    #[test]
    fn type_modifier_keywords_stay_identifiers() {
        // only a modifier when a name follows it — everything else is an
        // ordinary reference and must survive untouched
        unchanged("a: literal\n");
        unchanged("b: final[int]\n");
        unchanged("literal = 1\n");
    }

    #[test]
    fn stripped_collapses_type_modifier_padding() {
        let src = "a: literal str\n";
        assert_eq!(blank(src).stripped(src), "a: str\n");
    }
}
