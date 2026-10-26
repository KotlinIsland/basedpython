//! strips common indentation from triple-quoted multiline strings

use ruff_python_ast::str::{Dedent, dedent};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    AnyStringFlags, Expr, InterpolatedStringElement, InterpolatedStringElements, ModModule, Stmt,
    StringFlags,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct DedentString<'src> {
    source: &'src str,
}

impl<'src> DedentString<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for DedentString<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            edits: Vec::new(),
            errors: Vec::new(),
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.edits);
        ctx.errors.extend(state.errors);
    }
}

struct State<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
    errors: Vec<String>,
}

impl State<'_> {
    /// dedent the lone literal `string`, whose literal text — the text that is not a
    /// replacement field — is `literals`, or all of its body when that is `None`
    fn dedent(
        &mut self,
        string: TextRange,
        flags: AnyStringFlags,
        literals: Option<Vec<TextRange>>,
    ) {
        match dedent(self.source, string, flags) {
            Dedent::Unchanged => {}
            Dedent::Invalid(error) => self.errors.push(error.to_string()),
            Dedent::Runs { runs, .. } => {
                let written = dedented_literal(self.source, string, flags, &runs, literals);
                self.edits.push((string, written));
            }
        }
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::StringLiteral(string_expr) => {
                if let Some(part) = string_expr.as_single_part_string() {
                    self.dedent(part.range(), part.flags.into(), None);
                }
            }
            Expr::FString(string_expr) => {
                if let Some(fstring) = string_expr.as_single_part_fstring() {
                    let literals = literal_ranges(&fstring.elements);
                    self.dedent(fstring.range(), fstring.flags.into(), Some(literals));
                }
            }
            Expr::TString(string_expr) => {
                if let Some(tstring) = string_expr.as_single_part_tstring() {
                    let literals = literal_ranges(&tstring.elements);
                    self.dedent(tstring.range(), tstring.flags.into(), Some(literals));
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// the ranges of an f- or t-string's literal text, including the literal text of
/// its format specs, which a raw prefix makes raw too, in source order
fn literal_ranges(elements: &InterpolatedStringElements) -> Vec<TextRange> {
    let mut ranges = Vec::new();
    let mut pending = vec![elements];
    while let Some(elements) = pending.pop() {
        for element in elements {
            match element {
                InterpolatedStringElement::Literal(literal) => ranges.push(literal.range()),
                InterpolatedStringElement::Interpolation(interpolation) => {
                    if let Some(spec) = &interpolation.format_spec {
                        pending.push(&spec.elements);
                    }
                }
            }
        }
    }
    ranges.sort_unstable_by_key(Ranged::start);
    ranges
}

/// the literal `string` written so that it holds the text of `runs`, the runs of the
/// source its dedent keeps
///
/// the text is written between a quote ending in a line continuation and a line
/// continuation before the closing quote, so the literal keeps the lines it was written
/// on. a raw literal has no line continuation — its backslash and newline are both
/// text — so it is written as the plain literal that holds the same text instead: every
/// backslash and quote of its literal text, `literals` (in source order, or all of the
/// body when `None`), escaped
fn dedented_literal(
    source: &str,
    string: TextRange,
    flags: AnyStringFlags,
    runs: &[TextRange],
    literals: Option<Vec<TextRange>>,
) -> String {
    let quote = flags.quote_str();
    let opener = &source[TextRange::at(string.start(), flags.opener_len())];
    let mut prefix = opener[..opener.len() - quote.len()].to_owned();
    let raw = flags.prefix().is_raw();
    if raw {
        prefix.retain(|c| !matches!(c, 'r' | 'R'));
    }
    let quote_char = flags.quote_style().as_char();

    let mut content = String::new();
    // the runs and the literal text are both in source order, so one pass over each
    // finds the literal text every character of the runs is in
    let mut literals = literals.map(|literals| literals.into_iter().peekable());
    for run in runs {
        if !raw {
            content.push_str(&source[*run]);
            continue;
        }
        for (index, c) in source[*run].char_indices() {
            let offset = run.start() + TextSize::try_from(index).unwrap_or_default();
            let is_literal = match &mut literals {
                None => true,
                Some(literals) => {
                    while literals
                        .next_if(|literal| literal.end() <= offset)
                        .is_some()
                    {}
                    literals
                        .peek()
                        .is_some_and(|literal| literal.contains(offset))
                }
            };
            if (c == '\\' || c == quote_char) && is_literal {
                content.push('\\');
            }
            content.push(c);
        }
    }
    format!("{prefix}{quote}\\\n{content}\\\n{quote}")
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        check_at(Config::test_default().min_version, input, expected);
    }

    fn check_at(min_version: crate::PythonVersion, input: &str, expected: &str) {
        let config = Config {
            min_version,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn basic_dedent() {
        check(
            indoc! {r#"
                text = """
                    start-of-line
                    |
                    """
            "#},
            indoc! {r#"
                text = """\
                start-of-line
                |\
                """
            "#},
        );
    }

    #[test]
    fn fstring_dedent() {
        check(
            indoc! {r#"
                a = "asdf"
                text = f"""
                    start-of-line{a}
                    |
                    """
            "#},
            indoc! {r#"
                a = "asdf"
                text = f"""\
                start-of-line{a}
                |\
                """
            "#},
        );
    }

    /// t-strings are 3.14 syntax, so the target has to be one that can run them
    #[test]
    fn tstring_dedent() {
        check_at(
            crate::PythonVersion::PY314,
            indoc! {r#"
                a = "asdf"
                text = t"""
                    start-of-line{a}
                    |
                    """
            "#},
            indoc! {r#"
                a = "asdf"
                text = t"""\
                start-of-line{a}
                |\
                """
            "#},
        );
    }

    /// a raw literal has no line continuation, its backslash and newline both
    /// being text, so it is written as the plain literal holding the same text
    #[test]
    fn rstring_dedent() {
        check(
            indoc! {r#"
                text = r"""
                    \d+ "quoted"
                    |\
                    """
            "#},
            indoc! {r#"
                text = """\
                \\d+ \"quoted\"
                |\\\
                """
            "#},
        );
    }

    /// only the literal text of a raw f-string is escaped: a replacement field
    /// is code, and the literal text of its format spec is raw too
    #[test]
    fn raw_fstring_dedent() {
        check(
            indoc! {r#"
                a = "asdf"
                text = Rf"""
                    \w{a!r:\>10}
                    """
            "#},
            indoc! {r#"
                a = "asdf"
                text = f"""\
                \\w{a!r:\\>10}\
                """
            "#},
        );
    }

    /// the quotes that close a literal are its own, whatever other quotes it holds
    #[test]
    fn single_quoted_triple_holding_double_quotes() {
        check(
            indoc! {r#"
                text = '''
                    say """hi"""
                    '''
            "#},
            indoc! {r#"
                text = '''\
                say """hi"""\
                '''
            "#},
        );
    }

    #[test]
    fn multiline_content() {
        check(
            indoc! {r#"
                text = """
                    line 1
                    line 2
                    """
            "#},
            indoc! {r#"
                text = """\
                line 1
                line 2\
                """
            "#},
        );
    }

    #[test]
    fn single_quoted_triple() {
        check(
            "text = '''\n    hello\n    '''\n",
            "text = '''\\\nhello\\\n'''\n",
        );
    }

    #[test]
    fn single_quoted_unchanged() {
        check("text = \"hello\"\n", "text = \"hello\"\n");
    }

    #[test]
    fn not_indented_unchanged() {
        // no common indent → no transform
        check(
            "text = \"\"\"\nhello\n\"\"\"\n",
            "text = \"\"\"\nhello\n\"\"\"\n",
        );
    }

    #[test]
    fn inline_triple_unchanged() {
        // no leading newline → no transform
        check("text = \"\"\"hello\"\"\"\n", "text = \"\"\"hello\"\"\"\n");
    }

    #[test]
    fn unicode_prefix_dedented() {
        check(
            "text = u\"\"\"\n    hello\n    \"\"\"\n",
            "text = u\"\"\"\\\nhello\\\n\"\"\"\n",
        );
    }

    #[test]
    fn closing_more_indented_errors() {
        let err = transpile(
            indoc! {r#"
                text = """
                  asdf
                    """
            "#},
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            err.contains("closing") && err.contains("indented more"),
            "got: {err}"
        );
    }

    #[test]
    fn closing_more_indented_zero_content_indent_errors() {
        let err = transpile(
            indoc! {r#"
                text = """
                asdf
                    """
            "#},
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(err.contains("indented more"), "got: {err}");
    }

    /// the newline a `\\` ending the last line continues past is the one the dedent drops
    #[test]
    fn a_line_continuation_ending_the_last_line_errors() {
        for source in [
            "text = \"\"\"\n    a\\\n    \"\"\"\n",
            "text = \"\"\"\n    a\\\\\\\n    \"\"\"\n",
            "text = f\"\"\"\n    {a}\\\n    \"\"\"\n",
        ] {
            let err = transpile(source, &Config::test_default()).unwrap_err();
            assert!(err.contains("line continuation"), "{source:?} got: {err}");
        }
    }

    /// an escaped backslash is no line continuation, and a raw string has none
    #[test]
    fn a_backslash_ending_the_last_line_as_text() {
        check(
            "text = \"\"\"\n    a\\\\\n    \"\"\"\n",
            "text = \"\"\"\\\na\\\\\\\n\"\"\"\n",
        );
        check(
            "text = r\"\"\"\n    a\\\n    \"\"\"\n",
            "text = \"\"\"\\\na\\\\\\\n\"\"\"\n",
        );
    }

    #[test]
    fn closing_equal_indent_ok() {
        // closing at exactly the content's min indent — fine, dedents to col 0
        check(
            indoc! {r#"
                text = """
                    asdf
                    """
            "#},
            indoc! {r#"
                text = """\
                asdf\
                """
            "#},
        );
    }

    #[test]
    fn closing_more_indented_blank_content_unchanged() {
        // no real text inside the triple; the over-indent rule doesn't apply
        check(
            "text = \"\"\"\n\n    \"\"\"\n",
            "text = \"\"\"\n\n    \"\"\"\n",
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged(indoc! {r#"
            text = """
                hello
                """
        "#});
    }
}
