//! implicit-argument resolution for basedpython `context` parameters
//!
//! a `context` parameter that no explicit argument matches is filled from the
//! `context` declarations visible at the call site. resolution is by
//! assignability, not by name: a declaration is a candidate when its type is
//! assignable to the parameter's declared type. the innermost scope with at
//! least one candidate wins; more than one candidate in that scope is
//! ambiguous. in the scope containing the call only declarations lexically
//! before the call are considered; enclosing-scope declarations count
//! regardless of position (they are read late, like any closed-over name).
//! a function's own `context` parameters are declarations in its body, so
//! context requirements propagate through call chains
//!
//! the names a trailing lambda block binds implicitly count too: `it`, and
//! `self` when the block's callback declares a receiver. nobody writes either,
//! so they are ambient in the block body the same way a `context` declaration
//! is ambient in its scope
//!
//! the lowering writes the resolved *name* at the call site, so a candidate a
//! nearer scope shadows is not offered — the emitted argument would read that
//! scope's value instead
//!
//! candidates are typed at their declaration site (`binding_type` of the
//! declaration's definition), so a later reassignment that changes the type
//! is not accounted for

use ruff_db::files::{File, FileRange};
use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_core::definition::Definition;
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::{NodeWithScopeKind, ScopeId, ScopeKind};
use ty_python_core::{place_table, semantic_index};

use crate::Db;
use crate::types::ProgramEnvironment;
use crate::types::receivers::{ImplicitReceiverName, implicit_receiver_name};
use crate::types::signatures::{Parameter, Parameters, Signature};
use crate::types::trailing_lambda::{enclosing_block_callee, trailing_lambda_it_type};
use crate::types::{Type, binding_type};

/// the outcome of resolving one unmatched `context` parameter at a call site
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextResolution<'db> {
    /// exactly one candidate in the winning scope matches
    Resolved {
        name: Name,
        ty: Type<'db>,
        binding: CandidateBinding<'db>,
    },
    /// no visible candidate is assignable to the parameter
    NotFound,
    /// several candidates in the winning scope match, in source order
    Ambiguous(Vec<Name>),
    /// a candidate that matches in the winning scope is a `_` parameter its
    /// function repeats. python binds only one of those parameters to the name,
    /// and which one a read of `_` means is not decided, so the value is refused
    /// rather than guessed
    RepeatedUnderscore,
    /// the parameter being filled is a `_` its function repeats. which of those
    /// parameters a keyword `_` names is not decided, so no argument can be
    /// written for it
    RepeatedUnderscoreParameter,
}

/// a value that can fill a `context` parameter, found in one scope
struct Candidate<'db> {
    name: Name,
    /// full range of the declaration statement — used to exclude a
    /// self-referential declaration from its own value's call sites. `None`
    /// for a name a trailing lambda block binds implicitly: it is bound before
    /// the body runs, and has no value expression a call could sit inside
    range: Option<TextRange>,
    binding: CandidateBinding<'db>,
    /// whether this is a `_` parameter of a function that has several
    is_repeated_underscore: bool,
}

/// where a candidate's type comes from, and how a call site can name it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateBinding<'db> {
    /// a `context` declaration or `context` parameter written in the source
    Written(Definition<'db>),
    /// `it`, which a trailing lambda block binds implicitly. The block's `it`
    /// parameter is synthetic and carries no annotation, so its type comes from
    /// the callee's callback signature rather than from a definition
    BlockArgument(Type<'db>),
    /// `self`, the receiver a trailing lambda block binds implicitly. The
    /// lowering gives the receiver a name of its own, so a call site filling a
    /// `context` parameter from it cannot simply write `self`
    BlockReceiver(Type<'db>),
}

impl<'db> CandidateBinding<'db> {
    fn ty(self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Self::Written(definition) => binding_type(db, definition),
            Self::BlockArgument(ty) | Self::BlockReceiver(ty) => ty,
        }
    }
}

/// resolve the implicit argument for `parameter`, an unmatched `context`
/// parameter among `parameters`, of a call at `call_offset` inside `scope`
pub(crate) fn resolve_context_argument<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    call_offset: TextSize,
    parameters: &Parameters<'db>,
    parameter: &Parameter<'db>,
) -> ContextResolution<'db> {
    if is_repeated_underscore(db, parameters, parameter) {
        return ContextResolution::RepeatedUnderscoreParameter;
    }
    let parameter_ty = parameter.annotated_type();
    let file = scope.file(db);
    let index = semantic_index(db, db.program_file(file));
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);

    // a trailing lambda block's implicit names belong to the block's own scope,
    // which is the first enclosing scope that is not a comprehension — a block
    // body may open one, and the names stay ambient inside it
    let mut reached_block_scope = false;
    // the scopes already passed on the way out from the call. the lowering writes
    // the resolved *name* at the call site, so a nearer scope binding that name
    // would make the emitted argument read a different value than the one
    // resolved here
    let mut nearer_scopes: Vec<ScopeId<'db>> = Vec::new();

    for (file_scope_id, ancestor) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = file_scope_id.to_scope_id(db, db.program_file(file));
        let is_call_scope = file_scope_id == scope.file_scope_id(db);
        let mut candidates: Vec<Candidate<'db>> = Vec::new();
        // the implicit names come first, so a `context` declaration in the block
        // body that reuses one of their names shadows it
        if !reached_block_scope && !matches!(ancestor.kind(), ScopeKind::Comprehension) {
            reached_block_scope = true;
            collect_block_candidates(db, env, file, scope, &mut candidates);
        }
        collect_candidates(index, ancestor.node(), &module, &mut candidates);

        // in the call's own scope only declarations lexically before the call
        // are visible; everywhere a declaration never feeds a call nested in
        // its own value expression
        candidates.retain(|candidate| {
            candidate.range.is_none_or(|range| {
                !range.contains(call_offset) && (!is_call_scope || range.start() < call_offset)
            })
        });

        // a scope between the call and this one holding the name shadows the
        // candidate, exactly as it would shadow an ordinary load of that name
        candidates.retain(|candidate| {
            !nearer_scopes.iter().any(|nearer| {
                place_table(db, *nearer)
                    .symbol_by_name(&candidate.name)
                    .is_some_and(|place| place.is_bound() || place.is_declared())
            })
        });

        // a name redeclared later shadows its earlier declaration. a repeated `_`
        // parameter is not a redeclaration: python binds only one of them to the
        // name, and which one is not decided, so each stays a candidate
        candidates.reverse();
        let mut seen = Vec::new();
        candidates.retain(|candidate| {
            if candidate.is_repeated_underscore {
                return true;
            }
            let fresh = !seen.contains(&candidate.name);
            if fresh {
                seen.push(candidate.name.clone());
            }
            fresh
        });
        candidates.reverse();

        let matching: Vec<(Candidate<'db>, Type<'db>)> = candidates
            .into_iter()
            .filter_map(|candidate| {
                let ty = candidate.binding.ty(db);
                ty.is_assignable_to(db, env, parameter_ty)
                    .then_some((candidate, ty))
            })
            .collect();

        if matching
            .iter()
            .any(|(candidate, _)| candidate.is_repeated_underscore)
        {
            return ContextResolution::RepeatedUnderscore;
        }
        match <[_; 1]>::try_from(matching) {
            Ok([(candidate, ty)]) => {
                return ContextResolution::Resolved {
                    name: candidate.name,
                    ty,
                    binding: candidate.binding,
                };
            }
            Err(matching) if matching.is_empty() => {}
            Err(matching) => {
                return ContextResolution::Ambiguous(
                    matching
                        .into_iter()
                        .map(|(candidate, _)| candidate.name)
                        .collect(),
                );
            }
        }

        nearer_scopes.push(ancestor_scope);
    }

    ContextResolution::NotFound
}

/// whether `parameter` is one of several `parameters` the source names `_`, and no keyword
/// but an ambiguous `_` reaches it
///
/// the lowering names such a parameter and makes it positional-only, so the signature does
/// not call it `_` any more. one that takes its name from the method it overrides is
/// reached by that name, and is filled like any other
fn is_repeated_underscore<'db>(
    db: &'db dyn Db,
    parameters: &Parameters<'db>,
    parameter: &Parameter<'db>,
) -> bool {
    let is_underscore =
        |parameter: &Parameter<'db>| written_name(db, parameter).is_some_and(|name| name == "_");
    is_underscore(parameter)
        && parameters
            .iter()
            .filter(|other| is_underscore(other))
            .count()
            > 1
        && parameter.keyword_name().is_none_or(|name| name == "_")
}

/// the name `parameter` is written with, which the signature need not share
fn written_name<'db>(db: &'db dyn Db, parameter: &Parameter<'db>) -> Option<Name> {
    match parameter.definition() {
        Some(definition) if let ScopedPlaceId::Symbol(symbol) = definition.place(db) => Some(
            place_table(db, definition.scope(db))
                .symbol(symbol)
                .name()
                .clone(),
        ),
        _ => parameter.name().cloned(),
    }
}

/// one `context` parameter of a call site and the value filling it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplicitContextArgument {
    /// the `context` parameter left unmatched by the explicit arguments
    pub parameter: Name,
    /// the in-scope value resolved for it, spelled as the source spells it
    pub variable: Name,
    /// where that name is written, for an IDE to navigate to. `None` for a name
    /// a trailing lambda block binds implicitly — nothing is written for it
    pub declaration: Option<FileRange>,
    /// whether `variable` is the receiver a trailing lambda block binds. The
    /// lowering gives the receiver a name of its own, so the transpiler must
    /// write that rather than `self`
    pub is_block_receiver: bool,
    /// whether the binding is a module-level `private` variable. The lowering
    /// emits it under an underscored name, so the transpiler must write that
    /// rather than the name the source spells. A nearer binding that merely
    /// shares the name is not this one, which is why the question is answered
    /// off the binding rather than the name
    pub is_module_private: bool,
}

/// the `context` arguments a call site leaves implicit
#[derive(Debug, Clone, Default)]
pub struct ImplicitContextArguments {
    /// the arguments the transpiler must append to the call, in parameter order
    pub arguments: Vec<ImplicitContextArgument>,
    /// the `context` parameters whose value would come from a repeated `_`
    /// parameter, which checking reports and the transpiler must refuse. which of
    /// the repeated parameters a read of `_` means is not decided, so no argument
    /// can be written for them
    pub from_repeated_underscore: Vec<Name>,
    /// whether a `context` parameter left to be filled is a `_` the callee
    /// repeats, which checking reports and the transpiler must refuse. which of
    /// those parameters a keyword `_` names is not decided, so no argument can be
    /// written for it
    pub for_repeated_underscore: bool,
}

impl ImplicitContextArguments {
    /// whether no `context` parameter of the call is decided by what is in scope:
    /// nothing is filled, and nothing is refused for coming from a repeated `_`
    pub fn is_empty(&self) -> bool {
        self.arguments.is_empty()
            && self.from_repeated_underscore.is_empty()
            && !self.for_repeated_underscore
    }
}

/// the implicit arguments the transpiler must append to `call`: for each
/// `context` parameter of `callee` that no explicit argument matches, the
/// in-scope declaration that fills it, in parameter order. parameters that
/// fail to resolve are skipped — checking already reported them. calls that
/// use `*` / `**` unpacking are skipped entirely: whether the unpacking covers
/// a parameter is not knowable statically
///
/// An overloaded callee is read through its first overload, restricted to the parameters
/// `overload_fillable_context_parameters` says every overload agrees on. Which overload a
/// call selects is decided elsewhere, so only an argument that is the right one for each of
/// them separately may be written. A parameter left out falls back to the default it
/// declares, which is what it does when no candidate is in scope at all.
pub fn implicit_context_arguments<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    callee: Type<'db>,
    call: &ast::ExprCall,
) -> ImplicitContextArguments {
    let has_unpacking = call.arguments.args.iter().any(ast::Expr::is_starred_expr)
        || call.arguments.keywords.iter().any(|kw| kw.arg.is_none());
    if has_unpacking {
        return ImplicitContextArguments::default();
    }

    let signatures = callee_signatures(db, callee);
    let Some(first) = signatures.first() else {
        return ImplicitContextArguments::default();
    };

    let index = semantic_index(db, db.program_file(file));
    let Some(file_scope_id) = index.try_expression_scope_id(&ast::ExprRef::from(call)) else {
        return ImplicitContextArguments::default();
    };
    let scope = file_scope_id.to_scope_id(db, db.program_file(file));

    let fillable =
        overload_fillable_context_parameters(db, env, scope, call.range().start(), &signatures);
    implicit_context_arguments_for(
        db,
        env,
        file,
        scope,
        first.parameters(),
        fillable.as_ref().map(|agreed| &agreed.fillable),
        call,
    )
}

/// the signatures `callee` may be called through, in declaration order
fn callee_signatures<'db>(db: &'db dyn Db, callee: Type<'db>) -> Box<[Signature<'db>]> {
    match callee {
        Type::FunctionLiteral(function) => {
            function.signature(db).overloads.iter().cloned().collect()
        }
        Type::BoundMethod(method) => method
            .bound_signatures(db)
            .overloads
            .iter()
            .cloned()
            .collect(),
        _ => Box::default(),
    }
}

/// the `context` parameters of an overload set that a call site may fill, or `None` when
/// `signatures` is a single signature and the question does not arise.
///
/// A call selects one overload, and which one is decided by the arguments it is given — not
/// here. So a parameter may be filled only where doing so is right for every overload at
/// once: the name has to mean the same thing in all of them, and it has to be one the call
/// either gives or does not give in all of them alike.
///
/// That second half is what restricts this to **keyword-only** parameters. A keyword names
/// the same parameter in every overload, so whether the call already supplies it cannot
/// depend on which one is selected; a positional slot can sit at a different index in each,
/// and then an argument written for one of them is an argument the others never asked for.
/// A `decorator def`'s options are keyword-only, which is the shape this is here for.
pub(crate) fn overload_fillable_context_parameters<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    call_offset: TextSize,
    signatures: &[Signature<'db>],
) -> Option<OverloadContextParameters> {
    let (first, rest) = signatures.split_first()?;
    if rest.is_empty() {
        return None;
    }
    let resolutions = |signature: &Signature<'db>| {
        let parameters = signature.parameters();
        parameters
            .iter()
            .filter(|parameter| parameter.is_context() && parameter.is_keyword_only())
            .filter_map(|parameter| {
                let name = parameter.name()?.clone();
                let resolution =
                    resolve_context_argument(db, env, scope, call_offset, parameters, parameter);
                Some((name, resolution))
            })
            .collect::<Vec<_>>()
    };
    let per_overload: Vec<_> = signatures.iter().map(resolutions).collect();
    let mut agreed = resolutions(first);
    for signature in rest {
        let other = resolutions(signature);
        agreed.retain(|entry| other.contains(entry));
    }
    let fillable: Vec<Name> = agreed.into_iter().map(|(name, _)| name).collect();

    // every keyword-only `context` parameter any overload declares, in the order they are
    // first written. the ones that are not fillable are exactly the ones the overloads did
    // not agree on, and an author has no way to see that from "no overload matches arguments"
    let mut disagreements: Vec<ContextDisagreement> = Vec::new();
    for (name, _) in per_overload.iter().flatten() {
        if fillable.contains(name) || disagreements.iter().any(|d| d.parameter == *name) {
            continue;
        }
        disagreements.push(ContextDisagreement {
            parameter: name.clone(),
            per_overload: per_overload
                .iter()
                .map(|resolutions| {
                    match resolutions.iter().find(|(declared, _)| declared == name) {
                        None => ContextChoice::Undeclared,
                        Some((_, ContextResolution::Resolved { name, .. })) => {
                            ContextChoice::Takes(name.clone())
                        }
                        Some(_) => ContextChoice::NoValue,
                    }
                })
                .collect(),
        });
    }
    Some(OverloadContextParameters {
        fillable,
        disagreements,
    })
}

/// what an overload set makes of its keyword-only `context` parameters at one call site
#[derive(Debug, Clone)]
pub(crate) struct OverloadContextParameters {
    /// the names every overload agrees on, which are the ones a call may be given implicitly
    pub(crate) fillable: Vec<Name>,
    /// the names they do not agree on, with what each overload made of each
    pub(crate) disagreements: Vec<ContextDisagreement>,
}

/// one `context` parameter an overload set could not fill implicitly, because its overloads do
/// not agree on it.
///
/// Nothing is written for such a parameter, so a call that does not pass it explicitly goes
/// without an argument — which on its own reads as an ordinary failure to match. What each
/// overload made of the parameter is kept so the report can say where they part.
#[derive(Debug, Clone)]
pub(crate) struct ContextDisagreement {
    pub(crate) parameter: Name,
    /// one entry per overload, in declaration order
    pub(crate) per_overload: Vec<ContextChoice>,
}

/// what one overload would have done with a `context` parameter
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextChoice {
    /// the overload does not declare the parameter as a keyword-only `context` one at all
    Undeclared,
    /// it declares it, and would take the value this declaration holds
    Takes(Name),
    /// it declares it, and no visible declaration supplies a value for it
    NoValue,
}

impl ContextChoice {
    /// how this overload's answer reads in a report, as the tail of "overload N …"
    pub(crate) fn describe(&self, parameter: &Name) -> String {
        match self {
            ContextChoice::Undeclared => format!("does not declare `{parameter}`"),
            ContextChoice::Takes(name) => format!("would take `{name}`"),
            ContextChoice::NoValue => format!("finds no value for `{parameter}`"),
        }
    }
}

/// [`implicit_context_arguments`] against one signature's parameter list. `fillable`
/// restricts it to the names an overload set agrees on; `None` places no restriction
fn implicit_context_arguments_for<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    parameters: &Parameters<'db>,
    fillable: Option<&Vec<Name>>,
    call: &ast::ExprCall,
) -> ImplicitContextArguments {
    let mut implicit = ImplicitContextArguments::default();
    let positional_count = call.arguments.args.len();
    let mut positional_index = 0;
    for parameter in parameters {
        let fills_positional_slot = parameter.is_positional();
        let matched_positionally = fills_positional_slot && positional_index < positional_count;
        if fills_positional_slot {
            positional_index += 1;
        }
        if !parameter.is_context() {
            continue;
        }
        let Some(name) = parameter.name() else {
            continue;
        };
        if fillable.is_some_and(|fillable| !fillable.contains(name)) {
            continue;
        }
        let matched_by_keyword = call
            .arguments
            .keywords
            .iter()
            .any(|kw| kw.arg.as_ref().is_some_and(|arg| arg.id == *name));
        // a keyword `_` is not taken to match a `_` the callee repeats: which of
        // those parameters it names is not decided
        if matched_positionally
            || (matched_by_keyword && !is_repeated_underscore(db, parameters, parameter))
        {
            continue;
        }
        let resolution =
            resolve_context_argument(db, env, scope, call.range().start(), parameters, parameter);
        if resolution == ContextResolution::RepeatedUnderscore {
            implicit.from_repeated_underscore.push(name.clone());
        } else if resolution == ContextResolution::RepeatedUnderscoreParameter {
            implicit.for_repeated_underscore = true;
        } else if let ContextResolution::Resolved {
            name: variable,
            binding,
            ..
        } = resolution
        {
            let declaration = match binding {
                CandidateBinding::Written(definition) => Some(definition.focus_range(
                    db,
                    &parsed_module(db, db.program_file(file).python_file(db)).load(db),
                )),
                CandidateBinding::BlockArgument(_) | CandidateBinding::BlockReceiver(_) => None,
            };
            let is_module_private = match binding {
                CandidateBinding::Written(definition) => {
                    definition.scope(db).file_scope_id(db).is_global()
                        && crate::types::visibility::private_symbols(db, file).contains(&variable)
                }
                CandidateBinding::BlockArgument(_) | CandidateBinding::BlockReceiver(_) => false,
            };
            implicit.arguments.push(ImplicitContextArgument {
                parameter: name.clone(),
                variable,
                declaration,
                is_block_receiver: matches!(binding, CandidateBinding::BlockReceiver(_)),
                is_module_private,
            });
        }
    }
    implicit
}

/// the `context` parameters of `callee` that a decoration cannot fill.
///
/// `@deco` is a call — it runs `deco(g)` — but it is the one call whose argument list the
/// source has nowhere to write. The lowering completes a call by appending the resolved
/// argument after the ones the source wrote, and a decoration has none to append to: the
/// parameter would quietly take its default, or, with no default, the decoration would
/// raise `TypeError`. Neither is the ambient value the declaration promised, so each such
/// parameter is reported and the decoration is left as written — the same bargain every
/// other unresolvable `context` parameter gets.
///
/// The decorated definition fills the first positional slot, exactly as it does at runtime,
/// so a `context` parameter that *is* that slot is not among these.
///
/// An overloaded callee is read through every one of its overloads, and only a parameter
/// left unfilled in all of them is reported — which overload a decoration selects is not
/// decided here, and a report true of every one of them is true whichever it is. This is
/// what reaches a `decorator def`, whose declaration is always the pair of overloads it is
/// applied in.
pub(crate) fn unfilled_decoration_context_parameters<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Vec<Name> {
    let signatures = match callee {
        Type::FunctionLiteral(function) => function.signature(db).clone(),
        Type::BoundMethod(method) => method.bound_signatures(db).clone(),
        _ => return Vec::new(),
    };
    let mut unfilled: Option<Vec<Name>> = None;
    for signature in &signatures.overloads {
        let mut decorated_slot_taken = false;
        let mut in_this_one = Vec::new();
        for parameter in signature.parameters() {
            if parameter.is_positional() && !decorated_slot_taken {
                decorated_slot_taken = true;
                continue;
            }
            if !parameter.is_context() {
                continue;
            }
            if let Some(name) = parameter.name() {
                in_this_one.push(name.clone());
            }
        }
        unfilled = Some(match unfilled {
            None => in_this_one,
            Some(so_far) => so_far
                .into_iter()
                .filter(|name| in_this_one.contains(name))
                .collect(),
        });
    }
    unfilled.unwrap_or_default()
}

/// collect the names the trailing lambda block containing `scope` binds
/// implicitly: `self` when its callback declares a receiver, and `it`.
///
/// Only the innermost enclosing block is asked. Every block binds `it`, so a
/// nearer block always shadows an outer one's — and the lowering gives every
/// block's receiver the same name, so an outer receiver is shadowed too. Both
/// queries answer for the innermost block only, which keeps a name resolved
/// here meaning the same thing it means to the transpiler.
///
/// `self` goes through the same query the checker resolves a bare `self` in a
/// block with, so a method's own `self` — or anything else claiming the name —
/// keeps its meaning here as well. `it` is offered only when the callee gives
/// it a type: an uninspectable callee leaves it `Unknown`, which is assignable
/// to every `context` parameter and would fill them all with a value the block
/// never receives
fn collect_block_candidates<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    out: &mut Vec<Candidate<'db>>,
) {
    if let Some(ImplicitReceiverName::Receiver(ty)) =
        implicit_receiver_name(db, env, file, scope, "self", None)
    {
        out.push(Candidate {
            name: Name::new_static("self"),
            range: None,
            binding: CandidateBinding::BlockReceiver(ty),
            is_repeated_underscore: false,
        });
    }

    if let Some(callee) = enclosing_block_callee(db, scope)
        && let Some(ty) = trailing_lambda_it_type(db, callee)
    {
        out.push(Candidate {
            name: Name::new_static("it"),
            range: None,
            binding: CandidateBinding::BlockArgument(ty),
            is_repeated_underscore: false,
        });
    }
}

/// collect the `context` declarations and `context` parameters belonging to
/// one scope, in source order. nested scopes are not entered — their
/// declarations belong to them
fn collect_candidates<'db>(
    index: &ty_python_core::SemanticIndex<'db>,
    node: &NodeWithScopeKind,
    module: &ruff_db::parsed::ParsedModuleRef,
    out: &mut Vec<Candidate<'db>>,
) {
    let mut push_params = |parameters: &ast::Parameters| {
        let underscores = parameters
            .iter()
            .filter(|parameter| parameter.name() == "_")
            .count();
        for parameter in parameters.iter().map(ast::AnyParameterRef::as_parameter) {
            if parameter.is_context
                && let Some(definition) = index.try_definition(parameter)
            {
                out.push(Candidate {
                    name: parameter.name.id.clone(),
                    range: Some(parameter.range()),
                    binding: CandidateBinding::Written(definition),
                    is_repeated_underscore: parameter.name.id == "_" && underscores > 1,
                });
            }
        }
    };

    match node {
        NodeWithScopeKind::Module => {
            collect_declarations(index, module.suite(), out);
        }
        NodeWithScopeKind::Function(function) => {
            let function = function.node(module);
            push_params(&function.parameters);
            collect_declarations(index, &function.body, out);
        }
        NodeWithScopeKind::Lambda(lambda) => {
            if let Some(parameters) = lambda.node(module).parameters.as_deref() {
                push_params(parameters);
            }
        }
        NodeWithScopeKind::Class(class) => {
            collect_declarations(index, &class.node(module).body, out);
        }
        _ => {}
    }
}

/// walk a statement suite for `context` declarations, descending into
/// compound statements but not into nested scopes
fn collect_declarations<'db>(
    index: &ty_python_core::SemanticIndex<'db>,
    suite: &[ast::Stmt],
    out: &mut Vec<Candidate<'db>>,
) {
    for stmt in suite {
        match stmt {
            ast::Stmt::AnnAssign(decl) => {
                if let Some(target) = context_declaration_target(decl)
                    && let Some(definition) = index.try_definition(decl)
                {
                    out.push(Candidate {
                        name: target.id.clone(),
                        range: Some(decl.range()),
                        binding: CandidateBinding::Written(definition),
                        is_repeated_underscore: false,
                    });
                }
            }
            ast::Stmt::If(stmt) => {
                collect_declarations(index, &stmt.body, out);
                for clause in &stmt.elif_else_clauses {
                    collect_declarations(index, &clause.body, out);
                }
            }
            ast::Stmt::While(stmt) => {
                collect_declarations(index, &stmt.body, out);
                collect_declarations(index, &stmt.orelse, out);
            }
            ast::Stmt::For(stmt) => {
                collect_declarations(index, &stmt.body, out);
                collect_declarations(index, &stmt.orelse, out);
            }
            ast::Stmt::With(stmt) => {
                collect_declarations(index, &stmt.body, out);
            }
            ast::Stmt::Try(stmt) => {
                collect_declarations(index, &stmt.body, out);
                for ast::ExceptHandler::ExceptHandler(handler) in &stmt.handlers {
                    collect_declarations(index, &handler.body, out);
                }
                collect_declarations(index, &stmt.orelse, out);
                collect_declarations(index, &stmt.finalbody, out);
            }
            ast::Stmt::Match(stmt) => {
                for case in &stmt.cases {
                    collect_declarations(index, &case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// if `decl` was written with the `context` prefix (`context NAME [: T] = value`, and any
/// modifier chain around it), return the target name
fn context_declaration_target(decl: &ast::StmtAnnAssign) -> Option<&ast::ExprName> {
    if !decl.is_context {
        return None;
    }
    decl.target.as_name_expr()
}
