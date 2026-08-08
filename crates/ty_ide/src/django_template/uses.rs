//! every place the project writes a django name
//!
//! two features ask this question and want opposite things from the answer. a
//! rename must rewrite every occurrence or none of them: one left behind is a
//! project that no longer renders, and nothing about the result looks wrong.
//! references only has to *say* where the name is written, and an occurrence
//! nothing could rewrite is still one worth knowing about.
//!
//! so the scans live here once, and every occurrence carries what it is — the
//! name written out or worked out at run time, the project's own file or an
//! installed app's, a position a rewrite knows or one it does not. the two
//! features read that differently, and deliberately: [`Written`] is where the
//! difference is written down, and a later reader tempted to make one of them
//! agree with the other should read it first.
//!
//! what counts as evidence of a name at all is the one thing this is careful not
//! to overdo. it is a position — one of the calls and attributes [`super::python`]
//! reads, a template construct, or a constant a name is bound to, since a
//! constant carries a name somewhere nothing here can follow. it is deliberately
//! not "any literal spelling the same text": `detail`, `content` and `index` are
//! among the commonest strings in any codebase, and an `item.get("detail")` in
//! unrelated code is nothing to do with a route.

use compact_str::{CompactString, ToCompactString};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::source::{SourceText, source_text};
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::find_node::covering_node;
use ruff_text_size::{TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_project::Db;

use super::index::{Definition, TemplateIndex};
use super::lexer::{Construct, ConstructKind, Token, TokenKind, string_contents};
use super::project::{self, DiscoveredFile, RegistrationKind};
use super::{MAX_INHERITANCE_DEPTH, template_index};

/// the tag that reverses a route
pub(super) const URL_TAG: &str = "url";

/// the tags that name another template
pub(super) const TEMPLATE_TAGS: &[&str] = &["extends", "include"];

/// the tag that closes a `{% block %}`
const END_BLOCK_TAG: &str = "endblock";

/// the fragment separator a template reference addresses a partial with
pub(super) const FRAGMENT_SEPARATOR: char = '#';

/// which django name a position writes
pub(super) enum Named {
    /// a `{% block %}` name
    Block(CompactString),
    /// a route, under the name it is reversed by
    Route(CompactString),
    /// a route's own `name=`
    RouteDeclaration,
    /// a template, under the name its loader knows it by
    Template(CompactString),
    /// a tag's name, as a template writes it
    Tag(CompactString),
    /// a filter's name, as a template writes it
    Filter(CompactString),
}

/// one place a django name reaches
pub(super) struct Use {
    pub(super) file: File,
    /// the range that writes the name, or the expression standing in for one
    pub(super) range: TextRange,
    pub(super) written: Written,
    /// whether the file is the project's own, and so one a rename could edit
    ///
    /// an installed app's template is django's to load and nobody's to rewrite.
    pub(super) own: bool,
    /// whether it is a template that writes it, rather than python
    pub(super) template: bool,
    /// whether the occurrence declares the name rather than using it
    pub(super) declaration: bool,
}

/// how a position writes the name
///
/// this is the whole of what the two callers disagree about. everything but
/// [`Written::Unknown`] spells the name out, and so is a reference; everything
/// but [`Written::Whole`] is somewhere a rename cannot simply replace, and so is
/// a refusal. the overlap in the middle — a name written where no rewrite could
/// reach it — is reported by one and refused by the other on purpose.
pub(super) enum Written {
    /// as one literal a single range covers
    Whole,
    /// as a literal written in more than one piece, which no one range covers
    Pieces,
    /// not at all: the position works the name out at run time, so this is only
    /// *maybe* an occurrence of this name and no occurrence of it to report
    Unknown,
    /// as a literal somewhere nothing here reads as naming this kind of thing
    Stray,
    /// as the value of a constant, which carries the name out of sight
    Bound(CompactString),
    /// not as a name at all: the thing the name refers to, which is a position
    /// to navigate to rather than one to rewrite
    Itself,
}

/// where a django name is written, and how completely that could be worked out
#[derive(Default)]
pub(super) struct Uses {
    pub(super) found: Vec<Use>,
    /// why what was found may not be all of it
    ///
    /// a rename refuses over this; references ignores it, since an answer that
    /// may be missing something is still worth giving while a rewrite that may
    /// be missing something is not one to apply.
    pub(super) incomplete: Option<String>,
}

/// what the position at `offset` of `file` writes
///
/// `template` says the file is a django template rather than python, which the
/// caller knows and this cannot.
pub(super) fn named_at(
    db: &dyn Db,
    file: File,
    offset: TextSize,
    template: bool,
) -> Option<(Named, TextRange)> {
    if template {
        let source = source_text(db, file);
        at_template(template_index(db, file), source.as_str(), offset)
    } else {
        at_python(db, file, offset)
    }
}

/// what the position writes, read as a django template
fn at_template(
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
) -> Option<(Named, TextRange)> {
    let construct = index.lexed().construct_at(offset)?;
    if construct.kind == ConstructKind::Comment {
        return None;
    }

    let token = *index
        .lexed()
        .construct_tokens(construct)
        .iter()
        .find(|token| token.range.contains_inclusive(offset))?;

    // a tag and a filter are named by their own token, wherever they are written
    match token.kind {
        TokenKind::TagName => {
            return Some((
                Named::Tag(source[token.range].to_compact_string()),
                token.range,
            ));
        }
        TokenKind::FilterName => {
            return Some((
                Named::Filter(source[token.range].to_compact_string()),
                token.range,
            ));
        }
        _ => {}
    }

    if construct.kind != ConstructKind::Tag {
        return None;
    }
    let tag = &source[construct.name?];
    let arguments = arguments(index, construct);
    if !arguments
        .iter()
        .any(|argument| argument.range == token.range)
    {
        return None;
    }

    match tag {
        "block" | END_BLOCK_TAG => {
            let name = arguments.iter().find(|candidate| {
                matches!(candidate.kind, TokenKind::Variable | TokenKind::String)
            })?;
            if name.range != token.range {
                return None;
            }

            let range = string_contents(source, token.range);
            Some((Named::Block(source[range].to_compact_string()), range))
        }
        URL_TAG => {
            let range = first_string(source, arguments, token)?;
            Some((Named::Route(source[range].to_compact_string()), range))
        }
        tag if TEMPLATE_TAGS.contains(&tag) => {
            let range = strip_fragment(source, first_string(source, arguments, token)?);
            Some((Named::Template(source[range].to_compact_string()), range))
        }
        _ => None,
    }
}

/// the contents of the tag's first string argument, when `token` is that argument
fn first_string(source: &str, arguments: &[Token], token: Token) -> Option<TextRange> {
    let first = arguments
        .iter()
        .find(|candidate| candidate.kind == TokenKind::String)?;
    if first.range != token.range {
        return None;
    }

    let range = string_contents(source, token.range);
    // a token that is not actually quoted names nothing that could be rewritten
    (range != token.range).then_some(range)
}

/// `range` up to its `#`, which addresses a partial rather than naming the template
fn strip_fragment(source: &str, range: TextRange) -> TextRange {
    match source[range].find(FRAGMENT_SEPARATOR) {
        Some(index) => TextRange::at(
            range.start(),
            TextSize::try_from(index).unwrap_or(range.len()),
        ),
        None => range,
    }
}

/// a construct's tokens with its delimiters and its tag name dropped
fn arguments<'a>(index: &'a TemplateIndex, construct: &Construct) -> &'a [Token] {
    let tokens = index.lexed().inner_tokens(construct);

    match tokens.split_first() {
        Some((first, rest)) if first.kind == TokenKind::TagName => rest,
        _ => tokens,
    }
}

/// what the position writes, read as python
fn at_python(db: &dyn Db, file: File, offset: TextSize) -> Option<(Named, TextRange)> {
    // a route's own declaration is the `name=` the url index already recorded, so
    // recognising one needs no reading of the call it sits in
    if let Some(url) = project::url_names(db, db.project())
        .iter()
        .find(|url| url.file == file && url.range.contains_inclusive(offset))
    {
        return Some((Named::RouteDeclaration, url.range));
    }

    let parsed = parsed_module(db, file).load(db);
    let (names, value, range) = super::python::name_at(&parsed, offset)?;

    Some(match names {
        super::python::Names::Template => (Named::Template(value), range),
        super::python::Names::Route => (Named::Route(value), range),
    })
}

// ---------------------------------------------------------------------------
// a `{% block %}` name
// ---------------------------------------------------------------------------

/// every template of `file`'s inheritance family that declares the block `name`
pub(super) fn block(db: &dyn Db, file: File, name: &str) -> Uses {
    let mut incomplete = None;

    if !project::settings_are_authoritative(db, db.project()) {
        incomplete = Some(
            "the project's settings could not be read in full, so the templates that override \
             this block cannot all be found"
                .to_string(),
        );
    }

    let tree = Inheritance::of(db);

    // a template whose ancestry has a gap in it may be overriding this very block
    // somewhere nothing here would think to look
    if incomplete.is_none()
        && let Some(uncertain) = tree
            .uncertain
            .iter()
            .find(|candidate| defines_block(db, **candidate, name))
    {
        incomplete = Some(format!(
            "`{}` overrides `{name}`, and what it extends is decided at render time, so the \
             templates that override this block cannot all be found",
            path_of(db, *uncertain)
        ));
    }

    let mut found = Vec::new();
    for member in tree.family(file) {
        if !defines_block(db, member, name) {
            continue;
        }

        // the block is declared where nothing above it declares it, and overridden
        // everywhere below
        let declaration = !tree
            .ancestors(member)
            .any(|ancestor| defines_block(db, ancestor, name));

        found.extend(block_names(db, member, name).into_iter().map(|range| Use {
            file: member,
            range,
            written: Written::Whole,
            own: !tree.foreign.contains(&member),
            template: true,
            declaration,
        }));
    }
    sort(db, &mut found);

    Uses { found, incomplete }
}

/// every template django can load, and the one it extends
///
/// the templates are kept in discovery order rather than only in the maps, so
/// that the walks below answer the same way twice.
struct Inheritance {
    parents: FxHashMap<File, Option<File>>,
    children: FxHashMap<File, Vec<File>>,
    /// the templates whose chain of parents does not reach a root
    uncertain: FxHashSet<File>,
    /// the templates an installed app holds, which are nobody's to rewrite
    foreign: FxHashSet<File>,
}

impl Inheritance {
    fn of(db: &dyn Db) -> Self {
        let mut order = Vec::new();
        let mut parents: FxHashMap<File, Option<File>> = FxHashMap::default();
        let mut children: FxHashMap<File, Vec<File>> = FxHashMap::default();
        let mut broken: FxHashSet<File> = FxHashSet::default();
        let mut foreign: FxHashSet<File> = FxHashSet::default();

        for template in templates(db) {
            let file = template.file;
            if !template.own {
                foreign.insert(file);
            }

            let parent = template
                .index
                .extends()
                .and_then(|reference| project::resolve_template(db, &reference.name));

            // an `{% extends %}` written as a variable, and one naming a template
            // that isn't there, both leave this template's place in the tree unknown
            if template.index.extends_unresolved()
                || (template.index.extends().is_some() && parent.is_none())
            {
                broken.insert(file);
            }

            if let Some(parent) = parent {
                children.entry(parent).or_default().push(file);
            }
            order.push(file);
            parents.insert(file, parent);
        }

        let uncertain = order
            .into_iter()
            .filter(|file| {
                let mut current = *file;
                let mut seen = FxHashSet::default();

                for _ in 0..MAX_INHERITANCE_DEPTH {
                    // a chain that comes back round never reaches a root, and one
                    // whose next link is not a template django could load has left
                    // whatever this can see
                    if broken.contains(&current) || !seen.insert(current) {
                        return true;
                    }
                    match parents.get(&current) {
                        Some(Some(parent)) => current = *parent,
                        Some(None) => return false,
                        None => return true,
                    }
                }

                true
            })
            .collect();

        Self {
            parents,
            children,
            uncertain,
            foreign,
        }
    }

    /// every template joined to `file` by inheritance, however far along the chain
    ///
    /// a block is one name across the whole family: renaming it in a child means
    /// renaming it in the parent it overrides, and so in every *other* child that
    /// overrides the same parent.
    fn family(&self, file: File) -> Vec<File> {
        let mut queue = vec![file];
        let mut seen: FxHashSet<File> = queue.iter().copied().collect();
        let mut found = Vec::new();

        while let Some(current) = queue.pop() {
            found.push(current);

            let upwards = self.parents.get(&current).copied().flatten();
            let downwards = self.children.get(&current).into_iter().flatten().copied();

            for neighbour in upwards.into_iter().chain(downwards) {
                if seen.insert(neighbour) {
                    queue.push(neighbour);
                }
            }
        }

        found
    }

    /// the templates `file` extends, nearest first
    fn ancestors(&self, file: File) -> impl Iterator<Item = File> {
        let mut current = Some(file);

        std::iter::from_fn(move || {
            let parent = self.parents.get(&current?).copied().flatten()?;
            current = Some(parent);
            Some(parent)
        })
        .take(MAX_INHERITANCE_DEPTH)
    }
}

pub(super) fn defines_block(db: &dyn Db, file: File, name: &str) -> bool {
    template_index(db, file)
        .blocks()
        .iter()
        .any(|block| block.name == name)
}

/// every range of `file` that writes the block name `name`
fn block_names(db: &dyn Db, file: File, name: &str) -> Vec<TextRange> {
    let index = template_index(db, file);
    let source = source_text(db, file);
    let mut found = Vec::new();

    for definition in index.blocks().iter().filter(|block| block.name == name) {
        found.push(definition.name_range);

        // `{% endblock content %}` writes the name a second time
        if let Some(range) = closing_name(index, source.as_str(), definition) {
            found.push(range);
        }
    }

    found
}

/// the name written on the `{% endblock %}` that closes `definition`
fn closing_name(index: &TemplateIndex, source: &str, definition: &Definition) -> Option<TextRange> {
    let construct = index.lexed().constructs().iter().find(|construct| {
        construct.kind == ConstructKind::Tag
            && construct.range.end() == definition.full_range.end()
            && construct.range.start() > definition.full_range.start()
            && construct
                .name
                .is_some_and(|range| &source[range] == END_BLOCK_TAG)
    })?;

    let argument = arguments(index, construct)
        .iter()
        .find(|token| matches!(token.kind, TokenKind::Variable | TokenKind::String))?;

    let range = string_contents(source, argument.range);
    (source[range] == *definition.name.as_str()).then_some(range)
}

// ---------------------------------------------------------------------------
// a route name
// ---------------------------------------------------------------------------

/// where the route being asked about is declared, and how the request reached it
pub(super) enum Anchor {
    /// the cursor is in the declaration's own `name=`, whose literal spans `range`
    Declaration { file: File, range: TextRange },
    /// the cursor is in a `{% url %}` or a `reverse()`, which writes `name`
    Use { name: CompactString },
}

/// a route's own `name=`
pub(super) struct Declared {
    /// the literal's contents, or the whole literal where it is written in pieces
    pub(super) range: TextRange,
    /// the bare name it writes, absent where it is written in more than one piece
    pub(super) name: Option<CompactString>,
    /// whether every name it is reversed by is written out at `range`
    ///
    /// a rest framework router generates its routes' names from a basename rather
    /// than writing them out, so what is written there is a piece of each of them.
    pub(super) exact: bool,
}

/// a route, under every name it is reversed by
pub(super) struct Route {
    /// every name the route is reversed by, the namespaces it is mounted under
    /// included
    pub(super) names: Vec<CompactString>,
    /// the one declaration the request identifies, where it identifies one
    pub(super) declared: Option<Declared>,
    pub(super) uses: Uses,
}

/// every place the route `anchor` identifies is reversed
pub(super) fn route(db: &dyn Db, anchor: &Anchor) -> Route {
    let project = db.project();
    let mut incomplete = None;

    if !project::routes_are_authoritative(db, project) {
        incomplete = Some(
            "the project's url configuration could not be read in full, so every place this \
             route is reversed cannot be found"
                .to_string(),
        );
    }
    let urls = project::url_names(db, project);

    // whichever end the request came from, the declaration is what identifies the
    // route: two namespaces may give one bare name and one declaration may be
    // mounted under two
    let mut declarations: Vec<(File, TextRange)> = match anchor {
        Anchor::Declaration { file, range } => vec![(*file, *range)],
        Anchor::Use { name } => urls
            .iter()
            .filter(|url| url.name == *name)
            .map(|url| (url.file, url.range))
            .collect(),
    };
    declarations.sort_by_key(|(file, range)| (path_of(db, *file), range.start()));
    declarations.dedup();

    let one = match (declarations.as_slice(), anchor) {
        ([only], _) => Some(*only),
        ([], Anchor::Use { name }) => {
            incomplete.get_or_insert_with(|| format!("no route of the project is named `{name}`"));
            None
        }
        (_, Anchor::Use { name }) => {
            incomplete.get_or_insert_with(|| {
                format!(
                    "`{name}` is given by more than one declaration, so renaming it would be \
                     renaming several routes at once"
                )
            });
            None
        }
        _ => None,
    };

    // the name a use writes is the one the declaration gives qualified by the
    // namespaces it is mounted under, and one declaration may be mounted twice
    let mut names: Vec<CompactString> = match (one, anchor) {
        (Some(declaration), _) => urls
            .iter()
            .filter(|url| (url.file, url.range) == declaration)
            .map(|url| url.name.clone())
            .collect(),
        (None, Anchor::Use { name }) => vec![name.clone()],
        (None, Anchor::Declaration { .. }) => Vec::new(),
    };
    names.sort_unstable();
    names.dedup();

    let declared = one.map(|declaration| {
        let (file, range) = declaration;
        let exact = urls
            .iter()
            .filter(|url| (url.file, url.range) == declaration)
            .all(|url| url.exact);

        match python_literal(db, file, range) {
            Some((name, contents)) => Declared {
                range: contents,
                name: Some(name),
                exact,
            },
            None => Declared {
                range,
                name: None,
                exact,
            },
        }
    });

    let mut found: Vec<Use> = declarations
        .iter()
        .map(|(file, range)| {
            let (range, written) = match python_literal(db, *file, *range) {
                Some((_, contents)) => (contents, Written::Whole),
                None => (*range, Written::Pieces),
            };

            Use {
                file: *file,
                range,
                written,
                own: true,
                template: false,
                declaration: true,
            }
        })
        .collect();

    let listed: Vec<&str> = names.iter().map(CompactString::as_str).collect();
    found.extend(bindings(db, &listed));
    found.extend(python_uses(project::route_uses(db, project), &listed));
    found.extend(template_uses(db, &[URL_TAG], &listed, false));

    Route {
        names,
        declared,
        uses: Uses { found, incomplete },
    }
}

// ---------------------------------------------------------------------------
// a template name
// ---------------------------------------------------------------------------

/// a template, and every place it is loaded
pub(super) struct Loaded<'db> {
    /// the file the name loads, where exactly one of the project's is loadable
    /// as it
    pub(super) discovered: Option<&'db DiscoveredFile>,
    pub(super) uses: Uses,
}

/// every place the template `name` is extended, included or rendered
pub(super) fn template<'db>(db: &'db dyn Db, name: &str) -> Loaded<'db> {
    let project = db.project();
    let mut incomplete = None;

    if !project::settings_are_authoritative(db, project) {
        incomplete = Some(
            "the project's settings could not be read in full, so every place this template is \
             loaded cannot be found"
                .to_string(),
        );
    }

    let mut candidates: Vec<&DiscoveredFile> = project::template_files(db, project)
        .iter()
        .filter(|discovered| discovered.name == name)
        .collect();
    candidates.dedup_by(|left, right| left.path == right.path);

    let discovered = match candidates.as_slice() {
        [only] => Some(*only),
        [] => {
            incomplete.get_or_insert_with(|| {
                format!("no template of the project is loadable as `{name}`")
            });
            None
        }
        _ => {
            incomplete.get_or_insert_with(|| {
                format!(
                    "two of the project's template directories hold a `{name}`, so renaming one \
                     would change which of them the other's name loads"
                )
            });
            None
        }
    };

    // the file itself is where the name leads, which is the declaration a
    // reference list is asked to include or leave out
    let mut found: Vec<Use> = candidates
        .iter()
        .filter_map(|candidate| {
            Some(Use {
                file: system_path_to_file(db, &candidate.path).ok()?,
                range: TextRange::default(),
                written: Written::Itself,
                own: candidate.own,
                template: true,
                declaration: true,
            })
        })
        .collect();

    let listed = [name];
    found.extend(bindings(db, &listed));
    found.extend(python_uses(project::template_uses(db, project), &listed));
    found.extend(template_uses(db, TEMPLATE_TAGS, &listed, true));

    Loaded {
        discovered,
        uses: Uses { found, incomplete },
    }
}

// ---------------------------------------------------------------------------
// a tag or filter name
// ---------------------------------------------------------------------------

/// every template that writes the tag or filter `name`, and what registers it
///
/// a name nothing registers is a builtin, and every `{% if %}` in a project is
/// no answer to any question, so nothing is found for one.
pub(super) fn registration(db: &dyn Db, name: &str, filter: bool) -> Uses {
    let matching = |kind: RegistrationKind| match kind {
        RegistrationKind::Filter => filter,
        RegistrationKind::Tag { .. } => !filter,
    };

    let mut found: Vec<Use> = project::registrations(db, db.project())
        .iter()
        .filter(|registration| matching(registration.kind) && registration.name == name)
        .map(|registration| Use {
            file: registration.file,
            range: registration.range,
            written: Written::Itself,
            own: true,
            template: false,
            declaration: true,
        })
        .collect();

    if found.is_empty() {
        return Uses::default();
    }

    let written = if filter {
        TokenKind::FilterName
    } else {
        TokenKind::TagName
    };

    for template in templates(db) {
        let source = template.source.as_str();

        for token in template.index.lexed().tokens() {
            if token.kind != written || source[token.range] != *name {
                continue;
            }

            found.push(Use {
                file: template.file,
                range: token.range,
                written: Written::Whole,
                own: template.own,
                template: true,
                declaration: false,
            });
        }
    }

    Uses {
        found,
        incomplete: None,
    }
}

// ---------------------------------------------------------------------------
// the scans the kinds share
// ---------------------------------------------------------------------------

/// every constant of the project one of `names` is bound to
///
/// a constant is the way a name reaches a position django reads without being
/// written there — `TEMPLATE = "blog/base.html"` and then a `render(request,
/// TEMPLATE)`, or a helper nothing here can see at all.
fn bindings(db: &dyn Db, names: &[&str]) -> Vec<Use> {
    project::bound_names(db, names)
        .into_iter()
        .map(|binding| Use {
            file: binding.file,
            range: binding.value,
            written: Written::Bound(binding.bound_to),
            own: true,
            template: false,
            declaration: false,
        })
        .collect()
}

/// the python positions among `scanned` that name one of `names`
///
/// a position whose name is worked out at run time is kept: it names *something*,
/// and which of the two features that matters to is [`Written::Unknown`].
fn python_uses(scanned: &[project::NameUse], names: &[&str]) -> Vec<Use> {
    scanned
        .iter()
        .filter(|used| {
            used.name
                .as_deref()
                .is_none_or(|name| names.contains(&name))
        })
        .map(|used| Use {
            file: used.file,
            range: used.range,
            written: match (&used.name, used.whole) {
                (None, _) => Written::Unknown,
                (Some(_), false) => Written::Pieces,
                (Some(_), true) => Written::Whole,
            },
            own: true,
            template: false,
            declaration: false,
        })
        .collect()
}

/// every place a template writes one of `names` as an argument of one of `tags`
///
/// `fragment` strips the `#partial` a template reference may carry, which names
/// something inside the template rather than the template.
fn template_uses(db: &dyn Db, tags: &[&str], names: &[&str], fragment: bool) -> Vec<Use> {
    let mut found = Vec::new();

    for template in templates(db) {
        let source = template.source.as_str();
        let mut accounted = Vec::new();

        for construct in template.index.lexed().constructs() {
            if construct.kind != ConstructKind::Tag {
                continue;
            }
            let Some(tag) = construct.name.map(|range| &source[range]) else {
                continue;
            };
            if !tags.contains(&tag) {
                continue;
            }

            let Some(token) = arguments(template.index, construct)
                .iter()
                .find(|token| matches!(token.kind, TokenKind::Variable | TokenKind::String))
            else {
                continue;
            };

            if token.kind != TokenKind::String {
                // an installed app working out its own names at render time is
                // speculation about code nothing here could have rewritten
                // whatever it said
                if template.own {
                    found.push(Use {
                        file: template.file,
                        range: token.range,
                        written: Written::Unknown,
                        own: true,
                        template: true,
                        declaration: false,
                    });
                }
                continue;
            }

            let contents = string_contents(source, token.range);
            accounted.push(contents);

            let range = if fragment {
                strip_fragment(source, contents)
            } else {
                contents
            };
            if !names.contains(&&source[range]) {
                continue;
            }

            found.push(Use {
                file: template.file,
                range,
                written: Written::Whole,
                own: template.own,
                template: true,
                declaration: false,
            });
        }

        found.extend(
            template
                .index
                .lexed()
                .tokens()
                .iter()
                .filter(|token| token.kind == TokenKind::String)
                .map(|token| string_contents(source, token.range))
                .filter(|range| names.contains(&&source[*range]) && !accounted.contains(range))
                .map(|range| Use {
                    file: template.file,
                    range,
                    written: Written::Stray,
                    own: template.own,
                    template: true,
                    declaration: false,
                }),
        );
    }

    found
}

/// a template django can load, and everything a scan needs to read it
struct Template<'db> {
    file: File,
    /// whether it is the project's own, and so a file a rename could edit
    own: bool,
    index: &'db TemplateIndex,
    source: SourceText,
}

/// every template django could load, the project's own and its installed apps'
fn templates(db: &dyn Db) -> Vec<Template<'_>> {
    let mut seen = FxHashSet::default();

    project::template_files(db, db.project())
        .iter()
        .filter_map(|discovered| {
            let file = system_path_to_file(db, &discovered.path).ok()?;
            seen.insert(file).then(|| Template {
                file,
                own: discovered.own,
                index: template_index(db, file),
                source: source_text(db, file),
            })
        })
        .collect()
}

/// the value and contents range of the python string literal spanning `range`
fn python_literal(db: &dyn Db, file: File, range: TextRange) -> Option<(CompactString, TextRange)> {
    let parsed = parsed_module(db, file).load(db);
    let covering = covering_node(parsed.syntax().into(), range);

    let string = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::ExprStringLiteral(string) => Some(string),
        _ => None,
    })?;
    let [part] = string.value.as_slice() else {
        return None;
    };

    Some((string.value.to_str().into(), part.content_range()))
}

pub(super) fn path_of(db: &dyn Db, file: File) -> String {
    file.path(db).to_string()
}

/// put the uses in a stable order, so that two runs answer the same
pub(super) fn sort(db: &dyn Db, found: &mut [Use]) {
    found.sort_by(|left, right| {
        path_of(db, left.file)
            .cmp(&path_of(db, right.file))
            .then(left.range.start().cmp(&right.range.start()))
    });
}
