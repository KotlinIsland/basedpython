//! reverse of `crate::transforms::intersection`:
//!   `Intersection[A, B]`    → `A & B`
//!   `Intersection[A, B, C]` → `A & B & C`
//!
//! only fires in annotation positions when `Intersection` resolves to `ty_extensions`

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct IntersectionReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> IntersectionReverse<'src> {
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

    fn is_intersection_name(&self, expr: &Expr) -> bool {
        trailing_name(expr) == Some("Intersection") && self.types.subscript_is_type_context(expr)
    }

    fn rewrite(&mut self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Subscript(s) if self.is_intersection_name(&s.value) => {
                let elts: Vec<&Expr> = match s.slice.as_ref() {
                    Expr::Tuple(t) if !t.parenthesized => t.elts.iter().collect(),
                    other => vec![other],
                };
                if elts.len() < 2 {
                    return None;
                }
                let parts: Vec<String> = elts
                    .iter()
                    .map(|e| {
                        self.rewrite(e)
                            .unwrap_or_else(|| self.src(e.range()).to_owned())
                    })
                    .collect();
                Some(parts.join(" & "))
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

impl<'ast> Visitor<'ast> for IntersectionReverse<'_> {
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
    fn two_types() {
        check(
            "from ty_extensions import Intersection\na: Intersection[A, B]\n",
            "from ty_extensions import Intersection\na: A & B\n",
        );
    }

    #[test]
    fn three_types() {
        check(
            "from ty_extensions import Intersection\na: Intersection[A, B, C]\n",
            "from ty_extensions import Intersection\na: A & B & C\n",
        );
    }

    #[test]
    fn intersection_in_union() {
        check(
            "from ty_extensions import Intersection\na: Intersection[A, B] | C\n",
            "from ty_extensions import Intersection\na: A & B | C\n",
        );
    }

    #[test]
    fn intersection_nested_in_subscript() {
        check(
            "from ty_extensions import Intersection\na: list[Intersection[A, B]]\n",
            "from ty_extensions import Intersection\na: list[A & B]\n",
        );
    }

    #[test]
    fn function_parameter() {
        check(
            indoc! {"
                from ty_extensions import Intersection
                def f(x: Intersection[A, B]) -> Intersection[A, C]:
                    pass
            "},
            indoc! {"
                from ty_extensions import Intersection
                def f(x: A & B) -> A & C:
                    pass
            "},
        );
    }

    #[test]
    fn shadowed_unchanged() {
        check(
            indoc! {"
                Intersection = object()
                a: Intersection[A, B]
            "},
            indoc! {"
                Intersection = object()
                a: Intersection[A, B]
            "},
        );
    }
}
