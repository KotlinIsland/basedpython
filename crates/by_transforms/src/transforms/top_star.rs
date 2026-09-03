//! AST pass that lowers basedpython `X[..., *, ...]` to
//! `ty_extensions.Top[X[..., Any, ...]]`.
//!
//! The parser produces `ExprSubscript` slices with one or more
//! `Starred(Name(id="", ctx=Invalid))` markers. Each marker becomes
//! `Any` in the lowered slice, concrete elements pass through unchanged.
//! Markers nested inside type-position binops (e.g. `list[int | *]` →
//! `Top[list[int | Any]]`) are also rewritten — the walk stops at nested
//! `Subscript` boundaries so the marker binds to its enclosing subscript.
//!
//! traversal is delegated to [`type_expr_walker`], so the marker is
//! recognised in every type position consistently with other type-position
//! transforms

use ruff_python_ast::helpers::{is_top_star_marker, top_star_marker_ranges_in_slice};
use ruff_python_ast::{
    AtomicNodeIndex, Expr, ExprContext, ExprName, ExprSubscript, ModModule, Stmt, name::Name,
};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{AstPass, PassContext, render_expr};
use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};

pub(crate) struct TopStar;

impl TopStar {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl AstPass for TopStar {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            edits: Vec::new(),
            needs_top: false,
            needs_any: false,
        };
        let body: &[Stmt] = &module.body;
        walk_type_positions(body, None, &mut state);
        ctx.text_edits.extend(state.edits);
        if state.needs_top {
            ctx.required_imports
                .push("from ty_extensions import Top".to_owned());
        }
        if state.needs_any {
            ctx.required_imports
                .push("from typing import Any".to_owned());
        }
    }
}

struct State {
    edits: Vec<(TextRange, String)>,
    needs_top: bool,
    needs_any: bool,
}

impl TypeExprVisitor for State {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Expr::Subscript(s) = expr
            && !top_star_marker_ranges_in_slice(&s.slice).is_empty()
        {
            self.needs_top = true;
            self.needs_any = true;
            let mut new_slice = (*s.slice).clone();
            replace_markers_with_any(&mut new_slice);
            let inner = Expr::Subscript(ExprSubscript {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                value: s.value.clone(),
                slice: Box::new(new_slice),
                ctx: ExprContext::Load,
                is_typeof: false,
                is_type_decoration: false,
            });
            let outer = Expr::Subscript(ExprSubscript {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                value: Box::new(Expr::Name(ExprName {
                    node_index: AtomicNodeIndex::NONE,
                    range: TextRange::default(),
                    id: Name::from("Top"),
                    ctx: ExprContext::Load,
                })),
                slice: Box::new(inner),
                ctx: ExprContext::Load,
                is_typeof: false,
                is_type_decoration: false,
            });
            self.edits.push((expr.range(), render_expr(&outer)));
            // we just replaced the whole subscript with `Top[…]` — telling
            // the walker to stop avoids it descending into the original
            // slice (which would re-fire on the same marker via nested
            // subscripts but those rewrites are subsumed by our wide edit)
            return Recurse::Stop;
        }
        Recurse::Descend
    }
}

fn any_expr() -> Expr {
    Expr::Name(ExprName {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        id: Name::from("Any"),
        ctx: ExprContext::Load,
    })
}

/// Replace every top-star marker reachable from `expr` with an `Any` name,
/// stopping at nested `Subscript` boundaries so markers bind to their
/// enclosing subscript only
fn replace_markers_with_any(expr: &mut Expr) {
    if is_top_star_marker(expr) {
        *expr = any_expr();
        return;
    }
    match expr {
        Expr::Subscript(_) => {}
        Expr::BinOp(b) => {
            replace_markers_with_any(&mut b.left);
            replace_markers_with_any(&mut b.right);
        }
        Expr::Tuple(t) => {
            for elt in &mut t.elts {
                replace_markers_with_any(elt);
            }
        }
        Expr::UnaryOp(u) => replace_markers_with_any(&mut u.operand),
        Expr::BoolOp(b) => {
            for v in &mut b.values {
                replace_markers_with_any(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple_list() {
        check(
            "a: list[*]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[list[Any]]
            "},
        );
    }

    #[test]
    fn dict_star() {
        check(
            "a: dict[*, *]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[dict[Any, Any]]
            "},
        );
    }

    #[test]
    fn triple_star() {
        check(
            "a: X[*, *, *]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[X[Any, Any, Any]]
            "},
        );
    }

    #[test]
    fn in_union() {
        check(
            "a: list[*] | int\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[list[Any]] | int
            "},
        );
    }

    #[test]
    fn in_function_signature() {
        check(
            "def f(x: list[*]) -> set[*]: ...\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                def f(x: Top[list[Any]]) -> Top[set[Any]]: ...
            "},
        );
    }

    #[test]
    fn nested_attribute_value() {
        check(
            "a: collections.abc.Mapping[*]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[collections.abc.Mapping[Any]]
            "},
        );
    }

    #[test]
    fn regular_subscript_unchanged() {
        unchanged("a: list[int]\n");
    }

    #[test]
    fn mixed_str_then_star() {
        check(
            "a: dict[str, *]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[dict[str, Any]]
            "},
        );
    }

    #[test]
    fn mixed_star_then_int() {
        check(
            "a: dict[*, int]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[dict[Any, int]]
            "},
        );
    }

    #[test]
    fn mixed_in_function() {
        check(
            "def f(data: dict[str, *]): ...\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                def f(data: Top[dict[str, Any]]): ...
            "},
        );
    }

    #[test]
    fn mixed_three_args_middle_star() {
        check(
            "a: X[int, *, str]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[X[int, Any, str]]
            "},
        );
    }

    #[test]
    fn star_in_union_right() {
        check(
            "a: list[int | *]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[list[int | Any]]
            "},
        );
    }

    #[test]
    fn star_in_union_left() {
        check(
            "a: list[* | int]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[list[Any | int]]
            "},
        );
    }

    #[test]
    fn star_in_union_middle() {
        check(
            "a: list[int | * | str]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[list[int | Any | str]]
            "},
        );
    }

    #[test]
    fn star_in_union_inside_dict_value() {
        check(
            "a: dict[str, int | *]\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                a: Top[dict[str, int | Any]]
            "},
        );
    }

    #[test]
    fn star_in_function_signature_union() {
        check(
            "def f(a: list[int | *]): ...\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                def f(a: Top[list[int | Any]]): ...
            "},
        );
    }

    #[test]
    fn star_in_class_base() {
        check(
            "class C(list[*]): ...\n",
            indoc! {"
                from ty_extensions import Top
                from typing import Any
                class C(Top[list[Any]]): ...
            "},
        );
    }
}
