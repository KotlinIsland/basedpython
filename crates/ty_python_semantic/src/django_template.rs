//! the lints django support can report
//!
//! nothing here checks anything: the checks live in `ty_ide`'s django front end,
//! which is the only place that has read the project's templates and its url
//! tree. what has to live here is the *declaration* of each rule, because the
//! lint registry — and with it `[tool.ty.rules]`, the generated schema and the
//! generated documentation — is this crate's.
//!
//! most of them are a template's, and the last two are the python side of the
//! same join: a route pattern names the arguments django will hand the view, and
//! the view has to be able to take them.

use crate::declare_lint;
use crate::lint::{Level, LintRegistryBuilder, LintStatus};

/// Register the lints a django template can report.
pub(crate) fn register_lints(registry: &mut LintRegistryBuilder) {
    registry.register_lint(&UNCLOSED_TEMPLATE_BLOCK);
    registry.register_lint(&UNMATCHED_TEMPLATE_CLOSE);
    registry.register_lint(&UNKNOWN_TEMPLATE_LIBRARY);
    registry.register_lint(&UNKNOWN_TEMPLATE_TAG);
    registry.register_lint(&UNKNOWN_TEMPLATE_FILTER);
    registry.register_lint(&UNLOADED_TEMPLATE_LIBRARY);
    registry.register_lint(&UNRESOLVED_TEMPLATE);
    registry.register_lint(&UNRESOLVED_STATIC_FILE);
    registry.register_lint(&UNRESOLVED_ROUTE);
    registry.register_lint(&INVALID_ROUTE_ARGUMENTS);
    registry.register_lint(&UNKNOWN_TEMPLATE_BLOCK);
    registry.register_lint(&TEMPLATE_MEMBER_NEEDS_ARGUMENTS);
    registry.register_lint(&TEMPLATE_MEMBER_ALTERS_DATA);
    registry.register_lint(&INVALID_ROUTE_HANDLER);
    registry.register_lint(&INVALID_ROUTE_PARAMETER_TYPE);
}

declare_lint! {
    /// ## What it does
    /// Checks for a block tag in a django template whose closing tag is missing.
    ///
    /// ## Why is this bad?
    /// Django raises `TemplateSyntaxError` when it compiles the template, so the
    /// page does not render at all.
    ///
    /// Only a tag known to open a block is reported: one of django's own, or one
    /// the project registers with `@register.simple_block_tag`.
    ///
    /// ## Examples
    /// ```django
    /// {% if user.is_staff %}    {# error: no `{% endif %}` #}
    ///   <p>hello</p>
    /// ```
    pub static UNCLOSED_TEMPLATE_BLOCK = {
        summary: "detects a django template block tag that is never closed",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a closing tag in a django template that closes nothing.
    ///
    /// ## Why is this bad?
    /// Django raises `TemplateSyntaxError` when it compiles the template, so the
    /// page does not render at all.
    ///
    /// ## Examples
    /// ```django
    /// {% for book in books %}
    ///   <p>{{ book.title }}</p>
    /// {% endwith %}    {# error: nothing here opened a `{% with %}` #}
    /// {% endfor %}
    /// ```
    pub static UNMATCHED_TEMPLATE_CLOSE = {
        summary: "detects a django template closing tag that closes nothing",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a `{% load %}` of a tag library the project does not have.
    ///
    /// ## Why is this bad?
    /// Django raises `TemplateSyntaxError` when it compiles the template, so the
    /// page does not render at all.
    ///
    /// A library is a `templatetags` module of the project or of one of the apps
    /// `INSTALLED_APPS` names. Nothing is reported unless the settings module was
    /// found and every installed app resolved, since a library that cannot be
    /// reached is not a library that is missing.
    ///
    /// ## Examples
    /// ```django
    /// {% load blog_xtras %}    {# error: the library is `blog_extras` #}
    /// ```
    pub static UNKNOWN_TEMPLATE_LIBRARY = {
        summary: "detects a `{% load %}` of a library the project does not have",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a tag no library the template can reach registers.
    ///
    /// ## Why is this bad?
    /// Django raises `TemplateSyntaxError` when it compiles the template, so the
    /// page does not render at all.
    ///
    /// A tag whose library the template has simply not loaded is reported as
    /// `unloaded-template-library` instead.
    ///
    /// ## Examples
    /// ```django
    /// {% iff user.is_staff %}    {# error: no such tag #}
    /// ```
    pub static UNKNOWN_TEMPLATE_TAG = {
        summary: "detects a django template tag nothing registers",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a filter no library the template can reach registers.
    ///
    /// ## Why is this bad?
    /// Django raises `TemplateSyntaxError` when it compiles the template, so the
    /// page does not render at all.
    ///
    /// A filter whose library the template has simply not loaded is reported as
    /// `unloaded-template-library` instead.
    ///
    /// ## Examples
    /// ```django
    /// {{ book.title|uppercase }}    {# error: the filter is `upper` #}
    /// ```
    pub static UNKNOWN_TEMPLATE_FILTER = {
        summary: "detects a django template filter nothing registers",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a tag or filter used without the `{% load %}` it needs.
    ///
    /// ## Why is this bad?
    /// Django raises `TemplateSyntaxError` when it compiles the template, so the
    /// page does not render at all — the tag exists, but not in this template.
    ///
    /// A library named by `TEMPLATES[*]["OPTIONS"]["builtins"]` is loaded into
    /// every template already and is never reported.
    ///
    /// ## Examples
    /// ```django
    /// {% static 'css/site.css' %}    {# error: needs `{% load static %}` #}
    /// ```
    pub static UNLOADED_TEMPLATE_LIBRARY = {
        summary: "detects a tag or filter used without the `{% load %}` it needs",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for an `{% extends %}` or `{% include %}` naming a template that
    /// is not there.
    ///
    /// ## Why is this bad?
    /// Django raises `TemplateDoesNotExist` when it renders the template, so the
    /// page 500s.
    ///
    /// Nothing is reported unless the project's template directories are known:
    /// a project with no readable settings *and* no directory named `templates`
    /// is one whose template set cannot be established.
    ///
    /// ## Examples
    /// ```django
    /// {% extends "blog/bass.html" %}    {# error: the template is `blog/base.html` #}
    /// ```
    pub static UNRESOLVED_TEMPLATE = {
        summary: "detects a reference to a template that is not there",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a `{% static %}` naming a file that is not there.
    ///
    /// ## Why is this bad?
    /// Django's default storage builds the url from the name without checking it,
    /// so the page renders and the asset 404s — a failure nothing reports.
    ///
    /// A name whose directory holds no discovered file at all is left alone: that
    /// is what a bundle built into `static/` at deploy time looks like, and it is
    /// not something the source tree can answer.
    ///
    /// ## Examples
    /// ```django
    /// {% load static %}
    /// <link href="{% static 'css/sight.css' %}">    {# warning: it is `css/site.css` #}
    /// ```
    pub static UNRESOLVED_STATIC_FILE = {
        summary: "detects a `{% static %}` naming a file that is not there",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a `{% url %}` naming a route the url configuration does not
    /// have.
    ///
    /// ## Why is this bad?
    /// Django raises `NoReverseMatch` when it renders the template, so the page
    /// 500s.
    ///
    /// The route names are the ones the walk from `ROOT_URLCONF` finds. A project
    /// that does not say where its url tree starts, or whose tree could not be
    /// walked in full, reports nothing.
    ///
    /// ## Examples
    /// ```django
    /// <a href="{% url 'blog:detail' book.pk %}">    {# error: it is `blog:detail` #}
    /// ```
    pub static UNRESOLVED_ROUTE = {
        summary: "detects a `{% url %}` naming a route that does not exist",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks the arguments a `{% url %}` passes against the route's own pattern.
    ///
    /// ## Why is this bad?
    /// Django raises `NoReverseMatch` when it renders the template, so the page
    /// 500s. A route's pattern names the arguments it takes and the converter
    /// each of them goes through, so a missing, extra or misnamed argument — and
    /// a literal the converter would reject — is a reversal that cannot match.
    ///
    /// Only a route whose whole pattern is known is checked, and a name several
    /// routes share is reported only when none of them accepts the arguments.
    ///
    /// ## Examples
    /// ```django
    /// {# path("<int:pk>/", detail, name="detail") #}
    /// {% url 'detail' %}          {# error: `pk` is missing #}
    /// {% url 'detail' pk='x' %}   {# error: `pk` goes through `int` #}
    /// {% url 'detail' pk=1 %}     {# ok #}
    /// ```
    pub static INVALID_ROUTE_ARGUMENTS = {
        summary: "detects a `{% url %}` whose arguments the route cannot take",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a `{% block %}` overriding a block no ancestor template
    /// declares.
    ///
    /// ## Why is this bad?
    /// A child template's blocks are rendered by the parent, so a block the
    /// parent never declares is never rendered — silently, with no error and no
    /// output.
    ///
    /// Only a block written at the top level of a template that `{% extends %}`
    /// something is reported. A block nested inside another one is rendered as
    /// part of its enclosing block and needs no declaration above it.
    ///
    /// ## Examples
    /// ```django
    /// {# base.html declares `content`, and nothing else #}
    /// {% extends "base.html" %}
    /// {% block sidebar %}hello{% endblock %}    {# warning: never rendered #}
    /// ```
    pub static UNKNOWN_TEMPLATE_BLOCK = {
        summary: "detects a `{% block %}` no ancestor template declares",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a `{{ }}` lookup landing on a method that cannot be called
    /// without arguments.
    ///
    /// ## Why is this bad?
    /// Django calls whatever a lookup lands on. A method that needs an argument
    /// cannot be called, so django renders `string_if_invalid` — the empty string
    /// by default — silently, with no error and no output.
    ///
    /// ## Examples
    /// ```python
    /// class Book:
    ///     def headline(self, length: int) -> str: ...
    ///     def title(self) -> str: ...
    /// ```
    ///
    /// ```django
    /// {{ book.headline }}    {# warning: renders nothing #}
    /// {{ book.title }}       {# ok #}
    /// ```
    pub static TEMPLATE_MEMBER_NEEDS_ARGUMENTS = {
        summary: "detects a template lookup landing on a method that needs arguments",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks for a `{{ }}` lookup landing on a method django refuses to call
    /// from a template.
    ///
    /// ## Why is this bad?
    /// Django calls whatever a lookup lands on, except a method marked
    /// `alters_data = True` — a template is not allowed to write to the
    /// database. Rather than call it, django renders `string_if_invalid` — the
    /// empty string by default — silently, with no error and no output.
    ///
    /// The methods django marks are the ones that write: `save`, `delete` and
    /// their `async` twins on a model, and the `create`/`update`/`bulk_*`
    /// family on a queryset, a manager and the manager behind a relation.
    /// Overriding one keeps the mark, so an override is reported too, exactly
    /// as django's own `AltersData` propagates it at runtime.
    ///
    /// ## Examples
    /// ```django
    /// {{ book.save }}     {# warning: renders nothing #}
    /// {{ book.title }}    {# ok #}
    /// ```
    pub static TEMPLATE_MEMBER_ALTERS_DATA = {
        summary: "detects a template lookup landing on a method django refuses to call",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Warn,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks that the view a route names can take the arguments the route's
    /// pattern gives it.
    ///
    /// ## Why is this bad?
    /// A route pattern names its arguments, and django hands each of them to the
    /// view as a keyword argument. A view that names them differently, or does
    /// not take them at all, raises `TypeError` when the url is requested — a
    /// failure nothing reports until the page 500s.
    ///
    /// Only a project whose url tree could be walked in full is checked, since a
    /// route's arguments include the ones the patterns it is mounted behind
    /// contribute. A view taking `*args` or `**kwargs` accepts anything and is
    /// never reported, nor is one reached through a decorator the type checker
    /// cannot see through. A class-based view is checked through the handler
    /// methods it declares itself: what it inherits from django takes `**kwargs`.
    ///
    /// ## Examples
    /// ```python
    /// path("books/<int:pk>/", views.detail, name="detail")
    /// ```
    ///
    /// ```python
    /// def detail(request, id): ...     # error: the route names `pk`
    /// def detail(request): ...         # error: `pk` has nowhere to go
    /// def detail(request, pk): ...     # ok
    /// ```
    pub static INVALID_ROUTE_HANDLER = {
        summary: "detects a view that cannot take the arguments its route gives it",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Error,
    }
}

declare_lint! {
    /// ## What it does
    /// Checks the type a view declares for a route argument against the type the
    /// route's converter produces.
    ///
    /// ## Why is this bad?
    /// A path converter parses the url before django calls the view:
    /// `<int:pk>` hands the view an `int` and `<uuid:key>` a `uuid.UUID`. A view
    /// declaring something else is annotated with a type its argument never has,
    /// so everything written against the annotation is written against a value
    /// that is not there.
    ///
    /// Unlike `invalid-route-handler` this does not stop the request: django
    /// calls the view and the wrong value arrives silently.
    /// An unannotated parameter declares nothing and is never reported,
    /// and neither is one matched by a regular expression, which goes through no
    /// converter at all.
    ///
    /// ## Examples
    /// ```python
    /// path("books/<int:pk>/", views.detail, name="detail")
    /// ```
    ///
    /// ```python
    /// def detail(request, pk: str): ...    # warning: the converter gives an `int`
    /// def detail(request, pk: int): ...    # ok
    /// ```
    pub static INVALID_ROUTE_PARAMETER_TYPE = {
        summary: "detects a view declaring a type its route's converter does not produce",
        status: LintStatus::stable("0.0.69"),
        default_level: Level::Warn,
    }
}
