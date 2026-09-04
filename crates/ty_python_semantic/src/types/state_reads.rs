//! basedpython-ui: static tracking of the observables a function reads while
//! composing.
//!
//! A `@composable` re-runs whenever an observable it read during its last
//! composition changes. The runtime tracks that set exactly; this module
//! recovers a static approximation of it, so an editor can show a composable's
//! dependencies on its header (`def Counter(step: int = 1) reads count:`) and
//! a `derived(...)` computation's on its line.
//!
//! A *read* is one of:
//!
//! - `.value` on a `State[T]` or `Derived[T]`, `.current` on an `Ambient[T]`
//! - iteration, `len`, a subscript, `in`, or one of the reading methods
//!   (`each`, `each_indexed`, `snapshot`, `index_where`) on a `StateList[T]`;
//!   a subscript, `in`, `len`, `get`, `keys` or `items` on a `StateDict[K, V]`
//! - a use of a `context` parameter, which the caller fills from its own
//!   composition
//!
//! Each read names a *place*: the root definition the expression starts from
//! — a parameter, a local `let`, a module-level name — plus the attribute path
//! from there (`self.model.count`). A read of something with no such place
//! (`state(0).value`) is real but nameless, and is not shown.
//!
//! Only what runs *while composing* counts: the body itself, the `once`
//! content blocks written in it (`Column:`, `Row:`) and the `local` blocks
//! of a keyed `each`, which run before their call returns. A handler block, a
//! lambda, a nested `def` or an effect block runs later, so nothing in one is
//! a read of this composition.
//!
//! Inference is interprocedural, with the same two-phase shape as the
//! exception tracking in `exceptions.rs`: [`body_state_read_effects`] reads a
//! body's own reads and calls off its inferred types, and
//! [`inferred_state_reads`] takes the least fixed point over the call graph.
//! A callee contributes the reads rooted at its parameters, mapped through the
//! arguments the call writes, and the reads of module-level names; its own
//! locals are dropped, since a `state()` created inside a callee is that
//! callee's cell. A callee that cannot be followed — a `dynamic` value — marks
//! the set *opaque*, which the hint shows as `…`.
//!
//! The set is a superset approximation for hints and lints only: invalidation
//! at runtime always uses the exact set, so imprecision here can never cause a
//! missed re-render.

use ruff_db::files::{File, FileRange};
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashMap;
use ty_module_resolver::{KnownModule, file_to_module, resolve_module_confident};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind, NodeWithScopeRef, ScopeId};
use ty_python_core::{ProgramFile, SemanticIndex, semantic_index};

use crate::types::context_params::implicit_context_arguments;
use crate::types::dedicated::basedpython_ui::{ObservableKind, observable_kind};
use crate::types::function::{FunctionLiteral, OverloadLiteral};
use crate::types::infer::ScopeInference;
use crate::types::trailing_lambda::{
    block_callee, callee_callback_is_borrowed, callee_callback_is_once,
};
use crate::types::{ProgramEnvironment, Type, TypeContext, infer_scope_types};
use crate::{Db, FxIndexSet};

/// the methods of a `StateList` that read it — every one subscribes the
/// composition to the list, exactly as iterating it would
const STATE_LIST_READERS: &[&str] = &[
    "each",
    "each_indexed",
    "snapshot",
    "index_where",
    "__len__",
    "__iter__",
    "__getitem__",
    "__contains__",
];

/// the methods of a `StateDict` that read it
const STATE_DICT_READERS: &[&str] = &[
    "get",
    "keys",
    "items",
    "__len__",
    "__iter__",
    "__getitem__",
    "__contains__",
];

/// Where a state place is rooted, relative to the function whose body was
/// walked. The kind decides what a caller sees of the place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum PlaceRootKind {
    /// one of the function's own parameters, which a caller fills: `index` is
    /// its position among the positional parameters, `None` for a keyword-only
    /// one
    Parameter { index: Option<usize> },
    /// a name bound in the function's body, in a block written in it, or in a
    /// function enclosing it — a cell of this composition, which no caller sees
    Local,
    /// a module-level name, which every caller shares
    Global,
}

/// The definition a state place starts from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct PlaceRoot<'db> {
    /// the binding the name resolves to — the place's identity
    pub(super) definition: Definition<'db>,
    pub(super) kind: PlaceRootKind,
    /// the name as the source spells it
    pub(super) name: Name,
    /// the range of the binding's name in the file `definition` is in, for
    /// navigation and ordering; recorded while that file is being read, so
    /// that a caller in another file never needs its syntax tree
    pub(super) declaration: TextRange,
}

/// One observable a body reads, named by where it starts and the attributes
/// read from there: `count`, or `self.model.count`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct StatePlace<'db> {
    pub(super) root: PlaceRoot<'db>,
    pub(super) path: Box<[Name]>,
}

impl<'db> StatePlace<'db> {
    pub(super) fn at_root(root: PlaceRoot<'db>) -> Self {
        Self {
            root,
            path: Box::default(),
        }
    }

    /// this place with `path` read from it: `model` extended by `count` is
    /// `model.count`
    pub(super) fn extended(&self, path: &[Name]) -> Self {
        Self {
            root: self.root.clone(),
            path: self.path.iter().chain(path).cloned().collect(),
        }
    }

    /// whether this place and `other` name the same observable: the same
    /// binding, read through the same attributes. The root's kind is left out
    /// — it says how the body that read the place sees the binding, not which
    /// binding it is — so a composable's parameter and a nested function's
    /// capture of it compare equal
    pub(super) fn same_place(&self, other: &Self) -> bool {
        self.root.definition == other.root.definition && self.path == other.path
    }

    /// the place as it is written: the root name and the attribute path
    pub(crate) fn display_name(&self) -> String {
        let mut name = self.root.name.to_string();
        for attribute in &self.path {
            name.push('.');
            name.push_str(attribute);
        }
        name
    }

    /// where the place's root is declared
    pub(crate) fn declaration(&self, db: &'db dyn Db) -> FileRange {
        FileRange::new(self.root.definition.file(db), self.root.declaration)
    }

    /// the key the places of one function are ordered by: declaration order,
    /// with the reads of another file's globals after this file's
    fn order_key(&self, db: &'db dyn Db, file: File) -> (bool, TextSize, &[Name]) {
        (
            self.root.definition.file(db) != file,
            self.root.declaration.start(),
            &self.path,
        )
    }
}

/// One call in a body, with the places its written arguments name, so that
/// the callee's parameter-rooted reads can be mapped back onto the caller.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct CallEffect<'db> {
    /// the called function, whose own reads are resolved separately
    pub(super) callee: FunctionLiteral<'db>,
    /// whether the call carried a trailing block — `Card(count):`, a bare
    /// `Row:`. The runtime runs such a callee *inline*: it re-runs whenever
    /// its caller does, and what it reads subscribes the caller's scope
    /// rather than one of its own
    pub(super) inline: bool,
    /// whether the callee's first parameter is filled by something other than
    /// the written arguments: the receiver of a bound method, or the class a
    /// classmethod is called on
    bound: bool,
    /// the place a bound method's receiver names, when it names one
    receiver: Option<StatePlace<'db>>,
    /// the place each positional argument names, up to the first unpacked one
    positional: Box<[Option<StatePlace<'db>>]>,
    /// the place each keyword argument names, the implicit `context`
    /// arguments the call fills included
    keywords: Box<[(Name, Option<StatePlace<'db>>)]>,
    /// the arguments the call unpacks, which fill parameters the written
    /// arguments do not name
    unpacked: Unpacked,
}

/// What a call unpacks into its arguments: `Child(*cells)`, `Child(**options)`.
/// What an unpacked argument fills is not knowable statically, so a parameter
/// it may land on holds something the walk cannot see into.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue,
)]
struct Unpacked {
    /// `*cells`: it, and every positional argument written after it, land on
    /// the positional parameters the arguments before it left unfilled
    positional: bool,
    /// `**options`, which can fill any parameter
    keywords: bool,
}

impl<'db> CallEffect<'db> {
    /// the place the argument filling the callee's parameter `name` (at
    /// positional `index`) names, if the call writes one that names a place
    pub(super) fn argument(&self, index: Option<usize>, name: &Name) -> Option<&StatePlace<'db>> {
        if let Some((_, place)) = self.keywords.iter().find(|(keyword, _)| keyword == name) {
            return place.as_ref();
        }
        let index = index?;
        if self.bound {
            if index == 0 {
                return self.receiver.as_ref();
            }
            self.positional.get(index - 1)?.as_ref()
        } else {
            self.positional.get(index)?.as_ref()
        }
    }

    /// whether the callee's parameter `name` (at positional `index`) may be
    /// filled by an unpacked argument: nothing written fills it, and the call
    /// unpacks something that can. What such a parameter holds is unknown,
    /// so a read through it, or a slot handed through it, cannot be seen
    pub(super) fn may_fill_unseen(&self, index: Option<usize>, name: &Name) -> bool {
        if self.keywords.iter().any(|(keyword, _)| keyword == name) {
            return false;
        }
        let written_positionally = index.is_some_and(|index| {
            if self.bound {
                index == 0 || self.positional.len() >= index
            } else {
                self.positional.len() > index
            }
        });
        if written_positionally {
            return false;
        }
        self.unpacked.keywords || (index.is_some() && self.unpacked.positional)
    }
}

/// What a function body reads and calls while composing, with its callees
/// left unresolved.
///
/// Splitting the analysis here is what keeps the recursion cheap and safe:
/// collecting the effects reads the function's own inferred expression types,
/// while [`resolve_state_reads`] walks the call graph over effects alone and
/// never re-enters type inference.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct StateReadEffects<'db> {
    /// the places read directly in this body, in first-read order
    pub(super) reads: Box<[StatePlace<'db>]>,
    /// the calls made while composing
    pub(super) calls: Box<[CallEffect<'db>]>,
    /// whether something was called that cannot be followed
    pub(super) opaque: bool,
}

impl StateReadEffects<'_> {
    /// Whether this body can read nothing at all, without resolving any callee.
    pub(crate) fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.calls.is_empty() && !self.opaque
    }
}

/// The observables a function reads while composing, its callees followed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct StateReads<'db> {
    /// the places read, in declaration order of their roots
    pub(crate) places: Box<[StatePlace<'db>]>,
    /// whether a callee could not be followed, so the set may be missing
    /// something
    pub(crate) opaque: bool,
}

impl<'db> StateReads<'db> {
    pub(super) fn new(
        db: &'db dyn Db,
        file: File,
        places: FxIndexSet<StatePlace<'db>>,
        opaque: bool,
    ) -> Self {
        let mut places = places.into_iter().collect::<Vec<_>>();
        places.sort_by(|left, right| left.order_key(db, file).cmp(&right.order_key(db, file)));
        Self {
            places: places.into_boxed_slice(),
            opaque,
        }
    }

    /// Whether nothing is read and every callee was followed.
    pub(crate) fn is_empty(&self) -> bool {
        self.places.is_empty() && !self.opaque
    }
}

/// [`StateReadEffects`] for `overload`'s body, read off its own inferred types.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| StateReadEffects::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn body_state_read_effects<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> StateReadEffects<'db> {
    let file = overload.file(db);
    if !file.source_type(db).is_basedpython() {
        return StateReadEffects::default();
    }
    let program_file = overload.program_file(db);
    let env = ProgramEnvironment::from_file(program_file);
    // there is nothing to read unless the framework resolves in this program
    if resolve_module_confident(
        db,
        env.resolver_environment(db),
        &KnownModule::BasedpythonUiRuntime.name(),
    )
    .is_none()
    {
        return StateReadEffects::default();
    }

    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let index = semantic_index(db, program_file);
    let node = overload.node(db, file, &module);
    let mut collector = ReadsCollector::new(
        db,
        env,
        program_file,
        index,
        &module,
        function_parameters(index, node),
        overload.body_scope(db).file_scope_id(db),
    );
    collector.visit_body(&node.body);
    collector.finish()
}

/// The observables `overload` reads while composing, its callees followed.
#[salsa::tracked(
    returns(ref),
    cycle_initial = |_, _, _| StateReads::default(),
    heap_size = ruff_memory_usage::heap_size,
)]
pub(crate) fn inferred_state_reads<'db>(
    db: &'db dyn Db,
    overload: OverloadLiteral<'db>,
) -> StateReads<'db> {
    resolve_state_reads(
        db,
        overload.file(db),
        body_state_read_effects(db, overload),
        Some(overload.body_scope(db)),
    )
}

/// The observables a call to `function` reads: the union over its overloads
/// and implementation, since which overload a call matched is not known here.
pub(crate) fn function_state_reads<'db>(
    db: &'db dyn Db,
    function: FunctionLiteral<'db>,
) -> StateReads<'db> {
    let mut places = FxIndexSet::default();
    let mut opaque = false;
    for overload in function.iter_overloads_and_implementation(db) {
        let reads = inferred_state_reads(db, overload);
        places.extend(reads.places.iter().cloned());
        opaque |= reads.opaque;
    }
    StateReads::new(db, function.last_definition.file(db), places, opaque)
}

/// The observables a lambda reads while composing — what a `derived(lambda:
/// ...)` depends on. The lambda's closure is its enclosing function, so a
/// local of that function is a dependency here, not a dropped cell.
pub(crate) fn lambda_state_reads<'db>(
    db: &'db dyn Db,
    program_file: ProgramFile<'db>,
    index: &SemanticIndex<'db>,
    module: &ParsedModuleRef,
    lambda: &ast::ExprLambda,
) -> StateReads<'db> {
    let env = ProgramEnvironment::from_file(program_file);
    let scope = index.node_scope(NodeWithScopeRef::Lambda(lambda));
    // the function the lambda is written in, whose `context` parameters are
    // reads when the lambda uses them
    let parameters = index
        .ancestor_scopes(scope)
        .skip(1)
        .find_map(|(_, ancestor)| match ancestor.node() {
            NodeWithScopeKind::Function(function) => Some(function.node(module)),
            NodeWithScopeKind::Module | NodeWithScopeKind::Class(_) => None,
            _ => None,
        })
        .map_or_else(FxHashMap::default, |function| {
            function_parameters(index, function)
        });
    let mut collector =
        ReadsCollector::new(db, env, program_file, index, module, parameters, scope);
    collector.visit_expr(&lambda.body);
    let effects = collector.finish();
    resolve_state_reads(db, program_file.file(db), &effects, None)
}

/// Union the reads of `effects`, following each call into its callee.
///
/// `self_body_scope` is the scope of the function the effects belong to, when
/// it is known: a directly recursive call contributes exactly the set being
/// computed, so it is the identity of the union and can be dropped rather than
/// re-entered.
fn resolve_state_reads<'db>(
    db: &'db dyn Db,
    file: File,
    effects: &StateReadEffects<'db>,
    self_body_scope: Option<ScopeId<'db>>,
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
        let callee = function_state_reads(db, call.callee);
        opaque |= callee.opaque;
        for place in &callee.places {
            match place.root.kind {
                PlaceRootKind::Global => {
                    places.insert(place.clone());
                }
                // a callee's own cell is the callee's, not the caller's
                PlaceRootKind::Local => {}
                PlaceRootKind::Parameter { index } => {
                    if let Some(argument) = call.argument(index, &place.root.name) {
                        places.insert(argument.extended(&place.path));
                    } else if call.may_fill_unseen(index, &place.root.name) {
                        // the callee reads a parameter an unpacked argument
                        // fills, from something the walk cannot see into
                        opaque = true;
                    }
                }
            }
        }
    }

    StateReads::new(db, file, places, opaque)
}

/// What is known of one parameter of the function whose body is walked.
#[derive(Clone, Copy)]
pub(super) struct ParameterInfo {
    /// its position among the positional parameters, `None` for a keyword-only
    /// one
    pub(super) index: Option<usize>,
    /// whether it is a `context` parameter, whose use is a read
    is_context: bool,
}

/// The parameters of `function`, by their definitions.
pub(super) fn function_parameters<'db>(
    index: &SemanticIndex<'db>,
    function: &ast::StmtFunctionDef,
) -> FxHashMap<Definition<'db>, ParameterInfo> {
    parameter_definitions(index, function)
        .map(|(position, parameter, definition)| {
            (
                definition,
                ParameterInfo {
                    index: position,
                    is_context: parameter.is_context,
                },
            )
        })
        .collect()
}

/// The parameters of `function` a call can fill by position or by keyword —
/// every one but the variadic pair — each with its position among the
/// positional parameters (`None` for a keyword-only one), its node and its
/// definition.
pub(super) fn parameter_definitions<'a, 'db>(
    index: &'a SemanticIndex<'db>,
    function: &'a ast::StmtFunctionDef,
) -> impl Iterator<Item = (Option<usize>, &'a ast::Parameter, Definition<'db>)> + 'a {
    let parameters = &function.parameters;
    let positional = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .enumerate()
        .map(|(position, parameter)| (Some(position), &parameter.parameter));
    let keyword_only = parameters
        .kwonlyargs
        .iter()
        .map(|parameter| (None, &parameter.parameter));
    positional
        .chain(keyword_only)
        .filter_map(move |(position, parameter)| {
            Some((position, parameter, index.try_definition(parameter)?))
        })
}

/// Collects the [`StateReadEffects`] of a body.
pub(super) struct ReadsCollector<'a, 'db> {
    db: &'db dyn Db,
    env: ProgramEnvironment<'db>,
    program_file: ProgramFile<'db>,
    index: &'a SemanticIndex<'db>,
    module: &'a ParsedModuleRef,
    /// the scopes enclosing what is being visited, innermost last, each with
    /// its own inferred types: a content block and a comprehension are scopes
    /// of their own, run while composing
    scopes: Vec<(FileScopeId, &'db ScopeInference<'db>)>,
    /// the parameters of the function the effects are for
    parameters: FxHashMap<Definition<'db>, ParameterInfo>,
    /// whether any of those is a `context` parameter, so that a name load is
    /// worth resolving
    has_context_parameters: bool,
    /// the call a trailing-lambda block is attached to, while its expression
    /// is being visited: that call, and no call nested in its arguments, is
    /// the one that carries the block
    block_call: Option<TextRange>,
    reads: FxIndexSet<StatePlace<'db>>,
    calls: Vec<CallEffect<'db>>,
    opaque: bool,
}

impl<'a, 'db> ReadsCollector<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn Db,
        env: ProgramEnvironment<'db>,
        program_file: ProgramFile<'db>,
        index: &'a SemanticIndex<'db>,
        module: &'a ParsedModuleRef,
        parameters: FxHashMap<Definition<'db>, ParameterInfo>,
        scope: FileScopeId,
    ) -> Self {
        let has_context_parameters = parameters.values().any(|parameter| parameter.is_context);
        let mut collector = Self {
            db,
            env,
            program_file,
            index,
            module,
            scopes: Vec::new(),
            parameters,
            has_context_parameters,
            block_call: None,
            reads: FxIndexSet::default(),
            calls: Vec::new(),
            opaque: false,
        };
        collector.push_scope(scope);
        collector
    }

    pub(super) fn finish(self) -> StateReadEffects<'db> {
        StateReadEffects {
            reads: self.reads.into_iter().collect(),
            calls: self.calls.into_boxed_slice(),
            opaque: self.opaque,
        }
    }

    fn push_scope(&mut self, scope: FileScopeId) {
        let inference = infer_scope_types(
            self.db,
            scope.to_scope_id(self.db, self.program_file),
            TypeContext::default(),
        );
        self.scopes.push((scope, inference));
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// the scope the expression being visited is in
    fn scope(&self) -> Option<FileScopeId> {
        self.scopes.last().map(|(scope, _)| *scope)
    }

    pub(super) fn env(&self) -> &ProgramEnvironment<'db> {
        &self.env
    }

    pub(super) fn type_of(&self, expr: &Expr) -> Option<Type<'db>> {
        self.scopes
            .last()
            .and_then(|(_, inference)| inference.try_expression_type(expr))
    }

    pub(super) fn observable_of(&self, expr: &Expr) -> Option<ObservableKind> {
        observable_kind(self.db, &self.env, self.type_of(expr)?)
    }

    /// the binding a bare `name` resolves to from the current scope
    fn definition_of_name(&self, name: &str) -> Option<Definition<'db>> {
        root_definition(self.db, self.index, self.module, self.scope()?, name)
    }

    /// `definition`, which `name` resolves to, as a place root: how the body
    /// being walked sees the binding
    fn root_of(&self, definition: Definition<'db>, name: &str) -> PlaceRoot<'db> {
        let db = self.db;
        let kind = match self.parameters.get(&definition) {
            Some(parameter) => PlaceRootKind::Parameter {
                index: parameter.index,
            },
            None if definition.file_scope(db).is_global() => PlaceRootKind::Global,
            None => PlaceRootKind::Local,
        };
        PlaceRoot {
            definition,
            kind,
            name: Name::new(name),
            declaration: definition.focus_range(db, self.module).range(),
        }
    }

    /// the place `expr` names, when it is a name or an attribute chain from
    /// one, read from the current scope
    pub(super) fn place_of(&self, expr: &Expr) -> Option<StatePlace<'db>> {
        self.place_in(self.scope()?, expr, &mut Vec::new())
    }

    /// the place the bare `name` names, read from the current scope
    fn place_of_name(&self, name: &str) -> Option<StatePlace<'db>> {
        self.name_place(self.scope()?, name, &mut Vec::new())
    }

    /// The place `expr` names when read from `scope`.
    ///
    /// A name bound to another place — `let alias = count`, `let cell =
    /// model.count` — is an alias of it, not an observable of its own: the
    /// runtime sees one cell however many names it goes by, so the alias is
    /// followed to what it was bound to, binding by binding, in the scope each
    /// binding was made in. `visited` guards against two names bound to each
    /// other; such a chain names nothing.
    fn place_in(
        &self,
        scope: FileScopeId,
        expr: &Expr,
        visited: &mut Vec<Definition<'db>>,
    ) -> Option<StatePlace<'db>> {
        match expr {
            Expr::Name(name) => self.name_place(scope, name.id.as_str(), visited),
            Expr::Attribute(attribute) => Some(
                self.place_in(scope, &attribute.value, visited)?
                    .extended(std::slice::from_ref(&attribute.attr.id)),
            ),
            _ => None,
        }
    }

    /// [`Self::place_in`] for the bare `name`.
    fn name_place(
        &self,
        scope: FileScopeId,
        name: &str,
        visited: &mut Vec<Definition<'db>>,
    ) -> Option<StatePlace<'db>> {
        let db = self.db;
        let definition = root_definition(db, self.index, self.module, scope, name)?;
        if let Some(value) = alias_value(db, self.module, definition) {
            if visited.contains(&definition) {
                return None;
            }
            visited.push(definition);
            return self.place_in(definition.file_scope(db), value, visited);
        }
        Some(StatePlace::at_root(self.root_of(definition, name)))
    }

    /// Record a read of the observable `expr` names.
    fn record_read(&mut self, expr: &Expr) {
        if let Some(place) = self.place_of(expr) {
            self.reads.insert(place);
        }
    }

    /// Record `expr` as read when it is a collection observable in a position
    /// that iterates or measures it.
    fn record_collection_read(&mut self, expr: &Expr) {
        if matches!(
            self.observable_of(expr),
            Some(ObservableKind::StateList | ObservableKind::StateDict)
        ) {
            self.record_read(expr);
        }
    }

    /// Record a load of `name` when it is a `context` parameter of the
    /// function: the caller fills it from its own composition, so a use of it
    /// is a dependency on what the caller provides.
    fn record_context_parameter(&mut self, name: &ast::ExprName) {
        if !self.has_context_parameters {
            return;
        }
        let Some(definition) = self.definition_of_name(name.id.as_str()) else {
            return;
        };
        if self
            .parameters
            .get(&definition)
            .is_some_and(|parameter| parameter.is_context)
        {
            self.reads.insert(StatePlace::at_root(
                self.root_of(definition, name.id.as_str()),
            ));
        }
    }

    /// Record a call to `callee`, with the places its arguments name.
    ///
    /// `callee_expr` is the expression called, whose receiver a bound method
    /// reads; `call` is the call node, `None` for a bare block callee (`Row:`),
    /// whose only argument is the block.
    fn record_call(&mut self, callee: Type<'db>, callee_expr: &Expr, call: Option<&ast::ExprCall>) {
        let db = self.db;
        let (function, bound) = match callee {
            Type::FunctionLiteral(function) => (function, false),
            Type::BoundMethod(method) => {
                let function = method.function(db);
                (function, !function.is_staticmethod(db))
            }
            // what a `dynamic` value does when called cannot be seen
            Type::Dynamic(_) => {
                self.opaque = true;
                return;
            }
            _ => return,
        };

        let receiver = match callee_expr {
            Expr::Attribute(attribute) if bound && !function.is_classmethod(db) => {
                self.place_of(&attribute.value)
            }
            _ => None,
        };

        // a bare block callee (`Row:`) is recorded with no call node, and is
        // always the block's call
        let inline = call.is_none_or(|call| self.block_call == Some(call.range()));

        let mut positional = Vec::new();
        let mut keywords = Vec::new();
        let mut unpacked = Unpacked::default();
        if let Some(call) = call {
            for argument in &call.arguments.args {
                if argument.is_starred_expr() {
                    unpacked.positional = true;
                    break;
                }
                positional.push(self.place_of(argument));
            }
            for keyword in &call.arguments.keywords {
                match &keyword.arg {
                    Some(name) => keywords.push((name.id.clone(), self.place_of(&keyword.value))),
                    None => unpacked.keywords = true,
                }
            }
            // a `context` parameter the call leaves unmatched is filled from a
            // name in scope here, exactly as if the call had written it
            for implicit in
                implicit_context_arguments(db, &self.env, self.program_file.file(db), callee, call)
            {
                if implicit.is_block_receiver {
                    continue;
                }
                let place = self.place_of_name(implicit.variable.as_str());
                keywords.push((implicit.parameter, place));
            }
        }

        self.calls.push(CallEffect {
            callee: function.literal(db),
            inline,
            bound,
            receiver,
            positional: positional.into_boxed_slice(),
            keywords: keywords.into_boxed_slice(),
            unpacked,
        });
    }

    /// A call: the reading methods of a collection observable, what a builtin
    /// iterates, and the callee itself.
    fn visit_call(&mut self, call: &ast::ExprCall) {
        if let Expr::Attribute(attribute) = call.func.as_ref() {
            let readers: &[&str] = match self.observable_of(&attribute.value) {
                Some(ObservableKind::StateList) => STATE_LIST_READERS,
                Some(ObservableKind::StateDict) => STATE_DICT_READERS,
                _ => &[],
            };
            if readers.contains(&attribute.attr.as_str()) {
                self.record_read(&attribute.value);
            }
        }

        let Some(callee) = self.type_of(&call.func) else {
            return;
        };
        // `len(todos)`, `list(todos)`, `sum(todos)`: a builtin handed a
        // collection observable reads it — a superset that costs at most a
        // spurious name, never a missed one
        if is_builtin_callee(self.db, callee) {
            for argument in &call.arguments.args {
                self.record_collection_read(argument);
            }
            for keyword in &call.arguments.keywords {
                self.record_collection_read(&keyword.value);
            }
        }
        self.record_call(callee, &call.func, Some(call));
    }

    /// A trailing-lambda block: the call it makes belongs to this scope, and
    /// its body runs while composing only when the callee takes the block as
    /// a `once` or `local` callback.
    fn visit_block(&mut self, block: &ast::StmtFunctionDef) {
        for decorator in &block.decorator_list {
            let expression = match &decorator.expression {
                Expr::Await(await_expr) => await_expr.value.as_ref(),
                expression => expression,
            };
            // a bare `Row:` calls its callee with the block alone; a written
            // call is recorded when its expression is visited, and knows it
            // carries the block from `block_call`
            if !expression.is_call_expr()
                && let Some(callee) = self.type_of(expression)
            {
                self.record_call(callee, expression, None);
            }
            let outer = self.block_call.replace(expression.range());
            self.visit_expr(expression);
            self.block_call = outer;
        }

        let Some(callee) = block_callee(self.db, self.index, block) else {
            return;
        };
        if callee_callback_is_once(self.db, callee.ty)
            || callee_callback_is_borrowed(self.db, callee.ty) == Some(true)
        {
            self.push_scope(self.index.node_scope(NodeWithScopeRef::Function(block)));
            self.visit_body(&block.body);
            self.pop_scope();
        }
    }

    /// A comprehension: its first iterable is evaluated in the enclosing
    /// scope, everything else in a scope of its own — which still runs while
    /// composing.
    fn visit_comprehension(
        &mut self,
        scope: NodeWithScopeRef<'_>,
        elements: &[&Expr],
        generators: &[ast::Comprehension],
    ) {
        let Some((first, rest)) = generators.split_first() else {
            return;
        };
        self.visit_expr(&first.iter);
        self.record_collection_read(&first.iter);

        self.push_scope(self.index.node_scope(scope));
        self.visit_expr(&first.target);
        for condition in &first.ifs {
            self.visit_expr(condition);
        }
        for generator in rest {
            self.visit_expr(&generator.iter);
            self.record_collection_read(&generator.iter);
            self.visit_expr(&generator.target);
            for condition in &generator.ifs {
                self.visit_expr(condition);
            }
        }
        for element in elements {
            self.visit_expr(element);
        }
        self.pop_scope();
    }
}

impl<'ast> Visitor<'ast> for ReadsCollector<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(block) if block.is_trailing_lambda => self.visit_block(block),

            // a nested function runs when something calls it, not here; its
            // decorators and defaults do run here
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                for default in function
                    .parameters
                    .iter_non_variadic_params()
                    .filter_map(|parameter| parameter.default.as_deref())
                {
                    self.visit_expr(default);
                }
            }

            // a class body is a scope of its own, and nothing composes in one
            Stmt::ClassDef(class) => {
                for decorator in &class.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                for base in class.bases() {
                    self.visit_expr(base);
                }
                for keyword in class.keywords() {
                    self.visit_expr(&keyword.value);
                }
            }

            Stmt::For(for_stmt) => {
                self.visit_expr(&for_stmt.iter);
                self.record_collection_read(&for_stmt.iter);
                self.visit_expr(&for_stmt.target);
                self.visit_body(&for_stmt.body);
                self.visit_body(&for_stmt.orelse);
            }

            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            // a lambda body runs when something calls it; its defaults run here
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

            Expr::ListComp(comprehension) => {
                self.visit_comprehension(
                    NodeWithScopeRef::ListComprehension(comprehension),
                    &[&comprehension.elt],
                    &comprehension.generators,
                );
                return;
            }
            Expr::SetComp(comprehension) => {
                self.visit_comprehension(
                    NodeWithScopeRef::SetComprehension(comprehension),
                    &[&comprehension.elt],
                    &comprehension.generators,
                );
                return;
            }
            Expr::DictComp(comprehension) => {
                let elements: Vec<&Expr> = comprehension
                    .key
                    .iter()
                    .map(AsRef::as_ref)
                    .chain([comprehension.value.as_ref()])
                    .collect();
                self.visit_comprehension(
                    NodeWithScopeRef::DictComprehension(comprehension),
                    &elements,
                    &comprehension.generators,
                );
                return;
            }
            Expr::Generator(generator) => {
                self.visit_comprehension(
                    NodeWithScopeRef::GeneratorExpression(generator),
                    &[&generator.elt],
                    &generator.generators,
                );
                return;
            }

            // `count.value`, `theme.current`
            Expr::Attribute(attribute) if attribute.ctx.is_load() => {
                let read = matches!(
                    (
                        self.observable_of(&attribute.value),
                        attribute.attr.as_str(),
                    ),
                    (
                        Some(ObservableKind::State | ObservableKind::Derived),
                        "value"
                    ) | (Some(ObservableKind::Ambient), "current")
                );
                if read {
                    self.record_read(&attribute.value);
                }
            }

            // `todos[0]`, `table["a"]`
            Expr::Subscript(subscript) if subscript.ctx.is_load() => {
                self.record_collection_read(&subscript.value);
            }

            Expr::Call(call) => self.visit_call(call),

            // `key in table`
            Expr::Compare(compare) => {
                for (op, comparator) in compare.ops.iter().zip(&compare.comparators) {
                    if matches!(op, ast::CmpOp::In | ast::CmpOp::NotIn) {
                        self.record_collection_read(comparator);
                    }
                }
            }

            // `f(*todos)`, `(*todos,)`
            Expr::Starred(starred) => self.record_collection_read(&starred.value),

            Expr::Name(name) if name.ctx.is_load() => self.record_context_parameter(name),

            _ => {}
        }

        walk_expr(self, expr);
    }
}

/// The binding a bare `name` resolves to from `scope`: the first binding of
/// the innermost enclosing scope that binds it, following `global` and
/// `nonlocal` declarations. `None` for a name nothing in the file binds — a
/// builtin, or a member a block's receiver supplies.
fn root_definition<'db>(
    db: &'db dyn Db,
    index: &SemanticIndex<'db>,
    module: &ParsedModuleRef,
    scope: FileScopeId,
    name: &str,
) -> Option<Definition<'db>> {
    let first_binding = |scope: FileScopeId| {
        let symbol_id = index.place_table(scope).symbol_id(name)?;
        index
            .use_def_map(scope)
            .reachable_symbol_bindings(symbol_id)
            .filter_map(|binding| binding.binding.definition())
            .min_by_key(|definition| definition.focus_range(db, module).range().start())
    };

    for (scope_id, _) in index.visible_ancestor_scopes(scope) {
        let table = index.place_table(scope_id);
        let Some(symbol_id) = table.symbol_id(name) else {
            continue;
        };
        let symbol = table.symbol(symbol_id);
        if symbol.is_global() {
            return first_binding(FileScopeId::global());
        }
        if symbol.is_nonlocal() || !symbol.is_bound() {
            continue;
        }
        if let Some(definition) = first_binding(scope_id) {
            return Some(definition);
        }
    }
    None
}

/// The place a binding stands for when it binds its name to another place
/// rather than to a value of its own: the `count` of `let alias = count`, the
/// `model.count` of `let cell = model.count`. `None` for every other binding
/// — a call's result, a parameter, a loop target, an unpacking.
///
/// The expression is to be read in the scope `definition` is bound in.
fn alias_value<'ast, 'db>(
    db: &'db dyn Db,
    module: &'ast ParsedModuleRef,
    definition: Definition<'db>,
) -> Option<&'ast Expr> {
    let value = match definition.kind(db) {
        DefinitionKind::Assignment(assignment) => {
            // `a, b = pair` binds each name to an element the statement never
            // spells on its own
            if assignment.unpack().is_some() {
                return None;
            }
            assignment.value(module)
        }
        DefinitionKind::AnnotatedAssignment(assignment) => assignment.value(module)?,
        _ => return None,
    };
    matches!(value, Expr::Name(_) | Expr::Attribute(_)).then_some(value)
}

/// Whether `callee` is a function or class of the `builtins` module —
/// something that iterates or measures whatever collection it is handed.
fn is_builtin_callee<'db>(db: &'db dyn Db, callee: Type<'db>) -> bool {
    let program_file = match callee {
        Type::FunctionLiteral(function) => function.program_file(db),
        Type::ClassLiteral(class) => class.program_file(db),
        _ => return false,
    };
    file_to_module(db, program_file.resolver_file(db)).and_then(|module| module.known(db))
        == Some(KnownModule::Builtins)
}
