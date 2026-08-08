//! the outline of a django template, and the project's django names
//!
//! what a template declares is its `{% block %}`s and its `{% partialdef %}`s:
//! the names a child template can override and the fragments a `{% partial %}`
//! can render. everything else in the file is markup, which the editor outlines
//! far better than this could.
//!
//! [`workspace_symbols`] is the project-wide half of the same idea: the models,
//! admin classes, views, routes, templates, tags and filters a django project is
//! made of, so that a search across the workspace finds them by the names django
//! knows them by.

use std::borrow::Cow;

use ruff_db::files::{File, system_path_to_file};
use ruff_text_size::TextRange;
use rustc_hash::FxHashSet;
use ty_project::Db;

use crate::symbols::QueryPattern;
use crate::{SymbolInfo, SymbolKind};

use super::index::{Definition, TemplateIndex};
use super::project::{self, DjangoClassKind, RegistrationKind, TargetKind};
use super::template_index;

/// one entry of a template's outline
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// the name as written in the opening tag
    pub name_range: TextRange,
    /// the opening tag's `{%` through the closing tag's `%}`
    pub full_range: TextRange,
    /// the declarations written inside this one
    pub children: Vec<TemplateSymbol>,
}

impl TemplateSymbol {
    /// this symbol on its own, for a client that wants a flat list
    pub fn symbol_info(&self) -> SymbolInfo<'_> {
        SymbolInfo {
            name: self.name.as_str().into(),
            kind: self.kind,
            deprecated: false,
            imported_from: None,
            name_range: self.name_range,
            full_range: self.full_range,
        }
    }
}

/// the declarations of `index`, nested as they are written
pub(crate) fn document_symbols(index: &TemplateIndex) -> Vec<TemplateSymbol> {
    let mut definitions: Vec<(SymbolKind, &Definition)> = index
        .blocks()
        .iter()
        .map(|block| (SymbolKind::Module, block))
        .chain(
            index
                .partials()
                .iter()
                .map(|partial| (SymbolKind::Function, partial)),
        )
        .collect();

    // an enclosing declaration starts first, and starts at the same offset as
    // nothing else, so ordering by start alone puts every parent before its
    // children
    definitions.sort_unstable_by_key(|(_, definition)| definition.full_range.start());

    let mut roots = Vec::new();
    let mut open: Vec<TemplateSymbol> = Vec::new();

    for (kind, definition) in definitions {
        while open
            .last()
            .is_some_and(|enclosing| !enclosing.full_range.contains_range(definition.full_range))
        {
            let Some(finished) = open.pop() else { break };
            attach(finished, &mut open, &mut roots);
        }

        open.push(TemplateSymbol {
            name: definition.name.to_string(),
            kind,
            name_range: definition.name_range,
            full_range: definition.full_range,
            children: Vec::new(),
        });
    }

    while let Some(finished) = open.pop() {
        attach(finished, &mut open, &mut roots);
    }

    roots
}

/// one django thing a workspace symbol search can find
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DjangoSymbol {
    pub symbol: SymbolInfo<'static>,
    pub file: File,
    /// what django calls this kind of thing
    ///
    /// a model and an admin class are both classes python has already offered
    /// under the same name, and this is the whole of what tells them apart in a
    /// list of results.
    pub container: &'static str,
}

impl DjangoSymbol {
    fn new(
        name: impl Into<String>,
        kind: SymbolKind,
        container: &'static str,
        file: File,
        name_range: TextRange,
        full_range: TextRange,
    ) -> Self {
        Self {
            symbol: SymbolInfo {
                name: Cow::Owned(name.into()),
                kind,
                deprecated: false,
                imported_from: None,
                name_range,
                full_range,
            },
            file,
            container,
        }
    }
}

/// every django thing of the project whose name matches `query`
///
/// these are *added* to what python contributes rather than folded into it. some
/// of them — a route name, a template's loader name, a `{% partialdef %}` — are
/// names python never had at all. the rest are things python has already offered
/// as a plain class or function, and what this adds is which of them django
/// gives a role to.
///
/// a template's `{% block %}`s are deliberately not here. a block name is chosen
/// against the one template it overrides rather than to be unique in a project,
/// so a search for `content` would answer with one entry per template that
/// declares one — hundreds of identical answers for a name that identifies
/// nothing. a `{% partialdef %}` is the opposite: it exists to be rendered from
/// elsewhere by name, so it is a name worth finding.
pub(crate) fn workspace_symbols(db: &dyn Db, query: &QueryPattern) -> Vec<DjangoSymbol> {
    let project = db.project();
    let mut found = Vec::new();

    for class in project::django_classes(db, project) {
        if !query.is_match_symbol_name(&class.name) {
            continue;
        }
        let (kind, container) = match class.kind {
            DjangoClassKind::Model => (SymbolKind::Class, "django model"),
            DjangoClassKind::Admin => (SymbolKind::Class, "django admin"),
        };
        found.push(DjangoSymbol::new(
            class.name.as_str(),
            kind,
            container,
            class.file,
            class.range,
            class.full_range,
        ));
    }

    // one view answers several routes, and it is one thing however many
    let mut seen = FxHashSet::default();
    for route in project::url_names(db, project) {
        if query.is_match_symbol_name(&route.name) {
            found.push(DjangoSymbol::new(
                route.name.as_str(),
                SymbolKind::Constant,
                "django route",
                route.file,
                route.range,
                route.range,
            ));
        }

        if let Some(view) = route.view.as_ref().map(|view| &view.target)
            && query.is_match_symbol_name(&view.name)
            && seen.insert((view.file, view.range))
        {
            found.push(DjangoSymbol::new(
                view.name.as_str(),
                match view.kind {
                    TargetKind::Class => SymbolKind::Class,
                    TargetKind::Function => SymbolKind::Function,
                },
                "django view",
                view.file,
                view.range,
                view.full_range,
            ));
        }
    }

    for registration in project::registrations(db, project) {
        // django's own `{% for %}` and `|upper` are the language rather than
        // anything this project is made of
        if registration.django || !query.is_match_symbol_name(&registration.name) {
            continue;
        }
        let container = match registration.kind {
            RegistrationKind::Tag { .. } => "django tag",
            RegistrationKind::Filter => "django filter",
        };
        found.push(DjangoSymbol::new(
            registration.name.as_str(),
            SymbolKind::Function,
            container,
            registration.file,
            registration.range,
            registration.range,
        ));
    }

    for template in project::template_files(db, project) {
        // an installed app's templates are django's to render and nobody's to
        // search for
        if !template.own {
            continue;
        }
        let Ok(file) = system_path_to_file(db, &template.path) else {
            continue;
        };

        if query.is_match_symbol_name(&template.name) {
            found.push(DjangoSymbol::new(
                template.name.as_str(),
                SymbolKind::Module,
                "django template",
                file,
                TextRange::default(),
                TextRange::default(),
            ));
        }

        for partial in template_index(db, file).partials() {
            if !query.is_match_symbol_name(&partial.name) {
                continue;
            }
            found.push(DjangoSymbol::new(
                partial.name.as_str(),
                SymbolKind::Function,
                "django partial",
                file,
                partial.name_range,
                partial.full_range,
            ));
        }
    }

    found
}

/// hand a finished symbol to the declaration enclosing it, or to the outline
fn attach(symbol: TemplateSymbol, open: &mut [TemplateSymbol], roots: &mut Vec<TemplateSymbol>) {
    match open.last_mut() {
        Some(enclosing) => enclosing.children.push(symbol),
        None => roots.push(symbol),
    }
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::{DJANGO_ADMIN, TemplateTest};

    fn template(source: &str) -> TemplateTest {
        TemplateTest::new(&[("blog/templates/blog/post.html", source)])
    }

    #[test]
    fn a_template_outlines_its_blocks() {
        let test = template(
            "<CURSOR>{% block content %}a{% endblock %}\n{% block footer %}b{% endblock %}",
        );
        assert_eq!(test.symbols(), ["Module content", "Module footer"]);
    }

    #[test]
    fn a_block_inside_a_block_nests_under_it() {
        let test =
            template("<CURSOR>{% block content %}{% block inner %}a{% endblock %}{% endblock %}");
        assert_eq!(test.symbols(), ["Module content", "  Module inner"]);
    }

    #[test]
    fn a_partialdef_is_outlined_too() {
        let test = template("<CURSOR>{% partialdef card %}a{% endpartialdef %}");
        assert_eq!(test.symbols(), ["Function card"]);
    }

    #[test]
    fn a_partialdef_inside_a_block_nests_under_it() {
        let test = template(
            "<CURSOR>{% block content %}{% partialdef card %}a{% endpartialdef %}{% endblock %}",
        );
        assert_eq!(test.symbols(), ["Module content", "  Function card"]);
    }

    #[test]
    fn a_block_that_was_never_closed_is_still_outlined() {
        let test = template("<CURSOR>{% block content %}a");
        assert_eq!(test.symbols(), ["Module content"]);
    }

    #[test]
    fn a_template_with_no_declarations_has_no_outline() {
        assert!(
            template("<CURSOR><p>{{ book.title }}</p>")
                .symbols()
                .is_empty()
        );
    }

    // ---- the project's django names, for a workspace search ---------------

    /// a whole django project: models, an admin module, views, a url tree, a tag
    /// library and templates, with django installed beside it
    fn workspace() -> TemplateTest {
        TemplateTest::with_site_packages(
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
                    INSTALLED_APPS = ['blog']

                    TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]

                    ROOT_URLCONF = 'project.urls'
                    ",
                ),
                (
                    "project/urls.py",
                    "
                    from django.urls import include, path

                    urlpatterns = [path('blog/', include('blog.urls'))]
                    ",
                ),
                ("blog/__init__.py", ""),
                (
                    "blog/models.py",
                    "
                    from django.db import models


                    class Book(models.Model): ...
                    ",
                ),
                (
                    "blog/admin.py",
                    "
                    from django.contrib import admin

                    from blog.models import Book


                    @admin.register(Book)
                    class BookAdmin(admin.ModelAdmin): ...
                    ",
                ),
                (
                    "blog/views.py",
                    "
                    def listing(request): ...


                    class BookDetail: ...
                    ",
                ),
                (
                    "blog/urls.py",
                    "
                    from django.urls import path

                    from blog import views

                    app_name = 'blog'

                    urlpatterns = [
                        path('', views.listing, name='index'),
                        path('<int:pk>/', views.BookDetail.as_view(), name='detail'),
                    ]
                    ",
                ),
                ("blog/templatetags/__init__.py", ""),
                (
                    "blog/templatetags/blog_extras.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.filter
                    def shout(value):
                        return value

                    @register.simple_tag
                    def badge():
                        return ''
                    ",
                ),
                (
                    "blog/templates/blog/post.html",
                    "{% partialdef card %}a{% endpartialdef %}{% block content %}b{% endblock %}",
                ),
            ],
            DJANGO_ADMIN,
        )
    }

    #[test]
    fn a_model_is_found_as_one_beside_the_class_python_offers() {
        assert_eq!(
            workspace().workspace_symbols("Book"),
            [
                "django admin BookAdmin [/src/blog/admin.py]",
                "django model Book [/src/blog/models.py]",
                "django view BookDetail [/src/blog/views.py]",
                "python Book [/src/blog/models.py]",
                "python BookAdmin [/src/blog/admin.py]",
                "python BookDetail [/src/blog/views.py]",
            ]
        );
    }

    #[test]
    fn a_route_is_found_by_the_name_the_url_tree_gives_it() {
        assert_eq!(
            workspace().workspace_symbols("blog:detail"),
            ["django route blog:detail [/src/blog/urls.py]"]
        );
    }

    #[test]
    fn a_view_is_found_by_its_own_name() {
        assert_eq!(
            workspace().workspace_symbols("listing"),
            [
                "django view listing [/src/blog/views.py]",
                "python listing [/src/blog/views.py]",
            ]
        );
    }

    #[test]
    fn a_template_is_found_by_the_name_the_loader_uses() {
        assert_eq!(
            workspace().workspace_symbols("blog/post"),
            ["django template blog/post.html [/src/blog/templates/blog/post.html]"]
        );
    }

    #[test]
    fn a_partial_is_found_by_its_name() {
        assert_eq!(
            workspace().workspace_symbols("card"),
            ["django partial card [/src/blog/templates/blog/post.html]"]
        );
    }

    #[test]
    fn a_block_is_not_a_workspace_symbol() {
        // see `workspace_symbols`: a project has one `content` per template, and
        // a name that identifies nothing is worth nothing to a search
        assert_eq!(
            workspace().workspace_symbols("content"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_tag_and_a_filter_are_found_by_the_names_a_template_writes() {
        assert_eq!(
            workspace().workspace_symbols("badge"),
            [
                "django tag badge [/src/blog/templatetags/blog_extras.py]",
                "python badge [/src/blog/templatetags/blog_extras.py]",
            ]
        );
        assert_eq!(
            workspace().workspace_symbols("shout"),
            [
                "django filter shout [/src/blog/templatetags/blog_extras.py]",
                "python shout [/src/blog/templatetags/blog_extras.py]",
            ]
        );
    }

    #[test]
    fn a_project_that_is_no_django_project_answers_exactly_what_python_does() {
        let test = TemplateTest::new(&[("app.py", "class Book:\n    pass\n")]);

        assert_eq!(test.workspace_symbols("Book"), ["python Book [/app.py]"]);
    }
}
