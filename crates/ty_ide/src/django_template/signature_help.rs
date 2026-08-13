//! signature help for django templates
//!
//! a filter argument is the one thing a template writes that nothing on the page
//! explains. `{{ value|date:"Y-m-d" }}` says which filter runs, and every other
//! service in this module can say what `value` is, but what may follow the colon
//! is written nowhere the reader can see.
//!
//! django calls a filter with the value it was applied to and, where the template
//! wrote one, the argument — so the argument *is* the registered function's second
//! parameter. wherever that function can be read, which is the project's own
//! filters and django's own too whenever django itself is readable, the answer is
//! a real python parameter. where it cannot be read, django's description of the
//! filter is still more than the template says on its own.

use ruff_db::parsed::parsed_module;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::{self as ast, AnyNodeRef, Expr};
use ruff_text_size::TextSize;
use ty_project::Db;
use ty_python_semantic::SemanticModel;
use ty_python_semantic::types::ide_support::hintable_parameter_type;

use super::builtins;
use super::index::TemplateIndex;
use super::lexer::{ConstructKind, Token, TokenKind};
use super::project::{self, Registration, RegistrationKind};
use ty_python_semantic::ProgramEnvironment;

/// what django names the flag it fills in itself, on the decorator and on the
/// parameter it fills
const AUTOESCAPE_FLAG: &str = "needs_autoescape";
const AUTOESCAPE: &str = "autoescape";

/// what the filter argument under the cursor takes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSignature {
    /// the filter and its argument, written the way the template writes them
    pub label: String,
    /// the argument's own text within [`Self::label`], where the filter's
    /// function could be read and does take one
    pub parameter: Option<String>,
    /// what the filter is documented to do
    pub documentation: Option<String>,
}

/// what the filter whose argument `offset` sits in takes
pub(crate) fn signature_help(
    db: &dyn Db,
    env: &ProgramEnvironment<'_>,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
) -> Option<TemplateSignature> {
    let construct = index.lexed().construct_at(offset)?;
    if construct.kind == ConstructKind::Comment {
        return None;
    }

    let name = filter_argument_at(source, index.lexed().construct_tokens(construct), offset)?;

    signature(db, env, name)
}

/// the filter whose argument `offset` sits in
///
/// an argument runs from the `:` after the filter's name to whatever ends it: the
/// `|` starting the next filter, or the construct's closing delimiter. the cursor
/// counts as inside it anywhere in there, the empty position directly after the
/// colon included — which is where it sits the moment the colon is typed.
fn filter_argument_at<'src>(
    source: &'src str,
    tokens: &[Token],
    offset: TextSize,
) -> Option<&'src str> {
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::FilterName {
            continue;
        }

        let Some(colon) = tokens
            .get(index + 1)
            .filter(|token| is_operator(source, token, ":"))
        else {
            continue;
        };

        let end = tokens
            .get(index + 2..)
            .unwrap_or_default()
            .iter()
            .take_while(|token| {
                token.kind != TokenKind::Delimiter && !is_operator(source, token, "|")
            })
            .last()
            .map_or(colon.range.end(), |token| token.range.end());

        if (colon.range.end()..=end).contains(&offset) {
            return Some(&source[token.range]);
        }
    }

    None
}

fn is_operator(source: &str, token: &Token, operator: &str) -> bool {
    token.kind == TokenKind::Operator && &source[token.range] == operator
}

/// what the filter `name` takes
fn signature(db: &dyn Db, env: &ProgramEnvironment<'_>, name: &str) -> Option<TemplateSignature> {
    let registered = filter_registration(db, name);

    // as for hover: the table documents django's own filters, but which of them
    // this django has is that django's to say. a project's filter is documented
    // by its docstring and by nothing else
    let documentation = builtins::filter(name)
        .filter(|_| builtins::provided_by_django(db, name, true).is_some())
        .map(|filter| filter.documentation.to_string())
        .or_else(|| {
            registered
                .and_then(|registration| registration.documentation.as_deref())
                .map(str::to_string)
        });

    let Some(registration) = registered else {
        // a django this project cannot read still has a documented filter set,
        // and saying what the filter does is more than the template says
        return Some(TemplateSignature {
            label: format!("|{name}"),
            parameter: None,
            documentation: Some(documentation?),
        });
    };

    // a function taking only the value takes no argument, and a filter that takes
    // no argument has nothing to say about the one that was written
    let parameter = argument_parameter(db, env, registration)?;

    Some(TemplateSignature {
        label: format!("|{name}:{parameter}"),
        parameter: Some(parameter),
        documentation,
    })
}

/// the registration of the filter `name`, django's own or the project's
fn filter_registration<'db>(db: &'db dyn Db, name: &str) -> Option<&'db Registration> {
    project::registrations(db, db.project())
        .iter()
        .find(|registration| {
            registration.kind == RegistrationKind::Filter && registration.name == name
        })
}

/// the registered function's argument parameter, as the signature writes it
///
/// this is its *second* parameter: django passes the value being filtered first.
/// the type is written down only where the parameter is *annotated*. django's own
/// filters annotate almost none of theirs, and what an unannotated parameter
/// infers to says nothing the reader wants: `def date(value, arg=None)` would put
/// `arg: Unknown | None` in front of somebody looking for a format string.
fn argument_parameter(
    db: &dyn Db,
    env: &ProgramEnvironment<'_>,
    registration: &Registration,
) -> Option<String> {
    let parsed = parsed_module(db, db.program_file(registration.file).python_file(db)).load(db);
    let covering = covering_node(parsed.syntax().into(), registration.range)
        .find_first(|node| node.is_stmt_function_def())
        .ok()?;
    let AnyNodeRef::StmtFunctionDef(function) = covering.node() else {
        return None;
    };

    let injected = needs_autoescape(function).then_some(AUTOESCAPE);
    let parameter = &function
        .parameters
        .posonlyargs
        .iter()
        .chain(&function.parameters.args)
        .filter(|parameter| Some(parameter.parameter.name.as_str()) != injected)
        .nth(1)?
        .parameter;
    let name = parameter.name.as_str();

    let annotated = parameter.annotation.is_some();
    let model = SemanticModel::new(db, db.program_file(registration.file));

    match hintable_parameter_type(&model, parameter).filter(|_| annotated) {
        Some(ty) => Some(format!("{name}: {}", ty.display(db, env))),
        None => Some(name.to_string()),
    }
}

/// whether django fills a parameter of this filter in itself
///
/// `@register.filter(needs_autoescape=True)` makes django call the function with
/// an extra `autoescape=` keyword of its own, so the parameter taking it is
/// django's rather than the argument the template writes. django's own
/// `{{ x|urlize }}` is a filter that takes no argument and has two parameters
/// because of it.
fn needs_autoescape(function: &ast::StmtFunctionDef) -> bool {
    function.decorator_list.iter().any(|decorator| {
        let Expr::Call(call) = &decorator.expression else {
            return false;
        };

        call.arguments.find_keyword(AUTOESCAPE_FLAG).is_some_and(
            |keyword| matches!(&keyword.value, Expr::BooleanLiteral(literal) if literal.value),
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::{DJANGO_BUILTINS, TemplateTest};

    /// the same small django project the hover and completion tests use, with a
    /// filter of the project's own that takes an argument and one that does not
    fn project(template: &str) -> TemplateTest {
        TemplateTest::new(&[
            (
                "blog/models.py",
                "
                class Book:
                    title: str
                ",
            ),
            (
                "blog/views.py",
                "
                from blog.models import Book

                def post(request):
                    return render(request, 'blog/post.html', {'book': Book()})
                ",
            ),
            (
                "blog/templatetags/blog_extras.py",
                "
                from django import template

                register = template.Library()

                @register.filter
                def shout(value, suffix: str):
                    'shouts it.'
                    return value

                @register.filter
                def quieten(value):
                    'quietens it.'
                    return value
                ",
            ),
            ("blog/templates/blog/post.html", template),
        ])
    }

    #[test]
    fn a_project_filters_argument_is_its_functions_second_parameter() {
        assert_eq!(
            project(r#"{{ book.title|shout:"<CURSOR>" }}"#).signature(),
            "|shout:suffix: str [suffix: str] — shouts it."
        );
    }

    #[test]
    fn an_unannotated_argument_is_offered_by_name_alone() {
        let test = TemplateTest::new(&[
            (
                "blog/templatetags/blog_extras.py",
                "
                from django import template

                register = template.Library()

                @register.filter
                def shout(value, suffix):
                    'shouts it.'
                    return value
                ",
            ),
            (
                "blog/templates/blog/post.html",
                r#"{{ title|shout:"<CURSOR>" }}"#,
            ),
        ]);

        assert_eq!(test.signature(), "|shout:suffix [suffix] — shouts it.");
    }

    #[test]
    fn an_unannotated_argument_with_a_default_is_not_typed_from_it() {
        // this is how django writes almost all of its own: `arg: Unknown | None`
        // says nothing to somebody looking for what may follow the colon
        let test = TemplateTest::new(&[
            (
                "blog/templatetags/blog_extras.py",
                "
                from django import template

                register = template.Library()

                @register.filter
                def shout(value, suffix=None):
                    'shouts it.'
                    return value
                ",
            ),
            (
                "blog/templates/blog/post.html",
                r#"{{ title|shout:"<CURSOR>" }}"#,
            ),
        ]);

        assert_eq!(test.signature(), "|shout:suffix [suffix] — shouts it.");
    }

    #[test]
    fn the_parameter_django_fills_itself_is_not_the_templates_argument() {
        // `{{ x|urlize }}` is exactly this: two parameters, no argument
        let test = TemplateTest::new(&[
            (
                "blog/templatetags/blog_extras.py",
                "
                from django import template

                register = template.Library()

                @register.filter(is_safe=True, needs_autoescape=True)
                def shout(value, autoescape=True):
                    'shouts it.'
                    return value
                ",
            ),
            (
                "blog/templates/blog/post.html",
                r#"{{ title|shout:"<CURSOR>" }}"#,
            ),
        ]);

        assert_eq!(test.signature(), "no signature");
    }

    #[test]
    fn a_filter_django_fills_a_parameter_of_can_still_take_an_argument() {
        let test = TemplateTest::new(&[
            (
                "blog/templatetags/blog_extras.py",
                "
                from django import template

                register = template.Library()

                @register.filter(needs_autoescape=True)
                def pad(value, width: int, autoescape=True):
                    'pads it.'
                    return value
                ",
            ),
            (
                "blog/templates/blog/post.html",
                r#"{{ title|pad:"<CURSOR>" }}"#,
            ),
        ]);

        assert_eq!(test.signature(), "|pad:width: int [width: int] — pads it.");
    }

    #[test]
    fn a_filter_that_takes_no_argument_offers_nothing() {
        assert_eq!(
            project(r#"{{ book.title|quieten:"<CURSOR>" }}"#).signature(),
            "no signature"
        );
    }

    #[test]
    fn a_cursor_that_is_not_in_an_argument_offers_nothing() {
        assert_eq!(
            project("{{ book.ti<CURSOR>tle|shout:'!' }}").signature(),
            "no signature"
        );
        assert_eq!(
            project("{{ book.title|sho<CURSOR>ut:'!' }}").signature(),
            "no signature"
        );
        assert_eq!(
            project("{{ book.ti<CURSOR>tle }}").signature(),
            "no signature"
        );
    }

    #[test]
    fn a_cursor_directly_after_the_colon_is_in_the_argument() {
        assert_eq!(
            project("{{ book.title|shout:<CURSOR> }}").signature(),
            "|shout:suffix: str [suffix: str] — shouts it."
        );
    }

    #[test]
    fn only_the_filter_the_cursor_is_in_answers() {
        assert_eq!(
            project(r#"{{ book.title|quieten|shout:"<CURSOR>" }}"#).signature(),
            "|shout:suffix: str [suffix: str] — shouts it."
        );
    }

    #[test]
    fn a_django_filter_reads_its_argument_from_the_installed_django() {
        let test = TemplateTest::with_site_packages(
            &[
                (
                    "manage.py",
                    "
                    import os

                    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
                    ",
                ),
                ("project/__init__.py", ""),
                (
                    "project/settings.py",
                    "
                    INSTALLED_APPS = []

                    TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]
                    ",
                ),
                (
                    "app/templates/app/page.html",
                    r#"{{ value|shorten:"<CURSOR>" }}"#,
                ),
            ],
            DJANGO_BUILTINS,
        );

        assert_eq!(test.signature(), "|shorten:arg [arg]");
    }

    #[test]
    fn a_django_filter_with_no_readable_django_is_offered_by_its_documentation() {
        let test = TemplateTest::new(&[(
            "blog/templates/blog/post.html",
            r#"{{ value|date:"<CURSOR>" }}"#,
        )]);

        assert_eq!(
            test.signature(),
            "|date — formats a date with the given format string."
        );
    }

    #[test]
    fn a_filter_nothing_knows_offers_nothing() {
        assert_eq!(
            project(r#"{{ book.title|mystery:"<CURSOR>" }}"#).signature(),
            "no signature"
        );
    }

    #[test]
    fn a_comment_offers_nothing() {
        assert_eq!(
            project(r#"{# book.title|shout:"<CURSOR>" #}"#).signature(),
            "no signature"
        );
    }
}
