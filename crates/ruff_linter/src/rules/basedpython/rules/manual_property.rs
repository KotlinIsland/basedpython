use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_trivia::{indentation_at_offset, textwrap};
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextRange};

use crate::codes::Category;
use crate::checkers::ast::Checker;
use crate::{Edit, Fix, FixAvailability, Violation};

/// ## What it does
/// Checks for a `@property` in `.by` source, which basedpython declares with a
/// `let` or `var` and an accessor block.
///
/// ## Why is this bad?
/// A python property spells one member as two `def`s, each repeating the name,
/// and a reader has to collect them to see what the member is. basedpython gives
/// it one declaration site: the name, its type, and the accessors that back it,
/// under a single header.
///
/// ## Example
/// ```by
/// class Rect:
///     @property
///     def area(self) -> int:
///         return self.w * self.h
/// ```
///
/// Use instead:
/// ```by
/// class Rect:
///     let area: int
///         get() = self.w * self.h
/// ```
///
/// A property with a setter becomes a `var`, and the setter its `set` accessor:
///
/// ```by
/// class Person:
///     var age: int
///         get() = self._age
///         set(value):
///             assert value >= 0
///             self._age = value
/// ```
///
/// ## Fix safety
/// The fix is marked as unsafe when the setter's parameter is unannotated and
/// the getter's return type is not. A `set` accessor takes its parameter type
/// from the declaration, so the rewrite gives the parameter a type it did not
/// have, and a setter that accepted more than the getter returns would narrow.
///
/// No fix is offered when a comment falls inside the rewrite, since an accessor
/// body is re-rendered when it is lowered and does not keep one, nor when a
/// single token spans lines — a triple-quoted string — whose own indentation the
/// re-indent must not touch.
///
/// Only the plain `@property` shape is reported. A property carrying any further
/// decorator, one whose receiver is not named `self`, one with a `deleter`, and
/// one whose setter takes a different type than the getter returns are all left
/// alone: an accessor block has nowhere to put them.
///
/// ## References
/// - [basedpython documentation: properties](https://docs.basedpython.org/features/properties)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10", category = Category::Style)]
pub(crate) struct ManualProperty {
    name: String,
    keyword: &'static str,
}

impl Violation for ManualProperty {
    const FIX_AVAILABILITY: FixAvailability = FixAvailability::Sometimes;

    #[derive_message_formats]
    fn message(&self) -> String {
        let ManualProperty { name, keyword } = self;
        format!("`@property` can be written as `{keyword} {name}` with accessors")
    }

    fn fix_title(&self) -> Option<String> {
        let ManualProperty { name, keyword } = self;
        Some(format!("Replace with a `{keyword} {name}` declaration"))
    }
}

/// BY021
pub(crate) fn manual_property(checker: &Checker, class_def: &ast::StmtClassDef) {
    if !checker.source_type.is_basedpython() || checker.source_type.is_stub() {
        return;
    }
    // an `extension T:` member is lowered by its own member kind rather than by
    // shape, and an enum body reads its members to decide the enum's lowering.
    // in both a declaration means something other than what it means in a class
    if class_def.is_extension() || class_def.is_based_enum() || class_def.is_enum_variant() {
        return;
    }

    let body = &*class_def.body;
    for (index, stmt) in body.iter().enumerate() {
        let Some(getter) = stmt.as_function_def_stmt() else {
            continue;
        };
        if !is_plain_property(checker, getter) {
            continue;
        }

        // the setter has to be the very next member: absorbing one from further
        // down would reorder the class, and leaving it behind would strand a
        // `@name.setter` whose `name` is no longer a function
        let setter = body
            .get(index + 1)
            .and_then(Stmt::as_function_def_stmt)
            .filter(|setter| is_setter_of(setter, getter.name.as_str()));

        // any further member under the name — a second setter, a `deleter`, an
        // explicit `getter`, an overload — has no accessor-block spelling
        let members = body
            .iter()
            .filter_map(Stmt::as_function_def_stmt)
            .filter(|member| member.name.as_str() == getter.name.as_str())
            .count();
        if members != 1 + usize::from(setter.is_some()) {
            continue;
        }

        if setter.is_some_and(|setter| !accessor_types_agree(checker, getter, setter)) {
            continue;
        }
        // `var x` on its own is a parse error, where a read-only `let x` is not:
        // a mutable declaration has to say what it holds
        if setter.is_some() && getter.returns.is_none() {
            continue;
        }

        let keyword = if setter.is_some() { "var" } else { "let" };
        let mut diagnostic = checker.report_diagnostic(
            ManualProperty {
                name: getter.name.to_string(),
                keyword,
            },
            TextRange::new(getter.start(), setter.unwrap_or(getter).end()),
        );
        if let Some(fix) = property_fix(checker, keyword, getter, setter) {
            diagnostic.set_fix(fix);
        }
    }
}

/// Whether `function` is a `@property` getter an accessor block can express: no
/// further decorator, an ordinary `self` receiver, and a body to move.
fn is_plain_property(checker: &Checker, function: &ast::StmtFunctionDef) -> bool {
    let [decorator] = &*function.decorator_list else {
        return false;
    };
    checker
        .semantic()
        .match_builtin_expr(&decorator.expression, "property")
        && receiver_only(&function.parameters)
        && has_movable_body(function)
}

/// Whether `function` is the `@<name>.setter` companion of the property `name`.
fn is_setter_of(function: &ast::StmtFunctionDef, name: &str) -> bool {
    let [decorator] = &*function.decorator_list else {
        return false;
    };
    let Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = &decorator.expression else {
        return false;
    };
    let Expr::Name(property) = value.as_ref() else {
        return false;
    };
    if attr.as_str() != "setter" || property.id != name || function.name.as_str() != name {
        return false;
    }
    // `-> None` is what the accessor lowers to, and so is leaving it off
    if function
        .returns
        .as_deref()
        .is_some_and(|returns| !matches!(returns, Expr::NoneLiteral(_)))
    {
        return false;
    }
    let parameters = &function.parameters;
    let [receiver, value] = &*parameters.args else {
        return false;
    };
    no_extra_parameters(parameters)
        && is_plain_receiver(receiver)
        && value.default.is_none()
        && value.parameter.pattern.is_none()
        && !value.parameter.is_context
        && !value.parameter.is_some
        && has_movable_body(function)
}

/// Whether the declaration can carry the setter's parameter type, which a `set`
/// accessor takes from the declaration rather than writing out itself.
fn accessor_types_agree(
    checker: &Checker,
    getter: &ast::StmtFunctionDef,
    setter: &ast::StmtFunctionDef,
) -> bool {
    let declared = getter.returns.as_deref();
    let value = setter
        .parameters
        .args
        .get(1)
        .and_then(ast::ParameterWithDefault::annotation);
    match (declared, value) {
        // the parameter takes the declaration's type; whether that is what the
        // author meant is what makes the fix unsafe rather than safe
        (_, None) => true,
        (Some(declared), Some(value)) => {
            checker.locator().slice(declared) == checker.locator().slice(value)
        }
        // no declared type for the parameter to take
        (None, Some(_)) => false,
    }
}

/// Whether `parameters` is just a receiver, as a getter's are.
fn receiver_only(parameters: &ast::Parameters) -> bool {
    let [receiver] = &*parameters.args else {
        return false;
    };
    no_extra_parameters(parameters) && is_plain_receiver(receiver)
}

/// Whether `parameters` holds nothing outside its plain positional-or-keyword
/// list, which is all an accessor block writes.
fn no_extra_parameters(parameters: &ast::Parameters) -> bool {
    parameters.posonlyargs.is_empty()
        && parameters.kwonlyargs.is_empty()
        && parameters.vararg.is_none()
        && parameters.kwarg.is_none()
}

/// Whether `parameter` is the plain `self` an accessor block writes for you. A
/// receiver under any other name is left alone: the body names it, and the
/// accessor's own receiver is always `self`.
fn is_plain_receiver(parameter: &ast::ParameterWithDefault) -> bool {
    parameter.name().as_str() == "self"
        && parameter.annotation().is_none()
        && parameter.default.is_none()
        && parameter.parameter.pattern.is_none()
        && !parameter.parameter.is_context
        && !parameter.parameter.is_some
}

/// Whether the function's body is one an accessor block can hold, under a header
/// carrying nothing an accessor cannot say.
fn has_movable_body(function: &ast::StmtFunctionDef) -> bool {
    if function.is_async
        || function.is_trailing_lambda
        || function.is_asserts_return
        || function.type_params.is_some()
        || function.raises.is_some()
    {
        return false;
    }
    match &*function.body {
        [] => false,
        // `...` is a stub rather than a body, and `get() = ...` would return it
        [Stmt::Expr(expr)] => !matches!(expr.value.as_ref(), Expr::EllipsisLiteral(_)),
        _ => true,
    }
}

/// The replacement for the whole property, or `None` when it cannot be written
/// without disturbing something the rewrite does not own.
fn property_fix(
    checker: &Checker,
    keyword: &str,
    getter: &ast::StmtFunctionDef,
    setter: Option<&ast::StmtFunctionDef>,
) -> Option<Fix> {
    let replaced = TextRange::new(getter.start(), setter.unwrap_or(getter).end());

    // an accessor body is re-rendered from the AST when it is lowered, so a
    // comment moved into one would not survive the transpile
    if checker.comment_ranges().intersects(replaced) {
        return None;
    }

    let indent = indentation_at_offset(getter.start(), checker.source())?;
    let one = checker.stylist().indentation().as_str();
    let line_ending = checker.stylist().line_ending().as_str();

    let declared = getter
        .returns
        .as_deref()
        .map(|returns| format!(": {}", checker.locator().slice(returns)))
        .unwrap_or_default();

    let mut replacement = format!("{keyword} {}{declared}", getter.name);
    replacement.push_str(line_ending);
    replacement.push_str(indent);
    replacement.push_str(one);
    replacement.push_str(&accessor(checker, "get()", getter, one)?);

    if let Some(setter) = setter {
        let value = setter.parameters.args.get(1)?;
        replacement.push_str(line_ending);
        replacement.push_str(indent);
        replacement.push_str(one);
        let header = format!("set({})", value.name());
        replacement.push_str(&accessor(checker, &header, setter, one)?);
    }

    // a `set` accessor's parameter is typed by the declaration, so a setter that
    // had no annotation gains one
    let applicability = if setter.is_some_and(|setter| {
        getter.returns.is_some()
            && setter
                .parameters
                .args
                .get(1)
                .is_some_and(|value| value.annotation().is_none())
    }) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    Some(Fix::applicable_edit(
        Edit::range_replacement(replacement, replaced),
        applicability,
    ))
}

/// One accessor written under `header`: `header = <expr>` for a lone `return`,
/// and `header:` over the body, re-indented by `one` level to sit under it.
fn accessor(
    checker: &Checker,
    header: &str,
    function: &ast::StmtFunctionDef,
    one: &str,
) -> Option<String> {
    let locator = checker.locator();

    if let [
        Stmt::Return(ast::StmtReturn {
            value: Some(value), ..
        }),
    ] = &*function.body
    {
        return Some(format!("{header} = {}", locator.slice(value.as_ref())));
    }

    let first = function.body.first()?;
    let last = function.body.last()?;
    // a one-line body (`def x(self): a = 1; return a`) shares its line with the
    // header, and so has no indentation of its own for the re-indent to build on
    let body = TextRange::new(locator.line_start(first.start()), last.end());
    if !locator
        .slice(TextRange::new(body.start(), first.start()))
        .trim()
        .is_empty()
    {
        return None;
    }

    // the re-indent prefixes every line, which would rewrite the contents of a
    // string that spans them
    if checker.tokens().in_range(body).iter().any(|token| {
        matches!(
            token.kind(),
            TokenKind::String | TokenKind::FStringMiddle | TokenKind::TStringMiddle
        ) && locator.contains_line_break(token.range())
    }) {
        return None;
    }

    let indented = textwrap::indent(locator.slice(body), one);
    let line_ending = checker.stylist().line_ending().as_str();
    Some(format!("{header}:{line_ending}{indented}"))
}
