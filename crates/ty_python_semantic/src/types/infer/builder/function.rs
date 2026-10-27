use crate::types::any_over_type;
use crate::{
    Db, DisplaySettings, FxIndexMap, ProgramEnvironment,
    reachability::is_end_of_scope_reachable,
    types::{
        Binding, CallArguments, CallableType, ClassBase, ClassLiteral, ClassType, DynamicType,
        IntersectionBuilder, KnownClass, KnownInstanceType, ParamSpecAttrKind, SpecialFormType,
        SubclassOfInner, SubclassOfType, Type, TypeContext, TypeVarBoundOrConstraints, TypeVarKind,
        UnionBuilder, UnionType,
        callable::CallableTypeKind,
        constraints::ConstraintSetBuilder,
        context::InferContext,
        cyclic::ActiveRecursionDetector,
        dedicated::{pytest, role::FunctionFrameworkRole},
        diagnostic::{
            ABSTRACT_AND_FINAL_METHOD, ASSERT_TYPE_UNSPELLABLE_SUBTYPE, DISJOINT_CAST,
            FINAL_ON_NON_METHOD, INEFFECTIVE_PRIVATE, INVALID_ARGUMENT_TYPE, INVALID_FIXTURE_TYPE,
            INVALID_PARAMETER_DEFAULT, INVALID_PARAMETRIZE, INVALID_PARAMSPEC, INVALID_TYPE_FORM,
            InvalidDecoratorDef, REDUNDANT_CAST, REDUNDANT_RETURN_ANNOTATION, REIFIED_CLASSMETHOD,
            STATIC_ASSERT_ERROR, TRAILING_LAMBDA_PARAMETERS, TRAILING_LAMBDA_RETURN_TYPE,
            TYPE_ASSERTION_FAILURE, UNKNOWN_FIXTURE, UNSOUND_RETURN_STATEMENT,
            USELESS_OVERLOAD_BODY, add_type_expression_reference_link,
            is_invalid_typed_dict_literal, report_bad_argument_to_get_protocol_members,
            report_bad_argument_to_protocol_interface, report_bool_as_int,
            report_implicit_return_type, report_invalid_decorator_def,
            report_invalid_generator_function_return_type, report_invalid_repeated_underscore,
            report_invalid_return_type, report_invalid_runtime_checkable,
            report_invalid_total_ordering_call,
            report_issubclass_check_against_protocol_with_non_method_members,
            report_missing_function_body,
            report_runtime_check_against_non_runtime_checkable_protocol,
            report_runtime_check_against_typed_dict, report_shadowed_type_variable,
            report_unsound_return_statement,
        },
        extensions,
        function::{
            FunctionBodyKind, FunctionDecorators, FunctionLiteral, FunctionType, KnownFunction,
            OverloadLiteral, function_body_kind, infers_return_types, is_implicit_classmethod,
            report_revealed_type, revealed_declared_type, same_module_uncached_raw_signature,
        },
        function_framework_role,
        generics::{enclosing_generic_contexts, typing_self},
        infer::{
            InferenceFlags, TypeExpressionFlags, TypeInferenceBuilder,
            builder::{
                DeclaredAndInferredType, DeferredExpressionState, TypeAndRange,
                TypeParamReification, validate_paramspec_components,
            },
            function_known_decorator_flags, function_known_decorators, infer_deferred_types,
            infer_function_default_types, infer_statement_types, nearest_enclosing_function,
            original_class_type,
        },
        infer_definition_types,
        inferred_signature::return_type_from_body,
        lifetimes::InheritedBorrow,
        list_members::all_members,
        relation::TypeRelation,
        signatures::{
            DefaultFit, ReturnCallableTypeVarScope, default_fit,
            function_signature_expression_type, relate_default,
        },
        trailing_lambda::{
            BlockCallee, UnbindableParameters, block_callee, trailing_lambda_it_borrow,
            trailing_lambda_it_type,
        },
        tuple::{TupleSpec, TupleSpecBuilder, TupleType},
        typed_dict::extract_unpacked_typed_dict_keys_from_kwargs_annotation,
        typevar::TypeVarSet,
        visitor::non_any_dynamic_content,
    },
};
use ruff_db::{
    diagnostic::{Annotation, DiagnosticId, Severity, Span},
    parsed::parsed_module,
    source::source_text,
};
use ruff_diagnostics::{Edit, Fix};
use ruff_python_edits::unwrapped_call_argument;
use ty_module_resolver::{ImportingFile, ModuleName, resolve_module};
use ty_python_core::{
    Truthiness,
    definition::{Definition, DefinitionKind},
    scope::NodeWithScopeRef,
};

use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::helpers::{MemberVisibility, ReturnGuardForm, return_guards};
use ruff_python_ast::{self as ast, find_node::covering_node};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;

fn parameters_have_defaults(parameters: &ast::Parameters) -> bool {
    parameters
        .iter_non_variadic_params()
        .any(|param| param.default.is_some())
}

/// basedpython: whether the parser built this function node out of a construct that is not a
/// `def` at all — an `init(...)`, a property accessor block, or a trailing-lambda block. Each of
/// those records the form it was written as with a synthetic marker decorator.
fn is_synthesized_from_another_construct(function: &ast::StmtFunctionDef) -> bool {
    function.is_trailing_lambda
        || function.decorator_list.iter().any(|decorator| {
            matches!(
                &decorator.expression,
                ast::Expr::Name(marker)
                    if matches!(marker.ctx, ast::ExprContext::Invalid)
                        && matches!(
                            marker.id.as_str(),
                            "__init_method__" | "__property__" | "__static_property__"
                        )
            )
        })
}

/// basedpython: whether this function was written `decorator def`, which the parser records with a
/// synthetic `decorator_keyword` marker decorator.
fn is_decorator_def(function: &ast::StmtFunctionDef) -> bool {
    function.decorator_list.iter().any(|decorator| {
        matches!(
            &decorator.expression,
            ast::Expr::Name(marker)
                if matches!(marker.ctx, ast::ExprContext::Invalid)
                    && marker.id.as_str() == "decorator_keyword"
        )
    })
}

fn function_has_deferred_annotations(function: &ast::StmtFunctionDef) -> bool {
    function.type_params.is_none()
        && (function.returns.is_some()
            // basedpython: a `raises` clause is a type expression of the signature too
            || function.raises.is_some()
            || function
                .parameters
                .iter()
                .any(|param| param.annotation().is_some()))
}

/// Whether a non-static method receives an instance or the class itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodReceiverKind {
    Instance,
    Class,
}

impl MethodReceiverKind {
    /// Classifies methods by their decorators and implicit class-receiver rules.
    ///
    /// Free functions and ordinary static methods have no receiver; `__new__` receives the class.
    ///
    /// ```python
    /// class Example:
    ///     def instance(self): ...
    ///     @classmethod
    ///     def class_method(cls): ...
    ///     @staticmethod
    ///     def static_method(): ...
    /// ```
    fn from_function<'db>(
        db: &'db dyn Db,
        definition: Definition<'db>,
        function: &ast::StmtFunctionDef,
    ) -> Option<Self> {
        if !definition.scope(db).scope(db).kind().is_class() {
            return None;
        }

        let decorators = if function.decorator_list.is_empty() {
            FunctionDecorators::empty()
        } else {
            function_known_decorator_flags(db, definition)
        };
        if decorators.contains(FunctionDecorators::STATICMETHOD) && function.name.id != "__new__" {
            return None;
        }

        if decorators.contains(FunctionDecorators::CLASSMETHOD)
            || is_implicit_classmethod(&function.name)
            || function.name.id == "__new__"
        {
            Some(Self::Class)
        } else {
            Some(Self::Instance)
        }
    }

    /// Accepts only `Self` for an instance receiver and `type[Self]` for a class receiver.
    fn accepts_annotation(self, db: &dyn Db, annotation: Type<'_>) -> bool {
        match (self, annotation) {
            (Self::Instance, Type::TypeVar(typevar)) => typevar.typevar(db).is_self(db),
            (Self::Class, Type::SubclassOf(subclass)) => {
                matches!(
                    subclass.subclass_of(),
                    SubclassOfInner::TypeVar(typevar) if typevar.typevar(db).is_self(db)
                )
            }
            _ => false,
        }
    }
}

/// Return type policy for checking explicit `return` statements in a function body.
#[derive(Debug, Copy, Clone)]
struct ExpectedReturnType<'db> {
    /// The externally-visible return type.
    public: Type<'db>,
    /// The return type as seen from inside the function body.
    lexical: Type<'db>,
}

impl<'db> ExpectedReturnType<'db> {
    /// Creates the expected return type policy for `function`.
    fn from_function(db: &'db dyn Db, function: FunctionType<'db>) -> Self {
        /// Normalizes special return annotations to the type actually returned by expressions.
        fn normalize<'db>(
            db: &'db dyn Db,
            env: &ProgramEnvironment<'db>,
            ty: Type<'db>,
        ) -> Type<'db> {
            match ty.resolve_type_alias(db) {
                Type::TypeIs(_) | Type::TypeGuard(_) => KnownClass::Bool.to_instance(db, env),
                _ => ty,
            }
        }

        let env = ProgramEnvironment::from_file(function.program_file(db));
        let public = normalize(
            db,
            &env,
            same_module_uncached_raw_signature(db, function, ReturnCallableTypeVarScope::Public)
                .return_ty,
        );
        let lexical = normalize(
            db,
            &env,
            same_module_uncached_raw_signature(db, function, ReturnCallableTypeVarScope::Lexical)
                .return_ty,
        );

        Self { public, lexical }
    }

    /// Returns the externally-visible return type.
    fn public(self) -> Type<'db> {
        self.public
    }

    /// Returns `true` if `ty` is accepted by either the public return type or the lexical return
    /// type.
    fn accepts(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
        relation: TypeRelation,
    ) -> bool {
        let builder = ConstraintSetBuilder::new();

        let check =
            |target| ty.has_relation_to(db, env, target, &builder, TypeVarSet::None, relation);

        check(self.public)
            .or(db, &builder, || check(self.lexical))
            .is_always_satisfied(db, env, TypeVarSet::None)
    }
}

impl<'db, 'ast> TypeInferenceBuilder<'db, 'ast> {
    pub(super) fn infer_function_body(&mut self, function: &ast::StmtFunctionDef) {
        let env = self.program_environment();
        let db = self.db();

        // Parameters are odd: they are Definitions in the function body scope, but have no
        // constituent nodes that are part of the function body. In order to get diagnostics
        // merged/emitted for them, we need to explicitly infer their definitions here.
        //
        // basedpython: a parameter the parser synthesized may be left undeclared —
        // a trailing lambda block's `it`, when its callback passes none — and
        // then has no definition to infer.
        for parameter in &function.parameters {
            if self.index.try_definition(parameter).is_some() {
                self.infer_definition(parameter);
            }
        }

        // basedpython: a destructuring parameter's pattern binds in this scope too
        for parameter in function
            .parameters
            .iter()
            .map(ast::AnyParameterRef::as_parameter)
        {
            if let Some(pattern) = parameter.pattern.as_deref() {
                self.infer_match_pattern(pattern);
                self.check_destructure(pattern);
            }
        }

        validate_paramspec_components(&self.context, self.index, &function.parameters, |expr| {
            self.file_expression_type(expr)
        });
        self.validate_unpacked_typed_dict_kwargs(&function.parameters);

        self.check_pytest_function(function);

        // basedpython: enforce `local` (no escape), both on the function's own
        // parameters and on a trailing-lambda block's `it`, which is borrowed
        // when the callee's callback declares it so
        let inherited_borrow = self.trailing_lambda_inherited_borrow(function);
        crate::types::lifetimes::check_local_lifetimes(&self.context, function, inherited_borrow);

        // basedpython: a trailing-lambda block always returns `None`, so its
        // callback must be declared to return `None`, and it binds one argument,
        // so its callback may not take more
        if function.is_trailing_lambda {
            self.check_trailing_lambda_callback_returns_none(function);
            self.check_trailing_lambda_bindable_parameters(function);
        }

        // basedpython: a non-`once` trailing-lambda block is an ordinary closure
        // (unknown execution count), so non-local control flow is not allowed, and
        // it may not bind an enclosing `let` / `final` (which could then run twice)
        if function.is_trailing_lambda && !self.trailing_lambda_callee_is_once(function) {
            crate::types::lifetimes::check_non_once_trailing_lambda(&self.context, function);
            crate::types::lifetimes::check_non_once_trailing_lambda_final_writes(
                &self.context,
                self.index,
                self.module(),
                self.scope().file_scope_id(db),
                function,
            );
        }

        self.infer_scope_body(&function.body);

        // basedpython: a `local` borrow may only be passed on to another `local`
        // parameter — checked after the body so callee types are available
        crate::types::lifetimes::check_local_argument_passing(
            &self.context,
            function,
            inherited_borrow,
            |expr| self.try_expression_type(expr),
        );

        // basedpython: enforce the `once` count obligation. also after the body:
        // a callback declared with a receiver (`Html.() -> None`) is called as
        // `page.builder()`, and only the receiver's type says that is the call
        crate::types::lifetimes::check_once_obligations(
            &self.context,
            function,
            inherited_borrow,
            |attribute| self.is_implicit_receiver_attribute(attribute),
        );

        // basedpython: check the body against the `raises` clause. runs after the
        // body so callee types are available, and reads this in-progress
        // inference rather than re-entering it as a query
        crate::types::exceptions::check_function_exceptions(
            &self.context,
            function,
            self.scope(),
            self.index.expect_single_definition(function),
            |expr| self.try_expression_type(expr).unwrap_or_else(Type::unknown),
            |call| self.call_solution(call),
        );

        // basedpython: the guard a `raises` function is lowered behind names the declared
        // exceptions as the module reaches them, so it reads the bindings it names them by
        if function.raises.is_some() && self.is_basedpython_file() {
            let db = self.db();
            let function_ty =
                crate::types::binding_type(db, self.index.expect_single_definition(function));
            if let Some(target) = crate::types::exceptions::declared_raises_runtime_target(
                db,
                self.program_environment(),
                self.file(),
                function_ty,
            ) {
                self.context.record_spelling(&target.ceiling);
                if let Some(resolved) = &target.resolved {
                    self.context.record_spelling(&resolved.expression);
                }
            }
        }

        // basedpython: a trailing-lambda block in a loop that captures a loop
        // variable is a late-binding trap unless its callee confines it
        // (`local` / `once`) — the type-aware complement to ruff's `B023`
        crate::types::lifetimes::check_loop_variable_capture(
            &self.context,
            &function.body,
            |expr| self.try_expression_type(expr),
        );

        // basedpython: `None` is what a `def` returns when it says nothing, so an
        // explicit `-> None` on a body that already hands back `None` is a word
        // with nothing behind it
        self.check_redundant_return_annotation(function);

        // basedpython: a `def` written with no body at all declares a signature. where the
        // position asks for an implementation, the missing body is the whole of the problem
        self.check_missing_function_body(function);

        // basedpython: a `decorator def` the lowering cannot expand is refused when the file is
        // transpiled; reported here, the refusal arrives while the file is being checked
        self.check_decorator_def(function);

        // basedpython: a parameter list that repeats `_` in a shape the lowering has no python
        // spelling for is refused when the file is transpiled, and reported here
        self.check_repeated_underscores(function);

        let enclosing_function_for_return_check =
            nearest_enclosing_function(db, self.index, self.scope());

        // basedpython: if the function is an overloaded impl with no explicit
        // return annotation, validate against the union of the overload return
        // types inherited via `OverloadLiteral::raw_signature`. fall back to
        // the function name as the secondary diagnostic range. skip when the
        // impl body is just `...` or a docstring: that shape signals a
        // placeholder rather than a real implementation, so we don't want to
        // surface implicit-return-None against the inherited union
        let inherited_return_range = function.returns.as_ref().map_or_else(
            || {
                let enclosing = enclosing_function_for_return_check?;
                // basedpython: the getter of an untyped property with an initialiser is held to
                // the type that initialiser declares, as it would be to a written one. the
                // property's name is where that type was stated
                if function.property_construct_range().is_some()
                    && enclosing
                        .literal(db)
                        .last_definition
                        .property_initialiser_type(db)
                        .is_some()
                {
                    return Some(function.name.range());
                }
                let (overloads, implementation) = enclosing.overloads_and_implementation(db);
                if overloads.is_empty() || implementation.is_none() {
                    return None;
                }
                if function_body_kind(db, env, function, |expr| self.expression_type(expr))
                    == FunctionBodyKind::Stub
                {
                    return None;
                }
                Some(function.name.range())
            },
            |returns| Some(returns.range()),
        );

        if let Some(returns_range) = inherited_return_range {
            let has_empty_body = self.return_types_and_ranges.is_empty()
                && function_body_kind(db, env, function, |expr| self.expression_type(expr))
                    == FunctionBodyKind::Stub;

            let mut enclosing_class_context = None;

            if has_empty_body {
                // basedpython: a `def` written with no body at all is
                // `missing-function-body`'s to report, whatever it declares it returns. it
                // has no body to check a return type against, and where the position does
                // not ask for a declaration the body is what went missing — saying instead
                // that the function returns `None` describes the consequence, and says
                // nothing at all when the return type is `None` already
                if function.body.is_empty() {
                    return;
                }
                if self.in_stub() {
                    return;
                }
                if self.in_function_overload_or_abstractmethod() {
                    return;
                }
                if self.is_in_type_checking_block(self.scope(), function) {
                    return;
                }
                if let Some(class) = self.class_context_of_current_method() {
                    enclosing_class_context = Some(class);
                    if class.is_protocol(db) {
                        return;
                    }
                }
            }

            // the enclosing function has no type yet while this scope is still going round a
            // cycle — recovering an unannotated signature reads the body, which is what put us
            // here. the check runs on the iteration that settles, and diagnostics from the
            // provisional ones are discarded
            let Some(enclosing_function) = enclosing_function_for_return_check else {
                return;
            };
            let declared_ty = same_module_uncached_raw_signature(
                db,
                enclosing_function,
                ReturnCallableTypeVarScope::Public,
            )
            .return_ty;
            let expected_return = ExpectedReturnType::from_function(db, enclosing_function);
            let expected_ty = expected_return.public();

            let scope_id = self.index.node_scope(NodeWithScopeRef::Function(function));
            if scope_id.is_generator_function(self.index) {
                // TODO: `AsyncGeneratorType` and `GeneratorType` are both generic classes.
                //
                // If type arguments are supplied to `(Async)Iterable`, `(Async)Iterator`,
                // `(Async)Generator` or `(Async)GeneratorType` in the return annotation,
                // we should iterate over the `yield` expressions and `return` statements
                // in the function to check that they are consistent with the type arguments
                // provided. Once we do this, the `.to_instance_unknown` call below should
                // be replaced with `.to_specialized_instance`.
                let inferred_return = if function.is_async {
                    KnownClass::AsyncGeneratorType
                } else {
                    KnownClass::GeneratorType
                };
                if !inferred_return
                    .to_instance_unknown(db, env)
                    .is_assignable_to(db, env, expected_ty)
                {
                    report_invalid_generator_function_return_type(
                        &self.context,
                        returns_range,
                        inferred_return,
                        declared_ty,
                    );
                }

                if let Some(expected_return_ty) = declared_ty.generator_return_type(db, env) {
                    for returned in self.return_types_and_ranges.iter().copied() {
                        report_bool_as_int(
                            &self.context,
                            returned.range,
                            returned.ty,
                            expected_return_ty,
                        );
                    }
                    for &return_statement in &self.return_types_and_ranges {
                        if !return_statement
                            .ty
                            .is_assignable_to(db, env, expected_return_ty)
                        {
                            report_invalid_return_type(
                                &self.context,
                                return_statement.range,
                                returns_range,
                                expected_return_ty,
                                return_statement.ty,
                            );
                        } else if self.context.is_lint_enabled(&UNSOUND_RETURN_STATEMENT)
                            && expected_return_ty.is_fully_static(db, env)
                            && !return_statement.ty.is_pure_redundant_with(
                                db,
                                env,
                                expected_return_ty,
                            )
                        {
                            // N.B. the implementation here is the ~same as for `UNSOUND_YIELD` and `UNSOUND_ASSIGNMENT`;
                            // update those too if updating this!
                            report_unsound_return_statement(
                                &self.context,
                                return_statement.range,
                                returns_range,
                                expected_return_ty,
                                return_statement.ty,
                            );
                        }
                    }

                    if is_end_of_scope_reachable(db, self.scope())
                        && !Type::none(db, env).is_assignable_to(db, env, expected_return_ty)
                    {
                        let no_return = self.return_types_and_ranges.is_empty();
                        report_implicit_return_type(
                            &self.context,
                            returns_range,
                            expected_return_ty,
                            false,
                            None,
                            no_return,
                        );
                    }
                }

                return;
            }

            for returned in self.return_types_and_ranges.iter().copied() {
                report_bool_as_int(&self.context, returned.range, returned.ty, declared_ty);
            }

            for return_statement in
                self.return_types_and_ranges
                    .iter()
                    .copied()
                    .filter_map(|ty_range| match ty_range.ty {
                        // We skip `is_assignable_to` checks for `NotImplemented`,
                        // so we remove it beforehand.
                        Type::Union(union) => Some(TypeAndRange {
                            ty: union.filter(db, |ty| !ty.is_notimplemented(db)),
                            range: ty_range.range,
                        }),
                        ty if ty.is_notimplemented(db) => None,
                        _ => Some(ty_range),
                    })
            {
                // basedpython: a `return` is a conversion site — an in-scope
                // a conformance or a conversion dunder makes the value
                // returnable where it otherwise is not, and the transpiler emits the
                // conversion. the generator paths above return early, so this is the
                // plain case where the declared type really is the value's target
                // only when the type the transpiler will recover for this function is
                // the one being enforced here; otherwise the conversion it emits would
                // be built from a different target
                if crate::types::conversions::function_declared_return_type(
                    db,
                    self.file(),
                    function,
                ) == Some(declared_ty)
                    && crate::types::conversions::repair_conversion(
                        db,
                        env,
                        self.file(),
                        return_statement.ty,
                        declared_ty,
                        crate::types::conversions::returned_value_at(
                            function,
                            return_statement.range,
                        ),
                    )
                    .is_some_and(|repair| {
                        crate::types::conversions::accept_conversion(
                            &self.context,
                            return_statement.range,
                            &repair,
                        );
                        true
                    })
                {
                    continue;
                }
                if !expected_return.accepts(
                    db,
                    env,
                    return_statement.ty,
                    TypeRelation::Assignability,
                ) {
                    report_invalid_return_type(
                        &self.context,
                        return_statement.range,
                        returns_range,
                        declared_ty,
                        return_statement.ty,
                    );
                } else if self.context.is_lint_enabled(&UNSOUND_RETURN_STATEMENT)
                    && expected_return.public.is_fully_static(db, env)
                    && !expected_return.accepts(
                        db,
                        env,
                        return_statement.ty,
                        TypeRelation::Redundancy { pure: true },
                    )
                {
                    // N.B. the implementation here is the ~same as for `UNSOUND_YIELD` and `UNSOUND_ASSIGNMENT`;
                    // update those too if updating this!
                    report_unsound_return_statement(
                        &self.context,
                        return_statement.range,
                        returns_range,
                        declared_ty,
                        return_statement.ty,
                    );
                }
            }

            if is_end_of_scope_reachable(db, self.scope())
                && !Type::none(db, env).is_assignable_to(db, env, expected_ty)
            {
                let no_return = self.return_types_and_ranges.is_empty();
                report_implicit_return_type(
                    &self.context,
                    returns_range,
                    declared_ty,
                    has_empty_body,
                    enclosing_class_context,
                    no_return,
                );
            }
        }
    }

    /// basedpython: report a `def` written with no body at all, where the position asks for an
    /// implementation.
    ///
    /// The lowering fills the missing body in with `: ...`, so a `def` that declares nothing but
    /// a signature still produces a function — one that returns `None`. That is what a stub
    /// file, a protocol, an `abstract def`, an overload group and an `if TYPE_CHECKING` block
    /// each ask for. Everywhere else the implementation the signature promises was never
    /// written, and the `: ...` stands in for it silently.
    fn check_missing_function_body(&self, function: &ast::StmtFunctionDef) {
        if !function.body.is_empty() || !self.is_basedpython_file() {
            return;
        }
        // only a `def` the source wrote can be missing a body. every other construct the
        // parser builds a function out of is given the body its own form means — an
        // `init(...)` gets one built from its attribute parameters, an accessor block gets
        // the accessors — so an empty one reaching here failed to parse, which has been
        // reported already. a `decorator def` is not one of them: it is a `def` with a
        // keyword in front, and the dispatcher its lowering writes calls the body the source
        // is supposed to supply
        if is_synthesized_from_another_construct(function) {
            return;
        }
        if self.bodyless_def_declares_a_signature(function) {
            return;
        }
        report_missing_function_body(&self.context, function);
    }

    /// basedpython: report a `decorator def` that has no decorator to expand into.
    ///
    /// The lowering builds the two overloads and the runtime dispatcher out of a first parameter
    /// that receives the decorated function and options that each carry a default, and refuses
    /// anything else. What it refuses is what is reported here, so the shape is settled while the
    /// file is checked rather than when it is transpiled. ty models the accepted shape as the same
    /// pair of signatures — see [`Signature::decorator_keyword_overloads`] — and reads a
    /// declaration it refuses as the plain function it was written as.
    ///
    /// [`Signature::decorator_keyword_overloads`]: crate::types::signatures::Signature::decorator_keyword_overloads
    fn check_decorator_def(&self, function: &ast::StmtFunctionDef) {
        if !is_decorator_def(function) {
            return;
        }
        let name = function.name.id.as_str();
        let report = |node: AnyNodeRef, problem: &InvalidDecoratorDef| {
            report_invalid_decorator_def(&self.context, node, name, problem);
        };

        // the keyword is module-scope only: the overloads the lowering writes would stand in the
        // enclosing class's namespace, where they are attributes rather than declarations
        if self
            .index
            .ancestor_scopes(self.scope().file_scope_id(self.db()))
            .skip(1)
            .any(|(_, ancestor)| ancestor.kind().is_class())
        {
            report((&function.name).into(), &InvalidDecoratorDef::InClassBody);
            return;
        }

        let parameters = &function.parameters;
        if let Some(variadic) = parameters.vararg.as_deref().or(parameters.kwarg.as_deref()) {
            report(variadic.into(), &InvalidDecoratorDef::Variadic);
            return;
        }

        let mut positional = parameters.posonlyargs.iter().chain(&parameters.args);
        let Some(decorated) = positional.next() else {
            report(
                parameters.as_ref().into(),
                &InvalidDecoratorDef::NoDecoratedParameter,
            );
            return;
        };
        if let Some(default) = decorated.default.as_deref() {
            report(
                default.into(),
                &InvalidDecoratorDef::DecoratedParameterDefault {
                    parameter: decorated.name().as_str(),
                },
            );
            return;
        }

        for option in positional.chain(&parameters.kwonlyargs) {
            if option.default.is_none() {
                report(
                    (&option.parameter).into(),
                    &InvalidDecoratorDef::OptionWithoutDefault {
                        parameter: option.name().as_str(),
                    },
                );
                return;
            }
        }
    }

    /// basedpython: report a parameter list that repeats `_` in a shape the lowering refuses.
    /// the refusal is the one the definition's signature is built from — see
    /// [`OverloadLiteral::repeated_underscores`]
    fn check_repeated_underscores(&self, function: &ast::StmtFunctionDef) {
        let db = self.db();
        if function
            .parameters
            .iter()
            .filter(|parameter| parameter.name() == "_")
            .count()
            < 2
        {
            return;
        }
        let Some(function_type) = nearest_enclosing_function(db, self.index, self.scope()) else {
            return;
        };
        let Some(Err(refusal)) = function_type
            .literal(db)
            .last_definition
            .repeated_underscores(db)
        else {
            return;
        };
        let node = function.parameters.iter().nth(refusal.index()).map_or_else(
            || AnyNodeRef::from(function.parameters.as_ref()),
            |parameter| AnyNodeRef::from(parameter.as_parameter()),
        );
        report_invalid_repeated_underscore(&self.context, node, refusal);
    }

    /// basedpython: whether a `def` written with no body at all declares a signature in this
    /// position, rather than leaving out an implementation.
    fn bodyless_def_declares_a_signature(&self, function: &ast::StmtFunctionDef) -> bool {
        let db = self.db();

        if self.in_stub() || self.is_in_type_checking_block(self.scope(), function) {
            return true;
        }

        // `@overload` and `@abstractmethod`, which the `abstract` modifier resolves to
        if self.in_function_overload_or_abstractmethod() {
            return true;
        }

        // a bodyless `def` in a run of same-name defs is an overload declaration: the lowering
        // writes the `@overload` decorator the source leaves out, so it declares a signature
        // for the same reason a written one does
        if nearest_enclosing_function(db, self.index, self.scope())
            .is_some_and(|enclosing| enclosing.literal(db).last_definition.is_overload(db))
        {
            return true;
        }

        self.class_context_of_current_method()
            .is_some_and(|class| class.is_protocol(db))
    }

    /// basedpython: report an explicit `-> None` that leaves the function's type exactly where
    /// deleting it would.
    ///
    /// The question is only ever that one: what would this `def` return with the annotation
    /// gone? Where that type comes from — the body, an overridden base, a sibling overload
    /// group — decides nothing. Everything left unreported is a case where the answer is not
    /// `None`: a generator hands back a generator, a body that always raises hands back `Never`,
    /// and an override or an overload implementation hands back whatever it inherits.
    fn check_redundant_return_annotation(&self, function: &ast::StmtFunctionDef) {
        let env = self.program_environment();
        let db = self.db();

        let Some(returns) = function.returns.as_deref() else {
            return;
        };
        // `-> asserts x` names a place a call narrows, not a type
        if function.is_asserts_return || !returns.is_none_literal_expr() {
            return;
        }

        // an `init(...)` is given its `-> None` by the parser, zero-width and after the
        // parameter list. there is nothing in the source to remove, so there is nothing to say
        if returns.range().is_empty() {
            return;
        }

        // basedpython: a property getter's return annotation is the type its declaration wrote.
        // `let n: None` states the property's type, and there is no `-> None` to remove
        if function.property_construct_range().is_some() {
            return;
        }

        // everything below reads the class MRO and the enclosing overload chain, so don't
        // pay for it when nothing will be reported
        if !self.context.is_lint_enabled(&REDUNDANT_RETURN_ANNOTATION) {
            return;
        }

        // with nothing recovering the signature, dropping `-> None` widens the return type to
        // `Unknown`, so the annotation is carrying the type on its own
        if !infers_return_types(db, self.file()) {
            return;
        }

        let Some(function_type) = self.current_function_type() else {
            return;
        };

        let scope_id = self.index.node_scope(NodeWithScopeRef::Function(function));
        let without_annotation = function_type
            .literal(db)
            .last_definition
            .return_type_without_annotation(db, env, || {
                return_type_from_body(
                    db,
                    env,
                    scope_id.to_scope_id(db, self.program_file()),
                    function,
                    scope_id.is_generator_function(self.index),
                    self.index.use_def_map(scope_id),
                    |expr| self.expression_type(expr),
                )
            });

        if !without_annotation.is_none(db) {
            return;
        }

        if let Some(builder) = self
            .context
            .report_lint(&REDUNDANT_RETURN_ANNOTATION, returns)
        {
            let mut diagnostic = builder.into_diagnostic("Redundant `-> None` return annotation");
            diagnostic.info("a `def` that leaves out its return type already returns `None`");
            diagnostic.help("Remove the annotation");
        }
    }

    /// Check a pytest test or fixture function: its parameters against the
    /// fixtures they resolve to, and any `@pytest.mark.parametrize` markers
    /// against the function's signature. A function pytest does not manage is
    /// left untouched.
    fn check_pytest_function(&self, function_node: &ast::StmtFunctionDef) {
        let env = self.program_environment();
        let db = self.db();
        let Some(function) = self.current_function_type() else {
            return;
        };
        if !function_framework_role(db, function).is_some_and(FunctionFrameworkRole::is_pytest) {
            return;
        }

        self.check_parametrize(function_node, function);

        // parametrized names are supplied as arguments, not by fixtures, so
        // they are excluded from the fixture resolution below
        let parametrized = pytest::parametrized_names(db, function);

        let file = self.file();
        let callable = function.signature(db);
        let Some(signature) = callable.iter().last() else {
            return;
        };
        for parameter in signature.parameters() {
            if parameter.is_variadic() || parameter.is_keyword_variadic() {
                continue;
            }
            let Some(name) = parameter.name() else {
                continue;
            };
            if parametrized.contains(name) {
                continue;
            }
            let Some(range) = parameter
                .definition()
                .map(|definition| definition.focus_range(db, self.module()).range())
            else {
                continue;
            };

            match pytest::resolve_fixture(db, env, file, name.as_str()) {
                Some(fixture) => {
                    // only an explicitly annotated parameter can disagree with
                    // its fixture; an unannotated one adopts the fixture's type,
                    // whether it is left gradual or given an anonymous hole
                    if !parameter.should_annotation_be_displayed()
                        || parameter.annotated_type().is_inferred_parameter_hole(db)
                    {
                        continue;
                    }
                    let Some(provided) = fixture.provided_type else {
                        continue;
                    };
                    let declared = parameter.annotated_type();
                    if provided.is_assignable_to(db, env, declared) {
                        continue;
                    }
                    let Some(builder) = self.context.report_lint(&INVALID_FIXTURE_TYPE, range)
                    else {
                        continue;
                    };
                    let mut diagnostic = builder.into_diagnostic(format_args!(
                        "Fixture `{name}` provides `{}`, but the parameter is annotated `{}`",
                        provided.display(db, env),
                        declared.display(db, env),
                    ));
                    if let Some(fixture_definition) = fixture.definition {
                        let fixture_module =
                            parsed_module(db, fixture_definition.program_file(db).python_file(db))
                                .load(db);
                        let span = Span::from(fixture_definition.focus_range(db, &fixture_module));
                        diagnostic
                            .annotate(Annotation::secondary(span).message("fixture defined here"));
                    }
                }
                None => {
                    if let Some(builder) = self.context.report_lint(&UNKNOWN_FIXTURE, range) {
                        builder
                            .into_diagnostic(format_args!("No fixture named `{name}` is defined"));
                    }
                }
            }
        }
    }

    /// Check every `@pytest.mark.parametrize` marker on `function_node`: each
    /// name against the function's parameters, and each value row's length
    /// against the number of names.
    fn check_parametrize(&self, function_node: &ast::StmtFunctionDef, function: FunctionType<'db>) {
        let env = self.program_environment();
        let db = self.db();
        let callable = function.signature(db);
        let parameter_names: FxHashSet<&ast::name::Name> = callable
            .iter()
            .last()
            .map(|signature| {
                signature
                    .parameters()
                    .iter()
                    .filter_map(|parameter| parameter.name())
                    .collect()
            })
            .unwrap_or_default();

        for decorator in &function_node.decorator_list {
            let Some(marker) = pytest::parametrize_marker(db, env, function, decorator) else {
                continue;
            };

            for name in &marker.names {
                if !parameter_names.contains(name)
                    && let Some(builder) = self
                        .context
                        .report_lint(&INVALID_PARAMETRIZE, marker.argnames)
                {
                    builder.into_diagnostic(format_args!(
                        "`{}` has no parameter `{name}` to parametrize",
                        function_node.name,
                    ));
                }
            }

            if marker.names.len() > 1
                && let Some(argvalues) = marker.argvalues
            {
                self.check_parametrize_arity(argvalues, marker.names.len());
            }
        }
    }

    /// Check that each literal value row in `argvalues` has `arity` elements.
    /// Rows that are not list/tuple literals cannot be checked and are skipped.
    fn check_parametrize_arity(&self, argvalues: &ast::Expr, arity: usize) {
        let rows = match argvalues {
            ast::Expr::List(list) => &list.elts,
            ast::Expr::Tuple(tuple) => &tuple.elts,
            _ => return,
        };
        for row in rows {
            let row_len = match row {
                ast::Expr::Tuple(tuple) => tuple.elts.len(),
                ast::Expr::List(list) => list.elts.len(),
                _ => continue,
            };
            if row_len != arity {
                if let Some(builder) = self.context.report_lint(&INVALID_PARAMETRIZE, row) {
                    builder.into_diagnostic(format_args!(
                        "parametrize value set has {row_len} values, but {arity} names were given",
                    ));
                }
            }
        }
    }

    pub(super) fn infer_function_definition_statement(&mut self, function: &ast::StmtFunctionDef) {
        self.infer_definition(function);
    }

    pub(super) fn infer_function_definition(
        &mut self,
        function: &ast::StmtFunctionDef,
        definition: Definition<'db>,
    ) {
        let ast::StmtFunctionDef {
            range: _,
            node_index: _,
            is_async: _,
            name,
            type_params: _,
            parameters,
            returns: _,
            raises: _,
            body: _,
            decorator_list,
            is_trailing_lambda,
            is_asserts_return: _,
        } = function;

        let db = self.db();

        let decorator_inference =
            (!decorator_list.is_empty()).then(|| function_known_decorators(self.db(), definition));
        if let Some(decorator_inference) = decorator_inference.as_ref() {
            self.context.extend(decorator_inference.diagnostics());
            self.expressions
                .extend(decorator_inference.expression_types());
            self.bindings.extend(decorator_inference.bindings());
            self.called_functions
                .extend(decorator_inference.called_functions().iter().copied());
            self.implicit_aliases
                .extend(decorator_inference.implicit_aliases().iter().copied());
        }

        let mut decorator_types_and_nodes = Vec::with_capacity(decorator_list.len());
        let mut has_transforming_decorators = false;
        let mut function_decorators = FunctionDecorators::empty();
        let mut dataclass_transformer_params = None;
        let mut final_decorator = None;
        let mut visibility_modifier = None;

        for decorator in decorator_list {
            // basedpython: a trailing lambda block's synthetic decorator holds the
            // called expression, not a decorator. the call is checked (with the
            // lambda appended) in the decorators region; the function type itself
            // stays undecorated
            if *is_trailing_lambda {
                continue;
            }
            // basedpython `decorator def` parses as a synthetic decorator whose
            // expression name is `decorator_keyword` (with `ExprContext::Invalid`).
            // the transpile expands the function into overloads + a runtime
            // dispatcher; ty doesn't model that rewrite, so we skip the synthetic
            // decorator entirely to avoid polluting the function's type. the
            // visibility markers (`private`/`export`/`open`) are likewise
            // transpile-only — a rename or `__all__` entry with no type-level
            // effect — and would otherwise resolve to `Unknown` and poison the
            // function type. (`final`/`abstract`/`static`/… map to real stdlib
            // decorators via `synthetic_decorator_target_type` and are kept.)
            // `private` and `protected` have no decorator either, but they *are*
            // recorded: they decide who may reach the method, and a member no
            // widened view can reach is what makes a class's variance safe
            if let ast::Expr::Name(n) = &decorator.expression
                && matches!(n.ctx, ast::ExprContext::Invalid)
                && matches!(
                    n.id.as_str(),
                    // `__init_method__` marks the `init(...)` shorthand — a plain
                    // `__init__`, so the synthetic marker is dropped too
                    "decorator_keyword"
                        | "private"
                        | "protected"
                        | "export"
                        | "open"
                        | "__init_method__"
                )
            {
                match n.id.as_str() {
                    "private" => {
                        function_decorators |= FunctionDecorators::PRIVATE;
                        visibility_modifier = Some((MemberVisibility::Private, decorator));
                    }
                    "protected" => {
                        function_decorators |= FunctionDecorators::PROTECTED;
                        visibility_modifier = Some((MemberVisibility::Protected, decorator));
                    }
                    _ => {}
                }
                continue;
            }
            // `type def` parses as a synthetic `type_fn` marker. it is not a
            // runtime decorator — it records that applying this function with
            // `[]` in a type expression evaluates it
            if let ast::Expr::Name(n) = &decorator.expression
                && matches!(n.ctx, ast::ExprContext::Invalid)
                && n.id.as_str() == "type_fn"
            {
                // proof of concept: the body's parameters are types, not the
                // `TypeInfo` values it actually receives, so checking it as
                // ordinary code reports nonsense (`X <= int` on a typevar).
                // until the parameters are typed, the body is unchecked
                // a `type def` carries its own flag, so consumers can tell it apart
                // from an ordinary function. it *additionally* borrows
                // `NO_TYPE_CHECK` because its body genuinely cannot be checked yet:
                // the parameters are the type arguments of an application rather
                // than the `TypeInfo` values the body receives, and `->` declares the
                // resulting type rather than a value-level return. both must be
                // modelled before this can come off
                function_decorators |= FunctionDecorators::TYPE_FN;
                function_decorators |= FunctionDecorators::NO_TYPE_CHECK;
                self.context.inference_flags |= InferenceFlags::IN_NO_TYPE_CHECK;
                continue;
            }

            let decorator_type = decorator_inference
                .as_ref()
                .and_then(|decorator_inference| {
                    decorator_inference.expression_type(&decorator.expression)
                })
                .unwrap_or_else(Type::unknown);
            let decorator_function_decorator =
                FunctionDecorators::from_decorator_type(db, decorator_type);
            function_decorators |= decorator_function_decorator;

            match decorator_type {
                Type::FunctionLiteral(function) => match function.known(db) {
                    Some(KnownFunction::NoTypeCheck) => {
                        // If the function is decorated with the `no_type_check` decorator,
                        // we need to suppress any errors that come after the decorators.
                        self.context.inference_flags |= InferenceFlags::IN_NO_TYPE_CHECK;
                        continue;
                    }
                    Some(KnownFunction::Final) => {
                        final_decorator = Some(decorator);
                        continue;
                    }
                    _ => {}
                },
                Type::DataclassTransformer(params) => {
                    dataclass_transformer_params = Some(params);
                }
                _ => {}
            }
            // a decorator that maps to a flag is a marker: recording it is the whole
            // effect, so it is not applied. `classmethod` and `staticmethod` are applied
            // all the same, and so is basedpython's static-property marker — the flag
            // only says how the getter's receiver is typed, while the descriptor it
            // resolves to is the member's actual type
            if !decorator_function_decorator
                .difference(FunctionDecorators::BY_STATIC_PROPERTY)
                .is_empty()
                && !decorator_function_decorator
                    .intersects(FunctionDecorators::CLASSMETHOD | FunctionDecorators::STATICMETHOD)
            {
                continue;
            }

            // basedpython: the static-property marker resolves to the descriptor that is the
            // member's type, so it transforms the function like any unflagged decorator
            has_transforming_decorators |= decorator_function_decorator
                .difference(FunctionDecorators::BY_STATIC_PROPERTY)
                .is_empty();
            decorator_types_and_nodes.push((decorator_type, decorator));
        }
        if !has_transforming_decorators {
            // With only known decorators, use the complete overload set's declaration
            // flags, including its recovery for inconsistently decorated overloads.
            decorator_types_and_nodes.clear();
        }

        // Check for `@final` applied to non-method functions.
        // `@final` is only meaningful on methods and classes.
        if let Some(final_decorator) = final_decorator
            && !self
                .index
                .scope(self.scope().file_scope_id(db))
                .kind()
                .is_class()
            && let Some(builder) = self
                .context
                .report_lint(&FINAL_ON_NON_METHOD, final_decorator)
        {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`@final` cannot be applied to non-method function `{name}`",
            ));
            diagnostic.info("`@final` is only meaningful on methods and classes");
        }

        // basedpython: a visibility keyword the lowering cannot act on hides
        // nothing. it is reported here rather than left to the lowering, which can
        // only silently do nothing with it
        if let Some((visibility, visibility_modifier)) = visibility_modifier
            && !ruff_python_stdlib::basedpython::private_mangles(&name.id)
            && name.id != "__init__"
            && self
                .index
                .scope(self.scope().file_scope_id(db))
                .kind()
                .is_class()
            && let Some(builder) = self
                .context
                .report_lint(&INEFFECTIVE_PRIVATE, visibility_modifier)
        {
            let keyword = visibility.keyword();
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{keyword}` has no effect on `{name}`",
                name = name.id
            ));
            diagnostic.info(format_args!(
                "`{keyword}` renames a member to `{prefix}<name>`, and a dunder is called by \
                 the exact name python knows it under",
                prefix = visibility.name_prefix(),
            ));
            diagnostic.info("`init` is the one dunder a visibility keyword says something about");
        }

        // basedpython: a classmethod cannot have reified type parameters — the
        // classmethod binding hides the function whose closure would hold the
        // reified cells, so the specialization step has nothing to rebuild
        if self.is_basedpython_file()
            && (function_decorators.contains(FunctionDecorators::CLASSMETHOD)
                || is_implicit_classmethod(&name.id))
            && let Some(type_params) = function.type_params.as_deref()
        {
            let reified =
                crate::reified::reified_type_param_names(self.file().source_type(db), function);
            if let Some(first) = reified.first()
                && let Some(builder) = self.context.report_lint(&REIFIED_CLASSMETHOD, type_params)
            {
                let declared = type_params
                    .iter()
                    .any(|param| param.is_reified() && param.name().id == *first);
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "Classmethod `{name}` cannot have reified type parameters"
                ));
                let cause = if declared {
                    "is declared `reified`"
                } else {
                    "is referenced in a value position, which reifies it"
                };
                diagnostic.info(format_args!(
                    "type parameter `{first}` {cause} — the classmethod binding \
                     hides the function whose closure would hold its value"
                ));
                if is_implicit_classmethod(&name.id) {
                    diagnostic.info(format_args!("`{name}` is implicitly a classmethod"));
                }
            }
        }

        if function_decorators
            .contains(FunctionDecorators::ABSTRACT_METHOD | FunctionDecorators::FINAL)
            && self
                .index
                .scope(self.scope().file_scope_id(db))
                .kind()
                .is_class()
            && let Some(builder) = self.context.report_lint(&ABSTRACT_AND_FINAL_METHOD, name)
        {
            builder.into_diagnostic(format_args!(
                "Method `{name}` cannot be both `@abstractmethod` and `@final`",
            ));
        }

        // If there are type params, parameters and returns are evaluated in that scope. Otherwise,
        // we defer the inference of any parameter and return annotations. That ensures that we do
        // not add any spurious salsa cycles when applying decorators below. (Applying a decorator
        // requires getting the signature of this function definition, which in turn requires
        // (lazily) inferring the parameter and return types.) If defaults exist, we also defer so
        // they can be inferred once with type context in the enclosing scope.
        if function_has_deferred_annotations(function) || parameters_have_defaults(parameters) {
            self.deferred.insert(definition);
        }

        let known_function = KnownFunction::try_from_definition_and_name(db, definition, name);

        // `type_check_only` is itself not available at runtime
        if known_function == Some(KnownFunction::TypeCheckOnly) {
            function_decorators |= FunctionDecorators::TYPE_CHECK_ONLY;
        }

        let body_scope = self
            .index
            .node_scope(NodeWithScopeRef::Function(function))
            .to_scope_id(db, self.program_file());

        let overload_literal = OverloadLiteral::new(
            db,
            &name.id,
            known_function,
            body_scope,
            function_decorators,
            None,
            dataclass_transformer_params,
            function.returns.is_some(),
        );
        let function_literal = FunctionLiteral::new(db, overload_literal);
        let function_type = FunctionType::new(db, function_literal, None);
        let is_decorated_overload_implementation =
            has_transforming_decorators && function_literal.has_separate_implementation(db);
        let is_decorated_overload = has_transforming_decorators && overload_literal.is_overload(db);

        let mut inferred_ty = Type::FunctionLiteral(
            if is_decorated_overload_implementation || is_decorated_overload {
                FunctionType::new(db, function_literal.without_overloads(), None)
            } else {
                function_type
            },
        );
        // Explicit method wrappers apply in decorator order. Their declaration flags
        // must not make the input to an inner decorator look already wrapped.
        if has_transforming_decorators
            && function_decorators
                .intersects(FunctionDecorators::STATICMETHOD | FunctionDecorators::CLASSMETHOD)
        {
            inferred_ty = inferred_ty.underlying_function(db);
        }
        if !decorator_list.is_empty() {
            self.undecorated_type = Some(inferred_ty);
        }

        // Check that the function's own type parameters don't shadow
        // type variables from enclosing scopes (by name).
        if let Some(type_params) = &function.type_params {
            let current_scope = self.scope().file_scope_id(db);
            for type_param in type_params.iter() {
                let param_name = type_param.name();
                for enclosing in enclosing_generic_contexts(self.db(), self.index, current_scope) {
                    if let Some(other_typevar) = enclosing.binds_named_typevar(db, &param_name.id) {
                        let kind = match type_param {
                            ast::TypeParam::TypeVar(_) => TypeVarKind::Pep695TypeVar,
                            ast::TypeParam::ParamSpec(_) => {
                                TypeVarKind::double_starred_type_param(self.source_type())
                            }
                            ast::TypeParam::TypeVarTuple(_) => TypeVarKind::Pep695TypeVarTuple,
                        };
                        report_shadowed_type_variable(
                            &self.context,
                            &param_name.id,
                            "function",
                            &function.name.id,
                            function.name.range(),
                            kind,
                            other_typevar,
                        );
                    }
                }
            }
        }

        for (decorator_ty, decorator_node) in decorator_types_and_nodes.iter().rev() {
            let descriptor_kind = match decorator_ty {
                Type::ClassLiteral(class) => match class.known(db) {
                    Some(KnownClass::Staticmethod) => Some(CallableTypeKind::StaticMethodLike),
                    Some(KnownClass::Classmethod) => Some(CallableTypeKind::ClassMethodLike),
                    _ => None,
                },
                _ => None,
            };
            if let Some(kind) = descriptor_kind {
                let wrap = |ty: Type<'db>| match ty.resolve_type_alias(db) {
                    Type::FunctionLiteral(function)
                        if function.callable_type_kind(db) == CallableTypeKind::FunctionLike
                            || function.callable_type_kind(db) == kind =>
                    {
                        Some(Type::FunctionLiteral(
                            function.with_descriptor_kind(db, kind),
                        ))
                    }
                    Type::Callable(callable)
                        if matches!(
                            callable.kind(db),
                            CallableTypeKind::Regular | CallableTypeKind::FunctionLike
                        ) || callable.kind(db) == kind =>
                    {
                        Some(Type::Callable(callable.with_kind(db, kind)))
                    }
                    _ => None,
                };
                let wrapped = if let Some(union) = inferred_ty.as_union_like(db) {
                    union.try_map(db, self.program_environment(), |ty| wrap(*ty))
                } else {
                    wrap(inferred_ty)
                };
                if let Some(wrapped) = wrapped {
                    inferred_ty = wrapped;
                    continue;
                }
            }
            if let Type::KnownInstance(KnownInstanceType::Deprecated(deprecated)) = decorator_ty {
                match inferred_ty {
                    Type::FunctionLiteral(function) => {
                        inferred_ty =
                            Type::FunctionLiteral(function.with_deprecated(db, *deprecated));
                        continue;
                    }
                    Type::Callable(callable) => {
                        inferred_ty = Type::Callable(callable.with_deprecated(
                            db,
                            overload_literal.with_deprecated(db, *deprecated),
                        ));
                        continue;
                    }
                    _ => {}
                }
            }
            let decorated_ty = inferred_ty;
            inferred_ty = self.apply_decorator(
                *decorator_ty,
                inferred_ty,
                decorator_node,
                (!is_decorated_overload_implementation).then_some(function),
            );
            if let Type::PropertyInstance(property) = inferred_ty {
                inferred_ty = Type::PropertyInstance(property.with_accessor_definition(
                    db,
                    *decorator_ty,
                    decorated_ty,
                    definition,
                ));
            }
        }

        if is_decorated_overload_implementation {
            // Overloads describe the exposed function. The implementation check compares the
            // unbound callable, before classmethod or staticmethod descriptor binding.
            let unwrap_method = |ty| match ty {
                Type::KnownInstance(KnownInstanceType::MethodWrapper(wrapper)) => {
                    wrapper.wrapped(db)
                }
                _ => ty,
            };
            let implementation_ty = match inferred_ty {
                Type::Union(union) => {
                    union.map(db, self.program_environment(), |ty| unwrap_method(*ty))
                }
                _ => unwrap_method(inferred_ty),
            };
            let last_definition = match inferred_ty {
                Type::FunctionLiteral(function) => Some(function.literal(db).last_definition),
                Type::Callable(callable) => callable.deprecated(db),
                _ => None,
            };
            let function_type = if let Some(last_definition) = last_definition {
                FunctionType::new(
                    db,
                    function_literal.with_last_definition_metadata(db, last_definition),
                    None,
                )
            } else {
                function_type
            };
            let implementation_callables = implementation_ty
                .try_upcast_to_callable(db, self.program_environment())
                .map_or_else(Box::default, |callables| {
                    callables.iter().copied().collect()
                });
            inferred_ty = Type::FunctionLiteral(
                function_type.with_implementation_callables(db, implementation_callables),
            );
        } else if is_decorated_overload && let Type::FunctionLiteral(function) = inferred_ty {
            inferred_ty = Type::FunctionLiteral(FunctionType::new(
                db,
                function_literal
                    .with_last_definition_metadata(db, function.literal(db).last_definition),
                None,
            ));
        }

        self.add_declaration_with_binding(
            function.into(),
            definition,
            &DeclaredAndInferredType::are_the_same_type(inferred_ty),
        );

        if function_decorators.contains(FunctionDecorators::OVERLOAD) {
            for stmt in &function.body {
                match stmt {
                    ast::Stmt::Pass(_) => continue,
                    ast::Stmt::Expr(ast::StmtExpr { value, .. }) => {
                        if matches!(
                            &**value,
                            ast::Expr::StringLiteral(_) | ast::Expr::EllipsisLiteral(_)
                        ) {
                            continue;
                        }
                    }
                    _ => {}
                }
                let Some(builder) = self.context.report_lint(&USELESS_OVERLOAD_BODY, stmt) else {
                    continue;
                };
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "Useless body for `@overload`-decorated function `{}`",
                    function.name
                ));
                diagnostic.set_primary_annotation_message("This statement will never be executed");
                diagnostic.info(
                    "`@overload`-decorated functions are solely for type checkers \
                    and must be overwritten at runtime by a non-`@overload`-decorated implementation",
                );
                diagnostic.help("Consider replacing this function body with `...` or `pass`");
                break;
            }
        }
    }

    pub(super) fn extend_function_deferred(
        &mut self,
        definition: Definition<'db>,
        function: &ast::StmtFunctionDef,
    ) {
        let db = self.db();
        if function_has_deferred_annotations(function) {
            self.extend_definition(definition, infer_deferred_types(db, definition));
        }
        if parameters_have_defaults(&function.parameters) {
            self.extend_definition(definition, infer_function_default_types(db, definition));
        }
    }

    pub(super) fn infer_function_annotations(
        &mut self,
        definition: Definition<'db>,
        function: &ast::StmtFunctionDef,
    ) {
        // PEP 695 annotations are inferred in the function's type-parameter scope.
        if !function_has_deferred_annotations(function) {
            return;
        }

        self.suppress_errors_for_no_type_check(definition, function);
        let previous_typevar_binding_context = self.typevar_binding_context.replace(definition);
        self.infer_function_signature_annotations(function, definition);
        self.typevar_binding_context = previous_typevar_binding_context;
    }

    pub(super) fn infer_function_defaults(
        &mut self,
        definition: Definition<'db>,
        function: &ast::StmtFunctionDef,
    ) {
        let db = self.db();
        if !parameters_have_defaults(&function.parameters) {
            return;
        }

        self.suppress_errors_for_no_type_check(definition, function);
        let previous_typevar_binding_context = self.typevar_binding_context.replace(definition);

        // In stub files, default values may reference names that are defined later in the file.
        let previous_deferred_state = self.replace_deferred_state(self.in_stub().into());

        // Borrow annotation types from their own inference result instead of copying that result
        // into this query. Scope inference merges both regions when checking the whole function.
        for param_with_default in function.parameters.iter_non_variadic_params() {
            let Some(default) = param_with_default.default() else {
                continue;
            };
            let annotation = param_with_default
                .annotation()
                .map(|annotation| function_signature_expression_type(db, definition, annotation));
            self.infer_expression(default, TypeContext::declared(annotation));
        }

        self.deferred_state = previous_deferred_state;
        self.typevar_binding_context = previous_typevar_binding_context;
    }

    fn suppress_errors_for_no_type_check(
        &mut self,
        definition: Definition<'db>,
        function: &ast::StmtFunctionDef,
    ) {
        if !function.decorator_list.is_empty()
            && function_known_decorator_flags(self.db(), definition)
                .contains(FunctionDecorators::NO_TYPE_CHECK)
        {
            // Decorator expressions and their diagnostics belong to their own inference query.
            // Signature and default inference only need to know whether errors are suppressed.
            self.context.inference_flags |= InferenceFlags::IN_NO_TYPE_CHECK;
        }
    }

    /// basedpython: infer the `raises` clause's type expression.
    ///
    /// `raises ...` is the gradual exception set rather than a type expression,
    /// so the ellipsis is inferred as the plain value it is.
    fn infer_raises_clause(&mut self, function: &ast::StmtFunctionDef) {
        let Some(raises) = function.raises.as_deref() else {
            return;
        };

        if raises.is_ellipsis_literal_expr() {
            self.infer_expression(raises, TypeContext::default());
            return;
        }

        self.infer_type_expression_with_state(
            raises,
            DeferredExpressionState::from(self.defer_annotations()),
        );
    }

    fn infer_return_type_annotation(&mut self, function: &ast::StmtFunctionDef) {
        let Some(returns) = function.returns.as_deref() else {
            return;
        };

        // basedpython: `-> asserts x` names the place a call narrows, so the annotation
        // is not a type expression. the name is resolved against the callee's parameters
        // (or the calling scope) at each call site, not here
        if function.is_asserts_return {
            self.infer_asserts_return_annotation(function, returns);
            return;
        }

        self.context.inference_flags |= InferenceFlags::IN_RETURN_TYPE;
        self.infer_type_expression_with_state(
            returns,
            DeferredExpressionState::from(self.defer_annotations()),
        );
        self.context
            .inference_flags
            .remove(InferenceFlags::IN_RETURN_TYPE);
    }

    /// Infer what an assertion guard's annotation does contain — the asserted type of
    /// `-> asserts x is T` — and report one that doesn't name a place.
    fn infer_asserts_return_annotation(
        &mut self,
        function: &ast::StmtFunctionDef,
        returns: &ast::Expr,
    ) {
        let Some(guards) = return_guards(function) else {
            if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, returns) {
                builder.into_diagnostic(
                    "`asserts` must name a place, optionally negated with `not` \
                     or tested against a type with `is`",
                );
            }
            return;
        };

        for guard in guards {
            if let ReturnGuardForm::AssertsType { ty, .. } = guard.form {
                self.infer_type_expression_with_state(
                    ty,
                    DeferredExpressionState::from(self.defer_annotations()),
                );
            }
        }
    }

    pub(super) fn infer_function_type_params(&mut self, function: &ast::StmtFunctionDef) {
        let binding_context = self.index.expect_single_definition(function);
        let previous_typevar_binding_context =
            self.typevar_binding_context.replace(binding_context);
        self.infer_function_signature_annotations(function, binding_context);
        self.typevar_binding_context = previous_typevar_binding_context;
    }

    /// Infer an annotated method receiver before the rest of its signature so `Self` can be
    /// validated where it occurs, including inside parsed string annotations.
    ///
    /// ```python
    /// class Example:
    ///     def method(self: object) -> "Self | object": ...
    /// ```
    fn infer_function_signature_annotations(
        &mut self,
        function: &ast::StmtFunctionDef,
        definition: Definition<'db>,
    ) {
        let receiver_is_incompatible = self.infer_method_receiver_annotation(function, definition);
        let previous_incompatible_receiver = self.context.inference_flags.replace(
            InferenceFlags::HAS_INCOMPATIBLE_SELF_RECEIVER,
            receiver_is_incompatible == Some(true),
        );

        self.infer_return_type_annotation(function);
        self.infer_raises_clause(function);
        if let Some(type_params) = function.type_params.as_deref() {
            // basedpython: a `type def` is not a runtime function — the transpiler erases the
            // declaration, so there is no closure for the specialization step to rebuild
            let reification = if ast::helpers::is_type_def(function) {
                TypeParamReification::TypeDef
            } else {
                TypeParamReification::Function
            };
            self.infer_type_parameters(type_params, reification);
        }
        self.infer_parameters(&function.parameters, receiver_is_incompatible.is_some());

        self.context.inference_flags.set(
            InferenceFlags::HAS_INCOMPATIBLE_SELF_RECEIVER,
            previous_incompatible_receiver,
        );
    }

    /// Infers an explicitly annotated method receiver before the rest of its signature.
    ///
    /// Returns whether the annotation is incompatible with `Self`, or `None` for functions without
    /// an annotated instance or class receiver.
    fn infer_method_receiver_annotation(
        &mut self,
        function: &ast::StmtFunctionDef,
        definition: Definition<'db>,
    ) -> Option<bool> {
        let receiver = function
            .parameters
            .posonlyargs
            .first()
            .or_else(|| function.parameters.args.first())?;
        let annotation = receiver.parameter.annotation.as_deref()?;
        let receiver_kind = MethodReceiverKind::from_function(self.db(), definition, function)?;

        let previously_in_parameter_annotation = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_PARAMETER_ANNOTATION, true);
        let previously_in_init_receiver = self.context.inference_flags.replace(
            InferenceFlags::IN_INIT_RECEIVER_ANNOTATION,
            function.name.id == "__init__" && receiver_kind == MethodReceiverKind::Instance,
        );
        let annotation_type = self.infer_type_expression_with_state(
            annotation,
            DeferredExpressionState::from(self.defer_annotations()),
        );
        self.context.inference_flags.set(
            InferenceFlags::IN_PARAMETER_ANNOTATION,
            previously_in_parameter_annotation,
        );

        self.context.inference_flags.set(
            InferenceFlags::IN_INIT_RECEIVER_ANNOTATION,
            previously_in_init_receiver,
        );

        Some(!receiver_kind.accepts_annotation(self.db(), annotation_type))
    }

    fn infer_parameters(
        &mut self,
        parameters: &ast::Parameters,
        first_annotation_already_inferred: bool,
    ) {
        let ast::Parameters {
            range: _,
            node_index: _,
            posonlyargs: _,
            args: _,
            vararg,
            kwonlyargs: _,
            kwarg,
        } = parameters;

        self.context.inference_flags |= InferenceFlags::IN_PARAMETER_ANNOTATION;
        for param_with_default in parameters
            .iter_non_variadic_params()
            .skip(usize::from(first_annotation_already_inferred))
        {
            self.infer_parameter_with_default(param_with_default);
        }
        if let Some(vararg) = vararg {
            self.context.inference_flags |= InferenceFlags::IN_VARARG_ANNOTATION;
            self.infer_parameter(vararg);
            self.context
                .inference_flags
                .remove(InferenceFlags::IN_VARARG_ANNOTATION);
        }
        if let Some(kwarg) = kwarg {
            self.context.inference_flags |= InferenceFlags::IN_KWARG_ANNOTATION;
            self.infer_parameter(kwarg);
            self.context
                .inference_flags
                .remove(InferenceFlags::IN_KWARG_ANNOTATION);
        }
        self.context
            .inference_flags
            .remove(InferenceFlags::IN_PARAMETER_ANNOTATION);
    }

    /// Whether the type variable annotating `**kwargs` also annotates another parameter.
    ///
    /// Mirrors the deferral rule in `Parameters::from_annotation`: an unpacked type variable can
    /// only be solved from the keyword arguments when nothing else pins it down first.
    fn typevar_used_by_another_parameter(
        &mut self,
        annotated_type: Type<'db>,
        parameters: &ast::Parameters,
        kwargs_annotation: &ast::Expr,
    ) -> bool {
        let env = self.program_environment();
        let Type::TypeVar(typevar) = annotated_type else {
            return false;
        };
        let identity = typevar.identity(self.db());
        parameters
            .iter()
            .filter_map(ruff_python_ast::AnyParameterRef::annotation)
            .filter(|annotation| !std::ptr::eq(*annotation, kwargs_annotation))
            .any(|annotation| {
                any_over_type(self.db(), env, self.file_expression_type(annotation), false, |ty| {
                    matches!(ty, Type::TypeVar(other) if other.identity(self.db()) == identity)
                })
            })
    }

    fn validate_unpacked_typed_dict_kwargs(&mut self, parameters: &ast::Parameters) {
        let db = self.db();
        let env = self.program_environment();
        let Some(kwargs) = parameters.kwarg.as_ref() else {
            return;
        };
        let Some(annotation) = kwargs.annotation.as_deref() else {
            return;
        };
        let annotation_flags = self.file_type_expression_flags(annotation);
        if !annotation_flags.contains(TypeExpressionFlags::UNPACK) {
            return;
        }

        let annotated_type = self.file_expression_type(annotation);
        // basedpython: `**kwargs: **Kwargs` unpacks a keyword-variadic pack and `**kwargs: **P`
        // a `ParamSpec`'s keyword half, neither of which is a `TypedDict`
        if matches!(
            annotated_type,
            Type::TypeVar(typevar) if typevar.is_parameter_pack(self.db())
        ) {
            return;
        }
        // A type variable bounded by `TypedDict` will be solved to one, from these very keyword
        // arguments; there are no keys to check against the other parameters until then. This
        // only works while the keywords are the sole source of the type variable -- if another
        // parameter also mentions it, fall through and report the unpacked value as invalid.
        if annotated_type.is_typed_dict_bounded_typevar(self.db(), env)
            && !self.typevar_used_by_another_parameter(annotated_type, parameters, annotation)
        {
            return;
        }
        let Some(unpacked_keys) = extract_unpacked_typed_dict_keys_from_kwargs_annotation(
            db,
            annotated_type,
            annotation_flags,
        ) else {
            if !annotated_type.is_unknown()
                && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, annotation)
            {
                let diag = builder.into_diagnostic(format_args!(
                    "Unpacked value for `**kwargs` must be a TypedDict, not `{}`",
                    annotated_type.display(db, env)
                ));
                add_type_expression_reference_link(diag);
            }
            return;
        };

        // Legacy PEP 484 positional-only parameters like `def f(__x: int, **kwargs:
        // Unpack[TD])` are not callable by keyword, so they do not overlap with keys
        // accepted through `**kwargs`. The convention only applies to the leading
        // positional-or-keyword parameters that are actually converted to positional-only
        // parameters by `Parameters::from_parameters`.
        let pep_484_positional_only_count = if parameters.posonlyargs.is_empty() {
            parameters
                .args
                .iter()
                .take_while(|parameter| parameter.uses_pep_484_positional_only_convention())
                .count()
        } else {
            0
        };

        let overlapping = parameters
            .iter_non_variadic_params()
            .skip(parameters.posonlyargs.len() + pep_484_positional_only_count)
            .map(|parameter| &parameter.parameter)
            .filter(|parameter| unpacked_keys.contains_key(&parameter.name.id))
            .collect::<Vec<_>>();

        if overlapping.is_empty() {
            return;
        }

        let overlapping_names = overlapping
            .iter()
            .map(|parameter| format!("`{}`", parameter.name.id))
            .collect::<Vec<_>>()
            .join(", ");

        if let Some(builder) = self
            .context
            .report_lint(&INVALID_TYPE_FORM, kwargs.as_ref())
        {
            if overlapping.len() == 1 {
                builder.into_diagnostic(format_args!(
                    "Parameter {overlapping_names} overlaps with unpacked TypedDict key in \
                     `**kwargs` annotation",
                ));
            } else {
                builder.into_diagnostic(format_args!(
                    "Parameters {overlapping_names} overlap with unpacked TypedDict keys in \
                     `**kwargs` annotation",
                ));
            }
        }
    }

    fn infer_parameter_with_default(&mut self, parameter_with_default: &ast::ParameterWithDefault) {
        let ast::ParameterWithDefault {
            range: _,
            node_index: _,
            parameter,
            default: _,
        } = parameter_with_default;

        if let Some(annotation) = parameter.annotation.as_deref() {
            self.infer_type_expression_with_state(
                annotation,
                DeferredExpressionState::from(self.defer_annotations()),
            );
        }
    }

    fn infer_parameter(&mut self, parameter: &ast::Parameter) {
        let ast::Parameter {
            range: _,
            node_index: _,
            name: _,
            // the pattern of a destructuring parameter binds in the function's
            // body scope, so it is inferred there rather than here
            pattern: _,
            annotation,
            is_context: _,
            is_some: _,
        } = parameter;

        if let Some(annotation) = annotation.as_deref() {
            self.infer_type_expression_with_state(
                annotation,
                DeferredExpressionState::from(self.defer_annotations()),
            );
        }
    }

    /// Set initial declared type (if annotated) and inferred type for a function-parameter symbol,
    /// in the function body scope.
    ///
    /// The declared type is the annotated type, if any, or `Unknown`.
    ///
    /// The inferred type is the annotated type, if any. If there is no annotation, it is the union
    /// of `Unknown` and the type of the default value, if any.
    ///
    /// Parameter definitions are odd in that they define a symbol in the function-body scope, so
    /// the Definition belongs to the function body scope, but the expressions (annotation and
    /// default value) both belong to outer scopes. (The default value always belongs to the outer
    /// scope in which the function is defined, the annotation belongs either to the outer scope,
    /// or maybe to an intervening type-params scope, if it's a generic function.) So we don't use
    /// `self.infer_expression` or store any expression types here, we just query for the types of
    /// the expressions from their respective scopes.
    ///
    /// It is safe (non-cycle-causing) to query the annotation type via `file_expression_type`
    /// here, because an outer scope can't depend on a definition from an inner scope, so we
    /// shouldn't be in-process of inferring the outer scope here.
    pub(super) fn infer_parameter_definition(
        &mut self,
        parameter_with_default: &'ast ast::ParameterWithDefault,
        definition: Definition<'db>,
    ) {
        let env = self.program_environment();
        let ast::ParameterWithDefault {
            parameter,
            default,
            range: _,
            node_index: _,
        } = parameter_with_default;

        let db = self.db();

        let default_expr = default.as_ref();
        if let Some(annotation) = parameter.annotation.as_ref() {
            // `Overlapping[Key]` is a call-binder-only marker; inside the body the
            // parameter is seen as `Key`'s upper bound so it can never be written
            // back into `Key`-typed covariant storage
            let declared_ty = self
                .file_expression_type(annotation)
                .erase_overlapping(db, env);

            // P.args and P.kwargs are only valid as annotations on *args and **kwargs,
            // not on regular parameters. basedpython has no source spelling for them at all,
            // and says so once where the type expression is resolved
            if !self.is_basedpython_file()
                && let Type::TypeVar(typevar) = declared_ty
                && typevar.is_paramspec(db)
                && let Some(attr) = typevar.paramspec_attr(db)
            {
                let name = typevar.name(db);
                let (attr_name, variadic) = match attr {
                    ParamSpecAttrKind::Args => ("args", "*args"),
                    ParamSpecAttrKind::Kwargs => ("kwargs", "**kwargs"),
                };
                if let Some(builder) = self
                    .context
                    .report_lint(&INVALID_PARAMSPEC, annotation.as_ref())
                {
                    builder.into_diagnostic(format_args!(
                        "`{name}.{attr_name}` is only valid for annotating `{variadic}`",
                    ));
                }
            }

            if let Some(default_expr) = default_expr {
                let default_expr = default_expr.as_ref();
                let default_ty = self.file_expression_type(default_expr);

                report_bool_as_int(&self.context, default_expr, default_ty, declared_ty);

                // Avoid duplicate diagnostics: invalid TypedDict literals already emit specific errors.
                let suppress_invalid_default =
                    is_invalid_typed_dict_literal(db, env, declared_ty, default_expr.into());
                // basedpython: a default that fits only some specializations of the annotation's
                // type variables initialises them, and each call that leaves the argument out
                // is checked against the specialization it settles on
                if !default_ty.is_assignable_to(db, env, declared_ty)
                    && !suppress_invalid_default
                    && default_fit(db, definition) == DefaultFit::Never
                {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_PARAMETER_DEFAULT, parameter_with_default)
                    {
                        builder.into_diagnostic(format_args!(
                            "Default value of type `{}` is not assignable \
                             to annotated parameter type `{}`",
                            default_ty.display(db, env),
                            declared_ty.display(db, env)
                        ));
                    }
                }
            }

            // basedpython: an override that leaves the default out takes the overridden method's,
            // which was written against the base's annotation rather than this one
            if default_expr.is_none()
                && self.is_basedpython_file()
                && let Some(inherited) = self.inherited_parameter_default(parameter)
                && relate_default(db, env, definition, declared_ty, inherited) == DefaultFit::Never
                && let Some(builder) = self
                    .context
                    .report_lint(&INVALID_PARAMETER_DEFAULT, parameter_with_default)
            {
                builder.into_diagnostic(format_args!(
                    "Inherited default value of type `{}` is not assignable \
                     to annotated parameter type `{}`",
                    inherited.display(db, env),
                    declared_ty.display(db, env)
                ));
            }

            self.add_parameter(parameter.into(), definition, declared_ty);
        } else if let Some(function) = self.enclosing_trailing_lambda() {
            // basedpython: the implicit `it` parameter of a trailing lambda
            // block is context-typed from the called expression — the sole
            // positional parameter of the callable its last parameter is
            // declared as. the self/cls and overload-inheritance special
            // cases below must never apply to the synthetic parameter
            let ty = self
                .trailing_lambda_it_parameter_type(function)
                .unwrap_or_else(Type::unknown);
            self.add_binding(parameter.into(), definition)
                .insert(self, ty);
        } else {
            // basedpython: an unannotated parameter on an overload
            // implementation inherits its type from the union of the matching
            // overload parameter types. when the impl also supplies a default
            // value, the default is folded into the inherited base only if it
            // doesn't already fit
            let inherited = self.inherited_parameter_type(parameter);
            // basedpython: under `sound-types` an unannotated parameter is an anonymous type
            // parameter. it is the last thing tried, so pytest's fixture injection and the
            // implicit `self` still win, and it is read back off the signature rather than
            // rebuilt here so that the body and its call sites cannot disagree
            let hole = self.inferred_parameter_hole(parameter);
            let ty = if let Some(default_expr) = default_expr {
                let default_ty = self.file_expression_type(default_expr);
                if let Some(base) = inherited {
                    if default_ty.is_assignable_to(db, env, base) {
                        base
                    } else {
                        UnionType::from_two_elements(db, env, base, default_ty)
                    }
                } else if let Some(hole) = hole {
                    hole
                } else {
                    UnionType::from_two_elements(db, env, Type::unknown(), default_ty)
                }
            } else if let Some(ty) = self.special_first_method_parameter_type(parameter) {
                ty
            } else if let Some(ty) = inherited {
                ty
            } else if let Some(ty) = self.pytest_fixture_parameter_type(parameter) {
                ty
            } else if let Some(hole) = hole {
                hole
            } else {
                Type::unknown()
            };

            self.add_binding(parameter.into(), definition)
                .insert(self, ty);
        }
    }

    /// basedpython: for an unannotated parameter of a pytest test or fixture,
    /// the type of the fixture pytest binds it to by name. an annotated
    /// parameter is instead *checked* against that fixture, by
    /// [`check_pytest_function`].
    ///
    /// [`check_pytest_function`]: Self::check_pytest_function
    fn pytest_fixture_parameter_type(&self, parameter: &ast::Parameter) -> Option<Type<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let function = nearest_enclosing_function(db, self.index, self.scope())?;
        pytest::injected_parameter_type(db, env, function, parameter.name.as_str())
    }

    /// basedpython: for an unannotated parameter, look up the type that
    /// [`OverloadLiteral::parameter_signature`] filled in for it — from the sibling overloads when
    /// this is an overload implementation, or (under `sound-types`) from the overridden base
    /// method. matching is by name
    fn inherited_parameter_type(&self, parameter: &ast::Parameter) -> Option<Type<'db>> {
        let ty = self.signature_parameter_type(parameter)?;
        // an anonymous hole is not inherited from anywhere; it is offered separately, after
        // everything that should outrank it
        (!ty.is_inferred_parameter_hole(self.db())).then_some(ty)
    }

    /// basedpython: for an unannotated parameter under `sound-types`, the anonymous type
    /// parameter [`open_unannotated_parameter_holes`] opened for it.
    ///
    /// [`open_unannotated_parameter_holes`]: crate::types::signatures::Signature::open_unannotated_parameter_holes
    fn inferred_parameter_hole(&self, parameter: &ast::Parameter) -> Option<Type<'db>> {
        let ty = self.signature_parameter_type(parameter)?;
        ty.is_inferred_parameter_hole(self.db()).then_some(ty)
    }

    /// basedpython: the default the enclosing override takes for `parameter` from the method it
    /// overrides, when it writes none of its own
    fn inherited_parameter_default(&self, parameter: &ast::Parameter) -> Option<Type<'db>> {
        let db = self.db();
        let enclosing = nearest_enclosing_function(db, self.index, self.scope())?;
        enclosing
            .last_definition_parameter_signature(db, ReturnCallableTypeVarScope::Public)
            .parameters()
            .iter()
            .find(|candidate| candidate.name() == Some(&parameter.name.id))?
            .eager_default_type()
    }

    /// The type the enclosing function's signature gives `parameter`, when that type was not
    /// written as an annotation.
    ///
    /// A parameter that received no type at all still has its implicit `Unknown`, which
    /// `should_annotation_be_displayed` reports as not displayable, so this returns `None` for it.
    fn signature_parameter_type(&self, parameter: &ast::Parameter) -> Option<Type<'db>> {
        let db = self.db();
        let enclosing = nearest_enclosing_function(db, self.index, self.scope())?;
        // not the raw signature: that one's return type can be read off this very body
        let signature =
            enclosing.last_definition_parameter_signature(db, ReturnCallableTypeVarScope::Public);
        let matched = signature
            .parameters()
            .iter()
            .find(|p| p.name() == Some(&parameter.name.id))?;
        if matched.should_annotation_be_displayed() {
            Some(matched.annotated_type())
        } else {
            None
        }
    }

    /// Set initial declared/inferred types for a `*args` variadic positional parameter.
    ///
    /// The annotated type is implicitly wrapped in a homogeneous tuple.
    ///
    /// See [`infer_parameter_definition`] doc comment for some relevant observations about scopes.
    ///
    /// [`infer_parameter_definition`]: Self::infer_parameter_definition
    pub(super) fn infer_variadic_positional_parameter_definition(
        &mut self,
        parameter: &'ast ast::Parameter,
        definition: Definition<'db>,
    ) {
        let db = self.db();

        if let Some(annotation) = parameter.annotation() {
            let annotated_type = self.file_expression_type(annotation);
            let has_unpacked_annotation = self
                .file_type_expression_flags(annotation)
                .contains(TypeExpressionFlags::UNPACK);
            let ty = match annotated_type {
                Type::TypeVar(typevar)
                    if has_unpacked_annotation && typevar.is_typevartuple(db) =>
                {
                    Type::tuple(TupleType::new(
                        db,
                        self.program_environment(),
                        &TupleSpecBuilder::with_capacity(0)
                            .concat_variadic_typevar(db, self.program_environment(), typevar)
                            .build(),
                    ))
                }
                _ if has_unpacked_annotation => annotated_type,
                Type::TypeVar(typevar) if typevar.is_paramspec(db) => {
                    match typevar.paramspec_attr(db) {
                        // `*args: P.args`
                        Some(ParamSpecAttrKind::Args) => annotated_type,

                        // `*args: P.kwargs`
                        Some(ParamSpecAttrKind::Kwargs) => {
                            // TODO: Should this diagnostic be raised as part of
                            // `ArgumentTypeChecker`?
                            // basedpython says everything there is to say about the spelling
                            // where the type expression is resolved
                            if !self.is_basedpython_file()
                                && let Some(builder) =
                                    self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                            {
                                let name = typevar.name(db);
                                let mut diag = builder.into_diagnostic(format_args!(
                                    "`{name}.kwargs` is valid only in `**kwargs` annotation",
                                ));
                                diag.set_primary_annotation_message(format_args!(
                                    "Did you mean `{name}.args`?"
                                ));
                                add_type_expression_reference_link(diag);
                            }
                            Type::homogeneous_tuple(db, self.program_environment(), Type::unknown())
                        }

                        // `*args: P`
                        None => {
                            // The diagnostic for this case is handled in `in_type_expression`.
                            Type::homogeneous_tuple(db, self.program_environment(), Type::unknown())
                        }
                    }
                }
                _ => Type::homogeneous_tuple(db, self.program_environment(), annotated_type),
            };

            self.add_parameter(parameter.into(), definition, ty);
        } else {
            let inferred_ty =
                Type::homogeneous_tuple(db, self.program_environment(), Type::unknown());
            self.add_binding(parameter.into(), definition)
                .insert(self, inferred_ty);
        }
    }

    /// basedpython: the enclosing function when the current scope is a
    /// trailing lambda block's body
    fn enclosing_trailing_lambda(&self) -> Option<&'ast ast::StmtFunctionDef> {
        let function_scope = self.scope().scope(self.db());
        let function = function_scope.node().as_function()?.node(self.module());
        function.is_trailing_lambda.then_some(function)
    }

    /// basedpython: the callee a trailing lambda block is attached to, with what
    /// the block's call solves for it — read from the standalone-expression
    /// inferences the semantic index builder registers for the callee and the
    /// written arguments (independent of the enclosing definition's inference,
    /// so no cycle)
    fn trailing_lambda_block_callee(
        &self,
        function: &ast::StmtFunctionDef,
    ) -> Option<BlockCallee<'db>> {
        block_callee(self.db(), self.index, function)
    }

    /// basedpython: the type of a trailing lambda's implicit `it` parameter.
    /// `None` when the callee's signature or its last parameter's callable shape
    /// is not inspectable
    fn trailing_lambda_it_parameter_type(
        &self,
        function: &ast::StmtFunctionDef,
    ) -> Option<Type<'db>> {
        trailing_lambda_it_type(self.db(), self.trailing_lambda_block_callee(function)?)
    }

    /// basedpython: the borrow a trailing-lambda block's implicit `it` inherits
    /// from the callee's callback signature — the `local` of
    /// `def f(fn: (local int) -> None)`. The block body implements that callback,
    /// so the value bound to `it` may not escape it.
    ///
    /// `None` for an ordinary function, for a block whose callee declares no
    /// modifier, and for one whose callee is not inspectable — an opaque callee
    /// leaves the block unconstrained, as everywhere else in the borrow analysis.
    fn trailing_lambda_inherited_borrow(
        &self,
        function: &'ast ast::StmtFunctionDef,
    ) -> Option<InheritedBorrow<'db, 'db, 'ast>> {
        if !function.is_trailing_lambda {
            return None;
        }
        let db = self.db();
        let callee = function.trailing_lambda_callee()?;
        let borrow = trailing_lambda_it_borrow(db, self.trailing_lambda_block_callee(function)?);
        if !borrow.is_borrow() {
            return None;
        }
        // the block's `it` parameter is synthetic and zero-width, so the
        // diagnostics point at the callee, which is where the modifier is
        // visible from
        Some(InheritedBorrow {
            name: function.parameters.args.first()?.parameter.name.as_str(),
            borrow,
            declaration: callee.range(),
            index: self.index,
            block_scope: self.scope().file_scope_id(db),
        })
    }

    /// basedpython: whether this trailing-lambda block's callee marks its callback
    /// parameter `once` — the block then runs exactly once (`with`-like). Anything
    /// unresolvable is treated as not-`once` (the restricted default).
    fn trailing_lambda_callee_is_once(&self, function: &ast::StmtFunctionDef) -> bool {
        self.trailing_lambda_block_callee(function)
            .is_some_and(|callee| {
                crate::types::trailing_lambda::callee_callback_is_once(self.db(), callee.ty)
            })
    }

    /// basedpython: whether `attribute` resolves through an implicit receiver —
    /// `x.fn` where `fn` names a receiver callable in scope rather than a member
    /// of `x`, which the transpiler lowers to `fn(x)`
    fn is_implicit_receiver_attribute(&self, attribute: &ast::ExprAttribute) -> bool {
        let env = self.program_environment();
        self.is_basedpython_file()
            && self
                .try_expression_type(&attribute.value)
                .is_some_and(|receiver_ty| {
                    crate::types::receivers::is_implicit_receiver_attribute(
                        self.db(),
                        env,
                        self.file(),
                        self.scope(),
                        attribute,
                        receiver_ty,
                    )
                })
    }

    /// basedpython: a trailing-lambda block binds one argument, as `it`, plus the
    /// receiver its callback declares. Report a callback that takes more than
    /// that: the extra arguments have no parameter to land in, and no spelling
    /// in the body.
    fn check_trailing_lambda_bindable_parameters(&self, function: &ast::StmtFunctionDef) {
        let db = self.db();
        let Some(callee) = function.trailing_lambda_callee() else {
            return;
        };
        let Some(callee_ty) = self.trailing_lambda_block_callee(function) else {
            return;
        };
        let Some(unbindable) =
            crate::types::trailing_lambda::trailing_lambda_unbindable_parameters(db, callee_ty)
        else {
            return;
        };
        let Some(builder) = self
            .context
            .report_lint(&TRAILING_LAMBDA_PARAMETERS, callee)
        else {
            return;
        };
        let message = match unbindable {
            UnbindableParameters::TooMany(count) => format!(
                "a trailing-lambda block binds one argument, but this callback takes {count}"
            ),
            UnbindableParameters::Variadic => "a trailing-lambda block binds one argument, so it \
                 cannot fill a callback with a variadic parameter"
                .to_owned(),
        };
        let mut diagnostic = builder.into_diagnostic(message);
        diagnostic.info("the block's suite takes the implicit parameter `it`");
    }

    /// basedpython: a trailing-lambda block lowers to a function returning `None`
    /// (in a `once` block a `return` targets the enclosing function, not the
    /// block), so its callback must be declared to return `None`. Report a
    /// callback with any other return type — those are not yet supported.
    fn check_trailing_lambda_callback_returns_none(&self, function: &ast::StmtFunctionDef) {
        let env = self.program_environment();
        let db = self.db();
        let Some(callee) = function.trailing_lambda_callee() else {
            return;
        };
        let Some(callee_ty) = self.trailing_lambda_block_callee(function) else {
            return;
        };
        let Some(return_ty) =
            crate::types::trailing_lambda::trailing_lambda_callback_return_type(db, callee_ty)
        else {
            return;
        };
        // a block whose body awaits lowers to an `async def`, so what it returns
        // is the coroutine that calling it produces
        let block_return = if function.is_async {
            KnownClass::CoroutineType.to_specialized_instance(
                db,
                env,
                &[Type::any(), Type::any(), Type::none(db, env)],
            )
        } else {
            Type::none(db, env)
        };
        // a declared return type that accepts what the block returns (`None`,
        // `int | None`, `object`, …) is satisfiable, anything else is not
        if block_return.is_assignable_to(db, env, return_ty) {
            return;
        }
        if let Some(builder) = self
            .context
            .report_lint(&TRAILING_LAMBDA_RETURN_TYPE, callee)
        {
            builder.into_diagnostic(format_args!(
                "a trailing-lambda callback must return `{}`, not `{}` \
                 (other return types are not yet supported)",
                block_return.display(db, env),
                return_ty.display(db, env)
            ));
        }
    }

    /// Special case for unannotated `cls` and `self` arguments to class methods and instance methods.
    fn special_first_method_parameter_type(
        &mut self,
        parameter: &ast::Parameter,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let file = self.program_file();

        let function_scope_id = self.scope();
        let function_scope = function_scope_id.scope(db);
        let function = function_scope.node().as_function()?;

        let parent_file_scope_id = function_scope.parent()?;
        let mut parent_scope_id = parent_file_scope_id.to_scope_id(db, file);

        // Skip type parameter scopes, if the method itself is generic.
        if parent_scope_id.is_annotation(db) {
            let parent_scope = parent_scope_id.scope(db);
            parent_scope_id = parent_scope.parent()?.to_scope_id(db, file);
        }

        // Return early if this is not a method inside a class.
        let class = parent_scope_id.scope(db).node().as_class()?;

        let method_definition = self.index.expect_single_definition(function);
        let DefinitionKind::Function(function_definition) = method_definition.kind(db) else {
            return None;
        };

        if function_definition
            .node(self.module())
            .parameters
            .index(parameter.name())
            .is_none_or(|index| index != 0)
        {
            return None;
        }

        let function_node = function_definition.node(self.module());
        let function_name = &function_node.name;

        let mut is_classmethod = is_implicit_classmethod(function_name);
        let inference = infer_definition_types(db, method_definition);
        for decorator in &function_node.decorator_list {
            let decorator_ty = inference.expression_type(&decorator.expression);
            if let Some(known_class) = decorator_ty
                .as_class_literal()
                .and_then(|class| class.known(db))
            {
                // `__new__` is implicitly a static method, so spelling the decorator out
                // changes nothing: its first parameter is still the class being constructed
                if known_class == KnownClass::Staticmethod && function_name != "__new__" {
                    return None;
                }

                // basedpython: a `static let` getter is called with the owning
                // class, so its first parameter is typed like a classmethod's
                is_classmethod |= matches!(
                    known_class,
                    KnownClass::Classmethod | KnownClass::ByStaticProperty
                );
            }
        }

        let class_definition = self.index.expect_single_definition(class);
        let class_literal = original_class_type(db, class_definition)?;

        // basedpython: an extension of something that is not a class extends nothing, and
        // `invalid-extension` has already said so, so its `self` is left gradual
        if let ClassLiteral::Static(static_literal) = class_literal
            && static_literal.is_extension(db)
        {
            extensions::body_view_class(db, static_literal)?;
        }

        let typing_self = typing_self(db, self.scope(), Some(method_definition), class_literal);
        let receiver_kind = if is_classmethod || function_name == "__new__" {
            MethodReceiverKind::Class
        } else {
            MethodReceiverKind::Instance
        };
        match receiver_kind {
            MethodReceiverKind::Class => typing_self.map(|typing_self| {
                SubclassOfType::from(db, env, SubclassOfInner::TypeVar(typing_self))
            }),
            MethodReceiverKind::Instance => typing_self.map(Type::TypeVar),
        }
    }

    /// Set initial declared/inferred types for a `**kwargs` keyword-variadic parameter.
    ///
    /// The annotated type is implicitly wrapped in a string-keyed dictionary.
    ///
    /// See [`infer_parameter_definition`] doc comment for some relevant observations about scopes.
    ///
    /// [`infer_parameter_definition`]: Self::infer_parameter_definition
    pub(super) fn infer_variadic_keyword_parameter_definition(
        &mut self,
        parameter: &'ast ast::Parameter,
        definition: Definition<'db>,
    ) {
        let env = self.program_environment();
        let db = self.db();

        if let Some(annotation) = parameter.annotation() {
            let annotated_type = self.file_expression_type(annotation);
            let ty = if let Type::TypeVar(typevar) = annotated_type
                && typevar.is_paramspec(db)
            {
                match typevar.paramspec_attr(db) {
                    // `**kwargs: P.args`
                    Some(ParamSpecAttrKind::Args) => {
                        // TODO: Should this diagnostic be raised as part of `ArgumentTypeChecker`?
                        // basedpython says everything there is to say about the spelling where
                        // the type expression is resolved
                        if !self.is_basedpython_file()
                            && let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, annotation)
                        {
                            let name = typevar.name(db);
                            let mut diag = builder.into_diagnostic(format_args!(
                                "`{name}.args` is valid only in `*args` annotation",
                            ));
                            diag.set_primary_annotation_message(format_args!(
                                "Did you mean `{name}.kwargs`?"
                            ));
                            add_type_expression_reference_link(diag);
                        }
                        KnownClass::Dict.to_specialized_instance(
                            db,
                            env,
                            &[KnownClass::Str.to_instance(db, env), Type::unknown()],
                        )
                    }

                    // `**kwargs: P.kwargs`
                    Some(ParamSpecAttrKind::Kwargs) => annotated_type,

                    // `**kwargs: P`
                    None => {
                        // The diagnostic for this case is handled in `in_type_expression`.
                        KnownClass::Dict.to_specialized_instance(
                            db,
                            env,
                            &[KnownClass::Str.to_instance(db, env), Type::unknown()],
                        )
                    }
                }
            } else if extract_unpacked_typed_dict_keys_from_kwargs_annotation(
                db,
                annotated_type,
                self.file_type_expression_flags(annotation),
            )
            .is_some()
            {
                annotated_type
            } else {
                KnownClass::Dict.to_specialized_instance(
                    db,
                    env,
                    &[KnownClass::Str.to_instance(db, env), annotated_type],
                )
            };
            self.add_parameter(parameter.into(), definition, ty);
        } else {
            let inferred_ty = KnownClass::Dict.to_specialized_instance(
                db,
                env,
                &[KnownClass::Str.to_instance(db, env), Type::unknown()],
            );

            self.add_binding(parameter.into(), definition)
                .insert(self, inferred_ty);
        }
    }

    /// bind a parameter to `ty`, which is also the declared type of the parameter's name when the
    /// parameter is a declaration
    ///
    /// basedpython: an annotated parameter is one unless its name is not its own. a parameter
    /// list that repeats `_` lowers each `_` to a parameter of its own name, so none of their
    /// annotations declares the `_` the body binds
    fn add_parameter(
        &mut self,
        node: AnyNodeRef<'ast>,
        definition: Definition<'db>,
        ty: Type<'db>,
    ) {
        if definition
            .kind(self.db())
            .category(self.context.in_stub(), self.module())
            .is_declaration()
        {
            self.add_declaration_with_binding(
                node,
                definition,
                &DeclaredAndInferredType::are_the_same_type(ty),
            );
        } else {
            self.add_binding(node, definition).insert(self, ty);
        }
    }

    /// Set initial declared type (if annotated) and inferred type for a lambda-parameter symbol,
    /// in the lambda body scope.
    pub(super) fn infer_lambda_parameter_definition(
        &mut self,
        index: u32,
        parameter_with_default: &'ast ast::ParameterWithDefault,
        lambda: &'ast ast::ExprLambda,
        definition: Definition<'db>,
    ) {
        let ast::ParameterWithDefault {
            parameter,
            default,
            range: _,
            node_index: _,
        } = parameter_with_default;

        let env = &self.program_environment();
        let _default_expr = default.as_ref();
        let ty = if let Some(parameter_type) = self.annotated_lambda_parameter_type(index, lambda) {
            parameter_type
        } else if let Some(default_expr) = default {
            let default_ty = self.file_expression_type(default_expr);
            if self.settings().sound_types {
                // basedpython: same rule as an unannotated function parameter with a default —
                // the parameter takes the default's promoted type instead of folding in `Unknown`
                default_ty.promote(self.db(), env)
            } else {
                UnionType::from_two_elements(
                    self.db(),
                    env,
                    Type::Dynamic(DynamicType::UnknownLambdaParameter),
                    default_ty,
                )
            }
        } else {
            Type::Dynamic(DynamicType::UnknownLambdaParameter)
        };

        // basedpython typed lambdas (`lambda (a: int) -> int: ...`) make the
        // parameter a `DeclarationAndBinding` because the annotation is set,
        // matching annotated function parameters
        self.add_parameter(parameter.into(), definition, ty);
    }

    /// Set initial declared/inferred types for a `*args` variadic positional parameter
    /// in a lambda expression.
    pub(super) fn infer_variadic_positional_lambda_parameter_definition(
        &mut self,
        index: u32,
        parameter: &'ast ast::Parameter,
        lambda: &'ast ast::ExprLambda,
        definition: Definition<'db>,
    ) {
        let db = self.db();
        // Note that this currently always returns `None` because we do not support `Unpack`
        // annotations for callable types.
        let ty = if let Some(parameter_type) = self.annotated_lambda_parameter_type(index, lambda) {
            parameter_type
        } else {
            Type::homogeneous_tuple(
                db,
                self.program_environment(),
                Type::Dynamic(DynamicType::UnknownLambdaParameter),
            )
        };
        self.add_parameter(parameter.into(), definition, ty);
    }

    /// Set initial declared/inferred types for a `**kwargs` keyword-variadic parameter
    /// in a lambda expression.
    pub(super) fn infer_variadic_keyword_lambda_parameter_definition(
        &mut self,
        parameter: &'ast ast::Parameter,
        definition: Definition<'db>,
    ) {
        let db = self.db();
        let env = self.program_environment();
        let inferred_ty = KnownClass::Dict.to_specialized_instance(
            db,
            env,
            &[
                KnownClass::Str.to_instance(db, env),
                Type::Dynamic(DynamicType::UnknownLambdaParameter),
            ],
        );

        self.add_parameter(parameter.into(), definition, inferred_ty);
    }

    /// Returns the annotated type of the lambda parameter at the given index in the provided
    /// lambda expression, based on a `Callable` type annotation, if present.
    fn annotated_lambda_parameter_type(
        &mut self,
        index: u32,
        lambda: &'ast ast::ExprLambda,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let enclosing_stmt = infer_statement_types(
            self.db(),
            self.index.enclosing_lambda_statement(lambda.into())?,
        );
        let callable = enclosing_stmt.expression_type(lambda).as_callable()?;
        let [signature] = callable.signatures(self.db()).overloads.as_slice() else {
            // TODO: If there are multiple applicable overloads, we could attempt multi-inference.
            return None;
        };

        let parameter_type = signature.parameters().as_slice()[index as usize].annotated_type();
        (!parameter_type.has_provisional_marker(db, self.program_environment()))
            .then_some(parameter_type)
    }
}

impl KnownFunction {
    /// Evaluate a call to this known function, and emit any diagnostics that are necessary
    /// as a result of the call.
    pub(super) fn check_call<'db>(
        self,
        builder: &TypeInferenceBuilder<'db, '_>,
        overload: &mut Binding<'db>,
        call_arguments: &CallArguments<'_, 'db>,
        call_expression: &ast::ExprCall,
    ) {
        let db = builder.db();
        let parameter_types = overload.parameter_types();

        match self {
            KnownFunction::RevealType => {
                let env = builder.program_environment();
                let revealed_type = overload
                    .arguments_for_parameter(call_arguments, 0)
                    .fold(UnionBuilder::new(db, env), |builder, (_, ty)| {
                        builder.add(ty)
                    })
                    .build();
                report_revealed_type(
                    &builder.context,
                    revealed_type,
                    revealed_declared_type(&builder.context, revealed_type, call_expression),
                    call_argument_node(call_expression, "obj", 0)
                        .unwrap_or_else(|| ast::AnyNodeRef::from(call_expression)),
                );
            }

            KnownFunction::HasMember => {
                let [Some(ty), Some(Type::LiteralValue(literal))] = parameter_types else {
                    return;
                };
                let Some(member) = literal.as_string() else {
                    return;
                };
                let env = builder.program_environment();
                let ty_members = all_members(db, env, *ty);
                overload.set_return_type(Type::bool_literal(
                    ty_members.iter().any(|m| m.name == member.value(db)),
                ));
            }

            KnownFunction::AssertType => {
                let [Some(actual_ty), Some(asserted_ty)] = parameter_types else {
                    return;
                };
                let env = builder.program_environment();
                let asserted_ty = asserted_ty.project_type_form(db, env);
                if actual_ty.is_equivalent_to(db, env, asserted_ty) {
                    return;
                }
                let diagnostic = if actual_ty.is_spellable(db, env)
                    || !actual_ty.is_subtype_of(db, env, asserted_ty)
                {
                    &TYPE_ASSERTION_FAILURE
                } else {
                    &ASSERT_TYPE_UNSPELLABLE_SUBTYPE
                };
                if let Some(diagnostic) = builder.context.report_lint(diagnostic, call_expression) {
                    let settings = DisplaySettings::from_possibly_ambiguous_types(
                        db,
                        env,
                        [*actual_ty, asserted_ty],
                    );
                    let mut diagnostic = diagnostic.into_diagnostic(format_args!(
                        "Argument does not have asserted type `{}`",
                        asserted_ty.display_with(db, env, settings.clone()),
                    ));

                    diagnostic.annotate(
                        Annotation::secondary(
                            builder.context.span(
                                call_argument_node(call_expression, "val", 0)
                                    .unwrap_or_else(|| ast::AnyNodeRef::from(call_expression)),
                            ),
                        )
                        .message(format_args!(
                            "Inferred type is `{}`",
                            actual_ty.display_with(db, env, settings.clone())
                        )),
                    );

                    if actual_ty.is_subtype_of(db, env, asserted_ty) {
                        diagnostic.info(format_args!(
                            "`{inferred_type}` is a subtype of `{asserted_type}`, but they are not equivalent",
                            asserted_type = asserted_ty.display_with(db, env, settings.clone()),
                            inferred_type = actual_ty.display_with(db, env, settings.clone()),
                        ));
                    } else {
                        diagnostic.info(format_args!(
                            "`{asserted_type}` and `{inferred_type}` are not equivalent types",
                            asserted_type = asserted_ty.display_with(db, env, settings.clone()),
                            inferred_type = actual_ty.display_with(db, env, settings.clone()),
                        ));
                    }

                    diagnostic.set_concise_message(format_args!(
                        "Type `{}` does not match asserted type `{}`",
                        actual_ty.display_with(db, env, settings.clone()),
                        asserted_ty.display_with(db, env, settings),
                    ));
                }
            }

            KnownFunction::AssertNever => {
                let [Some(actual_ty)] = parameter_types else {
                    return;
                };
                let env = builder.program_environment();
                if actual_ty.is_equivalent_to(db, env, Type::Never) {
                    return;
                }
                if let Some(diagnostic) = builder
                    .context
                    .report_lint(&TYPE_ASSERTION_FAILURE, call_expression)
                {
                    let mut diagnostic =
                        diagnostic.into_diagnostic("Argument does not have asserted type `Never`");
                    diagnostic.annotate(
                        Annotation::secondary(
                            builder.context.span(
                                call_argument_node(call_expression, "arg", 0)
                                    .unwrap_or_else(|| ast::AnyNodeRef::from(call_expression)),
                            ),
                        )
                        .message(format_args!(
                            "Inferred type of argument is `{}`",
                            actual_ty.display(db, env)
                        )),
                    );
                    diagnostic.info(format_args!(
                        "`Never` and `{inferred_type}` are not equivalent types",
                        inferred_type = actual_ty.display(db, env),
                    ));

                    diagnostic.set_concise_message(format_args!(
                        "Type `{}` is not equivalent to `Never`",
                        actual_ty.display(db, env),
                    ));
                }
            }

            KnownFunction::StaticAssert => {
                let [Some(parameter_ty), message] = parameter_types else {
                    return;
                };
                let env = builder.program_environment();
                let truthiness = match parameter_ty.try_bool(db, env) {
                    Ok(truthiness) => truthiness,
                    Err(err) => {
                        err.report_diagnostic(
                            &builder.context,
                            call_argument_node(call_expression, "condition", 0)
                                .unwrap_or_else(|| ast::AnyNodeRef::from(call_expression)),
                        );

                        return;
                    }
                };

                if let Some(diagnostic) = builder
                    .context
                    .report_lint(&STATIC_ASSERT_ERROR, call_expression)
                {
                    if truthiness.is_always_true() {
                        return;
                    }
                    let mut diagnostic = if let Some(message) = message
                        .and_then(Type::as_string_literal)
                        .map(|s| s.value(db))
                    {
                        diagnostic
                            .into_diagnostic(format_args!("Static assertion error: {message}"))
                    } else if *parameter_ty == Type::bool_literal(false) {
                        diagnostic.into_diagnostic(
                            "Static assertion error: argument evaluates to `False`",
                        )
                    } else if truthiness.is_always_false() {
                        diagnostic.into_diagnostic(format_args!(
                            "Static assertion error: argument of type `{parameter_ty}` \
                            is always falsy",
                            parameter_ty = parameter_ty.display(db, env)
                        ))
                    } else {
                        diagnostic.into_diagnostic(format_args!(
                            "Static assertion error: argument of type `{parameter_ty}` \
                            has an ambiguous static truthiness",
                            parameter_ty = parameter_ty.display(db, env)
                        ))
                    };
                    if let Some(condition) = call_argument_node(call_expression, "condition", 0) {
                        diagnostic.annotate(
                            Annotation::secondary(builder.context.span(condition)).message(
                                format_args!(
                                    "Inferred type of argument is `{}`",
                                    parameter_ty.display(db, env)
                                ),
                            ),
                        );
                    }
                }
            }

            KnownFunction::Cast => {
                let [Some(casted_type), Some(source_type)] = parameter_types else {
                    return;
                };
                let env = builder.context.program_environment();
                let casted_type = casted_type.project_type_form(db, env);
                if source_type.is_equivalent_to(db, env, casted_type)
                    && non_any_dynamic_content(db, env, *source_type).is_absent()
                    && non_any_dynamic_content(db, env, casted_type).is_absent()
                {
                    if let Some(diagnostic) = builder
                        .context
                        .report_lint(&REDUNDANT_CAST, call_expression)
                    {
                        let source_display = source_type.display(db, env).to_string();
                        let casted_display = casted_type.display(db, env).to_string();
                        let mut diagnostic = diagnostic.into_diagnostic(format_args!(
                            "Value is already of type `{casted_display}`",
                        ));
                        if source_display != casted_display {
                            diagnostic.info(format_args!(
                                "`{casted_display}` is equivalent to `{source_display}`",
                            ));
                        }
                        if let Some(value) = call_expression.arguments.find_argument_value("val", 1)
                        {
                            let source = source_text(db, builder.file());
                            let covering = covering_node(
                                builder.context.module().syntax().into(),
                                call_expression.range(),
                            );
                            let replacement = unwrapped_call_argument(
                                call_expression,
                                value,
                                covering.parent(),
                                builder.module().tokens(),
                                &source,
                            );
                            diagnostic.help("Remove the redundant `cast`");
                            diagnostic.set_fix(Fix::safe_edit(Edit::range_replacement(
                                replacement,
                                call_expression.range(),
                            )));
                        }
                    }
                } else if builder.context.is_lint_enabled(&DISJOINT_CAST)
                    && !builder.file().is_stub(db)
                    && !builder.index.is_in_type_checking_block(
                        builder.scope().file_scope_id(db),
                        call_expression.range(),
                    )
                    && source_type.is_disjoint_from(db, env, casted_type)
                    && !casted_type.is_equivalent_to(db, env, Type::Never)
                    && !source_type.is_equivalent_to(db, env, Type::Never)
                    && call_expression
                        .arguments
                        .find_argument_value("val", 1)
                        .is_none_or(|value_expr| {
                            builder
                                .speculate_without_diagnostics()
                                .infer_expression(
                                    value_expr,
                                    TypeContext::declared(Some(casted_type)),
                                )
                                .is_disjoint_from(db, env, casted_type)
                        })
                    && let Some(diagnostic) =
                        builder.context.report_lint(&DISJOINT_CAST, call_expression)
                {
                    let types = [*source_type, casted_type];
                    let settings = DisplaySettings::from_possibly_ambiguous_types(db, env, types);
                    let source_display = source_type.display_with(db, env, settings.clone());
                    let casted_display = casted_type.display_with(db, env, settings.clone());
                    let mut diagnostic = diagnostic.into_diagnostic("Cast to a disjoint type");
                    diagnostic.set_concise_message(format_args!(
                        "Cast from `{source_display}` to disjoint type `{casted_display}`",
                    ));
                    if let Some(arg) = call_expression.arguments.find_argument_value("typ", 0) {
                        diagnostic.annotate(
                            builder
                                .context
                                .secondary(arg)
                                .message("Disjoint from the inferred type"),
                        );
                    }
                    if let Some(arg) = call_expression.arguments.find_argument_value("val", 1) {
                        diagnostic.annotate(
                            builder
                                .context
                                .secondary(arg)
                                .message(format_args!("Inferred as `{source_display}`")),
                        );
                    }

                    // deduplicate definitions before attaching a subdiagnostic to each definition,
                    // or we'd have multiple subdiagnostics pointing to a single definition
                    // if the two types are specializations of the same generic class.
                    let definitions: FxIndexMap<Definition<'db>, String> = types
                        .into_iter()
                        .filter_map(|ty| ty.definition(db, env))
                        .filter_map(|definition| definition.definition())
                        .filter_map(|definition| Some((definition, definition.name(db)?)))
                        .collect();

                    for (definition, name) in definitions {
                        let file = definition.python_file(db);
                        let module = parsed_module(db, file).load(db);
                        let mut range = definition.focus_range(db, &module);
                        if let DefinitionKind::Class(class) = definition.kind(db) {
                            let definition_types = infer_definition_types(db, definition);
                            if let Some(decorator) =
                                class.node(&module).decorator_list.iter().find(|decorator| {
                                    definition_types
                                        .expression_type(&decorator.expression)
                                        .as_function_literal()
                                        .is_some_and(|func| func.is_known(db, KnownFunction::Final))
                                })
                            {
                                range = range.cover_range(decorator.range());
                            }
                        }
                        diagnostic.annotate(
                            Annotation::secondary(Span::from(range))
                                .message(format_args!("`{name}` defined here")),
                        );
                    }

                    if casted_type.is_protocol_instance() {
                        if source_type.is_protocol_instance() {
                            diagnostic.info(format_args!(
                                "protocol `{casted_display}` is disjoint \
                                from protocol `{source_display}`"
                            ));
                        } else {
                            diagnostic.info(format_args!(
                                "protocol `{casted_display}` is disjoint \
                                from `{source_display}`"
                            ));
                        }
                    } else if source_type.is_protocol_instance() {
                        diagnostic.info(format_args!(
                            "`{casted_display}` is disjoint \
                            from protocol `{source_display}`"
                        ));
                    } else {
                        diagnostic.info(format_args!(
                            "`{casted_display}` is disjoint from `{source_display}`"
                        ));
                    }

                    source_type
                        .disjointness_error_context(db, env, casted_type)
                        .attach_to(db, env, &mut diagnostic);
                }
            }

            KnownFunction::RuntimeCheckable => {
                if let [Some(Type::ClassLiteral(class))] = parameter_types
                    && !class.is_protocol(builder.db())
                {
                    report_invalid_runtime_checkable(&builder.context, call_expression, *class);
                }
            }

            KnownFunction::GetProtocolMembers => {
                let [Some(Type::ClassLiteral(class))] = parameter_types else {
                    return;
                };
                if class.is_protocol(builder.db()) {
                    return;
                }
                report_bad_argument_to_get_protocol_members(
                    &builder.context,
                    call_expression,
                    *class,
                );
            }

            KnownFunction::RevealProtocolInterface => {
                let [Some(param_type)] = parameter_types else {
                    return;
                };
                let env = builder.program_environment();
                let Some(protocol_class) = param_type
                    .to_class_type(db)
                    .and_then(|class| class.into_protocol_class(db))
                else {
                    report_bad_argument_to_protocol_interface(
                        &builder.context,
                        call_expression,
                        *param_type,
                    );
                    return;
                };
                if let Some(diagnostic) = builder
                    .context
                    .report_diagnostic(DiagnosticId::RevealedType, Severity::Info)
                {
                    let mut diag = diagnostic.into_diagnostic("Revealed protocol interface");
                    let span = builder.context.span(
                        call_argument_node(call_expression, "protocol", 0)
                            .unwrap_or_else(|| ast::AnyNodeRef::from(call_expression)),
                    );
                    diag.annotate(Annotation::primary(span).message(format_args!(
                        "`{}`",
                        protocol_class.interface(db).display(db, env)
                    )));
                }
            }

            KnownFunction::RevealMro => {
                let [Some(param_type)] = parameter_types else {
                    return;
                };
                let mut good_argument = true;
                let classes = match param_type {
                    Type::ClassLiteral(class) => vec![ClassType::NonGeneric(*class)],
                    Type::GenericAlias(generic_alias) => vec![ClassType::Generic(*generic_alias)],
                    Type::Union(union) => {
                        let elements = union.elements(db);
                        let mut classes = Vec::with_capacity(elements.len());
                        for element in elements {
                            match element {
                                Type::ClassLiteral(class) => {
                                    classes.push(ClassType::NonGeneric(*class));
                                }
                                Type::GenericAlias(generic_alias) => {
                                    classes.push(ClassType::Generic(*generic_alias));
                                }
                                _ => {
                                    good_argument = false;
                                    break;
                                }
                            }
                        }
                        classes
                    }
                    _ => {
                        good_argument = false;
                        vec![]
                    }
                };
                if !good_argument {
                    let Some(builder) = builder
                        .context
                        .report_lint(&INVALID_ARGUMENT_TYPE, call_expression)
                    else {
                        return;
                    };
                    let mut diagnostic =
                        builder.into_diagnostic("Invalid argument to `reveal_mro`");
                    diagnostic.set_primary_annotation_message(format_args!(
                        "Can only pass a class object, generic alias or a union thereof"
                    ));
                    return;
                }
                if let Some(diagnostic) = builder
                    .context
                    .report_diagnostic(DiagnosticId::RevealedType, Severity::Info)
                {
                    let env = builder.program_environment();
                    let mut diag = diagnostic.into_diagnostic("Revealed MRO");
                    let span = builder.context.span(
                        call_argument_node(call_expression, "cls", 0)
                            .unwrap_or_else(|| ast::AnyNodeRef::from(call_expression)),
                    );
                    let mut message = String::new();
                    let display_settings = DisplaySettings::from_possibly_ambiguous_types(
                        db,
                        env,
                        classes
                            .iter()
                            .flat_map(|class| class.iter_mro(db))
                            .filter_map(ClassBase::into_class),
                    );
                    for (i, class) in classes.iter().enumerate() {
                        message.push('(');
                        for class in class.iter_mro(db) {
                            message.push_str(
                                &class
                                    .display_with(db, env, display_settings.clone())
                                    .to_string(),
                            );
                            // Omit the comma for the last element (which is always `object`)
                            if class
                                .into_class()
                                .is_none_or(|base| !base.is_object(builder.db()))
                            {
                                message.push_str(", ");
                            }
                        }
                        // If the last element was also the first element
                        // (i.e., it's a length-1 tuple -- which can only happen if we're revealing
                        // the MRO for `object` itself), add a trailing comma so that it's still a
                        // valid tuple display.
                        if class.is_object(db) {
                            message.push(',');
                        }
                        message.push(')');
                        if i < classes.len() - 1 {
                            message.push_str(" | ");
                        }
                    }
                    diag.annotate(Annotation::primary(span).message(message));
                }
            }

            KnownFunction::IsInstance | KnownFunction::IsSubclass => {
                let [Some(first_arg), Some(second_argument)] = parameter_types else {
                    return;
                };

                check_classinfo_in_isinstance(
                    db,
                    &builder.context,
                    call_expression,
                    self,
                    *second_argument,
                    crate::types::classinfo_spelling::classinfo_argument(call_expression),
                );

                if self == KnownFunction::IsInstance {
                    let env = builder.program_environment();
                    let truthiness = match second_argument {
                        Type::ClassLiteral(class) => {
                            is_instance_truthiness(db, env, *first_arg, *class)
                        }
                        Type::SpecialForm(
                            SpecialFormType::TypingCallable
                            | SpecialFormType::CollectionsAbcCallable,
                        ) => {
                            let callable_top = Type::Callable(CallableType::top(db));
                            if first_arg.is_subtype_of(db, env, callable_top) {
                                Truthiness::AlwaysTrue
                            } else {
                                Truthiness::Ambiguous
                            }
                        }
                        _ if is_instance_tuple_exhaustive(
                            db,
                            env,
                            *first_arg,
                            *second_argument,
                        ) =>
                        {
                            Truthiness::AlwaysTrue
                        }
                        _ => Truthiness::Ambiguous,
                    };
                    overload.set_return_type(Type::from_truthiness(db, env, truthiness));
                }
            }

            known @ (KnownFunction::DunderImport | KnownFunction::ImportModule) => {
                let [Some(first), rest @ ..] = parameter_types else {
                    return;
                };
                let Some(full_module_name) = first.as_string_literal() else {
                    return;
                };

                if rest.iter().any(Option::is_some) {
                    return;
                }

                let module_name = full_module_name.value(db);

                if known == KnownFunction::DunderImport && module_name.contains('.') {
                    // `__import__("collections.abc")` returns the `collections` module.
                    // `importlib.import_module("collections.abc")` returns the `collections.abc` module.
                    // ty doesn't have a way to represent the return type of the former yet.
                    // https://github.com/astral-sh/ruff/pull/19008#discussion_r2173481311
                    return;
                }

                let Some(module_name) = ModuleName::new(module_name) else {
                    return;
                };
                let importing_file = ImportingFile::File(
                    builder.file(),
                    builder.program_environment().resolver_environment(db),
                );
                let Some(module) = resolve_module(db, importing_file, &module_name) else {
                    return;
                };

                overload.set_return_type(Type::module_literal(db, builder.program_file(), module));
            }

            KnownFunction::TotalOrdering => {
                // When `total_ordering(cls)` is called as a function (not as a decorator),
                // check that the class defines at least one ordering method.
                let [Some(class_type)] = parameter_types else {
                    return;
                };

                let class = match class_type {
                    Type::ClassLiteral(class) => ClassType::NonGeneric(*class),
                    Type::GenericAlias(generic) => ClassType::Generic(*generic),
                    _ => return,
                };

                if !class.has_ordering_method_in_mro(db) {
                    report_invalid_total_ordering_call(
                        &builder.context,
                        class.class_literal(db),
                        call_expression,
                    );
                }
            }

            _ => {}
        }
    }
}

fn call_argument_node<'a>(
    call_expression: &'a ast::ExprCall,
    name: &str,
    position: usize,
) -> Option<ast::AnyNodeRef<'a>> {
    call_expression
        .arguments
        .find_argument(name, position)
        .map(|argument| match argument {
            ast::ArgOrKeyword::Arg(expr) => ast::AnyNodeRef::from(expr),
            ast::ArgOrKeyword::Keyword(keyword) => ast::AnyNodeRef::from(keyword),
        })
}

/// Check the second argument to `isinstance()` or `issubclass()` for types that cannot be used
/// at runtime (protocol classes, typed dicts, `typing.Any` in `isinstance`, and invalid
/// `UnionType` elements). Handles class literals, tuples (including nested tuples), and
/// recursively validates each element.
///
/// `classinfo_expr` is the AST expression corresponding to `classinfo`, if available. It is
/// used for precise annotation spans (e.g., highlighting just the `UnionType` inside a tuple
/// rather than the whole tuple). It may be `None` when the tuple is not a literal in the AST
/// (e.g., when it's stored in a variable).
fn check_classinfo_in_isinstance<'db>(
    db: &'db dyn Db,
    context: &InferContext<'db, '_>,
    call_expression: &ast::ExprCall,
    function: KnownFunction,
    classinfo: Type<'db>,
    classinfo_expr: Option<&ast::Expr>,
) {
    match classinfo {
        Type::ClassLiteral(class) => {
            if class.is_typed_dict(db) {
                report_runtime_check_against_typed_dict(context, call_expression, class, function);
            } else if let Some(protocol_class) = class.into_protocol_class(db) {
                if !protocol_class.is_runtime_checkable(db) {
                    report_runtime_check_against_non_runtime_checkable_protocol(
                        context,
                        call_expression,
                        protocol_class,
                        function,
                    );
                } else if function == KnownFunction::IsSubclass {
                    let non_method_members = protocol_class.interface(db).non_method_members(db);
                    if !non_method_members.is_empty() {
                        report_issubclass_check_against_protocol_with_non_method_members(
                            context,
                            call_expression,
                            protocol_class,
                            &non_method_members,
                        );
                    }
                }
            }
        }
        Type::SpecialForm(SpecialFormType::Any) if function == KnownFunction::IsInstance => {
            let Some(builder) = context.report_lint(&INVALID_ARGUMENT_TYPE, call_expression) else {
                return;
            };
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`typing.Any` cannot be used with `isinstance()`"
            ));
            diagnostic
                .set_primary_annotation_message("This call will raise `TypeError` at runtime");
        }
        Type::KnownInstance(KnownInstanceType::UnionType(_))
            if context.program_environment().python_version(db) < ast::PythonVersion::PY310
                && !spelled_as_classes_by_the_lowering(
                    context,
                    call_expression,
                    classinfo_expr,
                ) =>
        {
            report_union_classinfo_before_python_310(context, call_expression, function);
        }
        Type::KnownInstance(KnownInstanceType::UnionType(_)) => {
            report_invalid_union_type_elements(
                db,
                context,
                call_expression,
                function,
                classinfo,
                classinfo_expr,
            );
        }
        Type::NominalInstance(nominal)
            if let Some(tuple_spec) = nominal.tuple_spec(db, context.program_environment()) =>
        {
            let element_exprs = match classinfo_expr {
                Some(ast::Expr::Tuple(tuple_expr)) => Some(&tuple_expr.elts),
                _ => None,
            };
            for (index, element) in tuple_spec.iter_element_types(db).enumerate() {
                let element_expr = element_exprs.and_then(|elts| elts.get(index));
                check_classinfo_in_isinstance(
                    db,
                    context,
                    call_expression,
                    function,
                    element,
                    element_expr,
                );
            }
        }

        _ => {}
    }
}

/// whether the union `classinfo_expr` evaluates to is spelled as a tuple of classes where it
/// stands, by the lowering a basedpython module goes through before it runs. see
/// [`classinfo_spelling`](crate::types::classinfo_spelling)
fn spelled_as_classes_by_the_lowering(
    context: &InferContext<'_, '_>,
    call_expression: &ast::ExprCall,
    classinfo_expr: Option<&ast::Expr>,
) -> bool {
    context.file().source_type(context.db()) == ast::PySourceType::BasedPython
        && classinfo_expr.is_some_and(|classinfo_expr| {
            crate::types::classinfo_spelling::spelled_as_classes(call_expression, classinfo_expr)
        })
}

/// report a union object passed to `isinstance()` / `issubclass()` on a python older than
/// 3.10, where it is not a class and the call raises
fn report_union_classinfo_before_python_310(
    context: &InferContext<'_, '_>,
    call_expression: &ast::ExprCall,
    function: KnownFunction,
) {
    let Some(builder) = context.report_lint(&INVALID_ARGUMENT_TYPE, call_expression) else {
        return;
    };
    let function_name: &str = function.into();
    let mut diagnostic = builder.into_diagnostic(format_args!(
        "A union cannot be used with `{function_name}()` before Python 3.10"
    ));
    diagnostic.set_primary_annotation_message("This call will raise `TypeError` at runtime");
    diagnostic.info("Pass a tuple of classes instead");
}

/// Return whether a fixed `isinstance` tuple covers every possible type of an input.
///
/// Each class in the tuple uses the same truthiness inference as a single-class `isinstance` check.
///
/// ```python
/// def f(x: A | B) -> bool:
///     if isinstance(x, (A, B)):
///         return True
/// ```
fn is_instance_tuple_exhaustive<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    classinfo: Type<'db>,
) -> bool {
    let Some(tuple) = classinfo.tuple_instance_spec(db, env) else {
        return false;
    };
    if tuple.is_variadic() {
        return false;
    }

    is_instance_tuple_covers(db, env, &tuple, ty, &ActiveRecursionDetector::default())
}

fn is_instance_tuple_covers<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    tuple: &TupleSpec<'db>,
    ty: Type<'db>,
    recursion_guard: &ActiveRecursionDetector<Type<'db>>,
) -> bool {
    match ty {
        Type::TypeAlias(_) | Type::Recursive(_) => recursion_guard.visit(
            &ty,
            || true,
            || is_instance_tuple_covers(db, env, tuple, ty.resolve_type_alias(db), recursion_guard),
        ),
        Type::Union(union) => union
            .elements(db)
            .iter()
            .all(|element| is_instance_tuple_covers(db, env, tuple, *element, recursion_guard)),
        Type::Intersection(intersection) => intersection
            .positive(db)
            .iter()
            .any(|element| is_instance_tuple_covers(db, env, tuple, *element, recursion_guard)),
        Type::TypeVar(typevar) => match typevar.require_bound_or_constraints(db, env) {
            TypeVarBoundOrConstraints::UpperBound(bound) => {
                is_instance_tuple_covers(db, env, tuple, bound, recursion_guard)
            }
            TypeVarBoundOrConstraints::Constraints(constraints) => {
                constraints.elements(db).iter().all(|constraint| {
                    is_instance_tuple_covers(db, env, tuple, *constraint, recursion_guard)
                })
            }
        },
        ty => tuple.fixed_elements().any(|element| {
            let Type::ClassLiteral(class) = element else {
                return false;
            };
            is_instance_truthiness(db, env, ty, *class).is_always_true()
        }),
    }
}

/// Evaluate an `isinstance` call. Return `Truthiness::AlwaysTrue` if we can definitely infer that
/// this will return `True` at runtime, `Truthiness::AlwaysFalse` if we can definitely infer
/// that this will return `False` at runtime, or `Truthiness::Ambiguous` if we should infer `bool`
/// instead.
fn is_instance_truthiness<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    class: ClassLiteral<'db>,
) -> Truthiness {
    let is_instance = |ty: &Type<'_>| {
        ty.as_nominal_instance().is_some_and(|instance| {
            instance
                .class(db, env)
                .is_subtype_of_class_literal(db, class)
        })
    };

    let always_true_if = |test: bool| {
        if test {
            Truthiness::AlwaysTrue
        } else {
            Truthiness::Ambiguous
        }
    };

    match ty {
        // parameter-only marker; behaves as the type a body sees (bound of `Key`)
        Type::Overlapping(overlapping) => {
            is_instance_truthiness(db, env, overlapping.value_type(db, env), class)
        }
        Type::Restricted(restricted) => {
            is_instance_truthiness(db, env, restricted.value_type(db), class)
        }
        Type::Deferred(deferred) => {
            is_instance_truthiness(db, env, deferred.reduced(db, env), class)
        }
        Type::Recursive(recursive) => recursive
            .unfold(db, env)
            .map(|unfolded| is_instance_truthiness(db, env, unfolded, class))
            .unwrap_or(Truthiness::Ambiguous),
        Type::RecursiveVar(_) => {
            unreachable!("semantic operation on an unbound recursive variable")
        }
        Type::Union(..) => {
            // We do not handle unions specifically here, because something like `A | SubclassOfA` would
            // have been simplified to `A` anyway
            Truthiness::Ambiguous
        }

        // Which materialization is at hand is unknown, so an `isinstance()` check against any
        // one of them can never be statically true.
        Type::UnsafeUnion(_) => Truthiness::Ambiguous,

        // Create a new intersection that maps type variables to their upper bounds,
        // and evaluate the truthiness of the `isinstance()` check with that type.
        // Along the way, short-circuit to `AlwaysTrue` if we find any positive element
        // that is always true.
        Type::Intersection(intersection) => {
            let mut effective = IntersectionBuilder::new(db, env);
            let mut found_tvars_or_newtypes = false;

            for &positive in intersection.positive(db) {
                if is_instance_truthiness(db, env, positive, class).is_always_true() {
                    return Truthiness::AlwaysTrue;
                } else if let Type::TypeVar(tvar) = positive {
                    match tvar.require_bound_or_constraints(db, env) {
                        TypeVarBoundOrConstraints::UpperBound(bound) => {
                            effective.add_positive_in_place(bound);
                        }
                        TypeVarBoundOrConstraints::Constraints(constraints) => {
                            effective.add_positive_in_place(constraints.as_type(db, env));
                        }
                    }
                    found_tvars_or_newtypes = true;
                } else if let Type::NewTypeInstance(newtype) = positive {
                    found_tvars_or_newtypes = true;
                    effective.add_positive_in_place(newtype.concrete_base_type(db));
                } else {
                    effective.add_positive_in_place(positive);
                }
            }

            if !found_tvars_or_newtypes {
                return Truthiness::Ambiguous;
            }

            for &negative in intersection.negative(db) {
                if is_instance_truthiness(db, env, negative, class).is_always_true() {
                    return Truthiness::AlwaysFalse;
                }
                effective.add_negative_in_place(negative);
            }

            let effective = effective.build();

            if effective == ty {
                Truthiness::Ambiguous
            } else {
                is_instance_truthiness(db, env, effective, class)
            }
        }

        Type::EnumComplement(complement) => {
            is_instance_truthiness(db, env, complement.to_intersection(db, env), class)
        }

        Type::NominalInstance(..) => always_true_if(is_instance(&ty)),

        Type::NewTypeInstance(newtype) => {
            always_true_if(is_instance(&newtype.concrete_base_type(db)))
        }

        Type::LiteralValue(..) | Type::ModuleLiteral(..) | Type::FunctionLiteral(..) => {
            always_true_if(
                ty.literal_fallback_instance(db, env)
                    .as_ref()
                    .is_some_and(is_instance),
            )
        }

        Type::ClassLiteral(..) => {
            always_true_if(is_instance(&KnownClass::Type.to_instance(db, env)))
        }

        Type::TypeAlias(alias) => is_instance_truthiness(db, env, alias.value_type(db), class),

        Type::TypeVar(bound_typevar) => match bound_typevar.require_bound_or_constraints(db, env) {
            TypeVarBoundOrConstraints::UpperBound(bound) => {
                is_instance_truthiness(db, env, bound, class)
            }
            TypeVarBoundOrConstraints::Constraints(constraints) => always_true_if(
                constraints
                    .elements(db)
                    .iter()
                    .all(|c| is_instance_truthiness(db, env, *c, class).is_always_true()),
            ),
        },

        Type::BoundMethod(..)
        | Type::KnownBoundMethod(..)
        | Type::WrapperDescriptor(..)
        | Type::DataclassDecorator(..)
        | Type::DataclassTransformer(..)
        | Type::GenericAlias(..)
        | Type::SubclassOf(..)
        | Type::ProtocolInstance(..)
        | Type::SpecialForm(..)
        | Type::KnownInstance(..)
        | Type::PropertyInstance(..)
        | Type::SlotDescriptor(..)
        | Type::AlwaysTruthy
        | Type::AlwaysFalsy
        | Type::BoundSuper(..)
        | Type::TypeIs(..)
        | Type::TypeGuard(..)
        | Type::TypeForm(..)
        | Type::Callable(..)
        | Type::Dynamic(..)
        | Type::Divergent(_)
        | Type::Never
        | Type::TypedDict(_) => {
            // We could probably try to infer more precise types in some of these cases, but it's unclear
            // if it's worth the effort.
            Truthiness::Ambiguous
        }
    }
}

/// Report an error if a `types.UnionType` instance passed to `isinstance()`/`issubclass()`
/// contains elements that are not class objects.
fn report_invalid_union_type_elements<'db>(
    db: &'db dyn Db,
    context: &InferContext<'db, '_>,
    call_expression: &ast::ExprCall,
    function: KnownFunction,
    union_type: Type<'db>,
    union_type_expr: Option<&ast::Expr>,
) {
    fn find_invalid_elements<'db>(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        function: KnownFunction,
        ty: Type<'db>,
        invalid_elements: &mut Vec<Type<'db>>,
    ) {
        match ty {
            Type::ClassLiteral(_) => {}
            Type::NominalInstance(instance)
                if instance.has_known_class(db, KnownClass::NoneType) => {}
            Type::SpecialForm(special_form) if special_form.is_valid_isinstance_target() => {}
            // `Any` can be used in `issubclass()` calls but not `isinstance()` calls
            Type::SpecialForm(SpecialFormType::Any) if function == KnownFunction::IsSubclass => {}
            Type::KnownInstance(KnownInstanceType::UnionType(instance)) => {
                match instance.value_expression_types(db, env) {
                    Ok(value_expression_types) => {
                        for element in value_expression_types {
                            find_invalid_elements(db, env, function, element, invalid_elements);
                        }
                    }
                    Err(_) => {
                        invalid_elements.push(ty);
                    }
                }
            }
            _ => invalid_elements.push(ty),
        }
    }

    let mut invalid_elements = vec![];
    let env = context.program_environment();
    find_invalid_elements(db, env, function, union_type, &mut invalid_elements);

    let Some((first_invalid_element, other_invalid_elements)) = invalid_elements.split_first()
    else {
        return;
    };

    let Some(builder) = context.report_lint(&INVALID_ARGUMENT_TYPE, call_expression) else {
        return;
    };

    let function_name: &str = function.into();

    let mut diagnostic =
        builder.into_diagnostic(format_args!("Invalid second argument to `{function_name}`"));
    diagnostic.info(format_args!(
        "A `UnionType` instance can only be used as the second argument to \
        `{function_name}` if all elements are class objects"
    ));
    if let Some(union_type_expr) = union_type_expr {
        diagnostic.annotate(
            Annotation::secondary(context.span(union_type_expr))
                .message("This `UnionType` instance contains non-class elements"),
        );
    }

    // When we have a secondary annotation pointing at the UnionType expression,
    // "the union" is unambiguous. Otherwise, spell out the union type in the message.
    let env = context.program_environment();
    let union_suffix = match (&union_type_expr, union_type) {
        (None, Type::KnownInstance(KnownInstanceType::UnionType(instance))) => {
            match instance.union_type(db) {
                Ok(ty) => format!(" `{}`", ty.display(db, env)),
                Err(_) => String::new(),
            }
        }
        _ => String::new(),
    };

    match other_invalid_elements {
        [] => diagnostic.info(format_args!(
            "Element `{}` in the union{union_suffix} is not a class object",
            first_invalid_element.display(db, env)
        )),
        [single] => diagnostic.info(format_args!(
            "Elements `{}` and `{}` in the union{union_suffix} are not class objects",
            first_invalid_element.display(db, env),
            single.display(db, env),
        )),
        _ => diagnostic.info(format_args!(
            "Element `{}` in the union{union_suffix}, and {} more elements, are not class objects",
            first_invalid_element.display(db, env),
            other_invalid_elements.len(),
        )),
    }
}
