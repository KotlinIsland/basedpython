//! basedpython: type-side queries for trailing lambda blocks
//!
//! a statement-level `<call>:` block passes its suite as the call's last
//! argument. the lowering (and the checker's synthetic call binding) pass
//! that argument by keyword — the callee's last declared parameter — so that
//! `f:` binds the lambda to the last parameter even when earlier parameters
//! are defaulted. the implicit `it` parameter takes its type from that
//! parameter's declared callable type

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, ParameterBorrow};
use ty_python_core::scope::{ScopeId, ScopeKind};
use ty_python_core::{SemanticIndex, semantic_index};

use crate::Db;
use crate::types::call::{Argument, CallArguments};
use crate::types::constraints::ConstraintSetBuilder;
use crate::types::generics::Specialization;
use crate::types::signatures::{Parameter, Parameters, Signature};
use crate::types::soundness::single_signature;
use crate::types::{ProgramEnvironment, Type, TypeContext, infer_expression_types};

/// a trailing lambda block's callee, together with what the block's call
/// solves for it.
///
/// A generic free function's callback parameter mentions the function's own
/// type variables — `def each[T](items: tuple[T, ...], block: (T) -> None)` —
/// so what the block's `it` (and receiver) are is only known once the written
/// arguments have solved them. A bound method carries its receiver's
/// specialization in its signature already; a free function's is solved here,
/// from the block's own call, so `it` in `each(("a", "b")):` is `str` rather
/// than `T@each`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, salsa::SalsaValue, get_size2::GetSize)]
pub(crate) struct BlockCallee<'db> {
    /// the type of the expression the block is attached to
    pub(crate) ty: Type<'db>,
    /// the specialization the call's written arguments solve for a generic
    /// callee. `None` for a non-generic one, and when the call cannot be bound
    /// from what is written (an unpacked argument, an uninspectable callee)
    pub(crate) specialization: Option<Specialization<'db>>,
}

impl<'db> BlockCallee<'db> {
    /// a callee with nothing solved — all a callee reached without its call is
    pub(crate) fn unspecialized(ty: Type<'db>) -> Self {
        Self {
            ty,
            specialization: None,
        }
    }
}

/// the callee of the trailing lambda block `function`, with the specialization
/// its call solves. Reads the callee and the written arguments from their
/// standalone inferences (registered by the semantic index builder), so it can
/// be asked from inside the block's own scope without a cycle through the
/// enclosing definition's inference.
///
/// One block is asked for its callee many times over — for `it`, for the
/// receiver, for the borrow, for the callback's return type, and again by each
/// of the composition checks — and solving a generic callee re-binds the whole
/// call every time. Memoising that on the block's scope looks like the obvious
/// win, and is not available: this is deliberately re-entrant. Inferring the
/// block's own scope can need its callback's return type, which asks for the
/// callee again, and a salsa query would turn that recomputation into a
/// dependency-graph cycle rather than a repeat. Recomputing is what keeps it
/// safe. Anything cached here has to be keyed on something the block's own
/// inference cannot reach
pub(crate) fn block_callee<'db>(
    db: &'db dyn Db,
    index: &SemanticIndex<'db>,
    function: &ast::StmtFunctionDef,
) -> Option<BlockCallee<'db>> {
    let callee = function.trailing_lambda_callee()?;
    let expression = index.try_expression(callee)?;
    let ty = infer_expression_types(db, expression, TypeContext::default())
        .try_expression_type(callee)?;
    let env = ProgramEnvironment::from_program(expression.program(db));
    let specialization = block_call_specialization(db, &env, index, function, ty);
    Some(BlockCallee { ty, specialization })
}

/// what the written arguments of `function`'s call solve for a generic `callee`:
/// the call is bound exactly as the checker binds it in the enclosing scope —
/// the written arguments, then the block as a gradual callable in the callback's
/// position — and the single binding's specialization is read back. Binding
/// errors are not reported here (the enclosing scope does that); a partial
/// solution is still a solution for the parameters it covers
fn block_call_specialization<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    index: &SemanticIndex<'db>,
    function: &ast::StmtFunctionDef,
    callee: Type<'db>,
) -> Option<Specialization<'db>> {
    // only a callee with type variables of its own has anything to solve
    single_signature(db, callee)?.generic_context?;
    let call = function.trailing_lambda_call()?;
    let mut items: Vec<(Argument<'_>, Option<Type<'db>>)> = Vec::new();
    for argument in call.arguments.iter_source_order() {
        let (argument, value) = match argument {
            ast::ArgOrKeyword::Arg(value) => {
                if value.is_starred_expr() {
                    return None;
                }
                (Argument::Positional, value)
            }
            ast::ArgOrKeyword::Keyword(keyword) => {
                (Argument::Keyword(keyword.arg.as_ref()?), &keyword.value)
            }
        };
        let expression = index.try_expression(value)?;
        let ty = infer_expression_types(db, expression, TypeContext::default())
            .try_expression_type(value)?;
        items.push((argument, Some(ty)));
    }
    let keyword = trailing_lambda_keyword(db, callee);
    let block_ty = Type::single_callable(
        db,
        Signature::new(Parameters::gradual_form(), Type::unknown()),
    );
    items.push((
        match &keyword {
            Some(name) => Argument::Keyword(name),
            None => Argument::Positional,
        },
        Some(block_ty),
    ));
    let arguments: CallArguments<'_, 'db> = items.into_iter().collect();
    let constraints = ConstraintSetBuilder::new();
    let bindings = match callee
        .bindings(db, env)
        .match_parameters(db, env, &arguments)
        .check_types(
            db,
            env,
            &constraints,
            &arguments,
            TypeContext::default(),
            &[],
        ) {
        Ok(bindings) => bindings,
        Err(error) => *error.into_bindings(),
    };
    let [binding] = bindings.single_element()?.overloads() else {
        return None;
    };
    binding.merged_specialization(db, env)
}

/// the callee of the trailing lambda block whose body `scope` is in
pub(crate) fn enclosing_block_callee<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> Option<BlockCallee<'db>> {
    Some(enclosing_block(db, scope)?.1)
}

/// the trailing lambda block whose body `scope` is in: the block's own scope,
/// and its callee, specialized by the block's call. Walks out through
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
) -> Option<(ScopeId<'db>, BlockCallee<'db>)> {
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
        let callee = block_callee(db, index, function)?;
        return Some((ancestor_id.to_scope_id(db, program_file), callee));
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

/// basedpython: whether a call to `callee` can carry a trailing block at all:
/// its last declared parameter is a callable, which is what a block binds.
/// What the callee then makes of the block — `once`, `local`, or retained —
/// is a separate question; the framework's runtime runs a composable called
/// with a block *inline* whichever it is.
pub(crate) fn callee_accepts_block<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    last_parameter(db, callee)
        .is_some_and(|parameter| matches!(parameter.annotated_type(), Type::Callable(_)))
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

/// the callable a trailing lambda block fills: the declared type of the callee's
/// last parameter, with what the block's call solved applied to it — so a
/// generic callee's `(T) -> None` is `(str) -> None` for the call it is used in
fn callback_type<'db>(db: &'db dyn Db, callee: BlockCallee<'db>) -> Option<Type<'db>> {
    let declared = last_parameter(db, callee.ty)?.annotated_type();
    Some(match callee.specialization {
        Some(specialization) => declared.apply_specialization(db, specialization),
        None => declared,
    })
}

/// the single signature of the callback a trailing lambda block fills: the
/// callable the callee's last declared parameter is annotated as. `None` for
/// anything else — an unannotated, non-callable or overloaded parameter
fn callback_signature<'db>(
    db: &'db dyn Db,
    callee: BlockCallee<'db>,
) -> Option<&'db Signature<'db>> {
    let Type::Callable(callable) = callback_type(db, callee)? else {
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
fn it_parameter<'db>(db: &'db dyn Db, callee: BlockCallee<'db>) -> Option<Parameter<'db>> {
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
pub(crate) fn trailing_lambda_passes_it<'db>(
    db: &'db dyn Db,
    callee: BlockCallee<'db>,
) -> Option<bool> {
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
    callee: BlockCallee<'db>,
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
    callee: BlockCallee<'db>,
) -> ParameterBorrow {
    it_parameter(db, callee).map_or(ParameterBorrow::None, |parameter| parameter.borrow())
}

/// the type the block's callback declares as its *receiver* — the block body then
/// sees that type's members unqualified, and spells the receiver itself `self`.
/// `None` when the callback is an ordinary callable, which has no implicit
/// member scope
pub(crate) fn trailing_lambda_receiver_type<'db>(
    db: &'db dyn Db,
    callee: BlockCallee<'db>,
) -> Option<Type<'db>> {
    crate::types::receivers::receiver_type(db, callback_type(db, callee)?)
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
    callee: BlockCallee<'db>,
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
    callee: BlockCallee<'db>,
) -> Option<Type<'db>> {
    Some(callback_signature(db, callee)?.return_ty)
}
