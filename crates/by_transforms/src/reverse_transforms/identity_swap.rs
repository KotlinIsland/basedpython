//! reverse of `crate::transforms::identity_swap`:
//!   `x is y`      → `x === y`
//!   `x is not y`  → `x !== y`
//!
//! basedpython gives the `is` keyword to the type test and spells identity
//! `===`, so a python identity comparison round-trips to `===` / `!==`. that
//! includes `x is None`: basedpython reads `x is None` as a test for the type
//! `None`, and a type test the value's static type settles is emitted as its
//! answer. python's comparison runs whatever the annotations say — `x is None`
//! on a parameter annotated `int` is how python code defends against a caller
//! the annotation does not bind — so only the identity spelling keeps it a check
//! that runs. leaving a python `is not` in place would also re-read it as
//! `not isinstance(...)` on the way back out
//!
//! an `isinstance` call stays a call for the same reason. `x is int` is the
//! idiomatic basedpython test, and it folds wherever `x`'s type already decides
//! it, which would erase the validation a python `isinstance` guard exists to
//! perform. basedpython runs the call just as python does

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{CmpOp, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

pub(crate) struct IdentitySwapReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> IdentitySwapReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn process_compare(&mut self, c: &ruff_python_ast::ExprCompare) {
        let mut lhs_end = c.left.range().end();
        for (op, rhs) in c.ops.iter().zip(c.comparators.iter()) {
            let rhs_start = rhs.range().start();
            let between = &self.source[usize::from(lhs_end)..usize::from(rhs_start)];
            let words: &[&str] = match op {
                CmpOp::Is => &["is"],
                CmpOp::IsNot => &["is", "not"],
                _ => &[],
            };
            if let Some(tokens) = operator_tokens(between, lhs_end, words) {
                self.rewrite_operator(&tokens, between, lhs_end);
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
        if let Expr::Compare(c) = expr {
            self.process_compare(c);
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

    /// `is None` is a type test in basedpython, and one the value's static type
    /// settles is emitted as its answer, so it takes the identity operator like
    /// every other literal
    #[test]
    fn none_comparisons_take_the_identity_operator() {
        check("y = a is None\n", "y = a === None\n");
        check("y = a is not None\n", "y = a !== None\n");
    }

    #[test]
    fn other_literal_comparisons_take_the_identity_operator() {
        // `a is 1` is identity in python but a test for the type `Literal[1]`
        // in basedpython, and those part company for an int outside the
        // interned range — so the operator has to be written back out
        check("y = a is True\n", "y = a === True\n");
        check("y = a is not 1\n", "y = a !== 1\n");
    }

    /// `x is int` folds wherever `x`'s type decides it, so a call keeps the
    /// check python wrote
    #[test]
    fn isinstance_stays_a_call() {
        check("y = isinstance(a, int)\n", "y = isinstance(a, int)\n");
        check(
            "y = not isinstance(a, (int, str))\n",
            "y = not isinstance(a, (int, str))\n",
        );
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
    fn round_trips() {
        check_round_trip("y = a is b\n");
        check_round_trip("y = a is not b\n");
        check_round_trip("y = a is None\n");
        check_round_trip("y = a is not None\n");
        check_round_trip("y = isinstance(a, int)\n");
        check_round_trip("y = not isinstance(a, int)\n");
    }

    /// the annotation says what a check can only confirm at runtime. a type test
    /// basedpython settles statically is emitted as its answer, so a python
    /// check the annotations already decide must come back as the check
    #[test]
    fn a_check_the_annotations_settle_survives() {
        check_round_trip(indoc! {"
            def validate(x: int) -> int:
                if not isinstance(x, int):
                    raise TypeError
                return x
        "});
        check_round_trip(indoc! {"
            def f(x: int) -> None:
                assert x is not None
        "});
    }
}
