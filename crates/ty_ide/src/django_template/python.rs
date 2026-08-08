//! the django names a python module spells out as plain strings
//!
//! `render(request, "blog/post.html")` names a template and `reverse("blog:detail")`
//! names a route, but to python both are ordinary strings: nothing about the
//! literal itself says what it is. what says it is the position — which function
//! is being called, and which of its arguments the literal is — so that is all
//! that is read here, and a literal in no recognised position never reaches the
//! project's indexes at all.

use compact_str::CompactString;
use ruff_db::files::{File, FileRange};
use ruff_db::parsed::ParsedModuleRef;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{self as ast, AnyNodeRef};
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_project::Db;

use crate::completion::CompletionKind;
use crate::{NavigationTarget, NavigationTargets, RangedValue};

use super::project::{
    self, CONTEXT_CALLEES, REDIRECT_CALLEE, REVERSE_ARGUMENTS_KEYWORD, REVERSE_CALLEES,
    REVERSE_NAME_KEYWORD, TEMPLATE_NAME_ATTRIBUTE, callee_name, class_attribute,
};

/// what a string literal in a python module names
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Names {
    /// a template, under the name its loader knows it by
    Template,
    /// a route of the project's url configuration
    Route,
}

/// one django name offered for a python string literal
pub(crate) struct StringCompletion {
    pub(crate) name: CompactString,
    pub(crate) kind: CompletionKind,
    /// the literal's contents, which is what the suggestion replaces
    ///
    /// neither `blog/post.html` nor `blog:detail` is a word by any client's
    /// definition of one, so the range cannot be left to the client to guess.
    pub(crate) range: TextRange,
}

/// the django names the string literal the cursor is in could be spelling
///
/// `ancestors` walks outwards from the node under the cursor, as
/// [`ruff_python_ast::find_node::CoveringNode::ancestors`] does.
pub(crate) fn string_completions<'a>(
    db: &dyn Db,
    ancestors: impl Iterator<Item = AnyNodeRef<'a>>,
) -> Vec<StringCompletion> {
    let ancestors: Vec<AnyNodeRef<'a>> = ancestors.collect();

    if let Some(completions) = route_arguments(db, &ancestors) {
        return completions;
    }

    let Some((string, names)) = named(ancestors.iter().copied()) else {
        return Vec::new();
    };
    let Some(range) = contents(string) else {
        return Vec::new();
    };

    match names {
        Names::Template => {
            let mut seen = FxHashSet::default();

            project::template_files(db, db.project())
                .iter()
                // two apps may both hold a `base.html`; django's loader resolves
                // the name to whichever comes first, so it is offered once
                .filter(|file| seen.insert(file.name.clone()))
                .map(|file| StringCompletion {
                    name: file.name.clone(),
                    kind: CompletionKind::File,
                    range,
                })
                .collect()
        }
        Names::Route => project::url_names(db, db.project())
            .iter()
            .map(|url| StringCompletion {
                name: url.name.clone(),
                kind: CompletionKind::Reference,
                range,
            })
            .collect(),
    }
}

/// what the string literal at `offset` names, with its value and its contents range
///
/// every request that comes through here would otherwise walk the tree for a
/// position that is plainly not a string, which is a cost the whole crate would
/// pay for django, so the token stream settles that first.
pub(super) fn name_at(
    parsed: &ParsedModuleRef,
    offset: TextSize,
) -> Option<(Names, CompactString, TextRange)> {
    if !parsed
        .tokens()
        .at_offset(offset)
        .any(|token| token.kind() == TokenKind::String)
    {
        return None;
    }

    let covering = covering_node(parsed.syntax().into(), TextRange::empty(offset));
    let (string, names) = named(covering.ancestors())?;
    let range = contents(string)?;

    Some((names, string.value.to_str().into(), range))
}

/// where the django name the string literal at `offset` spells is defined
pub(crate) fn string_definition(
    db: &dyn Db,
    file: File,
    parsed: &ParsedModuleRef,
    offset: TextSize,
) -> Option<RangedValue<NavigationTargets>> {
    let (names, value, range) = name_at(parsed, offset)?;
    let value = value.as_str();

    let targets: NavigationTargets = match names {
        Names::Template => NavigationTargets::from_iter([NavigationTarget::new(
            project::resolve_template(db, value)?,
            TextRange::default(),
        )]),
        Names::Route => project::url_names(db, db.project())
            .iter()
            .filter(|url| url.name == value)
            .map(|url| NavigationTarget::new(url.file, url.range))
            .collect(),
    };

    (!targets.is_empty()).then(|| RangedValue {
        range: FileRange::new(file, range),
        value: targets,
    })
}

/// what the string literal among `ancestors` names, if its position names anything
fn named<'a>(
    ancestors: impl Iterator<Item = AnyNodeRef<'a>>,
) -> Option<(&'a ast::ExprStringLiteral, Names)> {
    let outwards: Vec<AnyNodeRef<'a>> = ancestors.collect();

    // a literal's only children are the parts it is written in, so the search
    // never reaches past the literal the cursor is actually in
    let (index, string) = outwards
        .iter()
        .enumerate()
        .find_map(|(index, node)| match node {
            AnyNodeRef::ExprStringLiteral(string) => Some((index, *string)),
            _ => None,
        })?;

    let mut enclosing = outwards[index + 1..]
        .iter()
        // an argument list and a keyword sit between a literal and the call that
        // takes it, and neither says anything on its own
        .skip_while(|node| matches!(node, AnyNodeRef::Arguments(_) | AnyNodeRef::Keyword(_)));

    let names = match enclosing.next()? {
        AnyNodeRef::ExprCall(call) => argument(call, string)?,
        AnyNodeRef::StmtAssign(_) | AnyNodeRef::StmtAnnAssign(_) => {
            // `template_name = "…"` names a template where django reads one,
            // which is the body of a view class
            let AnyNodeRef::StmtClassDef(class) = enclosing.next()? else {
                return None;
            };
            let names_a_template = class
                .body
                .iter()
                .filter_map(|statement| class_attribute(statement, TEMPLATE_NAME_ATTRIBUTE))
                .any(|(value, _)| value.range() == string.range());

            names_a_template.then_some(Names::Template)?
        }
        _ => return None,
    };

    Some((string, names))
}

/// the arguments the route a `reverse()` names still takes, where the literal
/// among `ancestors` is one of their names
///
/// `reverse("blog:detail", kwargs={"pk": 1})` names the route's arguments in the
/// keys of that dict, exactly as a `{% url 'blog:detail' pk=1 %}` names them —
/// and a key the user is still typing is a set as far as the parser is
/// concerned, so both spellings are read.
///
/// `None` where the position is not one of those keys, which is the difference
/// between a route that takes nothing more and a literal that names no argument
/// at all.
fn route_arguments(db: &dyn Db, ancestors: &[AnyNodeRef<'_>]) -> Option<Vec<StringCompletion>> {
    let (index, string) = ancestors
        .iter()
        .enumerate()
        .find_map(|(index, node)| match node {
            AnyNodeRef::ExprStringLiteral(string) => Some((index, *string)),
            _ => None,
        })?;
    let range = contents(string)?;

    let mut enclosing = ancestors[index + 1..].iter();
    let keys: Vec<&ast::Expr> = match enclosing.next()? {
        // a literal in a value position names an argument's value, not its name
        AnyNodeRef::ExprDict(dict) => dict
            .items
            .iter()
            .filter_map(|item| item.key.as_ref())
            .collect(),
        AnyNodeRef::ExprSet(set) => set.elts.iter().collect(),
        _ => return None,
    };
    if !keys.iter().any(|key| key.range() == string.range()) {
        return None;
    }

    let AnyNodeRef::Keyword(keyword) = enclosing.next()? else {
        return None;
    };
    if keyword.arg.as_ref().map(ast::Identifier::as_str) != Some(REVERSE_ARGUMENTS_KEYWORD) {
        return None;
    }

    let AnyNodeRef::ExprCall(call) =
        enclosing.find(|node| !matches!(node, AnyNodeRef::Arguments(_)))?
    else {
        return None;
    };
    if !REVERSE_CALLEES.contains(&callee_name(&call.func)?.as_str()) {
        return None;
    }

    let route = call
        .arguments
        .find_argument_value(REVERSE_NAME_KEYWORD, 0)?
        .as_string_literal_expr()?;

    let given: FxHashSet<&str> = keys
        .iter()
        .filter(|key| key.range() != string.range())
        .filter_map(|key| Some(key.as_string_literal_expr()?.value.to_str()))
        .collect();

    Some(
        project::route_parameters(db, route.value.to_str())
            .into_iter()
            .filter(|parameter| !given.contains(parameter.name.as_str()))
            .map(|parameter| StringCompletion {
                name: parameter.name,
                kind: CompletionKind::Field,
                range,
            })
            .collect(),
    )
}

/// what the argument `string` of `call` names
fn argument(call: &ast::ExprCall, string: &ast::ExprStringLiteral) -> Option<Names> {
    let callee = callee_name(&call.func)?;
    let positional = |index: usize| {
        call.arguments
            .args
            .get(index)
            .is_some_and(|argument| argument.range() == string.range())
    };

    if CONTEXT_CALLEES.contains(&callee.as_str()) {
        // both take the request first and the template second, and both accept
        // the template by keyword as well
        let names_a_template = positional(1)
            || call
                .arguments
                .find_keyword(TEMPLATE_NAME_ATTRIBUTE)
                .is_some_and(|keyword| keyword.value.range() == string.range());

        return names_a_template.then_some(Names::Template);
    }

    if REVERSE_CALLEES.contains(&callee.as_str()) {
        return positional(0).then_some(Names::Route);
    }

    if callee == REDIRECT_CALLEE {
        // a redirect takes a url or a model as readily as a route name. a name it
        // doesn't recognise it simply doesn't answer for, but a string with a
        // separator in it is a path beyond doubt and is refused outright
        return (positional(0) && !string.value.to_str().contains('/')).then_some(Names::Route);
    }

    None
}

/// the range of a literal's contents, its prefix and quotes excluded
///
/// an implicitly concatenated literal is refused: a name written in two pieces
/// is not one a suggestion could replace.
fn contents(string: &ast::ExprStringLiteral) -> Option<TextRange> {
    let [part] = string.value.as_slice() else {
        return None;
    };

    Some(part.content_range())
}
