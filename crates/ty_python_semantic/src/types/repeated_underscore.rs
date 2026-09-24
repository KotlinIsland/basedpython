//! basedpython: a parameter list that spells `_` more than once
//!
//! python refuses two parameters of one name, so the lowering gives every `_` after the
//! first a name of its own. those names are the lowering's rather than the author's, so no
//! call reaches such a parameter by keyword: it is positional-only in the python. a method
//! that overrides one is the exception, and takes the names the overridden method gives
//! those positions, so a call written against the base keeps working on the override — bar
//! a name the base's own lowering numbered a `_` with, which no call passes either
//!
//! what each parameter is called in the python, and how many of them a call reaches by
//! position alone, is decided here once — for ty's signature of the definition and for the
//! transpiler's output alike

use std::collections::HashSet;

use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ruff_python_stdlib::identifiers::is_identifier_continuation;
use ty_python_core::{FileScopeId, SemanticIndex};

use crate::types::signatures::Parameter;

/// the names a module spells anywhere in the text its author wrote — every run of
/// identifier characters, in code, strings and comments alike
///
/// a repeated `_` is numbered to a name outside these, so the parameter cannot
/// shadow a name the function reads from an enclosing scope:
///
/// ```by
/// _2 = [9]
///
/// def f(_: int, _: int) -> list[int]:
///     return _2  # the module's `_2`, so the second parameter is `_3`
/// ```
///
/// it is read off the written text rather than out of any scope, so it takes no
/// analysis to answer and every reader of a definition answers it the same way. the
/// text is the one written, before any rewrite: the transpiler's passes see a module
/// its enum lowering has added names to, and the native compiler does not
#[derive(Clone, Copy, Debug)]
pub struct WrittenNames<'src> {
    source: &'src str,
}

impl<'src> WrittenNames<'src> {
    /// the names spelled in `source`, the text of a module as it was written
    pub fn new(source: &'src str) -> Self {
        Self { source }
    }

    /// a name the module spells nowhere and `taken` does not answer for: `stem` itself when
    /// it is free, and `stem2`, `stem3`, … when it is not
    ///
    /// a binding the lowering writes under a name the source wrote would shadow it — the
    /// `inner` a `decorator def` writes standing where an option of that name is declared,
    /// which the dispatcher then passed the dispatcher itself for
    pub fn fresh_outside(self, stem: &str, taken: impl Fn(&str) -> bool) -> String {
        let free = |name: &str| !self.spells(name) && !taken(name);
        if free(stem) {
            return stem.to_owned();
        }
        // candidates only ever count up, so one never repeats an earlier one. a module
        // spells finitely many names, and each candidate is longer than the last, so one
        // of them is free long before the numbers run out
        (2u32..u32::MAX)
            .map(|number| format!("{stem}{number}"))
            .find(|candidate| free(candidate))
            .unwrap_or_else(|| stem.to_owned())
    }

    /// whether a name spelled anywhere in the module begins with `prefix`
    pub(crate) fn spells_a_name_beginning(self, prefix: &str) -> bool {
        self.source.match_indices(prefix).any(|(start, _)| {
            !self.source[..start]
                .chars()
                .next_back()
                .is_some_and(is_identifier_continuation)
        })
    }

    /// whether `name` is spelled anywhere in the module, as a whole run of identifier
    /// characters rather than as a part of a longer one
    pub fn spells(self, name: &str) -> bool {
        self.source.match_indices(name).any(|(start, _)| {
            let before = self.source[..start].chars().next_back();
            let after = self.source[start + name.len()..].chars().next();
            !before.is_some_and(is_identifier_continuation)
                && !after.is_some_and(is_identifier_continuation)
        })
    }
}

/// where a parameter stands in a parameter list. the order is the one the repeated `_`s
/// are numbered in, which is not the order they are declared in: a `*args` is declared
/// before the keyword-only parameters and numbered after them
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParameterSlot {
    PositionalOnly,
    PositionalOrKeyword,
    KeywordOnly,
    Variadic,
    KeywordVariadic,
}

impl ParameterSlot {
    const fn is_positional(self) -> bool {
        matches!(self, Self::PositionalOnly | Self::PositionalOrKeyword)
    }
}

/// each of `parameters` as its slot and the name the source spells, in declaration order
pub fn parameter_slots(parameters: &ast::Parameters) -> Vec<(ParameterSlot, &str)> {
    let mut slots = Vec::with_capacity(parameters.len());
    for parameter in &parameters.posonlyargs {
        slots.push((ParameterSlot::PositionalOnly, parameter.name().as_str()));
    }
    for parameter in &parameters.args {
        slots.push((
            ParameterSlot::PositionalOrKeyword,
            parameter.name().as_str(),
        ));
    }
    if let Some(parameter) = &parameters.vararg {
        slots.push((ParameterSlot::Variadic, parameter.name.as_str()));
    }
    for parameter in &parameters.kwonlyargs {
        slots.push((ParameterSlot::KeywordOnly, parameter.name().as_str()));
    }
    if let Some(parameter) = &parameters.kwarg {
        slots.push((ParameterSlot::KeywordVariadic, parameter.name.as_str()));
    }
    slots
}

/// the receiver an inline protocol's method writes first, `self` in
/// `protocol(def m(self, x: int) -> str)`. the parser marks it as a label, since it is a
/// parameter's name where every other element of the list is a type
pub fn protocol_method_receiver(signature: &ast::ExprCallableType) -> Option<&ast::ExprName> {
    match signature.args.first() {
        Some(ast::Expr::Name(name)) if name.ctx.is_invalid() => Some(name),
        _ => None,
    }
}

/// each parameter of a callable type, as its slot and the name it is written with, in the
/// order its signature lists them: the receiver of an inline protocol's method when
/// `method_receiver` says `callable` is one's signature and writes one, an implicit
/// receiver (`int.() -> str`), then each parameter written after them
///
/// a parameter written as a bare type is named `_<i>`, `i` its position among the written
/// ones. no call names it, and the `__call__` the transpiler declares a callable type with
/// writes it under that name, which a repeated `_` is then numbered around
pub fn callable_parameter_slots(
    callable: &ast::ExprCallableType,
    method_receiver: bool,
) -> Vec<(ParameterSlot, String)> {
    let receiver = if method_receiver {
        protocol_method_receiver(callable)
    } else {
        None
    };
    let offset = usize::from(receiver.is_some());
    let slash = callable
        .parameter_slash()
        .map(|index| (index as usize).saturating_sub(offset));
    let star = callable
        .parameter_star()
        .map(|index| (index as usize).saturating_sub(offset));
    let mut slots = Vec::with_capacity(callable.args.len() + 1);
    if let Some(receiver) = receiver {
        slots.push((ParameterSlot::PositionalOrKeyword, receiver.id.to_string()));
    }
    if callable.receiver.is_some() {
        slots.push((ParameterSlot::PositionalOnly, String::new()));
    }
    let mut keyword_only = false;
    for (index, element) in callable.args[offset..].iter().enumerate() {
        keyword_only |= Some(index) == star;
        let named = if slash.is_some_and(|slash| index < slash) {
            ParameterSlot::PositionalOnly
        } else if keyword_only {
            ParameterSlot::KeywordOnly
        } else {
            ParameterSlot::PositionalOrKeyword
        };
        let slot = match element {
            ast::Expr::Named(parameter) => match parameter.target.as_ref() {
                ast::Expr::Starred(starred) => match starred.value.as_ref() {
                    ast::Expr::Starred(inner) => (
                        ParameterSlot::KeywordVariadic,
                        inner
                            .value
                            .as_name_expr()
                            .map_or("kwargs", |name| name.id.as_str())
                            .to_owned(),
                    ),
                    value => {
                        keyword_only = true;
                        // the anonymous `*: *Ts` carries an empty name
                        let name = value
                            .as_name_expr()
                            .map(|name| name.id.as_str())
                            .filter(|name| !name.is_empty())
                            .unwrap_or("args");
                        (ParameterSlot::Variadic, name.to_owned())
                    }
                },
                target => (
                    named,
                    target
                        .as_name_expr()
                        .map_or_else(|| "_".to_owned(), |name| name.id.to_string()),
                ),
            },
            ast::Expr::Starred(starred) => match starred.value.as_ref() {
                ast::Expr::Starred(_) => (ParameterSlot::KeywordVariadic, "kwargs".to_owned()),
                _ => {
                    keyword_only = true;
                    (ParameterSlot::Variadic, "args".to_owned())
                }
            },
            _ => (ParameterSlot::PositionalOnly, format!("_{index}")),
        };
        slots.push(slot);
    }
    slots
}

/// a positional parameter of the method a definition overrides
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub struct BaseParameter {
    pub(crate) name: Name,
    pub(crate) positional_only: bool,
    /// the parameter is one of the base's own repeated `_`s, so `name` is the lowering's
    /// rather than one its author wrote
    pub(crate) underscore: bool,
}

impl BaseParameter {
    /// whether an override's `_` in this position has to take this parameter's name. a name
    /// the base's lowering numbered a `_` with is no name a call can pass: it is
    /// positional-only, so the override numbers its own `_` there, clear of the names its
    /// module spells — among them every name its body reads
    fn imposes_name(&self) -> bool {
        !(self.underscore && self.positional_only && self.name != "_")
    }
}

/// how the lowering writes a parameter list that repeats `_`
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub struct UnderscoreLowering {
    /// the parameter of the overridden method each positional `_` takes its name from, in
    /// declaration order. `None` when they are numbered instead
    inherited: Option<Box<[BaseParameter]>>,
    /// how many of the leading positional parameters a call reaches by position alone
    positional_only: usize,
}

impl UnderscoreLowering {
    /// how many of the leading positional parameters a call reaches by position alone
    pub fn positional_only(&self) -> usize {
        self.positional_only
    }

    /// whether the positional `_`s take their names from the overridden method. the first
    /// of them is called `_` in the python then only where the base calls its parameter `_`
    pub fn is_inherited(&self) -> bool {
        self.inherited.is_some()
    }

    /// the name each of `slots` binds in the python, in declaration order. `slots` is the
    /// parameter list this lowering was decided for, and `written` the module it is in
    pub fn names(&self, slots: &[(ParameterSlot, &str)], written: WrittenNames) -> Vec<Name> {
        let mut spelled: Vec<(ParameterSlot, Name)> = slots
            .iter()
            .map(|(slot, name)| (*slot, Name::from(*name)))
            .collect();
        let kept = self.inherited.as_ref().map(|inherited| {
            let mut kept = vec![false; spelled.len()];
            let underscores = spelled
                .iter_mut()
                .zip(&mut kept)
                .filter(|((slot, name), _)| slot.is_positional() && name == "_");
            for (((_, name), kept), base) in underscores.zip(inherited) {
                if base.imposes_name() {
                    name.clone_from(&base.name);
                    *kept = true;
                }
            }
            kept
        });
        let spelled: Vec<(ParameterSlot, &str)> = spelled
            .iter()
            .map(|(slot, name)| (*slot, name.as_str()))
            .collect();
        number_underscores(&spelled, kept.as_deref(), written)
    }
}

/// the name each parameter of a list takes in the python, in declaration order, which of them
/// are one of its `_`s, and how many of the leading positional ones a call reaches by position
/// alone
pub(crate) struct LoweredParameters {
    names: Vec<Name>,
    /// which of the parameters are written `_`. every one after the first has a python name
    /// the author never wrote, so the signature shows each as the `_` written, and a
    /// diagnostic about one says which it is by position
    underscores: Vec<bool>,
    positional_only: usize,
}

impl LoweredParameters {
    /// how the parameter list `slots`, written in the module `written`, is checked when
    /// `lowering` writes it
    pub(crate) fn new(
        lowering: &UnderscoreLowering,
        slots: &[(ParameterSlot, &str)],
        written: WrittenNames,
    ) -> Self {
        // a name taken from an overridden method is a name the author can spell, unless the
        // base has it from a repeated `_` of its own
        let mut inherited = lowering.inherited.iter().flatten();
        let underscores = slots
            .iter()
            .map(|(slot, spelled)| {
                *spelled == "_"
                    && (!lowering.is_inherited()
                        || !slot.is_positional()
                        || inherited.next().is_some_and(|base| base.underscore))
            })
            .collect();
        Self {
            names: lowering.names(slots, written),
            underscores,
            positional_only: lowering.positional_only(),
        }
    }

    /// `parameters`, the ones of the list in declaration order, under the names and kinds
    /// the lowering writes them with
    pub(crate) fn apply<'db>(&self, parameters: Vec<Parameter<'db>>) -> Vec<Parameter<'db>> {
        if self.names.len() != parameters.len() {
            return parameters;
        }
        parameters
            .into_iter()
            .zip(self.names.iter().zip(&self.underscores))
            .enumerate()
            .map(|(index, (parameter, (name, underscore)))| {
                parameter.lowered_as(name.clone(), index < self.positional_only, *underscore)
            })
            .collect()
    }
}

/// why the lowering cannot write a parameter list that repeats `_`
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub enum UnderscoreRefusal {
    /// a `_` after `*` or `*args` that is not the one keeping the name `_`. only a keyword
    /// reaches a keyword-only parameter, and its name is not the author's
    KeywordOnly { index: usize },
    /// a named parameter before a `_` that has to be positional-only. python's `/` makes
    /// every parameter before it positional-only, and that is a change to a parameter the
    /// author named, which is theirs to write
    DraggedPositionalOnly { index: usize, name: Name },
    /// the name a `_` takes from the overridden method is one the body reads from an
    /// enclosing scope, which the parameter would then shadow
    ShadowsEnclosingName { index: usize, name: Name },
    /// the `_`s have to be positional-only, and the python the module targets has no `/`
    /// to make them so
    NoPositionalOnlySyntax { index: usize },
}

impl UnderscoreRefusal {
    /// the declaration index of the parameter the refusal is about
    pub(crate) fn index(&self) -> usize {
        match self {
            Self::KeywordOnly { index }
            | Self::DraggedPositionalOnly { index, .. }
            | Self::ShadowsEnclosingName { index, .. }
            | Self::NoPositionalOnlySyntax { index } => *index,
        }
    }

    /// what is wrong, in the words a diagnostic and a transpile error both use
    pub fn message(&self) -> String {
        match self {
            Self::KeywordOnly { .. } => {
                "a repeated `_` parameter cannot be keyword-only".to_owned()
            }
            Self::DraggedPositionalOnly { name, .. } => {
                format!("the repeated `_` parameters after `{name}` make it positional-only")
            }
            Self::ShadowsEnclosingName { name, .. } => {
                format!("a repeated `_` parameter named `{name}` shadows a `{name}` the body reads")
            }
            Self::NoPositionalOnlySyntax { .. } => {
                "a repeated `_` parameter needs a `/`, which python before 3.8 does not have"
                    .to_owned()
            }
        }
    }

    /// how to write the definition so it is accepted
    pub fn help(&self) -> String {
        match self {
            Self::KeywordOnly { .. } => {
                "only a keyword reaches it, and it has no name of its own: move it before the `*`, \
                 or name it"
                    .to_owned()
            }
            Self::DraggedPositionalOnly { .. } => {
                "write the `/` after the last `_` to make them positional-only".to_owned()
            }
            Self::ShadowsEnclosingName { name, .. } => format!(
                "the parameter takes its name from the method it overrides; rename the `{name}` \
                 the body reads"
            ),
            Self::NoPositionalOnlySyntax { .. } => {
                "name the parameters, or require python 3.8 or later".to_owned()
            }
        }
    }
}

/// how the lowering writes `slots`, a parameter list in declaration order, when it spells
/// `_` more than once. `None` when it does not
///
/// `slash` says whether the python the module targets has the `/`: python 3.8 added it, and
/// before it nothing makes a parameter positional-only — so a list whose `_`s need one is
/// refused there, rather than lowered to parameters a call reaches by the number
///
/// `receiver` says whether the first parameter is the receiver a method binds, which a call
/// never passes by keyword, so making it positional-only changes nothing a caller wrote.
/// `base` is the overridden method's positional parameters, by position, when the definition
/// overrides one. `reads_from_enclosing_scope` says whether the body reads a name from an
/// enclosing scope, which a parameter of that name would then shadow
pub fn lower_repeated_underscores(
    slots: &[(ParameterSlot, &str)],
    receiver: bool,
    base: Option<&[Option<BaseParameter>]>,
    reads_from_enclosing_scope: impl Fn(&str) -> bool,
    slash: bool,
) -> Option<Result<UnderscoreLowering, UnderscoreRefusal>> {
    let underscores: Vec<usize> = (0..slots.len())
        .filter(|&index| slots[index].1 == "_")
        .collect();
    if underscores.len() < 2 {
        return None;
    }
    let written_positional_only = slots
        .iter()
        .filter(|(slot, _)| *slot == ParameterSlot::PositionalOnly)
        .count();
    let positional_underscores: Vec<usize> = underscores
        .iter()
        .copied()
        .filter(|&index| slots[index].0.is_positional())
        .collect();

    // every named parameter up to the `/` has to be positional-only as written, bar the
    // receiver
    let dragged = |positional_only: usize| {
        (0..positional_only).find_map(|index| {
            let (slot, name) = slots[index];
            let exempt =
                slot == ParameterSlot::PositionalOnly || name == "_" || (index == 0 && receiver);
            (!exempt).then(|| UnderscoreRefusal::DraggedPositionalOnly {
                index,
                name: Name::from(name),
            })
        })
    };

    // a `/` the lowering adds, where the target has none to add
    let missing_slash = |positional_only: usize| {
        (!slash && positional_only > written_positional_only).then(|| {
            UnderscoreRefusal::NoPositionalOnlySyntax {
                index: positional_only - 1,
            }
        })
    };

    if let Some(base) = base
        && let Some(inherited) = inherited_names(slots, &positional_underscores, base)
    {
        // no `_` keeps its name, so a keyword-only one would be numbered
        if let Some(&index) = underscores
            .iter()
            .find(|&&index| slots[index].0 == ParameterSlot::KeywordOnly)
        {
            return Some(Err(UnderscoreRefusal::KeywordOnly { index }));
        }
        // a name the list already gives a parameter is read from the definition itself, so a
        // parameter taking it shadows nothing. only `_` can be one: an override of a method
        // whose own `_`s are numbered takes `_` for the first of them
        if let Some((index, parameter)) = inherited.iter().find(|(_, parameter)| {
            parameter.imposes_name()
                && !slots
                    .iter()
                    .any(|(_, spelled)| *spelled == parameter.name.as_str())
                && reads_from_enclosing_scope(&parameter.name)
        }) {
            return Some(Err(UnderscoreRefusal::ShadowsEnclosingName {
                index: *index,
                name: parameter.name.clone(),
            }));
        }
        let positional_only = inherited
            .iter()
            .filter(|(_, parameter)| parameter.positional_only)
            .map(|(index, _)| index + 1)
            .max()
            .unwrap_or(0)
            .max(written_positional_only);
        if let Some(refusal) = dragged(positional_only).or_else(|| missing_slash(positional_only)) {
            return Some(Err(refusal));
        }
        return Some(Ok(UnderscoreLowering {
            inherited: Some(
                inherited
                    .into_iter()
                    .map(|(_, parameter)| parameter.clone())
                    .collect(),
            ),
            positional_only,
        }));
    }

    // the first `_` in numbering order keeps its name, and every other one is numbered
    let kept = underscores
        .iter()
        .copied()
        .min_by_key(|&index| (slots[index].0, index));
    let numbered = underscores
        .iter()
        .copied()
        .filter(|&index| Some(index) != kept);
    if let Some(index) = numbered
        .clone()
        .find(|&index| slots[index].0 == ParameterSlot::KeywordOnly)
    {
        return Some(Err(UnderscoreRefusal::KeywordOnly { index }));
    }
    let positional_only = numbered
        .filter(|&index| slots[index].0.is_positional())
        .map(|index| index + 1)
        .max()
        .unwrap_or(0)
        .max(written_positional_only);
    if let Some(refusal) = dragged(positional_only).or_else(|| missing_slash(positional_only)) {
        return Some(Err(refusal));
    }
    Some(Ok(UnderscoreLowering {
        inherited: None,
        positional_only,
    }))
}

/// the parameter of `base` each of `positional_underscores` stands in for, by position
///
/// `None` unless every one of them has a counterpart, and every counterpart whose name the
/// `_` has to take ([`BaseParameter::imposes_name`]) has a name the definition does not
/// already give a parameter of its own. an override that lines up with its base in part
/// does not line up with it, and is reported as an invalid override
fn inherited_names<'a>(
    slots: &[(ParameterSlot, &str)],
    positional_underscores: &[usize],
    base: &'a [Option<BaseParameter>],
) -> Option<Vec<(usize, &'a BaseParameter)>> {
    if positional_underscores.is_empty() {
        return None;
    }
    let own: HashSet<&str> = slots
        .iter()
        .map(|(_, name)| *name)
        .filter(|name| *name != "_")
        .collect();
    positional_underscores
        .iter()
        .map(|&index| {
            base.get(index)?
                .as_ref()
                .filter(|parameter| {
                    !parameter.imposes_name() || !own.contains(parameter.name.as_str())
                })
                .map(|parameter| (index, parameter))
        })
        .collect()
}

/// whether a parameter named `name` would change what the body of the function whose
/// scope is `scope` reads: the body mentions the name itself, or a scope nested in it reads
/// the name without binding it, which python resolves through the function
///
/// a nested scope that binds the name reads its own, but one nested in *that* is still
/// asked, which can only answer yes where the answer did not matter
pub(crate) fn reads_from_enclosing_scope(
    index: &SemanticIndex,
    scope: FileScopeId,
    name: &str,
) -> bool {
    if index.place_table(scope).symbol_by_name(name).is_some() {
        return true;
    }
    let mut pending: Vec<FileScopeId> = index.child_scopes(scope).map(|(id, _)| id).collect();
    while let Some(scope) = pending.pop() {
        if let Some(symbol) = index.place_table(scope).symbol_by_name(name)
            && !symbol.is_global()
            && (!symbol.is_bound() || symbol.is_nonlocal())
        {
            return true;
        }
        pending.extend(index.child_scopes(scope).map(|(id, _)| id));
    }
    false
}

/// the name each of `parameters`, a parameter list written as each one's slot and source
/// name, binds in the python it lowers to, where no parameter takes a name from an
/// overridden method
///
/// a parameter list the transpiler writes out of something other than a `def` — the
/// `__call__` of a callable type naming its parameters — numbers a repeated `_` as the
/// `def` it declares would. a number is never one the list or the module in `written`
/// already spells
pub fn lowered_parameter_names(
    parameters: &[(ParameterSlot, &str)],
    written: WrittenNames,
) -> Vec<Name> {
    number_underscores(parameters, None, written)
}

/// `parameters` with every `_` numbered but the ones that keep the name: the first in
/// numbering order, or when the positional ones have taken the names of an overridden
/// method, the ones `kept` says took one, which for a `_` is a `_` the base calls `_` too
fn number_underscores(
    parameters: &[(ParameterSlot, &str)],
    kept: Option<&[bool]>,
    written: WrittenNames,
) -> Vec<Name> {
    let taken: HashSet<&str> = parameters.iter().map(|(_, name)| *name).collect();
    let mut order: Vec<usize> = (0..parameters.len()).collect();
    order.sort_by_key(|&index| parameters[index].0);
    let mut names: Vec<Name> = parameters
        .iter()
        .map(|(_, name)| Name::from(*name))
        .collect();
    let mut keep_first = kept.is_none();
    // candidates only ever count up, so one never repeats an earlier one
    let mut next = 2u32;
    for index in order {
        let name = parameters[index].1;
        if name != "_"
            || kept.is_some_and(|kept| kept.get(index).copied().unwrap_or(false))
            || std::mem::replace(&mut keep_first, false)
        {
            continue;
        }
        let candidate = loop {
            let candidate = format!("_{next}");
            next += 1;
            if !taken.contains(candidate.as_str()) && !written.spells(&candidate) {
                break candidate;
            }
        };
        names[index] = Name::from(candidate);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    use ParameterSlot::{KeywordOnly, PositionalOnly, PositionalOrKeyword, Variadic};

    fn lower(
        slots: &[(ParameterSlot, &str)],
        receiver: bool,
        base: Option<&[Option<BaseParameter>]>,
    ) -> Option<Result<UnderscoreLowering, UnderscoreRefusal>> {
        lower_repeated_underscores(slots, receiver, base, |_| false, true)
    }

    fn base(parameters: &[(&str, bool)]) -> Vec<Option<BaseParameter>> {
        parameters
            .iter()
            .map(|(name, positional_only)| {
                Some(BaseParameter {
                    name: Name::from(*name),
                    positional_only: *positional_only,
                    underscore: false,
                })
            })
            .collect()
    }

    #[test]
    fn a_single_underscore_is_left_alone() {
        assert_eq!(lower(&[(PositionalOrKeyword, "_")], false, None), None);
    }

    #[test]
    fn numbered_underscores_are_positional_only() {
        let slots = [(PositionalOrKeyword, "_"), (PositionalOrKeyword, "_")];
        let lowering = lower(&slots, false, None).unwrap().unwrap();
        assert_eq!(lowering.positional_only(), 2);
        assert_eq!(lowering.names(&slots, WrittenNames::new("")), ["_", "_2"]);
    }

    #[test]
    fn a_named_parameter_after_the_last_underscore_stays_a_keyword() {
        let slots = [
            (PositionalOrKeyword, "_"),
            (PositionalOrKeyword, "_"),
            (PositionalOrKeyword, "a"),
        ];
        let lowering = lower(&slots, false, None).unwrap().unwrap();
        assert_eq!(lowering.positional_only(), 2);
    }

    #[test]
    fn a_named_parameter_before_the_last_underscore_is_refused() {
        let slots = [
            (PositionalOrKeyword, "a"),
            (PositionalOrKeyword, "_"),
            (PositionalOrKeyword, "_"),
        ];
        assert_eq!(
            lower(&slots, false, None),
            Some(Err(UnderscoreRefusal::DraggedPositionalOnly {
                index: 0,
                name: Name::from("a")
            }))
        );
    }

    #[test]
    fn the_receiver_is_not_dragged() {
        let slots = [
            (PositionalOrKeyword, "self"),
            (PositionalOrKeyword, "_"),
            (PositionalOrKeyword, "_"),
        ];
        let lowering = lower(&slots, true, None).unwrap().unwrap();
        assert_eq!(lowering.positional_only(), 3);
    }

    #[test]
    fn a_written_slash_is_accepted() {
        let slots = [
            (PositionalOnly, "a"),
            (PositionalOnly, "_"),
            (PositionalOnly, "_"),
        ];
        let lowering = lower(&slots, false, None).unwrap().unwrap();
        assert_eq!(lowering.positional_only(), 3);
    }

    #[test]
    fn a_numbered_keyword_only_underscore_is_refused() {
        let slots = [(PositionalOrKeyword, "_"), (KeywordOnly, "_")];
        assert_eq!(
            lower(&slots, false, None),
            Some(Err(UnderscoreRefusal::KeywordOnly { index: 1 }))
        );
    }

    /// python before 3.8 has no `/`, so a list whose `_`s need one is refused there, and a
    /// `/` the author wrote asks for nothing more
    #[test]
    fn a_slash_the_target_cannot_write_is_refused() {
        let slots = [(PositionalOrKeyword, "_"), (PositionalOrKeyword, "_")];
        assert_eq!(
            lower_repeated_underscores(&slots, false, None, |_| false, false),
            Some(Err(UnderscoreRefusal::NoPositionalOnlySyntax { index: 1 }))
        );
        let written = [(PositionalOnly, "_"), (PositionalOnly, "_")];
        assert!(matches!(
            lower_repeated_underscores(&written, false, None, |_| false, false),
            Some(Ok(_))
        ));
    }

    #[test]
    fn a_variadic_underscore_needs_no_slash() {
        let slots = [
            (PositionalOrKeyword, "a"),
            (PositionalOrKeyword, "_"),
            (Variadic, "_"),
        ];
        let lowering = lower(&slots, false, None).unwrap().unwrap();
        assert_eq!(lowering.positional_only(), 0);
        assert_eq!(
            lowering.names(&slots, WrittenNames::new("")),
            ["a", "_", "_2"]
        );
    }

    #[test]
    fn an_override_takes_the_base_names_and_kinds() {
        let slots = [
            (PositionalOrKeyword, "self"),
            (PositionalOrKeyword, "_"),
            (PositionalOrKeyword, "_"),
        ];
        let base = base(&[("self", false), ("x", true), ("y", false)]);
        let lowering = lower(&slots, true, Some(&base)).unwrap().unwrap();
        assert_eq!(lowering.positional_only(), 2);
        assert_eq!(
            lowering.names(&slots, WrittenNames::new("")),
            ["self", "x", "y"]
        );
    }

    #[test]
    fn an_override_that_does_not_line_up_is_numbered() {
        let slots = [
            (PositionalOrKeyword, "self"),
            (PositionalOrKeyword, "_"),
            (PositionalOrKeyword, "_"),
        ];
        let base = base(&[("self", false), ("x", false)]);
        let lowering = lower(&slots, true, Some(&base)).unwrap().unwrap();
        assert!(!lowering.is_inherited());
    }

    /// an override of a method whose own `_`s are numbered takes `_` for the first of them,
    /// and the body's `_` is already one of the list's parameters
    #[test]
    fn an_inherited_underscore_shadows_nothing() {
        let slots = [(PositionalOrKeyword, "_"), (PositionalOrKeyword, "_")];
        let base: Vec<_> = base(&[("_", true), ("_2", true)])
            .into_iter()
            .map(|parameter| {
                parameter.map(|parameter| BaseParameter {
                    underscore: true,
                    ..parameter
                })
            })
            .collect();
        let lowering =
            lower_repeated_underscores(&slots, false, Some(&base), |name| name == "_", true)
                .unwrap()
                .unwrap();
        assert!(lowering.is_inherited());
        assert_eq!(lowering.positional_only(), 2);
        let variadic = [
            (PositionalOrKeyword, "_"),
            (PositionalOrKeyword, "_"),
            (Variadic, "_"),
        ];
        assert_eq!(
            lowering.names(&variadic, WrittenNames::new("")),
            [Name::from("_"), Name::from("_2"), Name::from("_3")]
        );
    }

    /// a name the base's lowering numbered a `_` with is positional-only, so no call passes
    /// it, and the override numbers its own `_` there, past the `_2` its body reads
    #[test]
    fn a_numbered_base_name_is_numbered_again() {
        let slots = [(PositionalOrKeyword, "_"), (PositionalOrKeyword, "_")];
        let base: Vec<_> = base(&[("_", true), ("_2", true)])
            .into_iter()
            .map(|parameter| {
                parameter.map(|parameter| BaseParameter {
                    underscore: true,
                    ..parameter
                })
            })
            .collect();
        let lowering =
            lower_repeated_underscores(&slots, false, Some(&base), |name| name == "_2", true)
                .unwrap()
                .unwrap();
        assert!(lowering.is_inherited());
        assert_eq!(lowering.positional_only(), 2);
        assert_eq!(
            lowering.names(&slots, WrittenNames::new("return _2")),
            [Name::from("_"), Name::from("_3")]
        );
    }

    #[test]
    fn an_inherited_name_the_body_reads_is_refused() {
        let slots = [(PositionalOrKeyword, "_"), (PositionalOrKeyword, "_")];
        let base = base(&[("x", false), ("y", false)]);
        assert_eq!(
            lower_repeated_underscores(&slots, false, Some(&base), |name| name == "y", true),
            Some(Err(UnderscoreRefusal::ShadowsEnclosingName {
                index: 1,
                name: Name::from("y")
            }))
        );
    }
}
