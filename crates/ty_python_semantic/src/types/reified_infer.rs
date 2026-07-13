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
