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
use crate::types::class::{ClassLiteral, ClassType, GenericAlias};
use crate::types::function::FunctionType;
use crate::types::generics::{Specialization, combine_use_site_projections};
use crate::types::literal::LiteralValueTypeKind;
use crate::types::protocol_class::ReifiedMember;
use crate::types::tuple::Tuple;
use crate::types::typevar::{TypeVarBoundOrConstraints, TypeVarKind};
use crate::types::variance::TypeVarVariance;
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
) -> Result<Vec<TypeArgument>, ReifiedInferenceError<'db>> {
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
/// arguments arrive as plain types (positional, then keyword), and the result
/// is the *source text* of the specialization step to splice in after the
/// callee — `[int, str]`, or the `.__getitem__(…)` call form when a
/// keyword-variadic pack contributes fields, since a subscript takes no
/// keywords. any failure collapses to `None` — the checker reports those, the
/// caller just skips injection
pub(crate) fn injectable_call_specialization<'db>(
    db: &'db dyn Db,
    file: File,
    callee: Type<'db>,
    function: FunctionType<'db>,
    positional: Vec<Type<'db>>,
    keywords: Vec<(&str, Type<'db>)>,
) -> Option<String> {
    let arguments: CallArguments<'_, 'db> = positional
        .into_iter()
        .map(|ty| (Argument::Positional, Some(ty)))
        .chain(
            keywords
                .into_iter()
                .map(|(name, ty)| (Argument::Keyword(name), Some(ty))),
        )
        .collect();
    let rendered = inferred_call_type_arguments(db, file, callee, function, &arguments).ok()?;
    // an empty prefix means everything defaults — the bare call is already
    // correct and nothing is injected
    if rendered.is_empty() {
        return None;
    }
    // keyword fields are spelled after the positional arguments whatever their
    // declaration order: the wrapper binds them by name, not by slot
    let (fields, positional): (Vec<&TypeArgument>, Vec<&TypeArgument>) =
        rendered.iter().partition(|argument| argument.keyword);
    let parts: Vec<&str> = positional
        .into_iter()
        .chain(fields.iter().copied())
        .map(|argument| argument.text.as_str())
        .collect();
    Some(if fields.is_empty() {
        format!("[{}]", parts.join(", "))
    } else {
        format!(".__getitem__({})", parts.join(", "))
    })
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
) -> Result<Vec<TypeArgument>, ReifiedInferenceError<'db>> {
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
    let mut resolved: Vec<ResolvedParameter<'db, '_>> = Vec::with_capacity(generic_context.len(db));
    for (index, bound_typevar) in generic_context.variables(db).enumerate() {
        let typevar = bound_typevar.typevar(db);
        let kind = ParameterKind::of(typevar.kind(db));
        let solution = solved
            .get(index)
            .copied()
            .filter(|ty| kind.is_solution(db, *ty));
        if solution.is_some() {
            last_solved = Some(index);
        }
        resolved.push(ResolvedParameter {
            name: typevar.name(db),
            value: solution.or_else(|| typevar.default_type(db)),
            kind,
        });
    }

    // a parameter may stay valueless only when nothing depends on it: an
    // erased parameter outside the injected prefix. a reified parameter
    // without a default always needs a value, and a hole inside the prefix
    // cannot be spelled positionally
    let must_have_value = function.reified_type_params_requiring_argument(db);
    for (index, parameter) in resolved.iter().enumerate() {
        if parameter.value.is_none()
            && (last_solved.is_some_and(|last| index < last)
                || must_have_value.contains(parameter.name))
        {
            return Err(ReifiedInferenceError::Unsolved(parameter.name.clone()));
        }
    }
    let Some(last_solved) = last_solved else {
        return Ok(Vec::new());
    };

    resolved[..=last_solved]
        .iter()
        .map(|parameter| {
            let ty = parameter
                .value
                .ok_or_else(|| ReifiedInferenceError::Unsolved(parameter.name.clone()))?;
            let promoted = ty.promote(db);
            parameter
                .kind
                .spelling(db, file, promoted)
                .map(|text| TypeArgument {
                    text,
                    keyword: parameter.kind == ParameterKind::KeywordPack,
                })
                .ok_or_else(|| ReifiedInferenceError::Unspellable(parameter.name.clone(), promoted))
        })
        // a variadic or pack that absorbed nothing spells as nothing — it
        // occupies no slot in the injected list, exactly as the wrapper binds it
        .filter(|argument| !matches!(argument, Ok(argument) if argument.text.is_empty()))
        .collect()
}

/// one rendered argument of an injected specialization
pub(crate) struct TypeArgument {
    /// the source text of this argument — `int`, the comma-joined run of a
    /// `*Ts`, or the `foo=int, bar=str` fields of a `**Kwargs` pack
    text: String,
    /// whether the text is keyword-spelled, and so cannot go in a subscript
    keyword: bool,
}

/// a type parameter paired with the value the call solved it to
struct ResolvedParameter<'db, 'name> {
    name: &'name Name,
    value: Option<Type<'db>>,
    kind: ParameterKind,
}

/// how many arguments a type parameter stands for, and how they are spelled
#[derive(Clone, Copy, Eq, PartialEq)]
enum ParameterKind {
    /// a plain `T` — exactly one positional argument
    Single,
    /// a `*Ts` — the run of positional arguments it absorbs
    Variadic,
    /// a `**Kwargs` — the keyword fields it binds
    KeywordPack,
}

impl ParameterKind {
    fn of(kind: TypeVarKind) -> Self {
        if kind.is_typevartuple() {
            Self::Variadic
        } else if kind.is_keyword_variadic() {
            Self::KeywordPack
        } else {
            Self::Single
        }
    }

    /// whether the solver's answer for a parameter of this kind is one the
    /// call site can be specialized with. a run or a pack whose shape is not
    /// statically known is what the solver leaves behind when it could not
    /// determine it at all, which is "unsolved", not "solved to anything"
    fn is_solution<'db>(self, db: &'db dyn Db, ty: Type<'db>) -> bool {
        match self {
            Self::Single => is_solution(db, ty),
            Self::Variadic => variadic_elements(db, ty).is_some(),
            Self::KeywordPack => ty.keyword_pack_fields(db).is_some(),
        }
    }

    /// the source text this parameter's value spells as, or the empty string
    /// when it stands for no arguments at all
    fn spelling<'db>(self, db: &'db dyn Db, file: File, ty: Type<'db>) -> Option<String> {
        let fields = match self {
            Self::Single => return runtime_spelling(db, file, ty),
            Self::Variadic => {
                let spellings = variadic_elements(db, ty)?
                    .into_iter()
                    .map(|element| runtime_spelling(db, file, element.promote(db)))
                    .collect::<Option<Vec<_>>>()?;
                return Some(spellings.join(", "));
            }
            Self::KeywordPack => ty.keyword_pack_fields(db)?,
        };
        let spellings = fields
            .into_iter()
            .map(|(name, field)| {
                Some(format!(
                    "{name}={}",
                    runtime_spelling(db, file, field.promote(db))?
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(spellings.join(", "))
    }
}

/// the run of type arguments a `*Ts` parameter stands for — the elements of
/// the tuple that is its value
fn variadic_elements<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Vec<Type<'db>>> {
    let Type::NominalInstance(instance) = ty else {
        return None;
    };
    match instance.tuple_spec(db)?.into_owned() {
        Tuple::Fixed(elements) => Some(elements.elements_slice().to_vec()),
        Tuple::Variable(_) => None,
    }
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
                    .map(|element| runtime_spelling(db, file, element.promote(db)))
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
    // an argument is promoted one by one: a covariant parameter keeps the literal type it was
    // inferred from, and only a class object can be written at runtime
    let arguments = specialization
        .types(db)
        .iter()
        .map(|argument| runtime_spelling(db, file, argument.promote(db)))
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
/// A test means `type(value) <: C[args]` (isinstance-with-parameters
/// semantics, so it respects `C`'s declared variance). It is answered from
/// static types at compile time wherever possible; the runtime residue is an
/// equality check of reified type-param cells or a variance-aware
/// `__orig_class__` probe. The probe only works when the target's instances
/// carry `__orig_class__` — a user-defined generic. Against a builtin
/// collection, whose instances erase their type arguments, no sound runtime
/// answer exists and the test is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParametricIsPlan {
    /// statically decided — the test lowers to a constant
    Fold(bool),
    /// conjunction of runtime equality checks of reified type-param cells
    /// against target arguments, each spelled by its source range in the rhs
    TokenEq(Vec<(Name, TextRange)>),
    /// not decidable from static types, but the target is a user-defined
    /// generic whose instances carry `__orig_class__` — probe it at runtime,
    /// matching each argument by the target's effective variance (one entry
    /// per type parameter). a legitimate, unwarned runtime test
    Probe(Box<[ArgVariance]>),
    /// basedpython: not decidable from static types, and the target is a
    /// protocol — but every data member's specialized type has a runtime
    /// spelling, so the value's reified annotations can be checked structurally
    /// against the protocol's members. one entry per member to verify
    ProtocolStructural(Box<[ProtocolMemberCheck]>),
    /// not decidable from static types, and the target's instances never carry
    /// a usable `__orig_class__`, so no sound runtime probe exists — the test
    /// is an error. the reason picks the diagnostic wording
    ErasedTarget(ErasedTargetReason),
}

/// basedpython: one protocol member a parametric `is`-test checks structurally
/// at runtime, against the value's reified annotations for that member
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolMemberCheck {
    /// a data member: the value's class annotation for `name` must relate to
    /// `expected` per `variance` — an invariant (read-write) member demands
    /// equality, a read-only member a subtype, a write-only member a supertype
    Attribute {
        name: String,
        /// a python expression evaluating, in the checked module's scope, to the
        /// member's specialized type (`int`, `list[str]`)
        expected: String,
        variance: ArgVariance,
    },
    /// a method member: each declared positional parameter (contravariant) and,
    /// when the method declares a meaningful return, the return type (covariant)
    /// checked against the value method's reified parameter/return annotations
    Method {
        name: String,
        /// per-parameter `(specialized type spelling, variance)`, in declaration
        /// order after `self` — always contravariant
        params: Vec<(String, ArgVariance)>,
        /// the return `(specialized type spelling, variance)` — covariant, or
        /// `None` when the method declares no meaningful return to check
        ret: Option<(String, ArgVariance)>,
    },
}

/// why a parametric test's target cannot be probed at runtime
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasedTargetReason {
    /// a builtin collection (`list` / `dict` / `set` / `frozenset` / `tuple`)
    /// erases its type arguments — its C-level instances reject
    /// `__orig_class__` entirely
    BuiltinCollection,
    /// a protocol's instances record their own concrete class in
    /// `__orig_class__`, never the protocol, so a probe could never match it;
    /// and a structural `isinstance` check sees no type arguments (and raises
    /// outright unless the protocol is `@runtime_checkable`)
    Protocol,
}

/// how the runtime probe matches one type argument of the reified
/// specialization against the target's, per the target's declared variance
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgVariance {
    /// exact match required
    Invariant,
    /// reified argument must be a subtype of the target's (`out T`)
    Covariant,
    /// target argument must be a subtype of the reified's (`in T`)
    Contravariant,
    /// matches either way
    Bivariant,
}

/// why the target class can't back a runtime `__orig_class__` probe, or
/// `None` when it can. the builtin collections are C types that erase their
/// type arguments and reject the attribute; a protocol's instances record
/// their concrete class rather than the protocol; every other user-defined
/// generic carries a matching `__orig_class__` (set by
/// `types.GenericAlias.__call__` after construction)
fn erased_target_reason<'db>(
    db: &'db dyn Db,
    origin: ClassLiteral<'db>,
) -> Option<ErasedTargetReason> {
    if matches!(
        origin.known(db),
        Some(
            KnownClass::List
                | KnownClass::Dict
                | KnownClass::Set
                | KnownClass::FrozenSet
                | KnownClass::Tuple
        )
    ) {
        return Some(ErasedTargetReason::BuiltinCollection);
    }
    if origin.is_protocol(db) {
        return Some(ErasedTargetReason::Protocol);
    }
    None
}

/// The target specialization a parametric `is` test is checking against, drawn
/// from the inferred type of the rhs. It is a `GenericAlias` when the rhs is a
/// subscripted generic (`list[int]`) or an implicit alias bound to one
/// (`X = list[int]`); a PEP 695 `type` alias is unwrapped to the same. `None`
/// when the rhs is not a specialization — a bare class or value — so the caller
/// keeps the ordinary `isinstance` lowering.
pub(crate) fn parametric_is_target<'db>(
    db: &'db dyn Db,
    rhs_ty: Type<'db>,
) -> Option<GenericAlias<'db>> {
    match rhs_ty {
        Type::GenericAlias(alias) => Some(alias),
        _ => {
            let value = rhs_ty.as_type_alias()?.value_type(db);
            match value {
                Type::GenericAlias(alias) => Some(alias),
                Type::NominalInstance(instance) => match instance.class(db) {
                    ClassType::Generic(alias) => Some(alias),
                    ClassType::NonGeneric(_) => None,
                },
                _ => None,
            }
        }
    }
}

/// [`parametric_is_target`] for a target written in *type position* — a checked
/// cast's `cast T` operand, which ty infers as the instance type rather than the
/// class object an `is`-rhs evaluates to. This is the only difference between
/// the two forms; both then classify through [`classify_parametric_is`].
pub(crate) fn parametric_cast_target<'db>(
    db: &'db dyn Db,
    target_ty: Type<'db>,
) -> Option<GenericAlias<'db>> {
    match target_ty {
        Type::NominalInstance(instance) => match instance.class(db) {
            ClassType::Generic(alias) => Some(alias),
            ClassType::NonGeneric(_) => None,
        },
        Type::ProtocolInstance(instance) => match instance.inner {
            crate::types::instance::Protocol::FromClass(protocol_class) => match *protocol_class {
                ClassType::Generic(alias) => Some(alias),
                ClassType::NonGeneric(_) => None,
            },
            crate::types::instance::Protocol::Synthesized(_) => None,
        },
        // an alias name still resolves through the value-position rules
        _ => parametric_is_target(db, target_ty),
    }
}

/// Classify how `lhs is rhs` (keyword form) resolves, from the already-inferred
/// static type of the lhs. `rhs` evaluates to `rhs_alias` — it may be spelled
/// directly (`list[int]`), through an alias name whose value is that
/// specialization (`X = list[int]; … is X`), or through a PEP 695 alias.
///
/// Only a directly-subscripted target exposes its type arguments as syntax, so
/// the reified-cell token-equality path (which spells `T == <arg>`) is
/// available for a subscript rhs but not for an alias name; an alias name falls
/// back to the static fold or the runtime probe.
pub(crate) fn classify_parametric_is<'db>(
    db: &'db dyn Db,
    file: File,
    lhs_ty: Type<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    rhs_node: &ast::Expr,
) -> ParametricIsPlan {
    let target_origin = ClassLiteral::Static(rhs_alias.origin(db));
    let target_args_ast: Vec<&ast::Expr> = match rhs_node {
        ast::Expr::Subscript(subscript) => match subscript.slice.as_ref() {
            ast::Expr::Tuple(tuple) => tuple.elts.iter().collect(),
            single => vec![single],
        },
        _ => Vec::new(),
    };
    let plan = classify_value(
        db,
        lhs_ty.promote(db),
        target_origin,
        rhs_alias,
        &target_args_ast,
        rhs_node,
    );
    // the runtime probe unwinds the value's `__orig_class__` and its class's
    // generic bases across the mro, so a builtin-collection target is checkable
    // after all: a concrete subclass that fixes the arguments (`class B(list[int])`)
    // records `list[int]` in `__orig_bases__`. a protocol's instances never
    // record the protocol, so `__orig_class__` can't answer it — but basedpython
    // reifies class attribute annotations, so a protocol whose members are all
    // spellable data members can still be checked structurally against those
    // annotations. only a protocol that also has a method member (unrecoverable
    // from an annotation) stays an error
    if let ParametricIsPlan::Probe(_) = plan
        && let Some(ErasedTargetReason::Protocol) = erased_target_reason(db, target_origin)
    {
        return protocol_structural_members(db, file, ClassType::Generic(rhs_alias))
            .map(|checks| ParametricIsPlan::ProtocolStructural(checks.into_boxed_slice()))
            .unwrap_or(ParametricIsPlan::ErasedTarget(ErasedTargetReason::Protocol));
    }
    plan
}

/// basedpython: the structural runtime check for a protocol target whose data
/// members can all be verified against a value's reified class annotations, or
/// `None` when the protocol has a member that can't be — a method (its shape
/// isn't recoverable from an annotation) or a data member whose specialized
/// type has no runtime spelling.
///
/// shared by the parametric `is`-test (`x is A[int]`) and the checked cast
/// (`x cast A[int]`): both validate the same structural claim against the same
/// reified annotations, so both consult one source of truth.
pub(crate) fn protocol_structural_members<'db>(
    db: &'db dyn Db,
    file: File,
    class: ClassType<'db>,
) -> Option<Vec<ProtocolMemberCheck>> {
    let protocol_class = class.into_protocol_class(db)?;
    let mut checks = Vec::new();
    for member in protocol_class.interface(db).members(db) {
        let name = member.name().to_owned();
        let check = match member.reified_member_shape(db)? {
            ReifiedMember::Attribute {
                ty,
                readable,
                writable,
            } => {
                let expected = protocol_member_spelling(db, file, ty)?;
                let variance = match (readable, writable) {
                    (true, true) => ArgVariance::Invariant,
                    (true, false) => ArgVariance::Covariant,
                    (false, true) => ArgVariance::Contravariant,
                    (false, false) => return None,
                };
                ProtocolMemberCheck::Attribute {
                    name,
                    expected,
                    variance,
                }
            }
            ReifiedMember::Method { params, ret } => {
                // each parameter is contravariant; an unspellable parameter type
                // means the method can't be checked, so the whole protocol falls
                // back to the erased-target error
                let mut param_checks = Vec::with_capacity(params.len());
                for param_ty in params {
                    let expected = protocol_member_spelling(db, file, param_ty)?;
                    param_checks.push((expected, ArgVariance::Contravariant));
                }
                let ret = match reified_return_check(db, file, ret) {
                    ReturnCheck::Skip => None,
                    ReturnCheck::Check(expected) => Some((expected, ArgVariance::Covariant)),
                    ReturnCheck::Unspellable => return None,
                };
                ProtocolMemberCheck::Method {
                    name,
                    params: param_checks,
                    ret,
                }
            }
        };
        checks.push(check);
    }
    Some(checks)
}

/// [`runtime_spelling`] for a protocol member's specialized type, which may be a
/// *literal* (`A[True]` specializes `T` to `Literal[True]`).
///
/// A literal has no bare runtime spelling, so it is rendered as a call to the
/// structural check's own `_by_lit` helper, which rebuilds `typing.Literal[…]`.
/// That keeps the check exact — an invariant member typed `Literal[True]` must
/// not match a `bool` annotation — and, because the helper ships with the
/// protocol runtime, needs no import at the use site.
///
/// This deliberately does *not* widen [`runtime_spelling`] itself: that spelling
/// is also injected into reified calls (`f[int](…)`) and constructor
/// specializations (`A[int](1)`), where `_by_lit` is not in scope.
fn protocol_member_spelling<'db>(db: &'db dyn Db, file: File, ty: Type<'db>) -> Option<String> {
    if let Type::LiteralValue(literal) = ty {
        let value = match literal.kind() {
            LiteralValueTypeKind::Bool(boolean) => {
                (if boolean { "True" } else { "False" }).to_owned()
            }
            LiteralValueTypeKind::Int(int) => int.as_i64().to_string(),
            // only a plain-ascii string round-trips through rust's escaping as
            // valid python; anything else has no faithful spelling here
            LiteralValueTypeKind::String(string) => {
                let value = string.value(db);
                if value.is_ascii() && !value.contains(|c: char| c.is_ascii_control()) {
                    format!("{value:?}")
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        return Some(format!("_by_lit({value})"));
    }
    runtime_spelling(db, file, ty)
}

/// the covariant/skip/unspellable classification of a protocol method's return
/// type for a structural runtime check
enum ReturnCheck {
    /// the return imposes no runtime-checkable constraint (`None`, dynamic, or
    /// `object`) — nothing to verify
    Skip,
    /// check the value method's return annotation against this spelling
    Check(String),
    /// a meaningful return with no runtime spelling — the method can't be checked
    Unspellable,
}

fn reified_return_check<'db>(db: &'db dyn Db, file: File, ret: Type<'db>) -> ReturnCheck {
    if ret.is_none(db) || ret.is_dynamic() || is_object_instance(db, ret) {
        return ReturnCheck::Skip;
    }
    match protocol_member_spelling(db, file, ret) {
        Some(expected) => ReturnCheck::Check(expected),
        None => ReturnCheck::Unspellable,
    }
}

fn is_object_instance<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    matches!(ty, Type::NominalInstance(instance)
        if instance.class(db).class_literal(db).is_known(db, KnownClass::Object))
}

fn classify_value<'db>(
    db: &'db dyn Db,
    value_ty: Type<'db>,
    target_origin: ClassLiteral<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    target_args_ast: &[&ast::Expr],
    rhs_node: &ast::Expr,
) -> ParametricIsPlan {
    // when the value's type is carried by a reified type parameter, the answer
    // lives in a runtime cell rather than the static type — extract the cell
    // comparisons before falling back to static subtyping
    if value_ty.has_typevar(db)
        && let Some(plan) = try_token_eq(
            db,
            value_ty,
            target_origin,
            rhs_alias,
            target_args_ast,
            rhs_node,
        )
    {
        return plan;
    }

    // `a is C[args]` means `type(a) <: C[args]`, so the static answer is a
    // subtype question — this respects `C`'s declared variance for free
    let target_instance = Type::instance(db, ClassType::Generic(rhs_alias));
    if value_ty.is_subtype_of(db, target_instance) {
        ParametricIsPlan::Fold(true)
    } else if value_ty.is_disjoint_from(db, target_instance) {
        ParametricIsPlan::Fold(false)
    } else {
        // undecidable statically; `classify_parametric_is` turns this into a
        // runtime probe (user generic) or an erased-target error (builtin)
        ParametricIsPlan::Probe(target_variances(db, rhs_alias))
    }
}

/// The reified-cell comparisons for a value whose static type is (or is built
/// from) a reified type parameter — `x: T` against `C[args]` is `T == C[args]`,
/// `x: list[T]` against `list[int]` is `T == int`. `None` when the value is
/// not so shaped (the caller then resolves it statically).
fn try_token_eq<'db>(
    db: &'db dyn Db,
    value_ty: Type<'db>,
    target_origin: ClassLiteral<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    target_args_ast: &[&ast::Expr],
    rhs_node: &ast::Expr,
) -> Option<ParametricIsPlan> {
    match value_ty {
        // `x: T is <target>` compares the reified `T` cell against the target
        // *as spelled*. that is only sound when the source evaluates to the
        // specialization itself — a direct subscript. an alias name would
        // compare against the alias object (or, for a PEP 695 alias, a
        // `TypeAliasType` wrapper), so it falls through to the static resolution
        Type::TypeVar(bound_typevar)
            if is_reified_function_typevar(db, bound_typevar)
                && matches!(rhs_node, ast::Expr::Subscript(_)) =>
        {
            Some(ParametricIsPlan::TokenEq(vec![(
                bound_typevar.name(db).clone(),
                rhs_node.range(),
            )]))
        }
        Type::NominalInstance(instance) => {
            let ClassType::Generic(alias) = instance.class(db) else {
                return None;
            };
            if ClassLiteral::Static(alias.origin(db)) != target_origin {
                return None;
            }
            let mut tokens = Vec::new();
            unify_specializations(
                db,
                target_origin,
                alias.specialization(db),
                rhs_alias.specialization(db),
                Some(target_args_ast),
                &mut tokens,
            )
            .ok()
            .filter(|()| !tokens.is_empty())
            .map(|()| ParametricIsPlan::TokenEq(tokens))
        }
        _ => None,
    }
}

/// The runtime alias spelling and per-parameter variances for a *deep*
/// soundness check of `ty`, or `None` when no runtime parameter check is
/// possible. `Some` only when `ty` is an instance of a user-defined generic
/// class whose instances carry `__orig_class__` (so the type arguments survive
/// to runtime) and whose specialization has a runtime spelling (`A[int]`);
/// builtin collections erase their arguments and return `None`, as does any
/// specialization with an unspellable argument.
///
/// Used by the transpiler's soundness pass: where a shallow `isinstance`
/// check would validate only the base class, this lets it also validate the
/// type arguments against the value's reified `__orig_class__`.
pub(crate) fn parametric_soundness_spelling<'db>(
    db: &'db dyn Db,
    file: File,
    ty: Type<'db>,
) -> Option<(String, Box<[ArgVariance]>)> {
    let Type::NominalInstance(instance) = ty else {
        return None;
    };
    let ClassType::Generic(alias) = instance.class(db) else {
        return None;
    };
    let origin = ClassLiteral::Static(alias.origin(db));
    // a target whose instances don't carry a usable `__orig_class__` — a
    // builtin collection (erased arguments) or a protocol — has nothing to
    // probe, so the base `isinstance` check is all that's sound
    if erased_target_reason(db, origin).is_some() {
        return None;
    }
    let spelling = spell_class(db, file, ClassType::Generic(alias))?;
    let variances = target_variances(db, alias);
    if variances.is_empty() {
        return None;
    }
    Some((spelling, variances))
}

/// the effective variance of each of the target's type parameters — how the
/// runtime probe matches each argument. this is the declared variance combined
/// with any use-site projection the target spells (`A[out int]` matches
/// covariantly even when `A`'s `T` is declared invariant), using the same
/// combiner that decides subtyping, so the probe agrees with `is_subtype_of`
///
/// the probe reads the value's `__orig_class__`, which records a concrete
/// construction (`A[bool](…)`) and never a projected view, so the source side
/// of the combination carries no projection
fn target_variances<'db>(db: &'db dyn Db, alias: GenericAlias<'db>) -> Box<[ArgVariance]> {
    let origin = ClassLiteral::Static(alias.origin(db));
    let Some(generic_context) = origin.generic_context(db) else {
        return Box::default();
    };
    let specialization = alias.specialization(db);
    generic_context
        .variables(db)
        .map(|bound_typevar| {
            let declared = bound_typevar.variance(db);
            let effective = combine_use_site_projections(
                declared,
                None,
                specialization.projection_for(db, bound_typevar),
                false,
            )
            .unwrap_or(declared);
            match effective {
                TypeVarVariance::Invariant => ArgVariance::Invariant,
                TypeVarVariance::Covariant => ArgVariance::Covariant,
                TypeVarVariance::Contravariant => ArgVariance::Contravariant,
                TypeVarVariance::Bivariant => ArgVariance::Bivariant,
            }
        })
        .collect()
}

/// Match the value's specialization against the target's, position by
/// position, collecting a runtime token comparison for each reified type
/// variable found on the value side. `Err(())` means the two do not unify to
/// a set of token comparisons (the caller then resolves the test statically).
fn unify_specializations<'db>(
    db: &'db dyn Db,
    origin: ClassLiteral<'db>,
    value_spec: Specialization<'db>,
    target_spec: Specialization<'db>,
    target_args_ast: Option<&[&ast::Expr]>,
    tokens: &mut Vec<(Name, TextRange)>,
) -> Result<(), ()> {
    if origin.is_known(db, KnownClass::Tuple) {
        return match (value_spec.tuple(db), target_spec.tuple(db)) {
            (Some(Tuple::Fixed(value)), Some(Tuple::Fixed(target)))
                if value.elements_slice().len() == target.elements_slice().len() =>
            {
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
            _ => Err(()),
        };
    }
    let value_types = value_spec.types(db);
    let target_types = target_spec.types(db);
    if value_types.len() != target_types.len() {
        return Err(());
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
) -> Result<(), ()> {
    if value == target || value.is_equivalent_to(db, target) {
        return Ok(());
    }
    if let Type::TypeVar(bound_typevar) = value {
        if !is_reified_function_typevar(db, bound_typevar) {
            return Err(());
        }
        // a pep 696 default can leave a target position with no source
        // expression to compare against
        let target_ast = target_ast.ok_or(())?;
        tokens.push((bound_typevar.name(db).clone(), target_ast.range()));
        return Ok(());
    }
    // both sides specializations of the same class: recurse structurally
    // (`list[T]` vs the `list[int]` written in the rhs)
    if let (Type::NominalInstance(value_instance), Type::NominalInstance(target_instance)) =
        (value, target)
        && let (ClassType::Generic(value_alias), ClassType::Generic(target_alias)) =
            (value_instance.class(db), target_instance.class(db))
        && value_alias.origin(db) == target_alias.origin(db)
    {
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
    Err(())
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
    crate::reified::reified_type_param_names(source.as_str(), def_file.source_type(db), node)
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
            let missing = sub.reified_type_params_requiring_argument(db);
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
