use crate::Db;
use crate::ProgramEnvironment;
use compact_str::CompactString;
use ruff_python_ast::{self as ast, AnyNodeRef};

use super::TypeInferenceBuilder;
use crate::types::call::CallArguments;
use crate::types::constraints::ConstraintSetBuilder;
use crate::types::cyclic::CycleDetector;
use crate::types::deferred::{is_integer_operand, is_symbolic_operand};
use crate::types::diagnostic::{
    DIVISION_BY_ZERO, report_unsupported_augmented_assignment, report_unsupported_binary_operation,
};
use crate::types::function::OverloadLiteral;
use crate::types::inferred_signature::gradual_hole;
use crate::types::set_theoretic::RecursivelyDefined;
use crate::types::tuple::Tuple;
use crate::types::typevar::TypeVarConstraints;
use crate::types::{
    DeferredOperation, DeferredType, DynamicType, InternedConstraintSet, KnownClass,
    KnownInstanceType, LiteralValueTypeKind, MemberLookupPolicy, Type, TypeContext,
    TypeVarBoundOrConstraints, TypedDictType, UnionBuilder, UnionType, UnionTypeInstance,
    UnsafeUnionType,
};

enum BinaryExpressionOperandTypes<'db> {
    Inferred(Type<'db>, Type<'db>),
    TypedDictResult(Type<'db>),
}

type BinaryExpressionVisitor<'db> =
    CycleDetector<'db, ast::Operator, (Type<'db>, ast::Operator, Type<'db>), Option<Type<'db>>, 1>;

/// Diagnostic state shared across the alternatives of one binary or augmented operation.
#[derive(Default)]
pub(crate) struct BinaryInferenceState<'db> {
    pub(super) emitted_division_by_zero_diagnostic: bool,
    pub(super) deprecated_functions: Vec<OverloadLiteral<'db>>,
}

impl<'db> TypeInferenceBuilder<'db, '_> {
    pub(super) fn infer_binary_expression(
        &mut self,
        binary: &ast::ExprBinOp,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let env = self.program_environment();
        if tcx.is_typealias() {
            return self.infer_pep_604_union_type_alias(binary, tcx);
        }

        let ast::ExprBinOp {
            left,
            op,
            right,
            range: _,
            node_index: _,
        } = binary;

        // basedpython `a ?? b` lowers to `a if a is not None else b`. infer
        // accordingly: result is `(left minus None) | right`
        if *op == ast::Operator::Coalesce {
            let db = self.db();
            let left_ty = self.infer_expression(left, tcx);
            let right_ty = self.infer_expression(right, tcx);
            let none = Type::none(db, env);
            let left_non_none = if left_ty.is_subtype_of(db, env, none) {
                Type::Never
            } else {
                match left_ty {
                    Type::Union(u) => u.map(db, env, |elem| {
                        if elem.is_subtype_of(db, env, none) {
                            Type::Never
                        } else {
                            *elem
                        }
                    }),
                    _ => left_ty,
                }
            };
            if left_non_none.is_equivalent_to(db, env, right_ty) {
                return left_non_none;
            }
            return UnionType::from_two_elements(db, env, left_non_none, right_ty);
        }

        let (left_ty, right_ty) =
            match self.infer_binary_expression_operand_types(left, *op, right, tcx) {
                BinaryExpressionOperandTypes::TypedDictResult(ty) => return ty,
                BinaryExpressionOperandTypes::Inferred(left_ty, right_ty) => (left_ty, right_ty),
            };

        let mut state = BinaryInferenceState::default();
        let return_type = self
            .infer_binary_expression_type(binary.into(), left_ty, right_ty, *op, tcx, &mut state)
            // basedpython: an applicable extension may supply the left
            // operand's dunder, or the right operand's reflected one
            .or_else(|| self.try_binary_extension_operator(left_ty, *op, right_ty));
        self.report_deprecated_functions(binary, state.deprecated_functions);
        return_type.unwrap_or_else(|| {
            report_unsupported_binary_operation(&self.context, binary, left_ty, right_ty, *op);
            Type::unknown()
        })
    }

    fn infer_pep_604_union_type_alias(
        &mut self,
        node: &ast::ExprBinOp,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let env = self.program_environment();
        let ast::ExprBinOp {
            left,
            op,
            right,
            range: _,
            node_index: _,
        } = node;

        if *op != ast::Operator::BitOr {
            // TODO diagnostic?
            return Type::unknown();
        }

        let left_ty = self.infer_expression(left, tcx);
        let right_ty = self.infer_expression(right, tcx);

        // TODO this is overly aggressive; if the operands' `__or__` does not actually return a
        // `UnionType` at runtime, we should ideally not infer one here. But this is unlikely to be
        // a problem in practice: it would require someone having an explicitly annotated
        // `TypeAlias`, which uses `X | Y` syntax, where the returned type is not actually a union.
        // And attempting to enforce this more tightly showed a lot of potential false positives in
        // the ecosystem.
        if left_ty.is_equivalent_to(db, env, right_ty) {
            left_ty
        } else {
            UnionTypeInstance::from_value_expression_types(
                db,
                [left_ty, right_ty],
                self.scope(),
                self.typevar_binding_context,
                self.inference_flags(),
            )
        }
    }

    /// Returns a `TypedDict` result when a PEP 584 special case succeeds, otherwise the inferred
    /// operand types for ordinary binary inference.
    fn infer_binary_expression_operand_types(
        &mut self,
        left: &ast::Expr,
        op: ast::Operator,
        right: &ast::Expr,
        tcx: TypeContext<'db>,
    ) -> BinaryExpressionOperandTypes<'db> {
        let db = self.db();
        // As a special case, pass `tcx` to binary operands that are collection literals/displays.
        // Note that it's not correct to pass it to all binary operands, for example:
        // ```
        // x: list[str] = ["x"] * 3
        // ```
        // It doesn't make sense to pass the list type context to the `3` expression. It wouldn't
        // have any effect in this case, but it could in more complicated cases.
        // TODO: When we support passing `tcx` through generic method calls, we can remove this
        // special case and handle the relevant dunder method instead.
        let operand_tcx = |expr: &ast::Expr| -> TypeContext<'db> {
            match expr {
                ast::Expr::List(_)
                | ast::Expr::Tuple(_)
                | ast::Expr::Set(_)
                | ast::Expr::Dict(_)
                | ast::Expr::ListComp(_)
                | ast::Expr::SetComp(_)
                | ast::Expr::DictComp(_) => tcx,
                // Also pass `tcx` to nested binary expressions.
                ast::Expr::BinOp(_) => tcx,
                _ => TypeContext::default(),
            }
        };

        // When a dict literal is `|`'d with a TypedDict, infer the non-literal side first
        // so we can use bidirectional inference on the literal before calling the synthesized
        // `__or__`/`__ror__` method on the TypedDict side.
        if op == ast::Operator::BitOr && matches!(left, ast::Expr::Dict(_)) {
            let right_ty = self.infer_expression(right, operand_tcx(right));
            if let Type::TypedDict(typed_dict) = right_ty
                && let Some(ty) = self.try_typed_dict_pep_584_dunder(
                    left,
                    typed_dict.to_partial(db),
                    typed_dict,
                    "__ror__",
                )
            {
                return BinaryExpressionOperandTypes::TypedDictResult(ty);
            }

            // If the TypedDict update path rejects the literal, fall back to ordinary inference
            // even though that means re-inferring the literal without TypedDict context.
            return BinaryExpressionOperandTypes::Inferred(
                self.infer_expression(left, operand_tcx(left)),
                right_ty,
            );
        }

        let left_ty = self.infer_expression(left, operand_tcx(left));
        if op == ast::Operator::BitOr
            && let Type::TypedDict(typed_dict) = left_ty
            && matches!(right, ast::Expr::Dict(_))
            && let Some(ty) = self.try_typed_dict_pep_584_dunder(
                right,
                typed_dict.to_partial(db),
                typed_dict,
                "__or__",
            )
        {
            return BinaryExpressionOperandTypes::TypedDictResult(ty);
        }

        BinaryExpressionOperandTypes::Inferred(
            left_ty,
            self.infer_expression(right, operand_tcx(right)),
        )
    }

    fn try_typed_dict_pep_584_dunder(
        &mut self,
        update: &ast::Expr,
        update_context_typed_dict: TypedDictType<'db>,
        result_typed_dict: TypedDictType<'db>,
        dunder_name: &str,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let update_ty = self.speculate_without_diagnostics().infer_expression(
            update,
            TypeContext::new(Some(Type::TypedDict(update_context_typed_dict))),
        );
        let env = self.program_environment();

        Type::TypedDict(result_typed_dict)
            .try_call_dunder(
                db,
                env,
                dunder_name,
                CallArguments::positional([update_ty]),
                TypeContext::default(),
            )
            .ok()
            .map(|bindings| bindings.return_type(db, env))
    }

    /// Handle `TypedDict |= value` before the normal `__ior__` path runs.
    ///
    /// The normal path's bidirectional inference would emit spurious typed-dict diagnostics
    /// (e.g., `missing-typed-dict-key`, `invalid-key`) when the RHS doesn't exactly match
    /// the `TypedDict` schema. We probe here to decide the outcome without those side effects.
    ///
    /// Returns `Some` after handling either a compatible or incompatible operand.
    pub(super) fn try_infer_typed_dict_pep_584_augmented_assignment(
        &mut self,
        assignment: &ast::StmtAugAssign,
        target_type: Type<'db>,
        value_expr: &ast::Expr,
        infer_value_ty: &mut dyn FnMut(&mut Self, TypeContext<'db>) -> Type<'db>,
    ) -> Option<Type<'db>> {
        let db = self.db();
        if assignment.op != ast::Operator::BitOr {
            return None;
        }

        let Type::TypedDict(typed_dict) = target_type else {
            return None;
        };

        let typed_dict_ty = Type::TypedDict(typed_dict);

        // Prefer the full TypedDict as context when possible so exact-shape literals preserve the
        // named type in bidirectional inference.
        if self
            .try_typed_dict_pep_584_dunder(value_expr, typed_dict, typed_dict, "__ior__")
            .is_some()
        {
            infer_value_ty(self, TypeContext::new(Some(typed_dict_ty)));
            return Some(typed_dict_ty);
        }

        // Subset updates use the mutation-safe patch as context.
        let update_patch = typed_dict.to_update_patch(db);
        if self
            .try_typed_dict_pep_584_dunder(value_expr, update_patch, typed_dict, "__ior__")
            .is_some()
        {
            infer_value_ty(self, TypeContext::new(Some(Type::TypedDict(update_patch))));
            return Some(typed_dict_ty);
        }

        // The probe failed. Infer the RHS without TypedDict context so we report only the operator
        // failure, not spurious typed-dict diagnostics.
        let value_ty = infer_value_ty(self, TypeContext::default());
        report_unsupported_augmented_assignment(&self.context, assignment, target_type, value_ty);
        Some(target_type)
    }

    /// Maps an operation over each constraint of a constrained `TypeVar`.
    ///
    /// Returns the original `TypeVar` if each result is equivalent to its input constraint;
    /// otherwise returns the union of all results.
    pub(super) fn map_constrained_typevar_constraints(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar: Type<'db>,
        constraints: TypeVarConstraints<'db>,
        mut op: impl FnMut(Type<'db>) -> Option<Type<'db>>,
    ) -> Option<Type<'db>> {
        let mut builder = UnionBuilder::new(db, env);
        let mut any_different = false;

        for constraint in constraints.elements(db) {
            let result = op(*constraint)?;
            if !result.is_equivalent_to(db, env, *constraint) {
                any_different = true;
            }
            builder = builder.add(result);
        }

        Some(if any_different {
            builder.build()
        } else {
            typevar
        })
    }

    /// Collect deprecations from the selected operator methods while reusing cached resolution.
    fn infer_binary_dunder(
        &self,
        state: &mut BinaryInferenceState<'db>,
        left_ty: Type<'db>,
        op: ast::Operator,
        right_ty: Type<'db>,
        tcx: TypeContext<'db>,
    ) -> Option<Type<'db>> {
        let result = Type::try_call_bin_op_result_with_tcx(
            self.db(),
            self.program_environment(),
            left_ty,
            op,
            right_ty,
            tcx,
        )?;
        state
            .deprecated_functions
            .extend(&result.deprecated_functions);
        Some(result.return_type)
    }

    /// Infer the result type and collect deprecated methods for the enclosing operation.
    /// The caller reports them together after expanding union operands and in-place fallbacks.
    pub(super) fn infer_binary_expression_type(
        &mut self,
        node: AnyNodeRef<'_>,
        left_ty: Type<'db>,
        right_ty: Type<'db>,
        op: ast::Operator,
        tcx: TypeContext<'db>,
        state: &mut BinaryInferenceState<'db>,
    ) -> Option<Type<'db>> {
        self.infer_binary_expression_type_impl(
            node,
            left_ty,
            right_ty,
            op,
            &BinaryExpressionVisitor::new(Some(Type::Never)),
            tcx,
            state,
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn infer_binary_expression_type_impl(
        &mut self,
        node: AnyNodeRef<'_>,
        left_ty: Type<'db>,
        right_ty: Type<'db>,
        op: ast::Operator,
        visitor: &BinaryExpressionVisitor<'db>,
        tcx: TypeContext<'db>,
        state: &mut BinaryInferenceState<'db>,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        let db = self.db();

        // Check for division by zero; this doesn't change the inferred type for the expression, but
        // may emit a diagnostic
        if !state.emitted_division_by_zero_diagnostic
            && matches!(
                op,
                ast::Operator::Div | ast::Operator::FloorDiv | ast::Operator::Mod
            )
            && right_ty.as_literal_value().is_some_and(|literal| {
                literal.as_bool() == Some(false) || literal.as_int() == Some(0)
            })
        {
            state.emitted_division_by_zero_diagnostic =
                self.check_division_by_zero(node, op, left_ty);
        }

        match (left_ty, right_ty, op) {
            // parameter-only marker; behaves as the type a body sees (bound of `Key`)
            (Type::Overlapping(overlapping), _, _) => {
                visitor.visit(db, (left_ty, op, right_ty), || {
                    self.infer_binary_expression_type_impl(
                        node,
                        overlapping.value_type(db, env),
                        right_ty,
                        op,
                        visitor,
                        tcx,
                        state,
                    )
                })
            }
            // a use-site modifier says nothing about what the value can do
            (Type::Restricted(restricted), _, _) => {
                visitor.visit(db, (left_ty, op, right_ty), || {
                    self.infer_binary_expression_type_impl(
                        node,
                        restricted.value_type(db),
                        right_ty,
                        op,
                        visitor,
                        tcx,
                        state,
                    )
                })
            }
            (_, Type::Restricted(restricted), _) => {
                visitor.visit(db, (left_ty, op, right_ty), || {
                    self.infer_binary_expression_type_impl(
                        node,
                        left_ty,
                        restricted.value_type(db),
                        op,
                        visitor,
                        tcx,
                        state,
                    )
                })
            }
            (_, Type::Overlapping(overlapping), _) => {
                visitor.visit(db, (left_ty, op, right_ty), || {
                    self.infer_binary_expression_type_impl(
                        node,
                        left_ty,
                        overlapping.value_type(db, env),
                        op,
                        visitor,
                        tcx,
                        state,
                    )
                })
            }
            // basedpython: a hole nothing in the body bounded is the gradual type it replaced,
            // so an operation on it answers what that gradual type answered. resolving the
            // dunder against the hole instead lets the *other* operand decide the result —
            // `int * <hole>` reads as `int`, which `scale(3, 1.5)` disproves at runtime
            (left, right, _)
                if gradual_hole(db, env, left).is_some()
                    || gradual_hole(db, env, right).is_some() =>
            {
                visitor.visit(db, (left_ty, op, right_ty), || {
                    self.infer_binary_expression_type_impl(
                        node,
                        gradual_hole(db, env, left).unwrap_or(left),
                        gradual_hole(db, env, right).unwrap_or(right),
                        op,
                        visitor,
                        tcx,
                        state,
                    )
                })
            }

            // basedpython: arithmetic on a type parameter names a value that depends on the
            // specialization, so evaluating it through the bound's `__add__` would answer
            // `int` and throw that away. build the same symbolic operation the annotation
            // `-> I + 1` builds, so a body can be checked against its declared return type
            (left, right, op)
                if self.is_basedpython_file()
                    && DeferredOperation::Binary(op).is_checked_arithmetic()
                    && (is_symbolic_operand(left) || is_symbolic_operand(right))
                    && is_integer_operand(db, env, left)
                    && is_integer_operand(db, env, right) =>
            {
                Some(DeferredType::build(
                    db,
                    env,
                    &DeferredOperation::Binary(op),
                    Box::new([left, right]),
                ))
            }
            (Type::Deferred(deferred), _, _) => visitor.visit(db, (left_ty, op, right_ty), || {
                self.infer_binary_expression_type_impl(
                    node,
                    deferred.reduced(db, env),
                    right_ty,
                    op,
                    visitor,
                    tcx,
                    state,
                )
            }),
            (_, Type::Deferred(deferred), _) => visitor.visit(db, (left_ty, op, right_ty), || {
                self.infer_binary_expression_type_impl(
                    node,
                    left_ty,
                    deferred.reduced(db, env),
                    op,
                    visitor,
                    tcx,
                    state,
                )
            }),
            (Type::Union(lhs_union), rhs, _) => lhs_union.try_map(db, env, |lhs_element| {
                self.infer_binary_expression_type_impl(
                    node,
                    *lhs_element,
                    rhs,
                    op,
                    visitor,
                    tcx,
                    state,
                )
            }),
            (lhs, Type::Union(rhs_union), _) => rhs_union.try_map(db, env, |rhs_element| {
                self.infer_binary_expression_type_impl(
                    node,
                    lhs,
                    *rhs_element,
                    op,
                    visitor,
                    tcx,
                    state,
                )
            }),

            // Only the materializations that support the operator can be the one at hand, so
            // the results of those combine back into an unsafe union; the operator is
            // unsupported only if no materialization supports it.
            (Type::UnsafeUnion(lhs_unsafe_union), rhs, _) => {
                let results: Vec<_> = lhs_unsafe_union
                    .elements(db)
                    .iter()
                    .filter_map(|lhs_element| {
                        self.infer_binary_expression_type_impl(
                            node,
                            *lhs_element,
                            rhs,
                            op,
                            visitor,
                            tcx,
                            state,
                        )
                    })
                    .collect();
                (!results.is_empty())
                    .then(|| UnsafeUnionType::from_inferred_elements(db, env, results))
            }
            (lhs, Type::UnsafeUnion(rhs_unsafe_union), _) => {
                let results: Vec<_> = rhs_unsafe_union
                    .elements(db)
                    .iter()
                    .filter_map(|rhs_element| {
                        self.infer_binary_expression_type_impl(
                            node,
                            lhs,
                            *rhs_element,
                            op,
                            visitor,
                            tcx,
                            state,
                        )
                    })
                    .collect();
                (!results.is_empty())
                    .then(|| UnsafeUnionType::from_inferred_elements(db, env, results))
            }

            (Type::TypeAlias(alias), rhs, _) => visitor.visit(db, (left_ty, op, right_ty), || {
                self.infer_binary_expression_type_impl(
                    node,
                    alias.value_type(db),
                    rhs,
                    op,
                    visitor,
                    tcx,
                    state,
                )
            }),

            (lhs, Type::TypeAlias(alias), _) => visitor.visit(db, (left_ty, op, right_ty), || {
                self.infer_binary_expression_type_impl(
                    node,
                    lhs,
                    alias.value_type(db),
                    op,
                    visitor,
                    tcx,
                    state,
                )
            }),

            (Type::TypedDict(left_typed_dict), rhs, ast::Operator::BitOr)
                if rhs.is_assignable_to(db, env, Type::TypedDict(left_typed_dict)) =>
            {
                Some(Type::TypedDict(left_typed_dict))
            }

            (lhs, Type::TypedDict(right_typed_dict), ast::Operator::BitOr)
                if lhs.is_assignable_to(db, env, Type::TypedDict(right_typed_dict)) =>
            {
                Some(Type::TypedDict(right_typed_dict))
            }

            // Non-todo Anys take precedence over Todos (as if we fix this `Todo` in the future,
            // the result would then become Any or Unknown, respectively).
            (div @ Type::Divergent(_), _, _) | (_, div @ Type::Divergent(_), _) => Some(div),

            (unknown @ Type::Dynamic(DynamicType::AmbiguousOverload), _, _)
            | (_, unknown @ Type::Dynamic(DynamicType::AmbiguousOverload), _) => Some(unknown),

            (any @ Type::Dynamic(DynamicType::Any), _, _)
            | (_, any @ Type::Dynamic(DynamicType::Any), _) => Some(any),

            (unknown @ Type::Dynamic(DynamicType::Unknown), _, _)
            | (_, unknown @ Type::Dynamic(DynamicType::Unknown), _) => Some(unknown),

            (unknown @ Type::Dynamic(DynamicType::InvalidConcatenateUnknown), _, _)
            | (_, unknown @ Type::Dynamic(DynamicType::InvalidConcatenateUnknown), _) => {
                Some(unknown)
            }

            (unknown @ Type::Dynamic(DynamicType::UnknownGeneric(_)), _, _)
            | (_, unknown @ Type::Dynamic(DynamicType::UnknownGeneric(_)), _) => Some(unknown),

            (
                placeholder @ Type::Dynamic(
                    DynamicType::UnspecializedTypeVar | DynamicType::UnknownLambdaParameter,
                ),
                _,
                _,
            )
            | (
                _,
                placeholder @ Type::Dynamic(
                    DynamicType::UnspecializedTypeVar | DynamicType::UnknownLambdaParameter,
                ),
                _,
            ) => Some(placeholder),

            // When both operands are the same constrained TypeVar (e.g., `T: (int, str)`),
            // we check if the operation is valid for each constraint paired with itself.
            // This is different from treating it as a union, where we'd check all combinations.
            // For example, `T + T` where `T: (int, str)` should check `int + int` and `str + str`,
            // not `int + str` which would fail.
            //
            // If each constraint's operation returns the same type as the constraint (e.g.,
            // `int + int -> int`), we return the TypeVar to preserve the generic relationship.
            // Otherwise, we return the union of the return types.
            //
            // TODO: We expect to replace this with more general support for handling constrained TypeVars
            // in arbitrary method/function calls.
            (Type::TypeVar(left_tvar), Type::TypeVar(right_tvar), _)
                if left_tvar.identity(db) == right_tvar.identity(db) =>
            {
                match left_tvar.typevar(db).bound_or_constraints(db, env) {
                    Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                        Self::map_constrained_typevar_constraints(
                            db,
                            env,
                            left_ty,
                            constraints,
                            |constraint| {
                                self.infer_binary_expression_type(
                                    node, constraint, constraint, op, tcx, state,
                                )
                            },
                        )
                    }
                    // For bounded TypeVars or unconstrained TypeVars, fall through to the default handling.
                    _ => self.infer_binary_dunder(state, left_ty, op, right_ty, tcx),
                }
            }

            // When the left operand is a constrained TypeVar (e.g., `T: (int, float)`) and the
            // right operand is not a TypeVar, we check if each constraint supports the operation
            // with the right operand. For example, `T * 2` where `T: (int, float)` should check
            // `int * 2` and `float * 2`, both of which work.
            //
            // TODO: We expect to replace this with more general support once we migrate to the new
            // solver.
            (Type::TypeVar(left_tvar), rhs, _) if !rhs.is_type_var() => {
                match left_tvar.typevar(db).bound_or_constraints(db, env) {
                    Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                        Self::map_constrained_typevar_constraints(
                            db,
                            env,
                            left_ty,
                            constraints,
                            |constraint| {
                                self.infer_binary_expression_type_impl(
                                    node, constraint, rhs, op, visitor, tcx, state,
                                )
                            },
                        )
                    }
                    // For bounded TypeVars or unconstrained TypeVars, fall through to the default handling.
                    _ => self.infer_binary_dunder(state, left_ty, op, right_ty, tcx),
                }
            }

            // When the right operand is a constrained TypeVar and the left operand is not a TypeVar,
            // we check if each constraint supports the operation with the left operand.
            (lhs, Type::TypeVar(right_tvar), _) if !lhs.is_type_var() => {
                match right_tvar.typevar(db).bound_or_constraints(db, env) {
                    Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                        Self::map_constrained_typevar_constraints(
                            db,
                            env,
                            right_ty,
                            constraints,
                            |constraint| {
                                self.infer_binary_expression_type_impl(
                                    node, lhs, constraint, op, visitor, tcx, state,
                                )
                            },
                        )
                    }
                    // For bounded TypeVars or unconstrained TypeVars, fall through to the default handling.
                    _ => self.infer_binary_dunder(state, left_ty, op, right_ty, tcx),
                }
            }

            // `try_call_bin_op` works for almost all `NewType`s, but not for `NewType`s of `float`
            // and `complex`, where the concrete base type is a union. In that case it turns out
            // the `self` types of the dunder methods in typeshed don't match, because they don't
            // get the same `int | float` and `int | float | complex` special treatment that the
            // positional arguments get. In those cases we need to explicitly delegate to the base
            // type, so that it hits the `Type::Union` branches above.
            (Type::NewTypeInstance(newtype), rhs, _) => self
                .infer_binary_dunder(state, left_ty, op, right_ty, tcx)
                .or_else(|| {
                    self.infer_binary_expression_type_impl(
                        node,
                        newtype.concrete_base_type(db),
                        rhs,
                        op,
                        visitor,
                        tcx,
                        state,
                    )
                }),
            (lhs, Type::NewTypeInstance(newtype), _) => self
                .infer_binary_dunder(state, left_ty, op, right_ty, tcx)
                .or_else(|| {
                    self.infer_binary_expression_type_impl(
                        node,
                        lhs,
                        newtype.concrete_base_type(db),
                        op,
                        visitor,
                        tcx,
                        state,
                    )
                }),

            (todo @ Type::Dynamic(DynamicType::Todo(_)), _, _)
            | (_, todo @ Type::Dynamic(DynamicType::Todo(_)), _) => Some(todo),

            (Type::Never, _, _) | (_, Type::Never, _) => Some(Type::Never),

            (Type::LiteralValue(left), Type::LiteralValue(right), _) => {
                let recursively_defined = if left.recursively_defined().is_yes()
                    || right.recursively_defined().is_yes()
                {
                    RecursivelyDefined::Yes
                } else {
                    RecursivelyDefined::No
                };
                let result = literal_binary_op(
                    db,
                    env,
                    left_ty,
                    right_ty,
                    op,
                    self.is_basedpython_file(),
                    state,
                );

                result.map(|result| match result {
                    Type::LiteralValue(literal) => {
                        Type::LiteralValue(literal.with_recursively_defined(recursively_defined))
                    }
                    _ => result,
                })
            }

            (
                Type::KnownInstance(KnownInstanceType::ConstraintSet(left)),
                Type::KnownInstance(KnownInstanceType::ConstraintSet(right)),
                ast::Operator::BitAnd,
            ) => {
                let constraints = ConstraintSetBuilder::new();
                let result = constraints.into_owned(|constraints| {
                    let left = constraints.load(db, env, left.constraints(db));
                    let right = constraints.load(db, env, right.constraints(db));
                    left.and(db, constraints, || right)
                });
                Some(Type::KnownInstance(KnownInstanceType::ConstraintSet(
                    InternedConstraintSet::new(db, result),
                )))
            }

            (
                Type::KnownInstance(KnownInstanceType::ConstraintSet(left)),
                Type::KnownInstance(KnownInstanceType::ConstraintSet(right)),
                ast::Operator::BitOr,
            ) => {
                let constraints = ConstraintSetBuilder::new();
                let result = constraints.into_owned(|constraints| {
                    let left = constraints.load(db, env, left.constraints(db));
                    let right = constraints.load(db, env, right.constraints(db));
                    left.or(db, constraints, || right)
                });
                Some(Type::KnownInstance(KnownInstanceType::ConstraintSet(
                    InternedConstraintSet::new(db, result),
                )))
            }

            // PEP 604-style union types using the `|` operator.
            (
                Type::ClassLiteral(..)
                | Type::SubclassOf(..)
                | Type::GenericAlias(..)
                | Type::SpecialForm(_)
                | Type::KnownInstance(
                    KnownInstanceType::UnionType(_)
                    | KnownInstanceType::Literal(_)
                    | KnownInstanceType::Annotated(_)
                    | KnownInstanceType::TypeGenericAlias(_)
                    | KnownInstanceType::Callable(_)
                    | KnownInstanceType::TypeVar(_)
                    | KnownInstanceType::TypeAliasType(_)
                    | KnownInstanceType::NewType(_),
                ),
                Type::ClassLiteral(..)
                | Type::SubclassOf(..)
                | Type::GenericAlias(..)
                | Type::SpecialForm(_)
                | Type::KnownInstance(
                    KnownInstanceType::UnionType(_)
                    | KnownInstanceType::Literal(_)
                    | KnownInstanceType::Annotated(_)
                    | KnownInstanceType::TypeGenericAlias(_)
                    | KnownInstanceType::Callable(_)
                    | KnownInstanceType::TypeVar(_)
                    | KnownInstanceType::TypeAliasType(_)
                    | KnownInstanceType::NewType(_),
                ),
                ast::Operator::BitOr,
            ) => {
                if left_ty.is_equivalent_to(db, env, right_ty) {
                    Some(left_ty)
                } else {
                    Some(UnionTypeInstance::from_value_expression_types(
                        db,
                        [left_ty, right_ty],
                        self.scope(),
                        self.typevar_binding_context,
                        self.inference_flags(),
                    ))
                }
            }
            (
                Type::ClassLiteral(..)
                | Type::SubclassOf(..)
                | Type::GenericAlias(..)
                | Type::KnownInstance(..)
                | Type::SpecialForm(..),
                Type::NominalInstance(instance),
                ast::Operator::BitOr,
            )
            | (
                Type::NominalInstance(instance),
                Type::ClassLiteral(..)
                | Type::SubclassOf(..)
                | Type::GenericAlias(..)
                | Type::KnownInstance(..)
                | Type::SpecialForm(..),
                ast::Operator::BitOr,
            ) if instance.has_known_class(db, KnownClass::NoneType) => {
                Some(UnionTypeInstance::from_value_expression_types(
                    db,
                    [left_ty, right_ty],
                    self.scope(),
                    self.typevar_binding_context,
                    self.inference_flags(),
                ))
            }

            // We avoid calling `type.__(r)or__`, as typeshed annotates these methods as
            // accepting `Any` (since typeforms are inexpressable in the type system currently).
            // This means that many common errors would not be caught if we fell back to typeshed's stubs here.
            //
            // Note that if a class had a custom metaclass that overrode `__(r)or__`, we would also ignore
            // that custom method as we'd take one of the earlier branches.
            // This seems like it's probably rare enough that it's acceptable, however.
            (
                Type::ClassLiteral(..) | Type::GenericAlias(..) | Type::SubclassOf(..),
                _,
                ast::Operator::BitOr,
            )
            | (
                _,
                Type::ClassLiteral(..) | Type::GenericAlias(..) | Type::SubclassOf(..),
                ast::Operator::BitOr,
            ) => Type::try_call_bin_op_with_policy(
                db,
                env,
                left_ty,
                ast::Operator::BitOr,
                right_ty,
                TypeContext::default(),
                MemberLookupPolicy::META_CLASS_NO_TYPE_FALLBACK,
            )
            .ok()
            .map(|binding| {
                state.deprecated_functions.extend(
                    binding
                        .deprecated_functions(db)
                        .map(|(_, function)| function),
                );
                binding.return_type(db, env)
            }),

            // fold `(a, b) * n` (and `n * (a, b)`) into a fixed-length tuple with the
            // elements repeated `n` times, matching the runtime behaviour of
            // `tuple.__mul__`. without this, typeshed's stub widens the result to
            // `tuple[T, ...]`, discarding the exact element order and count
            (Type::NominalInstance(_), _, ast::Operator::Mult)
                if right_ty.as_int_like_literal().is_some() =>
            {
                fold_tuple_repeat(db, env, left_ty, right_ty)
                    .or_else(|| self.infer_binary_dunder(state, left_ty, op, right_ty, tcx))
            }
            (_, Type::NominalInstance(_), ast::Operator::Mult)
                if left_ty.as_int_like_literal().is_some() =>
            {
                fold_tuple_repeat(db, env, right_ty, left_ty)
                    .or_else(|| self.infer_binary_dunder(state, left_ty, op, right_ty, tcx))
            }

            // fold `(a, b) + (c,)` into `(a, b, c)`. as with `*`, typeshed's `tuple.__add__`
            // otherwise widens the concatenation to `tuple[T, ...]`
            (Type::NominalInstance(_), Type::NominalInstance(_), ast::Operator::Add) => {
                fold_tuple_concat(db, env, left_ty, right_ty)
                    .or_else(|| self.infer_binary_dunder(state, left_ty, op, right_ty, tcx))
            }

            // We've handled all of the special cases that we support for literals, so we need to
            // fall back on looking for dunder methods on one of the operand types.
            (
                Type::FunctionLiteral(_)
                | Type::Callable(..)
                | Type::BoundMethod(_)
                | Type::WrapperDescriptor(_)
                | Type::KnownBoundMethod(_)
                | Type::DataclassDecorator(_)
                | Type::DataclassTransformer(_)
                | Type::ModuleLiteral(_)
                | Type::ClassLiteral(_)
                | Type::GenericAlias(_)
                | Type::SubclassOf(_)
                | Type::NominalInstance(_)
                | Type::ProtocolInstance(_)
                | Type::SpecialForm(_)
                | Type::KnownInstance(_)
                | Type::PropertyInstance(_)
                | Type::SlotDescriptor(_)
                | Type::Intersection(_)
                | Type::EnumComplement(_)
                | Type::AlwaysTruthy
                | Type::AlwaysFalsy
                | Type::LiteralValue(_)
                | Type::BoundSuper(_)
                | Type::TypeVar(_)
                | Type::TypeIs(_)
                | Type::TypeGuard(_)
                | Type::TypeForm(_)
                | Type::TypedDict(_),
                Type::FunctionLiteral(_)
                | Type::Callable(..)
                | Type::BoundMethod(_)
                | Type::WrapperDescriptor(_)
                | Type::KnownBoundMethod(_)
                | Type::DataclassDecorator(_)
                | Type::DataclassTransformer(_)
                | Type::ModuleLiteral(_)
                | Type::ClassLiteral(_)
                | Type::GenericAlias(_)
                | Type::SubclassOf(_)
                | Type::NominalInstance(_)
                | Type::ProtocolInstance(_)
                | Type::SpecialForm(_)
                | Type::KnownInstance(_)
                | Type::PropertyInstance(_)
                | Type::SlotDescriptor(_)
                | Type::Intersection(_)
                | Type::EnumComplement(_)
                | Type::AlwaysTruthy
                | Type::AlwaysFalsy
                | Type::LiteralValue(_)
                | Type::BoundSuper(_)
                | Type::TypeVar(_)
                | Type::TypeIs(_)
                | Type::TypeGuard(_)
                | Type::TypeForm(_)
                | Type::TypedDict(_),
                op,
            ) => self.infer_binary_dunder(state, left_ty, op, right_ty, tcx),
        }
    }

    /// Raise a diagnostic if the given type cannot be divided by zero.
    ///
    /// Expects the resolved type of the left side of the binary expression.
    fn check_division_by_zero(
        &mut self,
        node: AnyNodeRef<'_>,
        op: ast::Operator,
        left: Type<'db>,
    ) -> bool {
        let db = self.db();
        match left {
            Type::LiteralValue(literal)
                if matches!(
                    literal.kind(),
                    LiteralValueTypeKind::Bool(_)
                        | LiteralValueTypeKind::Int(_)
                        | LiteralValueTypeKind::Float(_)
                ) => {}
            Type::NominalInstance(instance)
                if matches!(
                    instance.known_class(db),
                    Some(KnownClass::Float | KnownClass::Int | KnownClass::Bool)
                ) => {}
            _ => return false,
        }

        let (op, by_zero) = match op {
            ast::Operator::Div => ("divide", "by zero"),
            ast::Operator::FloorDiv => ("floor divide", "by zero"),
            ast::Operator::Mod => ("reduce", "modulo zero"),
            _ => return false,
        };

        if let Some(builder) = self.context.report_lint(&DIVISION_BY_ZERO, node) {
            builder.into_diagnostic(format_args!(
                "Cannot {op} object of type `{}` {by_zero}",
                left.display(db, self.program_environment())
            ));
        }

        true
    }
}

/// basedpython: extract an f64 value from a numeric-ish literal kind (bool/int/float)
pub(super) fn as_f64_value(kind: LiteralValueTypeKind<'_>) -> Option<f64> {
    match kind {
        LiteralValueTypeKind::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
        #[expect(clippy::cast_precision_loss)]
        LiteralValueTypeKind::Int(n) => Some(n.as_i64() as f64),
        LiteralValueTypeKind::Float(f) => Some(f.as_f64()),
        _ => None,
    }
}

/// basedpython: extract `(re, im)` from any numeric-ish literal kind (bool/int/float/complex)
fn as_complex_components<'db>(
    db: &'db dyn Db,
    kind: LiteralValueTypeKind<'db>,
) -> Option<(f64, f64)> {
    match kind {
        LiteralValueTypeKind::Complex(c) => Some((c.re(db), c.im(db))),
        _ => as_f64_value(kind).map(|v| (v, 0.0)),
    }
}

/// basedpython literal-arithmetic outcome
enum LiteralArithOutcome<'db> {
    /// A literal value was computed
    Literal(Type<'db>),
    /// Arithmetic is defined but the result is undefined at runtime (NaN, division by zero).
    /// Widen to the instance type rather than fall through to dunder dispatch, since the
    /// typeshed dunder may return `Any` (e.g. `float.__pow__(float) -> Any`)
    Widen,
    /// Op not supported on this type; fall through to dunder dispatch
    Unsupported,
}

/// basedpython: compute the result of a binary operation on two f64 values
fn float_binary_op_result<'db>(a: f64, b: f64, op: ast::Operator) -> LiteralArithOutcome<'db> {
    let result = match op {
        ast::Operator::Add => a + b,
        ast::Operator::Sub => a - b,
        ast::Operator::Mult => a * b,
        ast::Operator::Div => {
            if b == 0.0 {
                return LiteralArithOutcome::Widen;
            }
            a / b
        }
        ast::Operator::FloorDiv => {
            if b == 0.0 {
                return LiteralArithOutcome::Widen;
            }
            (a / b).floor()
        }
        ast::Operator::Mod => {
            if b == 0.0 {
                return LiteralArithOutcome::Widen;
            }
            a - (a / b).floor() * b
        }
        ast::Operator::Pow => {
            // Python promotes `(-1.0) ** 0.5` to a complex; f64 returns NaN. Widen to
            // `float` (= `int | float` in basedpython) rather than the typeshed dunder
            // which returns `Any` and erases all useful information
            let r = a.powf(b);
            if r.is_nan() {
                return LiteralArithOutcome::Widen;
            }
            r
        }
        _ => return LiteralArithOutcome::Unsupported,
    };
    if result.is_nan() {
        return LiteralArithOutcome::Widen;
    }
    LiteralArithOutcome::Literal(Type::float_literal(result))
}

/// basedpython: compute the result of a binary operation on two complex values.
/// Only Add/Sub/Mult/Div are supported; other operators fall through to the dunder
/// path so that `complex // complex` etc. surface as ordinary unsupported-op errors
fn complex_binary_op_result(
    db: &dyn Db,
    (a_re, a_im): (f64, f64),
    (b_re, b_im): (f64, f64),
    op: ast::Operator,
) -> LiteralArithOutcome<'_> {
    let (re, im) = match op {
        ast::Operator::Add => (a_re + b_re, a_im + b_im),
        ast::Operator::Sub => (a_re - b_re, a_im - b_im),
        ast::Operator::Mult => (a_re * b_re - a_im * b_im, a_re * b_im + a_im * b_re),
        ast::Operator::Div => {
            let denom = b_re * b_re + b_im * b_im;
            if denom == 0.0 {
                return LiteralArithOutcome::Widen;
            }
            (
                (a_re * b_re + a_im * b_im) / denom,
                (a_im * b_re - a_re * b_im) / denom,
            )
        }
        _ => return LiteralArithOutcome::Unsupported,
    };
    if re.is_nan() || im.is_nan() {
        return LiteralArithOutcome::Widen;
    }
    LiteralArithOutcome::Literal(Type::complex_literal(db, re, im))
}

/// Fold `tuple * n` into a fixed-length tuple whose elements are those of `tuple_ty`
/// repeated `n` times, where `multiplier` is a literal integer (or `bool`).
///
/// Returns `None` — leaving the caller to fall back on typeshed's `tuple.__mul__`, which
/// widens to `tuple[T, ...]` — when `tuple_ty` is not an exact fixed-length tuple, when
/// `multiplier` is not a literal integer, or when the repeated tuple would grow beyond
/// `MAX_LENGTH`. A non-positive multiplier folds to the empty tuple.
pub(crate) fn fold_tuple_repeat<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    tuple_ty: Type<'db>,
    multiplier: Type<'db>,
) -> Option<Type<'db>> {
    /// Repeating into a longer tuple discards the exact element types, so cap the work.
    const MAX_LENGTH: usize = 512;

    let factor = multiplier.as_int_like_literal()?;
    let spec = tuple_ty.exact_tuple_instance_spec(db)?;
    let Tuple::Fixed(fixed) = spec.as_ref() else {
        return None;
    };

    let elements = fixed.all_elements();
    let factor = usize::try_from(factor).unwrap_or(0);
    let new_length = elements.len().checked_mul(factor)?;
    if new_length > MAX_LENGTH {
        return None;
    }

    let mut repeated = Vec::with_capacity(new_length);
    for _ in 0..factor {
        repeated.extend_from_slice(elements);
    }
    Some(Type::heterogeneous_tuple(db, env, repeated))
}

/// Fold `left + right` into a single fixed-length tuple concatenating their elements.
///
/// Returns `None` — leaving the caller to fall back on typeshed's `tuple.__add__` — unless
/// both operands are exact fixed-length tuples.
pub(crate) fn fold_tuple_concat<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    left_ty: Type<'db>,
    right_ty: Type<'db>,
) -> Option<Type<'db>> {
    let left = left_ty.exact_tuple_instance_spec(db)?;
    let right = right_ty.exact_tuple_instance_spec(db)?;
    let (Tuple::Fixed(left), Tuple::Fixed(right)) = (left.as_ref(), right.as_ref()) else {
        return None;
    };
    Some(Type::heterogeneous_tuple(
        db,
        env,
        left.all_elements()
            .iter()
            .chain(right.all_elements())
            .copied(),
    ))
}

/// basedpython: fold a unary operation on a literal operand (`-3` → `Literal[-3]`,
/// `~0` → `Literal[-1]`). Returns `None` when the operand isn't a numeric literal or
/// the operator isn't `+`/`-`/`~`, so callers fall back to dunder dispatch. Shared
/// between value inference and the deferred type-operation path.
pub(crate) fn literal_unary_op<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    op: ast::UnaryOp,
    literal: crate::types::LiteralValueType<'db>,
) -> Option<Type<'db>> {
    match (op, literal.kind()) {
        (ast::UnaryOp::UAdd, LiteralValueTypeKind::Int(value)) => {
            Some(Type::int_literal(value.as_i64()))
        }
        (ast::UnaryOp::UAdd, LiteralValueTypeKind::Bool(value)) => {
            Some(Type::int_literal(i64::from(value)))
        }
        (ast::UnaryOp::USub, LiteralValueTypeKind::Int(value)) => Some(
            value
                .as_i64()
                .checked_neg()
                .map(Type::int_literal)
                .unwrap_or_else(|| KnownClass::Int.to_instance(db, env)),
        ),
        (ast::UnaryOp::USub, LiteralValueTypeKind::Bool(value)) => {
            Some(Type::int_literal(-i64::from(value)))
        }
        (ast::UnaryOp::USub, LiteralValueTypeKind::Float(value)) => {
            Some(Type::float_literal(-value.as_f64()))
        }
        (ast::UnaryOp::USub, LiteralValueTypeKind::Complex(c)) => {
            Some(Type::complex_literal(db, -c.re(db), -c.im(db)))
        }
        (ast::UnaryOp::Invert, LiteralValueTypeKind::Int(value)) => {
            Some(Type::int_literal(!value.as_i64()))
        }
        (ast::UnaryOp::Invert, LiteralValueTypeKind::Bool(value)) => {
            Some(Type::int_literal(!i64::from(value)))
        }
        _ => None,
    }
}

/// basedpython: evaluate a binary operation on two literal operands, reusing the
/// exact literal/type logic the value-expression inferrer uses (so `1 + 1` folds to
/// `Literal[2]`, `0.1 + 0.2` keeps IEEE-754 semantics, and so on). returns `None`
/// when neither operand is a literal or the operator isn't supported between them.
/// shared with the symbolic type-arithmetic path, where a specialized `Dim + 1`
/// re-evaluates to `Literal[6]`
pub(crate) fn literal_binary_op<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    left_ty: Type<'db>,
    right_ty: Type<'db>,
    op: ast::Operator,
    is_basedpython: bool,
    state: &mut BinaryInferenceState<'db>,
) -> Option<Type<'db>> {
    let (Type::LiteralValue(left), Type::LiteralValue(right)) = (left_ty, right_ty) else {
        return None;
    };
    match (left.kind(), right.kind(), op) {
        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::Add) => Some(
            n.as_i64()
                .checked_add(m.as_i64())
                .map(Type::int_literal)
                .unwrap_or_else(|| KnownClass::Int.to_instance(db, env)),
        ),

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::Sub) => Some(
            n.as_i64()
                .checked_sub(m.as_i64())
                .map(Type::int_literal)
                .unwrap_or_else(|| KnownClass::Int.to_instance(db, env)),
        ),

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::Mult) => Some(
            n.as_i64()
                .checked_mul(m.as_i64())
                .map(Type::int_literal)
                .unwrap_or_else(|| KnownClass::Int.to_instance(db, env)),
        ),

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::Div) => Some({
            // basedpython: int/int true division produces a float literal when the
            // divisor is non-zero. for `.py` files (or div-by-zero) fall back to the
            // float instance to preserve typeshed's `int|float` widening behaviour
            #[expect(clippy::cast_precision_loss)]
            let computed = if is_basedpython && m.as_i64() != 0 {
                Some(Type::float_literal(n.as_i64() as f64 / m.as_i64() as f64))
            } else {
                None
            };
            computed.unwrap_or_else(|| KnownClass::Float.to_instance(db, env))
        }),

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::FloorDiv) => {
            Some({
                let mut q = n.as_i64().checked_div(m.as_i64());
                let r = n.as_i64().checked_rem(m.as_i64());
                // Division works differently in Python than in Rust. If the result is negative and
                // there is a remainder, the division rounds down (instead of towards zero):
                if n.as_i64().is_negative() != m.as_i64().is_negative() && r.unwrap_or(0) != 0 {
                    q = q.map(|q| q - 1);
                }
                q.map(Type::int_literal)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(db, env))
            })
        }

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::Mod) => Some({
            let mut r = n.as_i64().checked_rem(m.as_i64());
            // Division works differently in Python than in Rust. If the result is negative and
            // there is a remainder, the division rounds down (instead of towards zero). Adjust
            // the remainder to compensate so that q * m + r == n:
            if n.as_i64().is_negative() != m.as_i64().is_negative() && r.unwrap_or(0) != 0 {
                r = r.map(|x| x + m.as_i64());
            }
            r.map(Type::int_literal)
                .unwrap_or_else(|| KnownClass::Int.to_instance(db, env))
        }),

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::Pow) => Some({
            if m.as_i64() < 0 {
                KnownClass::Float.to_instance(db, env)
            } else {
                u32::try_from(m.as_i64())
                    .ok()
                    .and_then(|m| n.as_i64().checked_pow(m))
                    .map(Type::int_literal)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(db, env))
            }
        }),

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::BitOr) => {
            Some(Type::int_literal(n.as_i64() | m.as_i64()))
        }

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::BitAnd) => {
            Some(Type::int_literal(n.as_i64() & m.as_i64()))
        }

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::BitXor) => {
            Some(Type::int_literal(n.as_i64() ^ m.as_i64()))
        }

        (
            LiteralValueTypeKind::Bytes(lhs),
            LiteralValueTypeKind::Bytes(rhs),
            ast::Operator::Add,
        ) => {
            let bytes = [lhs.value(db), rhs.value(db)].concat();
            Some(Type::bytes_literal(db, &bytes))
        }

        (
            LiteralValueTypeKind::String(lhs),
            LiteralValueTypeKind::String(rhs),
            ast::Operator::Add,
        ) => {
            let lhs_value = lhs.value(db);
            let rhs_value = rhs.value(db);
            let new_length = lhs_value.len() + rhs_value.len();
            let ty = if new_length <= TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE {
                let mut value = CompactString::with_capacity(new_length);
                value.push_str(lhs_value);
                value.push_str(rhs_value);
                Type::string_literal(db, value)
            } else {
                Type::literal_string()
            };
            Some(ty)
        }

        (
            LiteralValueTypeKind::String(_) | LiteralValueTypeKind::LiteralString,
            LiteralValueTypeKind::String(_) | LiteralValueTypeKind::LiteralString,
            ast::Operator::Add,
        ) => Some(Type::literal_string()),

        (LiteralValueTypeKind::String(s), LiteralValueTypeKind::Int(n), ast::Operator::Mult)
        | (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::String(s), ast::Operator::Mult) => {
            let ty = if n.as_i64() < 1 {
                Type::string_literal(db, "")
            } else if let Ok(n) = usize::try_from(n.as_i64())
                && let value = s.value(db)
                && n.checked_mul(value.len()).is_some_and(|new_length| {
                    new_length <= TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE
                })
            {
                let new_literal = value.repeat(n);
                Type::string_literal(db, &*new_literal)
            } else {
                Type::literal_string()
            };
            Some(ty)
        }

        (
            LiteralValueTypeKind::LiteralString,
            LiteralValueTypeKind::Int(n),
            ast::Operator::Mult,
        )
        | (
            LiteralValueTypeKind::Int(n),
            LiteralValueTypeKind::LiteralString,
            ast::Operator::Mult,
        ) => {
            let ty = if n.as_i64() < 1 {
                Type::string_literal(db, "")
            } else {
                Type::literal_string()
            };
            Some(ty)
        }

        (LiteralValueTypeKind::Bool(b1), LiteralValueTypeKind::Bool(b2), ast::Operator::BitOr) => {
            Some(Type::bool_literal(b1 | b2))
        }

        (LiteralValueTypeKind::Bool(b1), LiteralValueTypeKind::Bool(b2), ast::Operator::BitAnd) => {
            Some(Type::bool_literal(b1 & b2))
        }

        (LiteralValueTypeKind::Bool(b1), LiteralValueTypeKind::Bool(b2), ast::Operator::BitXor) => {
            Some(Type::bool_literal(b1 ^ b2))
        }

        (
            LiteralValueTypeKind::Bool(b1),
            LiteralValueTypeKind::Bool(_) | LiteralValueTypeKind::Int(_),
            op,
        ) => literal_binary_op(
            db,
            env,
            Type::int_literal(i64::from(b1)),
            right_ty,
            op,
            is_basedpython,
            state,
        ),

        (LiteralValueTypeKind::Int(_), LiteralValueTypeKind::Bool(b2), op) => literal_binary_op(
            db,
            env,
            left_ty,
            Type::int_literal(i64::from(b2)),
            op,
            is_basedpython,
            state,
        ),

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::LShift)
            if n.as_i64() == 0 && m.as_i64() >= 0 =>
        {
            Some(Type::int_literal(0))
        }

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::LShift) => {
            let n = n.as_i64();

            // An additional overflow check beyond `checked_shl` is necessary
            // here, because `checked_shl` only rejects shift amounts >= 64;
            // it does not detect when significant bits are shifted into (or
            // past) the sign bit. For example, `1i64.checked_shl(63)` returns
            // `Some(i64::MIN)`, but Python's `1 << 63` is a large positive int.
            //
            // We compute the "headroom": the number of redundant sign-extension
            // bits minus one (for the sign bit itself). A shift is safe iff
            // `m <= headroom`.
            let headroom = if n >= 0 {
                n.leading_zeros().saturating_sub(1)
            } else {
                n.leading_ones().saturating_sub(1)
            };
            Some(
                u32::try_from(m.as_i64())
                    .ok()
                    .filter(|&m| m <= headroom)
                    .and_then(|m| n.checked_shl(m))
                    .map(Type::int_literal)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(db, env)),
            )
        }

        (LiteralValueTypeKind::Int(n), LiteralValueTypeKind::Int(m), ast::Operator::RShift) => {
            let n = n.as_i64();
            let result = match u32::try_from(m.as_i64()) {
                Ok(m) => Type::int_literal(n >> m.clamp(0, 63)),
                Err(_) if m.as_i64() > 0 => Type::int_literal(if n >= 0 { 0 } else { -1 }),
                Err(_) => KnownClass::Int.to_instance(db, env),
            };
            Some(result)
        }

        (l, r, op) => {
            // basedpython: literal arithmetic on float/complex (and mixed numeric).
            // f64 arithmetic preserves IEEE 754 semantics — `0.1 + 0.2` becomes
            // `Literal[0.30000000000000004]`, not `Literal[0.3]`. Division by zero
            // and NaN results fall through to the dunder path so they don't surface
            // as `Literal[inf]` / `Literal[NaN]`
            let complex_involved = matches!(l, LiteralValueTypeKind::Complex(_))
                || matches!(r, LiteralValueTypeKind::Complex(_));
            let float_involved = matches!(l, LiteralValueTypeKind::Float(_))
                || matches!(r, LiteralValueTypeKind::Float(_));

            let outcome = if complex_involved {
                as_complex_components(db, l)
                    .zip(as_complex_components(db, r))
                    .map(|(a, b)| complex_binary_op_result(db, a, b, op))
            } else if float_involved {
                as_f64_value(l)
                    .zip(as_f64_value(r))
                    .map(|(a, b)| float_binary_op_result(a, b, op))
            } else {
                None
            };

            match outcome {
                Some(LiteralArithOutcome::Literal(ty)) => Some(ty),
                Some(LiteralArithOutcome::Widen) => {
                    // basedpython: widen to the typing-spec union form so the
                    // result mirrors how `float` / `complex` annotations are
                    // interpreted in `.py` files (`int | float`,
                    // `int | float | complex`). this gives callers something
                    // they can use without losing all type info.
                    //
                    // this does *not* consult `strict-float`, and an audit found no
                    // way to reach it from source with a result the union would be
                    // wrong for: the only producers are literal divisions by zero,
                    // which are diagnosed before the type is used, and the deferred
                    // path, which already passes `is_basedpython: true`
                    let widened = if complex_involved {
                        crate::types::set_theoretic::KnownUnion::Complex.to_type(db, env)
                    } else {
                        crate::types::set_theoretic::KnownUnion::Float.to_type(db, env)
                    };
                    Some(widened)
                }
                Some(LiteralArithOutcome::Unsupported) | None => {
                    let result = Type::try_call_bin_op_result(db, env, left_ty, op, right_ty)?;
                    state
                        .deprecated_functions
                        .extend(&result.deprecated_functions);
                    Some(result.return_type)
                }
            }
        }
    }
}
