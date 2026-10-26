//! Reverse of `crate::transforms::dedent_string`:
//!   `"""\\\nsome-text\\\n"""` → `"""\n    some-text\n    """`
//!   `"""\\\n  some-text"""`   → `"""\n    some-text\n    """`
//!
//! two forms are detected:
//! - form 1: opening `\<newline>`, last content line ends `\<newline>` (our forward-transform output)
//! - form 2: opening `\<newline>`, closing `"""` sits immediately after content on the same line
//!
//! in both cases content's common leading whitespace is stripped and replaced
//! with the containing line's indentation plus four spaces

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StringFlags};
use ruff_text_size::Ranged;

pub(crate) struct IndentString<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> IndentString<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn check(&mut self, expr: &dyn Ranged) {
        let start = usize::from(expr.range().start());
        let end = usize::from(expr.range().end());
        let raw = &self.source[start..end];

        let line_start = self.source[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_indent = leading_whitespace(&self.source[line_start..]);

        if let Some(transformed) = indent_triple_string(raw, line_indent) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                transformed,
                expr.range(),
            )));
        }
    }
}

impl<'ast> Visitor<'ast> for IndentString<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::StringLiteral(s) => {
                if let Some(part) = s.as_single_part_string() {
                    if part.flags.is_triple_quoted() {
                        self.check(s);
                    }
                }
            }
            Expr::FString(s) => {
                if let Some(f) = s.as_single_part_fstring() {
                    if f.flags.is_triple_quoted() {
                        self.check(s);
                    }
                }
            }
            Expr::TString(s) => {
                if let Some(t) = s.as_single_part_tstring() {
                    if t.flags.is_triple_quoted() {
                        self.check(s);
                    }
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

fn indent_triple_string(raw: &str, line_indent: &str) -> Option<String> {
    let quote_start = raw.find("\"\"\"").or_else(|| raw.find("'''"))?;
    let prefix = &raw[..quote_start];
    // a raw string has no line continuation: the backslash and newline after its opening
    // quotes are both text
    if prefix.contains(['r', 'R']) {
        return None;
    }
    let quote = &raw[quote_start..quote_start + 3];
    let after_open = &raw[quote_start + 3..];

    if !after_open.ends_with(quote) {
        return None;
    }

    // opening must be immediately followed by \<newline>
    if !after_open.starts_with("\\\n") {
        return None;
    }

    let inner_end = after_open.len() - 3;
    let inner = &after_open[2..inner_end]; // strip leading \<newline>

    // form 1: last content line ends with \<newline> (our forward-transform output)
    //   content is already stripped of indent; re-indent with line_indent + 4
    // form 2: closing """ sits right after the last content character (no trailing newline)
    //   content's leading whitespace is meaningful (string value) — preserve as-is
    // anything else (bare \n before closing """) is left alone
    // the `\\` before the closing line's newline is a line continuation only when it is not
    // the second half of an escaped backslash; an escaped one holds a backslash and a newline
    if let Some(content) = inner.strip_suffix("\\\n")
        && (content.len() - content.trim_end_matches('\\').len()) % 2 == 0
    {
        let indent = format!("{line_indent}    ");
        let indented_lines: Vec<String> = content
            .split('\n')
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    format!("{indent}{l}")
                }
            })
            .collect();
        let indented_content = indented_lines.join("\n");
        Some(format!(
            "{prefix}{quote}\n{indented_content}\n{indent}{quote}"
        ))
    } else if !inner.ends_with('\n') {
        // form 2: preserve content verbatim, close at statement indent level
        Some(format!("{prefix}{quote}\n{inner}\n{line_indent}{quote}"))
    } else {
        None
    }
}

fn leading_whitespace(s: &str) -> &str {
    let end = s.find(|c: char| c != ' ' && c != '\t').unwrap_or(s.len());
    &s[..end]
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    // basic forms

    #[test]
    fn basic_indent() {
        check(
            "text = \"\"\"\\\nsome-text\\\n\"\"\"\n",
            indoc! {r#"
                text = """
                    some-text
                    """
            "#},
        );
    }

    #[test]
    fn multiline_content() {
        check(
            "text = \"\"\"\\\nline 1\nline 2\\\n\"\"\"\n",
            indoc! {r#"
                text = """
                    line 1
                    line 2
                    """
            "#},
        );
    }

    #[test]
    fn single_quoted_triple() {
        check(
            "text = '''\\\nhello\\\n'''\n",
            "text = '''\n    hello\n    '''\n",
        );
    }

    // form 2: closing """ on same line as last content character

    #[test]
    fn form2_basic() {
        // leading whitespace is part of string value — preserved verbatim
        // closing """ returns to statement indent level (col 0 here)
        check(
            "text = \"\"\"\\\n  some-text\"\"\"\n",
            "text = \"\"\"\n  some-text\n\"\"\"\n",
        );
    }

    #[test]
    fn form2_no_content_indent() {
        check(
            "text = \"\"\"\\\nsome-text\"\"\"\n",
            "text = \"\"\"\nsome-text\n\"\"\"\n",
        );
    }

    #[test]
    fn form2_multiline() {
        // all content lines preserved as-is
        check(
            "text = \"\"\"\\\n  line 1\n  line 2\"\"\"\n",
            "text = \"\"\"\n  line 1\n  line 2\n\"\"\"\n",
        );
    }

    #[test]
    fn form2_nested_in_function() {
        // closing """ indented to match statement (4 spaces)
        check(
            "def foo():\n    text = \"\"\"\\\n  some-text\"\"\"\n",
            "def foo():\n    text = \"\"\"\n  some-text\n    \"\"\"\n",
        );
    }

    // prefixed strings

    #[test]
    fn fstring_indent() {
        check(
            "a = \"asdf\"\ntext = f\"\"\"\\\nstart{a}\\\n\"\"\"\n",
            indoc! {r#"
                a = "asdf"
                text = f"""
                    start{a}
                    """
            "#},
        );
    }

    #[test]
    fn tstring_indent() {
        check(
            "a = \"asdf\"\ntext = t\"\"\"\\\nstart{a}\\\n\"\"\"\n",
            indoc! {r#"
                a = "asdf"
                text = t"""
                    start{a}
                    """
            "#},
        );
    }

    /// a raw string's backslashes are text, so it holds no line continuation to read
    #[test]
    fn rstring_unchanged() {
        let source = "text = r\"\"\"\\\nhello\\\n\"\"\"\n";
        check(source, source);
    }

    /// a `\\` before the closing line is an escaped backslash, and the newline after it
    /// is part of the string
    #[test]
    fn escaped_backslash_before_the_closing_line_unchanged() {
        let source = "text = \"\"\"\\\nhello\\\\\n\"\"\"\n";
        check(source, source);
    }

    #[test]
    fn unicode_prefix_indent() {
        check(
            "text = u\"\"\"\\\nhello\\\n\"\"\"\n",
            "text = u\"\"\"\n    hello\n    \"\"\"\n",
        );
    }

    // indentation context

    #[test]
    fn nested_in_function() {
        // forward transform strips all content indent → content lands at col 0
        // reverse re-indents relative to the containing line (4 spaces → 8 spaces)
        check(
            "def foo():\n    text = \"\"\"\\\nsome-text\\\n\"\"\"\n",
            indoc! {r#"
                def foo():
                    text = """
                        some-text
                        """
            "#},
        );
    }

    // no-match cases

    #[test]
    fn inline_triple_unchanged() {
        // no \<newline> after opening — not the python polyfill form
        check("text = \"\"\"hello\"\"\"\n", "text = \"\"\"hello\"\"\"\n");
    }

    #[test]
    fn plain_newline_after_open_unchanged() {
        // newline but no backslash — already in basedpython form, leave alone
        check(
            "text = \"\"\"\nhello\n\"\"\"\n",
            "text = \"\"\"\nhello\n\"\"\"\n",
        );
    }

    #[test]
    fn bare_newline_before_close_unchanged() {
        // opening has \<newline> but content ends with bare \n before closing """
        // (not form 1 or form 2 — leave alone)
        check(
            "text = \"\"\"\\\nhello\n\"\"\"\n",
            "text = \"\"\"\\\nhello\n\"\"\"\n",
        );
    }

    #[test]
    fn single_quoted_non_triple_unchanged() {
        check("text = \"hello\"\n", "text = \"hello\"\n");
    }

    // round-trip: transpile(reverse_transpile(py)) matches transpile(basedpython)

    #[test]
    fn round_trip_basic() {
        let py = "text = \"\"\"\\\nsome-text\\\n\"\"\"\n";
        let bp = indoc! {r#"
            text = """
                some-text
                """
        "#};
        let config = &crate::Config::default();
        let from_py = crate::transpile(&reverse_transpile(py, config).unwrap(), config).unwrap();
        let from_bp = crate::transpile(bp, config).unwrap();
        assert_eq!(from_py, from_bp);
    }

    #[test]
    fn round_trip_multiline() {
        let py = "text = \"\"\"\\\nline 1\nline 2\\\n\"\"\"\n";
        let bp = indoc! {r#"
            text = """
                line 1
                line 2
                """
        "#};
        let config = &crate::Config::default();
        let from_py = crate::transpile(&reverse_transpile(py, config).unwrap(), config).unwrap();
        let from_bp = crate::transpile(bp, config).unwrap();
        assert_eq!(from_py, from_bp);
    }
}
