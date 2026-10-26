//! changing a function's parameters, and every call of it in the project with them
//!
//! ```py
//! def area(width, height=1):          def area(height, width, *, scale=1.0):
//!     return width * height     ──►       return width * height * scale
//!
//! area(2, 3)                          area(3, 2)
//! area(width=2)                       area(width=2, height=1)   # when `height` loses its default
//! ```
//!
//! the new parameter list is given in full: each parameter by its name, the parameter it was
//! (none for a new one), its kind, its default, its annotation, and for a new one the value each
//! call passes. a renamed parameter is renamed where the body reads it and where calls pass it by
//! keyword. the function can be given a new name, renamed everywhere it is referred to, and a new
//! return annotation
//!
//! every call is rebound the way python binds it — its positional arguments to the positional
//! parameters, its keywords by name — and written again for the new list: by position while the
//! parameters still line up, by keyword after that, and keeping a keyword a keyword. arguments are
//! evaluated in the order they are written, so where the new order would evaluate one before
//! another written before it, and either could change what the other does, the call passes them
//! by keyword in the order it wrote them. calls through a name an import gives the function, `from
//! m import area as a`, are calls of it too. the change is refused, with the reason, where a call
//! cannot be rewritten so that it means what it meant:
//!
//! - a call that unpacks `*args` or `**kwargs` into the function, whose arguments cannot be told
//!   apart, unless the change leaves every existing call valid as it is;
//! - an argument the change would drop when evaluating it could have an effect, and one whose
//!   order of evaluation cannot be kept because it can only be passed by position;
//! - a keyword a call passes into `**kwargs` that a parameter of the new list would take;
//! - a call with nothing to pass for a parameter without a default;
//! - the function used other than called — passed as a value, say — unless every existing call
//!   stays valid;
//! - a removed parameter the body still reads, and a list python would not accept: a parameter
//!   without a default after one with a default, two of a name, kinds out of order;
//! - a new parameter name the body already means something by: one it binds, or reads from outside
//!   the function, and a renamed parameter a nested scope that binds its new name reads;
//! - a default, an annotation or a value that is not one expression where it is written;
//! - a function its decorators bind to something other than itself, and one defined more than
//!   once where it is defined, as an `@overload` set is;
//! - a method another class overrides or that overrides another's, whose signatures would part

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::{self as ast, AnyNodeRef, Expr};
use ruff_python_parser::{Mode, ParseOptions};
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_python_stdlib::keyword::is_keyword;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_project::Db;
use ty_python_core::scope::{FileScopeId, NodeWithScopeRef};
use ty_python_core::{ProgramFile, SemanticIndex, semantic_index};
use ty_python_semantic::types::Type;
use ty_python_semantic::types::ide_support::decorated_function;
use ty_python_semantic::{HasType, SemanticModel};

use super::evaluation::{is_effect_free, is_pure};
use super::flow::{bindings_of_symbol, descendants};
use super::move_symbol::{SymbolMove, line_number};
use crate::code_action::FileEdit;
use crate::goto::find_goto_target;
use crate::method_kind::{MethodKind, resolved_method_kind};
use crate::references::{ReferencesMode, references};

/// how a parameter is passed
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParameterKind {
    /// before a `/`: only by position
    PositionalOnly,
    PositionalOrKeyword,
    /// `*args`
    Variadic,
    /// after a `*` or `*args`: only by keyword
    KeywordOnly,
    /// `**kwargs`
    VariadicKeyword,
}

impl ParameterKind {
    fn is_positional(self) -> bool {
        matches!(self, Self::PositionalOnly | Self::PositionalOrKeyword)
    }

    fn is_variadic(self) -> bool {
        matches!(self, Self::Variadic | Self::VariadicKeyword)
    }
}

/// a parameter of a function as it is
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureParameter {
    pub name: String,
    pub kind: ParameterKind,
    /// its default, as the source writes it
    pub default: Option<String>,
    /// its annotation, as the source writes it
    pub annotation: Option<String>,
}

/// a function whose signature [`change_signature`] can change
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSignature {
    /// the file it is defined in, which need not be the one asked about
    pub file: File,
    pub name: String,
    pub name_range: TextRange,
    /// its parameters, without the `self` or `cls` a method is called with, which stays as it
    /// is
    pub parameters: Vec<SignatureParameter>,
    /// its return annotation, as the source writes it
    pub returns: Option<String>,
}

/// what an annotation becomes
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AnnotationChange {
    /// as it is written: none, for a new parameter
    #[default]
    Keep,
    /// the annotation given, written as source, or none
    Set(Option<String>),
}

/// the whole of the signature [`change_signature`] gives a function
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureChange {
    /// its new name; `None` keeps the one it has
    pub name: Option<String>,
    /// its parameter list, in order, without a method's `self` or `cls`
    pub parameters: Vec<NewParameter>,
    pub returns: AnnotationChange,
}

/// a parameter of the list [`change_signature`] writes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewParameter {
    pub name: String,
    /// the parameter it is, for one the function has; `None` for a new one
    pub old_name: Option<String>,
    pub kind: ParameterKind,
    pub default: Option<String>,
    pub annotation: AnnotationChange,
    /// for a new parameter, what each call passes it; none leaves it to its default
    pub value: Option<String>,
}

/// the function defined or named at `offset`, and its parameters
pub fn function_signature(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<FunctionSignature> {
    let (file, offset) = definition_site(db, file, offset)?;
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let source = source_text(db, file.file(db));
    let (function, receiver) = function_at(db, file, &parsed, offset)?;
    Some(FunctionSignature {
        file: file.file(db),
        name: function.name.to_string(),
        name_range: function.name.range(),
        parameters: old_parameters(source.as_str(), function, receiver)
            .into_iter()
            .map(|old| old.parameter)
            .collect(),
        returns: function
            .returns
            .as_ref()
            .map(|returns| source[returns.range()].to_string()),
    })
}

/// gives the function defined or named at `offset` the signature `change`, and rewrites every call
/// of it in the project to pass its parameters, and every reference to it to use its new name.
/// `Err` says why it cannot be changed
pub fn change_signature(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    change: &SignatureChange,
) -> Result<SymbolMove, String> {
    let parameters = change.parameters.as_slice();
    let (file, offset) =
        definition_site(db, file, offset).ok_or_else(|| "there is no function here".to_string())?;
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let source = source_text(db, file.file(db));
    let source = source.as_str();
    let (function, receiver) = function_at(db, file, &parsed, offset)
        .ok_or_else(|| "there is no function here".to_string())?;
    let name = function.name.as_str();
    let old = old_parameters(source, function, receiver);
    let receiver_name = receiver.then(|| first_parameter_name(function)).flatten();

    validate(name, &old, parameters, receiver_name.as_deref())?;
    let basedpython = file.file(db).source_type(db).is_basedpython();
    for parameter in parameters {
        if let AnnotationChange::Set(Some(annotation)) = &parameter.annotation
            && !Place::Annotation.fits(annotation, basedpython)
        {
            return Err(format!("`{annotation}` is not an annotation"));
        }
        if let Some(default) = &parameter.default
            && !Place::Default.fits(default, basedpython)
        {
            return Err(format!("`{default}` is not an expression a default can be"));
        }
        if let Some(value) = &parameter.value
            && !Place::Argument.fits(value, basedpython)
        {
            return Err(format!("`{value}` is not an expression a call can pass"));
        }
    }
    if let AnnotationChange::Set(Some(returns)) = &change.returns
        && !Place::Returns.fits(returns, basedpython)
    {
        return Err(format!("`{returns}` is not an annotation"));
    }

    let model = SemanticModel::new(db, file);
    let index = semantic_index(db, file);
    let Some(definition) = index
        .try_definitions(AnyNodeRef::from(function))
        .and_then(<[_]>::first)
        .copied()
    else {
        return Err(format!("`{name}` cannot be searched for"));
    };
    // a decorator that binds the name to something other than the function decides what its
    // calls pass, and an `@overload` set or a redefinition has calls that may reach another `def`
    if decorated_function(db, definition)
        .is_none_or(|decorated| decorated.decorated != decorated.undecorated)
    {
        return Err(format!(
            "`{name}` is decorated, and its decorators make what its calls reach something other \
             than the function itself"
        ));
    }
    let defining_scope = definition.scope(db).file_scope_id(db);
    let defining_table = index.place_table(defining_scope);
    if let Some(symbol) = defining_table.symbol_id(name)
        && bindings_of_symbol(
            db,
            index,
            &parsed,
            defining_scope,
            symbol,
            file.file(db).is_stub(db),
        )
        .len()
            > 1
    {
        return Err(format!(
            "`{name}` is defined more than once where it is defined, as an `@overload` set or a \
             redefinition, and its calls may reach any of the definitions"
        ));
    }
    let Some(body_scope) = index.try_node_scope(NodeWithScopeRef::Function(function)) else {
        return Err(format!("`{name}` cannot be searched for"));
    };
    let new_name = change.name.as_deref().filter(|new_name| *new_name != name);
    if let Some(new_name) = new_name {
        if let Some(reason) = crate::invalid_new_name(new_name) {
            return Err(reason);
        }
        if binds_beside(db, file, function, new_name) {
            return Err(format!(
                "`{new_name}` is bound already where `{name}` is defined"
            ));
        }
    }

    if receiver {
        // what it overrides in the project would part from it; a class of a library or the
        // standard library, `object` above all, is not the project's to change
        let overrides = crate::super_members(db, file, function.name.start()).unwrap_or_default();
        if let Some(overridden) = overrides
            .iter()
            .find(|overridden| db.project().files(db).contains(overridden.target.file()))
        {
            return Err(format!(
                "`{name}` overrides {}.{name}, whose signature would no longer match",
                overridden.superclass
            ));
        }
        if let Some(implementations) = crate::goto_implementation(db, file, function.name.start())
            && implementations.value.into_iter().any(|target| {
                target.file() != file.file(db) || target.focus_range() != function.name.range()
            })
        {
            return Err(format!(
                "`{name}` is overridden, and the overrides' signatures would no longer match"
            ));
        }
    }

    let mut moved = SymbolMove::default();
    let file_id = file.file(db);

    // a name the function's parameters did not have must not be one its body already means
    // something else by
    for parameter in parameters {
        if parameter.old_name.as_deref() == Some(parameter.name.as_str())
            || old
                .iter()
                .any(|existing| existing.parameter.name == parameter.name)
        {
            continue;
        }
        if let Some(reason) = body_meaning_of(index, body_scope, &parameter.name) {
            return Err(reason);
        }
    }

    // the body: renamed parameters renamed where it reads them; removed ones must be unread
    for parameter in &old {
        let new = parameters
            .iter()
            .find(|new| new.old_name.as_deref() == Some(parameter.parameter.name.as_str()));
        if new.is_some_and(|new| new.name == parameter.parameter.name) {
            continue;
        }
        let Some(goto) = find_goto_target(&model, &parsed, parameter.name_range.start()) else {
            continue;
        };
        let reads: Vec<TextRange> = references(db, file, &goto, ReferencesMode::Rename)
            .unwrap_or_default()
            .into_iter()
            .filter(|reference| {
                reference.file() == file_id
                    && reference.range() != parameter.name_range
                    && function.range().contains_range(reference.range())
                    && !function
                        .parameters
                        .range()
                        .contains_range(reference.range())
            })
            .map(|reference| reference.range())
            .collect();
        match new {
            Some(new) => {
                if let Some(read) = reads.iter().find(|read| {
                    nested_binding_reads(index, &parsed, body_scope, **read, &new.name)
                }) {
                    return Err(format!(
                        "`{}` on line {} would read the `{}` a scope inside `{name}` binds",
                        parameter.parameter.name,
                        line_number(source, read.start()),
                        new.name
                    ));
                }
                moved.edits.extend(reads.into_iter().map(|range| FileEdit {
                    file: file_id,
                    edit: Edit::range_replacement(new.name.clone(), range),
                }));
            }
            None => {
                if let Some(read) = reads.first() {
                    return Err(format!(
                        "the body still reads `{}`, on line {}",
                        parameter.parameter.name,
                        line_number(source, read.start())
                    ));
                }
            }
        }
    }

    // the definition
    if let Some(new_name) = new_name {
        moved.edits.push(FileEdit {
            file: file_id,
            edit: Edit::range_replacement(new_name.to_string(), function.name.range()),
        });
    }
    moved.edits.push(FileEdit {
        file: file_id,
        edit: Edit::range_replacement(
            parameter_list(source, function, receiver_name.as_deref(), &old, parameters),
            function.parameters.range(),
        ),
    });
    if let AnnotationChange::Set(returns) = &change.returns {
        let edit = match (&function.returns, returns) {
            (Some(written), Some(returns)) => {
                Some(Edit::range_replacement(returns.clone(), written.range()))
            }
            (Some(written), None) => Some(Edit::range_deletion(TextRange::new(
                function.parameters.end(),
                written.end(),
            ))),
            (None, Some(returns)) => Some(Edit::insertion(
                format!(" -> {returns}"),
                function.parameters.end(),
            )),
            (None, None) => None,
        };
        moved.edits.extend(edit.map(|edit| FileEdit {
            file: file_id,
            edit,
        }));
    }

    // the calls
    let compatible = is_compatible(&old, parameters);
    let by_file = function_references(db, file, &model, &parsed, function)?;
    let mut calls: Vec<(File, TextRange, Vec<Piece>)> = Vec::new();
    for (reference_file, references) in by_file {
        let program_file = ProgramFile::new(db, reference_file, file.program(db));
        let parsed = parsed_module(db, program_file.python_file(db)).load(db);
        let text = source_text(db, reference_file);
        let text = text.as_str();
        let model = SemanticModel::new(db, program_file);
        let path = relative_path(db, reference_file);
        let reference_basedpython = reference_file.source_type(db).is_basedpython();
        if !compatible
            && reference_basedpython != basedpython
            && let Some(value) = parameters
                .iter()
                .filter_map(|parameter| parameter.value.as_deref())
                .find(|value| !Place::Argument.fits(value, reference_basedpython))
        {
            return Err(format!(
                "`{value}` is not an expression a call in {path} can pass"
            ));
        }
        for reference in &references {
            let range = &reference.range;
            if reference.spells_the_name
                && let Some(new_name) = new_name
            {
                moved.edits.push(FileEdit {
                    file: reference_file,
                    edit: Edit::range_replacement(new_name.to_string(), *range),
                });
            }
            let covering = covering_node(parsed.syntax().into(), *range);
            let call = call_of(&covering);
            let Some((call, bound)) =
                call.map(|call| (call, is_bound_call(&model, call, receiver, function)))
            else {
                // an import of it, or a mention in its own `__all__`, needs nothing
                if covering.ancestors().any(|node| {
                    matches!(
                        node,
                        AnyNodeRef::StmtImportFrom(_) | AnyNodeRef::StmtImport(_)
                    )
                }) {
                    continue;
                }
                if compatible {
                    continue;
                }
                return Err(format!(
                    "{path}:{} uses `{name}` other than by calling it, and its calls could not \
                     be rewritten",
                    line_number(text, range.start())
                ));
            };
            if compatible {
                continue;
            }
            let rewritten = rewrite_call(&model, text, call, bound, &old, parameters)
                .map_err(|reason| format!("{path}:{} {reason}", line_number(text, call.start())))?;
            if let Some(rewritten) = rewritten {
                calls.push((reference_file, call.arguments.range(), rewritten));
            }
        }
    }
    moved.edits = composed(db, moved.edits, calls);
    Ok(moved)
}

/// a parameter of the function as it is, with where its name is
struct OldParameter {
    parameter: SignatureParameter,
    name_range: TextRange,
    /// its text without its default: its modifiers, name and annotation
    text: String,
    /// the range of `text`'s name within it
    name_in_text: TextRange,
    /// the range of `text`'s annotation within it
    annotation_in_text: Option<TextRange>,
}

fn old_parameters(
    source: &str,
    function: &ast::StmtFunctionDef,
    receiver: bool,
) -> Vec<OldParameter> {
    let parameters = &function.parameters;
    let mut old = Vec::new();
    let mut push = |parameter: &ast::Parameter, kind: ParameterKind, default: Option<&Expr>| {
        let text = source[parameter.range()].to_string();
        old.push(OldParameter {
            parameter: SignatureParameter {
                name: parameter.name.to_string(),
                kind,
                default: default.map(|default| source[default.range()].to_string()),
                annotation: parameter
                    .annotation
                    .as_ref()
                    .map(|annotation| source[annotation.range()].to_string()),
            },
            name_range: parameter.name.range(),
            name_in_text: parameter.name.range() - parameter.start(),
            annotation_in_text: parameter
                .annotation
                .as_ref()
                .map(|annotation| annotation.range() - parameter.start()),
            text,
        });
    };
    let mut positional = parameters
        .posonlyargs
        .iter()
        .map(|parameter| (parameter, ParameterKind::PositionalOnly))
        .chain(
            parameters
                .args
                .iter()
                .map(|parameter| (parameter, ParameterKind::PositionalOrKeyword)),
        );
    if receiver {
        positional.next();
    }
    for (parameter, kind) in positional {
        push(&parameter.parameter, kind, parameter.default.as_deref());
    }
    if let Some(vararg) = &parameters.vararg {
        push(vararg, ParameterKind::Variadic, None);
    }
    for parameter in &parameters.kwonlyargs {
        push(
            &parameter.parameter,
            ParameterKind::KeywordOnly,
            parameter.default.as_deref(),
        );
    }
    if let Some(kwarg) = &parameters.kwarg {
        push(kwarg, ParameterKind::VariadicKeyword, None);
    }
    old
}

fn first_parameter_name(function: &ast::StmtFunctionDef) -> Option<String> {
    function
        .parameters
        .posonlyargs
        .iter()
        .chain(&function.parameters.args)
        .next()
        .map(|parameter| parameter.parameter.name.to_string())
}

/// checks that `parameters` is a list python accepts, made of the function's own parameters and
/// new ones
fn validate(
    name: &str,
    old: &[OldParameter],
    parameters: &[NewParameter],
    receiver: Option<&str>,
) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut old_seen: Vec<&str> = Vec::new();
    let mut previous_kind = ParameterKind::PositionalOnly;
    let mut positional_default: Option<&str> = None;
    for parameter in parameters {
        let new_name = parameter.name.as_str();
        if !is_identifier(new_name) || is_keyword(new_name) {
            return Err(format!("`{new_name}` is not a name a parameter can have"));
        }
        if seen.contains(&new_name) || receiver == Some(new_name) {
            return Err(format!(
                "`{name}` would have two parameters named `{new_name}`"
            ));
        }
        seen.push(new_name);
        if parameter.kind < previous_kind
            || (parameter.kind == previous_kind && parameter.kind.is_variadic())
        {
            return Err(format!(
                "`{new_name}` is out of order: python takes positional-only parameters, then the \
                 others, `*args`, keyword-only ones and `**kwargs`, one of each `*` kind"
            ));
        }
        previous_kind = parameter.kind;
        if parameter.kind.is_variadic()
            && (parameter.default.is_some() || parameter.value.is_some())
        {
            return Err(format!(
                "`{new_name}` takes what is left over, so it has no default"
            ));
        }
        if parameter.kind.is_positional() {
            match (&parameter.default, positional_default) {
                (Some(_), _) => positional_default = Some(new_name),
                (None, Some(with_default)) => {
                    return Err(format!(
                        "`{new_name}` has no default but follows `{with_default}`, which has one"
                    ));
                }
                (None, None) => {}
            }
        }
        match &parameter.old_name {
            Some(old_name) => {
                let Some(existing) = old
                    .iter()
                    .find(|existing| existing.parameter.name == *old_name)
                else {
                    return Err(format!("`{name}` has no parameter `{old_name}`"));
                };
                if old_seen.contains(&old_name.as_str()) {
                    return Err(format!("`{old_name}` is listed twice"));
                }
                old_seen.push(old_name);
                if existing.parameter.kind.is_variadic() != parameter.kind.is_variadic()
                    || (existing.parameter.kind.is_variadic()
                        && existing.parameter.kind != parameter.kind)
                {
                    return Err(format!(
                        "`{old_name}` cannot change between taking one argument and taking what \
                         is left over"
                    ));
                }
            }
            None => {
                if parameter.kind.is_variadic() {
                    continue;
                }
            }
        }
    }
    Ok(())
}

/// whether every call of the function as it is stays valid, and means the same, against
/// `parameters`: the parameters it has keep their names, kinds, places and defaults, and those
/// added after them all have defaults. a parameter added before `**kwargs` could take a keyword a
/// call passes to `**kwargs`, so only a `*args` can be
fn is_compatible(old: &[OldParameter], parameters: &[NewParameter]) -> bool {
    let mut new = parameters.iter().peekable();
    for existing in old {
        while let Some(added) = new.peek()
            && added.old_name.is_none()
            && existing.parameter.kind == ParameterKind::VariadicKeyword
        {
            if !added.kind.is_variadic() {
                return false;
            }
            new.next();
        }
        let Some(next) = new.next() else {
            return false;
        };
        if next.old_name.as_deref() != Some(existing.parameter.name.as_str())
            || next.name != existing.parameter.name
            || next.kind != existing.parameter.kind
            || (existing.parameter.default.is_some() && next.default.is_none())
        {
            return false;
        }
    }
    new.all(|added| {
        added.old_name.is_none()
            && (added.default.is_some() || added.kind.is_variadic())
            && !matches!(added.kind, ParameterKind::PositionalOnly)
    })
}

/// the parenthesized parameter list `parameters` makes, the receiver first
fn parameter_list(
    source: &str,
    function: &ast::StmtFunctionDef,
    receiver: Option<&str>,
    old: &[OldParameter],
    parameters: &[NewParameter],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if receiver.is_some()
        && let Some(first) = function
            .parameters
            .posonlyargs
            .iter()
            .chain(&function.parameters.args)
            .next()
    {
        parts.push(source[first.range()].to_string());
        if !function.parameters.posonlyargs.is_empty()
            && !parameters
                .iter()
                .any(|parameter| parameter.kind == ParameterKind::PositionalOnly)
        {
            parts.push("/".to_string());
        }
    }
    let has_variadic = parameters
        .iter()
        .any(|parameter| parameter.kind == ParameterKind::Variadic);
    let mut previous: Option<ParameterKind> = None;
    for parameter in parameters {
        if previous == Some(ParameterKind::PositionalOnly)
            && parameter.kind != ParameterKind::PositionalOnly
        {
            parts.push("/".to_string());
        }
        if parameter.kind == ParameterKind::KeywordOnly
            && !has_variadic
            && previous.is_none_or(|previous| previous < ParameterKind::Variadic)
        {
            parts.push("*".to_string());
        }
        previous = Some(parameter.kind);
        let existing = parameter.old_name.as_ref().and_then(|old_name| {
            old.iter()
                .find(|existing| existing.parameter.name == *old_name)
        });
        let annotation = match &parameter.annotation {
            AnnotationChange::Keep => {
                existing.and_then(|existing| existing.parameter.annotation.as_deref())
            }
            AnnotationChange::Set(annotation) => annotation.as_deref(),
        };
        let mut text = match existing {
            Some(existing) => {
                // the annotation after the name, so the name's range still holds once it changes
                let mut text = existing.text.clone();
                if let AnnotationChange::Set(_) = &parameter.annotation {
                    let after_name = existing.name_in_text.end().to_usize();
                    let written = existing
                        .annotation_in_text
                        .map_or(after_name..after_name, |range| {
                            after_name..range.end().to_usize()
                        });
                    text.replace_range(
                        written,
                        &annotation
                            .map(|annotation| format!(": {annotation}"))
                            .unwrap_or_default(),
                    );
                }
                text.replace_range(existing.name_in_text.to_std_range(), &parameter.name);
                text
            }
            None => {
                let star = match parameter.kind {
                    ParameterKind::Variadic => "*",
                    ParameterKind::VariadicKeyword => "**",
                    _ => "",
                };
                match annotation {
                    Some(annotation) => format!("{star}{}: {annotation}", parameter.name),
                    None => format!("{star}{}", parameter.name),
                }
            }
        };
        if let Some(default) = &parameter.default {
            let annotated = annotation.is_some();
            text.push_str(if annotated { " = " } else { "=" });
            text.push_str(default);
        }
        parts.push(text);
    }
    if previous == Some(ParameterKind::PositionalOnly) {
        parts.push("/".to_string());
    }
    format!("({})", parts.join(", "))
}

/// an argument of a call, as python binds it
#[derive(Clone, Copy)]
struct Argument<'a> {
    value: &'a Expr,
    keyword: Option<&'a str>,
}

/// an argument as the rewritten call writes it
#[derive(Clone)]
struct WrittenArgument<'a> {
    /// what the call passes, or `None` for a new parameter's value
    value: Option<&'a Expr>,
    piece: Piece,
    /// the name it can be passed by keyword under, `None` where it can only be passed by
    /// position
    keyword: Option<String>,
}

/// of `evaluated`, in the order a call evaluates them, the first two that are evaluated the other
/// way round from how they are written, written first then written second, where evaluating
/// either could change what the other does
fn reordered_pair<'a>(
    model: &SemanticModel<'_>,
    evaluated: &[&'a Expr],
) -> Option<(&'a Expr, &'a Expr)> {
    evaluated
        .iter()
        .enumerate()
        .flat_map(|(position, first)| {
            evaluated[position + 1..]
                .iter()
                .map(move |second| (*first, *second))
        })
        .find(|(first, second)| {
            second.start() < first.start()
                && !(is_constant(first)
                    || is_constant(second)
                    || (is_effect_free(model, first) && is_effect_free(model, second)))
        })
        .map(|(first, second)| (second, first))
}

/// whether `expr` is a literal, which reads nothing an effect could change
fn is_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Name(_) => false,
        Expr::Tuple(tuple) => is_pure(expr) && tuple.elts.iter().all(is_constant),
        _ => is_pure(expr),
    }
}

/// the arguments of `call` rewritten for `parameters`, parentheses included, or `None` when they
/// stay as they are
fn rewrite_call(
    model: &SemanticModel<'_>,
    source: &str,
    call: &ast::ExprCall,
    bound: bool,
    old: &[OldParameter],
    parameters: &[NewParameter],
) -> Result<Option<Vec<Piece>>, String> {
    let arguments = &call.arguments;
    if arguments
        .args
        .iter()
        .any(|argument| matches!(argument, Expr::Starred(_)))
        || arguments
            .keywords
            .iter()
            .any(|keyword| keyword.arg.is_none())
    {
        return Err("unpacks arguments into the call, which cannot be told apart".to_string());
    }

    // bind the call as python binds it to the function as it is
    let mut positionals = arguments.args.iter();
    let receiver_argument = if bound { None } else { positionals.next() };
    let mut bound_to: Vec<Option<Argument>> = vec![None; old.len()];
    let mut extra_positional: Vec<&Expr> = Vec::new();
    let mut extra_keyword: Vec<Argument> = Vec::new();
    let positional_slots: Vec<usize> = old
        .iter()
        .enumerate()
        .filter(|(_, parameter)| parameter.parameter.kind.is_positional())
        .map(|(index, _)| index)
        .collect();
    for (position, value) in positionals.enumerate() {
        match positional_slots.get(position) {
            Some(&slot) => {
                bound_to[slot] = Some(Argument {
                    value,
                    keyword: None,
                });
            }
            None if old
                .iter()
                .any(|parameter| parameter.parameter.kind == ParameterKind::Variadic) =>
            {
                extra_positional.push(value);
            }
            None => return Err("passes more arguments than the function takes".to_string()),
        }
    }
    for keyword in &arguments.keywords {
        let Some(name) = &keyword.arg else {
            continue;
        };
        let argument = Argument {
            value: &keyword.value,
            keyword: Some(name.as_str()),
        };
        match old.iter().position(|parameter| {
            parameter.parameter.name == name.as_str()
                && matches!(
                    parameter.parameter.kind,
                    ParameterKind::PositionalOrKeyword | ParameterKind::KeywordOnly
                )
        }) {
            Some(slot) => bound_to[slot] = Some(argument),
            None if old
                .iter()
                .any(|parameter| parameter.parameter.kind == ParameterKind::VariadicKeyword) =>
            {
                extra_keyword.push(argument);
            }
            None => return Err(format!("passes `{name}`, which the function does not take")),
        }
    }

    // a keyword `**kwargs` took that a parameter of the new list is named for would be taken by
    // that parameter instead
    if let Some(stolen) = extra_keyword.iter().find_map(|argument| {
        argument.keyword.filter(|keyword| {
            parameters.iter().any(|parameter| {
                parameter.name == *keyword
                    && matches!(
                        parameter.kind,
                        ParameterKind::PositionalOrKeyword | ParameterKind::KeywordOnly
                    )
            })
        })
    }) {
        return Err(format!(
            "passes `{stolen}` into `**kwargs`, where the parameter `{stolen}` would take it \
             instead"
        ));
    }

    // write them again for the new list
    let mut by_position: Vec<WrittenArgument> = Vec::new();
    let mut by_keyword: Vec<WrittenArgument> = Vec::new();
    let mut positional_ok = true;
    let mut used = vec![false; old.len()];
    for parameter in parameters {
        match parameter.kind {
            ParameterKind::Variadic => {
                if !extra_positional.is_empty() {
                    if !positional_ok {
                        return Err(format!(
                            "passes extra arguments by position, which can no longer reach \
                             `*{}`",
                            parameter.name
                        ));
                    }
                    by_position.extend(extra_positional.iter().map(|value| WrittenArgument {
                        value: Some(value),
                        piece: Piece::Source(value.range()),
                        keyword: None,
                    }));
                    extra_positional.clear();
                }
                continue;
            }
            ParameterKind::VariadicKeyword => {
                by_keyword.extend(extra_keyword.iter().map(|argument| WrittenArgument {
                    value: Some(argument.value),
                    piece: Piece::Source(argument.value.range()),
                    keyword: argument.keyword.map(ToString::to_string),
                }));
                extra_keyword.clear();
                continue;
            }
            _ => {}
        }
        let slot = parameter.old_name.as_ref().and_then(|old_name| {
            old.iter()
                .position(|existing| existing.parameter.name == *old_name)
        });
        if let Some(slot) = slot {
            used[slot] = true;
        }
        let passed = slot.and_then(|slot| bound_to[slot]);
        let (value, piece, was_keyword) = match (passed, &parameter.value) {
            (Some(argument), _) => (
                Some(argument.value),
                Piece::Source(argument.value.range()),
                argument.keyword.is_some(),
            ),
            (None, Some(value)) if slot.is_none() => (None, Piece::Text(value.clone()), false),
            (None, _) => {
                if parameter.default.is_none() {
                    return Err(format!("has nothing to pass for `{}`", parameter.name));
                }
                positional_ok = false;
                continue;
            }
        };
        let argument = WrittenArgument {
            value,
            piece,
            keyword: (parameter.kind != ParameterKind::PositionalOnly)
                .then(|| parameter.name.clone()),
        };
        if positional_ok && parameter.kind.is_positional() && !was_keyword {
            by_position.push(argument);
        } else {
            if argument.keyword.is_none() {
                return Err(format!(
                    "would have to pass `{}` by keyword, which it only takes by position",
                    parameter.name
                ));
            }
            positional_ok = false;
            by_keyword.push(argument);
        }
    }
    if !extra_positional.is_empty() || !extra_keyword.is_empty() {
        return Err("passes extra arguments the new parameters have nowhere to take".to_string());
    }

    // what is dropped must have no effect
    for (slot, argument) in bound_to.iter().enumerate() {
        if let Some(argument) = argument
            && !used[slot]
            && !is_effect_free(model, argument.value)
        {
            return Err(format!(
                "passes `{}` for `{}`, which would no longer be evaluated",
                &source[argument.value.range()],
                old[slot].parameter.name
            ));
        }
    }

    // python evaluates arguments in the order they are written. the arguments passed by
    // position are written in the order of the new parameters, which can evaluate one before an
    // argument written before it; where that could be told apart, the arguments from there on
    // are passed by keyword instead, written in the order the call wrote them
    let in_written_order = |arguments: &mut Vec<WrittenArgument>| {
        arguments.sort_by_key(|argument| {
            argument
                .value
                .map_or(TextSize::from(u32::MAX), Ranged::start)
        });
    };
    let mut layout = None;
    let mut first_reordered = None;
    for count in (0..=by_position.len()).rev() {
        if by_position[count..]
            .iter()
            .any(|argument| argument.keyword.is_none())
        {
            break;
        }
        let mut keywords: Vec<WrittenArgument> = by_position[count..]
            .iter()
            .chain(&by_keyword)
            .cloned()
            .collect();
        in_written_order(&mut keywords);
        let evaluated: Vec<&Expr> = by_position[..count]
            .iter()
            .chain(&keywords)
            .filter_map(|argument| argument.value)
            .collect();
        match reordered_pair(model, &evaluated) {
            Some(pair) => {
                first_reordered.get_or_insert(pair);
            }
            None => {
                layout = Some((count, keywords));
                break;
            }
        }
    }
    let Some((count, keywords)) = layout else {
        let Some((earlier, later)) = first_reordered else {
            return Err("cannot keep the order its arguments are evaluated in".to_string());
        };
        return Err(format!(
            "would evaluate `{}` after `{}`, which is written after it, and either could change \
             what the other does",
            &source[earlier.range()],
            &source[later.range()]
        ));
    };

    let mut written: Vec<Vec<Piece>> = Vec::new();
    if let Some(receiver) = receiver_argument {
        written.push(vec![Piece::Source(receiver.range())]);
    }
    written.extend(
        by_position
            .into_iter()
            .take(count)
            .map(|argument| vec![argument.piece]),
    );
    written.extend(keywords.into_iter().map(|argument| match argument.keyword {
        Some(keyword) => vec![Piece::Text(format!("{keyword}=")), argument.piece],
        None => vec![argument.piece],
    }));
    let mut pieces = vec![Piece::Text("(".to_string())];
    for (position, argument) in written.into_iter().enumerate() {
        if position > 0 {
            pieces.push(Piece::Text(", ".to_string()));
        }
        pieces.extend(argument);
    }
    pieces.push(Piece::Text(")".to_string()));
    let original: Vec<&str> = arguments
        .args
        .iter()
        .map(|value| &source[value.range()])
        .chain(
            arguments
                .keywords
                .iter()
                .map(|keyword| &source[keyword.range()]),
        )
        .collect();
    let as_written: String = pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(text) => text.as_str(),
            Piece::Source(range) => &source[*range],
        })
        .collect();
    if format!("({})", original.join(", ")) == as_written {
        return Ok(None);
    }
    Ok(Some(pieces))
}

/// a piece of the text a call's arguments are written again as
#[derive(Clone)]
enum Piece {
    Text(String),
    /// the text of the file there, with what the change does inside it
    Source(TextRange),
}

/// `edits`, with the calls whose arguments `calls` writes again, as edits that do not overlap,
/// which an editor would refuse: an edit inside an argument a call writes again, a parameter the
/// argument reads or a call of the function, is made in the argument's new text, and one in the
/// rest of the arguments, a keyword the call writes anew, is left out
fn composed(
    db: &dyn Db,
    mut edits: Vec<FileEdit>,
    mut calls: Vec<(File, TextRange, Vec<Piece>)>,
) -> Vec<FileEdit> {
    // a call inside another's arguments is written first, so the outer one takes it in
    calls.sort_by_key(|(_, range, _)| range.len());
    for (file, range, pieces) in calls {
        let source = source_text(db, file);
        let (inside, outside): (Vec<FileEdit>, Vec<FileEdit>) = edits
            .into_iter()
            .partition(|edit| edit.file == file && range.contains_range(edit.edit.range()));
        edits = outside;
        let mut text = String::new();
        for piece in pieces {
            match piece {
                Piece::Text(written) => text.push_str(&written),
                Piece::Source(piece_range) => {
                    let mut written = source[piece_range].to_string();
                    let mut within: Vec<&FileEdit> = inside
                        .iter()
                        .filter(|edit| piece_range.contains_range(edit.edit.range()))
                        .collect();
                    within.sort_by_key(|edit| std::cmp::Reverse(edit.edit.start()));
                    for edit in within {
                        written.replace_range(
                            (edit.edit.range() - piece_range.start()).to_std_range(),
                            edit.edit.content().unwrap_or_default(),
                        );
                    }
                    text.push_str(&written);
                }
            }
        }
        edits.push(FileEdit {
            file,
            edit: Edit::range_replacement(text, range),
        });
    }
    edits
}

/// the call whose callee the reference at `covering` names: `f` of `f(...)` or of `x.f(...)`
fn call_of<'a>(
    covering: &ruff_python_ast::find_node::CoveringNode<'a>,
) -> Option<&'a ast::ExprCall> {
    let mut ancestors = covering.ancestors();
    let mut callee = ancestors.next()?;
    if matches!(callee, AnyNodeRef::Identifier(_)) {
        callee = ancestors.next()?;
    }
    if !matches!(
        callee,
        AnyNodeRef::ExprName(_) | AnyNodeRef::ExprAttribute(_)
    ) {
        return None;
    }
    match ancestors.next()? {
        AnyNodeRef::ExprCall(call) if call.func.range() == callee.range() => Some(call),
        _ => None,
    }
}

/// whether `call` passes the method's receiver itself, as a call through an instance does, rather
/// than as its first argument, as a call through the class does
fn is_bound_call(
    model: &SemanticModel<'_>,
    call: &ast::ExprCall,
    receiver: bool,
    function: &ast::StmtFunctionDef,
) -> bool {
    if !receiver {
        return true;
    }
    let Expr::Attribute(attribute) = &*call.func else {
        return false;
    };
    let through_class = matches!(
        attribute.value.inferred_type(model),
        Some(Type::ClassLiteral(_) | Type::SubclassOf(_) | Type::GenericAlias(_))
    );
    !through_class || resolved_method_kind(model, function) == Some(MethodKind::Class)
}

/// the function of `file` whose definition `offset` is in the name or the parameters of, and
/// whether it is a method that is passed its receiver
fn function_at<'p>(
    db: &dyn Db,
    file: ProgramFile<'_>,
    parsed: &'p ruff_db::parsed::ParsedModuleRef,
    offset: TextSize,
) -> Option<(&'p ast::StmtFunctionDef, bool)> {
    let covering = covering_node(parsed.syntax().into(), TextRange::empty(offset));
    let function = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::StmtFunctionDef(function) => Some(function),
        _ => None,
    })?;
    if !(function.name.range().contains_inclusive(offset)
        || function.parameters.range().contains_inclusive(offset))
    {
        return None;
    }
    // only a function written directly in a class body is a method
    let receiver = is_in_class_body(parsed, function)
        && resolved_method_kind(&SemanticModel::new(db, file), function)? != MethodKind::Static;
    Some((function, receiver))
}

fn is_in_class_body(
    parsed: &ruff_db::parsed::ParsedModuleRef,
    function: &ast::StmtFunctionDef,
) -> bool {
    let covering = covering_node(parsed.syntax().into(), function.range());
    covering
        .ancestors()
        .skip_while(|node| node.range() == function.range())
        .find(|node| node.is_statement())
        .is_some_and(|node| matches!(node, AnyNodeRef::StmtClassDef(_)))
}

/// the file and offset of the function named at `offset`: `offset` itself when it is in a
/// definition's name or parameters, or else where the name at `offset` is defined
fn definition_site<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    offset: TextSize,
) -> Option<(ProgramFile<'db>, TextSize)> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    if function_at(db, file, &parsed, offset).is_some() {
        return Some((file, offset));
    }
    let targets = crate::goto_definition(db, file, offset)?;
    let mut targets = targets.value.into_iter();
    let target = targets.next()?;
    if targets.next().is_some() || !db.project().files(db).contains(target.file()) {
        return None;
    }
    let target_file = ProgramFile::new(db, target.file(), file.program(db));
    let target_parsed = parsed_module(db, target_file.python_file(db)).load(db);
    function_at(
        db,
        target_file,
        &target_parsed,
        target.focus_range().start(),
    )?;
    Some((target_file, target.focus_range().start()))
}

/// where the change writes source it is given
#[derive(Clone, Copy)]
enum Place {
    Annotation,
    Returns,
    Default,
    /// an argument of a call, by position or by keyword
    Argument,
}

impl Place {
    /// whether `text`, written here, is read as one whole expression, as a file that is
    /// basedpython or not reads it: `int, str` is two parameters, and `1)` ends the list early
    fn fits(self, text: &str, basedpython: bool) -> bool {
        // a keyword argument after another argument rules out what only a lone or a positional
        // argument can be: a bare generator, a starred expression, an unparenthesized walrus
        let (prefix, suffix) = match self {
            Self::Annotation => ("def _(p: ", "): pass"),
            Self::Returns => ("def _() -> ", ": pass"),
            Self::Default => ("def _(p=", "): pass"),
            Self::Argument => ("_(_, p=", ")"),
        };
        let code = format!("{prefix}{text}{suffix}\n");
        let Ok(parsed) = ruff_python_parser::parse(
            &code,
            ParseOptions::from(Mode::Module).with_basedpython(basedpython),
        ) else {
            return false;
        };
        let Some(module) = parsed.syntax().as_module() else {
            return false;
        };
        let read = match (self, module.body.as_slice()) {
            (Self::Argument, [ast::Stmt::Expr(statement)]) => match &*statement.value {
                Expr::Call(call) => call
                    .arguments
                    .keywords
                    .first()
                    .map(|keyword| keyword.value.range()),
                _ => None,
            },
            (Self::Returns, [ast::Stmt::FunctionDef(function)]) => {
                function.returns.as_ref().map(|returns| returns.range())
            }
            (Self::Annotation, [ast::Stmt::FunctionDef(function)]) => function
                .parameters
                .args
                .first()
                .and_then(|parameter| parameter.parameter.annotation.as_ref())
                .map(|annotation| annotation.range()),
            (Self::Default, [ast::Stmt::FunctionDef(function)]) => function
                .parameters
                .args
                .first()
                .and_then(|parameter| parameter.default.as_ref())
                .map(|default| default.range()),
            _ => None,
        };
        read == Some(TextRange::at(TextSize::of(prefix), TextSize::of(text)))
    }
}

/// why the body of the function whose scope is `body` already means something by `name`, which a
/// parameter of that name would change: a name it binds itself, or one it reads from outside the
/// function, as a global, a builtin or an enclosing function's, in its own scope or in a scope
/// nested in it that does not bind the name first
fn body_meaning_of(index: &SemanticIndex<'_>, body: FileScopeId, name: &str) -> Option<String> {
    let table = index.place_table(body);
    if let Some(symbol) = table.symbol_id(name) {
        let symbol = table.symbol(symbol);
        return Some(
            if symbol.is_local() || symbol.is_global() || symbol.is_nonlocal() {
                format!("the body binds `{name}` already")
            } else {
                format!("the body reads `{name}` from outside the function")
            },
        );
    }
    descendants(index, body)
        .any(|nested| {
            index.place_table(nested).symbol_id(name).is_some()
                && reaches(index, nested, body, name)
        })
        .then(|| format!("the body reads `{name}` from outside the function"))
}

/// whether the name `name` the scope `from` mentions resolves through `to`, an enclosing function
/// scope, before any scope on the way binds it or sends it to the module with `global`
fn reaches(index: &SemanticIndex<'_>, from: FileScopeId, to: FileScopeId, name: &str) -> bool {
    for (scope, _) in index.visible_ancestor_scopes(from) {
        if scope == to {
            return true;
        }
        let table = index.place_table(scope);
        if let Some(symbol) = table.symbol_id(name) {
            let symbol = table.symbol(symbol);
            if symbol.is_global() || symbol.is_local() {
                return false;
            }
        }
    }
    false
}

/// whether the read at `read` of a parameter of the function whose scope is `body`, renamed to
/// `name`, would resolve to a binding of `name` in a scope nested in the function instead
fn nested_binding_reads(
    index: &SemanticIndex<'_>,
    parsed: &ParsedModuleRef,
    body: FileScopeId,
    read: TextRange,
    name: &str,
) -> bool {
    let covering = covering_node(parsed.syntax().into(), read);
    let Some(scope) = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::ExprName(expr) => index.try_expression_scope_id(&ast::ExprRef::Name(expr)),
        _ => None,
    }) else {
        return false;
    };
    scope != body && !reaches(index, scope, body, name)
}

/// a reference to the function a change rewrites
struct FunctionReference {
    range: TextRange,
    /// whether it spells the function's own name, which a new name renames, rather than a name an
    /// import binds it to under another name
    spells_the_name: bool,
}

/// every reference to `function` in the project, other than its name where it is defined, by file
/// in the order of their paths. a name
/// an import binds it to under another name, `from m import area as a`, is followed to its own
/// references, wherever they are, since a call through it is a call of the function too
fn function_references(
    db: &dyn Db,
    file: ProgramFile<'_>,
    model: &SemanticModel<'_>,
    parsed: &ParsedModuleRef,
    function: &ast::StmtFunctionDef,
) -> Result<Vec<(File, Vec<FunctionReference>)>, String> {
    let goto = find_goto_target(model, parsed, function.name.start())
        .ok_or_else(|| format!("`{}` cannot be searched for", function.name))?;
    let mut pending: Vec<(File, TextRange, bool)> =
        references(db, file, &goto, ReferencesMode::RenameMultiFile)
            .unwrap_or_default()
            .into_iter()
            .map(|reference| (reference.file(), reference.range(), true))
            .collect();
    let mut seen: FxHashSet<(File, TextRange)> = FxHashSet::default();
    seen.insert((file.file(db), function.name.range()));
    let mut by_file: FxHashMap<File, Vec<FunctionReference>> = FxHashMap::default();
    while let Some((reference_file, range, spells_the_name)) = pending.pop() {
        if !seen.insert((reference_file, range)) {
            continue;
        }
        by_file
            .entry(reference_file)
            .or_default()
            .push(FunctionReference {
                range,
                spells_the_name,
            });
        let program_file = ProgramFile::new(db, reference_file, file.program(db));
        let parsed = parsed_module(db, program_file.python_file(db)).load(db);
        let covering = covering_node(parsed.syntax().into(), range);
        let Some(asname) = covering.ancestors().find_map(|node| match node {
            AnyNodeRef::Alias(alias) if alias.name.range() == range => alias.asname.as_ref(),
            _ => None,
        }) else {
            continue;
        };
        let model = SemanticModel::new(db, program_file);
        let Some(goto) = find_goto_target(&model, &parsed, asname.start()) else {
            return Err(format!(
                "{}:{} imports `{}` as `{asname}`, which cannot be searched for",
                relative_path(db, reference_file),
                line_number(source_text(db, reference_file).as_str(), range.start()),
                function.name
            ));
        };
        pending.extend(
            references(db, program_file, &goto, ReferencesMode::RenameMultiFile)
                .unwrap_or_default()
                .into_iter()
                .map(|reference| (reference.file(), reference.range(), false)),
        );
    }
    let mut by_file: Vec<(File, Vec<FunctionReference>)> = by_file.into_iter().collect();
    by_file.sort_by_key(|(file, _)| file.path(db).as_str().to_string());
    for (_, references) in &mut by_file {
        references.sort_by_key(|reference| reference.range.start());
    }
    Ok(by_file)
}

/// whether the scope `function` is defined in binds `name`
fn binds_beside(
    db: &dyn Db,
    file: ProgramFile<'_>,
    function: &ast::StmtFunctionDef,
    name: &str,
) -> bool {
    let index = ty_python_core::semantic_index(db, file);
    let Some(definition) = index
        .try_definitions(AnyNodeRef::from(function))
        .and_then(<[_]>::first)
    else {
        return false;
    };
    let table = index.place_table(definition.scope(db).file_scope_id(db));
    table
        .symbol_id(name)
        .is_some_and(|symbol| table.symbol(symbol).is_bound())
}

/// `file`'s path relative to the project's root
fn relative_path(db: &dyn Db, file: File) -> String {
    let path = file.path(db);
    let root = db.project().root(db);
    path.as_system_path()
        .and_then(|path| path.strip_prefix(root).ok())
        .map_or_else(|| path.to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::{
        AnnotationChange, NewParameter, ParameterKind, SignatureChange, change_signature,
        function_signature,
    };
    use crate::refactor::move_symbol::tests::rendered;
    use crate::tests::{CursorTest, CursorTestBuilder};

    const GEO: &str = r#"def <CURSOR>area(width, height=1, *, scale=1.0):
    return width * height * scale


def forward(*args, **kwargs):
    return area(*args, **kwargs)


print(area(2, 3))
print(area(2, height=3, scale=2.0))
print(area(width=2))
"#;

    fn project(geo: &str) -> CursorTest {
        CursorTestBuilder::default()
            .source("sig/__init__.py", "")
            .source("sig/geo.py", geo)
            .source(
                "sig/user.py",
                "from sig.geo import area\n\narea(4, 5, scale=3)\n",
            )
            .build()
    }

    /// `name`, as the parameter `old` was, of `kind`, with `default`
    fn kept(name: &str, old: &str, kind: ParameterKind, default: Option<&str>) -> NewParameter {
        NewParameter {
            name: name.to_string(),
            old_name: Some(old.to_string()),
            kind,
            default: default.map(ToString::to_string),
            annotation: AnnotationChange::Keep,
            value: None,
        }
    }

    fn added(
        name: &str,
        kind: ParameterKind,
        default: Option<&str>,
        value: Option<&str>,
    ) -> NewParameter {
        NewParameter {
            name: name.to_string(),
            old_name: None,
            kind,
            default: default.map(ToString::to_string),
            annotation: AnnotationChange::Keep,
            value: value.map(ToString::to_string),
        }
    }

    use ParameterKind::{KeywordOnly, PositionalOrKeyword};

    impl CursorTest {
        fn change(&self, parameters: &[NewParameter]) -> String {
            self.change_to(&SignatureChange {
                parameters: parameters.to_vec(),
                ..SignatureChange::default()
            })
        }

        fn change_to(&self, change: &SignatureChange) -> String {
            let changed = salsa::attach(&self.db, || {
                change_signature(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                    change,
                )
            });
            rendered(&self.db, changed)
        }
    }

    #[test]
    fn the_signature_of_a_function_is_its_parameters() {
        let test = project(GEO);
        let signature = salsa::attach(&test.db, || {
            function_signature(
                &test.db,
                test.program_file(test.cursor.file),
                test.cursor.offset,
            )
        });
        assert_snapshot!(format!("{signature:#?}"), @r#"
        Some(
            FunctionSignature {
                file: file(
                    Id(101),
                ),
                name: "area",
                name_range: 4..8,
                parameters: [
                    SignatureParameter {
                        name: "width",
                        kind: PositionalOrKeyword,
                        default: None,
                        annotation: None,
                    },
                    SignatureParameter {
                        name: "height",
                        kind: PositionalOrKeyword,
                        default: Some(
                            "1",
                        ),
                        annotation: None,
                    },
                    SignatureParameter {
                        name: "scale",
                        kind: KeywordOnly,
                        default: Some(
                            "1.0",
                        ),
                        annotation: None,
                    },
                ],
                returns: None,
            },
        )
        "#);
    }

    /// a method made static by a decorator under another name is passed no receiver, so its
    /// first parameter is one of the caller's
    #[test]
    fn a_static_method_under_another_name_lists_its_first_parameter() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                "static = staticmethod\n\n\nclass Shape:\n    @static\n    def <CURSOR>area(width, height):\n        return width * height\n",
            )
            .build();
        let parameters = salsa::attach(&test.db, || {
            function_signature(
                &test.db,
                test.program_file(test.cursor.file),
                test.cursor.offset,
            )
        })
        .map(|signature| {
            signature
                .parameters
                .into_iter()
                .map(|parameter| parameter.name)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            parameters,
            Some(vec!["width".to_string(), "height".to_string()])
        );
    }

    /// without the call that unpacks into it, or the one that leaves `height` to the default it
    /// loses, swapping the two parameters swaps what each call passes by position, and a call
    /// that passed `height` by keyword passes both by keyword, in the order it wrote them
    #[test]
    fn reordering_rewrites_each_call() {
        let test = project(
            &GEO.replace(
                "def forward(*args, **kwargs):\n    return area(*args, **kwargs)\n",
                "",
            )
            .replace("print(area(width=2))\n", ""),
        );
        assert_snapshot!(test.change(&[
            kept("height", "height", PositionalOrKeyword, None),
            kept("width", "width", PositionalOrKeyword, None),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ]), @"
        --- /sig/geo.py
        def area(height, width, *, scale=1.0):
            return width * height * scale




        print(area(3, 2))
        print(area(width=2, height=3, scale=2.0))
        --- /sig/user.py
        from sig.geo import area

        area(5, 4, scale=3)
        ");
    }

    /// `forward` unpacks `*args` into `area`, which cannot be rebound
    #[test]
    fn a_call_that_unpacks_is_refused() {
        let test = project(GEO);
        assert_snapshot!(test.change(&[
            kept("height", "height", PositionalOrKeyword, None),
            kept("width", "width", PositionalOrKeyword, None),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ]), @"refused: sig/geo.py:6 unpacks arguments into the call, which cannot be told apart");
    }

    /// a parameter added with a default leaves every call valid, so none is touched, the one
    /// that unpacks included
    #[test]
    fn adding_a_parameter_with_a_default_changes_no_call() {
        let test = project(GEO);
        assert_snapshot!(test.change(&[
            kept("width", "width", PositionalOrKeyword, None),
            kept("height", "height", PositionalOrKeyword, Some("1")),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
            added("depth", KeywordOnly, Some("1"), None),
        ]), @"
        --- /sig/geo.py
        def area(width, height=1, *, scale=1.0, depth=1):
            return width * height * scale


        def forward(*args, **kwargs):
            return area(*args, **kwargs)


        print(area(2, 3))
        print(area(2, height=3, scale=2.0))
        print(area(width=2))
        ");
    }

    /// a required parameter is passed the value given, by position while it can be
    #[test]
    fn adding_a_required_parameter_passes_its_value() {
        let test = project(&GEO.replace(
            "def forward(*args, **kwargs):\n    return area(*args, **kwargs)\n",
            "",
        ));
        assert_snapshot!(test.change(&[
            kept("width", "width", PositionalOrKeyword, None),
            added("depth", PositionalOrKeyword, None, Some("5")),
            kept("height", "height", PositionalOrKeyword, Some("1")),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ]), @"
        --- /sig/geo.py
        def area(width, depth, height=1, *, scale=1.0):
            return width * height * scale




        print(area(2, 5, 3))
        print(area(2, 5, height=3, scale=2.0))
        print(area(width=2, depth=5))
        --- /sig/user.py
        from sig.geo import area

        area(4, 5, 5, scale=3)
        ");
    }

    #[test]
    fn a_parameter_without_a_default_after_one_with_is_refused() {
        let test = project(GEO);
        assert_snapshot!(test.change(&[
            kept("width", "width", PositionalOrKeyword, None),
            kept("height", "height", PositionalOrKeyword, Some("1")),
            added("depth", PositionalOrKeyword, None, Some("5")),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ]), @"refused: `depth` has no default but follows `height`, which has one");
    }

    #[test]
    fn removing_a_parameter_the_body_reads_is_refused() {
        let test = project(GEO);
        assert_snapshot!(test.change(&[
            kept("width", "width", PositionalOrKeyword, None),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ]), @"refused: the body still reads `height`, on line 2");
    }

    /// a rename renames the body's reads and the calls' keywords
    #[test]
    fn renaming_renames_the_body_and_keywords() {
        let test = project(&GEO.replace(
            "def forward(*args, **kwargs):\n    return area(*args, **kwargs)\n",
            "",
        ));
        assert_snapshot!(test.change(&[
            kept("w", "width", PositionalOrKeyword, None),
            kept("height", "height", PositionalOrKeyword, Some("1")),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ]), @"
        --- /sig/geo.py
        def area(w, height=1, *, scale=1.0):
            return w * height * scale




        print(area(2, 3))
        print(area(2, height=3, scale=2.0))
        print(area(w=2))
        ");
    }

    /// dropping an argument whose evaluation could have an effect would lose the effect
    #[test]
    fn dropping_an_argument_with_an_effect_is_refused() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                "def <CURSOR>f(a, b):\n    return a\n\n\ndef g():\n    return 1\n\n\nf(1, g())\n",
            )
            .build();
        assert_snapshot!(test.change(&[kept("a", "a", PositionalOrKeyword, None)]), @"refused: main.py:9 passes `g()` for `b`, which would no longer be evaluated");
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                "def <CURSOR>f(a, b):\n    return a\n\n\nf(1, 2)\n",
            )
            .build();
        assert_snapshot!(test.change(&[kept("a", "a", PositionalOrKeyword, None)]), @"
        --- /main.py
        def f(a):
            return a


        f(1)
        ");
    }

    /// a method called through an instance is passed its receiver; through the class, its first
    /// argument is the receiver
    #[test]
    fn a_method_s_calls_keep_their_receiver() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                r#"class Shape:
    def <CURSOR>scaled(self, factor, offset=0):
        return factor + offset


shape = Shape()
shape.scaled(2, 1)
Shape.scaled(shape, 2, offset=1)
"#,
            )
            .build();
        assert_snapshot!(test.change(&[
            kept("offset", "offset", PositionalOrKeyword, None),
            kept("factor", "factor", PositionalOrKeyword, None),
        ]), @"
        --- /main.py
        class Shape:
            def scaled(self, offset, factor):
                return factor + offset


        shape = Shape()
        shape.scaled(1, 2)
        Shape.scaled(shape, factor=2, offset=1)
        ");
    }

    /// a new name renames the function where it is defined and everywhere it is referred to,
    /// imports included, alongside what the parameters do to its calls
    #[test]
    fn a_function_is_renamed_and_annotated() {
        let test = project(GEO);
        let mut width = kept("width", "width", PositionalOrKeyword, None);
        width.annotation = AnnotationChange::Set(Some("float".to_string()));
        assert_snapshot!(test.change_to(&SignatureChange {
            name: Some("surface".to_string()),
            parameters: vec![
                width,
                kept("height", "height", PositionalOrKeyword, Some("1")),
                kept("scale", "scale", KeywordOnly, Some("1.0")),
            ],
            returns: AnnotationChange::Set(Some("float".to_string())),
        }), @"
        --- /sig/geo.py
        def surface(width: float, height=1, *, scale=1.0) -> float:
            return width * height * scale


        def forward(*args, **kwargs):
            return surface(*args, **kwargs)


        print(surface(2, 3))
        print(surface(2, height=3, scale=2.0))
        print(surface(width=2))
        --- /sig/user.py
        from sig.geo import surface

        surface(4, 5, scale=3)
        ");
    }

    /// an annotation set to none is removed, and one kept is written as it was
    #[test]
    fn annotations_are_removed_and_kept() {
        let test = CursorTestBuilder::default()
            .source(
                "main.py",
                "def <CURSOR>area(width: int, height: int = 1) -> int:\n    return width * height\n",
            )
            .build();
        let mut width = kept("width", "width", PositionalOrKeyword, None);
        width.annotation = AnnotationChange::Set(None);
        let mut height = kept("height", "height", PositionalOrKeyword, Some("2"));
        height.annotation = AnnotationChange::Set(Some("float".to_string()));
        let mut depth = added("depth", KeywordOnly, Some("0"), None);
        depth.annotation = AnnotationChange::Set(Some("int".to_string()));
        assert_snapshot!(test.change_to(&SignatureChange {
            name: None,
            parameters: vec![width, height, depth],
            returns: AnnotationChange::Set(None),
        }), @"
        --- /main.py
        def area(width, height: float = 2, *, depth: int = 0):
            return width * height
        ");
        assert_snapshot!(test.change_to(&SignatureChange {
            name: Some("area".to_string()),
            parameters: vec![
                kept("height", "height", PositionalOrKeyword, None),
                kept("width", "width", PositionalOrKeyword, None),
            ],
            returns: AnnotationChange::Keep,
        }), @"
        --- /main.py
        def area(height: int, width: int) -> int:
            return width * height
        ");
    }

    /// a name a function cannot have, or one its scope binds already, and an annotation that is
    /// not an expression, are refused
    #[test]
    fn a_name_or_annotation_that_cannot_be_written_is_refused() {
        let test = project(GEO);
        let keep = vec![
            kept("width", "width", PositionalOrKeyword, None),
            kept("height", "height", PositionalOrKeyword, Some("1")),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ];
        let renamed = |name: &str| SignatureChange {
            name: Some(name.to_string()),
            parameters: keep.clone(),
            returns: AnnotationChange::Keep,
        };
        assert_snapshot!(test.change_to(&renamed("class")), @"refused: `class` is a keyword");
        assert_snapshot!(test.change_to(&renamed("2d")), @"refused: `2d` is not a valid name");
        assert_snapshot!(test.change_to(&renamed("forward")), @"refused: `forward` is bound already where `area` is defined");
        assert_snapshot!(test.change_to(&SignatureChange {
            name: None,
            parameters: keep.clone(),
            returns: AnnotationChange::Set(Some("int (".to_string())),
        }), @"refused: `int (` is not an annotation");
    }

    fn single(source: &str) -> CursorTest {
        CursorTestBuilder::default()
            .source("main.py", source)
            .build()
    }

    /// a parameter renamed to a name the body reads from outside the function would read
    /// itself instead
    #[test]
    fn renaming_to_a_name_the_body_reads_from_outside_is_refused() {
        let global = single("b = 1\n\n\ndef <CURSOR>f(a):\n    return a + b\n\n\nf(2)\n");
        assert_snapshot!(global.change(&[kept("b", "a", PositionalOrKeyword, None)]), @"refused: the body reads `b` from outside the function");
        let builtin = single("def <CURSOR>f(a):\n    return len(a)\n\n\nf([])\n");
        assert_snapshot!(builtin.change(&[kept("len", "a", PositionalOrKeyword, None)]), @"refused: the body reads `len` from outside the function");
        let nested = single("b = 1\n\n\ndef <CURSOR>f(a):\n    return lambda: a + b\n\n\nf(2)\n");
        assert_snapshot!(nested.change(&[kept("b", "a", PositionalOrKeyword, None)]), @"refused: the body reads `b` from outside the function");
        let local = single("def <CURSOR>f(a):\n    b = 1\n    return a + b\n\n\nf(2)\n");
        assert_snapshot!(local.change(&[kept("b", "a", PositionalOrKeyword, None)]), @"refused: the body binds `b` already");
    }

    /// a read of the renamed parameter in a nested scope that binds the new name itself would
    /// read that scope's own binding
    #[test]
    fn renaming_to_a_name_a_nested_scope_binds_is_refused() {
        let test = single(
            "def <CURSOR>f(a):\n    def g(b):\n        return a + b\n    return g(1)\n\n\nf(2)\n",
        );
        assert_snapshot!(test.change(&[kept("b", "a", PositionalOrKeyword, None)]), @"refused: `a` on line 3 would read the `b` a scope inside `f` binds");
    }

    /// a parameter added under a name the body reads from outside would be read instead
    #[test]
    fn adding_a_parameter_the_body_reads_from_outside_is_refused() {
        let test = single("b = 1\n\n\ndef <CURSOR>f(a):\n    return a + b\n\n\nf(2)\n");
        assert_snapshot!(test.change(&[
            kept("a", "a", PositionalOrKeyword, None),
            added("b", PositionalOrKeyword, Some("0"), None),
        ]), @"refused: the body reads `b` from outside the function");
    }

    /// two parameters that swap their names read each other's argument, which is what the
    /// rename asks for
    #[test]
    fn parameters_may_swap_their_names() {
        let test = single("def <CURSOR>f(a, b):\n    return a - b\n\n\nf(1, 2)\n");
        assert_snapshot!(test.change(&[
            kept("b", "a", PositionalOrKeyword, None),
            kept("a", "b", PositionalOrKeyword, None),
        ]), @"
        --- /main.py
        def f(b, a):
            return b - a


        f(1, 2)
        ");
    }

    /// reordering arguments evaluates them in a new order; where an argument's effect could be
    /// seen by another, the call keeps their order by passing them by keyword
    #[test]
    fn reordering_keeps_the_order_effects_happen_in() {
        let test = single(
            "def bump():\n    return 1\n\n\ndef <CURSOR>f(a, b, c):\n    return a\n\n\nn = 1\nf(bump(), n, 3)\nf(1, 2, bump())\n",
        );
        assert_snapshot!(test.change(&[
            kept("b", "b", PositionalOrKeyword, None),
            kept("c", "c", PositionalOrKeyword, None),
            kept("a", "a", PositionalOrKeyword, None),
        ]), @"
        --- /main.py
        def bump():
            return 1


        def f(b, c, a):
            return a


        n = 1
        f(a=bump(), b=n, c=3)
        f(2, bump(), 1)
        ");
    }

    /// an argument only taken by position cannot be passed by keyword to keep its order
    #[test]
    fn reordering_positional_only_arguments_with_effects_is_refused() {
        let test = single(
            "def bump():\n    return 1\n\n\ndef <CURSOR>f(a, b, /):\n    return a\n\n\nn = 1\nf(bump(), n)\n",
        );
        assert_snapshot!(test.change(&[
            kept("b", "b", ParameterKind::PositionalOnly, None),
            kept("a", "a", ParameterKind::PositionalOnly, None),
        ]), @"refused: main.py:10 would evaluate `bump()` after `n`, which is written after it, and either could change what the other does");
    }

    /// a call through a name an import gives the function is a call of it too
    #[test]
    fn a_call_through_an_import_alias_is_rewritten() {
        let test = CursorTestBuilder::default()
            .source(
                "m.py",
                "def <CURSOR>area(width, height):\n    return width * height\n",
            )
            .source("user.py", "from m import area as a\n\na(2, 3)\n")
            .build();
        assert_snapshot!(test.change(&[
            kept("height", "height", PositionalOrKeyword, None),
            kept("width", "width", PositionalOrKeyword, None),
        ]), @"
        --- /m.py
        def area(height, width):
            return width * height
        --- /user.py
        from m import area as a

        a(3, 2)
        ");
    }

    /// a keyword-only parameter added before `**kwargs` would take a keyword a call passes to
    /// `**kwargs`
    #[test]
    fn a_new_parameter_that_takes_a_keyword_from_kwargs_is_refused() {
        let test = single("def <CURSOR>f(**kw):\n    return kw\n\n\nf(x=1)\n");
        assert_snapshot!(test.change(&[
            added("x", KeywordOnly, Some("0"), None),
            kept("kw", "kw", ParameterKind::VariadicKeyword, None),
        ]), @"refused: main.py:5 passes `x` into `**kwargs`, where the parameter `x` would take it instead");
        assert_snapshot!(test.change(&[
            added("x", KeywordOnly, None, Some("2")),
            kept("kw", "kw", ParameterKind::VariadicKeyword, None),
        ]), @"refused: main.py:5 passes `x` into `**kwargs`, where the parameter `x` would take it instead");
        let unpacked = single("def <CURSOR>f(**kw):\n    return kw\n\n\nf(**{})\n");
        assert_snapshot!(unpacked.change(&[
            added("x", KeywordOnly, Some("0"), None),
            kept("kw", "kw", ParameterKind::VariadicKeyword, None),
        ]), @"refused: main.py:5 unpacks arguments into the call, which cannot be told apart");
    }

    /// a default or a value that is not one expression, as it would be written, is refused
    #[test]
    fn a_default_or_value_that_is_not_an_expression_is_refused() {
        let test = single("def <CURSOR>f(a):\n    return a\n\n\nf(1)\n");
        assert_snapshot!(test.change(&[
            kept("a", "a", PositionalOrKeyword, None),
            added("b", PositionalOrKeyword, Some("1)"), None),
        ]), @"refused: `1)` is not an expression a default can be");
        assert_snapshot!(test.change(&[
            kept("a", "a", PositionalOrKeyword, None),
            added("b", PositionalOrKeyword, None, Some("1, 2")),
        ]), @"refused: `1, 2` is not an expression a call can pass");
        assert_snapshot!(test.change(&[
            kept("a", "a", PositionalOrKeyword, None),
            added("b", PositionalOrKeyword, None, Some("*x")),
        ]), @"refused: `*x` is not an expression a call can pass");
        let mut annotated = kept("a", "a", PositionalOrKeyword, None);
        annotated.annotation = AnnotationChange::Set(Some("int, str".to_string()));
        assert_snapshot!(test.change(&[annotated]), @"refused: `int, str` is not an annotation");
    }

    /// a decorator that binds the name to something other than the function itself decides
    /// what its calls pass, which the change cannot see
    #[test]
    fn a_function_a_decorator_replaces_is_refused() {
        let test = single(
            "def inject(f):\n    return lambda *args: f(0, *args)\n\n\n@inject\ndef <CURSOR>f(a, b):\n    return a\n\n\nf(1)\n",
        );
        assert_snapshot!(test.change(&[
            kept("b", "b", PositionalOrKeyword, None),
            kept("a", "a", PositionalOrKeyword, None),
        ]), @"refused: `f` is decorated, and its decorators make what its calls reach something other than the function itself");
    }

    /// a call of the function in its own body is rewritten as any other is
    #[test]
    fn a_recursive_call_is_rewritten() {
        let test =
            single("def <CURSOR>f(a, b=0):\n    return f(a=a - 1, b=b) if a else b\n\n\nf(3)\n");
        assert_snapshot!(test.change(&[
            kept("x", "a", PositionalOrKeyword, None),
            kept("b", "b", PositionalOrKeyword, Some("0")),
        ]), @"
        --- /main.py
        def f(x, b=0):
            return f(x=x - 1, b=b) if x else b


        f(3)
        ");
    }

    /// a call passed to a call of the function is rewritten inside the outer call's new
    /// arguments
    #[test]
    fn a_call_inside_a_call_is_rewritten() {
        let test = single("def <CURSOR>f(a, b):\n    return a - b\n\n\nf(f(1, 2), 3)\n");
        assert_snapshot!(test.change(&[
            kept("b", "b", PositionalOrKeyword, None),
            kept("a", "a", PositionalOrKeyword, None),
        ]), @"
        --- /main.py
        def f(b, a):
            return a - b


        f(3, f(2, 1))
        ");
    }

    /// `@staticmethod` and `@classmethod` leave the function as it is called
    #[test]
    fn a_static_or_class_method_is_changed() {
        let test = single(
            "class Shape:\n    @staticmethod\n    def <CURSOR>area(width, height):\n        return width * height\n\n\nShape.area(2, 3)\n",
        );
        assert_snapshot!(test.change(&[
            kept("height", "height", PositionalOrKeyword, None),
            kept("width", "width", PositionalOrKeyword, None),
        ]), @"
        --- /main.py
        class Shape:
            @staticmethod
            def area(height, width):
                return width * height


        Shape.area(3, 2)
        ");
        let test = single(
            "class Shape:\n    @classmethod\n    def <CURSOR>area(cls, width, height):\n        return width * height\n\n\nShape.area(2, 3)\n",
        );
        assert_snapshot!(test.change(&[
            kept("height", "height", PositionalOrKeyword, None),
            kept("width", "width", PositionalOrKeyword, None),
        ]), @"
        --- /main.py
        class Shape:
            @classmethod
            def area(cls, height, width):
                return width * height


        Shape.area(3, 2)
        ");
    }

    /// one function of an `@overload` set is one of several its calls may reach
    #[test]
    fn an_overloaded_function_is_refused() {
        let test = single(
            "from typing import overload\n\n\n@overload\ndef f(a: int) -> int: ...\n@overload\ndef f(a: str) -> str: ...\ndef <CURSOR>f(a):\n    return a\n\n\nf(1)\n",
        );
        assert_snapshot!(test.change(&[
            kept("a", "a", PositionalOrKeyword, None),
            added("b", PositionalOrKeyword, Some("0"), None),
        ]), @"refused: `f` is defined more than once where it is defined, as an `@overload` set or a redefinition, and its calls may reach any of the definitions");
    }
}
