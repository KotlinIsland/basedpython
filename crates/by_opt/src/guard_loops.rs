//! asking whether a builtin still stands once on the way into a loop, not on every trip
//!
//! a call to `len` is lowered natively only where the name still resolves to the builtin,
//! and python resolves it on every call — so the lowering asks on every call too:
//!
//! ```python
//! def longest_run(line: str) -> int:
//!     i = 0
//!     while i < len(line):    # is `len` still the builtin?
//!         ...
//! ```
//!
//! the question is cheap, but it is not the cost. its answer decides between the native
//! length and a call through whatever the name holds, and the two arms merge again before
//! the comparison, which keeps the C compiler from folding anything across them.
//!
//! only python code can write a namespace. so where the answer was yes on the way into
//! the loop, and nothing since can have run python, it is still yes. the loop is
//! duplicated: one copy exactly as it stands, and one that answers yes without asking, with
//! a test on the way in that picks between them. the test also asks whether each parameter
//! the loop reads is exactly an `int` or a `str`, since an exact value is what lets `len(s)`
//! or `i < n` run without reaching a method a subclass wrote.
//!
//! the copy is left for the loop as written right after any operation that can run python
//! — see [`crate::runs_python`] — continuing at the very next operation of the original,
//! which asks every question again. so the copy never answers for a moment python code has
//! been able to change, and a loop whose body always runs python pays one test on the way
//! in and runs as it did

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use by_ir::function::{BasicBlock, Function, ModuleIr, RegisterDecl};
use by_ir::ops::{BlockId, Op, RegisterId, Terminator, Value};
use by_ir::rtype::RType;

use crate::runs_python::{Effects, Guard, Held, Kind, answer_yes};

/// how many loops in one function may be duplicated
///
/// the same bound `unswitch` keeps, for the same reason: a copy of a loop that holds a loop
/// is a copy of that loop too
const MAX_PER_FUNCTION: usize = 4;

/// the largest loop worth duplicating, in blocks
const MAX_BODY: usize = 32;

pub(crate) fn run(module: &mut ModuleIr) {
    // copies only ever drop questions and leave for the original, so what a call can run
    // is the same after as before
    let effects = Effects::of(module);
    for function in module.all_functions_mut() {
        // only the blocks the function started with are asked about. a block split off a
        // loop is entered from its copy's exits, and taking one for a loop's header would
        // put a test on the way back from every exit
        let mut versioned = 0;
        for index in 0..function.blocks.len() {
            if versioned == MAX_PER_FUNCTION {
                break;
            }
            if let Some(candidate) = candidate(&effects, function, BlockId(index))
                && version(&effects, function, candidate)
            {
                versioned += 1;
            }
        }
        if versioned == 0 {
            version_entry(&effects, function);
        }
    }
}

/// duplicate a whole function with no loop in it, entered through the test where it starts
///
/// the same copy a loop gets, for the calls a function makes once each: a function that
/// calls another module function asks whether that one still stands at every call, and
/// a recursive one asks it at every level. with nothing between the entry and the call
/// able to run python — once the parameters it reads are known to be exact — the
/// answer on the way in is the answer at the call
fn version_entry(effects: &Effects, function: &mut Function) -> bool {
    let entry = Function::entry();
    let count = function.blocks.len();
    if count == 0 || count > MAX_BODY || goes_round(function) {
        return false;
    }
    let all: Vec<BlockId> = (0..count).map(BlockId).collect();
    let (builtins, functions) = guards(function, &all);
    if builtins.is_empty() && functions.is_empty() {
        return false;
    }
    let exact = exact_parameters(function, &all);

    // the entry block's operations move to a block of their own, which the entry then
    // falls into — so the entry has an edge to redirect through the test, as a loop does
    let saved = function.blocks[entry.index()].clone();
    let header = BlockId(count);
    function.blocks.push(saved.clone());
    let start = &mut function.blocks[entry.index()];
    start.ops.clear();
    start.terminator = Terminator::Goto(header);
    start.error_target = None;

    let body: Vec<BlockId> = (1..=count).map(BlockId).collect();
    let held = effects.held_at_entry(function);
    let versioned = version(
        effects,
        function,
        Candidate {
            header,
            body,
            builtins,
            functions,
            exact,
            entry: held,
            region: Region::Entry,
        },
    );
    if !versioned {
        function.blocks.truncate(count);
        function.blocks[entry.index()] = saved;
    }
    versioned
}

/// whether any block can reach itself
fn goes_round(function: &Function) -> bool {
    let all: Vec<BlockId> = (0..function.blocks.len()).map(BlockId).collect();
    // with a block that cannot be on any cycle standing in for the header, a cycle among
    // the rest is a cycle at all
    let unreachable_header = BlockId(function.blocks.len());
    holds_a_loop(function, &all, unreachable_header)
}

/// a loop worth duplicating
struct Candidate {
    header: BlockId,
    /// every block on a path from the header back to itself, the header included
    body: Vec<BlockId>,
    /// the builtins the loop asks about
    builtins: Vec<String>,
    /// the module functions the loop asks about
    functions: Vec<String>,
    /// the parameters the loop reads that the entry test asks to be exact
    exact: Vec<(RegisterId, Kind)>,
    /// what is known on every edge into the header from outside the loop
    entry: Held,
    region: Region,
}

/// what is duplicated
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    /// a loop, entered through the test from outside it
    Loop,
    /// a whole function with no loop in it, entered through the test when it is called
    Entry,
}

/// duplicate the loop, and enter it through the test, reporting whether it was worth it
fn version(effects: &Effects, function: &mut Function, candidate: Candidate) -> bool {
    let Candidate {
        header,
        body,
        builtins,
        functions,
        exact,
        entry,
        region,
    } = candidate;

    // the copies land at the end, so every existing block keeps its id
    let first_copy = function.blocks.len();
    let copies: BTreeMap<BlockId, BlockId> = body
        .iter()
        .enumerate()
        .map(|(offset, id)| (*id, BlockId(function.blocks.len() + offset)))
        .collect();
    let standing: BTreeSet<Guard> = builtins
        .iter()
        .cloned()
        .map(Guard::Builtin)
        .chain(functions.iter().cloned().map(Guard::Function))
        .collect();
    let mut answered: Vec<(BlockId, usize)> = Vec::new();
    for id in &body {
        let mut block = function.blocks[id.index()].clone();
        map_edges(&mut block, true, |target| {
            copies.get(&target).copied().unwrap_or(target)
        });
        let copy = BlockId(function.blocks.len());
        answered.extend(
            answer_yes(&mut block, &standing)
                .into_iter()
                .map(|index| (copy, index)),
        );
        function.blocks.push(block);
    }
    let fast_header = copies[&header];

    // where the copy has to leave: straight after the first operation in a block that can
    // run python. the facts are those of the copy before any block was cut short, which
    // has every path the cut copy has and more, so they hold for it too
    let mut held = entry;
    for (register, kind) in &exact {
        held.assume_exact(*register, *kind);
    }
    held.assume_standing(&standing);
    let copy_ids: HashSet<BlockId> = copies.values().copied().collect();
    let entries = Held::solve(function, fast_header, held, |id| copy_ids.contains(&id));
    let mut exits: Vec<(BlockId, BlockId, usize)> = Vec::new();
    for (original, copy) in &copies {
        let Some(mut known) = entries.get(copy).cloned() else {
            continue;
        };
        let block = &function.blocks[copy.index()];
        for (index, op) in block.ops.iter().enumerate() {
            if effects.op_runs_python(function, op, &known) {
                exits.push((*copy, *original, index + 1));
                break;
            }
            known.step(function, op);
        }
    }

    // a copy that leaves on every trip round would cost a test on the way in and win
    // nothing, and then the loop is left as it stands
    let leaving: HashSet<BlockId> = exits.iter().map(|(copy, _, _)| *copy).collect();
    let worth_it = match region {
        Region::Loop => comes_round(function, fast_header, &copy_ids, &leaving),
        // a function is worth it where the copy reaches a question it no longer asks
        // before it leaves
        Region::Entry => {
            let before_leaving = reached_before_leaving(function, fast_header, &copy_ids, &leaving);
            answered.iter().any(|(copy, index)| {
                before_leaving.contains(copy)
                    && exits
                        .iter()
                        .find(|(leaving, _, _)| leaving == copy)
                        .is_none_or(|(_, _, at)| index < at)
            })
        }
    };
    if !worth_it {
        function.blocks.truncate(first_copy);
        return false;
    }

    // the original is split where the copy leaves it, latest point first so that an
    // earlier split of the same block splits what is left in front of the later one
    exits.sort_by(|a, b| (a.1, b.2).cmp(&(b.1, a.2)));
    let mut tails: HashMap<(BlockId, usize), BlockId> = HashMap::new();
    for (_, original, at) in &exits {
        if tails.contains_key(&(*original, *at)) {
            continue;
        }
        let tail = split(function, *original, *at);
        tails.insert((*original, *at), tail);
    }
    for (copy, original, at) in &exits {
        let block = &mut function.blocks[copy.index()];
        block.ops.truncate(*at);
        block.terminator = Terminator::Goto(tails[&(*original, *at)]);
    }

    // a copy the cut copy no longer reaches would only be dead C
    let reached = reachable(function, fast_header);
    for copy in copies.values() {
        if !reached.contains(copy) {
            let block = &mut function.blocks[copy.index()];
            block.ops.clear();
            block.terminator = Terminator::Unreachable;
            block.error_target = None;
        }
    }

    let answer = RegisterId(function.registers.len());
    function.registers.push(RegisterDecl {
        name: None,
        ty: RType::BIT,
        borrowed: false,
        may_be_unassigned: false,
    });
    let preheader = BlockId(function.blocks.len());
    function.blocks.push(BasicBlock {
        ops: vec![Op::LoopGuardsHold {
            dest: answer,
            builtins,
            functions,
            exact: exact
                .iter()
                .map(|(register, _)| Value::Register(*register))
                .collect(),
        }],
        terminator: Terminator::Branch {
            cond: Value::Register(answer),
            then_block: fast_header,
            else_block: header,
        },
        owned_at_exit: None,
        range: function.blocks[header.index()].range,
        position: function.blocks[header.index()].position,
        error_target: None,
    });

    // every edge that entered the loop from outside now enters through the test. the loop,
    // its copy, the blocks split off it and the test itself are all inside
    let inside: HashSet<BlockId> = body
        .iter()
        .copied()
        .chain(copy_ids)
        .chain(tails.values().copied())
        .chain([preheader])
        .collect();
    for id in 0..function.blocks.len() {
        if inside.contains(&BlockId(id)) {
            continue;
        }
        map_edges(&mut function.blocks[id], false, |target| {
            if target == header { preheader } else { target }
        });
    }
    true
}

/// every block of `copies` reached from `header` without passing through a block the copy
/// leaves from, `header` and the blocks it leaves from included
fn reached_before_leaving(
    function: &Function,
    header: BlockId,
    copies: &HashSet<BlockId>,
    leaving: &HashSet<BlockId>,
) -> HashSet<BlockId> {
    let mut seen = HashSet::from([header]);
    let mut queue = vec![header];
    while let Some(id) = queue.pop() {
        if leaving.contains(&id) {
            continue;
        }
        for next in function.blocks[id.index()].successors() {
            if copies.contains(&next) && seen.insert(next) {
                queue.push(next);
            }
        }
    }
    seen
}

/// whether the copy entered at `header` can come back round to it without leaving: every
/// block on the way is one of `copies`, and none of them is one the copy leaves from
fn comes_round(
    function: &Function,
    header: BlockId,
    copies: &HashSet<BlockId>,
    leaving: &HashSet<BlockId>,
) -> bool {
    let mut seen = HashSet::new();
    let mut queue = vec![header];
    while let Some(id) = queue.pop() {
        if leaving.contains(&id) {
            continue;
        }
        for next in function.blocks[id.index()].successors() {
            if next == header {
                return true;
            }
            if copies.contains(&next) && seen.insert(next) {
                queue.push(next);
            }
        }
    }
    false
}

/// the loop `header` heads, if it is one worth duplicating
fn candidate(effects: &Effects, function: &Function, header: BlockId) -> Option<Candidate> {
    if header == Function::entry() {
        return None;
    }
    let body = natural_loop(function, header);
    // a loop holding another is left to the inner one, whose copy is the one that saves a
    // question on every trip: the outer copy would only ever save the inner loop's first
    if body.len() < 2 || body.len() > MAX_BODY || holds_a_loop(function, &body, header) {
        return None;
    }
    {
        let (builtins, functions) = guards(function, &body);
        if builtins.is_empty() && functions.is_empty() {
            return None;
        }

        let outside: Vec<BlockId> = (0..function.blocks.len())
            .map(BlockId)
            .filter(|id| !body.contains(id))
            .filter(|id| function.blocks[id.index()].successors().contains(&header))
            .collect();
        // a handler entered from outside would carry an exception into the test, and a
        // loop already entered through a test of its own has been duplicated once
        if outside.is_empty()
            || outside.iter().any(|id| {
                let block = &function.blocks[id.index()];
                block.error_target == Some(header) || enters_by_test(block, header)
            })
        {
            return None;
        }
        let entries = Held::solve(
            function,
            Function::entry(),
            effects.held_at_entry(function),
            |_| true,
        );
        let mut entry: Option<Held> = None;
        for id in &outside {
            let Some(mut known) = entries.get(id).cloned() else {
                continue;
            };
            for op in &function.blocks[id.index()].ops {
                known.step(function, op);
            }
            known.step_terminator(&function.blocks[id.index()].terminator);
            entry = Some(match entry {
                None => known,
                Some(existing) => existing.meet(&known),
            });
        }
        let entry = entry?;
        let exact = exact_parameters(function, &body);
        Some(Candidate {
            header,
            body,
            builtins,
            functions,
            exact,
            entry,
            region: Region::Loop,
        })
    }
}

/// the builtins and the module functions the blocks of `body` ask about
fn guards(function: &Function, body: &[BlockId]) -> (Vec<String>, Vec<String>) {
    let mut builtins: Vec<String> = Vec::new();
    let mut functions: Vec<String> = Vec::new();
    for op in body.iter().flat_map(|id| &function.blocks[id.index()].ops) {
        match op {
            Op::BuiltinStands { name, .. } if !builtins.contains(name) => {
                builtins.push(name.clone());
            }
            Op::ResolveFunction { name, .. } if !functions.contains(name) => {
                functions.push(name.clone());
            }
            _ => {}
        }
    }
    (builtins, functions)
}

/// the parameters `body` reads that the entry test asks to be exact, and as what
fn exact_parameters(function: &Function, body: &[BlockId]) -> Vec<(RegisterId, Kind)> {
    let unwritten = unwritten_parameters(function);
    let mut exact: Vec<(RegisterId, Kind)> = Vec::new();
    for op in body.iter().flat_map(|id| &function.blocks[id.index()].ops) {
        // a register only the object protocol reads is asked to be a `list` where it is
        // measured or indexed: that is what an exact `list` answers without asking
        let measured = match op {
            Op::Len { src, .. } => Some(src),
            Op::GetItem { container, .. } => Some(container),
            _ => None,
        };
        for operand in op.operands() {
            let Value::Register(register) = operand else {
                continue;
            };
            let kind = match function.register(*register).map(|decl| &decl.ty) {
                Some(ty) if *ty == RType::INT => Kind::Int,
                Some(ty) if *ty == RType::STR => Kind::Str,
                Some(ty)
                    if (*ty == RType::OBJECT || *ty == RType::LIST)
                        && measured == Some(operand) =>
                {
                    Kind::List
                }
                _ => continue,
            };
            if unwritten.contains(register) && !exact.iter().any(|(held, _)| held == register) {
                exact.push((*register, kind));
            }
        }
    }
    exact
}

/// the parameters nothing in the function writes, which hold what the caller passed for
/// the whole call
fn unwritten_parameters(function: &Function) -> HashSet<RegisterId> {
    let mut unwritten: HashSet<RegisterId> = (0..function.param_count).map(RegisterId).collect();
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
    unwritten
}

/// move everything from `at` on in `id` into a block of its own, which `id` then falls
/// into, and name the new block
fn split(function: &mut Function, id: BlockId, at: usize) -> BlockId {
    let tail = BlockId(function.blocks.len());
    let block = &mut function.blocks[id.index()];
    let rest = block.ops.split_off(at);
    // a failing operation names the line of the last marker before it, and the tail starts
    // wherever the operations left in front of it had got to
    let position = block
        .ops
        .iter()
        .rev()
        .find_map(|op| match op {
            Op::Line { position } => Some(*position),
            _ => None,
        })
        .or(block.position);
    let terminator = std::mem::replace(&mut block.terminator, Terminator::Goto(tail));
    let moved = BasicBlock {
        ops: rest,
        terminator,
        owned_at_exit: None,
        range: block.range,
        position,
        error_target: block.error_target,
    };
    function.blocks.push(moved);
    tail
}

/// whether `block` is the test a duplicated loop is entered through
fn enters_by_test(block: &BasicBlock, header: BlockId) -> bool {
    matches!(block.terminator, Terminator::Branch { else_block, .. } if else_block == header)
        && matches!(block.ops.last(), Some(Op::LoopGuardsHold { .. }))
}

/// point every edge of `block` somewhere else, the error edge too where `errors`
fn map_edges(block: &mut BasicBlock, errors: bool, map: impl Fn(BlockId) -> BlockId) {
    match &mut block.terminator {
        Terminator::Goto(target) => *target = map(*target),
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => {
            *then_block = map(*then_block);
            *else_block = map(*else_block);
        }
        Terminator::NarrowShort {
            fits, otherwise, ..
        } => {
            *fits = map(*fits);
            *otherwise = map(*otherwise);
        }
        Terminator::Return(_) | Terminator::Unreachable => {}
    }
    if errors && let Some(target) = &mut block.error_target {
        *target = map(*target);
    }
}

/// whether the blocks of `body` other than `header` go round a cycle of their own
fn holds_a_loop(function: &Function, body: &[BlockId], header: BlockId) -> bool {
    let inside: HashSet<BlockId> = body.iter().copied().filter(|id| *id != header).collect();
    // a block is done once everything reachable from it inside has been seen without
    // coming back to a block still on the path
    let mut done: HashSet<BlockId> = HashSet::new();
    for start in body.iter().filter(|id| **id != header) {
        if done.contains(start) {
            continue;
        }
        let mut path: Vec<(BlockId, Vec<BlockId>)> =
            vec![(*start, successors_within(function, *start, &inside))];
        let mut on_path: HashSet<BlockId> = HashSet::from([*start]);
        while let Some((id, pending)) = path.last_mut() {
            match pending.pop() {
                Some(next) if on_path.contains(&next) => return true,
                Some(next) if !done.contains(&next) => {
                    on_path.insert(next);
                    let successors = successors_within(function, next, &inside);
                    path.push((next, successors));
                }
                Some(_) => {}
                None => {
                    done.insert(*id);
                    on_path.remove(id);
                    path.pop();
                }
            }
        }
    }
    false
}

fn successors_within(function: &Function, id: BlockId, inside: &HashSet<BlockId>) -> Vec<BlockId> {
    function.blocks[id.index()]
        .successors()
        .into_iter()
        .filter(|next| inside.contains(next))
        .collect()
}

/// the loop `header` heads: `header`, and every block that reaches one of its back edges
/// without passing through `header` again
///
/// a back edge is one into `header` from a block every path from the entry reaches through
/// `header`. an edge from anywhere else enters the loop, so a loop inside another is its
/// own loop, entered from the outer one's blocks, rather than the whole of the outer one
fn natural_loop(function: &Function, header: BlockId) -> Vec<BlockId> {
    let everywhere = reachable_avoiding(function, Function::entry(), None);
    let around = reachable_avoiding(function, Function::entry(), Some(header));
    let latches: Vec<BlockId> = everywhere
        .iter()
        .copied()
        .filter(|id| !around.contains(id))
        .filter(|id| function.blocks[id.index()].successors().contains(&header))
        .collect();
    if latches.is_empty() {
        return Vec::new();
    }
    let mut body: HashSet<BlockId> = HashSet::from([header]);
    let mut queue = latches;
    while let Some(id) = queue.pop() {
        if !body.insert(id) {
            continue;
        }
        for (from, block) in function.blocks.iter().enumerate() {
            let from = BlockId(from);
            if !body.contains(&from) && block.successors().contains(&id) {
                queue.push(from);
            }
        }
    }
    let mut body: Vec<BlockId> = body.into_iter().collect();
    body.sort_unstable();
    body
}

/// the blocks reachable from `start` without passing through `avoiding`
fn reachable_avoiding(
    function: &Function,
    start: BlockId,
    avoiding: Option<BlockId>,
) -> HashSet<BlockId> {
    let mut seen = HashSet::new();
    if Some(start) == avoiding {
        return seen;
    }
    seen.insert(start);
    let mut queue = vec![start];
    while let Some(id) = queue.pop() {
        for next in function.blocks[id.index()].successors() {
            if Some(next) != avoiding && seen.insert(next) {
                queue.push(next);
            }
        }
    }
    seen
}

/// the blocks reachable from `start`, `start` included
fn reachable(function: &Function, start: BlockId) -> HashSet<BlockId> {
    let mut seen = HashSet::from([start]);
    let mut queue = vec![start];
    while let Some(id) = queue.pop() {
        for next in function
            .block(id)
            .map(BasicBlock::successors)
            .unwrap_or_default()
        {
            if seen.insert(next) {
                queue.push(next);
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::{Function, ModuleIr};
    use by_ir::ops::{BinOp, CmpOp, Op, Terminator, Value};
    use by_ir::rtype::{IntWidth, RType};
    use by_ir::verify::verify;

    /// `i = 0; while i < len(s): [if i == 3: hook()]; i = i + 1; return i`, lowered the
    /// way the frontend lowers a `len` that may have been rebound
    fn counting(call_in_body: bool) -> Function {
        let mut builder = FunctionBuilder::new("count", RType::fixed(IntWidth::I64));
        let s = builder.param("s", RType::STR);
        let hook = builder.param("hook", RType::OBJECT);
        let i = builder.local("i", RType::fixed(IntWidth::I64));
        let stands = builder.temp(RType::BIT);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        let found = builder.temp(RType::OBJECT);
        let answered = builder.temp(RType::OBJECT);
        let more = builder.temp(RType::BIT);
        let called = builder.temp(RType::OBJECT);
        builder.assign(i, Value::Fixed(0));
        let header = builder.new_block();
        let native = builder.new_block();
        let rebound = builder.new_block();
        let guard = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        builder.terminate(Terminator::Goto(header));

        builder.switch_to(header);
        builder.push(Op::BuiltinStands {
            dest: stands,
            name: "len".to_string(),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(stands),
            then_block: native,
            else_block: rebound,
        });

        builder.switch_to(native);
        builder.assign(widened, Value::Register(s));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Goto(guard));

        builder.switch_to(rebound);
        builder.push(Op::LoadGlobal {
            dest: found,
            name: "len".to_string(),
        });
        builder.push(Op::CallValue {
            dest: answered,
            callee: Value::Register(found),
            args: vec![Value::Register(s)],
        });
        builder.push(Op::Unbox {
            dest: length,
            src: Value::Register(answered),
            to: RType::INT,
        });
        builder.terminate(Terminator::Goto(guard));

        builder.switch_to(guard);
        builder.push(Op::IntCompare {
            dest: more,
            op: CmpOp::Lt,
            lhs: Value::Register(i),
            rhs: Value::Register(length),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });

        builder.switch_to(body);
        if call_in_body {
            let third = builder.temp(RType::BIT);
            let calling = builder.new_block();
            let step = builder.new_block();
            builder.push(Op::IntCompare {
                dest: third,
                op: CmpOp::Eq,
                lhs: Value::Register(i),
                rhs: Value::Fixed(3),
            });
            builder.terminate(Terminator::Branch {
                cond: Value::Register(third),
                then_block: calling,
                else_block: step,
            });
            builder.switch_to(calling);
            builder.push(Op::CallValue {
                dest: called,
                callee: Value::Register(hook),
                args: Vec::new(),
            });
            builder.terminate(Terminator::Goto(step));
            builder.switch_to(step);
        }
        builder.push(Op::IntBinary {
            dest: i,
            op: BinOp::Add,
            lhs: Value::Register(i),
            rhs: Value::Fixed(1),
        });
        builder.terminate(Terminator::Goto(header));

        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(i)));
        builder.finish()
    }

    fn run(function: Function) -> Function {
        let mut module = ModuleIr::new("app");
        module.functions.push(function);
        super::run(&mut module);
        let function = module.functions.remove(0);
        assert_eq!(verify(&function), Ok(()));
        function
    }

    fn entry_tests(function: &Function) -> Vec<&Op> {
        function
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter(|op| matches!(op, Op::LoopGuardsHold { .. }))
            .collect()
    }

    /// the blocks reachable from the block the entry test sends a yes to
    fn fast_copy(function: &Function) -> Vec<by_ir::ops::BlockId> {
        let Some(Terminator::Branch { then_block, .. }) = function
            .blocks
            .iter()
            .find(|block| matches!(block.ops.last(), Some(Op::LoopGuardsHold { .. })))
            .map(|block| block.terminator.clone())
        else {
            return Vec::new();
        };
        let mut seen = vec![then_block];
        let mut queue = vec![then_block];
        while let Some(id) = queue.pop() {
            for next in function.blocks[id.index()].successors() {
                if !seen.contains(&next) {
                    seen.push(next);
                    queue.push(next);
                }
            }
        }
        seen
    }

    #[test]
    fn a_loop_that_runs_no_python_asks_once_on_the_way_in() {
        let function = run(counting(false));
        let tests = entry_tests(&function);
        assert_eq!(tests.len(), 1, "{function:?}");
        let Op::LoopGuardsHold {
            builtins, exact, ..
        } = tests[0]
        else {
            return;
        };
        assert_eq!(builtins, &["len".to_string()]);
        // the `str` is asked to be exact, which is what lets `len` answer without asking it
        assert_eq!(exact.len(), 1);

        // the copy the test says yes to asks nothing, and never reaches the rebound arm
        let copy = fast_copy(&function);
        let ops: Vec<&Op> = copy
            .iter()
            .flat_map(|id| &function.blocks[id.index()].ops)
            .collect();
        assert!(
            !ops.iter()
                .any(|op| matches!(op, Op::BuiltinStands { .. } | Op::CallValue { .. })),
            "{ops:?}"
        );
    }

    #[test]
    fn the_copy_leaves_for_the_loop_as_written_straight_after_a_call() {
        let function = run(counting(true));
        assert_eq!(entry_tests(&function).len(), 1, "{function:?}");
        let copy = fast_copy(&function);
        // the copy's block holding the call ends with it, and falls into a block of the
        // original, which asks whether `len` stands before it measures again
        let Some(leaving) = copy.iter().find(|id| {
            function.blocks[id.index()]
                .ops
                .iter()
                .any(|op| matches!(op, Op::CallValue { .. }))
        }) else {
            panic!("the copy lost its call: {function:?}");
        };
        let block = &function.blocks[leaving.index()];
        assert!(
            matches!(block.ops.last(), Some(Op::CallValue { .. })),
            "{block:?}"
        );
        assert!(
            copy.iter().any(|id| function.blocks[id.index()]
                .ops
                .iter()
                .any(|op| matches!(op, Op::BuiltinStands { .. }))),
            "after the call the loop asks again: {function:?}"
        );
    }

    #[test]
    fn a_loop_that_runs_python_before_every_question_is_left_alone() {
        // the copy would leave on every trip before it reached anything it saved
        let mut function = counting(false);
        let header = function.blocks[0].terminator.clone();
        let Terminator::Goto(header) = header else {
            return;
        };
        let hook = by_ir::ops::RegisterId(1);
        let called = by_ir::ops::RegisterId(function.registers.len());
        function.registers.push(by_ir::function::RegisterDecl {
            name: None,
            ty: RType::OBJECT,
            borrowed: false,
            may_be_unassigned: false,
        });
        function.blocks[header.index()].ops.insert(
            0,
            Op::CallValue {
                dest: called,
                callee: Value::Register(hook),
                args: Vec::new(),
            },
        );
        let function = run(function);
        assert_eq!(entry_tests(&function).len(), 0, "{function:?}");
    }

    #[test]
    fn a_loop_inside_another_is_duplicated_and_the_outer_one_is_not() {
        // `for _ in ...: count(s)` with the counting loop written out inside: the inner
        // loop asks on every trip, and the outer copy would save only its first question
        let mut function = counting(false);
        let Terminator::Goto(inner) = function.blocks[0].terminator else {
            return;
        };
        // a rebound `len` refuses rather than handing back an `int` the next trip round
        // the outer loop would have to let go of — the shape a buffer-holding loop has
        let Terminator::Branch { else_block, .. } = function.blocks[inner.index()].terminator
        else {
            return;
        };
        function.blocks[else_block.index()].ops = vec![Op::RaiseStandard {
            error: by_ir::ops::StandardError::RuntimeError,
            message: "rebound".to_string(),
        }];
        function.blocks[else_block.index()].terminator = Terminator::Unreachable;
        let exit = function
            .blocks
            .iter()
            .position(|block| matches!(block.terminator, Terminator::Return(_)))
            .expect("the counting loop returns");
        let restart = by_ir::ops::BlockId(function.blocks.len());
        let mut again = function.blocks[0].clone();
        again.terminator = Terminator::Goto(inner);
        function.blocks.push(again);
        let done = by_ir::ops::BlockId(function.blocks.len());
        let mut finish = function.blocks[exit].clone();
        finish.ops.clear();
        function.blocks.push(finish);
        let outer_more = by_ir::ops::RegisterId(0);
        function.blocks[exit].terminator = Terminator::Branch {
            cond: Value::Register(outer_more),
            then_block: restart,
            else_block: done,
        };
        function.blocks[0].terminator = Terminator::Goto(restart);
        // the condition is the `str` parameter, which the verifier would refuse as a bit,
        // so it is swapped for a bit of its own
        let bit = by_ir::ops::RegisterId(function.registers.len());
        function.registers.push(by_ir::function::RegisterDecl {
            name: None,
            ty: RType::BIT,
            borrowed: false,
            may_be_unassigned: false,
        });
        if let Terminator::Branch { cond, .. } = &mut function.blocks[exit].terminator {
            *cond = Value::Register(bit);
        }
        function.blocks[restart.index()].ops.push(Op::Assign {
            dest: bit,
            src: Value::Bit(true),
        });

        let function = run(function);
        assert_eq!(entry_tests(&function).len(), 1, "{function:?}");
        // the test sits in front of the inner loop, inside the outer one
        let test = function
            .blocks
            .iter()
            .position(|block| matches!(block.ops.last(), Some(Op::LoopGuardsHold { .. })))
            .expect("the entry test");
        assert!(
            function.blocks[restart.index()]
                .successors()
                .contains(&by_ir::ops::BlockId(test)),
            "{function:?}"
        );
    }

    /// `def bump(n: int) -> int: return add(n, 1)`, the call to `add` guarded, beside `add`
    fn guarded_call() -> ModuleIr {
        let mut add = FunctionBuilder::new("add", RType::INT);
        let a = add.param("a", RType::INT);
        let b = add.param("b", RType::INT);
        let sum = add.temp(RType::INT);
        add.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        add.terminate(Terminator::Return(Value::Register(sum)));

        let mut builder = FunctionBuilder::new("bump", RType::INT);
        let n = builder.param("n", RType::INT);
        let resolved = builder.temp(RType::OBJECT);
        let stands = builder.temp(RType::BIT);
        let answer = builder.temp(RType::INT);
        let callee = builder.temp(RType::OBJECT);
        let boxed = builder.temp(RType::OBJECT);
        let answered = builder.temp(RType::OBJECT);
        let native = builder.new_block();
        let rebound = builder.new_block();
        let join = builder.new_block();
        builder.push(Op::ResolveFunction {
            dest: resolved,
            name: "add".to_string(),
        });
        builder.push(Op::FunctionStands {
            dest: stands,
            src: Value::Register(resolved),
            name: "add".to_string(),
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
            dest: Some(answer),
            owner: None,
            callee: "add".to_string(),
            args: vec![Value::Register(n), Value::Int(1)],
        });
        builder.terminate(Terminator::Goto(join));
        builder.switch_to(rebound);
        builder.push(Op::FunctionCallee {
            dest: callee,
            src: Value::Register(resolved),
            name: "add".to_string(),
        });
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Register(n),
        });
        builder.push(Op::CallThrough {
            dest: answered,
            callee: Value::Register(callee),
            args: vec![Value::Register(boxed), Value::Register(boxed)],
            keywords: Vec::new(),
        });
        builder.push(Op::Unbox {
            dest: answer,
            src: Value::Register(answered),
            to: RType::INT,
        });
        builder.terminate(Terminator::Goto(join));
        builder.switch_to(join);
        builder.terminate(Terminator::Return(Value::Register(answer)));

        let mut module = ModuleIr::new("app");
        module.functions.push(add.finish());
        module.functions.push(builder.finish());
        module
    }

    #[test]
    fn a_function_with_no_loop_asks_on_the_way_in_where_its_arguments_are_exact() {
        let mut module = guarded_call();
        super::run(&mut module);
        let function = &module.functions[1];
        assert_eq!(verify(function), Ok(()));
        let tests = entry_tests(function);
        assert_eq!(tests.len(), 1, "{function:?}");
        let Op::LoopGuardsHold {
            functions, exact, ..
        } = tests[0]
        else {
            return;
        };
        assert_eq!(functions, &["add".to_string()]);
        assert_eq!(exact.len(), 1);
        // the test is what the function starts with, and the copy it says yes to never asks
        // whether the name stands and never reaches the arm that calls whatever it holds
        assert!(
            matches!(function.blocks[0].terminator, Terminator::Goto(_)),
            "{function:?}"
        );
        let copy = fast_copy(function);
        assert!(
            !copy
                .iter()
                .flat_map(|id| &function.blocks[id.index()].ops)
                .any(|op| matches!(op, Op::FunctionStands { .. } | Op::CallThrough { .. })),
            "{function:?}"
        );
    }

    #[test]
    fn duplicating_twice_is_not_attempted() {
        let mut module = ModuleIr::new("app");
        module.functions.push(counting(false));
        super::run(&mut module);
        let once = module.functions[0].blocks.len();
        super::run(&mut module);
        assert_eq!(module.functions[0].blocks.len(), once);
        assert_eq!(entry_tests(&module.functions[0]).len(), 1);
    }
}
