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
    self, CONTEXT_CALLEES, REDIRECT_CALLEE, REVERSE_CALLEES, TEMPLATE_NAME_ATTRIBUTE, callee_name,
    class_attribute,
};

/// what a string literal in a python module names
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Names {
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
    let Some((string, names)) = named(ancestors) else {
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

/// where the django name the string literal at `offset` spells is defined
pub(crate) fn string_definition(
    db: &dyn Db,
    file: File,
    parsed: &ParsedModuleRef,
    offset: TextSize,
) -> Option<RangedValue<NavigationTargets>> {
    // every goto request comes through here, and walking the tree for a position
    // that is plainly not a string would be a cost the whole crate pays for django
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
    let value = string.value.to_str();

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
