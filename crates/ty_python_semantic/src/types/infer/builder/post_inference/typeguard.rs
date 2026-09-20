use ruff_python_ast as ast;
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::definition::Definition;
use ty_python_core::place::PlaceExpr;
use ty_python_core::place_table;

use crate::place::{ConsideredDefinitions, symbol};
use crate::semantic_index;
use crate::types::function::{FunctionDecorators, OverloadLiteral};
use crate::types::inferred_narrowing::{
    NormalExit, holds_the_argument, narrowed_at_exit, normal_exits,
};
use crate::types::narrowing_guards::guard_place_display;
use crate::types::signatures::{NarrowingGuard, NarrowingGuardKind, Signature};
use crate::types::{
    IntersectionBuilder, Type, binding_type,
    context::InferContext,
    diagnostic::{
        INVALID_TYPE_GUARD_DEFINITION, UNESTABLISHED_ASSERTION_GUARD, UNRESOLVED_NARROWING_GUARD,
    },
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
    let env = context.program_environment();

    let overload = function.literal(db).last_definition;
    let signature = overload.signature(db);
    let return_ty = signature.return_ty;

    // basedpython: an assertion inherited from an overridden method is checked wherever the
    // override is written, which is not always a return annotation
    check_inherited_assertions(context, overload, &signature, node);

    // Every check here reports on the return annotation.
    let Some(returns_expr) = node.returns.as_deref() else {
        return;
    };

    let guards = signature.all_narrowing_guards(db);
    for guard in guards.iter() {
        // a guard that names nothing narrows nothing, and asking whether the body established
        // what it claims about a place that does not exist would report the typo twice
        if !check_guard_place_exists(context, &signature, guard, returns_expr) {
            continue;
        }
        if node.is_asserts_return && guard.is_written_assertion() {
            check_assertion_established(
                context,
                overload,
                &signature,
                guard,
                node,
                returns_expr.range(),
                None,
            );
        }
    }

    // Check if this is a `TypeIs` or `TypeGuard` return type, or basedpython's assertion
    // guard. `asserts x is not T` only removes `T`, so it constrains nothing to check, and a
    // guard on a member is checked against the attribute rather than the parameter.
    let (type_guard_form_name, narrowed_type) = match return_ty {
        Type::TypeIs(type_is) => ("TypeIs", Some(type_is.return_type(db))),
        Type::TypeGuard(_) => ("TypeGuard", None),
        _ => match guards.first() {
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

    let narrowed_param = match guards.first() {
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
        // basedpython: an unannotated parameter declares nothing, so its hole is the gradual
        // type it replaced and every narrowing fits it
        let param_ty = narrowed_param.annotated_type();
        let param_ty =
            crate::types::inferred_signature::gradual_hole(db, env, param_ty).unwrap_or(param_ty);
        if !narrowed_ty.is_assignable_to(db, env, param_ty)
            && let Some(builder) = context.report_lint(&INVALID_TYPE_GUARD_DEFINITION, returns_expr)
        {
            builder.into_diagnostic(format_args!(
                "Narrowed type `{narrowed}` is not assignable \
                    to the declared parameter type `{param}`",
                narrowed = narrowed_ty.display(db, env),
                param = param_ty.display(db, env)
            ));
        }
    }
}

/// basedpython: check this method's body against the assertions the method it overrides makes.
///
/// A call through the base narrows on the strength of what the base asserts, and what it is called
/// on may be an instance of this class, so an override that does not establish the same thing
/// makes that narrowing wrong. The override carries the assertion for its own callers too, which
/// is what [`OverloadLiteral::inherited_assertion_guards`] puts on its signature.
fn check_inherited_assertions<'db>(
    context: &InferContext<'db, '_>,
    overload: OverloadLiteral<'db>,
    signature: &Signature<'db>,
    node: &ast::StmtFunctionDef,
) {
    let db = context.db();
    let env = context.program_environment();
    let inherited = overload.inherited_assertion_guards(db, env, signature.parameters());
    if inherited.is_empty() {
        return;
    }
    let Some(base) = overload.overridden_method_display(db, env) else {
        return;
    };

    let guards = node
        .is_asserts_return
        .then(|| signature.all_narrowing_guards(db));
    for guard in &inherited {
        // an assertion this method writes for itself is checked against its own annotation, and
        // reporting the same place twice would say the same thing twice
        if guards.as_ref().is_some_and(|guards| {
            guards.iter().any(|own| {
                own.is_written_assertion() && own.name == guard.name && own.members == guard.members
            })
        }) {
            continue;
        }
        check_assertion_established(
            context,
            overload,
            signature,
            guard,
            node,
            node.name.range(),
            Some(&base),
        );
    }
}

/// basedpython: report a guard whose root name is neither a parameter nor a place where the
/// guard is written — it would narrow nothing at every call site.
///
/// Returns whether the guard names something.
fn check_guard_place_exists<'db>(
    context: &InferContext<'db, '_>,
    signature: &Signature<'db>,
    guard: &NarrowingGuard<'db>,
    returns_expr: &ast::Expr,
) -> bool {
    if signature
        .parameters()
        .iter()
        .any(|parameter| parameter.name() == Some(&guard.name))
    {
        return true;
    }

    // the annotation itself references the name, so merely appearing in a place table says
    // nothing — the name has to be bound or declared somewhere the guard can see
    let db = context.db();
    let file = context.file();
    let index = semantic_index(db, db.program_file(file));
    let root = PlaceExpr::from_symbol_with_members(&guard.name, &[]);
    let resolves = root.is_some_and(|root| {
        index
            .ancestor_scopes(context.scope().file_scope_id(db))
            .any(|(scope_id, _)| {
                let places = place_table(db, scope_id.to_scope_id(db, db.program_file(file)));
                places.place_id(&root).is_some_and(|place_id| {
                    let place = places.place(place_id);
                    place.is_bound() || place.is_declared()
                })
            })
    });
    if resolves {
        return true;
    }

    if let Some(builder) = context.report_lint(&UNRESOLVED_NARROWING_GUARD, returns_expr) {
        builder.into_diagnostic(format_args!(
            "`{place}` is neither a parameter nor a place here, so this guard narrows nothing",
            place = guard_place_display(guard)
        ));
    }
    false
}

/// basedpython: report each way out of a function body that does not establish what an assertion
/// guard claims about its place.
///
/// A call narrows its argument to the asserted type as soon as it returns, so the claim has to
/// hold at every `return` that can be reached and at the end of the body, if that can be reached.
fn check_assertion_established<'db>(
    context: &InferContext<'db, '_>,
    overload: OverloadLiteral<'db>,
    signature: &Signature<'db>,
    guard: &NarrowingGuard<'db>,
    node: &ast::StmtFunctionDef,
    anchor: TextRange,
    inherited_from: Option<&str>,
) {
    let db = context.db();
    let env = context.program_environment();
    let Some(asserted) = guard.kind.asserted_type(db, env) else {
        return;
    };

    // a `def` that declares a function without implementing one — in a stub file, as an
    // `@overload` or an `@abstractmethod`, or with a `...` body — makes its claim for whatever
    // implements it, and has no body of its own to establish anything
    if context.in_stub()
        || is_declaration_body(node)
        || overload.has_known_decorator(db, FunctionDecorators::OVERLOAD)
        || overload.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD)
    {
        return;
    }

    let program_file = overload.program_file(db);
    let index = semantic_index(db, program_file);
    let body_scope = overload.body_scope(db);
    let file_scope_id = body_scope.file_scope_id(db);
    let use_def = index.use_def_map(file_scope_id);
    let place_table = index.place_table(file_scope_id);

    let is_parameter = signature
        .parameters()
        .iter()
        .any(|parameter| parameter.name() == Some(&guard.name));

    // a guard's root is the caller's — the argument it passed, or the place it can see — and a
    // body that puts something else there, whether by assigning to the parameter or by binding a
    // local of the place's name, can only establish something about that
    if !holds_the_argument(db, index, file_scope_id, &guard.name, &[]) {
        if let Some(builder) = context.report_lint(&UNESTABLISHED_ASSERTION_GUARD, anchor) {
            let subject = if is_parameter {
                "the argument this guard narrows"
            } else {
                "the place this guard narrows"
            };
            builder.into_diagnostic(format_args!(
                "the body puts another value in `{name}`, so what it establishes is not about \
                 {subject}",
                name = guard.name
            ));
        }
        return;
    }

    // what a place held when the body began, which the body did not put there: a parameter is
    // always bound by then, and a place of the enclosing scopes is whatever it is there
    let outer_root_type = || {
        if is_parameter {
            return Type::unknown();
        }
        index
            .ancestor_scopes(file_scope_id)
            .skip(1)
            // the names of a class body are not visible to the functions nested in it, so a guard
            // written there names whatever place an enclosing scope has
            .filter(|(_, scope)| !scope.kind().is_class())
            .find_map(|(scope_id, _)| {
                let scope = scope_id.to_scope_id(db, program_file);
                symbol(db, scope, &guard.name, ConsideredDefinitions::AllReachable)
                    .place
                    .ignore_possibly_undefined()
            })
            .unwrap_or_else(Type::unknown)
    };

    let start_from = |definition: Option<Definition<'db>>, outer: &dyn Fn() -> Type<'db>| {
        definition.map_or_else(outer, |definition| binding_type(db, definition))
    };

    let claim = match guard.kind {
        NarrowingGuardKind::Asserts { is_positive: true } => "truthy".to_string(),
        NarrowingGuardKind::Asserts { is_positive: false } => "falsy".to_string(),
        NarrowingGuardKind::AssertsType {
            is_positive: true,
            ty,
        } => format!("`{}`", ty.display(db, env)),
        NarrowingGuardKind::AssertsType {
            is_positive: false,
            ty,
        } => format!("not `{}`", ty.display(db, env)),
        // an assertion recovered from a body says what that body established, which is a type
        NarrowingGuardKind::InferredAssertion { ty } => format!("`{}`", ty.display(db, env)),
        // a predicate narrows where the call is tested rather than once it returns, so there is
        // no assertion for a body to establish
        NarrowingGuardKind::Predicate | NarrowingGuardKind::InferredPredicate { .. } => return,
    };

    // a member is read off whatever the place above it holds, so rebinding that place leaves what
    // the body established about the member describing a value the caller never reads back
    let prefixes_hold = (0..guard.members.len()).all(|depth| {
        holds_the_argument(
            db,
            index,
            file_scope_id,
            &guard.name,
            &guard.members[..depth],
        )
    });

    for exit in normal_exits(db, use_def) {
        let mut established = match PlaceExpr::from_symbol_with_members(&guard.name, &[])
            .and_then(|root| place_table.place_id(&root))
        {
            Some(root) => narrowed_at_exit(db, env, use_def, exit, root, &|definition| {
                start_from(definition, &outer_root_type)
            }),
            None => outer_root_type(),
        };
        // each member is read off the value above it, and then narrowed by what the body
        // established about the member itself
        for depth in 1..=guard.members.len() {
            let parent = established;
            let read_member = || {
                parent
                    .member(db, env, &guard.members[depth - 1])
                    .place
                    .ignore_possibly_undefined()
                    .unwrap_or_else(Type::unknown)
            };
            established =
                match PlaceExpr::from_symbol_with_members(&guard.name, &guard.members[..depth])
                    .and_then(|place| place_table.place_id(&place))
                    .filter(|_| prefixes_hold)
                {
                    Some(place) => narrowed_at_exit(db, env, use_def, exit, place, &|definition| {
                        start_from(definition, &read_member)
                    }),
                    None => read_member(),
                };
        }

        // what the place is has to leave no room for the assertion to fail, which asks more than
        // assignability: `Any` is assignable to anything, and says nothing
        if IntersectionBuilder::new(db, env)
            .add_positive(established)
            .add_positive(asserted.negate(db, env))
            .build()
            .is_never()
        {
            continue;
        }
        let (range, location) = match exit {
            NormalExit::Return { range, .. } => (range, "where this returns"),
            NormalExit::EndOfBody => (anchor, "where the body ends"),
        };
        let Some(builder) = context.report_lint(&UNESTABLISHED_ASSERTION_GUARD, range) else {
            continue;
        };
        let asserter = match inherited_from {
            Some(base) => format!("`{base}`, which this overrides, asserts"),
            None => "this function asserts".to_string(),
        };
        builder.into_diagnostic(format_args!(
            "`{place}` is `{established}` {location}, but {asserter} it is {claim}",
            place = guard_place_display(guard),
            established = established.display(db, env),
        ));
    }
}

/// basedpython: whether `node` declares a function without implementing one.
///
/// A `...` body is a declaration: what it claims is for whatever implements it to establish. A
/// `pass` body is an implementation, and an empty one establishes nothing.
fn is_declaration_body(node: &ast::StmtFunctionDef) -> bool {
    ast::helpers::body_without_leading_docstring(&node.body)
        .iter()
        .all(|stmt| match stmt {
            ast::Stmt::Expr(ast::StmtExpr { value, .. }) => value.is_ellipsis_literal_expr(),
            _ => false,
        })
}
