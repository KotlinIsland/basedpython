use itertools::{Either, EitherOrBoth, Itertools};
use ruff_db::diagnostic::{Annotation, Diagnostic, Span};
use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, ArgOrKeyword, ExprContext, ParameterBorrow};
use ruff_text_size::Ranged;
use ty_module_resolver::file_to_module;

use super::TypeInferenceBuilder;
use crate::place::{DefinedPlace, Definedness, Place};
use crate::types::call::Argument;
use crate::types::call::CallErrorKind;
use crate::types::call::bind::CallableDescription;
use crate::types::constraints::ConstraintSetBuilder;
use crate::types::diagnostic::{
    CALL_NON_CALLABLE, INVALID_ARGUMENT_TYPE, INVALID_ASSIGNMENT, INVALID_KEY,
    INVALID_TYPE_ARGUMENTS, INVALID_TYPE_FORM, NOT_SUBSCRIPTABLE, POSSIBLY_MISSING_IMPLICIT_CALL,
    TypedDictDeleteErrorKind, report_cannot_delete_typed_dict_key,
    report_invalid_arguments_to_annotated, report_not_subscriptable,
};
use crate::types::generics::{GenericContext, bind_typevar};
use crate::types::infer::builder::annotation_expression::PEP613Policy;
use crate::types::infer::builder::type_expression::{
    resolve_use_site_variance_class, use_site_variance_slice_elements,
};
use crate::types::infer::builder::{ArgExpr, ArgumentsIter, MultiInferenceGuard};
use crate::types::infer::{InferenceFlags, TypeExpressionFlags};
use crate::types::match_type::{MatchTypeOutcome, evaluate_match_type};
use crate::types::regex;
use crate::types::special_form::AliasSpec;
use crate::types::subscript::{
    DunderMethod, LegacyGenericOrigin, SubscriptError, SubscriptErrorKind,
};
use crate::types::tuple::{Tuple, TupleSpecBuilder, TupleType, VariableSegment};
use crate::types::typed_dict::{
    TypedDictAssignmentKind, TypedDictExtraItems, TypedDictKeyAssignment,
};
use crate::types::typevar::TypeVarSet;
use crate::types::typevar::pack_bound_violation;
use crate::types::{
    BoundTypeVarInstance, CallArguments, CallDunderError, CallableBinding, CycleDetector,
    DynamicType, InternedType, KnownClass, KnownInstanceType, LintDiagnosticGuard,
    MemberLookupPolicy, Parameter, Parameters, SpecialFormType, StaticClassLiteral, Type,
    TypeAliasType, TypeAndQualifiers, TypeContext, TypeVarBoundOrConstraints, UnionType,
    UnionTypeInstance, any_over_type, todo_type,
};
use crate::{Db, FxOrderSet, ProgramEnvironment};
use ty_python_core::definition::Definition;
use ty_python_core::place::PlaceExpr;
use ty_python_core::scope::FileScopeId;
use ty_python_core::{SemanticIndex, place_table};

/// basedpython: the elements of a subscript that carries at least one keyword
/// field (`x[a, z=1]`). `None` for every all-positional subscript, which is
/// ordinary python and keeps its ordinary meaning.
///
/// The parser spells a keyword field as a named expression whose target is an
/// [`ast::ExprContext::Invalid`] name.
fn keyword_subscript_elements(subscript: &ast::ExprSubscript) -> Option<&[ast::Expr]> {
    let elements = match subscript.slice.as_ref() {
        ast::Expr::Tuple(tuple) if !tuple.parenthesized => &*tuple.elts,
        single => std::slice::from_ref(single),
    };
    elements
        .iter()
        .any(|element| matches!(element, ast::Expr::Named(named) if named.label().is_some()))
        .then_some(elements)
}

/// Given a string literal or a union of string literals, return an iterator over the contained
/// strings, or `None` if the type is neither.
fn string_literal_values<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
) -> Option<impl Iterator<Item = &'db str> + 'db> {
    if let Some(literal) = ty.as_string_literal() {
        Some(Either::Left(std::iter::once(literal.value(db))))
    } else {
        let elements = ty.as_union()?.elements(db);
        elements
            .iter()
            .all(|ty| ty.as_string_literal().is_some())
            .then(|| {
                Either::Right(
                    elements
                        .iter()
                        .filter_map(|ty| ty.as_string_literal().map(|lit| lit.value(db))),
                )
            })
    }
}

/// Points a diagnostic at where `typevar` was declared.
fn add_typevar_definition<'db>(
    db: &'db dyn Db,
    diagnostic: &mut Diagnostic,
    typevar: BoundTypeVarInstance<'db>,
) {
    let Some(definition) = typevar.typevar(db).definition(db) else {
        return;
    };
    let file = definition.file(db);
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let range = definition.focus_range(db, &module).range();
    diagnostic.annotate(
        Annotation::secondary(Span::from(file).with_range(range))
            .message("Type variable defined here"),
    );
}

impl<'db, 'ast> TypeInferenceBuilder<'db, 'ast> {
    /// basedpython: checks a variadic pack's specialization against its declared upper bound,
    /// reporting on `node`. Returns whether the bound was violated.
    fn check_pack_bound(
        &mut self,
        typevar: BoundTypeVarInstance<'db>,
        provided: Type<'db>,
        node: impl Ranged,
    ) -> bool {
        let env = self.program_environment();
        let db = self.db();
        let Some(violation) = pack_bound_violation(
            db,
            env,
            typevar,
            provided,
            &ConstraintSetBuilder::new(),
            TypeVarSet::None,
        ) else {
            return false;
        };
        if let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, node) {
            let mut diagnostic = builder.into_diagnostic(violation.message(db, env, typevar));
            add_typevar_definition(db, &mut diagnostic, typevar);
            violation.attach_context(db, env, typevar, &mut diagnostic);
        }
        true
    }

    pub(super) fn typed_dict_key_expected_type(&self, ty: Type<'db>) -> Option<Type<'db>> {
        struct TypedDictKeyExpectedType;
        type TypedDictKeyExpectedTypeVisitor<'db> =
            CycleDetector<'db, TypedDictKeyExpectedType, Type<'db>, Option<Type<'db>>, 3>;

        fn imp<'db>(
            db: &'db dyn Db,
            env: &ProgramEnvironment<'db>,
            ty: Type<'db>,
            visitor: &TypedDictKeyExpectedTypeVisitor<'db>,
        ) -> Option<Type<'db>> {
            match ty {
                Type::TypedDict(typed_dict) => {
                    if typed_dict.explicit_extra_items(db).is_some() {
                        return Some(KnownClass::Str.to_instance(db, env));
                    }
                    let keys = typed_dict
                        .items(db)
                        .keys()
                        .map(|key| Type::string_literal(db, key))
                        .collect_vec();
                    (!keys.is_empty()).then(|| UnionType::from_elements(db, env, keys))
                }
                Type::Union(union) => {
                    let keys = union
                        .elements(db)
                        .iter()
                        .filter_map(|element| imp(db, env, *element, visitor))
                        .collect_vec();
                    (!keys.is_empty()).then(|| UnionType::from_elements(db, env, keys))
                }
                Type::Intersection(intersection) => {
                    let keys = intersection
                        .positive(db)
                        .iter()
                        .filter_map(|element| imp(db, env, *element, visitor))
                        .collect_vec();
                    (!keys.is_empty()).then(|| UnionType::from_elements(db, env, keys))
                }
                Type::TypeAlias(alias) => {
                    visitor.visit(db, ty, || imp(db, env, alias.value_type(db), visitor))
                }
                _ => None,
            }
        }
        let db = self.db();

        imp(
            db,
            self.program_environment(),
            ty,
            &TypedDictKeyExpectedTypeVisitor::default(),
        )
    }

    fn store_typed_dict_key_expected_type(&mut self, slice: &ast::Expr, value_ty: Type<'db>) {
        if let Some(expected_key_ty) = self.typed_dict_key_expected_type(value_ty) {
            self.store_expected_type(slice, expected_key_ty);
        }
    }

    pub(super) fn infer_subscript_expression(
        &mut self,
        subscript: &ast::ExprSubscript,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let ast::ExprSubscript {
            value,
            slice,
            range: _,
            node_index: _,
            ctx,
            is_typeof: _,
        } = subscript;

        // basedpython: `super[T]` is sugar for `super(<MRO predecessor of T>, self)`
        // and must not be type-checked as a regular subscript expression. infer
        // the slice as a type so `reveal_type` etc. behave, then return the
        // pivot class type — the attribute lookup against the bound super is
        // performed in `infer_attribute_load`
        if self.is_basedpython_file()
            && let ast::Expr::Name(name) = value.as_ref()
            && name.id.as_str() == "super"
        {
            self.infer_expression(value, TypeContext::default());
            let slice_ty = self.infer_expression(slice, TypeContext::default());
            return slice_ty;
        }

        match ctx {
            ExprContext::Load => self
                .infer_subscript_load(subscript, tcx)
                .unwrap_or_else(|recovery_ty| recovery_ty),
            ExprContext::Store => {
                let value_ty = self.infer_expression(value, TypeContext::default());
                self.store_typed_dict_key_expected_type(slice, value_ty);
                let slice_ty = self.infer_expression(slice, TypeContext::default());
                let _ = self.infer_subscript_expression_types(
                    subscript,
                    value_ty,
                    slice_ty,
                    *ctx,
                    TypeContext::default(),
                );
                Type::Never
            }
            ExprContext::Del => {
                let value_ty = self.infer_expression(value, TypeContext::default());
                self.store_typed_dict_key_expected_type(slice, value_ty);
                let slice_ty = self.infer_expression(slice, TypeContext::default());
                self.validate_subscript_deletion(subscript, value_ty, slice_ty);
                Type::Never
            }
            ExprContext::Invalid => {
                let value_ty = self.infer_expression(value, TypeContext::default());
                let slice_ty = self.infer_expression(slice, TypeContext::default());
                let _ = self.infer_subscript_expression_types(
                    subscript,
                    value_ty,
                    slice_ty,
                    *ctx,
                    TypeContext::default(),
                );
                Type::unknown()
            }
        }
    }

    /// Infer a subscript load, returning its inferred type when the subscription succeeds.
    ///
    /// If the subscription fails, report the error and return the type that should be used to
    /// continue inference. This recovery type may be `Unknown` or, for example, the return type of
    /// `__getitem__` when its arguments are invalid. Keeping it separate from a successful result
    /// lets augmented assignments check their right-hand side without attempting a failed store.
    pub(super) fn infer_subscript_load(
        &mut self,
        subscript: &ast::ExprSubscript,
        tcx: TypeContext<'db>,
    ) -> Result<Type<'db>, Type<'db>> {
        let value_ty = self.infer_expression(&subscript.value, TypeContext::default());

        // basedpython: `F[int]` where `F` is a `type def` is a *type* expression form.
        // used as a value it has no runtime meaning — the declaration is erased by the
        // transpiler, so the emitted python would raise `NameError`
        if value_ty.is_type_fn(self.db()) {
            self.infer_expression(&subscript.slice, TypeContext::default());
            if let Some(builder) = self
                .context
                .report_lint(&crate::types::diagnostic::INVALID_TYPE_FORM, subscript)
            {
                builder.into_diagnostic(
                    "a `type def` can only be applied in a type expression, not used as a value",
                );
            }
            return Err(Type::unknown());
        }

        // basedpython `a?.b[0]`: the `?.` short-circuit covers the subscript too, matching the
        // `None if a is None else a.b[0]` lowering, so index the present-receiver value and
        // let the `None` ride out to the end of the chain
        let (value_ty, in_chain) = self.basedpython_chain_receiver(&subscript.value, value_ty);

        // If we have an implicit type alias like `MyList = list[T]`, and if `MyList` is being
        // used in another implicit type alias like `Numbers = MyList[int]`, then we infer the
        // right hand side as a value expression, and need to handle the specialization here.
        if value_ty.is_generic_alias() {
            return Ok(self.infer_explicit_type_alias_specialization(subscript, value_ty, false));
        }
        let loaded = self.infer_subscript_load_impl(value_ty, subscript, tcx);
        let ty = loaded.unwrap_or_else(|recovery_ty| recovery_ty);

        // `m[1]` goes through `Match.__getitem__`, whose stub can only say
        // `AnyStr | None`; the pattern says which of the two it is
        let ty = self
            .refine_regex_subscript(value_ty, subscript)
            .unwrap_or(ty);

        let ty = self.basedpython_chain_result(subscript, ty, in_chain);
        loaded.map(|_| ty).map_err(|_| ty)
    }

    /// The type of `m[key]` for a `re.Match` whose capture groups are known.
    fn refine_regex_subscript(
        &self,
        value_ty: Type<'db>,
        subscript: &ast::ExprSubscript,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let groups = regex::groups_of(db, value_ty)?;
        let any_str = regex::any_str_of(db, env, value_ty)?;
        let key = self.regex_group_key(&subscript.slice)?;
        match regex::group_type(db, env, groups, any_str, key) {
            Ok(ty) => Some(ty),
            Err(_) => {
                self.report_no_such_regex_group((&*subscript.slice).into(), key);
                Some(Type::unknown())
            }
        }
    }

    fn infer_subscript_load_impl(
        &mut self,
        value_ty: Type<'db>,
        subscript: &ast::ExprSubscript,
        tcx: TypeContext<'db>,
    ) -> Result<Type<'db>, Type<'db>> {
        let env = self.program_environment();
        let db = self.db();

        let ast::ExprSubscript {
            range: _,
            node_index: _,
            value: _,
            slice,
            ctx: _,
            is_typeof: _,
        } = subscript;

        self.store_typed_dict_key_expected_type(slice, value_ty);

        // basedpython use-site variance: `Container[in T]` projects reads
        // through T to `object` — the typevar appears in `__getitem__`'s
        // return (a covariant position), and a contravariantly-projected T
        // materializes to `object` there. We don't reject the read outright
        // (mirroring kotlin's `Container<in T>` where reads give `Any?`); the
        // returned `object` then forces narrower-typed targets to fail by
        // normal assignability rules.
        if instance_has_contravariant_projection(db, env, value_ty) {
            // still infer the slice to surface other diagnostics
            let _ = self.infer_expression(slice, TypeContext::default());
            return Err(Type::object());
        }

        let mut constraint_keys = vec![];

        // If `value` is a valid reference, we attempt type narrowing by assignment.
        if !value_ty.is_unknown() {
            if let Some(expr) = PlaceExpr::try_from_expr(subscript) {
                let (place, keys) = self.infer_place_load(expr, ast::ExprRef::Subscript(subscript));
                constraint_keys.extend(keys);
                if let Place::Defined(DefinedPlace {
                    ty,
                    definedness: Definedness::AlwaysDefined,
                    ..
                }) = place.place
                {
                    // Even if we can obtain the subscript type based on the assignments, we still perform default type inference
                    // (to store the expression type and to report errors).
                    let slice_ty = self.infer_expression(slice, TypeContext::default());
                    return self
                        .infer_subscript_expression_types(
                            subscript,
                            value_ty,
                            slice_ty,
                            ExprContext::Load,
                            TypeContext::default(),
                        )
                        .map(|_| ty)
                        .map_err(|_| ty);
                }
            }
        }

        let tuple_generic_alias = |env: &ProgramEnvironment<'db>, tuple: Option<TupleType<'db>>| {
            let tuple = tuple.unwrap_or_else(|| TupleType::homogeneous(db, env, Type::unknown()));
            Type::from(tuple.to_class_type(db))
        };

        match value_ty {
            Type::ClassLiteral(class) => {
                // basedpython use-site variance in *value* position — the
                // `is` target of a parametric type test (`x is A[out int]`).
                // Produce the same projected specialization the annotation
                // form does, as a class object rather than an instance, so
                // the test resolves against the projection instead of losing
                // it to ordinary subscript inference
                if self.is_basedpython_file()
                    && let Some(elements) = use_site_variance_slice_elements(slice)
                {
                    let class_type =
                        resolve_use_site_variance_class(db, value_ty, &elements, |elt| {
                            self.infer_type_expression(elt)
                        });
                    return Ok(class_type.map_or_else(Type::unknown, Type::from));
                }

                // HACK ALERT: If we are subscripting a generic class, short-circuit the rest of the
                // subscript inference logic and treat this as an explicit specialization.
                // TODO: Move this logic into a custom callable, and update `find_name_in_mro` to return
                // this callable as the `__class_getitem__` method on `type`. That probably requires
                // updating all of the subscript logic below to use custom callables for all of the _other_
                // special cases, too.
                if class.is_tuple(db) {
                    return Ok(tuple_generic_alias(
                        env,
                        self.infer_tuple_type_expression(subscript),
                    ));
                } else if class.is_known(db, KnownClass::Type) {
                    let argument_ty = self.infer_type_expression(slice);
                    return Ok(Type::KnownInstance(KnownInstanceType::TypeGenericAlias(
                        InternedType::new(db, argument_ty),
                    )));
                }

                if let Some(generic_context) = class.generic_context(db)
                    && let Some(class) = class.as_static()
                {
                    return Ok(self.infer_explicit_class_specialization(
                        subscript,
                        value_ty,
                        class,
                        generic_context,
                    ));
                }
            }
            Type::FunctionLiteral(function) => {
                let signature = function.signature(db);
                if let Some(overload) = signature.overloads.first()
                    && let Some(generic_context) = overload.generic_context
                {
                    return Ok(self.infer_explicit_function_specialization(
                        subscript,
                        value_ty,
                        generic_context,
                    ));
                }
            }
            // basedpython: a reified generic *method* is specialized through
            // `obj.m[int]` just like a free function. the explicit
            // specialization applies to the underlying function and re-wraps as
            // a bound method; gated on reification so erased generic methods are
            // unaffected. a classmethod is excluded — its binding is opaque at
            // runtime (reported at the def site by `reified-classmethod`), so
            // the subscript falls through to the ordinary non-subscriptable path
            Type::BoundMethod(bound_method) => {
                let function = bound_method.function(db);
                let signature = function.signature(db);
                if function.is_reified(db)
                    && !function.is_classmethod(db)
                    && let Some(overload) = signature.overloads.first()
                    && let Some(generic_context) = overload.generic_context
                {
                    return Ok(self.infer_explicit_function_specialization(
                        subscript,
                        value_ty,
                        generic_context,
                    ));
                }
            }
            Type::KnownInstance(KnownInstanceType::TypeAliasType(type_alias)) => {
                if let Some(generic_context) = type_alias.generic_context(db) {
                    return Ok(self.infer_explicit_type_alias_type_specialization(
                        subscript,
                        value_ty,
                        type_alias,
                        generic_context,
                    ));
                }
            }
            Type::SpecialForm(special_form) => match special_form {
                SpecialFormType::Tuple => {
                    return Ok(tuple_generic_alias(
                        env,
                        self.infer_tuple_type_expression(subscript),
                    ));
                }
                SpecialFormType::Literal => match self.infer_literal_parameter_type(slice) {
                    Ok(result) => {
                        return Ok(Type::KnownInstance(KnownInstanceType::Literal(
                            InternedType::new(db, result),
                        )));
                    }
                    Err(nodes) => {
                        for node in nodes {
                            let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, node)
                            else {
                                continue;
                            };
                            builder.into_diagnostic(
                                "Type arguments for `Literal` must be `None`, \
                                a literal value (int, bool, str, or bytes), \
                                or an enum member",
                            );
                        }
                        return Ok(Type::unknown());
                    }
                },
                SpecialFormType::Annotated => {
                    return Ok(self
                        .parse_subscription_of_annotated_special_form(
                            subscript,
                            AnnotatedExprContext::TypeExpression,
                        )
                        .inner_type());
                }
                SpecialFormType::Optional => {
                    if matches!(**slice, ast::Expr::Tuple(_))
                        && let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_FORM, subscript)
                    {
                        builder.into_diagnostic(format_args!(
                            "`typing.Optional` requires exactly one argument"
                        ));
                    }

                    let ty = self.infer_type_expression(slice);

                    // `Optional[None]` is equivalent to `None`:
                    if ty.is_none(db) {
                        return Ok(ty);
                    }
                    return Ok(Type::KnownInstance(KnownInstanceType::UnionType(
                        UnionTypeInstance::new(
                            db,
                            None,
                            Ok(UnionType::from_two_elements(
                                db,
                                env,
                                ty,
                                Type::none(db, env),
                            )),
                        ),
                    )));
                }
                SpecialFormType::Union => match **slice {
                    ast::Expr::Tuple(ref tuple) => {
                        let elements = tuple.iter().map(|elt| self.infer_type_expression(elt));

                        let union_type = Type::KnownInstance(KnownInstanceType::UnionType(
                            UnionTypeInstance::new(
                                db,
                                None,
                                Ok(UnionType::from_elements(db, env, elements)),
                            ),
                        ));

                        if tuple.is_empty()
                            && let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_FORM, subscript)
                        {
                            builder.into_diagnostic(
                                "`typing.Union` requires at least one type argument",
                            );
                        }

                        return Ok(union_type);
                    }
                    _ => {
                        return Ok(self.infer_expression(slice, TypeContext::default()));
                    }
                },
                SpecialFormType::Type => {
                    // Similar to the branch above that handles `type[…]`, handle `typing.Type[…]`
                    let argument_ty = self.infer_type_expression(slice);
                    return Ok(Type::KnownInstance(KnownInstanceType::TypeGenericAlias(
                        InternedType::new(db, argument_ty),
                    )));
                }
                SpecialFormType::TypingCallable | SpecialFormType::CollectionsAbcCallable => {
                    let callable = self
                        .infer_callable_type(subscript)
                        .as_callable()
                        .expect("always returns Type::Callable");

                    return Ok(Type::KnownInstance(KnownInstanceType::Callable(callable)));
                }
                SpecialFormType::Unpack => {
                    self.store_type_expression_flags(
                        ast::ExprRef::from(subscript),
                        TypeExpressionFlags::UNPACK,
                    );

                    let previously_in_unpack_type_argument = self
                        .context
                        .inference_flags
                        .replace(InferenceFlags::IN_UNPACK_TYPE_ARGUMENT, true);
                    let inner_ty = self.infer_type_expression(slice);
                    self.context.inference_flags.set(
                        InferenceFlags::IN_UNPACK_TYPE_ARGUMENT,
                        previously_in_unpack_type_argument,
                    );

                    return Ok(
                        if matches!(
                            inner_ty,
                            Type::TypeVar(typevar) if typevar.is_typevartuple(db)
                        ) || inner_ty.exact_tuple_instance_spec(db).is_some()
                        {
                            inner_ty
                        } else {
                            self.store_type_expression_flags(
                                ast::ExprRef::from(subscript),
                                TypeExpressionFlags::INVALID_UNPACK,
                            );
                            Type::unknown()
                        },
                    );
                }
                SpecialFormType::LegacyStdlibAlias(alias) => {
                    let AliasSpec {
                        class,
                        expected_argument_number,
                    } = alias.alias_spec();

                    let args = if let ast::Expr::Tuple(t) = &**slice {
                        &*t.elts
                    } else {
                        std::slice::from_ref(&**slice)
                    };

                    if args.len() != expected_argument_number
                        && let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_FORM, subscript)
                    {
                        let noun = if expected_argument_number == 1 {
                            "argument"
                        } else {
                            "arguments"
                        };
                        builder.into_diagnostic(format_args!(
                            "`typing.{name}` requires exactly \
                                {expected_argument_number} {noun}, got {got}",
                            name = special_form.name(),
                            got = args.len()
                        ));
                    }

                    let arg_types: Vec<_> = args
                        .iter()
                        .map(|arg| self.infer_type_expression(arg))
                        .collect();

                    return Ok(class
                        .to_specialized_class_type(db, env, arg_types)
                        .map(Type::from)
                        .unwrap_or_else(Type::unknown));
                }
                _ => {}
            },

            Type::KnownInstance(
                KnownInstanceType::UnionType(_)
                | KnownInstanceType::Annotated(_)
                | KnownInstanceType::Callable(_)
                | KnownInstanceType::TypeGenericAlias(_),
            ) => {
                return Ok(
                    self.infer_explicit_type_alias_specialization(subscript, value_ty, false)
                );
            }
            Type::Dynamic(DynamicType::Unknown) => {
                let slice_ty = self.infer_expression(slice, TypeContext::default());
                let mut variables = FxOrderSet::default();
                slice_ty.bind_and_find_all_legacy_typevars(
                    db,
                    env,
                    self.typevar_binding_context,
                    &mut variables,
                );
                let generic_context = GenericContext::from_typevar_instances(db, env, variables);
                return Ok(Type::Dynamic(DynamicType::UnknownGeneric(generic_context)));
            }
            _ => {}
        }

        let slice_ty = self.infer_expression(slice, TypeContext::default());
        self.infer_subscript_expression_types(subscript, value_ty, slice_ty, ExprContext::Load, tcx)
            .map(|ty| self.narrow_expr_with_applicable_constraints(subscript, ty, &constraint_keys))
            .map_err(|recovery_ty| {
                self.narrow_expr_with_applicable_constraints(
                    subscript,
                    recovery_ty,
                    &constraint_keys,
                )
            })
    }

    pub(super) fn infer_explicit_class_specialization(
        &mut self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
        generic_class: StaticClassLiteral<'db>,
        generic_context: GenericContext<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        let db = self.db();
        let specialize = &|types: &[Option<Type<'db>>]| {
            Type::from(generic_class.apply_specialization(db, |_| {
                generic_context.specialize_partial(db, types.iter().copied())
            }))
        };

        // Avoid constructing an identity specialization and a full protocol interface for the
        // many generic protocols that do not directly declare `__class__`.
        let disable_int_float_special_case = generic_class.is_protocol(db)
            && place_table(db, generic_class.body_scope(db))
                .symbol_id("__class__")
                .is_some()
            && generic_class
                .identity_specialization(db)
                .into_protocol_class(db)
                .is_some_and(|protocol| {
                    protocol
                        .interface(db)
                        .includes_generic_writable_instance_member(
                            db,
                            env,
                            "__class__",
                            generic_context,
                        )
                });
        let previously_disabled_int_float_special_case =
            disable_int_float_special_case.then(|| {
                self.context
                    .inference_flags
                    .replace(InferenceFlags::DISABLE_INT_FLOAT_SPECIAL_CASE, true)
            });

        let result = self.infer_explicit_callable_specialization(
            subscript,
            value_ty,
            generic_context,
            specialize,
        );

        if let Some(previously_disabled_int_float_special_case) =
            previously_disabled_int_float_special_case
        {
            self.context.inference_flags.set(
                InferenceFlags::DISABLE_INT_FLOAT_SPECIAL_CASE,
                previously_disabled_int_float_special_case,
            );
        }

        result
    }

    pub(super) fn infer_explicit_type_alias_type_specialization(
        &mut self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
        generic_type_alias: TypeAliasType<'db>,
        generic_context: GenericContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        if generic_type_alias.specialization(db).is_some() {
            if !self.in_string_annotation() {
                self.infer_expression(&subscript.slice, TypeContext::default());
            }
            if let Some(builder) = self.context.report_lint(&NOT_SUBSCRIPTABLE, subscript) {
                let mut diagnostic =
                    builder.into_diagnostic("Cannot specialize non-generic type alias");
                diagnostic.set_primary_annotation_message("Double specialization is not allowed");
            }
            return Type::unknown();
        }

        let specialize = &|types: &[Option<Type<'db>>]| {
            let type_alias = generic_type_alias.apply_specialization(db, |_| {
                generic_context.specialize_partial(db, types.iter().copied())
            });

            Type::KnownInstance(KnownInstanceType::TypeAliasType(type_alias))
        };

        let specialized = self.infer_explicit_callable_specialization(
            subscript,
            value_ty,
            generic_context,
            specialize,
        );

        // basedpython: a match type whose arguments are all known but match no `case` has no
        // value. Silently yielding `Unknown` would hide a mistake the author can act on —
        // either an argument is wrong or the match is missing a case
        if let Type::KnownInstance(KnownInstanceType::TypeAliasType(type_alias)) = specialized
            && let Some(alias) = type_alias.as_pep_695_type_alias()
            && matches!(
                evaluate_match_type(db, alias),
                Some(MatchTypeOutcome::NoCaseMatched)
            )
            && let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, subscript)
        {
            builder.into_diagnostic(format_args!(
                "No `case` of match type `{}` matches these type arguments",
                type_alias.name(db),
            ));
        }

        specialized
    }

    pub(super) fn infer_explicit_function_specialization(
        &mut self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
        generic_context: GenericContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let specialize = &|types: &[Option<Type<'db>>]| {
            let specialization = generic_context.specialize_partial(db, types.iter().copied());
            value_ty.apply_specialization(db, specialization)
        };

        self.infer_explicit_callable_specialization(
            subscript,
            value_ty,
            generic_context,
            specialize,
        )
    }

    pub(super) fn infer_explicit_callable_specialization(
        &mut self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
        generic_context: GenericContext<'db>,
        specialize: &dyn Fn(&[Option<Type<'db>>]) -> Type<'db>,
    ) -> Type<'db> {
        let previously_allowed_paramspec = self
            .context
            .inference_flags
            .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, true);
        let result = self.infer_explicit_callable_specialization_impl(
            subscript,
            value_ty,
            generic_context,
            specialize,
        );
        self.context.inference_flags.set(
            InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
            previously_allowed_paramspec,
        );
        result
    }

    fn infer_explicit_callable_specialization_impl(
        &mut self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
        generic_context: GenericContext<'db>,
        specialize: &dyn Fn(&[Option<Type<'db>>]) -> Type<'db>,
    ) -> Type<'db> {
        enum ExplicitSpecializationError {
            InvalidParamSpec,
            ParamSpecForTypeVar,
            UnsatisfiedBound,
            UnsatisfiedConstraints,
            /// These two errors override the errors above, causing all specializations to be `Unknown`.
            MissingTypeVars,
            TooManyArguments,
            /// This error overrides the errors above, causing the type itself to be `Unknown`.
            NonGeneric,
        }

        fn add_typevar_definition<'db>(
            db: &'db dyn Db,
            diagnostic: &mut Diagnostic,
            typevar: BoundTypeVarInstance<'db>,
        ) {
            let Some(definition) = typevar.typevar(db).definition(db) else {
                return;
            };
            let file = definition.file(db);
            let module = parsed_module(db, definition.python_file(db)).load(db);
            let range = definition.focus_range(db, &module).range();
            diagnostic.annotate(
                Annotation::secondary(Span::from(file).with_range(range))
                    .message("Type variable defined here"),
            );
        }

        /// A type argument after expanding any allowed `Unpack[tuple[...]]` syntax.
        #[derive(Clone, Copy)]
        struct TypeArgument<'ast, 'db> {
            /// The source expression used for diagnostics and deferred inference.
            node: &'ast ast::Expr,
            /// The already-inferred type, if this argument did not need deferred inference.
            ty: Option<Type<'db>>,
            /// The index of the original source argument before any `Unpack` expansion.
            source_index: usize,
        }

        /// What a basedpython keyword-form type subscript binds to one typevar slot.
        enum KeywordSlot<'ast> {
            /// A single type argument, given either by keyword or positionally.
            Bound(&'ast ast::Expr),
            /// The `name=type` fields collected for a keyword-variadic pack, in source order.
            Pack(Vec<(&'ast ast::name::Name, &'ast ast::Expr)>),
            /// The run of positional arguments a `*Ts` variadic absorbs, in source order.
            Variadic(Vec<&'ast ast::Expr>),
            /// A keyword argument named a type variable that cannot be given by name (a `*Ts`
            /// variadic). The expression is still type-checked, and the slot is filled with the
            /// gradual form so no cascading missing-argument error is reported.
            Invalid(&'ast ast::Expr),
            /// Nothing was provided; fall back to the typevar's declared default.
            Default,
        }

        let env = self.program_environment();
        let db = self.db();
        let constraints = ConstraintSetBuilder::new();
        let slice_node = subscript.slice.as_ref();

        let exactly_one_paramspec = generic_context.exactly_one_paramspec(db);
        // basedpython: a keyword-variadic pack (`class A[**Kwargs]`) is specialized by keyword,
        // and every keyword argument is one of its fields. its presence therefore changes what
        // a keyword argument means: with a pack, `A[foo=int]` names a *field*; without one,
        // `A[T=int]` names a *typevar*. the two spellings can't be mixed, so any other typevar
        // in a pack's context is specialized positionally
        let keyword_pack_index = generic_context
            .variables(db)
            .position(|typevar| typevar.is_keyword_variadic(db));
        let (type_arguments, store_inferred_type_arguments) = match slice_node {
            // basedpython: an anonymous named tuple `(name: T, ...)` is a
            // single type expression, not a list of generic arguments.
            ast::Expr::Tuple(tuple) if tuple.is_anon_named_tuple => {
                (std::slice::from_ref(slice_node), false)
            }
            // basedpython: a Parameters spec `(int, str, /, name: T)` is a
            // single subscript argument bound to a `ParamSpec`-shaped type
            // variable. inference treats it as one type expression.
            //
            // only the parenthesized spec form qualifies. a `name=T` label on its own means the
            // keyword-subscript form instead — `A[R=str, T=int]` binds typevars by name, and
            // `A[bytes, foo=int]` on a keyword-pack context binds the pack's fields
            ast::Expr::Tuple(tuple)
                if (tuple.is_parameter_shape
                    || tuple.parameter_slash().is_some()
                    || tuple.parameter_star().is_some())
                    && keyword_pack_index.is_none() =>
            {
                (std::slice::from_ref(slice_node), false)
            }
            // basedpython: a parenthesized tuple slice like `Iterable[(K, V)]`
            // is the tuple-literal type sugar (equivalent to
            // `Iterable[tuple[K, V]]`), a single type argument. unparenthesized
            // tuples remain the standard multi-arg subscript form
            // (`dict[K, V]`).
            // `A[()]` on a keyword-pack context is the empty pack, not the empty tuple type
            ast::Expr::Tuple(tuple)
                if tuple.parenthesized
                    && self.is_basedpython_file()
                    && !(tuple.elts.is_empty() && keyword_pack_index.is_some()) =>
            {
                (std::slice::from_ref(slice_node), false)
            }
            ast::Expr::Tuple(tuple) => {
                if exactly_one_paramspec && !tuple.elts.is_empty() {
                    (std::slice::from_ref(slice_node), false)
                } else {
                    (tuple.elts.as_slice(), true)
                }
            }
            _ => (std::slice::from_ref(slice_node), false),
        };

        let typevars = generic_context.variables(db).collect::<Vec<_>>();
        let typevars_len = typevars.len();
        let typevartuple_index = typevars
            .iter()
            .position(|typevar| typevar.is_typevartuple(db));

        // basedpython: kw-form type subscript like `A[R=str, T=int]`. bind
        // each kw arg to its named typevar; bare positional args fill the
        // remaining typevar slots in declaration order. unbound typevars
        // fall through to their declared default (or error if none).
        // `bp_kw_slots[i] = Some(expr)` if typevar i has a bound expr,
        // `None` if it should fall back to the declared default
        let has_keyword_argument = type_arguments.iter().any(|e| {
            matches!(
                e,
                ast::Expr::Named(n) if matches!(
                    n.target.as_ref(),
                    ast::Expr::Name(name) if matches!(name.ctx, ast::ExprContext::Invalid)
                )
            )
        });
        // a separated list also takes this path when every argument is positional, so that a
        // keyword-only type variable filled by position is still caught
        let has_separators = generic_context.has_type_param_separators(db);
        let bp_kw_slots: Option<Vec<KeywordSlot<'_>>> = if self.is_basedpython_file()
            && (has_keyword_argument || keyword_pack_index.is_some() || has_separators)
        {
            // source order, so leftover-argument diagnostics are reported deterministically
            let mut keywords: Vec<(&ast::name::Name, &ast::Expr)> = Vec::new();
            let mut pack_fields: Vec<(&ast::name::Name, &ast::Expr)> = Vec::new();
            let mut positional: Vec<&ast::Expr> = Vec::new();
            for expr in type_arguments {
                if let ast::Expr::Named(n) = expr
                    && let ast::Expr::Name(name) = n.target.as_ref()
                    && matches!(name.ctx, ast::ExprContext::Invalid)
                {
                    if keyword_pack_index.is_some() {
                        pack_fields.push((&name.id, n.value.as_ref()));
                    } else {
                        keywords.push((&name.id, n.value.as_ref()));
                    }
                } else {
                    positional.push(expr);
                }
            }
            let mut by_name: rustc_hash::FxHashMap<&str, &ast::Expr> = keywords
                .iter()
                .map(|(name, expr)| (name.as_str(), *expr))
                .collect();
            let mut slots: Vec<KeywordSlot<'_>> = Vec::with_capacity(typevars_len);
            let mut positional_iter = positional.into_iter();
            for (index, tv) in typevars.iter().enumerate() {
                let kind = generic_context.type_param_kind(db, index);
                if keyword_pack_index == Some(index) {
                    slots.push(KeywordSlot::Pack(std::mem::take(&mut pack_fields)));
                } else if let Some(expr) = by_name.remove(tv.name(db).as_str()) {
                    // basedpython: a `*Ts` variadic cannot be given by name, the same way `*args`
                    // rejects a keyword argument — it binds an unknown-length run of positions, so
                    // a single `Ts=int` is meaningless
                    if tv.is_typevartuple(db) {
                        if let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_ARGUMENTS, expr)
                        {
                            builder.into_diagnostic(format_args!(
                                "Type variable `{}` is variadic and cannot be given by name",
                                tv.name(db),
                            ));
                        }
                        slots.push(KeywordSlot::Invalid(expr));
                    } else {
                        // basedpython: `/` and a bare `*` restrict how a type argument may be
                        // given, exactly as they do for a value parameter
                        if kind == ast::TypeParamKind::PositionalOnly
                            && let Some(builder) =
                                self.context.report_lint(&INVALID_TYPE_ARGUMENTS, expr)
                        {
                            builder.into_diagnostic(format_args!(
                                "Type variable `{}` is positional-only",
                                tv.name(db),
                            ));
                        }
                        slots.push(KeywordSlot::Bound(expr));
                    }
                } else if tv.is_typevartuple(db) {
                    // basedpython: a variadic absorbs every positional argument the parameters
                    // after it don't need — those claim theirs from the back, and a slot given
                    // by name or a keyword pack claims none at all. this is the same rule the
                    // positional path applies, restated here because a keyword-variadic pack
                    // (or any keyword argument) routes the whole subscript through these slots
                    let claimed_after = typevars[index + 1..]
                        .iter()
                        .enumerate()
                        .filter(|(offset, later)| {
                            keyword_pack_index != Some(index + 1 + offset)
                                && !by_name.contains_key(later.name(db).as_str())
                        })
                        .count();
                    let run = positional_iter.len().saturating_sub(claimed_after);
                    slots.push(KeywordSlot::Variadic(
                        positional_iter.by_ref().take(run).collect(),
                    ));
                } else if let Some(expr) = positional_iter.next() {
                    if kind == ast::TypeParamKind::KeywordOnly
                        && let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_ARGUMENTS, expr)
                    {
                        builder.into_diagnostic(format_args!(
                            "Type variable `{}` is keyword-only",
                            tv.name(db),
                        ));
                    }
                    slots.push(KeywordSlot::Bound(expr));
                } else {
                    slots.push(KeywordSlot::Default);
                }
            }
            // arguments that reached no slot would otherwise be dropped, silently specializing
            // the class to something the source never asked for
            let described = || {
                CallableDescription::new(db, value_ty)
                    .map(|description| format!(" for {description}"))
                    .unwrap_or_default()
            };
            for extra in positional_iter {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, extra) {
                    builder.into_diagnostic(format_args!(
                        "Too many type arguments{}: expected {typevars_len}",
                        described(),
                    ));
                }
                self.infer_type_expression(extra);
            }
            for (name, _) in keywords
                .iter()
                .filter(|(name, _)| by_name.contains_key(name.as_str()))
            {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, subscript)
                {
                    builder.into_diagnostic(format_args!(
                        "No type variable named `{name}`{}",
                        described(),
                    ));
                }
            }
            Some(slots)
        } else {
            None
        };
        // basedpython kw-form: bypass the source-order pipeline and bind by
        // typevar name directly. unbound typevars use their declared default
        if let Some(slots) = bp_kw_slots {
            let mut specialization_types: Vec<Option<Type<'db>>> = Vec::with_capacity(typevars_len);
            let mut missing_typevars: Vec<_> = Vec::new();
            for (typevar, slot) in typevars.iter().zip(slots.iter()) {
                match slot {
                    KeywordSlot::Bound(expr) => {
                        let provided_type = self.infer_type_expression(expr);
                        specialization_types.push(Some(provided_type));
                        // bound/constraints checks intentionally skipped here;
                        // the regular path catches them via `infer_type_expression`
                        // diagnostics on the expr itself
                        let _ = typevar;
                    }
                    KeywordSlot::Pack(fields) => {
                        let parameters = Parameters::standard(fields.iter().map(|(name, expr)| {
                            let field_type = self.infer_type_expression(expr);
                            Parameter::keyword_only((*name).clone()).with_annotated_type(field_type)
                        }));
                        let provided_type = Type::paramspec_value_callable(db, parameters);
                        self.check_pack_bound(*typevar, provided_type, subscript);
                        specialization_types.push(Some(provided_type));
                    }
                    KeywordSlot::Variadic(exprs) => {
                        let mut tuple_builder = TupleSpecBuilder::with_capacity(exprs.len());
                        for expr in exprs.iter().copied() {
                            let previously_in_valid_unpack_context = self
                                .context
                                .inference_flags
                                .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
                            let provided_type = self.infer_type_expression(expr);
                            self.context.inference_flags.set(
                                InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                                previously_in_valid_unpack_context,
                            );
                            // `*tuple[int, str]` contributes its elements, not itself
                            if self
                                .type_expression_flags(expr)
                                .contains(TypeExpressionFlags::UNPACK)
                                && let Some(tuple) = provided_type.exact_tuple_instance_spec(db)
                            {
                                tuple_builder = tuple_builder.concat(db, env, &tuple);
                            } else {
                                tuple_builder.push(provided_type);
                            }
                        }
                        let provided_type =
                            Type::tuple(TupleType::new(db, env, &tuple_builder.build()));
                        self.check_pack_bound(*typevar, provided_type, subscript);
                        specialization_types.push(Some(provided_type));
                    }
                    KeywordSlot::Invalid(expr) => {
                        // still type-check the expression so its own diagnostics fire, then fill
                        // the slot gradually — the variadic error was already reported
                        let _ = self.infer_type_expression(expr);
                        specialization_types.push(Some(Type::unknown()));
                    }
                    KeywordSlot::Default => {
                        if typevar.default_type(db).is_some() {
                            specialization_types.push(None);
                        } else {
                            missing_typevars.push(*typevar);
                        }
                    }
                }
            }
            if !missing_typevars.is_empty() {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, subscript)
                {
                    let description = CallableDescription::new(db, value_ty);
                    let s = if missing_typevars.len() > 1 { "s" } else { "" };
                    builder.into_diagnostic(format_args!(
                        "No type argument{s} provided for required type variable{s} `{}`{}",
                        missing_typevars
                            .iter()
                            .map(|tv| tv.typevar(db).name(db))
                            .format("`, `"),
                        description
                            .map(|description| format!(" of {description}"))
                            .unwrap_or_default()
                    ));
                }
                let unknowns = generic_context
                    .variables(db)
                    .map(|tv| {
                        Some(if tv.is_paramspec(db) {
                            Type::paramspec_value_callable(db, Parameters::unknown())
                        } else {
                            Type::unknown()
                        })
                    })
                    .collect::<Vec<_>>();
                return specialize(&unknowns);
            }
            return specialize(&specialization_types);
        }

        let mut inferred_type_arguments = vec![None; type_arguments.len()];

        let mut expanded_type_arguments = Vec::with_capacity(type_arguments.len());

        for (source_index, expr) in type_arguments.iter().enumerate() {
            let typevar = if let Some(typevartuple_index) = typevartuple_index {
                let suffix_len = typevars_len - typevartuple_index - 1;
                let suffix_source_start = type_arguments.len().saturating_sub(suffix_len);
                if suffix_len > 0
                    && type_arguments.len() >= typevartuple_index + suffix_len
                    && source_index >= suffix_source_start
                {
                    let suffix_index = source_index - suffix_source_start;
                    typevars
                        .get(typevars_len - suffix_len + suffix_index)
                        .copied()
                } else {
                    typevars.get(expanded_type_arguments.len()).copied()
                }
            } else {
                typevars.get(expanded_type_arguments.len()).copied()
            };
            if exactly_one_paramspec || typevar.is_some_and(|typevar| typevar.is_paramspec(db)) {
                expanded_type_arguments.push(TypeArgument {
                    node: expr,
                    ty: None,
                    source_index,
                });
                continue;
            }

            let provided_type = if typevars_len == 0 {
                // If there are no typevars at all, this is not a generic type,
                // so we should not infer excess arguments as type expressions.
                // For example, `list[int][0]` — the `0` is not a type expression.
                self.infer_expression(expr, TypeContext::default())
            } else {
                let previously_in_valid_unpack_context = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
                let provided_type = self.infer_type_expression(expr);
                self.context.inference_flags.set(
                    InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                    previously_in_valid_unpack_context,
                );
                provided_type
            };

            inferred_type_arguments[source_index] = Some(provided_type);

            let type_expression_flags = self.type_expression_flags(expr);
            let is_unpack = type_expression_flags.contains(TypeExpressionFlags::UNPACK);

            if is_unpack
                && let Some(tuple) = provided_type.exact_tuple_instance_spec(db)
                && let Tuple::Fixed(tuple) = tuple.as_ref()
                && expanded_type_arguments.len() <= typevars_len
                && typevars[expanded_type_arguments.len()
                    ..usize::min(
                        expanded_type_arguments.len() + tuple.elements_slice().len(),
                        typevars_len,
                    )]
                    .iter()
                    .all(|typevar| !typevar.is_paramspec(db))
            {
                // Expand `Foo[Unpack[tuple[int, str]]]` to `Foo[int, str]`. ParamSpec arguments
                // must still use their dedicated inference path.
                expanded_type_arguments.extend(tuple.iter_all_elements().map(|ty| TypeArgument {
                    node: expr,
                    ty: Some(ty),
                    source_index,
                }));
            } else {
                if is_unpack
                    && !self
                        .inference_flags()
                        .contains(InferenceFlags::IN_KWARG_ANNOTATION)
                    && typevartuple_index.is_none()
                    && !type_expression_flags.contains(TypeExpressionFlags::INVALID_UNPACK)
                    && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, expr)
                {
                    builder.into_diagnostic(
                        "`Unpack` can only be used with a fixed tuple type in this context",
                    );
                }

                expanded_type_arguments.push(TypeArgument {
                    node: expr,
                    ty: Some(provided_type),
                    source_index,
                });
            }
        }

        let expanded_type_arguments = if let Some(typevartuple_index) = typevartuple_index {
            let suffix_len = typevars_len - typevartuple_index - 1;
            let typevartuple_end = expanded_type_arguments
                .len()
                .saturating_sub(suffix_len)
                .max(typevartuple_index);
            let mut packed = Vec::with_capacity(typevars_len);

            let mut tuple_builder = TupleSpecBuilder::with_capacity(
                typevartuple_end.saturating_sub(typevartuple_index),
            );
            for type_argument in
                &expanded_type_arguments[..expanded_type_arguments.len().min(typevartuple_index)]
            {
                let provided_type = type_argument.ty.unwrap_or_else(Type::unknown);
                let is_unpack = self
                    .type_expression_flags(type_argument.node)
                    .contains(TypeExpressionFlags::UNPACK);
                if is_unpack
                    && let Some(tuple) = provided_type.exact_tuple_instance_spec(db)
                    && let Tuple::Variable(variable) = tuple.as_ref()
                    && variable.prefix_elements().is_empty()
                    && variable.suffix_elements().is_empty()
                    && let Some(variable_type) = variable.variable().homogeneous_type()
                {
                    tuple_builder = tuple_builder.concat(db, env, &tuple);
                    packed.push(TypeArgument {
                        ty: Some(variable_type),
                        ..*type_argument
                    });
                } else if is_unpack
                    && (matches!(
                        provided_type,
                        Type::TypeVar(typevar) if typevar.is_typevartuple(db)
                    ) || matches!(
                        provided_type.exact_tuple_instance_spec(db).as_deref(),
                        Some(Tuple::Variable(variable))
                            if variable.variable().typevartuple().is_some()
                    ))
                {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_TYPE_FORM, type_argument.node)
                    {
                        builder.into_diagnostic(
                            "A TypeVarTuple cannot be split to provide a fixed type argument",
                        );
                    }
                    packed.push(TypeArgument {
                        ty: Some(Type::unknown()),
                        ..*type_argument
                    });
                } else {
                    packed.push(*type_argument);
                }
            }

            let typevartuple_start = expanded_type_arguments.len().min(typevartuple_index);
            for type_argument in &expanded_type_arguments
                [typevartuple_start..expanded_type_arguments.len().min(typevartuple_end)]
            {
                let provided_type = type_argument.ty.unwrap_or_else(|| {
                    let previously_in_valid_unpack_context = self
                        .context
                        .inference_flags
                        .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
                    let provided_type = self.infer_type_expression(type_argument.node);
                    self.context.inference_flags.set(
                        InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                        previously_in_valid_unpack_context,
                    );
                    inferred_type_arguments[type_argument.source_index] = Some(provided_type);
                    provided_type
                });
                let is_unpack = self
                    .type_expression_flags(type_argument.node)
                    .contains(TypeExpressionFlags::UNPACK);
                if is_unpack && let Some(tuple) = provided_type.exact_tuple_instance_spec(db) {
                    tuple_builder = tuple_builder.concat(db, env, &tuple);
                } else if is_unpack
                    && let Type::TypeVar(typevar) = provided_type
                    && typevar.is_typevartuple(db)
                {
                    tuple_builder = tuple_builder.concat_variadic_typevar(db, env, typevar);
                } else {
                    tuple_builder.push(provided_type);
                }
            }

            let mut packed_suffix = Vec::with_capacity(suffix_len);
            if suffix_len > 0 && expanded_type_arguments.len() >= typevartuple_index + suffix_len {
                for type_argument in
                    &expanded_type_arguments[expanded_type_arguments.len() - suffix_len..]
                {
                    let provided_type = type_argument.ty.unwrap_or_else(Type::unknown);
                    let is_unpack = self
                        .type_expression_flags(type_argument.node)
                        .contains(TypeExpressionFlags::UNPACK);
                    if is_unpack
                        && let Some(tuple) = provided_type.exact_tuple_instance_spec(db)
                        && let Tuple::Variable(variable) = tuple.as_ref()
                        && variable.prefix_elements().is_empty()
                        && variable.suffix_elements().is_empty()
                        && let Some(variable_type) = variable.variable().homogeneous_type()
                    {
                        tuple_builder = tuple_builder.concat(db, env, &tuple);
                        packed_suffix.push(TypeArgument {
                            ty: Some(variable_type),
                            ..*type_argument
                        });
                    } else if is_unpack
                        && (matches!(
                            provided_type,
                            Type::TypeVar(typevar) if typevar.is_typevartuple(db)
                        ) || matches!(
                            provided_type.exact_tuple_instance_spec(db).as_deref(),
                            Some(Tuple::Variable(variable))
                                if variable.variable().typevartuple().is_some()
                        ))
                    {
                        if let Some(builder) = self
                            .context
                            .report_lint(&INVALID_TYPE_FORM, type_argument.node)
                        {
                            builder.into_diagnostic(
                                "A TypeVarTuple cannot be split to provide a fixed type argument",
                            );
                        }
                        packed_suffix.push(TypeArgument {
                            ty: Some(Type::unknown()),
                            ..*type_argument
                        });
                    } else {
                        packed_suffix.push(*type_argument);
                    }
                }
            }

            if expanded_type_arguments.len() >= typevartuple_index {
                packed.push(TypeArgument {
                    node: expanded_type_arguments
                        .get(typevartuple_index)
                        .map_or(slice_node, |argument| argument.node),
                    ty: Some(Type::tuple(TupleType::new(db, env, &tuple_builder.build()))),
                    source_index: expanded_type_arguments
                        .get(typevartuple_index)
                        .map_or(0, |argument| argument.source_index),
                });
            }
            packed.extend(packed_suffix);

            packed
        } else {
            expanded_type_arguments
        };

        let mut specialization_types = Vec::with_capacity(typevars_len);
        let mut typevar_with_defaults = 0;
        let mut missing_typevars = vec![];
        let mut first_excess_type_argument_index = None;

        let mut error: Option<ExplicitSpecializationError> = None;

        for (index, item) in typevars
            .iter()
            .copied()
            .zip_longest(expanded_type_arguments.iter())
            .enumerate()
        {
            match item {
                EitherOrBoth::Both(typevar, type_argument) => {
                    if typevar.default_type(db).is_some() {
                        typevar_with_defaults += 1;
                    }

                    let provided_type = if typevar.is_paramspec(db) {
                        let provided_type = self
                            .infer_paramspec_explicit_specialization_value(
                                type_argument.node,
                                exactly_one_paramspec,
                            )
                            .unwrap_or_else(|()| {
                                error = Some(ExplicitSpecializationError::InvalidParamSpec);
                                Type::paramspec_value_callable(db, Parameters::unknown())
                            });
                        inferred_type_arguments[type_argument.source_index] = Some(provided_type);
                        provided_type
                    } else {
                        type_argument.ty.unwrap_or_else(|| {
                            let previously_in_valid_unpack_context = self
                                .context
                                .inference_flags
                                .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
                            let provided_type = self.infer_type_expression(type_argument.node);
                            self.context.inference_flags.set(
                                InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                                previously_in_valid_unpack_context,
                            );
                            inferred_type_arguments[type_argument.source_index] =
                                Some(provided_type);
                            provided_type
                        })
                    };

                    // A ParamSpec cannot be used to specialize a regular TypeVar.
                    if !typevar.is_paramspec(db)
                        && let Type::TypeVar(tv) = provided_type
                        && tv.is_paramspec(db)
                    {
                        if let Some(builder) = self
                            .context
                            .report_lint(&INVALID_TYPE_ARGUMENTS, type_argument.node)
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "ParamSpec `{}` cannot be used to specialize \
                                    type variable `{}`",
                                tv.typevar(db).name(db),
                                typevar.name(db),
                            ));
                            for (kind, var) in [("ParamSpec", tv), ("Type variable", typevar)] {
                                let Some(definition) = var.typevar(db).definition(db) else {
                                    continue;
                                };
                                let file = definition.file(db);
                                let module = parsed_module(db, definition.python_file(db)).load(db);
                                let range = definition.focus_range(db, &module).range();
                                diagnostic.annotate(
                                    Annotation::secondary(Span::from(file).with_range(range))
                                        .message(format_args!(
                                            "{kind} `{}` defined here",
                                            var.name(db)
                                        )),
                                );
                            }
                        }
                        error = Some(ExplicitSpecializationError::ParamSpecForTypeVar);
                        specialization_types.push(Some(Type::unknown()));
                        continue;
                    }

                    // basedpython: a bound range `T: Lower..Upper` also puts a floor under the
                    // argument, so check that end before the upper end below
                    if let Some(lower_bound) = typevar.typevar(db).lower_bound(db)
                        && lower_bound
                            .when_assignable_to(
                                db,
                                env,
                                provided_type,
                                &constraints,
                                TypeVarSet::None,
                            )
                            .is_never_satisfied(db, env)
                    {
                        if let Some(builder) = self
                            .context
                            .report_lint(&INVALID_TYPE_ARGUMENTS, type_argument.node)
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Type `{}` does not satisfy lower bound `{}` \
                                    of type variable `{}`",
                                provided_type.display(db, env),
                                lower_bound.display(db, env),
                                typevar.identity(db).display(db),
                            ));
                            add_typevar_definition(db, &mut diagnostic, typevar);
                            lower_bound
                                .assignability_error_context(db, env, provided_type)
                                .attach_to(db, env, &mut diagnostic);
                        }
                        error = Some(ExplicitSpecializationError::UnsatisfiedBound);
                        specialization_types.push(Some(Type::unknown()));
                        continue;
                    }

                    // basedpython: a pack's bound reads element-wise or whole-pack depending on
                    // its star count, and neither is the ordinary check below — that one would
                    // compare the packed tuple against an element bound and always fail
                    if typevar.is_typevartuple(db) && typevar.typevar(db).has_pack_bound(db, env) {
                        if self.check_pack_bound(typevar, provided_type, type_argument.node) {
                            error = Some(ExplicitSpecializationError::UnsatisfiedBound);
                        }
                        // the pack is kept even when it violates the bound: replacing it with
                        // `Unknown` would stop it reading as a tuple at all, and a match type
                        // over it would then report a second, misleading error
                        specialization_types.push(Some(provided_type));
                        continue;
                    }

                    // TODO consider just accepting the given specialization without checking
                    // against bounds/constraints, but recording the expression for deferred
                    // checking at end of scope. This would avoid a lot of cycles caused by eagerly
                    // doing assignment checks here.
                    match typevar.typevar(db).bound_or_constraints(db, env) {
                        Some(TypeVarBoundOrConstraints::UpperBound(bound)) => {
                            if provided_type
                                .when_assignable_to(db, env, bound, &constraints, TypeVarSet::None)
                                .is_never_satisfied(db, env)
                            {
                                if let Some(builder) = self
                                    .context
                                    .report_lint(&INVALID_TYPE_ARGUMENTS, type_argument.node)
                                {
                                    let mut diagnostic = builder.into_diagnostic(format_args!(
                                        "Type `{}` is not assignable to upper bound `{}` \
                                            of type variable `{}`",
                                        provided_type.display(db, env),
                                        bound.display(db, env),
                                        typevar.identity(db).display(db),
                                    ));
                                    add_typevar_definition(db, &mut diagnostic, typevar);
                                    provided_type
                                        .assignability_error_context(db, env, bound)
                                        .attach_to(db, env, &mut diagnostic);
                                }
                                error = Some(ExplicitSpecializationError::UnsatisfiedBound);
                                specialization_types.push(Some(Type::unknown()));
                            } else {
                                specialization_types.push(Some(provided_type));
                            }
                        }
                        Some(TypeVarBoundOrConstraints::Constraints(typevar_constraints)) => {
                            // TODO: this is wrong, the given specialization needs to be assignable
                            // to _at least one_ of the individual constraints, not to the union of
                            // all of them. `int | str` is not a valid specialization of a typevar
                            // constrained to `(int, str)`.
                            if provided_type
                                .when_assignable_to(
                                    db,
                                    env,
                                    typevar_constraints.as_type(db, env),
                                    &constraints,
                                    TypeVarSet::None,
                                )
                                .is_never_satisfied(db, env)
                            {
                                if let Some(builder) = self
                                    .context
                                    .report_lint(&INVALID_TYPE_ARGUMENTS, type_argument.node)
                                {
                                    let mut diagnostic = builder.into_diagnostic(format_args!(
                                        "Type `{}` does not satisfy constraints `{}` \
                                            of type variable `{}`",
                                        provided_type.display(db, env),
                                        typevar_constraints
                                            .elements(db)
                                            .iter()
                                            .map(|c| c.display(db, env))
                                            .format("`, `"),
                                        typevar.identity(db).display(db),
                                    ));
                                    add_typevar_definition(db, &mut diagnostic, typevar);
                                }
                                error = Some(ExplicitSpecializationError::UnsatisfiedConstraints);
                                specialization_types.push(Some(Type::unknown()));
                            } else {
                                specialization_types.push(Some(provided_type));
                            }
                        }
                        None => {
                            specialization_types.push(Some(provided_type));
                        }
                    }
                }
                EitherOrBoth::Left(typevar) => {
                    if typevar.default_type(db).is_none() {
                        // This is an error case, so no need to push into the specialization types.
                        missing_typevars.push(typevar);
                    } else {
                        typevar_with_defaults += 1;
                        specialization_types.push(None);
                    }
                }
                EitherOrBoth::Right(_) => {
                    first_excess_type_argument_index.get_or_insert(index);
                }
            }
        }

        if !missing_typevars.is_empty() {
            if let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, subscript) {
                let description = CallableDescription::new(db, value_ty);
                let s = if missing_typevars.len() > 1 { "s" } else { "" };
                builder.into_diagnostic(format_args!(
                    "No type argument{s} provided for required type variable{s} `{}`{}",
                    missing_typevars
                        .iter()
                        .map(|tv| tv.typevar(db).name(db))
                        .format("`, `"),
                    description
                        .map(|description| format!(" of {description}"))
                        .unwrap_or_default()
                ));
            }
            error = Some(ExplicitSpecializationError::MissingTypeVars);
        }

        if let Some(first_excess_type_argument_index) = first_excess_type_argument_index {
            if typevars_len == 0 {
                // Type parameter list cannot be empty, so if we reach here, `value_ty` is not a generic type.
                if let Some(builder) = self.context.report_lint(&NOT_SUBSCRIPTABLE, subscript) {
                    let mut diagnostic = builder.into_diagnostic(format_args!(
                        "Cannot subscript non-generic type `{}`",
                        value_ty.display(db, env)
                    ));
                    let already_specialized = match value_ty {
                        Type::GenericAlias(_) => true,
                        Type::KnownInstance(KnownInstanceType::UnionType(union)) => union
                            .value_expression_types(db, env)
                            .is_ok_and(|mut tys| tys.any(|ty| ty.is_generic_alias())),
                        _ => false,
                    };
                    if already_specialized {
                        diagnostic.annotate(
                            self.context
                                .secondary(&*subscript.value)
                                .message("Type is already specialized"),
                        );
                    }
                }
                error = Some(ExplicitSpecializationError::NonGeneric);
            } else {
                let node = expanded_type_arguments[first_excess_type_argument_index].node;
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, node) {
                    let description = CallableDescription::new(db, value_ty);
                    builder.into_diagnostic(format_args!(
                        "Too many type arguments{}: expected {}, got {}",
                        description
                            .map(|description| format!(" to {description}"))
                            .unwrap_or_default(),
                        if typevar_with_defaults == 0 {
                            format!("{typevars_len}")
                        } else {
                            format!(
                                "between {} and {}",
                                typevars_len - typevar_with_defaults,
                                typevars_len
                            )
                        },
                        expanded_type_arguments.len(),
                    ));
                }
                error = Some(ExplicitSpecializationError::TooManyArguments);
            }
        }

        if store_inferred_type_arguments {
            self.store_expression_type(
                slice_node,
                Type::heterogeneous_tuple(
                    db,
                    env,
                    inferred_type_arguments
                        .into_iter()
                        .map(|ty| ty.unwrap_or(Type::unknown())),
                ),
            );
        }

        match error {
            Some(ExplicitSpecializationError::NonGeneric) => Type::unknown(),
            Some(
                ExplicitSpecializationError::MissingTypeVars
                | ExplicitSpecializationError::TooManyArguments,
            ) => {
                let unknowns = generic_context
                    .variables(db)
                    .map(|typevar| {
                        Some(if typevar.is_paramspec(db) {
                            Type::paramspec_value_callable(db, Parameters::unknown())
                        } else if typevar.is_typevartuple(db) {
                            Type::homogeneous_tuple(db, env, Type::unknown())
                        } else {
                            Type::unknown()
                        })
                    })
                    .collect::<Vec<_>>();
                specialize(&unknowns)
            }
            Some(
                ExplicitSpecializationError::UnsatisfiedBound
                | ExplicitSpecializationError::UnsatisfiedConstraints
                | ExplicitSpecializationError::InvalidParamSpec
                | ExplicitSpecializationError::ParamSpecForTypeVar,
            )
            | None => specialize(&specialization_types),
        }
    }

    /// Infer the type of the expression that represents an explicit specialization of a
    /// `ParamSpec` type variable.
    fn infer_paramspec_explicit_specialization_value(
        &mut self,
        expr: &ast::Expr,
        exactly_one_paramspec: bool,
    ) -> Result<Type<'db>, ()> {
        let env = self.program_environment();
        let db = self.db();

        match expr {
            ast::Expr::EllipsisLiteral(_) => {
                return Ok(Type::paramspec_value_callable(
                    db,
                    Parameters::gradual_form(),
                ));
            }

            ast::Expr::Tuple(_) if !exactly_one_paramspec => {
                // Tuple expression is only allowed when the generic context contains only one
                // `ParamSpec` type variable and no other type variables.
            }

            ast::Expr::Tuple(ast::ExprTuple { elts, .. })
            | ast::Expr::List(ast::ExprList { elts, .. }) => {
                let previously_allowed_paramspec = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, false);
                // basedpython: the slice is a parameters spec, so a `name: T` field keeps its
                // name and the `/` and `*` markers keep their meaning — the same shape the
                // callable arrow `(int, /, name: T)` builds
                let (slash, star) = match expr {
                    ast::Expr::Tuple(tuple) => (
                        tuple.parameter_slash().map(|i| i as usize),
                        tuple.parameter_star().map(|i| i as usize),
                    ),
                    _ => (None, None),
                };
                let params = self
                    .infer_parameter_spec_elements(elts, slash, star, |_| ParameterBorrow::None);
                self.context.inference_flags.set(
                    InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
                    previously_allowed_paramspec,
                );

                // We currently infer `Todo` for the parameters to avoid invalid diagnostics when
                // trying to check for assignability or any other relation. For example,
                // `*tuple[int, str]`, `Unpack[]`, etc. are not yet supported.
                let return_todo = std::iter::zip(elts, &params).any(|(element, param)| {
                    param.annotated_type().is_todo()
                        && matches!(element, ast::Expr::Starred(_) | ast::Expr::Subscript(_))
                });

                let parameters = if return_todo {
                    // TODO: `Unpack`
                    Parameters::todo()
                } else {
                    Parameters::from_annotation(db, env, params)
                };

                return Ok(Type::paramspec_value_callable(db, parameters));
            }

            ast::Expr::Subscript(subscript) => {
                let value_ty = self.infer_expression(&subscript.value, TypeContext::default());

                if matches!(value_ty, Type::SpecialForm(SpecialFormType::Concatenate)) {
                    return Ok(Type::paramspec_value_callable(
                        db,
                        self.infer_concatenate_special_form(subscript),
                    ));
                }

                // Non-Concatenate subscript: fall back to todo
                return Ok(Type::paramspec_value_callable(db, Parameters::todo()));
            }

            ast::Expr::Name(name) => {
                if name.is_invalid() {
                    return Err(());
                }

                let previous_concatenate_context = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::IN_VALID_CONCATENATE_CONTEXT, true);
                let param_type = self.infer_type_expression(expr);
                self.context.inference_flags.set(
                    InferenceFlags::IN_VALID_CONCATENATE_CONTEXT,
                    previous_concatenate_context,
                );

                match param_type {
                    Type::TypeVar(typevar) if typevar.is_paramspec(db) => {
                        return Ok(param_type);
                    }

                    Type::KnownInstance(KnownInstanceType::TypeVar(typevar))
                        if typevar.is_paramspec(db) =>
                    {
                        if let Some(diagnostic_builder) =
                            self.context.report_lint(&INVALID_TYPE_ARGUMENTS, expr)
                        {
                            diagnostic_builder.into_diagnostic(format_args!(
                                "ParamSpec `{}` is unbound",
                                typevar.name(db)
                            ));
                        }
                        return Err(());
                    }

                    // This is to handle the following case:
                    //
                    // ```python
                    // from typing import ParamSpec
                    //
                    // class Foo[**P]: ...
                    //
                    // Foo[ParamSpec]  # P: (ParamSpec, /)
                    // ```
                    Type::NominalInstance(nominal)
                        if nominal.has_known_class(db, KnownClass::ParamSpec) =>
                    {
                        return Ok(Type::paramspec_value_callable(
                            db,
                            Parameters::from_annotation(
                                db,
                                env,
                                [
                                    Parameter::positional_only(None)
                                        .with_annotated_type(param_type),
                                ],
                            ),
                        ));
                    }

                    _ if exactly_one_paramspec => {
                        // Square brackets are optional when `ParamSpec` is the only type variable
                        // being specialized. This means that a single name expression represents a
                        // parameter list with a single parameter. For example,
                        //
                        // ```python
                        // class OnlyParamSpec[**P]: ...
                        //
                        // OnlyParamSpec[int]  # P: (int, /)
                        // ```
                        let parameters =
                            if param_type.is_todo() {
                                Parameters::todo()
                            } else if param_type.is_dynamic() && param_type != Type::any() {
                                // If we ended up with an `Unknown` type here, it almost certainly means
                                // that we already emitted an error elsewhere. Fallback to the more lenient
                                // type.
                                Parameters::unknown()
                            } else {
                                Parameters::from_annotation(
                                    db,
                                    env,
                                    [Parameter::positional_only(None)
                                        .with_annotated_type(param_type)],
                                )
                            };
                        return Ok(Type::paramspec_value_callable(db, parameters));
                    }

                    // This is specifically to handle a case where there are more than one type
                    // variables and at least one of them is a `ParamSpec` which is specialized
                    // using `typing.Any`. This isn't explicitly allowed in the spec, but both mypy
                    // and Pyright allows this and the ecosystem report suggested there are usages
                    // of this in the wild e.g., `staticmethod[Any, Any]`. For example,
                    //
                    // ```python
                    // class Foo[**P, T]: ...
                    //
                    // Foo[Any, int]  # P: (Any, /), T: int
                    // ```
                    Type::Dynamic(DynamicType::Any) => {
                        return Ok(Type::paramspec_value_callable(
                            db,
                            Parameters::gradual_form(),
                        ));
                    }

                    // If we ended up with an `Unknown` type here, it almost certainly means
                    // that we already emitted an error elsewhere
                    Type::Dynamic(_) => {
                        return Ok(Type::paramspec_value_callable(db, Parameters::unknown()));
                    }

                    _ => {}
                }
            }

            _ => {}
        }

        if let Some(builder) = self.context.report_lint(&INVALID_TYPE_ARGUMENTS, expr) {
            builder.into_diagnostic(
                "Type argument for `ParamSpec` must be either \
                    a list of types, `ParamSpec`, `Concatenate`, or `...`",
            );
        }

        Err(())
    }

    /// Infer a subscription and report failures while preserving their recovery types.
    pub(super) fn infer_subscript_expression_types(
        &self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
        slice_ty: Type<'db>,
        expr_context: ExprContext,
        tcx: TypeContext<'db>,
    ) -> Result<Type<'db>, Type<'db>> {
        let env = self.program_environment();
        let db = self.db();

        if let Some(origin) = match value_ty {
            Type::SpecialForm(SpecialFormType::Generic) => Some(LegacyGenericOrigin::Generic),
            Type::SpecialForm(SpecialFormType::Protocol) => Some(LegacyGenericOrigin::Protocol),
            _ => None,
        } {
            let arguments = if let ast::Expr::Tuple(tuple) = subscript.slice.as_ref() {
                &*tuple.elts
            } else {
                std::slice::from_ref(subscript.slice.as_ref())
            };
            let has_invalid_unpack_argument = arguments.iter().any(|argument| {
                self.type_expression_flags(argument)
                    .contains(TypeExpressionFlags::INVALID_UNPACK)
            });
            let is_unpacked_typevartuple = |argument: &ast::Expr| {
                let operand = match argument {
                    ast::Expr::Starred(starred) => &*starred.value,
                    ast::Expr::Subscript(subscript)
                        if self.expression_type(&subscript.value)
                            == Type::SpecialForm(SpecialFormType::Unpack) =>
                    {
                        &*subscript.slice
                    }
                    _ => return false,
                };
                let argument_ty = self.expression_type(argument);
                let operand_ty = self.expression_type(operand);
                matches!(
                    argument_ty,
                    Type::TypeVar(typevar) if typevar.is_typevartuple(db)
                ) || matches!(
                    argument_ty.exact_tuple_instance_spec(db).as_deref(),
                    Some(Tuple::Variable(variable))
                        if variable.variable().typevartuple().is_some()
                ) || matches!(
                    operand_ty,
                    Type::NominalInstance(instance)
                        if matches!(
                            instance.known_class(db),
                            Some(KnownClass::TypeVarTuple | KnownClass::ExtensionsTypeVarTuple)
                        )
                )
            };
            // A tuple type can preserve only one variable segment, so count unpacked
            // `TypeVarTuple`s before the argument tuple is lowered to its type.
            let has_multiple_typevartuple_arguments = arguments
                .iter()
                .filter(|argument| is_unpacked_typevartuple(argument))
                .nth(1)
                .is_some();
            if has_multiple_typevartuple_arguments {
                let error = SubscriptError::new(
                    Type::unknown(),
                    SubscriptErrorKind::MultipleTypeVarTuples { origin },
                );
                error.report_diagnostics(&self.context, subscript);
                return Err(error.result_type());
            }
            if has_invalid_unpack_argument {
                let error = SubscriptError::new(
                    Type::unknown(),
                    SubscriptErrorKind::InvalidLegacyGenericArgument {
                        origin,
                        argument_ty: Type::SpecialForm(SpecialFormType::Unpack),
                    },
                );
                error.report_diagnostics(&self.context, subscript);
                return Err(error.result_type());
            }
        }

        // Special typing forms for which subscriptions are context-dependent are parsed here,
        // outside of `Type::subscript`, which is a pure function that doesn't depend on the
        // semantic index or any context-dependent state.
        let subscript_result = match value_ty {
            Type::SpecialForm(SpecialFormType::Generic) => infer_legacy_generic_subscript(
                db,
                env,
                self.index,
                self.scope().file_scope_id(db),
                self.typevar_binding_context,
                slice_ty,
                LegacyGenericOrigin::Generic,
                KnownInstanceType::SubscriptedGeneric,
            ),
            Type::SpecialForm(SpecialFormType::Protocol) => infer_legacy_generic_subscript(
                db,
                env,
                self.index,
                self.scope().file_scope_id(db),
                self.typevar_binding_context,
                slice_ty,
                LegacyGenericOrigin::Protocol,
                KnownInstanceType::SubscriptedProtocol,
            ),
            Type::SpecialForm(SpecialFormType::Concatenate) => {
                // TODO: Add proper support for `Concatenate`
                let mut variables = FxOrderSet::default();
                slice_ty.bind_and_find_all_legacy_typevars(
                    db,
                    env,
                    self.typevar_binding_context,
                    &mut variables,
                );
                let generic_context = GenericContext::from_typevar_instances(db, env, variables);
                Ok(Type::Dynamic(DynamicType::UnknownGeneric(generic_context)))
            }
            // basedpython: a keyword subscript is a `__getitem__` call carrying
            // more than the one index argument — `x[a, z=1]` lowers to
            // `x.__getitem__(a, z=1)`, and is checked as that call. reached only
            // here, so a generic class / alias / special form keeps the
            // specialization reading it was given above
            _ if self.is_basedpython_file() && keyword_subscript_elements(subscript).is_some() => {
                self.infer_keyword_subscript(subscript, value_ty, slice_ty, tcx)
            }
            _ => value_ty.subscript(db, env, slice_ty, expr_context, tcx),
        };

        subscript_result.map_err(|error| {
            error.report_diagnostics(&self.context, subscript);
            error.result_type()
        })
    }

    /// basedpython: check `x[a, z=1]` as the `x.__getitem__(a, z=1)` call it
    /// lowers to. Every element's type is already inferred — the slice as a
    /// whole was — so this only re-reads them into an argument list.
    fn infer_keyword_subscript(
        &self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
        slice_ty: Type<'db>,
        tcx: TypeContext<'db>,
    ) -> Result<Type<'db>, SubscriptError<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let Some(elements) = keyword_subscript_elements(subscript) else {
            return value_ty.subscript(db, env, slice_ty, ast::ExprContext::Load, tcx);
        };
        let arguments: CallArguments<'_, 'db> = elements
            .iter()
            .map(|element| match element {
                ast::Expr::Named(named) => {
                    let ty = self.expression_type(&named.value);
                    match named.label() {
                        Some(label) => (Argument::Keyword(&label.id), Some(ty)),
                        None => (Argument::Positional, Some(ty)),
                    }
                }
                _ => (Argument::Positional, Some(self.expression_type(element))),
            })
            .collect();
        match value_ty.try_call_dunder(db, env, "__getitem__", arguments, tcx) {
            Ok(outcome) => Ok(outcome.return_type(db, env)),
            Err(CallDunderError::PossiblyUnbound { bindings, .. }) => Err(SubscriptError::new(
                bindings.return_type(db, env),
                SubscriptErrorKind::DunderPossiblyUnbound {
                    method: DunderMethod::GetItem,
                    value_ty,
                },
            )),
            Err(CallDunderError::CallError(_, bindings, _)) => Err(SubscriptError::new(
                bindings.return_type(db, env),
                SubscriptErrorKind::KeywordSubscriptCallError { bindings },
            )),
            Err(CallDunderError::MethodNotAvailable) => Err(SubscriptError::new(
                Type::unknown(),
                SubscriptErrorKind::NotSubscriptable {
                    value_ty,
                    method: DunderMethod::GetItem,
                },
            )),
        }
    }

    pub(super) fn infer_slice_expression(&mut self, slice: &ast::ExprSlice) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprSlice {
            range: _,
            node_index: _,
            lower,
            upper,
            step,
        } = slice;

        let ty_lower = self.infer_optional_expression(lower.as_deref(), TypeContext::default());
        let ty_upper = self.infer_optional_expression(upper.as_deref(), TypeContext::default());
        let ty_step = self.infer_optional_expression(step.as_deref(), TypeContext::default());

        KnownClass::Slice.to_specialized_instance(
            db,
            env,
            &[
                ty_lower.unwrap_or_else(|| Type::none(db, env)),
                ty_upper.unwrap_or_else(|| Type::none(db, env)),
                ty_step.unwrap_or_else(|| Type::none(db, env)),
            ],
        )
    }

    /// Validate a subscript assignment of the form `object[key] = rhs_value`.
    pub(super) fn validate_subscript_assignment(
        &mut self,
        target: &ast::ExprSubscript,
        rhs_value: &ast::Expr,
        object_ty: Type<'db>,
        infer_slice_ty: &mut dyn FnMut(&mut Self, TypeContext<'db>) -> Type<'db>,
        infer_rhs_value: &mut dyn FnMut(&mut Self, TypeContext<'db>) -> Type<'db>,
    ) -> bool {
        let env = self.program_environment();
        let ast::ExprSubscript {
            range: _,
            node_index: _,
            value: object,
            slice,
            ctx: _,
            is_typeof: _,
        } = target;

        let db = self.db();

        self.store_typed_dict_key_expected_type(slice, object_ty);

        // basedpython use-site variance: `Container[out T]` rejects writes
        // outright — the underlying T position in `__setitem__` is
        // contravariant and projects to `Never` under an `out` projection.
        // Emit a focused diagnostic and short-circuit before calling the
        // dunder.
        if instance_has_covariant_projection(self.db(), env, object_ty) {
            let slice_ty = self.infer_expression(slice, TypeContext::default());
            let rhs_ty = infer_rhs_value(self, TypeContext::default());
            if let Some(builder) = self
                .context
                .report_lint(&INVALID_ASSIGNMENT, target.slice.as_ref())
            {
                builder.into_diagnostic(format_args!(
                    "Invalid subscript assignment with key of type `{}` and value of \
                     type `{}` on object of type `{}`",
                    slice_ty.display(self.db(), env),
                    rhs_ty.display(self.db(), env),
                    object_ty.display(self.db(), env),
                ));
            }
            return false;
        }

        let is_valid_assignment = self.validate_subscript_assignment_impl(
            target,
            None,
            object_ty,
            infer_slice_ty,
            rhs_value,
            infer_rhs_value,
            true,
        );

        // Record the constraints for the object of the subscript assignment, if the object is an
        // unannotated collection initializer.
        if is_valid_assignment
            && self.fluid_specializations_enabled()
            && let Some(collection_def) = self.index.fluid_candidate_binding(object)
            && let Some((class_literal, _)) = object_ty.class_specialization(db, env)
        {
            let identity_instance =
                Type::instance(db, env, class_literal.identity_specialization(db));
            let collection_generic_context = class_literal.generic_context(db);

            let ast_arguments = [
                ArgOrKeyword::Arg(&target.slice),
                ArgOrKeyword::Arg(rhs_value),
            ];

            let mut call_arguments = CallArguments::positional([Type::unknown(), Type::unknown()]);

            if let Place::Defined(DefinedPlace {
                ty: dunder_callable,
                definedness: boundness,
                ..
            }) = identity_instance
                .member_lookup_with_policy(
                    db,
                    env,
                    "__setitem__",
                    MemberLookupPolicy::NO_INSTANCE_FALLBACK,
                )
                .place
            {
                let mut identity_bindings = dunder_callable
                    .bindings(db, env)
                    .match_parameters(db, env, &call_arguments)
                    // Perform inference against the type variables on the receiver's generic context.
                    .with_generic_context(db, collection_generic_context);

                let call_result = self
                    .speculate_without_diagnostics()
                    .infer_and_check_argument_types(
                        ArgumentsIter::synthesized(&ast_arguments),
                        &mut call_arguments,
                        &mut |builder, (_, expr, tcx)| {
                            // TODO: The argument types have already been inferred and stored in `call_arguments`.
                            // However, `object` would have been inferred to a be a collection with `Divergent`
                            // element types, meaning the type context for a given argument, by which the inferred
                            // type is keyed, may not be the same as the type context we get here. It is not immediately
                            // clear how to retrieve those types, and so we just re-infer the argument expressions
                            // for simplicity.
                            builder.infer_maybe_standalone_expression(expr, tcx)
                        },
                        &mut identity_bindings,
                        TypeContext::default(),
                    );

                if call_result.is_ok() && boundness == Definedness::AlwaysDefined {
                    for call_specialization in identity_bindings
                        .iter_flat()
                        .flat_map(CallableBinding::matching_overloads)
                        .filter_map(|(_, identity_overload)| {
                            identity_overload.specialization(db, env)
                        })
                    {
                        // Record the constraints on the receiver's generic context formed by
                        // the arguments to this dunder call.
                        let Some(constraints) = self.collection_use_constraint_from_specialization(
                            identity_instance,
                            collection_generic_context,
                            call_specialization,
                        ) else {
                            continue;
                        };

                        self.collection_use_constraints
                            .entry(collection_def)
                            .or_default()
                            .insert(constraints);
                    }
                }
            }
        }

        is_valid_assignment
    }

    #[expect(clippy::too_many_arguments)]
    fn validate_subscript_assignment_impl(
        &mut self,
        target: &ast::ExprSubscript,
        full_object_ty: Option<Type<'db>>,
        object_ty: Type<'db>,
        infer_slice_ty: &mut dyn FnMut(&mut Self, TypeContext<'db>) -> Type<'db>,
        rhs_value_node: &ast::Expr,
        infer_rhs_value: &mut dyn FnMut(&mut Self, TypeContext<'db>) -> Type<'db>,
        emit_diagnostic: bool,
    ) -> bool {
        let env = self.program_environment();
        let db = self.db();

        let attach_original_type_info = |diagnostic: &mut LintDiagnosticGuard| {
            if let Some(full_object_ty) = full_object_ty {
                diagnostic.info(format_args!(
                    "The full type of the subscripted object is `{}`",
                    full_object_ty.display(db, env)
                ));
            }
        };

        match object_ty {
            Type::Union(union) => {
                let mut infer_slice_ty = MultiInferenceGuard::new(infer_slice_ty);
                let mut infer_rhs_value = MultiInferenceGuard::new(infer_rhs_value);

                // Perform loud inference without type context, as there may be multiple
                // equally applicable type contexts for each union member.
                infer_slice_ty.infer_loud(self, TypeContext::default());
                infer_rhs_value.infer_loud(self, TypeContext::default());

                // Note that we use a loop here instead of .all(…) to avoid short-circuiting.
                // We need to keep iterating to emit all diagnostics.
                let mut valid = true;
                for element_ty in union.elements(db) {
                    valid &= self.validate_subscript_assignment_impl(
                        target,
                        full_object_ty.or(Some(object_ty)),
                        *element_ty,
                        &mut |builder, tcx| infer_slice_ty.infer_silent(builder, tcx),
                        rhs_value_node,
                        &mut |builder, tcx| infer_rhs_value.infer_silent(builder, tcx),
                        emit_diagnostic,
                    );
                }

                valid
            }

            Type::Intersection(intersection) => {
                let mut infer_slice_ty = MultiInferenceGuard::new(infer_slice_ty);
                let mut infer_rhs_value = MultiInferenceGuard::new(infer_rhs_value);

                let mut check_positive_elements = |emit_diagnostic_and_short_circuit| {
                    let mut valid = false;
                    for element_ty in intersection.positive(db) {
                        valid |= self.validate_subscript_assignment_impl(
                            target,
                            full_object_ty.or(Some(object_ty)),
                            *element_ty,
                            &mut |builder, tcx| infer_slice_ty.infer_silent(builder, tcx),
                            rhs_value_node,
                            &mut |builder, tcx| infer_rhs_value.infer_silent(builder, tcx),
                            emit_diagnostic_and_short_circuit,
                        );

                        if valid || emit_diagnostic_and_short_circuit {
                            // Otherwise, perform loud inference with the narrowed type context, or the
                            // type context of the first failing element.
                            infer_slice_ty.infer_loud(self, infer_slice_ty.last_tcx());
                            infer_rhs_value.infer_loud(self, infer_rhs_value.last_tcx());
                            break;
                        }
                    }

                    valid
                };

                // Perform an initial check of all elements. If the assignment is valid
                // for at least one element, we do not emit any diagnostics. Otherwise,
                // we re-run the check and emit a diagnostic on the first failing element.
                let valid = check_positive_elements(false);
                if !valid {
                    check_positive_elements(true);
                }

                valid
            }

            Type::EnumComplement(complement) => self.validate_subscript_assignment_impl(
                target,
                full_object_ty,
                complement.remaining_literal_union(db, env),
                infer_slice_ty,
                rhs_value_node,
                infer_rhs_value,
                emit_diagnostic,
            ),

            Type::TypedDict(typed_dict) => {
                // As an optimization, prevent calling `__setitem__` on (unions of) large `TypedDict`s, and
                // validate the assignment ourselves. This also allows us to emit better diagnostics.

                let mut valid = true;
                let slice_ty = infer_slice_ty(self, TypeContext::default());
                let Some(keys) = string_literal_values(db, slice_ty) else {
                    // Check if the key has a valid type. We only allow string literals, a union of string literals,
                    // or a dynamic type like `Any`. We can do this by checking assignability to `LiteralString`,
                    // but we need to exclude `LiteralString` itself. This check would technically allow weird key
                    // types like `LiteralString & Any` to pass, but it does not need to be perfect. We would just
                    // fail to provide the "can only be subscripted with a string literal key" hint in that case.

                    if slice_ty.is_dynamic() {
                        return true;
                    }

                    if slice_ty.is_assignable_to(db, env, KnownClass::Str.to_instance(db, env))
                        && let Some(expected_ty) = typed_dict.arbitrary_key_mutation_type(db, env)
                    {
                        let rhs_value_ty =
                            infer_rhs_value(self, TypeContext::new(Some(expected_ty)));
                        if rhs_value_ty.is_assignable_to(db, env, expected_ty) {
                            return true;
                        }

                        if emit_diagnostic
                            && let Some(builder) = self
                                .context
                                .report_lint(&INVALID_ASSIGNMENT, rhs_value_node)
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Cannot assign value of type `{}` to key of type `{}` \
                                on TypedDict `{}`",
                                rhs_value_ty.display(db, env),
                                slice_ty.display(db, env),
                                object_ty.display(db, env),
                            ));
                            diagnostic.set_primary_annotation_message(format_args!(
                                "Expected value assignable to `{}`",
                                expected_ty.display(db, env)
                            ));
                            attach_original_type_info(&mut diagnostic);
                        }
                        return false;
                    }

                    let rhs_value_ty = infer_rhs_value(self, TypeContext::default());
                    let assigned_d = rhs_value_ty.display(db, env);
                    let value_d = object_ty.display(db, env);

                    if slice_ty.is_assignable_to(db, env, Type::literal_string())
                        && !slice_ty.is_equivalent_to(db, env, Type::literal_string())
                    {
                        if let Some(builder) = self
                            .context
                            .report_lint(&INVALID_ASSIGNMENT, target.slice.as_ref())
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Cannot assign value of type `{assigned_d}` to key of type `{}` \
                                on TypedDict `{value_d}`",
                                slice_ty.display(db, env)
                            ));
                            attach_original_type_info(&mut diagnostic);
                        }
                    } else {
                        if let Some(builder) = self
                            .context
                            .report_lint(&INVALID_KEY, target.slice.as_ref())
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "TypedDict `{value_d}` can only be subscripted \
                                with a string literal key, got key of type `{}`.",
                                slice_ty.display(db, env)
                            ));
                            attach_original_type_info(&mut diagnostic);
                        }
                    }

                    return false;
                };

                // We may need to infer the value multiple times for distinct keys.
                let mut key_count = 0;
                let mut infer_rhs_value = MultiInferenceGuard::new(infer_rhs_value);

                for key in keys {
                    // Infer the value with type context.
                    let item = typed_dict.item(db, key);
                    let value_ty = infer_rhs_value.infer_silent(
                        self,
                        TypeContext::new(item.as_ref().map(|item| item.declared_ty)),
                    );

                    if item.is_some() {
                        key_count += 1;
                    }
                    valid &= TypedDictKeyAssignment {
                        context: &self.context,
                        typed_dict,
                        full_object_ty,
                        key,
                        value_ty,
                        typed_dict_node: target.value.as_ref().into(),
                        key_node: target.slice.as_ref().into(),
                        value_node: rhs_value_node.into(),
                        assignment_kind: TypedDictAssignmentKind::Subscript,
                        emit_diagnostic,
                    }
                    .validate();
                }

                // Perform loud inference with type context if there is a single key.
                if key_count == 1 {
                    infer_rhs_value.infer_loud(self, infer_rhs_value.last_tcx());
                } else {
                    infer_rhs_value.infer_loud(self, TypeContext::default());
                }

                valid
            }

            _ => {
                let ast_arguments = [
                    ArgOrKeyword::Arg(&target.slice),
                    ArgOrKeyword::Arg(rhs_value_node),
                ];

                let mut call_arguments =
                    CallArguments::positional([Type::unknown(), Type::unknown()]);

                let mut infer_argument_ty =
                    |builder: &mut Self, (argument_index, _, tcx): ArgExpr<'db, '_>| {
                        match argument_index {
                            0 => infer_slice_ty(builder, tcx),
                            1 => infer_rhs_value(builder, tcx),
                            _ => unreachable!(),
                        }
                    };

                let Err(call_dunder_err) = self.infer_and_try_call_dunder(
                    object_ty,
                    "__setitem__",
                    MemberLookupPolicy::NO_INSTANCE_FALLBACK,
                    ArgumentsIter::synthesized(&ast_arguments),
                    &mut call_arguments,
                    &mut infer_argument_ty,
                    TypeContext::default(),
                ) else {
                    return true;
                };

                match call_dunder_err {
                    CallDunderError::PossiblyUnbound { .. } => {
                        if emit_diagnostic
                            && let Some(builder) = self
                                .context
                                .report_lint(&POSSIBLY_MISSING_IMPLICIT_CALL, target)
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Method `__setitem__` of type `{}` may be missing",
                                object_ty.display(db, env),
                            ));
                            attach_original_type_info(&mut diagnostic);
                        }
                        false
                    }
                    CallDunderError::CallError(call_error_kind, bindings, _) => {
                        let slice_ty = bindings.type_for_argument(&call_arguments, 0);
                        let rhs_value_ty = bindings.type_for_argument(&call_arguments, 1);

                        match call_error_kind {
                            CallErrorKind::NotCallable => {
                                if emit_diagnostic
                                    && let Some(builder) =
                                        self.context.report_lint(&CALL_NON_CALLABLE, target)
                                {
                                    let mut diagnostic = builder.into_diagnostic(format_args!(
                                        "Method `__setitem__` of type `{}` is not callable \
                                             on object of type `{}`",
                                        bindings.callable_type().display(db, env),
                                        object_ty.display(db, env),
                                    ));
                                    attach_original_type_info(&mut diagnostic);
                                }
                            }
                            CallErrorKind::BindingError => {
                                if let Some(typed_dict) = object_ty.as_typed_dict() {
                                    if let Some(key) = slice_ty.as_string_literal() {
                                        let key = key.value(db);
                                        TypedDictKeyAssignment {
                                            context: &self.context,
                                            typed_dict,
                                            full_object_ty,
                                            key,
                                            value_ty: rhs_value_ty,
                                            typed_dict_node: target.value.as_ref().into(),
                                            key_node: target.slice.as_ref().into(),
                                            value_node: rhs_value_node.into(),
                                            assignment_kind: TypedDictAssignmentKind::Subscript,
                                            emit_diagnostic: true,
                                        }
                                        .validate();
                                    }
                                } else {
                                    if emit_diagnostic
                                        && let Some(builder) = self.context.report_lint(
                                            &INVALID_ASSIGNMENT,
                                            target.range.cover(rhs_value_node.range()),
                                        )
                                    {
                                        let assigned_d = rhs_value_ty.display(db, env);
                                        let object_d = object_ty.display(db, env);

                                        let mut diagnostic = builder.into_diagnostic(format_args!(
                                            "Invalid subscript assignment with key of type `{}` \
                                            and value of type `{assigned_d}` \
                                            on object of type `{object_d}`",
                                            slice_ty.display(db, env),
                                        ));

                                        // Special diagnostic for dictionaries
                                        if let Some([expected_key_ty, expected_value_ty]) =
                                            object_ty
                                                .known_specialization(db, env, KnownClass::Dict)
                                                .map(|s| s.types(db))
                                        {
                                            if !slice_ty.is_assignable_to(db, env, *expected_key_ty)
                                            {
                                                diagnostic.annotate(
                                                    self.context
                                                        .secondary(target.slice.as_ref())
                                                        .message(format_args!(
                                                            "Expected key of type `{}`, got `{}`",
                                                            expected_key_ty.display(db, env),
                                                            slice_ty.display(db, env),
                                                        )),
                                                );
                                            }

                                            if !rhs_value_ty.is_assignable_to(
                                                db,
                                                env,
                                                *expected_value_ty,
                                            ) {
                                                diagnostic.annotate(
                                                    self.context.secondary(rhs_value_node).message(
                                                        format_args!(
                                                            "Expected value of type `{}`, got `{}`",
                                                            expected_value_ty.display(db, env),
                                                            rhs_value_ty.display(db, env),
                                                        ),
                                                    ),
                                                );
                                            }
                                        }

                                        attach_original_type_info(&mut diagnostic);
                                    }
                                }
                            }
                            CallErrorKind::PossiblyNotCallable => {
                                if emit_diagnostic
                                    && let Some(builder) =
                                        self.context.report_lint(&CALL_NON_CALLABLE, target)
                                {
                                    let mut diagnostic = builder.into_diagnostic(format_args!(
                                        "Method `__setitem__` of type `{}` may not be callable \
                                        on object of type `{}`",
                                        bindings.callable_type().display(db, env),
                                        object_ty.display(db, env),
                                    ));
                                    attach_original_type_info(&mut diagnostic);
                                }
                            }
                        }
                        false
                    }
                    CallDunderError::MethodNotAvailable => {
                        if emit_diagnostic
                            && let Some(builder) =
                                self.context.report_lint(&INVALID_ASSIGNMENT, target)
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Cannot assign to a subscript on an object of type `{}`",
                                object_ty.display(db, env),
                            ));
                            attach_original_type_info(&mut diagnostic);

                            // If it's a user-defined class, suggest adding a `__setitem__` method.
                            if object_ty
                                .as_nominal_instance()
                                .and_then(|instance| {
                                    instance.class(db, env).static_class_literal(db)
                                })
                                .and_then(|(class_literal, _)| {
                                    let file = class_literal.program_file(db);
                                    file_to_module(db, file.resolver_file(db))
                                })
                                .and_then(|module| module.search_path(db))
                                .is_some_and(ty_module_resolver::SearchPath::is_first_party)
                            {
                                diagnostic.help(format_args!(
                                    "Consider adding a `__setitem__` method to `{}`.",
                                    object_ty.display(db, env),
                                ));
                            } else {
                                diagnostic.info(format_args!(
                                    "`{}` does not have a `__setitem__` method.",
                                    object_ty.display(db, env),
                                ));
                            }
                        }
                        false
                    }
                }
            }
        }
    }

    /// Validate a subscript deletion of the form `del object[key]`.
    fn validate_subscript_deletion(
        &self,
        target: &ast::ExprSubscript,
        object_ty: Type<'db>,
        slice_ty: Type<'db>,
    ) {
        self.validate_subscript_deletion_impl(target, None, object_ty, slice_ty);
    }

    fn validate_subscript_deletion_impl(
        &self,
        target: &'ast ast::ExprSubscript,
        full_object_ty: Option<Type<'db>>,
        object_ty: Type<'db>,
        slice_ty: Type<'db>,
    ) {
        let env = self.program_environment();
        let db = self.db();

        let attach_original_type_info = |diagnostic: &mut LintDiagnosticGuard| {
            if let Some(full_object_ty) = full_object_ty {
                diagnostic.info(format_args!(
                    "The full type of the subscripted object is `{}`",
                    full_object_ty.display(db, env)
                ));
            }
        };

        match object_ty {
            Type::Union(union) => {
                for element_ty in union.elements(db) {
                    self.validate_subscript_deletion_impl(
                        target,
                        full_object_ty.or(Some(object_ty)),
                        *element_ty,
                        slice_ty,
                    );
                }
            }

            Type::Intersection(intersection) => {
                // Check if any positive element supports deletion
                let positive = intersection.positive(db);
                let mut any_valid = false;
                for element_ty in positive {
                    if self.can_delete_subscript(*element_ty, slice_ty) {
                        any_valid = true;
                        break;
                    }
                }

                // If none are valid, emit a diagnostic for the first failing element
                if !any_valid && let Some(element_ty) = positive.first() {
                    self.validate_subscript_deletion_impl(
                        target,
                        full_object_ty.or(Some(object_ty)),
                        *element_ty,
                        slice_ty,
                    );
                }
            }

            Type::EnumComplement(complement) => self.validate_subscript_deletion_impl(
                target,
                full_object_ty,
                complement.remaining_literal_union(db, env),
                slice_ty,
            ),

            _ => {
                if let Type::TypedDict(typed_dict) = object_ty {
                    // Known undeclared keys can only refer to explicit extra items, so they can be
                    // deleted whenever those items are mutable. An arbitrary string key could
                    // instead refer to any declared field, so deletion is only safe when all
                    // possible fields are optional and mutable.
                    let can_delete_extra_literals = typed_dict
                        .explicit_extra_items(db)
                        .is_some_and(|extra_items| !extra_items.is_read_only())
                        && string_literal_values(db, slice_ty).is_some_and(|mut literals| {
                            literals.all(|literal| !typed_dict.items(db).contains_key(literal))
                        });
                    let can_delete_arbitrary_key =
                        slice_ty.is_assignable_to(db, env, KnownClass::Str.to_instance(db, env))
                            && typed_dict.supports_arbitrary_key_deletion(db);
                    if can_delete_extra_literals || can_delete_arbitrary_key {
                        return;
                    }
                }

                let Err(err) = object_ty.try_call_dunder(
                    db,
                    env,
                    "__delitem__",
                    CallArguments::positional([slice_ty]),
                    TypeContext::default(),
                ) else {
                    return;
                };

                match err {
                    CallDunderError::PossiblyUnbound { .. } => {
                        if let Some(builder) = self
                            .context
                            .report_lint(&POSSIBLY_MISSING_IMPLICIT_CALL, target)
                        {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Method `__delitem__` of type `{}` may be missing",
                                object_ty.display(db, env),
                            ));
                            attach_original_type_info(&mut diagnostic);
                        }
                    }
                    CallDunderError::CallError(call_error_kind, bindings, _) => {
                        match call_error_kind {
                            CallErrorKind::NotCallable => {
                                if let Some(builder) =
                                    self.context.report_lint(&CALL_NON_CALLABLE, target)
                                {
                                    let mut diagnostic = builder.into_diagnostic(format_args!(
                                        "Method `__delitem__` of type `{}` \
                                        is not callable on object of type `{}`",
                                        bindings.callable_type().display(db, env),
                                        object_ty.display(db, env),
                                    ));
                                    attach_original_type_info(&mut diagnostic);
                                }
                            }
                            CallErrorKind::BindingError => {
                                // For deletions of string literal keys on `TypedDict`, provide
                                // a more detailed diagnostic.
                                if let Some(typed_dict) = object_ty.as_typed_dict() {
                                    if let Some(string_literal) = slice_ty.as_string_literal() {
                                        let key = string_literal.value(db);
                                        let items = typed_dict.items(db);

                                        if let Some(field) = items.get(key) {
                                            // Key exists but is required (i.e., can't be deleted).
                                            report_cannot_delete_typed_dict_key(
                                                &self.context,
                                                (&*target.slice).into(),
                                                typed_dict,
                                                key,
                                                Some(field),
                                                TypedDictDeleteErrorKind::RequiredKey,
                                            );
                                        } else if typed_dict
                                            .explicit_extra_items(db)
                                            .is_some_and(TypedDictExtraItems::is_read_only)
                                        {
                                            report_cannot_delete_typed_dict_key(
                                                &self.context,
                                                (&*target.slice).into(),
                                                typed_dict,
                                                key,
                                                None,
                                                TypedDictDeleteErrorKind::ReadOnlyExtraItem,
                                            );
                                        } else {
                                            // Key doesn't exist.
                                            report_cannot_delete_typed_dict_key(
                                                &self.context,
                                                (&*target.slice).into(),
                                                typed_dict,
                                                key,
                                                None,
                                                TypedDictDeleteErrorKind::UnknownKey,
                                            );
                                        }
                                    } else {
                                        // Non-string-literal key on `TypedDict`.
                                        if let Some(builder) =
                                            self.context.report_lint(&INVALID_ARGUMENT_TYPE, target)
                                        {
                                            let mut diagnostic =
                                                builder.into_diagnostic(format_args!(
                                                    "Method `__delitem__` of type `{}` \
                                                    cannot be called with key of type \
                                                    `{}` on object of type `{}`",
                                                    bindings.callable_type().display(db, env),
                                                    slice_ty.display(db, env),
                                                    object_ty.display(db, env),
                                                ));
                                            attach_original_type_info(&mut diagnostic);
                                        }
                                    }
                                } else {
                                    // Non-`TypedDict` object
                                    if let Some(builder) =
                                        self.context.report_lint(&INVALID_ARGUMENT_TYPE, target)
                                    {
                                        let mut diagnostic = builder.into_diagnostic(format_args!(
                                            "Method `__delitem__` of type `{}` cannot \
                                            be called with key of type `{}` on \
                                            object of type `{}`",
                                            bindings.callable_type().display(db, env),
                                            slice_ty.display(db, env),
                                            object_ty.display(db, env),
                                        ));
                                        attach_original_type_info(&mut diagnostic);
                                    }
                                }
                            }
                            CallErrorKind::PossiblyNotCallable => {
                                if let Some(builder) =
                                    self.context.report_lint(&CALL_NON_CALLABLE, target)
                                {
                                    let mut diagnostic = builder.into_diagnostic(format_args!(
                                        "Method `__delitem__` of type `{}` may not be \
                                        callable on object of type `{}`",
                                        bindings.callable_type().display(db, env),
                                        object_ty.display(db, env),
                                    ));
                                    attach_original_type_info(&mut diagnostic);
                                }
                            }
                        }
                    }
                    CallDunderError::MethodNotAvailable => {
                        report_not_subscriptable(&self.context, target, object_ty, "__delitem__");
                    }
                }
            }
        }
    }

    /// Check if a type supports subscript deletion (has `__delitem__`).
    fn can_delete_subscript(&self, object_ty: Type<'db>, slice_ty: Type<'db>) -> bool {
        let db = self.db();
        object_ty
            .try_call_dunder(
                db,
                self.program_environment(),
                "__delitem__",
                CallArguments::positional([slice_ty]),
                TypeContext::default(),
            )
            .is_ok()
    }

    pub(super) fn parse_subscription_of_annotated_special_form(
        &mut self,
        subscript: &ast::ExprSubscript,
        subscript_context: AnnotatedExprContext,
    ) -> TypeAndQualifiers<'db> {
        let slice = &*subscript.slice;
        let ast::Expr::Tuple(ast::ExprTuple {
            elts: arguments, ..
        }) = slice
        else {
            report_invalid_arguments_to_annotated(&self.context, subscript);
            return subscript_context.infer(self, slice);
        };

        if arguments.len() < 2 {
            report_invalid_arguments_to_annotated(&self.context, subscript);
        }

        let Some(first_argument) = arguments.first() else {
            self.infer_expression(slice, TypeContext::default());
            return TypeAndQualifiers::declared(Type::unknown());
        };

        let previous_in_type_alias = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_TYPE_ALIAS, false);
        for metadata_element in &arguments[1..] {
            self.infer_expression(metadata_element, TypeContext::default());
        }
        self.context
            .inference_flags
            .set(InferenceFlags::IN_TYPE_ALIAS, previous_in_type_alias);

        subscript_context.infer(self, first_argument)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyGenericContextError<'db> {
    /// It's invalid to subscript `Generic` or `Protocol` with this type.
    InvalidArgument(Type<'db>),
    /// It's invalid to subscript `Generic` or `Protocol` with a variadic tuple type.
    /// We should emit a diagnostic for this, but we don't yet.
    VariadicTupleArguments,
    /// It's valid to subscribe `Generic` or `Protocol` with this type,
    /// but the type is not yet supported.
    NotYetSupported,
    /// A duplicate typevar was provided.
    DuplicateTypevar(&'db str),
    /// A `TypeVarTuple` was provided but not unpacked.
    ///
    /// The generic context is available when the argument is a bound `TypeVarTuple` and is used
    /// to avoid cascading errors during recovery.
    TypeVarTupleMustBeUnpacked(Option<GenericContext<'db>>),
}

impl<'db> LegacyGenericContextError<'db> {
    const fn into_type(self) -> Type<'db> {
        match self {
            LegacyGenericContextError::InvalidArgument(_)
            | LegacyGenericContextError::VariadicTupleArguments
            | LegacyGenericContextError::DuplicateTypevar(_)
            | LegacyGenericContextError::TypeVarTupleMustBeUnpacked(_) => Type::unknown(),
            LegacyGenericContextError::NotYetSupported => {
                todo_type!("ParamSpecs and TypeVarTuples")
            }
        }
    }
}

/// Validate the type arguments to `Generic[...]` or `Protocol[...]`, returning
/// either the resulting [`GenericContext`] or a [`SubscriptError`].
#[expect(clippy::too_many_arguments)]
fn infer_legacy_generic_subscript<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    index: &'db SemanticIndex<'db>,
    file_scope_id: FileScopeId,
    typevar_binding_context: Option<Definition<'db>>,
    slice_ty: Type<'db>,
    origin: LegacyGenericOrigin,
    wrap_ok: impl FnOnce(GenericContext<'db>) -> KnownInstanceType<'db>,
) -> Result<Type<'db>, SubscriptError<'db>> {
    match legacy_generic_class_context(
        db,
        env,
        index,
        file_scope_id,
        typevar_binding_context,
        slice_ty,
    ) {
        Ok(context) => Ok(Type::KnownInstance(wrap_ok(context))),
        Err(LegacyGenericContextError::InvalidArgument(argument_ty)) => Err(SubscriptError::new(
            Type::unknown(),
            SubscriptErrorKind::InvalidLegacyGenericArgument {
                origin,
                argument_ty,
            },
        )),
        Err(LegacyGenericContextError::DuplicateTypevar(typevar_name)) => Err(SubscriptError::new(
            Type::unknown(),
            SubscriptErrorKind::DuplicateTypevar {
                origin,
                typevar_name,
            },
        )),
        Err(LegacyGenericContextError::TypeVarTupleMustBeUnpacked(generic_context)) => {
            Err(SubscriptError::new(
                generic_context.map_or(Type::unknown(), |generic_context| {
                    Type::KnownInstance(wrap_ok(generic_context))
                }),
                SubscriptErrorKind::TypeVarTupleNotUnpacked { origin },
            ))
        }
        Err(
            error @ (LegacyGenericContextError::NotYetSupported
            | LegacyGenericContextError::VariadicTupleArguments),
        ) => Ok(error.into_type()),
    }
}

/// Parse the type arguments to `Generic[...]` or `Protocol[...]` and validate
/// that each argument is a type variable.
fn legacy_generic_class_context<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    index: &'db SemanticIndex<'db>,
    file_scope_id: FileScopeId,
    typevar_binding_context: Option<Definition<'db>>,
    typevars: Type<'db>,
) -> Result<GenericContext<'db>, LegacyGenericContextError<'db>> {
    let typevars_class_tuple_spec = typevars.exact_tuple_instance_spec(db);

    let unpacked_typevars;
    let typevars = if let Some(tuple_spec) = typevars_class_tuple_spec.as_deref() {
        match tuple_spec {
            Tuple::Fixed(typevars) => typevars.elements_slice(),
            Tuple::Variable(variable) => {
                if let VariableSegment::TypeVarTuple(typevartuple) = variable.variable() {
                    unpacked_typevars = variable
                        .iter_prefix_elements()
                        .chain(std::iter::once(Type::TypeVar(typevartuple)))
                        .chain(variable.iter_suffix_elements())
                        .collect::<Vec<_>>();
                    &unpacked_typevars
                } else {
                    return Err(LegacyGenericContextError::VariadicTupleArguments);
                }
            }
        }
    } else {
        std::slice::from_ref(&typevars)
    };

    let mut validated_typevars = FxOrderSet::default();
    for ty in typevars {
        let argument_ty = *ty;
        if let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = argument_ty {
            let bound = bind_typevar(db, index, file_scope_id, typevar_binding_context, typevar)
                .ok_or(LegacyGenericContextError::InvalidArgument(argument_ty))?;
            if bound.is_typevartuple(db) {
                validated_typevars.insert(bound);
                return Err(LegacyGenericContextError::TypeVarTupleMustBeUnpacked(Some(
                    GenericContext::from_typevar_instances(db, env, validated_typevars),
                )));
            }
            if !validated_typevars.insert(bound) {
                return Err(LegacyGenericContextError::DuplicateTypevar(
                    typevar.name(db),
                ));
            }
        } else if let Type::TypeVar(bound) = argument_ty
            && bound.is_typevartuple(db)
        {
            if !validated_typevars.insert(bound) {
                return Err(LegacyGenericContextError::DuplicateTypevar(bound.name(db)));
            }
        } else if let Type::NominalInstance(instance) = argument_ty
            && matches!(
                instance.known_class(db),
                Some(KnownClass::TypeVarTuple | KnownClass::ExtensionsTypeVarTuple)
            )
        {
            return Err(LegacyGenericContextError::TypeVarTupleMustBeUnpacked(None));
        } else if any_over_type(db, env, argument_ty, true, |inner_ty| match inner_ty {
            Type::NominalInstance(nominal) => matches!(
                nominal.known_class(db),
                Some(KnownClass::TypeVarTuple | KnownClass::ExtensionsTypeVarTuple)
            ),
            _ => false,
        }) {
            return Err(LegacyGenericContextError::NotYetSupported);
        } else {
            return Err(LegacyGenericContextError::InvalidArgument(argument_ty));
        }
    }
    Ok(GenericContext::from_typevar_instances(
        db,
        env,
        validated_typevars,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnnotatedExprContext {
    TypeExpression,
    AnnotationExpression,
}

impl AnnotatedExprContext {
    fn infer<'db>(
        self,
        builder: &mut TypeInferenceBuilder<'db, '_>,
        argument: &ast::Expr,
    ) -> TypeAndQualifiers<'db> {
        match self {
            AnnotatedExprContext::TypeExpression => {
                let inner = builder.infer_type_expression(argument);
                let outer = Type::KnownInstance(KnownInstanceType::Annotated(InternedType::new(
                    builder.db(),
                    inner,
                )));
                TypeAndQualifiers::declared(outer)
            }
            AnnotatedExprContext::AnnotationExpression => {
                let inner =
                    builder.infer_annotation_expression_impl(argument, PEP613Policy::Disallowed);
                let outer = Type::KnownInstance(KnownInstanceType::Annotated(InternedType::new(
                    builder.db(),
                    inner.inner_type(),
                )));
                TypeAndQualifiers::declared(outer).with_qualifier(inner.qualifiers())
            }
        }
    }
}

/// Returns `true` if `object_ty` is an instance of a generic class whose
/// specialization carries a covariant (`out`) use-site projection on any of
/// its typevars. Used by `validate_subscript_assignment` to short-circuit
/// writes — a covariantly-projected typevar appears in the `__setitem__`
/// value parameter (a contravariant position), and projects to `Never`,
/// so no assignment can succeed.
fn instance_has_covariant_projection<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    object_ty: Type<'db>,
) -> bool {
    use ruff_python_ast::helpers::UseSiteVariance;

    let Some(instance) = object_ty.as_nominal_instance() else {
        return false;
    };
    let crate::types::ClassType::Generic(alias) = instance.class(db, env) else {
        return false;
    };
    alias
        .specialization(db)
        .projections(db)
        .iter()
        .any(|p| matches!(p, Some(UseSiteVariance::Out)))
}

/// Symmetric helper for contravariant (`in`) projection — used by subscript
/// READS to reject calls to `__getitem__`.
fn instance_has_contravariant_projection<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    object_ty: Type<'db>,
) -> bool {
    use ruff_python_ast::helpers::UseSiteVariance;

    let Some(instance) = object_ty.as_nominal_instance() else {
        return false;
    };
    let crate::types::ClassType::Generic(alias) = instance.class(db, env) else {
        return false;
    };
    alias
        .specialization(db)
        .projections(db)
        .iter()
        .any(|p| matches!(p, Some(UseSiteVariance::In)))
}
