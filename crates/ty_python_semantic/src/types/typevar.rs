use crate::ProgramEnvironment;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use itertools::{Either, Itertools};
use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, PySourceType};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::{
    Db, FxOrderMap, TypeQualifiers,
    place::{
        DefinedPlace, Definedness, Place, PlaceAndQualifiers, Provenance, PublicTypePolicy,
        TypeOrigin,
    },
    types::{
        ApplySpecialization, ApplyTypeMappingVisitor, ClassLiteral, CycleDetector, DynamicType,
        GenericContext, InstanceProjection, IntersectionType, KnownClass, KnownInstanceType,
        LintDiagnosticGuard, MaterializationKind, Parameter, Parameters, Specialization, Type,
        TypeAliasType, TypeContext, TypeMapping, TypeVarVariance, UnionBuilder, UnionType,
        any_over_type, any_over_type_including_alias_arguments, binding_type,
        constraints::ConstraintSetBuilder,
        definition_expression_type,
        tuple::Tuple,
        variance::VarianceInferable,
        visitor::{
            self, TypeCollector, TypeVisitor, any_over_type_with_opaque_self, find_over_type,
            walk_type_with_recursion_guard,
        },
    },
};
use ty_python_core::{
    Program,
    definition::{Definition, DefinitionKind},
    scope::{NodeWithScopeKind, ScopeKind},
    semantic_index,
};

/// Which end of a type variable's own declaration stands in for it when a bound that names it has
/// to be read without a specialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeclaredEnd {
    /// The narrowest the variable can be — its declared lower bound, or `Never`.
    Floor,
    /// The widest the variable can be — its declared upper bound, or `object`.
    Ceiling,
}

impl<'db> Type<'db> {
    pub(crate) const fn is_type_var(self) -> bool {
        matches!(self, Type::TypeVar(_))
    }

    pub(crate) const fn as_typevar(self) -> Option<BoundTypeVarInstance<'db>> {
        match self {
            Type::TypeVar(bound_typevar) => Some(bound_typevar),
            _ => None,
        }
    }

    /// basedpython: whether this is the anonymous type parameter an unannotated parameter opens
    /// under `sound-types`, rather than a type anybody wrote.
    pub(crate) fn is_inferred_parameter_hole(self, db: &'db dyn Db) -> bool {
        matches!(
            self,
            Type::TypeVar(bound_typevar)
                if bound_typevar.typevar(db).kind(db) == TypeVarKind::InferredParameter
        )
    }

    /// This type with every type variable it names replaced by one end of that variable's own
    /// declaration.
    ///
    /// A bound may name another type parameter, and at the declaration nothing says what that
    /// parameter is. Reading each one at the end that makes the surrounding bound *widest* gives
    /// the most permissive type it can ever denote, so a check against it reports only what no
    /// specialization could rescue: an upper bound is widest at its ceiling, a lower bound at its
    /// floor. `def f[T, R: T..int]` is fine — `T` could be `bool` — while
    /// `class C[S = int, T: Sequence[S] = int]` has a default no `S` makes a `Sequence` of.
    ///
    /// `Self` is left alone: it is bound by the receiver, not by the list being declared.
    pub(crate) fn with_typevars_at_declared_end(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        end: DeclaredEnd,
    ) -> Type<'db> {
        /// A ceiling can name a type variable of its own, so the substitution repeats. Each round
        /// replaces a variable declared strictly earlier than the last, which the scope rule
        /// keeps acyclic — the cap is only there so a bound this rule never sanctioned cannot
        /// spin.
        const MAX_ROUNDS: usize = 8;

        let mut ty = self;
        for _ in 0..MAX_ROUNDS {
            let Some(bound_typevar) = find_over_type(db, env, ty, false, |ty| match ty {
                Type::TypeVar(bound_typevar) if !bound_typevar.typevar(db).is_self(db) => {
                    Some(bound_typevar)
                }
                _ => None,
            }) else {
                break;
            };
            let typevar = bound_typevar.typevar(db);
            let replacement = match end {
                DeclaredEnd::Ceiling => typevar.declared_ceiling(db, env),
                DeclaredEnd::Floor => typevar.lower_bound(db).unwrap_or(Type::Never),
            };
            ty = ty.apply_type_mapping(
                db,
                env,
                &TypeMapping::ApplySpecialization(ApplySpecialization::Single(
                    bound_typevar,
                    replacement,
                )),
                TypeContext::default(),
            );
        }
        ty
    }

    pub(crate) fn has_typevar(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> bool {
        any_over_type(db, env, self, false, |ty| matches!(ty, Type::TypeVar(_)))
    }

    /// basedpython: whether this parameter type spells out a generic class's type argument
    /// instead of naming it with a type variable.
    ///
    /// `items: list[T]` names it: `T` becomes whatever the argument's element type is, so
    /// solving `T` reports what the argument holds. `container: Wrapper[Callable[Concatenate[object, P], R]]`
    /// spells it out: the type argument has to be a callable of that shape, and the solved
    /// `P` and `R` describe the parameter's own demand rather than the argument.
    ///
    /// A fluid specialization may adopt the first — the call is telling the binding what it
    /// holds — but adopting the second would hand the argument the very type the parameter
    /// asked for, so an invariant container would accept a type argument that does not match
    /// it and nothing would report the mismatch.
    pub(crate) fn prescribes_type_arguments(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        let Some((_, specialization)) = self.class_specialization(db, env) else {
            return false;
        };
        specialization
            .types(db)
            .iter()
            .any(|argument| !argument.is_type_var() && argument.has_typevar(db, env))
    }

    pub(crate) fn references_typevar(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar_id: TypeVarIdentity<'db>,
    ) -> bool {
        any_over_type(db, env, self, false, |ty| match ty {
            Type::TypeVar(bound_typevar) => typevar_id == bound_typevar.typevar(db).identity(db),
            Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) => {
                typevar_id == typevar.identity(db)
            }
            _ => false,
        })
    }

    /// Returns whether this type might reference `typevar_id`, including type-alias arguments.
    ///
    /// Other non-lazy type-variable visitors stop at type aliases because inspecting an alias's
    /// value can trigger lazy inference or expand a recursive definition. Receiver specialization
    /// still needs to notice `T` in `Alias[T]`, so this visitor inspects the already-available
    /// specialization arguments without evaluating the alias body.
    ///
    /// This deliberately over-approximates: `type Alias[T] = int` does not actually depend on
    /// `T`, and specialization can also erase an argument. That can cause an unnecessary
    /// receiver-specialization attempt, but actual receiver constraints are still solved before
    /// changing the signature. Applying the same traversal to visitors that use type-variable
    /// occurrences to drive inference or diagnostics can instead change behavior.
    ///
    /// TODO: Explore whether other type-variable visitors can safely inspect alias arguments,
    /// accounting for unused parameters and arguments erased by specialization.
    pub(crate) fn references_typevar_through_aliases(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar_id: TypeVarIdentity<'db>,
    ) -> bool {
        any_over_type_including_alias_arguments(db, env, self, |ty| match ty {
            Type::TypeVar(typevar) => typevar_id == typevar.typevar(db).identity(db),
            Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) => {
                typevar_id == typevar.identity(db)
            }
            _ => false,
        })
    }

    pub(crate) fn has_non_self_typevar(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        any_over_type(
            db,
            env,
            self,
            false,
            |ty| matches!(ty, Type::TypeVar(tv) if !tv.typevar(db).is_self(db)),
        )
    }

    pub(crate) fn has_typevar_or_typevar_instance(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        any_over_type(db, env, self, false, |ty| {
            matches!(
                ty,
                Type::KnownInstance(KnownInstanceType::TypeVar(_)) | Type::TypeVar(_)
            )
        })
    }

    /// Like [`Self::has_typevar_or_typevar_instance`], but ignores `Self`.
    ///
    /// `Self` is bound by the enclosing class rather than by the generic context currently being
    /// defined, so a type mentioning only `Self` leaves nothing unsolved in that context.
    pub(crate) fn has_non_self_typevar_or_typevar_instance(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        any_over_type_with_opaque_self(db, env, self, |ty| match ty {
            Type::TypeVar(bound_typevar) => !bound_typevar.typevar(db).is_self(db),
            Type::KnownInstance(KnownInstanceType::TypeVar(_)) => true,
            _ => false,
        })
    }

    /// Whether this is a type variable that can only ever be solved to a `TypedDict`.
    ///
    /// Such a type variable is a stand-in for an as-yet-unknown `TypedDict`, so a construct that
    /// requires one (`**kwargs: Unpack[T]`) can accept it and defer until it is solved.
    pub(crate) fn is_typed_dict_bounded_typevar(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        let Type::TypeVar(bound_typevar) = self else {
            return false;
        };
        match bound_typevar.typevar(db).bound_or_constraints(db, env) {
            Some(TypeVarBoundOrConstraints::UpperBound(bound)) => {
                bound.resolve_type_alias(db).is_typed_dict()
            }
            Some(TypeVarBoundOrConstraints::Constraints(constraints)) => constraints
                .elements(db)
                .iter()
                .all(|constraint| constraint.resolve_type_alias(db).is_typed_dict()),
            None => false,
        }
    }

    pub(crate) fn has_unspecialized_type_var(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        any_over_type(db, env, self, false, |ty| {
            matches!(ty, Type::Dynamic(DynamicType::UnspecializedTypeVar))
        })
    }

    pub(crate) fn has_provisional_marker(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        any_over_type(db, env, self, false, |ty| {
            ty.as_dynamic()
                .is_some_and(DynamicType::is_provisional_marker)
        })
    }
}

/// A specific instance of a type variable that has not been bound to a generic context yet.
///
/// This is usually not the type that you want; if you are working with a typevar, in a generic
/// context, which might be specialized to a concrete type, you want [`BoundTypeVarInstance`]. This
/// type holds information that does not depend on which generic context the typevar is used in.
///
/// For a legacy typevar:
///
/// ```py
/// T = TypeVar("T")                       # [1]
/// def generic_function(t: T) -> T: ...   # [2]
/// ```
///
/// we will create a `TypeVarInstance` for the typevar `T` when it is instantiated. The type of `T`
/// at `[1]` will be a `KnownInstanceType::TypeVar` wrapping this `TypeVarInstance`. The typevar is
/// not yet bound to any generic context at this point.
///
/// The typevar is used in `generic_function`, which binds it to a new generic context. We will
/// create a [`BoundTypeVarInstance`] for this new binding of the typevar. The type of `T` at `[2]`
/// will be a `Type::TypeVar` wrapping this `BoundTypeVarInstance`.
///
/// For a PEP 695 typevar:
///
/// ```py
/// def generic_function[T](t: T) -> T: ...
/// #                          ╰─────╰─────────── [2]
/// #                    ╰─────────────────────── [1]
/// ```
///
/// the typevar is defined and immediately bound to a single generic context. Just like in the
/// legacy case, we will create a `TypeVarInstance` and [`BoundTypeVarInstance`], and the type of
/// `T` at `[1]` and `[2]` will be that `TypeVarInstance` and `BoundTypeVarInstance`, respectively.
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct TypeVarInstance<'db> {
    /// The identity of this typevar
    #[returns(copy)]
    pub(crate) identity: TypeVarIdentity<'db>,

    /// The upper bound or constraint on the type of this TypeVar, if any. Don't use this field
    /// directly; use the `bound_or_constraints` (or `upper_bound` and `constraints`) methods
    /// instead (to evaluate any lazy bound or constraints).
    #[returns(copy)]
    _bound_or_constraints: Option<TypeVarBoundOrConstraintsEvaluation<'db>>,

    /// basedpython: the lower bound of a bound range `T: Lower..Upper`, if any. A lower bound
    /// only ever accompanies an upper bound, since a range requires both ends. Don't use this
    /// field directly; use the `lower_bound` method instead (to evaluate any lazy bound).
    #[returns(copy)]
    _lower_bound: Option<TypeVarLowerBoundEvaluation<'db>>,

    /// The explicitly specified variance of the TypeVar
    #[returns(copy)]
    pub(super) explicit_variance: Option<TypeVarVariance>,

    /// The default type for this TypeVar, if any. Don't use this field directly, use the
    /// `default_type` method instead (to evaluate any lazy default).
    #[returns(copy)]
    _default: Option<TypeVarDefaultEvaluation<'db>>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for TypeVarInstance<'_> {}

pub(super) fn walk_type_var_type<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    typevar: TypeVarInstance<'db>,
    visitor: &V,
) {
    if let Some(bound_or_constraints) = if visitor.should_visit_lazy_type_attributes() {
        typevar.bound_or_constraints(db, visitor.program_environment())
    } else {
        match typevar._bound_or_constraints(db) {
            Some(TypeVarBoundOrConstraintsEvaluation::Eager(bound_or_constraints)) => {
                Some(bound_or_constraints)
            }
            Some(
                TypeVarBoundOrConstraintsEvaluation::LazyUpperBound
                | TypeVarBoundOrConstraintsEvaluation::LazyConstraints,
            ) => {
                visitor.notify_skipped_lazy_type_attributes();
                None
            }
            _ => None,
        }
    } {
        walk_type_var_bounds(db, bound_or_constraints, visitor);
    }
    if let Some(lower_bound) = if visitor.should_visit_lazy_type_attributes() {
        typevar.lower_bound(db)
    } else {
        match typevar._lower_bound(db) {
            Some(TypeVarLowerBoundEvaluation::Eager(lower_bound)) => Some(lower_bound),
            _ => None,
        }
    } {
        visitor.visit_type(db, lower_bound);
    }
    if let Some(default_type) = if visitor.should_visit_lazy_type_attributes() {
        typevar.default_type(db, visitor.program_environment())
    } else {
        match typevar._default(db) {
            Some(TypeVarDefaultEvaluation::Eager(default_type)) => Some(default_type),
            Some(TypeVarDefaultEvaluation::Lazy) => {
                visitor.notify_skipped_lazy_type_attributes();
                None
            }
            _ => None,
        }
    } {
        visitor.visit_type(db, default_type);
    }
}

#[salsa::tracked]
impl<'db> TypeVarInstance<'db> {
    pub(crate) fn with_binding_context(
        self,
        db: &'db dyn Db,
        binding_context: Definition<'db>,
    ) -> BoundTypeVarInstance<'db> {
        BoundTypeVarInstance::new(
            db,
            self,
            BindingContext::Definition(binding_context),
            None,
            TypeVarNonce::NONE,
        )
    }

    fn with_name_suffix(self, db: &'db dyn Db, suffix: &str) -> Self {
        Self::new(
            db,
            self.identity(db).with_name_suffix(db, suffix),
            self._bound_or_constraints(db),
            self._lower_bound(db),
            self.explicit_variance(db),
            self._default(db),
        )
    }

    pub(super) fn with_identity(self, db: &'db dyn Db, identity: TypeVarIdentity<'db>) -> Self {
        Self::new(
            db,
            identity,
            self._bound_or_constraints(db),
            self._lower_bound(db),
            self.explicit_variance(db),
            self._default(db),
        )
    }

    pub(crate) fn name(self, db: &'db dyn Db) -> &'db Name {
        self.identity(db).name(db)
    }

    pub(crate) fn definition(self, db: &'db dyn Db) -> Option<Definition<'db>> {
        self.identity(db).definition(db)
    }

    pub fn kind(self, db: &'db dyn Db) -> TypeVarKind {
        self.identity(db).kind(db)
    }

    pub(crate) fn is_self(self, db: &'db dyn Db) -> bool {
        matches!(self.kind(db), TypeVarKind::TypingSelf)
    }

    pub(crate) fn is_paramspec(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_paramspec()
    }

    pub(crate) fn is_keyword_variadic(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_keyword_variadic()
    }

    pub(crate) fn is_parameter_pack(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_parameter_pack()
    }

    pub(crate) fn is_typevartuple(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_typevartuple()
    }

    /// basedpython: whether this type variable carries a *pack* bound, which the generic solver
    /// must leave alone.
    ///
    /// A variadic pack's value is a tuple (`*Ts`) or a parameter list (`**Kwargs`), and its bound
    /// never describes that value: an unstarred bound describes each member and a starred one the
    /// pack's shape. Applying either as an ordinary upper bound would compare a tuple against an
    /// element type — and, in a contravariant position, intersect the two into the solution. The
    /// bound is checked where the pack is specialized instead.
    ///
    /// [`bound_or_constraints`](Self::bound_or_constraints) therefore hides it, and this is the
    /// only way to reach it.
    fn pack_bound(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Option<Type<'db>> {
        if !self.is_pack(db) {
            return None;
        }
        // a pack's bound never reaches the constraint set, so a bound naming another type
        // parameter has nowhere to record the relation it describes. it is reported where it is
        // written and dropped here, rather than silently checking nothing
        if self.bound_mentions_typevars(db) {
            return None;
        }
        match self._bound_or_constraints(db)? {
            TypeVarBoundOrConstraintsEvaluation::Eager(TypeVarBoundOrConstraints::UpperBound(
                bound,
            )) => Some(bound),
            TypeVarBoundOrConstraintsEvaluation::LazyUpperBound => self.lazy_bound(db, env),
            TypeVarBoundOrConstraintsEvaluation::Eager(TypeVarBoundOrConstraints::Constraints(
                _,
            ))
            | TypeVarBoundOrConstraintsEvaluation::LazyConstraints => None,
        }
    }

    pub(crate) fn has_pack_bound(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> bool {
        self.pack_bound(db, env).is_some()
    }

    /// Whether this type variable stands for a run of types (`*Ts`) or a field mapping
    /// (`**Kwargs`) rather than for a single type.
    fn is_pack(self, db: &'db dyn Db) -> bool {
        self.is_typevartuple(db) || self.is_keyword_variadic(db)
    }

    /// Whether either end of this type variable's bound names another type variable.
    ///
    /// Such a bound is a relation between two variables, and the constraint set is what expresses
    /// one — so the paths that treat a declared bound as a concrete type have to leave it alone
    /// and let [`SpecializationBuilder`](crate::types::generics::SpecializationBuilder) conjoin
    /// it instead. `Self` does not count: it is bound by the receiver, not by the generic context
    /// being solved.
    #[salsa::tracked(returns(copy), cycle_result=|_, _, _| false, heap_size=ruff_memory_usage::heap_size)]
    pub(crate) fn bound_mentions_typevars(self, db: &'db dyn Db) -> bool {
        // an eagerly-unbounded type variable is the common case and must not force anything
        if self._bound_or_constraints(db).is_none() && self._lower_bound(db).is_none() {
            return false;
        }
        let Some(definition) = self.definition(db) else {
            return false;
        };
        let env = ProgramEnvironment::from_definition(definition);
        let mentions = |ty: Type<'db>| ty.has_non_self_typevar_or_typevar_instance(db, &env);
        // read the bound directly rather than through `bound_or_constraints`, which hides a
        // variadic pack's — a pack bound is one of the callers that needs this answer
        let upper = match self._bound_or_constraints(db) {
            Some(TypeVarBoundOrConstraintsEvaluation::Eager(
                TypeVarBoundOrConstraints::UpperBound(bound),
            )) => Some(bound),
            Some(TypeVarBoundOrConstraintsEvaluation::LazyUpperBound) => self.lazy_bound(db, &env),
            // constraints naming a type variable are rejected outright, so there is never one
            // here to hide from the paths below
            _ => None,
        };
        upper.is_some_and(mentions) || self.lower_bound(db).is_some_and(mentions)
    }

    pub(crate) fn upper_bound(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<Type<'db>> {
        if let Some(TypeVarBoundOrConstraints::UpperBound(ty)) = self.bound_or_constraints(db, env)
        {
            Some(ty)
        } else {
            None
        }
    }

    /// Returns whether this type variable has constraints without evaluating a lazy bound.
    pub(super) fn is_constrained(self, db: &'db dyn Db) -> bool {
        matches!(
            self._bound_or_constraints(db),
            Some(
                TypeVarBoundOrConstraintsEvaluation::Eager(TypeVarBoundOrConstraints::Constraints(
                    _
                )) | TypeVarBoundOrConstraintsEvaluation::LazyConstraints
            )
        )
    }

    pub(crate) fn constraints(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<&'db [Type<'db>]> {
        if let Some(TypeVarBoundOrConstraints::Constraints(tuple)) =
            self.bound_or_constraints(db, env)
        {
            Some(tuple.elements(db))
        } else {
            None
        }
    }

    /// The declared ceiling on this type variable, as a type rather than as a name.
    ///
    /// A bound may be another type parameter (`def f[T, R: T]`), and a name is no ceiling at all
    /// to a caller that wants to measure a value against one — so the chain is followed until it
    /// reaches something that is not a type variable. An unbounded parameter is capped by
    /// `object`, and a constrained one by the union of its constraints.
    ///
    /// The scope rule makes the chain acyclic: a bound may only name a parameter declared before
    /// it, and one that breaks that rule is dropped rather than installed.
    pub(crate) fn declared_ceiling(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Type<'db> {
        let mut typevar = self;
        loop {
            let ceiling = match typevar.bound_or_constraints(db, env) {
                Some(bound_or_constraints) => bound_or_constraints.as_type(db, env),
                None => return Type::object(),
            };
            match ceiling {
                Type::TypeVar(named) => typevar = named.typevar(db),
                ceiling => return ceiling,
            }
        }
    }

    pub(crate) fn bound_or_constraints(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<TypeVarBoundOrConstraints<'db>> {
        // basedpython: a variadic pack's bound is never an upper bound on the pack's own value,
        // so it is kept out of the type lattice entirely — reach it through
        // [`pack_bound`](Self::pack_bound) instead
        if self.is_pack(db) {
            return None;
        }
        self._bound_or_constraints(db).and_then(|w| match w {
            TypeVarBoundOrConstraintsEvaluation::Eager(bound_or_constraints) => {
                Some(bound_or_constraints)
            }
            TypeVarBoundOrConstraintsEvaluation::LazyUpperBound => self
                .lazy_bound(db, env)
                .map(TypeVarBoundOrConstraints::UpperBound),
            TypeVarBoundOrConstraintsEvaluation::LazyConstraints => self
                .lazy_constraints(db, env)
                .map(TypeVarBoundOrConstraints::Constraints),
        })
    }

    /// basedpython: returns the lower bound of this typevar, if it was declared with a bound
    /// range `T: Lower..Upper`.
    pub(crate) fn lower_bound(self, db: &'db dyn Db) -> Option<Type<'db>> {
        match self._lower_bound(db)? {
            TypeVarLowerBoundEvaluation::Eager(ty) => Some(ty),
            TypeVarLowerBoundEvaluation::Lazy => self.lazy_lower_bound(db),
        }
    }

    #[salsa::tracked(
        returns(copy),
        cycle_fn=lazy_lower_bound_cycle_recover,
        cycle_initial=|_, _, _| None,
        heap_size=ruff_memory_usage::heap_size
    )]
    fn lazy_lower_bound(self, db: &'db dyn Db) -> Option<Type<'db>> {
        let definition = self.definition(db)?;
        let module = parsed_module(db, definition.program_file(db).python_file(db)).load(db);
        let DefinitionKind::TypeVar(typevar) = definition.kind(db) else {
            return None;
        };
        let lower =
            definition_expression_type(db, definition, typevar.node(&module).lower_bound.as_ref()?);

        // the lower end follows the same scope rule as the upper one, and is dropped for the
        // same reason when it breaks it
        if bound_scope_violation(db, self, lower).is_some() {
            return None;
        }

        Some(lower)
    }

    /// Returns the bounds or constraints of this typevar. If the typevar is unbounded, returns
    /// `object` as its upper bound.
    pub(crate) fn require_bound_or_constraints(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> TypeVarBoundOrConstraints<'db> {
        self.bound_or_constraints(db, env)
            .unwrap_or_else(|| TypeVarBoundOrConstraints::UpperBound(Type::object()))
    }

    pub(crate) fn default_type(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<Type<'db>> {
        let visitor = TypeVarDefaultVisitor::new(None);
        self.default_type_impl(db, env, &visitor)
    }

    fn default_type_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        visitor: &TypeVarDefaultVisitor<'db>,
    ) -> Option<Type<'db>> {
        visitor.visit(db, self, || {
            self._default(db).and_then(|default| match default {
                TypeVarDefaultEvaluation::Eager(ty) => Some(ty),
                TypeVarDefaultEvaluation::Lazy => self.lazy_default_impl(db, env, visitor),
            })
        })
    }

    fn materialize_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        materialization_kind: MaterializationKind,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Self {
        Self::new(
            db,
            self.identity(db),
            self._bound_or_constraints(db)
                .and_then(|bound_or_constraints| match bound_or_constraints {
                    TypeVarBoundOrConstraintsEvaluation::Eager(bound_or_constraints) => Some(
                        bound_or_constraints
                            .materialize_impl(db, env, materialization_kind, visitor)
                            .into(),
                    ),
                    TypeVarBoundOrConstraintsEvaluation::LazyUpperBound => {
                        self.lazy_bound(db, visitor.env).map(|bound| {
                            TypeVarBoundOrConstraints::UpperBound(bound)
                                .materialize_impl(db, env, materialization_kind, visitor)
                                .into()
                        })
                    }
                    TypeVarBoundOrConstraintsEvaluation::LazyConstraints => {
                        self.lazy_constraints(db, visitor.env).map(|constraints| {
                            TypeVarBoundOrConstraints::Constraints(constraints)
                                .materialize_impl(db, env, materialization_kind, visitor)
                                .into()
                        })
                    }
                }),
            self._lower_bound(db)
                .and_then(|lower_bound| match lower_bound {
                    TypeVarLowerBoundEvaluation::Eager(ty) => Some(
                        ty.materialize(db, env, materialization_kind, visitor)
                            .into(),
                    ),
                    TypeVarLowerBoundEvaluation::Lazy => self.lazy_lower_bound(db).map(|ty| {
                        ty.materialize(db, env, materialization_kind, visitor)
                            .into()
                    }),
                }),
            self.explicit_variance(db),
            self._default(db).and_then(|default| match default {
                TypeVarDefaultEvaluation::Eager(ty) => Some(
                    ty.materialize(db, env, materialization_kind, visitor)
                        .into(),
                ),
                TypeVarDefaultEvaluation::Lazy => self.lazy_default(db, visitor.env).map(|ty| {
                    ty.materialize(db, env, materialization_kind, visitor)
                        .into()
                }),
            }),
        )
    }

    fn to_instance(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<InstanceProjection<Self>> {
        let bound_or_constraints = match self.bound_or_constraints(db, env)? {
            TypeVarBoundOrConstraints::UpperBound(upper_bound) => upper_bound
                .to_instance(db, env)?
                .map(TypeVarBoundOrConstraints::UpperBound),
            TypeVarBoundOrConstraints::Constraints(constraints) => constraints
                .to_instance(db, env)?
                .map(TypeVarBoundOrConstraints::Constraints),
        };
        let lower_bound = match self.lower_bound(db) {
            Some(lower_bound) => Some(lower_bound.to_instance(db, env)?),
            None => None,
        };
        let identity = TypeVarIdentity::new(
            db,
            Name::concat(&[self.name(db).as_str(), "'instance"]),
            None, // definition
            self.kind(db),
        );
        // the projection is only exact if both ends of the bound project exactly
        let is_exact = bound_or_constraints.is_exact()
            && lower_bound
                .as_ref()
                .is_none_or(InstanceProjection::is_exact);
        Some(InstanceProjection::new(
            Self::new(
                db,
                identity,
                Some(bound_or_constraints.into_inner().into()),
                lower_bound.map(|lower_bound| lower_bound.into_inner().into()),
                self.explicit_variance(db),
                None, // _default
            ),
            is_exact,
        ))
    }

    fn type_is_self_referential(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
        visitor: &TypeVarDefaultVisitor<'db>,
    ) -> bool {
        type SeenTypeAliases<'db> = SmallVec<[Definition<'db>; 1]>;

        #[derive(Copy, Clone)]
        struct State<'db, 'a> {
            db: &'db dyn Db,
            env: &'a ProgramEnvironment<'db>,
            visitor: &'a TypeVarDefaultVisitor<'db>,
            seen_typevars: &'a RefCell<FxHashSet<TypeVarInstance<'db>>>,
            seen_type_aliases: &'a RefCell<SeenTypeAliases<'db>>,
        }

        fn typevar_default_is_self_referential<'db>(
            state: State<'db, '_>,
            env: &ProgramEnvironment<'db>,
            typevar: TypeVarInstance<'db>,
            self_identity: TypeVarIdentity<'db>,
        ) -> bool {
            let db = state.db;

            if typevar.identity(db) == self_identity {
                return true;
            }

            if !state.seen_typevars.borrow_mut().insert(typevar) {
                return false;
            }

            typevar
                .default_type_impl(db, state.env, state.visitor)
                .is_some_and(|default_ty| {
                    type_is_self_referential_impl(state, env, default_ty, self_identity)
                })
        }

        fn type_alias_is_self_referential<'db>(
            state: State<'db, '_>,
            env: &ProgramEnvironment<'db>,
            type_alias: TypeAliasType<'db>,
            self_identity: TypeVarIdentity<'db>,
        ) -> bool {
            let db = state.db;
            {
                let mut seen_type_aliases = state.seen_type_aliases.borrow_mut();
                let definition = type_alias.definition(db);
                // A recursive alias can produce a new specialization every time its body is
                // expanded, so use its definition as the stable recursion key.
                if seen_type_aliases.contains(&definition) {
                    return false;
                }
                seen_type_aliases.push(definition);
            }

            let value_type = if let Some(specialization) = type_alias.specialization(db) {
                if specialization
                    .types(db)
                    .iter()
                    .any(|ty| type_is_self_referential_impl(state, env, *ty, self_identity))
                {
                    return true;
                }
                type_alias.value_type(db)
            } else if let Some(generic_context) = type_alias.generic_context(db)
                && generic_context.variables(db).any(|typevar| {
                    typevar_default_is_self_referential(
                        state,
                        env,
                        typevar.typevar(db),
                        self_identity,
                    )
                })
            {
                return true;
            } else {
                type_alias.raw_value_type(db)
            };

            type_is_self_referential_impl(state, env, value_type, self_identity)
        }

        fn type_is_self_referential_impl<'db>(
            state: State<'db, '_>,
            env: &ProgramEnvironment<'db>,
            ty: Type<'db>,
            self_identity: TypeVarIdentity<'db>,
        ) -> bool {
            // `Self` is opaque here: its upper bound names the enclosing class's own type
            // parameters, so descending into it would make `class C[T = Self]` look like a
            // typevar whose default refers back to itself.
            any_over_type_with_opaque_self(state.db, env, ty, |inner_ty| match inner_ty {
                Type::TypeVar(bound_typevar) => typevar_default_is_self_referential(
                    state,
                    env,
                    bound_typevar.typevar(state.db),
                    self_identity,
                ),
                Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) => {
                    typevar_default_is_self_referential(state, env, typevar, self_identity)
                }
                Type::TypeAlias(alias) => {
                    type_alias_is_self_referential(state, env, alias, self_identity)
                }
                Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)) => {
                    type_alias_is_self_referential(state, env, alias, self_identity)
                }
                _ => false,
            })
        }

        let seen_typevars = RefCell::new(FxHashSet::default());
        let seen_type_aliases = RefCell::new(SeenTypeAliases::new());

        let state = State {
            db,
            env,
            visitor,
            seen_typevars: &seen_typevars,
            seen_type_aliases: &seen_type_aliases,
        };

        type_is_self_referential_impl(state, env, ty, self.identity(db))
    }

    /// Returns the "unchecked" upper bound of a type variable instance.
    /// `lazy_bound` checks if the upper bound type is generic (generic upper bound is not allowed).
    #[salsa::tracked(
        returns(copy),
        cycle_fn=lazy_bound_cycle_recover,
        cycle_initial=|_, _, _| None,
        heap_size=ruff_memory_usage::heap_size
    )]
    fn lazy_bound_unchecked(self, db: &'db dyn Db) -> Option<Type<'db>> {
        let definition = self.definition(db)?;
        let program_file = definition.program_file(db);
        let python_file = program_file.python_file(db);
        let module = parsed_module(db, python_file).load(db);
        let ty = match definition.kind(db) {
            // PEP 695 typevar
            DefinitionKind::TypeVar(typevar) => {
                let typevar_node = typevar.node(&module);
                definition_expression_type(db, definition, typevar_node.bound.as_ref()?)
            }
            // basedpython: `*Ts: int` bounds every element of the pack
            DefinitionKind::TypeVarTuple(typevartuple) => {
                let typevartuple_node = typevartuple.node(&module);
                definition_expression_type(db, definition, typevartuple_node.bound.as_ref()?)
            }
            // basedpython: `**Kwargs: int` bounds every field of a keyword-variadic pack
            DefinitionKind::ParamSpec(paramspec) => {
                let paramspec_node = paramspec.node(&module);
                definition_expression_type(db, definition, paramspec_node.bound.as_ref()?)
            }
            // legacy typevar
            DefinitionKind::Assignment(assignment) => {
                let call_expr = assignment.value(&module).as_call_expr()?;
                let expr = &call_expr.arguments.find_keyword("bound")?.value;
                definition_expression_type(db, definition, expr)
            }
            // basedpython: an unannotated parameter's hole is bounded by everything the
            // function requires of it, which is read out of the body it is used in
            DefinitionKind::Parameter(_) => {
                crate::types::inferred_signature::inferred_parameter_bound(db, definition)
            }
            _ => return None,
        };

        Some(ty)
    }

    /// basedpython: whether this pack's bound was written starred — `*Ts: *(int, str)` or
    /// `**Kwargs: **{"a": int}` — which bounds the pack *as a whole* rather than element by
    /// element.
    ///
    /// The star count follows the pack's declaration, so the shape of the bound expression is
    /// what distinguishes the two readings: an unstarred `*Ts: int` bounds each element, and the
    /// starred `*Ts: *tuple[int, ...]` bounds the pack itself.
    #[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
    pub(crate) fn has_whole_pack_bound(self, db: &'db dyn Db) -> bool {
        let Some(definition) = self.definition(db) else {
            return false;
        };
        let module = parsed_module(db, definition.program_file(db).python_file(db)).load(db);
        let bound = match definition.kind(db) {
            DefinitionKind::TypeVarTuple(typevartuple) => &typevartuple.node(&module).bound,
            DefinitionKind::ParamSpec(paramspec) => &paramspec.node(&module).bound,
            _ => return false,
        };
        bound
            .as_deref()
            .is_some_and(|bound| matches!(bound, ast::Expr::Starred(_)))
    }

    /// basedpython: whether this parameter is an anonymous hole rather than an entry someone
    /// wrote in a `[...]` list — the one a `some T` annotation declares, or the one an
    /// unannotated parameter opens under `sound-types`.
    ///
    /// A hole is not a supplyable position — it takes the name of the parameter that opened
    /// it — so anything that offers or reads back a type parameter list has to leave it out.
    #[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
    pub(crate) fn is_some_hole(self, db: &'db dyn Db) -> bool {
        if self.kind(db) == TypeVarKind::InferredParameter {
            return true;
        }
        let Some(definition) = self.definition(db) else {
            return false;
        };
        let module = parsed_module(db, definition.program_file(db).python_file(db)).load(db);
        match definition.kind(db) {
            DefinitionKind::TypeVar(typevar) => typevar.node(&module).is_some_hole,
            _ => false,
        }
    }

    fn lazy_bound(self, db: &'db dyn Db, _env: &ProgramEnvironment<'db>) -> Option<Type<'db>> {
        let bound = self.lazy_bound_unchecked(db)?;

        // a bound naming a type parameter that is already in scope is kept — `def f[T, R: T]`.
        // one naming a parameter that is not is reported *and dropped*, because the consumers
        // that reduce a type variable to its bound recurse into it
        if bound_scope_violation(db, self, bound).is_some() {
            return None;
        }

        Some(bound)
    }

    /// Returns the "unchecked" constraints of a type variable instance.
    /// `lazy_constraints` checks if any of the constraint types are generic (generic constraints are not allowed).
    #[salsa::tracked(
        returns(copy),
        cycle_fn=lazy_constraints_cycle_recover,
        cycle_initial=|_, _, _| None,
        heap_size=ruff_memory_usage::heap_size
    )]
    fn lazy_constraints_unchecked(self, db: &'db dyn Db) -> Option<TypeVarConstraints<'db>> {
        let definition = self.definition(db)?;
        let program_file = definition.program_file(db);
        let python_file = program_file.python_file(db);
        let env = ProgramEnvironment::from_file(program_file);
        let module = parsed_module(db, python_file).load(db);
        let constraints = match definition.kind(db) {
            // PEP 695 typevar
            DefinitionKind::TypeVar(typevar) => {
                let typevar_node = typevar.node(&module);
                let bound =
                    definition_expression_type(db, definition, typevar_node.bound.as_ref()?);
                if let Some(tuple) = bound.tuple_instance_spec(db, &env)
                    && let Tuple::Fixed(tuple) = tuple.into_owned()
                {
                    TypeVarConstraints::new(db, tuple.owned_elements())
                } else {
                    TypeVarConstraints::new(db, [Type::unknown()].as_slice())
                }
            }
            // legacy typevar
            DefinitionKind::Assignment(assignment) => {
                let call_expr = assignment.value(&module).as_call_expr()?;
                TypeVarConstraints::new(
                    db,
                    call_expr
                        .arguments
                        .args
                        .iter()
                        .skip(1)
                        .map(|arg| definition_expression_type(db, definition, arg))
                        .collect::<Box<_>>(),
                )
            }
            _ => return None,
        };

        Some(constraints)
    }

    fn lazy_constraints(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<TypeVarConstraints<'db>> {
        let constraints = self.lazy_constraints_unchecked(db)?;

        if constraints
            .elements(db)
            .iter()
            .any(|ty| ty.has_typevar_or_typevar_instance(db, env))
        {
            return None;
        }

        Some(constraints)
    }

    /// Returns the "unchecked" default type of a type variable instance.
    /// `lazy_default` checks if the default type is not self-referential.
    #[salsa::tracked(returns(copy), cycle_initial=|_, id, _| Some(Type::divergent(id)), cycle_fn=lazy_default_cycle_recover, heap_size=ruff_memory_usage::heap_size)]
    fn lazy_default_unchecked(self, db: &'db dyn Db) -> Option<Type<'db>> {
        fn convert_type_to_paramspec_value<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
            let parameters = match ty {
                Type::NominalInstance(nominal_instance)
                    if nominal_instance.has_known_class(db, KnownClass::EllipsisType) =>
                {
                    Parameters::gradual_form()
                }
                Type::NominalInstance(nominal_instance) => nominal_instance
                    .own_tuple_spec(db)
                    .map_or_else(Parameters::unknown, |tuple_spec| {
                        match tuple_spec.as_ref() {
                            Tuple::Fixed(tuple) => {
                                Parameters::standard(tuple.iter_all_elements().map(|ty| {
                                    Parameter::positional_only(None).with_annotated_type(ty)
                                }))
                            }
                            // A `ParamSpec` default cannot contain a variable-length tuple, so this
                            // branch only recovers from an invalid type expression.
                            Tuple::Variable(_) => Parameters::unknown(),
                        }
                    }),
                Type::Dynamic(dynamic) => match dynamic {
                    DynamicType::Todo(_) => Parameters::todo(),
                    DynamicType::Any
                    | DynamicType::Unknown
                    | DynamicType::UnknownGeneric(_)
                    | DynamicType::UnspecializedTypeVar
                    | DynamicType::UnknownLambdaParameter
                    | DynamicType::InvalidConcatenateUnknown
                    | DynamicType::AmbiguousOverload => Parameters::unknown(),
                },
                Type::Divergent(_) => Parameters::unknown(),
                Type::TypeVar(typevar) if typevar.is_parameter_pack(db) => {
                    return ty;
                }
                Type::KnownInstance(KnownInstanceType::TypeVar(typevar))
                    if typevar.is_parameter_pack(db) =>
                {
                    return ty;
                }
                _ => Parameters::unknown(),
            };
            Type::paramspec_value_callable(db, parameters)
        }

        let definition = self.definition(db)?;
        let program_file = definition.program_file(db);
        let python_file = program_file.python_file(db);
        let module = parsed_module(db, python_file).load(db);
        let ty = match definition.kind(db) {
            // PEP 695 typevar
            DefinitionKind::TypeVar(typevar) => {
                let typevar_node = typevar.node(&module);
                definition_expression_type(db, definition, typevar_node.default.as_ref()?)
            }
            // legacy typevar / ParamSpec
            DefinitionKind::Assignment(assignment) => {
                let call_expr = assignment.value(&module).as_call_expr()?;
                let func_ty = definition_expression_type(db, definition, &call_expr.func);
                let known_class = func_ty.as_class_literal().and_then(|cls| cls.known(db));
                let expr = &call_expr.arguments.find_keyword("default")?.value;
                let default_type = definition_expression_type(db, definition, expr);
                if matches!(
                    known_class,
                    Some(KnownClass::ParamSpec | KnownClass::ExtensionsParamSpec)
                ) {
                    convert_type_to_paramspec_value(db, default_type)
                } else {
                    default_type
                }
            }
            // PEP 695 ParamSpec
            DefinitionKind::ParamSpec(paramspec) => {
                let paramspec_node = paramspec.node(&module);
                let default_ty =
                    definition_expression_type(db, definition, paramspec_node.default.as_ref()?);
                convert_type_to_paramspec_value(db, default_ty)
            }
            // PEP 695 TypeVarTuple
            DefinitionKind::TypeVarTuple(typevartuple) => {
                let typevartuple_node = typevartuple.node(&module);
                definition_expression_type(db, definition, typevartuple_node.default.as_ref()?)
            }
            // basedpython: an unannotated parameter's hole defaults to the parameter's own
            // default value, so a call that omits the argument still names its type
            DefinitionKind::Parameter(_) => {
                crate::types::inferred_signature::inferred_parameter_default(db, definition)?
            }
            _ => return None,
        };

        Some(ty)
    }

    fn lazy_default(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Option<Type<'db>> {
        let visitor = TypeVarDefaultVisitor::new(None);
        self.lazy_default_impl(db, env, &visitor)
    }

    fn lazy_default_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        visitor: &TypeVarDefaultVisitor<'db>,
    ) -> Option<Type<'db>> {
        let default = self.lazy_default_unchecked(db)?;

        // Unlike bounds/constraints, default types are allowed to be generic
        // (https://typing.python.org/en/latest/spec/generics.html#defaults-for-type-parameters).
        // Here we simply check for non-self-referential.
        // TODO: We should also check for non-forward references.
        if self.type_is_self_referential(db, env, default, visitor) {
            return None;
        }

        Some(default)
    }

    pub fn bind_pep695(self, db: &'db dyn Db) -> Option<BoundTypeVarInstance<'db>> {
        if !matches!(
            self.identity(db).kind(db),
            TypeVarKind::Pep695TypeVar | TypeVarKind::Pep695ParamSpec
        ) {
            return None;
        }
        let typevar_definition = self.definition(db)?;
        let index = semantic_index(db, typevar_definition.program_file(db));
        let (_, child) = index
            .child_scopes(typevar_definition.file_scope(db))
            .next()?;
        GenericContext::of_node(db, child.node(), index)?.binds_typevar(db, self)
    }
}

/// How a type variable's bound names a type variable it may not name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum BoundScopeViolation<'db> {
    /// `def f[T: list[T]]` — the parameter is not in scope inside its own bound.
    SelfReference,
    /// `def f[S: T, T]` — PEP 695 allows a bound to name an earlier parameter, not a later one.
    LaterInList(TypeVarInstance<'db>),
    /// A legacy `TypeVar(bound=U)`, or a name no enclosing type-parameter list declares.
    OutOfScope,
}

impl BoundScopeViolation<'_> {
    pub(crate) const fn out_of_scope() -> Self {
        Self::OutOfScope
    }
}

/// Classifies every type variable `bound` names against the list `own_definition` belongs to.
///
/// A type parameter may name one that is already in scope where it is written: an earlier entry
/// in the same list, or one belonging to an enclosing type-parameter list. Because the entries of
/// a list appear in source order, "earlier" is decided by comparing offsets rather than by
/// walking the list.
///
/// `Self` is exempt. It is bound by the enclosing class rather than by the list being declared,
/// and `def method[T: Self]` is checked when the method binds its receiver.
///
/// The rule is decided in one place because two callers need the same answer for different
/// reasons: the diagnostic reports it, and [`lazy_bound`](TypeVarInstance::lazy_bound) has to
/// *drop* a bound that breaks it. Dropping matters more than the message — several consumers
/// reduce a type variable to its bound by plain recursion, so installing the mutually recursive
/// bounds of `def f[T: R, R: T]` would be a stack overflow rather than an error.
pub(crate) fn bound_scope_violation_for<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    own_definition: Definition<'db>,
    bound: Type<'db>,
) -> Option<BoundScopeViolation<'db>> {
    let program_file = own_definition.program_file(db);
    let file = own_definition.file(db);
    let own_scope = own_definition.file_scope(db);
    let index = semantic_index(db, program_file);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let own_start = Ranged::start(&own_definition.full_range(db, &module));

    // only a type parameter has a list to be early or late in. a legacy `TypeVar(bound=U)` is
    // declared by an assignment, so there is no position for `U` to precede — and one `TypeVar`
    // object can be reused by two unrelated generics, which is why the rule cannot be relaxed
    // for it by looking at the surrounding scope instead
    let in_type_param_list = index.scope(own_scope).kind() == ScopeKind::TypeParams;

    // The one enclosing list a bound may name, if there is one: the class's, when this list
    // belongs to one of that class's own methods. That is the only enclosing case anything
    // substitutes — projecting a member from `Owner[int]` applies `Owner`'s specialization to the
    // method's signature, and the bound is rewritten with it. Nothing does that for a list on a
    // nested class or a nested function, so naming an enclosing parameter there would leave a
    // variable in the bound that no specialization ever reaches, and every use of the generic
    // would fail a bound it cannot satisfy.
    //
    // The chain for a method is exactly [its own list, the class body, the class's list]; anything
    // longer has crossed something that does not carry the parameter along.
    let method_owner_type_params = index
        .ancestor_scopes(own_scope)
        .skip(1)
        .take(2)
        .collect_tuple()
        .filter(|((_, body), (_, type_params))| {
            matches!(
                index.scope(own_scope).node(),
                NodeWithScopeKind::FunctionTypeParameters(_)
            ) && body.kind().is_class()
                && matches!(
                    type_params.node(),
                    NodeWithScopeKind::ClassTypeParameters(_)
                )
        })
        .map(|(_, (type_params_scope, _))| type_params_scope);

    // the bound's own lazy attributes are not searched: a name this bound writes is a type
    // variable of its own, and what *that* variable is bounded by is its own declaration's problem
    find_over_type(db, env, bound, false, |ty| {
        let referenced = match ty {
            Type::TypeVar(bound_typevar) => bound_typevar.typevar(db),
            Type::KnownInstance(KnownInstanceType::TypeVar(typevar)) => typevar,
            _ => return None,
        };
        if referenced.is_self(db) {
            return None;
        }
        Some(match referenced.definition(db) {
            Some(definition) if definition == own_definition => BoundScopeViolation::SelfReference,
            Some(definition)
                if in_type_param_list
                    && definition.file(db) == file
                    && definition.file_scope(db) == own_scope =>
            {
                if Ranged::start(&definition.full_range(db, &module)) < own_start {
                    return None;
                }
                BoundScopeViolation::LaterInList(referenced)
            }
            Some(definition)
                if in_type_param_list
                    && definition.file(db) == file
                    && method_owner_type_params == Some(definition.file_scope(db)) =>
            {
                return None;
            }
            _ => BoundScopeViolation::OutOfScope,
        })
    })
}

/// The cached form of [`bound_scope_violation_for`], for the callers that have to consult it
/// whenever a bound is read rather than once per definition.
#[salsa::tracked(returns(copy), cycle_result=|_, _, _, _| None, heap_size=ruff_memory_usage::heap_size)]
fn bound_scope_violation<'db>(
    db: &'db dyn Db,
    typevar: TypeVarInstance<'db>,
    bound: Type<'db>,
) -> Option<BoundScopeViolation<'db>> {
    let own_definition = typevar.definition(db)?;
    let env = ProgramEnvironment::from_definition(own_definition);
    bound_scope_violation_for(db, &env, own_definition, bound)
}

/// A nonce that gives a bound typevar occurrence a fresh identity.
///
/// `0` is reserved for source-level, non-freshened typevars. Positive values identify fresh
/// occurrences.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TypeVarNonce(u32);

// This type does not have any heap storage.
impl get_size2::GetSize for TypeVarNonce {}

/// How far a signature's typevars may be freshened past the signature it is compared against.
///
/// Freshening only has to lift one signature's typevars clear of the other's, so the distance
/// needed is the nesting of same-context generic signatures inside one comparison — a handful
/// at most in real code. It is unbounded only when a comparison reproduces itself at a greater
/// freshness, which a self-referential inferred return type does: `def f(self): return self.f`
/// makes each comparison of `f` against itself demand a signature one nonce fresher than the
/// last, so no two rounds are ever equal and no memo or cycle guard can close the loop.
pub(crate) const MAX_TYPEVAR_FRESHNESS_DELTA: u32 = 32;

impl TypeVarNonce {
    pub(crate) const NONE: Self = Self(0);
    const FIRST: Self = Self(1);

    pub(crate) const fn value(self) -> u32 {
        self.0
    }

    pub(crate) fn increment(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("exhausted bound typevar freshness nonces"),
        )
    }

    fn add(self, delta: u32) -> Self {
        Self(
            self.0
                .checked_add(delta)
                .expect("exhausted bound typevar freshness nonces"),
        )
    }
}

#[derive(Debug)]
struct TypeVarNonceGeneratorInner<'db> {
    next: TypeVarNonce,
    seen: FxHashSet<GenericContext<'db>>,
    enclosing: FxHashSet<BindingContext<'db>>,
}

/// A clone-safe generator of fresh bound-typevar occurrence nonces.
///
/// The generator only allocates a nonce for the second and later occurrence of a generic context.
/// The first occurrence can use its source-level identity directly because there is no previous
/// occurrence for it to collide with.
#[derive(Clone, Debug)]
pub(crate) struct TypeVarNonceGenerator<'db> {
    inner: Rc<RefCell<TypeVarNonceGeneratorInner<'db>>>,
}

impl Default for TypeVarNonceGenerator<'_> {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(TypeVarNonceGeneratorInner {
                next: TypeVarNonce::FIRST,
                seen: FxHashSet::default(),
                enclosing: FxHashSet::default(),
            })),
        }
    }
}

impl<'db> TypeVarNonceGenerator<'db> {
    pub(crate) fn record_enclosing_binding_contexts(
        &self,
        binding_contexts: impl IntoIterator<Item = BindingContext<'db>>,
    ) {
        let mut inner = self.inner.borrow_mut();
        inner.enclosing.extend(binding_contexts);
    }

    pub(crate) fn should_freshen(
        &self,
        db: &'db dyn Db,
        generic_context: GenericContext<'db>,
    ) -> bool {
        let mut inner = self.inner.borrow_mut();
        let mut binding_contexts = generic_context
            .variables(db)
            .map(|typevar| typevar.binding_context(db));
        // A context inherited from an enclosing definition can be merged with another context.
        // Only the unmerged context represents a recursive occurrence that needs freshening.
        let matches_enclosing = binding_contexts.next().is_some_and(|binding_context| {
            inner.enclosing.contains(&binding_context)
                && binding_contexts.all(|other| other == binding_context)
        });
        matches_enclosing || !inner.seen.insert(generic_context)
    }

    pub(crate) fn next(&self) -> TypeVarNonce {
        let mut inner = self.inner.borrow_mut();
        let nonce = inner.next;
        inner.next = nonce.increment();
        nonce
    }
}

pub(crate) fn max_typevar_freshness_matching_generic_context<'db>(
    db: &'db dyn Db,
    types: impl IntoIterator<Item = Type<'db>>,
    generic_context: GenericContext<'db>,
) -> Option<TypeVarNonce> {
    struct MatchingFreshnessCollector<'a, 'db> {
        env: &'a ProgramEnvironment<'db>,
        base_identities: FxHashSet<BoundTypeVarIdentity<'db>>,
        recursion_guard: TypeCollector<'db>,
        max_freshness: Cell<Option<TypeVarNonce>>,
    }

    impl<'a, 'db> MatchingFreshnessCollector<'a, 'db> {
        fn new(
            db: &'db dyn Db,
            env: &'a ProgramEnvironment<'db>,
            generic_context: GenericContext<'db>,
        ) -> Self {
            let base_identities = generic_context
                .variables(db)
                .map(|typevar| {
                    let mut identity = typevar.identity(db);
                    identity.freshness = TypeVarNonce::NONE;
                    identity
                })
                .collect();
            Self {
                env,
                base_identities,
                recursion_guard: TypeCollector::default(),
                max_freshness: Cell::default(),
            }
        }
    }

    impl<'db> TypeVisitor<'db> for MatchingFreshnessCollector<'_, 'db> {
        fn program_environment(&self) -> &ProgramEnvironment<'db> {
            self.env
        }

        fn should_visit_lazy_type_attributes(&self) -> bool {
            false
        }

        fn visit_bound_type_var_type(
            &self,
            db: &'db dyn Db,
            bound_typevar: BoundTypeVarInstance<'db>,
        ) {
            let mut identity = bound_typevar.identity(db);
            identity.freshness = TypeVarNonce::NONE;
            if self.base_identities.contains(&identity) {
                self.max_freshness.set(
                    self.max_freshness
                        .get()
                        .max(Some(bound_typevar.freshness(db))),
                );
            }
        }

        fn visit_type(&self, db: &'db dyn Db, ty: Type<'db>) {
            walk_type_with_recursion_guard(db, ty, self, &self.recursion_guard);
        }
    }

    let env = ProgramEnvironment::from_program(generic_context.program(db));
    let collector = MatchingFreshnessCollector::new(db, &env, generic_context);
    for ty in types {
        collector.visit_type(db, ty);
    }
    collector.max_freshness.get()
}

/// A type variable that has been bound to a generic context, and which can be specialized to a
/// concrete type.
#[salsa::interned(
    debug,
    constructor = new_internal,
    heap_size = ruff_memory_usage::heap_size
)]
pub struct BoundTypeVarInstance<'db> {
    #[returns(copy)]
    pub typevar: TypeVarInstance<'db>,
    // This duplicates the source-level identity accessible through `typevar`, but keeps
    // `identity()` to a single interned-field read. Storing only the occurrence-specific fields
    // and reconstructing the full identity regresses hot-path project benchmarks.
    #[returns(copy)]
    identity_inner: BoundTypeVarIdentity<'db>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for BoundTypeVarInstance<'_> {}

impl<'db> BoundTypeVarInstance<'db> {
    pub(crate) fn new(
        db: &'db dyn Db,
        typevar: TypeVarInstance<'db>,
        binding_context: BindingContext<'db>,
        paramspec_attr: Option<ParamSpecAttrKind>,
        freshness: TypeVarNonce,
    ) -> Self {
        let identity = BoundTypeVarIdentity {
            identity: typevar.identity(db),
            binding_context,
            paramspec_attr,
            freshness,
        };
        Self::new_internal(db, typevar, identity)
    }

    pub(super) fn binding_context(self, db: &'db dyn Db) -> BindingContext<'db> {
        self.identity(db).binding_context
    }

    pub(super) fn paramspec_attr(self, db: &'db dyn Db) -> Option<ParamSpecAttrKind> {
        self.identity(db).paramspec_attr
    }

    pub(super) fn freshness(self, db: &'db dyn Db) -> TypeVarNonce {
        self.identity(db).freshness
    }

    pub(crate) fn with_name_suffix(self, db: &'db dyn Db, suffix: &str) -> Self {
        Self::new(
            db,
            self.typevar(db).with_name_suffix(db, suffix),
            self.binding_context(db),
            self.paramspec_attr(db),
            self.freshness(db),
        )
    }

    /// Get the identity of this bound typevar occurrence.
    ///
    /// This includes the source-level typevar, binding context, `ParamSpec` attribute, and
    /// freshness nonce. It is used for comparing whether two bound typevars represent the same
    /// occurrence, regardless of e.g. differences in their bounds or constraints due to
    /// materialization.
    pub(crate) fn identity(self, db: &'db dyn Db) -> BoundTypeVarIdentity<'db> {
        self.identity_inner(db)
    }

    pub fn name(self, db: &'db dyn Db) -> &'db Name {
        self.typevar(db).name(db)
    }

    pub(crate) fn kind(self, db: &'db dyn Db) -> TypeVarKind {
        self.identity(db).kind(db)
    }

    pub fn is_paramspec(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_paramspec()
    }

    pub fn is_keyword_variadic(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_keyword_variadic()
    }

    pub(crate) fn is_parameter_pack(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_parameter_pack()
    }

    pub fn is_typevartuple(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_typevartuple()
    }

    pub fn is_typing_self(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_typing_self()
    }

    /// Returns a new bound typevar instance with the given `ParamSpec` attribute set.
    ///
    /// This method will also set an appropriate upper bound on the typevar, based on the
    /// attribute kind. For `P.args`, the upper bound will be `tuple[object, ...]`, and for
    /// `P.kwargs`, the upper bound will be `Top[dict[str, Any]]`.
    ///
    /// It's the caller's responsibility to ensure that this method is only called on a parameter
    /// pack. basedpython's keyword-variadic packs have no source-level `.args`/`.kwargs`, but
    /// they share the unspecialized placeholder built by [`Parameters::paramspec`], which is
    /// spelled with these components.
    ///
    /// [`Parameters::paramspec`]: crate::types::Parameters::paramspec
    pub(crate) fn with_paramspec_attr(self, db: &'db dyn Db, kind: ParamSpecAttrKind) -> Self {
        debug_assert!(
            self.is_parameter_pack(db),
            "Expected a parameter pack, got {:?}",
            self.kind(db)
        );

        let env = ProgramEnvironment::from_program(self.binding_context(db).program(db));
        let upper_bound = TypeVarBoundOrConstraints::UpperBound(match kind {
            ParamSpecAttrKind::Args => Type::homogeneous_tuple(db, &env, Type::object()),
            ParamSpecAttrKind::Kwargs => KnownClass::Dict
                .to_specialized_instance(
                    db,
                    &env,
                    &[KnownClass::Str.to_instance(db, &env), Type::any()],
                )
                .top_materialization(db, &env),
        });

        let typevar = self.typevar(db);
        let typevar = TypeVarInstance::new(
            db,
            typevar.identity(db),
            Some(TypeVarBoundOrConstraintsEvaluation::Eager(upper_bound)),
            None, // `P.args` and `P.kwargs` have no lower bound
            typevar.explicit_variance(db),
            None, // `P.args` and `P.kwargs` cannot have defaults even though `P` can
        );

        Self::new(
            db,
            typevar,
            self.binding_context(db),
            Some(kind),
            self.freshness(db),
        )
    }

    /// Returns a new bound typevar instance without any `ParamSpec` attribute set.
    ///
    /// This method will also remove any upper bound that was set by `with_paramspec_attr`. This
    /// means that the returned typevar will have no upper bound or constraints.
    ///
    /// It's the caller's responsibility to ensure that this method is only called on a `ParamSpec`
    /// type variable.
    pub(crate) fn without_paramspec_attr(self, db: &'db dyn Db) -> Self {
        debug_assert!(
            self.is_parameter_pack(db),
            "Expected a parameter pack, got {:?}",
            self.kind(db)
        );

        let typevar = self.typevar(db);
        Self::new(
            db,
            TypeVarInstance::new(
                db,
                typevar.identity(db),
                None, // Remove the upper bound set by `with_paramspec_attr`
                None, // _lower_bound
                typevar.explicit_variance(db),
                None, // `P.args` and `P.kwargs` cannot have defaults even though `P` can
            ),
            self.binding_context(db),
            None,
            self.freshness(db),
        )
    }

    /// Returns whether two bound typevars represent the same occurrence, regardless of e.g.
    /// differences in their bounds or constraints due to materialization.
    pub(crate) fn is_same_typevar_as(self, db: &'db dyn Db, other: Self) -> bool {
        self.identity(db) == other.identity(db)
    }

    /// Create a new PEP 695 type variable that can be used in signatures
    /// of synthetic generic functions.
    pub(crate) fn synthetic(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        name: Name,
        variance: TypeVarVariance,
    ) -> Self {
        let identity = TypeVarIdentity::new(
            db,
            name,
            None, // definition
            TypeVarKind::Pep695TypeVar,
        );
        let typevar = TypeVarInstance::new(
            db,
            identity,
            None, // _bound_or_constraints
            None, // _lower_bound
            Some(variance),
            None, // _default
        );
        Self::new(
            db,
            typevar,
            BindingContext::Synthetic(env.program(db)),
            None,
            TypeVarNonce::NONE,
        )
    }

    /// Create a new synthetic `Self` type variable with the given upper bound.
    pub(crate) fn synthetic_self(
        db: &'db dyn Db,
        upper_bound: Type<'db>,
        binding_context: BindingContext<'db>,
    ) -> Self {
        let identity = TypeVarIdentity::new(
            db,
            Name::new_static("Self"),
            None, // definition
            TypeVarKind::TypingSelf,
        );
        let typevar = TypeVarInstance::new(
            db,
            identity,
            Some(TypeVarBoundOrConstraints::UpperBound(upper_bound).into()),
            None, // _lower_bound
            Some(TypeVarVariance::Invariant),
            None, // _default
        );
        Self::new(db, typevar, binding_context, None, TypeVarNonce::NONE)
    }

    /// Applies a specialization to this occurrence's declared upper bound or constraints, if any.
    fn apply_specialization_to_bound_or_constraints(
        self,
        db: &'db dyn Db,
        specialization: Specialization<'db>,
        env: &ProgramEnvironment<'db>,
    ) -> Self {
        self.map_bound_or_constraints(db, |original| {
            let original = original?;
            let mapping = TypeMapping::ApplySpecialization(ApplySpecialization::specialization(
                specialization,
            ));
            let visitor = ApplyTypeMappingVisitor::new(env);
            let bound = original.apply_type_mapping_impl(db, env, &mapping, &visitor);
            // basedpython: substituting into `C[T]` rebuilds the specialization from the
            // arguments alone, which loses the use-site projection the receiver was written
            // with. `Self` stands for that receiver, so it has to keep the same view of it —
            // otherwise the receiver fails its own `Self` bound at every call.
            let projections = specialization.projections(db);
            Some(match bound {
                TypeVarBoundOrConstraints::UpperBound(bound) => {
                    TypeVarBoundOrConstraints::UpperBound(bound.with_use_site_projections(
                        db,
                        env,
                        projections,
                    ))
                }
                TypeVarBoundOrConstraints::Constraints(constraints) => {
                    let projected: Vec<_> = constraints
                        .elements(db)
                        .iter()
                        .map(|constraint| {
                            constraint.with_use_site_projections(db, env, projections)
                        })
                        .collect();
                    TypeVarBoundOrConstraints::Constraints(TypeVarConstraints::new(
                        db,
                        projected.as_slice(),
                    ))
                }
            })
        })
    }

    /// Returns an identical type variable with its `TypeVarBoundOrConstraints` mapped by the
    /// provided closure.
    pub(crate) fn map_bound_or_constraints(
        self,
        db: &'db dyn Db,
        f: impl FnOnce(Option<TypeVarBoundOrConstraints<'db>>) -> Option<TypeVarBoundOrConstraints<'db>>,
    ) -> Self {
        let env = ProgramEnvironment::from_program(self.binding_context(db).program(db));
        let typevar = self.typevar(db);
        let bound_or_constraints = f(typevar.bound_or_constraints(db, &env));
        let typevar = TypeVarInstance::new(
            db,
            typevar.identity(db),
            bound_or_constraints.map(TypeVarBoundOrConstraintsEvaluation::Eager),
            typevar._lower_bound(db),
            typevar.explicit_variance(db),
            typevar._default(db),
        );

        Self::new(
            db,
            typevar,
            self.binding_context(db),
            self.paramspec_attr(db),
            self.freshness(db),
        )
    }

    pub(crate) fn variance_with_polarity(
        self,
        db: &'db dyn Db,
        polarity: TypeVarVariance,
    ) -> TypeVarVariance {
        let _span = tracing::trace_span!("variance_with_polarity").entered();

        match self.typevar(db).explicit_variance(db) {
            Some(explicit_variance) => explicit_variance.compose(polarity),
            None => match self.binding_context(db) {
                BindingContext::Definition(definition) => polarity.compose_thunk(|| {
                    let env = ProgramEnvironment::from_definition(definition);
                    let binding_ty = binding_type(db, definition);
                    // basedpython: a reified class parameter is part of what the
                    // instance *is* — the program can read the type argument back
                    // and test for it — so two specializations of the class match
                    // only when they were given the same argument. that is what
                    // invariance says, and inferring anything wider would let a
                    // construction solve the parameter to something the instance
                    // then reports it was never built with
                    if self.reifies_on(db, binding_ty) {
                        return TypeVarVariance::Invariant;
                    }
                    match binding_ty
                        .variance_of(db, &env, self.identity(db))
                        .evaluate(db)
                    {
                        // When both directions are valid, the typing spec selects covariance. It
                        // says so of a parameter the class never mentions; basedpython also infers
                        // bivariance for one that only a private member mentions, and that
                        // parameter really is used, so its inferred variance stands.
                        TypeVarVariance::Bivariant
                            if binding_ty
                                .as_class_literal()
                                .and_then(ClassLiteral::as_static)
                                .is_none_or(|class| {
                                    class.typevar_is_unused(db, self.identity(db))
                                }) =>
                        {
                            TypeVarVariance::Covariant
                        }
                        variance => variance,
                    }
                }),
                BindingContext::Synthetic(_) => TypeVarVariance::Invariant,
            },
        }
    }

    pub fn variance(self, db: &'db dyn Db) -> TypeVarVariance {
        self.variance_with_polarity(db, TypeVarVariance::Covariant)
    }

    /// basedpython: the variance a *runtime* probe should test, which is the inferred
    /// answer without the typing spec's rule that a bivariant class parameter is
    /// reported covariant. A parameter no member mentions really does match either
    /// way, and the parametric `is`-test skips comparing it rather than checking a
    /// direction that cannot fail.
    pub(crate) fn probe_variance(self, db: &'db dyn Db) -> TypeVarVariance {
        match self.typevar(db).explicit_variance(db) {
            Some(explicit_variance) => explicit_variance,
            None => match self.binding_context(db) {
                BindingContext::Definition(definition) => {
                    let binding_ty = binding_type(db, definition);
                    if self.reifies_on(db, binding_ty) {
                        return TypeVarVariance::Invariant;
                    }
                    let env = ProgramEnvironment::from_definition(definition);
                    binding_ty
                        .variance_of(db, &env, self.identity(db))
                        .evaluate(db)
                }
                BindingContext::Synthetic(_) => TypeVarVariance::Invariant,
            },
        }
    }

    /// basedpython: whether this parameter is bivariant only because nothing but a private
    /// member mentions it.
    ///
    /// Inference reads this to tell the two sources of bivariance apart. A parameter declared
    /// `in out`, and one no member mentions at all, are bivariant for good — there is no argument
    /// hiding behind them to recover. A privately used one is different: the class really was
    /// given an argument, and a solve that has to read it back gets nothing if the position is
    /// skipped. Inference for a parameter no member mentions never reaches this: the spec's rule
    /// reports it as covariant, so [`Self::variance`] never answers `Bivariant` for it.
    pub(crate) fn is_bivariant_by_privacy(self, db: &'db dyn Db) -> bool {
        self.typevar(db).explicit_variance(db).is_none()
            && self.variance(db) == TypeVarVariance::Bivariant
    }

    /// basedpython: the variance a *solve* should read at this parameter's position.
    ///
    /// A bivariant position relates nothing, so descending into one recovers no type argument.
    /// That is the right answer for a parameter that really is bivariant, and the wrong one for a
    /// parameter that is bivariant only [by privacy](Self::is_bivariant_by_privacy): the class
    /// was given an argument there, and reading it covariantly is what recovers it.
    ///
    /// Reading it is only free while it cannot make the call stricter, and it stops being free as
    /// soon as the variable being solved has a domain. `C[str]` and `C[int]` are mutually
    /// assignable when only a private member mentions `C`'s parameter, so every argument is a
    /// valid solution — but recovering `str` for a `U: int` and then measuring it against that
    /// bound rejects a call the checker elsewhere says is fine. A bounded or constrained target
    /// therefore keeps the bivariant reading and is left to ordinary inference.
    pub(crate) fn solving_variance_with_polarity(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        polarity: TypeVarVariance,
        target: Type<'db>,
    ) -> TypeVarVariance {
        let variance = self.variance_with_polarity(db, polarity);
        if variance == TypeVarVariance::Bivariant
            && self.is_bivariant_by_privacy(db)
            && !any_over_type(db, env, target, false, |ty| {
                matches!(ty, Type::TypeVar(target_typevar)
                    if target_typevar.typevar(db).bound_or_constraints(db, env).is_some())
            })
        {
            // composing the polarity with covariance leaves the polarity itself
            polarity
        } else {
            variance
        }
    }

    /// basedpython: whether this is a type parameter of a class that reifies it,
    /// so the type argument is a runtime property of every instance rather than
    /// something erased at construction.
    pub(crate) fn is_reified_class_typevar(self, db: &'db dyn Db) -> bool {
        let BindingContext::Definition(definition) = self.binding_context(db) else {
            return false;
        };
        self.reifies_on(db, binding_type(db, definition))
    }

    /// [`is_reified_class_typevar`](Self::is_reified_class_typevar) for a caller
    /// that already has the binding's type in hand.
    fn reifies_on(self, db: &'db dyn Db, binding_ty: Type<'db>) -> bool {
        binding_ty
            .as_class_literal()
            .and_then(ClassLiteral::as_static)
            .is_some_and(|class| {
                class
                    .reified_type_params(db)
                    .contains(self.typevar(db).name(db))
            })
    }

    /// The variance of this type variable at the position it is bound.
    ///
    /// A declared variance only says something about a generic *class*: it fixes how two
    /// specializations of that class relate. A type variable bound to a function has no such
    /// relation to declare, and a legacy `TypeVar("T")` is nevertheless invariant by python's own
    /// rules, so the declaration is read past and the type variable's position within the
    /// function's own signature answers instead. This keeps `def f[T]() -> T` and its legacy
    /// spelling saying the same thing.
    pub(crate) fn positional_variance(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> TypeVarVariance {
        let BindingContext::Definition(definition) = self.binding_context(db) else {
            return self.variance(db);
        };
        let binding_ty = binding_type(db, definition);
        if binding_ty.is_function_literal() {
            return binding_ty
                .with_polarity(TypeVarVariance::Covariant)
                .variance_of(db, env, self.identity(db))
                .evaluate(db);
        }
        self.variance(db)
    }

    /// basedpython: whether this type parameter belongs to a class rather than to a
    /// function.
    ///
    /// A class type parameter left unsolved by a call is the specialization of the
    /// instance that call builds, and nothing else: the class type parameters of a
    /// method are already fixed by the receiver before the method is bound.
    pub(crate) fn binds_class_specialization(self, db: &'db dyn Db) -> bool {
        let BindingContext::Definition(definition) = self.binding_context(db) else {
            return false;
        };
        binding_type(db, definition).is_class_literal()
    }

    /// basedpython: whether this parameter is declared `in out` on a class whose
    /// body never writes through it.
    ///
    /// Declared variance and literal widening answer different questions.
    /// `in out T` pins the *subtyping* relation between specializations, and
    /// pins it deliberately — it is not the bare `T` beside it, whose variance
    /// is inferred from the body. Widening asks something else: whether a write
    /// can reach the parameter, so that a later write of a different type would
    /// conflict with the literal the first one happened to have. A class that
    /// only takes `T` in `__init__` has no such write under either spelling, so
    /// the literal stands under both — that the declaration also seals the
    /// subtyping relation is beside the point.
    ///
    /// The inferred variance is what answers the write question, which is why
    /// it is consulted here rather than the declared one.
    ///
    /// Confined to `.by`, since `in out` is its syntax. A legacy `TypeVar("T")`
    /// is invariant under python's own rules, and refining that would change
    /// what a `.py` file means.
    pub(crate) fn is_declared_invariant_but_never_written(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        if self.typevar(db).explicit_variance(db) != Some(TypeVarVariance::Invariant) {
            return false;
        }
        let BindingContext::Definition(definition) = self.binding_context(db) else {
            return false;
        };
        if !definition.file(db).source_type(db).is_basedpython() {
            return false;
        }
        binding_type(db, definition)
            .with_polarity(TypeVarVariance::Covariant)
            .variance_of(db, env, self.identity(db))
            .evaluate(db)
            .is_covariant()
    }

    /// Whether a literal type solved for this type parameter has to widen before it can be part
    /// of an inferred declaration.
    ///
    /// Only an invariant or contravariant parameter does: it is written through, so a later write
    /// of a different type would conflict with the one the first write happened to have. Nothing
    /// writes through a covariant parameter (and nothing reads a bivariant one), so the literal
    /// stands. This mirrors how promotion descends into an already-built specialization.
    pub(crate) fn widens_literal_solutions(self, db: &'db dyn Db) -> bool {
        !self.variance(db).is_covariant()
    }

    pub(super) fn apply_type_mapping_impl<'a>(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        type_mapping: &TypeMapping<'a, 'db>,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Type<'db> {
        let mapped_specialization_type =
            |specialization: &ApplySpecialization<'a, 'db>| -> Option<Type<'db>> {
                let typevar = if self.is_paramspec(db) {
                    self.without_paramspec_attr(db)
                } else {
                    self
                };
                specialization.get(db, typevar).map(|ty| {
                    if let Some(attr) = self.paramspec_attr(db)
                        && let Type::TypeVar(typevar) = ty
                        && typevar.is_paramspec(db)
                    {
                        return Type::TypeVar(typevar.with_paramspec_attr(db, attr));
                    }
                    ty
                })
            };

        let possibly_apply_to_self = |specialization: &ApplySpecialization<'a, 'db>| {
            if self.typevar(db).is_self(db)
                && specialization.specialize_self_domain()
                && let Some(specialization) = specialization.as_specialization(db)
            {
                return Type::TypeVar(self.apply_specialization_to_bound_or_constraints(
                    db,
                    specialization,
                    visitor.env,
                ));
            }
            // a type variable the specialization does not name can still be bounded by one it
            // does — `class Owner[T]: def narrow[U: T]`, projected from an `Owner[int]`. the
            // bound is read off the variable wherever it turns up, so the variable itself has to
            // carry the substituted bound
            if self.typevar(db).bound_mentions_typevars(db) {
                return Type::TypeVar(self.with_mapped_bound_and_default(
                    db,
                    env,
                    self.freshness(db),
                    type_mapping,
                    visitor,
                ));
            }
            Type::TypeVar(self)
        };

        match type_mapping {
            TypeMapping::ApplySpecialization(specialization) => {
                mapped_specialization_type(specialization)
                    .unwrap_or_else(|| possibly_apply_to_self(specialization))
            }
            TypeMapping::ProjectUseSiteVariance {
                specialization,
                position,
            } => {
                use crate::types::TypeVarVariance;
                use ruff_python_ast::helpers::UseSiteVariance;

                let Some(value) = mapped_specialization_type(specialization) else {
                    // a projected mapping still crosses the member boundary, so a retained `Self`
                    // has to have its domain rewritten here exactly as it would without projections
                    return possibly_apply_to_self(specialization);
                };
                let projection = specialization
                    .as_specialization(db)
                    .and_then(|specialization| specialization.projection_for(db, self));
                match projection {
                    None | Some(UseSiteVariance::InOut) => value,
                    // an invariant or bivariant occurrence (e.g. the element of a
                    // returned `list[T]`) is sealed: it takes the value unchanged,
                    // in neither the read nor the write direction
                    _ if !matches!(
                        position,
                        TypeVarVariance::Covariant | TypeVarVariance::Contravariant
                    ) =>
                    {
                        value
                    }
                    // `out`: covariant (read) yields the value; contravariant
                    // (write) yields `Never`, so no argument can be written
                    Some(UseSiteVariance::Out) => {
                        if matches!(position, TypeVarVariance::Contravariant) {
                            Type::Never
                        } else {
                            value
                        }
                    }
                    // `in`: contravariant (write) yields the value; covariant
                    // (read) projects through to `object`
                    Some(UseSiteVariance::In) => {
                        if matches!(position, TypeVarVariance::Contravariant) {
                            value
                        } else {
                            Type::object()
                        }
                    }
                }
            }
            TypeMapping::ApplySpecializationWithMaterialization {
                specialization,
                materialization_kind,
            } => mapped_specialization_type(specialization)
                .map(|mapped| {
                    // Only materialize if the specialization actually substituted this
                    // typevar with a different type. A typevar that maps back to itself
                    // hasn't been substituted and should not be materialized.
                    if mapped == Type::TypeVar(self) {
                        mapped
                    } else {
                        let env = visitor.env;
                        // Materialization uses a different mapping mode. Reuse of the outer
                        // visitor can incorrectly hit a cache entry from specialization.
                        let materialization_visitor = visitor.for_new_materialization_root();
                        let materialized = mapped.materialize(
                            db,
                            env,
                            *materialization_kind,
                            &materialization_visitor,
                        );

                        if *materialization_kind == MaterializationKind::Top
                            && !materialization_visitor.is_equivalent_to_materialization(
                                db,
                                mapped,
                                materialized,
                            )
                            && let Some(upper_bound) = self.top_materialized_upper_bound(db)
                        {
                            IntersectionType::from_two_elements(db, env, materialized, upper_bound)
                        } else {
                            materialized
                        }
                    }
                })
                .unwrap_or_else(|| possibly_apply_to_self(specialization)),
            TypeMapping::BindSelf(binding) => {
                if binding.should_bind(db, visitor.env, self) {
                    binding.self_type()
                } else if self.bounds_mention_self(db, env) {
                    // a type variable can be bounded by `Self` (`def method[T: Self]`). that bound
                    // only constrains anything once its `Self` has been bound to the receiver too
                    Type::TypeVar(self.with_mapped_bound_and_default(
                        db,
                        env,
                        self.freshness(db),
                        type_mapping,
                        visitor,
                    ))
                } else {
                    Type::TypeVar(self)
                }
            }
            TypeMapping::ReplaceSelf { new_upper_bound } => {
                if self.typevar(db).is_self(db) {
                    Type::TypeVar(BoundTypeVarInstance::synthetic_self(
                        db,
                        *new_upper_bound,
                        self.binding_context(db),
                    ))
                } else {
                    Type::TypeVar(self)
                }
            }
            TypeMapping::FreshenBoundTypeVars {
                generic_context,
                delta,
            } => {
                if generic_context.contains(db, self.identity(db)) && !self.is_parameter_pack(db) {
                    Type::TypeVar(self.with_mapped_bound_and_default(
                        db,
                        env,
                        self.freshness(db).add(*delta),
                        type_mapping,
                        visitor,
                    ))
                } else {
                    Type::TypeVar(self)
                }
            }
            TypeMapping::Promote(..)
            | TypeMapping::ReplaceParameterDefaults
            | TypeMapping::BindLegacyTypevars(_)
            | TypeMapping::EagerExpansion
            | TypeMapping::RescopeReturnCallables(_)
            | TypeMapping::AttachRegexGroups(_) => Type::TypeVar(self),
            TypeMapping::Materialize(materialization_kind) => {
                if visitor.materialize_typevar_bounds_and_defaults {
                    Type::TypeVar(self.materialize_impl(db, env, *materialization_kind, visitor))
                } else {
                    Type::TypeVar(self)
                }
            }
        }
    }

    /// Returns the static upper bound used when materializing a gradual type argument.
    ///
    /// Constraints are unioned only when materializing an exposed member, where their union is a
    /// valid conservative upper bound. A bound may recursively refer to its own generic class,
    /// either directly or through other bounds. Such a bound has no finite static top
    /// materialization, so recover from its cycle without applying an upper bound.
    pub(super) fn top_materialized_upper_bound(self, db: &'db dyn Db) -> Option<Type<'db>> {
        #[salsa::tracked(
            returns(copy),
            cycle_result=|_, _, _| None,
            heap_size=ruff_memory_usage::heap_size
        )]
        fn top_materialized_upper_bound_inner<'db>(
            db: &'db dyn Db,
            bound_typevar: BoundTypeVarInstance<'db>,
        ) -> Option<Type<'db>> {
            let env =
                ProgramEnvironment::from_program(bound_typevar.binding_context(db).program(db));

            // every caller intersects the result into a *specialization*, so a bound naming
            // another type variable has nothing to offer here: substituting it would leave that
            // variable in the specialization
            if bound_typevar.typevar(db).bound_mentions_typevars(db) {
                return None;
            }

            bound_typevar
                .typevar(db)
                .bound_or_constraints(db, &env)
                .map(|bound_or_constraints| {
                    bound_or_constraints
                        .as_type(db, &env)
                        .top_materialization(db, &env)
                })
        }

        top_materialized_upper_bound_inner(db, self)
    }
}

pub(super) fn walk_bound_type_var_type<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    bound_typevar: BoundTypeVarInstance<'db>,
    visitor: &V,
) {
    visitor.visit_type_var_type(db, bound_typevar.typevar(db));
}

impl<'db> BoundTypeVarInstance<'db> {
    /// Returns the default value of this typevar, recursively applying its binding context to any
    /// other typevars that appear in the default.
    ///
    /// For instance, in
    ///
    /// ```py
    /// T = TypeVar("T")
    /// U = TypeVar("U", default=T)
    ///
    /// # revealed: typing.TypeVar[U = typing.TypeVar[T]]
    /// reveal_type(U)
    ///
    /// # revealed: typing.Generic[T, U = T@C]
    /// class C(reveal_type(Generic[T, U])): ...
    /// ```
    ///
    /// In the first case, the use of `U` is unbound, and so we have a `TypeVarInstance`, and its
    /// default value (`T`) is also unbound.
    ///
    /// By using `U` in the generic class, it becomes bound, and so we have a
    /// `BoundTypeVarInstance`. As part of binding `U` we must also bind its default value
    /// (resulting in `T@C`).
    pub fn default_type(self, db: &'db dyn Db) -> Option<Type<'db>> {
        bound_typevar_default_type(db, self)
    }

    fn materialize_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        materialization_kind: MaterializationKind,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Self {
        Self::new(
            db,
            self.typevar(db)
                .materialize_impl(db, env, materialization_kind, visitor),
            self.binding_context(db),
            self.paramspec_attr(db),
            self.freshness(db),
        )
    }

    /// Whether any end of this type variable's bound, or any of its constraints, mentions `Self`.
    ///
    /// A lazily-evaluated bound is invisible to [`Type::contains_self`], so callers that need to
    /// know whether `Self` binding has any work to do must ask this separately. That covers the
    /// lower end of a basedpython bound range (`def method[T: Self..object]`), which is lazy too
    pub(crate) fn bounds_mention_self(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        let typevar = self.typevar(db);
        if typevar
            .lower_bound(db)
            .is_some_and(|lower_bound| lower_bound.contains_self(db, env))
        {
            return true;
        }
        match typevar.bound_or_constraints(db, env) {
            None => false,
            Some(TypeVarBoundOrConstraints::UpperBound(bound)) => bound.contains_self(db, env),
            Some(TypeVarBoundOrConstraints::Constraints(constraints)) => constraints
                .elements(db)
                .iter()
                .any(|constraint| constraint.contains_self(db, env)),
        }
    }

    /// Rewrite this type variable's bound, constraints, and default through `type_mapping`,
    /// and give the result `nonce` as its freshness.
    fn with_mapped_bound_and_default(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        nonce: TypeVarNonce,
        type_mapping: &TypeMapping<'_, 'db>,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Self {
        let typevar = self.typevar(db);
        let bound_or_constraints = typevar.bound_or_constraints(db, env);
        let lower_bound = typevar.lower_bound(db);
        let default = self.default_type(db);

        if bound_or_constraints.is_none() && lower_bound.is_none() && default.is_none() {
            return Self::new(
                db,
                typevar,
                self.binding_context(db),
                self.paramspec_attr(db),
                nonce,
            );
        }

        let typevar = TypeVarInstance::new(
            db,
            typevar.identity(db),
            bound_or_constraints.map(|bound_or_constraints| {
                bound_or_constraints
                    .apply_type_mapping_impl(db, env, type_mapping, visitor)
                    .into()
            }),
            lower_bound.map(|ty| {
                ty.apply_type_mapping_impl(db, env, type_mapping, TypeContext::default(), visitor)
                    .into()
            }),
            typevar.explicit_variance(db),
            default.map(|ty| {
                ty.apply_type_mapping_impl(db, env, type_mapping, TypeContext::default(), visitor)
                    .into()
            }),
        );

        Self::new(
            db,
            typevar,
            self.binding_context(db),
            self.paramspec_attr(db),
            nonce,
        )
    }

    pub(super) fn to_instance(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<InstanceProjection<Self>> {
        Some(self.typevar(db).to_instance(db, env)?.map(|typevar| {
            Self::new(
                db,
                typevar,
                self.binding_context(db),
                self.paramspec_attr(db),
                self.freshness(db),
            )
        }))
    }
}

/// Whether this typevar was created via the legacy `TypeVar` constructor, using PEP 695 syntax,
/// or an implicit typevar like `Self` was used.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, get_size2::GetSize)]
pub enum TypeVarKind {
    /// `T = TypeVar("T")`
    LegacyTypeVar,
    /// `def foo[T](x: T) -> T: ...`
    Pep695TypeVar,
    /// `typing.Self`
    TypingSelf,
    /// `P = ParamSpec("P")`
    LegacyParamSpec,
    /// `def foo[**P]() -> None: ...`
    Pep695ParamSpec,
    /// `Ts = TypeVarTuple("Ts")`
    LegacyTypeVarTuple,
    /// `def foo[*Ts]() -> None: ...`
    Pep695TypeVarTuple,
    /// basedpython `class A[**Kwargs]: ...`
    ///
    /// in a basedpython file `**Name` is a *keyword-variadic pack* — an ordered
    /// mapping of parameter name to type — not a `ParamSpec`. it shares the
    /// `ParamSpec` value representation (a callable-shaped value) but is
    /// specialized by keyword (`A[foo=int, bar=str]`) and has no
    /// `.args`/`.kwargs` components
    Pep695KeywordVariadic,
    /// `Alias: typing.TypeAlias = T`
    Pep613Alias,
    /// basedpython: the anonymous type parameter an unannotated `def f(x)` parameter opens
    /// under `sound-types`.
    ///
    /// It is a `some` hole that nobody wrote: named after the parameter, bound by whatever the
    /// function turns out to require of it, and defaulted to the parameter's default value.
    InferredParameter,
}

impl TypeVarKind {
    /// The kind declared by a PEP-695 `**Name` type parameter.
    ///
    /// basedpython spells its keyword-variadic packs with the same `**Name` syntax python uses
    /// for `ParamSpec`, so the declaring file decides which one it is.
    ///
    /// Stubs are excluded: `.byi` is the interop surface with python's typing ecosystem, and the
    /// vendored typeshed is machine-converted from upstream, where `**P` means `ParamSpec`. A
    /// `ParamSpec` generic can still be declared in `.by` with the legacy `P = ParamSpec("P")`
    /// form.
    pub(super) const fn double_starred_type_param(source_type: PySourceType) -> Self {
        match source_type {
            PySourceType::BasedPython => Self::Pep695KeywordVariadic,
            _ => Self::Pep695ParamSpec,
        }
    }

    const fn is_paramspec(self) -> bool {
        matches!(self, Self::LegacyParamSpec | Self::Pep695ParamSpec)
    }

    pub(crate) const fn is_keyword_variadic(self) -> bool {
        matches!(self, Self::Pep695KeywordVariadic)
    }

    /// Whether this typevar is specialized by a *parameter list* rather than by a type, and so
    /// carries the callable-shaped value representation built by
    /// [`Type::paramspec_value_callable`](crate::types::Type::paramspec_value_callable).
    ///
    /// [`ParamSpec`](Self::is_paramspec) and basedpython's
    /// [keyword-variadic packs](Self::is_keyword_variadic) differ in how that parameter list is
    /// spelled and used, but agree on how it is stored.
    pub(super) const fn is_parameter_pack(self) -> bool {
        self.is_paramspec() || self.is_keyword_variadic()
    }

    pub(super) const fn is_typevartuple(self) -> bool {
        matches!(self, Self::LegacyTypeVarTuple | Self::Pep695TypeVarTuple)
    }

    const fn is_typing_self(self) -> bool {
        matches!(self, Self::TypingSelf)
    }
}

/// The identity of a type variable.
///
/// This represents the core identity of a typevar, independent of its bounds or constraints. Two
/// typevars have the same identity if they represent the same logical typevar, even if their
/// bounds have been materialized differently.
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct TypeVarIdentity<'db> {
    /// The name of this TypeVar (e.g. `T`)
    #[returns(ref)]
    pub(crate) name: Name,

    /// The type var's definition (None if synthesized)
    #[returns(copy)]
    pub(crate) definition: Option<Definition<'db>>,

    /// The kind of typevar (PEP 695, Legacy, or TypingSelf)
    #[returns(copy)]
    pub(crate) kind: TypeVarKind,
}

impl get_size2::GetSize for TypeVarIdentity<'_> {}

impl<'db> TypeVarIdentity<'db> {
    fn with_name_suffix(self, db: &'db dyn Db, suffix: &str) -> Self {
        let name = Name::concat(&[self.name(db).as_str(), "'", suffix]);
        Self::new(db, name, self.definition(db), self.kind(db))
    }
}

#[expect(clippy::ref_option)]
fn lazy_lower_bound_cycle_recover<'db>(
    db: &'db dyn Db,
    cycle: &salsa::Cycle,
    previous: &Option<Type<'db>>,
    current: Option<Type<'db>>,
    typevar: TypeVarInstance<'db>,
) -> Option<Type<'db>> {
    // Normalize the bound to ensure cycle convergence.
    match (previous, current) {
        (Some(prev), Some(current)) => {
            let env = &ProgramEnvironment::from_definition(typevar.definition(db)?);
            Some(current.cycle_normalized(db, env, *prev, cycle))
        }
        (None, Some(current)) => {
            let env = &ProgramEnvironment::from_definition(typevar.definition(db)?);
            Some(current.recursive_type_normalized(db, env, cycle))
        }
        (_, None) => None,
    }
}

#[expect(clippy::ref_option)]
fn lazy_bound_cycle_recover<'db>(
    db: &'db dyn Db,
    cycle: &salsa::Cycle,
    previous: &Option<Type<'db>>,
    current: Option<Type<'db>>,
    typevar: TypeVarInstance<'db>,
) -> Option<Type<'db>> {
    // Normalize the bounds/constraints to ensure cycle convergence.
    let current = current?;
    let program_file = typevar
        .definition(db)
        .expect("a lazy TypeVar bound must have a source definition")
        .program_file(db);
    let env = ProgramEnvironment::from_file(program_file);
    Some(match previous {
        Some(prev) => current.cycle_normalized(db, &env, *prev, cycle),
        None => current.recursive_type_normalized(db, &env, cycle),
    })
}

#[allow(clippy::trivially_copy_pass_by_ref)]
#[expect(clippy::ref_option)]
fn lazy_constraints_cycle_recover<'db>(
    db: &'db dyn Db,
    cycle: &salsa::Cycle,
    previous: &Option<TypeVarConstraints<'db>>,
    current: Option<TypeVarConstraints<'db>>,
    typevar: TypeVarInstance<'db>,
) -> Option<TypeVarConstraints<'db>> {
    // Normalize the bounds/constraints to ensure cycle convergence.
    let current = current?;
    let program_file = typevar
        .definition(db)
        .expect("lazy TypeVar constraints must have a source definition")
        .program_file(db);
    let env = ProgramEnvironment::from_file(program_file);
    Some(match previous {
        Some(prev) => current.cycle_normalized(db, &env, *prev, cycle),
        None => current.recursive_type_normalized(db, &env, cycle),
    })
}

#[expect(clippy::ref_option)]
fn lazy_default_cycle_recover<'db>(
    db: &'db dyn Db,
    cycle: &salsa::Cycle,
    previous_default: &Option<Type<'db>>,
    current: Option<Type<'db>>,
    typevar: TypeVarInstance<'db>,
) -> Option<Type<'db>> {
    // Normalize the default to ensure cycle convergence.
    let current = current?;
    let program_file = typevar
        .definition(db)
        .expect("a lazy TypeVar default must have a source definition")
        .program_file(db);
    let env = ProgramEnvironment::from_file(program_file);
    Some(match previous_default {
        Some(prev) => current.cycle_normalized(db, &env, *prev, cycle),
        None => current.recursive_type_normalized(db, &env, cycle),
    })
}

/// Where a type variable is bound and usable.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub enum BindingContext<'db> {
    /// The definition of the generic class, function, or type alias that binds this typevar.
    Definition(Definition<'db>),
    /// The typevar is synthesized internally, and is not associated with a particular definition
    /// in the source, but is still bound and eligible for specialization inference. Its program
    /// identifies the environment that cannot otherwise be recovered from a source definition.
    Synthetic(Program<'db>),
}

impl<'db> From<Definition<'db>> for BindingContext<'db> {
    fn from(definition: Definition<'db>) -> Self {
        BindingContext::Definition(definition)
    }
}

impl<'db> BindingContext<'db> {
    pub(crate) fn definition(self) -> Option<Definition<'db>> {
        match self {
            BindingContext::Definition(definition) => Some(definition),
            BindingContext::Synthetic(_) => None,
        }
    }

    fn program(self, db: &'db dyn Db) -> Program<'db> {
        match self {
            Self::Definition(definition) => definition.program(db),
            Self::Synthetic(program) => program,
        }
    }

    pub(super) fn name(self, db: &'db dyn Db) -> Option<String> {
        self.definition().and_then(|definition| definition.name(db))
    }
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum ParamSpecAttrKind {
    Args,
    Kwargs,
}

impl ParamSpecAttrKind {
    /// Returns the component represented by a `ParamSpec` attribute name.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "args" => Some(Self::Args),
            "kwargs" => Some(Self::Kwargs),
            _ => None,
        }
    }
}

impl std::fmt::Display for ParamSpecAttrKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamSpecAttrKind::Args => f.write_str("args"),
            ParamSpecAttrKind::Kwargs => f.write_str("kwargs"),
        }
    }
}

/// The identity of a bound type variable occurrence.
///
/// This identifies a specific binding of a typevar to a context (e.g., `T@ClassC` vs `T@FunctionF`),
/// plus a freshness nonce for fresh callable occurrences, independent of the typevar's
/// bounds or constraints. Two bound typevars have the same identity if they represent the same
/// occurrence, even if their bounds have been materialized differently. Two fresh occurrences of
/// the same source-level typevar have different bound identities.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub struct BoundTypeVarIdentity<'db> {
    pub(crate) identity: TypeVarIdentity<'db>,
    pub(crate) binding_context: BindingContext<'db>,
    /// If [`Some`], this indicates that this type variable is the `args` or `kwargs` component
    /// of a `ParamSpec` i.e., `P.args` or `P.kwargs`.
    pub(super) paramspec_attr: Option<ParamSpecAttrKind>,
    /// The freshness nonce for this bound typevar occurrence; `0` is the source-level occurrence.
    freshness: TypeVarNonce,
}

impl<'db> BoundTypeVarIdentity<'db> {
    fn kind(self, db: &'db dyn Db) -> TypeVarKind {
        self.identity.kind(db)
    }

    pub(crate) fn is_paramspec(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_paramspec()
    }

    fn is_parameter_pack(self, db: &'db dyn Db) -> bool {
        self.kind(db).is_parameter_pack()
    }

    pub(crate) fn without_paramspec_attr(mut self, db: &'db dyn Db) -> Self {
        debug_assert!(
            self.is_parameter_pack(db),
            "Expected a parameter pack, got {:?}",
            self.kind(db)
        );

        self.paramspec_attr = None;
        self
    }
}

/// A set of bound typevar occurrences.
///
/// Membership is keyed by [`BoundTypeVarIdentity`], including any freshness nonce, while the first
/// bound instance encountered for each identity is retained. This lets a fresh generic-callable
/// occurrence be inferable without making the surrounding source-level typevar inferable.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum TypeVarSet<'db> {
    None,
    Some(TypeVarSetInner<'db>),
}

impl<'db> TypeVarSet<'db> {
    pub(crate) fn from_typevars(
        db: &'db dyn Db,
        typevars: impl IntoIterator<Item = BoundTypeVarInstance<'db>>,
    ) -> Self {
        let mut typevars = typevars.into_iter().peekable();
        if typevars.peek().is_none() {
            return TypeVarSet::None;
        }

        let mut set = FxOrderMap::default();
        for typevar in typevars {
            set.entry(typevar.identity(db)).or_insert(typevar);
        }
        set.shrink_to_fit();
        Self::Some(TypeVarSetInner::new_internal(db, set))
    }
}

#[salsa::interned(debug, constructor=new_internal, heap_size=ruff_memory_usage::heap_size)]
pub(crate) struct TypeVarSetInner<'db> {
    #[returns(ref)]
    typevars: FxOrderMap<BoundTypeVarIdentity<'db>, BoundTypeVarInstance<'db>>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for TypeVarSetInner<'_> {}

impl<'db> BoundTypeVarIdentity<'db> {
    pub(crate) fn is_inferable(self, db: &'db dyn Db, inferable: TypeVarSet<'db>) -> bool {
        match inferable {
            TypeVarSet::None => false,
            TypeVarSet::Some(inner) => inner.typevars(db).contains_key(&self),
        }
    }
}

impl<'db> BoundTypeVarInstance<'db> {
    pub(crate) fn is_inferable(self, db: &'db dyn Db, inferable: TypeVarSet<'db>) -> bool {
        self.identity(db).is_inferable(db, inferable)
    }
}

impl<'db> TypeVarSet<'db> {
    pub(crate) fn merge(self, db: &'db dyn Db, other: Self) -> Self {
        #[salsa::tracked(returns(copy), heap_size=ruff_memory_usage::heap_size)]
        fn merge_inner<'db>(
            db: &'db dyn Db,
            self_inner: TypeVarSetInner<'db>,
            other_inner: TypeVarSetInner<'db>,
        ) -> TypeVarSet<'db> {
            TypeVarSet::from_typevars(
                db,
                self_inner
                    .typevars(db)
                    .values()
                    .chain(other_inner.typevars(db).values())
                    .copied(),
            )
        }

        match (self, other) {
            (TypeVarSet::None, other) | (other, TypeVarSet::None) => other,
            (TypeVarSet::Some(self_inner), TypeVarSet::Some(other_inner)) => {
                merge_inner(db, self_inner, other_inner)
            }
        }
    }

    // This is not an IntoIterator implementation because I have no desire to try to name the
    // iterator type.
    pub(crate) fn iter(
        self,
        db: &'db dyn Db,
    ) -> impl Iterator<Item = BoundTypeVarInstance<'db>> + 'db {
        match self {
            TypeVarSet::None => Either::Left(std::iter::empty()),
            TypeVarSet::Some(inner) => Either::Right(inner.typevars(db).values().copied()),
        }
    }

    // Keep this around for debugging purposes
    #[cfg_attr(not(test), expect(dead_code))]
    fn display(self, db: &'db dyn Db) -> String {
        format!(
            "[{}]",
            self.iter(db)
                .map(|typevar| typevar.identity(db).display(db))
                .format(", ")
        )
    }
}

#[salsa::tracked(
    returns(copy),
    cycle_initial=|_, id, _| Some(Type::divergent(id)),
    cycle_fn=bound_typevar_default_type_cycle_recover,
    heap_size=ruff_memory_usage::heap_size
)]
fn bound_typevar_default_type<'db>(
    db: &'db dyn Db,
    bound_typevar: BoundTypeVarInstance<'db>,
) -> Option<Type<'db>> {
    let typevar = bound_typevar.typevar(db);
    typevar._default(db)?;
    let definition = typevar
        .definition(db)
        .expect("a bound TypeVar with a default must have a source definition");
    let env = ProgramEnvironment::from_definition(definition);
    let default = typevar.default_type(db, &env)?;
    let binding_context = bound_typevar.binding_context(db);

    Some(default.apply_type_mapping(
        db,
        &env,
        &TypeMapping::BindLegacyTypevars(binding_context),
        TypeContext::default(),
    ))
}

#[expect(clippy::ref_option)]
fn bound_typevar_default_type_cycle_recover<'db>(
    db: &'db dyn Db,
    cycle: &salsa::Cycle,
    previous_default: &Option<Type<'db>>,
    default: Option<Type<'db>>,
    bound_typevar: BoundTypeVarInstance<'db>,
) -> Option<Type<'db>> {
    let default = default?;
    let program_file = bound_typevar
        .typevar(db)
        .definition(db)
        .expect("a bound TypeVar with a default must have a source definition")
        .program_file(db);
    let env = ProgramEnvironment::from_file(program_file);
    Some(match previous_default {
        Some(previous) => default.cycle_normalized(db, &env, *previous, cycle),
        None => default.recursive_type_normalized(db, &env, cycle),
    })
}

/// Whether a typevar's basedpython lower bound is eagerly specified or lazily evaluated.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum TypeVarLowerBoundEvaluation<'db> {
    /// The lower bound is lazily evaluated.
    Lazy,
    /// The lower bound is eagerly specified.
    Eager(Type<'db>),
}

impl<'db> From<Type<'db>> for TypeVarLowerBoundEvaluation<'db> {
    fn from(value: Type<'db>) -> Self {
        TypeVarLowerBoundEvaluation::Eager(value)
    }
}

/// Whether a typevar default is eagerly specified or lazily evaluated.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum TypeVarDefaultEvaluation<'db> {
    /// The default type is lazily evaluated.
    Lazy,
    /// The default type is eagerly specified.
    Eager(Type<'db>),
}

impl<'db> From<Type<'db>> for TypeVarDefaultEvaluation<'db> {
    fn from(value: Type<'db>) -> Self {
        TypeVarDefaultEvaluation::Eager(value)
    }
}

/// Whether a typevar bound/constraints is eagerly specified or lazily evaluated.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum TypeVarBoundOrConstraintsEvaluation<'db> {
    /// There is a lazily-evaluated upper bound.
    LazyUpperBound,
    /// There is a lazily-evaluated set of constraints.
    LazyConstraints,
    /// The upper bound/constraints are eagerly specified.
    Eager(TypeVarBoundOrConstraints<'db>),
}

impl<'db> From<TypeVarBoundOrConstraints<'db>> for TypeVarBoundOrConstraintsEvaluation<'db> {
    fn from(value: TypeVarBoundOrConstraints<'db>) -> Self {
        TypeVarBoundOrConstraintsEvaluation::Eager(value)
    }
}

/// Type variable constraints (e.g. `T: (int, str)`).
/// This is structurally identical to [`UnionType`], except that it does not perform simplification and preserves the element types.
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct TypeVarConstraints<'db> {
    #[returns(ref)]
    pub(super) elements: Box<[Type<'db>]>,
}

impl get_size2::GetSize for TypeVarConstraints<'_> {}

fn walk_type_var_constraints<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    constraints: TypeVarConstraints<'db>,
    visitor: &V,
) {
    for ty in constraints.elements(db) {
        visitor.visit_type(db, *ty);
    }
}

impl<'db> TypeVarConstraints<'db> {
    pub(super) fn as_type(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        UnionType::from_elements(db, env, self.elements(db))
    }

    fn to_instance(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> Option<InstanceProjection<TypeVarConstraints<'db>>> {
        let mut instance_elements = Vec::new();
        let mut is_exact = true;
        for ty in self.elements(db) {
            let projection = ty.to_instance(db, env)?;
            is_exact &= projection.is_exact();
            instance_elements.push(projection.into_inner());
        }
        Some(InstanceProjection::new(
            TypeVarConstraints::new(db, instance_elements.into_boxed_slice()),
            is_exact,
        ))
    }

    pub(super) fn map(
        self,
        db: &'db dyn Db,
        transform_fn: impl FnMut(&Type<'db>) -> Type<'db>,
    ) -> Self {
        let mapped = self
            .elements(db)
            .iter()
            .map(transform_fn)
            .collect::<Box<_>>();
        TypeVarConstraints::new(db, mapped)
    }

    pub(crate) fn map_with_boundness_and_qualifiers(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        mut transform_fn: impl FnMut(&Type<'db>) -> PlaceAndQualifiers<'db>,
    ) -> PlaceAndQualifiers<'db> {
        let mut builder = UnionBuilder::new(db, env);
        let mut qualifiers = TypeQualifiers::empty();

        let mut all_unbound = true;
        let mut possibly_unbound = false;
        let mut origin = TypeOrigin::Declared;
        for ty in self.elements(db) {
            let PlaceAndQualifiers {
                place: ty_member,
                qualifiers: new_qualifiers,
            } = transform_fn(ty);
            qualifiers |= new_qualifiers;
            match ty_member {
                Place::Undefined => {
                    possibly_unbound = true;
                }
                Place::Defined(DefinedPlace {
                    ty: ty_member,
                    origin: member_origin,
                    definedness: member_boundness,
                    ..
                }) => {
                    origin = origin.merge(member_origin);
                    if member_boundness == Definedness::PossiblyUndefined {
                        possibly_unbound = true;
                    }

                    all_unbound = false;
                    builder = builder.add(ty_member);
                }
            }
        }
        PlaceAndQualifiers {
            place: if all_unbound {
                Place::Undefined
            } else {
                Place::Defined(DefinedPlace {
                    ty: builder.build(),
                    origin,
                    definedness: if possibly_unbound {
                        Definedness::PossiblyUndefined
                    } else {
                        Definedness::AlwaysDefined
                    },
                    public_type_policy: PublicTypePolicy::Raw,
                    provenance: Provenance::Unknown,
                })
            },
            qualifiers,
        }
    }

    fn materialize_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        materialization_kind: MaterializationKind,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Self {
        let materialized = self
            .elements(db)
            .iter()
            .map(|ty| ty.materialize(db, env, materialization_kind, visitor))
            .collect::<Box<_>>();
        TypeVarConstraints::new(db, materialized)
    }

    fn apply_type_mapping_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        type_mapping: &TypeMapping<'_, 'db>,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Self {
        let mapped = self
            .elements(db)
            .iter()
            .map(|ty| {
                ty.apply_type_mapping_impl(db, env, type_mapping, TypeContext::default(), visitor)
            })
            .collect::<Box<_>>();
        TypeVarConstraints::new(db, mapped)
    }

    /// Normalize for cycle recovery by combining with the previous value and
    /// removing divergent types introduced by the cycle.
    ///
    /// See [`Type::cycle_normalized`] for more details on how this works.
    fn cycle_normalized(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        previous: Self,
        cycle: &salsa::Cycle,
    ) -> Self {
        let current_elements = self.elements(db);
        let prev_elements = previous.elements(db);
        TypeVarConstraints::new(
            db,
            current_elements
                .iter()
                .zip(prev_elements.iter())
                .map(|(ty, prev_ty)| ty.cycle_normalized(db, env, *prev_ty, cycle))
                .collect::<Box<_>>(),
        )
    }

    /// Normalize recursive types for cycle recovery when there's no previous value.
    ///
    /// See [`Type::recursive_type_normalized`] for more details.
    fn recursive_type_normalized(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        cycle: &salsa::Cycle,
    ) -> Self {
        self.map(db, |ty| ty.recursive_type_normalized(db, env, cycle))
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum TypeVarBoundOrConstraints<'db> {
    UpperBound(Type<'db>),
    Constraints(TypeVarConstraints<'db>),
}

fn walk_type_var_bounds<'db, V: visitor::TypeVisitor<'db> + ?Sized>(
    db: &'db dyn Db,
    bounds: TypeVarBoundOrConstraints<'db>,
    visitor: &V,
) {
    match bounds {
        TypeVarBoundOrConstraints::UpperBound(bound) => {
            visitor.visit_type(db, bound);
        }
        TypeVarBoundOrConstraints::Constraints(constraints) => {
            walk_type_var_constraints(db, constraints, visitor);
        }
    }
}

impl<'db> TypeVarBoundOrConstraints<'db> {
    fn materialize_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        materialization_kind: MaterializationKind,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Self {
        match self {
            TypeVarBoundOrConstraints::UpperBound(bound) => TypeVarBoundOrConstraints::UpperBound(
                bound.materialize(db, env, materialization_kind, visitor),
            ),
            TypeVarBoundOrConstraints::Constraints(constraints) => {
                TypeVarBoundOrConstraints::Constraints(constraints.materialize_impl(
                    db,
                    env,
                    materialization_kind,
                    visitor,
                ))
            }
        }
    }

    pub(crate) fn apply_type_mapping_impl(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        type_mapping: &TypeMapping<'_, 'db>,
        visitor: &ApplyTypeMappingVisitor<'_, 'db>,
    ) -> Self {
        match self {
            TypeVarBoundOrConstraints::UpperBound(bound) => {
                TypeVarBoundOrConstraints::UpperBound(bound.apply_type_mapping_impl(
                    db,
                    env,
                    type_mapping,
                    TypeContext::default(),
                    visitor,
                ))
            }
            TypeVarBoundOrConstraints::Constraints(constraints) => {
                TypeVarBoundOrConstraints::Constraints(constraints.apply_type_mapping_impl(
                    db,
                    env,
                    type_mapping,
                    visitor,
                ))
            }
        }
    }

    /// Represent the bound/constraints of this typevar as a single type, by unioning constraints.
    ///
    /// Careful with this method! It has both semantic and performance gotchas. Unioning
    /// constraints provides a conservative upper bound, but it loses precision. And for many use
    /// cases, it's more efficient to just map over the constraint types directly, rather than
    /// building a union out of them and mapping over that.
    pub(crate) fn as_type(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> Type<'db> {
        match self {
            TypeVarBoundOrConstraints::UpperBound(bound) => bound,
            TypeVarBoundOrConstraints::Constraints(constraints) => constraints.as_type(db, env),
        }
    }
}

/// basedpython: how a variadic pack's specialization fails the pack's declared upper bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PackBoundViolation<'db> {
    /// A member — an element of a `*Ts`, or a field's type in a `**Kwargs` — is outside the
    /// bound. For a whole-pack `*Ts: *(int, str)` the member is the packed tuple itself.
    Member(Type<'db>),
    /// A whole-pack `**Kwargs: **{"a": int}` names a field the specialization does not have.
    MissingField(Name),
}

impl<'db> PackBoundViolation<'db> {
    /// The bound this violation was measured against. Only ever `None` for a pack with no bound,
    /// which cannot produce a violation in the first place.
    fn bound(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar: BoundTypeVarInstance<'db>,
    ) -> Option<Type<'db>> {
        typevar.typevar(db).pack_bound(db, env)
    }

    pub(crate) fn message(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar: BoundTypeVarInstance<'db>,
    ) -> String {
        let bound = Self::bound(db, env, typevar)
            .map(|bound| bound.display(db, env).to_string())
            .unwrap_or_default();
        let kind = if typevar.is_typevartuple(db) {
            "type variable tuple"
        } else {
            "keyword-variadic pack"
        };
        let name = typevar.identity(db).display(db);
        match self {
            Self::Member(member) => format!(
                "Type `{}` is not assignable to upper bound `{bound}` of {kind} `{name}`",
                member.display(db, env),
            ),
            Self::MissingField(field) => {
                format!("Upper bound `{bound}` of {kind} `{name}` requires a field `{field}`")
            }
        }
    }

    /// Attaches the sub-diagnostic explaining *why* the member is not assignable, when there is
    /// a member to explain.
    pub(in crate::types) fn attach_context(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        typevar: BoundTypeVarInstance<'db>,
        diagnostic: &mut LintDiagnosticGuard<'_, '_>,
    ) {
        let (Self::Member(member), Some(bound)) = (self, Self::bound(db, env, typevar)) else {
            return;
        };
        member
            .assignability_error_context(db, env, bound)
            .attach_to(db, env, diagnostic);
    }
}

/// basedpython: checks a variadic pack's specialization against its declared upper bound.
///
/// The star count in the declaration decides which of two readings applies. An unstarred bound
/// bounds every *member* of the pack — every element of a `*Ts: int`, every field of a
/// `**Kwargs: int`. A starred one bounds the pack *as a whole*: `*Ts: *(int, str)` is an ordinary
/// assignability check against the packed tuple, and `**Kwargs: **{"a": int}` requires every
/// field the bound names to be present with an assignable type — extra fields are what an upper
/// bound permits.
///
/// `provided` is the value bound to the pack: a tuple for a `TypeVarTuple`, a parameter-list
/// callable for a keyword-variadic pack.
pub(crate) fn pack_bound_violation<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    typevar: BoundTypeVarInstance<'db>,
    provided: Type<'db>,
    constraints: &ConstraintSetBuilder<'db>,
    inferable: TypeVarSet<'db>,
) -> Option<PackBoundViolation<'db>> {
    let bound = typevar.typevar(db).pack_bound(db, env)?;
    let outside = |member: Type<'db>, bound: Type<'db>| {
        member
            .when_assignable_to(db, env, bound, constraints, inferable)
            .is_never_satisfied(db, env)
    };

    let whole_pack = typevar.typevar(db).has_whole_pack_bound(db);
    match (whole_pack, typevar.is_typevartuple(db)) {
        (true, true) => outside(provided, bound).then_some(PackBoundViolation::Member(provided)),
        (false, true) => provided
            .exact_tuple_instance_spec(db)
            .and_then(|spec| {
                spec.fixed_elements()
                    .find(|element| outside(**element, bound))
                    .copied()
            })
            .map(PackBoundViolation::Member),
        // an unspecialized, gradual or unknown pack has no fields to check
        (whole_pack, false) => provided.keyword_pack_fields(db).and_then(|fields| {
            if whole_pack {
                // a non-`TypedDict` whole-pack bound is reported where the pack is declared;
                // there is nothing to measure a specialization against here
                let required = bound.as_typed_dict()?.items(db);
                required.iter().find_map(|(name, field)| {
                    match fields.iter().find(|(field_name, _)| *field_name == name) {
                        Some((_, provided_field)) => outside(*provided_field, field.declared_ty)
                            .then(|| PackBoundViolation::Member(*provided_field)),
                        None => Some(PackBoundViolation::MissingField(name.clone())),
                    }
                })
            } else {
                fields
                    .iter()
                    .find(|(_, field)| outside(*field, bound))
                    .map(|(_, field)| PackBoundViolation::Member(*field))
            }
        }),
    }
}

/// A [`CycleDetector`] that is used in `TypeVarInstance::default_type`.
pub(crate) type TypeVarDefaultVisitor<'db> =
    CycleDetector<'db, VisitTypeVarDefault, TypeVarInstance<'db>, Option<Type<'db>>, 6>;
pub(crate) struct VisitTypeVarDefault;

impl<'db> super::cyclic::HasIdentity<'db> for TypeVarInstance<'db> {
    type Id = Self;

    fn to_identity(&self, _db: &'db dyn Db) -> Self::Id {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ruff_db::testing::assert_function_query_was_not_run_by_name;

    use crate::db::tests::setup_db;

    fn bound_typevar<'db>(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        name: &'static str,
        kind: TypeVarKind,
        bound_or_constraints: Option<TypeVarBoundOrConstraintsEvaluation<'db>>,
        freshness: TypeVarNonce,
    ) -> BoundTypeVarInstance<'db> {
        let identity = TypeVarIdentity::new(db, Name::new_static(name), None, kind);
        let typevar = TypeVarInstance::new(
            db,
            identity,
            bound_or_constraints,
            None,
            Some(TypeVarVariance::Invariant),
            None,
        );
        BoundTypeVarInstance::new(
            db,
            typevar,
            BindingContext::Synthetic(env.program(db)),
            None,
            freshness,
        )
    }

    #[test]
    fn typevar_set_empty_set_is_none() {
        let db = setup_db();
        let db = &db;
        let env = db.program_environment();
        let typevar = BoundTypeVarInstance::synthetic(
            db,
            &env,
            Name::new_static("T"),
            TypeVarVariance::Invariant,
        );
        let inferable = TypeVarSet::from_typevars(db, []);

        assert_eq!(inferable, TypeVarSet::None);
        assert_eq!(inferable.iter(db).count(), 0);
        assert!(!typevar.is_inferable(db, inferable));
        assert!(!typevar.identity(db).is_inferable(db, inferable));
    }

    #[test]
    fn typevar_set_keeps_first_instance_for_each_identity() {
        let mut db = setup_db();
        db.clear_salsa_events();
        let env = db.program_environment();

        // The synthetic lazy bound has no definition, so it is equivalent to the implicit
        // `object` upper bound represented eagerly below.
        let lazy = bound_typevar(
            &db,
            &env,
            "T",
            TypeVarKind::Pep695TypeVar,
            Some(TypeVarBoundOrConstraintsEvaluation::LazyUpperBound),
            TypeVarNonce::NONE,
        );
        let eager = bound_typevar(
            &db,
            &env,
            "T",
            TypeVarKind::Pep695TypeVar,
            Some(TypeVarBoundOrConstraints::UpperBound(Type::object()).into()),
            TypeVarNonce::NONE,
        );
        let u = BoundTypeVarInstance::synthetic(
            &db,
            &env,
            Name::new_static("U"),
            TypeVarVariance::Invariant,
        );
        let v = BoundTypeVarInstance::synthetic(
            &db,
            &env,
            Name::new_static("V"),
            TypeVarVariance::Invariant,
        );

        assert_ne!(lazy, eager);
        assert_eq!(lazy.identity(&db), eager.identity(&db));

        let left = TypeVarSet::from_typevars(&db, [lazy, u, eager]);
        let right = TypeVarSet::from_typevars(&db, [eager, v, lazy]);
        let merged = left.merge(&db, right);

        assert_eq!(left.iter(&db).collect::<Vec<_>>(), [lazy, u]);
        assert_eq!(right.iter(&db).collect::<Vec<_>>(), [eager, v]);
        assert_eq!(merged.iter(&db).collect::<Vec<_>>(), [lazy, u, v]);
        assert_eq!(merged, TypeVarSet::from_typevars(&db, [lazy, u, v]));
        assert!(lazy.is_inferable(&db, merged));
        assert!(eager.is_inferable(&db, merged));
        assert_eq!(merged.display(&db), "[T, U, V]");

        let events = db.take_salsa_events();
        assert_function_query_was_not_run_by_name(&db, "lazy_bound_unchecked", None, &events);
    }

    #[test]
    fn typevar_set_distinguishes_fresh_and_paramspec_identities() {
        let db = setup_db();
        let db = &db;
        let env = db.program_environment();
        let typevar = bound_typevar(
            db,
            &env,
            "T",
            TypeVarKind::Pep695TypeVar,
            None,
            TypeVarNonce::NONE,
        );
        let fresh = bound_typevar(
            db,
            &env,
            "T",
            TypeVarKind::Pep695TypeVar,
            None,
            TypeVarNonce::NONE.increment(),
        );
        let paramspec = bound_typevar(
            db,
            &env,
            "P",
            TypeVarKind::Pep695ParamSpec,
            None,
            TypeVarNonce::NONE,
        );
        let args = paramspec.with_paramspec_attr(db, ParamSpecAttrKind::Args);
        let kwargs = paramspec.with_paramspec_attr(db, ParamSpecAttrKind::Kwargs);

        let inferable = TypeVarSet::from_typevars(db, [typevar, fresh, args, kwargs]);
        assert_eq!(
            inferable.iter(db).collect::<Vec<_>>(),
            [typevar, fresh, args, kwargs]
        );
        assert!(typevar.is_inferable(db, inferable));
        assert!(fresh.is_inferable(db, inferable));
        assert!(args.is_inferable(db, inferable));
        assert!(kwargs.is_inferable(db, inferable));
        assert!(!paramspec.is_inferable(db, inferable));

        let paramspec_only = TypeVarSet::from_typevars(db, [paramspec]);
        assert!(
            args.identity(db)
                .without_paramspec_attr(db)
                .is_inferable(db, paramspec_only)
        );
        assert!(
            kwargs
                .identity(db)
                .without_paramspec_attr(db)
                .is_inferable(db, paramspec_only)
        );
    }
}
