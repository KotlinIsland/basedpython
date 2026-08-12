//! Compile-time lowering of the grapheme string surface.
//!
//! basedpython strings work in *extended grapheme clusters* (user-perceived
//! characters) as well as code points. `character_count` is the number of
//! grapheme clusters, and `first` / `last` read the string in whole graphemes.
//! these members are declared on `str` by the basedpython prelude (a builtin
//! `extension str:`), but none exist at runtime — every access is transformed
//! into a plain python expression:
//!
//! | basedpython           | Python output                          |
//! | --------------------- | -------------------------------------- |
//! | `s.character_count`   | `len(_by_graphemes(s))`                |
//! | `s.first`             | `(_by_graphemes(s)[0] if s else None)` |
//! | `s.last`              | `(_by_graphemes(s)[-1] if s else None)`|
//! | `s.characters`        | `[Character(c) for c in _by_graphemes(s)]` |
//! | `s.character_at(i)`   | `_by_graphemes(s)[i]`                  |
//!
//! python's occurrence-counting `str.count(sub)` method is left as-is — the
//! prelude does not touch it, so `s.count("a")` keeps its standard meaning.
//!
//! a `Character`-annotated assignment is materialised too: `x: Character = "a"`
//! becomes `x: Character = Character("a")`, so the runtime value's class is
//! `Character` rather than a plain `str` (skipped when the value is already a
//! `Character`).
//!
//! `character_count` / `first` / `last` / `characters` / `character_at` count
//! *grapheme clusters*, not code points, so they can't just be `len(s)` /
//! `s[0]`: the US flag `"\U0001F1FA\U0001F1F8"` is one grapheme but two code
//! points, and a ZWJ emoji like `"🤦🏼‍♂️"` is one grapheme but five. the injected
//! [`GRAPHEME_HELPER`] splits a string into grapheme clusters via the `regex`
//! module's `\X`, which is a runtime dependency of the grapheme surface (a
//! missing `regex` raises an actionable error, never a silently wrong
//! code-point count).
//!
//! the rewrites are type-gated: they fire only when the receiver is a string
//! (`str`, `Character`, `LiteralString`, a literal, or a `str` subclass), so
//! `character_count` on a list or a user-defined attribute passes through
//! untouched. the grapheme properties in callee position are left alone — they
//! are not callable and ty reports the error on the basedpython source.
//!
//! `first` / `last` re-emit the receiver twice; an impure receiver (a call)
//! is hoisted into a `:=` temp like the `??` lowering, so it runs once.
//! receiver spans pass through as [`Fragment::Src`], so lowerings nested in
//! the receiver compose instead of being clobbered

use std::collections::HashSet;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::coalesce::is_trivially_pure;
use crate::type_info::TypeInfo;

/// runtime helper injected when `character_count` / `first` / `last` /
/// `characters` / `character_at` are lowered. splits a string into extended
/// grapheme clusters (one `Character` each) via the `regex` module's `\X` — the only widely
/// available python engine that implements UAX #29 correctly (including ZWJ
/// emoji sequences and regional-indicator flags). `regex` is therefore a
/// runtime dependency of the grapheme surface: if it is missing we raise an
/// actionable error rather than silently miscounting with `list()`, whose
/// code-point split gives a wrong answer for any multi-code-point grapheme
pub(crate) const GRAPHEME_HELPER: &str = "\
def _by_graphemes(_text):
    try:
        import regex as _regex
    except ImportError as _err:
        raise ImportError(
            \"basedpython's grapheme string surface (character_count / first / last / \"
            \"characters / character_at / ...) needs the 'regex' package: pip install regex\"
        ) from _err
    return _regex.findall(r\"\\X\", _text)
";

/// `s.prefix(n)` — the first `n` grapheme clusters, joined. clamps `n` to `>= 0`,
/// so `prefix(0)` is empty and `prefix(large)` is the whole string
pub(crate) const PREFIX_HELPER: &str = "\
def _by_prefix(_text, _n):
    return \"\".join(_by_graphemes(_text)[:max(0, _n)])
";

/// `s.suffix(n)` — the last `n` grapheme clusters, joined. computed from the
/// front (not `[-n:]`) so `suffix(0)` is empty rather than the whole string
pub(crate) const SUFFIX_HELPER: &str = "\
def _by_suffix(_text, _n):
    _g = _by_graphemes(_text)
    return \"\".join(_g[max(0, len(_g) - _n):])
";

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent which-helpers-to-inject flags, not a state machine"
)]
struct GraphemeString<'src> {
    types: &'src dyn TypeInfo,
    /// attribute nodes that are the direct callee of a call — the grapheme
    /// properties are not callable there, so they pass through untouched
    callee_attrs: HashSet<TextRange>,
    /// set when a grapheme-based lowering fired, so [`GRAPHEME_HELPER`] is
    /// injected into the preamble
    needs_grapheme_helper: bool,
    /// set when a `Character`-producing accessor (`first` / `last` /
    /// `character_at` / `characters`) fired, so the concrete `Character` class
    /// is made available (via a `ty_extensions` import the lazy-import phase
    /// turns into `class Character(str)`)
    needs_character_class: bool,
    /// set when `prefix` / `suffix` are lowered, so their helpers are injected
    needs_prefix_helper: bool,
    needs_suffix_helper: bool,
    template_edits: Vec<(TextRange, Vec<Fragment>)>,
}

impl<'ast> Visitor<'ast> for GraphemeString<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // `x: Character = <value>` materialises a real `Character` instance:
        // the annotation is a type-only claim in python, so without this the
        // runtime value would be a plain `str`. values that are already a
        // `Character` (a grapheme accessor, an explicit `Character(...)`) are
        // left alone. the receiver span passes through as `Fragment::Src`, so
        // any lowering nested in the value still composes
        if let Stmt::AnnAssign(ann) = stmt
            && let Some(value) = &ann.value
            && self.types.annotation_is_character(&ann.annotation)
            && !self.types.is_character_instance(value)
        {
            self.needs_character_class = true;
            self.template(
                value,
                vec![
                    Fragment::Lit("Character(".to_owned()),
                    Fragment::Src(value.range()),
                    Fragment::Lit(")".to_owned()),
                ],
            );
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && let Expr::Attribute(attr) = call.func.as_ref()
        {
            self.callee_attrs.insert(attr.range());
        }
        walk_expr(self, expr);

        // grapheme *method* accessors (`character_at`, `prefix`, `suffix`,
        // `drop_first`, `drop_last`) — handled at the call node, since the
        // attribute alone is a bound-method reference, not a value
        if self.try_method_accessor(expr) {
            return;
        }

        let Expr::Attribute(attr) = expr else { return };
        if !attr.ctx.is_load() || !self.types.is_string_like(&attr.value) {
            return;
        }
        let receiver = attr.value.range();
        match attr.attr.as_str() {
            _ if self.callee_attrs.contains(&attr.range()) => {}
            // the number of grapheme clusters. python's `str.count` is the
            // occurrence-counting method, so the grapheme count is spelled out
            "character_count" => {
                self.needs_grapheme_helper = true;
                self.template(
                    expr,
                    vec![
                        Fragment::Lit("len(_by_graphemes(".to_owned()),
                        Fragment::Src(receiver),
                        Fragment::Lit("))".to_owned()),
                    ],
                );
            }
            "first" => self.element(expr, receiver, &attr.value, "0"),
            "last" => self.element(expr, receiver, &attr.value, "-1"),
            // `s.characters` — a `Sequence` of `Character`s over the graphemes
            // (a list, so it is indexable / sized / reversible)
            "characters" => {
                self.needs_grapheme_helper = true;
                self.needs_character_class = true;
                self.template(
                    expr,
                    vec![
                        Fragment::Lit("[Character(_by_c) for _by_c in _by_graphemes(".to_owned()),
                        Fragment::Src(receiver),
                        Fragment::Lit(")]".to_owned()),
                    ],
                );
            }
            // `s.reversed` — the string with its grapheme clusters reversed
            // (grapheme-safe, unlike `s[::-1]` which reverses code points)
            "reversed" => {
                self.needs_grapheme_helper = true;
                self.template(
                    expr,
                    vec![
                        Fragment::Lit("\"\".join(_by_graphemes(".to_owned()),
                        Fragment::Src(receiver),
                        Fragment::Lit(")[::-1])".to_owned()),
                    ],
                );
            }
            // `s.unicode_scalars` — an iterator over the code points (the
            // scalar view; plain python string iteration)
            "unicode_scalars" => self.template(
                expr,
                vec![
                    Fragment::Lit("iter(".to_owned()),
                    Fragment::Src(receiver),
                    Fragment::Lit(")".to_owned()),
                ],
            ),
            _ => {}
        }
    }
}

impl GraphemeString<'_> {
    fn template(&mut self, expr: &Expr, fragments: Vec<Fragment>) {
        self.template_edits.push((expr.range(), fragments));
    }

    /// `s.first` / `s.last` — the first / last *grapheme cluster* (or `None`
    /// when empty). the receiver appears in both the emptiness guard and the
    /// grapheme call, so an impure receiver is hoisted into a `:=` temp
    /// (mirroring the `??` lowering) to run exactly once
    fn element(&mut self, expr: &Expr, receiver: TextRange, receiver_expr: &Expr, index: &str) {
        self.needs_grapheme_helper = true;
        self.needs_character_class = true;
        let fragments = if is_trivially_pure(receiver_expr) {
            vec![
                Fragment::Lit("(Character(_by_graphemes(".to_owned()),
                Fragment::Src(receiver),
                Fragment::Lit(format!(")[{index}]) if ")),
                Fragment::Src(receiver),
                Fragment::Lit(" else None)".to_owned()),
            ]
        } else {
            vec![
                Fragment::Lit("(Character(_by_graphemes(_s)".to_owned()),
                Fragment::Lit(format!("[{index}]) if (_s := ")),
                Fragment::Src(receiver),
                Fragment::Lit(") else None)".to_owned()),
            ]
        };
        self.template(expr, fragments);
    }

    /// dispatch a grapheme *method* call on a string receiver
    /// (`s.character_at(i)`, `s.prefix(n)`, `s.suffix(n)`, `s.drop_first()`,
    /// `s.drop_last()`). returns `true` if it lowered the call. each receiver
    /// appears once in the output, so no `:=` hoisting is needed
    fn try_method_accessor(&mut self, expr: &Expr) -> bool {
        let Expr::Call(call) = expr else { return false };
        let Expr::Attribute(attr) = call.func.as_ref() else {
            return false;
        };
        if !attr.ctx.is_load()
            || !call.arguments.keywords.is_empty()
            || !self.types.is_string_like(&attr.value)
        {
            return false;
        }
        let recv = attr.value.range();
        let args = &call.arguments.args;
        let one_arg = || args.first().map(Ranged::range);
        let fragments = match (attr.attr.as_str(), args.len()) {
            ("character_at", 1) => {
                self.needs_character_class = true;
                vec![
                    Fragment::Lit("Character(_by_graphemes(".to_owned()),
                    Fragment::Src(recv),
                    Fragment::Lit(")[".to_owned()),
                    Fragment::Src(one_arg().unwrap()),
                    Fragment::Lit("])".to_owned()),
                ]
            }
            ("prefix", 1) => {
                self.needs_prefix_helper = true;
                vec![
                    Fragment::Lit("_by_prefix(".to_owned()),
                    Fragment::Src(recv),
                    Fragment::Lit(", ".to_owned()),
                    Fragment::Src(one_arg().unwrap()),
                    Fragment::Lit(")".to_owned()),
                ]
            }
            ("suffix", 1) => {
                self.needs_suffix_helper = true;
                vec![
                    Fragment::Lit("_by_suffix(".to_owned()),
                    Fragment::Src(recv),
                    Fragment::Lit(", ".to_owned()),
                    Fragment::Src(one_arg().unwrap()),
                    Fragment::Lit(")".to_owned()),
                ]
            }
            ("drop_first", 0) => vec![
                Fragment::Lit("\"\".join(_by_graphemes(".to_owned()),
                Fragment::Src(recv),
                Fragment::Lit(")[1:])".to_owned()),
            ],
            ("drop_last", 0) => vec![
                Fragment::Lit("\"\".join(_by_graphemes(".to_owned()),
                Fragment::Src(recv),
                Fragment::Lit(")[:-1])".to_owned()),
            ],
            _ => return false,
        };
        self.needs_grapheme_helper = true;
        self.template(expr, fragments);
        true
    }
}

pub(crate) struct GraphemeStringPass;

impl GraphemeStringPass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for GraphemeStringPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = GraphemeString {
            types,
            callee_attrs: HashSet::new(),
            needs_grapheme_helper: false,
            needs_character_class: false,
            needs_prefix_helper: false,
            needs_suffix_helper: false,
            template_edits: Vec::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        // the prefix / suffix helpers call `_by_graphemes`, so the base helper
        // is always injected first when either is used
        if inner.needs_grapheme_helper {
            ctx.required_imports.push(GRAPHEME_HELPER.to_owned());
        }
        // `Character`-producing accessors construct real instances; import the
        // name so the lazy-import phase materialises `class Character(str)`
        if inner.needs_character_class {
            ctx.required_imports
                .push("from ty_extensions import Character".to_owned());
        }
        if inner.needs_prefix_helper {
            ctx.required_imports.push(PREFIX_HELPER.to_owned());
        }
        if inner.needs_suffix_helper {
            ctx.required_imports.push(SUFFIX_HELPER.to_owned());
        }
        ctx.template_edits.extend(inner.template_edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// the grapheme helper preamble as it appears in transpiled output (the
    /// injected [`super::GRAPHEME_HELPER`] followed by the emitter's blank line)
    const HELPER: &str = "\
def _by_graphemes(_text):
    try:
        import regex as _regex
    except ImportError as _err:
        raise ImportError(
            \"basedpython's grapheme string surface (character_count / first / last / \"
            \"characters / character_at / ...) needs the 'regex' package: pip install regex\"
        ) from _err
    return _regex.findall(r\"\\X\", _text)

";

    /// the import injected by `Character`-constructing accessors (in test
    /// config the lazy-import phase is off, so it stays a plain import; a real
    /// transpile turns it into `class Character(str)`)
    const CHAR_IMPORT: &str = "from ty_extensions import Character\n";

    /// the `_by_prefix` / `_by_suffix` helper preambles as emitted
    const PREFIX_HELPER: &str = "\
def _by_prefix(_text, _n):
    return \"\".join(_by_graphemes(_text)[:max(0, _n)])

";
    const SUFFIX_HELPER: &str = "\
def _by_suffix(_text, _n):
    _g = _by_graphemes(_text)
    return \"\".join(_g[max(0, len(_g) - _n):])

";

    #[test]
    fn character_count_is_grapheme_count() {
        check(
            indoc! {"
                def f(s: str) -> int:
                    return s.character_count
            "},
            &format!(
                "{HELPER}{}",
                indoc! {"
                    def f(s: str) -> int:
                        return len(_by_graphemes(s))
                "}
            ),
        );
    }

    #[test]
    fn character_count_on_literal() {
        check(
            "n = \"hello\".character_count\n",
            &format!("{HELPER}n = len(_by_graphemes(\"hello\"))\n"),
        );
    }

    #[test]
    fn first_and_last_are_graphemes() {
        check(
            indoc! {"
                def f(s: str):
                    a = s.first
                    b = s.last
            "},
            &format!(
                "{CHAR_IMPORT}{HELPER}{}",
                indoc! {"
                    def f(s: str):
                        a = (Character(_by_graphemes(s)[0]) if s else None)
                        b = (Character(_by_graphemes(s)[-1]) if s else None)
                "}
            ),
        );
    }

    #[test]
    fn first_on_impure_receiver_runs_once() {
        check(
            indoc! {"
                def g() -> str: ...
                a = g().first
            "},
            &format!(
                "{CHAR_IMPORT}{HELPER}{}",
                indoc! {"
                    def g() -> str: ...
                    a = (Character(_by_graphemes(_s)[0]) if (_s := g()) else None)
                "}
            ),
        );
    }

    #[test]
    fn characters_iterates_graphemes() {
        check(
            indoc! {"
                def f(s: str):
                    for c in s.characters:
                        print(c)
            "},
            &format!(
                "{CHAR_IMPORT}{HELPER}{}",
                indoc! {"
                    def f(s: str):
                        for c in [Character(_by_c) for _by_c in _by_graphemes(s)]:
                            print(c)
                "}
            ),
        );
    }

    #[test]
    fn character_at_indexes_graphemes() {
        check(
            indoc! {"
                def f(s: str, i: int):
                    a = s.character_at(0)
                    b = s.character_at(i + 1)
            "},
            &format!(
                "{CHAR_IMPORT}{HELPER}{}",
                indoc! {"
                    def f(s: str, i: int):
                        a = Character(_by_graphemes(s)[0])
                        b = Character(_by_graphemes(s)[i + 1])
                "}
            ),
        );
    }

    #[test]
    fn character_at_on_non_string_untouched() {
        unchanged("def f(xs: list[str]) -> str:\n    return xs.character_at(0)\n");
    }

    #[test]
    fn character_annotation_constructs_instance() {
        // a `Character` annotation materialises a real instance, so the
        // runtime value's class is `Character` rather than plain `str`
        check(
            "x: Character = \"a\"\n",
            &format!("{CHAR_IMPORT}x: Character = Character(\"a\")\n"),
        );
    }

    #[test]
    fn character_annotation_skips_existing_character() {
        // the value is already a `Character` (a grapheme accessor), so it is
        // not wrapped a second time
        check(
            indoc! {"
                def f(s: str):
                    y: Character = s.character_at(0)
            "},
            &format!(
                "{CHAR_IMPORT}{HELPER}{}",
                indoc! {"
                    def f(s: str):
                        y: Character = Character(_by_graphemes(s)[0])
                "}
            ),
        );
    }

    #[test]
    fn non_character_annotation_untouched() {
        // a `str` annotation is not a `Character`, so the value is left alone
        unchanged("a: str = \"a\"\n");
        // a union is not exactly `Character` — strict, so no coercion
        unchanged("c: Character | None = \"a\"\n");
    }

    #[test]
    fn reversed_is_grapheme_safe() {
        check(
            "def f(s: str) -> str:\n    return s.reversed\n",
            &format!(
                "{HELPER}def f(s: str) -> str:\n    return \"\".join(_by_graphemes(s)[::-1])\n"
            ),
        );
    }

    #[test]
    fn drop_first_and_last() {
        check(
            indoc! {"
                def f(s: str):
                    a = s.drop_first()
                    b = s.drop_last()
            "},
            &format!(
                "{HELPER}{}",
                indoc! {"
                    def f(s: str):
                        a = \"\".join(_by_graphemes(s)[1:])
                        b = \"\".join(_by_graphemes(s)[:-1])
                "}
            ),
        );
    }

    #[test]
    fn prefix_and_suffix_use_helpers() {
        check(
            indoc! {"
                def f(s: str, n: int):
                    a = s.prefix(3)
                    b = s.suffix(n)
            "},
            &format!(
                "{HELPER}{PREFIX_HELPER}{SUFFIX_HELPER}{}",
                indoc! {"
                    def f(s: str, n: int):
                        a = _by_prefix(s, 3)
                        b = _by_suffix(s, n)
                "}
            ),
        );
    }

    #[test]
    fn unicode_scalars_is_code_point_iter() {
        // the scalar view is plain code-point iteration — no grapheme helper
        check(
            "def f(s: str):\n    for u in s.unicode_scalars:\n        print(u)\n",
            "def f(s: str):\n    for u in iter(s):\n        print(u)\n",
        );
    }

    #[test]
    fn accessors_on_non_string_untouched() {
        unchanged("def f(xs: list[int]) -> int:\n    return xs.drop_first()\n");
    }

    #[test]
    fn count_method_untouched() {
        // `str.count` stays python's occurrence-counting method — the prelude
        // does not touch it, so the call passes through unchanged
        unchanged("def f(s: str) -> int:\n    return s.count(\"a\")\n");
    }

    #[test]
    fn non_string_receivers_untouched() {
        unchanged("def f(xs: list[int]) -> int:\n    return xs.count(1)\n");
        unchanged(
            "class A:\n    character_count = 5\ndef f(a: A) -> int:\n    return a.character_count\n",
        );
    }

    #[test]
    fn dynamic_receiver_untouched() {
        unchanged("def f(x):\n    return x.character_count\n");
    }

    #[test]
    fn a_narrowed_dynamic_receiver_is_untouched_too() {
        // narrowing a gradual value leaves an intersection whose *positive* member is
        // still gradual, and a gradual member is assignable to `str` exactly as readily
        // as the whole type would be. so this asked "is it string-like?", got yes, and
        // rewrote a list's attribute access into a grapheme count.
        //
        // it has to go through `check` rather than `unchanged`: the latter transpiles as
        // *python*, where this surface does not exist and nothing would have fired either
        // way
        let source = indoc! {"
            def f(x):
                if not isinstance(x, int):
                    return x.character_count
                return 0
        "};
        check(source, source);
    }

    #[test]
    fn character_receiver() {
        check(
            indoc! {"
                def f(c: Character) -> int:
                    return c.character_count
            "},
            &format!(
                "from ty_extensions import Character\n{HELPER}{}",
                indoc! {"
                    def f(c: Character) -> int:
                        return len(_by_graphemes(c))
                "}
            ),
        );
    }

    #[test]
    fn character_count_in_expression_context() {
        check(
            indoc! {"
                def f(s: str) -> bool:
                    return s.character_count > 3
            "},
            &format!(
                "{HELPER}{}",
                indoc! {"
                    def f(s: str) -> bool:
                        return len(_by_graphemes(s)) > 3
                "}
            ),
        );
    }

    #[test]
    fn python_unchanged() {
        // plain python spells these as expressions already
        unchanged("def f(s: str) -> int:\n    return len(s)\n");
    }
}
