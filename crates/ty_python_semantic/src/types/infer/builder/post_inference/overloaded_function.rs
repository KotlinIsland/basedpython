use ruff_db::{
    diagnostic::{Annotation, Span},
    parsed::parsed_module,
};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;

use crate::{
    Db,
    place::{DefinedPlace, Definedness, Place, place_from_bindings},
    types::{
        CallableType, KnownClass, Type,
        context::InferContext,
        diagnostic::INVALID_OVERLOAD,
        function::{FunctionDecorators, FunctionType, KnownFunction, OverloadLiteral},
        infer::original_class_type,
        signatures::{ParameterConsistency, ReturnTypeConsistency},
    },
};
use ty_python_core::{
    SemanticIndex,
    definition::{Definition, DefinitionState},
    place::ScopedPlaceId,
    scope::{FileScopeId, NodeWithScopeKind},
};

/// Check the overloaded function this definition is part of, if it is the one this place holds
/// in this scope.
///
/// Which set a place holds is normally read off the end of the scope: the value that survives
/// everything written above it is the one worth reporting against, and a set shadowed by another
/// function of the same name is therefore left alone.
///
/// A scope whose end is unreachable — a function body whose every path returns — has no
/// end-of-scope value at all, and upstream checks nothing there. basedpython does not follow
/// that: whether a nested `def` is malformed has nothing to do with whether the function around
/// it happens to end in `return`, and "attempting to call `g` will raise `TypeError`" is as true
/// in one as in the other. So when the scope has no end to read, the set is taken to be the one
/// this place holds if it accounts for every declaration of the name the scope reaches — which is
/// the same question shadowing asked, answered without needing the scope to finish, and the same
/// rule [`check_called_overloaded_function`] uses.
pub(crate) fn check_overloaded_function<'db>(
    context: &InferContext<'db, '_>,
    ty: Type<'db>,
    definition: Definition<'db>,
    scope: &NodeWithScopeKind,
    index: &SemanticIndex<'db>,
    seen_overloaded_places: &mut FxHashSet<ScopedPlaceId>,
    seen_public_functions: &mut FxHashSet<FunctionType<'db>>,
) {
    let Type::FunctionLiteral(function) = ty else {
        return;
    };

    let db = context.db();
    let env = context.program_environment();

    if function.file(db) != context.file() {
        // If the function is not in this file, we don't need to check it.
        // https://github.com/astral-sh/ruff/pull/17609#issuecomment-2839445740
        return;
    }

    if !function.has_known_decorator(db, FunctionDecorators::OVERLOAD) {
        return;
    }

    let place = definition.place(db);

    let scope_id = context.scope().file_scope_id(db);
    let use_def = index.use_def_map(scope_id);
    let symbol = place.as_symbol().unwrap();

    let function =
        match place_from_bindings(db, env, use_def.end_of_scope_symbol_bindings(symbol)).place {
            Place::Defined(DefinedPlace {
                ty: Type::FunctionLiteral(end_of_scope),
                definedness: Definedness::AlwaysDefined,
                ..
            }) => {
                if !end_of_scope.contains_definition(db, definition) {
                    // The public end-of-scope binding for this place can be a different overloaded
                    // function value assigned to the same name. In that case, the current local
                    // overload definition is shadowed, and checking the public function here would
                    // report against the wrong function.
                    return;
                }
                end_of_scope
            }
            // Nothing survives to the end of the scope: a scope with no end to reach binds nothing
            // there, and reads back `Never`. The declarations still ran, and the set is still the one
            // the name holds wherever the scope was left — so it is checked, as long as no other
            // declaration of the name competes with it.
            Place::Undefined
            | Place::Defined(DefinedPlace {
                ty: Type::Never, ..
            }) => {
                if !accounts_for_every_declaration(db, index, scope_id, place, function) {
                    return;
                }
                function
            }
            Place::Defined(_) => return,
        };

    check_overload_set(
        context,
        function,
        place,
        scope,
        index,
        seen_overloaded_places,
        seen_public_functions,
    );
}

/// Check an overloaded function that this scope both defines and calls.
///
/// [`check_overloaded_function`] reads what the name holds once the scope has run, so it sees
/// nothing at all when the end of the scope is unreachable — and a nested `def` inside a
/// function whose every path returns is the ordinary shape of that, not an exotic one. The
/// `def`s written above a call still run when the call does, so an overload set that is
/// malformed there raises `TypeError` just the same.
///
/// The call may have resolved to only part of an overload set: a `g(1)` written between two
/// `@overload def g`s sees the first of them and not the second. So the called value is checked
/// only when its own overload set already accounts for every declaration written at that place
/// in this scope, which is what makes it the whole set rather than a prefix of one.
pub(crate) fn check_called_overloaded_function<'db>(
    context: &InferContext<'db, '_>,
    function: FunctionType<'db>,
    scope: &NodeWithScopeKind,
    index: &SemanticIndex<'db>,
    seen_overloaded_places: &mut FxHashSet<ScopedPlaceId>,
    seen_public_functions: &mut FxHashSet<FunctionType<'db>>,
) {
    let db = context.db();

    if function.file(db) != context.file() {
        return;
    }

    if !function.has_known_decorator(db, FunctionDecorators::OVERLOAD) {
        return;
    }

    let place = function.definition(db).place(db);
    let scope_id = context.scope().file_scope_id(db);

    if !accounts_for_every_declaration(db, index, scope_id, place, function) {
        return;
    }

    check_overload_set(
        context,
        function,
        place,
        scope,
        index,
        seen_overloaded_places,
        seen_public_functions,
    );
}

/// Whether `function`'s own overload set already covers every declaration of `symbol` this scope
/// reaches.
///
/// This is what makes a value the whole overload set rather than a part of one. A declaration the
/// set does not contain is either a competing definition of the name — the shadowing case — or a
/// later `@overload` the value was built too early to have seen, which is what a call written
/// part-way down a set resolves to.
fn accounts_for_every_declaration<'db>(
    db: &'db dyn crate::Db,
    index: &SemanticIndex<'db>,
    scope: FileScopeId,
    place: ScopedPlaceId,
    function: FunctionType<'db>,
) -> bool {
    let Some(symbol) = place.as_symbol() else {
        return false;
    };
    index
        .use_def_map(scope)
        .reachable_symbol_declarations(symbol)
        .all(|declaration| match declaration.declaration {
            DefinitionState::Defined(declaration) => function.contains_definition(db, declaration),
            DefinitionState::Undefined | DefinitionState::Deleted => true,
        })
}

/// Report everything that is wrong with one complete overload set.
fn check_overload_set<'db>(
    context: &InferContext<'db, '_>,
    function: FunctionType<'db>,
    place: ScopedPlaceId,
    scope: &NodeWithScopeKind,
    index: &SemanticIndex<'db>,
    seen_overloaded_places: &mut FxHashSet<ScopedPlaceId>,
    seen_public_functions: &mut FxHashSet<FunctionType<'db>>,
) {
    let db = context.db();
    let env = context.program_environment();

    // An overloaded function uses the same place for each of the overloads and the
    // implementation, so a place is only worth checking once.
    if !seen_overloaded_places.insert(place) {
        // We have already checked this overloaded function in this scope, so we can skip it.
        return;
    }

    if !seen_public_functions.insert(function) {
        // We have already checked this overloaded function as a public function, so we can skip it.
        return;
    }

    let (overloads, implementation) = function.overloads_and_implementation(db);
    if overloads.is_empty() {
        return;
    }

    let binding_decorator_inconsistencies =
        binding_decorator_inconsistencies(db, overloads, implementation.as_ref());

    if let Some(implementation) = implementation
        && binding_decorator_inconsistencies.is_empty()
        && context.is_lint_enabled(&INVALID_OVERLOAD)
    {
        let implementation_callables = function.implementation_callables(db);
        check_non_generic_overload_implementation_consistency(
            context,
            overloads,
            implementation,
            &implementation_callables,
        );
    }

    // Check that the overloaded function has at least two overloads
    if let [single_overload] = overloads {
        let function_node = single_overload.node(db, context.file(), context.module());
        if let Some(builder) = context.report_lint(&INVALID_OVERLOAD, &function_node.name) {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "Overloaded function `{}` requires at least two overloads",
                function_node.name
            ));
            diagnostic.set_primary_annotation_message("Only one overload defined here");
            if let Some(decorator) =
                single_overload.find_known_decorator_span(context.db(), KnownFunction::Overload)
            {
                diagnostic.annotate(Annotation::secondary(decorator));
            }
        }
    }

    // Check that the overloaded function has an implementation. Overload definitions
    // within stub files, protocols, and on abstract methods within abstract base classes
    // are exempt from this check.
    if implementation.is_none() && !context.in_stub() {
        let mut implementation_required = true;

        if function.iter_overloads_and_implementation(db).all(|f| {
            index.is_in_type_checking_block(
                f.body_scope(db).file_scope_id(db),
                f.node(db, context.file(), context.module()).range(),
            )
        }) {
            implementation_required = false;
        } else if let NodeWithScopeKind::Class(class_node_ref) = scope
            && let Some(class) = original_class_type(
                db,
                index.expect_single_definition(class_node_ref.node(context.module())),
            )
        {
            if class.is_protocol(db)
                || ({
                    Type::ClassLiteral(class).is_subtype_of(
                        db,
                        env,
                        KnownClass::ABCMeta.to_instance(db, env),
                    )
                } && overloads.iter().all(|overload| {
                    overload.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD)
                }))
            {
                implementation_required = false;
            }
        }

        if implementation_required {
            let function_node = overloads[0].node(db, context.file(), context.module());
            if let Some(builder) = context.report_lint(&INVALID_OVERLOAD, &function_node.name) {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "Overloads for function `{}` must be followed by a \
                    non-`@overload`-decorated implementation function",
                    function_node.name
                ));
                diagnostic.info(format_args!(
                    "Attempting to call `{}` will raise `TypeError` at runtime",
                    function_node.name
                ));
                diagnostic.info("Overloaded functions without implementations are only permitted:");
                diagnostic.info(" - in stub files");
                diagnostic.info(" - in `if TYPE_CHECKING` blocks");
                diagnostic.info(" - as methods on protocol classes");
                diagnostic.info(" - or as `@abstractmethod`-decorated methods on abstract classes");
                diagnostic.info(
                    "See https://docs.python.org/3/library/typing.html#typing.overload \
                            for more details",
                );
            }
        }
    }

    for inconsistency in binding_decorator_inconsistencies {
        let function_node = function.node(db, context.file(), context.module());
        if let Some(builder) = context.report_lint(&INVALID_OVERLOAD, &function_node.name) {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "Overloaded function `{}` does not use the `@{}` decorator \
                    consistently",
                function_node.name, inconsistency.decorator_name
            ));
            for function in inconsistency.missing {
                diagnostic.annotate(
                    context
                        .secondary(function.focus_range(db, context.module()))
                        .message(format_args!("Missing here")),
                );
                if let Some(decorator) =
                    function.find_known_decorator_span(context.db(), KnownFunction::Overload)
                {
                    diagnostic.annotate(Annotation::secondary(decorator));
                }
            }
        }
    }

    for (known_function, decorator) in [
        (KnownFunction::Final, FunctionDecorators::FINAL),
        (KnownFunction::Override, FunctionDecorators::OVERRIDE),
    ] {
        if let Some(implementation) = implementation {
            for overload in overloads {
                if !overload.has_known_decorator(db, decorator) {
                    continue;
                }
                let function_node = overload.node(db, context.file(), context.module());
                let Some(builder) = context.report_lint(&INVALID_OVERLOAD, &function_node.name)
                else {
                    continue;
                };
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`@{name}` decorator should be applied only to the \
                        overload implementation",
                    name = known_function.name()
                ));
                for known_function in [known_function, KnownFunction::Overload] {
                    if let Some(decorator) =
                        overload.find_known_decorator_span(context.db(), known_function)
                    {
                        diagnostic.annotate(Annotation::secondary(decorator));
                    }
                }
                diagnostic.annotate(
                    context
                        .secondary(implementation.focus_range(db, context.module()))
                        .message(format_args!("Implementation defined here")),
                );
            }
        } else {
            let mut overloads = overloads.iter();
            let Some(first_overload) = overloads.next() else {
                continue;
            };
            for overload in overloads {
                if !overload.has_known_decorator(db, decorator) {
                    continue;
                }
                let function_node = overload.node(db, context.file(), context.module());
                let Some(builder) = context.report_lint(&INVALID_OVERLOAD, &function_node.name)
                else {
                    continue;
                };
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`@{name}` decorator should be applied only to the \
                        first overload",
                    name = known_function.name()
                ));
                if let Some(decorator) =
                    overload.find_known_decorator_span(context.db(), known_function)
                {
                    diagnostic.annotate(Annotation::secondary(decorator));
                }
                let file = function.file(db);
                let module = parsed_module(db, first_overload.python_file(db)).load(db);
                let node = first_overload.node(db, file, &module);
                let span = if node.body.len() == 1 {
                    Span::from(file).with_range(node.range())
                } else {
                    first_overload.spans(db).decorators_and_header
                };
                diagnostic.annotate(
                    Annotation::secondary(span)
                        .message(format_args!("First overload defined here")),
                );
            }
        }
    }
}

/// Check non-generic overload signatures against their implementation.
///
/// This is the first, deliberately narrow pass at overload implementation consistency. Signature
/// compatibility is checked only when the overloads and implementation are all non-generic;
/// generic signatures require careful treatment of type-variable domains. Each callable
/// alternative of the implementation must contain a signature consistent with each overload.
fn check_non_generic_overload_implementation_consistency<'db>(
    context: &InferContext<'db, '_>,
    overloads: &'db [OverloadLiteral<'db>],
    implementation: OverloadLiteral<'db>,
    implementation_callables: &[CallableType<'db>],
) {
    let db = context.db();
    let env = context.program_environment();
    if implementation_callables.is_empty()
        || implementation_callables
            .iter()
            .any(|callable| callable.signatures(db).overloads.is_empty())
    {
        let function_node = implementation.node(db, context.file(), context.module());
        if let Some(builder) = context.report_lint(&INVALID_OVERLOAD, &function_node.name) {
            builder.into_diagnostic(format_args!(
                "Overload implementation is not callable after applying decorators"
            ));
        }
        return;
    }

    let db = context.db();

    // TODO: Remove this temporary non-generic restriction once overload implementation consistency
    // handles type-variable domains.
    //
    // basedpython: an unannotated parameter's hole is erased first — it makes the signature
    // generic without there being any type-variable domain to reason about, and skipping on it
    // would silently retire this check for every unannotated implementation
    if implementation
        .signature(db)
        .without_inferred_parameter_holes(db)
        .is_none()
    {
        return;
    }

    let Some(overload_signatures) = overloads
        .iter()
        .map(|overload| {
            overload
                .signature(db)
                .without_inferred_parameter_holes(db)
                .map(|signature| (overload, signature))
        })
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let overload_signatures = overload_signatures.into_iter();

    for (overload, overload_signature) in overload_signatures {
        let function_node = overload.node(db, context.file(), context.module());
        let Some((implementation_signature, parameter_consistency, return_type_consistency)) =
            implementation_callables.iter().find_map(|callable| {
                let mut inconsistency = None;
                for implementation_signature in &callable.signatures(db).overloads {
                    // basedpython: erase an unannotated parameter's hole before comparing —
                    // it makes the signature generic without there being any type-variable
                    // domain to reason about, and skipping on it would silently retire this
                    // check for every unannotated implementation
                    let Some(implementation_signature) =
                        implementation_signature.without_inferred_parameter_holes(db)
                    else {
                        continue;
                    };
                    let parameter_consistency = implementation_signature
                        .non_generic_implementation_parameters_consistency_with(
                            db,
                            env,
                            &overload_signature,
                        );
                    let return_type_consistency = implementation_signature
                        .non_generic_implementation_return_type_consistency_with(
                            db,
                            env,
                            &overload_signature,
                        );
                    if matches!(
                        (&parameter_consistency, &return_type_consistency),
                        (
                            ParameterConsistency::Consistent,
                            ReturnTypeConsistency::Consistent
                        )
                    ) {
                        return None;
                    }
                    inconsistency = Some((
                        implementation_signature,
                        parameter_consistency,
                        return_type_consistency,
                    ));
                }
                inconsistency
            })
        else {
            continue;
        };

        let (parameter_error_context, return_type_error_context, message) =
            match (parameter_consistency, return_type_consistency) {
                (ParameterConsistency::Consistent, ReturnTypeConsistency::Consistent) => continue,
                (
                    ParameterConsistency::Inconsistent(error_context),
                    ReturnTypeConsistency::Consistent,
                ) => (
                    Some(error_context),
                    None,
                    "Implementation does not accept all arguments of this overload",
                ),
                (
                    ParameterConsistency::Consistent,
                    ReturnTypeConsistency::Inconsistent(error_context),
                ) => (
                    None,
                    Some(error_context),
                    "Overload return type is not assignable to implementation return type",
                ),
                (
                    ParameterConsistency::Inconsistent(parameter_error_context),
                    ReturnTypeConsistency::Inconsistent(return_type_error_context),
                ) => (
                    Some(parameter_error_context),
                    Some(return_type_error_context),
                    "Overload signature is not consistent with implementation",
                ),
            };

        let Some(builder) = context.report_lint(&INVALID_OVERLOAD, &function_node.name) else {
            continue;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!("{message}"));
        if let Some(error_context) = parameter_error_context {
            diagnostic.info(format_args!(
                "Implementation signature `{}` is not assignable to overload signature `{}`",
                implementation_signature.display(db, env),
                overload_signature.display(db, env),
            ));
            error_context.attach_to(db, env, &mut diagnostic);
        }
        if let Some(error_context) = return_type_error_context {
            diagnostic.info(format_args!(
                "Overload returns `{}`, which is not assignable to implementation return type `{}`",
                overload_signature.return_ty.display(db, env),
                implementation_signature.return_ty.display(db, env),
            ));
            error_context.attach_to(db, env, &mut diagnostic);
        }
        diagnostic.annotate(
            context
                .secondary(implementation.focus_range(db, context.module()))
                .message(format_args!("Implementation defined here")),
        );
    }
}

/// A decorator that is applied inconsistently across an overload set.
struct BindingDecoratorInconsistency<'db, 'a> {
    /// The user-facing name of the decorator, without the leading `@`.
    decorator_name: &'static str,
    /// The overloads or implementation that are missing this decorator.
    missing: Vec<&'a OverloadLiteral<'db>>,
}

/// Finds binding-affecting decorator inconsistencies across an overload set.
///
/// `@classmethod` and `@staticmethod` affect the callable shape used for overload
/// implementation consistency checks. This returns a value for each decorator that appears on at
/// least one overload or implementation, but is missing from another.
///
/// For example, given:
///
/// ```py
/// @overload
/// @staticmethod
/// def f(x: int) -> int: ...
///
/// @overload
/// def f(x: str) -> str: ...
///
/// def f(x: int | str) -> int | str: ...
/// ```
///
/// this returns one `staticmethod` inconsistency whose `missing` entries are the second overload
/// and the implementation.
fn binding_decorator_inconsistencies<'db, 'a>(
    db: &dyn Db,
    overloads: &'a [OverloadLiteral<'db>],
    implementation: Option<&'a OverloadLiteral<'db>>,
) -> Vec<BindingDecoratorInconsistency<'db, 'a>> {
    const DECORATORS: [(FunctionDecorators, &str); 2] = [
        (FunctionDecorators::CLASSMETHOD, "classmethod"),
        (FunctionDecorators::STATICMETHOD, "staticmethod"),
    ];

    let mut inconsistencies = Vec::new();
    for (decorator, decorator_name) in DECORATORS {
        let mut decorator_present = false;
        let mut missing = vec![];

        for function in overloads.iter().chain(implementation) {
            if function.has_known_decorator(db, decorator) {
                decorator_present = true;
            } else {
                missing.push(function);
            }
        }

        if decorator_present && !missing.is_empty() {
            inconsistencies.push(BindingDecoratorInconsistency {
                decorator_name,
                missing,
            });
        }
    }
    inconsistencies
}
