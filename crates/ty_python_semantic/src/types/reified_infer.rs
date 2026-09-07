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

use std::fmt::Write as _;

use itertools::Itertools;
use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, name::Name};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashMap;

use crate::Db;
use crate::place::{builtins_symbol, global_symbol};
use crate::types::ProgramEnvironment;
use crate::types::call::{Argument, CallArguments};
use crate::types::class::{ClassLiteral, ClassType, GenericAlias};
use crate::types::class_base::ClassBase;
use crate::types::function::FunctionType;
use crate::types::generics::{Specialization, combine_use_site_projections};
use crate::types::instance::Protocol;
use crate::types::literal::LiteralValueTypeKind;
use crate::types::protocol_class::ReifiedMember;
use crate::types::tuple::Tuple;
use crate::types::typevar::TypeVarKind;
use crate::types::variance::TypeVarVariance;
use crate::types::{KnownClass, MemberLookupPolicy, Type};

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
    env: &ProgramEnvironment<'db>,
    file: File,
    callee: Type<'db>,
    function: FunctionType<'db>,
    arguments: &CallArguments<'_, 'db>,
) -> Result<Vec<TypeArgument>, ReifiedInferenceError<'db>> {
    let bindings = callee
        .try_call(db, env, arguments)
        .map_err(|_| ReifiedInferenceError::NoBinding)?;
    let specialization = bindings
        .single_element()
        .and_then(|callable| callable.matching_overloads().exactly_one().ok())
        .ok_or(ReifiedInferenceError::NoBinding)?
        .1
        .merged_specialization(db, env);
    rendered_type_arguments(db, env, file, function, specialization)
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
    env: &ProgramEnvironment<'db>,
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
    let rendered =
        inferred_call_type_arguments(db, env, file, callee, function, &arguments).ok()?;
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
    env: &ProgramEnvironment<'db>,
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
            .filter(|ty| kind.is_solution(db, env, *ty));
        if solution.is_some() {
            last_solved = Some(index);
        }
        resolved.push(ResolvedParameter {
            name: typevar.name(db),
            value: solution.or_else(|| typevar.default_type(db, env)),
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
            let promoted = ty.promote(db, env);
            parameter
                .kind
                .spelling(db, env, file, promoted)
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
    fn is_solution<'db>(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
    ) -> bool {
        match self {
            Self::Single => is_solution(db, ty),
            Self::Variadic => variadic_elements(db, env, ty).is_some(),
            Self::KeywordPack => ty.keyword_pack_fields(db).is_some(),
        }
    }

    /// the source text this parameter's value spells as, or the empty string
    /// when it stands for no arguments at all
    fn spelling<'db>(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        file: File,
        ty: Type<'db>,
    ) -> Option<String> {
        let fields = match self {
            Self::Single => return runtime_spelling(db, env, file, ty),
            Self::Variadic => {
                let spellings = variadic_elements(db, env, ty)?
                    .into_iter()
                    .map(|element| {
                        runtime_spelling(db, env, file, element.promote_in(db, env, file))
                    })
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
                    runtime_spelling(db, env, file, field.promote_in(db, env, file))?
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(spellings.join(", "))
    }
}

/// the run of type arguments a `*Ts` parameter stands for — the elements of
/// the tuple that is its value
fn variadic_elements<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Vec<Type<'db>>> {
    let Type::NominalInstance(instance) = ty else {
        return None;
    };
    match instance.tuple_spec(db, env)?.into_owned() {
        Tuple::Fixed(elements) => Some(elements.elements_slice().to_vec()),
        Tuple::Variable(_) => None,
    }
}

/// whether the solver produced an actual answer for a type parameter — a
/// dynamic type means "nothing to reify", and so does a typevar with no
/// runtime cell behind it. a *reified* typevar does have one, so it counts
fn is_solution<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    match ty {
        Type::Dynamic(_) | Type::Never => false,
        // a reified type parameter is a live runtime cell holding its type
        // argument, so a call that solves to one is answerable by forwarding
        // that cell (`f(data)` inside a reified caller becomes `f[T](data)`).
        // the cell is in scope by construction: the solver only reaches it
        // through an argument whose type mentions it
        Type::TypeVar(bound_typevar) => is_reified_function_typevar(db, bound_typevar),
        _ => true,
    }
}

/// A python expression that evaluates, in `file`'s module scope, to the
/// runtime object denoted by `ty` — or `None` when there is no such spelling.
fn runtime_spelling<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> Option<String> {
    if ty.is_none(db) {
        return Some("None".to_owned());
    }
    match ty {
        Type::NominalInstance(instance) => spell_class(db, env, file, instance.class(db, env)),
        // a reified type parameter spells as its own name: pep 695 compiles it
        // into the enclosing function's closure, and the `generic` wrapper fills
        // that cell with the type argument, so the name evaluates to the type
        Type::TypeVar(bound_typevar) if is_reified_function_typevar(db, bound_typevar) => {
            Some(bound_typevar.name(db).to_string())
        }
        Type::Union(union) => Some(
            union
                .elements(db)
                .iter()
                .map(|element| runtime_spelling(db, env, file, *element))
                .collect::<Option<Vec<_>>>()?
                .join(" | "),
        ),
        _ => None,
    }
}

fn spell_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    class: ClassType<'db>,
) -> Option<String> {
    match class {
        ClassType::NonGeneric(literal) => spell_class_literal(db, env, file, literal),
        ClassType::Generic(alias) => {
            let origin = ClassLiteral::Static(alias.origin(db));
            let base = spell_class_literal(db, env, file, origin)?;
            let arguments =
                spell_specialization_arguments(db, env, file, origin, alias.specialization(db))
                    .ok()?;
            Some(format!("{base}[{arguments}]"))
        }
    }
}

/// what is known about whether `origin[…]` *evaluates* at runtime rather than
/// raising `TypeError`
///
/// cpython makes a class subscriptable through pep 560's `__class_getitem__`.
/// a class written in python gets one by inheriting `typing.Generic`, which
/// every `class A[T]` does, so being generic and being subscriptable are the
/// same thing for it. a c type has to define the method by hand, and many never
/// did — `zip`, `map`, `filter`, `reversed` and `itertools.count` are all
/// generic to a type checker and all raise when subscripted — while others
/// gained one years later: `list` in 3.9, `array.array` in 3.12, `memoryview`
/// in 3.14
///
/// two things in a stub carry the fact. an explicit `__class_getitem__`, which
/// typeshed writes — behind a `sys.version_info` gate where it matters — for
/// each class whose runtime has one without inheriting `Generic`. and a base
/// the class spells out, which brings the base's own `__class_getitem__` with
/// it. neither is airtight on its own: a stub's bases are a typing fiction by
/// design, so they have to be read *after* the gate, or `memoryview` (spelled
/// as a `Sequence` subclass it does not inherit at runtime) would come out
/// subscriptable on a version where it is not
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeSubscript {
    /// the class object has a `__class_getitem__` at the target version; or it
    /// declares a specialized base, which supplies one; or its real definition
    /// is in view and it is generic, which at runtime is the same thing
    Supported,
    /// a `__class_getitem__` is written for this class, but not for the version
    /// being targeted — the runtime gained it in a later release
    Unsupported,
    /// nothing in view settles it: a stub class with no `__class_getitem__`
    /// anywhere in its mro *and* no base of its own to inherit one from. after
    /// the pep 695 conversion that is an empty base list and a parameter list,
    /// which is what `zip`, `map`, `filter`, `reversed` and nearly all of
    /// `itertools` look like — and also what `typing.IO` looks like, though it
    /// subscripts perfectly well
    ///
    /// TODO: settle this bucket from [`KnownClass`] rather than from the stub.
    /// the fact is not in the stub to be read: the conversion drops the
    /// `Generic[…]` base upstream typeshed wrote, so `zip` and `typing.IO` come
    /// out identical with opposite runtime answers. a `KnownClass` arm keyed on
    /// the target version is the natural place for it — the same table that
    /// already knows `list` from `dict` would answer "does cpython let you
    /// subscript this, and since when"
    ///
    /// the table is small. of the 140 generic classes in the vendored stdlib
    /// stubs, 44 declare the method and most of the rest inherit a base that
    /// does; the bucket left over is about 40, of which `typing.IO` is the only
    /// one that subscripts at runtime. everything else in it really does raise
    ///
    /// the same table would close the residual on the other side. a stub class
    /// *does* reach [`Supported`](RuntimeSubscript::Supported) through a
    /// declared base that the runtime turns out not to have —
    /// `email.message.MIMEPart` is spelled as a `Message[…]` subclass and
    /// raises when subscripted — so a specialization of one would be injected
    /// and would not run. no such class is reachable through a constructor call
    /// that solves a specialization today, which is why it is a residual rather
    /// than a bug
    ///
    /// once the fact comes from a table this enum collapses to two arms, and
    /// the two thresholds at the call sites below become one question
    Unknown,
}

fn runtime_subscript<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    origin: ClassLiteral<'db>,
) -> RuntimeSubscript {
    // a class whose real definition is in view needs no attestation: there a
    // type-parameter list or a `Generic` base is the runtime's own
    if !origin.file(db).source_type(db).is_stub() {
        return RuntimeSubscript::Supported;
    }
    // a member only *some* branch declares is no attestation: the emitted
    // subscript has to evaluate on every run
    if matches!(
        origin
            .class_member(db, env, "__class_getitem__", MemberLookupPolicy::default())
            .place,
        crate::place::Place::Defined(defined) if defined.is_definitely_defined()
    ) {
        return RuntimeSubscript::Supported;
    }
    if mro_mentions_class_getitem(db, origin) {
        return RuntimeSubscript::Unsupported;
    }
    // a stub class that reaches `Generic` through a base it *spells out* is one
    // whose stub author claimed it really is that base's subclass, and a real
    // base brings the real `__class_getitem__` with it — `collections.ChainMap`
    // declares none of its own but inherits `MutableMapping[Key, Value]`, and
    // `ChainMap[str, int]` evaluates. the c types that raise are the ones with
    // no such claim to make: `zip`, `map`, `filter` and nearly all of
    // `itertools` come out of the pep 695 conversion with an empty base list
    //
    // the claim is not always true — typeshed's `memoryview` is spelled as a
    // `Sequence` subclass it does not inherit at runtime — which is why this
    // comes *after* the version-gated arm above, the one that catches it
    if origin.explicit_bases(db).iter().any(Type::is_generic_alias) {
        return RuntimeSubscript::Supported;
    }
    RuntimeSubscript::Unknown
}

/// whether any class in `origin`'s mro writes a `__class_getitem__` in its body
/// — even in a `sys.version_info` branch this target version does not take
///
/// a version gate around the declaration is how typeshed records *when* the
/// runtime gained the method. reading the class body's symbols rather than
/// resolving the member is deliberate: resolution answers for the target
/// version, and this question is about every version
fn mro_mentions_class_getitem<'db>(db: &'db dyn Db, origin: ClassLiteral<'db>) -> bool {
    origin.iter_mro(db).any(|base| {
        let ClassBase::Class(class) = base else {
            return false;
        };
        let ClassLiteral::Static(literal) = class.class_literal(db) else {
            return false;
        };
        ty_python_core::place_table(db, literal.body_scope(db))
            .symbol_by_name("__class_getitem__")
            .is_some()
    })
}

/// the comma-joined runtime spellings of a specialization's type arguments —
/// what goes inside a class's `[...]` — or the reason there are none.
///
/// each parameter spells by its own kind, the way a function's do: a `*Ts`
/// flattens into the run of arguments it stands for, because that is what the
/// runtime binding reads back out of the subscript. tuples carry their precise
/// element shape out-of-band; spell it rather than the class's single typevar
fn spell_specialization_arguments<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    origin: ClassLiteral<'db>,
    specialization: Specialization<'db>,
) -> Result<String, ReifiedInferenceError<'db>> {
    // the caller may be about to *invent* an `origin[…]` the source never wrote,
    // so it fires only on positive evidence. a wrong yes kills the program at
    // import; a wrong no costs the specialization nothing else can observe
    if runtime_subscript(db, env, origin) != RuntimeSubscript::Supported {
        return Err(ReifiedInferenceError::NoBinding);
    }
    if origin.is_known(db, KnownClass::Tuple) {
        let Some(Tuple::Fixed(fixed)) = specialization.tuple(db) else {
            return Err(ReifiedInferenceError::NoBinding);
        };
        let elements = fixed
            .elements_slice()
            .iter()
            .map(|element| runtime_spelling(db, env, file, element.promote_in(db, env, file)))
            .collect::<Option<Vec<_>>>()
            .ok_or(ReifiedInferenceError::NoBinding)?;
        return Ok(if elements.is_empty() {
            "()".to_owned()
        } else {
            elements.join(", ")
        });
    }
    let Some(generic_context) = origin.generic_context(db) else {
        return Err(ReifiedInferenceError::NoBinding);
    };
    let mut arguments = Vec::new();
    for (bound_typevar, argument) in generic_context.variables(db).zip(specialization.types(db)) {
        let typevar = bound_typevar.typevar(db);
        let kind = ParameterKind::of(typevar.kind(db));
        // a class writes its specialization as a subscript, and a subscript
        // takes no keyword arguments, so a pack's fields cannot be spelled the
        // way a function's are — a class never reifies one for the same reason
        if matches!(kind, ParameterKind::KeywordPack) {
            return Err(ReifiedInferenceError::NoBinding);
        }
        // an argument is promoted one by one: a covariant parameter keeps the
        // literal type it was inferred from, and only a class object can be
        // written at runtime
        let promoted = argument.promote(db, env);
        // an unsolved parameter reads as `Never`, and one solved from something
        // the checker cannot see reads as `Unknown`. neither is an argument
        // anybody wrote down, so both say the same thing: name one
        if matches!(kind, ParameterKind::Single) && (promoted.is_never() || promoted.is_dynamic()) {
            return Err(ReifiedInferenceError::Unsolved(typevar.name(db).clone()));
        }
        let Some(spelling) = kind.spelling(db, env, file, promoted) else {
            return Err(ReifiedInferenceError::Unspellable(
                typevar.name(db).clone(),
                promoted,
            ));
        };
        // an empty run stands for no arguments at all, which is what the runtime
        // binding fills in for an unsupplied variadic
        if !spelling.is_empty() {
            arguments.push(spelling);
        }
    }
    Ok(arguments.join(", "))
}

/// The specialization a construction of `class_literal` builds, or the reason
/// it has none.
///
/// Read from the (already inferred, literal-promoted) type of the constructed
/// instance, so the answer always matches the checker's solved specialization —
/// including PEP 696 defaults and any usage-based widening. This is the single
/// decision behind both directions: [`constructor_specialization_display`] is
/// the transpiler's injection and [`reified_construction_error`] the checker's
/// diagnostic, so the two cannot disagree about which constructions are legal.
fn constructor_specialization<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    class_literal: ClassLiteral<'db>,
    constructed: Type<'db>,
) -> Result<String, ReifiedInferenceError<'db>> {
    let Type::NominalInstance(instance) = constructed.promote(db, env) else {
        return Err(ReifiedInferenceError::NoBinding);
    };
    let ClassType::Generic(alias) = instance.class(db, env) else {
        return Err(ReifiedInferenceError::NoBinding);
    };
    let origin = ClassLiteral::Static(alias.origin(db));
    if origin != class_literal {
        return Err(ReifiedInferenceError::NoBinding);
    }
    spell_specialization_arguments(db, env, file, origin, alias.specialization(db))
}

/// The bracketed type-argument spelling to inject at a bare constructor call
/// of the generic class `class_literal` (`A(1)` → `"int"`).
///
/// `None` when the construction has no writable specialization; unlike a bare
/// reified-generic call this is never an error on its own — the call simply
/// stays bare, and [`reified_construction_error`] reports it only when the class
/// reifies something.
pub(crate) fn constructor_specialization_display<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    class_literal: ClassLiteral<'db>,
    constructed: Type<'db>,
) -> Option<String> {
    constructor_specialization(db, env, file, class_literal, constructed).ok()
}

/// Why a construction of the reified generic class `class_literal` cannot say
/// which specialization it builds, or `None` when it can.
///
/// A reified class records its type arguments on the specialization its
/// instances are built from, so a construction has to name one. This is
/// [`constructor_specialization_display`]'s decision with the reason kept: the
/// transpiler injects the solved specialization wherever one can be written, and
/// where it cannot the instance would have no answer for a type parameter its
/// own methods read.
pub(crate) fn reified_construction_error<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    class_literal: ClassLiteral<'db>,
    constructed: Type<'db>,
) -> Option<ReifiedInferenceError<'db>> {
    constructor_specialization(db, env, file, class_literal, constructed).err()
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
    env: &ProgramEnvironment<'db>,
    file: File,
    literal: ClassLiteral<'db>,
) -> Option<String> {
    let name = literal.name(db);
    let resolved = global_symbol(db, db.program_file(file), name)
        .place
        .ignore_possibly_undefined()
        .or_else(|| {
            builtins_symbol(db, env, name)
                .place
                .ignore_possibly_undefined()
        })?;
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
    Probe {
        /// the target's runtime spelling (`A[int]`), or `None` when the source
        /// already spells it and should be passed through — which is the more
        /// robust of the two, since a name the source wrote is in scope by
        /// construction while a rebuilt spelling needs the origin class to be
        /// nameable where the test is written. `Some` where nothing in the
        /// source spells this target on its own: one arm of a union
        target: TargetSpelling,
        variances: Box<[ArgVariance]>,
    },
    /// basedpython: not decidable from static types, and the target is a
    /// protocol — but every data member's specialized type has a runtime
    /// spelling, so the value's reified annotations can be checked structurally
    /// against the protocol's members. one entry per member to verify
    ProtocolStructural(Box<[ProtocolMemberCheck]>),
    /// not decidable from static types, and the target's instances never carry
    /// a usable `__orig_class__`, so no sound runtime probe exists — the test
    /// is an error. the reason picks the diagnostic wording
    ErasedTarget(ErasedTargetReason),
    /// `isinstance(value, <spelling>)` — the target is a plain class, which is
    /// exactly what `isinstance` was built to answer. `None` where the source
    /// spells the target itself and should be passed through, which names even
    /// a class no module global does (an enum's `Shape.Circle`)
    Isinstance(TargetSpelling),
    /// `callable(value)` — a `Callable` with no signature, which asks only what
    /// the runtime records
    IsCallable,
    /// `value is None`. `None` is a value, not a class, so `isinstance` cannot
    /// take it, and identity is the whole test — there is only one `None`
    IsNone,
    /// `type(value) is <class> and value == <value>` — a literal target such as
    /// `Literal[3]`, whose type holds exactly one value. the class guard is not
    /// redundant: python's `1 == True` would otherwise let a `bool` satisfy
    /// `Literal[1]`
    Equality {
        class: String,
        /// the value to compare against. the source's own literal where it
        /// wrote one — which is already spelled correctly for wherever it sits,
        /// including inside an f-string on a python that forbids reusing the
        /// outer quote
        value: TargetSpelling,
    },
    /// `value is <spelling>` — an enum member, which is a singleton, so
    /// identity is both exact and what the runtime compares anyway. `None`
    /// passes the source through, as for [`Self::Isinstance`]
    Identity(TargetSpelling),
    /// `isinstance(value, type) and issubclass(value, <spelling>)` — a
    /// `type[C]` target, which the runtime can check in full. `None` passes the
    /// source through, as for [`Self::Isinstance`]
    Subclass(TargetSpelling),
    /// a template literal type: the value must be a `str` matching this
    /// regular expression, which spells the same language `matches_str` decides
    Pattern(String),
    /// the target is an interface something in scope visibly *conforms* to, so
    /// the conformance registry answers the test. a conforming type is not a
    /// subclass, so `isinstance` could never see the relationship
    Conformance {
        /// how the interface is written into the emitted python
        target: TargetSpelling,
        /// the interface's required member names, for the runtime check to look
        /// for on a value nothing registered
        members: Vec<String>,
    },
    /// nothing is known about the target — an error elsewhere left `Unknown`
    /// behind. the test is lowered as the plain `isinstance` the source spells
    /// and reported by whatever produced the `Unknown`, which is the sharper
    /// report and the only one
    Unresolved,
    /// the disjunction of these plans — the target is a union, and a value
    /// satisfies it as soon as one arm holds. the arms are carried as plans of
    /// their own because a union's arms need not be spelled in the source: a
    /// PEP 695 alias names one with a single identifier
    Union(Box<[ParametricIsPlan]>),
}

impl ParametricIsPlan {
    /// whether a `False` from this test proves the value does *not* have the
    /// type, so the negative branch may narrow.
    ///
    /// An exact check answers the question the type asks. The parametric ones
    /// do not: a runtime probe reads the arguments a value happens to record,
    /// and a value that records none answers `False` even where the static
    /// types say it is a match — narrowing on that would remove a type the
    /// value really has.
    pub(crate) fn narrows_negatively(&self) -> bool {
        match self {
            Self::Fold(_)
            | Self::Isinstance(_)
            | Self::IsCallable
            | Self::IsNone
            | Self::Equality { .. }
            | Self::Identity(_)
            | Self::Subclass(_)
            | Self::Pattern(_) => true,
            Self::Union(arms) => arms.iter().all(Self::narrows_negatively),
            Self::TokenEq(_)
            | Self::Probe { .. }
            | Self::ProtocolStructural(_)
            | Self::Conformance { .. }
            | Self::Unresolved
            | Self::ErasedTarget(_) => false,
        }
    }

    /// whether this plan proves the test can never hold, so the guarded branch
    /// is dead. only a static fold proves that; every runtime residue leaves
    /// the answer to the value
    pub(crate) fn never_holds(&self) -> bool {
        match self {
            Self::Fold(holds) => !holds,
            Self::Union(arms) => arms.iter().all(Self::never_holds),
            _ => false,
        }
    }
}

/// basedpython: how a type test's target is written into the emitted python
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSpelling {
    /// the source spells the target itself, so its own text is passed through.
    /// this is the more robust of the two: a name the source wrote is in scope
    /// by construction, while a rebuilt spelling needs the class to be nameable
    /// where the test is written
    Written,
    /// nothing in the source spells this target on its own — one arm of a union
    /// the source named with a single word — so it is rebuilt
    Rebuilt(String),
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

/// why a type test's target cannot be checked at runtime
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasedTargetReason {
    /// the target says nothing a runtime check could look for — `Any` and
    /// `Unknown` admit every value, so the test has no content
    Dynamic,
    /// a callable type: the runtime sees that a value is callable and nothing
    /// more, so its parameter and return types would be assumed rather than
    /// checked
    Callable,
    /// a `TypedDict`: its instances are plain dicts, so a runtime check can
    /// only ask whether the value is a `dict` and would assume every key
    TypedDict,
    /// an intersection of types has no single runtime form to test against
    Intersection,
    /// a protocol that is not `@runtime_checkable` and has a member with no
    /// runtime spelling, so neither python's presence check nor a structural
    /// one against the value's reified annotations can answer it
    NonRuntimeCheckableProtocol,
    /// a `type[…]` whose argument `issubclass` cannot take — `Any`, or a
    /// protocol with a data member
    Subclass,
    /// a literal whose value the runtime cannot be asked to compare: a `float`
    /// or `complex`, whose equality does not decide the type (`0.0 == -0.0`),
    /// or `LiteralString`, which is a property of how a value was written and
    /// not of the value
    UncomparableLiteral,
    /// the target has no runtime spelling at all — a type the checker can name
    /// but the emitted python cannot evaluate
    Unspellable,
    /// a builtin collection (`list` / `dict` / `set` / `frozenset` / `tuple`)
    /// erases its type arguments — its C-level instances reject
    /// `__orig_class__` entirely
    BuiltinCollection,
    /// a protocol's instances record their own concrete class in
    /// `__orig_class__`, never the protocol, so a probe could never match it;
    /// and a structural `isinstance` check sees no type arguments (and raises
    /// outright unless the protocol is `@runtime_checkable`)
    Protocol,
    /// the target class cannot be subscripted at runtime (`memoryview[int]`),
    /// so the runtime residue — which writes the target as spelled — has no
    /// expression to evaluate
    NotSubscriptable,
}

/// a type whose arms are specializations of one *erased* origin, differing in
/// a single type argument — `list[int] | list[str]`. the runtime cannot tell
/// those arms apart, so a value of this type carries no record of which one it
/// is; the specialization has to travel with the call instead
pub struct ErasedUnion {
    /// the shared origin's runtime spelling — `list`
    pub origin: String,
    /// which type-argument position the arms differ in
    pub position: usize,
    /// the differing argument of each arm, in declaration order — `int`, `str`
    pub arms: Vec<String>,
    /// the arguments the arms agree on, by position, so the rewritten
    /// annotation can put them back (`dict[str, int] | dict[str, bool]` keeps
    /// `str` in position 0)
    pub fixed: Vec<(usize, String)>,
}

/// classify `ty` as an [`ErasedUnion`], or `None` when it is anything else.
///
/// every arm must be a specialization of the *same* builtin-collection origin
/// and every argument must have a runtime spelling — an unspellable argument
/// (a scope-local class, a dynamic type) disqualifies the whole union rather
/// than producing a rewrite that cannot be spelled back out
pub(crate) fn erased_union<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> Option<ErasedUnion> {
    let Type::Union(union) = ty else {
        return None;
    };
    let mut origin: Option<ClassLiteral<'db>> = None;
    let mut rows: Vec<Vec<Type<'db>>> = Vec::new();
    for element in union.elements(db) {
        let Type::NominalInstance(instance) = element else {
            return None;
        };
        let ClassType::Generic(alias) = instance.class(db, env) else {
            return None;
        };
        let arm_origin = alias.origin(db);
        if !matches!(
            erased_target_reason(db, ClassLiteral::Static(arm_origin)),
            Some(ErasedTargetReason::BuiltinCollection)
        ) {
            return None;
        }
        match origin {
            Some(seen) if seen != ClassLiteral::Static(arm_origin) => return None,
            Some(_) => {}
            None => origin = Some(ClassLiteral::Static(arm_origin)),
        }
        rows.push(alias.specialization(db).types(db).to_vec());
    }
    let origin = origin?;
    // a single arm is already decidable — nothing to discriminate
    if rows.len() < 2 {
        return None;
    }
    let width = rows.first()?.len();
    if rows.iter().any(|row| row.len() != width) {
        return None;
    }

    // exactly one position may vary; the rest must agree across every arm, or
    // no single type parameter can stand for the difference
    let mut varying = None;
    for position in 0..width {
        let first = rows[0][position];
        if rows.iter().all(|row| row[position] == first) {
            continue;
        }
        if varying.is_some() {
            return None;
        }
        varying = Some(position);
    }
    let position = varying?;
    // the arms must be pairwise distinct at that position, else two of them are
    // the same specialization and the test could not tell them apart anyway
    if !rows.iter().map(|row| row[position]).all_unique() {
        return None;
    }

    let arms = rows
        .iter()
        .map(|row| runtime_spelling(db, env, file, row[position].promote_in(db, env, file)))
        .collect::<Option<Vec<_>>>()?;
    let fixed = (0..width)
        .filter(|index| *index != position)
        .map(|index| {
            runtime_spelling(db, env, file, rows[0][index].promote_in(db, env, file))
                .map(|text| (index, text))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ErasedUnion {
        origin: spell_class_literal(db, env, file, origin)?,
        position,
        arms,
        fixed,
    })
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
fn parametric_is_target<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    rhs_ty: Type<'db>,
) -> Option<GenericAlias<'db>> {
    match rhs_ty {
        Type::GenericAlias(alias) => Some(alias),
        _ => {
            let value = rhs_ty.as_type_alias()?.value_type(db);
            match value {
                Type::GenericAlias(alias) => Some(alias),
                Type::NominalInstance(instance) => match instance.class(db, env) {
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
    env: &ProgramEnvironment<'db>,
    target_ty: Type<'db>,
) -> Option<GenericAlias<'db>> {
    match target_ty {
        Type::NominalInstance(instance) => match instance.class(db, env) {
            ClassType::Generic(alias) => Some(alias),
            ClassType::NonGeneric(_) => None,
        },
        Type::ProtocolInstance(instance) => match instance.inner {
            Protocol::Materialized(_) => None,
            crate::types::instance::Protocol::FromClass(protocol_class) => match *protocol_class {
                ClassType::Generic(alias) => Some(alias),
                ClassType::NonGeneric(_) => None,
            },
            crate::types::instance::Protocol::Synthesized(_) => None,
        },
        // an alias name still resolves through the value-position rules
        _ => parametric_is_target(db, env, target_ty),
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
    env: &ProgramEnvironment<'db>,
    file: File,
    lhs_ty: Type<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    rhs_node: Option<&ast::Expr>,
) -> ParametricIsPlan {
    let target_origin = ClassLiteral::Static(rhs_alias.origin(db));
    let target_args_ast: Vec<&ast::Expr> = match rhs_node {
        Some(ast::Expr::Subscript(subscript)) => match subscript.slice.as_ref() {
            ast::Expr::Tuple(tuple) => tuple.elts.iter().collect(),
            single => vec![single],
        },
        _ => Vec::new(),
    };
    let plan = classify_value(
        db,
        env,
        file,
        lhs_ty.promote(db, env),
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
    if let ParametricIsPlan::Probe { .. } = plan
        && let Some(ErasedTargetReason::Protocol) = erased_target_reason(db, target_origin)
    {
        return protocol_structural_members(db, env, file, ClassType::Generic(rhs_alias))
            .map(|checks| ParametricIsPlan::ProtocolStructural(checks.into_boxed_slice()))
            .unwrap_or(ParametricIsPlan::ErasedTarget(ErasedTargetReason::Protocol));
    }
    // both remaining runtime residues write the target *as spelled* into the
    // emitted python — `_parametric_is(x, list[int], …)` and `T == list[int]` —
    // so a target the runtime refuses to subscript leaves nothing to evaluate.
    // a static fold emits a constant instead and is unaffected
    //
    // here the target is the source's own type expression rather than something
    // invented, so the threshold is the opposite one: reject only on positive
    // evidence *against*. rejecting an unsettled target would turn
    // `x is Sequence[int]` — which runs perfectly well — into an error
    if matches!(
        plan,
        ParametricIsPlan::Probe { .. } | ParametricIsPlan::TokenEq(_)
    ) && runtime_subscript(db, env, target_origin) == RuntimeSubscript::Unsupported
    {
        return ParametricIsPlan::ErasedTarget(ErasedTargetReason::NotSubscriptable);
    }
    plan
}

/// basedpython: how a type test — `value is Target` — resolves, for *any*
/// target type.
///
/// The right-hand side of a type test is a type expression, so an unusable
/// target has already been reported as `invalid-type-form` by the time this
/// runs. What is left is a narrower question: does the type it named have a
/// runtime form? A test must *earn* its `True` — it narrows — so a target the
/// runtime can only partly check is rejected rather than approximated. That
/// rules out a `TypedDict` (its instances are plain dicts), a callable type
/// (the runtime sees only that a value is callable), and `Any`.
///
/// `target_node` is the source the target was written as, which the
/// specialization plans use to spell type arguments back out.
///
/// `value_ty` is taken exactly as the value has it — a construction's `final A`
/// is disjoint from an unrelated class in a way a plain `A` is not, and that is
/// the whole point of tracking it. Only the specialization path widens, where a
/// literal argument would otherwise decide a test the runtime cannot.
pub(crate) fn type_test_plan<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    value_ty: Type<'db>,
    target: Type<'db>,
    target_node: Option<&ast::Expr>,
) -> ParametricIsPlan {
    type_test_plan_seen(
        db,
        env,
        file,
        value_ty,
        target,
        target_node,
        &mut Vec::new(),
    )
}

/// [`type_test_plan`] carrying the aliases already opened on the way here.
///
/// A `type` alias may name itself — `type A = int | B` with `type B = str | A`
/// — and the plan for one is the plan for its value, so following that without
/// a record would not terminate.
fn type_test_plan_seen<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    value_ty: Type<'db>,
    target: Type<'db>,
    target_node: Option<&ast::Expr>,
    open: &mut Vec<Type<'db>>,
) -> ParametricIsPlan {
    // an `Unknown` target is not evidence of anything: every value is a subtype
    // of it, and folding on that would answer `True` for a test the source got
    // wrong somewhere else
    if target.is_dynamic() {
        return runtime_test_plan(db, env, file, value_ty, target, target_node, open);
    }
    // a target that resolves statically needs no runtime residue at all, and
    // answering it here keeps every unspellable-but-decidable target working:
    // `x is Never` is `False` without the runtime ever seeing `Never`
    if value_ty.is_subtype_of(db, env, target) {
        return ParametricIsPlan::Fold(true);
    }
    if value_ty.is_disjoint_from(db, env, target) {
        return ParametricIsPlan::Fold(false);
    }
    runtime_test_plan(db, env, file, value_ty, target, target_node, open)
}

/// The runtime residue of a type test whose answer the static types do not
/// already give. Split from [`type_test_plan`] so a union arm can be planned
/// without re-asking the static question the whole test already answered.
fn runtime_test_plan<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    value_ty: Type<'db>,
    target: Type<'db>,
    target_node: Option<&ast::Expr>,
    open: &mut Vec<Type<'db>>,
) -> ParametricIsPlan {
    if target.is_none(db) {
        return ParametricIsPlan::IsNone;
    }
    match target {
        // an alias stands for its value, and a use-site modifier (`final T`)
        // constrains how the value may be used rather than what it is at
        // runtime — neither adds anything a runtime check could look for
        Type::TypeAlias(alias) => {
            // an alias that names itself has no value to resolve to, and the
            // definition is reported where it is written
            if open.contains(&target) {
                return ParametricIsPlan::Unresolved;
            }
            open.push(target);
            let plan = runtime_test_plan(db, env, file, value_ty, alias.value_type(db), None, open);
            open.pop();
            plan
        }
        // a use-site modifier is not python, so the source no longer spells
        // what is left once it is dropped
        Type::Restricted(restricted) => runtime_test_plan(
            db,
            env,
            file,
            value_ty,
            restricted.value_type(db),
            None,
            open,
        ),

        // a value satisfies a union as soon as it satisfies one arm. the arms
        // are planned separately because a union need not be spelled as one:
        // `type AU = int | str` names it with a single identifier, and
        // `isinstance` cannot take the alias object that identifier evaluates to
        Type::Union(union) => {
            let mut arms = Vec::with_capacity(union.elements(db).len());
            for element in union.elements(db) {
                // the source spells the union, not this arm, so the arm's own
                // plan may not read type arguments back out of it
                let arm = type_test_plan_seen(db, env, file, value_ty, *element, None, open);
                // one unusable arm makes the whole disjunction unusable: it may
                // not quietly fold to `False`, since that would answer `False`
                // for a value the arm would have accepted
                if let ParametricIsPlan::ErasedTarget(reason) = arm {
                    return ParametricIsPlan::ErasedTarget(reason);
                }
                // an arm the checker could not read has no spelling of its own,
                // and the source spells the union rather than the arm — so the
                // whole test falls back to what the source wrote
                if matches!(arm, ParametricIsPlan::Unresolved) {
                    return ParametricIsPlan::Unresolved;
                }
                arms.push(arm);
            }
            ParametricIsPlan::Union(arms.into_boxed_slice())
        }

        // a literal type holds exactly the values equal to it, which is what
        // the runtime compares. an enum member is a literal too, and its
        // equality is identity
        Type::LiteralValue(literal) => literal_target_plan(db, env, file, literal, target_node),

        // `type[C]`: the runtime can check this one in full — the value must be
        // a class, and a subclass of `C`
        Type::SubclassOf(subclass) => match subclass.subclass_of().into_class(db, env) {
            // `type[C]` is written as a subscript, so the source spells the
            // subscript rather than `C` — the spelling has to be rebuilt
            Some(class) => match spell_class(db, env, file, class) {
                Some(spelling) => ParametricIsPlan::Subclass(TargetSpelling::Rebuilt(spelling)),
                None => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable),
            },
            // `type[Any]`, or `type[<protocol>]` — the value must be a class,
            // and `issubclass` has nothing it can ask beyond that
            None => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Subclass),
        },

        Type::NominalInstance(_) | Type::ProtocolInstance(_) => {
            class_target_plan(db, env, file, value_ty, target, target_node)
        }

        // the value's type is carried by a reified type parameter on the
        // *target* side — `x is T` where `T` is reified spells `T` itself
        Type::TypeVar(bound_typevar) if is_reified_function_typevar(db, bound_typevar) => {
            ParametricIsPlan::Isinstance(TargetSpelling::Rebuilt(
                bound_typevar.name(db).to_string(),
            ))
        }

        // `Any` really does admit every value and is worth rejecting. `Unknown`
        // is what an already-reported error leaves behind, and a second report
        // on the same target would only repeat it
        Type::Dynamic(crate::types::DynamicType::Any) => {
            ParametricIsPlan::ErasedTarget(ErasedTargetReason::Dynamic)
        }
        Type::Dynamic(_) => ParametricIsPlan::Unresolved,
        // a bare `Callable` is exactly what `callable()` answers; only a
        // *signature* asks for something the value does not record
        Type::Callable(callable)
            if callable.signatures(db).iter().all(is_unannotated_signature) =>
        {
            ParametricIsPlan::IsCallable
        }
        Type::Callable(_) => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Callable),
        Type::Intersection(_) => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Intersection),
        // a `TypedDict`'s inhabitants are plain dicts: nothing at runtime
        // records which one a dict was built as
        Type::TypedDict(_) => ParametricIsPlan::ErasedTarget(ErasedTargetReason::TypedDict),
        _ => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable),
    }
}

/// The runtime test for a literal target. A `TypedDict`-like erasure is not
/// good enough here: the type holds exactly the values equal to the literal, so
/// the check is that equality.
fn literal_target_plan<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    literal: crate::types::LiteralValueType<'db>,
    target_node: Option<&ast::Expr>,
) -> ParametricIsPlan {
    let unspellable = ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable);
    // the guard names a builtin, whose spelling is fixed and always in scope
    let written = target_node
        .is_some_and(|node| node.is_literal_expr() || matches!(node, ast::Expr::UnaryOp(_)));
    let equality = |class: &str, value: String| ParametricIsPlan::Equality {
        class: class.to_owned(),
        value: if written {
            TargetSpelling::Written
        } else {
            TargetSpelling::Rebuilt(value)
        },
    };
    match literal.kind() {
        // a pattern is a set of strings, and the regular expression that spells
        // it decides exactly the language `matches_str` decides
        LiteralValueTypeKind::Template(template) => match template_pattern(db, env, template) {
            Some(pattern) => ParametricIsPlan::Pattern(pattern),
            None => unspellable,
        },
        LiteralValueTypeKind::Bool(value) => {
            equality("bool", (if value { "True" } else { "False" }).to_owned())
        }
        LiteralValueTypeKind::Int(value) => equality("int", value.as_i64().to_string()),
        LiteralValueTypeKind::String(value) => equality("str", python_str_literal(value.value(db))),
        LiteralValueTypeKind::Bytes(value) => {
            equality("bytes", python_bytes_literal(value.value(db)))
        }
        // an enum member is a singleton, so identity is exact — and it is the
        // comparison `Enum.__eq__` performs anyway
        LiteralValueTypeKind::Enum(member) => {
            match written_or_spelled(target_node, false, || {
                spell_class_literal(db, env, file, member.enum_class(db))
                    .map(|class| format!("{class}.{}", member.name(db)))
            }) {
                Some(spelling) => ParametricIsPlan::Identity(spelling),
                None => unspellable,
            }
        }
        // `LiteralString` is the *property* of having been written as a literal,
        // which nothing about a value at runtime records; `float` and `complex`
        // literals are values the checker tracks but does not promise are
        // distinguishable from equal ones
        LiteralValueTypeKind::LiteralString
        | LiteralValueTypeKind::Float(_)
        | LiteralValueTypeKind::Complex(_) => {
            ParametricIsPlan::ErasedTarget(ErasedTargetReason::UncomparableLiteral)
        }
    }
}

/// how a target is written into the emitted python: the spelling rebuilt from
/// the type, or the source's own text where that is what the runtime evaluates.
///
/// The rebuilt spelling is preferred, because the source names a *type* and the
/// emitted check needs a *value*, and the two part company far more often than
/// they look like they do: a PEP 695 alias evaluates to a `TypeAliasType`,
/// `Literal[…]` and `Annotated[…]` to special forms, `list[Any]` to a
/// subscripted generic — none of which `isinstance` will take. Falling back to
/// the source is for the one case rebuilding cannot express: a class the
/// emitting module cannot name as a global, such as an enum's attached variant
/// or a class imported under another name. Only a plain dotted name qualifies,
/// and only a [`Probe`](ParametricIsPlan::Probe) also accepts a subscript,
/// whose value is the specialization it unwinds.
fn written_or_spelled(
    target_node: Option<&ast::Expr>,
    subscript_evaluates: bool,
    spell: impl FnOnce() -> Option<String>,
) -> Option<TargetSpelling> {
    if let Some(spelling) = spell() {
        return Some(TargetSpelling::Rebuilt(spelling));
    }
    target_node
        .filter(|node| evaluates_to_its_target(node, subscript_evaluates))
        .map(|_| TargetSpelling::Written)
}

/// whether the runtime value of `node` is the thing the type it names denotes
fn evaluates_to_its_target(node: &ast::Expr, subscript_evaluates: bool) -> bool {
    match node {
        ast::Expr::Name(_) => true,
        ast::Expr::Attribute(attribute) => evaluates_to_its_target(&attribute.value, false),
        ast::Expr::Subscript(subscript) if subscript_evaluates => {
            evaluates_to_its_target(&subscript.value, false)
        }
        _ => false,
    }
}

/// a python string literal for `value`, escaped so the emitted source reads it
/// back character for character.
fn python_str_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ch if ch.is_control() || ch == '"' => push_escaped_char(ch, &mut out),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// a python bytes literal for `value`, written one `\xNN` escape per byte so
/// every byte round-trips whatever it is
fn python_bytes_literal(value: &[u8]) -> String {
    let mut out = String::with_capacity(value.len() * 4 + 3);
    out.push_str("b\"");
    for byte in value {
        let _ = write!(out, "\\x{byte:02x}");
    }
    out.push('"');
    out
}

/// The runtime test for an instance target — the common case, and the one the
/// parametric engine already answered for a specialization.
fn class_target_plan<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    value_ty: Type<'db>,
    target: Type<'db>,
    target_node: Option<&ast::Expr>,
) -> ParametricIsPlan {
    let Some(class) = target_class(db, env, target) else {
        return ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable);
    };
    let literal = class.class_literal(db);
    // a `TypedDict`'s instances are plain dicts, so the only runtime question
    // is whether the value is a `dict` — every key and value type would be
    // assumed. a test that must earn its `True` cannot assume them
    if literal.is_typed_dict(db) {
        return ParametricIsPlan::ErasedTarget(ErasedTargetReason::TypedDict);
    }
    match class {
        // a bare generic class in a type expression means every specialization
        // of it, which is the class itself — and `isinstance` answers exactly
        // that. only *written* arguments give the parametric engine something
        // to check
        ClassType::Generic(alias)
            if alias
                .specialization(db)
                .types(db)
                .iter()
                .all(Type::is_dynamic) =>
        {
            match written_or_spelled(target_node, false, || {
                spell_class_literal(db, env, file, ClassLiteral::Static(alias.origin(db)))
            }) {
                Some(spelling) => ParametricIsPlan::Isinstance(spelling),
                None => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable),
            }
        }
        // a specialization keeps the engine it always had: reified cells,
        // an `__orig_class__` probe, or a structural protocol check
        ClassType::Generic(alias) => {
            classify_parametric_is(db, env, file, value_ty, alias, target_node)
        }
        ClassType::NonGeneric(_) => {
            // an interface something visibly conforms to is answered by the
            // registry rather than by the class hierarchy, so it is checkable
            // even though a conforming type is not a subclass
            if let Some(members) = conformance_members(db, file, class) {
                return match written_or_spelled(target_node, false, || {
                    spell_class(db, env, file, class)
                }) {
                    Some(target) => ParametricIsPlan::Conformance { target, members },
                    None => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable),
                };
            }
            if let Some(protocol) = class.into_protocol_class(db) {
                // `@runtime_checkable` is the author's own statement that
                // `isinstance` may take the class, and it is what python then
                // checks — that the members are present
                if !protocol.is_runtime_checkable(db) {
                    // basedpython reifies class annotations, so a protocol whose
                    // members all have a runtime spelling can be checked against
                    // them member by member — a stricter answer than presence,
                    // and the only one available without the decorator
                    return match protocol_structural_members(db, env, file, class) {
                        Some(checks) => {
                            ParametricIsPlan::ProtocolStructural(checks.into_boxed_slice())
                        }
                        None => ParametricIsPlan::ErasedTarget(
                            ErasedTargetReason::NonRuntimeCheckableProtocol,
                        ),
                    };
                }
            }
            match written_or_spelled(target_node, false, || spell_class(db, env, file, class)) {
                Some(spelling) => ParametricIsPlan::Isinstance(spelling),
                None => ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable),
            }
        }
    }
}

/// The required member names of `class` when something in `file`'s scope
/// visibly conforms to it, or `None` when it is not a conformance interface.
fn conformance_members<'db>(
    db: &'db dyn Db,
    file: File,
    class: ClassType<'db>,
) -> Option<Vec<String>> {
    let conformed = crate::types::conformance::visible_conformances(db, file)
        .iter()
        .any(|(_, declared)| declared.class_literal(db) == class.class_literal(db));
    conformed.then(|| {
        crate::types::conformance::interface_requirements(db, class)
            .iter()
            .map(ToString::to_string)
            .collect()
    })
}

/// whether a callable signature says nothing beyond "this is callable" — the
/// gradual form `Callable[..., Any]` the bare `Callable` denotes
fn is_unannotated_signature(signature: &crate::types::signatures::Signature<'_>) -> bool {
    signature.parameters().is_gradual() && signature.return_ty.is_dynamic()
}

/// The class an instance target names, for both the nominal and the protocol
/// spelling of one.
fn target_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    target: Type<'db>,
) -> Option<ClassType<'db>> {
    match target {
        Type::NominalInstance(instance) => Some(instance.class(db, env)),
        Type::ProtocolInstance(instance) => match instance.inner {
            crate::types::instance::Protocol::FromClass(class) => Some(*class),
            crate::types::instance::Protocol::Materialized(_)
            | crate::types::instance::Protocol::Synthesized(_) => None,
        },
        _ => None,
    }
}

/// The regular expression that matches exactly the strings a template literal
/// type produces, for `re.fullmatch`, or `None` when one of its holes has no
/// regular-expression spelling.
///
/// The holes are read through the same [`HoleShape`](crate::types::template::HoleShape)
/// classification
/// `matches_str` uses, so the runtime test and the static one decide the same
/// language rather than two that happen to agree on the cases anyone tried.
fn template_pattern<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    template: crate::types::template::TemplateLiteralType<'db>,
) -> Option<String> {
    let mut pattern = String::new();
    for part in template.parts(db) {
        match part {
            crate::types::template::TemplatePart::Text(text) => {
                escape_regex(text.as_str(), &mut pattern);
            }
            crate::types::template::TemplatePart::Hole(hole) => {
                pattern.push_str(crate::types::template::HoleShape::of(db, env, *hole).regex()?);
            }
        }
    }
    Some(pattern)
}

/// Append `text` to `pattern` with every character python's `re` gives a
/// meaning escaped, so the text matches itself.
///
/// A character the emitted source cannot carry — anything the `str` escaping
/// below would have to spell — is written as its own `\\xNN` / `\\uNNNN`
/// escape, which `re` reads as that character. Passing one through raw would
/// put a literal control byte in the python, and CPython refuses to compile a
/// source containing a NUL at all.
fn escape_regex(text: &str, pattern: &mut String) {
    for ch in text.chars() {
        if "\\.^$*+?()[]{}|-#&~".contains(ch) {
            pattern.push('\\');
            pattern.push(ch);
        } else if ch.is_control() || ch == '"' {
            push_escaped_char(ch, pattern);
        } else {
            pattern.push(ch);
        }
    }
}

/// Write `ch` as the escape a python string literal reads back as that
/// character. Used for anything the emitted source cannot carry raw.
fn push_escaped_char(ch: char, out: &mut String) {
    match ch {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '"' => out.push_str("\\\""),
        ch if (ch as u32) < 0x100 => {
            let _ = write!(out, "\\x{:02x}", ch as u32);
        }
        ch if (ch as u32) < 0x1_0000 => {
            let _ = write!(out, "\\u{:04x}", ch as u32);
        }
        ch => {
            let _ = write!(out, "\\U{:08x}", ch as u32);
        }
    }
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
    env: &ProgramEnvironment<'db>,
    file: File,
    class: ClassType<'db>,
) -> Option<Vec<ProtocolMemberCheck>> {
    let protocol_class = class.into_protocol_class(db)?;
    let mut checks = Vec::new();
    for member in protocol_class.interface(db).members(db) {
        let name = member.name().to_owned();
        let check = match member.reified_member_shape(db, env)? {
            ReifiedMember::Attribute {
                ty,
                readable,
                writable,
            } => {
                let expected = protocol_member_spelling(db, env, file, ty)?;
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
                    let expected = protocol_member_spelling(db, env, file, param_ty)?;
                    param_checks.push((expected, ArgVariance::Contravariant));
                }
                let ret = match reified_return_check(db, env, file, ret) {
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
fn protocol_member_spelling<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> Option<String> {
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
    runtime_spelling(db, env, file, ty)
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

fn reified_return_check<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ret: Type<'db>,
) -> ReturnCheck {
    if ret.is_none(db) || ret.is_dynamic() || is_object_instance(db, env, ret) {
        return ReturnCheck::Skip;
    }
    match protocol_member_spelling(db, env, file, ret) {
        Some(expected) => ReturnCheck::Check(expected),
        None => ReturnCheck::Unspellable,
    }
}

fn is_object_instance<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> bool {
    matches!(ty, Type::NominalInstance(instance)
        if instance.class(db, env).class_literal(db).is_known(db, KnownClass::Object))
}

fn classify_value<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    value_ty: Type<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    target_args_ast: &[&ast::Expr],
    rhs_node: Option<&ast::Expr>,
) -> ParametricIsPlan {
    let target_origin = ClassLiteral::Static(rhs_alias.origin(db));
    // when the value's type is carried by a reified type parameter, the answer
    // lives in a runtime cell rather than the static type — extract the cell
    // comparisons before falling back to static subtyping
    if value_ty.has_typevar(db, env)
        && let Some(plan) = try_token_eq(
            db,
            env,
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
    let target_instance = Type::instance(db, env, ClassType::Generic(rhs_alias));
    if value_ty.is_subtype_of(db, env, target_instance) {
        ParametricIsPlan::Fold(true)
    } else if value_ty.is_disjoint_from(db, env, target_instance) {
        ParametricIsPlan::Fold(false)
    } else {
        // undecidable statically; `classify_parametric_is` turns this into a
        // runtime probe (user generic) or an erased-target error (builtin)
        // the source's own spelling is preferred: a name it wrote is in scope,
        // and the runtime probe unwraps a `TypeAliasType` for itself
        let Some(target) = written_or_spelled(rhs_node, true, || {
            spell_class(db, env, file, ClassType::Generic(rhs_alias))
        }) else {
            return ParametricIsPlan::ErasedTarget(ErasedTargetReason::Unspellable);
        };
        ParametricIsPlan::Probe {
            target,
            variances: target_variances(db, rhs_alias),
        }
    }
}

/// The reified-cell comparisons for a value whose static type is (or is built
/// from) a reified type parameter — `x: T` against `C[args]` is `T == C[args]`,
/// `x: list[T]` against `list[int]` is `T == int`. `None` when the value is
/// not so shaped (the caller then resolves it statically).
fn try_token_eq<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value_ty: Type<'db>,
    target_origin: ClassLiteral<'db>,
    rhs_alias: crate::types::class::GenericAlias<'db>,
    target_args_ast: &[&ast::Expr],
    rhs_node: Option<&ast::Expr>,
) -> Option<ParametricIsPlan> {
    match value_ty {
        // `x: T is <target>` compares the reified `T` cell against the target
        // *as spelled*. that is only sound when the source evaluates to the
        // specialization itself — a direct subscript. an alias name would
        // compare against the alias object (or, for a PEP 695 alias, a
        // `TypeAliasType` wrapper), so it falls through to the static resolution
        Type::TypeVar(bound_typevar)
            if is_reified_function_typevar(db, bound_typevar)
                && matches!(rhs_node, Some(ast::Expr::Subscript(_))) =>
        {
            Some(ParametricIsPlan::TokenEq(vec![(
                bound_typevar.name(db).clone(),
                rhs_node.expect("guarded above").range(),
            )]))
        }
        Type::NominalInstance(instance) => {
            let ClassType::Generic(alias) = instance.class(db, env) else {
                return None;
            };
            if ClassLiteral::Static(alias.origin(db)) != target_origin {
                return None;
            }
            let mut tokens = Vec::new();
            unify_specializations(
                db,
                env,
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
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> Option<(String, Box<[ArgVariance]>)> {
    let Type::NominalInstance(instance) = ty else {
        return None;
    };
    let ClassType::Generic(alias) = instance.class(db, env) else {
        return None;
    };
    let origin = ClassLiteral::Static(alias.origin(db));
    // a target whose instances don't carry a usable `__orig_class__` — a
    // builtin collection (erased arguments) or a protocol — has nothing to
    // probe, so the base `isinstance` check is all that's sound
    if erased_target_reason(db, origin).is_some() {
        return None;
    }
    let spelling = spell_class(db, env, file, ClassType::Generic(alias))?;
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
            let declared = bound_typevar.probe_variance(db);
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
    env: &ProgramEnvironment<'db>,
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
                        env,
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
            env,
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
    env: &ProgramEnvironment<'db>,
    value: Type<'db>,
    target: Type<'db>,
    target_ast: Option<&ast::Expr>,
    tokens: &mut Vec<(Name, TextRange)>,
) -> Result<(), ()> {
    if value == target || value.is_equivalent_to(db, env, target) {
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
        && let (ClassType::Generic(value_alias), ClassType::Generic(target_alias)) = (
            value_instance.class(db, env),
            target_instance.class(db, env),
        )
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
            env,
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
    let module = parsed_module(db, db.program_file(def_file).python_file(db)).load(db);
    let ty_python_core::definition::DefinitionKind::Function(function) = definition.kind(db) else {
        return false;
    };
    let node = function.node(&module);
    crate::reified::reified_type_param_names(def_file.source_type(db), node)
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
    env: &ProgramEnvironment<'db>,
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
            let base_interface = type_param_interface(db, env, base)?;
            let sub_interface = type_param_interface(db, env, sub)?;
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
                if !base_admissible.is_assignable_to(db, env, *sub_admissible) {
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
    env: &ProgramEnvironment<'db>,
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
                    (typevar.name(db), typevar.declared_ceiling(db, env))
                })
                .collect()
        })
        .unwrap_or_default();

    let module = parsed_module(db, overload_literal.program_file(db).python_file(db)).load(db);
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
