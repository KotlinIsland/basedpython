//! basedpython: type-side queries for trailing lambda blocks
//!
//! a statement-level `<call>:` block passes its suite as the call's last
//! argument. the lowering (and the checker's synthetic call binding) pass
//! that argument by keyword — the callee's last declared parameter — so that
//! `f:` binds the lambda to the last parameter even when earlier parameters
//! are defaulted. the implicit `it` parameter takes its type from that
//! parameter's declared callable type

use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::helpers::parameter_modifiers;
use ruff_python_ast::name::Name;

use crate::Db;
use crate::types::Type;
use crate::types::signatures::Parameter;
use crate::types::soundness::single_signature;

/// basedpython: whether the callee's callback — its last declared parameter, the
/// one a trailing block binds — is marked `once`.
///
/// A `once` block runs exactly once (`with`-like); a non-`once` one runs an
/// unknown number of times, which restricts what it may do. Resolving the marker
/// means reaching the callee's function definition to read its source span, so
/// this is conservatively `false` for anything not a single function literal (a
/// callable-typed value carries no such marker).
pub(crate) fn callee_callback_is_once<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    let Type::FunctionLiteral(function) = callee else {
        return false;
    };
    let overload = function.literal(db).last_definition;
    let file = overload.file(db);
    let module = parsed_module(db, file).load(db);
    let Some(last) = overload.node(db, file, &module).parameters.args.last() else {
        return false;
    };
    parameter_modifiers(&source_text(db, file), &last.parameter).once
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

/// the type of the implicit `it` parameter: the first positional parameter
/// type of the callable the callee's last parameter is declared as. `None`
/// when that shape doesn't hold — `it` is then left untyped
pub(crate) fn trailing_lambda_it_type<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<Type<'db>> {
    let parameter = last_parameter(db, callee)?;
    let Type::Callable(callable) = parameter.annotated_type() else {
        return None;
    };
    let [signature] = callable.signatures(db).overloads.as_slice() else {
        return None;
    };
    Some(signature.parameters().get_positional(0)?.annotated_type())
}

/// the declared return type of the callback the callee's last parameter is — the
/// callable a trailing lambda block fills. A block always returns `None`, so this
/// must accept `None`. `None` (the option) when the last parameter is not a
/// single-signature callable (nothing to check against).
pub(crate) fn trailing_lambda_callback_return_type<'db>(
    db: &'db dyn Db,
    callee: Type<'db>,
) -> Option<Type<'db>> {
    let parameter = last_parameter(db, callee)?;
    let Type::Callable(callable) = parameter.annotated_type() else {
        return None;
    };
    let [signature] = callable.signatures(db).overloads.as_slice() else {
        return None;
    };
    Some(signature.return_ty)
}
