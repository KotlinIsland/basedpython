//! `by check` over a django project.
//!
//! A template is not python and is not in the project's file set, so these are
//! also the tests that say `by check` reaches a file the type checker never sees —
//! and that it still never reads one as python.

use crate::CliTest;
use insta_cmd::assert_cmd_snapshot;

/// A django project whose settings, templates and url tree can all be read.
///
/// `django` is written *outside* the project root and reached through an extra
/// search path, which is what makes it an installed package rather than source of
/// the project's own — the same difference site-packages makes.
fn django_project(files: &[(&str, &str)]) -> anyhow::Result<CliTest> {
    let mut all: Vec<(&str, &str)> = vec![
        (
            "ty.toml",
            r#"
            [environment]
            extra-paths = ["../libs"]
            "#,
        ),
        (
            "manage.py",
            r#"
            import os

            os.environ.setdefault("DJANGO_SETTINGS_MODULE", "project.settings")
            "#,
        ),
        ("project/__init__.py", ""),
        (
            "project/settings.py",
            r#"
            INSTALLED_APPS = ["blog"]

            TEMPLATES = [{"DIRS": [], "APP_DIRS": True, "OPTIONS": {}}]

            ROOT_URLCONF = "project.urls"
            "#,
        ),
        (
            "project/urls.py",
            r#"
            from django.urls import include, path

            urlpatterns = [path("blog/", include("blog.urls"))]
            "#,
        ),
        ("blog/__init__.py", ""),
        (
            "blog/views.py",
            "
            def index(request): ...


            def detail(request, pk: int): ...
            ",
        ),
        (
            "blog/urls.py",
            r#"
            from django.urls import path

            from blog import views

            app_name = "blog"

            urlpatterns = [
                path("", views.index, name="index"),
                path("<int:pk>/", views.detail, name="detail"),
            ]
            "#,
        ),
        (
            "blog/templates/blog/base.html",
            "{% block content %}{% endblock %}",
        ),
    ];
    all.extend_from_slice(files);

    let case = CliTest::with_files(all)?;

    write_installed(
        &case,
        &[
            ("django/__init__.py", ""),
            (
                "django/urls/__init__.py",
                "def path(route, view, kwargs=None, name=None): ...\n\
                 def include(arg, namespace=None): ...\n",
            ),
        ],
    )?;

    Ok(case)
}

/// Write packages to the `libs` directory beside the project root, which
/// [`django_project`] puts on the search path.
fn write_installed(case: &CliTest, files: &[(&str, &str)]) -> anyhow::Result<()> {
    let libs = case
        .root()
        .parent()
        .expect("the project directory always has a parent")
        .join("libs");

    for (path, content) in files {
        let path = libs.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
    }

    Ok(())
}

#[test]
fn a_template_is_checked() -> anyhow::Result<()> {
    let case = django_project(&[(
        "blog/templates/blog/post.html",
        "{% if book %}\n<p>{{ book.title|uppercase }}</p>\n",
    )])?;

    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r"
    success: false
    exit_code: 1
    ----- stdout -----
    blog/templates/blog/post.html:1:1: error[unclosed-template-block] unclosed `if`
    blog/templates/blog/post.html:2:18: error[unknown-template-filter] no template filter named `uppercase`
    Found 2 diagnostics

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn a_template_is_never_read_as_python() -> anyhow::Result<()> {
    // every line of this is a python syntax error, and the only thing reported is
    // the one thing wrong with it as a template
    let case = django_project(&[(
        "blog/templates/blog/post.html",
        "<ul>\n{% for book in books %}\n  <li>{{ book.title }}</li>\n",
    )])?;

    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r"
    success: false
    exit_code: 1
    ----- stdout -----
    blog/templates/blog/post.html:2:1: error[unclosed-template-block] unclosed `for`
    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn a_template_named_on_the_command_line_is_still_not_read_as_python() -> anyhow::Result<()> {
    // a path passed explicitly is otherwise taken to be something ty can analyze,
    // whatever its extension
    let case = django_project(&[("blog/templates/blog/post.html", "{% for book in books %}\n")])?;

    assert_cmd_snapshot!(
        case.command()
            .arg("--output-format=concise")
            .arg("blog/templates/blog/post.html"),
        @r"
    success: false
    exit_code: 1
    ----- stdout -----
    blog/templates/blog/post.html:1:1: error[unclosed-template-block] unclosed `for`
    Found 1 diagnostic

    ----- stderr -----
    WARN No python files found under the given path(s)
    "
    );

    Ok(())
}

#[test]
fn a_suppression_comment_in_a_template_silences_it() -> anyhow::Result<()> {
    let case = django_project(&[(
        "blog/templates/blog/post.html",
        "{% if book %}{# ty: ignore[unclosed-template-block] #}\n\
         <p>{{ book.title|uppercase }}</p>{# ty: ignore[unknown-template-filter] #}\n",
    )])?;

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn a_template_rule_is_configured_like_any_other() -> anyhow::Result<()> {
    let case = django_project(&[
        (
            "blog/templates/blog/post.html",
            "{% if book %}\n<p>{{ book.title|uppercase }}</p>\n",
        ),
        (
            "ty.toml",
            r#"
            [environment]
            extra-paths = ["../libs"]

            [rules]
            unclosed-template-block = "warn"
            unknown-template-filter = "ignore"
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r"
    success: false
    exit_code: 1
    ----- stdout -----
    blog/templates/blog/post.html:1:1: warning[unclosed-template-block] unclosed `if`
    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn a_route_its_view_cannot_serve_is_reported() -> anyhow::Result<()> {
    let case = django_project(&[(
        "blog/urls.py",
        r#"
        from django.urls import path

        from blog import views

        app_name = "blog"

        urlpatterns = [
            path("<int:missing>/", views.index, name="broken"),
        ]
        "#,
    )])?;

    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r"
    success: false
    exit_code: 1
    ----- stdout -----
    blog/urls.py:9:28: error[invalid-route-handler] `index` takes no argument named `missing`
    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn a_suppression_that_silenced_a_route_is_not_reported_unused() -> anyhow::Result<()> {
    let case = django_project(&[(
        "blog/urls.py",
        r#"
        from django.urls import path

        from blog import views

        app_name = "blog"

        urlpatterns = [
            path("<int:missing>/", views.index, name="broken"),  # ty: ignore[invalid-route-handler]
        ]
        "#,
    )])?;

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn an_installed_apps_own_templates_are_not_the_projects_to_report() -> anyhow::Result<()> {
    // django loads these exactly as it loads the project's, and everything that
    // reads a template reaches them — but they are a dependency's source
    let case = django_project(&[
        (
            "project/settings.py",
            r#"
            INSTALLED_APPS = ["django.contrib.admin", "blog"]

            TEMPLATES = [{"DIRS": [], "APP_DIRS": True, "OPTIONS": {}}]

            ROOT_URLCONF = "project.urls"
            "#,
        ),
        // the same fault, in the project. it is what says the installed one was
        // discovered and left alone rather than never discovered at all
        ("blog/templates/blog/post.html", "{% if broken %}\n"),
    ])?;

    write_installed(
        &case,
        &[
            ("django/contrib/__init__.py", ""),
            ("django/contrib/admin/__init__.py", ""),
            (
                "django/contrib/admin/templates/admin/base.html",
                "{% if broken %}\n",
            ),
        ],
    )?;

    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r"
    success: false
    exit_code: 1
    ----- stdout -----
    blog/templates/blog/post.html:1:1: error[unclosed-template-block] unclosed `if`
    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn a_project_without_django_reports_nothing_about_its_templates() -> anyhow::Result<()> {
    // `templates/index.html` in a flask or a jinja project is not a django
    // template, and nothing here knows what its language even is
    let case = CliTest::with_files([
        ("app.py", "x: int = 1"),
        (
            "templates/index.html",
            "{% if book %}\n{{ book|uppercase }}\n",
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    Ok(())
}

#[test]
fn a_setting_has_the_type_the_settings_module_gives_it() -> anyhow::Result<()> {
    // the settings module is found from `manage.py`, which is a fact about the
    // project rather than about the file being checked — so this is the test that
    // says the type checker reaches it at all, and reaches it the same way the
    // language server does
    let case = django_project(&[(
        "blog/reads_settings.py",
        "
        from django.conf import settings

        reveal_type(settings.ROOT_URLCONF)
        reveal_type(settings.INSTALLED_APPS)
        reveal_type(settings.NAMED_NOWHERE)
        ",
    )])?;

    write_installed(
        &case,
        &[(
            "django/conf/__init__.py",
            "from typing import Any\n\
             \n\
             class LazySettings:\n\
             \x20   def __getattr__(self, name: str) -> Any: ...\n\
             \n\
             settings = LazySettings()\n",
        )],
    )?;

    // no path is named: naming one narrows the project to it, and the settings
    // module is then no more part of the project than any other file it excludes
    assert_cmd_snapshot!(case.command().arg("--output-format=concise"), @r"
    success: false
    exit_code: 1
    ----- stdout -----
    blog/reads_settings.py:4:1: warning[undefined-reveal] `reveal_type` used without importing it
    blog/reads_settings.py:4:13: info[revealed-type] Revealed type: `str`
    blog/reads_settings.py:5:1: warning[undefined-reveal] `reveal_type` used without importing it
    blog/reads_settings.py:5:13: info[revealed-type] Revealed type: `Any`
    blog/reads_settings.py:6:1: warning[undefined-reveal] `reveal_type` used without importing it
    blog/reads_settings.py:6:13: info[revealed-type] Revealed type: `Any`
    Found 6 diagnostics

    ----- stderr -----
    ");

    Ok(())
}
