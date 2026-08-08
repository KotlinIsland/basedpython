//! find-all-references for the django names written in two languages
//!
//! this is the direction the rest of the template services do not go. goto takes
//! a name to the one place it is declared; this takes a declaration to every
//! place it is used — which is the question you actually have when you are
//! editing a base template and want to know which children override the block
//! under the cursor, or whether anything still renders the file you are looking
//! at.
//!
//! the scans are [`super::uses`]', shared with the rename that has to find the
//! same occurrences. what this adds is the opposite temperament: a rename refuses
//! over a use it could not rewrite, and this *reports* one, since knowing where a
//! name is mentioned is useful whether or not a rewrite could reach it. the one
//! thing left out is a name worked out at run time, which is no occurrence of
//! this name so much as a position that might be one.

use ruff_db::files::File;
use ruff_text_size::TextSize;
use ty_project::Db;

use crate::{ReferenceKind, ReferenceTarget};

use super::resolve;
use super::uses::{self, Anchor, Named, Use, Written};

/// every place the django name at `offset` of `file` is written
///
/// `template` says the file is a django template rather than python, which the
/// caller knows and this cannot. a position in a template that names nothing in
/// particular is answered for the template *file*: "what renders this" is the
/// other question worth asking of a template, and a request anywhere in one is a
/// reasonable way to ask it.
pub(super) fn references(
    db: &dyn Db,
    file: File,
    offset: TextSize,
    include_declaration: bool,
    template: bool,
) -> Option<Vec<ReferenceTarget>> {
    let named = uses::named_at(db, file, offset, template);

    let mut found = match &named {
        Some((Named::Block(name), _)) => uses::block(db, file, name).found,
        Some((Named::Route(name), _)) => {
            uses::route(db, &Anchor::Use { name: name.clone() })
                .uses
                .found
        }
        Some((Named::RouteDeclaration, range)) => {
            uses::route(
                db,
                &Anchor::Declaration {
                    file,
                    range: *range,
                },
            )
            .uses
            .found
        }
        Some((Named::Template(name), _)) => uses::template(db, name).uses.found,
        Some((Named::Tag(name), _)) => uses::registration(db, name, false).found,
        Some((Named::Filter(name), _)) => uses::registration(db, name, true).found,
        None => Vec::new(),
    };

    // a builtin tag is registered nowhere and written everywhere, so it finds
    // nothing — and a position that names nothing at all finds nothing either.
    // both leave the file itself as what the request is about
    if found.is_empty() {
        if !template {
            return None;
        }
        found = uses::template(db, &resolve::template_name(db, file)?)
            .uses
            .found;
    }

    Some(targets(db, found, include_declaration))
}

/// the uses an editor is shown, in a stable order
fn targets(db: &dyn Db, mut found: Vec<Use>, include_declaration: bool) -> Vec<ReferenceTarget> {
    uses::sort(db, &mut found);
    found.dedup_by(|left, right| (left.file, left.range) == (right.file, right.range));

    found
        .into_iter()
        // a position that works its name out at run time writes no name to
        // report. everything else does, whether or not a rename could rewrite it
        .filter(|used| !matches!(used.written, Written::Unknown))
        .filter(|used| include_declaration || !used.declaration)
        .map(|used| {
            let kind = if used.declaration {
                ReferenceKind::Other
            } else {
                ReferenceKind::Read
            };

            ReferenceTarget::new(used.file, used.range, kind)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::TemplateTest;

    /// a whole django project, with `sources` written into it
    fn project(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = vec![
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
                "blog/urls.py",
                "
                from django.urls import path

                app_name = 'blog'

                urlpatterns = [path('<int:pk>/', detail, name='detail')]
                ",
            ),
        ];
        all.extend_from_slice(sources);

        TemplateTest::with_site_packages(&all, &[])
    }

    // -----------------------------------------------------------------------
    // a `{% block %}` name
    // -----------------------------------------------------------------------

    /// `base` and three children, two overriding `content` and one `footer`
    fn family(base: &str, cursor: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = vec![
            ("blog/templates/blog/base.html", base),
            (
                "blog/templates/blog/post.html",
                "{% extends 'blog/base.html' %}{% block content %}a{% endblock content %}",
            ),
            (
                "blog/templates/blog/list.html",
                "{% extends 'blog/base.html' %}{% block content %}b{% endblock %}",
            ),
            (
                "blog/templates/blog/sidebar.html",
                "{% extends 'blog/base.html' %}{% block footer %}c{% endblock %}",
            ),
        ];
        all.extend_from_slice(cursor);

        project(&all)
    }

    #[test]
    fn a_block_in_the_base_finds_every_child_that_overrides_it() {
        let test = family(
            "{% block co<CURSOR>ntent %}{% endblock %}{% block footer %}{% endblock %}",
            &[],
        );

        assert_eq!(
            test.references(),
            [
                "declaration /src/blog/templates/blog/base.html:1 content",
                "/src/blog/templates/blog/list.html:1 content",
                "/src/blog/templates/blog/post.html:1 content",
                "/src/blog/templates/blog/post.html:1 content",
            ]
        );
    }

    #[test]
    fn a_block_in_a_child_finds_the_base_and_its_siblings() {
        let test = family(
            "{% block content %}{% endblock %}{% block footer %}{% endblock %}",
            &[(
                "blog/templates/blog/edit.html",
                "{% extends 'blog/base.html' %}{% block co<CURSOR>ntent %}d{% endblock %}",
            )],
        );
        let found = test.references();

        assert!(
            found.contains(&"declaration /src/blog/templates/blog/base.html:1 content".to_string()),
            "got {found:?}"
        );
        assert!(
            found.contains(&"/src/blog/templates/blog/list.html:1 content".to_string()),
            "got {found:?}"
        );
        assert!(
            !found.iter().any(|target| target.contains("sidebar")),
            "a sibling overriding another block is no use of this one: got {found:?}"
        );
    }

    #[test]
    fn leaving_the_declaration_out_leaves_the_base_out() {
        let test = family("{% block co<CURSOR>ntent %}{% endblock %}", &[]);

        assert_eq!(
            test.references_without_declaration(),
            [
                "/src/blog/templates/blog/list.html:1 content",
                "/src/blog/templates/blog/post.html:1 content",
                "/src/blog/templates/blog/post.html:1 content",
            ]
        );
    }

    #[test]
    fn a_block_is_still_found_where_a_rename_would_refuse() {
        // an installed app's template overriding the block refuses the rename,
        // since it is not the project's to rewrite — but it is exactly what
        // someone editing the base wants to be shown
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
                    INSTALLED_APPS = ['blog', 'widgets']

                    TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]
                    ",
                ),
                ("blog/__init__.py", ""),
                (
                    "blog/templates/blog/base.html",
                    "{% block co<CURSOR>ntent %}{% endblock %}",
                ),
            ],
            &[
                ("widgets/__init__.py", ""),
                (
                    "widgets/templates/widgets/panel.html",
                    "{% extends 'blog/base.html' %}{% block content %}x{% endblock %}",
                ),
            ],
        );

        assert_eq!(
            test.prepare_rename(),
            "refused: `/site-packages/widgets/templates/widgets/panel.html` declares this block \
             too, and an installed app's template is not the project's to rewrite"
        );
        assert_eq!(
            test.references(),
            [
                "/site-packages/widgets/templates/widgets/panel.html:1 content",
                "declaration /src/blog/templates/blog/base.html:1 content",
            ]
        );
    }

    // -----------------------------------------------------------------------
    // a template
    // -----------------------------------------------------------------------

    /// the same project, with a view and a template that several things load
    fn loaded(post: &str) -> TemplateTest {
        project(&[
            (
                "blog/views.py",
                "
                from django.shortcuts import render


                def post(request):
                    return render(request, 'blog/post.html', {})


                class PostView:
                    template_name = 'blog/post.html'
                ",
            ),
            (
                "blog/templates/blog/list.html",
                "{% include 'blog/post.html' %}",
            ),
            ("blog/templates/blog/post.html", post),
        ])
    }

    #[test]
    fn a_template_is_found_from_a_position_that_names_nothing_in_it() {
        let test = loaded("<p>a p<CURSOR>ost</p>");

        assert_eq!(
            test.references(),
            [
                "/src/blog/templates/blog/list.html:1 blog/post.html",
                "declaration /src/blog/templates/blog/post.html:1 ",
                "/src/blog/views.py:6 blog/post.html",
                "/src/blog/views.py:10 blog/post.html",
            ]
        );
    }

    #[test]
    fn a_template_named_in_an_include_is_found_from_the_name_itself() {
        let test = project(&[
            (
                "blog/templates/blog/list.html",
                "{% include 'blog/po<CURSOR>st.html' %}",
            ),
            ("blog/templates/blog/post.html", "x"),
        ]);

        assert_eq!(
            test.references_without_declaration(),
            ["/src/blog/templates/blog/list.html:1 blog/post.html"]
        );
    }

    #[test]
    fn a_template_is_found_from_the_render_that_names_it_in_python() {
        let test = loaded("x");

        assert_eq!(
            test.references(),
            [
                "/src/blog/templates/blog/list.html:1 blog/post.html",
                "declaration /src/blog/templates/blog/post.html:1 ",
                "/src/blog/views.py:6 blog/post.html",
                "/src/blog/views.py:10 blog/post.html",
            ]
        );
    }

    #[test]
    fn a_constant_holding_a_templates_name_is_reported_rather_than_refused() {
        // a rename stops at a constant, since it cannot follow where one carries
        // the name — which is the very reason to show it
        let test = project(&[
            (
                "blog/tasks.py",
                "
                WELCOME = 'blog/post.html'


                def send(request):
                    return render(request, WELCOME, {})
                ",
            ),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
            ("blog/templates/blog/post.html", "x"),
        ]);

        assert!(
            test.prepare_rename()
                .contains("binds this name to `WELCOME`"),
            "got {}",
            test.prepare_rename()
        );
        assert_eq!(
            test.references_without_declaration(),
            [
                "/src/blog/tasks.py:2 blog/post.html",
                "/src/blog/templates/blog/child.html:1 blog/post.html",
            ]
        );
    }

    #[test]
    fn a_template_nothing_loads_is_found_only_as_itself() {
        let test = project(&[("blog/templates/blog/post.html", "<p>a p<CURSOR>ost</p>")]);

        assert_eq!(
            test.references(),
            ["declaration /src/blog/templates/blog/post.html:1 "]
        );
        assert!(test.references_without_declaration().is_empty());
    }

    // -----------------------------------------------------------------------
    // a route name
    // -----------------------------------------------------------------------

    /// the same project, with a route reversed from both languages
    fn reversed(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = vec![
            (
                "blog/views.py",
                "
                from django.urls import reverse, reverse_lazy


                def go(request):
                    reverse('blog:detail')
                    reverse_lazy('blog:detail')
                    return redirect('blog:detail')
                ",
            ),
            (
                "blog/templates/blog/nav.html",
                "{% url 'blog:detail' pk=1 %}",
            ),
        ];
        all.extend_from_slice(sources);

        project(&all)
    }

    #[test]
    fn a_route_is_found_from_a_url_tag_in_both_languages() {
        let test = reversed(&[(
            "blog/templates/blog/post.html",
            "{% url 'blog:de<CURSOR>tail' %}",
        )]);

        assert_eq!(
            test.references(),
            [
                "/src/blog/templates/blog/nav.html:1 blog:detail",
                "/src/blog/templates/blog/post.html:1 blog:detail",
                "declaration /src/blog/urls.py:6 detail",
                "/src/blog/views.py:6 blog:detail",
                "/src/blog/views.py:7 blog:detail",
                "/src/blog/views.py:8 blog:detail",
            ]
        );
    }

    #[test]
    fn a_route_is_found_from_its_own_declaration() {
        let test = project(&[
            (
                "blog/urls.py",
                "
                from django.urls import path

                app_name = 'blog'

                urlpatterns = [path('<int:pk>/', detail, name='de<CURSOR>tail')]
                ",
            ),
            (
                "blog/templates/blog/nav.html",
                "{% url 'blog:detail' pk=1 %}",
            ),
        ]);

        assert_eq!(
            test.references_without_declaration(),
            ["/src/blog/templates/blog/nav.html:1 blog:detail"]
        );
    }

    #[test]
    fn a_route_is_found_from_a_reverse_in_python() {
        let test = project(&[
            (
                "blog/templates/blog/nav.html",
                "{% url 'blog:detail' pk=1 %}",
            ),
            (
                "blog/views.py",
                "
                from django.urls import reverse


                def go(request):
                    return reverse('blog:de<CURSOR>tail')
                ",
            ),
        ]);

        assert_eq!(
            test.references(),
            [
                "/src/blog/templates/blog/nav.html:1 blog:detail",
                "declaration /src/blog/urls.py:6 detail",
                "/src/blog/views.py:6 blog:detail",
            ]
        );
    }

    #[test]
    fn a_route_reversed_at_run_time_is_no_use_of_this_one() {
        // it names *some* route, and a rename refuses over exactly that — but a
        // position that spells no name is no place this name is mentioned
        let test = reversed(&[
            (
                "blog/report.py",
                "
                from django.urls import reverse


                def go(request, wanted):
                    return reverse(wanted)
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        assert!(
            test.prepare_rename().contains("worked out at run time"),
            "got {}",
            test.prepare_rename()
        );
        assert!(
            !test
                .references()
                .iter()
                .any(|target| target.contains("report.py")),
            "got {:?}",
            test.references()
        );
    }

    // -----------------------------------------------------------------------
    // a tag or filter name
    // -----------------------------------------------------------------------

    /// the same project with a tag library, and a template using both of its
    /// registrations
    fn library(post: &str) -> TemplateTest {
        project(&[
            (
                "blog/templatetags/blog_extras.py",
                "
                from django import template

                register = template.Library()


                @register.filter
                def shout(value):
                    return value


                @register.simple_tag
                def book_count():
                    return 0
                ",
            ),
            (
                "blog/templates/blog/nav.html",
                "{% load blog_extras %}{% book_count %}{{ x|shout }}",
            ),
            ("blog/templates/blog/post.html", post),
        ])
    }

    #[test]
    fn a_tag_is_found_in_every_template_that_uses_it() {
        let test = library("{% load blog_extras %}{% book_c<CURSOR>ount %}");

        assert_eq!(
            test.references(),
            [
                "/src/blog/templates/blog/nav.html:1 book_count",
                "/src/blog/templates/blog/post.html:1 book_count",
                "declaration /src/blog/templatetags/blog_extras.py:13 book_count",
            ]
        );
    }

    #[test]
    fn a_filter_is_found_in_every_template_that_uses_it() {
        let test = library("{% load blog_extras %}{{ y|sho<CURSOR>ut }}");

        assert_eq!(
            test.references_without_declaration(),
            [
                "/src/blog/templates/blog/nav.html:1 shout",
                "/src/blog/templates/blog/post.html:1 shout",
            ]
        );
    }

    #[test]
    fn a_builtin_tag_answers_for_the_file_it_is_written_in() {
        // nothing registers `{% if %}`, and every `{% if %}` in a project is no
        // answer to any question — so the file itself is what is left to answer for
        let test = library("{% i<CURSOR>f x %}{% endif %}");

        assert_eq!(
            test.references(),
            ["declaration /src/blog/templates/blog/post.html:1 "]
        );
    }
}
