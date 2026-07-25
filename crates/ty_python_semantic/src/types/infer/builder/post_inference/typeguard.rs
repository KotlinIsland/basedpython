use ruff_python_ast as ast;
use ty_python_core::place::PlaceExpr;
use ty_python_core::place_table;

use crate::semantic_index;
use crate::types::narrowing_guards::guard_place_display;
use crate::types::signatures::{NarrowingGuard, NarrowingGuardKind, Signature};
use crate::types::{
    Type,
    context::InferContext,
    diagnostic::{INVALID_TYPE_GUARD_DEFINITION, UNRESOLVED_NARROWING_GUARD},
};

/// Check that all type guard function definitions have at least one positional parameter
/// (in addition to `self`/`cls` for methods), and for `TypeIs` and basedpython's
/// `-> asserts x is T`, that the narrowed type is assignable to the declared type of that
/// parameter. basedpython guards additionally have to name a place that exists.
pub(crate) fn check_type_guard_definition<'db>(
    context: &InferContext<'db, '_>,
    ty: Type<'db>,
    node: &ast::StmtFunctionDef,
) {
    let Type::FunctionLiteral(function) = ty else {
        return;
    };

    let db = context.db();

    let overload = function.literal(db).last_definition;
    let signature = overload.signature(db);
    let return_ty = signature.return_ty;

    // Every check here reports on the return annotation.
    let Some(returns_expr) = node.returns.as_deref() else {
        return;
    };

    for guard in &signature.narrowing_guards {
        check_guard_place_exists(context, &signature, guard, returns_expr);
    }

    // Check if this is a `TypeIs` or `TypeGuard` return type, or basedpython's assertion
    // guard. `asserts x is not T` only removes `T`, so it constrains nothing to check, and a
    // guard on a member is checked against the attribute rather than the parameter.
    let (type_guard_form_name, narrowed_type) = match return_ty {
        Type::TypeIs(type_is) => ("TypeIs", Some(type_is.return_type(db))),
        Type::TypeGuard(_) => ("TypeGuard", None),
        _ => match signature.narrowing_guards.first() {
            Some(NarrowingGuard {
                members,
                kind:
                    NarrowingGuardKind::AssertsType {
                        is_positive: true,
                        ty,
                    },
                ..
            }) if members.is_empty() => ("asserts", Some(*ty)),
            _ => return,
        },
    };

    // Check if this is a non-static method (first parameter is implicit `self`/`cls`).
    let has_implicit_receiver = overload.has_implicit_receiver(db);

    let narrowed_param = match signature.narrowing_guards.first() {
        // basedpython: `-> x is T` names the parameter it narrows. a name that is not a
        // parameter is resolved in each calling scope instead, and a member's declared type
        // is the attribute's, so neither has a parameter type to check against here
        Some(guard) => {
            let parameter = signature
                .parameters()
                .iter()
                .find(|parameter| parameter.name() == Some(&guard.name));
            match parameter {
                Some(parameter) if guard.members.is_empty() => parameter,
                _ => return,
            }
        }
        None => {
            // Find the first positional parameter to narrow (skip implicit `self`/`cls`).
            let positional_params: Vec<_> = signature.parameters().positional().collect();
            let Some(first_narrowed_param) =
                positional_params.get(usize::from(has_implicit_receiver))
            else {
                if let Some(builder) =
                    context.report_lint(&INVALID_TYPE_GUARD_DEFINITION, returns_expr)
                {
                    builder.into_diagnostic(format_args!(
                        "`{type_guard_form_name}` function must have a parameter to narrow"
                    ));
                }
                return;
            };
            *first_narrowed_param
        }
    };

    // For `TypeIs`, check that the narrowed type is assignable to the parameter type.
    if let Some(narrowed_ty) = narrowed_type {
        let param_ty = narrowed_param.annotated_type();
        if !narrowed_ty.is_assignable_to(db, param_ty)
            && let Some(builder) = context.report_lint(&INVALID_TYPE_GUARD_DEFINITION, returns_expr)
        {
            builder.into_diagnostic(format_args!(
                "Narrowed type `{narrowed}` is not assignable \
                    to the declared parameter type `{param}`",
                narrowed = narrowed_ty.display(db),
                param = param_ty.display(db)
            ));
        }
    }
}

/// basedpython: report a guard whose root name is neither a parameter nor a place where the
/// guard is written — it would narrow nothing at every call site.
fn check_guard_place_exists<'db>(
    context: &InferContext<'db, '_>,
    signature: &Signature<'db>,
    guard: &NarrowingGuard<'db>,
    returns_expr: &ast::Expr,
) {
    if signature
        .parameters()
        .iter()
        .any(|parameter| parameter.name() == Some(&guard.name))
    {
        return;
    }

    // the annotation itself references the name, so merely appearing in a place table says
    // nothing — the name has to be bound or declared somewhere the guard can see
    let db = context.db();
    let file = context.file();
    let index = semantic_index(db, file);
    let root = PlaceExpr::from_symbol_with_members(&guard.name, &[]);
    let resolves = root.is_some_and(|root| {
        index
            .ancestor_scopes(context.scope().file_scope_id(db))
            .any(|(scope_id, _)| {
                let places = place_table(db, scope_id.to_scope_id(db, file));
                places.place_id(&root).is_some_and(|place_id| {
                    let place = places.place(place_id);
                    place.is_bound() || place.is_declared()
                })
            })
    });
    if resolves {
        return;
    }

    if let Some(builder) = context.report_lint(&UNRESOLVED_NARROWING_GUARD, returns_expr) {
        builder.into_diagnostic(format_args!(
            "`{place}` is neither a parameter nor a place here, so this guard narrows nothing",
            place = guard_place_display(guard)
        ));
    }
}
