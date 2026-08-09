use crate::types::any_over_type;
use crate::{
    Db,
    types::{
        KnownClass, KnownInstanceType, ParamSpecAttrKind, SubclassOfInner, SubclassOfType, Type,
        TypeContext, TypeVarKind, UnionType,
        class::ClassLiteral,
        dedicated::pytest,
        diagnostic::{
            FINAL_ON_NON_METHOD, INVALID_FIXTURE_TYPE, INVALID_PARAMETER_DEFAULT,
            INVALID_PARAMETRIZE, INVALID_PARAMSPEC, INVALID_TYPE_FORM, REDUNDANT_RETURN_ANNOTATION,
            REIFIED_CLASSMETHOD, TRAILING_LAMBDA_PARAMETERS, TRAILING_LAMBDA_RETURN_TYPE,
            UNKNOWN_FIXTURE, USELESS_OVERLOAD_BODY, add_type_expression_reference_link,
            is_invalid_typed_dict_literal, report_bool_as_int, report_implicit_return_type,
            report_invalid_generator_function_return_type, report_invalid_return_type,
            report_shadowed_type_variable,
        },
        extensions,
        function::{
            FunctionBodyKind, FunctionDecorators, FunctionLiteral, FunctionType, KnownFunction,
            OverloadLiteral, function_body_kind, infers_unannotated_signatures,
            is_implicit_classmethod, same_module_uncached_raw_signature,
        },
        function_framework_role,
        generics::{enclosing_generic_contexts, typing_self},
        infer::{
            InferenceFlags, TypeExpressionFlags, TypeInferenceBuilder,
            builder::{
                DeclaredAndInferredType, DeferredExpressionState, TypeAndRange,
                TypeParamReification, validate_paramspec_components,
            },
            function_known_decorators, infer_statement_types, nearest_enclosing_function,
            original_class_type,
        },
        infer_definition_types, infer_expression_types, infer_scope_types,
        inferred_signature::{can_implicitly_return_none, return_type_from_body},
        lifetimes::InheritedBorrow,
        signatures::ReturnCallableTypeVarScope,
        trailing_lambda::{
            UnbindableParameters, trailing_lambda_it_borrow, trailing_lambda_it_type,
        },
        tuple::{TupleSpecBuilder, TupleType},
        typed_dict::extract_unpacked_typed_dict_keys_from_kwargs_annotation,
    },
};
use ty_python_core::{
    definition::{Definition, DefinitionKind},
    scope::NodeWithScopeRef,
};

use ruff_db::diagnostic::{Annotation, Span};
use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::helpers::{ReturnGuardForm, return_guards};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;

fn parameters_have_annotations(parameters: &ast::Parameters) -> bool {
    parameters
        .iter_non_variadic_params()
        .any(|param| param.parameter.annotation.is_some())
        || parameters
            .vararg
            .as_deref()
            .is_some_and(|param| param.annotation.is_some())
        || parameters
            .kwarg
            .as_deref()
            .is_some_and(|param| param.annotation.is_some())
}

/// Return type policy for checking explicit `return` statements in a function body.
#[derive(Debug, Copy, Clone)]
struct ExpectedReturnType<'db> {
    /// The externally-visible return type.
    public: Type<'db>,
    /// The lexical return type, if it differs for a generic PEP 695 function.
    lexical: Option<Type<'db>>,
}

impl<'db> ExpectedReturnType<'db> {
    /// Creates the expected return type policy for `function_node`.
    fn from_function(
        db: &'db dyn Db,
        function: FunctionType<'db>,
        function_node: &ast::StmtFunctionDef,
    ) -> Self {
        /// Normalizes special return annotations to the type actually returned by expressions.
        fn normalize<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
            match ty {
                Type::TypeIs(_) | Type::TypeGuard(_) => KnownClass::Bool.to_instance(db),
                ty => ty,
            }
        }

        let public = normalize(
            db,
            same_module_uncached_raw_signature(db, function, ReturnCallableTypeVarScope::Public)
                .return_ty,
        );
        let lexical = function_node.type_params.is_some().then(|| {
            normalize(
                db,
                same_module_uncached_raw_signature(
                    db,
                    function,
                    ReturnCallableTypeVarScope::Lexical,
                )
                .return_ty,
            )
        });

        Self { public, lexical }
    }

    /// Returns the externally-visible return type.
    fn public(self) -> Type<'db> {
        self.public
    }

    /// Returns `true` if `ty` is accepted by either the public return type or the lexical return
    /// type.
    fn accepts(self, db: &'db dyn Db, ty: Type<'db>) -> bool {
        ty.is_assignable_to(db, self.public)
            || self
                .lexical
                .is_some_and(|lexical| ty.is_assignable_to(db, lexical))
    }
}

impl<'db, 'ast> TypeInferenceBuilder<'db, 'ast> {
    pub(super) fn infer_function_body(&mut self, function: &ast::StmtFunctionDef) {
        let db = self.db();

        // Parameters are odd: they are Definitions in the function body scope, but have no
        // constituent nodes that are part of the function body. In order to get diagnostics
        // merged/emitted for them, we need to explicitly infer their definitions here.
        for parameter in &function.parameters {
            self.infer_definition(parameter);
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

        validate_paramspec_components(&self.context, &function.parameters, |expr| {
            self.file_expression_type(expr)
        });
        self.validate_unpacked_typed_dict_kwargs(&function.parameters);

        self.check_pytest_function(function);

        // basedpython: enforce `local` (no escape) and `once` (exactly one call),
        // both on the function's own parameters and on a trailing-lambda block's
        // `it`, which is borrowed when the callee's callback declares it so
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

        self.infer_body(&function.body);

        // basedpython: a `local` borrow may only be passed on to another `local`
        // parameter — checked after the body so callee types are available
        crate::types::lifetimes::check_local_argument_passing(
            &self.context,
            function,
            inherited_borrow,
            |expr| self.try_expression_type(expr),
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
        );

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
                let (overloads, implementation) = enclosing.overloads_and_implementation(db);
                if overloads.is_empty() || implementation.is_none() {
                    return None;
                }
                if function_body_kind(db, function, |expr| self.expression_type(expr))
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
                && function_body_kind(db, function, |expr| self.expression_type(expr))
                    == FunctionBodyKind::Stub;

            let mut enclosing_class_context = None;

            if has_empty_body {
                if self.in_stub() {
                    return;
                }
                if self.in_function_overload_or_abstractmethod() {
                    return;
                }
                if self.is_in_type_checking_block(self.scope(), function) {
                    return;
                }
                // basedpython: bodyless `def f(...) -> T` is implicit overload
                // / stub declaration. the transpiler lowers it to `: ...` and
                // adds `@overload` for consecutive same-name groups. don't
                // report empty-body on `.by` source — the runtime form
                // never actually executes
                if self.file().source_type(db).is_basedpython() && function.body.is_empty() {
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
            let expected_return =
                ExpectedReturnType::from_function(db, enclosing_function, function);
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
                    .to_instance_unknown(db)
                    .is_assignable_to(db, expected_ty)
                {
                    report_invalid_generator_function_return_type(
                        &self.context,
                        returns_range,
                        inferred_return,
                        declared_ty,
                    );
                }

                if let Some(expected_return_ty) = declared_ty.generator_return_type(db) {
                    for returned in self.return_types_and_ranges.iter().copied() {
                        report_bool_as_int(
                            &self.context,
                            returned.range,
                            returned.ty,
                            expected_return_ty,
                        );
                    }
                    for invalid in
                        self.return_types_and_ranges
                            .iter()
                            .copied()
                            .filter(|actual_return_ty| {
                                !actual_return_ty.ty.is_assignable_to(db, expected_return_ty)
                            })
                    {
                        report_invalid_return_type(
                            &self.context,
                            invalid.range,
                            returns_range,
                            expected_return_ty,
                            invalid.ty,
                        );
                    }

                    let use_def = self.index.use_def_map(scope_id);

                    if can_implicitly_return_none(db, use_def)
                        && !Type::none(db).is_assignable_to(db, expected_return_ty)
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

            for invalid in self
                .return_types_and_ranges
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
                .filter(|ty_range| !expected_return.accepts(db, ty_range.ty))
            {
                // basedpython: a `return` is a conversion site — an in-scope
                // `implementation A for B:` or a conversion dunder makes the value
                // returnable where it otherwise is not, and the transpiler emits the
                // conversion. the generator paths above return early, so this is the
                // plain case where the declared type really is the value's target
                // only when the type the transpiler will recover for this function is
                // the one being enforced here; otherwise the conversion it emits would
                // be built from a different target
                if crate::types::implementations::function_declared_return_type(
                    db,
                    self.file(),
                    function,
                ) == Some(declared_ty)
                    && crate::types::conversions::repair_conversion(
                        db,
                        self.file(),
                        invalid.ty,
                        declared_ty,
                        crate::types::conversions::returned_value_at(function, invalid.range),
                    )
                    .is_some_and(|repair| {
                        crate::types::conversions::report_ambiguous_conversion(
                            &self.context,
                            invalid.range,
                            &repair,
                        );
                        true
                    })
                {
                    continue;
                }
                report_invalid_return_type(
                    &self.context,
                    invalid.range,
                    returns_range,
                    declared_ty,
                    invalid.ty,
                );
            }
            let use_def = self.index.use_def_map(scope_id);
            if can_implicitly_return_none(db, use_def)
                && !Type::none(db).is_assignable_to(db, expected_ty)
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

    /// basedpython: report an explicit `-> None` that leaves the function's type exactly where
    /// deleting it would.
    ///
    /// The question is only ever that one: what would this `def` return with the annotation
    /// gone? Where that type comes from — the body, an overridden base, a sibling overload
    /// group — decides nothing. Everything left unreported is a case where the answer is not
    /// `None`: a generator hands back a generator, a body that always raises hands back `Never`,
    /// and an override or an overload implementation hands back whatever it inherits.
    fn check_redundant_return_annotation(&self, function: &ast::StmtFunctionDef) {
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

        // everything below reads the class MRO and the enclosing overload chain, so don't
        // pay for it when nothing will be reported
        if !self.context.is_lint_enabled(&REDUNDANT_RETURN_ANNOTATION) {
            return;
        }

        // with nothing recovering the signature, dropping `-> None` widens the return type to
        // `Unknown`, so the annotation is carrying the type on its own
        if !infers_unannotated_signatures(db, self.file()) {
            return;
        }

        let Some(function_type) = self.current_function_type() else {
            return;
        };

        let scope_id = self.index.node_scope(NodeWithScopeRef::Function(function));
        let without_annotation = function_type
            .literal(db)
            .last_definition
            .return_type_without_annotation(db, || {
                return_type_from_body(
                    db,
                    function,
                    scope_id.is_generator_function(self.index),
                    can_implicitly_return_none(db, self.index.use_def_map(scope_id)),
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
        let db = self.db();
        let Some(function) = self.current_function_type() else {
            return;
        };
        if function_framework_role(db, function).is_none() {
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

            match pytest::resolve_fixture(db, file, name.as_str()) {
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
                    if provided.is_assignable_to(db, declared) {
                        continue;
                    }
                    let Some(builder) = self.context.report_lint(&INVALID_FIXTURE_TYPE, range)
                    else {
                        continue;
                    };
                    let mut diagnostic = builder.into_diagnostic(format_args!(
                        "Fixture `{name}` provides `{}`, but the parameter is annotated `{}`",
                        provided.display(db),
                        declared.display(db),
                    ));
                    if let Some(fixture_definition) = fixture.definition {
                        let fixture_module =
                            parsed_module(db, fixture_definition.file(db)).load(db);
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
            let Some(marker) = pytest::parametrize_marker(db, function, decorator) else {
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
            type_params,
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
            (!decorator_list.is_empty()).then(|| function_known_decorators(db, definition));
        if let Some(decorator_inference) = decorator_inference.as_ref() {
            self.context.extend(decorator_inference.diagnostics());
            self.expressions
                .extend(decorator_inference.expression_types());
            self.bindings.extend(decorator_inference.bindings());
            self.called_functions
                .extend(decorator_inference.called_functions().iter().copied());
        }

        let mut decorator_types_and_nodes = Vec::with_capacity(decorator_list.len());
        let mut function_decorators = FunctionDecorators::empty();
        let mut dataclass_transformer_params = None;
        let mut final_decorator = None;

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
            // `private` has no decorator either, but it *is* recorded: privacy is
            // what makes a class's variance safe, so ty has to know about it
            if let ast::Expr::Name(n) = &decorator.expression
                && matches!(n.ctx, ast::ExprContext::Invalid)
                && matches!(
                    n.id.as_str(),
                    // `__init_method__` marks the `init(...)` shorthand — a plain
                    // `__init__`, so the synthetic marker is dropped too
                    "decorator_keyword" | "private" | "export" | "open" | "__init_method__"
                )
            {
                if n.id.as_str() == "private" {
                    function_decorators |= FunctionDecorators::PRIVATE;
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
            // effect, so it is not applied. basedpython's static-property marker is
            // the exception — the flag only says how the getter's receiver is typed,
            // while the descriptor it resolves to is the member's actual type
            if !decorator_function_decorator
                .difference(FunctionDecorators::BY_STATIC_PROPERTY)
                .is_empty()
            {
                continue;
            }

            decorator_types_and_nodes.push((decorator_type, decorator));
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

        // basedpython: a classmethod cannot have reified type parameters — the
        // classmethod binding hides the function whose closure would hold the
        // reified cells, so the specialization step has nothing to rebuild
        if self.is_basedpython_file()
            && (function_decorators.contains(FunctionDecorators::CLASSMETHOD)
                || is_implicit_classmethod(&name.id))
            && let Some(type_params) = function.type_params.as_deref()
        {
            let source = ruff_db::source::source_text(db, self.file());
            let reified = crate::reified::reified_type_param_names(
                source.as_str(),
                self.file().source_type(db),
                function,
            );
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

        let has_defaults = parameters
            .iter_non_variadic_params()
            .any(|param| param.default.is_some());

        // If there are type params, parameters and returns are evaluated in that scope. Otherwise,
        // we defer the inference of any parameter and return annotations. That ensures that we do
        // not add any spurious salsa cycles when applying decorators below. (Applying a decorator
        // requires getting the signature of this function definition, which in turn requires
        // (lazily) inferring the parameter and return types.) If defaults exist, we also defer so
        // they can be inferred once with type context in the enclosing scope.
        let has_signature_annotations = function.returns.is_some()
            || function.raises.is_some()
            || parameters_have_annotations(parameters);
        if (type_params.is_none() && has_signature_annotations) || has_defaults {
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
            .to_scope_id(db, self.file());

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

        let mut inferred_ty = Type::FunctionLiteral(FunctionType::new(db, function_literal, None));
        if !decorator_list.is_empty() {
            self.undecorated_type = Some(inferred_ty);
        }

        // Check that the function's own type parameters don't shadow
        // type variables from enclosing scopes (by name).
        if let Some(type_params) = &function.type_params {
            let current_scope = self.scope().file_scope_id(db);
            for type_param in type_params.iter() {
                let param_name = type_param.name();
                for enclosing in enclosing_generic_contexts(db, self.index, current_scope) {
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
            inferred_ty = if let Type::KnownInstance(KnownInstanceType::Deprecated(deprecated)) =
                decorator_ty
                && let Type::FunctionLiteral(function) = inferred_ty
            {
                Type::FunctionLiteral(function.with_deprecated(db, *deprecated))
            } else {
                self.apply_decorator(*decorator_ty, inferred_ty, decorator_node)
            };
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
                diagnostic.set_primary_message("This statement will never be executed");
                diagnostic.info(
                    "`@overload`-decorated functions are solely for type checkers \
                    and must be overwritten at runtime by a non-`@overload`-decorated implementation",
                );
                diagnostic.help("Consider replacing this function body with `...` or `pass`");
                break;
            }
        }
    }

    pub(super) fn infer_function_deferred(
        &mut self,
        definition: Definition<'db>,
        function: &ast::StmtFunctionDef,
    ) {
        let db = self.db();
        let mut prev_in_no_type_check = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_NO_TYPE_CHECK, true);
        for decorator in &function.decorator_list {
            let decorator_type = self.infer_decorator(decorator);
            if let Type::FunctionLiteral(function) = decorator_type
                && let Some(KnownFunction::NoTypeCheck) = function.known(db)
            {
                // If the function is decorated with the `no_type_check` decorator,
                // we need to suppress any errors that come after the decorators.
                prev_in_no_type_check = true;
                break;
            }
        }
        self.context
            .inference_flags
            .set(InferenceFlags::IN_NO_TYPE_CHECK, prev_in_no_type_check);

        let has_type_params = function.type_params.is_some();
        let has_defaults = function
            .parameters
            .iter_non_variadic_params()
            .any(|param| param.default.is_some());

        let previous_typevar_binding_context = self.typevar_binding_context.replace(definition);

        if !has_type_params {
            self.infer_return_type_annotation(function);
            self.infer_raises_clause(function);
            self.infer_parameters(function.parameters.as_ref());
        }

        if has_defaults {
            // In stub files, default values may reference names that are defined later in the file.
            let in_stub = self.in_stub();
            let previous_deferred_state =
                std::mem::replace(&mut self.deferred_state, in_stub.into());

            // For generic functions, only defaults are inferred here; annotation types come from
            // the type-params scope.
            if has_type_params {
                let type_params_scope = self
                    .index
                    .node_scope(NodeWithScopeRef::FunctionTypeParameters(function))
                    .to_scope_id(db, self.file());
                let type_params_inference =
                    infer_scope_types(db, type_params_scope, TypeContext::default());

                for param_with_default in function.parameters.iter_non_variadic_params() {
                    let Some(default) = param_with_default.default.as_deref() else {
                        continue;
                    };
                    let tcx = param_with_default
                        .parameter
                        .annotation
                        .as_deref()
                        .map(|annotation| {
                            TypeContext::new(Some(
                                type_params_inference.expression_type(annotation),
                            ))
                        })
                        .unwrap_or_else(TypeContext::default);
                    self.infer_expression(default, tcx);
                }
            } else {
                for param_with_default in function.parameters.iter_non_variadic_params() {
                    let Some(default) = param_with_default.default.as_deref() else {
                        continue;
                    };
                    let tcx = param_with_default
                        .parameter
                        .annotation
                        .as_deref()
                        .map(|annotation| TypeContext::new(Some(self.expression_type(annotation))))
                        .unwrap_or_else(TypeContext::default);
                    self.infer_expression(default, tcx);
                }
            }

            self.deferred_state = previous_deferred_state;
        }

        self.typevar_binding_context = previous_typevar_binding_context;
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
        let type_params = function
            .type_params
            .as_deref()
            .expect("function type params scope without type params");

        let binding_context = self.index.expect_single_definition(function);
        let previous_typevar_binding_context =
            self.typevar_binding_context.replace(binding_context);
        self.infer_return_type_annotation(function);
        self.infer_raises_clause(function);
        // basedpython: a `type def` is not a runtime function — the transpiler erases the
        // declaration, so there is no closure for the specialization step to rebuild
        let reification = if ast::helpers::is_type_def(function) {
            TypeParamReification::TypeDef
        } else {
            TypeParamReification::Function
        };
        self.infer_type_parameters(type_params, reification);
        self.infer_parameters(&function.parameters);
        self.typevar_binding_context = previous_typevar_binding_context;
    }

    fn infer_parameters(&mut self, parameters: &ast::Parameters) {
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
        for param_with_default in parameters.iter_non_variadic_params() {
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
        let Type::TypeVar(typevar) = annotated_type else {
            return false;
        };
        let identity = typevar.identity(self.db());
        parameters
            .iter()
            .filter_map(ruff_python_ast::AnyParameterRef::annotation)
            .filter(|annotation| !std::ptr::eq(*annotation, kwargs_annotation))
            .any(|annotation| {
                any_over_type(self.db(), self.file_expression_type(annotation), false, |ty| {
                    matches!(ty, Type::TypeVar(other) if other.identity(self.db()) == identity)
                })
            })
    }

    fn validate_unpacked_typed_dict_kwargs(&mut self, parameters: &ast::Parameters) {
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
        if annotated_type.is_typed_dict_bounded_typevar(self.db())
            && !self.typevar_used_by_another_parameter(annotated_type, parameters, annotation)
        {
            return;
        }
        let Some(unpacked_keys) = extract_unpacked_typed_dict_keys_from_kwargs_annotation(
            self.db(),
            annotated_type,
            annotation_flags,
        ) else {
            if !annotated_type.is_unknown()
                && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, annotation)
            {
                let diag = builder.into_diagnostic(format_args!(
                    "Unpacked value for `**kwargs` must be a TypedDict, not `{}`",
                    annotated_type.display(self.db())
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
            let declared_ty = self.file_expression_type(annotation).erase_overlapping(db);

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
                    is_invalid_typed_dict_literal(db, declared_ty, default_expr.into());
                if !default_ty.is_assignable_to(db, declared_ty)
                    && !suppress_invalid_default
                    && !((self.in_stub()
                        || self.in_function_overload_or_abstractmethod()
                        || self.is_in_type_checking_block(self.scope(), default_expr)
                        || self
                            .class_context_of_current_method()
                            .is_some_and(|class| class.is_protocol(db)))
                        && default
                            .as_ref()
                            .is_some_and(|d| d.is_ellipsis_literal_expr()))
                {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_PARAMETER_DEFAULT, parameter_with_default)
                    {
                        builder.into_diagnostic(format_args!(
                            "Default value of type `{}` is not assignable \
                             to annotated parameter type `{}`",
                            default_ty.display(db),
                            declared_ty.display(db)
                        ));
                    }
                }
            }

            self.add_declaration_with_binding(
                parameter.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(declared_ty),
            );
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
                    if default_ty.is_assignable_to(db, base) {
                        base
                    } else {
                        UnionType::from_two_elements(db, base, default_ty)
                    }
                } else if let Some(hole) = hole {
                    hole
                } else {
                    UnionType::from_two_elements(db, Type::unknown(), default_ty)
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
        let db = self.db();
        let function = nearest_enclosing_function(db, self.index, self.scope())?;
        pytest::injected_parameter_type(db, function, parameter.name.as_str())
    }

    /// basedpython: for an unannotated parameter, look up the type that
    /// [`OverloadLiteral::raw_signature`] filled in for it — from the sibling overloads when this
    /// is an overload implementation, or (under `sound-types`) from the overridden base method.
    /// matching is by name
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

    /// The type the enclosing function's signature gives `parameter`, when that type was not
    /// written as an annotation.
    ///
    /// A parameter that received no type at all still has its implicit `Unknown`, which
    /// `should_annotation_be_displayed` reports as not displayable, so this returns `None` for it.
    fn signature_parameter_type(&self, parameter: &ast::Parameter) -> Option<Type<'db>> {
        let db = self.db();
        let enclosing = nearest_enclosing_function(db, self.index, self.scope())?;
        let signature =
            enclosing.last_definition_raw_signature(db, ReturnCallableTypeVarScope::Public);
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
                        &TupleSpecBuilder::with_capacity(0)
                            .concat_variadic_typevar(db, typevar)
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
                                diag.set_primary_message(format_args!(
                                    "Did you mean `{name}.args`?"
                                ));
                                add_type_expression_reference_link(diag);
                            }
                            Type::homogeneous_tuple(db, Type::unknown())
                        }

                        // `*args: P`
                        None => {
                            // The diagnostic for this case is handled in `in_type_expression`.
                            Type::homogeneous_tuple(db, Type::unknown())
                        }
                    }
                }
                _ => Type::homogeneous_tuple(db, annotated_type),
            };

            self.add_declaration_with_binding(
                parameter.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(ty),
            );
        } else {
            let inferred_ty = Type::homogeneous_tuple(db, Type::unknown());
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

    /// basedpython: the type of the expression a trailing lambda block is
    /// attached to, read from its standalone-expression inference (registered by
    /// the semantic index builder — independent of the enclosing definition's
    /// inference, so no cycle)
    fn trailing_lambda_callee_type(&self, function: &ast::StmtFunctionDef) -> Option<Type<'db>> {
        let callee = function.trailing_lambda_callee()?;
        let expression = self.index.try_expression(callee)?;
        infer_expression_types(self.db(), expression, TypeContext::default())
            .try_expression_type(callee)
    }

    /// basedpython: the type of a trailing lambda's implicit `it` parameter.
    /// `None` when the callee's signature or its last parameter's callable shape
    /// is not inspectable
    fn trailing_lambda_it_parameter_type(
        &self,
        function: &ast::StmtFunctionDef,
    ) -> Option<Type<'db>> {
        trailing_lambda_it_type(self.db(), self.trailing_lambda_callee_type(function)?)
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
        let expression = self.index.try_expression(callee)?;
        let callee_ty = infer_expression_types(db, expression, TypeContext::default())
            .try_expression_type(callee)?;
        let borrow = trailing_lambda_it_borrow(db, callee_ty);
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
        self.trailing_lambda_callee_type(function)
            .is_some_and(|callee_ty| {
                crate::types::trailing_lambda::callee_callback_is_once(self.db(), callee_ty)
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
        let Some(callee_ty) = self.trailing_lambda_callee_type(function) else {
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
        let db = self.db();
        let Some(callee) = function.trailing_lambda_callee() else {
            return;
        };
        let Some(callee_ty) = self.trailing_lambda_callee_type(function) else {
            return;
        };
        let Some(return_ty) =
            crate::types::trailing_lambda::trailing_lambda_callback_return_type(db, callee_ty)
        else {
            return;
        };
        // the block returns `None`; a declared return type that accepts `None`
        // (`None`, `int | None`, `object`, …) is satisfiable, anything else is not
        if Type::none(db).is_assignable_to(db, return_ty) {
            return;
        }
        if let Some(builder) = self
            .context
            .report_lint(&TRAILING_LAMBDA_RETURN_TYPE, callee)
        {
            builder.into_diagnostic(format_args!(
                "a trailing-lambda callback must return `None`, not `{}` \
                 (other return types are not yet supported)",
                return_ty.display(db)
            ));
        }
    }

    /// Special case for unannotated `cls` and `self` arguments to class methods and instance methods.
    fn special_first_method_parameter_type(
        &mut self,
        parameter: &ast::Parameter,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let file = self.file();

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
                if known_class == KnownClass::Staticmethod {
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

        // basedpython: an extension method's `self` is the *extended* type,
        // specialized at the extension's view of its type parameters — not a
        // `Self` typevar of the synthetic extension class (whose own body holds
        // only the extension members)
        if let ClassLiteral::Static(static_literal) = class_literal
            && static_literal.is_extension(db)
        {
            let body_view = extensions::body_view_class(db, static_literal)?;
            return Some(if is_classmethod || function_name == "__new__" {
                SubclassOfType::from(db, SubclassOfInner::Class(body_view))
            } else {
                Type::instance(db, body_view)
            });
        }

        let typing_self = typing_self(db, self.scope(), Some(method_definition), class_literal);
        if is_classmethod || function_name == "__new__" {
            typing_self
                .map(|typing_self| SubclassOfType::from(db, SubclassOfInner::TypeVar(typing_self)))
        } else {
            typing_self.map(Type::TypeVar)
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
                            diag.set_primary_message(format_args!("Did you mean `{name}.kwargs`?"));
                            add_type_expression_reference_link(diag);
                        }
                        KnownClass::Dict.to_specialized_instance(
                            db,
                            &[KnownClass::Str.to_instance(db), Type::unknown()],
                        )
                    }

                    // `**kwargs: P.kwargs`
                    Some(ParamSpecAttrKind::Kwargs) => annotated_type,

                    // `**kwargs: P`
                    None => {
                        // The diagnostic for this case is handled in `in_type_expression`.
                        KnownClass::Dict.to_specialized_instance(
                            db,
                            &[KnownClass::Str.to_instance(db), Type::unknown()],
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
                KnownClass::Dict
                    .to_specialized_instance(db, &[KnownClass::Str.to_instance(db), annotated_type])
            };
            self.add_declaration_with_binding(
                parameter.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(ty),
            );
        } else {
            let inferred_ty = KnownClass::Dict
                .to_specialized_instance(db, &[KnownClass::Str.to_instance(db), Type::unknown()]);

            self.add_binding(parameter.into(), definition)
                .insert(self, inferred_ty);
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

        let default_expr = default.as_ref();
        let ty = if let Some(parameter_type) = self.annotated_lambda_parameter_type(index, lambda) {
            parameter_type
        } else if let Some(default_expr) = default_expr {
            let default_ty = self.file_expression_type(default_expr);
            if self.settings().sound_types {
                // basedpython: same rule as an unannotated function parameter with a default —
                // the parameter takes the default's promoted type instead of folding in `Unknown`
                default_ty.promote(self.db())
            } else {
                UnionType::from_two_elements(self.db(), Type::unknown(), default_ty)
            }
        } else {
            Type::unknown()
        };

        // basedpython typed lambdas (`lambda (a: int) -> int: ...`) make the
        // parameter a `DeclarationAndBinding` because the annotation is set,
        // matching annotated function parameters. The plain `add_binding`
        // path looks up `declarations_by_binding` which is only populated for
        // pure bindings — for declared+bound parameters we go through
        // `add_declaration_with_binding` instead, otherwise the lookup
        // panics with "no entry found for key".
        if parameter.annotation.is_some() {
            self.add_declaration_with_binding(
                parameter.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(ty),
            );
        } else {
            self.add_binding(parameter.into(), definition)
                .insert(self, ty);
        }
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
        // Note that this currently always returns `None` because we do not support `Unpack`
        // annotations for callable types.
        let ty = if let Some(parameter_type) = self.annotated_lambda_parameter_type(index, lambda) {
            parameter_type
        } else {
            Type::homogeneous_tuple(self.db(), Type::unknown())
        };
        // see `infer_lambda_parameter_definition` — annotated `*args` is a
        // `DeclarationAndBinding`, which doesn't populate
        // `declarations_by_binding`, so `add_binding` would panic
        if parameter.annotation.is_some() {
            self.add_declaration_with_binding(
                parameter.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(ty),
            );
        } else {
            self.add_binding(parameter.into(), definition)
                .insert(self, ty);
        }
    }

    /// Set initial declared/inferred types for a `**kwargs` keyword-variadic parameter
    /// in a lambda expression.
    pub(super) fn infer_variadic_keyword_lambda_parameter_definition(
        &mut self,
        parameter: &'ast ast::Parameter,
        definition: Definition<'db>,
    ) {
        let inferred_ty = KnownClass::Dict.to_specialized_instance(
            self.db(),
            &[KnownClass::Str.to_instance(self.db()), Type::unknown()],
        );

        if parameter.annotation.is_some() {
            self.add_declaration_with_binding(
                parameter.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(inferred_ty),
            );
        } else {
            self.add_binding(parameter.into(), definition)
                .insert(self, inferred_ty);
        }
    }

    /// Returns the annotated type of the lambda parameter at the given index in the provided
    /// lambda expression, based on a `Callable` type annotation, if present.
    fn annotated_lambda_parameter_type(
        &mut self,
        index: u32,
        lambda: &'ast ast::ExprLambda,
    ) -> Option<Type<'db>> {
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
        if parameter_type.is_unknown() || parameter_type.has_unspecialized_type_var(self.db()) {
            None
        } else {
            Some(parameter_type)
        }
    }
}
