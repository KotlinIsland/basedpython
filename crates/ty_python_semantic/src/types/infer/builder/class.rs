use crate::Db;
use crate::ProgramEnvironment;
use crate::place::Place;
use crate::reified::{UnansweredReason, reified_class_reads};
use crate::types::diagnostic::{INVALID_VARIANCE_DECLARATION, REIFIED_WITHOUT_RECEIVER};
use crate::types::{
    CallArguments, ClassLiteralFlags, DataclassFlags, DataclassParams, KnownClass,
    KnownInstanceType, MemberLookupPolicy, SpecialFormType, StaticClassLiteral, SubclassOfType,
    Type, TypeContext, TypingModule,
    call::CallError,
    function::KnownFunction,
    infer::{
        InferenceFlags, TypeInferenceBuilder,
        builder::{DeclaredAndInferredType, DeferredExpressionState, TypeParamReification},
        original_class_type,
    },
    special_form::TypeQualifier,
};
use ruff_db::source::source_text;
use ruff_python_ast::{self as ast, helpers::any_over_expr};
use ty_module_resolver::{ImportingFile, KnownModule, file_to_module};
use ty_python_core::{definition::Definition, scope::NodeWithScopeRef};

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// basedpython: report what `class`'s reified type parameters cannot do.
    ///
    /// A read no receiver can answer: a class's type argument belongs to the
    /// instance that carries it, so the class body — and a static method, which
    /// is handed no receiver — has nothing to read one from. The transpiler
    /// refuses the same reads from the same syntactic answer, so a program that
    /// checks clean is one it can lower.
    ///
    /// And a declared variance, which reification has already decided.
    fn report_reified_class_errors(&mut self, class: &ast::StmtClassDef) {
        let source_type = self.file().source_type(self.db());
        if !source_type.is_basedpython() {
            return;
        }
        let source = source_text(self.db(), self.file());
        let reads = reified_class_reads(source.as_str(), source_type, class);

        // reification fixes the variance, so a declaration saying anything else
        // is a contradiction rather than a refinement — and the variance it
        // would override is what makes a bare construction solvable
        for type_param in class.type_params.iter().flat_map(|params| params.iter()) {
            if let ast::TypeParam::TypeVar(type_var) = type_param
                && type_var.variance.is_some()
                && reads.names.contains(&type_var.name.id)
                && let Some(builder) = self
                    .context
                    .report_lint(&INVALID_VARIANCE_DECLARATION, type_param)
            {
                let name = type_param.name();
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "Type parameter `{name}` cannot declare a variance"
                ));
                diagnostic.info(
                    "a reified type parameter is invariant: the program can read its type \
                     argument back, so two specializations match only when they were given \
                     the same one",
                );
            }
        }

        // `A[int]` is what records a class's type arguments, so a class that
        // answers that subscript itself leaves them nowhere to go
        if let Some(range) = reads.own_class_getitem
            && !reads.names.is_empty()
            && let Some(builder) = self.context.report_lint(&REIFIED_WITHOUT_RECEIVER, range)
        {
            let mut diagnostic = builder
                .into_diagnostic("A reified class cannot define `__class_getitem__`".to_string());
            diagnostic.info(
                "`A[int]` is what records the type arguments an instance carries, so the \
                 class cannot answer that subscript itself",
            );
        }

        for read in reads.unanswerable {
            let Some(builder) = self
                .context
                .report_lint(&REIFIED_WITHOUT_RECEIVER, read.range)
            else {
                continue;
            };
            let name = &read.name;
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "Type parameter `{name}` has no receiver to be read from"
            ));
            diagnostic.info(match read.reason {
                UnansweredReason::OutsideMethod => {
                    "a class's type argument belongs to the instance that carries it, and this \
                     runs while the class is still being built"
                }
                UnansweredReason::WithoutReceiver => {
                    "a class's type argument belongs to the instance that carries it, and a \
                     static method is handed none"
                }
            });
        }
    }

    pub(super) fn infer_class_body(&mut self, class: &ast::StmtClassDef) {
        self.infer_body(&class.body);
    }

    pub(super) fn infer_class_type_params(&mut self, class: &ast::StmtClassDef) {
        let type_params = class
            .type_params
            .as_deref()
            .expect("class type params scope without type params");

        let binding_context = self.index.expect_single_definition(class);
        let previous_typevar_binding_context =
            self.typevar_binding_context.replace(binding_context);

        self.infer_type_parameters(type_params, TypeParamReification::Class);
        self.report_reified_class_errors(class);

        if class.arguments.is_some() {
            let defer_class_args = self.in_stub() || self.is_basedpython_file();
            let previous_deferred_state = self.replace_deferred_state(defer_class_args.into());

            // PEP 695 class headers are inferred in the type-parameter scope, before the completed
            // class type is available. Infer the bases first because `extra_items=T` is an
            // annotation in `class C[T](TypedDict, extra_items=T)`, but an ordinary value argument
            // in `class C[T](Base, extra_items=T)`.
            let mut is_typed_dict = false;

            for base in class.bases() {
                let ty = if let Some(ty) =
                    self.infer_parameter_shape_class_base(base, defer_class_args)
                {
                    ty
                } else if let ast::Expr::Starred(starred) = base {
                    let ty = self.infer_expression(&starred.value, TypeContext::default());
                    self.store_expression_type(base, ty);
                    ty
                } else {
                    self.infer_expression(base, TypeContext::default())
                };
                is_typed_dict |= match ty {
                    ty if TypingModule::from_typed_dict_type(self.db(), ty).is_some() => true,
                    Type::ClassLiteral(class) => class.is_typed_dict(self.db()),
                    Type::GenericAlias(alias) => alias.is_typed_dict(self.db()),
                    _ => false,
                };
            }

            for keyword in class.keywords() {
                if is_typed_dict && keyword.arg.as_deref() == Some("extra_items") {
                    self.infer_extra_items_kwarg(&keyword.value);
                } else {
                    self.infer_expression(&keyword.value, TypeContext::default());
                }
            }

            self.deferred_state = previous_deferred_state;
        }

        self.typevar_binding_context = previous_typevar_binding_context;
    }

    pub(super) fn infer_class_definition_statement(&mut self, class: &ast::StmtClassDef) {
        self.infer_definition(class);
    }

    pub(super) fn infer_class_definition(
        &mut self,
        class_node: &ast::StmtClassDef,
        definition: Definition<'db>,
    ) {
        let env = self.program_environment();
        let ast::StmtClassDef {
            range: _,
            node_index: _,
            name,
            type_params,
            decorator_list,
            arguments: _,
            body: _,
        } = class_node;
        let db = self.db();

        let mut decorator_types_and_nodes: Vec<(Type<'db>, &ast::Decorator)> =
            Vec::with_capacity(decorator_list.len());
        let mut is_sealed = false;
        let source = ruff_db::source::source_text(db, self.file());
        for decorator in decorator_list {
            let decorator_ty = self.infer_decorator(decorator);
            // basedpython `enum class Foo` / `protocol Foo` / `sealed class Foo`
            // parse to synthetic `enum_class` / `protocol_class` / `sealed` marker
            // decorators (no `@` in the source). their effect is applied here (the
            // `sealed` flag) or in `explicit_bases` (the injected `Enum` /
            // `Protocol` base); the marker itself is not a runtime decorator, so
            // applying it would resolve to `Unknown` and poison the whole class type.
            // the visibility markers (`private`/`export`/`open`) are transpile-time
            // only — a rename or `__all__` entry, with no type-level effect — so they
            // are skipped here for the same reason. (`final`/`abstract`/`data` carry
            // real type semantics and are recognized further down instead.)
            if let ast::Expr::Name(name) = &decorator.expression
                && matches!(
                    name.id.as_str(),
                    "enum_class"
                        | "protocol_class"
                        | "sealed"
                        | "enum_def"
                        | "extension_def"
                        | "build_def"
                        | "variant_unit"
                        | "variant_tuple"
                        | "private"
                        | "export"
                        | "open"
                )
                && source
                    .as_bytes()
                    .get(usize::from(decorator.range.start()))
                    .copied()
                    != Some(b'@')
            {
                if name.id.as_str() == "sealed" {
                    is_sealed = true;
                }
                continue;
            }
            decorator_types_and_nodes.push((decorator_ty, decorator));
        }

        let body_scope = self
            .index
            .node_scope(NodeWithScopeRef::Class(class_node))
            .to_scope_id(db, self.program_file());

        let file = self.program_file();
        let importing_file = ImportingFile::File(file.file(db), env.resolver_environment(db));
        let maybe_known_class = KnownClass::try_from_file_and_name(db, importing_file, name);

        let known_module = || {
            file_to_module(db, importing_file.resolver_file(db)).and_then(|module| module.known(db))
        };
        let in_typing_module = || {
            matches!(
                known_module(),
                Some(KnownModule::Typing | KnownModule::TypingExtensions)
            )
        };

        let mut decorators_to_apply = Vec::with_capacity(decorator_types_and_nodes.len());
        let mut metadata_applies_to_original_class = true;
        let mut deprecated = None;
        let mut type_check_only = false;
        let mut dataclass_params = None;
        // based-enum payload variants construct like frozen dataclasses: ty
        // synthesizes a positional `__init__` from their annotated fields. unit
        // variants are payload-less *values* and are modelled as enum-literal
        // members (see `enum_metadata`), so they need no dataclass synthesis
        if class_node.has_synthetic_marker("variant_tuple") {
            dataclass_params = Some(DataclassParams::from_flags(
                db,
                env,
                DataclassFlags::default() | DataclassFlags::FROZEN,
            ));
        }
        let mut dataclass_transformer_params = None;
        let mut total_ordering = false;
        // a basedpython header with no argument list can still have bases — an
        // implementation's interface, a based enum's `Enum` — so the flag has to
        // account for them, or the metaclass fast path below reads the class as
        // base-less and answers `type`
        let has_explicit_bases = class_node.has_injected_base()
            || class_node
                .arguments
                .as_deref()
                .is_some_and(|arguments| !arguments.args.is_empty());
        let has_explicit_metaclass = class_node
            .arguments
            .as_deref()
            .is_some_and(|arguments| arguments.find_keyword("metaclass").is_some());
        let mut class_flags = ClassLiteralFlags::empty();
        class_flags.set(ClassLiteralFlags::SEALED, is_sealed);
        class_flags.set(
            ClassLiteralFlags::HAS_DECORATORS,
            !class_node.decorator_list.is_empty(),
        );
        class_flags.set(
            ClassLiteralFlags::HAS_TYPE_PARAMS,
            class_node.type_params.is_some(),
        );
        class_flags.set(ClassLiteralFlags::HAS_EXPLICIT_BASES, has_explicit_bases);
        class_flags.set(
            ClassLiteralFlags::IS_ENUM_VARIANT,
            class_node.is_enum_variant(),
        );
        class_flags.set(ClassLiteralFlags::IS_EXTENSION, class_node.is_extension());
        class_flags.set(
            ClassLiteralFlags::HAS_EXPLICIT_METACLASS,
            has_explicit_metaclass,
        );
        let class_name = name.id.clone();
        let infer_original_class_ty = |deprecated,
                                       type_check_only,
                                       dataclass_params,
                                       dataclass_transformer_params,
                                       total_ordering| {
            match (maybe_known_class, &*name.id) {
                (None, "NamedTuple") if in_typing_module() => {
                    Type::SpecialForm(SpecialFormType::NamedTuple)
                }
                (None, "Any") if in_typing_module() => Type::SpecialForm(SpecialFormType::Any),
                // `_collections_abc` gives `Callable` a real class definition, the way
                // the runtime does; `typing` and `collections.abc` both re-export it.
                // ty models a subscripted `Callable` as its own callable type rather
                // than an instance of that class, so the name denotes the special form
                (None, "Callable")
                    if known_module() == Some(KnownModule::CollectionsAbcInternal) =>
                {
                    Type::SpecialForm(SpecialFormType::CollectionsAbcCallable)
                }
                (None, "InitVar") if known_module() == Some(KnownModule::Dataclasses) => {
                    Type::SpecialForm(SpecialFormType::TypeQualifier(TypeQualifier::InitVar))
                }
                _ => Type::from(StaticClassLiteral::new(
                    db,
                    &class_name,
                    body_scope,
                    maybe_known_class,
                    deprecated,
                    type_check_only,
                    dataclass_params,
                    dataclass_transformer_params,
                    total_ordering,
                    class_flags,
                )),
            }
        };
        // In the first pass, collect metadata decorators that shape the original class object.
        // Once an inner decorator replaces the public binding, outer decorators are ordinary
        // runtime applications only: they cannot retroactively add metadata to the original class.
        // For ordinary decorators that still apply to the original class, precompute the call so
        // the second pass can reuse it if no inner decorator has changed the binding.
        for &(decorator_ty, decorator) in decorator_types_and_nodes.iter().rev() {
            if !metadata_applies_to_original_class {
                decorators_to_apply.push((decorator_ty, decorator, None));
                continue;
            }

            if decorator_ty
                .as_function_literal()
                .is_some_and(|function| function.is_known(db, KnownFunction::Dataclass))
            {
                dataclass_params = Some(DataclassParams::default_params(db, env));
                continue;
            }

            if decorator_ty
                .as_function_literal()
                .is_some_and(|function| function.is_known(db, KnownFunction::TotalOrdering))
            {
                total_ordering = true;
                continue;
            }

            if let Type::DataclassDecorator(params) = decorator_ty {
                dataclass_params = Some(params);
                continue;
            }

            if decorator_ty.is_unknown()
                && let ast::Expr::Call(call) = &decorator.expression
                && self
                    .expression_type(&call.func)
                    .as_function_literal()
                    .is_some_and(|function| function.is_known(db, KnownFunction::Dataclass))
            {
                continue;
            }

            if let Type::KnownInstance(KnownInstanceType::Deprecated(deprecated_inst)) =
                decorator_ty
            {
                deprecated = Some(deprecated_inst);
                continue;
            }

            if decorator_ty
                .as_function_literal()
                .is_some_and(|function| function.is_known(db, KnownFunction::TypeCheckOnly))
            {
                type_check_only = true;
                continue;
            }

            // Skip identity decorators to avoid salsa cycles on typeshed.
            if decorator_ty.as_function_literal().is_some_and(|function| {
                matches!(
                    function.known(db),
                    Some(
                        KnownFunction::Final
                            | KnownFunction::DisjointBase
                            | KnownFunction::RuntimeCheckable
                    )
                )
            }) {
                continue;
            }

            if let Type::FunctionLiteral(f) = decorator_ty {
                // We do not yet detect or flag `@dataclass_transform` applied to more than one
                // overload, or an overload and the implementation both. Nevertheless, this is not
                // allowed. We do not try to treat the offenders intelligently -- just use the
                // params of the last seen usage of `@dataclass_transform`.
                //
                // In class-decorator position, dataclass-transform metadata shapes the
                // original class object. We keep it metadata-only here because the call path
                // uses synthetic dataclass-transform return types to model decorator factories;
                // treating this as an ordinary replacement-returning class decorator would
                // conflate those two cases.
                let transformer_params = f
                    .iter_overloads_and_implementation(db)
                    .rev()
                    .find_map(|overload| overload.dataclass_transformer_params(db));
                if let Some(transformer_params) = transformer_params {
                    dataclass_params = Some(DataclassParams::from_transformer_params(
                        db,
                        transformer_params,
                    ));
                    continue;
                }
            }

            if let Type::DataclassTransformer(params) = decorator_ty {
                dataclass_transformer_params = Some(params);
                continue;
            }

            let original_class_ty = infer_original_class_ty(
                deprecated,
                type_check_only,
                dataclass_params,
                dataclass_transformer_params,
                total_ordering,
            );
            let decorator_result = apply_class_decorator(db, env, decorator_ty, original_class_ty);
            let decorated_ty = match &decorator_result {
                Ok(return_ty) => *return_ty,
                Err(error) => error.return_type(db, env),
            };
            if !is_unknown_decorator_result(db, decorated_ty)
                && !type_retains_original_class(db, env, original_class_ty, decorated_ty)
            {
                metadata_applies_to_original_class = false;
            }

            decorators_to_apply.push((
                decorator_ty,
                decorator,
                Some((original_class_ty, decorator_result)),
            ));
        }

        let mut inferred_ty = infer_original_class_ty(
            deprecated,
            type_check_only,
            dataclass_params,
            dataclass_transformer_params,
            total_ordering,
        );

        let original_class_ty = inferred_ty;
        let mut undecorated_ty = None;

        // In the second pass, apply class decorators from inner to outer and use their return types
        // to update the public binding. `original_class_ty` remains the class object whose body and
        // metadata were inferred above.
        for (decorator_ty, decorator_node, precomputed_result) in decorators_to_apply {
            let decorator_result = match precomputed_result {
                // The metadata pass already called this decorator with the same input. If an inner
                // decorator changed the binding, apply this decorator to the new public binding.
                Some((precomputed_input_ty, decorator_result))
                    if precomputed_input_ty == inferred_ty =>
                {
                    decorator_result
                }
                _ => apply_class_decorator(db, env, decorator_ty, inferred_ty),
            };
            let decorated_ty = match decorator_result {
                Ok(return_ty) => return_ty,
                Err(CallError(_, bindings)) => {
                    self.defer_decorator_call(decorator_node, inferred_ty);
                    bindings.return_type(db, env)
                }
            };
            let decorated_ty = match decorated_ty {
                Type::DataclassDecorator(_) | Type::DataclassTransformer(_) => Type::unknown(),
                decorated_ty => decorated_ty,
            };
            inferred_ty = if is_unknown_decorator_result(db, decorated_ty) {
                inferred_ty
            } else if class_decorator_preserves_class_binding(
                db,
                env,
                original_class_ty,
                decorated_ty,
            ) {
                merge_class_preserving_decorator_result(
                    db,
                    env,
                    original_class_ty,
                    inferred_ty,
                    decorated_ty,
                )
            } else {
                // Only record an undecorated type once a decorator actually replaces the public
                // binding. If all decorators preserve the class, there is no alternate class type
                // to expose.
                undecorated_ty.get_or_insert(inferred_ty);
                decorated_ty
            };
        }

        self.undecorated_type = undecorated_ty;

        self.add_declaration_with_binding(
            class_node.into(),
            definition,
            &DeclaredAndInferredType::are_the_same_type(inferred_ty),
        );

        // if there are type parameters, then the keywords and bases are within that scope
        // and we don't need to run inference here
        if type_params.is_none() {
            // In stub files (and basedpython files, where self-refs are auto-quoted by the
            // transpiler), keyword values may reference names that are defined later.
            let defer_class_args = self.in_stub() || self.is_basedpython_file();
            let previous_deferred_state = self.replace_deferred_state(defer_class_args.into());
            for keyword in class_node.keywords() {
                if keyword.arg.as_deref() != Some("extra_items") {
                    self.infer_expression(&keyword.value, TypeContext::default());
                }
            }
            self.deferred_state = previous_deferred_state;

            // Inference of bases deferred in stubs/basedpython, or if any are string literals.
            if defer_class_args
                || class_node
                    .bases()
                    .iter()
                    .any(|expr| any_over_expr(expr, &ast::Expr::is_string_literal_expr))
                || class_node
                    .arguments
                    .as_deref()
                    .and_then(|args| args.find_keyword("extra_items"))
                    .is_some()
            {
                self.deferred.insert(definition);
            } else {
                let previous_typevar_binding_context =
                    self.typevar_binding_context.replace(definition);
                for base in class_node.bases() {
                    if self
                        .infer_parameter_shape_class_base(base, /* deferred = */ false)
                        .is_none()
                    {
                        self.infer_expression(base, TypeContext::default());
                    }
                }
                self.typevar_binding_context = previous_typevar_binding_context;
            }
        }
    }

    /// basedpython: resolve a class base written in the callable-parameter tuple form
    ///
    /// `(*: int)` is how basedpython spells `tuple[int, ...]`, and it is what the reverse
    /// transpiler writes into the `.byi` typeshed — `class _flags(_UninstantiableStructseq,
    /// (*: int))` is `sys.flags`. the form is type-only: no runtime value is ever spelled
    /// that way. inferring it as a value therefore reads its `*: int` element as an unpack
    /// in value position and yields `Unknown`, which as a base makes the class assignable
    /// to every type. so resolve the base as a type expression, exactly as the transpiler
    /// lowers it, and hand back the class the resulting instance is an instance of, which
    /// is the shape a base list needs.
    ///
    /// returns `None` for every base this does not apply to, leaving the caller's ordinary
    /// value inference to run
    fn infer_parameter_shape_class_base(
        &mut self,
        base: &ast::Expr,
        deferred: bool,
    ) -> Option<Type<'db>> {
        if !self.is_basedpython_file() {
            return None;
        }
        let ast::Expr::Tuple(tuple) = base else {
            return None;
        };
        if !tuple.has_parameter_shape() {
            return None;
        }
        let previous_deferred_state = if deferred {
            Some(std::mem::replace(
                &mut self.deferred_state,
                DeferredExpressionState::Deferred,
            ))
        } else {
            None
        };
        let ty = self.infer_type_expression_unstored(base);
        if let Some(previous) = previous_deferred_state {
            self.deferred_state = previous;
        }
        // a base that does not denote a class is recorded as it is, so the ordinary
        // `invalid-base` report fires instead of the type silently going missing
        let base_ty = ty
            .nominal_class(self.db(), self.program_environment())
            .map_or(ty, Type::from);
        self.store_expression_type(base, base_ty);
        Some(base_ty)
    }

    pub(super) fn infer_class_deferred(
        &mut self,
        definition: Definition<'db>,
        class: &ast::StmtClassDef,
    ) {
        let previous_typevar_binding_context = self.typevar_binding_context.replace(definition);
        let defer_class_args = self.in_stub() || self.is_basedpython_file();
        // resolve bases with `IN_TYPE_EXPRESSION` set so the basedpython
        // `dynamic` keyword resolves to `Any` in a base list (`class C(dynamic)`),
        // matching the transpiler's `dynamic → Any` rewrite. only `dynamic` name
        // resolution consults this flag, so other bases are unaffected
        let previously_in_type_expression = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_TYPE_EXPRESSION, true);
        for base in class.bases() {
            if self
                .infer_parameter_shape_class_base(base, defer_class_args)
                .is_some()
            {
                continue;
            }
            if defer_class_args {
                self.infer_expression_with_state(
                    base,
                    TypeContext::default(),
                    DeferredExpressionState::Deferred,
                );
            } else {
                self.infer_expression(base, TypeContext::default());
            }
        }
        self.context.inference_flags.set(
            InferenceFlags::IN_TYPE_EXPRESSION,
            previously_in_type_expression,
        );

        if let Some(arguments) = class.arguments.as_deref()
            && let Some(extra_items_keyword) = arguments.find_keyword("extra_items")
        {
            if original_class_type(self.db(), definition)
                .is_some_and(|class_literal| class_literal.is_typed_dict(self.db()))
            {
                self.infer_extra_items_kwarg(&extra_items_keyword.value);
            } else if defer_class_args {
                self.infer_expression_with_state(
                    &extra_items_keyword.value,
                    TypeContext::default(),
                    DeferredExpressionState::Deferred,
                );
            } else {
                self.infer_expression(&extra_items_keyword.value, TypeContext::default());
            }
        }

        self.typevar_binding_context = previous_typevar_binding_context;
    }
}

fn apply_class_decorator<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    decorator_ty: Type<'db>,
    decorated_ty: Type<'db>,
) -> Result<Type<'db>, CallError<'db>> {
    let call_arguments = CallArguments::positional([decorated_ty]);
    decorator_ty
        .try_call(db, env, &call_arguments)
        .map(|bindings| bindings.return_type(db, env))
}

/// Return true if a decorator result still binds the name to the original class.
///
/// For example, an identity decorator keeps the public name bound to the same class:
/// ```python
/// def identity[T](cls: type[T]) -> type[T]:
///     return cls
///
/// @identity
/// class C: ...
/// ```
///
/// This also accepts metaclass-shaped results such as `type[C]`, because those still describe the
/// original class object even if the decorator call produced a `SubclassOf` type internally.
fn class_decorator_preserves_class_binding<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    original_class: Type<'db>,
    decorated_class: Type<'db>,
) -> bool {
    let Type::ClassLiteral(original_literal) = original_class else {
        return false;
    };

    match decorated_class {
        Type::ClassLiteral(decorated_literal) => {
            let decorated_definition = decorated_literal.definition(db);
            decorated_literal == original_literal
                || decorated_definition.is_some()
                    && decorated_definition == original_literal.definition(db)
        }
        Type::SubclassOf(subclass_of) => subclass_of
            .subclass_of()
            .into_class(db, env)
            .is_some_and(|class| class == original_literal.default_specialization(db)),
        Type::Divergent(_) => true,
        Type::Union(union) => union.elements(db).iter().all(|element| {
            class_decorator_preserves_class_binding(db, env, original_class, *element)
        }),
        Type::TypeAlias(alias) => {
            class_decorator_preserves_class_binding(db, env, original_class, alias.value_type(db))
        }
        _ => SubclassOfType::try_from_type(db, env, original_class).is_some_and(
            |original_meta_type| decorated_class.is_equivalent_to(db, env, original_meta_type),
        ),
    }
}

/// Return true if a type still contains the original class object, even if it also carries extra
/// intersection members.
fn type_retains_original_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    original_class: Type<'db>,
    decorated_class: Type<'db>,
) -> bool {
    match decorated_class {
        Type::Intersection(intersection) => intersection
            .positive(db)
            .iter()
            .any(|element| type_retains_original_class(db, env, original_class, *element)),
        Type::Union(union) => union
            .elements(db)
            .iter()
            .all(|element| type_retains_original_class(db, env, original_class, *element)),
        Type::TypeAlias(alias) => {
            type_retains_original_class(db, env, original_class, alias.value_type(db))
        }
        _ => class_decorator_preserves_class_binding(db, env, original_class, decorated_class),
    }
}

/// Return true if a class-decorator result should leave the current binding unchanged.
///
/// This also handles `type[Unknown]` results from generic decorator factories whose type
/// variables are specialized before the returned decorator receives the class. Explicit `Any`
/// results do not trigger this fallback.
fn is_unknown_decorator_result<'db>(db: &'db dyn Db, result_ty: Type<'db>) -> bool {
    match result_ty.resolve_type_alias(db) {
        Type::SubclassOf(subclass_of) => subclass_of
            .subclass_of()
            .into_dynamic()
            .is_some_and(|dynamic| Type::Dynamic(dynamic).is_unknown()),
        result_ty => result_ty.is_unknown(),
    }
}

/// Merge a class-preserving decorator result into the public binding.
///
/// If earlier decorators already exposed extra members through an intersection, keep those
/// members instead of collapsing back to the undecorated class when a later decorator simply
/// returns the original class object again.
fn merge_class_preserving_decorator_result<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    original_class: Type<'db>,
    current_binding: Type<'db>,
    decorated_binding: Type<'db>,
) -> Type<'db> {
    if current_binding == original_class
        || type_retains_original_class(db, env, original_class, current_binding)
    {
        current_binding
    } else {
        decorated_binding
            .as_class_literal()
            .map(Type::ClassLiteral)
            .unwrap_or(original_class)
    }
}
