//! the strftime directives `date`, `time` and `datetime` read as their format
//! spec
//!
//! this is a different language from the [format specification mini-language],
//! not a dialect of it: `f"{when:%Y}"` calls `when.strftime("%Y")`, and none of
//! the fill/align/width rules apply. an unrecognised directive does not raise —
//! the platform writes it through and the output is quietly wrong — which is
//! what makes checking it worth doing
//!
//! [format specification mini-language]: https://docs.python.org/3/library/string.html#format-specification-mini-language

use std::ops::Range;

/// how well supported a directive is
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DirectiveKind {
    /// in the set python documents as working on every platform
    Portable,
    /// in `strftime(3)` but not in python's own list, so it is missing on some
    /// platforms — notably windows
    Platform,
    /// neither, so nothing renders it and the text comes out as written
    Unknown,
    /// a `%` with nothing after it
    Dangling,
    /// `%%`, which writes one `%`
    Escape,
}

/// one `%` directive
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    /// the letter that names it — `Y` in `%-Y`. absent when dangling
    pub code: Option<char>,
    /// whether any of the `-_0^#:` flags were written, which is itself a
    /// platform extension however portable the code is
    pub flagged: bool,
    /// where the whole directive sits in the format string
    pub span: Range<usize>,
    pub kind: DirectiveKind,
}

impl Directive {
    /// what this directive means
    pub fn documentation(&self) -> &'static str {
        let Some(code) = self.code else {
            return "a `%` with no directive after it";
        };
        documentation(code)
    }

    /// what this directive writes for [`SAMPLE`], or `None` when the answer
    /// depends on the machine rather than the value
    pub fn sample(&self) -> Option<&'static str> {
        let code = self.code?;
        // a flag changes the padding, so the unflagged sample would be a lie
        if self.flagged {
            return None;
        }
        sample(code)
    }
}

/// the instant every preview is rendered from: a naive `2001-02-03 04:05:06.000007`,
/// which is a saturday
pub const SAMPLE: &str = "datetime(2001, 2, 3, 4, 5, 6, 7)";

/// split a format string into its directives, in the order they are written
///
/// literal text between directives is not reported; only the `%` runs are
pub fn directives(text: &str) -> Vec<Directive> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = text[at..].find('%') {
        let start = at + offset;
        let mut cursor = start + 1;
        // glibc and bsd allow padding flags and a width between the `%` and the
        // letter; python documents neither, so their presence alone makes the
        // directive non-portable
        let mut flagged = false;
        while cursor < bytes.len()
            && matches!(bytes[cursor], b'-' | b'_' | b'0' | b'^' | b'#' | b':')
        {
            flagged = true;
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            flagged = true;
            cursor += 1;
        }
        let code = text[cursor..].chars().next();
        let end = code.map_or(cursor, |code| cursor + code.len_utf8());
        let kind = match code {
            None => DirectiveKind::Dangling,
            Some('%') if !flagged => DirectiveKind::Escape,
            Some(code) if flagged || PLATFORM.contains(code) => {
                if PORTABLE.contains(code) || PLATFORM.contains(code) {
                    DirectiveKind::Platform
                } else {
                    DirectiveKind::Unknown
                }
            }
            Some(code) if PORTABLE.contains(code) => DirectiveKind::Portable,
            Some(_) => DirectiveKind::Unknown,
        };
        found.push(Directive {
            code,
            flagged,
            span: start..end,
            kind,
        });
        at = end.max(start + 1);
    }
    found
}

/// the codes python documents as working on every platform
const PORTABLE: &str = "aAwdbBmyYHIpMSfzZjUWcxXGuV%";

/// codes in `strftime(3)` that python does not promise
const PLATFORM: &str = "DFTRrntklseCgh";

fn documentation(code: char) -> &'static str {
    match code {
        'a' => "weekday, abbreviated — `Sun`",
        'A' => "weekday — `Sunday`",
        'w' => "weekday as a number, `0` for sunday",
        'd' => "day of the month, zero padded",
        'b' => "month, abbreviated — `Jan`",
        'B' => "month — `January`",
        'm' => "month as a number, zero padded",
        'y' => "year without the century, zero padded",
        'Y' => "year with the century",
        'H' => "hour on the 24-hour clock, zero padded",
        'I' => "hour on the 12-hour clock, zero padded",
        'p' => "`AM` or `PM`",
        'M' => "minute, zero padded",
        'S' => "second, zero padded",
        'f' => "microsecond, zero padded to six digits",
        'z' => "utc offset — `+0000`, and empty when naive",
        'Z' => "time zone name, and empty when naive",
        'j' => "day of the year, zero padded",
        'U' => "week of the year, counting from the first sunday",
        'W' => "week of the year, counting from the first monday",
        'c' => "the locale's own date and time",
        'x' => "the locale's own date",
        'X' => "the locale's own time",
        'G' => "iso 8601 year",
        'u' => "iso 8601 weekday, `1` for monday",
        'V' => "iso 8601 week of the year",
        '%' => "a literal `%`",
        'D' => "`%m/%d/%y` — not on every platform",
        'F' => "`%Y-%m-%d` — not on every platform",
        'T' => "`%H:%M:%S` — not on every platform",
        'R' => "`%H:%M` — not on every platform",
        'r' => "the locale's 12-hour time — not on every platform",
        'n' => "a newline — not on every platform",
        't' => "a tab — not on every platform",
        'k' => "hour on the 24-hour clock, space padded — not on every platform",
        'l' => "hour on the 12-hour clock, space padded — not on every platform",
        'e' => "day of the month, space padded — not on every platform",
        's' => "seconds since the epoch — not on every platform",
        'C' => "the century — not on every platform",
        'g' => "iso 8601 year without the century — not on every platform",
        'h' => "the same as `%b` — not on every platform",
        _ => "not a directive any platform renders",
    }
}

/// what each directive writes for [`SAMPLE`], captured from cpython
fn sample(code: char) -> Option<&'static str> {
    Some(match code {
        'a' => "Sat",
        'A' => "Saturday",
        'w' => "6",
        'd' => "03",
        'b' => "Feb",
        'B' => "February",
        'm' => "02",
        'y' => "01",
        'Y' => "2001",
        'H' => "04",
        'I' => "04",
        'p' => "AM",
        'M' => "05",
        'S' => "06",
        'f' => "000007",
        // naive, so both are empty
        'z' | 'Z' => "",
        'j' => "034",
        'U' => "04",
        'W' => "05",
        'c' => "Sat Feb  3 04:05:06 2001",
        'x' => "02/03/01",
        'X' => "04:05:06",
        'G' => "2001",
        'u' => "6",
        'V' => "05",
        '%' => "%",
        'D' => "02/03/01",
        'F' => "2001-02-03",
        'T' => "04:05:06",
        'R' => "04:05",
        'r' => "04:05:06 AM",
        'n' => "\n",
        't' => "\t",
        'k' | 'l' => " 4",
        'e' => " 3",
        'C' => "20",
        'g' => "01",
        'h' => "Feb",
        // seconds since the epoch depend on the machine's time zone
        _ => return None,
    })
}

/// render `text` for [`SAMPLE`], or `None` when any part of it has no answer
pub fn preview(text: &str) -> Option<String> {
    let mut rendered = String::with_capacity(text.len());
    let mut at = 0;
    for directive in directives(text) {
        if directive.kind == DirectiveKind::Dangling {
            return None;
        }
        rendered.push_str(&text[at..directive.span.start]);
        rendered.push_str(directive.sample()?);
        at = directive.span.end;
    }
    rendered.push_str(&text[at..]);
    Some(rendered)
}

/// every directive worth suggesting, with what it means
pub fn completions() -> impl Iterator<Item = (char, &'static str)> {
    PORTABLE
        .chars()
        .chain(PLATFORM.chars())
        .map(|code| (code, documentation(code)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(String, DirectiveKind)> {
        directives(text)
            .into_iter()
            .map(|directive| (text[directive.span.clone()].to_string(), directive.kind))
            .collect()
    }

    #[test]
    fn classifies_each_directive() {
        assert_eq!(
            kinds("%d%-M%-Y%"),
            [
                ("%d".to_string(), DirectiveKind::Portable),
                ("%-M".to_string(), DirectiveKind::Platform),
                ("%-Y".to_string(), DirectiveKind::Platform),
                ("%".to_string(), DirectiveKind::Dangling),
            ]
        );
    }

    #[test]
    fn an_unrecognised_code_is_unknown() {
        assert_eq!(
            kinds("%Y-%Q"),
            [
                ("%Y".to_string(), DirectiveKind::Portable),
                ("%Q".to_string(), DirectiveKind::Unknown),
            ]
        );
    }

    #[test]
    fn a_doubled_percent_is_an_escape() {
        assert_eq!(kinds("100%%"), [("%%".to_string(), DirectiveKind::Escape)]);
    }

    #[test]
    fn literal_text_is_left_alone() {
        assert!(directives("no directives here").is_empty());
    }

    #[test]
    fn previews_match_cpython() {
        // `datetime(2001, 2, 3, 4, 5, 6, 7).strftime(..)` for each
        assert_eq!(preview("%Y-%m-%d").as_deref(), Some("2001-02-03"));
        assert_eq!(preview("%H:%M:%S.%f").as_deref(), Some("04:05:06.000007"));
        assert_eq!(
            preview("%A %d %B %Y").as_deref(),
            Some("Saturday 03 February 2001")
        );
        assert_eq!(
            preview("100%% done at %I:%M %p").as_deref(),
            Some("100% done at 04:05 AM")
        );
    }

    #[test]
    fn a_preview_needs_every_part_answered() {
        // the epoch depends on the machine's time zone, and a flag changes the
        // padding the sample was captured with
        assert_eq!(preview("%s"), None);
        assert_eq!(preview("%-d"), None);
        assert_eq!(preview("%Y%"), None);
    }
}
