//! inference of reified specializations at bare call sites (basedpython)
//!
//! a reified generic called without explicit `f[...]` is still legal when
//! every type parameter solves — from the call's arguments or its pep 696
//! default — to a type with a *runtime spelling*. the transpiler injects that
//! spelling at the call site (`f(1)` → `f[int](1)`), so the checker's
//! acceptance and the transpiler's injection must agree exactly: both sides
//! call [`inferred_call_type_arguments`] with the call's already-inferred
//! argument types, never their own private notion of the solution
//!
//! a spelling is only produced when evaluating it at the call site would
//! yield the intended runtime object: literals promote to their instance
//! class first (`Literal[1]` → `int`), and a class name is used only if the
//! bare name resolves in the module's globals (or builtins) to that same
//! class. anything else — unsolved parameters, dynamic types, scope-local
//! classes, exotic type forms — has no spelling and the bare call stays an
//! error

use itertools::Itertools;
use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, name::Name};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashMap;

use crate::Db;
use crate::place::{builtins_symbol, global_symbol};
use crate::types::call::{Argument, CallArguments};
use crate::types::class::{ClassLiteral, ClassType};
use crate::types::function::FunctionType;
use crate::types::generics::Specialization;
use crate::types::tuple::Tuple;
use crate::types::typevar::TypeVarBoundOrConstraints;
use crate::types::{KnownClass, Type};

/// why a bare call of a reified generic cannot be accepted
pub(crate) enum ReifiedInferenceError<'db> {
    /// the type parameter has no solution from the arguments and no default
    Unsolved(Name),
    /// solved, but the solution has no runtime spelling at this call site
    Unspellable(Name, Type<'db>),
    /// the callable's specialization could not be derived at all (no unique
    /// matching overload, or the arguments do not bind)
    NoBinding,
}

/// The rendered type arguments for a bare call of the reified `function`
/// with the given (already inferred) `arguments`, or the reason none exist.
///
/// `callee` is the called type — the function itself or its bound method —
/// so `self` binding is accounted for.
pub(crate) fn inferred_call_type_arguments<'db>(
    db: &'db dyn Db,
    file: File,
    callee: Type<'db>,
    function: FunctionType<'db>,
    arguments: &CallArguments<'_, 'db>,
) -> Result<Vec<String>, ReifiedInferenceError<'db>> {
    let bindings = callee
        .try_call(db, arguments)
        .map_err(|_| ReifiedInferenceError::NoBinding)?;
    let specialization = bindings
        .single_element()
        .and_then(|callable| callable.matching_overloads().exactly_one().ok())
        .ok_or(ReifiedInferenceError::NoBinding)?
        .1
        .specialization(db);
    rendered_type_arguments(db, file, function, specialization)
}

/// [`inferred_call_type_arguments`] for callers outside the `types` module:
/// arguments arrive as plain types (positional, then keyword) and any failure
/// collapses to `None` — the checker reports those, the caller just skips
/// injection
pub(crate) fn injectable_call_type_arguments<'db>(
    db: &'db dyn Db,
    file: File,
    callee: Type<'db>,
    function: FunctionType<'db>,
    positional: Vec<Type<'db>>,
    keywords: Vec<(&str, Type<'db>)>,
) -> Option<Vec<String>> {
    let arguments: CallArguments<'_, 'db> = positional
        .into_iter()
        .map(|ty| (Argument::Positional, Some(ty)))
        .chain(
            keywords
                .into_iter()
                .map(|(name, ty)| (Argument::Keyword(name), Some(ty))),
        )
        .collect();
    inferred_call_type_arguments(db, file, callee, function, &arguments)
        .ok()
        // an empty prefix means everything defaults — the bare call is
        // already correct and nothing is injected
        .filter(|rendered| !rendered.is_empty())
}

/// The rendered runtime spellings to inject, in declaration order, from the
/// call's solved `specialization`.
///
/// Only the prefix up to the *last argument-solved* type parameter is
/// rendered: trailing parameters that fall back to their pep 696 default
/// need no injection — the wrapper reads defaults off `__type_params__` at
/// runtime — so their defaults never need a spelling. An empty result means
/// the bare call is legal exactly as written (everything defaults).
fn rendered_type_arguments<'db>(
    db: &'db dyn Db,
    file: File,
    function: FunctionType<'db>,
    specialization: Option<Specialization<'db>>,
) -> Result<Vec<String>, ReifiedInferenceError<'db>> {
    let signature = function.signature(db);
    let generic_context = signature
        .overloads
        .first()
        .and_then(|overload| overload.generic_context)
        .ok_or(ReifiedInferenceError::NoBinding)?;

    let solved: &[Type<'db>] = match specialization {
        Some(specialization) if specialization.generic_context(db) == generic_context => {
            specialization.types(db)
        }
        _ => &[],
    };

    let mut last_solved = None;
    let mut resolved: Vec<(&Name, Option<Type<'db>>)> = Vec::with_capacity(generic_context.len(db));
    for (index, bound_typevar) in generic_context.variables(db).enumerate() {
        let solution = solved.get(index).copied().filter(|ty| is_solution(db, *ty));
        if solution.is_some() {
            last_solved = Some(index);
        }
        let typevar = bound_typevar.typevar(db);
        resolved.push((
            typevar.name(db),
            solution.or_else(|| typevar.default_type(db)),
        ));
    }

    // a parameter may stay valueless only when nothing depends on it: an
    // erased parameter outside the injected prefix. a reified parameter
    // without a default always needs a value, and a hole inside the prefix
    // cannot be spelled positionally
    let must_have_value = function.reified_type_params_without_default(db);
    for (index, (name, value)) in resolved.iter().enumerate() {
        if value.is_none()
            && (last_solved.is_some_and(|last| index < last) || must_have_value.contains(name))
        {
            return Err(ReifiedInferenceError::Unsolved((*name).clone()));
        }
    }
    let Some(last_solved) = last_solved else {
        return Ok(Vec::new());
    };

    resolved[..=last_solved]
        .iter()
        .map(|(name, value)| {
            let ty = value.ok_or_else(|| ReifiedInferenceError::Unsolved((*name).clone()))?;
            let promoted = ty.promote(db);
            runtime_spelling(db, file, promoted)
                .ok_or_else(|| ReifiedInferenceError::Unspellable((*name).clone(), promoted))
        })
        .collect()
}

/// whether the solver produced an actual answer for a type parameter —
/// dynamic types and leaked typevars mean "nothing to reify"
fn is_solution<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    let _ = db;
    !matches!(ty, Type::Dynamic(_) | Type::TypeVar(_) | Type::Never)
}

/// A python expression that evaluates, in `file`'s module scope, to the
/// runtime object denoted by `ty` — or `None` when there is no such spelling.
fn runtime_spelling<'db>(db: &'db dyn Db, file: File, ty: Type<'db>) -> Option<String> {
    if ty.is_none(db) {
        return Some("None".to_owned());
    }
    match ty {
        Type::NominalInstance(instance) => spell_class(db, file, instance.class(db)),
        Type::Union(union) => Some(
            union
                .elements(db)
                .iter()
                .map(|element| runtime_spelling(db, file, *element))
                .collect::<Option<Vec<_>>>()?
                .join(" | "),
        ),
        _ => None,
    }
}

fn spell_class<'db>(db: &'db dyn Db, file: File, class: ClassType<'db>) -> Option<String> {
    match class {
        ClassType::NonGeneric(literal) => spell_class_literal(db, file, literal),
        ClassType::Generic(alias) => {
            let origin = ClassLiteral::Static(alias.origin(db));
            let base = spell_class_literal(db, file, origin)?;
            let arguments =
                spell_specialization_arguments(db, file, origin, alias.specialization(db))?;
            Some(format!("{base}[{arguments}]"))
        }
    }
}

/// the comma-joined runtime spellings of a specialization's type arguments —
/// what goes inside a class's `[...]`. tuples carry their precise element
/// shape out-of-band; spell it rather than the class's single typevar
fn spell_specialization_arguments<'db>(
    db: &'db dyn Db,
    file: File,
    origin: ClassLiteral<'db>,
    specialization: Specialization<'db>,
) -> Option<String> {
    if origin.is_known(db, KnownClass::Tuple) {
        return match specialization.tuple(db)? {
            Tuple::Fixed(fixed) => {
                let elements = fixed
                    .elements_slice()
                    .iter()
                    .map(|element| runtime_spelling(db, file, *element))
                    .collect::<Option<Vec<_>>>()?;
                if elements.is_empty() {
                    Some("()".to_owned())
                } else {
                    Some(elements.join(", "))
                }
            }
            Tuple::Variable(_) => None,
        };
    }
    let arguments = specialization
        .types(db)
        .iter()
        .map(|argument| runtime_spelling(db, file, *argument))
        .collect::<Option<Vec<_>>>()?;
    Some(arguments.join(", "))
}

/// The bracketed type-argument spelling to inject at a bare constructor call
/// of the generic class `class_literal` (`A(1)` → `"int"`).
///
/// Read from the (already inferred, literal-promoted) type of the constructed
/// instance, so the injection always matches the checker's solved
/// specialization — including pep 696 defaults and any usage-based widening.
/// `None` when the instance is not a specialization of the called class or an
/// argument has no runtime spelling; unlike a bare reified-generic call this
/// is never an error — the call simply stays bare.
pub(crate) fn constructor_specialization_display<'db>(
    db: &'db dyn Db,
    file: File,
    class_literal: ClassLiteral<'db>,
    constructed: Type<'db>,
) -> Option<String> {
    let Type::NominalInstance(instance) = constructed.promote(db) else {
        return None;
    };
    let ClassType::Generic(alias) = instance.class(db) else {
        return None;
    };
    let origin = ClassLiteral::Static(alias.origin(db));
    if origin != class_literal {
        return None;
    }
    spell_specialization_arguments(db, file, origin, alias.specialization(db))
}

/// The full runtime spelling (`list[int]`, `tuple[int, str]`) with which the
/// transpiler makes a collection literal's inferred element types explicit
/// (`[1, 2]` → `list[int]([1, 2])`).
///
/// `None` when the literal's (literal-promoted) type is not a plain
/// specialization of the expected builtin: empty or partially-`Unknown`
/// elements, a `TypedDict`-typed dict display, a shadowed builtin name, or
/// elements without a runtime spelling.
pub(crate) fn collection_literal_spelling<'db>(
    db: &'db dyn Db,
    file: File,
    literal_ty: Type<'db>,
    expected: KnownClass,
) -> Option<String> {
    let Type::NominalInstance(instance) = literal_ty.promote(db) else {
        return None;
    };
    let class = instance.class(db);
    let ClassType::Generic(alias) = class else {
        return None;
    };
    if !ClassLiteral::Static(alias.origin(db)).is_known(db, expected) {
        return None;
    }
    spell_class(db, file, class)
}

/// the class's bare name, provided that name resolves — in the module's
/// globals, else builtins — to this very class, so the injected expression
/// evaluates to the intended type object
fn spell_class_literal<'db>(
    db: &'db dyn Db,
    file: File,
    literal: ClassLiteral<'db>,
) -> Option<String> {
    let name = literal.name(db);
    let resolved = global_symbol(db, file, name)
        .place
        .ignore_possibly_undefined()
        .or_else(|| builtins_symbol(db, name).place.ignore_possibly_undefined())?;
    let resolved_literal = resolved.as_class_literal()?;
    (resolved_literal == literal).then(|| name.to_string())
}

/// How a parametric type test (`x is C[args]`, keyword form) resolves.
///
/// Rust-style: the test is answered from static types at compile time
/// wherever possible; the runtime residue is an equality check of reified
/// type-param cells, a witness probe, or a `__orig_class__` probe. The last
/// only works when the target's instances carry `__orig_class__` — a
/// user-defined generic. Against a builtin collection, whose instances erase
/// their type arguments, no runtime answer exists and the test is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParametricIsPlan {
    /// statically decided — the test lowers to a constant
    Fold(bool),
    /// conjunction of runtime equality checks of reified type-param cells
    /// against target arguments, each spelled by its source range in the rhs
    TokenEq(Vec<(Name, TextRange)>),
    /// the lhs is a union of same-origin specializations with pairwise
    /// disjoint arguments — one witness element decides which arm this is
    /// (an empty collection has no witness and answers `False`)
    Witness(WitnessPlan),
    /// not decidable from static types, but the target is a user-defined
    /// generic whose instances carry `__orig_class__` — probe it at runtime.
    /// a legitimate, unwarned runtime test
    Probe,
    /// not decidable from static types, and the target is a builtin
    /// collection whose instances never carry `__orig_class__`, so no runtime
    /// probe can succeed — the test can never be true and is an error
    ErasedTarget,
}

/// where the witness element lives and the class (by source range in the
/// rhs) it must be an instance of
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessPlan {
    /// first element of the iterable
    Element { class: TextRange },
    /// first key of the mapping
    DictKey { class: TextRange },
    /// first value of the mapping
    DictValue { class: TextRange },
    /// a fixed tuple position
    TupleIndex { index: usize, class: TextRange },
}

/// whether specialized instances of `origin` carry `__orig_class__` at
/// runtime. the builtin collections are C types that erase their type
/// arguments and reject the attribute; a user-defined generic carries it
/// (set by `types.GenericAlias.__call__` after construction)
fn target_carries_orig_class<'db>(db: &'db dyn Db, origin: ClassLiteral<'db>) -> bool {
    !matches!(
        origin.known(db),
        Some(
            KnownClass::List
                | KnownClass::Dict
                | KnownClass::Set
                | KnownClass::FrozenSet
                | KnownClass::Tuple
        )
    )
}

/// Classify how `lhs is rhs` (keyword form, `rhs` a subscripted generic
/// class evaluating to `rhs_alias`) resolves, from the already-inferred
/// static type of the lhs.
pub(crate) fn classify_parametric_is<'db>(
    db: &'db dyn Db,
    lhs_ty: Type<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    rhs_node: &ast::ExprSubscript,
) -> ParametricIsPlan {
    let target_origin = ClassLiteral::Static(rhs_alias.origin(db));
    let target_args_ast: Vec<&ast::Expr> = match rhs_node.slice.as_ref() {
        ast::Expr::Tuple(tuple) => tuple.elts.iter().collect(),
        single => vec![single],
    };
    let plan = classify_value(
        db,
        lhs_ty.promote(db),
        target_origin,
        rhs_alias,
        &target_args_ast,
        rhs_node,
    );
    // a runtime `__orig_class__` probe is the last resort. it only works when
    // the target's instances carry that attribute — a builtin collection
    // never does, so the test can never be true and becomes an error
    match plan {
        ParametricIsPlan::Probe if !target_carries_orig_class(db, target_origin) => {
            ParametricIsPlan::ErasedTarget
        }
        other => other,
    }
}

fn classify_value<'db>(
    db: &'db dyn Db,
    value_ty: Type<'db>,
    target_origin: ClassLiteral<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    target_args_ast: &[&ast::Expr],
    rhs_node: &ast::ExprSubscript,
) -> ParametricIsPlan {
    match value_ty {
        // `x: T` against `C[args]` — the whole target compares against the
        // reified cell (`T == C[args]`)
        Type::TypeVar(bound_typevar) => {
            if is_reified_function_typevar(db, bound_typevar) {
                ParametricIsPlan::TokenEq(vec![(bound_typevar.name(db).clone(), rhs_node.range())])
            } else {
                ParametricIsPlan::Probe
            }
        }
        Type::NominalInstance(instance) => match instance.class(db) {
            ClassType::Generic(alias)
                if ClassLiteral::Static(alias.origin(db)) == target_origin =>
            {
                let mut tokens = Vec::new();
                match unify_specializations(
                    db,
                    target_origin,
                    alias.specialization(db),
                    rhs_alias.specialization(db),
                    Some(target_args_ast),
                    &mut tokens,
                ) {
                    Ok(()) if tokens.is_empty() => ParametricIsPlan::Fold(true),
                    Ok(()) => ParametricIsPlan::TokenEq(tokens),
                    Err(plan) => plan,
                }
            }
            // a different class: a *disjoint* static type excludes the target
            // (`x: str` is never `list[int]`), but a wider one (`object`, a
            // protocol, a superclass) merely fails to verify it
            _ => {
                let target_instance = Type::instance(db, ClassType::Generic(rhs_alias));
                if value_ty.is_disjoint_from(db, target_instance) {
                    ParametricIsPlan::Fold(false)
                } else {
                    ParametricIsPlan::Probe
                }
            }
        },
        Type::Union(union) => classify_union(db, union, target_origin, rhs_alias, target_args_ast),
        // any other value type — dynamic, a leaked typevar, a protocol — is
        // undecidable statically; `classify_parametric_is` turns this into a
        // runtime probe or an erased-target error depending on the target
        _ => ParametricIsPlan::Probe,
    }
}

/// Unify the value's specialization against the target's, position by
/// position. Equal arguments vanish; a reified type variable in the value's
/// side becomes a runtime token comparison against the target argument's
/// source. `Err` short-circuits with the final plan (a static mismatch folds
/// the whole test to `False`; anything undecidable degrades to a probe).
fn unify_specializations<'db>(
    db: &'db dyn Db,
    origin: ClassLiteral<'db>,
    value_spec: Specialization<'db>,
    target_spec: Specialization<'db>,
    target_args_ast: Option<&[&ast::Expr]>,
    tokens: &mut Vec<(Name, TextRange)>,
) -> Result<(), ParametricIsPlan> {
    if origin.is_known(db, KnownClass::Tuple) {
        return match (value_spec.tuple(db), target_spec.tuple(db)) {
            (Some(Tuple::Fixed(value)), Some(Tuple::Fixed(target))) => {
                if value.elements_slice().len() != target.elements_slice().len() {
                    return Err(ParametricIsPlan::Fold(false));
                }
                for (index, (s, t)) in value
                    .elements_slice()
                    .iter()
                    .zip(target.elements_slice())
                    .enumerate()
                {
                    unify_argument(
                        db,
                        *s,
                        *t,
                        target_args_ast.and_then(|args| args.get(index).copied()),
                        tokens,
                    )?;
                }
                Ok(())
            }
            _ => Err(ParametricIsPlan::Probe),
        };
    }
    let value_types = value_spec.types(db);
    let target_types = target_spec.types(db);
    if value_types.len() != target_types.len() {
        return Err(ParametricIsPlan::Probe);
    }
    for (index, (s, t)) in value_types.iter().zip(target_types).enumerate() {
        unify_argument(
            db,
            *s,
            *t,
            target_args_ast.and_then(|args| args.get(index).copied()),
            tokens,
        )?;
    }
    Ok(())
}

fn unify_argument<'db>(
    db: &'db dyn Db,
    value: Type<'db>,
    target: Type<'db>,
    target_ast: Option<&ast::Expr>,
    tokens: &mut Vec<(Name, TextRange)>,
) -> Result<(), ParametricIsPlan> {
    if value == target || value.is_equivalent_to(db, target) {
        return Ok(());
    }
    if let Type::TypeVar(bound_typevar) = value {
        if !is_reified_function_typevar(db, bound_typevar) {
            return Err(ParametricIsPlan::Probe);
        }
        // a pep 696 default can leave a target position with no source
        // expression to compare against
        let Some(target_ast) = target_ast else {
            return Err(ParametricIsPlan::Probe);
        };
        tokens.push((bound_typevar.name(db).clone(), target_ast.range()));
        return Ok(());
    }
    if value.is_dynamic() || target.is_dynamic() {
        return Err(ParametricIsPlan::Probe);
    }
    // both sides specializations of the same class: recurse structurally
    // (`list[T]` vs the `list[int]` written in the rhs)
    if let (Type::NominalInstance(value_instance), Type::NominalInstance(target_instance)) =
        (value, target)
        && let (ClassType::Generic(value_alias), ClassType::Generic(target_alias)) =
            (value_instance.class(db), target_instance.class(db))
    {
        if value_alias.origin(db) != target_alias.origin(db) {
            return Err(ParametricIsPlan::Fold(false));
        }
        let nested_ast: Option<Vec<&ast::Expr>> =
            if let Some(ast::Expr::Subscript(subscript)) = target_ast {
                Some(match subscript.slice.as_ref() {
                    ast::Expr::Tuple(tuple) => tuple.elts.iter().collect(),
                    single => vec![single],
                })
            } else {
                None
            };
        return unify_specializations(
            db,
            ClassLiteral::Static(value_alias.origin(db)),
            value_alias.specialization(db),
            target_alias.specialization(db),
            nested_ast.as_deref(),
            tokens,
        );
    }
    if value.has_typevar(db) || target.has_typevar(db) {
        return Err(ParametricIsPlan::Probe);
    }
    Err(ParametricIsPlan::Fold(false))
}

/// A union of same-origin specializations is decidable when the target is
/// one of its arms and some argument position discriminates it from every
/// other arm (pairwise disjoint, isinstance-able) — then one witness element
/// answers the test.
fn classify_union<'db>(
    db: &'db dyn Db,
    union: crate::types::UnionType<'db>,
    target_origin: ClassLiteral<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    target_args_ast: &[&ast::Expr],
) -> ParametricIsPlan {
    let mut arm_specs = Vec::new();
    for element in union.elements(db) {
        let Type::NominalInstance(instance) = element else {
            return ParametricIsPlan::Probe;
        };
        let ClassType::Generic(alias) = instance.class(db) else {
            return ParametricIsPlan::Probe;
        };
        if ClassLiteral::Static(alias.origin(db)) != target_origin {
            return ParametricIsPlan::Probe;
        }
        if element.has_typevar(db) {
            return ParametricIsPlan::Probe;
        }
        arm_specs.push(alias.specialization(db));
    }

    let arm_args: Vec<Vec<Type<'db>>> = if target_origin.is_known(db, KnownClass::Tuple) {
        let mut args = Vec::with_capacity(arm_specs.len() + 1);
        for spec in arm_specs.iter().chain([&rhs_alias.specialization(db)]) {
            match spec.tuple(db) {
                Some(Tuple::Fixed(fixed)) => args.push(fixed.elements_slice().to_vec()),
                _ => return ParametricIsPlan::Probe,
            }
        }
        args
    } else {
        arm_specs
            .iter()
            .chain([&rhs_alias.specialization(db)])
            .map(|spec| spec.types(db).to_vec())
            .collect()
    };
    let (target_args, arm_args) = arm_args.split_last().expect("chain includes the target");
    if arm_args.iter().any(|args| args.len() != target_args.len()) {
        return ParametricIsPlan::Probe;
    }
    if target_args.iter().any(Type::is_dynamic) {
        return ParametricIsPlan::Probe;
    }

    let arms_equal_target: Vec<bool> = arm_args
        .iter()
        .map(|args| {
            args.iter()
                .zip(target_args)
                .all(|(a, t)| *a == *t || a.is_equivalent_to(db, *t))
        })
        .collect();
    // the static type excludes the target entirely
    if !arms_equal_target.iter().any(|equal| *equal) {
        return ParametricIsPlan::Fold(false);
    }
    if arms_equal_target.iter().all(|equal| *equal) {
        return ParametricIsPlan::Fold(true);
    }

    // find an argument position where the target's class is disjoint from
    // every non-matching arm's — a single witness there decides the arm
    let discriminant = (0..target_args.len()).find(|&index| {
        let target_arg = target_args[index];
        // the witness check is a runtime isinstance, so the discriminant
        // must be a plain class
        let plain = matches!(
            target_arg,
            Type::NominalInstance(instance)
                if matches!(instance.class(db), ClassType::NonGeneric(_))
        );
        plain
            && target_args_ast.get(index).is_some()
            && arm_args
                .iter()
                .zip(&arms_equal_target)
                .filter(|(_, equal)| !**equal)
                .all(|(args, _)| target_arg.is_disjoint_from(db, args[index]))
    });
    let Some(index) = discriminant else {
        return ParametricIsPlan::Probe;
    };
    let class = target_args_ast[index].range();

    if target_origin.is_known(db, KnownClass::Dict) {
        match index {
            0 => ParametricIsPlan::Witness(WitnessPlan::DictKey { class }),
            _ => ParametricIsPlan::Witness(WitnessPlan::DictValue { class }),
        }
    } else if target_origin.is_known(db, KnownClass::Tuple) {
        ParametricIsPlan::Witness(WitnessPlan::TupleIndex { index, class })
    } else if matches!(
        target_origin.known(db),
        Some(KnownClass::List | KnownClass::Set | KnownClass::FrozenSet)
    ) {
        ParametricIsPlan::Witness(WitnessPlan::Element { class })
    } else {
        // arbitrary generic classes have no canonical element to witness
        ParametricIsPlan::Probe
    }
}

/// whether this type variable has a runtime cell to compare against — a
/// plain type parameter of a function that reifies it
fn is_reified_function_typevar<'db>(
    db: &'db dyn Db,
    bound_typevar: crate::types::typevar::BoundTypeVarInstance<'db>,
) -> bool {
    let crate::types::typevar::BindingContext::Definition(definition) =
        bound_typevar.binding_context(db)
    else {
        return false;
    };
    let def_file = definition.file(db);
    let module = parsed_module(db, def_file).load(db);
    let ty_python_core::definition::DefinitionKind::Function(function) = definition.kind(db) else {
        return false;
    };
    let node = function.node(&module);
    let source = ruff_db::source::source_text(db, def_file);
    crate::reified::reified_type_param_names(source.as_str(), node)
        .iter()
        .any(|name| name == bound_typevar.name(db))
}

/// why an override's reified type-parameter list is incompatible with the
/// base method it overrides
pub(crate) enum ReifiedOverrideError<'db> {
    /// the base reifies its type parameters; the override erases them, so a
    /// specialization through the base would subscript a plain function
    ErasesReified,
    /// the override reifies parameters the base leaves erased, and they have
    /// no defaults a bare call through the base could fall back on
    ReifiesErased(Box<[Name]>),
    /// the override does not accept every type-argument count the base
    /// permits
    Arity {
        base_required: usize,
        base_total: usize,
        sub_required: usize,
        sub_total: usize,
    },
    /// a bound rejects specializations the base permits — bounds are
    /// contravariant
    Bound {
        base_name: Name,
        sub_name: Name,
        base_admissible: Type<'db>,
        sub_admissible: Type<'db>,
    },
}

/// Compatibility of an override's reified type-parameter list with its base
/// method's. Once `[...]` is a runtime step, the type-parameter list is part
/// of the method's interface: callers specialize through the *base* type and
/// dispatch lands on the override, so the override must accept every
/// specialization the base permits. `None` means compatible (or out of this
/// check's scope — plain erased generics, overloads, `*Ts` / `**P` lists).
pub(crate) fn reified_override_error<'db>(
    db: &'db dyn Db,
    base: FunctionType<'db>,
    sub: FunctionType<'db>,
) -> Option<ReifiedOverrideError<'db>> {
    // a reified classmethod is rejected at its own definition
    if base.is_classmethod(db) || sub.is_classmethod(db) {
        return None;
    }
    match (base.is_reified(db), sub.is_reified(db)) {
        (false, false) => None,
        (true, false) => Some(ReifiedOverrideError::ErasesReified),
        (false, true) => {
            let missing = sub.reified_type_params_without_default(db);
            (!missing.is_empty())
                .then(|| ReifiedOverrideError::ReifiesErased(missing.iter().cloned().collect()))
        }
        (true, true) => {
            let base_interface = type_param_interface(db, base)?;
            let sub_interface = type_param_interface(db, sub)?;
            if sub_interface.required > base_interface.required
                || sub_interface.params.len() < base_interface.params.len()
            {
                return Some(ReifiedOverrideError::Arity {
                    base_required: base_interface.required,
                    base_total: base_interface.params.len(),
                    sub_required: sub_interface.required,
                    sub_total: sub_interface.params.len(),
                });
            }
            for ((base_name, base_admissible), (sub_name, sub_admissible)) in
                base_interface.params.iter().zip(&sub_interface.params)
            {
                if !base_admissible.is_assignable_to(db, *sub_admissible) {
                    return Some(ReifiedOverrideError::Bound {
                        base_name: base_name.clone(),
                        sub_name: sub_name.clone(),
                        base_admissible: *base_admissible,
                        sub_admissible: *sub_admissible,
                    });
                }
            }
            None
        }
    }
}

/// the positional type-parameter interface of a reified generic: one
/// `(name, admissible types)` entry per parameter, in declaration order, plus
/// how many have no default. `None` when the shape is outside this check —
/// overloaded functions, or `*Ts` / `**P` in the list (never reified)
struct TypeParamInterface<'db> {
    required: usize,
    params: Vec<(Name, Type<'db>)>,
}

fn type_param_interface<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
) -> Option<TypeParamInterface<'db>> {
    let signature = function.signature(db);
    let [overload] = signature.overloads.as_ref() else {
        return None;
    };
    let overload_literal = function.literal(db).last_definition;

    // admissible specializations per parameter name: the bound, the union of
    // the constraints, or `object` when unconstrained
    let admissible_by_name: FxHashMap<&Name, Type<'db>> = overload
        .generic_context
        .map(|generic_context| {
            generic_context
                .variables(db)
                .map(|bound_typevar| {
                    let typevar = bound_typevar.typevar(db);
                    let admissible = match typevar.bound_or_constraints(db) {
                        Some(TypeVarBoundOrConstraints::UpperBound(bound)) => bound,
                        Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                            constraints.as_type(db)
                        }
                        None => Type::object(),
                    };
                    (typevar.name(db), admissible)
                })
                .collect()
        })
        .unwrap_or_default();

    let module = parsed_module(db, overload_literal.file(db)).load(db);
    let node = overload_literal
        .body_scope(db)
        .node(db)
        .expect_function()
        .node(&module);
    let type_params = node.type_params.as_deref()?;

    let mut required = 0;
    let mut params = Vec::with_capacity(type_params.type_params.len());
    for type_param in &type_params.type_params {
        let ast::TypeParam::TypeVar(typevar) = type_param else {
            return None;
        };
        if typevar.default.is_none() {
            required += 1;
        }
        let admissible = admissible_by_name
            .get(&typevar.name.id)
            .copied()
            .unwrap_or_else(Type::object);
        params.push((typevar.name.id.clone(), admissible));
    }
    Some(TypeParamInterface { required, params })
}
