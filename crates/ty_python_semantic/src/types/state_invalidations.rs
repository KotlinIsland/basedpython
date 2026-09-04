//! basedpython-ui: which composition scopes a state write invalidates.
//!
//! [`state_reads`](super::state_reads) recovers what a composition reads;
//! this module runs that the other way, from a *write* site: the composables,
//! root blocks and `derived` computations whose composition depends on the
//! place being written, so an editor can show `count.value += step` followed
//! by `invalidates Counter` at the end of the statement.
//!
//! The runtime's answer is exact and dynamic — a write notifies the trackers
//! that read the cell during their last run, and its trace records which. The
//! static answer mirrors the runtime's subscription rules over the static read
//! sets:
//!
//! - a composable's *own* scope is subscribed to what its body, the content
//!   blocks written in it and the plain functions it calls while composing
//!   read ([`scope_state_reads`]). A composable callee reads for a scope of
//!   its own, so a place handed to one is followed one hop at a time, through
//!   the arguments the call writes, rather than lifted into the caller: a
//!   child that reads its parameter is named instead of the parent that only
//!   forwards it
//! - a composable called *with a content block* (`Card(count):`) is the
//!   exception: the runtime runs it inline, re-running it with its parent, and
//!   what it reads — its own cells included — subscribes the parent's scope,
//!   through as many inline parents as there are. Its reads are lifted into
//!   the caller's scope like a plain callee's, and the child is still named
//! - a `derived` is a tracker of its own: a write to what its lambda reads
//!   invalidates the derived, and then whatever reads the derived
//! - a `remember` computation reads on behalf of the scope that made it
//! - a nested composable that captures a slot of an enclosing one reads it
//!   as a dependency of its own
//! - the `root` of `run_app`, `compose_test` and `Runtime.set_root` is a
//!   scope like a composable's
//!
//! A written name stands for a *slot* when it was bound, while composing, to
//! what a call returned — `let count = state(0)`, in the body or in a content
//! block written in it. A name bound to another place (`let alias = count`,
//! `let cell = model.count`) is followed to that place, binding by binding, in
//! the body and in a handler alike. A slot's readers are looked for
//! throughout its file: the scopes that can see the binding, and the parents
//! an inline child's reads were lifted into. A parameter is followed, through
//! the callers that fill it, to wherever the argument came from; a
//! module-level slot may be read anywhere in its file. What a walk in one
//! file cannot see, the set says so with `…`: a caller, an inline parent or a
//! reader of a module-level slot in another file; a callee reached through a
//! `dynamic` value; an unpacked argument; and a written name that is not a
//! slot at all — a loop or comprehension target, a value bound after
//! composing or outside every composition, a subscript — whose readers could
//! be anywhere. `nothing` is said only of a slot no composition reads.
//!
//! Everything here is asked by the editor alone, after inference: it reads
//! other scopes' inferred types freely, which the checks that run *during*
//! inference cannot (see `type_in_scope` in `composition.rs`), and nothing in
//! `check_scope` reaches it.

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{KnownModule, resolve_module_confident};
use ty_python_core::definition::{Definition, DefinitionKind, DefinitionNodeKey};
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind, ScopeId};
use ty_python_core::{ProgramFile, SemanticIndex, semantic_index};

use crate::types::composition::{
    BlockKind, CompositionOwner, RootEntry, block_kind, composition_of_scope, computation_kind,
};
use crate::types::dedicated::basedpython_ui::{ObservableKind, is_composable, observable_kind};
use crate::types::function::{FunctionDecorators, FunctionLiteral, KnownFunction, OverloadLiteral};
use crate::types::state_reads::{
    ParameterInfo, PlaceRoot, PlaceRootKind, ReadsCollector, StatePlace, StateReadEffects,
    StateReads, body_state_read_effects, function_parameters, lambda_state_reads,
    parameter_definitions,
};
use crate::types::trailing_lambda::callee_accepts_block;
use crate::types::{ProgramEnvironment, Type, infer_definition_types};
use crate::{Db, FxIndexSet};

// ---------------------------------------------------------------------------
// what a write invalidates
// ---------------------------------------------------------------------------

/// One scope a write invalidates.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum Invalidated<'db> {
    /// a composable, whose scope re-runs
    Composable(FunctionLiteral<'db>),
    /// the root of an entry point — the runtime's `root` scope: the block of
    /// `run_app` / `compose_test`, or the function or lambda handed to
    /// `Runtime.set_root`
    Root(ScopeId<'db>),
    /// a `derived` computation, which recomputes and then invalidates its own
    /// readers when its value changed
    Derived(Definition<'db>),
}

/// One scope a write invalidates, with how the editor names and reaches it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct InvalidatedScope<'db> {
    pub(crate) what: Invalidated<'db>,
    /// the composable's name, `root`, or the name the derived is bound to —
    /// what the runtime's trace calls the scope
    pub(crate) name: Name,
    /// the range of the name — for a root, of the entry point called or of
    /// the argument handed to `set_root` — in the file `what` is in
    pub(crate) declaration: TextRange,
}

impl<'db> InvalidatedScope<'db> {
    /// the file `declaration` is in
    pub(crate) fn file(&self, db: &'db dyn Db) -> File {
        match self.what {
            Invalidated::Composable(function) => function.last_definition.file(db),
            Invalidated::Root(scope) => scope.file(db),
            Invalidated::Derived(definition) => definition.file(db),
        }
    }
}

/// The scopes a write to one place invalidates.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct StateInvalidations<'db> {
    /// in declaration order, the written slot's own file first
    scopes: Box<[InvalidatedScope<'db>]>,
    /// whether a reader may have been missed: a caller, an inline parent or a
    /// reader of a module-level slot in another file; a callee that cannot
    /// be followed; an argument that cannot be seen into; a written name that
    /// is not a slot
    opaque: bool,
}

/// A place a body writes, interned so that its readers are computed once
/// however many sites write it — the three buttons of a counter share one
/// answer.
#[salsa::interned(debug, heap_size = ruff_memory_usage::heap_size)]
pub(crate) struct WrittenPlace<'db> {
    /// the place as the writing body sees it: its root's kind says whether
    /// the slot is that body's own, its function's parameter or a
    /// module-level name, which decides where readers are looked for
    #[returns(ref)]
    place: StatePlace<'db>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for WrittenPlace<'_> {}

/// The scopes a write in `program_file` to `written` invalidates.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn place_invalidations<'db>(
    db: &'db dyn Db,
    program_file: ProgramFile<'db>,
    written: WrittenPlace<'db>,
) -> StateInvalidations<'db> {
    let file = program_file.file(db);
    let mut resolver = Resolver {
        db,
        found: FxIndexSet::default(),
        opaque: false,
        visited: FxHashSet::default(),
        handed: FxHashSet::default(),
    };
    resolver.follow(written.place(db), true);
    resolver.close_over_inline_children();
    let mut scopes = resolver.found.into_iter().collect::<Vec<_>>();
    scopes.sort_by_key(|scope| (scope.file(db) != file, scope.declaration.start()));
    StateInvalidations {
        scopes: scopes.into_boxed_slice(),
        opaque: resolver.opaque,
    }
}

/// Finds the readers of a place, following it through the call graph.
struct Resolver<'db> {
    db: &'db dyn Db,
    found: FxIndexSet<InvalidatedScope<'db>>,
    opaque: bool,
    /// the places already followed, with whether their callers were
    visited: FxHashSet<(StatePlace<'db>, bool)>,
    /// the plain bodies a place was already handed into, so that two helpers
    /// calling each other end
    handed: FxHashSet<(ScopeId<'db>, StatePlace<'db>)>,
}

impl<'db> Resolver<'db> {
    /// Find the readers of `place`.
    ///
    /// `through_callers` follows a parameter up to the arguments its callers
    /// write. That is right for the place a write site names — the slot came
    /// from a caller — and wrong for a parameter reached by handing a place
    /// *down* to a callee: the callee's other callers fill it with other
    /// slots, which the write does not touch.
    fn follow(&mut self, place: &StatePlace<'db>, through_callers: bool) {
        if !self.visited.insert((place.clone(), through_callers)) {
            return;
        }
        let db = self.db;
        let definition = place.root.definition;
        let program_file = definition.program_file(db);
        let index = semantic_index(db, program_file);
        let compositions = file_compositions(db, program_file);
        match place.root.kind {
            PlaceRootKind::Global => {
                self.sweep(compositions, index, place, None);
                // any file may import a module-level slot
                self.opaque = true;
            }
            PlaceRootKind::Local => {
                let module = parsed_module(db, program_file.python_file(db)).load(db);
                match local_binding(db, program_file, index, &module, definition) {
                    LocalBinding::Slot {
                        owner_scope,
                        inline_elsewhere,
                    } => {
                        self.sweep(compositions, index, place, Some(owner_scope));
                        // an inline parent in another file cannot be seen
                        self.opaque |= inline_elsewhere;
                    }
                    LocalBinding::Parameter { scope, position } => {
                        self.sweep(compositions, index, place, Some(scope));
                        if through_callers {
                            self.callers(compositions, program_file, scope, position, place);
                            // a caller in another file cannot be seen
                            self.opaque = true;
                        }
                    }
                    LocalBinding::Unknown => {
                        // whoever reads the same binding re-runs for sure;
                        // what else holds the value cannot be seen
                        self.sweep(compositions, index, place, Some(definition.file_scope(db)));
                        self.opaque = true;
                    }
                }
            }
            PlaceRootKind::Parameter { index: position } => {
                let scope = definition.file_scope(db);
                self.sweep(compositions, index, place, Some(scope));
                if through_callers {
                    self.callers(compositions, program_file, scope, position, place);
                    // a caller in another file cannot be seen
                    self.opaque = true;
                }
            }
        }
    }

    /// Look for readers of `place` in every composition scope and computation
    /// of a file.
    ///
    /// A place is identified by its binding, so any scope whose reads name it
    /// is a reader — the scopes that can see the binding, and the parents an
    /// inline child's reads were lifted into. Only where the binding is
    /// visible — the scopes within `region`, `None` for a module-level slot
    /// visible from everywhere — can a call hand it on, and can a callee that
    /// could not be followed, or an argument that cannot be seen into, have
    /// received it: those scopes alone are followed further and make the
    /// answer opaque.
    fn sweep(
        &mut self,
        compositions: &FileCompositions<'db>,
        index: &SemanticIndex<'db>,
        place: &StatePlace<'db>,
        region: Option<FileScopeId>,
    ) {
        let db = self.db;
        let in_region = |scope: FileScopeId| {
            region.is_none_or(|region| index.ancestor_scopes(scope).any(|(id, _)| id == region))
        };

        for scope in &compositions.scopes {
            let reads = match scope.what {
                Invalidated::Composable(function) => function_scope_reads(db, function),
                Invalidated::Root(root) => root_state_reads(db, root).clone(),
                Invalidated::Derived(_) => continue,
            };
            if reads.places.iter().any(|read| read.same_place(place)) {
                self.found.insert(scope.invalidated());
            }
            if !in_region(scope.scope.file_scope_id(db)) {
                continue;
            }
            self.opaque |= reads.opaque;
            match scope.what {
                Invalidated::Composable(function) => {
                    for overload in function.iter_overloads_and_implementation(db) {
                        self.hand_on(body_state_read_effects(db, overload), place);
                    }
                }
                Invalidated::Root(root) => {
                    self.hand_on(root_state_read_effects(db, root), place);
                }
                Invalidated::Derived(_) => {}
            }
        }

        for computation in &compositions.computations {
            let reads = lambda_scope_state_reads(db, computation.lambda);
            if in_region(computation.lambda.file_scope_id(db)) {
                self.opaque |= reads.opaque;
            }
            if !reads.places.iter().any(|read| read.same_place(place)) {
                continue;
            }
            match &computation.kind {
                // a `remember` reads on behalf of the scope that made it
                ComputationKind::Remember => match &computation.owner {
                    Some(owner) => {
                        self.found.insert(owner.clone());
                    }
                    None => self.opaque = true,
                },
                // a derived is invalidated, and then so is whatever reads it
                ComputationKind::Derived(Some(binding)) => {
                    self.found.insert(InvalidatedScope {
                        what: Invalidated::Derived(binding.definition),
                        name: binding.name.clone(),
                        declaration: binding.declaration,
                    });
                    self.follow(&binding.place(), false);
                }
                // a derived nothing is bound to is read through whatever
                // holds it, which cannot be named
                ComputationKind::Derived(None) => self.opaque = true,
            }
        }
    }

    /// Follow `place` into every callee of `effects` it is handed to, as the
    /// parameter the argument fills: a composable callee is followed as a
    /// scope of its own; a plain one — a helper that hands the slot on — is
    /// walked for the calls *it* makes, with the slot under its parameter's
    /// name, and under its own name when the helper captures it.
    fn hand_on(&mut self, effects: &StateReadEffects<'db>, place: &StatePlace<'db>) {
        let db = self.db;
        for call in &effects.calls {
            let composable = is_composable_literal(db, call.callee);
            for overload in call.callee.iter_overloads_and_implementation(db) {
                let callee_effects = (!composable).then(|| body_state_read_effects(db, overload));
                // a plain callee that calls nothing can hand nothing on
                if callee_effects.is_some_and(|effects| effects.calls.is_empty()) {
                    continue;
                }
                if let Some(callee_effects) = callee_effects {
                    self.hand_into(overload, callee_effects, place);
                }
                for parameter in parameter_roots(db, overload) {
                    let Some(argument) = call.argument(parameter.index, &parameter.root.name)
                    else {
                        // the parameter is filled by an unpacked argument,
                        // which may well be the slot
                        if call.may_fill_unseen(parameter.index, &parameter.root.name) {
                            self.opaque = true;
                        }
                        continue;
                    };
                    if argument.root.definition != place.root.definition
                        || !place.path.starts_with(&argument.path)
                    {
                        continue;
                    }
                    let handed = StatePlace::at_root(parameter.root.clone())
                        .extended(&place.path[argument.path.len()..]);
                    match callee_effects {
                        Some(callee_effects) => self.hand_into(overload, callee_effects, &handed),
                        None => self.follow(&handed, false),
                    }
                }
            }
        }
    }

    /// [`Self::hand_on`] for the body of the plain function `overload`, once
    /// per place.
    fn hand_into(
        &mut self,
        overload: OverloadLiteral<'db>,
        effects: &StateReadEffects<'db>,
        place: &StatePlace<'db>,
    ) {
        if self
            .handed
            .insert((overload.body_scope(self.db), place.clone()))
        {
            self.hand_on(effects, place);
        }
    }

    /// Add the composables every found scope calls with a content block,
    /// and theirs, and so on down.
    ///
    /// The runtime never skips a child called with a block: it re-runs
    /// whenever its parent does, whether or not it reads what was written.
    /// The calls of a plain function the scope calls while composing are the
    /// scope's own.
    fn close_over_inline_children(&mut self) {
        let db = self.db;
        let mut pending: Vec<InvalidatedScope<'db>> = self.found.iter().cloned().collect();
        let mut walked: FxHashSet<ScopeId<'db>> = FxHashSet::default();
        while let Some(scope) = pending.pop() {
            let mut bodies = Vec::new();
            match scope.what {
                Invalidated::Composable(function) => {
                    for overload in function.iter_overloads_and_implementation(db) {
                        if walked.insert(overload.body_scope(db)) {
                            bodies.push(body_state_read_effects(db, overload));
                        }
                    }
                }
                Invalidated::Root(root) => {
                    if walked.insert(root) {
                        bodies.push(root_state_read_effects(db, root));
                    }
                }
                Invalidated::Derived(_) => continue,
            }
            while let Some(effects) = bodies.pop() {
                for call in &effects.calls {
                    if !is_composable_literal(db, call.callee) {
                        for overload in call.callee.iter_overloads_and_implementation(db) {
                            if walked.insert(overload.body_scope(db)) {
                                effects_of_plain_callee(db, overload, &mut bodies);
                            }
                        }
                        continue;
                    }
                    if !call.inline {
                        continue;
                    }
                    let compositions =
                        file_compositions(db, call.callee.last_definition.program_file(db));
                    let child = compositions
                        .scopes
                        .iter()
                        .find(|scope| scope.what == Invalidated::Composable(call.callee))
                        .map(CompositionScope::invalidated);
                    if let Some(child) = child
                        && self.found.insert(child.clone())
                    {
                        pending.push(child);
                    }
                }
            }
        }
    }

    /// Follow the parameter at `position` of the function whose body is
    /// `function_scope` back to what each caller in the file writes for it.
    fn callers(
        &mut self,
        compositions: &FileCompositions<'db>,
        program_file: ProgramFile<'db>,
        function_scope: FileScopeId,
        position: Option<usize>,
        place: &StatePlace<'db>,
    ) {
        let db = self.db;
        let body = function_scope.to_scope_id(db, program_file);
        let mut arguments = Vec::new();
        for effects in compositions.effects(db) {
            for call in &effects.calls {
                if !call
                    .callee
                    .iter_overloads_and_implementation(db)
                    .any(|overload| overload.body_scope(db) == body)
                {
                    continue;
                }
                if let Some(argument) = call.argument(position, &place.root.name) {
                    arguments.push(argument.extended(&place.path));
                }
            }
        }
        for argument in arguments {
            self.follow(&argument, true);
        }
    }
}

/// Push the effects of the plain function `overload` onto `bodies`, when it
/// calls anything while composing.
fn effects_of_plain_callee<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
    bodies: &mut Vec<&'db StateReadEffects<'db>>,
) {
    let effects = body_state_read_effects(db, overload);
    if !effects.calls.is_empty() {
        bodies.push(effects);
    }
}

/// whether `function` is decorated with the framework's `@composable`
fn is_composable_literal<'db>(db: &'db dyn Db, function: FunctionLiteral<'db>) -> bool {
    function
        .iter_overloads_and_implementation(db)
        .any(|overload| overload.has_known_decorator(db, FunctionDecorators::COMPOSABLE))
}

/// What a name bound in a function stands for, when a body writes it.
enum LocalBinding {
    /// a slot: a name bound while composing to what a call returned — `let
    /// count = state(0)`, in a composable's body, in a content block written
    /// in it, or in a root. Its readers are wherever the name is visible, and
    /// the parents its scope runs inline in
    Slot {
        /// the scope of the composition the slot belongs to — for a slot
        /// declared in a content block, the composable's body, not the block
        owner_scope: FileScopeId,
        /// whether the composition can be called with a content block from
        /// a file the walk cannot see, so that an unseen parent may be
        /// subscribed to the slot: a composable with a callable last
        /// parameter, unless it is `private` to its file
        inline_elsewhere: bool,
    },
    /// a parameter of an enclosing function, seen from a scope nested in it:
    /// the callers of that function fill it
    Parameter {
        scope: FileScopeId,
        position: Option<usize>,
    },
    /// anything else — a loop or comprehension target, a block's `it`, a
    /// lambda's parameter, a value bound after composing or outside every
    /// composition, a subscript, an unpacking: what the name holds may have
    /// readers the walk cannot see
    Unknown,
}

/// What the name `definition` binds stands for.
fn local_binding<'db>(
    db: &'db dyn Db,
    program_file: ProgramFile<'db>,
    index: &SemanticIndex<'db>,
    module: &ParsedModuleRef,
    definition: Definition<'db>,
) -> LocalBinding {
    let scope = definition.file_scope(db);
    let binds_call = match definition.kind(db) {
        DefinitionKind::Parameter(_) => {
            // a parameter of a `def` is filled by the def's callers; a block's
            // `it` or a lambda's parameter by whatever calls the callback
            if let NodeWithScopeKind::Function(function) = index.scope(scope).node() {
                let function = function.node(module);
                if !function.is_trailing_lambda
                    && let Some(parameter) = function_parameters(index, function).get(&definition)
                {
                    return LocalBinding::Parameter {
                        scope,
                        position: parameter.index,
                    };
                }
            }
            return LocalBinding::Unknown;
        }
        DefinitionKind::Assignment(assignment) => {
            assignment.unpack().is_none() && assignment.value(module).is_call_expr()
        }
        DefinitionKind::AnnotatedAssignment(assignment) => {
            assignment.value(module).is_some_and(Expr::is_call_expr)
        }
        _ => false,
    };
    if !binds_call {
        return LocalBinding::Unknown;
    }
    let file = program_file.file(db);
    let Some(composition) = composition_of_scope(db, file, index, module, scope) else {
        return LocalBinding::Unknown;
    };
    if !composition.runs_while_composing() {
        return LocalBinding::Unknown;
    }
    let inline_elsewhere = match composition.owner {
        CompositionOwner::Composable(function) => {
            !function.has_known_decorator(db, FunctionDecorators::PRIVATE)
                && callee_accepts_block(db, Type::FunctionLiteral(function))
        }
        CompositionOwner::Root(_) => false,
    };
    LocalBinding::Slot {
        owner_scope: composition.owner_scope,
        inline_elsewhere,
    }
}

// ---------------------------------------------------------------------------
// what subscribes a scope
// ---------------------------------------------------------------------------

/// The observables `overload` reads for its *own* scope while composing: what
/// its body and content blocks read, and what the plain functions and the
/// composables called with a content block read on its behalf — mapped
/// through the arguments, kept when they are the callee's own cells or a name
/// it captures from an enclosing function. A composable callee without a
/// block is left out: it reads for a scope of its own.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| StateReads::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn scope_state_reads<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> StateReads<'db> {
    let parameters = parameter_roots(db, overload)
        .iter()
        .map(|parameter| (parameter.root.definition, parameter.index))
        .collect();
    resolve_scope_reads(
        db,
        overload.file(db),
        body_state_read_effects(db, overload),
        Some(overload.body_scope(db)),
        &parameters,
    )
}

/// [`scope_state_reads`] over every overload and the implementation of
/// `function`.
fn function_scope_reads<'db>(db: &'db dyn Db, function: FunctionLiteral<'db>) -> StateReads<'db> {
    let mut places = FxIndexSet::default();
    let mut opaque = false;
    for overload in function.iter_overloads_and_implementation(db) {
        let reads = scope_state_reads(db, overload);
        places.extend(reads.places.iter().cloned());
        opaque |= reads.opaque;
    }
    StateReads::new(db, function.last_definition.file(db), places, opaque)
}

/// [`StateReadEffects`] of the root whose scope is `scope`: what the `root`
/// of `run_app` / `compose_test`, or the function or lambda handed to
/// `Runtime.set_root`, reads and calls — walked as a composable body with no
/// parameters of its own.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn root_state_read_effects<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> StateReadEffects<'db> {
    let program_file = scope.program_file(db);
    if !program_file.file(db).source_type(db).is_basedpython() {
        return StateReadEffects::default();
    }
    let env = ProgramEnvironment::from_file(program_file);
    if !framework_resolves(db, &env) {
        return StateReadEffects::default();
    }
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let index = semantic_index(db, program_file);
    let mut collector = ReadsCollector::new(
        db,
        env,
        program_file,
        index,
        &module,
        FxHashMap::default(),
        scope.file_scope_id(db),
    );
    match scope.node(db) {
        NodeWithScopeKind::Function(root) => collector.visit_body(&root.node(&module).body),
        NodeWithScopeKind::Lambda(root) => collector.visit_expr(&root.node(&module).body),
        _ => return StateReadEffects::default(),
    }
    collector.finish()
}

/// The observables the root whose scope is `scope` reads for its own scope
/// — the runtime's `root`.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn root_state_reads<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> StateReads<'db> {
    resolve_scope_reads(
        db,
        scope.file(db),
        root_state_read_effects(db, scope),
        None,
        &FxHashMap::default(),
    )
}

/// The observables the `derived` / `remember` computation written as the
/// lambda whose scope is `scope` reads.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn lambda_scope_state_reads<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> StateReads<'db> {
    let NodeWithScopeKind::Lambda(lambda) = scope.node(db) else {
        return StateReads::default();
    };
    let program_file = scope.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let index = semantic_index(db, program_file);
    lambda_state_reads(db, program_file, index, &module, lambda.node(&module))
}

/// Union the reads of `effects` that subscribe the scope they belong to,
/// following each plain callee and each composable called with a block, and
/// stopping at each composable called without one.
///
/// `self_body_scope` drops a directly recursive call, as
/// [`resolve_state_reads`](super::state_reads::resolve_state_reads) does;
/// `parameters` are the body's own parameters, which decide how a name a
/// callee captures from this body is seen from here.
fn resolve_scope_reads<'db>(
    db: &'db dyn Db,
    file: File,
    effects: &StateReadEffects<'db>,
    self_body_scope: Option<ScopeId<'db>>,
    parameters: &FxHashMap<Definition<'db>, Option<usize>>,
) -> StateReads<'db> {
    let mut places: FxIndexSet<StatePlace<'db>> = effects.reads.iter().cloned().collect();
    let mut opaque = effects.opaque;

    for call in &effects.calls {
        if call
            .callee
            .iter_overloads_and_implementation(db)
            .any(|overload| Some(overload.body_scope(db)) == self_body_scope)
        {
            continue;
        }
        // a composable callee reads for a scope of its own — unless the call
        // carries a content block, which the runtime runs inline: the child
        // re-runs with this scope, and what it reads subscribes this scope
        if !call.inline && is_composable_literal(db, call.callee) {
            continue;
        }
        let callee = function_scope_reads(db, call.callee);
        opaque |= callee.opaque;
        for place in &callee.places {
            match place.root.kind {
                PlaceRootKind::Global => {
                    places.insert(place.clone());
                }
                PlaceRootKind::Parameter { index } => {
                    if let Some(argument) = call.argument(index, &place.root.name) {
                        places.insert(argument.extended(&place.path));
                    } else if call.may_fill_unseen(index, &place.root.name) {
                        // the callee reads a parameter an unpacked argument
                        // fills, from something the walk cannot see into
                        opaque = true;
                    }
                }
                // a cell the callee makes while composing is a slot of this
                // scope, read on its behalf — as is a name the callee
                // captures from a function enclosing it, which is seen from
                // here as this body sees the binding
                PlaceRootKind::Local => {
                    places.insert(reclassified(db, place, parameters));
                }
            }
        }
    }

    StateReads::new(db, file, places, opaque)
}

/// `place` with its root's kind as the body whose parameters are
/// `parameters` sees the binding
fn reclassified<'db>(
    db: &'db dyn Db,
    place: &StatePlace<'db>,
    parameters: &FxHashMap<Definition<'db>, Option<usize>>,
) -> StatePlace<'db> {
    let definition = place.root.definition;
    let kind = match parameters.get(&definition) {
        Some(index) => PlaceRootKind::Parameter { index: *index },
        None if definition.file_scope(db).is_global() => PlaceRootKind::Global,
        None => PlaceRootKind::Local,
    };
    StatePlace {
        root: PlaceRoot {
            kind,
            ..place.root.clone()
        },
        path: place.path.clone(),
    }
}

/// One parameter of a function as a place root, for the callee's side of a
/// call: a place handed to the call is seen inside the callee from here.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct ParameterRoot<'db> {
    /// its position among the positional parameters, `None` for a
    /// keyword-only one
    index: Option<usize>,
    root: PlaceRoot<'db>,
}

/// The parameters of `overload` a call can fill, as place roots.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn parameter_roots<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> Box<[ParameterRoot<'db>]> {
    let file = overload.file(db);
    let module = parsed_module(db, overload.python_file(db)).load(db);
    let index = semantic_index(db, overload.program_file(db));
    let node = overload.node(db, file, &module);
    parameter_definitions(index, node)
        .map(|(index, parameter, definition)| ParameterRoot {
            index,
            root: PlaceRoot {
                definition,
                kind: PlaceRootKind::Parameter { index },
                name: parameter.name.id.clone(),
                declaration: parameter.name.range(),
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the composition scopes and computations of a file
// ---------------------------------------------------------------------------

/// A composition scope of a file: a composable's body, or the root of an
/// entry point.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct CompositionScope<'db> {
    /// the scope of the body — the composable's, or the root's
    scope: ScopeId<'db>,
    /// the scope as the editor names it: `Invalidated::Composable` or
    /// `Invalidated::Root`, never `Invalidated::Derived`
    what: Invalidated<'db>,
    name: Name,
    declaration: TextRange,
}

impl<'db> CompositionScope<'db> {
    fn invalidated(&self) -> InvalidatedScope<'db> {
        InvalidatedScope {
            what: self.what.clone(),
            name: self.name.clone(),
            declaration: self.declaration,
        }
    }
}

/// The name a `derived(...)` result is bound to: `total` in `let total =
/// derived(lambda: ...)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct DerivedBinding<'db> {
    definition: Definition<'db>,
    name: Name,
    declaration: TextRange,
    /// whether the binding is module-level, so that any function may read it
    global: bool,
}

impl<'db> DerivedBinding<'db> {
    /// the binding as a place, seen from the scope that binds it
    fn place(&self) -> StatePlace<'db> {
        StatePlace::at_root(PlaceRoot {
            definition: self.definition,
            kind: if self.global {
                PlaceRootKind::Global
            } else {
                PlaceRootKind::Local
            },
            name: self.name.clone(),
            declaration: self.declaration,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
enum ComputationKind<'db> {
    /// `derived(lambda: ...)`, with the name its result is bound to when
    /// there is one
    Derived(Option<DerivedBinding<'db>>),
    /// `remember(lambda: ...)`
    Remember,
}

/// A `derived(lambda: ...)` or `remember(lambda: ...)` written in a file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
struct Computation<'db> {
    /// the lambda's scope, whose reads are what the computation depends on
    lambda: ScopeId<'db>,
    /// the composition scope the computation is made in, which a `remember`
    /// reads on behalf of
    owner: Option<InvalidatedScope<'db>>,
    kind: ComputationKind<'db>,
}

/// Everything in a file a write can invalidate, and every body whose calls
/// can hand a slot on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, get_size2::GetSize, salsa::SalsaValue)]
struct FileCompositions<'db> {
    scopes: Box<[CompositionScope<'db>]>,
    /// the functions of the file that are not composition scopes, whose
    /// calls made while composing can fill a parameter with a slot
    functions: Box<[FunctionLiteral<'db>]>,
    computations: Box<[Computation<'db>]>,
}

impl<'db> FileCompositions<'db> {
    /// the effects of every body of the file that calls while composing
    fn effects(&self, db: &'db dyn Db) -> Vec<&'db StateReadEffects<'db>> {
        let mut effects = Vec::new();
        for scope in &self.scopes {
            match scope.what {
                Invalidated::Composable(function) => {
                    for overload in function.iter_overloads_and_implementation(db) {
                        effects.push(body_state_read_effects(db, overload));
                    }
                }
                Invalidated::Root(root) => effects.push(root_state_read_effects(db, root)),
                Invalidated::Derived(_) => {}
            }
        }
        for function in &self.functions {
            for overload in function.iter_overloads_and_implementation(db) {
                effects.push(body_state_read_effects(db, overload));
            }
        }
        effects
    }
}

/// The composition scopes, plain functions and computations of a file.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn file_compositions<'db>(
    db: &'db dyn Db,
    program_file: ProgramFile<'db>,
) -> FileCompositions<'db> {
    let file = program_file.file(db);
    if !file.source_type(db).is_basedpython() {
        return FileCompositions::default();
    }
    let env = ProgramEnvironment::from_file(program_file);
    if !framework_resolves(db, &env) {
        return FileCompositions::default();
    }
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let index = semantic_index(db, program_file);

    // the range of what was handed to `set_root`, when `scope` is its root
    let set_root_declaration = |scope: FileScopeId| {
        let composition = composition_of_scope(db, file, index, &module, scope)?;
        if !matches!(
            composition.owner,
            CompositionOwner::Root(RootEntry::SetRoot)
        ) || composition.owner_scope != scope
        {
            return None;
        }
        composition.owner_range()
    };
    let root = |scope: ScopeId<'db>, declaration: TextRange| CompositionScope {
        scope,
        what: Invalidated::Root(scope),
        name: Name::new_static("root"),
        declaration,
    };

    let mut scopes = Vec::new();
    let mut functions = Vec::new();
    let mut lambdas = Vec::new();
    for scope in index.scope_ids() {
        let file_scope = scope.file_scope_id(db);
        match index.scope(file_scope).node() {
            NodeWithScopeKind::Function(function) => {
                let node = function.node(&module);
                if node.is_trailing_lambda {
                    if let BlockKind::Root(_) = block_kind(db, index, node)
                        && let Some(callee) = node.trailing_lambda_callee()
                    {
                        scopes.push(root(scope, callee.range()));
                    }
                    continue;
                }
                let Some(definition) = index.try_definition(node) else {
                    continue;
                };
                let Some(function) =
                    infer_definition_types(db, definition).function_type(definition)
                else {
                    continue;
                };
                let literal = function.literal(db);
                if is_composable(db, function) {
                    scopes.push(CompositionScope {
                        scope,
                        what: Invalidated::Composable(literal),
                        name: node.name.id.clone(),
                        declaration: node.name.range(),
                    });
                } else if let Some(declaration) = set_root_declaration(file_scope) {
                    scopes.push(root(scope, declaration));
                } else {
                    functions.push(literal);
                }
            }
            NodeWithScopeKind::Lambda(lambda) => {
                if let Some(declaration) = set_root_declaration(file_scope) {
                    scopes.push(root(scope, declaration));
                } else {
                    lambdas.push((scope, lambda.node(&module)));
                }
            }
            _ => {}
        }
    }

    let mut computations = Vec::new();
    for (scope, lambda) in lambdas {
        let file_scope = scope.file_scope_id(db);
        let Some(known) = computation_kind(db, index, &module, file_scope, lambda.range()) else {
            continue;
        };
        let owner =
            composition_of_scope(db, file, index, &module, file_scope).and_then(|composition| {
                scopes
                    .iter()
                    .find(|scope| scope.scope.file_scope_id(db) == composition.owner_scope)
                    .map(CompositionScope::invalidated)
            });
        let kind = if known == KnownFunction::BasedpythonUiRemember {
            ComputationKind::Remember
        } else {
            ComputationKind::Derived(derived_binding(index, &module, file_scope, lambda))
        };
        computations.push(Computation {
            lambda: scope,
            owner,
            kind,
        });
    }

    FileCompositions {
        scopes: scopes.into_boxed_slice(),
        functions: functions.into_boxed_slice(),
        computations: computations.into_boxed_slice(),
    }
}

/// The name the `derived(...)` call whose `compute` is `lambda` (the scope
/// `scope`) is bound to, when the call is the whole value of an assignment to
/// one name in the enclosing scope.
fn derived_binding<'db>(
    index: &SemanticIndex<'db>,
    module: &ParsedModuleRef,
    scope: FileScopeId,
    lambda: &ast::ExprLambda,
) -> Option<DerivedBinding<'db>> {
    let parent = index.parent_scope_id(scope)?;
    let body: &[Stmt] = match index.scope(parent).node() {
        NodeWithScopeKind::Function(function) => &function.node(module).body,
        NodeWithScopeKind::Module => &module.syntax().body,
        NodeWithScopeKind::Class(class) => &class.node(module).body,
        _ => return None,
    };
    let mut finder = BindingFinder {
        compute: lambda.range(),
        found: None,
    };
    finder.visit_body(body);
    let (target, definition) = finder.found?;
    Some(DerivedBinding {
        definition: index.try_definition(definition)?,
        name: target.id.clone(),
        declaration: target.range(),
        global: parent.is_global(),
    })
}

/// Finds, in a scope's own statements, the assignment whose value is the
/// call with `compute` as its computation: the name bound, and the key its
/// definition is filed under — the target for `total = derived(...)`, the
/// statement for `let total = derived(...)`, which is an annotated
/// assignment.
struct BindingFinder<'a> {
    compute: TextRange,
    found: Option<(&'a ast::ExprName, DefinitionNodeKey)>,
}

impl BindingFinder<'_> {
    fn binds(&self, value: &Expr) -> bool {
        matches!(
            value,
            Expr::Call(call)
                if call
                    .arguments
                    .find_argument_value("compute", 0)
                    .is_some_and(|compute| compute.range() == self.compute)
        )
    }
}

impl<'a> Visitor<'a> for BindingFinder<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.found.is_some() {
            return;
        }
        match stmt {
            // a nested scope's statements are not this scope's
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::Assign(assign) => {
                if let [Expr::Name(target)] = assign.targets.as_slice()
                    && self.binds(&assign.value)
                {
                    let key = <DefinitionNodeKey as From<&ast::ExprName>>::from(target);
                    self.found = Some((target, key));
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Expr::Name(target) = assign.target.as_ref()
                    && let Some(value) = assign.value.as_deref()
                    && self.binds(value)
                {
                    let key = <DefinitionNodeKey as From<&ast::StmtAnnAssign>>::from(assign);
                    self.found = Some((target, key));
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, _expr: &'a Expr) {}
}

/// whether the framework resolves in the program `env` belongs to
fn framework_resolves<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> bool {
    resolve_module_confident(
        db,
        env.resolver_environment(db),
        &KnownModule::BasedpythonUiRuntime.name(),
    )
    .is_some()
}

// ---------------------------------------------------------------------------
// a write site
// ---------------------------------------------------------------------------

/// Where a statement or a lambda body writes observables.
#[derive(Clone, Copy)]
pub enum WriteSite<'a> {
    /// a statement: its own expressions, not those of the statements nested
    /// in it, which are write sites of their own
    Statement(&'a Stmt),
    /// a lambda's body — `on_click=lambda: count.set(0)`
    Lambda(&'a ast::ExprLambda),
}

/// What the writes of one site invalidate.
pub(crate) struct SiteInvalidations<'db> {
    /// in declaration order, the site's own file first
    pub(crate) scopes: Vec<InvalidatedScope<'db>>,
    /// whether a reader may have been missed, or a written observable has no
    /// name to look readers up by
    pub(crate) opaque: bool,
    /// where the last write of the site is spelled — `count.value`,
    /// `todos.append`, `table["a"]` — for placing a hint on its line when the
    /// site itself spans more than one
    pub(crate) anchor: TextRange,
}

/// The scopes the observable writes of `site` invalidate. `None` when the
/// site writes no observable, and when it runs while composing — such a
/// write is a diagnostic, not something to trace.
pub(crate) fn site_invalidations<'db>(
    db: &'db dyn Db,
    program_file: ProgramFile<'db>,
    site: WriteSite<'_>,
) -> Option<SiteInvalidations<'db>> {
    let file = program_file.file(db);
    if !file.source_type(db).is_basedpython() {
        return None;
    }

    // the syntax alone says whether anything here could be a write, so the
    // common statement costs no type lookup at all
    let mut candidates = Candidates::default();
    match site {
        WriteSite::Statement(stmt) => candidates.visit_stmt(stmt),
        WriteSite::Lambda(lambda) => candidates.visit_expr(&lambda.body),
    }
    if candidates.found.is_empty() {
        return None;
    }

    let env = ProgramEnvironment::from_file(program_file);
    if !framework_resolves(db, &env) {
        return None;
    }
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let index = semantic_index(db, program_file);

    let writes = candidates
        .found
        .into_iter()
        .filter_map(|expr| resolve_write(db, &env, program_file, index, &module, expr))
        .collect::<Vec<_>>();
    let (first, last) = match writes.as_slice() {
        [] => return None,
        [first, .., last] | [first @ last] => (first, last),
    };
    if !runs_after_composing(db, file, index, &module, first.scope) {
        return None;
    }

    let mut found = FxIndexSet::default();
    let mut opaque = false;
    for write in &writes {
        match &write.place {
            Some(place) => {
                let written = WrittenPlace::new(db, place.clone());
                let invalidations = place_invalidations(db, program_file, written);
                found.extend(invalidations.scopes.iter().cloned());
                opaque |= invalidations.opaque;
            }
            None => opaque = true,
        }
    }
    let mut scopes = found.into_iter().collect::<Vec<_>>();
    scopes.sort_by_key(|scope| (scope.file(db) != file, scope.declaration.start()));
    Some(SiteInvalidations {
        scopes,
        opaque,
        anchor: last.range,
    })
}

/// One observable write of a site.
struct StateWrite<'db> {
    /// the expression that spells the write
    range: TextRange,
    /// the place written; `None` for an observable with no name to look
    /// readers up by (`state(0).value = 1`, `cells[0].set(1)`), and for a
    /// name bound to such a thing
    place: Option<StatePlace<'db>>,
    /// the scope the write is in
    scope: FileScopeId,
}

/// `expr`, a syntactic candidate, as the observable write it is — or `None`
/// when its receiver is not an observable, or the method is not one of that
/// observable's mutators.
fn resolve_write<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    program_file: ProgramFile<'db>,
    index: &SemanticIndex<'db>,
    module: &ParsedModuleRef,
    expr: &Expr,
) -> Option<StateWrite<'db>> {
    let scope = index.try_expression_scope_id(expr)?;
    let collector = ReadsCollector::new(
        db,
        env.clone(),
        program_file,
        index,
        module,
        enclosing_parameters(index, module, scope),
        scope,
    );
    match expr {
        // `count.value = 1`
        Expr::Attribute(attribute) => (collector.observable_of(&attribute.value)
            == Some(ObservableKind::State))
        .then(|| StateWrite {
            range: attribute.range(),
            place: collector.place_of(&attribute.value),
            scope,
        }),
        // `todos[0] = todo`, `table["a"] = 1`
        Expr::Subscript(subscript) => matches!(
            collector.observable_of(&subscript.value),
            Some(ObservableKind::StateList | ObservableKind::StateDict)
        )
        .then(|| StateWrite {
            range: subscript.range(),
            place: collector.place_of(&subscript.value),
            scope,
        }),
        // `count.set(0)`, `todos.append(todo)`
        Expr::Call(call) => {
            let Type::BoundMethod(method) = collector.type_of(&call.func)? else {
                return None;
            };
            let kind = observable_kind(db, collector.env(), method.self_instance(db))?;
            if !kind.is_mutator(method.function(db).name(db)) {
                return None;
            }
            let place = match call.func.as_ref() {
                Expr::Attribute(attribute) => collector.place_of(&attribute.value),
                _ => None,
            };
            Some(StateWrite {
                range: call.func.range(),
                place,
                scope,
            })
        }
        _ => None,
    }
}

/// The parameters of the function `scope` is written in — the nearest
/// enclosing `def` that is not a content block — which decide how a name the
/// scope writes is seen: as that function's parameter, a local, or a global.
fn enclosing_parameters<'db>(
    index: &SemanticIndex<'db>,
    module: &ParsedModuleRef,
    scope: FileScopeId,
) -> FxHashMap<Definition<'db>, ParameterInfo> {
    for (_, ancestor) in index.ancestor_scopes(scope) {
        match ancestor.node() {
            NodeWithScopeKind::Function(function) => {
                let function = function.node(module);
                if !function.is_trailing_lambda {
                    return function_parameters(index, function);
                }
            }
            NodeWithScopeKind::Module | NodeWithScopeKind::Class(_) => break,
            _ => {}
        }
    }
    FxHashMap::default()
}

/// whether what `scope` does runs after a composition: in a handler block, a
/// lambda, a nested `def` or an effect of a composition, or in a function
/// outside every composition, which runs when something calls it. Module-level
/// code runs at import, before any composition there is
fn runs_after_composing<'db>(
    db: &'db dyn Db,
    file: File,
    index: &SemanticIndex<'db>,
    module: &ParsedModuleRef,
    scope: FileScopeId,
) -> bool {
    match composition_of_scope(db, file, index, module, scope) {
        Some(composition) => composition.runs_after_composing(),
        None => index.ancestor_scopes(scope).any(|(_, ancestor)| {
            matches!(
                ancestor.node(),
                NodeWithScopeKind::Function(_) | NodeWithScopeKind::Lambda(_)
            )
        }),
    }
}

/// Collects the expressions of one statement, or one lambda body, that are
/// spelled like an observable write: a store to `.value`, a subscript store,
/// a call to a method named like a mutator of some observable. Which of them
/// are writes is decided by their types afterwards.
#[derive(Default)]
struct Candidates<'a> {
    found: Vec<&'a Expr>,
}

impl<'a> Visitor<'a> for Candidates<'a> {
    // the statements nested in a compound statement, and the body of a
    // nested function or block, are write sites of their own
    fn visit_body(&mut self, _body: &'a [Stmt]) {}

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            // a lambda body is a write site of its own; its defaults run here
            Expr::Lambda(lambda) => {
                for default in lambda
                    .parameters
                    .iter()
                    .flat_map(|parameters| parameters.iter_non_variadic_params())
                    .filter_map(|parameter| parameter.default.as_deref())
                {
                    self.visit_expr(default);
                }
                return;
            }
            Expr::Attribute(attribute)
                if attribute.ctx.is_store() && attribute.attr.as_str() == "value" =>
            {
                self.found.push(expr);
            }
            Expr::Subscript(subscript) if subscript.ctx.is_store() => self.found.push(expr),
            Expr::Call(call)
                if matches!(
                    call.func.as_ref(),
                    Expr::Attribute(attribute)
                        if ObservableKind::is_any_mutator(attribute.attr.as_str())
                ) =>
            {
                self.found.push(expr);
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}
