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

use super::builtins::{self, Provided};
use super::index::{Block, TemplateIndex};
use super::lexer::{Construct, ConstructKind, Token, TokenKind, string_contents};
use super::project::{self, LibrarySource, Registration, RegistrationKind};
use super::resolve;
use super::uses::URL_TAG;

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
        Context::RouteArgument(route) => route_arguments(db, file, index, offset, &cursor, &route),
        Context::Library => libraries(db, index, &cursor),
        Context::BlockName => block_names(db, file, index, &cursor),
        Context::PartialName => partial_names(db, file, index, &cursor),
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
    /// an argument the named route takes, in a `{% url %}` that has named one
    RouteArgument(CompactString),
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
        let tokens = index.lexed().inner_tokens(construct);

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

    /// whether the cursor is inside a string literal
    ///
    /// the argument naming a template, a route or an asset has to be quoted, so
    /// a suggestion offered where there is no literal yet has to bring the quotes
    /// along with it.
    fn in_string(&self) -> bool {
        self.current
            .and_then(|index| self.tokens.get(index))
            .is_some_and(|token| token.kind == TokenKind::String)
    }

    /// the index in `tokens` of the token being typed, or of the position the
    /// cursor would insert one at
    fn index(&self) -> usize {
        match self.current {
            Some(index) => index,
            None => self
                .tokens
                .iter()
                .position(|token| token.range.start() >= self.offset)
                .unwrap_or(self.tokens.len()),
        }
    }

    /// the token before the cursor, skipping the one being typed
    fn previous(&self) -> Option<&'a Token> {
        self.tokens.get(self.index().checked_sub(1)?)
    }

    /// how many argument tokens the cursor is preceded by
    ///
    /// the tag name is not one of them, so the first argument of `{% url %}` is
    /// at position zero.
    fn argument_position(&self) -> usize {
        let tag_name = usize::from(self.construct.name.is_some());
        self.index().saturating_sub(tag_name)
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
                (URL_TAG, 0) => return Context::UrlName,
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
            _ => match self.route() {
                Some(route) => Context::RouteArgument(route),
                None => Context::Variable,
            },
        }
    }

    /// the route a `{% url %}` names, where the cursor is starting an argument
    /// to it
    ///
    /// only a position that begins a fresh argument counts: what follows a
    /// `name=` is that argument's value rather than another argument's name, and
    /// what follows an `as` names the tag's result rather than its input. a
    /// quoted position is a value too, since a route takes its arguments by bare
    /// name.
    fn route(&self) -> Option<CompactString> {
        if self.construct.kind != ConstructKind::Tag
            || self.tag() != URL_TAG
            || self.argument_position() == 0
            || self.in_string()
        {
            return None;
        }

        let before = self.tokens.get(..self.index())?;
        if before.iter().any(|token| self.is_keyword(token, "as"))
            || before
                .last()
                .is_some_and(|token| token.kind == TokenKind::Operator)
        {
            return None;
        }

        let name = before
            .iter()
            .find(|token| token.kind == TokenKind::String)?;
        Some(self.source[string_contents(self.source, name.range)].to_compact_string())
    }

    /// the arguments a `{% url %}` already passes, as the names it gives and
    /// whether it passes anything by position
    ///
    /// the token being typed is not one of them: a name half written is one the
    /// user is still choosing.
    fn arguments_given(&self) -> (FxHashSet<CompactString>, bool) {
        let mut named = FxHashSet::default();
        let mut positional = false;
        // the tag's own name and the route it reverses come before its arguments
        let start = usize::from(self.construct.name.is_some()) + 1;

        for (index, token) in self.tokens.iter().enumerate().skip(start) {
            if self.is_keyword(token, "as") {
                break;
            }
            if Some(index) == self.current {
                continue;
            }

            match token.kind {
                TokenKind::KeywordArgument => {
                    named.insert(self.source[token.range].to_compact_string());
                }
                // a value joined to what precedes it by an operator belongs to
                // that argument rather than starting one of its own, which is
                // what tells `pk=book.pk` from a positional `book.pk`
                TokenKind::String
                | TokenKind::Number
                | TokenKind::Variable
                | TokenKind::BuiltinConstant => {
                    positional |= !self.tokens[..index]
                        .last()
                        .is_some_and(|previous| previous.kind == TokenKind::Operator);
                }
                _ => {}
            }
        }

        (named, positional)
    }

    fn is_keyword(&self, token: &Token, word: &str) -> bool {
        token.kind == TokenKind::Keyword && self.source[token.range] == *word
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

/// the text a suggestion for a quoted argument inserts, when it differs from
/// the label
///
/// `{% extends '<CURSOR>' %}` needs the name alone; `{% extends <CURSOR> %}`
/// needs it quoted, or accepting the suggestion writes something django reads as
/// a variable.
fn quoted_insert(label: &str, cursor: &Cursor<'_>) -> Option<String> {
    (!cursor.in_string()).then(|| format!("'{label}'"))
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

    let loaded = loaded_libraries(db, index);
    let load_edit = |library: Option<&str>| load_edit_for(index, &loaded, library);
    let registrations = project::registrations(db, db.project());

    for tag in builtins::TAGS {
        // a django that was read and does not register it is a django it is not
        // in, and offering it would be offering a name django has no tag for
        let Some(library) =
            builtins::provided_by_django(db, tag.name, false).map(Provided::library)
        else {
            continue;
        };

        let mut completion =
            TemplateCompletion::new(tag.name, CompletionKind::Keyword, cursor.range)
                .documentation(tag.documentation);
        if let Some(library) = library {
            completion = completion.detail(format!("{{% load {library} %}}"));
        }
        completion.additional_edit = load_edit(library);
        completions.push(completion);
    }

    for registration in registrations {
        if registration.kind == RegistrationKind::Filter
            || offered_by_the_table(registration, false)
        {
            continue;
        }

        let library = needs_loading(registration);
        let mut completion = TemplateCompletion::new(
            registration.name.as_str(),
            CompletionKind::Keyword,
            cursor.range,
        );
        if let Some(library) = library {
            completion = completion.detail(format!("{{% load {library} %}}"));
        }
        completion.documentation = registration
            .documentation
            .as_deref()
            .map(ToString::to_string);
        completion.additional_edit = load_edit(library);
        completions.push(completion);
    }

    completions
}

/// the library a registration's template has to load first, where there is one
///
/// django's implicit builtins are a library like any other here, and a `{% load
/// defaulttags %}` is not something anybody should be shown.
fn needs_loading(registration: &Registration) -> Option<&str> {
    (!registration.always_loaded).then(|| registration.library.as_str())
}

/// how an open block's tag reads, for a closing tag's detail line
fn opening_tag(source: &str, block: &Block) -> String {
    source[block.open_range].trim().to_string()
}

/// whether the builtin table already offers this registration
///
/// django's own libraries are discovered *and* tabulated, and the table's entry
/// is the richer of the two — it carries the documentation, and for a tag the
/// block structure as well — so it is the one offered.
fn offered_by_the_table(registration: &Registration, filter: bool) -> bool {
    registration.django
        && if filter {
            builtins::filter(&registration.name).is_some()
        } else {
            builtins::tag(&registration.name).is_some()
        }
}

fn filter_names(
    db: &dyn Db,
    index: &TemplateIndex,
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    let loaded = loaded_libraries(db, index);
    let mut completions = Vec::new();
    let registrations = project::registrations(db, db.project());

    for filter in builtins::FILTERS {
        let Some(library) =
            builtins::provided_by_django(db, filter.name, true).map(Provided::library)
        else {
            continue;
        };

        let mut completion =
            TemplateCompletion::new(filter.name, CompletionKind::Function, cursor.range)
                .documentation(filter.documentation);
        if let Some(library) = library {
            completion = completion.detail(format!("{{% load {library} %}}"));
        }
        completion.additional_edit = load_edit_for(index, &loaded, library);
        completions.push(completion);
    }

    for registration in registrations {
        if registration.kind != RegistrationKind::Filter || offered_by_the_table(registration, true)
        {
            continue;
        }

        let library = needs_loading(registration);
        let mut completion = TemplateCompletion::new(
            registration.name.as_str(),
            CompletionKind::Function,
            cursor.range,
        );
        if let Some(library) = library {
            completion = completion.detail(format!("{{% load {library} %}}"));
        }
        completion.documentation = registration
            .documentation
            .as_deref()
            .map(ToString::to_string);
        completion.additional_edit = load_edit_for(index, &loaded, library);
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
            .or_else(|| Some(variable.source.description().to_string()));
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
            let mut completion =
                TemplateCompletion::new(file.name.as_str(), CompletionKind::File, cursor.range)
                    .detail(file.path.as_str());
            completion.insert = quoted_insert(&file.name, cursor);
            completion
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
            completion.insert = quoted_insert(&url.name, cursor);
            completion
        })
        .collect()
}

/// the arguments the route a `{% url %}` names still takes
///
/// an argument can be passed by position as readily as by name, so the
/// template's own variables are offered alongside the names rather than instead
/// of them.
fn route_arguments(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    offset: TextSize,
    cursor: &Cursor<'_>,
    route: &str,
) -> Vec<TemplateCompletion> {
    let mut completions = Vec::new();
    let (named, positional) = cursor.arguments_given();

    // django reverses by position or by name and refuses a mixture, so a tag
    // that has already passed something positionally has no name left to take
    if !positional {
        for parameter in project::route_parameters(db, route) {
            if named.contains(&parameter.name) {
                continue;
            }

            let mut completion = TemplateCompletion::new(
                parameter.name.as_str(),
                CompletionKind::Field,
                cursor.range,
            );
            completion.detail = parameter
                .converter
                .map(|converter| converter.name().to_string());
            completion.insert = Some(format!("{}=", parameter.name));
            completions.push(completion);
        }
    }

    completions.extend(variables(db, file, index, offset, cursor));
    completions
}

fn libraries(db: &dyn Db, index: &TemplateIndex, cursor: &Cursor<'_>) -> Vec<TemplateCompletion> {
    let loaded = loaded_libraries(db, index);
    let mut completions = Vec::new();
    let mut seen = FxHashSet::default();

    // the table's list is a fallback like the tables themselves: where django was
    // read, the libraries it ships are discovered below and are the right list
    if !project::django_is_authoritative(db, db.project()) {
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
    }

    for library in project::tag_libraries(db, db.project()) {
        if loaded.contains(&library.name) || !seen.insert(library.name.clone()) {
            continue;
        }
        completions.push(
            TemplateCompletion::new(library.name.as_str(), CompletionKind::Module, cursor.range)
                .detail(match library.source {
                    LibrarySource::Django => "django",
                    LibrarySource::Project => "this project",
                    LibrarySource::Installed => "installed",
                }),
        );
    }

    completions
}

fn block_names(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    // a `{% block %}` in a child template is only useful when it overrides one of
    // the parent's, so those are exactly what is offered
    let defined: FxHashSet<_> = index
        .blocks()
        .iter()
        .map(|block| block.name.clone())
        .collect();

    inherited(db, file, index, |parent| {
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
    file: File,
    index: &TemplateIndex,
    cursor: &Cursor<'_>,
) -> Vec<TemplateCompletion> {
    let mut names: Vec<CompactString> = index
        .partials()
        .iter()
        .map(|partial| partial.name.clone())
        .collect();

    // a partial defined by a template this one extends is in scope here too
    names.extend(inherited(db, file, index, |parent| {
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
    file: File,
    index: &TemplateIndex,
    names: impl Fn(&TemplateIndex) -> Vec<CompactString>,
) -> Vec<CompactString> {
    super::ancestors(db, file, index)
        .into_iter()
        .flat_map(|(_, parent)| names(parent))
        .collect()
}

/// the libraries the template does not have to `{% load %}`
///
/// the ones it has loaded already, and the ones the settings load into every
/// template — a `{% load %}` for one of those is not wrong, but it is noise, so
/// neither is one suggested.
pub(super) fn loaded_libraries(db: &dyn Db, index: &TemplateIndex) -> FxHashSet<CompactString> {
    index
        .loads()
        .iter()
        .map(|load| load.library.clone())
        .chain(
            project::tag_libraries(db, db.project())
                .iter()
                .filter(|library| library.always_loaded)
                .map(|library| library.name.clone()),
        )
        .collect()
}

/// the `{% load %}` a suggestion from `library` needs, when it isn't loaded yet
pub(super) fn load_edit_for(
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
    use crate::django_template::tests::{DJANGO_BUILTINS, TemplateTest};

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

                class Chapter:
                    title: str
                    number: int

                # stands in for `models.Model`: a framework base that brings a
                # pile of machinery with it
                class Model:
                    def save(self) -> None: ...
                    def delete(self) -> None: ...

                class Novel(Model):
                    title: str

                    def chapters(self) -> list[Chapter]: ...
                    def rename(self, title: str) -> str: ...
                ",
            ),
            (
                "blog/views.py",
                "
                from blog.models import Book, Novel

                def post(request):
                    book = Book()
                    return render(request, 'blog/post.html', {'book': book, 'shelf': [book], 'novel': Novel()})
                ",
            ),
            (
                "blog/urls.py",
                "
                app_name = 'blog'

                urlpatterns = [
                    path('books/<int:pk>/', detail, name='detail'),
                    path('books/', index, name='index'),
                    path('books/<slug:slug>/<int:page>/', paged, name='paged'),
                    re_path(r'^archive/(?P<year>[0-9]{4})/$', archive, name='archive'),
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

    /// the same project with a mock django installed beside it, the `humanize`
    /// contrib app among its `INSTALLED_APPS`
    ///
    /// `options` goes into the one template engine, which is how a project asks
    /// for a library to be loaded into every template.
    fn with_humanize(template: &str, options: &str) -> TemplateTest {
        let settings = format!(
            "
            INSTALLED_APPS = ['django.contrib.humanize', 'blog']

            TEMPLATES = [{{'APP_DIRS': True, 'OPTIONS': {{{options}}}}}]
            "
        );

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
                ("project/settings.py", &settings),
                ("blog/__init__.py", ""),
                ("blog/templatetags/__init__.py", ""),
                (
                    "blog/templatetags/blog_extras.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.filter
                    def shout(value):
                        return value
                    ",
                ),
                ("blog/templates/blog/post.html", template),
            ],
            &[
                ("django/__init__.py", ""),
                ("django/contrib/__init__.py", ""),
                ("django/contrib/humanize/__init__.py", ""),
                ("django/contrib/humanize/templatetags/__init__.py", ""),
                (
                    "django/contrib/humanize/templatetags/humanize.py",
                    "
                    from django.template import Library

                    register = Library()

                    @register.filter
                    def intcomma(value):
                        '''adds thousand separators.'''
                        return value
                    ",
                ),
            ],
        )
    }

    /// a project whose settings name two of django's own context processors
    ///
    /// what a processor returns is in scope in every template the project
    /// renders, which is what makes `{{ user }}` complete in a template no view
    /// ever mentions it to. `context` is the dict the one view passes.
    fn with_processors(template: &str, context: &str) -> TemplateTest {
        let view = format!(
            "
            from blog.models import Book

            def post(request):
                return render(request, 'blog/post.html', {context})
            "
        );

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

                    TEMPLATES = [{'APP_DIRS': True, 'OPTIONS': {'context_processors': [
                        'django.template.context_processors.request',
                        'django.contrib.auth.context_processors.auth',
                    ]}}]
                    ",
                ),
                ("blog/__init__.py", ""),
                (
                    "blog/models.py",
                    "
                    class Book:
                        title: str
                    ",
                ),
                ("blog/views.py", &view),
                ("blog/templates/blog/post.html", template),
            ],
            &[
                ("django/__init__.py", ""),
                ("django/template/__init__.py", ""),
                (
                    "django/template/context_processors.py",
                    "
                    def request(request):
                        return {'request': request, 'site': 'a blog'}
                    ",
                ),
                ("django/contrib/__init__.py", ""),
                ("django/contrib/auth/__init__.py", ""),
                (
                    "django/contrib/auth/context_processors.py",
                    "
                    class User:
                        username: str

                    class PermWrapper:
                        def __init__(self, user): ...

                    def auth(request):
                        user = User()
                        return {'user': user, 'perms': PermWrapper(user)}
                    ",
                ),
            ],
        )
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
    fn a_template_path_offered_outside_a_literal_brings_its_quotes() {
        let test = project("{% extends <CURSOR> %}");
        let completions = django_template_completions(&test.db, test.file, test.offset);

        let first = completions.first().expect("a template to be offered");
        assert_eq!(first.label, "blog/base.html");
        assert_eq!(
            first.insert.as_deref(),
            Some("'blog/base.html'"),
            "an unquoted path would be read as a variable"
        );
    }

    #[test]
    fn a_template_path_offered_inside_a_literal_does_not() {
        let test = project("{% extends '<CURSOR>' %}");
        let completions = django_template_completions(&test.db, test.file, test.offset);

        assert_eq!(completions[0].insert, None);
    }

    #[test]
    fn a_url_name_offered_outside_a_literal_brings_its_quotes() {
        let test = project("{% url <CURSOR> %}");
        let completions = django_template_completions(&test.db, test.file, test.offset);

        let first = completions.first().expect("a route to be offered");
        assert_eq!(first.insert.as_deref(), Some("'blog:detail'"));
    }

    #[test]
    fn url_offers_the_projects_route_names_namespaced() {
        let completions = project("{% url '<CURSOR>' %}").detailed();
        assert_eq!(
            completions,
            [
                "blog:detail — books/<int:pk>/",
                "blog:index — books/",
                "blog:paged — books/<slug:slug>/<int:page>/",
                "blog:archive — ^archive/(?P<year>[0-9]{4})/$"
            ]
        );
    }

    #[test]
    fn url_offers_the_routes_a_rest_framework_router_generates() {
        let test = TemplateTest::new(&[
            (
                "api/views.py",
                "
                class BookViewSet:
                    queryset = Book.objects.all()

                    @action(detail=True)
                    def mark_read(self, request, pk=None): ...
                ",
            ),
            (
                "api/urls.py",
                "
                from api.views import BookViewSet

                router = DefaultRouter()
                router.register('books', BookViewSet)

                urlpatterns = router.urls
                ",
            ),
            ("blog/templates/blog/post.html", "{% url '<CURSOR>' %}"),
        ]);

        assert_eq!(
            test.detailed(),
            [
                "api-root",
                "book-list — books",
                "book-detail — books",
                "book-mark-read — books"
            ]
        );
    }

    #[test]
    fn url_offers_the_arguments_the_named_route_takes() {
        let completions = project("{% url 'blog:detail' <CURSOR> %}").detailed();
        assert_eq!(completions[0], "pk — int");
    }

    #[test]
    fn a_route_argument_is_offered_with_the_equals_that_names_it() {
        let test = project("{% url 'blog:detail' <CURSOR> %}");
        let completions = django_template_completions(&test.db, test.file, test.offset);

        assert_eq!(completions[0].insert.as_deref(), Some("pk="));
    }

    #[test]
    fn url_offers_every_argument_a_route_takes() {
        let completions = project("{% url 'blog:paged' <CURSOR> %}").detailed();
        assert_eq!(&completions[..2], ["slug — slug", "page — int"]);
    }

    #[test]
    fn url_does_not_offer_an_argument_already_given() {
        let completions = project("{% url 'blog:paged' slug='a' <CURSOR> %}").detailed();
        assert_eq!(completions[0], "page — int");
    }

    #[test]
    fn url_offers_an_argument_the_cursor_is_rewriting() {
        let completions = project("{% url 'blog:paged' sl<CURSOR> page=2 %}").detailed();
        assert_eq!(completions[0], "slug — slug");
    }

    #[test]
    fn url_offers_the_argument_of_a_re_path_route_without_a_converter() {
        let completions = project("{% url 'blog:archive' <CURSOR> %}").detailed();
        assert_eq!(completions[0], "year");
    }

    #[test]
    fn url_offers_no_argument_for_a_route_that_takes_none() {
        // the template's own variables are still offered: they are what a
        // position accepts even where no argument is named
        let completions = project("{% url 'blog:index' <CURSOR> %}").detailed();
        assert!(
            !completions
                .iter()
                .any(|completion| completion == "pk — int")
        );
        assert_eq!(completions.first().map(String::as_str), Some("book — Book"));
    }

    #[test]
    fn url_offers_no_argument_for_a_route_that_is_not_there() {
        let completions = project("{% url 'blog:missing' <CURSOR> %}").detailed();
        assert_eq!(completions.first().map(String::as_str), Some("book — Book"));
    }

    #[test]
    fn url_offers_no_argument_once_one_is_given_by_position() {
        // django reverses by position or by name and refuses a mixture
        let completions = project("{% url 'blog:paged' book.title <CURSOR> %}").detailed();
        assert!(
            !completions
                .iter()
                .any(|completion| completion == "page — int")
        );
    }

    #[test]
    fn url_offers_the_variables_a_positional_argument_could_be() {
        let completions = project("{% url 'blog:detail' <CURSOR> %}").completions();
        assert!(completions.contains(&"book".to_string()));
    }

    #[test]
    fn url_offers_no_argument_where_a_value_goes() {
        let completions = project("{% url 'blog:detail' pk=<CURSOR> %}").detailed();
        assert_eq!(completions.first().map(String::as_str), Some("book — Book"));
    }

    #[test]
    fn url_offers_no_argument_for_the_name_it_binds_its_result_to() {
        let completions = project("{% url 'blog:detail' pk=1 as <CURSOR> %}").detailed();
        assert!(
            !completions
                .iter()
                .any(|completion| completion == "pk — int")
        );
    }

    #[test]
    fn url_offers_no_argument_inside_a_quoted_position() {
        let completions = project("{% url 'blog:detail' '<CURSOR>' %}").detailed();
        assert!(
            !completions
                .iter()
                .any(|completion| completion == "pk — int")
        );
    }

    #[test]
    fn url_offers_the_arguments_of_every_route_that_shares_the_name() {
        // django allows two routes one name and reverses against whichever of
        // them matches, so what the name takes is the union of the two
        let test = TemplateTest::new(&[
            (
                "blog/urls.py",
                "
                app_name = 'blog'

                urlpatterns = [
                    path('shelf/<int:pk>/', shelf, name='shelf'),
                    path('shelf/<slug:slug>/', shelf, name='shelf'),
                ]
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:shelf' <CURSOR> %}",
            ),
        ]);

        assert_eq!(test.detailed(), ["pk — int", "slug — slug"]);
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
    fn load_offers_an_installed_apps_library() {
        let completions = with_humanize("{% load <CURSOR> %}", "").detailed();

        assert!(completions.contains(&"humanize — django".to_string()));
        assert!(completions.contains(&"blog_extras — this project".to_string()));
    }

    #[test]
    fn a_filter_from_an_installed_apps_library_is_offered_with_the_load_it_needs() {
        let test = with_humanize("{% load humanize %}{{ x|<CURSOR> }}", "");

        assert!(
            test.detailed()
                .contains(&"intcomma — {% load humanize %}".to_string()),
            "django's own `humanize` filter is as available as the table's are"
        );

        let edit = django_template_completions(&test.db, test.file, test.offset)
            .into_iter()
            .find(|completion| completion.label == "intcomma")
            .and_then(|completion| completion.additional_edit);
        assert!(
            edit.is_none(),
            "the template loaded it already, so no second `{{% load %}}` is written"
        );
    }

    #[test]
    fn a_filter_from_an_unloaded_installed_library_brings_its_load_with_it() {
        let test = with_humanize("{{ x|<CURSOR> }}", "");

        let edit = django_template_completions(&test.db, test.file, test.offset)
            .into_iter()
            .find(|completion| completion.label == "intcomma")
            .and_then(|completion| completion.additional_edit)
            .expect("`intcomma` to come with the load it needs");
        assert_eq!(edit.text, "{% load humanize %}\n");
    }

    #[test]
    fn a_library_loaded_into_every_template_is_neither_offered_nor_loaded() {
        let options = "'builtins': ['django.contrib.humanize.templatetags.humanize']";

        assert!(
            !with_humanize("{% load <CURSOR> %}", options)
                .completions()
                .contains(&"humanize".to_string()),
            "every template has it already, so a `{{% load %}}` for it is noise"
        );

        let test = with_humanize("{{ x|<CURSOR> }}", options);
        let intcomma = django_template_completions(&test.db, test.file, test.offset)
            .into_iter()
            .find(|completion| completion.label == "intcomma")
            .expect("the filter to be offered without a `{% load %}`");
        assert!(intcomma.additional_edit.is_none());
    }

    #[test]
    fn a_bare_name_offers_the_views_context() {
        let completions = project("{{ <CURSOR> }}").completions();
        assert_eq!(completions, ["book", "shelf", "novel"]);
    }

    #[test]
    fn a_bare_name_offers_the_templates_own_bindings_first() {
        let completions =
            project("{% for book in shelf %}{{ <CURSOR> }}{% endfor %}").completions();
        assert_eq!(completions, ["book", "forloop", "shelf", "novel"]);
    }

    #[test]
    fn a_bare_name_offers_the_context_processors_names_after_the_views() {
        let completions = with_processors("{{ <CURSOR> }}", "{'book': Book()}").completions();
        assert_eq!(completions, ["book", "request", "site", "user", "perms"]);
    }

    #[test]
    fn a_processors_name_that_has_no_type_still_says_where_it_comes_from() {
        let completions = with_processors("{{ <CURSOR> }}", "{'book': Book()}").detailed();
        assert!(
            completions.contains(&"site — from a context processor".to_string()),
            "got {completions:?}"
        );
    }

    #[test]
    fn a_context_processors_name_carries_its_type() {
        let completions = with_processors("{{ user.<CURSOR> }}", "{'book': Book()}").detailed();
        assert_eq!(completions, ["username — str"]);
    }

    #[test]
    fn a_name_the_view_supplies_shadows_the_context_processors() {
        let test = with_processors("{{ user.<CURSOR> }}", "{'book': Book(), 'user': Book()}");
        assert_eq!(
            test.detailed(),
            ["title — str"],
            "the view's `user` is what django renders with"
        );

        let offered = with_processors("{{ <CURSOR> }}", "{'book': Book(), 'user': Book()}")
            .completions()
            .iter()
            .filter(|label| *label == "user")
            .count();
        assert_eq!(offered, 1, "and it is offered once");
    }

    #[test]
    fn a_name_a_tag_binds_shadows_the_context_processors() {
        let completions = with_processors(
            "{% with user=book %}{{ user.<CURSOR> }}{% endwith %}",
            "{'book': Book()}",
        )
        .detailed();

        assert_eq!(completions, ["title — str"]);
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
    fn a_no_argument_method_is_called_the_way_django_calls_it() {
        // django's variable lookup calls whatever it lands on, so `book.chapters`
        // is the list the method returns, not the method
        let completions = project("{{ novel.chapters.<CURSOR> }}").completions();
        assert!(
            completions.contains(&"append".to_string()),
            "got {completions:?}"
        );
    }

    #[test]
    fn a_loop_over_a_called_method_binds_the_element_type() {
        let completions =
            project("{% for c in novel.chapters %}{{ c.<CURSOR> }}{% endfor %}").detailed();
        assert_eq!(completions, ["number — int", "title — str"]);
    }

    #[test]
    fn a_method_that_needs_an_argument_is_not_called() {
        // `book.rename(title)` takes an argument, so django would render nothing
        // rather than call it, and there is no return type to offer
        assert!(
            project("{{ novel.rename.<CURSOR> }}")
                .completions()
                .is_empty()
        );
    }

    #[test]
    fn a_types_own_members_come_before_the_ones_it_inherits() {
        // the base stands in for `models.Model`: what a template is written
        // against is the subclass' own fields, and burying them alphabetically
        // among a framework's machinery is nearly the same as not offering them
        let completions = project("{{ novel.<CURSOR> }}").completions();
        let own = ["chapters", "rename", "title"];

        let last_own = own
            .iter()
            .map(|name| completions.iter().position(|c| c == name).expect(name))
            .max()
            .unwrap();
        let first_inherited = completions
            .iter()
            .position(|c| c == "save")
            .expect("the inherited member to be offered too");

        assert!(
            last_own < first_inherited,
            "own members must lead: {completions:?}"
        );
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

    /// a project whose django can be read all the way down to its implicit
    /// builtins, which register something other than what the table says
    fn with_djangos_own_builtins(template: &str) -> TemplateTest {
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
                    INSTALLED_APPS = []

                    TEMPLATES = [{'APP_DIRS': True}]
                    ",
                ),
                ("app/templates/app/page.html", template),
            ],
            DJANGO_BUILTINS,
        )
    }

    #[test]
    fn a_tag_this_django_registers_that_the_table_never_heard_of_is_offered() {
        let test = with_djangos_own_builtins("{% <CURSOR> %}");

        assert!(
            test.detailed().contains(&"squish".to_string()),
            "django registers it into every template, so it is offered with no `{{% load %}}`"
        );

        let squish = django_template_completions(&test.db, test.file, test.offset)
            .into_iter()
            .find(|completion| completion.label == "squish")
            .expect("the tag to be offered");
        assert!(squish.additional_edit.is_none());
        assert_eq!(squish.documentation.as_deref(), Some("squishes its body."));
    }

    #[test]
    fn a_filter_this_django_registers_that_the_table_never_heard_of_is_offered() {
        assert!(
            with_djangos_own_builtins("{{ x|<CURSOR> }}")
                .detailed()
                .contains(&"shorten".to_string())
        );
    }

    #[test]
    fn a_name_the_table_has_that_this_django_does_not_register_is_not_offered() {
        assert!(
            !with_djangos_own_builtins("{% <CURSOR> %}")
                .completions()
                .contains(&"lorem".to_string()),
            "the table's entry is not evidence about a django that has been read"
        );
        assert!(
            !with_djangos_own_builtins("{{ x|<CURSOR> }}")
                .completions()
                .contains(&"slugify".to_string())
        );
    }

    #[test]
    fn the_table_is_offered_in_full_where_django_cannot_be_read() {
        assert!(
            project("{% <CURSOR> %}")
                .completions()
                .contains(&"lorem".to_string())
        );
        assert!(
            project("{{ x|<CURSOR> }}")
                .completions()
                .contains(&"slugify".to_string())
        );
    }
}
