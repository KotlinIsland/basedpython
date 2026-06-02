//! Runtime lowering for custom string tags (`tag"..."`).
//!
//! The parser models `tag"..."` as an `ExprCall` carrying `is_string_tag:
//! true`, whose single argument is the abutting string parsed as a t-string
//! (so interpolations are structured). The desugaring is uniform — a tag
//! always receives a `Template` — regardless of whether the literal
//! interpolates:
//!
//! ```by
//! a = greet"hello"
//! b = greet"hi {name}"
//! ```
//!
//! On Python 3.14+, `Template` is `string.templatelib.Template` and `t"..."`
//! is native, so the call lowers to `tag(t"...")` with two narrow edits (a
//! `(t` insertion before the quote and a `)` after the close) that keep the
//! literal's source verbatim — any sibling lowering inside an interpolation
//! still applies.
//!
//! Below 3.14 there is no runtime t-string, so the literal is rewritten to an
//! explicit `_Template(...)` constructor over a polyfill with the same
//! `strings` / `interpolations` shape. The polyfill classes are
//! underscore-prefixed so they never collide with a user's own `Template`
//! import. Literal segments become string arguments and each replacement field
//! becomes an `_Interpolation(value, "source", conversion, "format_spec")`. The
//! interpolation `value` passes through as source so inner lowerings still
//! compose.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    ConversionFlag, Expr, ExprCall, InterpolatedStringElement, ModModule, PythonVersion, Stmt,
};
use ruff_text_size::{Ranged, TextRange};

use crate::Config;

use super::ast_driver::{AstPass, Fragment, PassContext};

/// PEP 750 `Template` / `Interpolation` polyfill for runtimes before 3.14.
///
/// matches the `string.templatelib` shape a tag relies on: `Template.strings`
/// is the literal segments (always one more than the interpolations),
/// `Template.interpolations` is the replacement fields, and `Template.values`
/// is their evaluated values. iterating a `Template` yields the segments and
/// interpolations interleaved in source order, the same as the stdlib type
pub(crate) const TEMPLATE_RUNTIME: &str = "\
class _Interpolation:
    def __init__(self, value, expression, conversion=None, format_spec=\"\"):
        self.value = value
        self.expression = expression
        self.conversion = conversion
        self.format_spec = format_spec


class _Template:
    def __init__(self, *args):
        strings = []
        interpolations = []
        if not args or isinstance(args[-1], _Interpolation):
            args = (*args, \"\")
        pending = \"\"
        for arg in args:
            if isinstance(arg, _Interpolation):
                strings.append(pending)
                pending = \"\"
                interpolations.append(arg)
            else:
                pending += arg
        strings.append(pending)
        self.strings = tuple(strings)
        self.interpolations = tuple(interpolations)

    @property
    def values(self):
        return tuple(i.value for i in self.interpolations)

    def __iter__(self):
        for index, string in enumerate(self.strings):
            if string:
                yield string
            if index < len(self.interpolations):
                yield self.interpolations[index]
";

pub(crate) struct StringTagPass<'src> {
    source: &'src str,
    config: Config,
}

impl<'src> StringTagPass<'src> {
    pub(crate) fn new(source: &'src str, config: Config) -> Self {
        Self { source, config }
    }
}

impl AstPass for StringTagPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let native = self.config.min_version >= PythonVersion::PY314;
        let mut state = State {
            source: self.source,
            native,
            text_edits: Vec::new(),
            template_edits: Vec::new(),
            used_polyfill: false,
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        ctx.text_edits.extend(state.text_edits);
        ctx.template_edits.extend(state.template_edits);
        if state.used_polyfill {
            ctx.required_imports.push(TEMPLATE_RUNTIME.to_owned());
        }
    }
}

struct State<'src> {
    source: &'src str,
    native: bool,
    text_edits: Vec<(TextRange, String)>,
    template_edits: Vec<(TextRange, Vec<Fragment>)>,
    used_polyfill: bool,
}

impl State<'_> {
    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// the abutting template literal of a string-tag call. the parser stores it
    /// as the call's single positional argument.
    fn tag_template(call: &ExprCall) -> Option<&Expr> {
        if !call.is_string_tag {
            return None;
        }
        match (
            call.arguments.args.as_ref(),
            call.arguments.keywords.as_ref(),
        ) {
            ([arg], []) => Some(arg),
            _ => None,
        }
    }

    fn lower(&mut self, call: &ExprCall) {
        let Some(template) = Self::tag_template(call) else {
            return;
        };
        let Expr::TString(tstring) = template else {
            return;
        };
        let func_end = call.func.range().end();
        let lit_range = tstring.range();
        if self.native {
            // wrap the verbatim literal: `tag` `(t` `"..."` `)`. two narrow
            // edits keep the source bytes between them, so a lowering inside an
            // interpolation still applies
            self.text_edits
                .push((TextRange::empty(func_end), "(t".to_owned()));
            self.text_edits
                .push((TextRange::empty(call.range().end()), ")".to_owned()));
            return;
        }

        // build `tag(_Template(<parts>))` over the whole call range. the call
        // already reads `tag` then the literal, so replace it wholesale; the
        // wide replacement covers `func` and the literal together
        let _ = lit_range;
        let mut frags: Vec<Fragment> = vec![Fragment::Lit(format!(
            "{}(_Template(",
            self.src(call.func.range())
        ))];
        let mut first = true;
        // a single-part t-string is the common case; concatenated parts iterate
        // their elements in order, which is still correct for the flat
        // strings/interpolations model
        for part in &tstring.value {
            for element in &part.elements {
                if !first {
                    frags.push(Fragment::Lit(", ".to_owned()));
                }
                first = false;
                match element {
                    InterpolatedStringElement::Literal(lit) => {
                        frags.push(Fragment::Lit(string_repr(&lit.value)));
                    }
                    InterpolatedStringElement::Interpolation(interp) => {
                        frags.push(Fragment::Lit("_Interpolation(".to_owned()));
                        // value passes through as source so inner lowerings compose
                        frags.push(Fragment::Src(interp.expression.range()));
                        frags.push(Fragment::Lit(format!(
                            ", {}",
                            string_repr(self.src(interp.expression.range()))
                        )));
                        let conversion = conversion_arg(interp.conversion);
                        let format_spec = interp
                            .format_spec
                            .as_ref()
                            .map(|spec| self.format_spec_text(spec.range()));
                        // only emit trailing optional args when needed, keeping
                        // the common no-conversion no-spec form to two args
                        if let Some(spec) = format_spec {
                            frags.push(Fragment::Lit(format!(
                                ", {conversion}, {}",
                                string_repr(&spec)
                            )));
                        } else if interp.conversion != ConversionFlag::None {
                            frags.push(Fragment::Lit(format!(", {conversion}")));
                        }
                        frags.push(Fragment::Lit(")".to_owned()));
                    }
                }
            }
        }
        frags.push(Fragment::Lit("))".to_owned()));
        self.template_edits.push((call.range(), frags));
        self.used_polyfill = true;
    }

    /// the format-spec source between the `:` and the closing `}`, with the
    /// leading colon dropped
    fn format_spec_text(&self, range: TextRange) -> String {
        self.src(range)
            .strip_prefix(':')
            .unwrap_or_else(|| self.src(range))
            .to_owned()
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && call.is_string_tag
        {
            self.lower(call);
            // still descend so a nested tag inside an interpolation lowers too
        }
        walk_expr(self, expr);
    }
}

/// render `value` as a python string literal. defers to a conservative escaper
/// that always emits a double-quoted form so the result re-lexes as one token
fn string_repr(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// the PEP 750 conversion argument: `None`, or the conversion char as a string
fn conversion_arg(flag: ConversionFlag) -> String {
    match flag {
        ConversionFlag::None => "None".to_owned(),
        ConversionFlag::Str => "\"s\"".to_owned(),
        ConversionFlag::Repr => "\"r\"".to_owned(),
        ConversionFlag::Ascii => "\"a\"".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, PythonVersion, transpile};

    /// transpile at 3.14, where `Template` and `t"..."` are native
    fn native(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY314,
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
    }

    /// transpile at the default 3.10 target, where the polyfill is injected.
    /// the polyfill class is prepended and separated from the body by a blank
    /// line, the same as other injected runtime classes
    fn polyfilled(input: &str, expected_body: &str) {
        let out = transpile(input, &Config::test_default()).unwrap();
        let body = out
            .strip_prefix(super::TEMPLATE_RUNTIME)
            .and_then(|rest| rest.strip_prefix('\n'))
            .unwrap_or_else(|| panic!("template polyfill not prepended; got:\n{out}"));
        assert_eq!(body, expected_body);
    }

    #[test]
    fn native_non_interpolating() {
        native("a = greet\"hello\"\n", "a = greet(t\"hello\")\n");
    }

    #[test]
    fn native_interpolating() {
        native("b = greet\"hi {name}\"\n", "b = greet(t\"hi {name}\")\n");
    }

    #[test]
    fn native_multiple_fields() {
        native(
            "q = sql\"select {a} from {b}\"\n",
            "q = sql(t\"select {a} from {b}\")\n",
        );
    }

    #[test]
    fn polyfill_non_interpolating() {
        polyfilled("a = greet\"hello\"\n", "a = greet(_Template(\"hello\"))\n");
    }

    #[test]
    fn polyfill_interpolating() {
        polyfilled(
            "b = greet\"hi {name}\"\n",
            "b = greet(_Template(\"hi \", _Interpolation(name, \"name\")))\n",
        );
    }

    #[test]
    fn polyfill_multiple_fields() {
        polyfilled(
            "q = sql\"select {a} from {b}\"\n",
            "q = sql(_Template(\"select \", _Interpolation(a, \"a\"), \" from \", _Interpolation(b, \"b\")))\n",
        );
    }

    #[test]
    fn polyfill_conversion_flag() {
        polyfilled(
            "x = tag\"{v!r}\"\n",
            "x = tag(_Template(_Interpolation(v, \"v\", \"r\")))\n",
        );
    }

    #[test]
    fn polyfill_format_spec() {
        polyfilled(
            "x = tag\"{v:>10}\"\n",
            "x = tag(_Template(_Interpolation(v, \"v\", None, \">10\")))\n",
        );
    }

    #[test]
    fn polyfill_only_injected_once() {
        let out = transpile("a = x\"1\"\nb = y\"2\"\n", &Config::test_default()).unwrap();
        assert_eq!(out.matches("class _Template:").count(), 1);
    }

    // `sqlr` lexes greedily as a single tag name, not `sql` + raw
    #[test]
    fn greedy_tag_name() {
        native("q = sqlr\"raw {z}\"\n", "q = sqlr(t\"raw {z}\")\n");
    }

    // builtin string prefixes stay builtin strings, never tags
    #[test]
    fn builtin_prefixes_unchanged() {
        unchanged("a = f\"{x}\"\n");
        unchanged("a = rb\"bytes\"\n");
        unchanged("a = t\"{x}\"\n");
    }

    // a one-letter function named `f` is not tag-callable: `f"x"` is an f-string
    #[test]
    fn one_letter_f_is_fstring() {
        unchanged("f\"{x}\"\n");
    }

    // a plain call with a t-string argument is not a tag and is left alone at 3.14
    #[test]
    fn explicit_call_with_tstring_native() {
        native("q = sql(t\"x\")\n", "q = sql(t\"x\")\n");
    }
}
