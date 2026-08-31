use compact_str::CompactString;
use itertools::Either;
use ruff_python_ast::helpers::{
    UseSiteVariance, is_dotted_name, is_top_star_marker, top_star_marker_ranges_in_slice,
    top_star_slice_elements, type_modifier_marker, use_site_variance_marker,
};
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, ParameterBorrow, PythonVersion};
use ruff_text_size::{Ranged, TextRange};

use super::{DeferredExpressionState, TypeInferenceBuilder};
use crate::types::ClassType;
use crate::types::call::CallArguments;
use crate::types::diagnostic::{
    self, EXPERIMENTAL_SYNTAX, INVALID_PARAMSPEC, INVALID_TYPE_FORM, NOT_SUBSCRIPTABLE,
    UNBOUND_TYPE_VARIABLE, UNRESOLVED_ATTRIBUTE, UNSUPPORTED_OPERATOR,
    report_invalid_argument_number_to_special_form, report_invalid_arguments_to_callable,
    report_invalid_concatenate_last_arg, report_missing_type_arguments,
    report_unsupported_binary_operation,
};
use crate::types::function::{FunctionDecorators, FunctionType};
use crate::types::infer::builder::subscript::AnnotatedExprContext;
use crate::types::infer::{InferenceFlags, TypeExpressionFlags};
use crate::types::signatures::{ConcatenateTail, Signature};
use crate::types::special_form::{AliasSpec, LegacyStdlibAlias};
use crate::types::string_annotation::parse_string_annotation;
use crate::types::template::{Promotable, TemplateLiteralType, TemplatePart};
use crate::types::tuple::{TupleSpec, TupleSpecBuilder, TupleType};
use crate::types::type_fn::{
    TypeFnArguments, TypeFnOutcome, arity_mismatch, declared_return_type, evaluate_type_fn,
    first_bound_violation,
};
use ty_python_core::scope::ScopeKind;

use crate::types::ProgramEnvironment;
use crate::types::{
    BindingContext, BoundTypeVarInstance, CallableType, DeferredOperation, DeferredType,
    DynamicType, GenericContext, InternedType, IntersectionBuilder, IntersectionType, KnownClass,
    KnownInstanceType, LintDiagnosticGuard, LiteralValueTypeKind, OverlappingType,
    ParamSpecAttrKind, Parameter, Parameters, RestrictedType, SpecialFormType, SubclassOfType,
    Type, TypeContext, TypeFormType, TypeGuardType, TypeIsType, TypeMapping, TypeVarKind,
    UnionBuilder, UnionType, UnsafeUnionType, any_over_type, todo_type,
};
use crate::{FxOrderSet, add_inferred_python_version_hint_to_diagnostic};

/// Type expressions
impl<'db> TypeInferenceBuilder<'db, '_> {
    const fn type_expression_context(&self) -> &'static str {
        self.inference_flags().type_expression_context()
    }

    /// Infer the type of a type expression.
    pub(super) fn infer_type_expression(&mut self, expression: &ast::Expr) -> Type<'db> {
        let previous_deferred_state = self.deferred_state;
        let was_in_type_expression = self
            .inference_flags()
            .contains(InferenceFlags::IN_TYPE_EXPRESSION);
        let was_in_nested_type_expression = self
            .inference_flags()
            .contains(InferenceFlags::IN_NESTED_TYPE_EXPRESSION);
        let previously_in_nested_type_expression = self.context.inference_flags.replace(
            InferenceFlags::IN_NESTED_TYPE_EXPRESSION,
            was_in_type_expression || was_in_nested_type_expression,
        );
        let previously_in_type_expression = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_TYPE_EXPRESSION, true);

        // `DeferredExpressionState::InStringAnnotation` takes precedence over other states.
        // However, if it's not a stringified annotation, we must still ensure that annotation expressions
        // are always deferred in stub files.
        match previous_deferred_state {
            DeferredExpressionState::None => {
                if self.in_stub() {
                    self.deferred_state = DeferredExpressionState::Deferred;
                }
            }
            DeferredExpressionState::InStringAnnotation(_) | DeferredExpressionState::Deferred => {}
        }

        let ty = self.infer_type_expression_no_store(expression);
        self.deferred_state = previous_deferred_state;
        self.context.inference_flags.set(
            InferenceFlags::IN_NESTED_TYPE_EXPRESSION,
            previously_in_nested_type_expression,
        );
        self.context.inference_flags.set(
            InferenceFlags::IN_TYPE_EXPRESSION,
            previously_in_type_expression,
        );
        self.store_expression_type(expression, ty);
        ty
    }

    /// Similar to [`infer_type_expression`], but accepts a [`DeferredExpressionState`].
    ///
    /// [`infer_type_expression`]: TypeInferenceBuilder::infer_type_expression
    pub(super) fn infer_type_expression_with_state(
        &mut self,
        expression: &ast::Expr,
        deferred_state: DeferredExpressionState,
    ) -> Type<'db> {
        let previous_deferred_state = std::mem::replace(&mut self.deferred_state, deferred_state);
        let annotation_ty = self.infer_type_expression(expression);
        self.deferred_state = previous_deferred_state;
        annotation_ty
    }

    fn report_invalid_type_expression(
        &self,
        expression: impl Ranged,
        message: impl std::fmt::Display,
    ) -> Option<LintDiagnosticGuard<'_, '_>> {
        self.context
            .report_lint(&INVALID_TYPE_FORM, expression)
            .map(|builder| {
                diagnostic::add_type_expression_reference_link(builder.into_diagnostic(message))
            })
    }

    /// basedpython: an [attribute type](crate::types::deferred) whose receiver is
    /// itself a type expression — `X[A].x` is the type of `X`'s member `x` when `T`
    /// is `A`. The receiver is resolved as a type rather than as a value, so the
    /// lookup runs against an instance of it, exactly as it does for the bare
    /// type-parameter form.
    ///
    /// A receiver that still mentions a type parameter (`X[T].x`) keeps the whole
    /// thing symbolic; a ground one folds here and now.
    fn infer_attribute_type_expression(&mut self, attribute: &ast::ExprAttribute) -> Type<'db> {
        let env = self.program_environment();
        let receiver = self.infer_type_expression(&attribute.value);
        let db = self.db();
        let member = &attribute.attr.id;
        if receiver.member(db, env, member).place.is_undefined() {
            if let Some(builder) = self.context.report_lint(&UNRESOLVED_ATTRIBUTE, attribute) {
                builder.into_diagnostic(format_args!(
                    "Object of type `{}` has no attribute `{member}`",
                    receiver.display(db, env),
                ));
            }
            return Type::unknown();
        }
        DeferredType::build(
            db,
            env,
            &DeferredOperation::Attribute(member.clone()),
            Box::from([receiver]),
        )
    }

    /// Infer a dotted name that *is* a type expression, as opposed to one reached
    /// through a type expression's nested value inference. Only here does basedpython
    /// read `T.a` as an [attribute type](crate::types::deferred).
    pub(super) fn infer_dotted_type_expression(
        &mut self,
        attribute: &ast::ExprAttribute,
    ) -> Type<'db> {
        let previously_resolving = self
            .context
            .inference_flags
            .replace(InferenceFlags::RESOLVING_DOTTED_TYPE_EXPRESSION, true);
        let ty = self.infer_attribute_expression(attribute);
        self.context.inference_flags.set(
            InferenceFlags::RESOLVING_DOTTED_TYPE_EXPRESSION,
            previously_resolving,
        );
        ty
    }

    pub(super) fn infer_name_or_attribute_type_expression(
        &self,
        ty: Type<'db>,
        annotation: &ast::Expr,
    ) -> Type<'db> {
        // a dotted name whose lookup already produced a type names that type directly;
        // there is no value whose type-expression meaning still has to be taken. that
        // covers `P.args` / `P.kwargs` and basedpython's `T.a` attribute types
        let db = self.db();
        let env = self.program_environment();
        if annotation.is_attribute_expr()
            && match ty {
                Type::TypeVar(tvar) => tvar.paramspec_attr(self.db()).is_some(),
                Type::Deferred(deferred) => deferred.is_attribute(self.db()),
                _ => false,
            }
        {
            if let Type::TypeVar(tvar) = ty
                && let Some(attr) = tvar.paramspec_attr(self.db())
            {
                self.report_paramspec_attribute_spelling(annotation, tvar, attr);
            }
            return ty;
        }
        // basedpython: a bare enum member (`E.A`) in type position denotes its
        // enum-literal type — `a: E.A` is `a: Literal[E.A]`
        if self.is_basedpython_file() && ty.as_enum_literal().is_some() {
            return ty;
        }
        report_missing_type_arguments(&self.context, ty, annotation);
        let result_ty = ty
            .default_specialize(db, env)
            .in_type_expression(
                db,
                self.scope(),
                self.typevar_binding_context,
                self.inference_flags(),
            )
            .unwrap_or_else(|error| {
                error.into_fallback_type(&self.context, annotation, self.inference_flags())
            });
        self.check_for_unbound_type_variable(annotation, result_ty)
    }

    /// basedpython: a `ParamSpec`'s two halves are unpacked with stars — `*args: *P` and
    /// `**kwargs: **P` — the same way every other pack is. `P.args` / `P.kwargs` names an
    /// attribute of the type variable, which is not a thing a type expression can mean; it is
    /// the python spelling and stays confined to `.py` files.
    fn report_paramspec_attribute_spelling(
        &self,
        annotation: &ast::Expr,
        typevar: BoundTypeVarInstance<'db>,
        attr: ParamSpecAttrKind,
    ) {
        if !self.is_basedpython_file() {
            return;
        }
        let name = typevar.name(self.db());
        // a keyword-variadic pack has only the keyword half, so there is no positional
        // spelling to point at
        let suggestion = match attr {
            ParamSpecAttrKind::Args if typevar.is_keyword_variadic(self.db()) => None,
            ParamSpecAttrKind::Args => Some(format!("*{name}")),
            ParamSpecAttrKind::Kwargs => Some(format!("**{name}")),
        };
        if let Some(builder) = self.context.report_lint(&INVALID_PARAMSPEC, annotation) {
            let mut diagnostic = builder.into_diagnostic(format_args!(
                "`{name}.{attr}` is the python spelling of a parameter pack's \
                 {attr} and is not valid in a `.by` file",
            ));
            if let Some(suggestion) = suggestion {
                diagnostic
                    .set_primary_annotation_message(format_args!("Did you mean `{suggestion}`?"));
            } else {
                diagnostic.set_primary_annotation_message(format_args!(
                    "Keyword-variadic pack `{name}` has no positional parameters"
                ));
            }
        }
    }

    /// basedpython: if `ty` is `ty_extensions.Top` / `Bottom` and we are inside
    /// a subscript slice, record the materialization on the enclosing subscript
    /// and contribute `Any` / `Never` to the inner type. caller should return
    /// the result instead of going through the standard error path
    fn intercept_nested_top_bottom(&mut self, ty: Type<'db>) -> Option<Type<'db>> {
        if !self.is_basedpython_file()
            || !self
                .inference_flags()
                .contains(InferenceFlags::IN_SUBSCRIPT_SLICE)
        {
            return None;
        }
        let Type::SpecialForm(sf) = ty else {
            return None;
        };
        match sf {
            crate::types::SpecialFormType::Top => {
                self.slice_materialization = Some(crate::types::MaterializationKind::Top);
                Some(Type::any())
            }
            crate::types::SpecialFormType::Bottom => {
                self.slice_materialization = Some(crate::types::MaterializationKind::Bottom);
                Some(Type::Never)
            }
            _ => None,
        }
    }

    /// Infer the type of a type expression without storing the result.
    pub(super) fn infer_type_expression_no_store(&mut self, expression: &ast::Expr) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ignore_runtime_errors = |builder: &Self| {
            builder.deferred_state.is_deferred()
                || builder.in_stub()
                || builder.is_in_type_checking_block(builder.scope(), expression)
                || builder
                    .inference_flags()
                    .contains(InferenceFlags::IN_PEP_613_ALIAS_FIRST_PASS)
        };

        // basedpython: a use-site type modifier (`literal T`, `final T`). The
        // parser encodes it as a marker subscript, so it has to be recognised
        // before the generic subscript arm reads it as a subscription
        if let Some((modifier, inner)) = type_modifier_marker(expression) {
            let inner_ty = self.infer_type_expression(inner);
            return RestrictedType::from_type_expression(self.db(), env, modifier, inner_ty);
        }

        // https://typing.python.org/en/latest/spec/annotations.html#grammar-token-expression-grammar-type_expression
        match expression {
            ast::Expr::Name(name) => match name.ctx {
                ast::ExprContext::Load => {
                    let ty = self.infer_name_expression(name, TypeContext::default());
                    if let Some(materialized) = self.intercept_nested_top_bottom(ty) {
                        return materialized;
                    }
                    self.infer_name_or_attribute_type_expression(ty, expression)
                }
                ast::ExprContext::Invalid => Type::unknown(),
                ast::ExprContext::Store | ast::ExprContext::Del => {
                    todo_type!("Name expression annotation in Store/Del context")
                }
            },

            ast::Expr::Attribute(attribute_expression) => {
                // basedpython: an attribute type over a receiver that is not a plain
                // dotted name — `X[A].x`, and the chains built on it. a dotted name
                // has an established meaning (`mod.Class`, `Outer.Inner`) that the
                // attribute-type reading must not take over, but nothing else here
                // has any meaning at all: this is otherwise the error path below
                if self.is_basedpython_file()
                    && matches!(attribute_expression.ctx, ast::ExprContext::Load)
                    && !is_dotted_name(&attribute_expression.value)
                {
                    return self.infer_attribute_type_expression(attribute_expression);
                }
                if is_dotted_name(expression) {
                    match attribute_expression.ctx {
                        ast::ExprContext::Load => {
                            // basedpython: `float.inf` / `float.nan` in a type
                            // position are the infinity / not-a-number float
                            // literals. infer the receiver once so the builtin
                            // check and the non-`float` fallback share it
                            let ty = if self.is_basedpython_file()
                                && let Some(value) =
                                    basedpython_float_constant(&attribute_expression.attr)
                            {
                                let receiver = self.infer_maybe_standalone_expression(
                                    &attribute_expression.value,
                                    TypeContext::default(),
                                );
                                if let Type::ClassLiteral(class) = receiver
                                    && class.is_known(self.db(), KnownClass::Float)
                                {
                                    return Type::unpromotable_float_literal(value);
                                }
                                // receiver isn't the builtin `float` — finish
                                // the normal attribute load with the type we
                                // already inferred, so it isn't inferred twice
                                self.infer_attribute_load_impl(attribute_expression, receiver)
                                    .unwrap_or_else(|recovery_ty| recovery_ty)
                            } else {
                                self.infer_dotted_type_expression(attribute_expression)
                            };
                            if let Some(materialized) = self.intercept_nested_top_bottom(ty) {
                                return materialized;
                            }
                            self.infer_name_or_attribute_type_expression(ty, expression)
                        }
                        ast::ExprContext::Invalid => Type::unknown(),
                        ast::ExprContext::Store | ast::ExprContext::Del => {
                            todo_type!("Attribute expression annotation in Store/Del context")
                        }
                    }
                } else {
                    if !self.in_string_annotation() {
                        self.infer_attribute_expression(attribute_expression);
                    }
                    self.report_invalid_type_expression(
                        expression,
                        format_args!(
                            "Only simple names, dotted names and subscripts \
                            can be used in {}s",
                            self.type_expression_context()
                        ),
                    );
                    Type::unknown()
                }
            }

            ast::Expr::NoneLiteral(_literal) => Type::none(db, env),

            // https://typing.python.org/en/latest/spec/annotations.html#string-annotations
            ast::Expr::StringLiteral(string) => self.infer_string_type_expression(string),

            ast::Expr::Subscript(subscript) => {
                let ast::ExprSubscript {
                    value,
                    slice,
                    ctx: _,
                    range: _,
                    node_index: _,
                    is_typeof,
                } = subscript;

                // basedpython use-site variance — `Container[out T]` /
                // `Container[in T]` / `Container[in out T]`. The parser
                // encoded each marked element as a `Subscript(Name(<marker>,
                // Invalid), inner)`. The result is the same instance type
                // that `Container[T]` would normally produce, but with a
                // per-typevar projection recorded on the specialization;
                // downstream member access consults the projection to
                // implement kotlin-style read/write restrictions.
                if self.is_basedpython_file()
                    && let Some(slice_elements) = use_site_variance_slice_elements(slice)
                {
                    let value_ty = self.infer_expression(value, TypeContext::default());
                    return resolve_use_site_variance(
                        self.db(),
                        env,
                        value_ty,
                        &slice_elements,
                        |elt| self.infer_type_expression(elt),
                    );
                }

                // basedpython `X[*]` desugars to `Top[X[Any]]`. The slice is
                // the parser-synthesized `Starred(Name(id="", ctx=Invalid))`
                // marker, which can't resolve, so dispatch on the shape
                // before falling through to the regular subscript path
                if self.is_basedpython_file()
                    && let Some(elts) = top_star_slice_elements(slice)
                {
                    let value_ty = self.infer_expression(value, TypeContext::default());
                    let inner_ty = match value_ty {
                        Type::ClassLiteral(class_literal) => {
                            if class_literal.is_known(self.db(), KnownClass::Tuple) {
                                // tuple has variadic typevars — treat any marker
                                // as a homogeneous-Any tuple regardless of mix
                                Type::homogeneous_tuple(self.db(), env, Type::any())
                            } else {
                                let db = self.db();
                                let arg_types: Vec<Type<'db>> = elts
                                    .iter()
                                    .map(|elt| {
                                        if is_top_star_marker(elt) {
                                            Type::any()
                                        } else {
                                            self.infer_type_expression(elt)
                                        }
                                    })
                                    .collect();
                                let class_type =
                                    class_literal.apply_specialization(db, |generic_context| {
                                        let n = generic_context.len(db);
                                        if arg_types.len() == n {
                                            generic_context.specialize(db, arg_types.as_slice())
                                        } else {
                                            // arity mismatch — fall back to
                                            // all-Any so we don't crash; the
                                            // user gets a downstream error from
                                            // normal subscription checking
                                            generic_context
                                                .specialize(db, vec![Type::any(); n].as_slice())
                                        }
                                    });
                                Type::instance(db, env, class_type)
                            }
                        }
                        _ => value_ty,
                    };
                    return inner_ty.top_materialization(self.db(), env);
                }

                // basedpython `typeof X` desugars to `ty_extensions.TypeOf[X]`
                // for type inference. The synthetic value `Name("typeof")`
                // doesn't resolve, so dispatch on the flag directly
                let value_ty = if *is_typeof {
                    Type::SpecialForm(crate::types::SpecialFormType::TypeOf)
                } else {
                    self.infer_expression(value, TypeContext::default())
                };

                // basedpython: top-star marker nested inside the slice (e.g.
                // `list[int | *]`). the marker arm returns `Any`, but the
                // outer subscript still needs the top-materialization wrap
                // that the direct/tuple form gets above
                if self.is_basedpython_file()
                    && !is_top_star_marker(slice)
                    && top_star_slice_elements(slice).is_none()
                    && !top_star_marker_ranges_in_slice(slice).is_empty()
                    && (*is_typeof || is_dotted_name(value))
                {
                    let inner =
                        self.infer_subscript_type_expression_no_store(subscript, slice, value_ty);
                    return inner.top_materialization(self.db(), env);
                }

                if *is_typeof || is_dotted_name(value) {
                    // Preserve the flag for another `Unpack` so that nested unpacking emits a
                    // diagnostic. Other subscripts are no longer the direct unpack operand.
                    let previously_in_unpack_type_argument =
                        if value_ty == Type::SpecialForm(SpecialFormType::Unpack) {
                            None
                        } else {
                            Some(
                                self.context
                                    .inference_flags
                                    .replace(InferenceFlags::IN_UNPACK_TYPE_ARGUMENT, false),
                            )
                        };
                    let ty =
                        self.infer_subscript_type_expression_no_store(subscript, slice, value_ty);
                    if let Some(previously_in_unpack_type_argument) =
                        previously_in_unpack_type_argument
                    {
                        self.context.inference_flags.set(
                            InferenceFlags::IN_UNPACK_TYPE_ARGUMENT,
                            previously_in_unpack_type_argument,
                        );
                    }
                    ty
                } else {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    self.report_invalid_type_expression(
                        expression,
                        format_args!(
                            "Only simple names and dotted names can be subscripted in {}s",
                            self.type_expression_context()
                        ),
                    );
                    Type::unknown()
                }
            }

            ast::Expr::BinOp(binary) => {
                match binary.op {
                    // PEP-604 unions are okay, e.g., `int | str`
                    ast::Operator::BitOr => {
                        let left_ty = self.infer_type_expression(&binary.left);
                        let right_ty = self.infer_type_expression(&binary.right);

                        // Detect runtime errors from e.g. `int | "bytes"` on Python <3.14 without `__future__` annotations.
                        if !ignore_runtime_errors(self) {
                            let mut speculative_builder = self.speculate_without_diagnostics();
                            // If the left-hand side of the union is itself a PEP-604 union,
                            // we'll already have checked whether it can be used with `|` in a previous inference step
                            // and emitted a diagnostic if it was appropriate. We should skip inferring it here to
                            // avoid duplicate diagnostics; just assume that the l.h.s. is a `UnionType` instance
                            // in that case.
                            let left_type_value = speculative_builder
                                .infer_expression(&binary.left, TypeContext::default());
                            let right_type_value = speculative_builder
                                .infer_expression(&binary.right, TypeContext::default());

                            let dunder_fails = Type::try_call_bin_op(
                                db,
                                env,
                                left_type_value,
                                ast::Operator::BitOr,
                                right_type_value,
                            )
                            .is_err();

                            // As well as trying the normal dunder lookup,
                            // we also check for the case where one of the operands is a class-literal type
                            // or generic-alias type and the other is a string literal. The normal dunder lookup
                            // fails to catch this error, since typeshed annotates `type.__(r)or__` as accepting `Any`.
                            let should_emit_error = if dunder_fails {
                                true
                            } else {
                                let literal = match (left_type_value, right_type_value) {
                                    (Type::ClassLiteral(class), Type::LiteralValue(literal))
                                    | (Type::LiteralValue(literal), Type::ClassLiteral(class))
                                        if class.metaclass(db)
                                            == KnownClass::Type.to_class_literal(db, env) =>
                                    {
                                        Some(literal)
                                    }
                                    (Type::GenericAlias(_), Type::LiteralValue(literal))
                                    | (Type::LiteralValue(literal), Type::GenericAlias(_)) => {
                                        Some(literal)
                                    }
                                    _ => None,
                                };
                                literal.is_some_and(|literal| !literal.is_enum())
                            };

                            if should_emit_error
                                && let Some(builder) =
                                    self.context.report_lint(&UNSUPPORTED_OPERATOR, binary)
                            {
                                let mut diagnostic =
                                    builder.into_diagnostic("Unsupported `|` operation");

                                if left_type_value.is_equivalent_to(db, env, right_type_value) {
                                    diagnostic.set_primary_annotation_message(format_args!(
                                        "Both operands have type `{}`",
                                        left_type_value.display(db, env)
                                    ));
                                    diagnostic.set_concise_message(format_args!(
                                        "Operator `|` is unsupported between \
                                        two objects of type `{}`",
                                        left_type_value.display(db, env)
                                    ));
                                } else {
                                    for (operand, ty) in [
                                        (&*binary.left, left_type_value),
                                        (&*binary.right, right_type_value),
                                    ] {
                                        diagnostic.annotate(
                                            self.context.secondary(operand).message(format_args!(
                                                "Has type `{}`",
                                                ty.display(db, env)
                                            )),
                                        );
                                    }
                                    diagnostic.set_concise_message(format_args!(
                                        "Operator `|` is unsupported between \
                                        objects of type `{}` and `{}`",
                                        left_type_value.display(db, env),
                                        right_type_value.display(db, env)
                                    ));
                                }

                                match self.scope.scope(self.db()).kind() {
                                    ScopeKind::TypeAlias => diagnostic.info(
                                        "A type alias scope is lazy but will be \
                                        executed at runtime if the `__value__` property is \
                                        accessed",
                                    ),
                                    ScopeKind::TypeParams => diagnostic.info(
                                        "Type parameter scopes are lazy but may be \
                                        executed at runtime if the `__bound__`, `__value__`
                                        or `__constraints__` property of a type parameter is \
                                        accessed",
                                    ),
                                    _ => {
                                        let python_version =
                                            self.program_environment().python_version(db);

                                        if python_version < PythonVersion::PY314 {
                                            diagnostic.info(format_args!(
                                                "All {}s are evaluated at \
                                                runtime by default on Python <3.14",
                                                self.type_expression_context()
                                            ));
                                            add_inferred_python_version_hint_to_diagnostic(
                                                db,
                                                self.file(),
                                                &mut diagnostic,
                                                "inferring types",
                                            );
                                            if binary.left.is_string_literal_expr()
                                                || binary.right.is_string_literal_expr()
                                            {
                                                diagnostic.help(
                                                    "Put quotes around the whole union \
                                                    rather than just certain elements",
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        UnionType::from_elements_leave_aliases(db, env, [left_ty, right_ty])
                    }
                    // basedpython: `A & B` in a type annotation is an
                    // intersection type. core syntax in `.by` / `.byi`,
                    // experimental in standard python
                    ast::Operator::BitAnd => {
                        if !self.is_basedpython_file()
                            && let Some(builder) =
                                self.context.report_lint(&EXPERIMENTAL_SYNTAX, binary)
                        {
                            builder.into_diagnostic("Intersection type syntax is experimental");
                        }

                        let left_ty = self.infer_type_expression(&binary.left);
                        let right_ty = self.infer_type_expression(&binary.right);

                        // the transpiler lowers `A & B` before runtime, so the runtime
                        // `__and__` probe only applies to standard python files
                        if !self.is_basedpython_file() && !ignore_runtime_errors(self) {
                            // Infer the operands as values to report the types used by the runtime
                            // operation rather than their interpretation as type expressions.
                            let mut speculative_builder = self.speculate_without_diagnostics();
                            let left_value = speculative_builder
                                .infer_expression(&binary.left, TypeContext::default());
                            let right_value = speculative_builder
                                .infer_expression(&binary.right, TypeContext::default());
                            if Type::try_call_bin_op(
                                db,
                                env,
                                left_value,
                                ast::Operator::BitAnd,
                                right_value,
                            )
                            .is_err()
                            {
                                report_unsupported_binary_operation(
                                    &self.context,
                                    binary,
                                    left_value,
                                    right_value,
                                    ast::Operator::BitAnd,
                                );
                            }
                        }

                        IntersectionType::from_two_elements(db, env, left_ty, right_ty)
                    }
                    // anything else is an invalid annotation:
                    op => {
                        // basedpython: a symbolic operation in a type expression,
                        // e.g. `1 + 1`, `A + B`, `1 + typeof d`. evaluate the
                        // operands as types and apply the operator with the same
                        // type-level binary logic ty uses for value expressions, so
                        // `1 + 1` resolves to `Literal[2]`. this reuses literal/type
                        // alias/typevar handling for free via
                        // `infer_binary_expression_type`
                        if self.is_basedpython_file() {
                            let db = self.db();
                            let left_ty = self.infer_type_expression(&binary.left);
                            let right_ty = self.infer_type_expression(&binary.right);
                            // an operation that still mentions a type parameter (e.g.
                            // `Dim + 1`) can't be evaluated until the parameter is
                            // specialized. keep it symbolic so `Array[Dim + 1]`
                            // re-evaluates to `Array[6]` at the call site
                            let operands = [left_ty, right_ty];
                            if DeferredType::is_deferred(db, env, &operands) {
                                return DeferredType::build(
                                    db,
                                    env,
                                    &DeferredOperation::Binary(op),
                                    Box::new(operands),
                                );
                            }
                            if let Some(result) = self.infer_binary_expression_type(
                                binary.into(),
                                false,
                                left_ty,
                                right_ty,
                                op,
                                TypeContext::default(),
                            ) {
                                return result;
                            }
                            // operands are already inferred as type expressions
                            // above; the operator just isn't supported between them
                            self.report_invalid_type_expression(
                                expression,
                                format_args!(
                                    "Invalid binary operator `{}` in type annotation",
                                    op.as_str()
                                ),
                            );
                            return Type::unknown();
                        }
                        // Avoid inferring the types of invalid binary expressions that have been
                        // parsed from a string annotation, as they are not present in the semantic
                        // index.
                        if !self.in_string_annotation() {
                            self.infer_binary_expression(binary, TypeContext::default());
                        }
                        self.report_invalid_type_expression(
                            expression,
                            format_args!(
                                "Invalid binary operator `{}` in type annotation",
                                op.as_str()
                            ),
                        );
                        Type::unknown()
                    }
                }
            }

            // =====================================================================================
            // Forms which are invalid in the context of annotation expressions: we infer their
            // nested expressions as normal expressions, but the type of the top-level expression is
            // always `Type::unknown` in these cases.
            // =====================================================================================
            ast::Expr::BytesLiteral(bytes) => {
                // basedpython: bytes literal in type position is the literal type
                if self.is_basedpython_file()
                    && let Some(single_element) = bytes.as_single_part_bytestring()
                {
                    return Type::bytes_literal(self.db(), &single_element.value);
                }
                if let Some(mut diagnostic) = self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Bytes literals are not allowed in this context in a {}",
                        self.type_expression_context()
                    ),
                ) {
                    if let Some(single_element) = bytes.as_single_part_bytestring()
                        && let Ok(valid_string) = String::from_utf8(single_element.value.to_vec())
                    {
                        diagnostic.set_primary_annotation_message(format_args!(
                            "Did you mean `typing.Literal[b\"{valid_string}\"]`?"
                        ));
                    }
                }
                Type::unknown()
            }

            ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                value: ast::Number::Int(int),
                ..
            }) => {
                // basedpython: int literal in type position is the literal type
                if self.is_basedpython_file() {
                    if let Some(int) = int.as_i64() {
                        return Type::int_literal(int);
                    }
                    return KnownClass::Int.to_instance(self.db(), env);
                }
                if let Some(mut diagnostic) = self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Int literals are not allowed in this context in a {}",
                        self.type_expression_context()
                    ),
                ) {
                    if let Some(int) = int.as_i64() {
                        diagnostic.set_primary_annotation_message(format_args!(
                            "Did you mean `typing.Literal[{int}]`?"
                        ));
                    }
                }

                Type::unknown()
            }

            ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                value: ast::Number::Float(v),
                ..
            }) => {
                // basedpython: float literal in type position is the literal type
                if self.is_basedpython_file() {
                    return Type::float_literal(*v);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Float literals are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                value: ast::Number::Complex { real, imag },
                ..
            }) => {
                // basedpython: complex literal in type position is the literal type
                if self.is_basedpython_file() {
                    return Type::complex_literal(self.db(), *real, *imag);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Complex literals are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::BooleanLiteral(bool_value) => {
                // basedpython: bool literal in type position is the literal type
                if self.is_basedpython_file() {
                    return Type::bool_literal(bool_value.value);
                }
                if let Some(mut diagnostic) = self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Boolean literals are not allowed in this context in a {}",
                        self.type_expression_context()
                    ),
                ) {
                    diagnostic.set_primary_annotation_message(format_args!(
                        "Did you mean `typing.Literal[{}]`?",
                        if bool_value.value { "True" } else { "False" }
                    ));
                }
                Type::unknown()
            }

            ast::Expr::List(list) => {
                if !self.in_string_annotation() {
                    self.infer_list_expression(list, TypeContext::default());
                }

                if let Some(mut diagnostic) = self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "List literals are not allowed in this context in a {}",
                        self.type_expression_context()
                    ),
                ) && let [single_element] = &*list.elts
                {
                    let mut speculative_builder = self.speculate_without_diagnostics();
                    let inner_type = speculative_builder.infer_type_expression(single_element);

                    if inner_type.is_hintable(self.db(), env) {
                        let hinted_type =
                            KnownClass::List.to_specialized_instance(db, env, &[inner_type]);

                        diagnostic.set_primary_annotation_message(format_args!(
                            "Did you mean `{}`?",
                            hinted_type.display(db, env),
                        ));
                    }
                }
                Type::unknown()
            }

            ast::Expr::Tuple(tuple) => {
                // basedpython anonymous named tuple type: `(name: T, name: T, ...)`.
                // Returns an *instance* of the synthesized `NamedTuple` class
                // so attribute access (`a.name`) resolves to the field's
                // declared type. Identity is shape-based — two structurally
                // identical anonymous named tuples in the same file resolve
                // to the same class.
                if tuple.is_anon_named_tuple {
                    let class_lit = self
                        .synthesize_anon_named_tuple_class(tuple, /* is_type_form = */ true);
                    return class_lit
                        .to_instance_approximation(self.db(), env)
                        .unwrap_or(class_lit);
                }

                // basedpython parameter-shape tuple in type position. three
                // sub-cases:
                //   * variadic (`(*args: T)`, `(int, *args: T, str)`,
                //     `(*: T)`): emit a real variable-length tuple type so
                //     assignability matches `tuple[T, ...]` semantics
                //   * fixed with markers (`(int, /, name: T)`): the markers
                //     signal parameter-spec semantics — drop names and emit
                //     a heterogeneous `tuple[...]`. matches the forward
                //     transpile lowering
                //   * fixed without markers but with named fields
                //     (`(int, name: T)`): handled upstream by
                //     `is_anon_named_tuple` dispatch
                if tuple.has_parameter_shape() {
                    if let Some(ty) = self.lower_parameter_shape_to_tuple_type(tuple) {
                        return ty;
                    }
                    let has_markers =
                        tuple.parameter_slash().is_some() || tuple.parameter_star().is_some();
                    if has_markers {
                        let elt_tys: Vec<Type<'db>> = tuple
                            .elts
                            .iter()
                            .map(|e| match e {
                                ast::Expr::Named(n) => self.infer_type_expression(&n.value),
                                _ => self.infer_type_expression(e),
                            })
                            .collect();
                        return Type::heterogeneous_tuple(self.db(), env, elt_tys);
                    }
                    let class_lit = self
                        .synthesize_anon_named_tuple_class(tuple, /* is_type_form = */ true);
                    return class_lit
                        .to_instance_approximation(self.db(), env)
                        .unwrap_or(class_lit);
                }

                // basedpython: parenthesized tuples are valid type
                // expressions equivalent to `tuple[...]`, unpacked elements
                // included — `(int, *A)` splices `A` in exactly as
                // `tuple[int, *A]` does
                if tuple.parenthesized && self.is_basedpython_file() {
                    let spec = self.infer_fixed_tuple_elements(
                        tuple,
                        tuple.range(),
                        /* specialization = */ false,
                    );
                    return Type::tuple(TupleType::new(self.db(), env, &spec));
                }

                if tuple.parenthesized {
                    if !self.in_string_annotation() {
                        for element in tuple {
                            self.infer_expression(element, TypeContext::default());
                        }
                    }

                    if let Some(mut diagnostic) = self.report_invalid_type_expression(
                        expression,
                        format_args!(
                            "Tuple literals are not allowed in this context in a {}",
                            self.type_expression_context()
                        ),
                    ) {
                        let mut speculative = self.speculate_without_diagnostics();
                        let inner_types: Vec<Type<'db>> = tuple
                            .elts
                            .iter()
                            .map(|element| speculative.infer_type_expression(element))
                            .collect();

                        if inner_types.iter().all(|ty| ty.is_hintable(self.db(), env)) {
                            let hinted_type = Type::heterogeneous_tuple(db, env, inner_types);
                            diagnostic.set_primary_annotation_message(format_args!(
                                "Did you mean `{}`?",
                                hinted_type.display(db, env),
                            ));
                        }
                    }
                } else {
                    for element in tuple {
                        self.infer_type_expression(element);
                    }
                }

                Type::unknown()
            }

            ast::Expr::BoolOp(bool_op) => {
                // basedpython: `A or B` / `A and B` in a type annotation are the
                // keyword spellings of union / intersection. allowed only in
                // `.by` / `.byi`
                if self.is_basedpython_file() {
                    let elements: Vec<Type<'db>> = bool_op
                        .values
                        .iter()
                        .map(|value| self.infer_type_expression(value))
                        .collect();
                    return match bool_op.op {
                        ast::BoolOp::Or => {
                            UnionType::from_elements_leave_aliases(self.db(), env, elements)
                        }
                        ast::BoolOp::And => {
                            IntersectionType::from_elements(self.db(), env, elements)
                        }
                    };
                }
                if !self.in_string_annotation() {
                    self.infer_boolean_expression(bool_op, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Boolean operations are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Named(named) => {
                // basedpython: a Named with `Invalid` ctx target is a keyword
                // type-arg binding (`A[T=int]`) or anon-NT field — not a walrus.
                // infer the value as a type expression. Position-by-name is
                // not modelled here; for single-typevar generics the position
                // matches naturally.
                if let ast::Expr::Name(n) = named.target.as_ref()
                    && matches!(n.ctx, ast::ExprContext::Invalid)
                    && self.is_basedpython_file()
                {
                    return self.infer_type_expression(&named.value);
                }
                if !self.in_string_annotation() {
                    self.infer_named_expression(named);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Named expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            // basedpython: `~` in type position stays a numeric operation on
            // literals (`~0` → `Literal[-1]`); the negation type is spelled
            // `not T`. the `~T` negation syntax applies to standard python only
            ast::Expr::UnaryOp(
                unary @ ast::ExprUnaryOp {
                    op: ast::UnaryOp::Invert,
                    operand,
                    ..
                },
            ) if !self.is_basedpython_file() => {
                if let Some(builder) = self.context.report_lint(&EXPERIMENTAL_SYNTAX, unary) {
                    builder.into_diagnostic("Negation type syntax is experimental");
                }

                let operand_ty = self.infer_type_expression(operand);

                if !ignore_runtime_errors(self) {
                    let operand_value = self
                        .speculate_without_diagnostics()
                        .infer_expression(operand, TypeContext::default());
                    if let Err(error) = operand_value.try_call_dunder(
                        db,
                        env,
                        "__invert__",
                        CallArguments::none(),
                        TypeContext::default(),
                    ) {
                        self.report_unsupported_unary_operator(
                            unary,
                            ast::UnaryOp::Invert,
                            operand_value,
                            "__invert__",
                            Some(&error),
                        );
                    }
                }

                operand_ty.negate(db, env)
            }

            ast::Expr::UnaryOp(unary) => {
                // basedpython: `not T` in type position is sugar for
                // `ty_extensions.Not[T]` — the negation type. Compute the
                // inner type and apply the Not special form via inference
                if matches!(unary.op, ast::UnaryOp::Not) && self.is_basedpython_file() {
                    let inner = self.infer_type_expression(&unary.operand);
                    return inner.negate(self.db(), env);
                }
                // basedpython: `-float.inf` is the negative-infinity float
                // literal (`-float.nan` stays nan). the exact `-float.<inf|nan>`
                // shape produces an *unpromotable* float literal, which the
                // generic numeric handling below would not preserve
                if self.is_basedpython_file()
                    && matches!(unary.op, ast::UnaryOp::USub)
                    && let ast::Expr::Attribute(attr) = &*unary.operand
                    && basedpython_float_constant(&attr.attr).is_some()
                    && let ast::Expr::Name(name) = &*attr.value
                    && name.id.as_str() == "float"
                {
                    let inner = self.infer_type_expression(&unary.operand);
                    if let Type::LiteralValue(literal) = inner
                        && let LiteralValueTypeKind::Float(value) = literal.kind()
                    {
                        return Type::unpromotable_float_literal(-value.as_f64());
                    }
                    // shape matched but `float` was shadowed — `inner` is
                    // already inferred and stored, so report without re-running
                    // value inference on the operand
                    self.report_invalid_type_expression(
                        expression,
                        format_args!(
                            "Unary operations are not allowed in {}s",
                            self.type_expression_context()
                        ),
                    );
                    return Type::unknown();
                }
                // basedpython: `T?` in type position is the optional type
                // `T | None`. a nested optional (`T??`) cannot collapse into a
                // union (the outer- and inner-`None` states would merge), so
                // each extra layer wraps the inner type in `WrappedOptional` —
                // and `?` over a *bare type variable* is wrapped too
                // (`WrappedOptional(T | None)`), because specializing a plain
                // `T | None` with an optional `T` would flatten the layer
                // (`f[T](t: T) -> T?` called with `int | None` must yield
                // `int??`, not `int | None`). a generic optional's values are
                // therefore constructed with `Some(…)` / `None`, matching the
                // wrapped runtime convention regardless of what `T` binds to
                if matches!(unary.op, ast::UnaryOp::Optional) && self.is_basedpython_file() {
                    let inner = self.infer_type_expression(&unary.operand);
                    let operand_is_optional = matches!(
                        &*unary.operand,
                        ast::Expr::UnaryOp(operand)
                            if matches!(operand.op, ast::UnaryOp::Optional)
                    );
                    if operand_is_optional {
                        return Type::KnownInstance(KnownInstanceType::WrappedOptional(
                            InternedType::new(self.db(), inner),
                        ));
                    }
                    let decomposition = UnionType::from_elements_leave_aliases(
                        self.db(),
                        env,
                        [inner, Type::none(self.db(), env)],
                    );
                    // `Self` is the one type variable that can never bind to an
                    // optional: it stands for the enclosing class, so `Self?` has
                    // no inner layer to keep apart and is the plain union
                    if matches!(inner, Type::TypeVar(typevar)
                        if !matches!(typevar.kind(self.db()), TypeVarKind::TypingSelf))
                    {
                        return Type::KnownInstance(KnownInstanceType::WrappedOptional(
                            InternedType::new(self.db(), decomposition),
                        ));
                    }
                    return decomposition;
                }
                // basedpython: a unary numeric operation in a type expression,
                // e.g. `-3` → `Literal[-3]` or `~0` → `Literal[-1]`. evaluate the
                // operand as a type and apply the operator with the same unary
                // logic ty uses for value expressions, mirroring the symbolic
                // binary-operation handling above
                if self.is_basedpython_file()
                    && matches!(
                        unary.op,
                        ast::UnaryOp::USub | ast::UnaryOp::UAdd | ast::UnaryOp::Invert
                    )
                {
                    let db = self.db();
                    let operand_ty = self.infer_type_expression(&unary.operand);
                    let operands = [operand_ty];
                    if DeferredType::is_deferred(db, env, &operands) {
                        return DeferredType::build(
                            db,
                            env,
                            &DeferredOperation::Unary(unary.op),
                            Box::new(operands),
                        );
                    }
                    return self.infer_unary_expression_type(unary.op, operand_ty, unary);
                }
                if !self.in_string_annotation() {
                    self.infer_unary_expression(unary);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Unary operations are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Lambda(lambda_expression) => {
                if !self.in_string_annotation() {
                    self.infer_lambda_expression(lambda_expression, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "`lambda` expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::If(if_expression) => {
                if !self.in_string_annotation() {
                    self.infer_if_expression(if_expression, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "`if` expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Dict(dict) => {
                // basedpython: `{"key": T, ...}` in a type position is sugar
                // for a synthesized `TypedDict` subclass. Identity is shape-
                // based so the same shape resolves to the same class.
                if self.is_basedpython_file()
                    && let Some(ty) = self.synthesize_typed_dict_literal(dict)
                {
                    return ty;
                }
                if !self.in_string_annotation() {
                    self.infer_dict_expression(dict, TypeContext::default());
                }
                if let Some(mut diagnostic) = self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Dict literals are not allowed in {}s",
                        self.type_expression_context()
                    ),
                ) && let [
                    ast::DictItem {
                        key: Some(key),
                        value,
                    },
                ] = &*dict.items
                {
                    let mut speculative = self.speculate_without_diagnostics();
                    let key_type = speculative.infer_type_expression(key);
                    let value_type = speculative.infer_type_expression(value);
                    if key_type.is_hintable(self.db(), env)
                        && value_type.is_hintable(self.db(), env)
                    {
                        let hinted_type = KnownClass::Dict.to_specialized_instance(
                            db,
                            env,
                            &[key_type, value_type],
                        );
                        diagnostic.set_primary_annotation_message(format_args!(
                            "Did you mean `{}`?",
                            hinted_type.display(db, env),
                        ));
                    }
                }
                Type::unknown()
            }

            ast::Expr::Set(set) => {
                if !self.in_string_annotation() {
                    self.infer_set_expression(set, TypeContext::default());
                }
                if let Some(mut diagnostic) = self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Set literals are not allowed in {}s",
                        self.type_expression_context()
                    ),
                ) && let [single_element] = &*set.elts
                {
                    let mut speculative_builder = self.speculate_without_diagnostics();
                    let inner_type = speculative_builder.infer_type_expression(single_element);

                    if inner_type.is_hintable(self.db(), env) {
                        let hinted_type =
                            KnownClass::Set.to_specialized_instance(db, env, &[inner_type]);

                        diagnostic.set_primary_annotation_message(format_args!(
                            "Did you mean `{}`?",
                            hinted_type.display(db, env),
                        ));
                    }
                }
                Type::unknown()
            }

            ast::Expr::DictComp(dictcomp) => {
                if !self.in_string_annotation() {
                    self.infer_dict_comprehension_expression(dictcomp, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Dict comprehensions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::ListComp(listcomp) => {
                if !self.in_string_annotation() {
                    self.infer_list_comprehension_expression(listcomp, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "List comprehensions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::SetComp(setcomp) => {
                if !self.in_string_annotation() {
                    self.infer_set_comprehension_expression(setcomp, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Set comprehensions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Generator(generator) => {
                if !self.in_string_annotation() {
                    self.infer_generator_expression(generator, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Generator expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Await(await_expression) => {
                if !self.in_string_annotation() {
                    self.infer_await_expression(await_expression, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "`await` expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Yield(yield_expression) => {
                if !self.in_string_annotation() {
                    self.infer_yield_expression(yield_expression);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "`yield` expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::YieldFrom(yield_from) => {
                if !self.in_string_annotation() {
                    self.infer_yield_from_expression(yield_from);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "`yield from` expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Compare(compare) => {
                // basedpython: `a is T` in return-type position is sugar
                // for `typing.TypeIs[T]`. Single `is` op with a Name lhs
                // and a single comparator. Compute the inner type and
                // wrap it via `KnownInstanceType::TypeIs`-equivalent
                // semantics
                if self.is_basedpython_file()
                    && compare.ops.len() == 1
                    && matches!(compare.ops[0], ast::CmpOp::Is)
                    && is_narrowing_predicate_place(compare.left.as_ref())
                    && let [target] = compare.comparators.as_ref()
                {
                    // skip resolving the lhs place — `a is T` in return-type
                    // position is a labeled annotation, not a reference; the
                    // forward transpile drops the name when lowering to
                    // `TypeIs[T]`. we don't infer the name here, but we
                    // still need to seed the expression cache with a sentinel
                    // type so other passes don't choke
                    let _ = compare.left.as_ref();
                    let narrowed = self.infer_type_expression(target);
                    let expanded = narrowed.expand_eagerly(self.db(), env);
                    if expanded.is_divergent() {
                        return expanded;
                    }
                    return crate::types::TypeIsType::from_type_expression(self.db(), narrowed);
                }
                // basedpython: a rich comparison in a type expression folds like it
                // does on values (`1 < 2` → `Literal[True]`), and is kept symbolic
                // while an operand still mentions a type parameter so `I < 2` can
                // re-fold once `I` is specialized
                if self.is_basedpython_file()
                    && compare.ops.len() == 1
                    && matches!(
                        compare.ops[0],
                        ast::CmpOp::Eq
                            | ast::CmpOp::NotEq
                            | ast::CmpOp::Lt
                            | ast::CmpOp::LtE
                            | ast::CmpOp::Gt
                            | ast::CmpOp::GtE
                    )
                    && let [comparator] = compare.comparators.as_ref()
                {
                    let db = self.db();
                    let left_ty = self.infer_type_expression(&compare.left);
                    let right_ty = self.infer_type_expression(comparator);
                    return DeferredType::build(
                        db,
                        env,
                        &DeferredOperation::Compare(compare.ops[0]),
                        Box::new([left_ty, right_ty]),
                    );
                }
                // in a `.by` file a comparison *is* allowed here — the arm above
                // folds one — so saying they are not allowed at all contradicts
                // the line beside it. name the shape that does fold instead.
                //
                // the operands are inferred as the type expressions they are,
                // rather than through `infer_compare_expression`, which resolves
                // the operator and would pile an `unsupported-operator` about
                // comparing two *types* on top — noise the reader cannot act on
                if self.is_basedpython_file() {
                    self.infer_type_expression(&compare.left);
                    for comparator in &compare.comparators {
                        self.infer_type_expression(comparator);
                    }
                    let what = if compare.ops.len() > 1 {
                        "A chained comparison"
                    } else {
                        "An identity or membership comparison"
                    };
                    self.report_invalid_type_expression(
                        expression,
                        format_args!(
                            "{what} has no symbolic fold, so it is not allowed in {}s; \
                             only a single `==`, `!=`, `<`, `<=`, `>` or `>=` folds",
                            self.type_expression_context()
                        ),
                    );
                    return Type::unknown();
                }
                if !self.in_string_annotation() {
                    self.infer_compare_expression(compare);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Comparison expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Call(call_expr) => {
                // basedpython: a method call in a type expression folds like it does on
                // values (`"ab".startswith("a")` → `Literal[True]`), and is kept symbolic
                // while the receiver still mentions a type parameter, so
                // `S.startswith("foo")` (with `S: str`) re-folds once `S` is specialized.
                // the receiver is inferred as a type expression, so a bare type parameter
                // denotes an instance of its bound (`Type::TypeVar`), the same view a
                // method body has of a value of that type. only plain positional method
                // calls take this path
                if self.is_basedpython_file()
                    && !self.in_string_annotation()
                    && call_expr.arguments.keywords.is_empty()
                    && call_expr
                        .arguments
                        .args
                        .iter()
                        .all(|arg| !arg.is_starred_expr())
                    && let ast::Expr::Attribute(method) = &*call_expr.func
                {
                    let db = self.db();
                    let receiver_ty = self.infer_type_expression(&method.value);
                    let callee_ty = receiver_ty
                        .member(db, env, method.attr.as_str())
                        .ignore_possibly_undefined()
                        .unwrap_or_else(Type::unknown);
                    // record a type for the method-access node so the expression map
                    // stays complete (the receiver and arguments store themselves)
                    self.store_expression_type(&call_expr.func, callee_ty);
                    let mut operands = Vec::with_capacity(call_expr.arguments.args.len() + 1);
                    operands.push(callee_ty);
                    for arg in &call_expr.arguments.args {
                        operands.push(self.infer_type_expression(arg));
                    }
                    return DeferredType::build(
                        db,
                        env,
                        &DeferredOperation::Call,
                        operands.into_boxed_slice(),
                    );
                }
                if !self.in_string_annotation() {
                    self.infer_call_expression(call_expr, TypeContext::default());
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Function calls are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            // basedpython: an f-string in a type position is a template literal
            // type — the set of strings its pattern produces
            ast::Expr::FString(fstring) if self.is_basedpython_file() => {
                self.infer_template_literal_type_expression(fstring)
            }

            ast::Expr::FString(fstring) => {
                if !self.in_string_annotation() {
                    self.infer_fstring_expression(fstring);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "F-strings are not allowed in {}s",
                        self.type_expression_context(),
                    ),
                );
                Type::unknown()
            }

            ast::Expr::TString(tstring) => {
                if !self.in_string_annotation() {
                    self.infer_tstring_expression(tstring);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "T-strings are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Slice(slice) => {
                if !self.in_string_annotation() {
                    self.infer_slice_expression(slice);
                }
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Slices are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Statement(_) => {
                // the wrapped statement is not inferred here: a type expression is
                // not a control-flow position, so the semantic index recorded no
                // definitions for it
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "Statement expressions are not allowed in {}s",
                        self.type_expression_context()
                    ),
                );
                Type::unknown()
            }

            // =================================================================================
            // Branches where we probably should emit diagnostics in some context, but don't yet
            // =================================================================================
            // TODO: When this case is implemented and the `todo!` usage
            // is removed, consider adding `todo = "warn"` to the Clippy
            // lint configuration in `Cargo.toml`. At time of writing,
            // 2025-08-22, this was the only usage of `todo!` in ruff/ty.
            // ---AG
            ast::Expr::IpyEscapeCommand(_) => todo!("Implement Ipy escape command support"),

            ast::Expr::EllipsisLiteral(_) => {
                self.report_invalid_type_expression(
                    expression,
                    format_args!(
                        "`...` is not allowed in this context in a {}",
                        self.type_expression_context(),
                    ),
                );
                Type::unknown()
            }

            ast::Expr::Starred(starred) => self.infer_starred_type_expression(starred),

            // basedpython: `(int, str) -> bool` sugar for `Callable[[int, str], bool]`
            // `(...) -> R` — a single bare ellipsis parameter list is the
            // gradual "any arguments" callable, equivalent to `Callable[..., R]`
            ast::Expr::CallableType(callable) => self.infer_callable_arrow(callable, None),

            // basedpython: `protocol(a: int; def f(self) -> int)` — an inline structural protocol
            ast::Expr::ProtocolType(protocol) => self.synthesize_inline_protocol(protocol),

            // a method member is only meaningful inside `protocol(...)`, which consumes it
            // directly rather than recursing through this dispatch
            ast::Expr::ProtocolMethod(method) => {
                let ty = self.infer_protocol_method_signature(method);
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, method) {
                    builder.into_diagnostic(
                        "A `def` method member is only valid inside an inline `protocol(...)` \
                         type",
                    );
                }
                ty
            }
        }
    }

    /// basedpython: the callable type of a method member of an inline protocol,
    /// `def f(self, x: int) -> str`.
    ///
    /// The receiver is a parameter *name* rather than a type, so the parser marks it as a label
    /// and it is turned into an unannotated positional parameter here — under the parameter-spec
    /// encoding a bare name is a positional-only parameter's *type*, which is what every later
    /// parameter still means.
    pub(super) fn infer_protocol_method_signature(
        &mut self,
        method: &ast::ExprProtocolMethod,
    ) -> Type<'db> {
        let ast::Expr::CallableType(signature) = method.signature.as_ref() else {
            return self.infer_type_expression(&method.signature);
        };
        let receiver = signature.args.first().and_then(|first| match first {
            ast::Expr::Name(name) if name.ctx.is_invalid() => Some(name),
            _ => None,
        });
        self.infer_callable_arrow(signature, receiver)
    }

    /// basedpython: `(int, str) -> bool`, sugar for `Callable[[int, str], bool]`.
    ///
    /// `receiver` is the implicit first parameter of a protocol method member, which is excluded
    /// from `callable.args` interpreted as types. It is `None` for a plain callable arrow.
    fn infer_callable_arrow(
        &mut self,
        callable: &ast::ExprCallableType,
        receiver: Option<&ast::ExprName>,
    ) -> Type<'db> {
        let env = self.program_environment();
        let db = self.db();
        let receiver_offset = usize::from(receiver.is_some());
        let args = &callable.args[receiver_offset..];
        let receiver_parameter =
            || receiver.map(|name| Parameter::positional_or_keyword(name.id.clone()));
        // basedpython: the *implicit* receiver of `int.() -> str` — a type rather than a
        // parameter name, and never spelled on the same callable as a method receiver.
        // Inferred before any early return so its type expression always gets a type
        let implicit_receiver = self.infer_receiver_parameter(callable);

        // `(...) -> R` — a single bare ellipsis parameter list is the gradual "any arguments"
        // callable, equivalent to `Callable[..., R]`. It already accepts the receiver
        if matches!(args, [ast::Expr::EllipsisLiteral(_)]) {
            let return_type = self.infer_type_expression(&callable.returns);
            // an implicit receiver survives the gradual rest as a concatenated prefix
            let parameters = match implicit_receiver {
                Some(implicit_receiver) => {
                    Parameters::concatenate(db, vec![implicit_receiver], ConcatenateTail::Gradual)
                }
                None => Parameters::gradual_form(),
            };
            return Type::single_callable(db, Signature::new(parameters, return_type));
        }

        // basedpython: a trailing bare `**P` unpacks a parameter pack —
        // `(**P) -> R` is `Callable[P, R]` and `(T1, …, **P) -> R` is
        // `Callable[Concatenate[T1, …, P], R]`. the same spelling unpacks a
        // keyword-variadic pack, whose value contributes keyword-only
        // parameters instead of positional ones. `**P` parses to
        // `Starred(Starred(Name))`; an annotated `**kwargs: T` is a
        // `Named` node and is left to the ordinary variadic handling
        if let Some((last, prefix)) = args.split_last()
            && let ast::Expr::Starred(outer) = last
            && let ast::Expr::Starred(inner) = outer.value.as_ref()
            && matches!(inner.value.as_ref(), ast::Expr::Name(_))
        {
            let paramspec_expr = inner.value.as_ref();
            // resolve the bare name; only a parameter pack takes this path (a
            // plain `**kwargs` that isn't one falls through to the ordinary
            // variadic handling below)
            let prev = self
                .context
                .inference_flags
                .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, true);
            let ps_ty = self.infer_type_expression_no_store(paramspec_expr);
            self.context
                .inference_flags
                .set(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, prev);
            if let Type::TypeVar(tv) = ps_ty
                && tv.is_parameter_pack(db)
            {
                // the speculative resolution above deliberately does not store, so the
                // fall-through can infer the same node itself. this branch consumes it
                // instead, so record the type — the transpiler reads it back to pick
                // the matching lowering
                self.store_expression_type(paramspec_expr, ps_ty);
                let return_type = self.infer_type_expression(&callable.returns);
                let parameters =
                    if prefix.is_empty() && receiver.is_none() && implicit_receiver.is_none() {
                        // pure pack `(**P)` — identical to `Callable[P, R]`
                        Parameters::paramspec(db, tv)
                    } else {
                        // `(T1, …, **P)` — `Callable[Concatenate[T1, …, P], R]`
                        let prefix_params: Vec<Parameter<'db>> = receiver_parameter()
                            .into_iter()
                            .chain(implicit_receiver)
                            .chain(prefix.iter().map(|t| {
                                Parameter::positional_only(None)
                                    .with_annotated_type(self.infer_type_expression(t))
                            }))
                            .collect();
                        self.infer_concatenate_tail(paramspec_expr)
                            .map(|tail| Parameters::concatenate(db, prefix_params, tail))
                            .unwrap_or_else(Parameters::unknown)
                    };
                return Type::single_callable(db, Signature::new(parameters, return_type));
            }
        }

        // the marker indices the parser recorded are relative to the full argument list
        let shift = |index: Option<u32>| {
            index.map(|index| (index as usize).saturating_sub(receiver_offset))
        };
        let params: Vec<Parameter<'db>> = receiver_parameter()
            .into_iter()
            .chain(implicit_receiver)
            .chain(self.infer_parameter_spec_elements(
                args,
                shift(callable.parameter_slash()),
                shift(callable.parameter_star()),
                // basedpython: the modifiers are recorded against the written
                // parameters, so a method receiver taken out of `args` shifts
                // them the same way the separators are shifted
                |index| callable.parameter_borrow(index + receiver_offset),
            ))
            .collect();
        let parameters = Parameters::from_annotation(db, env, params);
        let return_type = self.infer_type_expression(&callable.returns);
        let previous = self
            .inference_flags()
            .replace(InferenceFlags::CHECK_UNBOUND_TYPEVARS, false);
        let result = Type::single_callable(db, Signature::new(parameters, return_type));
        self.inference_flags()
            .set(InferenceFlags::CHECK_UNBOUND_TYPEVARS, previous);
        result
    }

    /// basedpython: the leading parameter an implicit receiver contributes — the
    /// `int` of `int.() -> str`. It is positional-only (it is passed by position
    /// when the callable is called directly) and marked as the receiver, which is
    /// what makes `x.fn()` and a trailing lambda's implicit member scope resolve
    /// against it.
    fn infer_receiver_parameter(
        &mut self,
        callable: &ast::ExprCallableType,
    ) -> Option<Parameter<'db>> {
        let receiver = callable.receiver.as_ref()?;
        let receiver_type = self.infer_type_expression(receiver);
        Some(
            Parameter::positional_only(None)
                .with_annotated_type(receiver_type)
                .with_receiver(),
        )
    }

    /// basedpython: builds the parameters of a *parameters spec* — the shared shape behind the
    /// callable arrow `(int, /, name: T, *args: U, **kwargs: V) -> R` and the parameter list a
    /// `ParamSpec` is specialized with, `A[(int, name: T)]`.
    ///
    /// `slash` and `star` are the marker positions the parser recorded on the enclosing node.
    /// Field encodings: a bare type is positional-only (it has no name to be passed by), `name: T`
    /// is `Named(Name, T)`, `*name: T` / `**name: T` are `Named(Starred(..), T)`, and the
    /// anonymous `*: T` / `**: T` are `Starred(T)` / `Starred(Starred(T))`.
    ///
    /// A positional field may instead unpack a variadic type — `(*Ts) -> R` and
    /// `(Unpack[Ts]) -> R` stand for the parameters `Ts` expands to, exactly as
    /// `Callable[[*Ts], R]` does.
    pub(super) fn infer_parameter_spec_elements(
        &mut self,
        elements: &[ast::Expr],
        slash: Option<usize>,
        star: Option<usize>,
        borrow_at: impl Fn(usize) -> ParameterBorrow,
    ) -> Vec<Parameter<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let mut params: Vec<Parameter<'db>> = Vec::with_capacity(elements.len());
        for (index, element) in elements.iter().enumerate() {
            let after_star = star.is_some_and(|star| index >= star);
            let before_slash = slash.is_some_and(|slash| index < slash);
            match element {
                // `*name: T` / `**name: T` — a named variadic
                ast::Expr::Named(named) => {
                    if let ast::Expr::Starred(starred) = named.target.as_ref() {
                        if let ast::Expr::Starred(inner) = starred.value.as_ref() {
                            // `**kwargs: Unpack[TD]` unpacks the `TypedDict`'s keys into
                            // keyword parameters exactly as it does on a `def`; a plain
                            // `**kwargs: T` types each keyword's value instead
                            let (ty, unpacks) =
                                self.infer_kwargs_annotation_type_expression(&named.value);
                            let parameter = Parameter::keyword_variadic(Name::new(
                                inner
                                    .value
                                    .as_name_expr()
                                    .map_or("kwargs", |n| n.id.as_str()),
                            ))
                            .with_annotated_type(ty);
                            params.push(if unpacks {
                                parameter.with_unpacked_kwargs(db, env)
                            } else {
                                parameter
                            });
                            continue;
                        }
                        // `*args: *Ts` / `*args: Unpack[Ts]` — the annotation is the unpacked
                        // type the variadic stands for, not the type of each argument
                        let (ty, unpacks) = self.infer_unpackable_type_expression(&named.value);
                        let parameter = Parameter::variadic(Name::new(
                            starred
                                .value
                                .as_name_expr()
                                .map_or("args", |n| n.id.as_str()),
                        ))
                        .with_annotated_type(ty);
                        params.push(if unpacks {
                            parameter.with_starred_annotation()
                        } else {
                            parameter
                        });
                        continue;
                    }
                    let ty = self.infer_type_expression(&named.value);
                    let name = Name::new(named.target.as_name_expr().map_or("", |n| n.id.as_str()));
                    let parameter = if after_star {
                        Parameter::keyword_only(name)
                    } else if before_slash {
                        Parameter::positional_only(Some(name))
                    } else {
                        Parameter::positional_or_keyword(name)
                    };
                    params.push(parameter.with_annotated_type(ty));
                }
                // `*: T` / `**: T` — an anonymous variadic
                ast::Expr::Starred(starred) => {
                    let (ty, parameter) = match starred.value.as_ref() {
                        ast::Expr::Starred(inner) => {
                            // `(**TD)` / `(**P)` contribute the `TypedDict`'s keys or the
                            // protocol's data members as keyword parameters. a bare name in
                            // the `**` position is always an unpack — it already is for a
                            // parameter pack — so only the labelled `**kwargs: T` above types
                            // each keyword's value
                            let (ty, unpacks) =
                                self.infer_kwargs_annotation_type_expression(&inner.value);
                            let parameter = Parameter::keyword_variadic(Name::new_static("kwargs"))
                                .with_annotated_type(ty);
                            let parameter = if unpacks || inner.value.is_name_expr() {
                                parameter.with_unpacked_kwargs(db, env)
                            } else {
                                parameter
                            };
                            params.push(parameter);
                            continue;
                        }
                        value => {
                            // `*Ts` shares this shape with the anonymous variadic `*: T`, so the
                            // unpacking reading is taken only for a `TypeVarTuple`, which is never
                            // a valid annotation for the individual arguments of a `*args`
                            let ty = self.infer_unpack_operand_type_expression(value);
                            if let Type::TypeVar(typevar) = ty
                                && typevar.is_typevartuple(db)
                            {
                                params.push(
                                    Parameter::variadic(Name::new_static("args"))
                                        .with_annotated_type(ty)
                                        .with_starred_annotation(),
                                );
                                continue;
                            }
                            (ty, Parameter::variadic(Name::new_static("args")))
                        }
                    };
                    params.push(parameter.with_annotated_type(ty));
                }
                // a bare type has no name, so it can only be passed positionally — unless it
                // unpacks a variadic type, which contributes the parameters it expands to
                _ => {
                    let (ty, unpacks) = self.infer_unpackable_type_expression(element);
                    params.push(if unpacks {
                        Parameter::variadic(Name::new_static("args"))
                            .with_annotated_type(ty)
                            .with_starred_annotation()
                    } else {
                        Parameter::positional_only(None).with_annotated_type(ty)
                    });
                }
            }
        }
        // every element contributes exactly one parameter, so the modifiers the
        // parser recorded against the elements apply by position
        debug_assert_eq!(params.len(), elements.len());
        params
            .into_iter()
            .enumerate()
            .map(|(index, parameter)| parameter.with_borrow(borrow_at(index)))
            .collect()
    }

    /// Infers a type expression that is allowed to unpack a variadic type (`*Ts`, `Unpack[Ts]`,
    /// `*tuple[int, str]`), reporting whether it did.
    fn infer_unpackable_type_expression(&mut self, expression: &ast::Expr) -> (Type<'db>, bool) {
        let previously_in_valid_unpack_context = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
        let ty = self.infer_type_expression(expression);
        self.context.inference_flags.set(
            InferenceFlags::IN_VALID_UNPACK_CONTEXT,
            previously_in_valid_unpack_context,
        );
        let unpacks = self
            .type_expression_flags(expression)
            .contains(TypeExpressionFlags::UNPACK)
            && (matches!(ty, Type::TypeVar(typevar) if typevar.is_typevartuple(self.db()))
                || ty.exact_tuple_instance_spec(self.db()).is_some());
        (ty, unpacks)
    }

    /// Infers a `**kwargs` annotation, reporting whether it was written as an `Unpack[...]`.
    /// The flag is what lets `Unpack` appear here at all, and it makes `Unpack[TD]` evaluate to
    /// `TD` itself rather than to an unpacked-tuple form.
    ///
    /// A callable arrow is reached from within an enclosing type expression, but its parameter
    /// annotations are top-level kwargs annotations in their own right — `Unpack` is as welcome
    /// in `(**kwargs: Unpack[TD]) -> R` as it is on the equivalent `def`. So the enclosing
    /// nesting is dropped for the annotation, leaving `Unpack`'s own no-nesting rule to apply
    /// to whatever it contains.
    fn infer_kwargs_annotation_type_expression(
        &mut self,
        expression: &ast::Expr,
    ) -> (Type<'db>, bool) {
        let previously_in_kwarg_annotation = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_KWARG_ANNOTATION, true);
        let previously_in_type_expression = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_TYPE_EXPRESSION, false);
        let previously_in_nested_type_expression = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_NESTED_TYPE_EXPRESSION, false);
        let ty = self.infer_type_expression(expression);
        self.context.inference_flags.set(
            InferenceFlags::IN_NESTED_TYPE_EXPRESSION,
            previously_in_nested_type_expression,
        );
        self.context.inference_flags.set(
            InferenceFlags::IN_TYPE_EXPRESSION,
            previously_in_type_expression,
        );
        self.context.inference_flags.set(
            InferenceFlags::IN_KWARG_ANNOTATION,
            previously_in_kwarg_annotation,
        );
        let unpacks = self
            .type_expression_flags(expression)
            .contains(TypeExpressionFlags::UNPACK);
        (ty, unpacks)
    }

    /// Infers the operand of a `*` unpack without reporting a bare `TypeVarTuple` as invalid —
    /// the enclosing `*` is what makes naming it here legal.
    fn infer_unpack_operand_type_expression(&mut self, expression: &ast::Expr) -> Type<'db> {
        let previously_in_unpack_type_argument = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_UNPACK_TYPE_ARGUMENT, true);
        let ty = self.infer_type_expression(expression);
        self.context.inference_flags.set(
            InferenceFlags::IN_UNPACK_TYPE_ARGUMENT,
            previously_in_unpack_type_argument,
        );
        ty
    }

    fn infer_starred_type_expression(&mut self, starred: &ast::ExprStarred) -> Type<'db> {
        let env = self.program_environment();
        let db = self.db();
        let ast::ExprStarred {
            range: _,
            node_index: _,
            value,
            ctx: _,
        } = starred;

        // basedpython top-star marker `Starred(Name(""))` in nested position
        // (e.g. inside `int | *`) resolves to `Any`. the surrounding subscript
        // detects the marker and top-materializes the result, so contributing
        // `Any` here yields the desired top projection
        if self.is_basedpython_file()
            && let ast::Expr::Name(name) = value.as_ref()
            && name.id.is_empty()
            && matches!(name.ctx, ast::ExprContext::Invalid)
        {
            return Type::any();
        }

        self.store_type_expression_flags(ast::ExprRef::from(starred), TypeExpressionFlags::UNPACK);

        // basedpython: `**kwargs: **Kwargs` unpacks a keyword-variadic pack into the keyword
        // parameters, the way `*args: *Ts` unpacks a `TypeVarTuple` into the positional ones —
        // the star count follows the pack's declaration. the double star parses to
        // `Starred(Starred(_))`; the pack is named bare inside, so it needs the same allowance a
        // bare `ParamSpec` gets.
        //
        // a pack's own bound takes the same double star to bound the whole pack rather than
        // each field — `**Kwargs: **{"a": int}` — and the inner type expression there is the
        // shape the pack must have, not a pack reference
        if self.is_basedpython_file()
            && self
                .context
                .inference_flags
                .intersects(InferenceFlags::IN_KWARG_ANNOTATION | InferenceFlags::IN_PACK_BOUND)
            && let ast::Expr::Starred(inner) = value.as_ref()
        {
            let previously_allowed_paramspec = self
                .context
                .inference_flags
                .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, true);
            let pack_type = self.infer_type_expression(&inner.value);
            self.context.inference_flags.set(
                InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
                previously_allowed_paramspec,
            );
            // a `ParamSpec` carries positional parameters as well, so `**kwargs` takes only
            // its keyword half — the same component python spells `P.kwargs`
            if let Type::TypeVar(typevar) = pack_type
                && typevar.is_paramspec(self.db())
                && self
                    .inference_flags()
                    .contains(InferenceFlags::IN_KWARG_ANNOTATION)
            {
                return Type::TypeVar(
                    typevar.with_paramspec_attr(self.db(), ParamSpecAttrKind::Kwargs),
                );
            }
            return pack_type;
        }

        // basedpython: `*args: *P` unpacks a `ParamSpec`'s positional parameters, the way
        // `**kwargs: **P` unpacks its keyword ones. the pack is named bare, so it needs the
        // same allowance the double-starred form gets — but only for a bare reference at the
        // top of the annotation, so a pack cannot leak into a nested type position
        let allow_bare_pack = self.is_basedpython_file()
            && self
                .inference_flags()
                .contains(InferenceFlags::IN_VARARG_ANNOTATION)
            && !self
                .inference_flags()
                .contains(InferenceFlags::IN_NESTED_TYPE_EXPRESSION)
            && matches!(value.as_ref(), ast::Expr::Name(_) | ast::Expr::Attribute(_));
        let previously_allowed_paramspec = self
            .context
            .inference_flags
            .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, allow_bare_pack);
        let previously_in_unpack_type_argument = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_UNPACK_TYPE_ARGUMENT, true);
        let starred_type = self.infer_type_expression(value);
        self.context.inference_flags.set(
            InferenceFlags::IN_UNPACK_TYPE_ARGUMENT,
            previously_in_unpack_type_argument,
        );
        self.context.inference_flags.set(
            InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
            previously_allowed_paramspec,
        );

        if allow_bare_pack
            && let Type::TypeVar(typevar) = starred_type
            && typevar.is_parameter_pack(self.db())
        {
            if typevar.is_paramspec(self.db()) {
                return Type::TypeVar(
                    typevar.with_paramspec_attr(self.db(), ParamSpecAttrKind::Args),
                );
            }
            // a keyword-variadic pack has no positional half to take
            self.store_type_expression_flags(
                ast::ExprRef::from(starred),
                TypeExpressionFlags::INVALID_UNPACK,
            );
            if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, starred) {
                diagnostic::add_type_expression_reference_link(builder.into_diagnostic(
                    format_args!(
                        "Keyword-variadic pack `{}` has no positional parameters to unpack",
                        typevar.name(self.db())
                    ),
                ));
            }
            return Type::homogeneous_tuple(self.db(), env, Type::unknown());
        }

        if let Some(target) = unpack_target(self.db(), starred_type) {
            target
        } else {
            self.store_type_expression_flags(
                ast::ExprRef::from(starred),
                TypeExpressionFlags::INVALID_UNPACK,
            );
            if !starred_type.is_unknown()
                && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, starred)
            {
                diagnostic::add_type_expression_reference_link(
                    builder.into_diagnostic("`*` can only unpack a tuple type or `TypeVarTuple`"),
                );
            }
            Type::homogeneous_tuple(db, self.program_environment(), Type::unknown())
        }
    }

    pub(super) fn infer_subscript_type_expression_no_store(
        &mut self,
        subscript: &ast::ExprSubscript,
        slice: &ast::Expr,
        value_ty: Type<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        // basedpython: track `ty_extensions.Top` / `Bottom` appearing in nested
        // type-position inside this subscript's slice. the Name/Attribute arms
        // set `slice_materialization` when they encounter one; on exit the
        // materialization is applied to the result of this subscript
        let (prev_slice_flag, prev_slice_materialization) = if self.is_basedpython_file() {
            let prev_flag = self
                .context
                .inference_flags
                .replace(InferenceFlags::IN_SUBSCRIPT_SLICE, true);
            let prev_mat = self.slice_materialization.take();
            (Some(prev_flag), prev_mat)
        } else {
            (None, None)
        };

        let result =
            self.infer_subscript_type_expression_no_store_inner(subscript, slice, value_ty);

        if let Some(prev_flag) = prev_slice_flag {
            let kind = self.slice_materialization.take();
            self.slice_materialization = prev_slice_materialization;
            self.context
                .inference_flags
                .set(InferenceFlags::IN_SUBSCRIPT_SLICE, prev_flag);
            match kind {
                Some(crate::types::MaterializationKind::Top) => {
                    return result.top_materialization(self.db(), env);
                }
                Some(crate::types::MaterializationKind::Bottom) => {
                    return result.bottom_materialization(self.db(), env);
                }
                None => {}
            }
        }
        result
    }

    fn infer_subscript_type_expression_no_store_inner(
        &mut self,
        subscript: &ast::ExprSubscript,
        slice: &ast::Expr,
        value_ty: Type<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        // basedpython use-site variance also fires here — the annotation
        // expression path enters `infer_subscript_type_expression_no_store`
        // directly for `list[in T]` / `Container[out T]` annotations on
        // assignments, dropping into the regular subscript path without ever
        // visiting `infer_type_expression`.
        if self.is_basedpython_file()
            && let Some(slice_elements) = use_site_variance_slice_elements(slice)
        {
            return resolve_use_site_variance(self.db(), env, value_ty, &slice_elements, |elt| {
                self.infer_type_expression(elt)
            });
        }
        // basedpython: `F[bool]` where `F` is a `type def` applies the type
        // function — its body is executed and the result is the type
        if let Type::FunctionLiteral(function) = value_ty
            && function.has_known_decorator(self.db(), FunctionDecorators::TYPE_FN)
        {
            return self.infer_type_fn_application(subscript, slice, function);
        }

        match value_ty {
            Type::ClassLiteral(class_literal) => match class_literal.known(self.db()) {
                Some(KnownClass::Tuple) => Type::tuple(self.infer_tuple_type_expression(subscript)),
                Some(KnownClass::Type) => self.infer_subclass_of_type_expression(slice),
                _ => self.infer_subscript_type_expression(subscript, value_ty),
            },
            _ => self.infer_subscript_type_expression(subscript, value_ty),
        }
    }

    /// basedpython: evaluates an application of a `type def`.
    ///
    /// Proof of concept — the arguments are inferred as ordinary type
    /// expressions and the function's body is executed once per application.
    /// See [`crate::types::type_fn`] for what is deliberately missing.
    fn infer_type_fn_application(
        &mut self,
        subscript: &ast::ExprSubscript,
        slice: &ast::Expr,
        function: FunctionType<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        let db = self.db();
        let arguments: Vec<Type<'db>> = match slice {
            ast::Expr::Tuple(tuple) if !tuple.parenthesized => tuple
                .elts
                .iter()
                .map(|element| self.infer_type_expression(element))
                .collect(),
            single => vec![self.infer_type_expression(single)],
        };

        // arity is checked before bounds (and before execution): passing the wrong
        // number of arguments would otherwise reach the interpreter and come back
        // as a python traceback rather than a diagnostic
        if let Some((expected, actual)) = arity_mismatch(db, function, &arguments)
            && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript)
        {
            builder.into_diagnostic(format_args!(
                "`{}` takes {expected} type argument{}, but {actual} {} given",
                function.name(db),
                if expected == 1 { "" } else { "s" },
                if actual == 1 { "was" } else { "were" },
            ));
            return Type::unknown();
        }

        // a bound is a precondition: it is checked before the function runs, so an
        // impossible argument costs no interpreter. it is also the only check that
        // works on a symbolic argument, so it happens before the deferral below
        if let Some((index, argument, bound)) = first_bound_violation(db, env, function, &arguments)
            && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript)
        {
            builder.into_diagnostic(format_args!(
                "argument {} to `{}` is `{}`, which is not assignable to its bound `{}`",
                index + 1,
                function.name(db),
                argument.display(db, env),
                bound.display(db, env),
            ));
            return Type::unknown();
        }

        // an argument that still mentions a type parameter cannot be evaluated —
        // `F[T]` inside a generic function is only knowable once `T` is
        // substituted. keep the application symbolic; `DeferredType` re-runs it on
        // specialization, and until then it behaves as the declared return type
        if DeferredType::is_deferred(db, env, &arguments) {
            let mut operands = Vec::with_capacity(arguments.len() + 1);
            operands.push(Type::FunctionLiteral(function));
            operands.extend_from_slice(&arguments);
            return DeferredType::build(
                db,
                env,
                &DeferredOperation::TypeFn,
                operands.into_boxed_slice(),
            );
        }

        let interned = TypeFnArguments::new(db, arguments.into_boxed_slice());
        match evaluate_type_fn(db, function, interned) {
            TypeFnOutcome::Type(ty) => *ty,
            TypeFnOutcome::TypeError(message) => {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    builder.into_diagnostic(message.clone());
                }
                declared_return_type(db, function).unwrap_or_else(Type::unknown)
            }
            TypeFnOutcome::Failed(message) => {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    builder.into_diagnostic(format_args!(
                        "`{}` could not be evaluated: {message}",
                        function.name(db)
                    ));
                }
                declared_return_type(db, function).unwrap_or_else(Type::unknown)
            }
        }
    }

    /// Infer the type of a string type expression.
    /// basedpython: an f-string in a type position, read as the set of strings
    /// its pattern produces.
    ///
    /// each interpolation is a type expression of its own — `f"v{int}"` has the
    /// `int` *instance* in its hole, not `type[int]` — and each holds a place
    /// that `str()` fills. a conversion or a format spec would change what fills
    /// it, so both are rejected rather than quietly ignored.
    fn infer_template_literal_type_expression(&mut self, fstring: &ast::ExprFString) -> Type<'db> {
        let mut parts: Vec<TemplatePart<'db>> = Vec::new();
        for part in &fstring.value {
            match part {
                ast::FStringPart::Literal(literal) => {
                    parts.push(TemplatePart::Text(CompactString::new(&literal.value)));
                }
                ast::FStringPart::FString(nested) => {
                    for element in &nested.elements {
                        match element {
                            ast::InterpolatedStringElement::Literal(literal) => {
                                parts.push(TemplatePart::Text(CompactString::new(&literal.value)));
                            }
                            ast::InterpolatedStringElement::Interpolation(interpolation) => {
                                let hole = self.infer_type_expression(&interpolation.expression);
                                if interpolation.debug_text.is_some()
                                    || !interpolation.conversion.is_none()
                                    || interpolation.format_spec.is_some()
                                {
                                    self.report_invalid_type_expression(
                                        interpolation,
                                        format_args!(
                                            "A hole in a template literal type cannot have \
                                            a conversion or a format specifier"
                                        ),
                                    );
                                    parts.push(TemplatePart::Hole(Type::unknown()));
                                } else {
                                    parts.push(TemplatePart::Hole(hole));
                                }
                            }
                        }
                    }
                }
            }
        }
        TemplateLiteralType::from_parts(
            self.db(),
            self.program_environment(),
            parts,
            Promotable::No,
        )
    }

    pub(super) fn infer_string_type_expression(
        &mut self,
        string: &ast::ExprStringLiteral,
    ) -> Type<'db> {
        // basedpython: string in type position is `Literal[<str>]`, not a forward ref
        if self.is_basedpython_file() {
            let value = string.value.to_str();
            return Type::string_literal(self.db(), value);
        }
        match parse_string_annotation(&self.context, self.inference_flags(), string) {
            Some(parsed) => {
                self.string_annotations
                    .insert(ruff_python_ast::ExprRef::StringLiteral(string).into());
                // String annotations are always evaluated in the deferred context.
                let parsed_expr = parsed.expr();
                let string_was_nested = self
                    .inference_flags()
                    .contains(InferenceFlags::IN_NESTED_TYPE_EXPRESSION);
                let previously_in_type_expression = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::IN_TYPE_EXPRESSION, false);
                let previously_in_nested_type_expression = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::IN_NESTED_TYPE_EXPRESSION, string_was_nested);
                let ty = self.infer_type_expression_with_state(
                    parsed_expr,
                    DeferredExpressionState::InStringAnnotation(
                        self.enclosing_node_key(string.into()),
                    ),
                );
                self.context.inference_flags.set(
                    InferenceFlags::IN_NESTED_TYPE_EXPRESSION,
                    previously_in_nested_type_expression,
                );
                self.context.inference_flags.set(
                    InferenceFlags::IN_TYPE_EXPRESSION,
                    previously_in_type_expression,
                );
                let parsed_flags = self.type_expression_flags(parsed_expr);
                if !parsed_flags.is_empty() {
                    self.store_type_expression_flags(
                        ruff_python_ast::ExprRef::StringLiteral(string),
                        parsed_flags,
                    );
                }
                ty
            }
            None => Type::unknown(),
        }
    }

    /// Infer the element types of a fixed-length tuple type, splicing each unpacked
    /// element (`*T` or `Unpack[T]`) into the result rather than nesting it as a
    /// single field.
    ///
    /// `specialization` says the elements came from a `tuple[...]` subscript, whose
    /// `...` element has its own misuse message. A basedpython parenthesized tuple
    /// type passes `false`: there `...` is rejected by the element's own inference,
    /// and the `tuple`-specific wording would be wrong. `report_at` anchors the
    /// diagnostics that apply to both forms.
    fn infer_fixed_tuple_elements<'a>(
        &mut self,
        elements: impl IntoIterator<Item = &'a ast::Expr>,
        report_at: TextRange,
        specialization: bool,
    ) -> TupleSpec<'db> {
        let env = self.program_environment();
        let mut element_types = TupleSpecBuilder::with_capacity(0);
        let mut first_unpacked_variadic_tuple = None;

        for element in elements {
            if specialization && element.is_ellipsis_literal_expr() {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, report_at) {
                    let mut diagnostic = builder.into_diagnostic("Invalid `tuple` specialization");
                    diagnostic.set_primary_annotation_message(
                        "`...` can only be used as the second element \
                                in a two-element `tuple` specialization",
                    );
                }
                self.store_expression_type(element, Type::unknown());
                element_types.push(Type::unknown());
                continue;
            }
            let previously_in_valid_unpack_context = self
                .context
                .inference_flags
                .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
            let element_ty = self.infer_type_expression(element);
            self.context.inference_flags.set(
                InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                previously_in_valid_unpack_context,
            );
            // Determine if this element unpacks a tuple: either `*expr` or `Unpack[expr]`
            let is_unpack = matches!(element, ast::Expr::Starred(_))
                || matches!(
                    element,
                    ast::Expr::Subscript(ast::ExprSubscript { value, .. })
                        if self.expression_type(value)
                            == Type::SpecialForm(SpecialFormType::Unpack)
                );

            if is_unpack {
                let mut report_too_many_unpacked_tuples = || {
                    if let Some(first_unpacked_variadic_tuple) = first_unpacked_variadic_tuple {
                        if let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_FORM, report_at)
                        {
                            let mut diagnostic = builder.into_diagnostic(
                                "Multiple unpacked variadic tuples \
                                            are not allowed in a `tuple` specialization",
                            );
                            diagnostic.annotate(
                                self.context
                                    .secondary(first_unpacked_variadic_tuple)
                                    .message("First unpacked variadic tuple"),
                            );
                            diagnostic.annotate(
                                self.context
                                    .secondary(element)
                                    .message("Later unpacked variadic tuple"),
                            );
                        }
                    } else {
                        first_unpacked_variadic_tuple = Some(element);
                    }
                };

                if let Some(inner_tuple) = element_ty.exact_tuple_instance_spec(self.db()) {
                    element_types = element_types.concat(self.db(), env, &inner_tuple);

                    if inner_tuple.is_variadic() {
                        report_too_many_unpacked_tuples();
                    }
                } else if let Type::TypeVar(typevar) = element_ty
                    && typevar.is_typevartuple(self.db())
                {
                    report_too_many_unpacked_tuples();
                    element_types = element_types.concat_variadic_typevar(self.db(), env, typevar);
                } else {
                    // TODO: emit a diagnostic
                }
            } else {
                element_types.push(element_ty);
            }
        }

        element_types.build()
    }

    /// Return the type represented by a `tuple[]` expression in a type annotation.
    ///
    /// This method assumes that a type has already been inferred and stored for the `value`
    /// of the subscript passed in.
    pub(super) fn infer_tuple_type_expression(
        &mut self,
        tuple: &ast::ExprSubscript,
    ) -> Option<TupleType<'db>> {
        let db = self.db();
        let env = self.program_environment();
        match &*tuple.slice {
            ast::Expr::Tuple(elements) => {
                if let [element, ellipsis @ ast::Expr::EllipsisLiteral(_)] = &*elements.elts {
                    self.infer_expression(ellipsis, TypeContext::default());
                    let previously_in_valid_unpack_context = self
                        .context
                        .inference_flags
                        .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
                    let element_ty = self.infer_type_expression(element);
                    self.context.inference_flags.set(
                        InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                        previously_in_valid_unpack_context,
                    );
                    if self
                        .type_expression_flags(element)
                        .contains(TypeExpressionFlags::UNPACK)
                        && let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, tuple)
                    {
                        let mut diagnostic =
                            builder.into_diagnostic("Invalid `tuple` specialization");
                        diagnostic.set_primary_annotation_message(
                            "`...` cannot be used after an unpacked element",
                        );
                    }
                    let result = TupleType::homogeneous(db, env, element_ty);
                    self.store_expression_type(&tuple.slice, Type::tuple(Some(result)));
                    return Some(result);
                }

                let element_types = self.infer_fixed_tuple_elements(
                    elements,
                    tuple.range(),
                    /* specialization = */ true,
                );

                let ty = TupleType::new(self.db(), env, &element_types);

                // Here, we store the type for the inner `int, str` tuple-expression,
                // while the type for the outer `tuple[int, str]` slice-expression is
                // stored in the surrounding `infer_type_expression` call:
                self.store_expression_type(&tuple.slice, Type::tuple(ty));

                ty
            }
            single_element => {
                if single_element.is_ellipsis_literal_expr() {
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, tuple) {
                        let mut diagnostic =
                            builder.into_diagnostic("Invalid `tuple` specialization");
                        diagnostic.set_primary_annotation_message(
                            "`...` can only be used as the second element \
                                in a two-element `tuple` specialization",
                        );
                    }
                    self.store_expression_type(single_element, Type::unknown());
                    return TupleType::heterogeneous(db, env, std::iter::once(Type::unknown()));
                }
                let previously_in_valid_unpack_context = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
                let single_element_ty = self.infer_type_expression(single_element);
                self.context.inference_flags.set(
                    InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                    previously_in_valid_unpack_context,
                );
                let single_element_is_unpack = matches!(single_element, ast::Expr::Starred(_))
                    || matches!(
                        single_element,
                        ast::Expr::Subscript(ast::ExprSubscript { value, .. })
                            if self.expression_type(value)
                                == Type::SpecialForm(SpecialFormType::Unpack)
                    );
                if single_element_is_unpack {
                    if let Some(inner_tuple) =
                        single_element_ty.exact_tuple_instance_spec(self.db())
                    {
                        return TupleType::new(db, env, &inner_tuple);
                    } else if let Type::TypeVar(typevar) = single_element_ty
                        && typevar.is_typevartuple(self.db())
                    {
                        return TupleType::new(
                            db,
                            env,
                            &TupleSpecBuilder::with_capacity(0)
                                .concat_variadic_typevar(db, env, typevar)
                                .build(),
                        );
                    }
                }
                TupleType::heterogeneous(db, env, std::iter::once(single_element_ty))
            }
        }
    }

    /// Given the slice of a `type[]` annotation, return the type that the annotation represents
    fn infer_subclass_of_type_expression(&mut self, slice: &ast::Expr) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let invalid_type_argument = |builder: &Self, slice: &ast::Expr| {
            builder.report_invalid_type_expression(
                slice,
                "The argument to `type[]` must be a class object type",
            );
            SubclassOfType::subclass_of_unknown()
        };

        let subclass_of_type_argument = |builder: &Self, slice: &ast::Expr, slice_ty: Type<'db>| {
            let slice_ty = slice_ty.resolve_type_alias(db);
            let slice_ty = match slice_ty {
                Type::Union(union) if union.has_aliases(builder.db()) => {
                    union.expand_aliases(db, env)
                }
                _ => slice_ty,
            };
            SubclassOfType::try_from_instance(db, env, slice_ty).unwrap_or_else(|| match slice_ty {
                Type::Callable(_) => invalid_type_argument(builder, slice),
                _ => todo_type!("unsupported type[X] special form"),
            })
        };

        let infer_type_argument = |builder: &mut Self, slice: &ast::Expr| {
            let slice_ty = builder.infer_type_expression(slice);
            subclass_of_type_argument(builder, slice, slice_ty)
        };

        match slice {
            ast::Expr::Name(_) | ast::Expr::Attribute(_) | ast::Expr::StringLiteral(_) => {
                infer_type_argument(self, slice)
            }
            ast::Expr::BinOp(binary) if binary.op == ast::Operator::BitOr => {
                infer_type_argument(self, slice)
            }
            ast::Expr::Tuple(_) => {
                if !self.in_string_annotation() {
                    self.infer_expression(slice, TypeContext::default());
                }
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, slice) {
                    builder.into_diagnostic("type[...] must have exactly one type argument");
                }
                Type::unknown()
            }
            ast::Expr::NoneLiteral(_) => {
                self.infer_expression(slice, TypeContext::default());
                KnownClass::NoneType.to_subclass_of(db, env)
            }
            ast::Expr::Subscript(ast::ExprSubscript { value, .. }) if !is_dotted_name(value) => {
                infer_type_argument(self, slice)
            }
            ast::Expr::Subscript(
                subscript @ ast::ExprSubscript {
                    value,
                    slice: parameters,
                    ..
                },
            ) => {
                let parameters_ty = match self.infer_expression(value, TypeContext::default()) {
                    Type::SpecialForm(SpecialFormType::Union) => match &**parameters {
                        ast::Expr::Tuple(tuple) => {
                            let ty = UnionType::from_elements_leave_aliases(
                                db,
                                env,
                                tuple
                                    .iter()
                                    .map(|element| self.infer_subclass_of_type_expression(element)),
                            );
                            self.store_expression_type(parameters, ty);
                            ty
                        }
                        _ => self.infer_subclass_of_type_expression(parameters),
                    },
                    value_ty @ Type::ClassLiteral(class_literal) => {
                        if class_literal.is_tuple(self.db()) {
                            let class_type = self
                                .infer_tuple_type_expression(subscript)
                                .map(|tuple_type| tuple_type.to_class_type(self.db()))
                                .unwrap_or_else(|| class_literal.default_specialization(db));
                            SubclassOfType::from(db, env, class_type)
                        } else {
                            match class_literal.generic_context(db) {
                                Some(generic_context) => {
                                    let specialize = &|types: &[Option<Type<'db>>]| {
                                        let class = class_literal.apply_specialization(db, |_| {
                                            generic_context
                                                .specialize_partial(db, types.iter().copied())
                                        });
                                        if class_literal.is_protocol(db) {
                                            match Type::instance(db, env, class) {
                                                Type::ProtocolInstance(protocol) => {
                                                    SubclassOfType::from_protocol(protocol)
                                                }
                                                _ => SubclassOfType::from(db, env, class),
                                            }
                                        } else {
                                            SubclassOfType::from(db, env, class)
                                        }
                                    };
                                    self.infer_explicit_callable_specialization(
                                        subscript,
                                        value_ty,
                                        generic_context,
                                        specialize,
                                    )
                                }
                                None => {
                                    self.infer_expression(parameters, TypeContext::default());
                                    if let Some(builder) =
                                        self.context.report_lint(&NOT_SUBSCRIPTABLE, subscript)
                                    {
                                        builder.into_diagnostic(format_args!(
                                            "Cannot subscript non-generic type `{}`",
                                            value_ty.display(db, self.program_environment())
                                        ));
                                    }
                                    Type::unknown()
                                }
                            }
                        }
                    }
                    Type::SpecialForm(
                        special_form @ (SpecialFormType::TypingCallable
                        | SpecialFormType::CollectionsAbcCallable),
                    ) => {
                        self.infer_parameterized_special_form_type_expression(
                            subscript,
                            special_form,
                        );
                        invalid_type_argument(self, slice)
                    }
                    value_ty @ (Type::SpecialForm(
                        SpecialFormType::Top | SpecialFormType::Bottom | SpecialFormType::Annotated,
                    )
                    | Type::KnownInstance(_)
                    | Type::GenericAlias(_)
                    | Type::Callable(_)) => {
                        let slice_ty = self.infer_subscript_type_expression(subscript, value_ty);
                        subclass_of_type_argument(self, slice, slice_ty)
                    }
                    _ => {
                        self.infer_expression(parameters, TypeContext::default());
                        todo_type!("unsupported nested subscript in type[X]")
                    }
                };
                self.store_expression_type(slice, parameters_ty);
                parameters_ty
            }
            _ => {
                self.infer_expression(slice, TypeContext::default());
                todo_type!("unsupported type[X] special form")
            }
        }
    }

    /// Infer the type of an explicitly specialized generic type alias (implicit or PEP 613).
    pub(crate) fn infer_explicit_type_alias_specialization(
        &mut self,
        subscript: &ast::ExprSubscript,
        mut value_ty: Type<'db>,
        in_type_expression: bool,
    ) -> Type<'db> {
        let env = self.program_environment();
        let db = self.db();

        if let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = value_ty
            && let Some(definition) = typevar.definition(db)
        {
            value_ty = value_ty.apply_type_mapping(
                db,
                env,
                &TypeMapping::BindLegacyTypevars(BindingContext::Definition(definition)),
                TypeContext::default(),
            );
        }

        let mut variables = FxOrderSet::default();
        value_ty.find_legacy_typevars(db, env, None, &mut variables);
        let generic_context = GenericContext::from_typevar_instances(db, env, variables);

        let scope_id = self.scope();
        let current_typevar_binding_context = self.typevar_binding_context;
        let current_inference_flags = self.inference_flags();

        // TODO
        // If we explicitly specialize a recursive generic (PEP-613 or implicit) type alias,
        // we currently miscount the number of type variables. For example, for a nested
        // dictionary type alias `NestedDict = dict[K, "V | NestedDict[K, V]"]]`, we might
        // infer `<class 'dict[K, Divergent]'>`, and therefore count just one type variable
        // instead of two. So until we properly support these, specialize all remaining type
        // variables with a `@Todo` type (since we don't know which of the type arguments
        // belongs to the remaining type variables).
        if any_over_type(db, env, value_ty, true, |ty| ty.is_divergent()) {
            let value_ty = value_ty.apply_specialization(
                db,
                generic_context.specialize(
                    db,
                    std::iter::repeat_n(
                        todo_type!("specialized recursive generic type alias"),
                        generic_context.len(db),
                    )
                    .collect::<Vec<_>>(),
                ),
            );
            return if in_type_expression {
                value_ty
                    .in_type_expression(
                        db,
                        scope_id,
                        current_typevar_binding_context,
                        current_inference_flags,
                    )
                    .unwrap_or_else(|_| Type::unknown())
            } else {
                value_ty
            };
        }

        let specialize = &|types: &[Option<Type<'db>>]| {
            let specialized = value_ty.apply_specialization(
                db,
                generic_context.specialize_partial(db, types.iter().copied()),
            );

            if in_type_expression {
                specialized
                    .in_type_expression(
                        db,
                        scope_id,
                        current_typevar_binding_context,
                        current_inference_flags,
                    )
                    .unwrap_or_else(|_| Type::unknown())
            } else {
                specialized
            }
        };

        self.infer_explicit_callable_specialization(
            subscript,
            value_ty,
            generic_context,
            specialize,
        )
    }

    fn infer_subscript_type_expression(
        &mut self,
        subscript: &ast::ExprSubscript,
        value_ty: Type<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprSubscript {
            range: _,
            node_index: _,
            value: _,
            slice,
            ctx: _,
            is_typeof: _,
        } = subscript;

        // basedpython use-site variance: catch here too — this is the
        // central subscript type-expression resolver, reached from both the
        // annotation-expression and type-expression entry points. Without
        // this check, variable-annotation forms like `x: list[out int]`
        // would fall through to ordinary subscript inference and lose the
        // projection.
        if self.is_basedpython_file()
            && let Some(slice_elements) = use_site_variance_slice_elements(slice)
        {
            return resolve_use_site_variance(self.db(), env, value_ty, &slice_elements, |elt| {
                self.infer_type_expression(elt)
            });
        }

        match value_ty {
            Type::Never => {
                // This case can be entered when we use a type annotation like `Literal[1]`
                // in unreachable code, since we infer `Never` for `Literal`.  We call
                // `infer_expression` (instead of `infer_type_expression`) here to avoid
                // false-positive `invalid-type-form` diagnostics (`1` is not a valid type
                // expression).
                if !self.in_string_annotation() {
                    self.infer_expression(slice, TypeContext::default());
                }
                Type::unknown()
            }
            Type::SpecialForm(special_form) => {
                self.infer_parameterized_special_form_type_expression(subscript, special_form)
            }
            Type::KnownInstance(known_instance) => match known_instance {
                KnownInstanceType::SubscriptedProtocol(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`typing.Protocol` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::SubscriptedGeneric(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`typing.Generic` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::Deprecated(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`warnings.deprecated` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::Field(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`dataclasses.Field` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::ConstraintSet(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`ty_extensions._internal.ConstraintSet` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::ConstraintSetSolution(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`ty_extensions._internal.ConstraintSetSolution` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::GenericContext(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`ty_extensions._internal.GenericContext` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::Specialization(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`ty_extensions._internal.Specialization` is not allowed in {}s",
                            self.type_expression_context(),
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::TypeAliasType(type_alias) => {
                    match type_alias.generic_context(self.db()) {
                        Some(generic_context) => {
                            let specialized_type_alias = self
                                .infer_explicit_type_alias_type_specialization(
                                    subscript,
                                    value_ty,
                                    type_alias,
                                    generic_context,
                                );

                            specialized_type_alias
                                .in_type_expression(
                                    db,
                                    self.scope(),
                                    self.typevar_binding_context,
                                    self.inference_flags(),
                                )
                                .unwrap_or(Type::unknown())
                        }
                        None => {
                            if !self.in_string_annotation() {
                                self.infer_expression(slice, TypeContext::default());
                            }
                            if let Some(builder) =
                                self.context.report_lint(&NOT_SUBSCRIPTABLE, subscript)
                            {
                                let mut diagnostic = builder.into_diagnostic(format_args!(
                                    "Cannot specialize non-generic type alias `{}`",
                                    type_alias.name(self.db())
                                ));
                                let secondary = self.context.secondary(&*subscript.value);
                                let value_type = type_alias.raw_value_type(self.db());
                                if value_type.is_specialized_generic(self.db()) {
                                    diagnostic.annotate(secondary.message(format_args!(
                                        "Alias to `{}`, which is already specialized",
                                        value_type.display(db, env)
                                    )));
                                } else {
                                    diagnostic.annotate(secondary.message(format_args!(
                                        "Alias to `{}`, which is not generic",
                                        value_type.display(db, env)
                                    )));
                                }
                            }

                            Type::unknown()
                        }
                    }
                }
                KnownInstanceType::Literal(ty) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`{ty}` is not a generic class",
                            ty = ty.inner(self.db()).display(db, env)
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::WrappedOptional(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "a wrapped optional cannot be specialized",
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::TypeVar(typevar) => {
                    // The type variable designated as a generic type alias by `typing.TypeAlias` can be explicitly specialized.
                    // ```py
                    // from typing import TypeVar, TypeAlias
                    // T = TypeVar('T')
                    // Annotated: TypeAlias = T
                    // _: Annotated[int] = 1  # valid
                    // ```
                    if typevar.identity(self.db()).kind(self.db()) == TypeVarKind::Pep613Alias {
                        self.infer_explicit_type_alias_specialization(subscript, value_ty, false)
                    } else {
                        if !self.in_string_annotation() {
                            self.infer_expression(slice, TypeContext::default());
                        }
                        if let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_FORM, subscript)
                        {
                            builder.into_diagnostic(format_args!(
                                "A type variable itself cannot be specialized",
                            ));
                        }
                        Type::unknown()
                    }
                }
                KnownInstanceType::LiteralStringAlias(_)
                | KnownInstanceType::UnionType(_)
                | KnownInstanceType::Callable(_)
                | KnownInstanceType::Annotated(_)
                | KnownInstanceType::TypeGenericAlias(_) => {
                    self.infer_explicit_type_alias_specialization(subscript, value_ty, true)
                }
                KnownInstanceType::NewType(newtype) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(&subscript.slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`{}` is a `NewType` and cannot be specialized",
                            newtype.name(self.db())
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::Sentinel(sentinel) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(&subscript.slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`{}` is a sentinel and cannot be specialized",
                            sentinel.name(self.db())
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::NamedTupleSpec(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(&subscript.slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`NamedTuple` specs cannot be specialized",
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::FunctoolsPartial(_)
                | KnownInstanceType::FunctoolsPartialCall(_) => {
                    self.infer_type_expression(&subscript.slice);
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`functools.partial` instances cannot be specialized",
                        ));
                    }
                    Type::unknown()
                }
                KnownInstanceType::Range { .. } => {
                    if !self.in_string_annotation() {
                        self.infer_expression(&subscript.slice, TypeContext::default());
                    }
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        builder.into_diagnostic(format_args!(
                            "`range` instances cannot be specialized"
                        ));
                    }
                    Type::unknown()
                }
            },
            Type::Dynamic(DynamicType::UnknownGeneric(_)) => {
                self.infer_explicit_type_alias_specialization(subscript, value_ty, true)
            }
            Type::Dynamic(_) | Type::Divergent(_) => {
                // Infer slice as a value expression to avoid false-positive
                // `invalid-type-form` diagnostics, when we have e.g.
                // `MyCallable[[int, str], None]` but `MyCallable` is dynamic.
                if !self.in_string_annotation() {
                    self.infer_expression(slice, TypeContext::default());
                }
                value_ty
            }
            Type::ClassLiteral(class) => {
                match (class.generic_context(self.db()), class.as_static()) {
                    (Some(generic_context), Some(static_class)) => {
                        let specialized_class = self.infer_explicit_class_specialization(
                            subscript,
                            value_ty,
                            static_class,
                            generic_context,
                        );

                        specialized_class
                            .in_type_expression(
                                db,
                                self.scope(),
                                self.typevar_binding_context,
                                self.inference_flags(),
                            )
                            .unwrap_or(Type::unknown())
                    }
                    _ => {
                        self.infer_expression(slice, TypeContext::default());
                        if let Some(builder) =
                            self.context.report_lint(&NOT_SUBSCRIPTABLE, subscript)
                        {
                            builder.into_diagnostic(format_args!(
                                "Cannot subscript non-generic type `{}`",
                                value_ty.display(db, self.program_environment())
                            ));
                        }
                        Type::unknown()
                    }
                }
            }
            Type::GenericAlias(_) => {
                self.infer_explicit_type_alias_specialization(subscript, value_ty, true)
            }
            Type::LiteralValue(literal) if literal.is_string() => {
                self.infer_expression(slice, TypeContext::default());
                // For stringified TypeAlias; remove once properly supported
                todo_type!("string literal subscripted in type expression")
            }
            Type::Union(union) => {
                let db = self.db();
                let mut union_builder =
                    UnionBuilder::new(db, env).recursively_defined(union.recursively_defined(db));

                for (index, element) in union.elements(db).iter().enumerate() {
                    let mut speculative_builder = self.speculate();
                    let subscript_ty =
                        speculative_builder.infer_subscript_type_expression(subscript, *element);
                    if index == 0 {
                        self.extend(speculative_builder);
                    } else {
                        self.context.extend(&speculative_builder.context.finish());
                    }
                    union_builder = union_builder.add(subscript_ty);
                }

                union_builder.build()
            }
            _ => {
                if !self.in_string_annotation() {
                    self.infer_expression(slice, TypeContext::default());
                }
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    builder.into_diagnostic(format_args!(
                        "Invalid subscript of object of type `{}` in a {}",
                        value_ty.display(db, env),
                        self.type_expression_context()
                    ));
                }
                Type::unknown()
            }
        }
    }

    fn infer_parameterized_legacy_typing_alias(
        &mut self,
        subscript_node: &ast::ExprSubscript,
        alias: LegacyStdlibAlias,
    ) -> Type<'db> {
        let db = self.db();
        let arguments = &*subscript_node.slice;
        let args = if let ast::Expr::Tuple(t) = arguments
            && !t.is_anon_named_tuple
        {
            &*t.elts
        } else {
            std::slice::from_ref(arguments)
        };

        let AliasSpec {
            class,
            expected_argument_number,
        } = alias.alias_spec();

        if args.len() != expected_argument_number {
            if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript_node) {
                let noun = if expected_argument_number == 1 {
                    "argument"
                } else {
                    "arguments"
                };
                builder.into_diagnostic(format_args!(
                    "Legacy alias `{alias}` expected exactly {expected_argument_number} {noun}, \
                    got {}",
                    args.len()
                ));
            }
        }
        let ty = class.to_specialized_instance(
            db,
            self.program_environment(),
            args.iter()
                .map(|node| self.infer_type_expression(node))
                .collect::<Vec<_>>(),
        );
        if arguments.is_tuple_expr() {
            self.store_expression_type(arguments, ty);
        }
        ty
    }

    /// Infer the type of a `Callable[...]` type expression.
    pub(crate) fn infer_callable_type(&mut self, subscript: &ast::ExprSubscript) -> Type<'db> {
        fn inner<'db>(
            builder: &mut TypeInferenceBuilder<'db, '_>,
            subscript: &ast::ExprSubscript,
        ) -> Type<'db> {
            let db = builder.db();

            let arguments_slice = &*subscript.slice;

            let mut arguments = match arguments_slice {
                ast::Expr::Tuple(tuple) => Either::Left(tuple.iter()),
                _ => {
                    builder.infer_callable_parameter_types(arguments_slice);
                    Either::Right(std::iter::empty::<&ast::Expr>())
                }
            };

            let first_argument = arguments.next();

            let previously_allowed_concatenate = builder
                .context
                .inference_flags
                .replace(InferenceFlags::IN_VALID_CONCATENATE_CONTEXT, true);
            let parameters =
                first_argument.and_then(|arg| builder.infer_callable_parameter_types(arg));
            builder.context.inference_flags.set(
                InferenceFlags::IN_VALID_CONCATENATE_CONTEXT,
                previously_allowed_concatenate,
            );

            let return_type = arguments
                .next()
                .map(|arg| builder.infer_type_expression(arg));

            let callable_type = if parameters.is_none()
                && let Some(first_argument) = first_argument
                && let ast::Expr::List(list) = first_argument
                && let [single_param] = &list.elts[..]
                && single_param.is_ellipsis_literal_expr()
            {
                builder.store_expression_type(single_param, Type::unknown());
                if let Some(mut diagnostic) = builder.report_invalid_type_expression(
                    first_argument,
                    "`[...]` is not a valid parameter list for `Callable`",
                ) {
                    if let Some(returns) = return_type {
                        diagnostic.set_primary_annotation_message(format_args!(
                            "Did you mean `Callable[..., {}]`?",
                            returns.display(db, builder.program_environment())
                        ));
                    }
                }
                Type::single_callable(
                    db,
                    Signature::new(
                        Parameters::unknown(),
                        return_type.unwrap_or_else(Type::unknown),
                    ),
                )
            } else {
                let correct_argument_number = if let Some(third_argument) = arguments.next() {
                    builder.infer_type_expression(third_argument);
                    for argument in arguments {
                        builder.infer_type_expression(argument);
                    }
                    false
                } else {
                    return_type.is_some()
                };

                if !correct_argument_number {
                    report_invalid_arguments_to_callable(&builder.context, subscript);
                }

                if correct_argument_number
                    && let (Some(parameters), Some(return_type)) = (parameters, return_type)
                {
                    Type::single_callable(db, Signature::new(parameters, return_type))
                } else {
                    Type::Callable(CallableType::unknown(db))
                }
            };

            // `Signature` / `Parameters` are not a `Type` variant, so we're storing
            // the outer callable type on these expressions instead.
            builder.store_expression_type(arguments_slice, callable_type);
            if let Some(first_argument) = first_argument {
                builder.store_expression_type(first_argument, callable_type);
            }

            callable_type
        }

        // There is disagreement among type checkers about whether `Callable` annotations
        // in the global scope or similar should be considered to create an implicit generic context.
        // For now, we do not report unbound type variables in any `Callable` contexts, but we may
        // decide to revisit this in the future.
        let previous_check_unbound_typevars = self
            .context
            .inference_flags
            .replace(InferenceFlags::CHECK_UNBOUND_TYPEVARS, false);
        let result = inner(self, subscript);
        self.context.inference_flags.set(
            InferenceFlags::CHECK_UNBOUND_TYPEVARS,
            previous_check_unbound_typevars,
        );
        result
    }

    fn infer_parameterized_special_form_type_expression(
        &mut self,
        subscript: &ast::ExprSubscript,
        special_form: SpecialFormType,
    ) -> Type<'db> {
        let env = self.program_environment();
        let db = self.db();
        let arguments_slice = &*subscript.slice;
        match special_form {
            SpecialFormType::Annotated => self
                .parse_subscription_of_annotated_special_form(
                    subscript,
                    AnnotatedExprContext::TypeExpression,
                )
                .inner_type()
                .in_type_expression(db, self.scope(), None, self.inference_flags())
                .unwrap_or_else(|err| {
                    err.into_fallback_type(&self.context, subscript, self.inference_flags())
                }),
            SpecialFormType::Literal => match self.infer_literal_parameter_type(arguments_slice) {
                Ok(ty) => ty,
                Err(nodes) => {
                    for node in nodes {
                        let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, node)
                        else {
                            continue;
                        };
                        builder.into_diagnostic(
                            "Type arguments for `Literal` must be `None`, \
                            a literal value (int, bool, str, or bytes), or an enum member",
                        );
                    }
                    Type::unknown()
                }
            },
            SpecialFormType::Optional => {
                let param_type = self.infer_type_expression(arguments_slice);
                UnionType::from_elements_leave_aliases(db, env, [param_type, Type::none(db, env)])
            }
            SpecialFormType::Union => {
                // TODO: Support the union of a `TypeVarTuple`'s elements. Until then, reject
                // `Union[*Ts]` and recover to `object` rather than treating `Ts` as one member.
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let mut has_unpacked_typevartuple = false;
                let union_ty = UnionType::from_elements_leave_aliases(
                    db,
                    env,
                    arguments.iter().map(|argument| {
                        let ty = self.infer_type_expression(argument);
                        if self
                            .type_expression_flags(argument)
                            .contains(TypeExpressionFlags::UNPACK)
                        {
                            let is_typevartuple =
                                matches!(
                                    ty,
                                    Type::TypeVar(typevar) if typevar.is_typevartuple(db)
                                ) || if let ast::Expr::Subscript(subscript) = argument {
                                    matches!(
                                        self.expression_type(&subscript.slice),
                                        Type::TypeVar(typevar) if typevar.is_typevartuple(db)
                                    )
                                } else {
                                    false
                                };

                            if is_typevartuple {
                                has_unpacked_typevartuple = true;
                                if !ty.is_unknown()
                                    && let Some(builder) =
                                        self.context.report_lint(&INVALID_TYPE_FORM, argument)
                                {
                                    diagnostic::add_type_expression_reference_link(
                                        builder.into_diagnostic(
                                            "Unpacking a `TypeVarTuple` in `Union` \
                                            is not supported",
                                        ),
                                    );
                                }
                            }
                        }
                        ty
                    }),
                );
                let ty = if has_unpacked_typevartuple {
                    Type::object()
                } else {
                    union_ty
                };
                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, ty);
                }
                ty
            }
            SpecialFormType::TypingCallable | SpecialFormType::CollectionsAbcCallable => {
                self.infer_callable_type(subscript)
            }

            // `ty_extensions` special forms
            SpecialFormType::Not => {
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
                    && !tuple.is_anon_named_tuple
                {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let num_arguments = arguments.len();
                let negated_type = if num_arguments == 1 {
                    self.infer_type_expression(&arguments[0]).negate(db, env)
                } else {
                    if !self.in_string_annotation() {
                        for argument in arguments {
                            self.infer_expression(argument, TypeContext::default());
                        }
                    }
                    report_invalid_argument_number_to_special_form(
                        &self.context,
                        subscript,
                        special_form,
                        num_arguments,
                        1,
                    );
                    Type::unknown()
                };
                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, negated_type);
                }
                negated_type
            }
            SpecialFormType::Intersection => {
                let elements = match arguments_slice {
                    ast::Expr::Tuple(tuple) => Either::Left(tuple.iter()),
                    element => Either::Right(std::iter::once(element)),
                };

                let ty = elements
                    .fold(IntersectionBuilder::new(db, env), |builder, element| {
                        builder.add_positive(self.infer_type_expression(element))
                    })
                    .build();

                if matches!(arguments_slice, ast::Expr::Tuple(_)) {
                    self.store_expression_type(arguments_slice, ty);
                }
                ty
            }
            SpecialFormType::UnsafeUnion => {
                let elements = match arguments_slice {
                    ast::Expr::Tuple(tuple) => Either::Left(tuple.iter()),
                    element => Either::Right(std::iter::once(element)),
                };

                let ty = UnsafeUnionType::from_elements(
                    db,
                    env,
                    elements
                        .map(|element| self.infer_type_expression(element))
                        .collect::<Vec<_>>(),
                );

                if matches!(arguments_slice, ast::Expr::Tuple(_)) {
                    self.store_expression_type(arguments_slice, ty);
                }
                ty
            }
            SpecialFormType::Overlapping => {
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
                    && !tuple.is_anon_named_tuple
                {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let num_arguments = arguments.len();
                let type_argument = if num_arguments == 1 {
                    self.infer_type_expression(&arguments[0])
                } else {
                    if !self.in_string_annotation() {
                        for argument in arguments {
                            self.infer_expression(argument, TypeContext::default());
                        }
                    }
                    report_invalid_argument_number_to_special_form(
                        &self.context,
                        subscript,
                        special_form,
                        num_arguments,
                        1,
                    );
                    Type::unknown()
                };
                let overlapping = OverlappingType::from_type_expression(db, type_argument);
                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, overlapping);
                }
                overlapping
            }
            SpecialFormType::Top => {
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
                    && !tuple.is_anon_named_tuple
                {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let num_arguments = arguments.len();
                let arg = if num_arguments == 1 {
                    self.infer_type_expression(&arguments[0])
                } else {
                    if !self.in_string_annotation() {
                        for argument in arguments {
                            self.infer_expression(argument, TypeContext::default());
                        }
                    }
                    report_invalid_argument_number_to_special_form(
                        &self.context,
                        subscript,
                        special_form,
                        num_arguments,
                        1,
                    );
                    Type::unknown()
                };
                arg.top_materialization(db, env)
            }
            SpecialFormType::Bottom => {
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
                    && !tuple.is_anon_named_tuple
                {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let num_arguments = arguments.len();
                let arg = if num_arguments == 1 {
                    self.infer_type_expression(&arguments[0])
                } else {
                    if !self.in_string_annotation() {
                        for argument in arguments {
                            self.infer_expression(argument, TypeContext::default());
                        }
                    }
                    report_invalid_argument_number_to_special_form(
                        &self.context,
                        subscript,
                        special_form,
                        num_arguments,
                        1,
                    );
                    Type::unknown()
                };
                arg.bottom_materialization(db, env)
            }
            SpecialFormType::TypeOf => {
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
                    && !tuple.is_anon_named_tuple
                {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let num_arguments = arguments.len();
                let type_of_type = if num_arguments == 1 {
                    // N.B. This uses `infer_expression` rather than `infer_type_expression`
                    self.infer_expression(&arguments[0], TypeContext::default())
                } else {
                    if !self.in_string_annotation() {
                        for argument in arguments {
                            self.infer_expression(argument, TypeContext::default());
                        }
                    }
                    report_invalid_argument_number_to_special_form(
                        &self.context,
                        subscript,
                        special_form,
                        num_arguments,
                        1,
                    );
                    Type::unknown()
                };
                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, type_of_type);
                }
                type_of_type
            }
            SpecialFormType::TypeForm => {
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let type_argument = if let [argument] = arguments {
                    self.infer_type_expression(argument)
                } else {
                    let num_arguments = arguments.len();

                    if !self.in_string_annotation() {
                        for argument in arguments {
                            self.infer_expression(argument, TypeContext::default());
                        }
                    }
                    report_invalid_argument_number_to_special_form(
                        &self.context,
                        subscript,
                        special_form,
                        num_arguments,
                        1,
                    );

                    Type::unknown()
                };
                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, type_argument);
                }
                TypeFormType::from_type_expression(db, type_argument)
            }

            SpecialFormType::CallableTypeOf | SpecialFormType::RegularCallableTypeOf => {
                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
                    && !tuple.is_anon_named_tuple
                {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };
                let num_arguments = arguments.len();

                if num_arguments != 1 {
                    if !self.in_string_annotation() {
                        for argument in arguments {
                            self.infer_expression(argument, TypeContext::default());
                        }
                    }
                    report_invalid_argument_number_to_special_form(
                        &self.context,
                        subscript,
                        special_form,
                        num_arguments,
                        1,
                    );
                    if arguments_slice.is_tuple_expr() {
                        self.store_expression_type(arguments_slice, Type::unknown());
                    }
                    return Type::unknown();
                }

                let argument_type = self.infer_expression(&arguments[0], TypeContext::default());
                let Some(callable_type) = argument_type
                    .try_upcast_to_callable_with_recursive_fallback(
                        db,
                        env,
                        self.recursive_type_expression_definition(),
                    )
                    .map(|callables| {
                        if special_form == SpecialFormType::RegularCallableTypeOf {
                            callables
                                .map(|callable| callable.into_regular(db))
                                .into_type(db, env)
                        } else {
                            callables.into_type(db, env)
                        }
                    })
                else {
                    if let Some(builder) = self
                        .context
                        .report_lint(&INVALID_TYPE_FORM, arguments_slice)
                    {
                        builder.into_diagnostic(format_args!(
                            "Expected the first argument to `{special_form}` \
                                 to be a callable object, \
                                 but got an object of type `{actual_type}`",
                            actual_type = argument_type.display(db, env)
                        ));
                    }
                    if arguments_slice.is_tuple_expr() {
                        self.store_expression_type(arguments_slice, Type::unknown());
                    }
                    return Type::unknown();
                };

                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, callable_type);
                }
                callable_type
            }
            SpecialFormType::LegacyStdlibAlias(alias) => {
                self.infer_parameterized_legacy_typing_alias(subscript, alias)
            }
            SpecialFormType::TypeQualifier(qualifier) => {
                if self.inference_flags().intersects(
                    InferenceFlags::IN_PARAMETER_ANNOTATION
                        | InferenceFlags::IN_RETURN_TYPE
                        | InferenceFlags::IN_TYPE_ALIAS,
                ) {
                    self.report_invalid_type_expression(
                        subscript,
                        format_args!(
                            "Type qualifier `{qualifier}` is not allowed in {}s",
                            self.inference_flags().type_expression_context(),
                        ),
                    );
                } else {
                    self.report_invalid_type_expression(
                        subscript,
                        format_args!(
                            "Type qualifier `{qualifier}` is not allowed in type expressions \
                            (only in annotation expressions)",
                        ),
                    );
                }
                self.infer_type_expression(arguments_slice)
            }
            SpecialFormType::TypeIs => match arguments_slice {
                ast::Expr::Tuple(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(arguments_slice, TypeContext::default());
                    }

                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        let diag = builder.into_diagnostic(
                            "Special form `typing.TypeIs` expected exactly one type parameter",
                        );
                        diagnostic::add_type_expression_reference_link(diag);
                    }

                    Type::unknown()
                }
                _ => {
                    let narrowed = self.infer_type_expression(arguments_slice);
                    let expanded = narrowed.expand_eagerly(db, env);

                    if expanded.is_divergent() {
                        expanded
                    } else {
                        TypeIsType::from_type_expression(self.db(), narrowed)
                    }
                }
            },
            SpecialFormType::TypeGuard => match arguments_slice {
                ast::Expr::Tuple(_) => {
                    if !self.in_string_annotation() {
                        self.infer_expression(arguments_slice, TypeContext::default());
                    }

                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        let diag = builder.into_diagnostic(
                            "Special form `typing.TypeGuard` expected exactly one type parameter",
                        );
                        diagnostic::add_type_expression_reference_link(diag);
                    }

                    Type::unknown()
                }
                _ => TypeGuardType::unbound(self.db(), self.infer_type_expression(arguments_slice)),
            },
            SpecialFormType::Concatenate => {
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    let mut diag = builder.into_diagnostic(format_args!(
                        "`typing.Concatenate` is not allowed in this context in a {}",
                        self.type_expression_context()
                    ));
                    diag.info("`typing.Concatenate` is only valid:");
                    diag.info(" - as the first argument to `Callable`");
                    diag.info(" - as a type argument for a `ParamSpec` parameter");
                }

                let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
                    && !tuple.is_anon_named_tuple
                {
                    &*tuple.elts
                } else {
                    std::slice::from_ref(arguments_slice)
                };

                for (i, argument) in arguments.iter().enumerate() {
                    if argument.is_ellipsis_literal_expr() {
                        // The trailing `...` in `Concatenate[int, str, ...]` is valid;
                        // store without going through type-expression inference.
                        self.store_expression_type(argument, Type::unknown());
                    } else if i < arguments.len() - 1 {
                        let previously_allowed_paramspec = self
                            .context
                            .inference_flags
                            .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, false);
                        self.infer_type_expression(argument);
                        self.context.inference_flags.set(
                            InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
                            previously_allowed_paramspec,
                        );
                    } else {
                        let previously_allowed_paramspec = self
                            .context
                            .inference_flags
                            .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, true);
                        self.infer_type_expression(argument);
                        self.context.inference_flags.set(
                            InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
                            previously_allowed_paramspec,
                        );
                    }
                }

                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, Type::unknown());
                }

                Type::Dynamic(DynamicType::InvalidConcatenateUnknown)
            }
            SpecialFormType::Unpack => {
                self.store_type_expression_flags(
                    ast::ExprRef::from(subscript),
                    TypeExpressionFlags::UNPACK,
                );

                let inference_flags = self.inference_flags();
                let is_nested_unpack =
                    inference_flags.contains(InferenceFlags::IN_UNPACK_TYPE_ARGUMENT);
                let is_nested_kwargs = inference_flags
                    .contains(InferenceFlags::IN_KWARG_ANNOTATION)
                    && inference_flags.contains(InferenceFlags::IN_NESTED_TYPE_EXPRESSION);
                let is_invalid_context = !inference_flags.intersects(
                    InferenceFlags::IN_VARARG_ANNOTATION
                        | InferenceFlags::IN_KWARG_ANNOTATION
                        | InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                );

                let previously_in_unpack_type_argument = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::IN_UNPACK_TYPE_ARGUMENT, true);
                let inner_ty = if self.in_string_annotation()
                    && (is_nested_unpack || is_nested_kwargs || is_invalid_context)
                {
                    // Invalid string annotations never execute, so their operands must not
                    // produce runtime errors even though their inferred types are still needed.
                    let mut speculative = self.speculate_without_diagnostics();
                    let inner_ty = speculative.infer_type_expression(arguments_slice);
                    self.extend(speculative);
                    inner_ty
                } else {
                    self.infer_type_expression(arguments_slice)
                };
                self.context.inference_flags.set(
                    InferenceFlags::IN_UNPACK_TYPE_ARGUMENT,
                    previously_in_unpack_type_argument,
                );

                if is_nested_unpack {
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        diagnostic::add_type_expression_reference_link(
                            builder.into_diagnostic("`Unpack` cannot be nested"),
                        );
                    }
                    return Type::unknown();
                }

                if is_nested_kwargs {
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        diagnostic::add_type_expression_reference_link(builder.into_diagnostic(
                            "`Unpack` is only valid as the top-level `**kwargs` annotation form",
                        ));
                    }
                    return Type::unknown();
                }

                if is_invalid_context {
                    if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                        diagnostic::add_type_expression_reference_link(builder.into_diagnostic(
                            format_args!(
                                "`Unpack` is not allowed in {}s",
                                self.type_expression_context()
                            ),
                        ));
                    }
                    return Type::unknown();
                }

                if self
                    .inference_flags()
                    .contains(InferenceFlags::IN_KWARG_ANNOTATION)
                {
                    return inner_ty;
                }

                // Preserve valid unpack targets so that `Unpack[...]` follows the same
                // argument-binding path as an equivalent starred annotation.
                if let Some(target) = unpack_target(self.db(), inner_ty) {
                    target
                } else {
                    self.store_type_expression_flags(
                        ast::ExprRef::from(subscript),
                        TypeExpressionFlags::INVALID_UNPACK,
                    );
                    if !inner_ty.is_unknown()
                        && let Some(builder) =
                            self.context.report_lint(&INVALID_TYPE_FORM, subscript)
                    {
                        diagnostic::add_type_expression_reference_link(builder.into_diagnostic(
                            "`Unpack` can only unpack a tuple type or `TypeVarTuple`",
                        ));
                    }
                    Type::homogeneous_tuple(db, env, Type::unknown())
                }
            }
            SpecialFormType::NoReturn
            | SpecialFormType::Never
            | SpecialFormType::AlwaysTruthy
            | SpecialFormType::AlwaysFalsy => {
                if !self.in_string_annotation() {
                    self.infer_expression(arguments_slice, TypeContext::default());
                }

                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    builder.into_diagnostic(format_args!(
                        "Type `{special_form}` expected no type parameter",
                    ));
                }
                Type::unknown()
            }
            SpecialFormType::TypingSelf
            | SpecialFormType::TypeAlias
            | SpecialFormType::TypedDict(_)
            | SpecialFormType::Unknown
            | SpecialFormType::Divergent
            | SpecialFormType::Todo
            | SpecialFormType::Any
            | SpecialFormType::NamedTuple => {
                if !self.in_string_annotation() {
                    self.infer_expression(arguments_slice, TypeContext::default());
                }

                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    builder.into_diagnostic(format_args!(
                        "Special form `{special_form}` expected no type parameter",
                    ));
                }
                Type::unknown()
            }
            SpecialFormType::LiteralString => {
                let arguments = self.infer_expression(arguments_slice, TypeContext::default());

                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    let mut diag =
                        builder.into_diagnostic("`LiteralString` expects no type parameter");

                    let arguments_as_tuple = arguments.exact_tuple_instance_spec(db);

                    let argument_elements = arguments_as_tuple.as_ref().map_or_else(
                        || vec![arguments],
                        |tuple| tuple.iter_element_types(db).collect(),
                    );
                    let mut argument_elements = argument_elements.into_iter();

                    let probably_meant_literal = argument_elements.all(|ty| match ty {
                        Type::LiteralValue(literal)
                            if matches!(
                                literal.kind(),
                                LiteralValueTypeKind::String(_)
                                    | LiteralValueTypeKind::Bytes(_)
                                    | LiteralValueTypeKind::Enum(_)
                                    | LiteralValueTypeKind::Bool(_)
                            ) =>
                        {
                            true
                        }
                        Type::NominalInstance(instance) => {
                            instance.has_known_class(db, KnownClass::NoneType)
                        }
                        _ => false,
                    });

                    if probably_meant_literal {
                        diag.annotate(
                            self.context
                                .secondary(&*subscript.value)
                                .message("Did you mean `Literal`?"),
                        );
                        diag.set_concise_message(
                            "`LiteralString` expects no type parameter - did you mean `Literal`?",
                        );
                    }
                }
                Type::unknown()
            }
            SpecialFormType::Type => self.infer_subclass_of_type_expression(arguments_slice),
            SpecialFormType::Tuple => Type::tuple(self.infer_tuple_type_expression(subscript)),
            SpecialFormType::Generic | SpecialFormType::Protocol => {
                if !self.in_string_annotation() {
                    self.infer_expression(arguments_slice, TypeContext::default());
                }
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    builder.into_diagnostic(format_args!(
                        "`{special_form}` is not allowed in {}s",
                        self.type_expression_context(),
                    ));
                }
                Type::unknown()
            }
        }
    }

    pub(crate) fn infer_literal_parameter_type<'param>(
        &mut self,
        parameters: &'param ast::Expr,
    ) -> Result<Type<'db>, Vec<&'param ast::Expr>> {
        let db = self.db();
        let env = self.program_environment();
        let ty = match parameters {
            ast::Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                let value_ty = self.infer_expression(value, TypeContext::default());
                if matches!(value_ty, Type::SpecialForm(SpecialFormType::Literal)) {
                    let ty = self.infer_literal_parameter_type(slice)?;

                    // This branch deals with annotations such as `Literal[Literal[1]]`.
                    // Here, we store the type for the inner `Literal[1]` expression:
                    self.store_expression_type(parameters, ty);
                    ty
                } else {
                    self.infer_expression(slice, TypeContext::default());
                    self.store_expression_type(parameters, Type::unknown());

                    return Err(vec![parameters]);
                }
            }
            ast::Expr::Tuple(tuple) if !tuple.parenthesized => {
                let mut errors = vec![];
                let mut builder = UnionBuilder::new(db, env);
                for elt in tuple {
                    match self.infer_literal_parameter_type(elt) {
                        Ok(ty) => {
                            builder = builder.add(ty);
                        }
                        Err(nodes) => {
                            errors.extend(nodes);
                        }
                    }
                }
                if errors.is_empty() {
                    let union_type = builder.build();

                    // This branch deals with annotations such as `Literal[1, 2]`. Here, we
                    // store the type for the inner `1, 2` tuple-expression:
                    self.store_expression_type(parameters, union_type);

                    union_type
                } else {
                    self.store_expression_type(parameters, Type::unknown());

                    return Err(errors);
                }
            }

            literal @ (ast::Expr::StringLiteral(_)
            | ast::Expr::BytesLiteral(_)
            | ast::Expr::BooleanLiteral(_)
            | ast::Expr::NoneLiteral(_)) => self.infer_expression(literal, TypeContext::default()),
            literal @ ast::Expr::NumberLiteral(number) if number.value.is_int() => {
                self.infer_expression(literal, TypeContext::default())
            }

            // for negative and positive numbers
            ast::Expr::UnaryOp(unary @ ast::ExprUnaryOp { op, operand, .. })
                if matches!(op, ast::UnaryOp::USub | ast::UnaryOp::UAdd)
                    && matches!(
                        &**operand,
                        ast::Expr::NumberLiteral(ast::ExprNumberLiteral {
                            value: ast::Number::Int(_),
                            ..
                        })
                    ) =>
            {
                let ty = self.infer_unary_expression(unary);
                self.store_expression_type(parameters, ty);
                ty
            }
            // enum members and aliases to literal types
            ast::Expr::Name(_) | ast::Expr::Attribute(_) => {
                let subscript_ty = self.infer_expression(parameters, TypeContext::default());
                match subscript_ty {
                    // type aliases to literal types
                    Type::KnownInstance(KnownInstanceType::TypeAliasType(type_alias)) => {
                        let value_ty = type_alias.value_type(db);
                        if value_ty.is_literal_or_union_of_literals(db, env) {
                            return Ok(value_ty);
                        }
                    }
                    Type::KnownInstance(KnownInstanceType::Literal(ty)) => {
                        return Ok(ty.inner(self.db()));
                    }
                    // `Literal[SomeEnum.Member]`
                    Type::LiteralValue(literal) if literal.is_enum() => {
                        // Avoid promoting values originating from an explicitly annotated literal type.
                        return Ok(Type::LiteralValue(literal.to_unpromotable()));
                    }
                    // `Literal[SingletonEnum.Member]`, where `SingletonEnum.Member` simplifies to
                    // just `SingletonEnum`.
                    Type::NominalInstance(_) if subscript_ty.is_enum(db, env) => {
                        return Ok(subscript_ty);
                    }
                    // suppress false positives for e.g. members of functional-syntax enums
                    Type::Dynamic(DynamicType::Todo(_)) => {
                        return Ok(subscript_ty);
                    }
                    _ => {}
                }
                return Err(vec![parameters]);
            }
            _ => {
                if !self.in_string_annotation() {
                    self.infer_expression(parameters, TypeContext::default());
                }
                return Err(vec![parameters]);
            }
        };

        Ok(if let Type::LiteralValue(literal) = ty {
            // Avoid promoting values originating from an explicitly annotated literal type.
            Type::LiteralValue(literal.to_unpromotable())
        } else {
            ty
        })
    }

    /// Infer the first argument to a `typing.Callable` type expression and returns the
    /// corresponding [`Parameters`].
    ///
    /// It returns `None` if the argument is invalid i.e., not a list of types, parameter
    /// specification, `typing.Concatenate`, or `...`.
    fn infer_callable_parameter_types(
        &mut self,
        parameters: &ast::Expr,
    ) -> Option<Parameters<'db>> {
        let env = self.program_environment();
        let db = self.db();
        match parameters {
            ast::Expr::EllipsisLiteral(ast::ExprEllipsisLiteral { .. }) => {
                return Some(Parameters::gradual_form());
            }
            // basedpython: `Callable[(int, str), R]` — tuples and parameter
            // lists are interchangeable. lower the parenthesized tuple to
            // a parameter list with the same shape semantics. positional
            // fields, named fields, variadic (`*name: T`), and kwargs
            // catch-all (`**name: T`) all map onto Parameter slots
            ast::Expr::Tuple(tuple) if tuple.parenthesized => {
                let mut params: Vec<Parameter<'db>> = Vec::with_capacity(tuple.elts.len());
                let parameter_star = tuple.parameter_star().map(|i| i as usize);
                for (i, elt) in tuple.elts.iter().enumerate() {
                    let after_star = parameter_star.is_some_and(|s| i >= s);
                    match elt {
                        // `*: T` or `**: T`
                        ast::Expr::Starred(s) => match s.value.as_ref() {
                            // `**: T` — anonymous kwargs catch-all
                            ast::Expr::Starred(inner) => {
                                let ty = self.infer_type_expression(&inner.value);
                                params.push(
                                    Parameter::keyword_variadic(
                                        ruff_python_ast::name::Name::new_static("kwargs"),
                                    )
                                    .with_annotated_type(ty),
                                );
                            }
                            // `*: T` — anonymous variadic
                            _ => {
                                let ty = self.infer_type_expression(&s.value);
                                params.push(
                                    Parameter::variadic(ruff_python_ast::name::Name::new_static(
                                        "args",
                                    ))
                                    .with_annotated_type(ty),
                                );
                            }
                        },
                        ast::Expr::Named(named) => match named.target.as_ref() {
                            ast::Expr::Starred(starred) => {
                                let name_str = match starred.value.as_ref() {
                                    ast::Expr::Starred(inner_inner) => {
                                        // `**name: T`
                                        inner_inner
                                            .value
                                            .as_name_expr()
                                            .map(|n| n.id.as_str())
                                            .unwrap_or("kwargs")
                                            .to_owned()
                                    }
                                    // the anonymous `*: *Ts` carries the empty
                                    // name marker, and reads as the `*: T` it is
                                    // the starred spelling of
                                    _ => starred
                                        .value
                                        .as_name_expr()
                                        .map(|n| n.id.as_str())
                                        .filter(|name| !name.is_empty())
                                        .unwrap_or("args")
                                        .to_owned(),
                                };
                                let ty = self.infer_type_expression(&named.value);
                                let is_kwvariadic =
                                    matches!(starred.value.as_ref(), ast::Expr::Starred(_));
                                let p = if is_kwvariadic {
                                    Parameter::keyword_variadic(ruff_python_ast::name::Name::new(
                                        &name_str,
                                    ))
                                } else {
                                    Parameter::variadic(ruff_python_ast::name::Name::new(&name_str))
                                };
                                params.push(p.with_annotated_type(ty));
                            }
                            _ => {
                                // `name: T`
                                let name_str = named
                                    .target
                                    .as_name_expr()
                                    .map(|n| n.id.as_str().to_owned())
                                    .unwrap_or_default();
                                let ty = self.infer_type_expression(&named.value);
                                let p = if after_star {
                                    Parameter::keyword_only(ruff_python_ast::name::Name::new(
                                        &name_str,
                                    ))
                                } else {
                                    Parameter::positional_or_keyword(
                                        ruff_python_ast::name::Name::new(&name_str),
                                    )
                                };
                                params.push(p.with_annotated_type(ty));
                            }
                        },
                        _ => {
                            let ty = self.infer_type_expression(elt);
                            params.push(Parameter::positional_only(None).with_annotated_type(ty));
                        }
                    }
                }
                return Some(Parameters::from_annotation(self.db(), env, params));
            }
            ast::Expr::List(ast::ExprList { elts: params, .. }) => {
                if let [ast::Expr::EllipsisLiteral(_)] = &params[..] {
                    // Return `None` here so that we emit a specific diagnostic at the callsite.
                    return None;
                }

                let mut parameters = Vec::with_capacity(params.len());

                let previously_in_valid_unpack_context = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::IN_VALID_UNPACK_CONTEXT, true);
                for param in params {
                    let param_type = self.infer_type_expression(param);
                    let is_unpack = self
                        .type_expression_flags(param)
                        .contains(TypeExpressionFlags::UNPACK);

                    if is_unpack {
                        if let Type::TypeVar(typevar) = param_type
                            && typevar.is_typevartuple(self.db())
                        {
                            parameters.push(
                                Parameter::variadic(Name::new_static("args"))
                                    .with_annotated_type(Type::TypeVar(typevar))
                                    .with_starred_annotation(),
                            );
                            continue;
                        }

                        if param_type.exact_tuple_instance_spec(self.db()).is_some() {
                            parameters.push(
                                Parameter::variadic(Name::new_static("args"))
                                    .with_annotated_type(param_type)
                                    .with_starred_annotation(),
                            );
                            continue;
                        }
                    }

                    parameters
                        .push(Parameter::positional_only(None).with_annotated_type(param_type));
                }
                self.context.inference_flags.set(
                    InferenceFlags::IN_VALID_UNPACK_CONTEXT,
                    previously_in_valid_unpack_context,
                );

                return Some(Parameters::from_annotation(db, env, parameters));
            }
            ast::Expr::Subscript(subscript) => {
                let value_ty = self.infer_expression(&subscript.value, TypeContext::default());

                if matches!(value_ty, Type::SpecialForm(SpecialFormType::Concatenate)) {
                    return Some(self.infer_concatenate_special_form(subscript));
                }

                self.infer_subscript_type_expression(subscript, value_ty);

                // Non-Concatenate subscript (e.g. Unpack): fall back to todo
                return Some(Parameters::todo());
            }
            ast::Expr::Name(_) | ast::Expr::Attribute(_) => {
                if parameters
                    .as_name_expr()
                    .is_some_and(ast::ExprName::is_invalid)
                {
                    // This is a special case to avoid raising the error suggesting what the first
                    // argument should be. This only happens when there's already a syntax error like
                    // `Callable[]`.
                    return None;
                }
                let previously_allowed_paramspec = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, true);
                let parameters_type = self.infer_type_expression_no_store(parameters);
                self.context.inference_flags.set(
                    InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
                    previously_allowed_paramspec,
                );
                if let Type::TypeVar(tvar) = parameters_type
                    && tvar.is_paramspec(self.db())
                {
                    return Some(Parameters::paramspec(db, tvar));
                }
                if parameters_type == Type::Dynamic(DynamicType::InvalidConcatenateUnknown) {
                    // Avoid emitting a confusing error here saying that the first argument to
                    // `Callable` must be "Concatenate, `...`, a parameter list or a ParamSpec"
                    // if the first argument *was* in fact `Concatenate` -- it was just used
                    // incorrectly. We'll have emitted an error elsewhere about the invalid use.
                    return Some(Parameters::unknown());
                }
            }
            ast::Expr::StringLiteral(string) => {
                if let Some(parsed) =
                    parse_string_annotation(&self.context, self.inference_flags(), string)
                {
                    self.string_annotations
                        .insert(ruff_python_ast::ExprRef::StringLiteral(string).into());
                    let node_key = self.enclosing_node_key(string.into());

                    let previous_deferred_state = std::mem::replace(
                        &mut self.deferred_state,
                        DeferredExpressionState::InStringAnnotation(node_key),
                    );
                    let result = matches!(
                        parsed.expr(),
                        ast::Expr::Name(_) | ast::Expr::Attribute(_) | ast::Expr::Subscript(_)
                    )
                    .then(|| self.infer_callable_parameter_types(parsed.expr()));
                    self.deferred_state = previous_deferred_state;

                    if let Some(result) = result {
                        return result;
                    }
                }
            }
            _ => {}
        }
        if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, parameters) {
            let diag = builder.into_diagnostic(format_args!(
                "The first argument to `Callable` must be either a list of types, \
                ParamSpec, Concatenate, or `...`",
            ));
            diagnostic::add_type_expression_reference_link(diag);
        }
        None
    }

    /// Infer the parameter types represented by a `typing.Concatenate` special form.
    pub(super) fn infer_concatenate_special_form(
        &mut self,
        subscript: &ast::ExprSubscript,
    ) -> Parameters<'db> {
        let db = self.db();
        let previous_concatenate_context = self
            .context
            .inference_flags
            .replace(InferenceFlags::IN_VALID_CONCATENATE_CONTEXT, false);

        let arguments_slice = &*subscript.slice;
        let arguments = if let ast::Expr::Tuple(tuple) = arguments_slice
            && !tuple.is_anon_named_tuple
        {
            &*tuple.elts
        } else {
            std::slice::from_ref(arguments_slice)
        };

        let (last_arg, prefix_args) = match arguments.split_last() {
            Some((last_arg, prefix_args)) if !prefix_args.is_empty() => (last_arg, prefix_args),
            _ => {
                if !self.in_string_annotation() {
                    for argument in arguments {
                        self.infer_expression(argument, TypeContext::default());
                    }
                }
                if let Some(builder) = self.context.report_lint(&INVALID_TYPE_FORM, subscript) {
                    builder.into_diagnostic(format_args!(
                        "`typing.Concatenate` requires at least 2 arguments when used in a \
                        type expression (got {})",
                        arguments.len()
                    ));
                }
                if arguments_slice.is_tuple_expr() {
                    self.store_expression_type(arguments_slice, Type::unknown());
                }
                return Parameters::gradual_form();
            }
        };

        let previously_allowed_paramspec = self
            .context
            .inference_flags
            .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, false);
        let prefix_params: Vec<Parameter<'db>> = prefix_args
            .iter()
            .flat_map(|arg| -> Vec<Parameter<'db>> {
                // basedpython: tuples and parameter lists are equivalent —
                // a parenthesized tuple in `Concatenate`'s prefix expands
                // into individual positional parameters
                if let ast::Expr::Tuple(tuple) = arg
                    && tuple.parenthesized
                {
                    return tuple
                        .elts
                        .iter()
                        .map(|elt| {
                            let ty = match elt {
                                ast::Expr::Named(named) => self.infer_type_expression(&named.value),
                                _ => self.infer_type_expression(elt),
                            };
                            Parameter::positional_only(None).with_annotated_type(ty)
                        })
                        .collect();
                }
                vec![
                    Parameter::positional_only(None)
                        .with_annotated_type(self.infer_type_expression(arg)),
                ]
            })
            .collect();
        self.context.inference_flags.set(
            InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
            previously_allowed_paramspec,
        );

        let parameters = self
            .infer_concatenate_tail(last_arg)
            .map(|tail| Parameters::concatenate(db, prefix_params, tail));

        if arguments_slice.is_tuple_expr() {
            // TODO: What type to store for the argument slice in `Concatenate` because
            // `Parameters` is not a `Type` variant?
            self.store_expression_type(arguments_slice, Type::unknown());
        }

        let result = parameters.unwrap_or_else(Parameters::unknown);

        self.context.inference_flags.set(
            InferenceFlags::IN_VALID_CONCATENATE_CONTEXT,
            previous_concatenate_context,
        );
        result
    }

    /// Infer the last argument to a `typing.Concatenate` special form, which can be either `...`
    /// (for gradual typing), a `ParamSpec` type variable, or a string annotation that evaluates to
    /// a `ParamSpec` type variable.
    fn infer_concatenate_tail(&mut self, expr: &ast::Expr) -> Option<ConcatenateTail<'db>> {
        match expr {
            ast::Expr::EllipsisLiteral(_) => Some(ConcatenateTail::Gradual),
            ast::Expr::Name(_) | ast::Expr::Attribute(_) => {
                if expr.as_name_expr().is_some_and(ast::ExprName::is_invalid) {
                    return None;
                }
                let previously_allowed_paramspec = self
                    .context
                    .inference_flags
                    .replace(InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR, true);
                let expr_type = self.infer_type_expression_no_store(expr);
                self.context.inference_flags.set(
                    InferenceFlags::ALLOW_PARAMSPEC_TYPE_EXPR,
                    previously_allowed_paramspec,
                );
                let Type::TypeVar(typevar) = expr_type else {
                    // `Concatenate` *is* allowed inside `Concatenate`, so avoid emitting here a diagnostic
                    // saying that the argument is invalid if the inner type is an invalid use of the
                    // `Concatenate` special form (we'll already have complained about the invalid use
                    // elsewhere)
                    if expr_type != Type::Dynamic(DynamicType::InvalidConcatenateUnknown) {
                        report_invalid_concatenate_last_arg(&self.context, expr, expr_type);
                    }
                    return None;
                };
                if !typevar.is_parameter_pack(self.db()) {
                    report_invalid_concatenate_last_arg(&self.context, expr, expr_type);
                    return None;
                }
                Some(ConcatenateTail::ParamSpec(typevar))
            }
            ast::Expr::StringLiteral(string) => {
                let Some(parsed) =
                    parse_string_annotation(&self.context, self.inference_flags(), string)
                else {
                    report_invalid_concatenate_last_arg(&self.context, expr, Type::unknown());
                    return None;
                };

                self.string_annotations
                    .insert(ruff_python_ast::ExprRef::StringLiteral(string).into());
                let node_key = self.enclosing_node_key(string.into());

                if !matches!(
                    parsed.expr(),
                    ast::Expr::Name(_) | ast::Expr::Attribute(_) | ast::Expr::Subscript(_)
                ) {
                    report_invalid_concatenate_last_arg(&self.context, expr, Type::unknown());
                    return None;
                }

                let previous_deferred_state = std::mem::replace(
                    &mut self.deferred_state,
                    DeferredExpressionState::InStringAnnotation(node_key),
                );
                let result = self.infer_concatenate_tail(parsed.expr());
                self.deferred_state = previous_deferred_state;

                result
            }
            _ => {
                let ty = self.infer_type_expression(expr);
                if ty != Type::Dynamic(DynamicType::InvalidConcatenateUnknown) {
                    report_invalid_concatenate_last_arg(&self.context, expr, ty);
                }
                None
            }
        }
    }

    /// Checks if the inferred type is an unbound type variable and reports a diagnostic if so.
    ///
    /// Returns `Unknown` as a fallback if the type variable is unbound, otherwise returns the
    /// original type unchanged.
    fn check_for_unbound_type_variable(&self, expression: &ast::Expr, ty: Type<'db>) -> Type<'db> {
        if !self
            .inference_flags()
            .contains(InferenceFlags::CHECK_UNBOUND_TYPEVARS)
        {
            return ty;
        }
        if let Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) = ty {
            if let Some(builder) = self.context.report_lint(&UNBOUND_TYPE_VARIABLE, expression) {
                builder.into_diagnostic(format_args!(
                    "Type variable `{name}` is not bound to any outer generic context",
                    name = typevar.name(self.db())
                ));
            }
            Type::unknown()
        } else {
            ty
        }
    }
}

/// One element of a subscript slice that may or may not carry a variance
/// marker.
pub(super) struct VarianceSliceElement<'ast> {
    variance: Option<UseSiteVariance>,
    /// inner expression — for marker elements this is the unwrapped
    /// expression inside the marker; for non-marker elements the element
    /// itself.
    inner: &'ast ast::Expr,
}

/// The type an unpack target (`*T` or `Unpack[T]`) splices in, or `None` if `ty`
/// is not a valid target.
///
/// A type alias is resolved first, so `type A = tuple[int, str]` unpacks exactly
/// as the tuple it names does. The resolved type is what gets returned, since it
/// is the spliced elements the caller needs.
fn unpack_target<'db>(db: &'db dyn crate::Db, ty: Type<'db>) -> Option<Type<'db>> {
    let resolved = ty.resolve_type_alias(db);
    let is_target = resolved.exact_tuple_instance_spec(db).is_some()
        || matches!(resolved, Type::TypeVar(typevar) if typevar.is_typevartuple(db));
    is_target.then_some(resolved)
}

/// basedpython: map the attribute name of `float.inf` / `float.nan` to its
/// `f64` value, or `None` for any other attribute
fn basedpython_float_constant(attr: &ast::Identifier) -> Option<f64> {
    match attr.as_str() {
        "inf" => Some(f64::INFINITY),
        "nan" => Some(f64::NAN),
        _ => None,
    }
}

/// If `slice` contains at least one use-site variance marker, return the
/// flat list of slice elements with their variance and inner expression.
/// Returns `None` if no element is variance-marked.
pub(super) fn use_site_variance_slice_elements(
    slice: &ast::Expr,
) -> Option<Vec<VarianceSliceElement<'_>>> {
    let elements: Vec<&ast::Expr> = match slice {
        ast::Expr::Tuple(t)
            if !t.parenthesized && !t.is_anon_named_tuple && !t.is_anon_named_tuple_value =>
        {
            t.elts.iter().collect()
        }
        other => vec![other],
    };
    let mut any_marker = false;
    let mapped: Vec<VarianceSliceElement<'_>> = elements
        .into_iter()
        .map(|elt| {
            if let Some((variance, inner)) = use_site_variance_marker(elt) {
                any_marker = true;
                VarianceSliceElement {
                    variance: Some(variance),
                    inner,
                }
            } else {
                VarianceSliceElement {
                    variance: None,
                    inner: elt,
                }
            }
        })
        .collect();
    if any_marker { Some(mapped) } else { None }
}

/// Build the projected class type for `value_ty[slice...]` where at least one
/// slice element carries a use-site variance marker. Returns the outer class
/// specialized with the slice's types and tagged with per-typevar projections
/// so downstream member access can apply kotlin-style variance restrictions.
/// `None` if the outer is not a class.
pub(super) fn resolve_use_site_variance_class<'db, 'ast>(
    db: &'db dyn crate::Db,
    value_ty: Type<'db>,
    elements: &[VarianceSliceElement<'ast>],
    mut infer_inner: impl FnMut(&'ast ast::Expr) -> Type<'db>,
) -> Option<ClassType<'db>> {
    let Type::ClassLiteral(class_literal) = value_ty else {
        // variance keywords on a non-class outer — infer inners for
        // diagnostics and bail.
        for element in elements {
            let _ = infer_inner(element.inner);
        }
        return None;
    };
    let arg_types: Vec<Type<'db>> = elements.iter().map(|elt| infer_inner(elt.inner)).collect();
    let projections: Vec<Option<UseSiteVariance>> =
        elements.iter().map(|elt| elt.variance).collect();
    let class_type = class_literal.apply_specialization(db, |generic_context| {
        let n = generic_context.len(db);
        if arg_types.len() == n {
            let spec = generic_context.specialize(db, arg_types.as_slice());
            spec.with_projections(db, projections.clone().into_boxed_slice())
        } else {
            generic_context.specialize(db, vec![Type::unknown(); n].as_slice())
        }
    });
    Some(class_type)
}

/// The instance type a variance-marked subscript denotes in a type expression.
/// Falls back to `Unknown` if the outer is not a class.
fn resolve_use_site_variance<'db, 'ast>(
    db: &'db dyn crate::Db,
    env: &ProgramEnvironment<'db>,
    value_ty: Type<'db>,
    elements: &[VarianceSliceElement<'ast>],
    infer_inner: impl FnMut(&'ast ast::Expr) -> Type<'db>,
) -> Type<'db> {
    match resolve_use_site_variance_class(db, value_ty, elements, infer_inner) {
        Some(class_type) => Type::instance(db, env, class_type),
        None => Type::unknown(),
    }
}

/// basedpython: whether `expr` is a place a narrowing predicate annotation can name — a
/// bare name, or an attribute chain rooted at one (`self.data`).
fn is_narrowing_predicate_place(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Name(_) => true,
        ast::Expr::Attribute(attribute) => is_narrowing_predicate_place(&attribute.value),
        _ => false,
    }
}
