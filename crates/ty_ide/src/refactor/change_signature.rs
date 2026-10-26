//! Changing a function's parameters, and every call of it in the project with them.
//!
//! ```py
//! def area(width, height=1):          def area(height, width, *, scale=1.0):
//!     return width * height     ──►       return width * height * scale
//!
//! area(2, 3)                          area(3, 2)
//! area(width=2)                       area(width=2, height=1)   # when `height` loses its default
//! ```
//!
//! The new parameter list is given in full: each parameter by its name, the parameter it was
//! (none for a new one), its kind, its default, and for a new one the value each call passes.
//! A renamed parameter is renamed where the body reads it and where calls pass it by keyword.
//!
//! Every call is rebound the way python binds it — its positional arguments to the positional
//! parameters, its keywords by name — and written again for the new list: by position while the
//! parameters still line up, by keyword after that, and keeping a keyword a keyword. The change is
//! refused, with the reason, where a call cannot be rewritten so that it means what it meant:
//!
//! - a call that unpacks `*args` or `**kwargs` into the function, whose arguments cannot be told
//!   apart, unless the change leaves every existing call valid as it is;
//! - an argument the change would drop, or evaluate after one written after it, when evaluating it
//!   could have an effect;
//! - a call with nothing to pass for a parameter without a default;
//! - the function used other than called — passed as a value, say — unless every existing call
//!   stays valid;
//! - a removed parameter the body still reads, and a list python would not accept: a parameter
//!   without a default after one with a default, two of a name, kinds out of order;
//! - a method another class overrides or that overrides another's, whose signatures would part.

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::{self as ast, AnyNodeRef, Expr};
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_python_stdlib::keyword::is_keyword;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashMap;
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::types::Type;
use ty_python_semantic::{HasType, SemanticModel};

use super::evaluation::is_effect_free;
use super::move_symbol::{SymbolMove, line_number};
use crate::code_action::FileEdit;
use crate::goto::find_goto_target;
use crate::method_kind::{MethodKind, resolved_method_kind};
use crate::references::{ReferencesMode, references};

/// How a parameter is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParameterKind {
    /// Before a `/`: only by position.
    PositionalOnly,
    PositionalOrKeyword,
    /// `*args`.
    Variadic,
    /// After a `*` or `*args`: only by keyword.
    KeywordOnly,
    /// `**kwargs`.
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

/// A parameter of a function as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureParameter {
    pub name: String,
    pub kind: ParameterKind,
    /// Its default, as the source writes it.
    pub default: Option<String>,
    /// Its annotation, as the source writes it.
    pub annotation: Option<String>,
}

/// A function whose signature [`change_signature`] can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSignature {
    /// The file it is defined in, which need not be the one asked about.
    pub file: File,
    pub name: String,
    pub name_range: TextRange,
    /// Its parameters, without the `self` or `cls` a method is called with, which stays as it
    /// is.
    pub parameters: Vec<SignatureParameter>,
}

/// A parameter of the list [`change_signature`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewParameter {
    pub name: String,
    /// The parameter it is, for one the function has; `None` for a new one.
    pub old_name: Option<String>,
    pub kind: ParameterKind,
    pub default: Option<String>,
    /// For a new parameter, what each call passes it; none leaves it to its default.
    pub value: Option<String>,
}

/// The function defined or named at `offset`, and its parameters.
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
    })
}

/// Gives the function defined or named at `offset` the parameters `parameters`, and rewrites every
/// call of it in the project to pass them. `Err` says why it cannot be changed.
pub fn change_signature(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
    parameters: &[NewParameter],
) -> Result<SymbolMove, String> {
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

    // the body: renamed parameters renamed where it reads them; removed ones must be unread
    let model = SemanticModel::new(db, file);
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
            Some(new) => moved.edits.extend(reads.into_iter().map(|range| FileEdit {
                file: file_id,
                edit: Edit::range_replacement(new.name.clone(), range),
            })),
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
    moved.edits.push(FileEdit {
        file: file_id,
        edit: Edit::range_replacement(
            parameter_list(source, function, receiver_name.as_deref(), &old, parameters),
            function.parameters.range(),
        ),
    });

    // the calls
    let compatible = is_compatible(&old, parameters);
    let goto = find_goto_target(&model, &parsed, function.name.start())
        .ok_or_else(|| format!("`{name}` cannot be searched for"))?;
    let mut by_file: FxHashMap<File, Vec<TextRange>> = FxHashMap::default();
    for reference in
        references(db, file, &goto, ReferencesMode::RenameMultiFile).unwrap_or_default()
    {
        if reference.file() == file_id && reference.range() == function.name.range() {
            continue;
        }
        by_file
            .entry(reference.file())
            .or_default()
            .push(reference.range());
    }
    let mut files: Vec<File> = by_file.keys().copied().collect();
    files.sort_by_key(|file| file.path(db).as_str().to_string());
    for reference_file in files {
        let program_file = ProgramFile::new(db, reference_file, file.program(db));
        let parsed = parsed_module(db, program_file.python_file(db)).load(db);
        let text = source_text(db, reference_file);
        let text = text.as_str();
        let model = SemanticModel::new(db, program_file);
        let path = relative_path(db, reference_file);
        for range in &by_file[&reference_file] {
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
                moved.edits.push(FileEdit {
                    file: reference_file,
                    edit: Edit::range_replacement(rewritten, call.arguments.range()),
                });
            }
        }
    }
    Ok(moved)
}

/// A parameter of the function as it is, with where its name is.
struct OldParameter {
    parameter: SignatureParameter,
    name_range: TextRange,
    /// Its text without its default: its modifiers, name and annotation.
    text: String,
    /// The range of `text`'s name within it.
    name_in_text: TextRange,
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

/// Checks that `parameters` is a list python accepts, made of the function's own parameters and
/// new ones.
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

/// Whether every call of the function as it is stays valid, and means the same, against
/// `parameters`: the parameters it has keep their names, kinds, places and defaults, and those
/// added after them all have defaults.
fn is_compatible(old: &[OldParameter], parameters: &[NewParameter]) -> bool {
    let mut new = parameters.iter().peekable();
    for existing in old {
        // added keyword-only ones may come before `**kwargs`
        while let Some(added) = new.peek()
            && added.old_name.is_none()
            && existing.parameter.kind == ParameterKind::VariadicKeyword
        {
            if added.default.is_none() && !added.kind.is_variadic() {
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

/// The parenthesized parameter list `parameters` makes, the receiver first.
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
        let mut text = match existing {
            Some(existing) => {
                let mut text = existing.text.clone();
                text.replace_range(existing.name_in_text.to_std_range(), &parameter.name);
                text
            }
            None => match parameter.kind {
                ParameterKind::Variadic => format!("*{}", parameter.name),
                ParameterKind::VariadicKeyword => format!("**{}", parameter.name),
                _ => parameter.name.clone(),
            },
        };
        if let Some(default) = &parameter.default {
            let annotated =
                existing.is_some_and(|existing| existing.parameter.annotation.is_some());
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

/// An argument of a call, as python binds it.
#[derive(Clone, Copy)]
struct Argument<'a> {
    value: &'a Expr,
    keyword: Option<&'a str>,
}

/// The arguments of `call` rewritten for `parameters`, or `None` when they stay as they are.
fn rewrite_call(
    model: &SemanticModel<'_>,
    source: &str,
    call: &ast::ExprCall,
    bound: bool,
    old: &[OldParameter],
    parameters: &[NewParameter],
) -> Result<Option<String>, String> {
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

    // write them again for the new list
    let mut by_position: Vec<(Option<TextSize>, String)> = Vec::new();
    let mut by_keyword: Vec<(Option<TextSize>, String)> = Vec::new();
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
                    by_position.extend(
                        extra_positional
                            .iter()
                            .map(|value| (Some(value.start()), source[value.range()].to_string())),
                    );
                    extra_positional.clear();
                }
                continue;
            }
            ParameterKind::VariadicKeyword => {
                by_keyword.extend(extra_keyword.iter().map(|argument| {
                    (
                        Some(argument.value.start()),
                        format!(
                            "{}={}",
                            argument.keyword.unwrap_or_default(),
                            &source[argument.value.range()]
                        ),
                    )
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
        let (start, value, was_keyword) = match (passed, &parameter.value) {
            (Some(argument), _) => (
                Some(argument.value.start()),
                source[argument.value.range()].to_string(),
                argument.keyword.is_some(),
            ),
            (None, Some(value)) if slot.is_none() => (None, value.clone(), false),
            (None, _) => {
                if parameter.default.is_none() {
                    return Err(format!("has nothing to pass for `{}`", parameter.name));
                }
                positional_ok = false;
                continue;
            }
        };
        if positional_ok && parameter.kind.is_positional() && !was_keyword {
            by_position.push((start, value));
        } else {
            if parameter.kind == ParameterKind::PositionalOnly {
                return Err(format!(
                    "would have to pass `{}` by keyword, which it only takes by position",
                    parameter.name
                ));
            }
            positional_ok = false;
            by_keyword.push((start, format!("{}={value}", parameter.name)));
        }
    }
    if !extra_positional.is_empty() || !extra_keyword.is_empty() {
        return Err("passes extra arguments the new parameters have nowhere to take".to_string());
    }

    // what is dropped, or moved after something written after it, must have no effect
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
    by_keyword.sort_by_key(|(start, _)| start.unwrap_or(TextSize::from(u32::MAX)));
    let order: Vec<TextSize> = by_position
        .iter()
        .chain(&by_keyword)
        .filter_map(|(start, _)| *start)
        .collect();
    for (earlier, later) in order.iter().zip(order.iter().skip(1)) {
        if later < earlier {
            let moved = arguments
                .args
                .iter()
                .chain(arguments.keywords.iter().map(|keyword| &keyword.value))
                .find(|value| value.start() == *earlier);
            if let Some(moved) = moved
                && !is_effect_free(model, moved)
            {
                return Err(format!(
                    "would evaluate `{}` after an argument written after it",
                    &source[moved.range()]
                ));
            }
        }
    }

    let mut written: Vec<String> = Vec::new();
    if let Some(receiver) = receiver_argument {
        written.push(source[receiver.range()].to_string());
    }
    written.extend(by_position.into_iter().map(|(_, text)| text));
    written.extend(by_keyword.into_iter().map(|(_, text)| text));
    let rewritten = format!("({})", written.join(", "));
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
    if format!("({})", original.join(", ")) == rewritten {
        return Ok(None);
    }
    Ok(Some(rewritten))
}

/// The call whose callee the reference at `covering` names: `f` of `f(...)` or of `x.f(...)`.
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

/// Whether `call` passes the method's receiver itself, as a call through an instance does, rather
/// than as its first argument, as a call through the class does.
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

/// The function of `file` whose definition `offset` is in the name or the parameters of, and
/// whether it is a method that is passed its receiver.
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

/// The file and offset of the function named at `offset`: `offset` itself when it is in a
/// definition's name or parameters, or else where the name at `offset` is defined.
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

/// `file`'s path relative to the project's root.
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

    use super::{NewParameter, ParameterKind, change_signature, function_signature};
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

    /// `name`, as the parameter `old` was, of `kind`, with `default`.
    fn kept(name: &str, old: &str, kind: ParameterKind, default: Option<&str>) -> NewParameter {
        NewParameter {
            name: name.to_string(),
            old_name: Some(old.to_string()),
            kind,
            default: default.map(ToString::to_string),
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
            value: value.map(ToString::to_string),
        }
    }

    use ParameterKind::{KeywordOnly, PositionalOrKeyword};

    impl CursorTest {
        fn change(&self, parameters: &[NewParameter]) -> String {
            let changed = salsa::attach(&self.db, || {
                change_signature(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                    parameters,
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

    /// Without the call that unpacks into it, or the one that leaves `height` to the default it
    /// loses, swapping the two parameters swaps what each call passes by position, and a call
    /// that passed `height` by keyword passes both by keyword, in the order it wrote them.
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

    /// `forward` unpacks `*args` into `area`, which cannot be rebound.
    #[test]
    fn a_call_that_unpacks_is_refused() {
        let test = project(GEO);
        assert_snapshot!(test.change(&[
            kept("height", "height", PositionalOrKeyword, None),
            kept("width", "width", PositionalOrKeyword, None),
            kept("scale", "scale", KeywordOnly, Some("1.0")),
        ]), @"refused: sig/geo.py:6 unpacks arguments into the call, which cannot be told apart");
    }

    /// A parameter added with a default leaves every call valid, so none is touched, the one
    /// that unpacks included.
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

    /// A required parameter is passed the value given, by position while it can be.
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

    /// A rename renames the body's reads and the calls' keywords.
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

    /// Dropping an argument whose evaluation could have an effect would lose the effect.
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

    /// A method called through an instance is passed its receiver; through the class, its first
    /// argument is the receiver.
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
}
