//! an f-string `=` field prints what the *author* wrote, not what it lowered to
//!
//! python renders `f"{expr = }"` as the source text between the `{` and the `=`,
//! followed by the value:
//!
//! ```python
//! a = 3
//! f"{(a) = }"   # '(a) = 3' — the parentheses and the spaces are the author's
//! ```
//!
//! so every lowering that rewrites an expression inside such a field used to print
//! itself. `f"{(a!) = }"` came out as `(_force_unwrap(a)) = 3`, and the same held for
//! `??`, `?.`, a conversion, an extension call, a grapheme accessor and `typeof`
//!
//! there is no python spelling of "print this text, evaluate that expression" inside
//! one field, so a field whose expression lowers is taken apart: the author's text
//! becomes literal text of the string around it, and the lowered expression an
//! ordinary replacement field beside it. `f"{(a!) = }"` lowers to
//! `f"(a!) = {(_force_unwrap(a))!r}"`, which is what python prints for the source the
//! author wrote. the `!r` is the conversion python applies to a `=` field that names
//! none of its own
//!
//! a field nothing lowered is left exactly as written, so a `.py` file — and every
//! `f"{x = }"` in a `.by` one — still passes through untouched
//!
//! the taking-apart happens twice over, because a lowering reaches the output two
//! ways. a `TypeAwarePass` emits a text edit keyed on the expression's source range,
//! and [`claim_lowered`] answers those with an edit of its own over the whole field.
//! an `AstPass` instead replaces the expression node, and the statement around it is
//! re-rendered from the syntax tree — so [`rewrite_changed`] answers those in the
//! tree, where the re-render will read them
//!
//! "the expression lowers" is read off the edits, and a lowering whose python spelling
//! is the one the author already wrote still counts — a type test against `None` on an
//! optional is written `a is not None` either way. such a field is taken apart for no
//! gain: the text printed is the same either way, so the answer is right and only the
//! output is longer than it needed to be
//!
//! the reverse direction goes the other way and leaves these fields alone entirely, for
//! the same reason the forward one takes them apart — see [`printed_expressions`]

use std::cell::RefCell;
use std::collections::HashMap;

use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::visitor::transformer::{self, Transformer};
use ruff_python_ast::{
    AnyStringFlags, AtomicNodeIndex, ConversionFlag, Expr, FString, InterpolatedElement,
    InterpolatedStringElement, InterpolatedStringElements, InterpolatedStringLiteralElement,
    ModModule, Stmt, StringFlags, TString,
};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::Fragment;

/// the conversion python applies to a `=` field once the field is taken apart
///
/// a `=` field that names no conversion of its own prints the value's `repr`, unless it
/// carries a format spec: `f"{a=}"` is `f"a={a!r}"`, and `f"{a=:>6}"` is `f"a={a:>6}"`
fn conversion_of(part: &InterpolatedElement) -> ConversionFlag {
    match part.conversion {
        ConversionFlag::None if part.format_spec.is_none() => ConversionFlag::Repr,
        named => named,
    }
}

/// whether the caller writes the literal text into the output itself, or hands it to the
/// code generator
#[derive(Clone, Copy, PartialEq, Eq)]
enum Braces {
    /// the text goes straight into the string, where a brace of its own would open a
    /// replacement field
    Double,
    /// the text goes into the syntax tree, and the code generator doubles a brace when it
    /// prints the literal element
    Leave,
}

/// the author's text written as literal text of a string with these flags
///
/// the text moves out of the replacement field and into the string around it, where a
/// backslash starts an escape sequence and the string's own quote ends it — neither of
/// which it meant inside the field. `None` where the string's flags have no spelling for
/// it at all: a raw string escapes nothing, so it can hold neither its own quote nor, on
/// one line, a line break
fn as_literal_text(text: &str, flags: AnyStringFlags, braces: Braces) -> Option<String> {
    let raw = flags.prefix().is_raw();
    let quote = flags.quote_style().as_char();
    let triple = flags.is_triple_quoted();
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '{' | '}' if braces == Braces::Double => {
                out.push(ch);
                out.push(ch);
            }
            '\\' if !raw => out.push_str("\\\\"),
            '\n' if !raw => out.push_str("\\n"),
            '\r' if !raw => out.push_str("\\r"),
            _ if ch == quote && !raw => {
                out.push('\\');
                out.push(quote);
            }
            // a raw string holds every byte it is given literally, which is what makes it
            // faithful here and also what makes these two impossible in it
            _ if ch == quote => return None,
            '\n' | '\r' if !triple => return None,
            _ => out.push(ch),
        }
    }
    Some(out)
}

/// the message for a field whose author text the string it sits in cannot spell
fn cannot_spell(text: &str) -> String {
    format!(
        "this f-string's `=` field prints `{text}`, the source text between its braces, and a \
         lowering rewrote the expression there — so the text has to be written beside the \
         field rather than inside it, which this string's own quotes cannot hold. Use a \
         different quote style, or drop the `r` prefix"
    )
}

/// every replacement field of `elements`, its format specs included, with the flags of the
/// string it belongs to
fn fields_of<'a>(
    elements: &'a InterpolatedStringElements,
    flags: AnyStringFlags,
    out: &mut Vec<(&'a InterpolatedElement, AnyStringFlags)>,
) {
    for element in elements {
        if let InterpolatedStringElement::Interpolation(part) = element {
            out.push((part, flags));
            if let Some(spec) = &part.format_spec {
                fields_of(&spec.elements, flags, out);
            }
        }
    }
}

/// every replacement field written anywhere in `suite`
fn fields(suite: &[Stmt]) -> Vec<(&InterpolatedElement, AnyStringFlags)> {
    struct Fields<'a>(Vec<(&'a InterpolatedElement, AnyStringFlags)>);

    impl<'a> SourceOrderVisitor<'a> for Fields<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::FString(fstring) => {
                    for part in fstring.value.f_strings() {
                        fields_of(&part.elements, part.flags.into(), &mut self.0);
                    }
                }
                Expr::TString(tstring) => {
                    for part in &tstring.value {
                        fields_of(&part.elements, part.flags.into(), &mut self.0);
                    }
                }
                _ => {}
            }
            source_order::walk_expr(self, expr);
        }
    }

    let mut found = Fields(Vec::new());
    for stmt in suite {
        source_order::walk_stmt(&mut found, stmt);
    }
    found.0
}

/// whether `edit` rewrites the expression of a field spanning `field`
///
/// a zero-width insertion counts at either end of the expression: a lowering that wraps an
/// expression inserts its opening text at the first byte and its closing text at the last.
/// an edit that covers the whole field, on the other hand, is a rewrite *of* the field —
/// an f-string in a type position replaced by the type it names, say — and leaves nothing
/// of the author's text to print
pub(super) fn rewrites_expression(
    edit: TextRange,
    expression: TextRange,
    field: TextRange,
) -> bool {
    if edit.contains_range(field) {
        return false;
    }
    if edit.is_empty() {
        expression.contains_inclusive(edit.start())
    } else {
        edit.start() < expression.end() && edit.end() > expression.start()
    }
}

/// the source span of every expression a `=` field prints as its own text
///
/// python renders `f"{expr = }"` by printing the text between the `{` and the `=`, so a
/// rewrite of the expression there changes what the program *prints*, not only how it is
/// written. the reverse direction leaves those spans alone for that reason: the `.by` it
/// writes has to print what the python it was given printed, and `f"{(a ?? b)=}"` prints
/// `(a ?? b)=` where `f"{(a if a is not None else b)=}"` prints the conditional
pub(crate) fn printed_expressions(suite: &[Stmt]) -> Vec<TextRange> {
    fields(suite)
        .into_iter()
        .filter_map(|(part, _)| part.debug_text.as_ref().map(|_| part.expression.range()))
        .collect()
}

/// take apart every `=` field an edit rewrites the expression of
///
/// `edited` is every range the passes claimed, and `claimed` every field
/// [`rewrite_changed`] already took apart in the syntax tree
pub(crate) fn claim_lowered(
    suite: &[Stmt],
    edited: &[TextRange],
    claimed: &[TextRange],
) -> (Vec<(TextRange, Vec<Fragment>)>, Vec<String>) {
    let mut edits = Vec::new();
    let mut errors = Vec::new();
    for (part, flags) in fields(suite) {
        let Some(debug) = &part.debug_text else {
            continue;
        };
        if claimed.contains(&part.range()) {
            continue;
        }
        let expression = part.expression.range();
        let field = part.range();
        if !edited
            .iter()
            .any(|edit| rewrites_expression(*edit, expression, field))
        {
            continue;
        }
        let Some(text) = as_literal_text(debug.as_str(), flags, Braces::Double) else {
            errors.push(cannot_spell(debug.as_str()));
            continue;
        };
        // the expression is parenthesised so that whatever it lowered to still reads as
        // the whole value of the field it moved into, ahead of the conversion
        let mut fragments = vec![
            Fragment::Lit(format!("{text}{{(")),
            Fragment::Src(expression),
        ];
        let mut tail = String::from(")");
        if let Some(conversion) = conversion_of(part).to_char() {
            tail.push('!');
            tail.push(conversion);
        }
        match &part.format_spec {
            Some(spec) => {
                tail.push(':');
                fragments.push(Fragment::Lit(tail));
                fragments.push(Fragment::Src(spec.range()));
                fragments.push(Fragment::Lit("}".to_owned()));
            }
            None => {
                tail.push('}');
                fragments.push(Fragment::Lit(tail));
            }
        }
        edits.push((part.range(), fragments));
    }
    (edits, errors)
}

/// take apart every `=` field whose expression an `AstPass` replaced
///
/// `original` is the module body as it parsed, which is what says whether a field's
/// expression is still the one the author wrote. answers with the fields it took apart, so
/// [`claim_lowered`] leaves them alone
pub(crate) fn rewrite_changed(
    module: &mut ModModule,
    original: &[Stmt],
    changed: &[usize],
) -> (Vec<TextRange>, Vec<String>) {
    // a field is matched to the one the author wrote by the range of the field itself,
    // which an `AstPass` replacing an expression *inside* it leaves alone. matching by
    // position in a walk instead would go wrong the moment a pass added or removed one
    let mut written: HashMap<TextRange, ComparableExpr<'_>> = HashMap::new();
    for &idx in changed {
        let Some(stmt) = original.get(idx) else {
            continue;
        };
        for (part, _) in fields(std::slice::from_ref(stmt)) {
            if part.debug_text.is_some() {
                written.insert(part.range(), ComparableExpr::from(&*part.expression));
            }
        }
    }

    let rewrite = Rewrite {
        written: &written,
        claimed: RefCell::new(Vec::new()),
        errors: RefCell::new(Vec::new()),
    };
    for &idx in changed {
        if let Some(stmt) = module.body.get_mut(idx) {
            rewrite.visit_stmt(stmt);
        }
    }
    (rewrite.claimed.into_inner(), rewrite.errors.into_inner())
}

struct Rewrite<'a> {
    written: &'a HashMap<TextRange, ComparableExpr<'a>>,
    claimed: RefCell<Vec<TextRange>>,
    errors: RefCell<Vec<String>>,
}

impl Rewrite<'_> {
    /// whether this field still prints the expression the author wrote under it
    fn lowered(&self, part: &InterpolatedElement) -> bool {
        match self.written.get(&part.range()) {
            Some(written) => *written != ComparableExpr::from(&*part.expression),
            // a field the original has none of at this range is one a pass built itself,
            // and its text is the pass's own to get right
            None => false,
        }
    }

    fn rebuild(&self, elements: &mut InterpolatedStringElements, flags: AnyStringFlags) {
        if !elements
            .interpolations()
            .any(|part| part.debug_text.is_some() && self.lowered(part))
        {
            return;
        }
        let mut rebuilt: Vec<InterpolatedStringElement> = Vec::with_capacity(elements.len() + 1);
        for element in elements.iter() {
            match element {
                InterpolatedStringElement::Interpolation(part)
                    if let Some(debug) = &part.debug_text
                        && self.lowered(part) =>
                {
                    // the code generator doubles a brace when it prints a literal element,
                    // so only what it does not know about is escaped here
                    let Some(text) = as_literal_text(debug.as_str(), flags, Braces::Leave) else {
                        self.errors.borrow_mut().push(cannot_spell(debug.as_str()));
                        rebuilt.push(element.clone());
                        continue;
                    };
                    self.claimed.borrow_mut().push(part.range());
                    rebuilt.push(InterpolatedStringElement::Literal(
                        InterpolatedStringLiteralElement {
                            range: TextRange::default(),
                            node_index: AtomicNodeIndex::NONE,
                            value: text.into(),
                        },
                    ));
                    let mut moved = part.clone();
                    moved.conversion = conversion_of(part);
                    moved.debug_text = None;
                    rebuilt.push(InterpolatedStringElement::Interpolation(moved));
                }
                kept => rebuilt.push(kept.clone()),
            }
        }
        *elements = rebuilt.into();
    }
}

impl Transformer for Rewrite<'_> {
    fn visit_f_string(&self, f_string: &mut FString) {
        transformer::walk_f_string(self, f_string);
        self.rebuild(&mut f_string.elements, f_string.flags.into());
    }

    fn visit_t_string(&self, t_string: &mut TString) {
        transformer::walk_t_string(self, t_string);
        self.rebuild(&mut t_string.elements, t_string.flags.into());
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    /// the one line of the output the source's last line lowered to
    fn lowered(source: &str) -> String {
        lowered_with(source, &Config::test_default())
    }

    /// the same, for a source whose replacement fields need what 3.12 added to them — a
    /// backslash, and the string's own quote nested inside a field
    fn lowered_at_312(source: &str) -> String {
        lowered_with(
            source,
            &Config {
                min_version: crate::PythonVersion::PY312,
                ..Config::test_default()
            },
        )
    }

    fn lowered_with(source: &str, config: &Config) -> String {
        let out = transpile(source, config).unwrap_or_else(|error| panic!("{source:?}: {error}"));
        out.lines()
            .last()
            .expect("some output")
            .trim_start()
            .to_owned()
    }

    /// every operator lowering that can reach inside a `=` field leaves the author's own
    /// text in front of it, and puts what it lowered to in a field of its own
    #[test]
    fn an_operator_lowering_keeps_the_field_text() {
        // a force-unwrap
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{(a!) = }\")\n"),
            "print(f\"(a!) = {(_force_unwrap(a))!r}\")"
        );
        // `??`
        assert_eq!(
            lowered("def go(a: int?, b: int) -> None:\n    print(f\"{a ?? b = }\")\n"),
            "print(f\"a ?? b = {(a if a is not None else b)!r}\")"
        );
        // an optional chain
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{a?.bit_length() = }\")\n"),
            "print(f\"a?.bit_length() = {((None if a is None else a.bit_length()))!r}\")"
        );
        // a grapheme accessor, which re-emits its receiver
        assert_eq!(
            lowered("def go(s: str) -> None:\n    print(f\"{s.first = }\")\n"),
            "print(f\"s.first = {((Character(_by_graphemes(s)[0]) if s else None))!r}\")"
        );
    }

    /// `typeof` is folded in the syntax tree rather than emitted as an edit, so the
    /// statement around it is re-rendered — the other half of the rewrite
    #[test]
    fn a_syntax_tree_fold_keeps_the_field_text() {
        assert_eq!(
            lowered("def go(a: int) -> None:\n    print(f\"{typeof(a) = }\")\n"),
            "print(f\"typeof(a) = {TypeOf[a]!r}\")"
        );
    }

    /// a soundness check inside a `=` field of a statement a syntax-tree fold already
    /// rewrote is written into the tree last of all, so the field is taken apart after
    /// that rather than before it
    #[test]
    fn a_check_written_into_a_re_rendered_statement_keeps_the_field_text() {
        let checked = Config {
            soundness: crate::config::SoundnessPositions::defaults(),
            ..Config::test_default()
        };
        let out = lowered_with(
            "def go(d: dict[str, int]) -> None:\n    print(f\"{d.get('k') = }\", typeof(d))\n",
            &checked,
        );
        assert_eq!(
            out,
            "print(f\"d.get('k') = {_soundness_check(d.get('k'), (int, type(None)))!r}\", \
             TypeOf[d])"
        );
    }

    /// an extension call and a conversion are resolved from the checker's answer rather
    /// than from the syntax, and reach the field the same way
    #[test]
    fn a_type_directed_rewrite_keeps_the_field_text() {
        assert_eq!(
            lowered(
                "extension list:\n    def second(self) -> Element:\n        return self[1]\n\nxs = [1, 2, 3]\nprint(f\"{xs.second() = }\")\n"
            ),
            "print(f\"xs.second() = {(_by_ext__list__second(xs))!r}\")"
        );
        // a conversion fires where the argument is written, which here is inside the field
        assert_eq!(
            lowered(
                "class Meters:\n    def __init__(self, value: float):\n        self.value = value\n\n    @classmethod\n    def __of__(cls, value: int) -> Self:\n        return cls(float(value))\n\ndef take(m: Meters) -> int:\n    return 1\n\nprint(f\"{take(3) = }\")\n"
            ),
            "print(f\"take(3) = {(take(Meters.__of__(3)))!r}\")"
        );
    }

    /// the conversion python applies to a `=` field is carried over: `repr` where the
    /// field names none and has no format spec of its own, and the field's own otherwise
    #[test]
    fn the_field_keeps_the_conversion_python_would_apply() {
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{(a!)=!s}\")\n"),
            "print(f\"(a!)={(_force_unwrap(a))!s}\")"
        );
        // a format spec stands in for the repr, exactly as in python
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{(a!)=:>6}\")\n"),
            "print(f\"(a!)={(_force_unwrap(a)):>6}\")"
        );
    }

    /// a brace in the author's text opens no field of its own once the text is literal,
    /// and a backslash starts no escape sequence — python prints both as written
    #[test]
    fn the_field_text_is_escaped_for_the_string_it_moves_into() {
        assert_eq!(
            lowered("def go(a: int?) -> None:\n    print(f\"{({1: a}[1]!) = }\")\n"),
            "print(f\"({{1: a}}[1]!) = {(_force_unwrap({1: a}[1]))!r}\")"
        );
        assert_eq!(
            lowered_at_312("def go() -> None:\n    print(f\"{('a\\n'!) = }\")\n"),
            "print(f\"('a\\\\n'!) = {(_force_unwrap('a\\n'))!r}\")"
        );
        // the string's own quote, which the field allowed and the literal text does not
        assert_eq!(
            lowered_at_312(
                "def go(d: dict[str, int]) -> None:\n    print(f\"{(d[\"k\"]!) = }\")\n"
            ),
            "print(f\"(d[\\\"k\\\"]!) = {(_force_unwrap(d[\"k\"]))!r}\")"
        );
    }

    /// a raw string escapes nothing, so it has no way to hold its own quote in literal
    /// text — the field is refused rather than emitted as something that would not parse
    #[test]
    fn a_raw_string_that_cannot_spell_its_own_text_is_refused() {
        let error = transpile(
            "def go(d: dict[str, int]) -> None:\n    print(rf\"{(d[\"k\"]!) = }\")\n",
            &Config::test_default(),
        )
        .expect_err("a raw f-string cannot hold its own quote in literal text");
        assert!(error.contains("`=` field"), "{error}");
        assert!(error.contains("quote"), "{error}");
    }

    /// the reverse direction leaves a `=` field's expression exactly as written. rewriting
    /// it would change what the program prints — `f"{(a ?? b)=}"` prints `(a ?? b)=` where
    /// the python it came from printed the conditional — so the `.by` written out keeps the
    /// text the python had
    #[test]
    fn the_reverse_direction_leaves_a_field_text_alone() {
        let out = crate::reverse_transpile(
            "def go(a: int | None, b: int) -> None:\n    print(f\"{(a if a is not None else b)=}\")\n",
            &Config::test_default(),
        )
        .expect("reverse should succeed");
        assert!(
            out.contains("f\"{(a if a is not None else b)=}\""),
            "got:\n{out}"
        );
        // the same expression outside a `=` field is rewritten as it always was
        let out = crate::reverse_transpile(
            "def go(a: int | None, b: int) -> None:\n    print(f\"{(a if a is not None else b)}\")\n",
            &Config::test_default(),
        )
        .expect("reverse should succeed");
        assert!(out.contains("f\"{(a ?? b)}\""), "got:\n{out}");
    }

    /// a lowering that writes back the text the author already wrote leaves the field
    /// alone. a type test against `None` on an optional is spelled `a is not None` either
    /// way, so taking the field apart would print the same characters out of a longer
    /// f-string — and a `.py` file's own `=` field stopped surviving a round trip through
    /// `.by` because of it
    #[test]
    fn a_lowering_that_writes_the_same_text_leaves_the_field_alone() {
        unchanged_by_round_trip(
            "def go(a: int | None, b: int) -> None:\n    print(f\"{(a if a is not None else b)=}\")\n",
        );
        assert_eq!(
            lowered(
                "def go(a: int?, b: int) -> None:\n    print(f\"{(a if a is not None else b)=}\")\n"
            ),
            "print(f\"{(a if a is not None else b)=}\")"
        );
        // the same test against a value that cannot be `None` folds to a constant, which
        // is text the author did not write — that field is still taken apart
        assert_eq!(
            lowered("def go(a: int) -> None:\n    print(f\"{(a is not None)=}\")\n"),
            "print(f\"(a is not None)={(True)!r}\")"
        );
    }

    /// `python` → `.by` → `python` gives back the file it started from
    fn unchanged_by_round_trip(source: &str) {
        let config = Config::test_default();
        let based = crate::reverse_transpile(source, &config).expect("reverse should succeed");
        let back = transpile(&based, &config).expect("forward should succeed");
        let tail = back
            .lines()
            .rev()
            .take(source.lines().count())
            .collect::<Vec<_>>();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        assert_eq!(tail, source.trim_end(), "via:\n{based}");
    }

    /// a field nothing lowered keeps python's own compact spelling, in a `.by` file as in
    /// a `.py` one
    #[test]
    fn an_unlowered_field_is_left_alone() {
        unchanged("x = 1\nprint(f\"{x = }\")\n");
        unchanged("x = 1\nprint(f\"{ {1: 2}[1] = }\")\n");
        assert_eq!(
            lowered("def go(a: int?, b: int) -> None:\n    print(f\"{a ?? b} {b = }\")\n"),
            "print(f\"{a if a is not None else b} {b = }\")"
        );
    }
}
