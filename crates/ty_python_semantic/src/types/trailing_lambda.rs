//! basedpython: type-side queries for trailing lambda blocks
//!
//! a statement-level `<call>:` block passes its suite as the call's last
//! argument. the lowering (and the checker's synthetic call binding) pass
//! that argument by keyword — the callee's last declared parameter — so that
//! `f:` binds the lambda to the last parameter even when earlier parameters
//! are defaulted. the implicit `it` parameter takes its type from that
//! parameter's declared callable type

use ruff_db::parsed::parsed_module;
use ruff_python_ast::ParameterBorrow;
use ruff_python_ast::name::Name;
use ty_python_core::scope::{ScopeId, ScopeKind};
use ty_python_core::semantic_index;

use crate::Db;
use crate::types::signatures::{Parameter, Signature};
use crate::types::soundness::single_signature;
use crate::types::{Type, TypeContext, infer_expression_types};

/// the type of the expression the trailing lambda block whose body `scope` is in
/// is attached to
pub(crate) fn enclosing_block_callee_type<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> Option<Type<'db>> {
    Some(enclosing_block(db, scope)?.1)
}

/// the trailing lambda block whose body `scope` is in: the block's own scope,
/// and the type of the expression it is attached to. Walks out through
/// comprehension scopes (which a block body may open) but stops at the first
/// function, class, or module scope: a nested definition is its own body, not
/// the block's.
///
/// The callee is inferred as a standalone expression (registered by the semantic
/// index builder), which is independent of the enclosing definition's inference
/// — so asking for it from inside the block body is not a cycle.
///
/// Tracked because [implicit receiver] resolution asks it of *every* name a
/// basedpython file loads, and the answer is a property of the scope alone.
///
/// Asking it that often is what makes it re-enter itself: inferring the callee
/// can reach a definition whose own inference runs the block body, and the first
/// name that body loads asks for the callee again. The cycle starts from "this
/// scope is not a block body" and iterates, so a name resolved while the callee
/// is still being worked out simply does not see the receiver — the same
/// recovery the [extension] queries use
///
/// [implicit receiver]: crate::types::receivers::implicit_receiver_name
/// [extension]: crate::types::extensions::extensions_in_module
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| None,
    heap_size = ruff_memory_usage::heap_size
)]
pub(crate) fn enclosing_block<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> Option<(ScopeId<'db>, Type<'db>)> {
    let program_file = db.program_file(scope.file(db));
    let index = semantic_index(db, program_file);
    for (ancestor_id, ancestor) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        match ancestor.kind() {
            ScopeKind::Comprehension => continue,
            ScopeKind::Function => {}
            _ => return None,
        }
        let module = parsed_module(db, program_file.python_file(db)).load(db);
        let function = ancestor.node().as_function()?.node(&module);
        if !function.is_trailing_lambda {
            return None;
        }
        let callee = function.trailing_lambda_callee()?;
        let expression = index.try_expression(callee)?;
        let callee_ty = infer_expression_types(db, expression, TypeContext::default())
            .try_expression_type(callee)?;
        return Some((ancestor_id.to_scope_id(db, program_file), callee_ty));
    }
    None
}

/// basedpython: whether the callee's callback — its last declared parameter, the
/// one a trailing block binds — is marked `once`.
///
/// A `once` block runs exactly once (`with`-like); a non-`once` one runs an
/// unknown number of times, which restricts what it may do. Resolving the marker
/// means reaching the callee's function definition, so this is `false` for
/// anything but a function literal or a bound method (a callable-typed value
/// carries no such marker).
pub(crate) fn callee_callback_is_once<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    let function = match callee {
        Type::FunctionLiteral(function) => function,
        Type::BoundMethod(method) => method.function(db),
        _ => return false,
    };
    function
        .literal(db)
        .last_definition
        .callback_parameter_modifiers(db)
        .last_bound_once
}

/// basedpython: whether the callee's callback parameter is a borrow (`local` or
/// `once`) — the block is then confined to the call, so a captured loop variable
/// cannot dangle. `Some(true)` = borrowed, `Some(false)` = resolved but not a
/// borrow, `None` = the callee is not a resolvable function / bound method (an
/// opaque callee is left alone, like elsewhere in the borrow analysis).
pub(crate) fn callee_callback_is_borrowed<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<bool> {
    let function = match callee {
        Type::FunctionLiteral(function) => function,
        Type::BoundMethod(method) => method.function(db),
        _ => return None,
    };
    Some(
        function
            .literal(db)
            .last_definition
            .callback_parameter_modifiers(db)
            .last_bound_borrowed,
    )
}

/// the callee's last declared parameter, when the callee has a single
/// inspectable signature and that parameter is a plain (non-variadic) one.
/// `None` for overloaded / uninspectable callees and `*args` / `**kwargs`
fn last_parameter<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<Parameter<'db>> {
    let signature = match callee {
        // a callable-typed value (`a: (int) -> str`) — not covered by
        // `single_signature`, which reads function literals and bound methods
        Type::Callable(callable) => {
            let [signature] = callable.signatures(db).overloads.as_slice() else {
                return None;
            };
            signature.clone()
        }
        _ => single_signature(db, callee)?,
    };
    let parameter = signature.parameters().iter().next_back()?;
    if parameter.is_variadic() || parameter.is_keyword_variadic() {
        return None;
    }
    Some(parameter.clone())
}

/// the keyword a trailing lambda is passed with: the name of the callee's
/// last declared parameter. `None` — meaning "append the lambda as a
/// positional argument" — when the callee has no single inspectable
/// signature, or the last parameter is variadic, positional-only, or unnamed
pub(crate) fn trailing_lambda_keyword<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<Name> {
    let parameter = last_parameter(db, callee)?;
    if parameter.is_positional_only() {
        return None;
    }
    parameter.name().cloned()
}

/// the single signature of the callback a trailing lambda block fills: the
/// callable the callee's last declared parameter is annotated as. `None` for
/// anything else — an unannotated, non-callable or overloaded parameter
fn callback_signature<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<&'db Signature<'db>> {
    let parameter = last_parameter(db, callee)?;
    let Type::Callable(callable) = parameter.annotated_type() else {
        return None;
    };
    let [signature] = callable.signatures(db).overloads.as_slice() else {
        return None;
    };
    Some(signature)
}

/// whether the callback's leading parameter is a receiver, which the block binds
/// implicitly rather than as `it`
fn declares_receiver(signature: &Signature<'_>) -> bool {
    signature
        .parameters()
        .iter()
        .next()
        .is_some_and(Parameter::is_receiver)
}

/// the parameter the implicit `it` binds: the first parameter of the callback
/// the block fills that the block does not bind implicitly — the leading one, or
/// the one after the receiver when the callback declares one. `None` when that
/// shape doesn't hold
fn it_parameter<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<Parameter<'db>> {
    let signature = callback_signature(db, callee)?;
    let index = usize::from(declares_receiver(signature));
    Some(signature.parameters().get_positional(index)?.clone())
}

/// basedpython: whether the callee's callback passes an argument for the block to
/// bind as `it`.
///
/// `Some(false)` says the callback shape is inspectable and passes nothing, so the
/// block has no `it`; `Some(true)` that it passes one. `None` is "cannot tell" — an
/// overloaded, unannotated or non-callable parameter, or a callee with no single
/// signature.
///
/// The semantic index answers the same question while building the block's scope, but
/// it can only see a `def` in the file it is indexing: a callee reached through an
/// import is unresolvable there, and it assumes a binding rather than losing one. This
/// runs after inference, where an imported callee resolves like any other.
pub(crate) fn trailing_lambda_passes_it<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<bool> {
    let signature = callback_signature(db, callee)?;
    let parameters = signature.parameters();
    // the gradual `(...)` form is the deliberately unchecked one, and a variadic stands
    // for any number of arguments — neither settles whether one arrives
    if parameters.is_gradual()
        || parameters
            .iter()
            .any(|parameter| parameter.is_variadic() || parameter.is_keyword_variadic())
    {
        return None;
    }
    let index = usize::from(declares_receiver(signature));
    Some(parameters.get_positional(index).is_some())
}

/// the type of the implicit `it` parameter. `None` when the callee's callback
/// shape is not inspectable — `it` is then left untyped
pub(crate) fn trailing_lambda_it_type<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<Type<'db>> {
    Some(it_parameter(db, callee)?.annotated_type())
}

/// basedpython: the `local` / `once` modifier the callee declares on the
/// parameter `it` binds — the `local` of `def f(fn: (local int) -> None)`.
///
/// The block body is the *implementation* of that callback, so the value bound
/// to `it` is borrowed from the call and may not escape the block.
/// [`ParameterBorrow::None`] when the callee's callback shape is not
/// inspectable, which leaves the block unconstrained the way an opaque callee
/// does everywhere else in the borrow analysis.
pub(crate) fn trailing_lambda_it_borrow<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> ParameterBorrow {
    it_parameter(db, callee).map_or(ParameterBorrow::None, |parameter| parameter.borrow())
}

/// the type the block's callback declares as its *receiver* — the block body then
/// sees that type's members unqualified, and spells the receiver itself `self`.
/// `None` when the callback is an ordinary callable, which has no implicit
/// member scope
pub(crate) fn trailing_lambda_receiver_type<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<Type<'db>> {
    let parameter = last_parameter(db, callee)?;
    crate::types::receivers::receiver_type(db, parameter.annotated_type())
}

/// a callback parameter a trailing lambda block has no way to bind
pub(crate) enum UnbindableParameters {
    /// more parameters than the single `it` a block binds
    TooMany(usize),
    /// a variadic parameter, which stands for any number of arguments
    Variadic,
}

/// the parameters of the callee's callback that a trailing lambda block cannot
/// bind. A block binds its callback's receiver implicitly and one further
/// argument as `it`, so anything beyond that is unreachable from the body — and
/// passed to a block that has no parameter for it at runtime.
///
/// `None` when the block covers the callback, when the callback is not an
/// inspectable single-signature callable, or when its parameter list is gradual
/// (`(...) -> None`, the deliberately unchecked form)
pub(crate) fn trailing_lambda_unbindable_parameters<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<UnbindableParameters> {
    let signature = callback_signature(db, callee)?;
    let parameters = signature.parameters();
    if parameters.is_gradual() {
        return None;
    }
    let bound_implicitly = usize::from(declares_receiver(signature));
    let declared = parameters.iter().skip(bound_implicitly);
    if declared
        .clone()
        .any(|parameter| parameter.is_variadic() || parameter.is_keyword_variadic())
    {
        return Some(UnbindableParameters::Variadic);
    }
    let count = declared.count();
    (count > 1).then_some(UnbindableParameters::TooMany(count))
}

/// the declared return type of the callback the callee's last parameter is — the
/// callable a trailing lambda block fills. A block always returns `None`, so this
/// must accept `None`. `None` (the option) when the last parameter is not a
/// single-signature callable (nothing to check against).
pub(crate) fn trailing_lambda_callback_return_type<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<Type<'db>> {
    Some(callback_signature(db, callee)?.return_ty)
}
