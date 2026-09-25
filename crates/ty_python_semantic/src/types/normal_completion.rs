//! basedpython: whether a call to a function whose return type nobody wrote can return, read off
//! the function's control flow before its recovered return type
//!
//! a statement-level call ends the flow when the function it calls cannot return: when its return
//! type is `Never`. a function annotated `-> NoReturn` says so in its signature. a function whose
//! return type is recovered from its body (`infer-unannotated-signatures`, which `sound-types`
//! implies) returns `Never` when no `return` in the body and not the end of the body can be
//! reached — but recovering that type means inferring the whole body, and everything the body
//! reads. for a call made at module level, what the body reads includes the module's own globals,
//! whose visibility at the end of the module depends on whether that very call returns. every body
//! involved then joins one salsa cycle, which is iterated as a whole until it settles
//!
//! so the question is first answered here from the body's control flow: the reachability decision
//! diagram the semantic index already builds for the body
//! ([`UseDefMap::normal_exit_reachability`]), decided without inferring any expression:
//!
//! - `raise`, `return`, `break` and `continue` are already part of the diagram's shape
//! - a condition is known only when it is a literal the type of which would be known too
//!   (`while True`), and ambiguous otherwise
//! - a statement-level call in the body is decided only when its callee resolves through the
//!   semantic index alone to a single function, by that function's written return type, or by its
//!   body under this same reading. resolution follows a name to the one definition its scope has
//!   for it, an import to the module it names, and `self`, `cls` or a class's own name to a method
//!   of a class with no bases. every other call is ambiguous
//!
//! the only types read are written return annotations: a callee's, and those of the methods it
//! overrides, which [`OverloadLiteral::return_type_source`] consults before the body. no module
//! global is looked up by type, and no reachability is decided with types, so none of this depends
//! on the visibility of a module's globals at its end: the cycle has nothing to close through
//!
//! the reading has three answers. a body no `return` and no end of which can be reached cannot
//! complete, and a call to it does not return. a body that always reaches one completes, and a
//! call to it returns. a body whose answer depends on something only types decide leaves the
//! question open, and the checker reads the recovered return type instead, as it reads any other
//! callee's. only then is the body inferred, and a cycle through the module's globals is one the
//! answer really depends on
//!
//! wherever types would be needed to say more, the reading is ambiguous, and ternary logic is
//! monotone in it, so its two definite answers are ones the types agree with. a body it finds
//! cannot complete has no reachable `return` and no reachable end by types either, and the
//! recovered return type leaves out what cannot be reached, so it is `Never`. a body it finds
//! always completes reaches a `return` or its end by types too. the one place the two can still
//! differ is the value a reached `return` hands back, which would need the value's type:
//! `return sys.exit()` is a `return` that is reached, and the call is read as returning although
//! its recovered return type is `Never`
//!
//! recursion between bodies is resolved as a least fixed point: a body's answer starts as "cannot
//! complete" and can only grow towards "completes". that is the reading execution agrees with — a
//! call that returns is a finite one, so `def f(): f()` never returns, it ends in `RecursionError`
//! — and it never calls a body terminal that has a completing path, because such a path is found
//! without assuming anything about the body it is in
//!
//! a cycle can also pass through the checker's own inference, when a callee's written annotation
//! names a global whose reachability depends on the call. there too, every call the least fixed
//! point calls terminal is one whose body cannot complete, or one whose annotation declares
//! `NoReturn` and whose body the checker holds to that. what the fixed point leaves out is only
//! code that runs after such a call returns, so if one of them did return, the first to do so
//! would have to have returned through a path that is blocked by an earlier one

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use std::cell::Cell;

use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{
    ImportingFile, Module, ModuleName, resolve_module, resolve_module_for_import_from,
};
use ty_python_core::definition::{Definition, DefinitionKind, ParameterDefinitionNodeKind};
use ty_python_core::predicate::CallableAndCallExpr;
use ty_python_core::reachability_constraints::ScopedReachabilityConstraintId;
use ty_python_core::scope::{NodeWithScopeRef, ScopeId};
use ty_python_core::{
    ProgramFile, Truthiness, UseDefMap, global_scope, semantic_index, use_def_map,
};

use crate::place::implicit_builtins_symbol_scope;
use crate::reachability::{PredicateReading, evaluate_with_reading};
use crate::types::Type;
use crate::types::definition_resolution::{find_symbol_in_scope, visible_definitions_for_name};
use crate::types::function::{
    FunctionForm, FunctionHeader, FunctionType, OverloadLiteral, OverriddenReturnType,
    ReturnTypeSource, infers_unannotated_signatures,
};
use crate::types::signatures::function_signature_expression_type;
use crate::{Db, ProgramEnvironment, attribute_assignments, attribute_declarations};

/// whether a statement-level call to `function` can return, when the function's return type is
/// recovered from a body: `AlwaysFalse` when it cannot, `AlwaysTrue` when it does, and `Ambiguous`
/// when only the recovered return type can tell
///
/// `None` when the return type is not recovered from a body: it is written down, or supplied by an
/// overload group, a replaced signature or a `decorator def`. the signature answers those without
/// reading a body, and the caller reads it
pub(crate) fn body_recovered_call_returns<'db>(
    db: &'db dyn Db,
    function: FunctionType<'db>,
    is_await: bool,
) -> Option<Truthiness> {
    if function.has_replaced_signature(db) {
        return None;
    }
    let (overloads, implementation) = function.overloads_and_implementation(db);
    let implementation = implementation.filter(|_| overloads.is_empty())?;
    unannotated_call_returns(db, implementation, is_await)
}

/// [`body_recovered_call_returns`] for the one definition of a function
///
/// a cycle through it starts from the answer that the call returns, which is what the checker
/// answers for any call it cannot see through
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _, _| Some(Truthiness::AlwaysTrue),
    heap_size = ruff_memory_usage::heap_size,
)]
fn unannotated_call_returns<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
    is_await: bool,
) -> Option<Truthiness> {
    let header = overload.header(db);
    if header.form == FunctionForm::DecoratorKeyword {
        return None;
    }
    // an `async def` called without `await` hands back a coroutine, whatever its body does
    if header.is_async && !is_await {
        return Some(Truthiness::AlwaysTrue);
    }
    let env = ProgramEnvironment::from_scope(overload.body_scope(db));
    let base_is_undecided = Cell::new(false);
    let source = overload.return_type_source(db, &env, false, |base| {
        overridden_return_type(db, base).unwrap_or_else(|| {
            base_is_undecided.set(true);
            OverriddenReturnType::NotOverridden
        })
    });
    // what the base supplies is not known without its recovered return type, so neither is where
    // this function's return type comes from
    if base_is_undecided.get() {
        return Some(Truthiness::Ambiguous);
    }
    match source {
        ReturnTypeSource::Body => Some(body_call_returns(db, overload.body_scope(db))),
        ReturnTypeSource::Overridden(return_ty) => Some(Truthiness::from(
            !return_ty.is_equivalent_to(db, &env, Type::Never),
        )),
        ReturnTypeSource::Written
        | ReturnTypeSource::PropertyInitialiser(_)
        | ReturnTypeSource::Overloads(_)
        | ReturnTypeSource::Gradual => None,
    }
}

/// what the method `base` supplies as the return type of a method that overrides it, told without
/// recovering any return type; `None` when that cannot be told
fn overridden_return_type<'db>(
    db: &'db dyn Db,
    base: FunctionType<'db>,
) -> Option<OverriddenReturnType<'db>> {
    let (overloads, implementation) = base.overloads_and_implementation(db);
    // which type an overloaded base returns depends on the arguments, so the override's return
    // type stays gradual
    match implementation.filter(|_| overloads.is_empty()) {
        Some(base) => return_type_seen_by_overrides(db, base),
        None => Some(OverriddenReturnType::Inexpressible),
    }
}

/// [`overridden_return_type`] for the one definition of a base method
///
/// this follows [`OverloadLiteral::overridden_return_type`], which reads the base's signature and
/// so recovers the base's own return type from its body when it writes none. here that body is
/// read the same way as any other body. one that cannot complete returns `Never`. one that
/// reaches a `return` or its end returns a type this reading does not recover, which stands as
/// the gradual type: it is only used to tell whether a call returns, and a call returning the
/// gradual type is taken to. one that only types can read leaves the answer open
///
/// a cycle through it starts from the open answer, which sends the caller to the types
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| None,
    heap_size = ruff_memory_usage::heap_size,
)]
fn return_type_seen_by_overrides<'db>(
    db: &'db dyn Db,
    base: OverloadLiteral<'db>,
) -> Option<OverriddenReturnType<'db>> {
    let env = ProgramEnvironment::from_scope(base.body_scope(db));
    let base_is_undecided = Cell::new(false);
    let source = base.return_type_source(db, &env, false, |base| {
        overridden_return_type(db, base).unwrap_or_else(|| {
            base_is_undecided.set(true);
            OverriddenReturnType::NotOverridden
        })
    });
    if base_is_undecided.get() {
        return None;
    }
    Some(match source {
        ReturnTypeSource::Written => match written_return_type(db, base.definition(db)) {
            Some(return_ty) => OverriddenReturnType::of(db, &env, return_ty),
            None => OverriddenReturnType::NotOverridden,
        },
        ReturnTypeSource::PropertyInitialiser(return_ty)
        | ReturnTypeSource::Overridden(return_ty)
        | ReturnTypeSource::Overloads(return_ty) => OverriddenReturnType::of(db, &env, return_ty),
        ReturnTypeSource::Body => match body_call_returns(db, base.body_scope(db)) {
            Truthiness::AlwaysTrue => OverriddenReturnType::Declared(Type::unknown()),
            Truthiness::AlwaysFalse => OverriddenReturnType::Declared(Type::Never),
            Truthiness::Ambiguous => return None,
        },
        ReturnTypeSource::Gradual => OverriddenReturnType::NotOverridden,
    })
}

/// the type the return annotation of the function `definition` declares, `None` when it has none
///
/// an `asserts` clause names a place rather than a type, and such a function returns `None`
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| Some(Type::unknown()),
    heap_size = ruff_memory_usage::heap_size,
)]
fn written_return_type<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<Type<'db>> {
    let module = parsed_module(db, definition.python_file(db)).load(db);
    let function = definition.kind(db).as_function()?.node(&module);
    let returns = function.returns.as_deref()?;
    Some(if function.is_asserts_return {
        Type::none(db, &ProgramEnvironment::from_scope(definition.scope(db)))
    } else {
        function_signature_expression_type(db, definition, returns)
    })
}

/// whether a call to the function whose body is `body_scope` returns, given that the call is not
/// an `async def` left unawaited: a generator function's call builds the generator without running
/// the body, and any other call returns when the body completes
fn body_call_returns<'db>(db: &'db dyn Db, body_scope: ScopeId<'db>) -> Truthiness {
    let index = semantic_index(db, body_scope.program_file(db));
    if body_scope.file_scope_id(db).is_generator_function(index) {
        return Truthiness::AlwaysTrue;
    }
    function_body_completion(db, body_scope)
}

/// whether control leaves the function body `body_scope` normally — reaches a `return` or the end
/// of the body — as read off its control flow, without inferring any expression in it
///
/// `AlwaysFalse` when it cannot and `AlwaysTrue` when it always can. `Ambiguous` when that depends
/// on something only types can decide, such as a condition that is not a literal
///
/// recursion is solved as a least fixed point: the provisional answer is `AlwaysFalse`, and an
/// iteration can only move it towards `AlwaysTrue`. see the module documentation for why that is
/// sound
#[salsa::tracked(
    returns(copy),
    cycle_initial = |_, _, _| Truthiness::AlwaysFalse,
    cycle_fn = |_, _, previous: &Truthiness, result: Truthiness, _| previous.or(result),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn function_body_completion<'db>(
    db: &'db dyn Db,
    body_scope: ScopeId<'db>,
) -> Truthiness {
    let use_def = use_def_map(db, body_scope);
    let mut reading = ControlFlowReading {
        db,
        env: ProgramEnvironment::from_scope(body_scope),
        scope: body_scope,
        use_def,
        continuations: FxHashMap::default(),
    };
    evaluate_with_reading(
        db,
        use_def.reachability_constraints(),
        use_def.predicates(),
        use_def.normal_exit_reachability(),
        &mut reading,
    )
}

/// decides a scope's reachability predicates from control flow alone
struct ControlFlowReading<'a, 'db> {
    db: &'db dyn Db,
    env: ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    use_def: &'a UseDefMap<'db>,
    /// a sequence of `finally` suites nests each one's continuation in the next, so each is
    /// evaluated once
    continuations: FxHashMap<ScopedReachabilityConstraintId, Truthiness>,
}

impl<'db> PredicateReading<'db> for ControlFlowReading<'_, 'db> {
    fn types(&self) -> Option<&ProgramEnvironment<'db>> {
        None
    }

    fn statement_call(&mut self, call: CallableAndCallExpr<'db>) -> Truthiness {
        let db = self.db;
        let module = parsed_module(db, self.scope.python_file(db)).load(db);
        let callee = call.callable.node_ref(db).node(&module);
        let mut resolver = CalleeResolver {
            db,
            env: &self.env,
            visited: FxHashSet::default(),
        };
        match resolver.resolve(call.callable.scope(db), callee) {
            Some(Resolved::Function(definition)) => {
                call_truthiness(db, &self.env, definition, false, call.is_await)
            }
            Some(Resolved::Method(definition)) => {
                call_truthiness(db, &self.env, definition, true, call.is_await)
            }
            Some(Resolved::Module(_) | Resolved::Class(_) | Resolved::Receiver(_)) | None => {
                Truthiness::Ambiguous
            }
        }
    }

    fn continuation(
        &mut self,
        scope: ScopeId<'db>,
        continuation: ScopedReachabilityConstraintId,
    ) -> Truthiness {
        if scope != self.scope {
            return Truthiness::Ambiguous;
        }
        if let Some(reachability) = self.continuations.get(&continuation) {
            return *reachability;
        }
        let use_def = self.use_def;
        let reachability = evaluate_with_reading(
            self.db,
            use_def.reachability_constraints(),
            use_def.predicates(),
            continuation,
            self,
        );
        self.continuations.insert(continuation, reachability);
        reachability
    }
}

/// what the `def` line of a function says, and which body it runs
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct CalleeFacts<'db> {
    header: FunctionHeader,
    body_scope: ScopeId<'db>,
    /// its return annotation, if it writes one
    writes_return_type: bool,
    name: Name,
    /// it has decorators, and each is the builtin `staticmethod` or `classmethod`
    only_method_kind_decorators: bool,
    has_decorators: bool,
}

/// [`CalleeFacts`] of the function `definition`, `None` when it is not a function
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn callee_facts<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<CalleeFacts<'db>> {
    let file = definition.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let node = definition.kind(db).as_function()?.node(&module);
    let env = ProgramEnvironment::from_scope(definition.scope(db));
    let only_method_kind_decorators = node.decorator_list.iter().all(|decorator| {
        ["staticmethod", "classmethod"].into_iter().any(|name| {
            is_builtin_name(db, &env, definition.scope(db), &decorator.expression, name)
        })
    });
    Some(CalleeFacts {
        header: FunctionHeader::of(node),
        body_scope: semantic_index(db, file)
            .node_scope(NodeWithScopeRef::Function(node))
            .to_scope_id(db, file),
        writes_return_type: node.returns.is_some(),
        name: node.name.id.clone(),
        only_method_kind_decorators,
        has_decorators: !node.decorator_list.is_empty(),
    })
}

/// whether a call to the function `definition` returns: `AlwaysFalse` when it cannot, `AlwaysTrue`
/// when it does, and `Ambiguous` when only types can tell
///
/// `in_class` is whether the function was found in a class body, where `staticmethod` and
/// `classmethod` leave it the same function to call; any other decorator could replace it
fn call_truthiness<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    definition: Definition<'db>,
    in_class: bool,
    is_await: bool,
) -> Truthiness {
    let Some(facts) = callee_facts(db, definition).as_ref() else {
        return Truthiness::Ambiguous;
    };
    let decorators_keep_the_function =
        !facts.has_decorators || (in_class && facts.only_method_kind_decorators);
    if !decorators_keep_the_function {
        return Truthiness::Ambiguous;
    }
    // an `async def` called without `await` hands back a coroutine, whatever its body does
    if facts.header.is_async && !is_await {
        return Truthiness::AlwaysTrue;
    }

    if facts.writes_return_type {
        // what awaiting something other than a coroutine hands back is a question for its type
        if is_await && !facts.header.is_async {
            return Truthiness::Ambiguous;
        }
        let Some(return_ty) = written_return_type(db, definition) else {
            return Truthiness::Ambiguous;
        };
        // a type variable in the return type is solved by the call's arguments, which can solve
        // it to `Never`
        return if return_ty.is_equivalent_to(db, env, Type::Never) {
            Truthiness::AlwaysFalse
        } else if return_ty.has_typevar(db, env) {
            Truthiness::Ambiguous
        } else {
            Truthiness::AlwaysTrue
        };
    }

    // without the setting, what an unannotated function returns is the gradual type
    if !infers_unannotated_signatures(db, definition.file(db)) {
        return Truthiness::AlwaysTrue;
    }
    // `__new__`'s return type is never read off its body, and what a method of a class with no
    // bases returns when it overrides something `object` defines comes from `object`
    if facts.name == "__new__"
        || facts.header.form == FunctionForm::DecoratorKeyword
        || (in_class && defined_on_object(db, env, &facts.name))
    {
        return Truthiness::Ambiguous;
    }
    body_call_returns(db, facts.body_scope)
}

/// what a callee expression resolves to through the semantic index
#[derive(Debug, Clone, Copy)]
enum Resolved<'db> {
    Module(ProgramFile<'db>),
    Class(Definition<'db>),
    Function(Definition<'db>),
    /// a function found in the body of a class
    Method(Definition<'db>),
    /// the first parameter of a method of the class `Definition`: its instance or, in a
    /// `classmethod`, the class itself
    Receiver(Definition<'db>),
}

/// resolves callee expressions through the semantic index alone: no expression's type is inferred
///
/// a name, or a member of a module or class, resolves only when every definition the scope has for
/// it is one definition. the type of a name the scope binds more than once may be a union, which a
/// call is never terminal through
struct CalleeResolver<'a, 'db> {
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    /// imports already followed, which ends a cycle of re-exports
    visited: FxHashSet<Definition<'db>>,
}

impl<'db> CalleeResolver<'_, 'db> {
    fn resolve(&mut self, scope: ScopeId<'db>, expr: &ast::Expr) -> Option<Resolved<'db>> {
        match expr {
            ast::Expr::Name(name) => self.resolve_name(scope, name.id.as_str()),
            ast::Expr::Attribute(attribute) => match self.resolve(scope, &attribute.value)? {
                Resolved::Module(file) => self.module_member(file, attribute.attr.as_str()),
                Resolved::Class(class) | Resolved::Receiver(class) => {
                    self.class_member(class, attribute.attr.as_str())
                }
                Resolved::Function(_) | Resolved::Method(_) => None,
            },
            _ => None,
        }
    }

    fn resolve_name(&mut self, scope: ScopeId<'db>, name: &str) -> Option<Resolved<'db>> {
        let db = self.db;
        let mut definitions = visible_definitions_for_name(db, scope, name);
        if definitions.is_empty() {
            let builtins = implicit_builtins_symbol_scope(db, self.env, name)?;
            definitions = find_symbol_in_scope(db, builtins, name)
                .into_iter()
                .filter(|definition| definition.is_reexported(db))
                .collect();
        }
        let definition = single(definitions)?;
        self.resolve_definition(definition, name)
    }

    fn resolve_definition(
        &mut self,
        definition: Definition<'db>,
        name: &str,
    ) -> Option<Resolved<'db>> {
        let db = self.db;
        if !self.visited.insert(definition) {
            return None;
        }
        match definition.kind(db) {
            DefinitionKind::Function(_) => Some(Resolved::Function(definition)),
            DefinitionKind::Class(_) => Some(Resolved::Class(definition)),
            DefinitionKind::Parameter(parameter) => self.receiver(definition, parameter),
            DefinitionKind::Import(_)
            | DefinitionKind::ImportFrom(_)
            | DefinitionKind::StarImport(_) => match import_target(db, definition).as_ref()? {
                ImportTarget::Module(file) => Some(self.module(*file)),
                ImportTarget::Member {
                    module,
                    name,
                    importing_file,
                } => self.imported_member(*module, *importing_file, name),
                // a star import binds every name it finds, so the one looked up is its own
                ImportTarget::Members {
                    module,
                    importing_file,
                } => self.imported_member(*module, *importing_file, name),
            },
            _ => None,
        }
    }

    fn module(&self, file: File) -> Resolved<'db> {
        Resolved::Module(ProgramFile::new(self.db, file, self.env.program(self.db)))
    }

    /// `from module import name`: the module's own `name`, or its submodule of that name
    fn imported_member(
        &mut self,
        module: Module<'db>,
        importing_file: File,
        name: &str,
    ) -> Option<Resolved<'db>> {
        let db = self.db;
        if let Some(file) = module.file(db) {
            let file = ProgramFile::new(db, file, self.env.program(db));
            if !self.module_members(file, name).is_empty() {
                return self.module_member(file, name);
            }
        }
        let mut submodule = module.name(db).clone();
        submodule.extend(&ModuleName::new(name)?);
        let importing = ImportingFile::File(importing_file, self.env.resolver_environment(db));
        Some(self.module(resolve_module(db, importing, &submodule)?.file(db)?))
    }

    /// the definitions of `name` in `file`'s global scope that an import can see: in a stub, only
    /// the ones it re-exports
    fn module_members(&self, file: ProgramFile<'db>, name: &str) -> Vec<Definition<'db>> {
        let db = self.db;
        let is_stub = file.file(db).is_stub(db);
        find_symbol_in_scope(db, global_scope(db, file), name)
            .into_iter()
            .filter(|definition| !is_stub || definition.is_reexported(db))
            .collect()
    }

    fn module_member(&mut self, file: ProgramFile<'db>, name: &str) -> Option<Resolved<'db>> {
        let definition = single(self.module_members(file, name))?;
        self.resolve_definition(definition, name)
    }

    /// a function defined in the body of `class`, when nothing else can supply the member: the
    /// class has no bases, no decorators and no metaclass, and no method assigns the name on an
    /// instance
    fn class_member(&mut self, class: Definition<'db>, name: &str) -> Option<Resolved<'db>> {
        let db = self.db;
        let body = class_body(db, class)?;
        if !body.is_closed {
            return None;
        }
        let assigned_on_instance =
            attribute_assignments(db, body.scope, name).any(|(mut bindings, _)| {
                bindings.any(|binding| binding.binding.definition().is_some())
            }) || attribute_declarations(db, body.scope, name).any(|(mut declarations, _)| {
                declarations.any(|declaration| declaration.declaration.definition().is_some())
            });
        if assigned_on_instance {
            return None;
        }
        let definition = single(find_symbol_in_scope(db, body.scope, name))?;
        definition
            .kind(db)
            .as_function()
            .map(|_| Resolved::Method(definition))
    }

    /// the first parameter of a method: `self`, or `cls` in a `classmethod`
    ///
    /// a name resolves within its own file, so the method is in the file being read
    fn receiver(
        &self,
        definition: Definition<'db>,
        parameter: &ParameterDefinitionNodeKind,
    ) -> Option<Resolved<'db>> {
        let db = self.db;
        let ParameterDefinitionNodeKind::Parameter(parameter) = parameter else {
            return None;
        };
        let file = definition.program_file(db);
        let module = parsed_module(db, file.python_file(db)).load(db);
        let index = semantic_index(db, file);
        let function_scope = definition.scope(db).file_scope_id(db);
        let function = index
            .scope(function_scope)
            .node()
            .as_function()?
            .node(&module);
        let first = function
            .parameters
            .posonlyargs
            .iter()
            .chain(&function.parameters.args)
            .next()?;
        if first.parameter.name.as_str() != parameter.node(&module).parameter.name.as_str() {
            return None;
        }
        let class_scope = index.parent_scope_id(function_scope)?;
        let class = index.scope(class_scope).node().as_class()?;
        // a `staticmethod` has no receiver, and any other decorator could have changed what the
        // first parameter is bound to
        let class_body = class_scope.to_scope_id(db, file);
        let receives = match function.decorator_list.as_slice() {
            [] => true,
            [decorator] => is_builtin_name(
                db,
                self.env,
                class_body,
                &decorator.expression,
                "classmethod",
            ),
            _ => false,
        };
        if !receives {
            return None;
        }
        index.try_definition(class).map(Resolved::Receiver)
    }
}

/// what an import definition binds, read once so that following an import into another module
/// does not tie the reader to that module's syntax tree
#[derive(Debug, Clone, PartialEq, Eq, Hash, salsa::SalsaValue)]
enum ImportTarget<'db> {
    /// `import a.b` binds `a`, and `import a.b as c` binds `a.b`
    Module(File),
    /// `from module import name`, in `importing_file`
    Member {
        module: Module<'db>,
        name: Name,
        importing_file: File,
    },
    /// `from module import *`, in `importing_file`
    Members {
        module: Module<'db>,
        importing_file: File,
    },
}

#[salsa::tracked(returns(ref))]
fn import_target<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<ImportTarget<'db>> {
    let file = definition.program_file(db);
    let env = ProgramEnvironment::from_scope(definition.scope(db));
    let importing_file = file.file(db);
    let importing = ImportingFile::File(importing_file, env.resolver_environment(db));
    let module = parsed_module(db, file.python_file(db)).load(db);
    match definition.kind(db) {
        DefinitionKind::Import(import) => {
            let alias = import.alias(&module);
            let module_name = if alias.asname.is_some() {
                ModuleName::new(&alias.name)?
            } else {
                ModuleName::new(alias.name.split('.').next()?)?
            };
            Some(ImportTarget::Module(
                resolve_module(db, importing, &module_name)?.file(db)?,
            ))
        }
        DefinitionKind::ImportFrom(import) => Some(ImportTarget::Member {
            module: resolve_module_for_import_from(db, importing, import.import(&module))?,
            name: import.alias(&module).name.id.clone(),
            importing_file,
        }),
        DefinitionKind::StarImport(import) => Some(ImportTarget::Members {
            module: resolve_module_for_import_from(db, importing, import.import(&module))?,
            importing_file,
        }),
        _ => None,
    }
}

/// the body of a class, and whether nothing but it can supply a member: the class has no
/// decorators and no bases but `object`, and passes no keyword such as `metaclass`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct ClassBody<'db> {
    scope: ScopeId<'db>,
    is_closed: bool,
}

#[salsa::tracked(returns(copy), heap_size = ruff_memory_usage::heap_size)]
fn class_body<'db>(db: &'db dyn Db, class: Definition<'db>) -> Option<ClassBody<'db>> {
    let file = class.program_file(db);
    let module = parsed_module(db, file.python_file(db)).load(db);
    let node = class.kind(db).as_class()?.node(&module);
    let env = ProgramEnvironment::from_scope(class.scope(db));
    let is_closed = node.decorator_list.is_empty()
        && node.arguments.as_deref().is_none_or(|arguments| {
            arguments.keywords.is_empty()
                && arguments
                    .args
                    .iter()
                    .all(|base| is_builtin_name(db, &env, class.scope(db), base, "object"))
        });
    Some(ClassBody {
        scope: semantic_index(db, file)
            .node_scope(NodeWithScopeRef::Class(node))
            .to_scope_id(db, file),
        is_closed,
    })
}

/// the one element of `definitions`, if it has exactly one
fn single<'db>(definitions: impl IntoIterator<Item = Definition<'db>>) -> Option<Definition<'db>> {
    let mut definitions = definitions.into_iter();
    let definition = definitions.next()?;
    definitions.next().is_none().then_some(definition)
}

/// whether `expr`, written in `scope`, names the builtin class `name`: a name no enclosing scope
/// defines, which the builtins module defines as a class
fn is_builtin_name<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    expr: &ast::Expr,
    name: &str,
) -> bool {
    let ast::Expr::Name(expr) = expr else {
        return false;
    };
    expr.id.as_str() == name
        && visible_definitions_for_name(db, scope, name).is_empty()
        && builtin_class(db, env, name).is_some()
}

/// the builtins module's definition of the class `name`
fn builtin_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    name: &str,
) -> Option<Definition<'db>> {
    let builtins = implicit_builtins_symbol_scope(db, env, name)?;
    let definition = single(find_symbol_in_scope(db, builtins, name))?;
    definition
        .kind(db)
        .as_class()
        .is_some()
        .then_some(definition)
}

/// whether `object` defines a member `name`, which every class inherits
fn defined_on_object<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, name: &str) -> bool {
    let Some(object) = builtin_class(db, env, "object").and_then(|object| class_body(db, object))
    else {
        return true;
    };
    !find_symbol_in_scope(db, object.scope, name).is_empty()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use ruff_db::files::system_path_to_file;
    use ruff_db::system::DbWithWritableSystem as _;
    use ruff_db::testing::assert_function_query_was_not_run_by_name;
    use ty_python_core::ProgramFile;

    use crate::db::tests::setup_db;
    use crate::place::{DefinedPlace, Place, global_symbol};

    /// A package that, at module level and behind a condition, calls the first of `length`
    /// unannotated functions in a submodule, each of which calls the next both as a statement and
    /// in its `return`. The last one ends with `last`. Returns whether `AFTER`, bound after the
    /// call, is reachable.
    ///
    /// Recovering the chain's return types infers every body in it, and the last one reads the
    /// package back, so all of them joined one cycle with the package's globals: at 500 links
    /// checking the package took 45 CPU seconds in a release build, and far longer in a debug one.
    fn chain(length: usize, last: &str) -> anyhow::Result<(crate::db::tests::TestDb, bool)> {
        let mut db = setup_db();
        db.write_dedented(
            "/src/pkg/__init__.py",
            r#"
            import os

            VALUE = os.environ.get("VALUE")

            if "FLAG" in os.environ:
                import pkg.sub as sub

                sub.f0()
                AFTER = 1
            "#,
        )?;
        let mut sub = String::from("import sys\n\nimport pkg\n\n");
        for link in 0..length {
            if link + 1 < length {
                let next = link + 1;
                writeln!(
                    sub,
                    "def f{link}():\n    f{next}()\n    return f{next}()\n\n"
                )?;
            } else {
                writeln!(sub, "def f{link}():\n    {last}\n")?;
            }
        }
        db.write_file("/src/pkg/sub.py", sub)?;

        let file = system_path_to_file(&db, "/src/pkg/__init__.py")?;
        let file = ProgramFile::new(&db, file, db.program_environment().program(&db));
        // a binding the flow never reaches has the type `Never`
        let Place::Defined(DefinedPlace { ty: after, .. }) =
            global_symbol(&db, file, "AFTER").place
        else {
            anyhow::bail!("`AFTER` is always bound in the source");
        };
        let after = !after.is_never();
        Ok((db, after))
    }

    /// runs `test` on a thread with the stack ty's checking threads have
    ///
    /// each link of a chain is read by a query the reading of the link before it waits on, so how
    /// deep the reading goes grows with the chain, and a chain of 500 links needs more than the
    /// stack a test thread starts with
    fn on_a_checking_stack(
        test: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<()> {
        std::thread::Builder::new()
            .stack_size(ruff_db::STACK_SIZE)
            .spawn(test)?
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    #[test]
    fn a_long_chain_is_read_without_recovering_its_return_types() -> anyhow::Result<()> {
        on_a_checking_stack(|| {
            let (mut db, after) = chain(500, "return pkg.VALUE")?;
            assert!(after, "the chain returns, so `AFTER` is reachable");
            let events = db.take_salsa_events();
            assert_function_query_was_not_run_by_name(&db, "inferred_return_type", None, &events);
            assert_reads_each_link_once(&db, &events, 500);
            Ok(())
        })
    }

    #[test]
    fn a_long_chain_that_exits_never_returns() -> anyhow::Result<()> {
        on_a_checking_stack(|| {
            let (mut db, after) = chain(500, "sys.exit(1)")?;
            assert!(!after, "the chain never returns, so `AFTER` is unreachable");
            let events = db.take_salsa_events();
            assert_function_query_was_not_run_by_name(&db, "inferred_return_type", None, &events);
            assert_reads_each_link_once(&db, &events, 500);
            Ok(())
        })
    }

    /// Each link's body is read once, and neither the reading nor the module-level call it
    /// answers is part of a cycle that has to be iterated: the cost is linear in the chain.
    fn assert_reads_each_link_once(
        db: &crate::db::tests::TestDb,
        events: &[salsa::Event],
        length: usize,
    ) {
        salsa::Database::attach(db, |_| {
            let executions = |query: &str| {
                events
                    .iter()
                    .filter(|event| match event.kind {
                        salsa::EventKind::WillExecute { database_key } => {
                            format!("{database_key:?}").starts_with(query)
                        }
                        _ => false,
                    })
                    .count()
            };
            assert_eq!(executions("function_body_completion("), length);
            let iterated = events
                .iter()
                .filter_map(|event| match event.kind {
                    salsa::EventKind::WillIterateCycle { database_key, .. } => {
                        Some(format!("{database_key:?}"))
                    }
                    _ => None,
                })
                .filter(|head| {
                    head.starts_with("function_body_completion(")
                        || head.starts_with("analyze_non_terminal_call(")
                })
                .collect::<Vec<_>>();
            assert_eq!(iterated, Vec::<String>::new());
        });
    }
}
