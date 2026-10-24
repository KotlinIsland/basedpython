//! basedpython: the type arguments a generic base takes from the class that inherits it, when
//! the class writes the base without them
//!
//! ```by
//! class A[T]:
//!     x: T
//!
//! class B(A):  # inherits `A[int]`
//!     x = 1
//! ```
//!
//! every member `B` writes itself overrides the one `A` declares, so it has to fit `A`'s
//! declaration. the type arguments `A` is inherited at are the ones that make every override fit,
//! solved the way a call solves a generic function's type parameters from its arguments
//!
//! only what `B` *writes* counts, never what it inherits: an unannotated method parameter takes
//! its type from the method it overrides, so reading it here would make the base depend on itself

use std::cell::RefCell;

use ruff_db::parsed::parsed_module;
use ruff_python_ast::{self as ast, name::Name};
use rustc_hash::FxHashSet;
use ty_python_core::{
    definition::{Definition, DefinitionKind, DefinitionState},
    place_table, semantic_index, use_def_map,
};

use crate::place::promote_undeclared;
use crate::types::constraints::ConstraintSetBuilder;
use crate::types::generics::{GenericContext, Specialization, SpecializationBuilder};
use crate::types::overrides::is_constructor_like_method;
use crate::types::signatures::{ReturnCallableTypeVarScope, Signature};
use crate::types::{
    BoundTypeVarInstance, StaticClassLiteral, Type, TypeQualifiers, TypeVarVariance, any_over_type,
    binding_type, inferred_declaration,
};
use crate::{Db, ProgramEnvironment};

/// the specialization of `base` that `class` inherits when its header writes `base` with no type
/// arguments
///
/// every type parameter of `base` an override constrains takes what the overrides solve it to,
/// and every other one its default. `None` — the base is inherited as written, and
/// `missing-type-argument` says so where an argument is needed — unless:
///
/// - `base` can be subscripted as the class is made, since the transpiled class writes the
///   arguments into its header
/// - some override constrains a type parameter, and every type parameter without a default
/// - each solution is a type the class header could write: nothing gradual, and no type variable
///   but the class's own
/// - every override fits the base at that specialization
#[salsa::tracked(returns(copy), cycle_initial=|_, _, _, _| None, heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn inferred_base_specialization<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    base: StaticClassLiteral<'db>,
) -> Option<Specialization<'db>> {
    let generic_context = base.generic_context(db)?;
    let env = &ProgramEnvironment::from_scope(class.body_scope(db));
    if !is_subscriptable_at_runtime(db, env, base) {
        return None;
    }

    let declaring = base.identity_specialization(db);
    let declaring_instance = Type::instance(db, env, declaring);
    let declaring_object = Type::from(declaring);
    let mentions_base_typevar = |ty: Type<'db>| {
        any_over_type(db, env, ty, false, |ty| {
            matches!(ty, Type::TypeVar(typevar)
                if generic_context.variables(db).any(|variable| variable == unattributed(db, typevar)))
        })
    };

    let mut overrides = Vec::new();
    for written in written_members(db, class) {
        match written {
            WrittenMember::Attribute { name, definition } => {
                // the base is read first, so a member it does not declare in terms of its type
                // parameters costs nothing
                let member = declaring_instance.member(db, env, &name);
                if member.qualifiers.intersects(RENAMED) {
                    continue;
                }
                let Some(declared) = member.place.ignore_possibly_undefined() else {
                    continue;
                };
                if !mentions_base_typevar(declared) {
                    continue;
                }
                let Some(written) = written_attribute_type(db, env, definition) else {
                    continue;
                };
                // an attribute that can be assigned through the base is invariant: a subclass
                // that narrowed it would let a write through the base store a value the
                // subclass rules out
                let fit = if member
                    .qualifiers
                    .intersects(TypeQualifiers::FINAL | TypeQualifiers::READ_ONLY)
                {
                    TypeVarVariance::Covariant
                } else {
                    TypeVarVariance::Invariant
                };
                overrides.push(Override {
                    declared,
                    written,
                    fit,
                });
            }
            WrittenMember::Method { name, definition } => {
                let Some(declared) = declaring_object
                    .member(db, env, &name)
                    .place
                    .ignore_possibly_undefined()
                    .and_then(|declared| unbound_method(db, env, declared))
                else {
                    continue;
                };
                if !declared
                    .parameters()
                    .iter()
                    .any(|parameter| mentions_base_typevar(parameter.annotated_type()))
                    && !mentions_base_typevar(declared.return_ty)
                {
                    continue;
                }
                let Some(written) = written_method(db, env, definition) else {
                    continue;
                };
                overrides.extend(method_overrides(&declared, &written));
            }
        }
    }
    if overrides.is_empty() {
        return None;
    }

    let constraints = ConstraintSetBuilder::new();
    let mut builder = SpecializationBuilder::new(db, env, &constraints, generic_context);
    for Override {
        declared,
        written,
        fit,
    } in &overrides
    {
        // a member that cannot fit the declaration at any specialization is the override
        // check's to report, against the base as it is written
        builder
            .infer_with_polarity(*declared, *written, *fit)
            .ok()?;
    }
    let inference = builder.build_inference_with(|_, _| None).ok()?;
    let solved = RefCell::new(Vec::new());
    inference.merged_specialization_with(db, |typevar, inferred| {
        solved.borrow_mut().push((typevar, inferred));
        None
    });
    let solved = solved.into_inner();

    // the class header can name its own type parameters, `class H[U](A[U])`, and nothing else
    // a solution could mention: not a method's type parameter, not the `Self` of a method, not
    // an anonymous `some` hole
    let own: FxHashSet<BoundTypeVarInstance<'db>> = class
        .generic_context(db)
        .map(|context| context.variables(db).collect())
        .unwrap_or_default();
    let writable = |ty: Type<'db>| {
        !ty.has_dynamic(db, env)
            && !any_over_type(
                db,
                env,
                ty,
                false,
                |ty| matches!(ty, Type::TypeVar(typevar) if !own.contains(&unattributed(db, typevar))),
            )
    };
    let mut any_solved = false;
    for (typevar, inferred) in &solved {
        match inferred {
            Some(inferred) if writable(*inferred) => any_solved = true,
            Some(_) => return None,
            None if typevar.default_type(db).is_some() => {}
            None => return None,
        }
    }
    if !any_solved {
        return None;
    }

    let specialization =
        generic_context.specialize_recursive(db, solved.iter().map(|(_, inferred)| *inferred));
    overrides
        .iter()
        .all(
            |Override {
                 declared,
                 written,
                 fit,
             }| {
                let declared = declared.apply_specialization(db, specialization);
                match fit {
                    TypeVarVariance::Invariant => written.is_equivalent_to(db, env, declared),
                    TypeVarVariance::Contravariant => declared.is_assignable_to(db, env, *written),
                    TypeVarVariance::Covariant | TypeVarVariance::Bivariant => {
                        written.is_assignable_to(db, env, declared)
                    }
                }
            },
        )
        .then_some(specialization)
}

/// the type variable `typevar` names: `P` for `P.args` and `P.kwargs`, which are how a callable
/// spells a `ParamSpec`
fn unattributed<'db>(
    db: &'db dyn Db,
    typevar: BoundTypeVarInstance<'db>,
) -> BoundTypeVarInstance<'db> {
    if typevar.paramspec_attr(db).is_some() {
        typevar.without_paramspec_attr(db)
    } else {
        typevar
    }
}

/// a member declared with a visibility keyword is emitted under a name of its own, so it neither
/// overrides nor is overridden by a member written under the plain one
const RENAMED: TypeQualifiers = TypeQualifiers::PRIVATE.union(TypeQualifiers::PROTECTED);

/// whether `class[...]` can be evaluated as a class header is: the transpiled subclass writes its
/// inferred arguments there
///
/// a class whose source runs is generic only through the machinery that makes it subscriptable
/// — a type parameter list, `Generic[...]`, or a subscriptable base. a stub describes a class it
/// does not define, and calls `map` generic without `map[int]` evaluating, so a stub's class is
/// subscriptable only where the stub says it has a `__class_getitem__`
fn is_subscriptable_at_runtime<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: StaticClassLiteral<'db>,
) -> bool {
    !class.file(db).is_stub(db)
        || Type::from(class.default_specialization(db))
            .member(db, env, "__class_getitem__")
            .place
            .ignore_possibly_undefined()
            .is_some()
}

/// one thing an override says about the base's type parameters: `written` has to relate to
/// `declared` the way `fit` says — exactly, as a subtype, or as a supertype
struct Override<'db> {
    declared: Type<'db>,
    written: Type<'db>,
    fit: TypeVarVariance,
}

/// a member `class` writes in its own body that can override one it inherits, not yet typed
enum WrittenMember<'db> {
    /// `x: int`, `let x: int`, or `x = 1`
    Attribute {
        name: Name,
        definition: Definition<'db>,
    },
    /// a `def`
    Method {
        name: Name,
        definition: Definition<'db>,
    },
}

/// every member `class` writes in its own body that can override one it inherits
fn written_members<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Vec<WrittenMember<'db>> {
    let body_scope = class.body_scope(db);
    let table = place_table(db, body_scope);
    let use_def = use_def_map(db, body_scope);
    let mut members = Vec::new();
    let mut declared = FxHashSet::default();

    for (symbol_id, _) in use_def.all_end_of_scope_symbol_declarations() {
        let name = table.symbol(symbol_id).name();
        for declaration in use_def.reachable_symbol_declarations(symbol_id) {
            let DefinitionState::Defined(definition) = declaration.declaration else {
                continue;
            };
            // a declared name's bindings are what its declarations say they are
            declared.insert(symbol_id);
            if !overrides_by_name(name) {
                continue;
            }
            let name = name.clone();
            match definition.kind(db) {
                DefinitionKind::AnnotatedAssignment(_) => {
                    members.push(WrittenMember::Attribute { name, definition });
                }
                DefinitionKind::Function(_) => {
                    members.push(WrittenMember::Method { name, definition });
                }
                _ => {}
            }
        }
    }

    for (symbol_id, bindings) in use_def.all_end_of_scope_symbol_bindings() {
        let name = table.symbol(symbol_id).name();
        if declared.contains(&symbol_id) || !overrides_by_name(name) {
            continue;
        }
        for binding in bindings {
            let DefinitionState::Defined(definition) = binding.binding else {
                continue;
            };
            if matches!(definition.kind(db), DefinitionKind::Assignment(_)) {
                members.push(WrittenMember::Attribute {
                    name: name.clone(),
                    definition,
                });
            }
        }
    }

    members
}

/// whether a member called `name` overrides the base's member of that name: a constructor is not
/// held to the one it overrides, and a name-mangled `__name` is a different name in every class
fn overrides_by_name(name: &str) -> bool {
    let mangled = name.starts_with("__") && !name.ends_with("__");
    !mangled && !is_constructor_like_method(name)
}

/// the type an attribute's own declaration gives it: its annotation, or for an untyped
/// class-body assignment the promoted type of its value. `None` for a declaration under a
/// visibility keyword, which is emitted under a name of its own
fn written_attribute_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
) -> Option<Type<'db>> {
    match definition.kind(db) {
        DefinitionKind::AnnotatedAssignment(_) => {
            let declared = inferred_declaration(db, definition).declared()?;
            (!declared.qualifiers().intersects(RENAMED)).then(|| declared.inner_type())
        }
        DefinitionKind::Assignment(_) => {
            Some(promote_undeclared(db, env, binding_type(db, definition)))
        }
        _ => None,
    }
}

/// the modifiers that leave a `def` an ordinary instance method under its own name. the others
/// either make it something else (`static`) or rename it (`private`, `protected`)
const PLAIN_METHOD_MODIFIERS: &[&str] = &["final", "abstract", "override", "open", "export"];

/// a `def` in the class body with its receiver bound away, typed from what it writes itself
///
/// a decorator makes the member whatever the decorator returns, which only inferring the decorator
/// says, so a decorated `def` is not read here
fn written_method<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
) -> Option<WrittenSignature<'db>> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let module = parsed_module(db, definition.program_file(db).python_file(db)).load(db);
    let node = function.node(&module);
    let plain = node.decorator_list.iter().all(|decorator| {
        matches!(&decorator.expression, ast::Expr::Name(marker)
            if marker.is_invalid() && PLAIN_METHOD_MODIFIERS.contains(&marker.id.as_str()))
    });
    if !plain {
        return None;
    }
    // a parameter list that repeats `_` is renamed by a lowering this does not run, so the
    // names it would compare by are not the ones the method has
    let names: Vec<_> = node
        .parameters
        .iter()
        .map(ast::AnyParameterRef::name)
        .collect();
    if names
        .iter()
        .enumerate()
        .any(|(i, name)| names[..i].contains(name))
    {
        return None;
    }

    let index = semantic_index(db, definition.program_file(db));
    let generic_context = node.type_params.as_ref().and_then(|type_params| {
        GenericContext::from_type_params(db, index, definition, type_params)
    });
    // the receiver is bound away below, so whether it is positional-only changes nothing
    let signature = Signature::from_function(
        db,
        env,
        generic_context,
        definition,
        node,
        true,
        ReturnCallableTypeVarScope::Public,
        None,
    );
    Some(WrittenSignature {
        signature: signature.bind_self(db, env, None),
        writes_return: node.returns.is_some() || node.is_asserts_return,
    })
}

/// a method's signature, typed from what it writes itself
struct WrittenSignature<'db> {
    signature: Signature<'db>,
    /// whether the `def` writes its return type. one it leaves out is read off its body, which is
    /// typed in terms of the very base being inferred
    writes_return: bool,
}

/// a method the base declares, read off its class object, with the receiver bound away the same
/// way [`written_method`] binds it
fn unbound_method<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    declared: Type<'db>,
) -> Option<Signature<'db>> {
    let Type::FunctionLiteral(function) = declared else {
        return None;
    };
    if function.is_classmethod(db) || function.is_staticmethod(db) {
        return None;
    }
    let [signature] = function.signature(db).overloads.as_slice() else {
        return None;
    };
    Some(signature.bind_self(db, env, None))
}

/// what overriding `declared` with `written` says about the base's type parameters
///
/// parameters correspond the way a call binds arguments to them: positional parameters by
/// position and keyword-only ones by name, while a `*args` or `**kwargs` stands for a run of
/// arguments that nothing corresponds to. a parameter the override leaves unannotated takes its
/// type from the one it overrides, so it says nothing, and neither does a return type it leaves out
fn method_overrides<'db>(
    declared: &Signature<'db>,
    written: &WrittenSignature<'db>,
) -> Vec<Override<'db>> {
    let mut overrides = Vec::new();
    if declared.parameters().is_standard() {
        let mut declared_positional = declared
            .parameters()
            .iter()
            .take_while(|parameter| parameter.is_positional());
        for parameter in written.signature.parameters() {
            let corresponding = if parameter.is_positional() {
                declared_positional.next()
            } else if parameter.is_keyword_only() {
                declared.parameters().iter().find(|candidate| {
                    candidate.is_keyword_only() && candidate.name() == parameter.name()
                })
            } else {
                None
            };
            if parameter.inferred_annotation {
                continue;
            }
            if let Some(corresponding) = corresponding {
                overrides.push(Override {
                    declared: corresponding.annotated_type(),
                    written: parameter.annotated_type(),
                    fit: TypeVarVariance::Contravariant,
                });
            }
        }
    }
    if written.writes_return {
        overrides.push(Override {
            declared: declared.return_ty,
            written: written.signature.return_ty,
            fit: TypeVarVariance::Covariant,
        });
    }
    overrides
}
