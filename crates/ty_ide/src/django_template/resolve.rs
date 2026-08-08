//! joining a template's variable paths to the python types behind them
//!
//! this is what makes template support more than a syntax mode. `{{ book.a… }}`
//! can only be completed if `book` is known to be a `Book`, and `book` is only
//! known to be a `Book` because some view wrote `render(request, "…", {"book":
//! book})` and the type checker knows what that `book` is. the chain runs
//! template binding → context entry → python expression → [`Type`], and each link
//! is one function here.

use ruff_db::files::{File, FilePath};
use ruff_db::parsed::parsed_module;
use ruff_python_ast::find_node::covering_node;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_project::Db;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::ide_support::{
    TemplateLookup, iterable_element_type, no_argument_call_return_type, own_class_member_names,
    template_lookup,
};
use ty_python_semantic::types::list_members::{Member, all_members};
use ty_python_semantic::{HasType, SemanticModel};

use super::index::{Binding, BindingOrigin, TemplateIndex};
use super::lexer::TokenKind;
use super::project::{self, ContextVariable};

/// how many `{% with %}` hops a path is followed through before giving up
///
/// a template can write `{% with a=b %}{% with b=a %}`, which has no fixed point;
/// the limit is what keeps resolution total rather than the shape of the input.
const MAX_INDIRECTIONS: u32 = 8;

/// what a name written in a template refers to
#[derive(Debug, Clone, Copy)]
pub(crate) enum Origin<'a> {
    /// a tag in this template bound it
    Binding(&'a Binding),
    /// a view put it in this template's context
    Context(&'a ContextVariable),
}

/// what the name `name`, written at `offset`, refers to
///
/// a name a tag binds shadows one from the context, exactly as it does at render
/// time.
pub(crate) fn resolve_root<'a>(
    db: &'a dyn Db,
    template: File,
    index: &'a TemplateIndex,
    offset: TextSize,
    name: &str,
) -> Option<Origin<'a>> {
    if let Some(binding) = index.resolve_binding(name, offset) {
        return Some(Origin::Binding(binding));
    }

    context_variables(db, template)
        .into_iter()
        .find(|variable| variable.name == name)
        .map(Origin::Context)
}

/// every name in scope in this template, nearest first
///
/// a view's own context comes first, and the names the project's context
/// processors put in every template's context follow it: a view supplying a name
/// a processor also supplies is the one django renders with, exactly as a tag's
/// binding outranks them both.
pub(crate) fn context_variables(db: &dyn Db, template: File) -> Vec<&ContextVariable> {
    let mut found = match template_name(db, template) {
        Some(name) => project::context_for_template(db, &name),
        None => Vec::new(),
    };

    for variable in project::context_processor_variables(db, db.project()) {
        if !found.iter().any(|existing| existing.name == variable.name) {
            found.push(variable);
        }
    }

    found
}

/// the name the template loader knows `file` by, e.g. `blog/post.html`
pub(crate) fn template_name(db: &dyn Db, file: File) -> Option<String> {
    let FilePath::System(path) = file.path(db) else {
        return None;
    };

    project::template_files(db, db.project())
        .iter()
        .find(|candidate| *candidate.path == **path)
        .map(|candidate| candidate.name.to_string())
}

/// the type of the dotted path `segments`, written at `offset` of `template`
///
/// `segments` is the path as written: `["book", "author", "name"]` for
/// `book.author.name`. the empty path has no type.
pub(crate) fn path_type<'db>(
    db: &'db dyn Db,
    template: File,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
    segments: &[&str],
) -> Option<Type<'db>> {
    resolve_path(
        db,
        template,
        index,
        source,
        offset,
        segments,
        MAX_INDIRECTIONS,
    )
}

fn resolve_path<'db>(
    db: &'db dyn Db,
    template: File,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
    segments: &[&str],
    fuel: u32,
) -> Option<Type<'db>> {
    let (root, rest) = segments.split_first()?;
    let mut ty = root_type(db, template, index, source, offset, root, fuel)?;

    for segment in rest {
        ty = member_type(db, ty, segment)?;
    }

    Some(ty)
}

/// the type of the leading name of a path
fn root_type<'db>(
    db: &'db dyn Db,
    template: File,
    index: &TemplateIndex,
    source: &str,
    offset: TextSize,
    name: &str,
    fuel: u32,
) -> Option<Type<'db>> {
    if fuel == 0 {
        return None;
    }

    match resolve_root(db, template, index, offset, name)? {
        Origin::Binding(binding) => {
            // django's `forloop` is built by the template engine, not by any
            // python type the project could name
            if binding.origin == BindingOrigin::ForLoop {
                return None;
            }

            let value = binding.value?;
            let segments = path_segments(index, source, value);
            let value_type = resolve_path(
                db,
                template,
                index,
                source,
                // the tag's value expression is written in the tag itself, so it
                // resolves in the scope *before* this binding takes effect
                binding.range.start(),
                &segments,
                fuel - 1,
            )?;

            match binding.origin {
                BindingOrigin::LoopVariable => iterable_element_type(db, value_type),
                BindingOrigin::Alias | BindingOrigin::ForLoop => Some(value_type),
            }
        }
        Origin::Context(variable) => expression_type(db, variable.file, variable.value?),
    }
}

/// the names making up the dotted path covering `range` of the template
pub(crate) fn path_segments<'src>(
    index: &TemplateIndex,
    source: &'src str,
    range: TextRange,
) -> Vec<&'src str> {
    index
        .lexed()
        .tokens()
        .iter()
        .filter(|token| range.contains_range(token.range))
        .filter(|token| matches!(token.kind, TokenKind::Variable | TokenKind::Attribute))
        .map(|token| &source[token.range])
        .collect()
}

/// the type of `name` accessed on `ty`, as the template engine will see it
///
/// django's variable lookup tries a dictionary subscript before an attribute, but
/// a mapping's keys are values rather than types, so an attribute is all a static
/// answer can cover.
///
/// what it *does* also do is [call what the lookup lands on][resolved], which is
/// the difference between `author.book_set.all` naming a queryset and naming a
/// method.
///
/// [resolved]: https://docs.djangoproject.com/en/stable/ref/templates/language/#variables
pub(crate) fn member_type<'db>(db: &'db dyn Db, ty: Type<'db>, name: &str) -> Option<Type<'db>> {
    let member = uncalled_member_type(db, ty, name)?;

    match template_lookup(db, ty, name, member) {
        TemplateLookup::Calls => Some(resolved(db, member)),
        TemplateLookup::UsesUncalled => Some(member),
        // django renders `string_if_invalid` here, which is configurable and by
        // default the empty string. nothing useful can be said about a path
        // continuing through it, and saying `str` would offer `upper` on what a
        // template cannot read at all
        TemplateLookup::Refuses => None,
    }
}

/// the type of `name` accessed on `ty`, before the template engine calls it
///
/// this is the difference between a method django *would* call and what it gives
/// once it has: a diagnostic about a member django cannot call has to see the
/// member itself.
pub(crate) fn uncalled_member_type<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
    name: &str,
) -> Option<Type<'db>> {
    members(db, ty)
        .into_iter()
        .find(|member| member.name == name)
        .map(|member| member.ty)
}

/// what the template engine ends up with once it has resolved a lookup
///
/// django calls whatever the lookup found if it is callable, so a member that
/// takes no arguments contributes its return type rather than its own.
fn resolved<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    no_argument_call_return_type(db, ty).unwrap_or(ty)
}

/// every attribute a value of type `ty` has
///
/// the type's own class comes first and the rest follows, each alphabetically. a
/// django model inherits several dozen members from `models.Model`, and sorting
/// its fields in among them puts `title` below `save_base`.
pub(crate) fn members<'db>(db: &'db dyn Db, ty: Type<'db>) -> Vec<Member<'db>> {
    let own = own_class_member_names(db, ty);

    let mut members: Vec<_> = all_members(db, ty)
        .into_iter()
        // a template can only write a `\w+` name after a dot, so a dunder is both
        // unreachable and noise
        .filter(|member| !member.name.starts_with('_'))
        .collect();

    members.sort_unstable_by(|left, right| {
        own.contains(&right.name)
            .cmp(&own.contains(&left.name))
            .then_with(|| left.name.cmp(&right.name))
    });
    members
}

/// the type of the python expression at `range` of `file`
pub(crate) fn expression_type(db: &dyn Db, file: File, range: TextRange) -> Option<Type<'_>> {
    let parsed = parsed_module(db, file).load(db);
    let covering = covering_node(parsed.syntax().into(), range);

    // the smallest node covering the range must be the expression itself; a
    // larger one means the range no longer picks out an expression
    if covering.node().range() != range {
        return None;
    }

    let expression = covering.node().as_expr_ref()?;
    expression.inferred_type(&SemanticModel::new(db, file))
}
