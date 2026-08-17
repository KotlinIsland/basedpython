//! reverse of `crate::transforms::annotation`:
//!   `tuple[int, str]` → `(int, str)` in annotation positions
//!   `tuple[int]`      → `(int,)`
//!
//! only fires on the builtin `tuple` subscript in annotation positions.
//!
//! the rewrite is emitted as two edits over the brackets — `tuple[` becomes `(`
//! and `]` becomes `)` — rather than one replacement rendered over the whole
//! expression. the element text is then never re-rendered, so another
//! transform's edit inside it survives (a whole-expression replacement built
//! from raw source would silently undo it) and the source's line breaks are
//! kept

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct TupleTypeReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> TupleTypeReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    /// offset just past the `[` opening a subscript's slice
    fn open_bracket_end(&self, value_end: TextSize) -> TextSize {
        let from = usize::from(value_end);
        let offset = self.source[from..].find('[').map_or(0, |i| i + 1);
        value_end + TextSize::try_from(offset).unwrap_or_default()
    }

    /// whether a `,` already sits between `after` and the subscript's `]`
    fn has_trailing_comma(&self, after: TextSize, close: TextSize) -> bool {
        self.source[usize::from(after)..usize::from(close)]
            .trim_end()
            .ends_with(',')
    }

    fn replace(&mut self, range: TextRange, text: &str) {
        let edit = if text.is_empty() {
            Edit::range_deletion(range)
        } else {
            Edit::range_replacement(text.to_owned(), range)
        };
        self.edits.push(Fix::safe_edit(edit));
    }

    fn is_tuple_name(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Name(_) => {
                trailing_name(expr) == Some("tuple") && self.types.subscript_is_type_context(expr)
            }
            _ => false,
        }
    }

    fn is_type_context_subscript(&self, value: &Expr) -> bool {
        trailing_name(value).is_some() && self.types.subscript_is_type_context(value)
    }

    /// `*tuple[T, ...]` as a tuple element round-trips to the basedpython
    /// variadic spelling `*: T`. returns the inner element type when `elt` has
    /// that shape
    fn variadic_element<'a>(&self, elt: &'a Expr) -> Option<&'a Expr> {
        let Expr::Starred(starred) = elt else {
            return None;
        };
        let Expr::Subscript(sub) = starred.value.as_ref() else {
            return None;
        };
        if !self.is_tuple_name(&sub.value) {
            return None;
        }
        homogeneous_element(&sub.slice)
    }

    /// the operand of an unpack (`*T` or `Unpack[T]`).
    ///
    /// a homogeneous `tuple[T, ...]` is left subscripted here: `*tuple[T, ...]`
    /// already spells exactly that type, and `*(*: T)` only says it twice
    fn visit_unpack_operand(&mut self, operand: &Expr) {
        if let Expr::Subscript(s) = operand
            && self.is_tuple_name(&s.value)
            && let Some(element) = homogeneous_element(&s.slice)
        {
            self.visit_type_expr(element);
            return;
        }
        self.visit_type_expr(operand);
    }

    /// walk a type expression, emitting bracket edits for every `tuple[...]`
    /// within it
    fn visit_type_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Subscript(s) if self.is_tuple_name(&s.value) => {
                // homogeneous variadic `tuple[T, ...]` → `(*: T)`
                if let Some(element) = homogeneous_element(&s.slice) {
                    self.replace(TextRange::new(s.start(), element.start()), "(*: ");
                    self.replace(TextRange::new(element.end(), s.end()), ")");
                    self.visit_type_expr(element);
                    return;
                }
                let Some(elements) = tuple_elements(&s.slice) else {
                    return;
                };
                let (Some(first), Some(last)) = (elements.first(), elements.last()) else {
                    return;
                };
                // only the `tuple[` and the closing `]` are rewritten, so a
                // newline after the bracket and a trailing comma both survive —
                // a multi-line tuple type keeps its layout
                let open_end = self.open_bracket_end(s.value.end());
                let close_start = s.end() - TextSize::from(1);
                self.replace(TextRange::new(s.start(), open_end), "(");
                // `tuple[int]` → `(int,)` — a single positional needs the
                // trailing comma to disambiguate from a parenthesized
                // expression. an unpacked element (`tuple[*A]` → `(*A)`) is
                // already unambiguous, so it needs none
                let needs_comma = elements.len() == 1
                    && !first.is_starred_expr()
                    && !self.has_trailing_comma(last.end(), close_start);
                self.replace(
                    TextRange::new(close_start, s.end()),
                    if needs_comma { ",)" } else { ")" },
                );
                for element in elements {
                    if let Some(inner) = self.variadic_element(element) {
                        self.replace(TextRange::new(element.start(), inner.start()), "*: ");
                        self.replace(TextRange::new(inner.end(), element.end()), "");
                        self.visit_type_expr(inner);
                    } else {
                        self.visit_type_expr(element);
                    }
                }
            }

            // propagate into the slice of any other type-context subscript
            Expr::Subscript(s) if self.is_type_context_subscript(&s.value) => {
                if subscript_head(&s.value) == Some("Unpack") {
                    self.visit_unpack_operand(&s.slice);
                } else {
                    self.visit_type_expr(&s.slice);
                }
            }

            Expr::BinOp(b) => {
                self.visit_type_expr(&b.left);
                self.visit_type_expr(&b.right);
            }

            // an unparenthesized tuple is a subscript slice; a parenthesized one
            // is a basedpython tuple type that may nest more
            Expr::Tuple(t) => {
                for element in &t.elts {
                    self.visit_type_expr(element);
                }
            }

            // a `Callable[[A, B], R]` parameter list
            Expr::List(l) => {
                for element in &l.elts {
                    self.visit_type_expr(element);
                }
            }

            Expr::Starred(s) => self.visit_unpack_operand(&s.value),

            _ => {}
        }
    }
}

/// the name a subscript is applied to, for the heads that matter here
fn subscript_head(value: &Expr) -> Option<&str> {
    match value {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        _ => None,
    }
}

/// the elements of a `tuple[...]` slice, or `None` when the slice has no
/// denotable basedpython form: `tuple[()]` (the empty tuple) and a
/// parenthesized slice, which the subscript reverse owns
fn tuple_elements(slice: &Expr) -> Option<Vec<&Expr>> {
    match slice {
        Expr::Tuple(t) if !t.parenthesized => Some(t.elts.iter().collect()),
        Expr::Tuple(_) => None,
        other => Some(vec![other]),
    }
}

/// the element type of a homogeneous `tuple[T, ...]` slice
fn homogeneous_element(slice: &Expr) -> Option<&Expr> {
    let Expr::Tuple(t) = slice else {
        return None;
    };
    match t.elts.as_slice() {
        [element, Expr::EllipsisLiteral(_)] if !t.parenthesized => Some(element),
        _ => None,
    }
}

impl<'ast> Visitor<'ast> for TupleTypeReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        crate::transforms::source_util::for_each_annotation_in_stmt(stmt, |ann| {
            self.visit_type_expr(ann);
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
    fn simple_tuple() {
        check("a: tuple[int, str]\n", "a: (int, str)\n");
    }

    #[test]
    fn variadic_tuple_round_trip() {
        // forward `(int, *: str)` lowers to `tuple[int, *tuple[str, ...]]`;
        // reverse must restore the basedpython spelling
        check("b: tuple[int, *tuple[str, ...]]\n", "b: (int, *: str)\n");
    }

    #[test]
    fn single_element() {
        check("a: tuple[int]\n", "a: (int,)\n");
    }

    /// an unpacked element keeps its star; the lone-element form needs no
    /// trailing comma, since the star already makes it a tuple
    #[test]
    fn unpacked_element() {
        check("a: tuple[*A]\n", "a: (*A)\n");
    }

    #[test]
    fn unpacked_element_after_a_prefix() {
        check("a: tuple[int, *A]\n", "a: (int, *A)\n");
    }

    #[test]
    fn nested_tuple() {
        check(
            "a: tuple[int, tuple[str, float]]\n",
            "a: (int, (str, float))\n",
        );
    }

    #[test]
    fn tuple_in_union() {
        check("a: tuple[int, str] | None\n", "a: (int, str) | None\n");
    }

    #[test]
    fn tuple_in_subscript() {
        check("a: list[tuple[int, str]]\n", "a: list[(int, str)]\n");
    }

    #[test]
    fn function_annotation() {
        check(
            indoc! {"
                def f(x: tuple[int, str]) -> tuple[bool, float]:
                    pass
            "},
            indoc! {"
                def f(x: (int, str)) -> (bool, float):
                    pass
            "},
        );
    }

    #[test]
    fn homogeneous_tuple_to_variadic() {
        // tuple[int, ...] round-trips to the basedpython variadic spelling
        // `(*: int)` so `(*args: T)` ↔ `tuple[T, ...]` is symmetric
        check("a: tuple[int, ...]\n", "a: (*: int)\n");
    }

    /// the elements keep an edit another reverse transform made inside them —
    /// a whole-expression replacement rendered from raw source would undo it
    #[test]
    fn keeps_a_nested_rewrite() {
        check(
            "from typing import Callable
a: tuple[Callable[[int], str], int]
",
            "from typing import Callable
a: ((int) -> str, int)
",
        );
    }

    /// a multi-line tuple type keeps its line breaks and trailing comma: only
    /// the brackets are rewritten
    #[test]
    fn multiline_layout_is_kept() {
        check(
            "a: tuple[\n    int,\n    str,\n]\n",
            "a: (\n    int,\n    str,\n)\n",
        );
    }

    /// a pep 695 bound is a type expression. in a basedpython file `T: (int, str)`
    /// is a tuple upper bound — constraints take the `constraints` keyword — so
    /// the conversion means what `tuple[int, str]` meant
    #[test]
    fn type_param_bound() {
        check(
            "class A[T: tuple[int, str]]: ...\n",
            "class A[T: (int, str)]\n",
        );
    }

    #[test]
    fn type_param_default() {
        check(
            "class A[T = tuple[int, str]]: ...\n",
            "class A[T = (int, str)]\n",
        );
    }

    /// a class base is a runtime value position: `class C((str, int))` is an
    /// invalid base that raises `TypeError`, so the tuple type stays subscripted
    #[test]
    fn class_base_left_alone() {
        check(
            "class A(tuple[str, int]): ...\n",
            "class A(tuple[str, int])\n",
        );
    }

    /// an alias declared the legacy way is a type expression too
    #[test]
    fn type_alias_value() {
        check(
            "from typing import TypeAlias
Alias: TypeAlias = tuple[int, str]
",
            "from typing import TypeAlias
type Alias = (int, str)
",
        );
    }

    #[test]
    fn value_context_unchanged() {
        check("x = tuple[int, str]\n", "x = tuple[int, str]\n");
    }
}
