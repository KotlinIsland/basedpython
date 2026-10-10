//! Text-edit pass: replaces non-scalar default arguments with a `_MISSING`
//! marker and injects a guard at the top of each function body.
//!
//! ```text
//! def f(x=[]):        →   _MISSING: Any = _by_default("[]", __name__, "_MISSING")
//!     ...                 def f(x=_MISSING):
//!                             if x is _MISSING:
//!                                 x = []
//!                             ...
//! ```
//!
//! Only number, bool, None, string, and ellipsis literals (and unary +/-
//! on a number) are kept as-is; everything else is re-evaluated per call.
//!
//! Such a default has no value until a call evaluates it, so nothing truthful can
//! stand in the signature as its value. The marker standing there instead carries
//! the default's text, which its `repr` shows, so `inspect.signature` reports
//! `x=<default: []>` rather than an anonymous object. Each default has a marker of
//! its own, bound at module level ([`DefaultMarkers`]), and its guard tests for that
//! one object, so a marker handed back as an argument — `BoundArguments.apply_defaults`
//! does — is the argument not given, as leaving it out is, while another default's
//! marker is a value like any other.
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
//!
//! The third thing a parameter list can call for is written here too, because it is
//! the same question asked of the same list: basedpython carries a method's defaults
//! to its overrides, so a parameter that writes none of its own may still have one,
//! and it is written into the signature the override emits:
//!
//!   class B(A):         →   class B(A):
//!       def f(self, a):         def f(self, a=1):
//!
//! Deciding that before the two rewrites above is what lets them compose: an inherited
//! default makes the parameters after it "after a default", exactly as a written one
//! would, so a required parameter following one still gets its sentinel and its
//! raising guard.
//!
//! There is no reverse transform. Going the other way means *dropping* a default an
//! override writes because a base declares the same one, and whether that is safe
//! depends on the whole MRO the reverse direction is reading — including bases it may
//! not have read yet. A dropped default is silent when the reading is wrong, so the
//! python → basedpython direction leaves every written default where it is.

use std::cell::RefCell;
use std::collections::HashMap;

use ruff_python_ast::helpers::is_immutable_scalar_default;
use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ParameterWithDefault, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::repeated_underscore::{WrittenNames, reads_underscore, rebound_underscore};
use super::source_util::{
    PrologueStatement, body_prologue, first_body_statement, python_string_literal,
    standalone_expression,
};
use crate::type_info::TypeInfo;

/// what a `_MISSING`-marker guard does when the argument was not supplied
pub(crate) enum Guard {
    /// re-evaluate the written default per call (mutable defaults). the default
    /// is carried as its source *range*, not its text: the guard re-emits it
    /// through a [`Fragment::Src`] passthrough so the lowerings written inside
    /// it (`?.`, `!`, an `is` test, a `context` argument) land in the body copy
    /// rather than being dropped with the signature they came from
    Reevaluate {
        name: String,
        marker: String,
        default: Vec<Fragment>,
    },
    /// raise — the parameter is required, its sentinel default only exists
    /// because python rejects a required parameter after a defaulted one
    Required {
        name: String,
        marker: String,
        function: String,
        /// the name the builtin `TypeError` is written under
        type_error: String,
    },
    /// bind `_` to the parameter that was the first `_`, for a body that reads it. a
    /// repeated `_` that takes its name from an overridden method leaves no parameter
    /// named `_`, and a read of `_` is the first one's value — after its default is
    /// settled, so this comes after the guards above
    Rebind { parameter: String },
}

impl PrologueStatement for Guard {
    fn push(&self, frags: &mut Vec<Fragment>, base: &str) {
        match self {
            Guard::Reevaluate {
                name,
                marker,
                default,
            } => {
                frags.push(Fragment::Lit(format!(
                    "if {name} is {marker}:\n{base}    {name} = "
                )));
                frags.extend(default.iter().cloned());
            }
            Guard::Required {
                name,
                marker,
                function,
                type_error,
            } => {
                frags.push(Fragment::Lit(format!(
                    "if {name} is {marker}:\n{base}    raise {type_error}(\"{function}() missing required argument: '{name}'\")"
                )));
            }
            Guard::Rebind { parameter } => {
                frags.push(Fragment::Lit(format!("_ = {parameter}")));
            }
        }
    }
}

struct MutableDefaults<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    markers: &'src DefaultMarkers,
    types: &'src dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    /// the `_MISSING` substitutions, whose defaults the guards re-evaluate
    relocating: Vec<(TextRange, Vec<Fragment>)>,
    /// the guard suites, anchored at the body statement they precede
    guards: Vec<(TextSize, Vec<Fragment>)>,
    /// the lines binding the markers the module's signatures name
    definitions: Vec<String>,
    /// functions whose body starts with parser-synthesized statements, so a
    /// guard has no source position to anchor to
    unanchored: Vec<String>,
    is_stub: bool,
    /// `(function, parameter)` for each parameter a stub cannot declare. see
    /// [`ParameterGuards::undeclarable`]
    undeclarable: Vec<(String, String)>,
}

/// replace a default with its marker. a template rather than plain text so it
/// *absorbs* the zero-width insertions a lowering anchored to the default's first
/// token left behind (`_force_unwrap(`), which a plain-text replacement leaves
/// stranded in the signature. the guard re-emits them
fn sentinel_edit(default: TextRange, marker: &str) -> (TextRange, Vec<Fragment>) {
    (default, vec![Fragment::Lit(marker.to_owned())])
}

/// what a marker stands in for
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MarkerSite {
    /// the default at this range, re-evaluated per call
    Default(TextRange),
    /// the parameter at this range, required after a defaulted one
    Required(TextRange),
}

/// The marker standing in the signature for each default re-evaluated per call, and for
/// each required parameter after a defaulted one: one object per site, bound at module
/// level, which the site's own guard tests for by identity.
///
/// A marker stands for "no argument was given" at exactly one parameter. Shared between
/// sites, one function's marker read off its signature would be an argument not given to
/// any other, so each site gets its own, under a name nothing else in the module has —
/// `_MISSING`, `_MISSING2`, … in the order the lowerings ask for them, which is the same
/// for every transpile of the module. The guard's test is then the one identity test the
/// sentinel always was, at the same cost per call.
///
/// The text a marker shows is the default as basedpython reads it — after the
/// lowerings that run ahead of this pass, such as the qualification of an enum member —
/// on one line: the comments in it are dropped, and a line break between two of its
/// tokens becomes a space, or nothing just inside a bracket. A string literal keeps its
/// own text, a triple-quoted one its own line breaks.
#[derive(Debug, Default)]
pub(crate) struct DefaultMarkers {
    /// the text each default's marker shows, by the default's range
    texts: HashMap<TextRange, String>,
    /// each site named so far, with its name
    named: RefCell<Vec<(MarkerSite, String)>>,
}

impl DefaultMarkers {
    /// the text of every parameter default `module` writes, read off `tokens`, the tokens of
    /// `source`
    pub(crate) fn collect(module: &[Stmt], tokens: &Tokens, source: &str) -> Self {
        struct Defaults<'a> {
            tokens: &'a Tokens,
            source: &'a str,
            texts: HashMap<TextRange, String>,
        }

        impl<'a> Visitor<'a> for Defaults<'a> {
            fn visit_stmt(&mut self, stmt: &'a Stmt) {
                if let Stmt::FunctionDef(f) = stmt {
                    for default in f
                        .parameters
                        .iter_non_variadic_params()
                        .filter_map(|parameter| parameter.default.as_deref())
                    {
                        self.texts.insert(
                            default.range(),
                            one_line(self.tokens, self.source, default.range()),
                        );
                    }
                }
                walk_stmt(self, stmt);
            }
        }

        let mut defaults = Defaults {
            tokens,
            source,
            texts: HashMap::new(),
        };
        defaults.visit_body(module);
        Self {
            texts: defaults.texts,
            named: RefCell::default(),
        }
    }

    /// the name `site`'s marker is bound under, and the line binding it
    fn marker(&self, written: WrittenNames, site: MarkerSite, source: &str) -> (String, String) {
        let mut named = self.named.borrow_mut();
        let name = if let Some((_, name)) = named.iter().find(|(named, _)| *named == site) {
            name.clone()
        } else {
            let name = written.fresh_outside("_MISSING", |candidate| {
                named.iter().any(|(_, name)| name == candidate)
            });
            named.push((site, name.clone()));
            name
        };
        let text = match site {
            MarkerSite::Default(range) => python_string_literal(
                self.texts
                    .get(&range)
                    .map_or(&source[range], String::as_str),
            ),
            MarkerSite::Required(_) => "None".to_owned(),
        };
        let definition = format!(
            "{name}: {} = {}({text}, __name__, \"{name}\")",
            written.imported("typing", "Any"),
            crate::runtime::DEFAULT.name()
        );
        (name, definition)
    }
}

/// the text of `range`, an expression in `source`, on one line and without its comments
fn one_line(tokens: &Tokens, source: &str, range: TextRange) -> String {
    let mut text = String::new();
    let mut previous: Option<(TokenKind, TextSize)> = None;
    let mut broken = false;
    for token in tokens.in_range(range) {
        match token.kind() {
            TokenKind::Comment => {}
            TokenKind::NonLogicalNewline | TokenKind::Newline => broken = true,
            kind => {
                if let Some((before, end)) = previous {
                    let gap = &source[TextRange::new(end, token.start())];
                    // a backslash continuation is a line break too
                    if broken || gap.contains(['\n', '\r']) {
                        let opens = matches!(
                            before,
                            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace
                        );
                        let closes =
                            matches!(kind, TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace);
                        if !opens && !closes {
                            text.push(' ');
                        }
                    } else {
                        text.push_str(gap);
                    }
                }
                text.push_str(&source[token.range()]);
                previous = Some((kind, token.end()));
                broken = false;
            }
        }
    }
    text
}

/// A default written into the signature at the end of the parameter it belongs to.
///
/// `=` spacing mirrors python style: spaced when the parameter is annotated.
fn written_default(pw: &ParameterWithDefault, value: &str) -> (TextRange, Vec<Fragment>) {
    let text = if pw.parameter.annotation.is_some() {
        format!(" = {value}")
    } else {
        format!("={value}")
    };
    (
        TextRange::empty(pw.parameter.range().end()),
        vec![Fragment::Lit(text)],
    )
}

/// The signature edits and body guards `f`'s parameter list calls for: the default an override
/// inherits from the method it overrides, a `_MISSING` sentinel wherever a default would
/// otherwise be shared between calls, and one wherever python's own parameter order is relaxed.
pub(crate) struct ParameterGuards {
    /// the `_MISSING` left where a written default stood. *relocating*: the
    /// guard evaluates the default's own source instead, so at this span this
    /// edit leads every lowering written inside the default
    pub(crate) sentinels: Vec<(TextRange, Vec<Fragment>)>,
    /// a default written into the signature that the source did not write — an
    /// inherited one, or the `_MISSING` a relaxed-order parameter needs. these
    /// relocate nothing; they add text at the end of a parameter
    pub(crate) written: Vec<(TextRange, Vec<Fragment>)>,
    pub(crate) guards: Vec<Guard>,
    /// the lines binding the markers the signature names, which the module defines ahead
    /// of everything that reads them
    pub(crate) definitions: Vec<String>,
    /// in a stub, the parameters python cannot declare where they stand: a
    /// required one after a defaulted one. python rejects the shape, and the
    /// sentinel default a module gets for it would declare the parameter
    /// optional — only the guard in the body says otherwise, and a stub has none
    pub(crate) undeclarable: Vec<String>,
}

/// The parameter guards `f` calls for. A stub is never run, so it gets only what
/// its signature declares: an inherited default, and none of the guards or the
/// sentinels they re-evaluate — the defaults it writes are never shared between
/// calls.
pub(crate) fn parameter_guards(
    source: &str,
    f: &StmtFunctionDef,
    written_names: WrittenNames,
    markers: &DefaultMarkers,
    types: &dyn TypeInfo,
    is_stub: bool,
) -> ParameterGuards {
    let mut sentinels = Vec::new();
    let mut written = Vec::new();
    let mut guards = Vec::new();
    let mut definitions = Vec::new();
    let mut undeclarable = Vec::new();
    let mut marker = |site| {
        let (name, definition) = markers.marker(written_names, site, source);
        definitions.push(definition);
        name
    };
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
                if !is_stub && !is_immutable_scalar_default(d) && !body_cannot_evaluate(d) {
                    let marker = marker(MarkerSite::Default(d.range()));
                    sentinels.push(sentinel_edit(d.range(), &marker));
                    guards.push(Guard::Reevaluate {
                        name: crate::python_parameter_name(params, &pw.parameter, written_names)
                            .to_string(),
                        marker,
                        default: standalone_expression(source, d.range()),
                    });
                }
            }
            // an inherited default is a value, so it needs no guard — and it is what makes
            // the parameters after it "after a default", exactly as a written one would
            None if let Some(value) = types.inherited_parameter_default(pw) => {
                seen_default = true;
                written.push(written_default(pw, &value));
            }
            None if seen_default && is_stub => {
                undeclarable.push(
                    crate::python_parameter_name(params, &pw.parameter, written_names).to_string(),
                );
            }
            None if seen_default => {
                let marker = marker(MarkerSite::Required(pw.range()));
                written.push(written_default(pw, &marker));
                guards.push(Guard::Required {
                    name: crate::python_parameter_name(params, &pw.parameter, written_names)
                        .to_string(),
                    marker,
                    function: f.name.id.to_string(),
                    type_error: written_names.builtin("TypeError"),
                });
            }
            None => {}
        }
    }
    for pw in &params.kwonlyargs {
        match pw.default.as_deref() {
            Some(d) if !is_stub && !is_immutable_scalar_default(d) && !body_cannot_evaluate(d) => {
                let marker = marker(MarkerSite::Default(d.range()));
                sentinels.push(sentinel_edit(d.range(), &marker));
                guards.push(Guard::Reevaluate {
                    name: crate::python_parameter_name(params, &pw.parameter, written_names)
                        .to_string(),
                    marker,
                    default: standalone_expression(source, d.range()),
                });
            }
            Some(_) => {}
            None => {
                if let Some(value) = types.inherited_parameter_default(pw) {
                    written.push(written_default(pw, &value));
                }
            }
        }
    }
    if !is_stub
        && let Some(parameter) = rebound_underscore(params, written_names)
        && reads_underscore(&f.body)
    {
        guards.push(Guard::Rebind {
            parameter: parameter.to_string(),
        });
    }
    ParameterGuards {
        sentinels,
        written,
        guards,
        definitions,
        undeclarable,
    }
}

/// the error for a parameter a stub cannot declare. see
/// [`ParameterGuards::undeclarable`]
pub(crate) fn undeclarable_error(function: &str, parameter: &str) -> String {
    format!(
        "a stub cannot declare parameter `{parameter}` of `{function}`: it is required but \
         follows a defaulted parameter, and python has no spelling for that in a signature"
    )
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
fn is_bodyless_init_shorthand(f: &StmtFunctionDef) -> bool {
    f.decorator_list
        .iter()
        .any(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "__init_method__"))
        && first_body_statement(f).is_none()
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
        let ParameterGuards {
            sentinels,
            written,
            guards,
            definitions,
            undeclarable,
        } = parameter_guards(
            self.source,
            f,
            self.written,
            self.markers,
            self.types,
            self.is_stub,
        );
        self.definitions.extend(definitions);
        self.undeclarable.extend(
            undeclarable
                .into_iter()
                .map(|parameter| (f.name.to_string(), parameter)),
        );
        self.relocating.extend(sentinels);
        self.edits.extend(written);
        if guards.is_empty() {
            return;
        }

        // the guards go at the start of the first body statement the source
        // actually wrote
        match body_prologue(self.source, f, &guards) {
            Some(anchored) => self.guards.extend(anchored),
            // nothing in the body came from the source, so there is nowhere to
            // put the guard. say so rather than splice it at a synthesized
            // node's offset, which lands inside the signature
            None => self.unanchored.push(f.name.to_string()),
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
    written: WrittenNames<'src>,
    markers: &'src DefaultMarkers,
    is_stub: bool,
}

impl<'src> MutableDefaultsPass<'src> {
    pub(crate) fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        markers: &'src DefaultMarkers,
        is_stub: bool,
    ) -> Self {
        Self {
            source,
            written,
            markers,
            is_stub,
        }
    }
}

/// Define `definitions`, the lines binding markers a module's signatures name, ahead of
/// everything that reads them, with the import their annotation names and the runtime
/// class they are made of.
///
/// A marker stands where the source wrote an annotated default, so a marker of a class of
/// its own there is a default the parameter cannot hold and a checker reading the output
/// says so. Declaring each marker `Any` is how typeshed declares its own sentinels, and it
/// costs the output nothing: a marker never reaches the body, whose guard replaces it
/// before anything reads it.
pub(crate) fn define_markers(
    written: WrittenNames,
    definitions: Vec<String>,
    ctx: &mut PassContext,
) {
    if definitions.is_empty() {
        return;
    }
    ctx.runtime.insert(crate::runtime::DEFAULT);
    ctx.required_imports
        .push(written.import_from("typing", &["Any"]));
    ctx.required_imports.extend(definitions);
}

impl TypeAwarePass for MutableDefaultsPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = MutableDefaults {
            source: self.source,
            written: self.written,
            markers: self.markers,
            types,
            edits: Vec::new(),
            relocating: Vec::new(),
            guards: Vec::new(),
            definitions: Vec::new(),
            unanchored: Vec::new(),
            is_stub: self.is_stub,
            undeclarable: Vec::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        if let Some((function, parameter)) = inner.undeclarable.first() {
            ctx.errors.push(undeclarable_error(function, parameter));
            return;
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
        define_markers(self.written, inner.definitions, ctx);
        ctx.template_edits.extend(inner.edits);
        ctx.relocating_edits.extend(inner.relocating);
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

    /// the runtime is taken from a module written beside the output, as a build writes it,
    /// so an expectation shows the import of the helper that makes the markers rather than
    /// its definition
    fn check_at(min_version: crate::PythonVersion, input: &str, expected: &str) {
        let config = crate::Config {
            min_version,
            runtime_module: Some("_by_runtime".to_owned()),
            ..crate::Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// a method's defaults are part of what it declares, so an override that leaves one out
    /// writes the base's into its own signature — that is what makes the call site's missing
    /// argument mean anything at runtime
    #[test]
    fn an_override_writes_the_default_it_inherits() {
        check(
            indoc! {"
                class A:
                    def f(self, a = 1): ...

                class B(A):
                    def f(self, a):
                        print(a)
            "},
            indoc! {"
                class A:
                    def f(self, a = 1): ...

                class B(A):
                    def f(self, a=1):
                        print(a)
            "},
        );
    }

    /// `=` spacing follows python style, which spaces it when the parameter is annotated
    #[test]
    fn an_annotated_parameter_takes_the_spaced_form() {
        check(
            indoc! {"
                class A:
                    def f(self, a: int = 1, *, k: str = \"x\"): ...

                class B(A):
                    def f(self, a: int, *, k: str):
                        print(a, k)
            "},
            indoc! {"
                class A:
                    def f(self, a: int = 1, *, k: str = \"x\"): ...

                class B(A):
                    def f(self, a: int = 1, *, k: str = \"x\"):
                        print(a, k)
            "},
        );
    }

    /// an inherited default is a default like any other, so a parameter after it is one
    /// python would reject — the same sentinel and raising guard a written default calls for
    #[test]
    fn a_parameter_after_an_inherited_default_gets_its_guard() {
        check(
            indoc! {"
                class A:
                    def f(self, a = 1, b = 2): ...

                class B(A):
                    def f(self, a, b):
                        print(a, b)
            "},
            indoc! {"
                class A:
                    def f(self, a = 1, b = 2): ...

                class B(A):
                    def f(self, a=1, b=2):
                        print(a, b)
            "},
        );
    }

    /// basedpython re-evaluates a non-scalar default on every call, in the scope its own `def`
    /// was written in — there is no value to write into the override's signature, and the
    /// parameter stays required
    #[test]
    fn an_expression_default_is_not_carried_to_an_override() {
        check(
            indoc! {"
                class A:
                    def f(self, a = []): ...

                class B(A):
                    def f(self, a):
                        print(a)
            "},
            indoc! {"
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
                class A:
                    def f(self, a = _MISSING): 
                        if a is _MISSING:
                            a = []
                        ...

                class B(A):
                    def f(self, a):
                        print(a)
            "},
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(None, __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"{}\", __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"{1, 2}\", __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING2: Any = _by_default(\"{}\", __name__, \"_MISSING2\")
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
                def f(x=_MISSING, y=_MISSING2):
                    if x is _MISSING:
                        x = []
                    if y is _MISSING2:
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default("[]", __name__, "_MISSING")
                def f(x=_MISSING):
                    """doc"""
                    if x is _MISSING:
                        x = []
                    pass
            "#},
        );
    }

    /// a docstring written on the `def`'s own line is a body of exactly one statement, and
    /// nothing can follow a statement there. the guard needs a line, so the docstring is
    /// given one first — written where it stood, the guard was indented under a suite that
    /// had already closed and the emitted file did not parse
    #[test]
    fn a_docstring_on_the_def_line_is_given_a_line_of_its_own() {
        check(
            "def f(x=[]): \"\"\"doc\"\"\"\n",
            "from typing import Any\nfrom _by_runtime import _by_default\n_MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")\ndef f(x=_MISSING): \n    \"\"\"doc\"\"\"\n    if x is _MISSING:\n        x = []\n",
        );
    }

    /// a body written on the `def`'s own line can hold more than the one statement, and the
    /// break and the guard then want different offsets: the break goes ahead of the
    /// docstring, which has to stay the body's first statement to stay a docstring, and the
    /// guard goes ahead of the statement that follows it. taking both from the guard's
    /// offset left the docstring on the clause's line with an indented suite under it, and
    /// the whole file failed to parse with `Unexpected indentation`
    #[test]
    fn a_docstring_followed_by_statements_on_the_def_line_is_given_a_line_of_its_own() {
        check(
            "def f(x=[]): \"\"\"doc\"\"\"; x.append(1); return x\n",
            "from typing import Any\nfrom _by_runtime import _by_default\n_MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")\ndef f(x=_MISSING): \n    \"\"\"doc\"\"\"; \n    if x is _MISSING:\n        x = []\n    x.append(1); return x\n",
        );
    }

    /// the same body without a docstring breaks at the one offset, because the statement the
    /// guard goes ahead of is the body's first
    #[test]
    fn statements_on_the_def_line_without_a_docstring_take_one_break() {
        check(
            "def f(x=[]): x.append(1); return x\n",
            "from typing import Any\nfrom _by_runtime import _by_default\n_MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")\ndef f(x=_MISSING): \n    if x is _MISSING:\n        x = []\n    x.append(1); return x\n",
        );
    }

    /// and a method reached the same way, since the break is indented from the `def` rather
    /// than from the module
    #[test]
    fn a_method_body_on_the_clause_line_is_broken_at_its_own_indentation() {
        check(
            "class C:\n    def m(self, x=[]): \"\"\"doc\"\"\"; x.append(1); return x\n",
            "from typing import Any\nfrom _by_runtime import _by_default\n_MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")\nclass C:\n    def m(self, x=_MISSING): \n        \"\"\"doc\"\"\"; \n        if x is _MISSING:\n            x = []\n        x.append(1); return x\n",
        );
    }

    /// with no guard to write, nothing breaks the line at all: a body on the clause's line
    /// is left exactly as it was written
    #[test]
    fn a_body_on_the_def_line_is_left_alone_when_nothing_writes_into_it() {
        unchanged("def f(x: int) -> int: \"\"\"doc\"\"\"; return x\n");
        unchanged("def f(x: int) -> int: return x\n");
        unchanged("class C:\n    def m(self, x: int) -> int: \"\"\"doc\"\"\"; return x\n");
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING2: Any = _by_default(\"{}\", __name__, \"_MISSING2\")
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
                def f(x=_MISSING):
                    if x is _MISSING:
                        x = []
                    pass
                def g(y=_MISSING2):
                    if y is _MISSING2:
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(None, __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(None, __name__, \"_MISSING\")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING2: Any = _by_default(None, __name__, \"_MISSING2\")
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
                def f(x=_MISSING, a=_MISSING2):
                    if x is _MISSING:
                        x = []
                    if a is _MISSING2:
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default("f\"asdf{data}\"", __name__, "_MISSING")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default("t\"asdf{data}\"", __name__, "_MISSING")
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"a + 1\", __name__, \"_MISSING\")
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
            "from typing import Any\nfrom _by_runtime import _by_default\n_MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")\ndef f(\n    a: int = _MISSING\n) -> int: \n    if a is _MISSING:\n        a = []\n    ...\n",
        );
    }

    #[test]
    fn inline_ellipsis_body() {
        check(
            "def f(x=[]): ...",
            "from typing import Any\nfrom _by_runtime import _by_default\n_MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")\ndef f(x=_MISSING): \n    if x is _MISSING:\n        x = []\n    ...",
        );
    }

    #[test]
    fn default_lowerings_survive() {
        // the default is re-emitted in the body through a `Src` passthrough, so
        // the lowerings written inside it land in the guard rather than being
        // dropped with the signature they came from. `1 is int` is a type test
        // the checker settles, so what lands is the constant it settled on
        check(
            indoc! {"
                def f(x = [1 is int]):
                    return x
            "},
            indoc! {"
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[1 is int]\", __name__, \"_MISSING\")
                def f(x = _MISSING):
                    if x is _MISSING:
                        x = [True]
                    return x
            "},
        );
    }

    #[test]
    fn default_lowering_spanning_the_whole_default_survives() {
        // the sentinel and the `is` lowering claim the *same* span. the sentinel
        // relocates the default, so it decides the signature and the lowering
        // materializes in the guard — which holds even here, where the lowering
        // settles to a constant and so replaces the span just as flatly
        check(
            indoc! {"
                def f(x = 1 is int):
                    return x
            "},
            indoc! {"
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"1 is int\", __name__, \"_MISSING\")
                def f(x = _MISSING):
                    if x is _MISSING:
                        x = True
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
                from typing import Any
                from _by_runtime import _by_default
                _MISSING: Any = _by_default(\"[]\", __name__, \"_MISSING\")
                def f(xs: list[int] = _MISSING) -> int:
                    if xs is _MISSING:
                        xs = []
                    a: int | None = None
                    return a if a is not None else len(xs)
            "},
        );
    }

    fn stub() -> crate::Config {
        crate::Config {
            is_stub: true,
            ..crate::Config::test_default()
        }
    }

    /// a stub is never run, so a default it declares is never shared between
    /// calls. it stays as written, with no guard
    #[test]
    fn a_stub_keeps_a_mutable_default() {
        let source = "def f(xs: list[int] = [], *, ys: list[int] = []) -> None: ...\n";
        assert_eq!(transpile(source, &stub()).unwrap(), source);
    }

    /// an inherited default is part of what an override declares, which a stub
    /// is for
    #[test]
    fn a_stub_writes_the_default_an_override_inherits() {
        let out = transpile(
            indoc! {"
                class A:
                    def f(self, a: int = 1) -> None: ...

                class B(A):
                    def f(self, a: int) -> None: ...
            "},
            &stub(),
        )
        .unwrap();
        assert_eq!(
            out,
            indoc! {"
                class A:
                    def f(self, a: int = 1) -> None: ...

                class B(A):
                    def f(self, a: int = 1) -> None: ...
            "}
        );
    }

    /// python rejects a required parameter after a defaulted one. a module gets
    /// a sentinel default and a guard in the body that raises, and a stub has no
    /// body for the guard, so the default alone would declare the parameter
    /// optional
    #[test]
    fn a_stub_cannot_declare_a_required_parameter_after_a_default() {
        let error = transpile("def f(x: int = 1, y: int) -> None: ...\n", &stub()).unwrap_err();
        assert!(
            error.contains("a stub cannot declare parameter `y` of `f`"),
            "got: {error}"
        );
    }

    /// the sentinel stands for "no argument was given", which a value the module itself binds
    /// is not — the guard would take a real argument for a missing one, and the module's own
    /// binding would be the one overwritten
    #[test]
    fn the_sentinel_goes_past_a_name_the_module_binds() {
        check(
            indoc! {"
                _MISSING = 5

                def f(xs: list[int] = []) -> int:
                    return len(xs) + _MISSING
            "},
            indoc! {"
                from typing import Any
                from _by_runtime import _by_default
                _MISSING2: Any = _by_default(\"[]\", __name__, \"_MISSING2\")
                _MISSING = 5

                def f(xs: list[int] = _MISSING2) -> int:
                    if xs is _MISSING2:
                        xs = []
                    return len(xs) + _MISSING
            "},
        );
    }
}
