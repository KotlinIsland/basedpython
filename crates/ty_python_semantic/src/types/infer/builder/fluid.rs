//! fluid specializations
//!
//! a binding like `a = [1]` or `a = A(1)` creates a generic instance whose
//! specialization was inferred rather than declared. as long as the type
//! checker can see every observer of the value, later uses of the binding may
//! refine the inferred specialization instead of being checked against it:
//!
//! ```py
//! a = [1]          # list[Literal[1]]
//! a[0]             # Literal[1] — reads don't change anything
//! a.append(2)      # no error — the specialization widens
//! a[0]             # Literal[1, 2]
//! b = a            # the value escapes: promote and lock — a and b are list[int]
//! ```
//!
//! the moment the value escapes to a context the checker can't analyze
//! (passed to a function whose parameter constrains the class typevars,
//! aliased to another name, stored in a container, ...), the specialization
//! is "locked": the escape's declared type context is adopted if there is
//! one, literals are promoted, and later incompatible uses are errors again
//!
//! this is flow-sensitive: each use of the binding solves the specialization
//! from the creation-time constraints plus the constraining events that can
//! have executed before the use. the binding's public type (seen by nested
//! scopes and post-lock uses) is the solution at the lock (or at the end of
//! the scope), so the narrowing is only ever a refinement of the public type
//!
//! the index-side tracking of candidate bindings and their classified uses
//! lives in `ty_python_core::fluid`

use itertools::Itertools;
use ruff_python_ast as ast;
use rustc_hash::FxHashSet;

use ty_python_core::Statement;
use ty_python_core::ast_ids::ExpressionNodeKey;
use ty_python_core::definition::{Definition, DefinitionNodeKey};
use ty_python_core::fluid::FluidUseRole;

use super::TypeInferenceBuilder;
use crate::types::any_over_type;
use crate::types::binding_type;
use crate::types::constraints::ConstraintSetBuilder;
use crate::types::generics::{GenericContext, SpecializationBuilder};
use crate::types::infer::{InferenceRegion, infer_expression_types, infer_statement_types};
use crate::types::infer_definition_types;
use crate::types::{KnownFunction, Type, TypeContext, TypeVarVariance};

/// the constraining and locking events of a fluid candidate binding that can
/// have executed before a given program point
pub(super) struct FluidConstraints<'db> {
    /// constraint types learned from widening uses, in flow order
    pub(super) constraints: Vec<Type<'db>>,
    /// whether the specialization is locked at this point
    pub(super) locked: bool,
    /// whether the lock promotes literal types: an escape promotes (the unknown
    /// observer sees the promoted type), while an adopting lock uses the observer's
    /// exact view, which is already part of `constraints`
    pub(super) promote_on_lock: bool,
}

impl FluidConstraints<'_> {
    /// whether the specialization at this point is exactly the creation-time
    /// specialization, with literals retained
    pub(super) fn is_creation(&self) -> bool {
        self.constraints.is_empty() && !self.locked
    }
}

impl<'db> TypeInferenceBuilder<'db, '_> {
    /// if the current region is the inference of a fluid candidate's assigned value —
    /// either the standalone inference of a collection literal, or the definition
    /// inference of a constructor-call assignment — returns the candidate definition
    pub(super) fn fluid_candidate_definition(
        &self,
        expr: ast::ExprRef<'_>,
    ) -> Option<Definition<'db>> {
        let candidate_def = match self.region {
            InferenceRegion::Expression(current_expr, _) => {
                if current_expr.node_ref(self.db()).index()
                    != *ruff_python_ast::HasNodeIndex::node_index(&expr)
                {
                    return None;
                }

                let assignment = current_expr.assigned_to(self.db())?;
                let candidate_def =
                    DefinitionNodeKey::from_assignment(assignment.node(self.module()))
                        .exactly_one()
                        .ok()?;
                self.index.try_definition(candidate_def)?
            }
            InferenceRegion::Definition(definition) => {
                let assignment = definition.kind(self.db()).as_unannotated_assignment()?;
                if ExpressionNodeKey::from(assignment.value(self.module()))
                    != ExpressionNodeKey::from(expr)
                {
                    return None;
                }
                definition
            }
            _ => return None,
        };

        self.is_fluid_candidate(candidate_def)
            .then_some(candidate_def)
    }

    /// whether this definition can have a fluid specialization: a single unannotated
    /// assignment whose place has no declared type (a declared place has a declared
    /// specialization, which is never fluid)
    pub(super) fn is_fluid_candidate(&self, candidate_def: Definition<'db>) -> bool {
        let db = self.db();

        if !self.fluid_specializations_enabled() {
            return false;
        }

        if candidate_def.kind(db).as_unannotated_assignment().is_none() {
            return false;
        }

        let use_def = self
            .index
            .use_def_map(candidate_def.scope(db).file_scope_id(db));
        !use_def
            .declarations_at_binding(candidate_def)
            .any(|declaration| declaration.declaration.definition().is_some())
    }

    /// infer a constructor call that may be the assigned value of a fluid candidate:
    /// the inferred specialization retains literal types, and constraints from later
    /// uses of the binding are combined with it. only direct constructor calls
    /// qualify — a call to a function that returns a generic instance hands the value
    /// to another observer before the binding even exists
    pub(super) fn infer_fluid_constructor_call(
        &mut self,
        call_expr: &ast::ExprCall,
        callable_type: Type<'db>,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let fluid_def = if tcx.annotation.is_none() && callable_type.is_class_literal() {
            self.fluid_candidate_definition(ast::ExprRef::Call(call_expr))
        } else {
            None
        };

        let tcx = if fluid_def.is_some() {
            TypeContext {
                preserve_literals: true,
                ..tcx
            }
        } else {
            tcx
        };

        let ty = self.infer_call_expression_impl(call_expr, callable_type, tcx);

        if let Some(fluid_def) = fluid_def
            && let Some((class_literal, _)) = ty.class_specialization(self.db())
            && let Some(generic_context) = class_literal.generic_context(self.db())
        {
            let identity_instance =
                Type::instance(self.db(), class_literal.identity_specialization(self.db()));
            return self.fluid_eventual_type(fluid_def, identity_instance, generic_context, ty);
        }

        ty
    }

    /// record adoption locks for fluid arguments of a call whose return type mentions
    /// the typevars solved from them: the returned value carries the argument's
    /// specialization, so capturing it creates a new observer
    /// (`xi = f(a)` with `def f[T](t: list[T]) -> list[T]` locks `a` to the solved
    /// parameter type)
    pub(super) fn record_fluid_return_observers(
        &mut self,
        arguments: &ast::Arguments,
        bindings: &mut crate::types::Bindings<'db>,
    ) {
        if !self.fluid_specializations_enabled() {
            return;
        }

        let db = self.db();

        for (argument_index, argument) in arguments.iter_source_order().enumerate() {
            let argument_value = match argument {
                ast::ArgOrKeyword::Arg(expr) => expr,
                ast::ArgOrKeyword::Keyword(keyword) => &keyword.value,
            };

            let Some(candidate_def) = self
                .index
                .fluid_candidate_binding(ExpressionNodeKey::from(argument_value))
            else {
                continue;
            };

            // If the statement discards the call's result, the returned observer does
            // not survive the call.
            if self
                .index
                .fluid_uses(candidate_def)
                .iter()
                .find(|use_| use_.use_expression == ExpressionNodeKey::from(argument_value))
                .is_some_and(|use_| use_.discarded_call_result)
            {
                continue;
            }

            for callable_binding in bindings.iter_flat_mut() {
                let receiver_offset = usize::from(callable_binding.bound_type.is_some());

                for (_, overload) in callable_binding.matching_overloads_mut() {
                    // Checker intrinsics observe without becoming observers.
                    if overload
                        .callable_type
                        .as_function_literal()
                        .and_then(|function| function.known(db))
                        .is_some_and(|known| {
                            matches!(known, KnownFunction::RevealType | KnownFunction::AssertType)
                        })
                    {
                        continue;
                    }

                    let Some(specialization) = overload.specialization(db) else {
                        continue;
                    };
                    let Some(matched) = overload
                        .argument_matches()
                        .get(argument_index + receiver_offset)
                    else {
                        continue;
                    };

                    let return_ty = overload.signature.return_ty;
                    let parameter_indices = matched.parameters.clone();

                    for matched_parameter in parameter_indices {
                        let parameters = overload.signature.parameters();
                        let Some(parameter) = parameters.get(matched_parameter.index) else {
                            continue;
                        };
                        let parameter_ty = parameter.annotated_type();

                        // The returned value observes the argument's specialization only
                        // if a typevar solved from this parameter occurs in the return
                        // type.
                        let shared_variance = std::cell::Cell::new(TypeVarVariance::Bivariant);
                        let shares_typevar = any_over_type(db, parameter_ty, false, |ty| {
                            ty.as_typevar().is_some_and(|typevar| {
                                let shared = std::cell::Cell::new(false);
                                return_ty.visit_specialization(db, |return_part, variance| {
                                    if return_part.as_typevar().is_some_and(|return_typevar| {
                                        return_typevar.identity(db) == typevar.identity(db)
                                    }) {
                                        shared.set(true);
                                        shared_variance.set(shared_variance.get().join(variance));
                                    }
                                });
                                shared.get()
                            })
                        });

                        if !shares_typevar {
                            continue;
                        }
                        let shared_variance = shared_variance.get();

                        // A returned observer that uses the typevars only covariantly
                        // never consumes from the value: its perspective stays valid
                        // under any future widening, as long as it is solved against
                        // the binding's eventual specialization. The binding stays
                        // fluid. This only applies to structured parameters — a bare
                        // typevar captures the whole instance invariantly.
                        if shared_variance == TypeVarVariance::Covariant
                            && parameter_ty.as_typevar().is_none()
                        {
                            if let Some(signature_context) = overload.signature.generic_context {
                                let eventual = binding_type(db, candidate_def);
                                let constraints = ConstraintSetBuilder::new();
                                let inferable = signature_context.inferable_typevars(db);
                                let mut builder =
                                    SpecializationBuilder::new(db, &constraints, inferable);
                                if builder.infer(parameter_ty, eventual).is_ok() {
                                    let eventual_specialization =
                                        builder.build_with(signature_context, |_, bounds| {
                                            let lower = bounds?.lower?;
                                            Some(lower)
                                        });
                                    overload.return_ty =
                                        return_ty.apply_specialization(db, eventual_specialization);
                                }
                            }
                            continue;
                        }

                        let adopted = parameter_ty.apply_specialization(db, specialization);
                        self.fluid_adoptions
                            .insert(ExpressionNodeKey::from(argument_value), adopted);
                    }
                }
            }
        }
    }

    /// gather the constraining and locking events of a fluid candidate that can have
    /// executed before the given use, or before the end of the scope if `upto` is `None`
    ///
    /// constraints learned after the first locking event are not included: the
    /// specialization can no longer change once unknown observers exist
    pub(super) fn gather_fluid_constraints(
        &self,
        candidate_def: Definition<'db>,
        identity_instance: Type<'db>,
        generic_context: GenericContext<'db>,
        upto: Option<ExpressionNodeKey>,
    ) -> FluidConstraints<'db> {
        let db = self.db();
        let uses = self.index.fluid_uses(candidate_def);

        let upto = upto.and_then(|key| uses.iter().find(|use_| use_.use_expression == key));

        let mut constraints = Vec::new();
        let mut locked = false;
        let mut promote_on_lock = false;
        // Constraints are tracked per statement; only read each statement once.
        let mut seen_statements: FxHashSet<Statement<'db>> = FxHashSet::default();

        for use_ in uses {
            if let Some(upto) = upto
                && !upto.may_follow(use_)
            {
                continue;
            }

            // Arguments are evaluated before a call mutates its receiver, so only
            // receiver-position uses observe the constraints recorded by their own
            // statement; every other use sees the pre-statement state. This also
            // breaks the self-reference of uses like `x.append(x)`, where the
            // argument's prefix would otherwise include the constraint formed from
            // the argument itself.
            if let Some(upto) = upto
                && !matches!(
                    upto.role,
                    FluidUseRole::MethodReceiver | FluidUseRole::SubscriptStore
                )
                && use_.statement_range == upto.statement_range
                && use_.role.contributes_constraints()
            {
                continue;
            }

            match use_.role {
                FluidUseRole::Read => {}

                FluidUseRole::Escape => {
                    locked = true;
                    promote_on_lock = true;
                    break;
                }

                FluidUseRole::MethodReceiver
                | FluidUseRole::SubscriptStore
                | FluidUseRole::TypeContextual => {
                    let Some(statement) = use_.statement else {
                        // Constraint-bearing roles always carry a statement; be
                        // conservative if one is somehow missing.
                        locked = true;
                        promote_on_lock = true;
                        break;
                    };

                    let statement_use_types = infer_statement_types(db, statement);

                    if let Some(divergent) = statement_use_types
                        .expression_type(use_.use_expression)
                        .as_divergent()
                    {
                        // Infer `C[Divergent]` for the initial cycle result.
                        let divergent_specialization =
                            generic_context.repeat_specialization(db, Type::Divergent(divergent));
                        constraints.push(
                            identity_instance.apply_specialization(db, divergent_specialization),
                        );
                        continue;
                    }

                    // A type-contextual use whose bidirectional context constrains the
                    // class typevars hands the value to an observer that relies on
                    // that specialization: adopt it and lock. A context blind to the
                    // typevars (e.g. `print(a)`, `len(a)`) leaves the binding fluid.
                    if use_.role == FluidUseRole::TypeContextual {
                        if let Some(adoption) =
                            statement_use_types.fluid_adoption(use_.use_expression)
                            && !adoption.has_unspecialized_type_var(db)
                            && self.fluid_constraint_binds_typevars(
                                identity_instance,
                                generic_context,
                                adoption,
                            )
                        {
                            constraints.push(adoption);
                            locked = true;
                            break;
                        }
                        continue;
                    }

                    let Some(use_constraints) =
                        statement_use_types.collection_use_constraints(candidate_def)
                    else {
                        // No constraints were learned at this use (e.g. a read-only
                        // method call); the binding stays fluid.
                        continue;
                    };

                    // A constraint is only a widening event if it actually binds the
                    // class typevars: a read-only method call (`a.pop()`) records an
                    // all-dynamic constraint, which must not promote the creation-time
                    // literals. Constraints are tracked per statement; only record
                    // each statement's constraints once.
                    if seen_statements.insert(statement) {
                        constraints.extend(
                            use_constraints
                                .iter()
                                .copied()
                                .filter(|constraint| {
                                    !constraint.has_unspecialized_type_var(db)
                                        && self.fluid_constraint_binds_typevars(
                                            identity_instance,
                                            generic_context,
                                            *constraint,
                                        )
                                })
                                .map(|constraint| {
                                    // A widening event inside a loop may execute any
                                    // number of times with different values, so its
                                    // literal types are promoted. This also keeps
                                    // self-feeding loops like
                                    // `for n in nums: nums.add(n + 1)` convergent.
                                    if use_.loops.is_empty() {
                                        constraint
                                    } else {
                                        self.solve_fluid_specialization(
                                            identity_instance,
                                            generic_context,
                                            std::iter::once(constraint),
                                            true,
                                        )
                                        .unwrap_or(constraint)
                                    }
                                }),
                        );
                    }
                }
            }
        }

        FluidConstraints {
            constraints,
            locked,
            promote_on_lock,
        }
    }

    /// whether solving the candidate's identity specialization against this
    /// constraint binds any of the class typevars to a static type. contexts
    /// that are blind to the class typevars (e.g. `object`, `Sized`) place no
    /// requirements on the specialization and so don't lock the binding
    fn fluid_constraint_binds_typevars(
        &self,
        identity_instance: Type<'db>,
        generic_context: GenericContext<'db>,
        constraint: Type<'db>,
    ) -> bool {
        let db = self.db();
        let constraints = ConstraintSetBuilder::new();
        let inferable = generic_context.inferable_typevars(db);
        let mut builder = SpecializationBuilder::new(db, &constraints, inferable);

        if builder.infer(identity_instance, constraint).is_err() {
            // An incompatible context still hands the value to another observer.
            return true;
        }

        let specialization = builder.build_with(generic_context, |_, bounds| {
            let lower = bounds?.lower?;
            Some(lower)
        });

        specialization
            .types(db)
            .iter()
            .any(|ty| !ty.is_dynamic() && !ty.is_never())
    }

    /// solve the candidate's specialization from the given constraint instances.
    /// literal types are promoted only once the specialization is locked
    pub(super) fn solve_fluid_specialization(
        &self,
        identity_instance: Type<'db>,
        generic_context: GenericContext<'db>,
        constraint_instances: impl IntoIterator<Item = Type<'db>>,
        promote: bool,
    ) -> Option<Type<'db>> {
        let db = self.db();
        let constraints = ConstraintSetBuilder::new();
        let inferable = generic_context.inferable_typevars(db);
        let mut builder = SpecializationBuilder::new(db, &constraints, inferable);

        for constraint in constraint_instances {
            builder.infer(identity_instance, constraint).ok()?;
        }

        let specialization = builder.build_with(generic_context, |_, bounds| {
            let lower = bounds?.lower?;
            Some(if promote {
                // Match the promotion policy of collection-literal inference: promote
                // literal types in invariant position, and promote singleton types to
                // `T | Unknown` (e.g. `[None]` is inferred as `list[None | Unknown]`).
                lower.promote(db).promote_singletons_recursively(db)
            } else {
                lower
            })
        });

        Some(identity_instance.apply_specialization(db, specialization))
    }

    /// the public ("eventual") type of a fluid candidate: the solution of the
    /// creation-time constraints plus every constraining event up to the first
    /// lock. also records the creation-time type so later uses can re-solve
    /// their own prefix of the events
    pub(super) fn fluid_eventual_type(
        &mut self,
        candidate_def: Definition<'db>,
        identity_instance: Type<'db>,
        generic_context: GenericContext<'db>,
        creation: Type<'db>,
    ) -> Type<'db> {
        self.fluid_creation = Some(creation);

        let gathered =
            self.gather_fluid_constraints(candidate_def, identity_instance, generic_context, None);

        // With no events and nothing to promote, keep the creation type as-is: a
        // re-solve can lose structure that the constructor inference produced (e.g.
        // the `Top[...]` materialization of a ParamSpec specialization).
        if gathered.is_creation()
            && !any_over_type(self.db(), creation, false, |ty| {
                ty.as_literal_value().is_some() || ty.is_singleton(self.db())
            })
        {
            // A fluid empty collection is `Never`-specialized, which is the precise type
            // for flow-sensitive uses (recorded above as `fluid_creation`). Its public
            // type — what untracked escapes such as multi-binding uses or other scopes
            // observe — must stay gradual, so present an empty collection as `Unknown`.
            return self.promote_empty_specialization(identity_instance, generic_context, creation);
        }

        // The eventual type is the binding's public type: it is what nested scopes,
        // other modules, and uses with non-unique bindings observe. Those observers
        // are not flow-sensitively tracked, so the public type is promoted — unless
        // an adopting lock pinned the observer's exact view.
        let promote = !gathered.locked || gathered.promote_on_lock;
        self.solve_fluid_specialization(
            identity_instance,
            generic_context,
            self.fluid_creation_constraint(identity_instance, generic_context, creation)
                .into_iter()
                .chain(gathered.constraints),
            promote,
        )
        .unwrap_or(creation)
    }

    /// If `creation` is an empty collection — every element typevar solved to `Never`,
    /// as an empty literal like `[]` does in fluid mode — return its gradual
    /// `Unknown`-specialized form, suitable as a public type. Otherwise return `creation`
    /// unchanged.
    fn promote_empty_specialization(
        &self,
        identity_instance: Type<'db>,
        generic_context: GenericContext<'db>,
        creation: Type<'db>,
    ) -> Type<'db> {
        let db = self.db();
        let Some((_, specialization)) = creation.class_specialization(db) else {
            return creation;
        };
        if !specialization.types(db).iter().all(Type::is_never) {
            return creation;
        }
        identity_instance.apply_specialization(
            db,
            generic_context.repeat_specialization(db, Type::unknown()),
        )
    }

    /// the creation-time type as a constraint for re-solving the specialization, or
    /// `None` if it binds nothing (e.g. `list[Unknown]` from an empty literal, which
    /// must not leak `Unknown` into the widened solution)
    fn fluid_creation_constraint(
        &self,
        identity_instance: Type<'db>,
        generic_context: GenericContext<'db>,
        creation: Type<'db>,
    ) -> Option<Type<'db>> {
        self.fluid_constraint_binds_typevars(identity_instance, generic_context, creation)
            .then_some(creation)
    }

    /// the flow-sensitive type of a use of a fluid candidate binding: the
    /// solution of the creation-time constraints plus the constraining events
    /// that can have executed before this use (including events from the use's
    /// own statement, so that e.g. the receiver of `a.append("a")` is typed
    /// with the widened specialization)
    pub(super) fn fluid_type_at_use(
        &self,
        candidate_def: Definition<'db>,
        use_expr: ast::ExprRef<'_>,
        fallback: Type<'db>,
        tcx: TypeContext<'db>,
    ) -> Type<'db> {
        let db = self.db();

        if !self.is_fluid_candidate(candidate_def) {
            return fallback;
        }

        let Some(assignment) = candidate_def.kind(db).as_unannotated_assignment() else {
            return fallback;
        };
        let value = assignment.value(self.module());

        // Collection-literal candidates store their creation type on the standalone
        // inference of the assigned value; constructor-call candidates store it on the
        // definition inference of the assignment.
        let creation = if let Some(expression) = self.index.try_expression(value) {
            infer_expression_types(db, expression, TypeContext::default()).fluid_creation()
        } else {
            infer_definition_types(db, candidate_def).fluid_creation()
        };
        let Some(creation) = creation else {
            return fallback;
        };

        let Some((class_literal, _)) = creation.class_specialization(db) else {
            // The creation type contains a cycle-recovery placeholder; fall back
            // until the fixpoint converges.
            return fallback;
        };
        let Some(generic_context) = class_literal.generic_context(db) else {
            return fallback;
        };
        let identity_instance = Type::instance(db, class_literal.identity_specialization(db));

        let mut gathered = self.gather_fluid_constraints(
            candidate_def,
            identity_instance,
            generic_context,
            Some(ExpressionNodeKey::from(use_expr)),
        );

        // A use with a bidirectional type context that constrains the specialization
        // adopts that context: this use is itself the locking event, and it observes
        // the adopted specialization. A parametric context (e.g. the `list[T]`
        // parameter of a generic function) adapts to any specialization and so never
        // locks one in — only a concrete declared type can share a perspective.
        // Parametric contexts solve their typevars from the argument, so the use
        // presents the promoted view of the specialization (what the binding would
        // eventually be), without locking it.
        if !gathered.locked
            && let Some(annotation) = tcx.annotation
        {
            // An unstructured typevar context (`def id[T](x: T)`, `reveal_type`)
            // places no requirement on the specialization and observes the narrow
            // type as-is; a structured one (`def f[T](t: list[T])`) solves its
            // typevars against the promoted view.
            if annotation.has_unspecialized_type_var(db)
                && annotation.class_specialization(db).is_some()
            {
                return self
                    .solve_fluid_specialization(
                        identity_instance,
                        generic_context,
                        self.fluid_creation_constraint(
                            identity_instance,
                            generic_context,
                            creation,
                        )
                        .into_iter()
                        .chain(gathered.constraints),
                        true,
                    )
                    .unwrap_or(fallback);
            }

            if self.fluid_constraint_binds_typevars(identity_instance, generic_context, annotation)
            {
                gathered.constraints.push(annotation);
                gathered.locked = true;
            }
        }

        if gathered.is_creation() {
            return creation;
        }

        self.solve_fluid_specialization(
            identity_instance,
            generic_context,
            self.fluid_creation_constraint(identity_instance, generic_context, creation)
                .into_iter()
                .chain(gathered.constraints),
            // Literal types accumulate through widening events and are promoted once
            // the specialization escapes; an adopting lock uses the observer's exact
            // view instead.
            gathered.locked && gathered.promote_on_lock,
        )
        .unwrap_or(fallback)
    }
}
