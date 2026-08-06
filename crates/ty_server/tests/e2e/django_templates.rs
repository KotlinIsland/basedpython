//! end-to-end coverage of the django template language services
//!
//! the unit tests in `ty_ide` cover what each feature answers; these cover that a
//! template document reaches those features at all — that the server recognises
//! the language, keeps the type checker away from it, and routes its requests to
//! the template implementations rather than the python ones.

use anyhow::Result;
use lsp_types::{
    DocumentDiagnosticReport, LanguageKind, Position, TextDocumentIdentifier,
    TextDocumentPositionParams,
};
use ruff_db::system::SystemPath;

use crate::{TestServer, TestServerBuilder};

const VIEWS: &str = "\
from blog.models import Book


def post(request):
    return render(request, 'blog/post.html', {'book': Book()})
";

const MODELS: &str = "\
class Author:
    name: str


class Book:
    title: str
    author: Author
";

const URLS: &str = "\
app_name = 'blog'

urlpatterns = [path('books/', index, name='index')]
";

const BASE: &str = "{% block content %}{% endblock %}\n";

/// a project with a view, a model, a url configuration and a base template, plus
/// the template under test
fn server(template: &str) -> Result<TestServer> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/blog/models.py"), MODELS)?
        .with_file(SystemPath::new("src/blog/views.py"), VIEWS)?
        .with_file(SystemPath::new("src/blog/urls.py"), URLS)?
        .with_file(SystemPath::new("src/blog/templates/blog/base.html"), BASE)?
        .with_file(
            SystemPath::new("src/blog/templates/blog/post.html"),
            template,
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document_as(
        SystemPath::new("src/blog/templates/blog/post.html"),
        template,
        1,
        LanguageKind::new("django-html"),
    );

    Ok(server)
}

fn template_uri(server: &TestServer) -> lsp_types::Uri {
    server.file_uri(SystemPath::new("src/blog/templates/blog/post.html"))
}

#[test]
fn a_template_completes_the_views_context() -> Result<()> {
    let mut server = server("{{  }}\n")?;
    let uri = template_uri(&server);

    let completions = server.completion_request(&uri, Position::new(0, 3));
    let labels: Vec<_> = completions
        .iter()
        .map(|completion| completion.label.as_str())
        .collect();

    assert_eq!(labels, ["book"]);

    Ok(())
}

#[test]
fn a_template_completes_the_attributes_of_a_context_variable() -> Result<()> {
    let mut server = server("{{ book. }}\n")?;
    let uri = template_uri(&server);

    let completions = server.completion_request(&uri, Position::new(0, 8));
    let labels: Vec<_> = completions
        .iter()
        .map(|completion| completion.label.as_str())
        .collect();

    assert_eq!(labels, ["author", "title"]);

    Ok(())
}

#[test]
fn a_template_completes_tag_names() -> Result<()> {
    let mut server = server("{%  %}\n")?;
    let uri = template_uri(&server);

    let completions = server.completion_request(&uri, Position::new(0, 3));
    let labels: Vec<_> = completions
        .iter()
        .map(|completion| completion.label.as_str())
        .collect();

    assert!(labels.contains(&"extends"), "got {labels:?}");
    assert!(labels.contains(&"partialdef"), "got {labels:?}");

    Ok(())
}

#[test]
fn a_template_completion_replaces_an_explicit_range() -> Result<()> {
    // the client is told exactly what to replace, because a template name is not
    // a word by any client's definition of one
    let mut server = server("{% extends '' %}\n")?;
    let uri = template_uri(&server);

    let completions = server.completion_request(&uri, Position::new(0, 12));
    let first = completions.first().expect("a template to be offered");

    assert!(
        matches!(
            first.text_edit,
            Some(lsp_types::CompletionItemTextEdit::TextEdit(_))
        ),
        "got {:?}",
        first.text_edit
    );

    Ok(())
}

#[test]
fn a_template_produces_semantic_tokens() -> Result<()> {
    let mut server = server("<p>{{ book.title }}</p>\n")?;
    let uri = template_uri(&server);

    let tokens = server
        .semantic_tokens_full_request(&uri)
        .expect("a template to be highlighted");

    // `{{`, `book`, `.`, `title`, `}}` — the markup around them is the client's
    assert_eq!(tokens.data.len(), 5);

    Ok(())
}

#[test]
fn a_template_navigates_to_the_template_it_extends() -> Result<()> {
    let mut server = server("{% extends 'blog/base.html' %}\n")?;
    let uri = template_uri(&server);

    let response = server
        .send_request_await::<lsp_types::DefinitionRequest>(lsp_types::DefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position::new(0, 15),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        })
        .expect("a definition response");

    let rendered = format!("{response:?}");
    assert!(rendered.contains("base.html"), "got {rendered}");

    Ok(())
}

#[test]
fn a_template_is_never_type_checked_as_python() -> Result<()> {
    // every line of this would be a python syntax error
    let mut server = server("<!doctype html>\n<p>{{ book.title }}</p>\n")?;

    let report = server
        .document_diagnostic_request(SystemPath::new("src/blog/templates/blog/post.html"), None);

    let DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) = report else {
        panic!("expected a full report, got {report:?}");
    };
    assert!(
        report.full_document_diagnostic_report.items.is_empty(),
        "got {:?}",
        report.full_document_diagnostic_report.items
    );

    Ok(())
}

#[test]
fn an_html_file_outside_a_templates_directory_is_not_a_template() -> Result<()> {
    // the client called it `html`, and nothing about its path says otherwise, so
    // the server must not read it as a django template
    let page = SystemPath::new("src/static/index.html");
    let content = "{%  %}\n";

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/blog/models.py"), MODELS)?
        .with_file(SystemPath::new("src/blog/views.py"), VIEWS)?
        .with_file(page, content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document_as(page, content, 1, LanguageKind::new("html"));

    let completions = server.completion_request(&server.file_uri(page), Position::new(0, 3));
    let labels: Vec<_> = completions
        .iter()
        .map(|completion| completion.label.as_str())
        .collect();

    assert!(
        !labels.contains(&"partialdef"),
        "django tags were offered in a plain html file: {labels:?}"
    );

    Ok(())
}

#[test]
fn an_html_file_inside_a_templates_directory_is_a_template_whatever_the_client_calls_it()
-> Result<()> {
    // vs code reports a plain `.html` language for most django projects, so the
    // path has to be enough on its own
    let template = "{%  %}\n";
    let path = SystemPath::new("src/blog/templates/blog/post.html");

    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/blog/models.py"), MODELS)?
        .with_file(SystemPath::new("src/blog/views.py"), VIEWS)?
        .with_file(path, template)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document_as(path, template, 1, LanguageKind::new("html"));

    let completions = server.completion_request(&server.file_uri(path), Position::new(0, 3));
    let labels: Vec<_> = completions
        .iter()
        .map(|completion| completion.label.as_str())
        .collect();

    assert!(labels.contains(&"partialdef"), "got {labels:?}");

    Ok(())
}
