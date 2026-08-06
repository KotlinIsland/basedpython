//! reverse of `crate::transforms::annotation`:
//!   `tuple[int, str]` → `(int, str)` in annotation positions
//!   `tuple[int]`      → `(int,)`
//!
//! only fires on the builtin `tuple` subscript in annotation positions

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::TypeInfo;

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

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn is_tuple_name(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Name(n) => n.id.as_str() == "tuple" && self.types.subscript_is_type_context(n),
            _ => false,
        }
    }

    fn rewrite(&mut self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Subscript(s) if self.is_tuple_name(&s.value) => {
                // homogeneous variadic `tuple[T, ...]` → `(*: T)` so the
                // basedpython parameter-shape syntax round-trips
                if let Expr::Tuple(t) = s.slice.as_ref()
                    && !t.parenthesized
                    && t.elts.len() == 2
                    && matches!(t.elts.get(1), Some(Expr::EllipsisLiteral(_)))
                {
                    let elem = self
                        .rewrite(&t.elts[0])
                        .unwrap_or_else(|| self.src(t.elts[0].range()).to_owned());
                    return Some(format!("(*: {elem})"));
                }
                let elts: Vec<&Expr> = match s.slice.as_ref() {
                    Expr::Tuple(t) if !t.parenthesized => t.elts.iter().collect(),
                    // parenthesized tuple slice is handled by the subscript reverse; skip
                    Expr::Tuple(_) => return None,
                    other => vec![other],
                };
                let parts: Vec<String> = elts
                    .iter()
                    .map(|e| {
                        // `*tuple[T, ...]` inside a tuple type round-trips
                        // back to the variadic tuple syntax `*: T`
                        if let Expr::Starred(starred) = e
                            && let Expr::Subscript(inner_sub) = starred.value.as_ref()
                            && self.is_tuple_name(&inner_sub.value)
                            && let Expr::Tuple(inner_t) = inner_sub.slice.as_ref()
                            && !inner_t.parenthesized
                            && inner_t.elts.len() == 2
                            && matches!(inner_t.elts.get(1), Some(Expr::EllipsisLiteral(_)))
                        {
                            let elem_src = self
                                .rewrite(&inner_t.elts[0])
                                .unwrap_or_else(|| self.src(inner_t.elts[0].range()).to_owned());
                            return format!("*: {elem_src}");
                        }
                        self.rewrite(e)
                            .unwrap_or_else(|| self.src(e.range()).to_owned())
                    })
                    .collect();
                let inner = if parts.len() == 1 && !parts[0].starts_with('*') {
                    // `tuple[int]` → `(int,)` — single positional needs the
                    // trailing comma to disambiguate from a parenthesized
                    // expression. variadic spelling `(*: int)` doesn't, since
                    // it isn't a valid single-expression group anyway
                    format!("{},", parts[0])
                } else {
                    parts.join(", ")
                };
                Some(format!("({inner})"))
            }

            Expr::BinOp(b) => {
                let l = self.rewrite(&b.left);
                let r = self.rewrite(&b.right);
                if l.is_some() || r.is_some() {
                    let ls = l.unwrap_or_else(|| self.src(b.left.range()).to_owned());
                    let rs = r.unwrap_or_else(|| self.src(b.right.range()).to_owned());
                    Some(format!("{ls} | {rs}"))
                } else {
                    None
                }
            }

            Expr::Subscript(s) => {
                // only propagate into slices of known type-context subscripts
                let is_type_ctx = match s.value.as_ref() {
                    Expr::Name(n) => self.types.subscript_is_type_context(n),
                    Expr::Attribute(a) => {
                        matches!(a.value.as_ref(), Expr::Name(n) if self.types.attr_base_is_type_context(n))
                    }
                    _ => false,
                };
                if !is_type_ctx {
                    return None;
                }
                let slice_rewrite = match s.slice.as_ref() {
                    Expr::Tuple(t) if !t.parenthesized => {
                        let rewrites: Vec<Option<String>> =
                            t.elts.iter().map(|e| self.rewrite(e)).collect();
                        if rewrites.iter().any(Option::is_some) {
                            let parts: Vec<String> = rewrites
                                .into_iter()
                                .zip(t.elts.iter())
                                .map(|(r, e)| r.unwrap_or_else(|| self.src(e.range()).to_owned()))
                                .collect();
                            Some(parts.join(", "))
                        } else {
                            None
                        }
                    }
                    slice => self.rewrite(slice),
                };
                slice_rewrite.map(|s_text| format!("{}[{s_text}]", self.src(s.value.range())))
            }

            _ => None,
        }
    }

    fn visit_annotation(&mut self, ann: &Expr) {
        if let Some(rewrite) = self.rewrite(ann) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                rewrite,
                ann.range(),
            )));
        }
    }
}

impl<'ast> Visitor<'ast> for TupleTypeReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        crate::transforms::source_util::for_each_annotation_in_stmt(stmt, |ann| {
            self.visit_annotation(ann);
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

    #[test]
    fn value_context_unchanged() {
        check("x = tuple[int, str]\n", "x = tuple[int, str]\n");
    }
}
