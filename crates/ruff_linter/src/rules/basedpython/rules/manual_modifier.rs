use ruff_diagnostics::Applicability;
use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{Decorator, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::checkers::ast::Checker;
use crate::{AlwaysFixableViolation, Edit, Fix};

/// ## What it does
/// Checks for a decorator in `.by` source that basedpython writes as a modifier
/// keyword on the declaration itself.
///
/// ## Why is this bad?
/// The decorators that say what kind of declaration this is — `@staticmethod`,
/// `@final`, `@dataclass` — are read as part of the header, so basedpython puts
/// them there. One line says what two did, and the header no longer starts on
/// the line below the thing that qualifies it.
///
/// ## Example
/// ```by
/// @dataclass(slots=True)
/// class Point:
///     x: int
///
///     @staticmethod
///     def origin() -> "Point":
///         return Point(0)
/// ```
///
/// Use instead:
/// ```by
/// data class Point:
///     x: int
///
///     static def origin() -> "Point":
///         return Point(0)
/// ```
///
/// The decorators with a keyword spelling are `@staticmethod`, `@classmethod`,
/// `@abstractmethod`, `@override` and `@final` on a `def`, and `@final` and
/// `@dataclass` on a `class`. A `@dataclass` is only one when its arguments are
/// what `data class` emits: `slots=True`, and `frozen=True` beside it for
/// `frozen data class`.
///
/// ## Fix safety
/// This rule's fix is marked as unsafe when a comment sits between the decorator
/// and the header, which the rewrite would drop along with the line it is on.
///
/// Only the last decorator is rewritten, since a modifier keyword belongs to the
/// header and cannot be written above one. Running the fix again takes the next
/// one, so a stack of them unwinds an entry at a time.
///
/// ## References
/// - [basedpython documentation: modifiers](https://docs.basedpython.org/features/modifiers)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "0.0.1-a10")]
pub(crate) struct ManualModifier {
    decorator: String,
    modifier: &'static str,
}

impl AlwaysFixableViolation for ManualModifier {
    #[derive_message_formats]
    fn message(&self) -> String {
        let ManualModifier {
            decorator,
            modifier,
        } = self;
        format!("`@{decorator}` can be written as `{modifier}`")
    }

    fn fix_title(&self) -> String {
        let ManualModifier { modifier, .. } = self;
        format!("Replace with the `{modifier}` modifier")
    }
}

/// BY022
pub(crate) fn manual_modifier(checker: &Checker, stmt: &Stmt) {
    if !checker.source_type.is_basedpython() {
        return;
    }

    let (decorators, header) = match stmt {
        Stmt::FunctionDef(function) => (&function.decorator_list, Header::Function),
        Stmt::ClassDef(class) => {
            // a marker for something other than a modifier stands for a whole
            // surface construct, and a keyword does not go in front of one
            if class.is_extension() || class.is_based_enum() || class.is_enum_variant() {
                return;
            }
            (&class.decorator_list, Header::Class)
        }
        _ => return,
    };

    // the modifiers already written as keywords are synthetic decorators the
    // parser appends after the real ones, so the last real decorator is the one
    // a keyword can be written in front of
    let Some(index) = decorators
        .iter()
        .rposition(|decorator| !is_synthetic(decorator))
    else {
        return;
    };
    let decorator = &decorators[index];
    let Some(modifier) = modifier_for(checker, decorator, header) else {
        return;
    };

    // everything from the `@` up to whatever comes next: the following synthetic
    // decorator's modifier text, or the `def` / `class` keyword itself
    let Some(next) = decorators
        .get(index + 1)
        .map(Ranged::start)
        .or_else(|| header_start(checker, decorator.end()))
    else {
        return;
    };
    let replaced = TextRange::new(decorator.start(), next);

    let applicability = if checker.comment_ranges().intersects(replaced) {
        Applicability::Unsafe
    } else {
        Applicability::Safe
    };

    checker
        .report_diagnostic(
            ManualModifier {
                decorator: checker
                    .locator()
                    .slice(decorator.expression.range())
                    .to_string(),
                modifier,
            },
            decorator.range(),
        )
        .set_fix(Fix::applicable_edit(
            Edit::range_replacement(format!("{modifier} "), replaced),
            applicability,
        ));
}

/// What the decorator is attached to, which decides how it reads.
#[derive(Clone, Copy)]
enum Header {
    Function,
    Class,
}

/// The modifier keyword `decorator` is spelled with, if it has one.
fn modifier_for(checker: &Checker, decorator: &Decorator, header: Header) -> Option<&'static str> {
    let semantic = checker.semantic();
    let expression = &decorator.expression;
    match header {
        Header::Function => {
            if semantic.match_builtin_expr(expression, "staticmethod") {
                return Some("static");
            }
            if semantic.match_builtin_expr(expression, "classmethod") {
                return Some("class");
            }
            if semantic.match_typing_expr(expression, "override") {
                return Some("override");
            }
            if semantic.match_typing_expr(expression, "final") {
                return Some("final");
            }
            semantic
                .resolve_qualified_name(expression)
                .filter(|qualified| qualified.segments() == ["abc", "abstractmethod"])
                .map(|_| "abstract")
        }
        Header::Class => {
            if semantic.match_typing_expr(expression, "final") {
                return Some("final");
            }
            dataclass_modifier(checker, expression)
        }
    }
}

/// The modifier a `@dataclass` is spelled with, when its arguments are the ones
/// `data class` writes. A `@dataclass` configured any other way says something
/// the keyword does not, so it keeps its decorator.
fn dataclass_modifier(checker: &Checker, expression: &Expr) -> Option<&'static str> {
    let Expr::Call(call) = expression else {
        return None;
    };
    if checker
        .semantic()
        .resolve_qualified_name(&call.func)
        .is_none_or(|qualified| qualified.segments() != ["dataclasses", "dataclass"])
    {
        return None;
    }
    if !call.arguments.args.is_empty() {
        return None;
    }
    let keywords = &*call.arguments.keywords;
    let is_true = |name: &str| {
        keywords.iter().any(|keyword| {
            keyword.arg.as_ref().is_some_and(|arg| arg == name)
                && matches!(&keyword.value, Expr::BooleanLiteral(value) if value.value)
        })
    };
    match (keywords.len(), is_true("slots"), is_true("frozen")) {
        (1, true, false) => Some("data"),
        (2, true, true) => Some("frozen data"),
        _ => None,
    }
}

/// Whether `decorator` is one the parser synthesized for a modifier already
/// written as a keyword, rather than one the source spells with an `@`.
fn is_synthetic(decorator: &Decorator) -> bool {
    matches!(&decorator.expression, Expr::Name(name) if name.is_invalid())
}

/// The offset of the `def` / `class` keyword the decorator list runs into.
fn header_start(checker: &Checker, after: TextSize) -> Option<TextSize> {
    checker
        .tokens()
        .after(after)
        .iter()
        .find(|token| {
            matches!(
                token.kind(),
                TokenKind::Def | TokenKind::Class | TokenKind::Async
            )
        })
        .map(Ranged::start)
}
