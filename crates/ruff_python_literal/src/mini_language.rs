//! the [format specification mini-language] as the standard `__format__`
//! implementations interpret it
//!
//! a spec is only meaningful against the implementation that reads it.
//! `str.__format__` rejects the `,` that `int.__format__` groups with, and a
//! type that writes its own `__format__` — `datetime` reading strftime codes —
//! shares none of these rules. so every rule here is keyed on a
//! [`FormatTarget`], one of the four standard implementations, and a caller
//! that cannot name the target must not apply them
//!
//! [format specification mini-language]: https://docs.python.org/3/library/string.html#format-specification-mini-language

use std::fmt::Write as _;
use std::ops::Range;

use crate::Case;
use crate::format::{FormatAlign, FormatGrouping, FormatSign, FormatSpecSpans, StaticFormatSpec};

/// which standard `__format__` implementation reads the spec
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FormatTarget {
    Str,
    Int,
    Float,
    Complex,
}

impl FormatTarget {
    /// the name this target reports in its own error messages
    pub fn type_name(self) -> &'static str {
        match self {
            FormatTarget::Str => "str",
            FormatTarget::Int => "int",
            FormatTarget::Float => "float",
            FormatTarget::Complex => "complex",
        }
    }

    /// the presentation types this target accepts, in documentation order, as
    /// `(code, a word or two for a list, the rest of the explanation)`
    pub fn presentation_types(self) -> &'static [(char, &'static str, &'static str)] {
        match self {
            FormatTarget::Str => &[('s', "string", "the only one a string has")],
            FormatTarget::Int => &[
                ('d', "decimal", "what an integer does with no type at all"),
                ('b', "binary", ""),
                ('o', "octal", ""),
                ('x', "hexadecimal", "lowercase"),
                ('X', "hexadecimal", "uppercase"),
                ('c', "character", "the character with this code point"),
                ('n', "decimal", "grouped for the current locale"),
                ('e', "scientific", "lowercase `e`"),
                ('E', "scientific", "uppercase `E`"),
                ('f', "fixed point", ""),
                ('F', "fixed point", "uppercase `INF` and `NAN`"),
                (
                    'g',
                    "general",
                    "fixed point or scientific, whichever is shorter",
                ),
                ('G', "general", "uppercase"),
                ('%', "percentage", "multiplied by 100, with a trailing `%`"),
            ],
            FormatTarget::Float => &[
                ('e', "scientific", "lowercase `e`"),
                ('E', "scientific", "uppercase `E`"),
                ('f', "fixed point", ""),
                ('F', "fixed point", "uppercase `INF` and `NAN`"),
                (
                    'g',
                    "general",
                    "fixed point or scientific, whichever is shorter",
                ),
                ('G', "general", "uppercase"),
                ('n', "general", "grouped for the current locale"),
                ('%', "percentage", "multiplied by 100, with a trailing `%`"),
            ],
            // a complex has no percentage form
            FormatTarget::Complex => &[
                ('e', "scientific", "lowercase `e`"),
                ('E', "scientific", "uppercase `E`"),
                ('f', "fixed point", ""),
                ('F', "fixed point", "uppercase `INF` and `NAN`"),
                (
                    'g',
                    "general",
                    "fixed point or scientific, whichever is shorter",
                ),
                ('G', "general", "uppercase"),
                ('n', "general", "grouped for the current locale"),
            ],
        }
    }

    /// how the value a preview is rendered from is written in source
    pub fn sample_source(self) -> &'static str {
        match self {
            FormatTarget::Str => "\"spam\"",
            FormatTarget::Int => "1234",
            FormatTarget::Float => "1234.5678",
            FormatTarget::Complex => "1.5 + 2.5j",
        }
    }

    /// the value a preview is rendered from
    fn sample(self) -> Sample {
        match self {
            FormatTarget::Str => Sample::Str("spam"),
            FormatTarget::Int => Sample::Int(1234),
            FormatTarget::Float => Sample::Float(1234.5678),
            FormatTarget::Complex => Sample::Complex(1.5, 2.5),
        }
    }
}

enum Sample {
    Str(&'static str),
    Int(i64),
    Float(f64),
    Complex(f64, f64),
}

/// one clause of a format spec
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FormatSpecComponent {
    Conversion,
    Fill,
    Align,
    Sign,
    AlternateForm,
    Zero,
    Width,
    Grouping,
    Precision,
    Type,
}

impl FormatSpecComponent {
    /// every component in the order it is written
    pub const ALL: [FormatSpecComponent; 10] = [
        FormatSpecComponent::Conversion,
        FormatSpecComponent::Fill,
        FormatSpecComponent::Align,
        FormatSpecComponent::Sign,
        FormatSpecComponent::AlternateForm,
        FormatSpecComponent::Zero,
        FormatSpecComponent::Width,
        FormatSpecComponent::Grouping,
        FormatSpecComponent::Precision,
        FormatSpecComponent::Type,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FormatSpecComponent::Conversion => "conversion",
            FormatSpecComponent::Fill => "fill",
            FormatSpecComponent::Align => "align",
            FormatSpecComponent::Sign => "sign",
            FormatSpecComponent::AlternateForm => "alternate form",
            FormatSpecComponent::Zero => "zero padding",
            FormatSpecComponent::Width => "width",
            FormatSpecComponent::Grouping => "grouping",
            FormatSpecComponent::Precision => "precision",
            FormatSpecComponent::Type => "presentation type",
        }
    }

    pub fn documentation(self) -> &'static str {
        match self {
            FormatSpecComponent::Conversion => {
                "applied before formatting: `!s` calls `str`, `!r` calls `repr`, `!a` calls `ascii`"
            }
            FormatSpecComponent::Fill => "the character the padding is made of",
            FormatSpecComponent::Align => {
                "where the value sits in its field: `<` left, `>` right, `^` centred, `=` after the sign"
            }
            FormatSpecComponent::Sign => {
                "`+` signs both, `-` only negatives, a space leaves a gap where a `+` would go"
            }
            FormatSpecComponent::AlternateForm => {
                "`#` writes the `0b`/`0o`/`0x` prefix, and keeps the decimal point on a float"
            }
            FormatSpecComponent::Zero => "pads with zeroes after the sign",
            FormatSpecComponent::Width => "the minimum number of characters",
            FormatSpecComponent::Grouping => {
                "`,` or `_` between digit groups — every three digits, or every four in base 2, 8 and 16"
            }
            FormatSpecComponent::Precision => {
                "digits after the point, significant digits, or the length a string is truncated to"
            }
            FormatSpecComponent::Type => "how the value is presented",
        }
    }

    /// the values this clause can be written as
    ///
    /// empty for the clauses whose value the author chooses rather than picks —
    /// a fill character, a width — and for the conversion, which sits outside
    /// the spec
    pub fn choices(self, target: Option<FormatTarget>) -> Vec<Choice> {
        let choice = |text: &str, summary: &'static str, detail: &'static str| Choice {
            insert: text.to_string(),
            summary,
            detail,
            component: self,
        };
        match self {
            FormatSpecComponent::Align => vec![
                choice("<", "left", ""),
                choice(">", "right", ""),
                choice("^", "centred", ""),
                choice(
                    "=",
                    "after the sign",
                    "padding goes between the sign and the digits",
                ),
            ],
            FormatSpecComponent::Sign => vec![
                choice(
                    "+",
                    "always signed",
                    "a `+` on positive as well as a `-` on negative",
                ),
                choice("-", "negative only", "the default"),
                choice(" ", "space for a sign", "a space where a `+` would go"),
            ],
            FormatSpecComponent::AlternateForm => vec![choice(
                "#",
                "base prefix",
                "writes `0b`/`0o`/`0x`, and keeps the decimal point on a float",
            )],
            FormatSpecComponent::Zero => vec![choice("0", "zero padded", "pads after the sign")],
            FormatSpecComponent::Grouping => vec![
                choice(",", "every 3 digits", ""),
                choice("_", "every 3 or 4 digits", "four in base 2, 8 and 16"),
            ],
            FormatSpecComponent::Precision => vec![choice(
                ".",
                "precision",
                "digits after the point, significant digits, or the length a string is \
                 truncated to",
            )],
            // with no target known the spec could be read by any of them, so
            // the widest set is offered rather than none
            FormatSpecComponent::Type => target
                .unwrap_or(FormatTarget::Int)
                .presentation_types()
                .iter()
                .map(|(presentation, summary, detail)| {
                    choice(&presentation.to_string(), summary, detail)
                })
                .collect(),
            FormatSpecComponent::Width
            | FormatSpecComponent::Fill
            | FormatSpecComponent::Conversion => Vec::new(),
        }
    }

    /// what this clause means, written as `written`
    pub fn describe(self, written: &str, target: Option<FormatTarget>) -> String {
        self.choices(target)
            .into_iter()
            .find(|choice| choice.insert == written)
            .map_or_else(
                || format!("{} — {}", self.label(), self.documentation()),
                |choice| format!("{} — {}", self.label(), choice.summary),
            )
    }
}

/// one thing that can be written for a clause
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// the text itself
    pub insert: String,
    /// a word or two naming what it does, short enough to sit beside it in a
    /// completion list
    pub summary: &'static str,
    /// the rest of the explanation, when the summary does not say all of it
    pub detail: &'static str,
    /// the clause it belongs to
    pub component: FormatSpecComponent,
}

impl Choice {
    /// the full explanation, for a documentation panel
    pub fn documentation(&self) -> String {
        let mut rendered = format!("{} — {}", self.component.label(), self.summary);
        if !self.detail.is_empty() {
            rendered.push_str("\n\n");
            rendered.push_str(self.detail);
        }
        rendered
    }
}

impl FormatSpecSpans {
    /// where `component` was written, if it was
    pub fn component(&self, component: FormatSpecComponent) -> Option<&Range<usize>> {
        match component {
            FormatSpecComponent::Conversion => self.conversion.as_ref(),
            FormatSpecComponent::Fill => self.fill.as_ref(),
            FormatSpecComponent::Align => self.align.as_ref(),
            FormatSpecComponent::Sign => self.sign.as_ref(),
            FormatSpecComponent::AlternateForm => self.alternate_form.as_ref(),
            FormatSpecComponent::Zero => self.zero.as_ref(),
            FormatSpecComponent::Width => self.width.as_ref(),
            FormatSpecComponent::Grouping => self.grouping_option.as_ref(),
            FormatSpecComponent::Precision => self.precision.as_ref(),
            FormatSpecComponent::Type => self.format_type.as_ref(),
        }
    }

    /// the component `offset` falls in, if any
    pub fn at(&self, offset: usize) -> Option<FormatSpecComponent> {
        FormatSpecComponent::ALL.into_iter().find(|component| {
            self.component(*component)
                .is_some_and(|span| span.contains(&offset))
        })
    }

    /// every written component, in the order it appears
    pub fn iter(&self) -> impl Iterator<Item = (FormatSpecComponent, Range<usize>)> + '_ {
        FormatSpecComponent::ALL
            .into_iter()
            .filter_map(|component| Some((component, self.component(component)?.clone())))
    }
}

/// a spec the target's `__format__` raises on
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatSpecViolation {
    /// a presentation type this target does not have
    UnknownType(char),
    /// a clause this target never accepts, or does not accept with this
    /// presentation type
    NotAllowed {
        component: FormatSpecComponent,
        /// the presentation type that rejects it, when the clause would have
        /// been fine under another one
        with_type: Option<char>,
    },
}

impl FormatSpecViolation {
    /// the clause to point the report at
    pub fn component(&self) -> FormatSpecComponent {
        match self {
            FormatSpecViolation::UnknownType(_) => FormatSpecComponent::Type,
            FormatSpecViolation::NotAllowed { component, .. } => *component,
        }
    }

    /// a one-line explanation, in the terms the runtime would use
    pub fn describe(&self, target: FormatTarget) -> String {
        match self {
            FormatSpecViolation::UnknownType(found) => format!(
                "`{found}` is not a presentation type for `{}`",
                target.type_name()
            ),
            FormatSpecViolation::NotAllowed {
                component,
                with_type: Some(with_type),
            } => format!(
                "`{}` is not allowed with presentation type `{with_type}`",
                component.label()
            ),
            FormatSpecViolation::NotAllowed {
                component,
                with_type: None,
            } => format!(
                "`{}` is not allowed when formatting `{}`",
                component.label(),
                target.type_name()
            ),
        }
    }
}

fn not_allowed(component: FormatSpecComponent) -> FormatSpecViolation {
    FormatSpecViolation::NotAllowed {
        component,
        with_type: None,
    }
}

impl StaticFormatSpec {
    /// a clause the presentation type does not accept
    ///
    /// the message names that type only when it was written out. an `int` with
    /// no type formats as `d`, and reporting "not allowed with `d`" for a spec
    /// that never says `d` explains nothing
    fn rejected_by_type(&self, component: FormatSpecComponent) -> FormatSpecViolation {
        FormatSpecViolation::NotAllowed {
            component,
            with_type: self.format_type.as_ref().map(char::from),
        }
    }

    /// check the spec against the rules `target`'s `__format__` enforces
    ///
    /// the checks run in the order the runtime runs them, so a spec with more
    /// than one problem reports the one that would actually be raised
    pub fn validate(&self, target: FormatTarget) -> Result<(), FormatSpecViolation> {
        let presentation = self.format_type.as_ref().map(char::from);
        match target {
            FormatTarget::Str => self.validate_str(presentation),
            FormatTarget::Int => self.validate_int(presentation),
            FormatTarget::Float => self.validate_float(presentation, /* complex */ false),
            FormatTarget::Complex => self.validate_float(presentation, /* complex */ true),
        }
    }

    fn validate_str(&self, presentation: Option<char>) -> Result<(), FormatSpecViolation> {
        if let Some(found) = presentation
            && found != 's'
        {
            return Err(FormatSpecViolation::UnknownType(found));
        }
        if self.grouping_option.is_some() {
            return Err(self.rejected_by_type(FormatSpecComponent::Grouping));
        }
        if self.sign.is_some() {
            return Err(not_allowed(FormatSpecComponent::Sign));
        }
        if self.alternate_form {
            return Err(not_allowed(FormatSpecComponent::AlternateForm));
        }
        // a leading `0` is shorthand for `0=`, and `=` is the alignment a
        // string never has anywhere to put
        if self.align == Some(FormatAlign::AfterSign) {
            return Err(not_allowed(if self.zero {
                FormatSpecComponent::Zero
            } else {
                FormatSpecComponent::Align
            }));
        }
        Ok(())
    }

    fn validate_int(&self, presentation: Option<char>) -> Result<(), FormatSpecViolation> {
        // an `int` with no presentation type formats as `d`
        let presentation = presentation.unwrap_or('d');
        if !matches!(
            presentation,
            'b' | 'c' | 'd' | 'o' | 'x' | 'X' | 'n' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%'
        ) {
            return Err(FormatSpecViolation::UnknownType(presentation));
        }
        // the float presentation types convert first, and then follow the
        // float rules rather than the integer ones
        if matches!(presentation, 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%') {
            return self.validate_float(Some(presentation), /* complex */ false);
        }
        if self.precision.is_some() {
            return Err(self.rejected_by_type(FormatSpecComponent::Precision));
        }
        if presentation == 'c' {
            if self.sign.is_some() {
                return Err(self.rejected_by_type(FormatSpecComponent::Sign));
            }
            if self.alternate_form {
                return Err(self.rejected_by_type(FormatSpecComponent::AlternateForm));
            }
        }
        self.validate_grouping(presentation)
    }

    fn validate_float(
        &self,
        presentation: Option<char>,
        complex: bool,
    ) -> Result<(), FormatSpecViolation> {
        let allowed = if complex {
            matches!(
                presentation,
                None | Some('e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'n')
            )
        } else {
            matches!(
                presentation,
                None | Some('e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'n' | '%')
            )
        };
        if !allowed {
            return Err(FormatSpecViolation::UnknownType(
                presentation.unwrap_or('\0'),
            ));
        }
        // a complex has two numbers and one sign slot, so there is no
        // meaningful place to pad after the sign
        if complex && self.align == Some(FormatAlign::AfterSign) {
            return Err(not_allowed(if self.zero {
                FormatSpecComponent::Zero
            } else {
                FormatSpecComponent::Align
            }));
        }
        self.validate_grouping(presentation.unwrap_or('\0'))
    }

    /// `,` groups the decimal presentations only; `_` additionally groups the
    /// power-of-two bases, where it separates every four digits
    fn validate_grouping(&self, presentation: char) -> Result<(), FormatSpecViolation> {
        let Some(grouping) = &self.grouping_option else {
            return Ok(());
        };
        let allowed = match presentation {
            'd' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%' | '\0' => true,
            'b' | 'o' | 'x' | 'X' => *grouping == FormatGrouping::Underscore,
            // `n` already groups for the locale
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(self.rejected_by_type(FormatSpecComponent::Grouping))
        }
    }

    /// render `target`'s sample value through this spec, for a preview of what
    /// the spec does
    ///
    /// returns `None` when the spec is not one `target` accepts
    pub fn preview(&self, target: FormatTarget) -> Option<String> {
        self.validate(target).ok()?;
        Some(match target.sample() {
            Sample::Str(value) => self.render_str(value),
            Sample::Int(value) => self.render_int(value),
            Sample::Float(value) => self.render_float(value),
            Sample::Complex(re, im) => self.render_complex(re, im),
        })
    }

    fn render_str(&self, value: &str) -> String {
        let truncated: String = match self.precision {
            Some(precision) => value.chars().take(precision).collect(),
            None => value.to_string(),
        };
        self.pad("", &truncated, FormatAlign::Left)
    }

    fn render_int(&self, value: i64) -> String {
        let presentation = self.format_type.as_ref().map_or('d', char::from);
        if matches!(presentation, 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%') {
            #[expect(clippy::cast_precision_loss, reason = "the sample is a small int")]
            return self.render_float(value as f64);
        }
        if presentation == 'c' {
            let character = u32::try_from(value)
                .ok()
                .and_then(char::from_u32)
                .map_or_else(String::new, String::from);
            return self.pad("", &character, FormatAlign::Right);
        }

        let (radix, prefix, case, group) = match presentation {
            'b' => (2, "0b", Case::Lower, 4),
            'o' => (8, "0o", Case::Lower, 4),
            'x' => (16, "0x", Case::Lower, 4),
            'X' => (16, "0X", Case::Upper, 4),
            _ => (10, "", Case::Lower, 3),
        };
        let digits = to_radix(value.unsigned_abs(), radix, case);
        let mut head = self.sign_prefix(value < 0).to_string();
        if self.alternate_form {
            head.push_str(prefix);
        }
        self.pad_number(&head, &digits, "", group)
    }

    fn render_float(&self, value: f64) -> String {
        let presentation = self.format_type.as_ref().map(char::from);
        let head = self.sign_prefix(value.is_sign_negative()).to_string();
        let rendered = self.render_float_magnitude(value.abs(), presentation);
        let split = integer_part_end(&rendered);
        self.pad_number(&head, &rendered[..split], &rendered[split..], 3)
    }

    /// a complex renders as `real` then `imag` with its own sign, so the whole
    /// body is padded as one unit and never after the sign
    fn render_complex(&self, re: f64, im: f64) -> String {
        let presentation = self.format_type.as_ref().map(char::from);
        let part = |value: f64| {
            let rendered = self.render_float_magnitude(value, presentation);
            match &self.grouping_option {
                Some(grouping) => {
                    let split = integer_part_end(&rendered);
                    format!(
                        "{}{}",
                        insert_separators(&rendered[..split], 3, separator(grouping)),
                        &rendered[split..]
                    )
                }
                None => rendered,
            }
        };
        let real = format!(
            "{}{}",
            if re.is_sign_negative() { "-" } else { "" },
            part(re.abs())
        );
        let imaginary = format!(
            "{}{}j",
            if im.is_sign_negative() { "-" } else { "+" },
            part(im.abs())
        );
        // with no presentation type of its own a complex keeps `repr`'s
        // parentheses, however else the spec pads or rounds it
        let body = if presentation.is_none() {
            format!("({real}{imaginary})")
        } else {
            format!("{real}{imaginary}")
        };
        self.pad("", &body, FormatAlign::Right)
    }

    /// the digits of a non-negative float, without sign, grouping or padding
    fn render_float_magnitude(&self, value: f64, presentation: Option<char>) -> String {
        match presentation {
            Some('f' | 'F') => format!("{value:.*}", self.precision.unwrap_or(6)),
            Some('e') => scientific(value, self.precision.unwrap_or(6), Case::Lower),
            Some('E') => scientific(value, self.precision.unwrap_or(6), Case::Upper),
            Some('%') => format!("{:.*}%", self.precision.unwrap_or(6), value * 100.0),
            Some(other @ ('g' | 'G' | 'n')) => general(
                value,
                self.precision.unwrap_or(6),
                if other == 'G' {
                    Case::Upper
                } else {
                    Case::Lower
                },
                self.alternate_form,
            ),
            // no presentation type keeps `repr`'s shortest round-trip form,
            // except that a precision makes it behave like `g`
            _ => match self.precision {
                Some(precision) => general(value, precision, Case::Lower, self.alternate_form),
                None => shortest(value),
            },
        }
    }

    fn sign_prefix(&self, negative: bool) -> &'static str {
        if negative {
            "-"
        } else {
            match self.sign {
                Some(FormatSign::Plus) => "+",
                Some(FormatSign::MinusOrSpace) => " ",
                _ => "",
            }
        }
    }

    /// pad a number whose integer run is `digits` and whose fraction and
    /// exponent are `suffix`, inserting group separators as it goes
    ///
    /// zero padding is not fill in the ordinary sense: it lengthens the number
    /// itself, so the separators fall between the padding zeroes too and the
    /// result can overshoot the requested width when no digit count lands on
    /// it exactly
    fn pad_number(&self, head: &str, digits: &str, suffix: &str, group: usize) -> String {
        let separator = self.grouping_option.as_ref().map(separator);
        let zero_padded = self.fill == Some('0') && self.align == Some(FormatAlign::AfterSign);
        if let (true, Some(separator)) = (zero_padded, separator) {
            let width = self.width.unwrap_or(0);
            let available = width.saturating_sub(head.chars().count() + suffix.chars().count());
            let written = digits.chars().count();
            let mut count = written;
            while count + (count.saturating_sub(1)) / group < available {
                count += 1;
            }
            let padded = format!("{}{digits}", "0".repeat(count - written));
            return format!(
                "{head}{}{suffix}",
                insert_separators(&padded, group, separator)
            );
        }
        let body = match separator {
            Some(separator) => {
                format!("{}{suffix}", insert_separators(digits, group, separator))
            }
            None => format!("{digits}{suffix}"),
        };
        self.pad(head, &body, FormatAlign::Right)
    }

    /// pad `head + body` out to the requested width. `head` is the sign and
    /// any base prefix, which `=` alignment keeps to the left of the fill
    fn pad(&self, head: &str, body: &str, default_align: FormatAlign) -> String {
        let width = self.width.unwrap_or(0);
        let current = head.chars().count() + body.chars().count();
        if current >= width {
            return format!("{head}{body}");
        }
        let missing = width - current;
        let fill = self.fill.unwrap_or(' ');
        let filled = |count: usize| std::iter::repeat_n(fill, count).collect::<String>();
        match self.align.unwrap_or(default_align) {
            FormatAlign::Left => format!("{head}{body}{}", filled(missing)),
            FormatAlign::Right => format!("{}{head}{body}", filled(missing)),
            FormatAlign::AfterSign => format!("{head}{}{body}", filled(missing)),
            FormatAlign::Center => {
                let left = missing / 2;
                format!("{}{head}{body}{}", filled(left), filled(missing - left))
            }
        }
    }
}

/// where the integer run of a rendered number ends
fn integer_part_end(rendered: &str) -> usize {
    rendered.find(['.', 'e', 'E']).unwrap_or(rendered.len())
}

fn separator(grouping: &FormatGrouping) -> char {
    match grouping {
        FormatGrouping::Comma => ',',
        FormatGrouping::Underscore => '_',
    }
}

fn to_radix(mut value: u64, radix: u64, case: Case) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let alphabet: &[u8] = match case {
        Case::Lower => b"0123456789abcdef",
        Case::Upper => b"0123456789ABCDEF",
    };
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(alphabet[usize::try_from(value % radix).unwrap_or(0)]);
        value /= radix;
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

fn insert_separators(digits: &str, group: usize, separator: char) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / group);
    let count = digits.chars().count();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (count - index).is_multiple_of(group) {
            out.push(separator);
        }
        out.push(digit);
    }
    out
}

/// python's shortest round-trip form, which always keeps a decimal point
fn shortest(value: f64) -> String {
    if value.is_infinite() {
        return "inf".to_string();
    }
    if value.is_nan() {
        return "nan".to_string();
    }
    let rendered = format!("{value}");
    if rendered.contains(['.', 'e']) {
        rendered
    } else {
        format!("{rendered}.0")
    }
}

/// scientific notation with python's two-digit signed exponent
fn scientific(value: f64, precision: usize, case: Case) -> String {
    let (mantissa, exponent) = split_scientific(value, precision);
    let marker = match case {
        Case::Lower => 'e',
        Case::Upper => 'E',
    };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}{marker}{sign}{:02}", exponent.abs())
}

/// the mantissa and exponent of `value` rounded to `precision` digits after
/// the point
fn split_scientific(value: f64, precision: usize) -> (String, i32) {
    if value == 0.0 {
        let mut mantissa = String::from("0");
        if precision > 0 {
            let _ = write!(mantissa, ".{}", "0".repeat(precision));
        }
        return (mantissa, 0);
    }
    // rust renders the same rounded mantissa, only with a bare exponent
    let rendered = format!("{value:.precision$e}");
    let (mantissa, exponent) = rendered
        .split_once('e')
        .expect("rust always writes an exponent for `{:e}`");
    (mantissa.to_string(), exponent.parse().unwrap_or_default())
}

/// the `g` presentation: fixed point when the exponent is small, scientific
/// otherwise, with trailing zeroes removed unless the alternate form keeps them
fn general(value: f64, precision: usize, case: Case, alternate_form: bool) -> String {
    let precision = precision.max(1);
    let (_, exponent) = split_scientific(value, precision - 1);
    let mut rendered = if (-4..i32::try_from(precision).unwrap_or(i32::MAX)).contains(&exponent) {
        let places = usize::try_from(i32::try_from(precision).unwrap_or(i32::MAX) - 1 - exponent)
            .unwrap_or(0);
        format!("{value:.places$}")
    } else {
        scientific(value, precision - 1, case)
    };
    if !alternate_form {
        rendered = strip_trailing_zeroes(&rendered);
    }
    rendered
}

fn strip_trailing_zeroes(rendered: &str) -> String {
    let (digits, exponent) = match rendered.find(['e', 'E']) {
        Some(index) => rendered.split_at(index),
        None => (rendered, ""),
    };
    if !digits.contains('.') {
        return rendered.to_string();
    }
    let trimmed = digits.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}{exponent}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::FormatSpec;

    use FormatTarget::{Complex, Float, Int, Str};

    /// what cpython itself produces for each of these, captured by running
    /// `format(sample, spec)` on the sample value of each target
    const EXPECTED: &[(FormatTarget, &str, Option<&str>)] = &[
        (Str, "", Some("spam")),
        (Str, "s", Some("spam")),
        (Str, ">10", Some("      spam")),
        (Str, "<10", Some("spam      ")),
        (Str, "^10", Some("   spam   ")),
        (Str, "*^10", Some("***spam***")),
        (Str, ".2", Some("sp")),
        (Str, "10.3", Some("spa       ")),
        (Str, ".0", Some("")),
        (Str, "=10", None),
        (Str, ",", None),
        (Str, "+", None),
        (Str, "#", None),
        (Str, "05", None),
        (Str, "d", None),
        (Int, "", Some("1234")),
        (Int, "d", Some("1234")),
        (Int, "b", Some("10011010010")),
        (Int, "o", Some("2322")),
        (Int, "x", Some("4d2")),
        (Int, "X", Some("4D2")),
        (Int, "c", Some("Ӓ")),
        (Int, "n", Some("1234")),
        (Int, "+d", Some("+1234")),
        (Int, " d", Some(" 1234")),
        (Int, "-d", Some("1234")),
        (Int, ",", Some("1,234")),
        (Int, "_", Some("1_234")),
        (Int, "#x", Some("0x4d2")),
        (Int, "#b", Some("0b10011010010")),
        (Int, "#o", Some("0o2322")),
        (Int, "#X", Some("0X4D2")),
        (Int, "08", Some("00001234")),
        (Int, "+08", Some("+0001234")),
        (Int, ">10", Some("      1234")),
        (Int, "^10", Some("   1234   ")),
        (Int, "=10", Some("      1234")),
        (Int, "010,", Some("00,001,234")),
        (Int, "09,", Some("0,001,234")),
        (Int, "08,", Some("0,001,234")),
        (Int, "07,", Some("001,234")),
        (Int, "06,", Some("01,234")),
        (Int, "012_", Some("0_000_001_234")),
        (Int, "#012_x", Some("0x0_0000_04d2")),
        (Int, "e", Some("1.234000e+03")),
        (Int, "E", Some("1.234000E+03")),
        (Int, "f", Some("1234.000000")),
        (Int, ".2f", Some("1234.00")),
        (Int, "g", Some("1234")),
        (Int, "%", Some("123400.000000%")),
        (Int, ".1%", Some("123400.0%")),
        (Int, "#010x", Some("0x000004d2")),
        (Int, "_x", Some("4d2")),
        (Int, "_b", Some("100_1101_0010")),
        (Int, "*=10,", Some("*****1,234")),
        (Int, "0=10,", Some("00,001,234")),
        (Int, ".2", None),
        (Int, "s", None),
        (Int, ",c", None),
        (Int, "#c", None),
        (Float, "", Some("1234.5678")),
        (Float, "f", Some("1234.567800")),
        (Float, ".2f", Some("1234.57")),
        (Float, "e", Some("1.234568e+03")),
        (Float, "E", Some("1.234568E+03")),
        (Float, ".3e", Some("1.235e+03")),
        (Float, "g", Some("1234.57")),
        (Float, "G", Some("1234.57")),
        (Float, ".3g", Some("1.23e+03")),
        (Float, "%", Some("123456.780000%")),
        (Float, ".1%", Some("123456.8%")),
        (Float, ",", Some("1,234.5678")),
        (Float, ",.2f", Some("1,234.57")),
        (Float, "_", Some("1_234.5678")),
        (Float, ">15", Some("      1234.5678")),
        (Float, "015", Some("0000001234.5678")),
        (Float, "+.2f", Some("+1234.57")),
        (Float, " .2f", Some(" 1234.57")),
        (Float, "=15.2f", Some("        1234.57")),
        (Float, "^15.2f", Some("    1234.57    ")),
        (Float, ".0f", Some("1235")),
        (Float, "#g", Some("1234.57")),
        (Float, "n", Some("1234.57")),
        (Float, ".10g", Some("1234.5678")),
        (Float, ".1g", Some("1e+03")),
        (Float, "015,", Some("00,001,234.5678")),
        (Float, "016,", Some("000,001,234.5678")),
        (Float, "020,.2f", Some("0,000,000,001,234.57")),
        (Float, "d", None),
        (Float, "x", None),
        (Float, "s", None),
        (Float, "b", None),
        (Complex, "", Some("(1.5+2.5j)")),
        (Complex, "f", Some("1.500000+2.500000j")),
        (Complex, ".2f", Some("1.50+2.50j")),
        (Complex, "e", Some("1.500000e+00+2.500000e+00j")),
        (Complex, "g", Some("1.5+2.5j")),
        (Complex, ">20", Some("          (1.5+2.5j)")),
        (Complex, "^20.2f", Some("     1.50+2.50j     ")),
        (Complex, ".3g", Some("1.5+2.5j")),
        (Complex, "20", Some("          (1.5+2.5j)")),
        (Complex, "<20", Some("(1.5+2.5j)          ")),
        (Complex, ".3", Some("(1.5+2.5j)")),
        (Complex, "n", Some("1.5+2.5j")),
        (Complex, "020", None),
        (Complex, "%", None),
        (Complex, "d", None),
    ];

    #[test]
    fn previews_match_cpython() {
        let mut failures = Vec::new();
        for (target, spec, expected) in EXPECTED {
            let FormatSpec::Static(parsed) = FormatSpec::parse(spec).unwrap() else {
                panic!("`{spec}` is not a static spec");
            };
            let actual = parsed.preview(*target);
            let expected = expected.map(str::to_string);
            if actual != expected {
                failures.push(format!(
                    "{}: `{spec}` produced {actual:?}, expected {expected:?}",
                    target.type_name()
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
