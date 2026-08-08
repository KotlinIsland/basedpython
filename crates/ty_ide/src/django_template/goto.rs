//! go-to-definition for django templates
//!
//! nearly every name a template writes points somewhere: a `{% extends %}` at
//! another template, a `{% partial %}` at the `{% partialdef %}` that declares it,
//! a `{% url %}` at the `path(…, name=…)` that names the route, a tag or filter at
//! the python function registered under it, and a `{{ variable }}` at the view
//! that put it in the context.

use ruff_db::files::{File, FileRange};
use ruff_text_size::{TextRange, TextSize};
use ty_project::Db;

use crate::{HasNavigationTargets, NavigationTarget, NavigationTargets, RangedValue};

use super::index::TemplateIndex;
use super::lexer::{ConstructKind, Token, TokenKind, string_contents};
use super::project::{self, RegistrationKind};
use super::resolve::{self, Origin};

/// where the name at `offset` of the template `file` is defined
pub(crate) fn goto_definition(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
) -> Option<RangedValue<NavigationTargets>> {
    let construct = index.lexed().construct_at(offset)?;
    if construct.kind == ConstructKind::Comment {
        return None;
    }

    let tokens = index.lexed().construct_tokens(construct);
    let token = *tokens
        .iter()
        .find(|token| token.range.contains_inclusive(offset))?;

    let site = Site {
        db,
        file,
        index,
        source,
        offset,
        tag: construct.name.map_or("", |range| &source[range]),
        tokens,
        token,
    };

    let targets = site.definition()?;

    (!targets.is_empty()).then(|| RangedValue {
        range: FileRange::new(file, token.range),
        value: targets,
    })
}

/// the one token a navigation starts from, with everything needed to interpret it
struct Site<'a> {
    db: &'a dyn Db,
    file: File,
    index: &'a TemplateIndex,
    source: &'a str,
    offset: TextSize,
    /// the name of the tag the token sits in, or `""` outside a tag
    tag: &'a str,
    /// the tokens of the construct the token sits in
    tokens: &'a [Token],
    token: Token,
}

impl Site<'_> {
    fn text(&self) -> &str {
        &self.source[self.token.range]
    }

    fn definition(&self) -> Option<NavigationTargets> {
        match self.token.kind {
            TokenKind::TagName => Some(self.registration(false)),
            TokenKind::FilterName => Some(self.registration(true)),
            TokenKind::String => self.string_definition(),
            TokenKind::Variable => self.name_definition(),
            TokenKind::Attribute => {
                // the attribute itself has no template-visible declaration, so
                // this navigates to the type the attribute is read off: for a
                // django model field that is the model class
                let segments = path_up_to(self.source, self.tokens, self.token, false);
                let ty = resolve::path_type(
                    self.db,
                    self.file,
                    self.index,
                    self.source,
                    self.offset,
                    &segments,
                )?;
                Some(ty.navigation_targets(self.db))
            }
            _ => None,
        }
    }

    /// the python function the token's name is registered by
    fn registration(&self, filter: bool) -> NavigationTargets {
        let name = self.text();

        project::registrations(self.db, self.db.project())
            .iter()
            .filter(|registration| {
                (registration.kind == RegistrationKind::Filter) == filter
                    && registration.name == name
            })
            .map(|registration| NavigationTarget::new(registration.file, registration.range))
            .collect()
    }

    /// what a string argument names, which depends on the tag it sits in
    fn string_definition(&self) -> Option<NavigationTargets> {
        let db = self.db;
        let value = string_contents(self.source, self.token.range);

        match self.tag {
            "extends" | "include" => {
                let reference = self
                    .index
                    .extends()
                    .into_iter()
                    .chain(self.index.includes())
                    .find(|reference| reference.range == value)?;

                let target = project::resolve_template(db, &reference.name)?;
                let range = match &reference.partial {
                    // `blog.html#comment-item` addresses one fragment of it. a
                    // fragment that isn't there is no reason to refuse to open
                    // the template — that is where the user is going to look
                    Some(partial) => super::template_index(db, target)
                        .partials()
                        .iter()
                        .find(|candidate| candidate.name == *partial)
                        .map_or_else(TextRange::default, |candidate| candidate.name_range),
                    None => TextRange::default(),
                };

                Some(NavigationTargets::from_iter([NavigationTarget::new(
                    target, range,
                )]))
            }
            "url" => {
                let name = &self.source[value];
                Some(
                    project::url_names(db, db.project())
                        .iter()
                        .filter(|url| url.name == name)
                        .map(|url| NavigationTarget::new(url.file, url.range))
                        .collect(),
                )
            }
            _ => None,
        }
    }

    /// what a bare name refers to, which again depends on the tag it sits in
    fn name_definition(&self) -> Option<NavigationTargets> {
        let (db, file, index) = (self.db, self.file, self.index);
        let text = self.text();

        match self.tag {
            // `{% load static %}` points at the module the library lives in
            "load" => Some(
                project::registrations(db, db.project())
                    .iter()
                    .filter(|registration| registration.library == text)
                    .map(|registration| {
                        NavigationTarget::new(registration.file, TextRange::default())
                    })
                    .collect(),
            ),
            // `{% partial card %}` points at the `{% partialdef card %}`, which
            // this template may well declare itself
            "partial" => definition_in_chain(db, file, index, text, false, TemplateIndex::partials),
            // a `{% block %}` *is* a definition, so navigating from one is only
            // useful if it goes to the block in the parent that it overrides
            "block" => definition_in_chain(db, file, index, text, true, TemplateIndex::blocks),
            _ => {
                // a name in a path resolves through the path's leading segment
                let segments = path_up_to(self.source, self.tokens, self.token, true);
                let root = segments.first()?;
                if *root != text {
                    // the name is not the path's root, so it is an attribute the
                    // `Attribute` arm would have handled had it lexed as one
                    return None;
                }

                match resolve::resolve_root(db, file, index, self.offset, root)? {
                    Origin::Binding(binding) => {
                        Some(NavigationTargets::from_iter([NavigationTarget::new(
                            file,
                            binding.range,
                        )]))
                    }
                    Origin::Context(variable) => {
                        Some(NavigationTargets::from_iter([NavigationTarget::new(
                            variable.file,
                            variable.range,
                        )]))
                    }
                }
            }
        }
    }
}

/// the definition named `name`, looked for in this template and then up its
/// `{% extends %}` chain
///
/// `skip_current` starts the search at the parent, for a name whose occurrence in
/// this template is itself the definition.
fn definition_in_chain(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    name: &str,
    skip_current: bool,
    definitions: impl Fn(&TemplateIndex) -> &[super::index::Definition],
) -> Option<NavigationTargets> {
    let current = (!skip_current).then_some((file, index));

    current
        .into_iter()
        .chain(super::ancestors(db, file, index))
        .find_map(|(file, index)| {
            let definition = definitions(index)
                .iter()
                .find(|definition| definition.name == name)?;

            Some(NavigationTargets::from_iter([NavigationTarget::new(
                file,
                definition.name_range,
            )]))
        })
}

/// the path segments up to and including `token`
///
/// `inclusive` keeps `token` itself, for a name whose own type is wanted; leaving
/// it out gives the type the name is an attribute *of*.
fn path_up_to<'src>(
    source: &'src str,
    tokens: &[Token],
    token: Token,
    inclusive: bool,
) -> Vec<&'src str> {
    let Some(position) = tokens
        .iter()
        .position(|candidate| candidate.range == token.range)
    else {
        return Vec::new();
    };

    // walk back to the path's leading name
    let mut start = position;
    while start > 0 {
        let previous = start - 1;
        if tokens.get(previous).is_none_or(|candidate| {
            candidate.kind != TokenKind::Operator || source[candidate.range] != *"."
        }) {
            break;
        }
        let Some(segment) = previous.checked_sub(1) else {
            break;
        };
        if tokens.get(segment).is_none_or(|candidate| {
            !matches!(candidate.kind, TokenKind::Variable | TokenKind::Attribute)
        }) {
            break;
        }
        start = segment;
    }

    let end = if inclusive { position + 1 } else { position };
    tokens
        .get(start..end.max(start))
        .unwrap_or_default()
        .iter()
        .filter(|candidate| matches!(candidate.kind, TokenKind::Variable | TokenKind::Attribute))
        .map(|candidate| &source[candidate.range])
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::TemplateTest;

    /// the same small django project the completion tests use
    fn project(template: &str) -> TemplateTest {
        TemplateTest::new(&[
            (
                "blog/models.py",
                "
                class Author:
                    name: str

                class Book:
                    title: str
                    author: Author
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
                "blog/urls.py",
                "
                app_name = 'blog'

                urlpatterns = [path('books/', index, name='index')]
                ",
            ),
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
                "blog/templates/blog/base.html",
                "{% block content %}{% endblock %}",
            ),
            ("blog/templates/blog/post.html", template),
        ])
    }

    #[test]
    fn extends_navigates_to_the_template_it_names() {
        let definitions = project("{% extends 'blog/b<CURSOR>ase.html' %}").definitions();
        assert_eq!(definitions, ["/blog/templates/blog/base.html:"]);
    }

    #[test]
    fn include_navigates_to_the_template_it_names() {
        let definitions = project("{% include 'blog/<CURSOR>base.html' %}").definitions();
        assert_eq!(definitions, ["/blog/templates/blog/base.html:"]);
    }

    #[test]
    fn an_include_with_a_fragment_navigates_to_the_partial_inside_it() {
        let test = TemplateTest::new(&[
            (
                "blog/templates/blog/cards.html",
                "{% partialdef card %}x{% endpartialdef %}",
            ),
            (
                "blog/templates/blog/post.html",
                "{% include 'blog/car<CURSOR>ds.html#card' %}",
            ),
        ]);

        assert_eq!(test.definitions(), ["/blog/templates/blog/cards.html:card"]);
    }

    #[test]
    fn an_include_whose_fragment_is_missing_still_opens_the_template() {
        let test = TemplateTest::new(&[
            (
                "blog/templates/blog/cards.html",
                "{% block a %}{% endblock %}",
            ),
            (
                "blog/templates/blog/post.html",
                "{% include 'blog/car<CURSOR>ds.html#gone' %}",
            ),
        ]);

        assert_eq!(test.definitions(), ["/blog/templates/blog/cards.html:"]);
    }

    #[test]
    fn an_inheritance_cycle_does_not_hang() {
        let test = TemplateTest::new(&[
            (
                "blog/templates/a.html",
                "{% extends 'b.html' %}{% block x<CURSOR> %}",
            ),
            ("blog/templates/b.html", "{% extends 'a.html' %}"),
        ]);

        assert!(test.definitions().is_empty());
    }

    #[test]
    fn url_navigates_to_the_route_that_is_named() {
        let definitions = project("{% url 'blog:<CURSOR>index' %}").definitions();
        assert_eq!(definitions, ["/blog/urls.py:'index'"]);
    }

    #[test]
    fn a_filter_navigates_to_the_function_registered_under_it() {
        let definitions = project("{{ book.title|sh<CURSOR>out }}").definitions();
        assert_eq!(definitions, ["/blog/templatetags/blog_extras.py:shout"]);
    }

    #[test]
    fn a_tag_navigates_to_the_function_registered_under_it() {
        let definitions = project("{% book_c<CURSOR>ount %}").definitions();
        assert_eq!(
            definitions,
            ["/blog/templatetags/blog_extras.py:book_count"]
        );
    }

    #[test]
    fn a_builtin_tag_navigates_nowhere() {
        assert!(
            project("{% ext<CURSOR>ends 'blog/base.html' %}")
                .definitions()
                .is_empty()
        );
    }

    #[test]
    fn load_navigates_to_the_library_module() {
        let definitions = project("{% load blog_ex<CURSOR>tras %}").definitions();
        assert_eq!(definitions, ["/blog/templatetags/blog_extras.py:"]);
    }

    #[test]
    fn a_partial_navigates_to_the_partialdef_that_declares_it() {
        let definitions =
            project("{% partialdef card %}x{% endpartialdef %}{% partial ca<CURSOR>rd %}")
                .definitions();
        assert_eq!(definitions, ["/blog/templates/blog/post.html:card"]);
    }

    #[test]
    fn a_block_in_a_child_navigates_to_the_parents_block() {
        let definitions =
            project("{% extends 'blog/base.html' %}{% block con<CURSOR>tent %}{% endblock %}")
                .definitions();
        assert_eq!(definitions, ["/blog/templates/blog/base.html:content"]);
    }

    #[test]
    fn a_context_variable_navigates_to_the_view_that_supplies_it() {
        let definitions = project("{{ bo<CURSOR>ok }}").definitions();
        assert_eq!(definitions, ["/blog/views.py:'book'"]);
    }

    #[test]
    fn a_loop_variable_navigates_to_the_tag_that_binds_it() {
        let definitions =
            project("{% for entry in shelf %}{{ ent<CURSOR>ry }}{% endfor %}").definitions();
        assert_eq!(definitions, ["/blog/templates/blog/post.html:entry"]);
    }

    #[test]
    fn an_attribute_navigates_to_the_type_it_is_read_off() {
        let definitions = project("{{ book.au<CURSOR>thor }}").definitions();
        assert_eq!(definitions, ["/blog/models.py:Book"]);
    }

    #[test]
    fn an_unknown_name_navigates_nowhere() {
        assert!(project("{{ myst<CURSOR>ery }}").definitions().is_empty());
    }

    #[test]
    fn a_comment_navigates_nowhere() {
        assert!(project("{# bo<CURSOR>ok #}").definitions().is_empty());
    }
}
