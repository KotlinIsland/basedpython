//! Reverse-style normalization for type subscripts:
//!   `dict[(int, str)]` → `dict[int, str]`
//!
//! When a type subscript's slice is a parenthesized tuple, strip the parens.
//! Both forms parse to the same AST, but basedpython's preferred surface form
//! omits the redundant parentheses.
//!
//! Value-context subscripts are left alone: basedpython's forward direction
//! no longer rewrites runtime tuple-keyed subscripts (the rewrite was
//! shelved — see `docs/basedpython/features/` for the active feature list),
//! so there is nothing to reverse on the value side.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextSize};

use crate::type_info::TypeInfo;

pub(crate) struct SubscriptReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> SubscriptReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn is_type_subscript(&self, value: &Expr) -> bool {
        match value {
            Expr::Name(n) => self.types.subscript_is_type_context(n),
            Expr::Attribute(a) => match a.value.as_ref() {
                Expr::Name(base) => self.types.attr_base_is_type_context(base),
                _ => false,
            },
            _ => false,
        }
    }
}

impl<'ast> Visitor<'ast> for SubscriptReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(s) = expr
            && self.is_type_subscript(&s.value)
            && let Expr::Tuple(t) = s.slice.as_ref()
            && t.parenthesized
            && !t.elts.is_empty()
        {
            // Strip exactly one outer paren on each side of the tuple's range.
            let outer = t.range();
            let inner_start = outer.start() + TextSize::from(1);
            let inner_end = outer.end() - TextSize::from(1);
            let inner = self.source[usize::from(inner_start)..usize::from(inner_end)].to_owned();
            self.edits
                .push(Fix::safe_edit(Edit::range_replacement(inner, outer)));
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn dict_with_paren_tuple() {
        check("a: dict[(int, str)]\n", "a: dict[int, str]\n");
    }

    #[test]
    fn list_with_paren_tuple() {
        // `list[(int,)]` is unusual but the rule applies the same way.
        check(
            "a: tuple[(int, str, float)]\n",
            "a: tuple[int, str, float]\n",
        );
    }

    #[test]
    fn already_unparenthesized_unchanged() {
        check("a: dict[int, str]\n", "a: dict[int, str]\n");
    }

    #[test]
    fn scalar_slice_unchanged() {
        check("a: list[int]\n", "a: list[int]\n");
    }

    #[test]
    fn value_subscript_unchanged() {
        // Value subscripts are never rewritten in either direction: the
        // forward 1-tuple-of-tuple wrap was shelved, so a literal
        // `d[(a, b),]` in `.py` source is faithfully preserved.
        check("d = {}\nv = d[(a, b),]\n", "d = {}\nv = d[(a, b),]\n");
    }

    #[test]
    fn empty_tuple_subscript_unchanged() {
        // `tuple[()]` is the empty tuple type; stripping parens would produce
        // invalid syntax `tuple[]`, so the transform must leave it alone.
        check("a: tuple[()]\n", "a: tuple[()]\n");
    }

    #[test]
    fn nested_inside_optional() {
        check("a: dict[(int, str)] | None\n", "a: dict[int, str] | None\n");
    }

    #[test]
    fn recursive_tuple_growth_terminates() {
        // asking for a type mid-walk enters inference at the scope this
        // expression is in, so a recursive body's fixed point is reached from a
        // different query than a whole-file check reaches it from. a body that
        // concatenates onto its own result has to settle either way
        let source = "def shape(tensor):\n    if not hasattr(tensor, '__iter__'):\n        return ()\n    seq = list(tensor)\n    return (len(seq),) + shape(seq[0])\n";
        check(source, source);
    }

    #[test]
    fn call_narrowing_its_own_callee_terminates() {
        // every statement-level call in a basedpython file records an assertion-guard
        // predicate over its arguments, so working out what one of these calls narrows can
        // need a type whose inference reads a place that same call narrows — a question
        // asked while it is already being answered. the subscript is the entry point: it is
        // what asks for a type mid-walk
        let source = "def merge(word):\n    while True:\n        smallest = min(pairs)\n        merged = []\n        while index:\n            merged.extend(word)\n            merged.append(word[index])\n            pairs = pairs_of(word)\n";
        check(source, source);
    }
}
