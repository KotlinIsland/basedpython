//! Text-edit pass: replaces non-scalar default arguments with a `_MISSING`
//! sentinel and injects a guard at the top of each function body.
//!
//!   def f(x=[]):        →   def f(x=_MISSING):
//!       ...                     if x is _MISSING:
//!                                   x = []
//!                               ...
//!
//! Only number, bool, None, string, and ellipsis literals (and unary +/-
//! on a number) are kept as-is; everything else is re-evaluated per call.
//!
//! The same sentinel machinery lowers basedpython's relaxed parameter order —
//! a `def` may declare a required parameter after a defaulted one (so a
//! trailing lambda can bind the last parameter while earlier parameters keep
//! their defaults). Python rejects that shape, so the required parameter gets
//! a sentinel default and a guard that raises:
//!
//!   def f(x=1, a):      →   def f(x=1, a=_MISSING):
//!       ...                     if a is _MISSING:
//!                                   raise ...  # a `TypeError`, like python's own
//!                               ...
//!
//! The rewrite touches only the default expressions (each swapped for the
//! sentinel) and inserts the guard lines at the body start — the rest of the
//! function, body included, keeps its source bytes, so sibling lowerings
//! (`??`, `?.`, `int?` annotations, …) anywhere in the function still apply.

use ruff_python_ast::helpers::is_immutable_scalar_default;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::source_util::{line_indent, line_start};
use crate::type_info::TypeInfo;

/// what a `_MISSING`-sentinel guard does when the argument was not supplied
pub(crate) enum Guard {
    /// re-evaluate the written default per call (mutable defaults). the default
    /// is carried as its source *range*, not its text: the guard re-emits it
    /// through a [`Fragment::Src`] passthrough so the lowerings written inside
    /// it (`?.`, `!`, an `is` test, a `context` argument) land in the body copy
    /// rather than being dropped with the signature they came from
    Reevaluate { name: String, default: TextRange },
    /// raise — the parameter is required, its sentinel default only exists
    /// because python rejects a required parameter after a defaulted one
    Required { name: String, function: String },
}

impl Guard {
    pub(crate) fn push(&self, frags: &mut Vec<Fragment>, base: &str) {
        match self {
            Guard::Reevaluate { name, default } => {
                frags.push(Fragment::Lit(format!(
                    "if {name} is _MISSING:\n{base}    {name} = "
                )));
                frags.push(Fragment::Src(*default));
            }
            Guard::Required { name, function } => {
                frags.push(Fragment::Lit(format!(
                    "if {name} is _MISSING:\n{base}    raise TypeError(\"{function}() missing required argument: '{name}'\")"
                )));
            }
        }
    }
}

struct MutableDefaults<'src> {
    source: &'src str,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    /// the guard suites, anchored at the body statement they precede
    guards: Vec<(TextSize, Vec<Fragment>)>,
    used: bool,
    /// functions whose body starts with parser-synthesized statements, so a
    /// guard has no source position to anchor to
    unanchored: Vec<String>,
}

/// the statement's range when it is a position in the *body*. a statement the
/// parser synthesized for an `init(…)` shorthand carries either an empty range
/// or the range of the parameter it was built from, so it points into the
/// header — splicing a guard there lands it in the middle of the signature
fn body_range(stmt: &Stmt, header_end: TextSize) -> Option<TextRange> {
    let range = stmt.range();
    (!range.is_empty() && range.start() >= header_end).then_some(range)
}

/// replace a default with the sentinel. a template rather than plain text so
/// it *absorbs* the zero-width insertions a lowering anchored to the default's
/// first token left behind (`_force_unwrap(`), which a plain-text replacement
/// leaves stranded in the signature. the guard re-emits them
fn sentinel_edit(default: TextRange) -> (TextRange, Vec<Fragment>) {
    (default, vec![Fragment::Lit("_MISSING".to_owned())])
}

/// The signature edits and body guards `f`'s parameter list calls for: a
/// `_MISSING` sentinel wherever a default would otherwise be shared between
/// calls, and one wherever python's own parameter order is relaxed.
pub(crate) fn parameter_guards(
    f: &StmtFunctionDef,
) -> (Vec<(TextRange, Vec<Fragment>)>, Vec<Guard>) {
    let mut sentinels = Vec::new();
    let mut guards = Vec::new();
    let params = f.parameters.as_ref();
    // positional parameters: swap non-scalar defaults for the sentinel, and give
    // basedpython's required-after-defaulted parameters a sentinel default plus
    // a raising guard (keyword-only parameters may follow a default without one
    // in python already)
    let mut seen_default = false;
    for pw in params.posonlyargs.iter().chain(params.args.iter()) {
        match pw.default.as_deref() {
            Some(d) => {
                seen_default = true;
                if !is_immutable_scalar_default(d) && !body_cannot_evaluate(d) {
                    sentinels.push(sentinel_edit(d.range()));
                    guards.push(Guard::Reevaluate {
                        name: pw.parameter.name.id.to_string(),
                        default: d.range(),
                    });
                }
            }
            None if seen_default => {
                // `=` spacing mirrors python style: spaced when annotated
                let sentinel = if pw.parameter.annotation.is_some() {
                    " = _MISSING"
                } else {
                    "=_MISSING"
                };
                sentinels.push((
                    TextRange::empty(pw.parameter.range().end()),
                    vec![Fragment::Lit(sentinel.to_owned())],
                ));
                guards.push(Guard::Required {
                    name: pw.parameter.name.id.to_string(),
                    function: f.name.id.to_string(),
                });
            }
            None => {}
        }
    }
    for pw in &params.kwonlyargs {
        if let Some(d) = pw.default.as_deref()
            && !is_immutable_scalar_default(d)
            && !body_cannot_evaluate(d)
        {
            sentinels.push(sentinel_edit(d.range()));
            guards.push(Guard::Reevaluate {
                name: pw.parameter.name.id.to_string(),
                default: d.range(),
            });
        }
    }
    (sentinels, guards)
}

/// Whether the *callee's* body could evaluate `default` at all.
///
/// Re-evaluating a default there is the point of the guard, and it is what lets
/// a later parameter default from an earlier one. But an `await` belongs to the
/// suspension of the function the `def` was written in, and a callee that is
/// not itself async cannot host one — cpython rejects the result outright:
///
/// ```text
/// async def outer():
///     def g(a=await value()): ...
/// ```
///
/// So that default is left exactly as written, which is what python does with
/// it. A mutable one left shared is `mutable-argument-default`'s to report.
fn body_cannot_evaluate(default: &Expr) -> bool {
    struct Finder {
        found: bool,
    }

    impl<'a> Visitor<'a> for Finder {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Await(_) | Expr::Yield(_) | Expr::YieldFrom(_) => self.found = true,
                // a nested function's own suspension is its own
                Expr::Lambda(_) => {}
                _ if !self.found => walk_expr(self, expr),
                _ => {}
            }
        }
    }

    let mut finder = Finder { found: false };
    finder.visit_expr(default);
    finder.found
}

/// Whether `f` is an `init(…)` shorthand whose whole body the parser
/// synthesized — the source wrote none of it, so there is nothing to anchor to.
pub(crate) fn is_bodyless_init_shorthand(f: &StmtFunctionDef) -> bool {
    f.decorator_list
        .iter()
        .any(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "__init_method__"))
        && first_body_statement(f).is_none()
}

/// The first statement in `f`'s body that came from the source, docstring
/// included — that is, whether the source wrote a body at all.
///
/// [`init_method`] hangs the body it generates off this, and it is what
/// [`is_bodyless_init_shorthand`] means by bodyless. A docstring *is* a body:
/// generating another one around it would leave two.
///
/// [`init_method`]: super::init_method
pub(crate) fn first_body_statement(f: &StmtFunctionDef) -> Option<&Stmt> {
    let header_end = header_end(f);
    f.body
        .iter()
        .find(|stmt| body_range(stmt, header_end).is_some())
}

/// The offset past everything a `def`'s header can span, so a statement before
/// it is one the parser synthesized from a parameter rather than a body.
fn header_end(f: &StmtFunctionDef) -> TextSize {
    f.parameters.range().end().max(
        f.returns
            .as_ref()
            .map_or(TextSize::new(0), |r| r.range().end()),
    )
}

/// The first statement in `f`'s body that came from the source, skipping a
/// docstring and any statement the parser synthesized.
///
/// This is the anchor a body insertion hangs off, and [`init_method`] reads the
/// same one: a body the two disagree about is a body where one of them emits a
/// `_MISSING` sentinel and the other emits no guard for it.
///
/// [`init_method`]: super::init_method
pub(crate) fn first_source_statement(f: &StmtFunctionDef) -> Option<&Stmt> {
    let header_end = header_end(f);
    let docstring_count = if let Some(Stmt::Expr(e)) = f.body.first() {
        usize::from(matches!(e.value.as_ref(), Expr::StringLiteral(_)))
    } else {
        0
    };
    f.body
        .iter()
        .skip(docstring_count)
        .find(|s| body_range(s, header_end).is_some())
}

impl MutableDefaults<'_> {
    fn process_function(&mut self, f: &StmtFunctionDef) {
        // the `init(…)` shorthand with no body of its own has no source
        // statement to splice a guard before, so [`init_method`] — which writes
        // that body — emits both the sentinels and the guards for it
        //
        // [`init_method`]: super::init_method
        if is_bodyless_init_shorthand(f) {
            return;
        }
        let (sentinels, guards) = parameter_guards(f);
        self.edits.extend(sentinels);
        if guards.is_empty() {
            return;
        }
        self.used = true;

        // insert the guards at the start of the first body statement the source
        // actually wrote
        let docstring_count = if let Some(Stmt::Expr(e)) = f.body.first() {
            usize::from(matches!(e.value.as_ref(), Expr::StringLiteral(_)))
        } else {
            0
        };
        // the body begins after everything the header can span
        let header_end = f.parameters.range().end().max(
            f.returns
                .as_ref()
                .map_or(TextSize::new(0), |r| r.range().end()),
        );
        let mut frags: Vec<Fragment> = Vec::new();
        if let Some(range) = first_source_statement(f).and_then(|s| body_range(s, header_end)) {
            let insert_at = range.start();
            let prefix = &self.source
                [usize::from(line_start(self.source, insert_at))..usize::from(insert_at)];
            if prefix.trim().is_empty() {
                // the insertion lands after the statement's own indentation;
                // each guard re-establishes it for the following line
                let base = prefix.to_owned();
                for guard in &guards {
                    guard.push(&mut frags, &base);
                    frags.push(Fragment::Lit(format!("\n{base}")));
                }
            } else {
                // single-line body (`def f(x=[]): ...`) — break it onto its own
                // indented line after the guards
                let base = format!("{}    ", line_indent(self.source, f.range().start()));
                for guard in &guards {
                    frags.push(Fragment::Lit(format!("\n{base}")));
                    guard.push(&mut frags, &base);
                }
                frags.push(Fragment::Lit(format!("\n{base}")));
            }
            self.guards.push((insert_at, frags));
        } else if let Some(range) = docstring_count
            .checked_sub(1)
            .and_then(|i| f.body.get(i))
            .and_then(|s| body_range(s, header_end))
        {
            // docstring-only body: append the guards after it
            let base = format!("{}    ", line_indent(self.source, f.range().start()));
            for guard in &guards {
                frags.push(Fragment::Lit(format!("\n{base}")));
                guard.push(&mut frags, &base);
            }
            self.guards.push((range.end(), frags));
        } else {
            // nothing in the body came from the source, so there is nowhere to
            // put the guard. say so rather than splice it at a synthesized
            // node's offset, which lands inside the signature
            self.unanchored.push(f.name.to_string());
        }
    }
}

impl<'ast> Visitor<'ast> for MutableDefaults<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            self.process_function(f);
        }
        walk_stmt(self, stmt);
    }
}

pub(crate) struct MutableDefaultsPass<'src> {
    source: &'src str,
}

impl<'src> MutableDefaultsPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for MutableDefaultsPass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = MutableDefaults {
            source: self.source,
            edits: Vec::new(),
            guards: Vec::new(),
            used: false,
            unanchored: Vec::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if let Some(name) = inner.unanchored.first() {
            // the `init(…)` shorthand is the one construct that generates its
            // own body, and it emits its own guards; anything else reaching here
            // means a pass grew a synthesized body without saying where a
            // statement goes in it. refuse rather than splice one into the
            // signature
            ctx.errors.push(format!(
                "`{name}` has a body nothing in the source anchors, so a parameter guard has nowhere to go"
            ));
            return;
        }
        if inner.used {
            ctx.required_imports.push("_MISSING = object()".to_owned());
        }
        ctx.template_edits.extend(inner.edits);
        ctx.statement_inserts.extend(inner.guards);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::transpile;
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        check_at(crate::Config::test_default().min_version, input, expected);
    }

    fn check_at(min_version: crate::PythonVersion, input: &str, expected: &str) {
        let config = crate::Config {
            min_version,
            ..crate::Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// a guard is a *statement*, so an expression rewrite of the body statement
    /// it sits in front of must not absorb it into its own passthrough — that
    /// spliced the guard suite into the middle of a call's arguments
    #[test]
    fn a_guard_is_not_absorbed_by_a_rewrite_of_the_statement_it_precedes() {
        let out = transpile(
            indoc! {r#"
                DEFAULT = "x"

                class A:
                    d: dict[str, int]

                    def f(self, k: str = DEFAULT):
                        self.d.pop(k, None)
            "#},
            &crate::Config {
                soundness: crate::SoundnessPositions::defaults(),
                ..crate::Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains(
                "        if k is _MISSING:\n            k = DEFAULT\n        _soundness_check(self.d.pop(k, None)"
            ),
            "got:\n{out}"
        );
    }

    /// an `init(…)` shorthand generates its own body, so the guards go in it —
    /// there is no source statement to splice them before
    #[test]
    fn a_generated_constructor_body_takes_the_guard() {
        // the `init(…)` shorthand has no source statement to splice a guard
        // before, so the body it generates carries one
        check(
            indoc! {"
                class S:
                    init(items: list[int] = [])
            "},
            indoc! {"
                _MISSING = object()
                class S:
                    def __init__(self, items: list[int] = _MISSING):
                        if items is _MISSING:
                            items = []
            "},
        );
        check(
            indoc! {"
                class S:
                    init(let items: list[int] = [])
            "},
            indoc! {"
                _MISSING = object()
                class S:
                    def __init__(self, items: list[int] = _MISSING):
                        if items is _MISSING:
                            items = []
                        self.items: list[int] = items
            "},
        );
    }

    #[test]
    fn a_generated_constructor_takes_a_required_after_default_guard() {
        // python rejects a required parameter after a defaulted one, so the
        // shorthand's relaxed order lowers to a sentinel and a raising guard
        check(
            indoc! {"
                class S:
                    init(let first: int = 1, let second: str)
            "},
            indoc! {"
                _MISSING = object()
                class S:
                    def __init__(self, first: int = 1, second: str = _MISSING):
                        if second is _MISSING:
                            raise TypeError(\"__init__() missing required argument: 'second'\")
                        self.first: int = first
                        self.second: str = second
            "},
        );
    }

    #[test]
    fn a_shorthand_with_a_body_anchors_before_its_own_first_statement() {
        // the generated field assignments sit ahead of the written body, and
        // the guard has to precede both
        check(
            indoc! {"
                class S:
                    init(let items: list[int] = []):
                        print(items)
            "},
            indoc! {"
                _MISSING = object()
                class S:
                    def __init__(self, items: list[int] = _MISSING):
                        if items is _MISSING:
                            items = []
                        self.items: list[int] = items
                        print(items)
            "},
        );
    }

    /// the neighbouring shapes still lower: a scalar default needs no guard at
    /// all, and a hand-written constructor has a real body to lower into
    #[test]
    fn a_hand_written_constructor_still_lowers() {
        check(
            indoc! {"
                class S:
                    init(let items: int = 0)
            "},
            indoc! {"
                class S:
                    def __init__(self, items: int = 0):
                        self.items: int = items
            "},
        );
        check(
            indoc! {"
                class S:
                    def __init__(self, items: list[int] = []) -> None:
                        self.items = items
            "},
            indoc! {"
                _MISSING = object()
                class S:
                    def __init__(self, items: list[int] = _MISSING) -> None:
                        if items is _MISSING:
                            items = []
                        self.items = items
            "},
        );
    }

    #[test]
    fn list_default() {
        check(
            indoc! {"
                def f(x=[]):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = []
                    pass
            "},
        );
    }

    #[test]
    fn dict_default() {
        check(
            indoc! {"
                def f(x={}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = {}
                    pass
            "},
        );
    }

    #[test]
    fn set_default() {
        check(
            indoc! {"
                def f(x={1, 2}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = {1, 2}
                    pass
            "},
        );
    }

    #[test]
    fn scalar_default_unchanged() {
        check(
            indoc! {"
                def f(x=0):
                    pass
            "},
            indoc! {"
                def f(x=0):
                    pass
            "},
        );
    }

    #[test]
    fn ellipsis_default_unchanged() {
        unchanged(indoc! {"
                def f(x=...):
                    pass
            "});
    }

    #[test]
    fn none_default_unchanged() {
        check(
            indoc! {"
                def f(x=None):
                    pass
            "},
            indoc! {"
                def f(x=None):
                    pass
            "},
        );
    }

    #[test]
    fn multiple_mutable_defaults() {
        check(
            indoc! {"
                def f(x=[], y={}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING, y=_MISSING):
                    if x is _MISSING:
                        x = []
                    if y is _MISSING:
                        y = {}
                    pass
            "},
        );
    }

    #[test]
    fn preserves_docstring() {
        check(
            indoc! {r#"
                def f(x=[]):
                    """doc"""
                    pass
            "#},
            indoc! {r#"
                _MISSING = object()
                def f(x=_MISSING):
                    """doc"""
                    if x is _MISSING:
                        x = []
                    pass
            "#},
        );
    }

    #[test]
    fn sentinel_defined_once_for_multiple_functions() {
        check(
            indoc! {"
                def f(x=[]):
                    pass
                def g(y={}):
                    pass
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = []
                    pass
                def g(y=_MISSING):
                    if y is _MISSING:
                        y = {}
                    pass
            "},
        );
    }

    #[test]
    fn required_after_default() {
        check(
            indoc! {"
                def f(x=1, a):
                    print(x, a)
            "},
            indoc! {"
                _MISSING = object()
                def f(x=1, a=_MISSING):
                    if a is _MISSING:
                        raise TypeError(\"f() missing required argument: 'a'\")
                    print(x, a)
            "},
        );
    }

    #[test]
    fn required_after_default_annotated() {
        check(
            indoc! {"
                def f(x: int = 1, a: int):
                    print(x, a)
            "},
            indoc! {"
                _MISSING = object()
                def f(x: int = 1, a: int = _MISSING):
                    if a is _MISSING:
                        raise TypeError(\"f() missing required argument: 'a'\")
                    print(x, a)
            "},
        );
    }

    #[test]
    fn required_after_mutable_default() {
        // both duties in one signature: the mutable default re-evaluates, the
        // required parameter raises — guards in parameter order
        check(
            indoc! {"
                def f(x=[], a):
                    print(x, a)
            "},
            indoc! {"
                _MISSING = object()
                def f(x=_MISSING, a=_MISSING):
                    if x is _MISSING:
                        x = []
                    if a is _MISSING:
                        raise TypeError(\"f() missing required argument: 'a'\")
                    print(x, a)
            "},
        );
    }

    #[test]
    fn required_keyword_only_untouched() {
        // a keyword-only parameter without a default after a defaulted one is
        // already valid python
        check(
            indoc! {"
                def f(x=1, *, a):
                    print(x, a)
            "},
            indoc! {"
                def f(x=1, *, a):
                    print(x, a)
            "},
        );
    }

    #[test]
    fn fstring_default() {
        check(
            indoc! {r#"
                data = "fdsa"
                def f(a=f"asdf{data}"):
                    print(a)
            "#},
            indoc! {r#"
                _MISSING = object()
                data = "fdsa"
                def f(a=_MISSING):
                    if a is _MISSING:
                        a = f"asdf{data}"
                    print(a)
            "#},
        );
    }

    /// t-strings are 3.14 syntax, so the target has to be one that can run them
    #[test]
    fn tstring_default() {
        check_at(
            crate::PythonVersion::PY314,
            indoc! {r#"
                data = "fdsa"
                def f(a=t"asdf{data}"):
                    print(a)
            "#},
            indoc! {r#"
                _MISSING = object()
                data = "fdsa"
                def f(a=_MISSING):
                    if a is _MISSING:
                        a = t"asdf{data}"
                    print(a)
            "#},
        );
    }

    #[test]
    fn default_references_earlier_param() {
        // the signature keeps its source layout (only the default expression
        // is swapped for the sentinel)
        check(
            indoc! {"
                def f(a, b = a + 1):
                    print(a)


                f(1)
                f(2)
            "},
            indoc! {"
                _MISSING = object()
                def f(a, b = _MISSING):
                    if b is _MISSING:
                        b = a + 1
                    print(a)


                f(1)
                f(2)
            "},
        );
    }

    #[test]
    fn multiline_signature_inline_ellipsis_body() {
        // the signature keeps its source layout; the inline body breaks onto
        // its own line after the guard
        check(
            indoc! {"
                def f(
                    a: int = []
                ) -> int: ...
            "},
            "_MISSING = object()\ndef f(\n    a: int = _MISSING\n) -> int: \n    if a is _MISSING:\n        a = []\n    ...\n",
        );
    }

    #[test]
    fn inline_ellipsis_body() {
        check(
            "def f(x=[]): ...",
            "_MISSING = object()\ndef f(x=_MISSING): \n    if x is _MISSING:\n        x = []\n    ...",
        );
    }

    #[test]
    fn default_lowerings_survive() {
        // the default is re-emitted in the body through a `Src` passthrough, so
        // the lowerings written inside it land in the guard rather than being
        // dropped with the signature they came from
        check(
            indoc! {"
                def f(x = [1 is int]):
                    return x
            "},
            indoc! {"
                _MISSING = object()
                def f(x = _MISSING):
                    if x is _MISSING:
                        x = [isinstance(1, int)]
                    return x
            "},
        );
    }

    #[test]
    fn default_lowering_spanning_the_whole_default_survives() {
        // the sentinel and the `is` lowering claim the *same* span. the sentinel
        // substitutes it and the lowering rewrites it, so the substitution
        // decides the signature and the rewrite materializes in the guard
        check(
            indoc! {"
                def f(x = 1 is int):
                    return x
            "},
            indoc! {"
                _MISSING = object()
                def f(x = _MISSING):
                    if x is _MISSING:
                        x = isinstance(1, int)
                    return x
            "},
        );
    }

    #[test]
    fn default_lowering_anchored_to_the_first_token_survives() {
        // `!` lowers to a wrap: an insertion at the default's first token and a
        // replacement at its last. the insertion has to move into the guard with
        // the rest — left in the signature it would open a call that never closes
        let out = transpile(
            indoc! {"
                def f(a: int?, x = a!):
                    return x
            "},
            &crate::Config::test_default(),
        )
        .unwrap();
        assert!(
            out.ends_with(indoc! {"
                def f(a: int | None, x = _MISSING):
                    if x is _MISSING:
                        x = _force_unwrap(a)
                    return x
            "}),
            "{out}"
        );
    }

    #[test]
    fn body_lowerings_survive() {
        // the body keeps its source bytes, so sibling lowerings inside it
        // (`int?`, `??`) still apply — previously the whole-def re-render
        // clobbered them
        check(
            indoc! {"
                def f(xs: list[int] = []) -> int:
                    a: int? = None
                    return a ?? len(xs)
            "},
            indoc! {"
                _MISSING = object()
                def f(xs: list[int] = _MISSING) -> int:
                    if xs is _MISSING:
                        xs = []
                    a: int | None = None
                    return a if a is not None else len(xs)
            "},
        );
    }
}
