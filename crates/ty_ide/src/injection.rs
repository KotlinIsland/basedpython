//! basedpython language injection: where a fragment of another language sits
//! inside a basedpython file, and which language it is written in.
//!
//! nothing here reads a fragment. an injection says *where* one is and *what*
//! it is; acting on that is the editor's job, which injects the language it was
//! told and lets its own support for that language take over. so a language
//! `by` knows nothing about works exactly as well as basedpython does, and the
//! only thing this module has to get right is deciding the language.
//!
//! two markers say it outright. a comment above a statement, which is how
//! editors have spelled this for years:
//!
//! ```by
//! # language=javascript
//! script = "const x = 1"
//! ```
//!
//! and a parameter that declares what it is handed, which is the same statement
//! made once for every caller:
//!
//! ```by
//! def run(source: Annotated[str, "language=javascript"]): ...
//!
//! run("const x = 1")
//! ```
//!
//! the second one travels. a string handed to a parameter that only passes it
//! on is the same fragment one call further out, so the language reaches it
//! too:
//!
//! ```by
//! def run_twice(source: str):
//!     run(source)
//!     run(source)
//!
//! run_twice("const x = 1")  # javascript, by way of `run`
//! ```
//!
//! that walk is deliberately timid, because it is inference nobody wrote down.
//! it follows a parameter only while the parameter is handed straight on: a body
//! that rebinds the name, or that uses it from a nested scope, gives up and
//! reports nothing. being wrong here would put a language on a string that is
//! not written in it, and an editor would then report ordinary text as broken
//! code.

use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::visitor::source_order::{
    SourceOrderVisitor, TraversalSignal, walk_body, walk_expr, walk_stmt,
};
use ruff_python_ast::{self as ast, AnyNodeRef};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashMap;
use ty_python_core::ProgramFile;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_semantic::SemanticModel;
use ty_python_semantic::types::ide_support::call_signature_details;

use crate::Db;

/// How far a language is carried from the parameter that declares it back
/// towards the call sites that supply the string.
///
/// Each hop is one call, so this bounds how many functions a fragment may be
/// handed through untouched. Chains of pure pass-through parameters do not get
/// deep in real code, and an unbounded walk would pay for a whole call graph on
/// every keystroke.
const MAX_PROPAGATION_DEPTH: usize = 8;

/// The text both spellings share.
const MARKER: &str = "language=";

/// A fragment of another language inside a basedpython file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    /// The language the fragment is written in, exactly as the marker spelled
    /// it. Nothing here interprets it — an editor matches it against the
    /// languages it has.
    pub language: String,

    /// Where the fragment's text is in the host file, quotes excluded, one
    /// range per literal part.
    ///
    /// A plain string has one. An implicitly concatenated string has a range per
    /// piece, in source order, because the fragment is their contents joined:
    /// `"SELECT *" " FROM t"` is one query written as two literals.
    pub ranges: Vec<TextRange>,

    /// What decided the language, which is what an editor shows a user who asks
    /// why a string is being treated as another language.
    pub origin: InjectionOrigin,
}

/// What decided that a fragment is in a particular language.
///
/// Ordered by how directly the marker names the string it applies to, which is
/// the order two answers about one string are resolved in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InjectionOrigin {
    /// A `# language=` comment on the statement above.
    Comment,

    /// The parameter this string is passed to declares the language.
    Declared,

    /// The parameter this string is passed to hands it on to one that declares
    /// the language.
    Propagated,
}

impl InjectionOrigin {
    /// The wire name, which is what a client sees.
    pub const fn as_str(self) -> &'static str {
        match self {
            InjectionOrigin::Comment => "comment",
            InjectionOrigin::Declared => "declared",
            InjectionOrigin::Propagated => "propagated",
        }
    }
}

/// Every fragment of another language in `file`, in source order.
pub fn injections<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Vec<Injection> {
    let parsed = parsed_module(db, file.python_file(db));
    let module = parsed.load(db);
    let model = SemanticModel::new(db, file);
    let source = source_text(db, file.file(db));

    let markers = comment_markers(&module, source.as_str());

    let mut finder = InjectionFinder {
        db,
        model: &model,
        source: source.as_str(),
        markers: &markers,
        expectations: Expectations::default(),
        found: Vec::new(),
    };
    walk_body(&mut finder, module.suite());

    let mut found = finder.found;
    // One string can be reached both ways — a marked statement whose string is
    // also a call argument. Sorting by origin puts the marker the reader wrote
    // closest to the string first, and the duplicate behind it is dropped.
    found.sort_by_key(|injection| {
        (
            injection.ranges.first().map(Ranged::start),
            injection.origin,
        )
    });
    found.dedup_by(|left, right| left.ranges == right.ranges);
    found
}

/// Every `# language=<id>` comment in the file, as (comment range, language).
fn comment_markers(module: &ParsedModuleRef, source: &str) -> Vec<(TextRange, String)> {
    module
        .tokens()
        .iter()
        .filter(|token| token.kind() == TokenKind::Comment)
        .filter_map(|token| {
            let language = language_from_marker(&source[token.range()])?;
            Some((token.range(), language))
        })
        .collect()
}

/// The language a marker names, from either spelling's text.
///
/// The id runs to the next space, so a comment can carry a trailing note and a
/// language whose name has a space in it cannot be spelled. No language has one.
fn language_from_marker(text: &str) -> Option<String> {
    let (_, rest) = text.split_once(MARKER)?;
    let id = rest.split_whitespace().next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// What a parameter was already found to expect, so that a function called many
/// times in one file is looked at once.
///
/// Only answers reached without running into [`MAX_PROPAGATION_DEPTH`] are kept,
/// which is every answer started from a call site: one cut short by the bound
/// depends on how far along a chain it was asked, and reusing it somewhere
/// nearer the start would report nothing where there was something to report.
type Expectations<'db> = FxHashMap<(Definition<'db>, String), Option<(String, InjectionOrigin)>>;

struct InjectionFinder<'a, 'db> {
    db: &'db dyn Db,
    model: &'a SemanticModel<'db>,
    source: &'a str,
    markers: &'a [(TextRange, String)],
    expectations: Expectations<'db>,
    found: Vec<Injection>,
}

impl InjectionFinder<'_, '_> {
    /// The language a marker comment gives the statement starting at
    /// `statement`, if nothing but blank lines and further comments separate
    /// them.
    ///
    /// A marker with a statement between it and this one belongs to that one.
    /// More comments do not break the run, so a marker can be written above the
    /// note that explains it.
    fn marker_for_statement(&self, statement: &ast::Stmt) -> Option<String> {
        self.markers
            .iter()
            .rev()
            .find(|(range, _)| range.end() <= statement.start())
            .filter(|(range, _)| {
                let between = TextRange::new(range.end(), statement.start());
                self.source
                    .get(between.start().to_usize()..between.end().to_usize())
                    .is_some_and(|text| {
                        text.lines().all(|line| {
                            line.trim().is_empty() || line.trim_start().starts_with('#')
                        })
                    })
            })
            .map(|(_, language)| language.clone())
    }

    /// Record every string literal in `statement`'s own expressions as a
    /// fragment of `language`.
    ///
    /// A nested statement's strings are not its own: a marker above a `def`
    /// marks the signature it is attached to, not every string in the body.
    fn mark_statement(&mut self, statement: &ast::Stmt, language: &str) {
        let mut strings = StringLiteralCollector::default();
        walk_stmt(&mut strings, statement);
        for string in strings.found {
            self.found.push(Injection {
                language: language.to_string(),
                ranges: content_ranges(string),
                origin: InjectionOrigin::Comment,
            });
        }
    }

    /// Record the string arguments of `call` whose parameters name a language.
    fn mark_call_arguments(&mut self, call: &ast::ExprCall) {
        for (index, argument) in call.arguments.iter_source_order().enumerate() {
            let ast::Expr::StringLiteral(string) = argument.value() else {
                continue;
            };
            let Some((language, origin)) = self.language_of_argument(call, index) else {
                continue;
            };
            self.found.push(Injection {
                language,
                ranges: content_ranges(string),
                origin,
            });
        }
    }

    /// The language declared for the parameter that argument `index` of `call`
    /// binds to.
    fn language_of_argument(
        &mut self,
        call: &ast::ExprCall,
        index: usize,
    ) -> Option<(String, InjectionOrigin)> {
        for (definition, name) in parameters_matching(self.model, call, index) {
            if let Some(known) = self.expectations.get(&(definition, name.clone())) {
                if let Some(found) = known {
                    return Some(found.clone());
                }
                continue;
            }
            let found = parameter_language(self.db, definition, &name, 0);
            self.expectations.insert((definition, name), found.clone());
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

/// The parameters argument `index` of `call` could bind to, as the function that
/// declares each and the parameter's name.
///
/// More than one when the callee is a union or is overloaded, in which case the
/// argument is the same string whichever signature took it.
fn parameters_matching<'db>(
    model: &SemanticModel<'db>,
    call: &ast::ExprCall,
    index: usize,
) -> Vec<(Definition<'db>, String)> {
    call_signature_details(model, call)
        .into_iter()
        .filter_map(|details| {
            let definition = details.definition?;
            let parameter = (*details.argument_to_displayed_parameter_mapping.get(index)?)?;
            Some((definition, details.parameters.get(parameter)?.name.clone()))
        })
        .collect()
}

impl<'db> SourceOrderVisitor<'db> for InjectionFinder<'_, 'db> {
    fn visit_stmt(&mut self, statement: &'db ast::Stmt) {
        if let Some(language) = self.marker_for_statement(statement) {
            self.mark_statement(statement, &language);
        }
        walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expr: &'db ast::Expr) {
        if let ast::Expr::Call(call) = expr {
            self.mark_call_arguments(call);
        }
        walk_expr(self, expr);
    }
}

/// The content ranges of a string expression, one per literal part.
fn content_ranges(string: &ast::ExprStringLiteral) -> Vec<TextRange> {
    string
        .value
        .iter()
        .map(ast::StringLiteral::content_range)
        .collect()
}

/// The language the parameter named `name` of the function `definition` defines
/// expects: either because it says so, or because it hands the value on to a
/// parameter that does.
fn parameter_language(
    db: &dyn Db,
    definition: Definition<'_>,
    name: &str,
    depth: usize,
) -> Option<(String, InjectionOrigin)> {
    if depth > MAX_PROPAGATION_DEPTH {
        return None;
    }

    let parsed = parsed_module(db, definition.python_file(db)).load(db);
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let function = function.node(&parsed);

    let parameter = function
        .parameters
        .iter()
        .find(|parameter| parameter.name().as_str() == name)?;

    if let Some(annotation) = parameter.annotation()
        && let Some(language) = language_of_annotation(annotation)
    {
        return Some((language, InjectionOrigin::Declared));
    }

    // Nothing declared here, so follow the value: if the body does no more than
    // hand this parameter on, whatever the parameter it reaches expects is what
    // reaches this one too.
    let mut uses = PassThroughUses::new(name);
    walk_body(&mut uses, &function.body);
    if uses.rebound || uses.found.is_empty() {
        return None;
    }

    let model = SemanticModel::new(db, definition.program_file(db));
    uses.found.into_iter().find_map(|(call, index)| {
        parameters_matching(&model, call, index)
            .into_iter()
            .find_map(|(inner, name)| parameter_language(db, inner, &name, depth + 1))
            // Whatever named the language, this parameter is one call further
            // out than the one that did.
            .map(|(language, _)| (language, InjectionOrigin::Propagated))
    })
}

/// The language an annotation declares, from `Annotated[T, "language=<id>"]`.
///
/// [PEP 593](https://peps.python.org/pep-0593/) put `Annotated`'s metadata there
/// for exactly this: a value a tool agrees to read, which the type checker
/// passes through untouched. So a marked parameter is still a `str` parameter,
/// and marking one cannot make working code stop checking.
fn language_of_annotation(annotation: &ast::Expr) -> Option<String> {
    let ast::Expr::Subscript(subscript) = annotation else {
        return None;
    };
    if !is_annotated(&subscript.value) {
        return None;
    }
    let ast::Expr::Tuple(arguments) = subscript.slice.as_ref() else {
        return None;
    };
    // The first element is the type itself; the rest is the metadata.
    arguments
        .elts
        .iter()
        .skip(1)
        .find_map(|element| match element {
            ast::Expr::StringLiteral(string) => language_from_marker(string.value.to_str()),
            _ => None,
        })
}

/// Whether an expression spells `Annotated`, however it was imported.
fn is_annotated(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Name(name) => name.id.as_str() == "Annotated",
        ast::Expr::Attribute(attribute) => attribute.attr.as_str() == "Annotated",
        _ => false,
    }
}

/// The string literals in one statement's own expressions.
#[derive(Default)]
struct StringLiteralCollector<'a> {
    /// How many statements the walk has entered, so that the one it started at
    /// is entered and everything nested inside it is not.
    depth: usize,
    found: Vec<&'a ast::ExprStringLiteral>,
}

impl<'a> SourceOrderVisitor<'a> for StringLiteralCollector<'a> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if node.is_statement() {
            self.depth += 1;
            if self.depth > 1 {
                return TraversalSignal::Skip;
            }
        }
        TraversalSignal::Traverse
    }

    fn visit_expr(&mut self, expr: &'a ast::Expr) {
        if let ast::Expr::StringLiteral(string) = expr {
            self.found.push(string);
        }
        walk_expr(self, expr);
    }
}

/// The calls a parameter is handed to, unchanged, inside a function body.
struct PassThroughUses<'a> {
    name: &'a str,
    /// The call and argument index of each use as an argument.
    found: Vec<(&'a ast::ExprCall, usize)>,
    /// Whether the body assigns the name, which makes every use of it ambiguous.
    rebound: bool,
}

impl<'a> PassThroughUses<'a> {
    fn new(name: &'a str) -> Self {
        Self {
            name,
            found: Vec::new(),
            rebound: false,
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for PassThroughUses<'a> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        // A nested scope may bind the same name to something else entirely, and
        // deciding which binding a use means is more than this walk can do.
        match node {
            AnyNodeRef::StmtFunctionDef(_)
            | AnyNodeRef::StmtClassDef(_)
            | AnyNodeRef::ExprLambda(_) => TraversalSignal::Skip,
            _ => TraversalSignal::Traverse,
        }
    }

    fn visit_stmt(&mut self, statement: &'a ast::Stmt) {
        if binds_name(statement, self.name) {
            self.rebound = true;
        }
        walk_stmt(self, statement);
    }

    fn visit_expr(&mut self, expr: &'a ast::Expr) {
        if let ast::Expr::Call(call) = expr {
            for (index, argument) in call.arguments.iter_source_order().enumerate() {
                if let ast::Expr::Name(name) = argument.value()
                    && name.id.as_str() == self.name
                {
                    self.found.push((call, index));
                }
            }
        }
        walk_expr(self, expr);
    }
}

/// Whether a statement binds `name` to something new.
fn binds_name(statement: &ast::Stmt, name: &str) -> bool {
    let mut bound = false;
    let mut note = |target: &ast::Expr| {
        if let ast::Expr::Name(bound_name) = target
            && bound_name.id.as_str() == name
        {
            bound = true;
        }
    };
    match statement {
        ast::Stmt::Assign(assign) => assign.targets.iter().for_each(&mut note),
        ast::Stmt::AnnAssign(assign) => note(&assign.target),
        ast::Stmt::AugAssign(assign) => note(&assign.target),
        ast::Stmt::For(for_statement) => note(&for_statement.target),
        ast::Stmt::With(with_statement) => {
            for item in &with_statement.items {
                if let Some(target) = &item.optional_vars {
                    note(target);
                }
            }
        }
        _ => {}
    }
    bound
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{CursorTest, IntoDiagnostic};
    use insta::assert_snapshot;
    use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, LintName, Severity, Span};
    use ruff_db::files::File;

    impl CursorTest {
        fn injections(&self) -> String {
            let found = injections(&self.db, self.program_file(self.cursor.file));
            if found.is_empty() {
                return "no injections".to_string();
            }
            let file = self.cursor.file;
            self.render_diagnostics(
                found
                    .into_iter()
                    .map(|injection| FoundInjection { file, injection }),
            )
        }
    }

    /// A file with nothing but the source under test in it. The cursor the
    /// harness insists on is parked at the end, where it marks nothing.
    fn injection_test(source: &str) -> CursorTest {
        CursorTest::builder()
            .source("main.by", format!("{source}<CURSOR>"))
            .build()
    }

    struct FoundInjection {
        file: File,
        injection: Injection,
    }

    impl IntoDiagnostic for FoundInjection {
        fn into_diagnostic(self) -> Diagnostic {
            let mut ranges = self.injection.ranges.into_iter();
            let mut main = Diagnostic::new(
                DiagnosticId::Lint(LintName::of("injection")),
                Severity::Info,
                format!(
                    "{} ({})",
                    self.injection.language,
                    self.injection.origin.as_str()
                ),
            );
            let first = ranges.next().expect("an injection to cover something");
            main.annotate(Annotation::primary(Span::from(self.file).with_range(first)));
            for range in ranges {
                main.annotate(Annotation::secondary(
                    Span::from(self.file).with_range(range),
                ));
            }
            main
        }
    }

    #[test]
    fn comment_marks_the_statement_below_it() {
        let test = injection_test(
            "\
# language=javascript
script = \"const x = 1\"
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: javascript (comment)
         --> main.by:2:11
          |
        2 | script = "const x = 1"
          |           ^^^^^^^^^^^
        "#);
    }

    #[test]
    fn a_comment_that_is_not_a_marker_marks_nothing() {
        let test = injection_test(
            "\
# just a comment
script = \"const x = 1\"
",
        );

        assert_snapshot!(test.injections(), @"no injections");
    }

    #[test]
    fn the_marked_language_is_whatever_was_written() {
        let test = injection_test(
            "\
# language=basedpython
snippet = \"None\"
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: basedpython (comment)
         --> main.by:2:12
          |
        2 | snippet = "None"
          |            ^^^^
        "#);
    }

    #[test]
    fn a_note_below_the_marker_does_not_break_it() {
        let test = injection_test(
            "\
# language=javascript
# what this runs, and why
script = \"const x = 1\"
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: javascript (comment)
         --> main.by:3:11
          |
        3 | script = "const x = 1"
          |           ^^^^^^^^^^^
        "#);
    }

    #[test]
    fn a_statement_between_takes_the_marker() {
        let test = injection_test(
            "\
# language=javascript
first = \"const x = 1\"
second = \"not javascript\"
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: javascript (comment)
         --> main.by:2:10
          |
        2 | first = "const x = 1"
          |          ^^^^^^^^^^^
        "#);
    }

    #[test]
    fn a_marker_above_a_def_reaches_its_signature_and_not_its_body() {
        let test = injection_test(
            "\
# language=javascript
def f(source = \"const x = 1\"):
    other = \"not javascript\"
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: javascript (comment)
         --> main.by:2:17
          |
        2 | def f(source = "const x = 1"):
          |                 ^^^^^^^^^^^
        "#);
    }

    #[test]
    fn concatenated_parts_are_one_fragment() {
        let test = injection_test(
            "\
# language=sql
query = \"SELECT *\" \" FROM t\"
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: sql (comment)
         --> main.by:2:10
          |
        2 | query = "SELECT *" " FROM t"
          |          ^^^^^^^^   -------
        "#);
    }

    #[test]
    fn a_parameter_declares_what_it_is_handed() {
        let test = injection_test(
            "\
from typing import Annotated

def run(source: Annotated[str, \"language=javascript\"]): ...

run(\"const x = 1\")
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: javascript (declared)
         --> main.by:5:6
          |
        5 | run("const x = 1")
          |      ^^^^^^^^^^^
        "#);
    }

    #[test]
    fn a_keyword_argument_is_matched_to_its_parameter() {
        let test = injection_test(
            "\
from typing import Annotated

def run(first: str, source: Annotated[str, \"language=javascript\"]): ...

run(source=\"const x = 1\", first=\"plain\")
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: javascript (declared)
         --> main.by:5:13
          |
        5 | run(source="const x = 1", first="plain")
          |             ^^^^^^^^^^^
        "#);
    }

    #[test]
    fn the_language_travels_through_a_parameter_passed_straight_on() {
        let test = injection_test(
            "\
from typing import Annotated

def f1(s: Annotated[str, \"language=basedpython\"]): ...

def f2(s: str):
    f1(s)

f2(\"None\")
",
        );

        assert_snapshot!(test.injections(), @r#"
        info[injection]: basedpython (propagated)
         --> main.by:8:5
          |
        8 | f2("None")
          |     ^^^^
        "#);
    }

    #[test]
    fn a_rebound_parameter_stops_the_language_travelling() {
        let test = injection_test(
            "\
from typing import Annotated

def f1(s: Annotated[str, \"language=basedpython\"]): ...

def f2(s: str):
    s = \"something else\"
    f1(s)

f2(\"None\")
",
        );

        assert_snapshot!(test.injections(), @"no injections");
    }

    #[test]
    fn the_language_travels_across_modules() {
        let test = CursorTest::builder()
            .source(
                "runner.by",
                "\
from typing import Annotated

def run(source: Annotated[str, \"language=javascript\"]): ...
",
            )
            .source(
                "main.by",
                "\
from runner import run

def run_later(source: str):
    run(source)

run_later(\"const x = 1\")<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.injections(), @r#"
        info[injection]: javascript (propagated)
         --> main.by:6:12
          |
        6 | run_later("const x = 1")
          |            ^^^^^^^^^^^
        "#);
    }
}
