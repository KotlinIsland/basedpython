//! reverse of `crate::transforms::string_tag`:
//!   `tag(t"...")` → `tag"..."`
//!
//! the forward transform lowers an abutting tag `tag"..."` to a call
//! `tag(t"...")` (the native form on 3.14+). the reverse re-sugars that exact
//! shape: a bare-name or attribute callee applied to a single positional
//! t-string argument, with no keywords.
//!
//! there is no provenance side-channel in the produced python, so the shape is
//! the only signal that a call originated as a tag. re-sugaring is left off
//! whenever it would not round-trip identically:
//!
//!  - a raw t-string (`rt"..."`) can't be re-sugared, since dropping the `t`
//!    would leave a raw string, not a tag — the forward transform only ever
//!    emits a plain `t"..."`
//!  - a callee whose name is itself a builtin string prefix (`f`, `rb`, `t`, …)
//!    is left alone: `f(t"x")` must not become `f"x"`, which is an f-string
//!
//! both directions agree on the same boundary, so a forward-then-reverse (or
//! reverse-then-forward) pass is stable.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprCall, Stmt};
use ruff_text_size::{Ranged, TextRange};

pub(crate) struct StringTagReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> StringTagReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// the re-sugared `tag"..."` text for a call, or `None` if the call is not
    /// a round-trippable tag shape
    fn resugar(&self, call: &ExprCall) -> Option<String> {
        // a callee the quote can be glued to: a bare name, or an attribute
        // whose trailing name is what carries the tag. either way the name the
        // quote lands on must not be a builtin string prefix
        let tag_name = match call.func.as_ref() {
            Expr::Name(name) => name.id.as_str(),
            Expr::Attribute(attribute) => attribute.attr.as_str(),
            _ => return None,
        };
        if is_builtin_string_prefix(tag_name) {
            return None;
        }
        // exactly one positional argument, a t-string, no keywords
        if !call.arguments.keywords.is_empty() {
            return None;
        }
        let [arg] = call.arguments.args.as_ref() else {
            return None;
        };
        let Expr::TString(tstring) = arg else {
            return None;
        };
        // a single, non-raw t-string part. dropping the `t` prefix leaves the
        // bare string the tag abuts
        let part = tstring.as_single_part_tstring()?;
        if part.flags.prefix().is_raw() {
            return None;
        }
        let literal = self.src(arg.range());
        let body = literal
            .strip_prefix('t')
            .or_else(|| literal.strip_prefix('T'))?;
        Some(format!("{}{body}", self.src(call.func.range())))
    }
}

impl<'ast> Visitor<'ast> for StringTagReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && let Some(replacement) = self.resugar(call)
        {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                replacement,
                expr.range(),
            )));
            // descend so a nested tag inside an argument also re-sugars
        }
        walk_expr(self, expr);
    }
}

/// whether `name` is exactly a valid builtin string-prefix combination, which
/// the lexer would read as a string prefix rather than a tag name. mirrors the
/// lexer's `try_single_char_prefix` / `try_double_char_prefix` acceptance
fn is_builtin_string_prefix(name: &str) -> bool {
    let mut chars = name.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(a), None, _) => {
            matches!(a, 'f' | 'F' | 't' | 'T' | 'u' | 'U' | 'b' | 'B' | 'r' | 'R')
        }
        (Some(a), Some(b), None) => {
            let lower = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
            matches!(lower, ('r', 'f' | 't' | 'b') | ('f' | 't' | 'b', 'r'))
        }
        _ => false,
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

    #[test]
    fn non_interpolating() {
        check("a = greet(t\"hello\")\n", "a = greet\"hello\"\n");
    }

    #[test]
    fn interpolating() {
        check("b = greet(t\"hi {name}\")\n", "b = greet\"hi {name}\"\n");
    }

    #[test]
    fn multiple_fields() {
        check(
            "q = sql(t\"select {a} from {b}\")\n",
            "q = sql\"select {a} from {b}\"\n",
        );
    }

    // a builtin-prefix-named callee must not be re-sugared: `f(t"x")` stays a
    // call, since `f"x"` would be an f-string
    #[test]
    fn builtin_prefix_callee_unchanged() {
        check("f(t\"x\")\n", "f(t\"x\")\n");
        check("rb(t\"x\")\n", "rb(t\"x\")\n");
    }

    // a raw t-string can't be re-sugared (dropping `t` would leave a raw string)
    #[test]
    fn raw_tstring_unchanged() {
        check("q = sql(rt\"raw\")\n", "q = sql(rt\"raw\")\n");
    }

    // a call with extra arguments is not a tag shape
    #[test]
    fn extra_argument_unchanged() {
        check("q = sql(t\"x\", 1)\n", "q = sql(t\"x\", 1)\n");
    }

    // a plain string argument (not a t-string) is not a tag
    #[test]
    fn plain_string_unchanged() {
        check("q = sql(\"x\")\n", "q = sql(\"x\")\n");
    }

    // an attribute callee carries a tag just as a bare name does
    #[test]
    fn attribute_callee() {
        check(
            "line = doc.text(t\"hi {who}\")\n",
            "line = doc.text\"hi {who}\"\n",
        );
    }

    // the name the quote lands on is the attribute, so that is what the
    // builtin-prefix rule applies to
    #[test]
    fn attribute_named_builtin_prefix_unchanged() {
        check("doc.f(t\"x\")\n", "doc.f(t\"x\")\n");
    }

    // round-trip stability: re-sugared output forward-transpiles back to the
    // same call shape (checked at 3.14 for the native form)
    #[test]
    fn round_trips_through_forward() {
        use crate::{PythonVersion, transpile};
        let py = "q = sql(t\"select {x}\")\n";
        let by = reverse_transpile(py, &Config::test_default()).unwrap();
        assert_eq!(by, "q = sql\"select {x}\"\n");
        let config = Config {
            min_version: PythonVersion::PY314,
            ..Config::test_default()
        };
        assert_eq!(transpile(&by, &config).unwrap(), py);
    }

    #[test]
    fn attribute_round_trips_through_forward() {
        use crate::{PythonVersion, transpile};
        let py = "line = doc.text(t\"hi {who}\")\n";
        let by = reverse_transpile(py, &Config::test_default()).unwrap();
        assert_eq!(by, "line = doc.text\"hi {who}\"\n");
        let config = Config {
            min_version: PythonVersion::PY314,
            ..Config::test_default()
        };
        assert_eq!(transpile(&by, &config).unwrap(), py);
    }
}
