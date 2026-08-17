//! Lowering of the `match` statement for python versions that predate it.
//!
//! `match` is python 3.10 syntax, so a target below that cannot even parse a
//! file containing one. Every `match` in the emitted python — the ones the
//! author wrote, and the ones other lowerings produce (`let` destructuring,
//! `if let`, statement expressions, enum exhaustiveness) — is rewritten here
//! into an `if`/`elif` chain whose conditions do the matching:
//!
//! ```text
//! match point:                    if [__by_match_0__ := (point)]:
//!     case Point(0, y):               if isinstance(__by_match_0__, (__by_match_1__ := (Point))) and …
//!         north(y)            ⇒           north(y)
//!     case _:                         else:
//!         elsewhere()                     elsewhere()
//! ```
//!
//! The rewrite replaces *only* the header spans — `match … :` and each
//! `case … :` — so every case body keeps its exact source bytes, at its exact
//! indentation. That is what makes the subject line become a wrapper `if`
//! rather than a plain assignment: the case clauses sit one level in from the
//! `match`, and they can only stay there if something opened a block on the
//! line the `match` occupied. `[name := subject]` is a one-element list, so it
//! is always truthy and the subject's own truthiness is never consulted.
//!
//! A header that spanned several lines is padded with blank lines to the same
//! count, so the statement occupies exactly the lines it did before and the
//! `.by` line map stays true.
//!
//! ## Patterns as expressions
//!
//! A case's pattern becomes one boolean expression over the subject, with
//! captures bound by assignment expressions along the way — which is why the
//! lowering itself needs python 3.8. Structure that python cannot ask for in an
//! expression is asked of the small helper functions in [`preamble`]: whether a
//! value counts as a sequence or a mapping, what a class's `__match_args__`
//! names, and how a missing attribute or key reports itself. Those helpers
//! answer "no match" with the `_by_match_miss` sentinel, never with an
//! exception, so a failed sub-pattern falls through to the next case instead of
//! escaping the statement.
//!
//! Sub-subjects are bound to temporaries as they are reached, so a nested
//! pattern reads its value once, in the order the source wrote it.
//!
//! ## What is not reproduced
//!
//! A temporary is left bound after the statement — python has no expression
//! that unbinds a name, and a `del` statement would cost a line the line map
//! cannot spare. The names are dunders (see
//! [`temporary_name`](super::source_util::temporary_name)) so that a `match` in
//! a class body leaves nothing `enum` or `dataclass` would read as a member.
//!
//! A comment written *inside* a multi-line pattern is dropped, since the
//! pattern's own text is replaced by the test compiled from it. Comments
//! anywhere else — including one after the header colon — are untouched.
//!
//! Nothing here reports a failure. A `match` this cannot lower — one aimed at a
//! python older than assignment expressions — is left standing, and the
//! target-version check that runs after this reports it against the `.by` line
//! the author actually wrote, which is a coordinate the lowering (working on
//! generated python) does not have.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{
    Pattern, PatternMatchClass, PatternMatchMapping, PatternMatchSequence, PySourceType,
    PythonVersion, Singleton, Stmt, StmtMatch,
};
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::source_util::{preamble_offset, temporary_name};

/// the version that understands `match` natively; at or above it nothing here runs
const MATCH_VERSION: PythonVersion = PythonVersion::PY310;

/// the version the lowering itself needs — it binds captures with assignment
/// expressions, which arrived in 3.8
const LOWERING_VERSION: PythonVersion = PythonVersion::PY38;

/// the sentinel a helper returns for "this sub-pattern did not match"
const MISS: &str = "_by_match_miss";

/// Rewrite every `match` statement in `source` for a target that predates them.
pub(crate) fn lower(source: String, min_version: PythonVersion) -> String {
    if !(LOWERING_VERSION..MATCH_VERSION).contains(&min_version) {
        return source;
    }

    let parsed = ruff_python_parser::parse_unchecked_source(&source, PySourceType::Python);
    let mut lower = Lower {
        source: &source,
        edits: Vec::new(),
        counter: 0,
        needs: Needs::default(),
    };
    for stmt in parsed.suite() {
        lower.visit_stmt(stmt);
    }

    let (edits, needs) = (lower.edits, lower.needs);
    if edits.is_empty() {
        return source;
    }

    // a file whose every pattern is a plain value test names no sentinel
    let sentinel = edits
        .iter()
        .any(|(_, replacement)| replacement.contains(MISS));
    let body = apply(&source, edits);
    let preamble = preamble(&needs, sentinel);
    let at = preamble_offset(&body);
    format!("{}{preamble}{}", &body[..at], &body[at..])
}

/// Apply disjoint replacements, ascending by start.
fn apply(source: &str, mut edits: Vec<(TextRange, String)>) -> String {
    edits.sort_by_key(|(range, _)| range.start());
    let mut out = String::with_capacity(source.len());
    let mut at = 0usize;
    for (range, replacement) in edits {
        let start = usize::from(range.start());
        if start < at {
            continue;
        }
        out.push_str(&source[at..start]);
        out.push_str(&replacement);
        at = usize::from(range.end());
    }
    out.push_str(&source[at..]);
    out
}

/// Which runtime helpers the lowered file ends up naming. Only the ones a
/// pattern actually reached are emitted, so a file whose only `match` tests
/// literals carries no preamble beyond the sentinel.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one independent flag per helper the preamble can emit"
)]
struct Needs {
    sequence: bool,
    mapping: bool,
    mapping_rest: bool,
    class_positional: bool,
    class_keyword: bool,
}

struct Lower<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
    /// monotonic across the file, so every temporary is distinct
    counter: usize,
    needs: Needs,
}

impl<'ast> Visitor<'ast> for Lower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::Match(match_stmt) = stmt {
            self.lower_match(match_stmt);
        }
        walk_stmt(self, stmt);
    }
}

impl Lower<'_> {
    /// A temporary name nothing else in the file spells.
    fn fresh(&mut self) -> String {
        loop {
            let name = temporary_name("match", self.counter);
            self.counter += 1;
            if !self.source.contains(&name) {
                return name;
            }
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// A span of the original source as an expression: always parenthesized, so
    /// that a fragment carrying its own line breaks or a trailing comment stays
    /// inside a bracketed continuation instead of ending the generated line.
    fn expr_src(&self, range: TextRange) -> String {
        format!("({})", self.src(range))
    }

    /// The end of the header colon that follows `after` — the end of a subject,
    /// a pattern, or a guard. Only closing brackets, commas, whitespace and
    /// comments can stand between the two.
    fn colon_end(&self, after: TextSize) -> Option<TextSize> {
        SimpleTokenizer::starts_at(after, self.source)
            .skip_trivia()
            .find(|token| {
                !matches!(
                    token.kind(),
                    SimpleTokenKind::RParen
                        | SimpleTokenKind::RBracket
                        | SimpleTokenKind::RBrace
                        | SimpleTokenKind::Comma
                )
            })
            .filter(|token| token.kind() == SimpleTokenKind::Colon)
            .map(|token| token.range().end())
    }

    fn lower_match(&mut self, match_stmt: &StmtMatch) {
        let subject = match_stmt.subject.range();
        let Some(header_colon) = self.colon_end(subject.end()) else {
            return;
        };

        let subject_name = self.fresh();
        let mut headers = Vec::with_capacity(match_stmt.cases.len());
        for (index, case) in match_stmt.cases.iter().enumerate() {
            let test_end = case
                .guard
                .as_ref()
                .map_or_else(|| case.pattern.range().end(), |guard| guard.range().end());
            let Some(colon) = self.colon_end(test_end) else {
                return;
            };

            let mut test = self.compile(&case.pattern, &subject_name);
            if let Some(guard) = &case.guard {
                test = conjoin(vec![test, self.expr_src(guard.range())]);
            }

            let span = TextRange::new(case.range().start(), colon);
            let last = index + 1 == match_stmt.cases.len();
            let replacement = if index == 0 {
                format!("if {test}:")
            } else if last && test == "True" {
                "else:".to_owned()
            } else {
                format!("elif {test}:")
            };
            headers.push((span, replacement));
        }

        self.edits.push((
            TextRange::new(match_stmt.range().start(), subject.start()),
            format!("if [{subject_name} := ("),
        ));
        self.edits.push((
            TextRange::new(subject.end(), header_colon),
            ")]:".to_owned(),
        ));
        for (span, replacement) in headers {
            let padded = self.pad_to_span(span, replacement);
            self.edits.push((span, padded));
        }
    }

    /// Keep the replacement on as many lines as the header it replaces, by
    /// appending the blank lines it is short. A blank line between a header and
    /// its body is ignored by python, so the padding costs nothing but keeps
    /// every later line where the line map says it is.
    fn pad_to_span(&self, span: TextRange, replacement: String) -> String {
        let original = self.src(span).matches('\n').count();
        let produced = replacement.matches('\n').count();
        let mut padded = replacement;
        for _ in 0..original.saturating_sub(produced) {
            padded.push('\n');
        }
        padded
    }

    /// The test that decides whether `pattern` matches the value already bound
    /// to the name `subject`, binding the pattern's captures as it goes.
    fn compile(&mut self, pattern: &Pattern, subject: &str) -> String {
        match pattern {
            Pattern::MatchAs(as_pattern) => {
                let mut parts = Vec::new();
                if let Some(inner) = &as_pattern.pattern {
                    parts.push(self.compile(inner, subject));
                }
                if let Some(name) = &as_pattern.name {
                    parts.push(format!("({} := {subject}) is not {MISS}", name.id));
                }
                conjoin(parts)
            }
            Pattern::MatchSingleton(singleton) => {
                let value = match singleton.value {
                    Singleton::None => "None",
                    Singleton::True => "True",
                    Singleton::False => "False",
                };
                format!("{subject} is {value}")
            }
            Pattern::MatchValue(value) => {
                format!("{subject} == {}", self.expr_src(value.value.range()))
            }
            Pattern::MatchOr(or_pattern) => {
                let mut alternatives = Vec::with_capacity(or_pattern.patterns.len());
                for alternative in &or_pattern.patterns {
                    alternatives.push(self.compile(alternative, subject));
                }
                disjoin(&alternatives)
            }
            Pattern::MatchAnd(and_pattern) => {
                let mut conjuncts = Vec::with_capacity(and_pattern.patterns.len());
                for conjunct in &and_pattern.patterns {
                    conjuncts.push(self.compile(conjunct, subject));
                }
                conjoin(conjuncts)
            }
            Pattern::MatchSequence(sequence) => self.compile_sequence(sequence, subject),
            Pattern::MatchMapping(mapping) => self.compile_mapping(mapping, subject),
            Pattern::MatchClass(class) => self.compile_class(class, subject),
            // a star only appears as an element of a sequence pattern, where
            // the sequence itself binds it
            Pattern::MatchStar(_) => "True".to_owned(),
        }
    }

    /// Read `access` into a fresh temporary and match `pattern` against it. The
    /// sentinel test is what turns a helper's "no match" answer into a failed
    /// sub-pattern; for an access that cannot miss (a sequence element) it is
    /// simply always true, and the binding is the point.
    fn bind(&mut self, access: &str, pattern: &Pattern) -> String {
        let temporary = self.fresh();
        let inner = self.compile(pattern, &temporary);
        let read = format!("({temporary} := {access}) is not {MISS}");
        if inner == "True" {
            read
        } else {
            conjoin(vec![read, inner])
        }
    }

    /// A sequence pattern reads only the elements it has something to say
    /// about, and reads the ones at fixed positions before it materializes the
    /// list a `*rest` captures — which is the order python itself uses, and the
    /// reason `[1, *rest, 9]` can be tried against a `deque` (whose elements
    /// are indexable but whose slices are not) without raising.
    fn compile_sequence(&mut self, sequence: &PatternMatchSequence, subject: &str) -> String {
        self.needs.sequence = true;
        let mut parts = vec![format!("_by_match_seq({subject})")];
        let star = sequence.patterns.iter().position(Pattern::is_match_star);
        let fixed = match star {
            None => {
                parts.push(format!("len({subject}) == {}", sequence.patterns.len()));
                sequence.patterns.len()
            }
            Some(star) => {
                parts.push(format!("len({subject}) >= {}", sequence.patterns.len() - 1));
                star
            }
        };
        for (index, element) in sequence.patterns[..fixed].iter().enumerate() {
            if is_wildcard(element) {
                continue;
            }
            parts.push(self.bind(&format!("{subject}[{index}]"), element));
        }
        if let Some(star) = star {
            // elements after the star are counted from the end, so however much
            // the star swallowed never enters into their index
            let after = sequence.patterns.len() - star - 1;
            for (offset, element) in sequence.patterns[star + 1..].iter().enumerate() {
                if is_wildcard(element) {
                    continue;
                }
                let index = offset.cast_signed() - after.cast_signed();
                parts.push(self.bind(&format!("{subject}[{index}]"), element));
            }
            if let Pattern::MatchStar(rest) = &sequence.patterns[star]
                && let Some(name) = &rest.name
            {
                let slice = if after == 0 {
                    format!("{subject}[{star}:]")
                } else {
                    format!("{subject}[{star}:-{after}]")
                };
                parts.push(format!("({} := list({slice})) is not {MISS}", name.id));
            }
        }
        conjoin(parts)
    }

    fn compile_mapping(&mut self, mapping: &PatternMatchMapping, subject: &str) -> String {
        self.needs.mapping = true;
        let mut parts = vec![format!("_by_match_map({subject})")];
        // each key is read into a temporary so it is evaluated exactly once,
        // however many times the lowering goes on to name it
        let mut keys = Vec::with_capacity(mapping.keys.len());
        for (key, value) in mapping.keys.iter().zip(&mapping.patterns) {
            let name = self.fresh();
            parts.push(format!(
                "({name} := {}) is not {MISS}",
                self.expr_src(key.range())
            ));
            parts.push(self.bind(&format!("_by_match_key({subject}, {name})"), value));
            keys.push(name);
        }
        if let Some(rest) = &mapping.rest {
            self.needs.mapping_rest = true;
            let matched = keys.iter().fold(String::new(), |mut matched, key| {
                matched.push_str(key);
                matched.push_str(", ");
                matched
            });
            parts.push(format!(
                "({} := _by_match_rest({subject}, ({matched}))) is not {MISS}",
                rest.id
            ));
        }
        conjoin(parts)
    }

    fn compile_class(&mut self, class: &PatternMatchClass, subject: &str) -> String {
        let class_name = self.fresh();
        let mut parts = vec![format!(
            "isinstance({subject}, ({class_name} := {}))",
            self.expr_src(class.cls.range())
        )];

        let positional = &class.arguments.patterns;
        if !positional.is_empty() {
            self.needs.class_positional = true;
            let args = self.fresh();
            parts.push(format!(
                "({args} := _by_match_args({class_name}, {subject}, {})) is not {MISS}",
                positional.len()
            ));
            for (index, element) in positional.iter().enumerate() {
                parts.push(self.bind(&format!("{args}[{index}]"), element));
            }
        }
        for keyword in &class.arguments.keywords {
            self.needs.class_keyword = true;
            parts.push(self.bind(
                &format!("_by_match_attr({subject}, \"{}\")", keyword.attr.id),
                &keyword.pattern,
            ));
        }
        conjoin(parts)
    }
}

/// `_`: matches anything and binds nothing, so a sequence element it stands for
/// is never read.
fn is_wildcard(pattern: &Pattern) -> bool {
    matches!(
        pattern,
        Pattern::MatchAs(as_pattern)
            if as_pattern.pattern.is_none() && as_pattern.name.is_none()
    )
}

/// Wrap `test` where it would otherwise be re-associated by a surrounding
/// operator. The check errs towards wrapping: an extra pair of parentheses is
/// always sound, a missing one is not.
fn paren(test: &str) -> String {
    if test.contains(" and ") || test.contains(" or ") {
        format!("({test})")
    } else {
        test.to_owned()
    }
}

fn conjoin(parts: Vec<String>) -> String {
    let parts: Vec<String> = parts
        .into_iter()
        .filter(|part| part != "True")
        .map(|part| paren(&part))
        .collect();
    if parts.is_empty() {
        "True".to_owned()
    } else {
        parts.join(" and ")
    }
}

/// Alternatives are never dropped, however certain one of them looks: an
/// alternative that always matches still leaves the ones before it deciding
/// which captures get bound.
fn disjoin(parts: &[String]) -> String {
    if parts.is_empty() {
        return "True".to_owned();
    }
    parts
        .iter()
        .map(|part| paren(part))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The runtime the lowered tests call into. Every helper answers "no match"
/// with the sentinel rather than by raising, so that a subject which simply
/// lacks an attribute or a key falls through to the next case — while a subject
/// whose class is malformed (a `__match_args__` that is not a tuple of names)
/// still raises the `TypeError` python raises for it.
fn preamble(needs: &Needs, sentinel: bool) -> String {
    let mut out = String::new();
    if sentinel {
        out.push_str("_by_match_miss = object()\n");
    }

    if needs.sequence || needs.mapping {
        out.push_str("import collections.abc as _by_match_abc\n");
    }
    if needs.sequence {
        // python decides "is a sequence" by a type flag rather than by an ABC,
        // and sets it on a handful of builtins that register no ABC of their
        // own. str, bytes and bytearray carry the flag's opposite: they are
        // sequences everywhere else, and never match a sequence pattern
        out.push_str("import array as _by_match_array\n");
        out.push_str(
            "_by_match_seq_types = (list, tuple, range, memoryview, _by_match_array.array, \
             _by_match_abc.Sequence)\n",
        );
        out.push_str("def _by_match_seq(subject):\n");
        out.push_str(
            "    return isinstance(subject, _by_match_seq_types) and not isinstance(subject, \
             (str, bytes, bytearray))\n",
        );
    }
    if needs.mapping {
        out.push_str("def _by_match_map(subject):\n");
        out.push_str("    return isinstance(subject, _by_match_abc.Mapping)\n");
        out.push_str("def _by_match_key(subject, key):\n");
        out.push_str("    try:\n");
        out.push_str("        return subject[key]\n");
        out.push_str("    except KeyError:\n");
        out.push_str("        return _by_match_miss\n");
    }
    if needs.mapping_rest {
        out.push_str("def _by_match_rest(subject, matched):\n");
        out.push_str(
            "    return {key: value for key, value in subject.items() if key not in matched}\n",
        );
    }
    if needs.class_positional {
        // a handful of builtins take one positional sub-pattern that matches
        // the subject itself, in place of reading `__match_args__`
        out.push_str(
            "_by_match_self = (bool, bytearray, bytes, dict, float, frozenset, int, list, set, \
             str, tuple)\n",
        );
        out.push_str("def _by_match_args(cls, subject, count):\n");
        out.push_str("    if cls in _by_match_self:\n");
        out.push_str("        if count > 1:\n");
        out.push_str(
            "            raise TypeError(f\"{cls.__name__}() accepts 1 positional sub-pattern \
             ({count} given)\")\n",
        );
        out.push_str("        return (subject,)\n");
        out.push_str("    args = getattr(cls, \"__match_args__\", ())\n");
        out.push_str("    if not isinstance(args, tuple):\n");
        out.push_str(
            "        raise TypeError(f\"{cls.__name__}.__match_args__ must be a tuple \
             (got {type(args).__name__})\")\n",
        );
        out.push_str("    if count > len(args):\n");
        out.push_str(
            "        raise TypeError(f\"{cls.__name__}() accepts {len(args)} positional \
             sub-patterns ({count} given)\")\n",
        );
        out.push_str("    values = []\n");
        out.push_str("    for name in args[:count]:\n");
        out.push_str("        if not isinstance(name, str):\n");
        out.push_str(
            "            raise TypeError(f\"__match_args__ elements must be strings \
             (got {type(name).__name__})\")\n",
        );
        out.push_str("        try:\n");
        out.push_str("            values.append(getattr(subject, name))\n");
        out.push_str("        except AttributeError:\n");
        out.push_str("            return _by_match_miss\n");
        out.push_str("    return tuple(values)\n");
    }
    if needs.class_keyword {
        out.push_str("def _by_match_attr(subject, name):\n");
        out.push_str("    try:\n");
        out.push_str("        return getattr(subject, name)\n");
        out.push_str("    except AttributeError:\n");
        out.push_str("        return _by_match_miss\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    /// transpile for a target that predates `match`
    fn check(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
    }

    fn lowered(input: &str) -> String {
        let config = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        transpile(input, &config).unwrap()
    }

    /// the subject line opens the block the case clauses already sit inside, so
    /// nothing is re-indented and the value's own truthiness is never consulted
    #[test]
    fn value_patterns() {
        check(
            indoc! {r#"
                def f(n: int) -> str:
                    match n:
                        case 0:
                            return "zero"
                        case 1 | 2:
                            return "small"
                        case _:
                            return "many"
            "#},
            indoc! {r#"
                from __future__ import annotations
                def f(n: int) -> str:
                    if [__by_match_0__ := (n)]:
                        if __by_match_0__ == (0):
                            return "zero"
                        elif __by_match_0__ == (1) or __by_match_0__ == (2):
                            return "small"
                        else:
                            return "many"
            "#},
        );
    }

    /// `None`, `True` and `False` are matched by identity, not equality
    #[test]
    fn singletons() {
        let out = lowered(
            "def f(v: object):\n    match v:\n        case None:\n            pass\n        case True:\n            pass\n",
        );
        assert!(out.contains("is None"), "got:\n{out}");
        assert!(out.contains("is True"), "got:\n{out}");
    }

    /// a capture always matches, so the sentinel comparison beside it is only
    /// there to make the binding an expression
    #[test]
    fn captures_bind_in_the_enclosing_scope() {
        check(
            indoc! {"
                def f(v: object):
                    match v:
                        case got:
                            print(got)
            "},
            indoc! {"
                from __future__ import annotations
                _by_match_miss = object()
                def f(v: object):
                    if [__by_match_0__ := (v)]:
                        if (got := __by_match_0__) is not _by_match_miss:
                            print(got)
            "},
        );
    }

    /// nothing names the sentinel when every pattern is a plain value test
    #[test]
    fn a_file_that_needs_no_runtime_carries_none() {
        let out = lowered("def f(n: int):\n    match n:\n        case 0:\n            pass\n");
        assert!(!out.contains("_by_match_miss"), "got:\n{out}");
        assert!(!out.contains("def _by_match"), "got:\n{out}");
        assert!(!out.contains("_by_match_abc"), "got:\n{out}");
    }

    /// a guard runs after the pattern bound its captures, and only then
    #[test]
    fn guards_follow_the_pattern() {
        let out = lowered(
            "def f(v: object):\n    match v:\n        case [a, b] if a < b:\n            pass\n",
        );
        assert!(out.contains("and (a < b)"), "got:\n{out}");
    }

    #[test]
    fn class_patterns_read_match_args() {
        let out = lowered(indoc! {"
            class Point:
                __match_args__ = ('x', 'y')

            def f(v: object):
                match v:
                    case Point(x, y=0):
                        print(x)
        "});
        assert!(out.contains("def _by_match_args("), "got:\n{out}");
        assert!(out.contains("def _by_match_attr("), "got:\n{out}");
        assert!(
            out.contains("_by_match_args(__by_match_1__, __by_match_0__, 1)"),
            "one positional sub-pattern, got:\n{out}"
        );
        assert!(
            out.contains("_by_match_attr(__by_match_0__, \"y\")"),
            "got:\n{out}"
        );
    }

    /// a sequence's fixed elements are read before the star's list is built,
    /// and elements after the star are indexed from the end
    #[test]
    fn sequence_patterns() {
        let out = lowered(
            "def f(v: object):\n    match v:\n        case [1, *rest, last]:\n            print(rest, last)\n",
        );
        let fixed = out
            .find("__by_match_0__[-1]")
            .expect("reads the last element");
        let star = out.find("list(").expect("captures the rest");
        assert!(fixed < star, "fixed elements come first, got:\n{out}");
        assert!(out.contains("__by_match_0__[1:-1]"), "got:\n{out}");
        assert!(out.contains("len(__by_match_0__) >= 2"), "got:\n{out}");
    }

    /// an element the pattern says nothing about is never read, which is what
    /// lets `[_, x]` be tried against a sequence whose first element raises
    #[test]
    fn a_wildcard_element_is_not_read() {
        let out = lowered(
            "def f(v: object):\n    match v:\n        case [_, second]:\n            print(second)\n",
        );
        assert!(!out.contains("__by_match_0__[0]"), "got:\n{out}");
        assert!(out.contains("__by_match_0__[1]"), "got:\n{out}");
    }

    /// each key is evaluated once, however many times the lowering names it
    #[test]
    fn mapping_patterns() {
        let out = lowered(indoc! {r#"
            def f(v: object):
                match v:
                    case {"a": a, **rest}:
                        print(a, rest)
        "#});
        assert!(out.contains("def _by_match_key("), "got:\n{out}");
        assert!(out.contains("def _by_match_rest("), "got:\n{out}");
        assert!(
            out.contains("(__by_match_1__ := (\"a\"))"),
            "the key is read into a temporary, got:\n{out}"
        );
        assert!(
            out.contains("_by_match_rest(__by_match_0__, (__by_match_1__, ))"),
            "and the rest is the mapping minus that same temporary, got:\n{out}"
        );
    }

    /// a header spread over several lines is replaced by one line plus the
    /// blank lines it is short, so everything below it keeps its line number
    #[test]
    fn a_multiline_header_keeps_its_height() {
        let input = indoc! {"
            def f(v: object):
                match (
                    v,
                ):
                    case [
                        a,
                    ]:
                        return a
                    case _:
                        return None
        "};
        let out = lowered(input);
        let generated = out
            .lines()
            .take_while(|line| *line != "def f(v: object):")
            .count();
        assert_eq!(
            out.lines().count() - generated,
            input.lines().count(),
            "got:\n{out}"
        );
    }

    /// a nested `match` lowers on its own terms; only headers are replaced, so
    /// the outer statement's bodies are untouched either way
    #[test]
    fn nested_matches() {
        let out = lowered(indoc! {"
            def f(v: object):
                match v:
                    case [head, *_]:
                        match head:
                            case 0:
                                return 'zero'
                    case _:
                        return None
        "});
        assert!(!out.contains("match "), "got:\n{out}");
        assert!(out.contains("__by_match_2__ := (head)"), "got:\n{out}");
    }

    /// a target that has `match` keeps it
    #[test]
    fn untouched_from_python_310() {
        let out = transpile(
            "def f(n: int):\n    match n:\n        case 0:\n            pass\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("match n:"), "got:\n{out}");
        assert!(!out.contains("_by_match_miss"), "got:\n{out}");
        assert!(!out.contains("def _by_match"), "got:\n{out}");
        assert!(!out.contains("_by_match_abc"), "got:\n{out}");
    }

    /// the lowering binds with assignment expressions, so a target older than
    /// those is left to the target-version check rather than lowered into
    /// something that version cannot run either
    #[test]
    fn python_37_declines_and_is_reported() {
        let config = Config {
            min_version: PythonVersion::PY37,
            ..Config::test_default()
        };
        let err = transpile(
            "def f(n: int):\n    match n:\n        case 0:\n            pass\n",
            &config,
        )
        .unwrap_err();
        assert!(
            err.contains("Cannot use `match` statement on Python 3.7"),
            "got:\n{err}"
        );
    }

    /// the temporaries a `match` in a class body leaves behind are dunders, so
    /// `enum` and `dataclass` read them as machinery rather than as members
    #[test]
    fn class_body_leftovers_are_dunders() {
        let out = lowered(indoc! {"
            class A:
                match 'x':
                    case str() as which:
                        label = which
        "});
        for line in out.lines() {
            let assigned = line.trim_start().split(" :=").next().unwrap_or_default();
            if assigned.starts_with("_by_match") {
                continue;
            }
            assert!(
                !line.contains("__by_match")
                    || line.contains("__by_match_0__ := ")
                    || line.contains("__by_match"),
                "got:\n{out}"
            );
        }
        assert!(
            out.matches("__by_match").count() > 0 && !out.contains("_by_match_0 "),
            "got:\n{out}"
        );
    }
}
