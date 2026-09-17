//! which operations can run python code
//!
//! a guard answers a question about state only python code can change: whether `len`
//! still names the builtin, whether a published function still stands. a pass that
//! wants to ask such a question once rather than on every trip round a loop has to
//! know that nothing it skips over can have changed the answer — and that is a question
//! about *every* operation on the path, not about the guard.
//!
//! python code runs in more places than a call. a release can reach a `__del__`; an
//! allocation of an object the collector tracks can start a collection, which runs
//! finalizers; a comparison of two objects reaches `__eq__`; a dict lookup hashes and
//! compares keys; an attribute read on a class something can extend reaches whatever
//! the extension put there. so this is written the conservative way round: an operation
//! runs python unless it is one of the few this file has read the emitted C for and
//! found cannot, and a call runs python unless the callee's whole body is proven not to.
//!
//! most of those few depend on what the operands hold at that point. comparing two
//! tagged integers is a machine comparison while both are exact `int`s and a subclass's
//! `__lt__` otherwise, and overwriting a register is free while what it held was an exact
//! `int` and a finalizer otherwise. [`Held`] is what the function knows about that, at
//! each point

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use by_ir::function::{BasicBlock, Function, ModuleIr, qualify};
use by_ir::ops::{BlockId, Op, RegisterId, Terminator, UnaryOp, Value};
use by_ir::rtype::{Primitive, RType};

/// which of a module's functions a native call can reach python code through
pub(crate) struct Effects {
    summaries: Arc<Summaries>,
}

/// a question a guarded call asks before it goes native: whether a builtin still resolves
/// to itself, or whether a published module function still stands for its native entry
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Guard {
    Builtin(String),
    Function(String),
}

/// what a native call to each function does, as far as it has been proven
#[derive(Debug, Default)]
pub(crate) struct Summaries {
    /// the functions a call cannot run python code through, whatever it is handed: every
    /// operation in the body cannot, every call the body makes cannot, and nothing the frame
    /// lets go of on its way out can
    clear: HashSet<String>,
    /// the functions a call cannot run python code through where every `int` and `str` it is
    /// handed is exact — which is most arithmetic, since a subclass's own operators are the
    /// python code the body would otherwise reach
    clear_when_exact: HashSet<String>,
    /// the functions whose answer is an exact value where every `int` and `str` handed to
    /// them is
    exact_when_exact: HashSet<String>,
    /// the functions a call cannot run python code through where its arguments are exact and
    /// every guard in [`Self::assumes`] stands — so each guarded call in the body goes native
    ///
    /// this is what makes a recursion clear: its own call to itself is guarded, and the arm
    /// that guard turns away calls whatever the name holds, which may be anything
    clear_when_standing: HashSet<String>,
    /// the functions whose answer is exact under the same conditions
    exact_when_standing: HashSet<String>,
    /// the guards a function's body, and the body of every function it calls natively, asks
    assumes: HashMap<String, BTreeSet<Guard>>,
}

impl Effects {
    /// a fixed point over the native call graph, from the optimistic side: every function
    /// starts clear, and one whose own body or whose callee is not loses it. a cycle whose
    /// bodies are all clear stays clear, which is right — going round it runs nothing but
    /// the bodies, and a depth count running out raises without running anything
    pub(crate) fn of(module: &ModuleIr) -> Self {
        let names: HashSet<String> = module
            .all_functions()
            .map(Function::qualified_name)
            .collect();
        let assumes = assumed_guards(module);
        let mut summaries = Arc::new(Summaries {
            clear: names.clone(),
            clear_when_exact: names.clone(),
            exact_when_exact: names.clone(),
            clear_when_standing: names.clone(),
            exact_when_standing: names,
            assumes: assumes.clone(),
        });
        loop {
            let mut next = Summaries {
                assumes: assumes.clone(),
                ..Summaries::default()
            };
            for function in module.all_functions() {
                let name = function.qualified_name();
                let none = BTreeSet::new();
                let (runs, _) = body_effects(function, &summaries, false, &none);
                if !runs {
                    next.clear.insert(name.clone());
                }
                let (runs, exact_answer) = body_effects(function, &summaries, true, &none);
                if !runs {
                    next.clear_when_exact.insert(name.clone());
                }
                if exact_answer {
                    next.exact_when_exact.insert(name.clone());
                }
                let standing = assumes.get(&name).cloned().unwrap_or_default();
                let mut answered = function.clone();
                for block in &mut answered.blocks {
                    answer_yes(block, &standing);
                }
                let (runs, exact_answer) = body_effects(&answered, &summaries, true, &standing);
                if !runs {
                    next.clear_when_standing.insert(name.clone());
                }
                if exact_answer {
                    next.exact_when_standing.insert(name);
                }
            }
            // the sets only ever shrink from the full ones they start at, so they settle
            let settled = next.clear == summaries.clear
                && next.clear_when_exact == summaries.clear_when_exact
                && next.exact_when_exact == summaries.exact_when_exact
                && next.clear_when_standing == summaries.clear_when_standing
                && next.exact_when_standing == summaries.exact_when_standing;
            summaries = Arc::new(next);
            if settled {
                return Self { summaries };
            }
        }
    }

    /// what `function` knows on entry, with what its native calls answer taken from here
    pub(crate) fn held_at_entry(&self, function: &Function) -> Held {
        Held::at_entry_with(function, Some(Arc::clone(&self.summaries)))
    }

    /// whether running `op` in `function` can run python code, where `held` is what the
    /// function knows just before it
    pub(crate) fn op_runs_python(&self, function: &Function, op: &Op, held: &Held) -> bool {
        call_runs_python(&self.summaries, function, op, held)
            .unwrap_or_else(|| op_itself_runs_python(function, op, held))
    }

    /// whether a native call to the function named `qualified` can run python code
    #[cfg(test)]
    fn call_runs_python(&self, qualified: &str) -> bool {
        !self.summaries.clear.contains(qualified)
    }
}

/// the guards each function asks, together with those of every function it calls natively
fn assumed_guards(module: &ModuleIr) -> HashMap<String, BTreeSet<Guard>> {
    let mut assumes: HashMap<String, BTreeSet<Guard>> = HashMap::new();
    let mut calls: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for function in module.all_functions() {
        let name = function.qualified_name();
        let own = assumes.entry(name.clone()).or_default();
        let called = calls.entry(name).or_default();
        for op in function.blocks.iter().flat_map(|block| &block.ops) {
            match op {
                Op::BuiltinStands { name, .. } => {
                    own.insert(Guard::Builtin(name.clone()));
                }
                Op::ResolveFunction { name, .. } => {
                    own.insert(Guard::Function(name.clone()));
                }
                Op::CallNative { owner, callee, .. } => {
                    called.push(qualify(owner.as_deref(), callee));
                }
                _ => {}
            }
        }
    }
    loop {
        let mut changed = false;
        for (name, called) in &calls {
            let gathered: Vec<Guard> = called
                .iter()
                .filter_map(|callee| assumes.get(callee))
                .flatten()
                .cloned()
                .collect();
            if let Some(own) = assumes.get_mut(name) {
                for guard in gathered {
                    changed |= own.insert(guard);
                }
            }
        }
        if !changed {
            return assumes;
        }
    }
}

/// answer each guard of `standing` in `block` with yes, and take the arm a yes takes,
/// handing back where each answered question was
pub(crate) fn answer_yes(block: &mut BasicBlock, standing: &BTreeSet<Guard>) -> Vec<usize> {
    let mut answered = Vec::new();
    let mut replaced = Vec::new();
    for (index, op) in block.ops.iter_mut().enumerate() {
        match op {
            Op::BuiltinStands { dest, name }
                if standing.contains(&Guard::Builtin(name.clone())) =>
            {
                replaced.push(index);
                answered.push(*dest);
                *op = Op::Assign {
                    dest: *dest,
                    src: Value::Bit(true),
                };
            }
            Op::ResolveFunction { dest, name }
                if standing.contains(&Guard::Function(name.clone())) =>
            {
                *op = Op::FunctionStood {
                    dest: *dest,
                    name: name.clone(),
                };
            }
            Op::FunctionStands { dest, name, .. }
                if standing.contains(&Guard::Function(name.clone())) =>
            {
                replaced.push(index);
                answered.push(*dest);
                *op = Op::Assign {
                    dest: *dest,
                    src: Value::Bit(true),
                };
            }
            _ => {}
        }
    }
    if let Terminator::Branch {
        cond: Value::Register(cond),
        then_block,
        ..
    } = block.terminator
        && answered.contains(&cond)
    {
        block.terminator = Terminator::Goto(then_block);
    }
    replaced
}

/// whether a native call can run python code, or `None` for an operation that is not one
fn call_runs_python(
    summaries: &Summaries,
    function: &Function,
    op: &Op,
    held: &Held,
) -> Option<bool> {
    let Op::CallNative {
        owner,
        callee,
        args,
        ..
    } = op
    else {
        return None;
    };
    let callee = qualify(owner.as_deref(), callee);
    let exact = held.all_exact(function, args);
    let clear = summaries.clear.contains(&callee)
        || (exact && summaries.clear_when_exact.contains(&callee))
        || (exact
            && summaries.clear_when_standing.contains(&callee)
            && held.stands_for(summaries, &callee));
    Some(!clear || overwrites_something_live(function, op, held))
}

/// whether anything in `function`'s body can run python code, and whether every answer it
/// returns is exact, with the parameters taken to be exact where `exact_parameters`
fn body_effects(
    function: &Function,
    summaries: &Arc<Summaries>,
    exact_parameters: bool,
    standing: &BTreeSet<Guard>,
) -> (bool, bool) {
    let mut start = Held::at_entry_with(function, Some(Arc::clone(summaries)));
    start.assume_standing(standing);
    if exact_parameters {
        for (index, decl) in function.params().iter().enumerate() {
            match &decl.ty {
                ty if *ty == RType::INT => start.assume_exact(RegisterId(index), Kind::Int),
                ty if *ty == RType::STR => start.assume_exact(RegisterId(index), Kind::Str),
                _ => {}
            }
        }
    }
    let entries = Held::solve(function, Function::entry(), start, |_| true);
    let mut runs = false;
    let mut exact_answer = true;
    // a failing operation leaves through the function's own exit from wherever it is, and
    // that exit lets go of every register the frame owns. what is known at every point of
    // the function at once is what holds wherever that happens
    let mut everywhere: Option<Held> = None;
    for (index, block) in function.blocks.iter().enumerate() {
        let Some(mut held) = entries.get(&BlockId(index)).cloned() else {
            continue;
        };
        everywhere = Some(everywhere.map_or_else(|| held.clone(), |known| known.meet(&held)));
        for op in &block.ops {
            runs |= call_runs_python(summaries, function, op, &held)
                .unwrap_or_else(|| op_itself_runs_python(function, op, &held));
            held.step(function, op);
            everywhere = everywhere.map(|known| known.meet(&held));
        }
        held.step_terminator(&block.terminator);
        if let Terminator::Return(value) = &block.terminator {
            runs |= !held.lets_go_of_nothing_live(function);
            exact_answer &= held.exact_kind(function, value).is_some();
        }
    }
    if let Some(everywhere) = everywhere {
        runs |= !everywhere.lets_go_of_nothing_live(function);
    }
    (runs, exact_answer)
}

/// the builtin type an exact value is an instance of, exactly and not through a subclass
///
/// each has operations the interpreter answers without asking a method a subclass could
/// have written, and all but a list have no finalizer and hold no other object, so letting
/// go of one runs nothing
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Int,
    Float,
    Str,
    Bytes,
    /// `None`, `True` or `False`, which are never let go of for good
    Singleton,
    /// a `list`, whose length and elements are read without asking it anything — but
    /// whose elements are let go of with it
    List,
}

impl Kind {
    /// whether letting go of an exact value of this kind runs nothing
    const fn has_no_finalizer(self) -> bool {
        !matches!(self, Self::List)
    }
}

/// what one register is known to hold, at one point
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// nothing: the frame has not written it, or has let go of what it held
    Empty,
    /// an exact value of this kind
    Exact(Kind),
    /// what a parameter the frame never writes holds, which the caller holds a reference
    /// to for the whole call
    Pinned,
    /// an element of a `list` a parameter the frame never writes holds, read since the
    /// last operation that could have changed what the list holds
    Element,
    /// one of the four above, where letting go of the value runs nothing, depending on the
    /// path that got here
    Inert,
}

/// what a function knows about the registers it holds, at one point of it
///
/// a register the frame has not written holds nothing, and letting go of nothing runs
/// nothing. one written from an operation that builds an exact value holds that, until it
/// is written again. and one that copies a parameter the frame never writes holds an object
/// the caller holds a reference to for the whole call, so letting go of the copy cannot be
/// what frees it. a register this knows nothing about may hold anything
#[derive(Clone, Debug)]
pub(crate) struct Held {
    states: HashMap<RegisterId, State>,
    /// the parameters the frame never writes
    unwritten: HashSet<RegisterId>,
    /// what the native calls the function makes answer with, where it is known
    summaries: Option<Arc<Summaries>>,
    /// the guards known to stand here, because the region was entered through a test of them
    /// and nothing since can have run python
    standing: Arc<BTreeSet<Guard>>,
}

impl PartialEq for Held {
    fn eq(&self, other: &Self) -> bool {
        self.states == other.states && self.unwritten == other.unwritten
    }
}

impl Held {
    /// on entry to `function`: every register but the parameters holds nothing
    #[cfg(test)]
    fn at_entry(function: &Function) -> Self {
        Self::at_entry_with(function, None)
    }

    fn at_entry_with(function: &Function, summaries: Option<Arc<Summaries>>) -> Self {
        let mut unwritten: HashSet<RegisterId> =
            (0..function.param_count).map(RegisterId).collect();
        for block in &function.blocks {
            for op in &block.ops {
                for register in op
                    .dest()
                    .into_iter()
                    .chain(op.unbinds())
                    .chain(op.loop_cursor())
                {
                    unwritten.remove(&register);
                }
                if let Op::Move {
                    src: Value::Register(register),
                    ..
                } = op
                {
                    unwritten.remove(register);
                }
            }
            if let Some(register) = block.terminator.dest() {
                unwritten.remove(&register);
            }
        }
        Self {
            states: (function.param_count..function.registers.len())
                .map(|index| (RegisterId(index), State::Empty))
                .collect(),
            unwritten,
            summaries,
            standing: Arc::default(),
        }
    }

    /// take every guard of `standing` to stand
    pub(crate) fn assume_standing(&mut self, standing: &BTreeSet<Guard>) {
        let mut all = (*self.standing).clone();
        all.extend(standing.iter().cloned());
        self.standing = Arc::new(all);
    }

    /// whether every guard a native call to `callee` assumes is known to stand
    fn stands_for(&self, summaries: &Summaries, callee: &str) -> bool {
        summaries
            .assumes
            .get(callee)
            .is_none_or(|assumed| assumed.is_subset(&self.standing))
    }

    /// whether every `int` and `str` among `values` is exact
    fn all_exact(&self, function: &Function, values: &[Value]) -> bool {
        values.iter().all(|value| {
            let wanted = match function.value_type(value) {
                Some(ty) if ty == RType::INT => Kind::Int,
                Some(ty) if ty == RType::STR => Kind::Str,
                _ => return true,
            };
            self.exact_kind(function, value) == Some(wanted)
        })
    }

    /// take `register` to hold an exact `kind`, because something tested it and nothing
    /// has written it since
    pub(crate) fn assume_exact(&mut self, register: RegisterId, kind: Kind) {
        self.states.insert(register, State::Exact(kind));
    }

    /// the kind of exact value `value` is, where it is one
    ///
    /// a literal is built by the module as the builtin it spells
    fn exact_kind(&self, function: &Function, value: &Value) -> Option<Kind> {
        match value {
            Value::Register(register) => match self.states.get(register) {
                Some(State::Exact(kind)) => Some(*kind),
                _ => function
                    .register(*register)
                    .and_then(|decl| no_reference_kind(&decl.ty)),
            },
            Value::Int(_) | Value::Fixed(_) => Some(Kind::Int),
            Value::Float(_) => Some(Kind::Float),
            Value::Bool(_) | Value::Bit(_) | Value::None => Some(Kind::Singleton),
            Value::Str(_) => Some(Kind::Str),
            Value::Bytes(_) => Some(Kind::Bytes),
        }
    }

    /// whether letting go of what `register` holds here can run nothing
    fn release_is_inert(&self, function: &Function, register: RegisterId) -> bool {
        function
            .register(register)
            .is_some_and(|decl| holds_no_reference(&decl.ty))
            || self.unwritten.contains(&register)
            || match self.states.get(&register) {
                Some(State::Exact(kind)) => kind.has_no_finalizer(),
                Some(State::Empty | State::Pinned | State::Element | State::Inert) => true,
                None => false,
            }
    }

    /// whether an exit here, which lets go of every register the frame owns, runs nothing
    fn lets_go_of_nothing_live(&self, function: &Function) -> bool {
        (function.param_count..function.registers.len())
            .map(RegisterId)
            .all(|register| self.release_is_inert(function, register))
    }

    /// what is known after `op` has run
    pub(crate) fn step(&mut self, function: &Function, op: &Op) {
        // a list changes only through python code or a call, and an element read before is
        // then no longer known to be held by it
        let lists_may_change =
            matches!(op, Op::CallNative { .. }) || op_itself_runs_python(function, op, self);
        let produced = op.dest().map(|dest| {
            let state = match self.produced(function, op, dest) {
                Some(kind) => Some(State::Exact(kind)),
                None => match op {
                    Op::GetItem {
                        container: Value::Register(container),
                        index,
                        ..
                    } if self.unwritten.contains(container)
                        && self.states.get(container) == Some(&State::Exact(Kind::List))
                        && self.exact_kind(function, index) == Some(Kind::Int) =>
                    {
                        Some(State::Element)
                    }
                    Op::Assign {
                        src: Value::Register(src),
                        ..
                    }
                    | Op::Move {
                        src: Value::Register(src),
                        ..
                    } if self.states.get(src) == Some(&State::Pinned)
                        || self.unwritten.contains(src) =>
                    {
                        Some(State::Pinned)
                    }
                    _ => None,
                },
            };
            (dest, state)
        });
        if lists_may_change {
            self.states.retain(|_, state| *state != State::Element);
        }
        match op {
            Op::Move {
                src: Value::Register(src),
                path,
                ..
            }
            | Op::Release {
                value: Value::Register(src),
                path,
            } if path.is_empty() => {
                self.states.insert(*src, State::Empty);
            }
            _ => {}
        }
        if let Some(register) = op.unbinds() {
            self.states.insert(register, State::Empty);
        }
        if let Some((dest, state)) = produced {
            match state {
                Some(state) => self.states.insert(dest, state),
                None => self.states.remove(&dest),
            };
        }
        // a function known to stand resolves to nothing to call
        match op {
            Op::ResolveFunction { dest, name }
                if self.standing.contains(&Guard::Function(name.clone())) =>
            {
                self.states.insert(*dest, State::Empty);
            }
            Op::FunctionStood { dest, .. } => {
                self.states.insert(*dest, State::Empty);
            }
            _ => {}
        }
    }

    /// what is known after `terminator` has run
    pub(crate) fn step_terminator(&mut self, terminator: &Terminator) {
        if let Some(dest) = terminator.dest() {
            self.states.remove(&dest);
        }
    }

    /// what is known on a path that could have come from either
    pub(crate) fn meet(&self, other: &Self) -> Self {
        Self {
            states: self
                .states
                .iter()
                .filter_map(|(register, state)| {
                    let theirs = other.states.get(register)?;
                    let inert = |state: &State| {
                        !matches!(state, State::Exact(kind) if !kind.has_no_finalizer())
                    };
                    if theirs == state {
                        Some((*register, *state))
                    } else if inert(state) && inert(theirs) {
                        Some((*register, State::Inert))
                    } else {
                        None
                    }
                })
                .collect(),
            unwritten: self.unwritten.clone(),
            summaries: self.summaries.clone(),
            standing: Arc::clone(&self.standing),
        }
    }

    /// what is known at the entry to each block reached from `start`, where `held` is what
    /// is known there and only blocks `within` says are part of the region are followed
    ///
    /// a handler is entered from the middle of the blocks it covers, so it is given only
    /// what held at every point of each of them
    pub(crate) fn solve(
        function: &Function,
        start: BlockId,
        held: Self,
        within: impl Fn(BlockId) -> bool,
    ) -> HashMap<BlockId, Self> {
        let mut entries: HashMap<BlockId, Self> = HashMap::new();
        entries.insert(start, held);
        let mut pending = vec![start];
        while let Some(id) = pending.pop() {
            let (Some(block), Some(entry)) = (function.block(id), entries.get(&id).cloned()) else {
                continue;
            };
            let mut known = entry.clone();
            let mut throughout = entry;
            for op in &block.ops {
                known.step(function, op);
                throughout = throughout.meet(&known);
            }
            known.step_terminator(&block.terminator);
            // the exception edge is the last of the successors, and it carries only what
            // held all through the block
            let mut normal = block.successors();
            if block.error_target.is_some() {
                normal.pop();
            }
            let mut flows: Vec<(BlockId, Self)> = normal
                .into_iter()
                .map(|successor| (successor, known.clone()))
                .collect();
            if let Some(handler) = block.error_target {
                flows.push((handler, throughout));
            }
            for (successor, facts) in flows {
                if !within(successor) {
                    continue;
                }
                let merged = match entries.get(&successor) {
                    None => facts,
                    Some(existing) => existing.meet(&facts),
                };
                if entries.get(&successor) != Some(&merged) {
                    entries.insert(successor, merged);
                    pending.push(successor);
                }
            }
        }
        entries
    }

    /// the kind of exact value `op` leaves in `dest`, where it leaves one
    fn produced(&self, function: &Function, op: &Op, dest: RegisterId) -> Option<Kind> {
        let ty = function.register(dest).map(|decl| &decl.ty)?;
        if let Some(kind) = no_reference_kind(ty) {
            return Some(kind);
        }
        let exact = |value: &Value| self.exact_kind(function, value);
        match op {
            // an answer a callee is proven to make exact, from arguments that are
            Op::CallNative {
                owner,
                callee,
                args,
                ..
            } => {
                let summaries = self.summaries.as_ref()?;
                let callee = qualify(owner.as_deref(), callee);
                (self.all_exact(function, args)
                    && (summaries.exact_when_exact.contains(&callee)
                        || (summaries.exact_when_standing.contains(&callee)
                            && self.stands_for(summaries, &callee))))
                .then(|| exact_kind_of(ty))
                .flatten()
            }
            Op::Assign { src, .. } => exact(src),
            Op::Move { src, path, .. } if path.is_empty() => exact(src),
            // the interpreter's own arithmetic on two exact `int`s answers an exact `int`, on
            // the slow path too
            Op::IntBinary { lhs, rhs, .. }
                if exact(lhs) == Some(Kind::Int) && exact(rhs) == Some(Kind::Int) =>
            {
                Some(Kind::Int)
            }
            Op::Unary {
                op: UnaryOp::Neg | UnaryOp::Invert,
                operand,
                ..
            } if exact(operand) == Some(Kind::Int) && *ty == RType::INT => Some(Kind::Int),
            // a machine value boxed is built as the builtin, and an exact value boxed is
            // itself
            Op::TagShort { .. } => Some(Kind::Int),
            Op::Box { src, .. } => match function.value_type(src)? {
                RType::Primitive(Primitive::Fixed(_)) => Some(Kind::Int),
                RType::Primitive(Primitive::Float) => Some(Kind::Float),
                RType::Primitive(Primitive::Bool | Primitive::Bit | Primitive::None) => {
                    Some(Kind::Singleton)
                }
                _ => exact(src),
            },
            // a length is a machine integer before it is tagged, whatever `__len__` it came
            // from
            Op::Len { .. } | Op::ArrayLen { .. } if *ty == RType::INT => Some(Kind::Int),
            // a character of an exact `str` is an exact `str`
            Op::StrGetItem {
                container, index, ..
            } if exact(container) == Some(Kind::Str) && exact(index) == Some(Kind::Int) => {
                Some(Kind::Str)
            }
            _ => None,
        }
    }
}

/// the kind an exact value of this representation is
fn exact_kind_of(ty: &RType) -> Option<Kind> {
    match ty {
        RType::Primitive(Primitive::Int) => Some(Kind::Int),
        RType::Primitive(Primitive::Str) => Some(Kind::Str),
        other => no_reference_kind(other),
    }
}

/// the kind a value of this representation is when it holds no object at all
fn no_reference_kind(ty: &RType) -> Option<Kind> {
    match ty {
        RType::Primitive(Primitive::Fixed(_)) => Some(Kind::Int),
        RType::Primitive(Primitive::Float) => Some(Kind::Float),
        RType::Primitive(Primitive::Bool | Primitive::Bit | Primitive::None) => {
            Some(Kind::Singleton)
        }
        _ => None,
    }
}

/// whether a value of this representation holds no reference to any object, so that
/// overwriting or letting go of one runs nothing
fn holds_no_reference(ty: &RType) -> bool {
    match ty {
        RType::Primitive(primitive) => matches!(
            primitive,
            Primitive::Fixed(_)
                | Primitive::Float
                | Primitive::Bool
                | Primitive::Bit
                | Primitive::None
        ),
        // the buffer is freed with the elements it holds, which are only safe to free when
        // they are not objects themselves
        RType::Array(element) => holds_no_reference(element),
        RType::Tuple(items) => items.iter().all(holds_no_reference),
        RType::Instance { .. } => false,
    }
}

/// whether `op` lets go of something that may be the last reference to an object with a
/// finalizer: the value its destination held, or the local it unbinds
fn overwrites_something_live(function: &Function, op: &Op, held: &Held) -> bool {
    op.dest()
        .into_iter()
        .chain(op.unbinds())
        .any(|register| !held.release_is_inert(function, register))
}

/// whether `op` can run python code, leaving a native call's callee aside
fn op_itself_runs_python(function: &Function, op: &Op, held: &Held) -> bool {
    if overwrites_something_live(function, op, held) {
        return true;
    }
    let kind = |value: &Value| held.exact_kind(function, value);
    match op {
        // copies and loads of what the frame already holds, comparisons and arithmetic on
        // machine values, and questions answered by reading a pointer or a flag
        Op::Assign { .. }
        | Op::Move { .. }
        | Op::FloatBinary { .. }
        | Op::FloatCompare { .. }
        | Op::IsNull { .. }
        | Op::StopIterationValue { .. }
        | Op::Line { .. }
        | Op::IsMissing { .. }
        | Op::Identity { .. }
        | Op::IsSequence { .. }
        | Op::IsMapping { .. }
        | Op::HoldsLayout { .. }
        | Op::LoadEllipsis { .. }
        | Op::ModuleDict { .. }
        | Op::LoadClass { .. }
        | Op::FunctionStands { .. }
        | Op::FunctionStood { .. }
        | Op::FunctionCallee { .. }
        | Op::TupleGet { .. }
        | Op::TupleBuild { .. }
        | Op::PushHandled { .. }
        | Op::DeleteLocal { .. } => false,

        // a field is a load at an offset, and whether it is set is a load and a compare;
        // raising `AttributeError` or `UnboundLocalError` for a missing one builds an
        // exception of the interpreter's own class
        Op::GetField { .. }
        | Op::RequireField { .. }
        | Op::FieldIsSet { .. }
        | Op::GetCell { .. } => false,

        // `PyLong_AsDouble` reads the digits of any `int`, a subclass's included, and asks
        // it nothing
        Op::IntToFloat { .. } => false,
        // a short is a word with its tag, and building one allocates nothing
        Op::TagShort { .. } => false,

        // a buffer of the compiler's own, grown with the allocator and not the collector
        Op::ArrayNew { .. }
        | Op::ArrayGet { .. }
        | Op::ArrayLen { .. }
        | Op::ArrayRead { .. }
        | Op::ArrayPush { .. }
        | Op::ArrayStoreLength { .. } => false,
        // a store lets go of the element it replaces
        Op::ArraySet { array, .. } => !function
            .value_type(array)
            .is_some_and(|ty| holds_no_reference(&ty)),

        // a narrowing is a type test, and the failure it raises names the type it found
        Op::Unbox { to, .. } => !matches!(
            to,
            RType::Primitive(
                Primitive::Int
                    | Primitive::Float
                    | Primitive::Bool
                    | Primitive::None
                    | Primitive::Str
                    | Primitive::List
            ) | RType::Instance { .. }
        ),
        // boxing a machine value builds an `int` or a `float`, neither of which the collector
        // tracks, or answers with a singleton; boxing an object is a new reference
        Op::Box { src, .. } => !matches!(
            function.value_type(src),
            Some(RType::Primitive(_) | RType::Instance { .. })
        ),

        // the interpreter's own arithmetic and comparison, while both sides are exact `int`s
        Op::IntBinary { lhs, rhs, .. } | Op::IntCompare { lhs, rhs, .. } => {
            !(kind(lhs) == Some(Kind::Int) && kind(rhs) == Some(Kind::Int))
        }
        Op::StrCompare { lhs, rhs, .. } => {
            !(kind(lhs) == Some(Kind::Str) && kind(rhs) == Some(Kind::Str))
        }
        Op::Unary { op, operand, .. } => match op {
            UnaryOp::Not => false,
            UnaryOp::Neg | UnaryOp::Invert => {
                !matches!(kind(operand), Some(Kind::Int | Kind::Float))
            }
        },
        // an exact `str`, `bytes` or `list` knows its own size
        Op::Len { src, .. } => !matches!(kind(src), Some(Kind::Str | Kind::Bytes | Kind::List)),
        // an element of an exact `list` at an index that is an exact `int`, which raises
        // `IndexError` of the interpreter's own out of range
        Op::GetItem {
            container, index, ..
        } if kind(container) == Some(Kind::List) && kind(index) == Some(Kind::Int) => false,
        // a character of an exact `str`, at an index that is an exact `int`
        Op::StrGetItem {
            container, index, ..
        }
        | Op::StrItemCompare {
            container, index, ..
        } => !(kind(container) == Some(Kind::Str) && kind(index) == Some(Kind::Int)),
        // a release frees the object only where nothing else is known to hold it and it
        // may have a finalizer or hold other objects
        Op::Release { value, path } => match value {
            Value::Register(register) => {
                !path.is_empty() || !held.release_is_inert(function, *register)
            }
            _ => false,
        },

        // the callee decides, and only [`Effects`] knows the callee
        Op::CallNative { .. } => true,

        // a store lets go of the value it replaces, and a published `__dict__` holds
        // whatever python last wrote into it
        Op::SetField { .. } => true,

        // allocations the collector tracks, which can start a collection
        Op::NewInstance { .. }
        | Op::MakeClosure { .. }
        | Op::BuildList { .. }
        | Op::BuildSet { .. }
        | Op::BuildTuple { .. }
        | Op::BuildDict { .. }
        | Op::MakeSlice { .. }
        | Op::ToTuple { .. }
        | Op::RaiseStandard { .. }
        | Op::RaiseWith { .. } => true,

        // a function known to stand resolves to nothing to call, from a flag
        Op::ResolveFunction { name, .. } => !held.standing.contains(&Guard::Function(name.clone())),

        // a namespace lookup compares keys, and a store or a delete lets go of a value
        Op::LoadGlobal { .. }
        | Op::StoreGlobal { .. }
        | Op::DeleteGlobal { .. }
        | Op::BuiltinStands { .. }
        | Op::LoopGuardsHold { .. } => true,

        // a licence arms itself with a lookup through the type's dicts, and an instance
        // dict can hold a key of any type
        Op::MethodStands { .. }
        | Op::AccessorStands { .. }
        | Op::FieldStands { .. }
        | Op::DictShadows { .. }
        | Op::LicenceHolds { .. } => true,

        // the object protocol, which is python code wherever a type says so
        Op::ObjectBinary { .. }
        | Op::ObjectCompare { .. }
        | Op::ObjectRichCompare { .. }
        | Op::Truthy { .. }
        | Op::FloatObjectBinary { .. }
        | Op::FloatObjectCompare { .. }
        | Op::IsInstance { .. }
        | Op::CheckSound { .. }
        | Op::MatchAttr { .. }
        | Op::MatchSlice { .. }
        | Op::MatchKey { .. }
        | Op::MatchRest { .. }
        | Op::Contains { .. }
        | Op::GetAttr { .. }
        | Op::SetAttr { .. }
        | Op::ReadAttribute { .. }
        | Op::WriteAttribute { .. }
        | Op::DeleteAttr { .. }
        | Op::GetItem { .. }
        | Op::SetItem { .. }
        | Op::DeleteItem { .. }
        | Op::DictFind { .. }
        | Op::Format { .. }
        | Op::StrConcat { .. }
        | Op::StrOfInt { .. }
        | Op::StrConcatInt { .. }
        | Op::Unpack { .. }
        | Op::Extend { .. }
        | Op::MergeKeywords { .. }
        | Op::GetIter { .. }
        | Op::IterNext { .. } => true,

        // calls out of the unit, and the protocols built on them
        Op::CallPython { .. }
        | Op::CallValue { .. }
        | Op::CallThrough { .. }
        | Op::CallMethod { .. }
        | Op::CallUnpacked { .. }
        | Op::ImportModule { .. }
        | Op::ImportFrom { .. }
        | Op::Enter { .. }
        | Op::BindExit { .. }
        | Op::ExitContext { .. }
        | Op::DelegateIter { .. }
        | Op::DelegateStep { .. }
        | Op::AsyncContext { .. }
        | Op::AsyncIter { .. }
        | Op::Warn { .. } => true,

        // an exception is normalised by calling its class, handled ones are let go of, and
        // leaving a frame releases what it holds
        Op::FetchException { .. }
        | Op::ExceptionMatches { .. }
        | Op::PopHandled { .. }
        | Op::RaiseObject { .. }
        | Op::Reraise { .. }
        | Op::LeaveGenerator { .. }
        | Op::FinishFrame { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::{ClassIr, FieldDecl, Function, ModuleIr};
    use by_ir::ops::{BinOp, BlockId, CmpOp, Op, RegisterId, Terminator, Value};
    use by_ir::rtype::{IntWidth, RType};

    use std::collections::BTreeSet;

    use super::{Effects, Guard, Held, Kind};

    fn module(functions: Vec<Function>) -> ModuleIr {
        let mut module = ModuleIr::new("app");
        module.functions = functions;
        module
    }

    /// every operation of the function named `name` that a pass asking about each of them
    /// would find can run python code, as its debug rendering
    fn running(module: &ModuleIr, name: &str) -> Vec<String> {
        let effects = Effects::of(module);
        let mut out = Vec::new();
        for function in module
            .all_functions()
            .filter(|function| function.name == name)
        {
            let entries = Held::solve(
                function,
                Function::entry(),
                effects.held_at_entry(function),
                |_| true,
            );
            for (index, block) in function.blocks.iter().enumerate() {
                let Some(mut held) = entries.get(&BlockId(index)).cloned() else {
                    continue;
                };
                for op in &block.ops {
                    if effects.op_runs_python(function, op, &held) {
                        out.push(format!("{op:?}"));
                    }
                    held.step(function, op);
                }
            }
        }
        out
    }

    /// `def scale(x: float) -> float: return x * 2.0`
    fn float_body(name: &str) -> Function {
        let mut builder = FunctionBuilder::new(name, RType::FLOAT);
        let x = builder.param("x", RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::FloatBinary {
            dest: out,
            op: BinOp::Mul,
            lhs: Value::Register(x),
            rhs: Value::Float(2.0),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        builder.finish()
    }

    fn call(name: &str, owner: Option<&str>, callee: &str) -> Function {
        let mut builder = FunctionBuilder::new(name, RType::FLOAT);
        let x = builder.param("x", RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::CallNative {
            dest: Some(out),
            owner: owner.map(str::to_string),
            callee: callee.to_string(),
            args: vec![Value::Register(x)],
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        builder.finish()
    }

    fn class(name: &str, methods: Vec<Function>) -> ClassIr {
        ClassIr {
            name: name.to_string(),
            immutable: false,
            environment: false,
            exported: true,
            base: None,
            inherited_init: false,
            fields_are_parameters: false,
            dataclass: false,
            generic: false,
            declares_slots: false,
            slots_weak_references: false,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: vec![FieldDecl {
                cell: false,
                name: "v".to_string(),
                ty: RType::INT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            methods,
            resume: None,
            keywords: Vec::new(),
        }
    }

    fn receiver() -> RType {
        RType::Instance {
            class: "Box".to_string(),
            exact: false,
        }
    }

    #[test]
    fn float_arithmetic_and_a_call_to_it_run_nothing() {
        let module = module(vec![float_body("scale"), call("outer", None, "scale")]);
        let effects = Effects::of(&module);
        assert!(!effects.call_runs_python("scale"));
        assert!(!effects.call_runs_python("outer"));
        assert_eq!(running(&module, "outer"), Vec::<String>::new());
    }

    #[test]
    fn a_call_out_of_the_unit_runs_python_and_so_does_its_caller() {
        let module = module(vec![call("outer", None, "elsewhere")]);
        assert!(Effects::of(&module).call_runs_python("outer"));
        assert_eq!(running(&module, "outer").len(), 1);
    }

    #[test]
    fn a_getter_calling_anything_runs_python_through_every_call_to_it() {
        // `@property def v(self) -> float: return helper(1.0)`, where `helper` is
        // nothing the unit compiled — so the getter's own body is otherwise clear, and it
        // is the call alone that decides
        let mut getter = FunctionBuilder::new("v$get", RType::FLOAT);
        getter.param("self", receiver());
        let out = getter.temp(RType::FLOAT);
        getter.push(Op::CallNative {
            dest: Some(out),
            owner: None,
            callee: "helper".to_string(),
            args: vec![Value::Float(1.0)],
        });
        getter.terminate(Terminator::Return(Value::Register(out)));

        let mut module = module(vec![call("reader", Some("Box"), "v$get")]);
        module.classes.push(class("Box", vec![getter.finish()]));
        let effects = Effects::of(&module);
        assert!(effects.call_runs_python("Box.v$get"));
        assert!(effects.call_runs_python("reader"));
        assert_eq!(running(&module, "reader").len(), 1);
    }

    /// `def add(a: int, b: int) -> int: return a + b`
    fn add() -> Function {
        let mut builder = FunctionBuilder::new("add", RType::INT);
        let a = builder.param("a", RType::INT);
        let b = builder.param("b", RType::INT);
        let out = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        builder.finish()
    }

    #[test]
    fn a_call_handed_exact_ints_runs_nothing_where_one_handed_anything_may() {
        // `add(n, 1)` asks a subclass's `__add__` where `n` is one; `add(add(0, 1), 1)` is
        // two machine additions, and what the inner call answers is itself an exact `int`
        let mut builder = FunctionBuilder::new("caller", RType::INT);
        let n = builder.param("n", RType::INT);
        let first = builder.temp(RType::INT);
        let second = builder.temp(RType::INT);
        let third = builder.temp(RType::INT);
        let call = |dest, args| Op::CallNative {
            dest: Some(dest),
            owner: None,
            callee: "add".to_string(),
            args,
        };
        builder.push(call(first, vec![Value::Int(0), Value::Int(1)]));
        builder.push(call(second, vec![Value::Register(first), Value::Int(1)]));
        builder.push(call(third, vec![Value::Register(n), Value::Int(1)]));
        builder.terminate(Terminator::Return(Value::Register(second)));
        let module = module(vec![add(), builder.finish()]);
        let effects = Effects::of(&module);
        assert!(effects.call_runs_python("add"));
        let ops = running(&module, "caller");
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert!(ops[0].contains("Register(RegisterId(0))"), "{ops:?}");
    }

    #[test]
    fn a_recursion_runs_nothing_only_where_its_own_name_still_stands() {
        // `def depth(n): return 0 if n < 1 else depth(n - 1) + 1`, with the recursive call
        // guarded: the arm the guard turns away calls whatever the name holds, so a call
        // runs nothing only where the name is known to stand
        let mut builder = FunctionBuilder::new("depth", RType::INT);
        let n = builder.param("n", RType::INT);
        let less = builder.temp(RType::BIT);
        let resolved = builder.temp(RType::OBJECT);
        let stands = builder.temp(RType::BIT);
        let smaller = builder.temp(RType::INT);
        let below = builder.temp(RType::INT);
        let callee = builder.temp(RType::OBJECT);
        let boxed = builder.temp(RType::OBJECT);
        let answered = builder.temp(RType::OBJECT);
        let sum = builder.temp(RType::INT);
        let base = builder.new_block();
        let guard = builder.new_block();
        let native = builder.new_block();
        let rebound = builder.new_block();
        let join = builder.new_block();
        builder.push(Op::IntCompare {
            dest: less,
            op: CmpOp::Lt,
            lhs: Value::Register(n),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(less),
            then_block: base,
            else_block: guard,
        });
        builder.switch_to(base);
        builder.terminate(Terminator::Return(Value::Int(0)));
        builder.switch_to(guard);
        builder.push(Op::ResolveFunction {
            dest: resolved,
            name: "depth".to_string(),
        });
        builder.push(Op::IntBinary {
            dest: smaller,
            op: BinOp::Sub,
            lhs: Value::Register(n),
            rhs: Value::Int(1),
        });
        builder.push(Op::FunctionStands {
            dest: stands,
            src: Value::Register(resolved),
            name: "depth".to_string(),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(stands),
            then_block: native,
            else_block: rebound,
        });
        builder.switch_to(native);
        builder.push(Op::Release {
            value: Value::Register(resolved),
            path: Box::new([]),
        });
        builder.push(Op::CallNative {
            dest: Some(below),
            owner: None,
            callee: "depth".to_string(),
            args: vec![Value::Register(smaller)],
        });
        builder.terminate(Terminator::Goto(join));
        builder.switch_to(rebound);
        builder.push(Op::FunctionCallee {
            dest: callee,
            src: Value::Register(resolved),
            name: "depth".to_string(),
        });
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Register(smaller),
        });
        builder.push(Op::CallThrough {
            dest: answered,
            callee: Value::Register(callee),
            args: vec![Value::Register(boxed)],
            keywords: Vec::new(),
        });
        builder.push(Op::Unbox {
            dest: below,
            src: Value::Register(answered),
            to: RType::INT,
        });
        builder.terminate(Terminator::Goto(join));
        builder.switch_to(join);
        builder.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(below),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(sum)));

        let module = module(vec![builder.finish()]);
        let effects = Effects::of(&module);
        let function = &module.functions[0];
        let call = Op::CallNative {
            dest: None,
            owner: None,
            callee: "depth".to_string(),
            args: vec![Value::Int(5)],
        };
        let held = effects.held_at_entry(function);
        assert!(effects.op_runs_python(function, &call, &held));
        let mut standing = held;
        standing.assume_standing(&BTreeSet::from([Guard::Function("depth".to_string())]));
        assert!(!effects.op_runs_python(function, &call, &standing));
        // and not with an argument that may be a subclass
        let unknown = Op::CallNative {
            dest: None,
            owner: None,
            callee: "depth".to_string(),
            args: vec![Value::Register(n)],
        };
        assert!(effects.op_runs_python(function, &unknown, &standing));
    }

    #[test]
    fn a_cycle_of_clear_bodies_stays_clear() {
        let module = module(vec![call("ping", None, "pong"), call("pong", None, "ping")]);
        let effects = Effects::of(&module);
        assert!(!effects.call_runs_python("ping"));
        assert!(!effects.call_runs_python("pong"));
    }

    #[test]
    fn letting_go_of_an_object_that_may_have_a_finalizer_runs_python() {
        // `o.method()` hands back anything at all, `__del__` included, and the local
        // holding it lets go of it
        let mut builder = FunctionBuilder::new("hold", RType::FLOAT);
        let given = builder.param("o", RType::OBJECT);
        let held = builder.local("held", RType::OBJECT);
        builder.push(Op::CallMethod {
            dest: held,
            receiver: Value::Register(given),
            name: "method".to_string(),
            args: Vec::new(),
        });
        builder.push(Op::Release {
            value: Value::Register(held),
            path: Box::new([]),
        });
        builder.terminate(Terminator::Return(Value::Float(0.0)));
        let module = module(vec![builder.finish()]);
        let ops = running(&module, "hold");
        assert!(ops.iter().any(|op| op.starts_with("Release")), "{ops:?}");
    }

    #[test]
    fn letting_go_of_an_exact_int_or_str_or_a_copy_of_a_parameter_runs_nothing() {
        let mut builder = FunctionBuilder::new("digits", RType::FLOAT);
        let count = builder.param("n", RType::fixed(IntWidth::I64));
        let given = builder.param("o", RType::OBJECT);
        let tagged = builder.local("t", RType::INT);
        let text = builder.local("s", RType::STR);
        let copy = builder.local("c", RType::OBJECT);
        builder.push(Op::Box {
            dest: tagged,
            src: Value::Register(count),
        });
        builder.assign(text, Value::Str("x".to_string()));
        builder.assign(copy, Value::Register(given));
        for register in [tagged, text, copy] {
            builder.push(Op::Release {
                value: Value::Register(register),
                path: Box::new([]),
            });
        }
        builder.terminate(Terminator::Return(Value::Float(0.0)));
        let module = module(vec![builder.finish()]);
        assert_eq!(running(&module, "digits"), Vec::<String>::new());
        assert!(!Effects::of(&module).call_runs_python("digits"));
    }

    #[test]
    fn an_allocation_the_collector_tracks_runs_python_and_a_boxed_int_does_not() {
        // an `int` is not tracked by the collector, so building one cannot start a
        // collection; a list is
        let build = |with_list: bool| {
            let mut builder = FunctionBuilder::new("build", RType::FLOAT);
            let n = builder.param("n", RType::fixed(IntWidth::I64));
            let tagged = builder.local("t", RType::INT);
            builder.push(Op::Box {
                dest: tagged,
                src: Value::Register(n),
            });
            if with_list {
                let list = builder.temp(RType::LIST);
                builder.push(Op::BuildList {
                    dest: list,
                    items: Vec::new(),
                });
            }
            builder.terminate(Terminator::Return(Value::Float(0.0)));
            module(vec![builder.finish()])
        };
        assert_eq!(running(&build(false), "build"), Vec::<String>::new());
        let ops = running(&build(true), "build");
        assert!(
            ops.iter().all(|op| op.starts_with("BuildList")) && ops.len() == 1,
            "{ops:?}"
        );
    }

    #[test]
    fn a_comparison_of_objects_runs_python() {
        let mut builder = FunctionBuilder::new("same", RType::BIT);
        let a = builder.param("a", RType::OBJECT);
        let b = builder.param("b", RType::OBJECT);
        let out = builder.temp(RType::BIT);
        builder.push(Op::ObjectCompare {
            dest: out,
            op: CmpOp::Eq,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let module = module(vec![builder.finish()]);
        assert_eq!(running(&module, "same").len(), 1);
    }

    #[test]
    fn a_comparison_of_tagged_ints_runs_python_unless_both_are_exact() {
        // a parameter may be an `int` subclass with an `__lt__` of its own; a length is
        // an exact `int` whatever it measured
        let build = |bound_is_a_length: bool| {
            let mut builder = FunctionBuilder::new("below", RType::BIT);
            let n = builder.param("n", RType::INT);
            let s = builder.param("s", RType::STR);
            let bound = builder.temp(RType::INT);
            let index = builder.local("i", RType::fixed(IntWidth::I64));
            builder.assign(index, Value::Fixed(0));
            if bound_is_a_length {
                builder.push(Op::Len {
                    dest: bound,
                    src: Value::Register(s),
                });
            } else {
                builder.push(Op::IntBinary {
                    dest: bound,
                    op: BinOp::Add,
                    lhs: Value::Register(n),
                    rhs: Value::Int(1),
                });
            }
            let out = builder.temp(RType::BIT);
            builder.push(Op::IntCompare {
                dest: out,
                op: CmpOp::Lt,
                lhs: Value::Register(index),
                rhs: Value::Register(bound),
            });
            builder.terminate(Terminator::Return(Value::Register(out)));
            module(vec![builder.finish()])
        };

        // `n + 1` asks the subclass, and so does comparing against what it answered
        let ops = running(&build(false), "below");
        assert_eq!(ops.len(), 2, "{ops:?}");

        // the length of a `str` that may be a subclass is its `__len__`, but what that
        // answered is an exact `int`
        let ops = running(&build(true), "below");
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert!(ops[0].starts_with("Len"), "{ops:?}");

        // and once something has tested that the `str` is exact, nothing runs at all
        let module = build(true);
        let function = &module.functions[0];
        let effects = Effects::of(&module);
        let mut held = Held::at_entry(function);
        held.assume_exact(RegisterId(1), Kind::Str);
        for op in &function.blocks[0].ops {
            assert!(!effects.op_runs_python(function, op, &held), "{op:?}");
            held.step(function, op);
        }
    }

    #[test]
    fn overwriting_a_register_that_may_hold_an_int_subclass_runs_python() {
        let mut builder = FunctionBuilder::new("keep", RType::FLOAT);
        let n = builder.param("n", RType::INT);
        let held = builder.local("held", RType::INT);
        builder.push(Op::IntBinary {
            dest: held,
            op: BinOp::Add,
            lhs: Value::Register(n),
            rhs: Value::Int(1),
        });
        builder.assign(held, Value::Int(1));
        builder.terminate(Terminator::Return(Value::Float(0.0)));
        let module = module(vec![builder.finish()]);
        let ops = running(&module, "keep");
        assert!(ops.iter().any(|op| op.starts_with("Assign")), "{ops:?}");
    }

    #[test]
    fn what_one_path_leaves_unknown_is_unknown_where_the_paths_meet() {
        // `t = len(s)` on one arm and `t = s.count()` on the other: after the merge a
        // comparison against `t` may reach an `int` subclass
        let mut builder = FunctionBuilder::new("merge", RType::BIT);
        let s = builder.param("s", RType::STR);
        let which = builder.param("which", RType::BIT);
        let bound = builder.local("t", RType::INT);
        let other = builder.temp(RType::OBJECT);
        let left = builder.new_block();
        let right = builder.new_block();
        let join = builder.new_block();
        builder.terminate(Terminator::Branch {
            cond: Value::Register(which),
            then_block: left,
            else_block: right,
        });
        builder.switch_to(left);
        builder.push(Op::Len {
            dest: bound,
            src: Value::Register(s),
        });
        builder.terminate(Terminator::Goto(join));
        builder.switch_to(right);
        builder.push(Op::CallMethod {
            dest: other,
            receiver: Value::Register(s),
            name: "count".to_string(),
            args: Vec::new(),
        });
        builder.push(Op::Unbox {
            dest: bound,
            src: Value::Register(other),
            to: RType::INT,
        });
        builder.terminate(Terminator::Goto(join));
        builder.switch_to(join);
        let out = builder.temp(RType::BIT);
        builder.push(Op::IntCompare {
            dest: out,
            op: CmpOp::Lt,
            lhs: Value::Fixed(0),
            rhs: Value::Register(bound),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let module = module(vec![builder.finish()]);
        let ops = running(&module, "merge");
        assert!(ops.iter().any(|op| op.starts_with("IntCompare")), "{ops:?}");
    }

    #[test]
    fn an_attribute_read_or_a_field_licence_on_an_extendable_class_runs_python() {
        let build = |licence: bool| {
            let mut builder = FunctionBuilder::new("read", RType::BIT);
            let o = builder.param("o", receiver());
            if licence {
                let stands = builder.temp(RType::BIT);
                builder.push(Op::FieldStands {
                    dest: stands,
                    src: Value::Register(o),
                    class: "Box".to_string(),
                    field: "v".to_string(),
                });
            } else {
                let got = builder.temp(RType::OBJECT);
                builder.push(Op::GetAttr {
                    dest: got,
                    receiver: Value::Register(o),
                    name: "v".to_string(),
                });
            }
            builder.terminate(Terminator::Return(Value::Bit(true)));
            let mut module = module(vec![builder.finish()]);
            module.classes.push(class("Box", Vec::new()));
            module
        };
        let ops = running(&build(true), "read");
        assert!(
            ops.len() == 1 && ops[0].starts_with("FieldStands"),
            "{ops:?}"
        );
        let ops = running(&build(false), "read");
        assert!(ops.len() == 1 && ops[0].starts_with("GetAttr"), "{ops:?}");
    }

    #[test]
    fn a_rebinding_of_a_namespace_runs_python() {
        let mut builder = FunctionBuilder::new("rebind", RType::BIT);
        let status = builder.temp(RType::BIT);
        builder.push(Op::StoreGlobal {
            dest: status,
            name: "len".to_string(),
            value: Value::Int(0),
        });
        builder.terminate(Terminator::Return(Value::Register(status)));
        let module = module(vec![builder.finish()]);
        assert_eq!(running(&module, "rebind").len(), 1);
    }
}
