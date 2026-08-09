//! hover for django templates
//!
//! a template says very little about itself: `{{ book.title }}` is three words and
//! a dot, and what makes it meaningful is the model behind `book`, the view that
//! put it there and the filter library the `|` reaches into. hover is where all of
//! that is already known and only has to be written down, so every answer here is
//! a lookup the other features have made already — a type from [`super::resolve`],
//! a documentation string from [`super::builtins`] or from the project's own
//! registrations, a template the loader resolved.

use std::fmt::{self, Formatter};

use ruff_db::files::{File, FileRange};
use ruff_text_size::TextSize;
use ty_project::Db;

use crate::{MarkupKind, RangedValue};

use super::builtins;
use super::goto::path_up_to;
use super::index::TemplateIndex;
use super::lexer::{ConstructKind, Token, TokenKind, string_contents};
use super::project::{self, RegistrationKind};
use super::resolve::{self, Origin};

/// the language a template construct is rendered as, for a client that knows it
const DJANGO: &str = "django-html";

/// what a hover over a template says
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateHover {
    contents: Vec<Content>,
}

impl TemplateHover {
    /// render the hover in the markup the client asked for
    pub fn display(&self, kind: MarkupKind) -> DisplayTemplateHover<'_> {
        DisplayTemplateHover { hover: self, kind }
    }
}

/// one section of a hover
#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    /// source written in `language`
    Code {
        language: &'static str,
        text: String,
    },
    /// prose, in markdown
    Text(String),
}

pub struct DisplayTemplateHover<'a> {
    hover: &'a TemplateHover,
    kind: MarkupKind,
}

impl fmt::Display for DisplayTemplateHover<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (position, content) in self.hover.contents.iter().enumerate() {
            if position > 0 {
                write!(f, "{}", self.kind.horizontal_line())?;
            }

            match content {
                Content::Code { language, text } => {
                    write!(f, "{}", self.kind.fenced_code_block(text, language))?;
                }
                Content::Text(text) => write!(f, "{text}")?,
            }
        }

        Ok(())
    }
}

/// what the thing at `offset` of the template `file` is
pub(crate) fn hover(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
) -> Option<RangedValue<TemplateHover>> {
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

    let contents = site.contents();

    (!contents.is_empty()).then(|| RangedValue {
        range: FileRange::new(file, token.range),
        value: TemplateHover { contents },
    })
}

/// the one token the hover is about, with everything needed to interpret it
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

    fn contents(&self) -> Vec<Content> {
        match self.token.kind {
            TokenKind::TagName => {
                self.registered_contents(false, format!("{{% {} %}}", self.text()))
            }
            TokenKind::FilterName => self.registered_contents(true, format!("|{}", self.text())),
            TokenKind::String => self.literal_contents(),
            TokenKind::Variable | TokenKind::Attribute => self.name_contents(),
            _ => Vec::new(),
        }
    }

    /// a tag or a filter: either one django ships or one the project registers,
    /// and in both cases the answer is what it is documented to do
    fn registered_contents(&self, filter: bool, written: String) -> Vec<Content> {
        let name = self.text();
        let header = Content::Code {
            language: DJANGO,
            text: written,
        };

        // the table documents django's tags and filters, but which of them this
        // django has and where each comes from is that django's to say
        let documented = if filter {
            builtins::filter(name).map(|filter| filter.documentation)
        } else {
            builtins::tag(name).map(|tag| tag.documentation)
        };

        if let Some(documented) = documented
            && let Some(provided) = builtins::provided_by_django(self.db, name, filter)
        {
            return [header]
                .into_iter()
                .chain(documentation(documented, provided.library()))
                .collect();
        }

        let registered = project::registrations(self.db, self.db.project())
            .iter()
            .find(|registration| {
                (registration.kind == RegistrationKind::Filter) == filter
                    && registration.name == name
            });

        let Some(registration) = registered else {
            return Vec::new();
        };

        [header]
            .into_iter()
            .chain(documentation(
                registration.documentation.as_deref().unwrap_or_default(),
                Some(&registration.library),
            ))
            .collect()
    }

    /// what a string argument names, which depends on the tag it sits in
    fn literal_contents(&self) -> Vec<Content> {
        let value = string_contents(self.source, self.token.range);

        match self.tag {
            "extends" | "include" => {
                let Some(reference) = self
                    .index
                    .extends()
                    .into_iter()
                    .chain(self.index.includes())
                    .find(|reference| reference.range == value)
                else {
                    return Vec::new();
                };

                match project::resolve_template(self.db, &reference.name) {
                    Some(target) => vec![Content::Text(format!(
                        "template `{}`",
                        target.path(self.db)
                    ))],
                    None => Vec::new(),
                }
            }
            "url" => {
                let name = &self.source[value];
                project::url_names(self.db, self.db.project())
                    .iter()
                    .filter(|url| url.name == name)
                    .filter_map(|url| url.route.as_deref())
                    .map(|route| Content::Text(format!("route `{route}`")))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// what a bare name is, which again depends on the tag it sits in
    fn name_contents(&self) -> Vec<Content> {
        match self.tag {
            "block" | "partialdef" => self.declaration_contents(),
            "partial" => self.partial_contents(),
            _ => self.path_contents(),
        }
    }

    /// a `{% block %}` or a `{% partialdef %}` declares the name it writes
    fn declaration_contents(&self) -> Vec<Content> {
        let block = self.tag == "block";
        let definitions = if block {
            self.index.blocks()
        } else {
            self.index.partials()
        };

        let Some(definition) = definitions
            .iter()
            .find(|definition| definition.name_range == self.token.range)
        else {
            return Vec::new();
        };

        let header = Content::Code {
            language: DJANGO,
            text: format!("{{% {} {} %}}", self.tag, definition.name),
        };

        if !block {
            return vec![
                header,
                Content::Text(format!(
                    "a fragment `{{% partial {} %}}` renders",
                    definition.name
                )),
            ];
        }

        // a block in a child template is an override, and which template it
        // overrides is the thing worth saying about it
        let overridden = super::ancestors(self.db, self.file, self.index)
            .into_iter()
            .find(|(_, ancestor)| {
                ancestor
                    .blocks()
                    .iter()
                    .any(|candidate| candidate.name == definition.name)
            });

        let text = match overridden {
            Some((ancestor, _)) => format!("overrides the block in `{}`", self.label(ancestor)),
            None => "a block a child template can override".to_string(),
        };

        vec![header, Content::Text(text)]
    }

    /// a `{% partial %}` renders a fragment this template or a parent declares
    fn partial_contents(&self) -> Vec<Content> {
        let name = self.text();

        let declaring = std::iter::once((self.file, self.index))
            .chain(super::ancestors(self.db, self.file, self.index))
            .find(|(_, index)| index.partials().iter().any(|partial| partial.name == name));

        let Some((declaring, _)) = declaring else {
            return Vec::new();
        };

        vec![
            Content::Code {
                language: DJANGO,
                text: format!("{{% partialdef {name} %}}"),
            },
            Content::Text(format!(
                "a fragment declared in `{}`",
                self.label(declaring)
            )),
        ]
    }

    /// the type of the path the name ends
    fn path_contents(&self) -> Vec<Content> {
        let segments = path_up_to(self.source, self.tokens, self.token, true);

        if let Some(ty) = resolve::path_type(
            self.db,
            self.file,
            self.index,
            self.source,
            self.offset,
            &segments,
        ) {
            return vec![Content::Code {
                language: "python",
                text: format!("{}: {}", self.text(), ty.display(self.db)),
            }];
        }

        // a name nothing gives a type to is still worth placing: the alternative
        // is a hover that says nothing at all about a name the template knows
        let [root] = segments[..] else {
            return Vec::new();
        };
        if root != self.text() {
            return Vec::new();
        }

        match resolve::resolve_root(self.db, self.file, self.index, self.offset, root) {
            Some(Origin::Binding(_)) => vec![Content::Text("bound by this template".to_string())],
            Some(Origin::Context(variable)) => {
                vec![Content::Text(variable.source.description().to_string())]
            }
            None => Vec::new(),
        }
    }

    /// how a template reads in prose: the name the loader knows it by, and its
    /// path when it is not under a template root at all
    fn label(&self, file: File) -> String {
        resolve::template_name(self.db, file).unwrap_or_else(|| file.path(self.db).to_string())
    }
}

/// a documentation section, with the `{% load %}` its name needs
fn documentation(text: &str, library: Option<&str>) -> Option<Content> {
    match library {
        Some(library) => Some(Content::Text(format!(
            "{text}\n\nrequires `{{% load {library} %}}`"
        ))),
        None if text.is_empty() => None,
        None => Some(Content::Text(text.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::{TemplateTest, with_forward_slashes};

    /// the same small django project the completion and goto tests use
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
                    'shouts it.'
                    return value
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
    fn a_context_variable_hovers_as_its_type() {
        assert_eq!(
            project("{{ bo<CURSOR>ok }}").hover(),
            "```python\nbook: Book\n```"
        );
    }

    #[test]
    fn an_attribute_hovers_as_its_type() {
        assert_eq!(
            project("{{ book.ti<CURSOR>tle }}").hover(),
            "```python\ntitle: str\n```"
        );
    }

    #[test]
    fn a_nested_attribute_hovers_as_its_type() {
        assert_eq!(
            project("{{ book.author.na<CURSOR>me }}").hover(),
            "```python\nname: str\n```"
        );
    }

    #[test]
    fn a_name_with_no_type_still_says_where_it_comes_from() {
        assert_eq!(
            project("{% for entry in shelf %}{{ ent<CURSOR>ry }}{% endfor %}").hover(),
            "bound by this template"
        );
    }

    #[test]
    fn a_loop_variable_hovers_as_the_element_type() {
        let test = TemplateTest::new(&[
            (
                "blog/models.py",
                "
                class Chapter:
                    title: str

                class Book:
                    chapters: list[Chapter]
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
                "blog/templates/blog/post.html",
                "{% for chapter in book.chapters %}{{ chap<CURSOR>ter }}{% endfor %}",
            ),
        ]);

        assert_eq!(test.hover(), "```python\nchapter: Chapter\n```");
    }

    #[test]
    fn a_builtin_filter_hovers_as_its_documentation() {
        let hover = project("{{ book.title|up<CURSOR>per }}").hover();
        assert!(
            hover.starts_with("```django-html\n|upper\n```"),
            "got {hover}"
        );
        assert!(hover.contains("upper-case"), "got {hover}");
    }

    #[test]
    fn a_project_filter_hovers_as_its_docstring() {
        assert_eq!(
            project("{{ book.title|sh<CURSOR>out }}").hover(),
            "```django-html\n|shout\n```\n---\nshouts it.\n\nrequires `{% load blog_extras %}`"
        );
    }

    #[test]
    fn a_builtin_tag_hovers_as_its_documentation() {
        let hover = project("{% for<CURSOR> book in books %}{% endfor %}").hover();
        assert!(
            hover.starts_with("```django-html\n{% for %}\n```"),
            "got {hover}"
        );
    }

    #[test]
    fn a_tag_from_a_library_says_what_to_load() {
        let hover = project("{% st<CURSOR>atic 'a.css' %}").hover();
        assert!(
            hover.contains("requires `{% load static %}`"),
            "got {hover}"
        );
    }

    #[test]
    fn an_unknown_tag_hovers_as_nothing() {
        assert_eq!(project("{% myst<CURSOR>ery %}").hover(), "");
    }

    #[test]
    fn extends_hovers_as_the_resolved_path() {
        assert_eq!(
            with_forward_slashes(project("{% extends 'blog/b<CURSOR>ase.html' %}").hover()),
            "template `/blog/templates/blog/base.html`"
        );
    }

    #[test]
    fn a_url_hovers_as_its_route() {
        assert_eq!(
            project("{% url 'blog:in<CURSOR>dex' %}").hover(),
            "route `books/`"
        );
    }

    #[test]
    fn a_block_in_a_child_says_what_it_overrides() {
        assert_eq!(
            project("{% extends 'blog/base.html' %}{% block con<CURSOR>tent %}{% endblock %}")
                .hover(),
            "```django-html\n{% block content %}\n```\n---\noverrides the block in `blog/base.html`"
        );
    }

    #[test]
    fn a_block_with_no_parent_says_it_can_be_overridden() {
        assert_eq!(
            project("{% block con<CURSOR>tent %}{% endblock %}").hover(),
            "```django-html\n{% block content %}\n```\n---\na block a child template can override"
        );
    }

    #[test]
    fn a_partialdef_says_how_it_is_rendered() {
        assert_eq!(
            project("{% partialdef ca<CURSOR>rd %}x{% endpartialdef %}").hover(),
            "```django-html\n{% partialdef card %}\n```\n---\na fragment `{% partial card %}` renders"
        );
    }

    #[test]
    fn a_partial_says_where_it_is_declared() {
        assert_eq!(
            project("{% partialdef card %}x{% endpartialdef %}{% partial ca<CURSOR>rd %}").hover(),
            "```django-html\n{% partialdef card %}\n```\n---\na fragment declared in `blog/post.html`"
        );
    }

    #[test]
    fn a_comment_hovers_as_nothing() {
        assert_eq!(project("{# bo<CURSOR>ok #}").hover(), "");
    }

    #[test]
    fn markup_outside_a_construct_hovers_as_nothing() {
        assert_eq!(project("<p<CURSOR>>a</p>").hover(), "");
    }
}
