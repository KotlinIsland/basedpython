//! completions for django templates
//!
//! what a template can usefully offer depends entirely on where the cursor is:
//! the first word of a `{% %}` is a tag, the word after a `|` is a filter, the
//! word after a `.` is an attribute of whatever precedes it, and the string
//! argument of an `{% extends %}` is a template path. [`Context`] is that
//! classification, and everything else here is one context's answer.
//!
//! results come back in priority order. the caller is expected to preserve it —
//! the server does, by deriving each item's `sortText` from its position — because
//! a lot of the value is in the ordering: the `{% endfor %}` that closes the block
//! the cursor is in must come before the fifty other tags that are also legal
//! there.

use compact_str::{CompactString, ToCompactString};
use ruff_db::files::File;
use ruff_text_size::{TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_project::Db;

use crate::completion::CompletionKind;

use super::builtins;
use super::index::{Block, TemplateIndex};
use super::lexer::{Construct, ConstructKind, Token, TokenKind};
use super::project::{self, RegistrationKind};
use super::resolve;

/// how many `{% extends %}` hops the parent chain is followed
const MAX_INHERITANCE_DEPTH: u32 = 16;

/// an edit a completion carries alongside the text it inserts
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateEdit {
    pub range: TextRange,
    pub text: String,
}

/// one suggestion for a position in a django template
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateCompletion {
    pub label: String,
    pub kind: CompletionKind,
    /// the type, route or library the suggestion comes with
    pub detail: Option<String>,
    /// markdown documentation
    pub documentation: Option<String>,
    /// the text to insert, when it differs from the label
    pub insert: Option<String>,
    /// the range this suggestion replaces
    ///
    /// the range is always given rather than left to the client's own idea of a
    /// word, because template names such as `blog/post.html` and namespaced url
    /// names such as `polls:detail` are not words by any client's definition.
    pub range: TextRange,
    /// the `{% load %}` the suggestion needs, when its library isn't loaded yet
    pub additional_edit: Option<TemplateEdit>,
}

impl TemplateCompletion {
    fn new(label: impl Into<String>, kind: CompletionKind, range: TextRange) -> Self {
        Self {
            label: label.into(),
            kind,
            detail: None,
            documentation: None,
            insert: None,
            range,
            additional_edit: None,
        }
    }

    #[must_use]
    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    fn documentation(mut self, documentation: impl Into<String>) -> Self {
        self.documentation = Some(documentation.into());
        self
    }
}

/// the suggestions for `offset` in the template `file`
pub(crate) fn completions(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
) -> Vec<TemplateCompletion> {
    let Some(construct) = index.lexed().construct_at(offset) else {
        // outside a construct the file is whatever markup it is, and the editor
        // completes that far better than this could
        return Vec::new();
    };

    let cursor = Cursor::new(index, source, construct, offset);

    match cursor.context() {
        Context::None => Vec::new(),
        Context::TagName => tag_names(db, index, source, &cursor),
        Context::FilterName => filter_names(db, index, &cursor),
        Context::Member(path) => members(db, file, index, source, offset, &path, &cursor),
        Context::Variable => variables(db, file, index, offset, &cursor),
        Context::TemplatePath => template_paths(db, &cursor),
        Context::StaticPath => static_paths(db, &cursor),
        Context::UrlName => url_names(db, &cursor),
        Context::Library => libraries(db, index, &cursor),
        Context::BlockName => block_names(db, index, &cursor),
        Context::PartialName => partial_names(db, index, &cursor),
    }
}

/// what the cursor's position calls for
#[derive(Debug, Clone, PartialEq, Eq)]
enum Context {
    /// nothing useful; a comment, or a position with no candidates
    None,
    /// the first word of a `{% %}`
    TagName,
    /// the word after a `|`
    FilterName,
    /// the word after a `.`, on the given path
    Member(Vec<CompactString>),
    /// a bare name position, where the template's own variables go
    Variable,
    /// the template a `{% extends %}`/`{% include %}` names
    TemplatePath,
    /// the file a `{% static %}` names
    StaticPath,
    /// the route a `{% url %}` reverses
    UrlName,
    /// the library a `{% load %}` loads
    Library,
    /// the name a `{% block %}` overrides
    BlockName,
    /// the fragment a `{% partial %}` renders
    PartialName,
}

/// the cursor's position within a construct, resolved once for every rule to use
struct Cursor<'a> {
    construct: &'a Construct,
    source: &'a str,
    offset: TextSize,
    /// the construct's tokens with its delimiters dropped
    tokens: &'a [Token],
    /// the index in `tokens` of the token the cursor is in or at the end of
    current: Option<usize>,
    /// the range the completion replaces
    range: TextRange,
}

impl<'a> Cursor<'a> {
    fn new(
        index: &'a TemplateIndex,
        source: &'a str,
        construct: &'a Construct,
        offset: TextSize,
    ) -> Self {
        let all = index.lexed().construct_tokens(construct);
        let start = usize::from(
            all.first()
                .is_some_and(|token| token.kind == TokenKind::Delimiter),
        );
        let end = all
            .iter()
            .rposition(|token| token.kind != TokenKind::Delimiter)
            .map_or(start, |index| index + 1);
        let tokens = all.get(start..end.max(start)).unwrap_or_default();

        // only a token the user could still be *typing* counts as the one under
        // the cursor. a cursor at the end of a `.` or a `|` is not editing that
        // operator, it is starting the word after it — which is the whole reason
        // `{{ book.` completes attributes rather than variables.
        let current = tokens
            .iter()
            .position(|token| is_word(token.kind) && token.range.contains_inclusive(offset));

        // a partially typed word is replaced whole; a cursor in open space
        // inserts without replacing anything
        let range = match current.map(|index| &tokens[index]) {
            Some(token) if token.kind == TokenKind::String => string_contents(source, token.range),
            Some(token) => token.range,
            None => TextRange::empty(offset),
        };

        Self {
            construct,
            source,
            offset,
            tokens,
            current,
            range,
        }
    }

    fn tag(&self) -> &'a str {
        self.construct.name.map_or("", |range| &self.source[range])
    }

    /// the token before the cursor, skipping the one being typed
    fn previous(&self) -> Option<&'a Token> {
        let before = match self.current {
            Some(index) => index,
            None => self
                .tokens
                .iter()
                .position(|token| token.range.start() >= self.offset)
                .unwrap_or(self.tokens.len()),
        };

        self.tokens.get(before.checked_sub(1)?)
    }

    /// how many argument tokens the cursor is preceded by
    ///
    /// the tag name is not one of them, so the first argument of `{% url %}` is
    /// at position zero.
    fn argument_position(&self) -> usize {
        let tag_name = usize::from(self.construct.name.is_some());
        let before = match self.current {
            Some(index) => index,
            None => self
                .tokens
                .iter()
                .position(|token| token.range.start() >= self.offset)
                .unwrap_or(self.tokens.len()),
        };

        before.saturating_sub(tag_name)
    }

    /// whether the cursor is still on the tag's own name
    fn on_tag_name(&self) -> bool {
        match self.construct.name {
            // no name typed yet: everything up to the closing delimiter is the
            // name's position
            None => true,
            Some(name) => self.offset <= name.end(),
        }
    }

    fn context(&self) -> Context {
        if self.construct.kind == ConstructKind::Comment {
            return Context::None;
        }

        if self.construct.kind == ConstructKind::Tag {
            if self.on_tag_name() {
                return Context::TagName;
            }

            // a tag whose argument names something other than a variable
            let position = self.argument_position();
            match (self.tag(), position) {
                ("extends" | "include", 0) => return Context::TemplatePath,
                ("static", 0) => return Context::StaticPath,
                ("url", 0) => return Context::UrlName,
                ("load", _) => return Context::Library,
                ("block", 0) => return Context::BlockName,
                ("partial", 0) => return Context::PartialName,
                _ => {}
            }
        }

        match self.previous() {
            Some(token) if self.source[token.range] == *"|" => Context::FilterName,
            Some(token) if self.source[token.range] == *"." => match self.path_before() {
                path if path.is_empty() => Context::None,
                path => Context::Member(path),
            },
            _ => Context::Variable,
        }
    }

    /// the dotted path written immediately before the cursor's `.`
    fn path_before(&self) -> Vec<CompactString> {
        let Some(dot) = self.current.unwrap_or(self.tokens.len()).checked_sub(1) else {
            return Vec::new();
        };

        let mut segments = Vec::new();
        let mut index = dot;

        // walk back over `… . name . name`, ending on the path's leading name
        while let Some(previous) = index.checked_sub(1) {
            let Some(token) = self.tokens.get(previous) else {
                break;
            };
            match token.kind {
                TokenKind::Attribute => {
                    segments.push(self.source[token.range].to_compact_string());
                }
                TokenKind::Variable => {
                    segments.push(self.source[token.range].to_compact_string());
                    break;
                }
                _ => break,
            }

            // step over the `.` joining this segment to the previous one
            let Some(dot) = previous.checked_sub(1) else {
                break;
            };
            if self
                .tokens
                .get(dot)
                .is_none_or(|token| self.source[token.range] != *".")
            {
                break;
            }
            index = dot;
        }

        segments.reverse();
        segments
    }
}

/// whether a token of this kind is a word the user can be part-way through
fn is_word(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::TagName
            | TokenKind::FilterName
            | TokenKind::Variable
            | TokenKind::Attribute
            | TokenKind::KeywordArgument
            | TokenKind::Keyword
            | TokenKind::BuiltinConstant
            | TokenKind::String
            | TokenKind::Number
    )
}

/// the contents of a string literal, its quotes excluded
fn string_contents(source: &str, range: TextRange) -> TextRange {
    let text = &source[range];
    let Some(quote) = text.chars().next() else {
        return range;
    };
    if !matches!(quote, '"' | '\'') {
        return range;
    }

    let start = range.start() + TextSize::from(1);
    let end = if text.len() > 1 && text.ends_with(quote) {
        range.end() - TextSize::from(1)
    } else {
        range.end()
    };

    TextRange::new(start, end.max(start))
}

fn tag_names(
    db: &dyn Db,
    index: &TemplateIndex,
    source: &str,
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    let mut completions = Vec::new();
    let open = index.open_blocks_at(cursor.construct.range.start());

    // the tag that closes the block the cursor is in is nearly always the one
    // wanted, so it leads
    for block in &open {
        if block.closed {
            continue;
        }
        completions.push(
            TemplateCompletion::new(
                block.end_tag.as_str(),
                CompletionKind::Keyword,
                cursor.range,
            )
            .detail(format!("closes {}", opening_tag(source, block)))
            .documentation(format!("closes the `{{% {} %}}` above.", block.name)),
        );
    }

    if let Some(innermost) = open.first() {
        for branch in builtins::tag(&innermost.name)
            .map(|tag| tag.branches)
            .unwrap_or_default()
        {
            completions.push(
                TemplateCompletion::new(*branch, CompletionKind::Keyword, cursor.range)
                    .detail(format!("branch of {}", opening_tag(source, innermost))),
            );
        }
    }

    let loaded = loaded_libraries(index);
    let load_edit = |library: Option<&str>| load_edit_for(index, &loaded, library);

    for tag in builtins::TAGS {
        let mut completion =
            TemplateCompletion::new(tag.name, CompletionKind::Keyword, cursor.range)
                .documentation(tag.documentation);
        if let Some(library) = tag.library {
            completion = completion.detail(format!("{{% load {library} %}}"));
        }
        completion.additional_edit = load_edit(tag.library);
        completions.push(completion);
    }

    for registration in project::registrations(db, db.project()) {
        if registration.kind == RegistrationKind::Filter {
            continue;
        }

        let mut completion = TemplateCompletion::new(
            registration.name.as_str(),
            CompletionKind::Keyword,
            cursor.range,
        )
        .detail(format!("{{% load {} %}}", registration.library));
        completion.documentation = registration
            .documentation
            .as_deref()
            .map(ToString::to_string);
        completion.additional_edit = load_edit(Some(&registration.library));
        completions.push(completion);
    }

    completions
}

/// how an open block's tag reads, for a closing tag's detail line
fn opening_tag(source: &str, block: &Block) -> String {
    source[block.open_range].trim().to_string()
}

fn filter_names(
    db: &dyn Db,
    index: &TemplateIndex,
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    let loaded = loaded_libraries(index);
    let mut completions = Vec::new();

    for filter in builtins::FILTERS {
        let mut completion =
            TemplateCompletion::new(filter.name, CompletionKind::Function, cursor.range)
                .documentation(filter.documentation);
        if let Some(library) = filter.library {
            completion = completion.detail(format!("{{% load {library} %}}"));
        }
        completion.additional_edit = load_edit_for(index, &loaded, filter.library);
        completions.push(completion);
    }

    for registration in project::registrations(db, db.project()) {
        if registration.kind != RegistrationKind::Filter {
            continue;
        }

        let mut completion = TemplateCompletion::new(
            registration.name.as_str(),
            CompletionKind::Function,
            cursor.range,
        )
        .detail(format!("{{% load {} %}}", registration.library));
        completion.documentation = registration
            .documentation
            .as_deref()
            .map(ToString::to_string);
        completion.additional_edit = load_edit_for(index, &loaded, Some(&registration.library));
        completions.push(completion);
    }

    completions
}

fn members(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
    path: &[CompactString],
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    let segments: Vec<&str> = path.iter().map(CompactString::as_str).collect();
    let Some(ty) = resolve::path_type(db, file, index, source, offset, &segments) else {
        return Vec::new();
    };

    resolve::members(db, ty)
        .into_iter()
        .map(|member| {
            TemplateCompletion::new(member.name.as_str(), CompletionKind::Field, cursor.range)
                .detail(member.ty.display(db).to_string())
        })
        .collect()
}

fn variables(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    offset: TextSize,
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    let mut completions = Vec::new();
    let mut seen = FxHashSet::default();

    // a name the template itself binds shadows one from the context, so it goes
    // first for the same reason it wins at render time
    for binding in index.bindings_at(offset) {
        if seen.insert(binding.name.clone()) {
            completions.push(
                TemplateCompletion::new(
                    binding.name.as_str(),
                    CompletionKind::Variable,
                    cursor.range,
                )
                .detail("bound by this template"),
            );
        }
    }

    for variable in resolve::context_variables(db, file) {
        if !seen.insert(variable.name.clone()) {
            continue;
        }

        let mut completion = TemplateCompletion::new(
            variable.name.as_str(),
            CompletionKind::Variable,
            cursor.range,
        );
        completion.detail = variable
            .value
            .and_then(|value| resolve::expression_type(db, variable.file, value))
            .map(|ty| ty.display(db).to_string())
            .or_else(|| Some("from the view's context".to_string()));
        completions.push(completion);
    }

    completions
}

fn template_paths(db: &dyn Db, cursor: &Cursor<'_>) -> Vec<TemplateCompletion> {
    discovered(project::template_files(db, db.project()), cursor)
}

fn static_paths(db: &dyn Db, cursor: &Cursor<'_>) -> Vec<TemplateCompletion> {
    discovered(project::static_files(db, db.project()), cursor)
}

/// one suggestion per name found under the project's template or static roots
///
/// two apps may both hold a `base.html`; django's loader resolves the name to
/// whichever app comes first, so the name is offered once, with the path that
/// would win shown alongside it.
fn discovered(files: &[project::DiscoveredFile], cursor: &Cursor<'_>) -> Vec<TemplateCompletion> {
    let mut seen = FxHashSet::default();

    files
        .iter()
        .filter(|file| seen.insert(file.name.clone()))
        .map(|file| {
            TemplateCompletion::new(file.name.as_str(), CompletionKind::File, cursor.range)
                .detail(file.path.as_str())
        })
        .collect()
}

fn url_names(db: &dyn Db, cursor: &Cursor<'_>) -> Vec<TemplateCompletion> {
    project::url_names(db, db.project())
        .iter()
        .map(|url| {
            let mut completion =
                TemplateCompletion::new(url.name.as_str(), CompletionKind::Reference, cursor.range);
            completion.detail = url.route.as_deref().map(ToString::to_string);
            completion
        })
        .collect()
}

fn libraries(db: &dyn Db, index: &TemplateIndex, cursor: &Cursor<'_>) -> Vec<TemplateCompletion> {
    let loaded = loaded_libraries(index);
    let mut completions = Vec::new();
    let mut seen = FxHashSet::default();

    for library in builtins::LIBRARIES {
        if loaded.contains(&library.to_compact_string()) {
            continue;
        }
        seen.insert(library.to_compact_string());
        completions.push(
            TemplateCompletion::new(*library, CompletionKind::Module, cursor.range)
                .detail("django"),
        );
    }

    for registration in project::registrations(db, db.project()) {
        if loaded.contains(&registration.library) || !seen.insert(registration.library.clone()) {
            continue;
        }
        completions.push(
            TemplateCompletion::new(
                registration.library.as_str(),
                CompletionKind::Module,
                cursor.range,
            )
            .detail("this project"),
        );
    }

    completions
}

fn block_names(db: &dyn Db, index: &TemplateIndex, cursor: &Cursor<'_>) -> Vec<TemplateCompletion> {
    // a `{% block %}` in a child template is only useful when it overrides one of
    // the parent's, so those are exactly what is offered
    let defined: FxHashSet<_> = index
        .blocks()
        .iter()
        .map(|block| block.name.clone())
        .collect();

    inherited(db, index, |parent| {
        parent
            .blocks()
            .iter()
            .map(|block| block.name.clone())
            .collect()
    })
    .into_iter()
    .filter(|name| !defined.contains(name))
    .map(|name| {
        TemplateCompletion::new(name.as_str(), CompletionKind::Function, cursor.range)
            .detail("overrides the parent template's block")
    })
    .collect()
}

fn partial_names(
    db: &dyn Db,
    index: &TemplateIndex,
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    let mut names: Vec<CompactString> = index
        .partials()
        .iter()
        .map(|partial| partial.name.clone())
        .collect();

    // a partial defined by a template this one extends is in scope here too
    names.extend(inherited(db, index, |parent| {
        parent
            .partials()
            .iter()
            .map(|partial| partial.name.clone())
            .collect()
    }));

    let mut seen = FxHashSet::default();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .map(|name| {
            TemplateCompletion::new(name.as_str(), CompletionKind::Function, cursor.range)
                .detail("a fragment defined with `{% partialdef %}`")
        })
        .collect()
}

/// collect `names` from every template up the `{% extends %}` chain
fn inherited(
    db: &dyn Db,
    index: &TemplateIndex,
    names: impl Fn(&TemplateIndex) -> Vec<CompactString>,
) -> Vec<CompactString> {
    let mut collected = Vec::new();
    let mut seen = FxHashSet::default();
    let mut parent = index.extends().map(|reference| reference.name.clone());

    for _ in 0..MAX_INHERITANCE_DEPTH {
        let Some(name) = parent.take() else { break };
        let Some(file) = project::resolve_template(db, &name) else {
            break;
        };
        if !seen.insert(file) {
            // a cycle in the inheritance chain; django would fail to render it,
            // but the editor must not hang on it
            break;
        }

        let parent_index = super::template_index(db, file);
        collected.extend(names(parent_index));
        parent = parent_index
            .extends()
            .map(|reference| reference.name.clone());
    }

    collected
}

/// the libraries the template has already `{% load %}`ed
fn loaded_libraries(index: &TemplateIndex) -> FxHashSet<CompactString> {
    index
        .loads()
        .iter()
        .map(|load| load.library.clone())
        .collect()
}

/// the `{% load %}` a suggestion from `library` needs, when it isn't loaded yet
fn load_edit_for(
    index: &TemplateIndex,
    loaded: &FxHashSet<CompactString>,
    library: Option<&str>,
) -> Option<TemplateEdit> {
    let library = library?;
    if loaded.contains(&library.to_compact_string()) {
        return None;
    }

    // `{% extends %}` has to stay the first tag in the file, so a `{% load %}`
    // goes after it rather than above it
    let after_extends = index.extends().and_then(|reference| {
        index
            .lexed()
            .constructs()
            .iter()
            .find(|construct| construct.range.contains_range(reference.range))
            .map(|construct| construct.range.end())
    });
    let start = after_extends.unwrap_or_default();

    Some(TemplateEdit {
        range: TextRange::empty(start),
        text: if start == TextSize::new(0) {
            format!("{{% load {library} %}}\n")
        } else {
            format!("\n{{% load {library} %}}")
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::django_template::django_template_completions;
    use crate::django_template::tests::TemplateTest;

    /// a small but complete django project: a couple of models, a view that
    /// renders a template with them, a url configuration, and a tag library
    fn project(template: &str) -> TemplateTest {
        TemplateTest::new(&[
            (
                "blog/models.py",
                "
                class Author:
                    name: str
                    email: str

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
                    book = Book()
                    return render(request, 'blog/post.html', {'book': book, 'shelf': [book]})
                ",
            ),
            (
                "blog/urls.py",
                "
                app_name = 'blog'

                urlpatterns = [
                    path('books/<int:pk>/', detail, name='detail'),
                    path('books/', index, name='index'),
                ]
                ",
            ),
            (
                "blog/templatetags/blog_extras.py",
                "
                from django import template

                register = template.Library()

                @register.filter
                def shout(value):
                    '''upper-cases and adds an exclamation mark.'''
                    return value

                @register.simple_tag(name='book_count')
                def count_books():
                    return 0
                ",
            ),
            (
                "blog/templates/blog/base.html",
                "{% block content %}{% endblock %}",
            ),
            ("blog/static/blog/app.css", "body {}"),
            ("blog/templates/blog/post.html", template),
        ])
    }

    #[test]
    fn a_position_outside_a_construct_offers_nothing() {
        assert!(project("<p>he<CURSOR>llo</p>").completions().is_empty());
    }

    #[test]
    fn a_comment_offers_nothing() {
        assert!(project("{# to<CURSOR>do #}").completions().is_empty());
    }

    #[test]
    fn the_first_word_of_a_tag_offers_tag_names() {
        let completions = project("{% <CURSOR> %}").completions();

        assert!(completions.contains(&"extends".to_string()));
        assert!(completions.contains(&"include".to_string()));
        assert!(completions.contains(&"partialdef".to_string()));
        assert!(
            completions.contains(&"book_count".to_string()),
            "the project's own tag is offered too"
        );
        assert!(
            !completions.contains(&"shout".to_string()),
            "a filter is not a tag"
        );
    }

    #[test]
    fn the_tag_closing_the_enclosing_block_is_offered_first() {
        let completions = project("{% for book in shelf %}{% <CURSOR>").completions();

        assert_eq!(completions[0], "endfor");
        assert_eq!(
            completions[1], "empty",
            "the branch tag of the enclosing block comes next"
        );
    }

    #[test]
    fn nested_blocks_offer_their_closing_tags_innermost_first() {
        let completions = project("{% for b in shelf %}{% if b %}{% <CURSOR>").completions();
        assert_eq!(&completions[..2], ["endif", "endfor"]);
    }

    #[test]
    fn a_block_that_is_already_closed_does_not_offer_its_closing_tag() {
        // the structure is complete, so what the user is adding here is a branch
        // of the `{% if %}`, never a second `{% endif %}`
        let completions =
            project("{% for b in shelf %}{% if b %}{% <CURSOR> %}{% endif %}{% endfor %}")
                .completions();

        assert_eq!(&completions[..2], ["elif", "else"]);
    }

    #[test]
    fn a_closed_block_does_not_offer_its_closing_tag_again() {
        let completions = project("{% if x %}{% endif %}{% <CURSOR> %}").completions();
        assert_ne!(completions[0], "endif");
    }

    #[test]
    fn a_partially_typed_tag_name_is_replaced_whole() {
        let source = "{% ext<CURSOR> %}";
        let test = project(source);
        let completions = django_template_completions(&test.db, test.file, test.offset);

        let extends = completions
            .iter()
            .find(|completion| completion.label == "extends")
            .expect("`extends` to be offered");

        assert_eq!(
            &"{% ext %}"[usize::from(extends.range.start())..usize::from(extends.range.end())],
            "ext"
        );
    }

    #[test]
    fn the_word_after_a_pipe_offers_filters() {
        let completions = project("{{ book.title|<CURSOR> }}").completions();

        assert!(completions.contains(&"upper".to_string()));
        assert!(completions.contains(&"truncatewords".to_string()));
        assert!(
            completions.contains(&"shout".to_string()),
            "the project's own filter is offered too"
        );
        assert!(
            !completions.contains(&"book_count".to_string()),
            "a tag is not a filter"
        );
    }

    #[test]
    fn a_tag_from_an_unloaded_library_carries_the_load_it_needs() {
        let test = project("{% <CURSOR> %}");
        let completions = django_template_completions(&test.db, test.file, test.offset);

        let edit = completions
            .iter()
            .find(|completion| completion.label == "static")
            .and_then(|completion| completion.additional_edit.clone())
            .expect("`{% static %}` to come with the load it needs");

        assert_eq!(edit.text, "{% load static %}\n");
        assert!(
            edit.range.is_empty(),
            "the load is inserted, not a replacement"
        );
        assert_eq!(u32::from(edit.range.start()), 0, "at the top of the file");
    }

    #[test]
    fn a_load_is_written_below_an_extends_rather_than_above_it() {
        let source = "{% extends 'blog/base.html' %}\n{% <CURSOR> %}";
        let test = project(source);
        let completions = django_template_completions(&test.db, test.file, test.offset);

        let edit = completions
            .iter()
            .find(|completion| completion.label == "static")
            .and_then(|completion| completion.additional_edit.clone())
            .expect("`{% static %}` to come with the load it needs");

        assert_eq!(edit.text, "\n{% load static %}");
        assert_eq!(
            &source[..usize::from(edit.range.start())],
            "{% extends 'blog/base.html' %}"
        );
    }

    #[test]
    fn an_already_loaded_library_needs_no_load() {
        let test = project("{% load static %}{% <CURSOR> %}");
        let completions = django_template_completions(&test.db, test.file, test.offset);

        let static_tag = completions
            .iter()
            .find(|completion| completion.label == "static")
            .expect("`static` to be offered");
        assert!(static_tag.additional_edit.is_none());
    }

    #[test]
    fn extends_offers_the_projects_templates() {
        let completions = project("{% extends '<CURSOR>' %}").completions();
        assert_eq!(completions, ["blog/base.html", "blog/post.html"]);
    }

    #[test]
    fn include_offers_the_projects_templates() {
        let completions = project("{% include \"<CURSOR>\" %}").completions();
        assert!(completions.contains(&"blog/base.html".to_string()));
    }

    #[test]
    fn a_template_path_completion_replaces_the_string_but_not_its_quotes() {
        let source = "{% extends 'blog/<CURSOR>' %}";
        let test = project(source);
        let completions = django_template_completions(&test.db, test.file, test.offset);

        let range = completions[0].range;
        assert_eq!(&source[usize::from(range.start())..], "blog/<CURSOR>' %}");
    }

    #[test]
    fn url_offers_the_projects_route_names_namespaced() {
        let completions = project("{% url '<CURSOR>' %}").detailed();
        assert_eq!(
            completions,
            ["blog:detail — books/<int:pk>/", "blog:index — books/"]
        );
    }

    #[test]
    fn static_offers_the_projects_assets() {
        let completions = project("{% load static %}{% static '<CURSOR>' %}").completions();
        assert_eq!(completions, ["blog/app.css"]);
    }

    #[test]
    fn load_offers_the_libraries_not_yet_loaded() {
        let completions = project("{% load <CURSOR> %}").detailed();

        assert!(completions.contains(&"static — django".to_string()));
        assert!(completions.contains(&"blog_extras — this project".to_string()));
    }

    #[test]
    fn load_does_not_offer_a_library_already_loaded() {
        let completions = project("{% load i18n %}{% load <CURSOR> %}").completions();
        assert!(!completions.contains(&"i18n".to_string()));
    }

    #[test]
    fn a_bare_name_offers_the_views_context() {
        let completions = project("{{ <CURSOR> }}").completions();
        assert_eq!(completions, ["book", "shelf"]);
    }

    #[test]
    fn a_bare_name_offers_the_templates_own_bindings_first() {
        let completions =
            project("{% for book in shelf %}{{ <CURSOR> }}{% endfor %}").completions();
        assert_eq!(completions, ["book", "forloop", "shelf"]);
    }

    #[test]
    fn a_context_variable_shows_the_type_the_view_gives_it() {
        let completions = project("{{ <CURSOR> }}").detailed();
        assert_eq!(completions[0], "book — Book");
    }

    #[test]
    fn the_word_after_a_dot_offers_the_types_attributes() {
        let completions = project("{{ book.<CURSOR> }}").detailed();
        assert_eq!(completions, ["author — Author", "title — str"]);
    }

    #[test]
    fn attributes_are_followed_through_a_whole_path() {
        let completions = project("{{ book.author.<CURSOR> }}").detailed();
        assert_eq!(completions, ["email — str", "name — str"]);
    }

    #[test]
    fn a_loop_variable_takes_the_element_type_of_what_it_loops_over() {
        let completions =
            project("{% for entry in shelf %}{{ entry.<CURSOR> }}{% endfor %}").detailed();
        assert_eq!(completions, ["author — Author", "title — str"]);
    }

    #[test]
    fn a_with_alias_carries_the_type_of_the_path_it_binds() {
        let completions =
            project("{% with writer=book.author %}{{ writer.<CURSOR> }}{% endwith %}").detailed();
        assert_eq!(completions, ["email — str", "name — str"]);
    }

    #[test]
    fn an_unresolvable_path_offers_nothing_rather_than_guessing() {
        assert!(project("{{ mystery.<CURSOR> }}").completions().is_empty());
    }

    #[test]
    fn partial_offers_the_fragments_this_template_defines() {
        let completions =
            project("{% partialdef card %}{% endpartialdef %}{% partial <CURSOR> %}").completions();
        assert_eq!(completions, ["card"]);
    }

    #[test]
    fn block_offers_the_blocks_of_the_template_being_extended() {
        let completions =
            project("{% extends 'blog/base.html' %}{% block <CURSOR> %}").completions();
        assert_eq!(completions, ["content"]);
    }

    #[test]
    fn block_does_not_offer_a_block_this_template_already_overrides() {
        let completions = project(
            "{% extends 'blog/base.html' %}{% block content %}{% endblock %}{% block <CURSOR> %}",
        )
        .completions();
        assert!(completions.is_empty());
    }
}
