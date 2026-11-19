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
//! a stub's types are in the stub's terms, and are read in the runtime module's
//! ([`stub_type_in_runtime`]):
//!
//! - a class the stub defines is the runtime module's class of the same name, found by the
//!   pairing, nested classes included, with its type arguments read the same way
//! - `Self` in a stub method is the runtime method's `Self`
//! - a type variable the stub function binds becomes the runtime function's own: the runtime
//!   function is generic over it, so its body sees `T` and its callers solve it
//! - a type variable a stub class binds is the runtime class's type parameter in the same
//!   position, where the runtime class is generic over as many
//!
//! a type naming anything else only the stub has (a class with no runtime counterpart, a type
//! variable of a class the runtime does not make generic, an enum member, a `NewType`) means
//! nothing in the runtime module, and is not carried over: the runtime body is held to no more than
//! the stub can say in terms the body shares.

use std::cell::{Cell, RefCell};

use ruff_python_ast::name::Name;
use rustc_hash::FxHashMap;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ProgramFile, semantic_index};

use crate::Db;
use crate::place::{Place, PlaceAndQualifiers};
use crate::stub_mapping::{
    runtime_definitions, runtime_module_stub, stub_counterpart_declarations, stub_scope,
};
use crate::types::class::ClassType;
use crate::types::definition::TypeDefinition;
use crate::types::function::OverloadLiteral;
use crate::types::generics::{enclosing_binding_contexts, typing_self};
use crate::types::infer::{nearest_enclosing_class, original_class_type};
use crate::types::signatures::{ReturnCallableTypeVarScope, Signature};
use crate::types::subclass_of::SubclassOfInner;
use crate::types::typevar::{BoundTypeVarInstance, TypeVarBoundOrConstraints};
use crate::types::visitor::any_over_type;
use crate::types::{
    ApplyTypeMappingVisitor, BindingContext, ClassLiteral, ProgramEnvironment, SubclassOfType,
    Type, TypeContext, TypeMapping, TypeQualifiers, UnionType, infer_definition_types,
    inferred_declaration,
};

/// how the classes and type variables of a stub read in the runtime module it describes: see
/// [`stub_type_in_runtime`]
#[derive(Debug, Clone, PartialEq, Eq, Default, get_size2::GetSize)]
pub struct StubTypeMapping<'db> {
    /// each class the stub defines, and the runtime module's class of the same name
    classes: FxHashMap<ClassLiteral<'db>, ClassLiteral<'db>>,
    /// each type variable the stub binds, and what it is in the runtime module
    typevars: FxHashMap<BoundTypeVarInstance<'db>, Type<'db>>,
}

impl<'db> StubTypeMapping<'db> {
    /// `ty` as the runtime module reads it, where `ty` itself is one of the stub's classes or type
    /// variables; `None` for any other type, whose parts `recurse` reads in turn
    pub(crate) fn map_type(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        ty: Type<'db>,
        recurse: impl Fn(Type<'db>) -> Type<'db>,
    ) -> Option<Type<'db>> {
        match ty {
            Type::TypeVar(typevar) => self.typevars.get(&typevar).copied(),
            Type::ClassLiteral(class) => self
                .classes
                .get(&class)
                .map(|&runtime| Type::ClassLiteral(runtime)),
            Type::GenericAlias(alias) => self
                .map_class(db, ClassType::Generic(alias), &recurse)
                .map(Type::from),
            Type::NominalInstance(instance) => self
                .map_class(db, instance.class(db, env), &recurse)
                .map(|class| Type::instance(db, env, class)),
            Type::ProtocolInstance(protocol) => self
                .map_class(db, *protocol.class_origin(db)?, &recurse)
                .map(|class| Type::instance(db, env, class)),
            Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
                SubclassOfInner::Class(class) => self
                    .map_class(db, class, &recurse)
                    .map(|class| SubclassOfType::from(db, env, class)),
                _ => None,
            },
            _ => None,
        }
    }

    /// the runtime module's class for `class`, one of the stub's, specialized with the type
    /// arguments `class` has read the same way
    ///
    /// a runtime class generic over a different number of type parameters than the stub's takes
    /// its default specialization: the stub's arguments say nothing about its parameters
    fn map_class(
        &self,
        db: &'db dyn Db,
        class: ClassType<'db>,
        recurse: &impl Fn(Type<'db>) -> Type<'db>,
    ) -> Option<ClassType<'db>> {
        let (literal, specialization) = class.class_literal_and_specialization(db);
        let runtime = *self.classes.get(&literal)?;
        let Some(runtime_static) = runtime.as_static() else {
            return Some(runtime.default_specialization(db));
        };
        let arguments: Option<Vec<Type<'db>>> = specialization.map(|specialization| {
            specialization
                .types(db)
                .iter()
                .map(|argument| recurse(*argument))
                .collect()
        });
        Some(
            runtime_static.apply_specialization(db, |context| match arguments {
                Some(arguments) if arguments.len() == context.len(db) => {
                    context.specialize(db, arguments)
                }
                _ => context.default_specialization(db, runtime_static.known(db)),
            }),
        )
    }
}

/// where in the runtime module a stub's type is read
#[derive(Clone, Copy)]
pub(crate) struct RuntimeReader<'db> {
    stub: ProgramFile<'db>,
    /// the runtime scope the type is read in: the scope a function is defined in, or the module or
    /// class body a variable is declared in
    scope: ScopeId<'db>,
    /// the runtime function the type is read for, whose own type variables the stub function's
    /// become, and whose `Self` the stub method's is
    function: Option<Definition<'db>>,
}

impl<'db> RuntimeReader<'db> {
    /// for the parameters and return type of the runtime function `definition`
    pub(crate) fn for_function(db: &'db dyn Db, definition: Definition<'db>) -> Option<Self> {
        Some(Self {
            stub: runtime_module_stub(db, definition.program_file(db))?,
            scope: definition.scope(db),
            function: Some(definition),
        })
    }

    /// for a variable declared in the runtime scope `scope`
    pub(crate) fn for_scope(db: &'db dyn Db, scope: ScopeId<'db>) -> Option<Self> {
        Some(Self {
            stub: runtime_module_stub(db, scope.program_file(db))?,
            scope,
            function: None,
        })
    }

    /// the type variables the runtime declaration can name: those of the scopes around it, and
    /// the function's own
    fn visible(self, db: &'db dyn Db) -> Vec<BindingContext<'db>> {
        let index = semantic_index(db, self.scope.program_file(db));
        let mut visible: Vec<BindingContext<'db>> =
            enclosing_binding_contexts(index, self.scope.file_scope_id(db)).collect();
        if let Some(function) = self.function {
            visible.push(function.into());
        }
        visible
    }

    /// the runtime method's `Self`
    fn runtime_self(self, db: &'db dyn Db) -> Option<BoundTypeVarInstance<'db>> {
        let function = self.function?;
        let scope = function.scope(db);
        let class = nearest_enclosing_class(db, semantic_index(db, scope.program_file(db)), scope)?;
        typing_self(db, scope, Some(function), class.into())
    }
}

/// the runtime module's class of the same name as `class`, one the stub defines
fn runtime_class<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> Option<ClassLiteral<'db>> {
    let definition = class.definition(db)?;
    runtime_definitions(db, definition, None)?
        .into_iter()
        .filter(|definition| matches!(definition.kind(db), DefinitionKind::Class(_)))
        .find_map(|definition| original_class_type(db, definition))
}

/// the class a type is an instance, a class object or a `type[]` of
fn class_of<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<ClassLiteral<'db>> {
    match ty {
        Type::ClassLiteral(class) => Some(class),
        Type::GenericAlias(alias) => Some(ClassLiteral::Static(alias.origin(db))),
        Type::NominalInstance(instance) => Some(instance.class(db, env).class_literal(db)),
        Type::ProtocolInstance(protocol) => Some(protocol.class_origin(db)?.class_literal(db)),
        Type::SubclassOf(subclass_of) => match subclass_of.subclass_of() {
            SubclassOfInner::Class(class) => Some(class.class_literal(db)),
            _ => None,
        },
        _ => None,
    }
}

/// `ty`, a type the stub writes, as the runtime module reads it where `reader` says, or `None`
/// where it names something only the stub has: see the module documentation
pub(crate) fn stub_type_in_runtime<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    reader: RuntimeReader<'db>,
) -> Option<Type<'db>> {
    let stub_file = reader.stub.file(db);
    let visible = reader.visible(db);
    let classes: RefCell<Vec<ClassLiteral<'db>>> = RefCell::new(Vec::new());
    let typevars: RefCell<Vec<BoundTypeVarInstance<'db>>> = RefCell::new(Vec::new());
    let unreadable = Cell::new(false);
    let scan = |ty: Type<'db>| {
        any_over_type(db, env, ty, false, |ty| {
            if let Type::TypeVar(typevar) = ty {
                if !visible.contains(&typevar.binding_context(db)) {
                    typevars.borrow_mut().push(typevar);
                }
                return false;
            }
            if let Some(class) = class_of(db, env, ty) {
                if class
                    .definition(db)
                    .is_some_and(|definition| definition.file(db) == stub_file)
                {
                    classes.borrow_mut().push(class);
                }
                return false;
            }
            // anything else the stub itself defines is a value of the stub's that the runtime module
            // has no counterpart of
            if let Some(
                TypeDefinition::EnumMember(definition)
                | TypeDefinition::NewType(definition)
                | TypeDefinition::DynamicClass(definition),
            ) = ty.definition(db, env)
                && definition.file(db) == stub_file
            {
                unreadable.set(true);
            }
            false
        });
    };
    scan(ty);
    // a type variable the runtime function takes over keeps its bound or constraints, which name
    // the stub's classes as its annotations do
    let foreign: Vec<BoundTypeVarInstance<'db>> = typevars.borrow().clone();
    for typevar in foreign {
        match typevar.typevar(db).bound_or_constraints(db, env) {
            Some(TypeVarBoundOrConstraints::UpperBound(bound)) => scan(bound),
            Some(TypeVarBoundOrConstraints::Constraints(constraints)) => {
                scan(constraints.as_type(db, env));
            }
            None => {}
        }
    }
    if unreadable.get() {
        return None;
    }
    let (classes, typevars) = (classes.into_inner(), typevars.into_inner());
    if classes.is_empty() && typevars.is_empty() {
        return Some(ty);
    }

    let mut mapping = StubTypeMapping::default();
    for class in classes {
        if mapping.classes.contains_key(&class) {
            continue;
        }
        mapping.classes.insert(class, runtime_class(db, class)?);
    }
    for typevar in typevars {
        if mapping.typevars.contains_key(&typevar) {
            continue;
        }
        let mapped = runtime_typevar(db, env, typevar, reader, &mapping)?;
        mapping.typevars.insert(typevar, mapped);
    }
    Some(ty.apply_type_mapping(
        db,
        env,
        &TypeMapping::StubToRuntime(&mapping),
        TypeContext::default(),
    ))
}

/// what `typevar`, a type variable the stub binds, is in the runtime module
///
/// `classes` reads the stub's classes, which the bound or constraints of a type variable the runtime
/// function takes over can name
fn runtime_typevar<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    typevar: BoundTypeVarInstance<'db>,
    reader: RuntimeReader<'db>,
    classes: &StubTypeMapping<'db>,
) -> Option<Type<'db>> {
    if typevar.typevar(db).is_self(db) {
        return reader.runtime_self(db).map(Type::TypeVar);
    }
    let BindingContext::Definition(binder) = typevar.binding_context(db) else {
        return None;
    };
    match binder.kind(db) {
        // the stub function's own: the runtime function becomes generic over the same variable
        DefinitionKind::Function(_) => {
            let function = reader.function?;
            if typevar.is_paramspec(db)
                || typevar.is_typevartuple(db)
                || typevar.paramspec_attr(db).is_some()
            {
                return None;
            }
            let rebound = BoundTypeVarInstance::new(
                db,
                typevar.typevar(db),
                BindingContext::Definition(function),
                None,
                typevar.freshness(db),
            );
            let mapping = TypeMapping::StubToRuntime(classes);
            let visitor = ApplyTypeMappingVisitor::new(env);
            Some(Type::TypeVar(rebound.map_bound_or_constraints(
                db,
                |bound| {
                    bound.map(|bound| bound.apply_type_mapping_impl(db, env, &mapping, &visitor))
                },
            )))
        }
        // a stub class's: the runtime class's type parameter in the same position
        DefinitionKind::Class(_) => {
            let stub_class = original_class_type(db, binder)?.as_static()?;
            let runtime = runtime_class(db, ClassLiteral::Static(stub_class))?.as_static()?;
            let stub_context = stub_class.generic_context(db)?;
            let runtime_context = runtime.generic_context(db)?;
            if stub_context.len(db) != runtime_context.len(db) {
                return None;
            }
            let position = stub_context
                .variables(db)
                .position(|variable| variable.is_same_occurrence(db, typevar))?;
            runtime_context
                .variables(db)
                .nth(position)
                .map(Type::TypeVar)
        }
        _ => None,
    }
}

/// the stub's declaration of a runtime function, read as the runtime function reads it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct StubSignature<'db> {
    /// every parameter with the type the stub gives it, read in the runtime module; a parameter
    /// whose annotation means nothing there reads as unannotated
    pub(crate) signature: Signature<'db>,
    /// what calling the stub's declaration returns, read in the runtime module; `None` where the
    /// stub writes no return type or one that means nothing there
    pub(crate) returns: Option<Type<'db>>,
    /// the return type the stub writes, read the same way, which for an `async def` is what its
    /// coroutine resolves to
    pub(crate) awaited: Option<Type<'db>>,
    /// whether the stub's first parameter is bound by the receiver
    pub(crate) binds_receiver: bool,
    /// whether the stub's declaration is an `async def`
    pub(crate) is_async: bool,
    pub(crate) overload: OverloadLiteral<'db>,
}

/// the signatures the stub declares for the runtime function `definition`, read in the runtime
/// module: one for a plain declaration, one per overload, of the declarations in the code that runs
///
/// `None` where the module has no stub, or its stub declares no function of that name there
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn stub_signatures<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> Option<Box<[StubSignature<'db>]>> {
    let declarations = stub_counterpart_declarations(db, definition).as_deref()?;
    let reader = RuntimeReader::for_function(db, definition)?;
    let env = ProgramEnvironment::from_file(definition.program_file(db));
    let read = |ty: Type<'db>| stub_type_in_runtime(db, &env, ty, reader);
    let signatures: Box<[StubSignature<'db>]> = declarations
        .iter()
        .filter_map(|&declaration| {
            let function = infer_definition_types(db, declaration).function_type(declaration)?;
            let overload = function.literal(db).last_definition;
            let raw = overload.raw_signature(db, ReturnCallableTypeVarScope::Public);
            let is_async = overload.header(db).is_async;
            let mut signature = raw.clone();
            signature.map_parameter_annotations(read);
            let (returns, awaited) = if overload.has_explicit_return_annotation(db) {
                let awaited = read(raw.return_ty);
                let returns = if is_async {
                    read(overload.signature(db).return_ty)
                } else {
                    awaited
                };
                (returns, awaited)
            } else {
                (None, None)
            };
            Some(StubSignature {
                signature,
                returns,
                awaited,
                binds_receiver: overload.binds_first_parameter(db),
                is_async,
                overload,
            })
        })
        .collect();
    (!signatures.is_empty()).then_some(signatures)
}

/// the return type the stub declares for the runtime function `definition`, as the function's
/// own body returns it
///
/// a function the stub overloads returns what any of its overloads return, as the implementation
/// of overloads written in one file does. `None` where any declaration writes no return type, or
/// one that means nothing in the runtime module, and where the runtime function is an `async def`
/// that the stub does not declare as one: what such a function resolves to is not what the stub's
/// return type says
pub(crate) fn stub_return_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
    is_async: bool,
) -> Option<Type<'db>> {
    let signatures = stub_signatures(db, definition).as_deref()?;
    let mut returns = Vec::with_capacity(signatures.len());
    for stub_signature in signatures {
        let returned = match (is_async, stub_signature.is_async) {
            (true, true) => stub_signature.awaited?,
            (false, _) => stub_signature.returns?,
            (true, false) => return None,
        };
        returns.push(returned);
    }
    Some(UnionType::from_elements(db, env, returns))
}

/// whether what the stub declares for the runtime method `definition` names `Self`, which the
/// runtime method's receiver then has to be for its callers to solve
pub(crate) fn stub_names_self<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
) -> bool {
    let Some(signatures) = stub_signatures(db, definition).as_deref() else {
        return false;
    };
    let names_self = |ty: Type<'db>| {
        any_over_type(
            db,
            env,
            ty,
            false,
            |ty| matches!(ty, Type::TypeVar(typevar) if typevar.typevar(db).is_self(db)),
        )
    };
    signatures.iter().any(|stub_signature| {
        stub_signature
            .signature
            .parameters()
            .iter()
            .skip(usize::from(stub_signature.binds_receiver))
            .any(|parameter| {
                !parameter.inferred_annotation && names_self(parameter.annotated_type())
            })
            || stub_signature.returns.is_some_and(names_self)
    })
}

/// the types the stub declares for the variables of the runtime scope `scope`, a module's or a
/// class body's, by name: each one the stub declares with an annotation in the code that runs,
/// read in the runtime module
///
/// what the runtime module leaves undeclared takes this as its declaration (see
/// [`stub_declared_type`]); a `Final` in the stub says importers may not reassign the name, which
/// the runtime module's own assignment of it does not do, so the qualifier is not carried
#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| None,
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn stub_declared_types<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> Option<FxHashMap<Name, PlaceAndQualifiers<'db>>> {
    let stub_scope = stub_scope(db, scope).as_ref()?;
    let reader = RuntimeReader::for_scope(db, scope)?;
    let env = ProgramEnvironment::from_scope(scope);

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
        if ty.is_unknown() {
            continue;
        }
        let Some(ty) = stub_type_in_runtime(db, &env, ty, reader) else {
            continue;
        };
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

/// `ty`, a type the stub of the module `function` is in writes for it, as the runtime function
/// reads it: see [`stub_type_in_runtime`]
pub(crate) fn stub_type_for_function<'db>(
    db: &'db dyn Db,
    function: Definition<'db>,
    ty: Type<'db>,
) -> Option<Type<'db>> {
    let reader = RuntimeReader::for_function(db, function)?;
    let env = ProgramEnvironment::from_file(function.program_file(db));
    stub_type_in_runtime(db, &env, ty, reader)
}
