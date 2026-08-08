//! inlay hints for django templates
//!
//! a template is short on nouns. `{% for book in shelf %}` never says what a
//! `book` is, and `{% include "card.html" %}` never says which of the several
//! `card.html`s in the project django will load — both are answers this module
//! already computes for hover and goto, and an inlay hint is what puts them where
//! the reader is looking rather than where the pointer is.

use ruff_db::files::File;
use ruff_text_size::{TextRange, TextSize};
use ty_project::Db;

use crate::InlayHintSettings;

use super::index::{BindingOrigin, TemplateIndex, TemplateReference};
use super::lexer::TokenKind;
use super::project;
use super::resolve;

/// a hint written into a template between what the template itself says
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateInlayHint {
    pub position: TextSize,
    pub label: String,
    pub kind: TemplateInlayHintKind,
}

/// what a template hint says
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateInlayHintKind {
    /// the type of a name the template binds
    Type,
    /// the file a template name resolves to
    Template,
}

/// every hint `range` of the template `file` shows
pub(crate) fn inlay_hints(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    range: TextRange,
    settings: &InlayHintSettings,
) -> Vec<TemplateInlayHint> {
    let mut hints = Vec::new();

    if settings.template_binding_types {
        binding_types(db, file, index, source, range, &mut hints);
    }

    if settings.resolved_templates {
        for reference in index.extends().into_iter().chain(index.includes()) {
            resolved_template(db, index, range, reference, &mut hints);
        }
    }

    hints.sort_unstable_by_key(|hint| hint.position);
    hints
}

/// the element type each `{% for %}` binding takes
fn binding_types(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    range: TextRange,
    hints: &mut Vec<TemplateInlayHint>,
) {
    for binding in index.bindings() {
        if binding.origin != BindingOrigin::LoopVariable
            || !range.contains_inclusive(binding.range.end())
        {
            continue;
        }

        // `{% for key, value in mapping.items %}` gives each target the *same*
        // element type, because unpacking one element across several names is
        // not something the resolution models. one wrong type written into the
        // source is worse than the two the template already leaves unsaid
        if index
            .bindings()
            .iter()
            .filter(|candidate| {
                candidate.origin == BindingOrigin::LoopVariable && candidate.scope == binding.scope
            })
            .count()
            > 1
        {
            continue;
        }

        let Some(ty) = resolve::path_type(
            db,
            file,
            index,
            source,
            binding.scope.start(),
            &[&binding.name],
        ) else {
            continue;
        };

        hints.push(TemplateInlayHint {
            position: binding.range.end(),
            label: format!(": {}", ty.display(db)),
            kind: TemplateInlayHintKind::Type,
        });
    }
}

/// the file an `{% extends %}` or an `{% include %}` name resolves to
fn resolved_template(
    db: &dyn Db,
    index: &TemplateIndex,
    range: TextRange,
    reference: &TemplateReference,
    hints: &mut Vec<TemplateInlayHint>,
) {
    // the reference's range is the literal's contents; the hint belongs after the
    // literal itself, closing quote included
    let Some(literal) = index.lexed().tokens().iter().find(|token| {
        token.kind == TokenKind::String && token.range.contains_range(reference.range)
    }) else {
        return;
    };

    if !range.contains_inclusive(literal.range.end()) {
        return;
    }

    let Some(target) = project::resolve_template(db, &reference.name) else {
        return;
    };
    let Some(path) = path_label(db, target) else {
        return;
    };

    hints.push(TemplateInlayHint {
        position: literal.range.end(),
        label: format!(" → {path}"),
        kind: TemplateInlayHintKind::Template,
    });
}

/// how a resolved template reads: its path within the project, since what the
/// hint is for is telling two same-named templates in two apps apart
fn path_label(db: &dyn Db, file: File) -> Option<String> {
    let path = file.path(db).as_system_path()?;

    Some(
        path.strip_prefix(db.project().root(db))
            .unwrap_or(path)
            .as_str()
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::TemplateTest;

    /// a project whose view puts a list of books in a template's context
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
                    return render(request, 'blog/post.html', {'shelf': [Book()]})
                ",
            ),
            (
                "blog/templates/blog/card.html",
                "{% block card %}{% endblock %}",
            ),
            ("blog/templates/blog/post.html", template),
        ])
    }

    #[test]
    fn a_loop_over_a_known_iterable_shows_the_element_type() {
        assert_eq!(
            project("{% for book in shelf %}{{ book.title }}{% endfor %}").hints(),
            ["Type at `book`: `: Book`"]
        );
    }

    #[test]
    fn a_loop_over_an_unknown_iterable_shows_nothing() {
        assert_eq!(
            project("{% for book in mystery %}{{ book }}{% endfor %}").hints(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_loop_that_unpacks_shows_nothing() {
        assert_eq!(
            project("{% for a, b in shelf %}{{ a }}{% endfor %}").hints(),
            Vec::<String>::new(),
            "one element type across two names would be wrong for both"
        );
    }

    #[test]
    fn an_include_shows_the_file_it_resolves_to() {
        assert_eq!(
            project("{% include 'blog/card.html' %}").hints(),
            ["Template at `'blog/card.html'`: ` → blog/templates/blog/card.html`"]
        );
    }

    #[test]
    fn an_extends_shows_the_file_it_resolves_to() {
        assert_eq!(
            project("{% extends 'blog/card.html' %}").hints(),
            ["Template at `'blog/card.html'`: ` → blog/templates/blog/card.html`"]
        );
    }

    #[test]
    fn an_include_that_resolves_to_nothing_shows_nothing() {
        assert_eq!(
            project("{% include 'blog/missing.html' %}").hints(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_hint_outside_the_requested_range_is_left_out() {
        let test = project("{% include 'blog/card.html' %}\n{% for book in shelf %}{% endfor %}");

        assert_eq!(
            test.hints_in(0..31),
            ["Template at `'blog/card.html'`: ` → blog/templates/blog/card.html`"]
        );
    }

    #[test]
    fn a_setting_that_is_off_shows_none_of_its_kind() {
        let test = project("{% include 'blog/card.html' %}{% for book in shelf %}{% endfor %}");

        assert_eq!(
            test.hints_with(|settings| settings.template_binding_types = true),
            ["Type at `book`: `: Book`"]
        );
        assert_eq!(
            test.hints_with(|settings| settings.resolved_templates = true),
            ["Template at `'blog/card.html'`: ` → blog/templates/blog/card.html`"]
        );
    }
}
