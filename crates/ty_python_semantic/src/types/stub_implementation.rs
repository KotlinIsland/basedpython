//! basedpython: a runtime module checked against the stub that describes it
//!
//! importers of a module that has a stub read the stub, so whatever the stub declares is what the
//! module's users are promised. the module's own body is held to the same promise: what the stub
//! declares for a function, a class attribute or a module variable the runtime module leaves
//! unannotated is the declared type there.
//!
//! ```python
//! # m.pyi                         # m.py
//! def f(a: int) -> str: ...       def f(a):
//!                                     reveal_type(a)  # int
//!                                     return a        # error: `int` is not `str`
//! X: int                          X = "a"             # error: `str` is not `int`
//! ```
//!
//! an explicit annotation in the runtime module stays authoritative there; where it disagrees
//! with the stub, that is reported by the stub consistency rules (see `stub_consistency`).
//!
//! a stub's types are in the stub's terms, and two of them have no meaning in the runtime module:
//! a class the stub itself defines is a different class from the runtime module's class of the
//! same name, and a type variable the stub binds is bound to the stub's declaration. a type that
//! names either is not carried over, so the runtime body is held to no more than the stub can say
//! about it in terms the body shares.

use ruff_python_ast::name::Name;
use rustc_hash::FxHashMap;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, semantic_index};

use crate::Db;
use crate::place::{Place, PlaceAndQualifiers};
use crate::stub_mapping::{runtime_module_stub, stub_counterpart_declarations, stub_scope};
use crate::types::definition::TypeDefinition;
use crate::types::function::OverloadLiteral;
use crate::types::generics::enclosing_binding_contexts;
use crate::types::signatures::{ReturnCallableTypeVarScope, Signature};
use crate::types::visitor::any_over_type;
use crate::types::{
    BindingContext, ProgramEnvironment, Type, TypeQualifiers, UnionType, infer_definition_types,
    inferred_declaration,
};

/// whether `ty`, a type the stub `stub` declares, means in the runtime module what it means in
/// the stub: it names no class the stub defines and no type variable bound anywhere but a scope
/// `is_visible` accepts
pub(crate) fn is_statable_in_runtime<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    stub: ProgramFile<'db>,
    is_visible: impl Fn(BindingContext<'db>) -> bool,
) -> bool {
    let stub_file = stub.file(db);
    !any_over_type(db, env, ty, false, |ty| {
        if let Type::TypeVar(typevar) = ty {
            return !is_visible(typevar.binding_context(db));
        }
        match ty.definition(db, env) {
            Some(
                TypeDefinition::StaticClass(definition)
                | TypeDefinition::DynamicClass(definition)
                | TypeDefinition::NewType(definition)
                | TypeDefinition::EnumMember(definition),
            ) => definition.file(db) == stub_file,
            _ => false,
        }
    })
}

/// the stub's declaration of a runtime function, read as the runtime function's own signature
/// reads it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct StubSignature<'db> {
    /// every parameter with the type the stub gives it, and the return type the stub writes
    pub(crate) signature: Signature<'db>,
    /// whether the stub's first parameter is bound by the receiver
    pub(crate) binds_receiver: bool,
    /// whether the stub's declaration is an `async def`, whose return type is what its
    /// coroutine resolves to
    pub(crate) is_async: bool,
    pub(crate) overload: OverloadLiteral<'db>,
}

/// the signatures the stub declares for the runtime function `definition`: one for a plain
/// declaration, one per overload, of the declarations in the code that runs
///
/// `None` where the module has no stub, or its stub declares no function of that name there
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn stub_signatures<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<Box<[StubSignature<'db>]>> {
    let declarations = stub_counterpart_declarations(db, definition).as_deref()?;
    let signatures: Box<[StubSignature<'db>]> = declarations
        .iter()
        .filter_map(|&declaration| {
            let function = infer_definition_types(db, declaration).function_type(declaration)?;
            let overload = function.literal(db).last_definition;
            Some(StubSignature {
                signature: overload
                    .raw_signature(db, ReturnCallableTypeVarScope::Public)
                    .clone(),
                binds_receiver: overload.binds_first_parameter(db),
                is_async: overload.header(db).is_async,
                overload,
            })
        })
        .collect();
    (!signatures.is_empty()).then_some(signatures)
}

/// the stub of the module `definition` is in, when it has one
pub(crate) fn definition_stub<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<ProgramFile<'db>> {
    runtime_module_stub(db, definition.program_file(db))
}

/// a test for whether a type variable is one the runtime declaration `definition` can name: one
/// bound by a scope around it, or by the declaration itself
pub(crate) fn visible_binding_contexts<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Vec<BindingContext<'db>> {
    let index = semantic_index(db, definition.program_file(db));
    let mut visible: Vec<BindingContext<'db>> =
        enclosing_binding_contexts(index, definition.file_scope(db)).collect();
    visible.push(definition.into());
    visible
}

/// the return type the stub declares for the runtime function `definition`, as the function's
/// own body returns it
///
/// a function the stub overloads returns what any of its overloads return, as the implementation
/// of overloads written in one file does. `None` where any declaration writes no return type, or
/// one that is not statable in the runtime module (see [`is_statable_in_runtime`]), and where the
/// runtime function is an `async def` that the stub does not declare as one: what such a function
/// resolves to is not what the stub's return type says
pub(crate) fn stub_return_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
    is_async: bool,
) -> Option<Type<'db>> {
    let signatures = stub_signatures(db, definition).as_deref()?;
    let stub = definition_stub(db, definition)?;
    let visible = visible_binding_contexts(db, definition);
    let mut returns = Vec::with_capacity(signatures.len());
    for stub_signature in signatures {
        if !stub_signature.overload.has_explicit_return_annotation(db) {
            return None;
        }
        let returned = match (is_async, stub_signature.is_async) {
            (true, true) | (false, false) => stub_signature.signature.return_ty,
            (false, true) => stub_signature.overload.signature(db).return_ty,
            (true, false) => return None,
        };
        if !is_statable_in_runtime(db, env, returned, stub, |context| {
            visible.contains(&context)
        }) {
            return None;
        }
        returns.push(returned);
    }
    Some(UnionType::from_elements(db, env, returns))
}

/// the types the stub declares for the variables of the runtime scope `scope`, a module's or a
/// class body's, by name: each one the stub declares with an annotation in the code that runs,
/// where the type is statable in the runtime module
///
/// what the runtime module leaves undeclared takes this as its declaration (see
/// [`stub_declared_type`]); a `Final` in the stub says importers may not reassign the name, which
/// the runtime module's own assignment of it does not do, so the qualifier is not carried
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn stub_declared_types<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> Option<FxHashMap<Name, PlaceAndQualifiers<'db>>> {
    let stub_scope = stub_scope(db, scope).as_ref()?;
    let stub = runtime_module_stub(db, scope.program_file(db))?;
    let env = ProgramEnvironment::from_scope(scope);
    let index = semantic_index(db, scope.program_file(db));
    let visible: Vec<BindingContext<'db>> =
        enclosing_binding_contexts(index, scope.file_scope_id(db)).collect();

    let mut declared = FxHashMap::default();
    for member in &stub_scope.members {
        let mut types = Vec::new();
        let mut qualifiers = TypeQualifiers::empty();
        for &definition in &member.stub {
            if !matches!(definition.kind(db), DefinitionKind::AnnotatedAssignment(_)) {
                continue;
            }
            let Some(declaration) = inferred_declaration(db, definition).declared() else {
                continue;
            };
            types.push(declaration.inner_type());
            qualifiers |= declaration.qualifiers();
        }
        if types.is_empty() {
            continue;
        }
        let ty = UnionType::from_elements(db, &env, types);
        if ty.is_unknown()
            || !is_statable_in_runtime(db, &env, ty, stub, |context| visible.contains(&context))
        {
            continue;
        }
        declared.insert(
            member.name.clone(),
            Place::declared(ty).with_qualifiers(qualifiers.difference(TypeQualifiers::FINAL)),
        );
    }
    declared.shrink_to_fit();
    (!declared.is_empty()).then_some(declared)
}

/// the declaration the stub supplies for `name` in the runtime scope `scope`, a module's or a
/// class body's, where the runtime module declares nothing for it itself
pub(crate) fn stub_declared_type<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    name: &str,
) -> Option<PlaceAndQualifiers<'db>> {
    // most modules have no stub, and asking the per-scope query for one would leave an entry
    // behind for every scope that looks a name up
    runtime_module_stub(db, scope.program_file(db))?;
    stub_declared_types(db, scope).as_ref()?.get(name).copied()
}
