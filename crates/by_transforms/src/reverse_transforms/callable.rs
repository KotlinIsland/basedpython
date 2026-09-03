//! reverse of `crate::transforms::callable`:
//!   `Callable[[int], int]`       → `(int) -> int`
//!   `Callable[[int, str], bool]` → `(int, str) -> bool`
//!   `Callable[[], None]`         → `() -> None`
//!
//! only fires in annotation positions when `Callable` resolves to the typing import.
//!
//! the rewrite is emitted as edits over the punctuation — `Callable[[` becomes
//! `(`, the `],` between the parameters and the return becomes `) -> `, and the
//! closing `]` goes — rather than one replacement rendered over the whole
//! expression. the operand text is then never re-rendered, so another
//! transform's edit inside it survives (a whole-expression replacement built
//! from raw source would silently undo it) and the source's line breaks are kept

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct CallableReverse<'src> {
    types: &'src dyn TypeInfo,
    /// in stub mode the `Callable[[A, B], R]` list form is left intact for
    /// `by_typeshed_patch`'s `arrow-callable`, which converts the same shapes
    /// but also parenthesises by precedence and handles `ParamSpec`,
    /// `Concatenate` and variadic parameter lists. the gradual
    /// `Callable[..., R]` form has no parameter list to get wrong and is always
    /// rewritten to `(...) -> R`
    stub: bool,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> CallableReverse<'src> {
    pub(crate) fn new(_source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            types,
            stub: false,
            edits: Vec::new(),
        }
    }

    pub(crate) fn stub(mut self) -> Self {
        self.stub = true;
        self
    }

    fn replace(&mut self, range: TextRange, text: &str) {
        let edit = if text.is_empty() {
            Edit::range_deletion(range)
        } else {
            Edit::range_replacement(text.to_owned(), range)
        };
        self.edits.push(Fix::safe_edit(edit));
    }

    fn is_callable_name(&self, expr: &Expr) -> bool {
        trailing_name(expr) == Some("Callable") && self.types.subscript_is_type_context(expr)
    }

    fn is_type_context_subscript(&self, value: &Expr) -> bool {
        trailing_name(value).is_some() && self.types.subscript_is_type_context(value)
    }

    /// rewrite the punctuation of `Callable[<params>, <ret>]` into the arrow
    /// form. `params_open` and `params_close` bound the text the parameters sit
    /// inside — the list brackets for `[A, B]`, the `...` itself for the gradual
    /// form — so the emitted parentheses land where they belong
    fn arrow_punctuation(
        &mut self,
        sub_range: TextRange,
        params_open: TextRange,
        params_close: TextRange,
        ret: &Expr,
    ) {
        self.replace(TextRange::new(sub_range.start(), params_open.end()), "(");
        self.replace(TextRange::new(params_close.start(), ret.start()), ") -> ");
        self.replace(TextRange::new(ret.end(), sub_range.end()), "");
    }

    /// walk a type expression, emitting arrow edits for every convertible
    /// `Callable[...]` within it
    fn visit_type_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Subscript(s) if self.is_callable_name(&s.value) => {
                let Some((params, ret)) = callable_parts(&s.slice) else {
                    return;
                };
                match params {
                    // `Callable[..., R]` — "any arguments" — reverses to
                    // `(...) -> R`. safe in stub mode: there is no parameter
                    // list to lose
                    Expr::EllipsisLiteral(ellipsis) => {
                        let dots = ellipsis.range();
                        self.replace(TextRange::new(s.start(), dots.start()), "(");
                        self.replace(TextRange::new(dots.end(), ret.start()), ") -> ");
                        self.replace(TextRange::new(ret.end(), s.end()), "");
                    }
                    // the list form: in stub mode leave the `Callable[...]`
                    // wrapper intact but still descend, so a nested
                    // `Callable[..., R]` is converted
                    Expr::List(args) if !self.stub => {
                        let brackets = args.range();
                        let open =
                            TextRange::new(brackets.start(), brackets.start() + TextSize::from(1));
                        let close =
                            TextRange::new(brackets.end() - TextSize::from(1), brackets.end());
                        self.arrow_punctuation(s.range(), open, close, ret);
                        for arg in &args.elts {
                            self.visit_type_expr(arg);
                        }
                        self.visit_type_expr(ret);
                        return;
                    }
                    _ => {
                        self.visit_type_expr(params);
                        self.visit_type_expr(ret);
                        return;
                    }
                }
                self.visit_type_expr(ret);
            }

            Expr::Subscript(s) if self.is_type_context_subscript(&s.value) => {
                self.visit_type_expr(&s.slice);
            }

            Expr::BinOp(b) => {
                self.visit_type_expr(&b.left);
                self.visit_type_expr(&b.right);
            }

            Expr::Tuple(t) => {
                for element in &t.elts {
                    self.visit_type_expr(element);
                }
            }

            Expr::List(l) => {
                for element in &l.elts {
                    self.visit_type_expr(element);
                }
            }

            Expr::Starred(s) => self.visit_type_expr(&s.value),

            _ => {}
        }
    }
}

impl<'ast> Visitor<'ast> for CallableReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        crate::transforms::source_util::for_each_annotation_in_stmt(stmt, |ann| {
            self.visit_type_expr(ann);
        });
    }
}

/// the `(parameters, return)` of a `Callable[P, R]` slice
fn callable_parts(slice: &Expr) -> Option<(&Expr, &Expr)> {
    let Expr::Tuple(t) = slice else {
        return None;
    };
    match t.elts.as_slice() {
        [params, ret] if !t.parenthesized => Some((params, ret)),
        _ => None,
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

    fn check_stub(input: &str, expected: &str) {
        let config = Config {
            is_stub: true,
            ..Config::test_default()
        };
        assert_eq!(reverse_transpile(input, &config).unwrap(), expected);
    }

    #[test]
    fn stub_keeps_list_form_but_rewrites_ellipsis() {
        // in stub mode the `Callable[[A], R]` list form is preserved (can't
        // carry `Unpack[Ts]`/`*Ts` through the arrow), but the gradual
        // `Callable[..., R]` form is still rewritten to `(...) -> R`
        check_stub(
            "from typing import Callable\na: Callable[..., int]\nb: Callable[[int], str]\n",
            "from typing import Callable\na: (...) -> int\nb: Callable[[int], str]\n",
        );
    }

    #[test]
    fn stub_rewrites_nested_ellipsis_inside_list_form() {
        check_stub(
            "from typing import Callable\na: Callable[[Callable[..., int]], str]\n",
            "from typing import Callable\na: Callable[[(...) -> int], str]\n",
        );
    }

    #[test]
    fn simple_callable() {
        check(
            "from typing import Callable\na: Callable[[int], int]\n",
            "from typing import Callable\na: (int) -> int\n",
        );
    }

    #[test]
    fn no_args() {
        check(
            "from typing import Callable\na: Callable[[], None]\n",
            "from typing import Callable\na: () -> None\n",
        );
    }

    #[test]
    fn multi_args() {
        check(
            "from typing import Callable\na: Callable[[int, str], bool]\n",
            "from typing import Callable\na: (int, str) -> bool\n",
        );
    }

    #[test]
    fn ellipsis_args() {
        check(
            "from typing import Callable\na: Callable[..., int]\n",
            "from typing import Callable\na: (...) -> int\n",
        );
    }

    #[test]
    fn ellipsis_args_nested_return() {
        check(
            "from typing import Callable\na: Callable[..., Callable[[int], str]]\n",
            "from typing import Callable\na: (...) -> (int) -> str\n",
        );
    }

    #[test]
    fn callable_in_union() {
        check(
            "from typing import Callable\na: Callable[[int], int] | None\n",
            "from typing import Callable\na: (int) -> int?\n",
        );
    }

    #[test]
    fn nested_callable() {
        check(
            "from typing import Callable\na: Callable[[int], Callable[[str], bool]]\n",
            "from typing import Callable\na: (int) -> (str) -> bool\n",
        );
    }

    #[test]
    fn callable_in_function_signature() {
        check(
            indoc! {"
                from typing import Callable
                def f(x: Callable[[int], bool]) -> Callable[[str], None]:
                    pass
            "},
            indoc! {"
                from typing import Callable
                def f(x: (int) -> bool) -> (str) -> None:
                    pass
            "},
        );
    }

    #[test]
    fn callable_inside_list_subscript() {
        check(
            "from typing import Callable\na: list[Callable[[int], int]]\n",
            "from typing import Callable\na: list[(int) -> int]\n",
        );
    }

    /// the operands keep an edit another reverse transform made inside them —
    /// a whole-expression replacement rendered from raw source would undo it
    #[test]
    fn keeps_a_nested_rewrite() {
        check(
            "from typing import Callable
a: Callable[[tuple[int, str]], tuple[bool]]
",
            "from typing import Callable
a: ((int, str)) -> (bool,)
",
        );
    }

    /// an alias declared the legacy way is a type expression too
    #[test]
    fn type_alias_value() {
        check(
            "from typing import Callable, TypeAlias
Alias: TypeAlias = Callable[[int], str]
",
            "from typing import Callable, TypeAlias
type Alias = (int) -> str
",
        );
    }

    #[test]
    fn shadowed_callable_unchanged() {
        check(
            indoc! {"
                Callable = object()
                a: Callable[[int], int]
            "},
            indoc! {"
                Callable = object()
                a: Callable[[int], int]
            "},
        );
    }
}
