//! Lowering for the starred wildcard in a class pattern: `case A(x, *_, y)`.
//!
//! A class pattern's positions are the names its class lists in
//! `__match_args__`, so `*_` stands for the run of them nobody asked about, and
//! whatever follows it names the *last* of them:
//!
//! ```text
//! class Line:
//!     __match_args__ = ("start", "mid", "end")
//!
//! case Line(a, *_, b)      ⇒    case Line(a, _, b)
//! ```
//!
//! Which is to say the star is filled in with as many wildcards as it stood
//! for, so the pattern python sees is the one the author would have had to
//! write out. That count is a static fact about the class, which is why the
//! lowering is type-aware; where it cannot be had, [`ClassPatternStarPass`]
//! refuses rather than emit a pattern that reads the wrong attributes. The
//! checker refuses the same source, as `invalid-match-pattern`.
//!
//! A `*_` written last needs no count at all: python already lets a class
//! pattern name fewer positions than the class has, so `case A(x, *_)` matches
//! exactly what `case A(x)` matches. Those are erased along with the comma that
//! separated them, which is also what a star that turns out to stand for no
//! positions at all comes down to.
//!
//! The edits replace the `*_` bytes alone, so every sibling lowering inside the
//! pattern — and the whole-header rewrites that `let` destructuring and `if let`
//! build around one — still composes.
//!
//! There is no reverse transform. `case A(x, _, _, y)` is a perfectly ordinary
//! pattern that says what it means, and rewriting it to `case A(x, *_, y)` would
//! change what it means: the run of wildcards is fixed where the star is not, so
//! the two only agree for as long as `__match_args__` stays the length it is
//! today. A reader who wrote the positions out asked for the first reading.

use ruff_python_ast::visitor::{Visitor, walk_pattern, walk_stmt};
use ruff_python_ast::{Pattern, PatternMatchClass, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

struct ClassPatternStar<'a> {
    source: &'a str,
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
    errors: Vec<String>,
}

impl<'ast> Visitor<'ast> for ClassPatternStar<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_pattern(&mut self, pattern: &'ast Pattern) {
        walk_pattern(self, pattern);

        if let Pattern::MatchClass(class_pattern) = pattern {
            self.lower(class_pattern);
        }
    }
}

impl ClassPatternStar<'_> {
    /// The 1-based line number of `offset`, for diagnostics.
    fn line_of(&self, offset: TextSize) -> usize {
        1 + self.source[..usize::from(offset)].matches('\n').count()
    }

    fn lower(&mut self, class_pattern: &PatternMatchClass) {
        let patterns = &class_pattern.arguments.patterns;
        let Some(star) = patterns.iter().position(Pattern::is_match_star) else {
            return;
        };
        // a second star is a parse error, so this file is not going to be
        // emitted; leaving it alone keeps the reported error the parser's
        if patterns[star + 1..].iter().any(Pattern::is_match_star) {
            return;
        }

        let after = patterns.len() - star - 1;
        if after == 0 {
            self.erase(class_pattern, star);
            return;
        }

        let named = patterns.len() - 1;
        let Some(positions) = self
            .types
            .class_pattern_positional_count(&class_pattern.cls)
        else {
            self.errors.push(format!(
                "a subpattern after `*_` names one of the last entries of `__match_args__`, \
                which this class does not list statically (line {})",
                self.line_of(class_pattern.range().start()),
            ));
            return;
        };
        let Some(fill) = positions.checked_sub(named) else {
            self.errors.push(format!(
                "this class pattern names {named} positions, but the class has only \
                {positions} (line {})",
                self.line_of(class_pattern.range().start()),
            ));
            return;
        };

        if fill == 0 {
            self.erase(class_pattern, star);
            return;
        }
        self.edits.push((
            patterns[star].range(),
            std::iter::repeat_n("_", fill)
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    /// Delete a star that stands for no positions, taking the comma that
    /// separated it from whichever neighbour it has with it.
    fn erase(&mut self, class_pattern: &PatternMatchClass, star: usize) {
        let arguments = &class_pattern.arguments;
        let range = arguments.patterns[star].range();
        let following = arguments
            .patterns
            .get(star + 1)
            .map(Ranged::range)
            .or_else(|| arguments.keywords.first().map(Ranged::range));
        let deleted = match (following, star.checked_sub(1)) {
            (Some(following), _) => TextRange::new(range.start(), following.start()),
            (None, Some(preceding)) => {
                TextRange::new(arguments.patterns[preceding].end(), range.end())
            }
            (None, None) => range,
        };
        self.edits.push((deleted, String::new()));
    }
}

pub(crate) struct ClassPatternStarPass<'a> {
    source: &'a str,
}

impl<'a> ClassPatternStarPass<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for ClassPatternStarPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ClassPatternStar {
            source: self.source,
            types,
            edits: Vec::new(),
            errors: Vec::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.text_edits.extend(inner.edits);
        ctx.errors.extend(inner.errors);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    /// A class whose four positions are wide enough apart to tell which one a
    /// subpattern landed on.
    const LINE: &str = "\
class Line:
    __match_args__ = (\"start\", \"mid\", \"stop\", \"end\")
    start: int = 0
    mid: int = 1
    stop: int = 2
    end: int = 3
";

    fn check(case: &str, expected: &str) {
        let source = format!("{LINE}match shape:\n    case {case}:\n        pass\n");
        let output = transpile(&source, &Config::test_default()).unwrap();
        let expected = format!("{LINE}match shape:\n    case {expected}:\n        pass\n");
        assert_eq!(
            output,
            crate::python_passthrough::lazify_expected(&expected)
        );
    }

    fn error(case: &str) -> String {
        let source = format!("{LINE}match shape:\n    case {case}:\n        pass\n");
        transpile(&source, &Config::test_default()).unwrap_err()
    }

    #[test]
    fn fills_the_gap_the_star_stood_for() {
        check("Line(a, *_, b)", "Line(a, _, _, b)");
    }

    #[test]
    fn leading_star_counts_every_position_back() {
        check("Line(*_, b)", "Line(_, _, _, b)");
    }

    #[test]
    fn trailing_star_is_erased_with_its_comma() {
        check("Line(a, *_)", "Line(a)");
    }

    #[test]
    fn lone_star_matches_the_class_alone() {
        check("Line(*_)", "Line()");
    }

    #[test]
    fn star_before_a_keyword_is_erased() {
        check("Line(a, *_, end=b)", "Line(a, end=b)");
    }

    #[test]
    fn lone_star_before_a_keyword_is_erased() {
        check("Line(*_, end=b)", "Line(end=b)");
    }

    #[test]
    fn a_star_standing_for_nothing_is_erased() {
        // all four positions are named, so the star fills in no wildcards at
        // all and comes out the same as writing them without it
        check("Line(a, b, c, *_, d)", "Line(a, b, c, d)");
    }

    #[test]
    fn nested_class_pattern() {
        check("Outer(Line(a, *_, b))", "Outer(Line(a, _, _, b))");
    }

    #[test]
    fn guard_is_untouched() {
        check("Line(a, *_, b) if a < b", "Line(a, _, _, b) if a < b");
    }

    #[test]
    fn more_positions_than_the_class_has_is_a_hard_error() {
        let err = error("Line(a, b, c, d, *_, e)");
        assert!(err.contains("names 5 positions"), "got: {err}");
    }

    #[test]
    fn an_unknown_class_is_a_hard_error() {
        let source = "\
match shape:
    case Unresolved(a, *_, b):
        pass
";
        let err = transpile(source, &Config::test_default()).unwrap_err();
        assert!(err.contains("does not list statically"), "got: {err}");
    }

    #[test]
    fn an_unknown_class_under_a_trailing_star_still_lowers() {
        // nothing follows the star, so no position has to be counted and the
        // class need not be known at all
        let source = "\
match shape:
    case Unresolved(a, *_):
        pass
";
        let expected = "\
match shape:
    case Unresolved(a):
        pass
";
        assert_eq!(
            transpile(source, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn python_class_pattern_unchanged() {
        unchanged("match shape:\n    case Line(a, b):\n        pass\n");
    }
}
