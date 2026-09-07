//! reverse of `crate::transforms::identity_swap`:
//!   `x is y`                → `x === y`
//!   `x is not y`            → `x !== y`
//!   `isinstance(x, y)`      → `x is y`
//!   `not isinstance(x, y)`  → `x is not y`
//!
//! basedpython's `is` is the instance check, so a python identity comparison
//! round-trips to `===` / `!==` and an `isinstance` call round-trips to `is`
//!
//! `x is None` is left alone: basedpython reads it as a type test against the
//! type `None`, which is the same runtime check, so rewriting it to
//! `x === None` would churn idiomatic source for no change in meaning. Every
//! *other* literal must still be rewritten — `x is 3` is identity in python but
//! a test for the type `Literal[3]` in basedpython, and those differ for an int
//! outside the interned range.
//!
//! This is also why the operator has to be rewritten rather than skipped:
//! leaving a python `is not` in place re-reads it as a type test on the way
//! back out.
//!
//! An `isinstance` call is only rewritten when its second argument is
//! something a type expression can say. A tuple of classes is `isinstance`'s
//! own spelling for a union and reads as a *tuple type* in a type expression,
//! so it is written back out as `A | B`; anything else — `type(y)`, a variable
//! holding classinfo — keeps the call it was, which basedpython runs just as
//! python does

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{CmpOp, Expr, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange, TextSize};

pub(crate) struct IdentitySwapReverse<'src> {
    source: &'src str,
    /// `isinstance` calls already folded into an enclosing `not`, so the call
    /// itself must not also be rewritten into an overlapping edit
    folded_into_not: Vec<TextRange>,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> IdentitySwapReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            folded_into_not: Vec::new(),
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn process_compare(&mut self, c: &ruff_python_ast::ExprCompare) {
        let mut lhs_end = c.left.range().end();
        for (op, rhs) in c.ops.iter().zip(c.comparators.iter()) {
            let rhs_start = rhs.range().start();
            let between = &self.source[usize::from(lhs_end)..usize::from(rhs_start)];
            // `is None` reads the same in both languages; every other literal
            // rhs changes meaning, so it takes the identity spelling
            if !rhs.is_none_literal_expr() {
                let words: &[&str] = match op {
                    CmpOp::Is => &["is"],
                    CmpOp::IsNot => &["is", "not"],
                    _ => &[],
                };
                if let Some(tokens) = operator_tokens(between, lhs_end, words) {
                    self.rewrite_operator(&tokens, between, lhs_end);
                }
            }
            lhs_end = rhs.range().end();
        }
    }

    /// replace the located operator tokens with their basedpython spelling
    ///
    /// `is` is one token and becomes `===`. `is not` is two, and is normally
    /// replaced as one span so `!==` lands where the operator was. when a
    /// comment sits between the two words that span would swallow it, so the
    /// words are rewritten separately instead — `not` goes with the spaces
    /// after it, which leaves the comment and the line's indentation intact
    fn rewrite_operator(&mut self, tokens: &[TextRange], gap: &str, gap_start: TextSize) {
        let Some(first) = tokens.first() else {
            return;
        };
        let (spelling, Some(last)) = (if tokens.len() == 1 { "===" } else { "!==" }, tokens.get(1))
        else {
            // `is` on its own — replace the one word in place
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                "===".to_owned(),
                *first,
            )));
            return;
        };

        let between_words =
            &gap[usize::from(first.end() - gap_start)..usize::from(last.start() - gap_start)];
        if between_words.trim().is_empty() {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                spelling.to_owned(),
                TextRange::new(first.start(), last.end()),
            )));
            return;
        }

        // a comment sits between the two words, so replace them separately.
        // `not` goes along with the spaces after it, which keeps the line it
        // sat on indented as it was
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            spelling.to_owned(),
            *first,
        )));
        let trailing = gap[usize::from(last.end() - gap_start)..]
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                last.start(),
                last.end() + TextSize::from(u32::try_from(trailing).unwrap_or(0)),
            ))));
    }

    /// `not isinstance(x, y)` → `x is not y`, the exact inverse of the forward
    /// transform. rewriting only the call would leave the correct but clumsier
    /// `not x is y`
    fn process_unary(&mut self, unary: &ruff_python_ast::ExprUnaryOp) {
        if unary.op != UnaryOp::Not {
            return;
        }
        let Expr::Call(call) = unary.operand.as_ref() else {
            return;
        };
        let Some((x, y)) = isinstance_operands(call) else {
            return;
        };
        let Some(target) = self.type_expression_target(y) else {
            return;
        };
        let x_src = self.src(x.range()).to_owned();
        self.folded_into_not.push(call.range());
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            format!("{x_src} is not {target}"),
            unary.range(),
        )));
    }

    fn process_call(&mut self, call: &ruff_python_ast::ExprCall) {
        if self.folded_into_not.contains(&call.range()) {
            return;
        }
        let Some((x, y)) = isinstance_operands(call) else {
            return;
        };
        let Some(target) = self.type_expression_target(y) else {
            return;
        };
        let x_src = self.src(x.range()).to_owned();
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            format!("{x_src} is {target}"),
            call.range(),
        )));
    }

    /// how an `isinstance` classinfo argument is written as the type expression
    /// a type test takes, or `None` when its shape is not one a type expression
    /// has and the call has to stay a call.
    ///
    /// A tuple is `isinstance`'s spelling for "any of these", which a type
    /// expression spells `A | B` — writing the tuple back out would name the
    /// *tuple type* instead, a different test entirely.
    fn type_expression_target(&self, classinfo: &Expr) -> Option<String> {
        match classinfo {
            Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_) => {
                Some(self.src(classinfo.range()).to_owned())
            }
            Expr::BinOp(binop) if binop.op == ruff_python_ast::Operator::BitOr => {
                Some(self.src(classinfo.range()).to_owned())
            }
            Expr::Tuple(tuple) => {
                let arms: Option<Vec<String>> = tuple
                    .elts
                    .iter()
                    .map(|element| self.type_expression_target(element))
                    .collect();
                let arms = arms?;
                // an empty `isinstance(x, ())` is always `False`; there is no
                // union to write for it
                (!arms.is_empty()).then(|| arms.join(" | "))
            }
            _ => None,
        }
    }
}

/// the two operands of an `isinstance(x, y)` call. anything else — a keyword
/// argument, a different arity — stays as-is rather than lose semantics
fn isinstance_operands(call: &ruff_python_ast::ExprCall) -> Option<(&Expr, &Expr)> {
    if !matches!(call.func.as_ref(), Expr::Name(n) if n.id.as_str() == "isinstance") {
        return None;
    }
    if !call.arguments.keywords.is_empty() {
        return None;
    }
    let [x, y] = &*call.arguments.args else {
        return None;
    };
    Some((x, y))
}

/// the range of each word of the operator written between two comparison
/// operands, where `gap_start` is that gap's offset in the file
///
/// the gap holds only the operator, but it may also hold comments, line
/// continuations and newlines — a comment is skipped rather than searched, so
/// `a is  # this\n not b` finds the real `not` and not the one inside the
/// comment. `None` if what is there is not exactly `words`, which leaves an
/// operator this cannot account for untouched
fn operator_tokens(gap: &str, gap_start: TextSize, words: &[&str]) -> Option<Vec<TextRange>> {
    if words.is_empty() {
        return None;
    }
    let mut tokens = Vec::with_capacity(words.len());
    let mut cursor = 0usize;
    while cursor < gap.len() {
        let rest = &gap[cursor..];
        let skip = match rest.as_bytes()[0] {
            b'#' => rest.find('\n').map_or(rest.len(), |end| end + 1),
            b'\\' => 1,
            byte if byte.is_ascii_whitespace() => 1,
            _ => 0,
        };
        if skip > 0 {
            cursor += skip;
            continue;
        }
        let end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        if tokens.len() == words.len() || &rest[..end] != words[tokens.len()] {
            return None;
        }
        let start = gap_start + TextSize::try_from(cursor).ok()?;
        tokens.push(TextRange::new(
            start,
            gap_start + TextSize::try_from(cursor + end).ok()?,
        ));
        cursor += end;
    }
    (tokens.len() == words.len()).then_some(tokens)
}

impl<'ast> Visitor<'ast> for IdentitySwapReverse<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Compare(c) => self.process_compare(c),
            // before the walk reaches the call inside it
            Expr::UnaryOp(unary) => self.process_unary(unary),
            Expr::Call(call) => self.process_call(call),
            _ => {}
        }
        walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    /// python in, reversed to basedpython, transpiled forward again: the
    /// comparison must come back meaning what it meant. this is what a bare
    /// `is not` used to fail — it survived the reverse untouched and then read
    /// as `not isinstance(...)` on the way out
    fn check_round_trip(python: &str) {
        let by = reverse_transpile(python, &Config::test_default()).unwrap();
        let back = transpile(&by, &Config::test_default()).unwrap();
        assert!(
            back.ends_with(python),
            "round trip diverged\n python: {python:?}\n     by: {by:?}\n   back: {back:?}"
        );
    }

    #[test]
    fn isinstance_to_is() {
        check(
            indoc! {"
                if isinstance(x, int):
                    pass
            "},
            indoc! {"
                if x is int:
                    pass
            "},
        );
    }

    #[test]
    fn not_isinstance_to_is_not() {
        check(
            indoc! {"
                if not isinstance(x, str):
                    pass
            "},
            indoc! {"
                if x is not str:
                    pass
            "},
        );
    }

    #[test]
    fn identity_to_triple_equals() {
        check("y = a is b\n", "y = a === b\n");
    }

    #[test]
    fn negated_identity_to_bang_equals() {
        check("y = a is not b\n", "y = a !== b\n");
    }

    #[test]
    fn negated_identity_over_multiple_lines() {
        check(
            indoc! {"
                y = (
                    a
                    is
                    not b
                )
            "},
            indoc! {"
                y = (
                    a
                    !== b
                )
            "},
        );
    }

    /// a comment inside the operator must not be swallowed by the replacement,
    /// and must not stop the rewrite either — leaving `is not` in place would
    /// re-read it as `not isinstance(...)`
    #[test]
    fn negated_identity_around_a_comment() {
        check(
            indoc! {"
                y = (a is  # this note mentions not
                     not b)
            "},
            indoc! {"
                y = (a !==  # this note mentions not
                     b)
            "},
        );
    }

    #[test]
    fn none_comparisons_left_alone() {
        // basedpython reads `is None` as a test for the type `None`, which is
        // the same runtime check python's identity performs
        check("y = a is None\n", "y = a is None\n");
        check("y = a is not None\n", "y = a is not None\n");
    }

    #[test]
    fn a_classinfo_tuple_becomes_a_union() {
        // `isinstance`'s tuple means "any of these"; a type expression spells
        // that `A | B`, and a tuple there is the tuple *type*
        check("y = isinstance(a, (int, str))\n", "y = a is int | str\n");
        check(
            "y = not isinstance(a, (int, (str, bytes)))\n",
            "y = a is not int | str | bytes\n",
        );
    }

    #[test]
    fn a_classinfo_no_type_expression_can_say_keeps_the_call() {
        // a call is not a type expression, and neither is the empty tuple —
        // whose test is always `False` and has no union to write
        check(
            "y = isinstance(a, type(a))\n",
            "y = isinstance(a, type(a))\n",
        );
        check("y = isinstance(a, ())\n", "y = isinstance(a, ())\n");
    }

    #[test]
    fn other_literal_comparisons_take_the_identity_operator() {
        // `a is 1` is identity in python but a test for the type `Literal[1]`
        // in basedpython, and those part company for an int outside the
        // interned range — so the operator has to be written back out
        check("y = a is True\n", "y = a === True\n");
        check("y = a is not 1\n", "y = a !== 1\n");
    }

    /// the comment case cannot round-trip byte for byte — the operator's layout
    /// normalises around the comment. what must survive is the *meaning*: it
    /// has to come back as identity, not as the `not isinstance(...)` a
    /// left-alone `is not` used to produce
    #[test]
    fn comment_case_round_trips_semantically() {
        let python = "y = (a is  # note\n     not b)\n";
        let by = reverse_transpile(python, &Config::test_default()).unwrap();
        let back = transpile(&by, &Config::test_default()).unwrap();
        assert!(back.contains("is not"), "{back:?}");
        assert!(!back.contains("isinstance"), "{back:?}");
    }

    #[test]
    fn unrelated_call_left_alone() {
        check("y = some(x, int)\n", "y = some(x, int)\n");
    }

    #[test]
    fn isinstance_with_keyword_left_alone() {
        check(
            "y = isinstance(x, class_or_tuple=int)\n",
            "y = isinstance(x, class_or_tuple=int)\n",
        );
    }

    #[test]
    fn round_trips() {
        check_round_trip("y = a is b\n");
        check_round_trip("y = a is not b\n");
        check_round_trip("y = a is None\n");
        check_round_trip("y = a is not None\n");
        check_round_trip("y = isinstance(a, int)\n");
        check_round_trip("y = not isinstance(a, int)\n");
    }
}
