//! reverse of `crate::transforms::flexible_keyword`:
//!   `f(**{"a-b": 1})` → `f("a-b"=1)`
//!
//! a mapping unpacking is only rewritten when it exists *because* of a key
//! python cannot spell as a keyword. `f(**{"a": 1})` says "pass this mapping"
//! in perfectly ordinary python and is left alone; `f(**{"a-b": 1})` says it
//! only because `a-b` has no bare spelling, and basedpython can write what it
//! means.
//!
//! the rewrite has to be sure the result is still one call with the same
//! arguments, so it stands down unless every entry has a plain string-literal
//! key, the keys are distinct, and none of them collides with a keyword the
//! call already writes — `f(x=1, **{"x": 2})` is a runtime `TypeError`, while
//! `f(x=1, x=2)` is a syntax error.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Arguments, Expr, ExprDict, Identifier, Stmt};
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_text_size::{Ranged, TextRange};
use std::collections::HashSet;

pub(crate) struct FlexibleKeywordReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> FlexibleKeywordReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// the `name=value` arguments a mapping unpacking can be written as, or
    /// `None` if this one has to stay a mapping
    fn rewrite_mapping(&self, dict: &ExprDict, taken: &HashSet<&str>) -> Option<String> {
        let mut written = HashSet::new();
        let mut arguments = Vec::with_capacity(dict.items.len());
        let mut needed = false;

        for item in &dict.items {
            // a `**spread` entry has no key to name
            let key = item.key.as_ref()?;
            let Expr::StringLiteral(literal) = key else {
                return None;
            };
            if literal.value.is_implicit_concatenated() {
                return None;
            }
            let name = literal.value.to_str();
            // a name written twice is two entries of one mapping but two
            // arguments of one call, which python rejects outright
            if taken.contains(name) || !written.insert(name) {
                return None;
            }
            // the key is written back from its own source, so a spelling the
            // call cannot carry — one spanning lines — stays a mapping
            let spelling = self.src(key.range());
            if spelling.contains(['\n', '\r']) {
                return None;
            }
            if is_identifier(name) {
                arguments.push(format!("{name}={}", self.src(item.value.range())));
            } else {
                needed = true;
                arguments.push(format!("{spelling}={}", self.src(item.value.range())));
            }
        }

        // every key python can spell bare: this is ordinary python saying
        // "pass this mapping", not a flexible name that had nowhere else to go
        needed.then(|| arguments.join(", "))
    }

    fn rewrite_arguments(&mut self, arguments: &Arguments) {
        let taken: HashSet<&str> = arguments
            .keywords
            .iter()
            .filter_map(|keyword| keyword.arg.as_ref().map(Identifier::as_str))
            .collect();

        for keyword in &arguments.keywords {
            if keyword.arg.is_some() {
                continue;
            }
            let Expr::Dict(dict) = &keyword.value else {
                continue;
            };
            if let Some(replacement) = self.rewrite_mapping(dict, &taken) {
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    replacement,
                    keyword.range(),
                )));
            }
        }
    }
}

impl<'ast> Visitor<'ast> for FlexibleKeywordReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt
            && let Some(arguments) = &class.arguments
        {
            self.rewrite_arguments(arguments);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.rewrite_arguments(&call.arguments);
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

    fn unchanged(input: &str) {
        check(input, input);
    }

    #[test]
    fn a_key_python_cannot_spell() {
        check("f(**{\"a-b\": 1})\n", "f(\"a-b\"=1)\n");
    }

    #[test]
    fn every_entry_of_the_mapping_comes_along() {
        // the mapping exists because of `a-b`, so all of it is written as
        // arguments — and a key python can spell is written bare
        check(
            "f(**{\"a\": 1, \"a-b\": 2, \"c.d\": g(3)})\n",
            "f(a=1, \"a-b\"=2, \"c.d\"=g(3))\n",
        );
    }

    #[test]
    fn the_key_keeps_its_own_spelling() {
        check("f(**{'a-b': 1})\n", "f('a-b'=1)\n");
    }

    #[test]
    fn a_mapping_python_can_spell_is_ordinary_python() {
        unchanged("f(**{\"a\": 1, \"b\": 2})\n");
    }

    #[test]
    fn a_name_the_call_already_writes_stays_a_mapping() {
        // `f(x=1, x=2)` is a syntax error, where the mapping is only a
        // `TypeError` — the two are not the same program
        unchanged("f(x=1, **{\"x\": 2, \"a-b\": 3})\n");
    }

    #[test]
    fn a_repeated_key_stays_a_mapping() {
        unchanged("f(**{\"a-b\": 1, \"a-b\": 2})\n");
    }

    #[test]
    fn a_spread_or_a_computed_key_stays_a_mapping() {
        unchanged("f(**{\"a-b\": 1, **rest})\n");
        unchanged("f(**{key: 1, \"a-b\": 2})\n");
        unchanged("f(**{f\"a-{b}\": 1})\n");
    }

    #[test]
    fn a_mapping_that_is_not_a_display_stays_one() {
        unchanged("f(**headers)\n");
    }

    #[test]
    fn class_arguments() {
        // the empty body goes too, as an empty declaration — a separate reverse
        check(
            "class C(Base, **{\"x-y\": 1}): ...\n",
            "class C(Base, \"x-y\"=1)\n",
        );
    }
}
