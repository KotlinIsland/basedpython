//! checking the formatting of a value — the `__format__` call an f-string
//! replacement field makes, and whether the result is worth printing
//!
//! an f-string field is a call: `f"{value:>10}"` runs
//! `type(value).__format__(value, ">10")`, after the conversion (`!r`, `!s`,
//! `!a`) has had its turn. checking the field is therefore checking that call,
//! and the format spec is just its argument
//!
//! on top of that, the spec's *content* means something when — and only when —
//! the `__format__` being called is one of the four standard implementations,
//! which is what [`format_target`] establishes. `datetime.__format__` reads the
//! same string as strftime codes, so applying the mini-language to it would
//! invent errors

use ruff_python_ast as ast;
use ruff_python_literal::format::{FormatSpec, FormatSpecError};
use ruff_python_literal::mini_language::{FormatSpecViolation, FormatTarget};
use ruff_python_literal::strftime::{self, DirectiveKind};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::types::class::{ClassLiteral, ClassType};
use crate::types::class_base::ClassBase;
use crate::types::context::InferContext;
use crate::Db;
use crate::types::diagnostic::INVALID_FORMAT_SPEC;
use crate::types::{CallArguments, KnownClass, Type, TypeContext};

/// the language a format spec is written in, which is decided by the
/// `__format__` that reads it
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SpecLanguage {
    /// `str`, `int`, `float` and `complex` read the format specification
    /// mini-language
    MiniLanguage(FormatTarget),
    /// `date`, `time` and `datetime` hand the spec to `strftime`
    Strftime,
}

/// which language `ty`'s `__format__` reads its spec in, if it is one we know
///
/// this is decided by the class that *owns* `__format__`, not by the class of
/// the value: a subclass of `int` that adds no `__format__` of its own still
/// formats by `int`'s rules
pub fn spec_language<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<SpecLanguage> {
    let owner = owner_of(
        db,
        ty.erase_restriction(db).nominal_class(db)?,
        "__format__",
    )?;
    let literal = owner.class_literal(db);
    if is_named(db, literal, STRFTIME_OWNERS) {
        return Some(SpecLanguage::Strftime);
    }
    match literal.known(db)? {
        KnownClass::Str => Some(SpecLanguage::MiniLanguage(FormatTarget::Str)),
        KnownClass::Int | KnownClass::Bool => Some(SpecLanguage::MiniLanguage(FormatTarget::Int)),
        KnownClass::Float => Some(SpecLanguage::MiniLanguage(FormatTarget::Float)),
        KnownClass::Complex => Some(SpecLanguage::MiniLanguage(FormatTarget::Complex)),
        _ => None,
    }
}

/// the classes whose `__format__` hands the spec to `strftime`
///
/// `datetime` inherits `date`'s, so matching the owner covers all three
const STRFTIME_OWNERS: &[&str] = &["datetime.date", "datetime.time"];

/// which mini-language target formats `ty`, when that is the language it reads
pub fn format_target<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<FormatTarget> {
    match spec_language(db, ty)? {
        SpecLanguage::MiniLanguage(target) => Some(target),
        SpecLanguage::Strftime => None,
    }
}

/// the class in `class`'s MRO that defines `name` itself
fn owner_of<'db>(db: &'db dyn Db, class: ClassType<'db>, name: &str) -> Option<ClassType<'db>> {
    class
        .iter_mro(db)
        .filter_map(ClassBase::into_class)
        .find(|base| !base.own_class_member(db, None, name).is_undefined())
}

/// whether `class` is one of a list of class names
///
/// a name is qualified (`datetime.date`); a class in `builtins` may also be
/// spelled bare, matching how the other class-list options read
fn is_named<'db>(
    db: &'db dyn Db,
    class: ClassLiteral<'db>,
    names: impl IntoIterator<Item: AsRef<str>>,
) -> bool {
    let names: Vec<_> = names.into_iter().collect();
    if names.is_empty() {
        return false;
    }
    let qualified = class.qualified_name(db).to_string();
    let matches = |name: &str| names.iter().any(|entry| entry.as_ref() == name);
    matches(&qualified) || qualified.strip_prefix("builtins.").is_some_and(matches)
}

/// the format spec written in a replacement field
pub(crate) struct WrittenSpec<'ast> {
    /// the spec text, when every part of it is literal. a spec containing a
    /// nested replacement field (`f"{v:{width}}"`) is only known at runtime
    pub(crate) literal: Option<&'ast str>,
    /// the range the spec occupies, for reporting
    pub(crate) range: TextRange,
}

impl<'ast> WrittenSpec<'ast> {
    /// read the spec off a replacement field. a field with no spec at all has
    /// the empty spec, which is the one every type accepts
    pub(crate) fn of(element: &'ast ast::InterpolatedElement) -> Self {
        let Some(spec) = &element.format_spec else {
            // an empty range just past the expression, so a report about the
            // absent spec still points somewhere sensible
            let at = element.expression.range().end();
            return Self {
                literal: Some(""),
                range: TextRange::empty(at),
            };
        };
        // adjacent literal runs are merged, so a spec that is entirely literal
        // is exactly one element — anything else holds a replacement field
        let literal = match &*spec.elements {
            [] => Some(""),
            [ast::InterpolatedStringElement::Literal(only)] => Some(&*only.value),
            _ => None,
        };
        Self {
            literal,
            range: spec.range(),
        }
    }

    /// the type the spec argument has at the `__format__` call
    fn argument_type<'db>(&self, db: &'db dyn Db) -> Type<'db> {
        match self.literal {
            Some(literal) => Type::string_literal(db, literal),
            None => KnownClass::Str.to_instance(db),
        }
    }

    /// the range of `span`, which is relative to the start of the spec text
    fn span(&self, span: &std::ops::Range<usize>) -> TextRange {
        let start = self.range.start() + TextSize::try_from(span.start).unwrap_or_default();
        let end = self.range.start() + TextSize::try_from(span.end).unwrap_or_default();
        TextRange::new(start, end)
    }
}

/// check one replacement field of an f-string
///
/// `value_ty` is the type of the expression before the conversion is applied
pub(crate) fn check_interpolation<'db>(
    context: &InferContext<'db, '_>,
    element: &ast::InterpolatedElement,
    value_ty: Type<'db>,
) {
    let db = context.db();
    // a use-site modifier says nothing about how the value renders: `A()` is
    // inferred as `final A`, and it is `A` that has or lacks a `__format__`
    let value_ty = value_ty.erase_restriction(db);
    let formatted = converted(db, value_ty, element.conversion);
    let spec = WrittenSpec::of(element);

    // the empty spec is the one every type accepts by construction:
    // `object.__format__` takes it, and an override can only widen from there.
    // a call that fails on `""` is this checker failing to resolve it — a
    // typevar, a union it cannot see through — not the program being wrong. an
    // override that really does refuse `""` is reported as a bad override
    if spec.literal != Some("")
        && formatted
            .try_call_dunder(
                db,
                "__format__",
                CallArguments::positional([spec.argument_type(db)]),
                TypeContext::default(),
            )
            .is_err()
    {
        report_rejected_spec(context, &spec, formatted);
        return;
    }

    check_spec_content(context, &spec, formatted);
}

/// the type that reaches `__format__`, once the conversion has run
///
/// every conversion produces a `str`, so the spec is read by `str.__format__`
/// rather than by the value's own
fn converted<'db>(
    db: &'db dyn Db,
    value_ty: Type<'db>,
    conversion: ast::ConversionFlag,
) -> Type<'db> {
    match conversion {
        ast::ConversionFlag::None => value_ty,
        ast::ConversionFlag::Str => value_ty.str(db),
        ast::ConversionFlag::Repr => value_ty.repr(db),
        // `ascii` is `repr` with the non-ascii escaped, which cannot be read
        // off the type
        ast::ConversionFlag::Ascii => KnownClass::Str.to_instance(db),
    }
}

/// the spec is not one this type's `__format__` accepts at all
fn report_rejected_spec<'db>(
    context: &InferContext<'db, '_>,
    spec: &WrittenSpec<'_>,
    formatted: Type<'db>,
) {
    let db = context.db();
    // the rejection may be nothing but a stub's silence, which settles nothing
    if inherits_object_format(db, formatted) && !declares_what_it_implements(db, formatted) {
        return;
    }
    let Some(builder) = context.report_lint(&INVALID_FORMAT_SPEC, spec.range) else {
        return;
    };
    let written = spec.literal.unwrap_or_default();
    let mut diagnostic = builder.into_diagnostic(format_args!(
        "`{written}` is not a valid format spec for `{}`",
        formatted.display(db)
    ));
    // the overwhelmingly common cause is a class that never opted in
    if inherits_object_format(db, formatted) {
        diagnostic.info(format_args!(
            "`{}` inherits `object.__format__`, which accepts only the empty spec",
            formatted.display(db)
        ));
        diagnostic.help("define `__format__` to give the class a format spec of its own");
    }
}

/// whether a class declaring no `__format__` can be read as having none
///
/// the vendored typeshed is ours — patched, and covered by the tests that run
/// over it — so a stdlib class that takes a spec declares `__format__` there.
/// every other stub was written against an `object.__format__` that accepted
/// any `str`, which gave nobody a reason to declare one: numpy's `generic`
/// implements the whole mini-language and its stub never mentions `__format__`.
/// silence outside the vendored typeshed is therefore not a rejection
///
fn declares_what_it_implements<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    let Some(class) = ty.nominal_class(db) else {
        return false;
    };
    class
        .iter_mro(db)
        .filter_map(ClassBase::into_class)
        .take_while(|base| base.class_literal(db).known(db) != Some(KnownClass::Object))
        .all(|base| {
            let file = base.class_literal(db).file(db);
            !file.is_stub(db) || file.path(db).is_vendored_path()
        })
}

/// whether the `__format__` that would be called is `object`'s own
fn inherits_object_format<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    ty.nominal_class(db)
        .and_then(|class| owner_of(db, class, "__format__"))
        .is_some_and(|owner| owner.class_literal(db).known(db) == Some(KnownClass::Object))
}

/// check the spec's content against the rules of the implementation reading it
fn check_spec_content<'db>(
    context: &InferContext<'db, '_>,
    spec: &WrittenSpec<'_>,
    formatted: Type<'db>,
) {
    let db = context.db();
    let (Some(written), Some(language)) = (spec.literal, spec_language(db, formatted)) else {
        return;
    };
    let target = match language {
        SpecLanguage::MiniLanguage(target) => target,
        SpecLanguage::Strftime => return check_strftime(context, spec, written),
    };
    let (parsed, spans) = match FormatSpec::parse_spanned(written) {
        Ok(parsed) => parsed,
        Err(error) => {
            report_malformed_spec(context, spec, &error);
            return;
        }
    };
    // a spec built at runtime says nothing statically
    let FormatSpec::Static(parsed) = parsed else {
        return;
    };
    let Err(violation) = parsed.validate(target) else {
        return;
    };
    let at = spans
        .component(violation.component())
        .map_or(spec.range, |span| spec.span(span));
    let Some(builder) = context.report_lint(&INVALID_FORMAT_SPEC, at) else {
        return;
    };
    let mut diagnostic = builder.into_diagnostic(violation.describe(target));
    // when the presentation type is the problem, saying what would work is
    // more use than saying again what did not
    if matches!(violation, FormatSpecViolation::UnknownType(_)) {
        let accepted = target
            .presentation_types()
            .iter()
            .map(|(presentation, ..)| format!("`{presentation}`"))
            .collect::<Vec<_>>()
            .join(", ");
        diagnostic.info(format_args!("`{}` accepts {accepted}", target.type_name()));
    }
}

/// report the strftime directives no platform renders
///
/// an unrecognised directive does not raise — `strftime` writes it through and
/// the output is quietly wrong — so this is the only thing that catches it. a
/// directive that merely is not portable is left alone; plenty of code uses
/// `%-d` deliberately
fn check_strftime(context: &InferContext<'_, '_>, spec: &WrittenSpec<'_>, written: &str) {
    for directive in strftime::directives(written) {
        let message = match directive.kind {
            DirectiveKind::Unknown => format!(
                "`{}` is not a directive `strftime` renders",
                &written[directive.span.clone()]
            ),
            DirectiveKind::Dangling => "a `%` with no directive after it".to_string(),
            DirectiveKind::Portable | DirectiveKind::Platform | DirectiveKind::Escape => continue,
        };
        let Some(builder) = context.report_lint(&INVALID_FORMAT_SPEC, spec.span(&directive.span))
        else {
            continue;
        };
        let mut diagnostic = builder.into_diagnostic(message);
        diagnostic.info("it is written through as-is, so the output holds the text itself");
        if directive.kind == DirectiveKind::Dangling {
            diagnostic.help("write `%%` for a literal `%`");
        }
    }
}

/// the spec is not even a well-formed one
fn report_malformed_spec(
    context: &InferContext<'_, '_>,
    spec: &WrittenSpec<'_>,
    error: &FormatSpecError,
) {
    let Some(builder) = context.report_lint(&INVALID_FORMAT_SPEC, spec.range) else {
        return;
    };
    let written = spec.literal.unwrap_or_default();
    let mut diagnostic =
        builder.into_diagnostic(format_args!("`{written}` is not a valid format spec"));
    if let Some(detail) = malformed_detail(error) {
        diagnostic.info(detail);
    }
}

fn malformed_detail(error: &FormatSpecError) -> Option<String> {
    match error {
        FormatSpecError::InvalidFormatType(found) => {
            Some(format!("`{found}` is not a presentation type"))
        }
        FormatSpecError::DecimalDigitsTooMany => Some("the width has too many digits".to_string()),
        FormatSpecError::PrecisionTooBig => Some("the precision is too large".to_string()),
        FormatSpecError::PlaceholderRecursionExceeded | FormatSpecError::InvalidPlaceholder(_) => {
            Some("a replacement field inside a spec cannot hold another one".to_string())
        }
        _ => None,
    }
}
