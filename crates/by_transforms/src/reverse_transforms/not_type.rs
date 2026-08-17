//! reverse of `crate::transforms::not_type`:
//!   `Not[T]` → `not T`
//!
//! only fires in annotation positions when `Not` resolves to `ty_extensions`

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Operator, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct NotTypeReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> NotTypeReverse<'src> {
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

    fn is_not_name(&self, expr: &Expr) -> bool {
        trailing_name(expr) == Some("Not") && self.types.subscript_is_type_context(expr)
    }

    fn rewrite(&mut self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Subscript(s) if self.is_not_name(&s.value) => {
                let inner = self
                    .rewrite(&s.slice)
                    .unwrap_or_else(|| self.src(s.slice.range()).to_owned());
                Some(format!("not {inner}"))
            }
            Expr::BinOp(b) if matches!(b.op, Operator::BitOr | Operator::BitAnd) => {
                let l = self.rewrite(&b.left);
                let r = self.rewrite(&b.right);
                if l.is_some() || r.is_some() {
                    let op = b.op.as_str();
                    let ls = l.unwrap_or_else(|| self.src(b.left.range()).to_owned());
                    let rs = r.unwrap_or_else(|| self.src(b.right.range()).to_owned());
                    Some(format!("{ls} {op} {rs}"))
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

impl<'ast> Visitor<'ast> for NotTypeReverse<'_> {
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
    fn not_annotation_round_trip() {
        check(
            indoc! {"
                from ty_extensions import Not
                x: Not[int]
            "},
            indoc! {"
                from ty_extensions import Not
                x: not int
            "},
        );
    }

    #[test]
    fn not_in_return_type() {
        check(
            indoc! {"
                from ty_extensions import Not
                def f() -> Not[str]: ...
            "},
            indoc! {"
                from ty_extensions import Not
                def f() -> not str
            "},
        );
    }

    #[test]
    fn not_in_nested_subscript() {
        check(
            indoc! {"
                from ty_extensions import Not
                x: list[Not[int]]
            "},
            indoc! {"
                from ty_extensions import Not
                x: list[not int]
            "},
        );
    }
}
