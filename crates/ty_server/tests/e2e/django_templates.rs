//! end-to-end coverage of the django template language services
//!
//! the unit tests in `ty_ide` cover what each feature answers; these cover that a
//! template document reaches those features at all — that the server recognises
//! the language, keeps the type checker away from it, and routes its requests to
//! the template implementations rather than the python ones.

use anyhow::Result;
use lsp_types::{
    DocumentDiagnosticReport, FileChangeType, FileEvent, LanguageKind, Position,
    TextDocumentIdentifier, TextDocumentPositionParams,
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
fn a_template_hovers_a_context_variables_attribute_as_its_type() -> Result<()> {
    let mut server = server("<p>{{ book.title }}</p>\n")?;

    let hover = server
        .hover_request(
            SystemPath::new("src/blog/templates/blog/post.html"),
            Position::new(0, 13),
        )
        .expect("a hover response");

    let rendered = format!("{:?}", hover.contents);
    assert!(rendered.contains("title: str"), "got {rendered}");

    Ok(())
}

#[test]
fn a_template_hovers_a_filter_as_its_documentation() -> Result<()> {
    let mut server = server("<p>{{ book.title|upper }}</p>\n")?;

    let hover = server
        .hover_request(
            SystemPath::new("src/blog/templates/blog/post.html"),
            Position::new(0, 19),
        )
        .expect("a hover response");

    let rendered = format!("{:?}", hover.contents);
    assert!(rendered.contains("|upper"), "got {rendered}");

    Ok(())
}

#[test]
fn a_template_outlines_its_blocks() -> Result<()> {
    let mut server =
        server("{% block content %}\n{% block inner %}\na\n{% endblock %}\n{% endblock %}\n")?;
    let uri = template_uri(&server);

    let response = server
        .document_symbol_request(&uri)
        .expect("a document symbol response");

    let rendered = format!("{response:?}");
    assert!(rendered.contains("content"), "got {rendered}");
    assert!(rendered.contains("inner"), "got {rendered}");

    Ok(())
}

#[test]
fn a_template_folds_its_block_tags() -> Result<()> {
    let mut server = server("{% for book in books %}\n<p>{{ book.title }}</p>\n{% endfor %}\n")?;
    let uri = template_uri(&server);

    let ranges = server
        .folding_range_request(&uri)
        .expect("a folding range response");

    assert_eq!(ranges.len(), 1, "got {ranges:?}");
    assert_eq!(ranges[0].start_line, 0);
    assert_eq!(ranges[0].end_line, 2);

    Ok(())
}

#[test]
fn a_template_created_during_the_session_is_offered_to_an_extends() -> Result<()> {
    // discovering templates means walking the file system, which salsa cannot
    // see into: without something to invalidate the walk the server would go on
    // offering the set of templates that existed when it started
    let mut server = server("{% extends '' %}\n")?;
    let uri = template_uri(&server);

    let offered = |server: &mut TestServer| -> Vec<String> {
        server
            .completion_request(&uri, Position::new(0, 12))
            .iter()
            .map(|completion| completion.label.clone())
            .collect()
    };

    assert!(!offered(&mut server).contains(&"blog/card.html".to_string()));

    let created = SystemPath::new("src/blog/templates/blog/card.html");
    server.write_file(created, "<p>a card</p>\n")?;
    server.did_change_watched_files(vec![FileEvent {
        uri: server.file_uri(created),
        kind: FileChangeType::Created,
    }]);

    assert!(
        offered(&mut server).contains(&"blog/card.html".to_string()),
        "a template created after the server started is still a template"
    );

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

/// a view that names a template and reverses a route, both as plain strings
const WHOLE_VIEWS: &str = "\
from django.shortcuts import render
from django.urls import reverse


def post(request):
    reverse('blog:detail')
    return render(request, 'blog/post.html', {})
";

/// the same project, with a settings module that makes the indexes authoritative
///
/// a rename refuses outright unless the whole project can be read, so nothing
/// below can share the smaller fixture above.
fn whole_project(templates: &[(&str, &str)]) -> Result<TestServer> {
    let mut builder = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(
            SystemPath::new("src/manage.py"),
            "import os\n\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')\n",
        )?
        .with_file(SystemPath::new("src/project/__init__.py"), "")?
        .with_file(
            SystemPath::new("src/project/settings.py"),
            "INSTALLED_APPS = ['blog']\n\nTEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]\n\nROOT_URLCONF = 'project.urls'\n",
        )?
        .with_file(
            SystemPath::new("src/project/urls.py"),
            "from django.urls import include, path\n\nurlpatterns = [path('blog/', include('blog.urls'))]\n",
        )?
        .with_file(SystemPath::new("src/blog/__init__.py"), "")?
        .with_file(
            SystemPath::new("src/blog/urls.py"),
            "from django.urls import path\n\napp_name = 'blog'\n\nurlpatterns = [path('<int:pk>/', detail, name='detail')]\n",
        )?
        .with_file(SystemPath::new("src/blog/views.py"), WHOLE_VIEWS)?;

    for (path, contents) in templates {
        builder = builder.with_file(SystemPath::new(path), contents)?;
    }

    let mut server = builder.build().wait_until_workspaces_are_initialized();

    for (path, contents) in templates {
        server.open_text_document_as(
            SystemPath::new(path),
            contents,
            1,
            LanguageKind::new("django-html"),
        );
    }

    Ok(server)
}

/// a uri as the path below the workspace root, so that a test reads the same
/// wherever it was run
fn relative(uri: &lsp_types::Uri) -> String {
    let path = uri.as_str();
    match path.split_once("/src/") {
        Some((_, below)) => below.to_string(),
        None => path.to_string(),
    }
}

/// every edit of a workspace edit, as `path:line:column-column -> text`, sorted
fn rendered(edit: &lsp_types::WorkspaceEdit) -> Vec<String> {
    let render = |uri: &lsp_types::Uri, edits: &[lsp_types::TextEdit]| -> Vec<String> {
        edits
            .iter()
            .map(|edit| {
                format!(
                    "{}:{:03}:{:03}-{:03} -> {}",
                    relative(uri),
                    edit.range.start.line,
                    edit.range.start.character,
                    edit.range.end.character,
                    edit.new_text,
                )
            })
            .collect()
    };

    let mut lines = Vec::new();

    if let Some(changes) = &edit.changes {
        let mut by_file: Vec<_> = changes.iter().collect();
        by_file.sort_by_key(|(uri, _)| uri.as_str());

        for (uri, edits) in by_file {
            lines.extend(render(uri, edits));
        }
    }

    for change in edit.document_changes.iter().flatten() {
        match change {
            lsp_types::DocumentChange::TextDocumentEdit(document) => {
                let edits: Vec<lsp_types::TextEdit> = document
                    .edits
                    .iter()
                    .map(|edit| match edit {
                        lsp_types::Edit::TextEdit(edit) => edit.clone(),
                        other => panic!("expected a plain text edit, got {other:?}"),
                    })
                    .collect();
                lines.extend(render(
                    &document.text_document.text_document_identifier.uri,
                    &edits,
                ));
            }
            lsp_types::DocumentChange::RenameFile(rename) => lines.push(format!(
                "move {} -> {}",
                relative(&rename.old_uri),
                relative(&rename.new_uri),
            )),
            other => panic!("expected only edits and a file rename, got {other:?}"),
        }
    }

    lines.sort();
    lines
}

fn rename_request(
    server: &mut TestServer,
    uri: &lsp_types::Uri,
    position: Position,
    new_name: &str,
) -> lsp_types::WorkspaceEdit {
    server
        .send_request_await::<lsp_types::RenameRequest>(lsp_types::RenameParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            new_name: new_name.to_string(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        })
        .expect("a workspace edit")
}

#[test]
fn a_block_rename_reaches_the_base_and_every_child() -> Result<()> {
    let mut server = whole_project(&[
        (
            "src/blog/templates/blog/base.html",
            "{% block content %}{% endblock content %}\n",
        ),
        (
            "src/blog/templates/blog/post.html",
            "{% extends 'blog/base.html' %}{% block content %}a{% endblock %}\n",
        ),
    ])?;
    let uri = server.file_uri(SystemPath::new("src/blog/templates/blog/post.html"));

    let prepared = server
        .send_request_await::<lsp_types::PrepareRenameRequest>(lsp_types::PrepareRenameParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(0, 40),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        })
        .expect("a prepare rename response");

    assert!(
        matches!(
            &prepared,
            lsp_types::PrepareRenameResult::PrepareRenamePlaceholder(placeholder)
                if placeholder.placeholder == "content"
        ),
        "got {prepared:?}"
    );

    let edit = rename_request(&mut server, &uri, Position::new(0, 40), "body");
    assert_eq!(
        rendered(&edit),
        [
            "blog/templates/blog/base.html:000:009-016 -> body",
            "blog/templates/blog/base.html:000:031-038 -> body",
            "blog/templates/blog/post.html:000:039-046 -> body",
        ]
    );

    Ok(())
}

#[test]
fn a_route_rename_reaches_the_declaration_the_python_and_the_template() -> Result<()> {
    let mut server = whole_project(&[(
        "src/blog/templates/blog/post.html",
        "{% url 'blog:detail' pk=1 %}\n",
    )])?;
    let uri = server.file_uri(SystemPath::new("src/blog/templates/blog/post.html"));

    let edit = rename_request(&mut server, &uri, Position::new(0, 15), "entry");
    assert_eq!(
        rendered(&edit),
        [
            "blog/templates/blog/post.html:000:013-019 -> entry",
            "blog/urls.py:004:047-053 -> entry",
            "blog/views.py:005:018-024 -> entry",
        ]
    );

    Ok(())
}

#[test]
fn a_template_rename_moves_the_file_as_well_as_rewriting_its_name() -> Result<()> {
    let mut server = whole_project(&[
        ("src/blog/templates/blog/post.html", "<p>a post</p>\n"),
        (
            "src/blog/templates/blog/list.html",
            "{% include 'blog/post.html' %}\n",
        ),
    ])?;
    let uri = server.file_uri(SystemPath::new("src/blog/templates/blog/list.html"));

    let edit = rename_request(&mut server, &uri, Position::new(0, 15), "blog/entry.html");
    assert_eq!(
        rendered(&edit),
        [
            "blog/templates/blog/list.html:000:012-026 -> blog/entry.html",
            "blog/views.py:006:028-042 -> blog/entry.html",
            "move blog/templates/blog/post.html -> blog/templates/blog/entry.html",
        ]
    );

    Ok(())
}

#[test]
fn a_rename_that_cannot_be_completed_comes_back_as_an_error() -> Result<()> {
    let mut server = whole_project(&[
        (
            "src/blog/templates/blog/dynamic.html",
            "{% extends parent %}{% block content %}a{% endblock %}\n",
        ),
        (
            "src/blog/templates/blog/base.html",
            "{% block content %}{% endblock %}\n",
        ),
    ])?;
    let uri = server.file_uri(SystemPath::new("src/blog/templates/blog/base.html"));

    let id =
        server.send_request::<lsp_types::PrepareRenameRequest>(lsp_types::PrepareRenameParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position::new(0, 12),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        });

    let failure = server
        .try_await_response::<lsp_types::PrepareRenameRequest>(&id, None)
        .expect_err("a refusal rather than a rename the editor could not finish");

    assert!(
        format!("{failure}").contains("render time"),
        "got {failure}"
    );

    Ok(())
}

#[test]
fn a_route_renamed_from_python_leaves_the_python_services_alone() -> Result<()> {
    // `reverse('blog:detail')` is a string to python, so the python rename
    // declines it and the django one answers — while the class beside it is
    // still renamed the way it always was
    let mut server = whole_project(&[(
        "src/blog/templates/blog/post.html",
        "{% url 'blog:detail' pk=1 %}\n",
    )])?;
    let views = SystemPath::new("src/blog/views.py");
    server.open_text_document(views, WHOLE_VIEWS, 1);
    let uri = server.file_uri(views);

    let edit = rename_request(&mut server, &uri, Position::new(5, 20), "entry");
    assert_eq!(
        rendered(&edit),
        [
            "blog/templates/blog/post.html:000:013-019 -> entry",
            "blog/urls.py:004:047-053 -> entry",
            "blog/views.py:005:018-024 -> entry",
        ]
    );

    Ok(())
}

/// every location a references request answers with, as `path:line:column`, sorted
fn located(locations: Option<Vec<lsp_types::Location>>) -> Vec<String> {
    let mut lines: Vec<String> = locations
        .unwrap_or_default()
        .iter()
        .map(|location| {
            format!(
                "{}:{:03}:{:03}",
                relative(&location.uri),
                location.range.start.line,
                location.range.start.character,
            )
        })
        .collect();

    lines.sort();
    lines
}

fn references_request(
    server: &mut TestServer,
    uri: &lsp_types::Uri,
    position: Position,
    include_declaration: bool,
) -> Vec<String> {
    located(
        server.send_request_await::<lsp_types::ReferencesRequest>(lsp_types::ReferenceParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
            context: lsp_types::ReferenceContext {
                include_declaration,
            },
        }),
    )
}

#[test]
fn a_blocks_references_are_every_template_of_the_family_that_declares_it() -> Result<()> {
    let mut server = whole_project(&[
        (
            "src/blog/templates/blog/base.html",
            "{% block content %}{% endblock content %}\n",
        ),
        (
            "src/blog/templates/blog/post.html",
            "{% extends 'blog/base.html' %}{% block content %}a{% endblock %}\n",
        ),
        (
            "src/blog/templates/blog/list.html",
            "{% extends 'blog/base.html' %}{% block content %}b{% endblock %}\n",
        ),
    ])?;
    let uri = server.file_uri(SystemPath::new("src/blog/templates/blog/base.html"));

    assert_eq!(
        references_request(&mut server, &uri, Position::new(0, 12), true),
        [
            "blog/templates/blog/base.html:000:009",
            "blog/templates/blog/base.html:000:031",
            "blog/templates/blog/list.html:000:039",
            "blog/templates/blog/post.html:000:039",
        ]
    );
    assert_eq!(
        references_request(&mut server, &uri, Position::new(0, 12), false),
        [
            "blog/templates/blog/list.html:000:039",
            "blog/templates/blog/post.html:000:039",
        ]
    );

    Ok(())
}

#[test]
fn a_templates_references_are_everything_that_renders_it() -> Result<()> {
    // the cursor is in the template's own text, which names nothing in
    // particular — "what renders this file" is what is left to answer
    let mut server = whole_project(&[
        ("src/blog/templates/blog/post.html", "<p>a post</p>\n"),
        (
            "src/blog/templates/blog/list.html",
            "{% include 'blog/post.html' %}\n",
        ),
    ])?;
    let uri = server.file_uri(SystemPath::new("src/blog/templates/blog/post.html"));

    assert_eq!(
        references_request(&mut server, &uri, Position::new(0, 5), false),
        [
            "blog/templates/blog/list.html:000:012",
            "blog/views.py:006:028",
        ]
    );

    Ok(())
}

#[test]
fn a_routes_references_reach_both_languages() -> Result<()> {
    let mut server = whole_project(&[(
        "src/blog/templates/blog/post.html",
        "{% url 'blog:detail' pk=1 %}\n",
    )])?;
    let uri = server.file_uri(SystemPath::new("src/blog/templates/blog/post.html"));

    assert_eq!(
        references_request(&mut server, &uri, Position::new(0, 15), true),
        [
            "blog/templates/blog/post.html:000:008",
            "blog/urls.py:004:047",
            "blog/views.py:005:013",
        ]
    );

    Ok(())
}

#[test]
fn a_python_symbols_references_are_found_the_way_they_always_were() -> Result<()> {
    // `reverse('blog:detail')` is a string to python, so the python references
    // decline it and the django ones answer — while the parameter beside it is
    // still found the way it always was
    let mut server = whole_project(&[(
        "src/blog/templates/blog/post.html",
        "{% url 'blog:detail' pk=1 %}\n",
    )])?;
    let views = SystemPath::new("src/blog/views.py");
    server.open_text_document(views, WHOLE_VIEWS, 1);
    let uri = server.file_uri(views);

    assert_eq!(
        references_request(&mut server, &uri, Position::new(5, 20), true),
        [
            "blog/templates/blog/post.html:000:008",
            "blog/urls.py:004:047",
            "blog/views.py:005:013",
        ]
    );
    assert_eq!(
        references_request(&mut server, &uri, Position::new(4, 10), true),
        ["blog/views.py:004:009", "blog/views.py:006:018"]
    );

    Ok(())
}
