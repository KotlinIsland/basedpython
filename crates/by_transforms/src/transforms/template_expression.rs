//! a t-string field hands a reader its source text, which has to be the author's
//!
//! python 3.14 gives every replacement field of a `t"..."` to the program as a
//! `string.templatelib.Interpolation`, whose `expression` attribute is the *source text*
//! of the field: everything between the `{` and the end of the expression, its grouping
//! parentheses kept and its trailing whitespace dropped.
//!
//! ```python
//! a = 3
//! t"{ (a) !r}".interpolations[0].expression   # ' (a)'
//! ```
//!
//! that text is what the attribute is for — a template library names a cache entry by it,
//! shows it in an error, or renders a `=`-style debug view from it. python reads it off
//! the file it compiled, which here is the lowered one, so `t"{a!}"` handed a reader
//! `_force_unwrap(a)`. that is not what the author wrote, and it is not something the
//! reader could evaluate either: the helper it names is private to the generated module.
//! so the author's text is the answer, the same answer
//! [`debug_field`](super::debug_field) reached for `f"{x = }"`
//!
//! there is no way to spell "this field's text is that" in a t-string literal, because
//! python derives it from the source. a t-string whose fields a lowering reached is
//! therefore rebuilt once, at runtime: [`_by_template`] takes the lowered template and
//! the author's text for each field it changed, and returns a template carrying the same
//! values under the author's own text. a t-string nothing lowered is left exactly as
//! written, and so is every field of a rebuilt one that nothing lowered — those keep the
//! interpolation python built, so their text stays byte-identical to python's own
//!
//! a `tag"..."` literal is a t-string only at 3.14, where the tag is called with it. the
//! source writes it without the `t`, so the prefix is written here too — inside the rebuild
//! when there is one, and on its own when there is not. below 3.14 it is not a t-string in
//! the output at all: [`string_tag`](super::string_tag) builds its template explicitly, and
//! gives each field the author's text as it does
//!
//! [`_by_template`]: crate::runtime

use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::visitor::transformer::{self, Transformer};
use ruff_python_ast::{
    Arguments, AtomicNodeIndex, Expr, ExprCall, ExprName, ExprNoneLiteral, ExprTString, ExprTuple,
    InterpolatedElement, ModModule, Stmt,
};
use ruff_python_ast::{ExprContext, name::Name};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::Fragment;
use super::debug_field::rewrites_expression;
use super::source_util::string_repr;
use crate::runtime;

/// a t-string to rebuild, and what each of its fields is to say it was written as
///
/// `texts` runs parallel to the template's interpolations at runtime. `None` is a field
/// nothing lowered, which keeps the interpolation python built for it
pub(crate) struct Wrap {
    /// the t-string literal, as the author's source spans it
    pub(crate) range: TextRange,
    texts: Vec<Option<String>>,
    /// the literal is a string tag's, which the source writes without its `t`
    pub(crate) tagged: bool,
}

impl Wrap {
    /// the call that rebuilds this template, re-emitting the literal from source so a
    /// lowering inside any of its fields still applies
    pub(crate) fn fragments(&self) -> Vec<Fragment> {
        let prefix = if self.tagged { "t" } else { "" };
        if !self.rebuilds() {
            return vec![Fragment::Lit(prefix.to_owned()), Fragment::Src(self.range)];
        }
        vec![
            Fragment::Lit(format!("{}({prefix}", runtime::TEMPLATE_TEXT.name())),
            Fragment::Src(self.range),
            Fragment::Lit(format!(", {})", self.texts_tuple())),
        ]
    }

    /// whether the template is rebuilt at runtime: a field of it was lowered. a string tag's
    /// literal nothing lowered only has its `t` written
    pub(crate) fn rebuilds(&self) -> bool {
        self.texts.iter().any(Option::is_some)
    }

    /// the author's texts as a python tuple literal
    ///
    /// a template that reached this pass has at least one field, and a one-element tuple
    /// needs a trailing comma to be a tuple at all
    fn texts_tuple(&self) -> String {
        let mut out = String::from("(");
        for (index, text) in self.texts.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            match text {
                Some(text) => out.push_str(&string_repr(text)),
                None => out.push_str("None"),
            }
        }
        if self.texts.len() == 1 {
            out.push(',');
        }
        out.push(')');
        out
    }

    /// the same call as a syntax tree, for a statement the driver re-renders
    fn call(&self, template: Expr) -> Expr {
        let texts = self
            .texts
            .iter()
            .map(|text| match text {
                Some(text) => Expr::StringLiteral(super::source_util::string_literal(text)),
                None => Expr::NoneLiteral(ExprNoneLiteral {
                    node_index: AtomicNodeIndex::NONE,
                    range: TextRange::default(),
                }),
            })
            .collect();
        Expr::Call(ExprCall {
            node_index: AtomicNodeIndex::NONE,
            range_start: TextSize::default(),
            func: Box::new(Expr::Name(ExprName {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                id: Name::from(runtime::TEMPLATE_TEXT.name()),
                ctx: ExprContext::Load,
            })),
            arguments: Arguments {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
                args: Box::new([
                    template,
                    Expr::Tuple(ExprTuple {
                        node_index: AtomicNodeIndex::NONE,
                        range: TextRange::default(),
                        elts: texts,
                        ctx: ExprContext::Load,
                        parenthesized: true,
                        is_anon_named_tuple: false,
                        is_anon_named_tuple_value: false,
                        callable_shape: None,
                        is_parameter_shape: false,
                    }),
                ]),
                keywords: thin_vec::ThinVec::new(),
            },
            cast_kind: None,
            is_string_tag: false,
        })
    }
}

/// the source text python hands a reader as a field's `expression`
///
/// python takes it from just past the `{` to the end of the expression, keeping whatever
/// grouping parentheses the expression was written inside and dropping the whitespace
/// after them. the expression node's own range holds neither, so both ends are read off
/// the field: the start from its brace, and the end by stepping over the parentheses that
/// close between the expression's last token and the `=`, conversion, format spec or `}`
/// that ends the field — nothing else can stand there
pub(crate) fn author_text<'src>(source: &'src str, field: &InterpolatedElement) -> &'src str {
    let start = usize::from(field.range().start()) + '{'.len_utf8();
    let expression_end = usize::from(field.expression.range().end());
    let Some(tail) = source.get(expression_end..usize::from(field.range().end())) else {
        return "";
    };
    let mut closing = 0;
    for (offset, ch) in tail.char_indices() {
        match ch {
            ')' => closing = offset + ch.len_utf8(),
            ch if ch.is_whitespace() => {}
            _ => break,
        }
    }
    source
        .get(start..expression_end + closing)
        .unwrap_or_default()
}

/// a t-string of the source, the interpolations it hands a reader in the order it hands
/// them over, and whether it is a string tag's
type Template<'a> = (&'a ExprTString, Vec<&'a InterpolatedElement>, bool);

/// every t-string of `suite` whose text a reader would get from the lowering
///
/// a `tag"..."` template is one only when `tags` says the target calls its tag with a
/// t-string — see the module documentation
fn templates(suite: &[Stmt], tags: bool) -> Vec<Template<'_>> {
    struct Found<'a> {
        templates: Vec<Template<'a>>,
        tagged: Vec<TextRange>,
    }

    impl<'a> SourceOrderVisitor<'a> for Found<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Call(call) if call.is_string_tag => {
                    if let [Expr::TString(tstring)] = &*call.arguments.args {
                        self.tagged.push(tstring.range());
                    }
                }
                Expr::TString(tstring) => {
                    // a format spec's own fields are not interpolations of the template:
                    // python evaluates them where they stand and hands the field one
                    // finished spec string, so only the top level is walked here
                    let fields = tstring
                        .value
                        .iter()
                        .flat_map(|part| part.elements.interpolations())
                        .collect();
                    self.templates.push((tstring, fields, false));
                }
                _ => {}
            }
            source_order::walk_expr(self, expr);
        }
    }

    let mut found = Found {
        templates: Vec::new(),
        tagged: Vec::new(),
    };
    for stmt in suite {
        source_order::walk_stmt(&mut found, stmt);
    }
    let Found {
        mut templates,
        tagged,
    } = found;
    templates.retain_mut(|(tstring, fields, is_tagged)| {
        *is_tagged = tagged.contains(&tstring.range());
        if *is_tagged { tags } else { !fields.is_empty() }
    });
    templates
}

/// the source span of every expression a t-string reports to the program that reads it
///
/// the reverse direction leaves these alone. the `.by` it writes has to report what the
/// python it was given reported, and a `t"{a ?? b}"` says `a ?? b` where the
/// `t"{a if a is not None else b}"` it came from says the conditional. a `tag"..."`
/// template is in here too: its fields reach a reader the same way, whichever of the two
/// shapes [`string_tag`](super::string_tag) lowers it to
pub(crate) fn read_expressions(suite: &[Stmt]) -> Vec<TextRange> {
    struct Read(Vec<TextRange>);

    impl<'a> SourceOrderVisitor<'a> for Read {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::TString(tstring) = expr {
                self.0.extend(
                    tstring
                        .value
                        .iter()
                        .flat_map(|part| part.elements.interpolations())
                        .map(|field| field.expression.range()),
                );
            }
            source_order::walk_expr(self, expr);
        }
    }

    let mut found = Read(Vec::new());
    for stmt in suite {
        source_order::walk_stmt(&mut found, stmt);
    }
    found.0
}

/// the fields of `suite` whose expression an [`AstPass`](super::ast_driver::AstPass)
/// replaced, by the range of the field the author wrote
///
/// read before any later rewrite of a field reaches the tree, so the tree still holds one
/// field per field the author wrote. a field the original has none of at this range is one
/// a pass built for itself, and its text is that pass's own to get right
pub(crate) fn replaced_in_tree(
    module: &ModModule,
    original: &[Stmt],
    changed: &[usize],
) -> Vec<TextRange> {
    let mut replaced = Vec::new();
    for &idx in changed {
        let (Some(before), Some(after)) = (original.get(idx), module.body.get(idx)) else {
            continue;
        };
        let written = templates(std::slice::from_ref(before), true);
        for (tstring, fields, _) in templates(std::slice::from_ref(after), true) {
            let Some((_, was, _)) = written
                .iter()
                .find(|(other, _, _)| other.range() == tstring.range())
            else {
                continue;
            };
            for field in fields {
                if was
                    .iter()
                    .find(|earlier| earlier.range() == field.range())
                    .is_some_and(|earlier| {
                        ComparableExpr::from(&*earlier.expression)
                            != ComparableExpr::from(&*field.expression)
                    })
                {
                    replaced.push(field.range());
                }
            }
        }
    }
    replaced
}

/// every t-string that has to be rebuilt, and the author's text for each of its fields
///
/// `edited` is every range the passes claimed that writes something other than what it
/// covers, and `replaced` the fields [`replaced_in_tree`] found. `tags` says the target
/// calls a string tag with a t-string, whose `t` is then written here whether or not a
/// field needs rebuilding
pub(crate) fn claim(
    source: &str,
    suite: &[Stmt],
    edited: &[TextRange],
    replaced: &[TextRange],
    tags: bool,
) -> Vec<Wrap> {
    let mut wraps = Vec::new();
    for (tstring, fields, tagged) in templates(suite, tags) {
        let texts: Vec<Option<String>> = fields
            .iter()
            .map(|field| {
                let lowered = replaced.contains(&field.range())
                    || edited.iter().any(|edit| {
                        rewrites_expression(*edit, field.expression.range(), field.range())
                    });
                lowered.then(|| author_text(source, field).to_owned())
            })
            .collect();
        if tagged || texts.iter().any(Option::is_some) {
            wraps.push(Wrap {
                range: tstring.range(),
                texts,
                tagged,
            });
        }
    }
    wraps
}

/// rebuild in the syntax tree every t-string of `wraps` that stands in `module`
///
/// the driver re-renders a statement an `AstPass` changed from the tree, where an edit
/// keyed on source ranges does not land — so a t-string in such a statement is wrapped
/// here instead. a t-string keeps its own range while a pass rewrites an expression
/// inside it, which is what matches one to its wrap
pub(crate) fn rewrite_changed(module: &mut ModModule, changed: &[usize], wraps: &[Wrap]) {
    let rewrite = Rewrite { wraps };
    for &idx in changed {
        if let Some(stmt) = module.body.get_mut(idx) {
            rewrite.visit_stmt(stmt);
        }
    }
}

struct Rewrite<'a> {
    wraps: &'a [Wrap],
}

impl Transformer for Rewrite<'_> {
    fn visit_expr(&self, expr: &mut Expr) {
        transformer::walk_expr(self, expr);
        let Expr::TString(tstring) = expr else {
            return;
        };
        let Some(wrap) = self.wraps.iter().find(|wrap| wrap.range == tstring.range()) else {
            return;
        };
        let template = std::mem::replace(
            expr,
            Expr::NoneLiteral(ExprNoneLiteral {
                node_index: AtomicNodeIndex::NONE,
                range: TextRange::default(),
            }),
        );
        *expr = wrap.call(template);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, PythonVersion, transpile};

    /// the one line of the output the source's last line lowered to
    ///
    /// t-strings are 3.14 syntax, so every source here is transpiled for 3.14
    fn lowered(source: &str) -> String {
        let out = transpile(
            source,
            &Config {
                min_version: PythonVersion::PY314,
                ..Config::test_default()
            },
        )
        .unwrap_or_else(|error| panic!("{source:?}: {error}"));
        out.lines()
            .last()
            .expect("some output")
            .trim_start()
            .to_owned()
    }

    /// every operator lowering that can reach inside a t-string field says what the
    /// author wrote rather than what it lowered to
    /// a string tag's literal is written without its `t`, which the rebuild writes inside
    /// it, so the tag is handed the template with the author's texts
    #[test]
    fn a_string_tag_is_handed_the_authors_text() {
        assert_eq!(
            lowered("def tag(t: object) -> object:\n    return t\na: int? = 1\nx = tag\"{a!}\"\n"),
            "x = tag(_by_template(t\"{_force_unwrap(a)}\", (\"a!\",)))"
        );
    }

    /// a string tag's literal nothing lowered keeps python's own template, and only has its
    /// `t` written
    #[test]
    fn a_string_tag_nothing_lowered_is_called_with_the_literal() {
        assert_eq!(
            lowered("def tag(t: object) -> object:\n    return t\na = 1\nx = tag\"{a}\"\n"),
            "x = tag(t\"{a}\")"
        );
    }

    #[test]
    fn an_operator_lowering_reports_the_author_text() {
        // a force-unwrap
        assert_eq!(
            lowered("def go(a: int?):\n    print(t\"{a!}\")\n"),
            "print(_by_template(t\"{_force_unwrap(a)}\", (\"a!\",)))"
        );
        // `??`
        assert_eq!(
            lowered("def go(a: int?, b: int):\n    print(t\"{a ?? b}\")\n"),
            "print(_by_template(t\"{a if a is not None else b}\", (\"a ?? b\",)))"
        );
        // an optional chain
        assert_eq!(
            lowered("def go(a: int?):\n    print(t\"{a?.bit_length()}\")\n"),
            "print(_by_template(t\"{(None if a is None else a.bit_length())}\", \
             (\"a?.bit_length()\",)))"
        );
        // a grapheme accessor, which re-emits its receiver
        assert_eq!(
            lowered("def go(s: str):\n    print(t\"{s.first}\")\n"),
            "print(_by_template(t\"{(Character(_by_graphemes(s)[0]) if s else None)}\", \
             (\"s.first\",)))"
        );
    }

    /// `typeof` is folded in the syntax tree rather than emitted as an edit, so the
    /// statement around it is re-rendered — the other half of the rewrite
    #[test]
    fn a_syntax_tree_fold_reports_the_author_text() {
        assert_eq!(
            lowered("def go(a: int):\n    print(t\"{typeof(a)}\")\n"),
            "print(_by_template(t\"{TypeOf[a]}\", ('typeof(a)',)))"
        );
    }

    /// an extension call and a conversion are resolved from the checker's answer rather
    /// than from the syntax, and reach the field the same way
    #[test]
    fn a_type_directed_rewrite_reports_the_author_text() {
        assert_eq!(
            lowered(
                "extension list:\n    def second(self) -> Element:\n        return self[1]\n\nxs = [1, 2, 3]\nprint(t\"{xs.second()}\")\n"
            ),
            "print(_by_template(t\"{_by_ext__list__second(xs)}\", (\"xs.second()\",)))"
        );
        // a conversion fires where the argument is written, which here is inside the field
        assert_eq!(
            lowered(
                "class Meters:\n    def __init__(self, value: float):\n        self.value = value\n\n    @classmethod\n    def __of__(cls, value: int) -> Self:\n        return cls(float(value))\n\ndef take(m: Meters) -> int:\n    return 1\n\nprint(t\"{take(3)}\")\n"
            ),
            "print(_by_template(t\"{take(Meters.__of__(3))}\", (\"take(3)\",)))"
        );
    }

    /// a `=` field of a t-string is taken apart by `debug_field` into the literal text
    /// python itself would have put there and a field beside it, and that field reports
    /// the author's text too — the parentheses the taking-apart adds are not the author's
    #[test]
    fn a_debug_field_reports_the_author_text() {
        assert_eq!(
            lowered("def go(a: int?):\n    print(t\"{(a!) = }\")\n"),
            "print(_by_template(t\"(a!) = {(_force_unwrap(a))!r}\", (\"(a!)\",)))"
        );
    }

    /// only the fields a lowering reached are rebuilt. the rest keep the interpolation
    /// python built, so their own text stays byte-identical to python's
    #[test]
    fn an_untouched_field_keeps_pythons_own_interpolation() {
        assert_eq!(
            lowered("def go(a: int?, b: int):\n    print(t\"{b} {a!} {b}\")\n"),
            "print(_by_template(t\"{b} {_force_unwrap(a)} {b}\", (None, \"a!\", None)))"
        );
    }

    /// python reads the text from just past the `{` to the end of the expression, keeping
    /// the grouping parentheses and dropping the whitespace after them. `fields_carry_the_
    /// text_python_would_have_reported` in the runtime test holds these to a real 3.14
    #[test]
    fn the_author_text_is_the_span_python_would_have_reported() {
        assert_eq!(
            lowered("def go(a: int?):\n    print(t\"{  (a!)  }\")\n"),
            "print(_by_template(t\"{  (_force_unwrap(a))  }\", (\"  (a!)\",)))"
        );
        // a conversion and a format spec end the text the same way the closing brace does
        assert_eq!(
            lowered("def go(a: int?):\n    print(t\"{ (a!) !r:>6}\")\n"),
            "print(_by_template(t\"{ (_force_unwrap(a)) !r:>6}\", (\" (a!)\",)))"
        );
    }

    /// a t-string nothing lowered is left exactly as written, in a `.by` file as in a
    /// `.py` one — there is nothing to correct, and python's own text is already right
    #[test]
    fn an_unlowered_template_is_left_alone() {
        unchanged("x = 1\nprint(t\"{x}\")\n");
        assert_eq!(
            lowered("def go(a: int?, b: int):\n    print(t\"{b}\")\n"),
            "print(t\"{b}\")"
        );
    }

    /// the reverse direction leaves a t-string's fields exactly as written. rewriting one
    /// would change what the program reports — `t"{a ?? b}"` reports `a ?? b` where the
    /// python it came from reports the conditional
    #[test]
    fn the_reverse_direction_leaves_a_field_alone() {
        let out = crate::reverse_transpile(
            "def go(a: int | None, b: int):\n    print(t\"{a if a is not None else b}\")\n",
            &Config::test_default(),
        )
        .expect("reverse should succeed");
        assert!(
            out.contains("t\"{a if a is not None else b}\""),
            "got:\n{out}"
        );
        // the same expression outside a t-string is rewritten as it always was
        let out = crate::reverse_transpile(
            "def go(a: int | None, b: int):\n    print(f\"{a if a is not None else b}\")\n",
            &Config::test_default(),
        )
        .expect("reverse should succeed");
        assert!(out.contains("f\"{a ?? b}\""), "got:\n{out}");
    }
}
