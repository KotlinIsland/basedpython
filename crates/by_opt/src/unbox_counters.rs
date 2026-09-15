//! giving a loop counter a machine-integer representation inside a copy of its loop
//!
//! a tagged `int` pays, on every trip of a counting loop, a shortness test on each
//! operand of the comparison, another pair on the step, the step's overflow
//! computation, and two tests of the results. an `int64_t` pays a compare and a
//! checked add. on a scalar float loop that is 19% of the whole running time — the
//! largest single cost left in a loop whose arithmetic already matches what a C
//! compiler would emit for the same program written in C.
//!
//! a counter is still an `int`, though, and python's `int` does not stop at the machine
//! word:
//!
//! ```python
//! i = 9223372036854775806
//! while i < n:        # n = 2**63 + 3
//!     i = i + 1       # the second step leaves the word, and python counts on
//! ```
//!
//! so the machine representation lives only in a copy of the loop, which the passes that
//! duplicate loops (`guard_loops` and `unswitch`) make anyway. the copy is entered by
//! narrowing each counter it reads from its tagged register, and leaves by writing each
//! counter read afterwards back. a step that would leave the word takes an edge rather
//! than raising: it writes the counters back and continues at that same step in the loop
//! as written, whose tagged arithmetic carries on exactly as python's does. the tagged
//! registers are the counters everywhere outside the copy, so nothing after the loop can
//! tell which of the two ran.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use by_ir::function::{BasicBlock, Function, RegisterDecl};
use by_ir::ops::{BinOp, BlockId, CmpOp, Op, RegisterId, Terminator, Value};
use by_ir::rtype::{IntWidth, Primitive, RType};

use crate::guard_loops::split;
use crate::liveness::{error_live, live_in, live_out, read_registers};

/// the registers the loop made of `body` can hold as machine integers
///
/// a counter is an `int` register whose every write inside the loop is a literal, a
/// length, a copy of another counter, or a sum or difference of counters, literals and
/// bounds — `int` registers nothing in the loop writes. only the writes inside the loop
/// decide: the copy narrows whatever a counter or a bound holds on the way in, and a value
/// too large to narrow runs the loop as written
///
/// no *use* can disqualify one, because a reader that wants the tagged value is handed it.
/// a counter that a handler outside the loop reads is left alone: a failing operation
/// leaves the copy by its error edge with nothing on the way to write the counter back
pub(crate) fn counters(function: &Function, body: &[BlockId]) -> Vec<RegisterId> {
    let live = live_in(function, &read_registers);
    counters_given(function, body, &live)
}

pub(crate) fn counters_given(
    function: &Function,
    body: &[BlockId],
    live: &[HashSet<RegisterId>],
) -> Vec<RegisterId> {
    // every write to each register inside the loop, and the registers it takes its value
    // from
    let mut writes: BTreeMap<RegisterId, Vec<Write>> = BTreeMap::new();
    let mut refused: BTreeSet<RegisterId> = BTreeSet::new();
    let invariant = |register: RegisterId| is_invariant(function, body, register);
    for id in body {
        let block = &function.blocks[id.index()];
        for op in &block.ops {
            for written in op.unbinds().into_iter().chain(op.loop_cursor()) {
                refused.insert(written);
            }
            if let Some(dest) = op.dest() {
                writes
                    .entry(dest)
                    .or_default()
                    .push(machine_write(function, op));
            }
        }
        if let Some(dest) = block.terminator.dest() {
            refused.insert(dest);
        }
        // a handler outside the loop is reached with nothing to write a counter back
        if let Some(handler) = block.error_target
            && !body.contains(&handler)
            && let Some(read) = live.get(handler.index())
        {
            refused.extend(read.iter().copied());
        }
    }
    let mut members: BTreeSet<RegisterId> = writes
        .iter()
        .filter(|(register, each)| {
            function
                .register(**register)
                .is_some_and(|decl| decl.ty == RType::INT && !decl.may_be_unassigned)
                && !refused.contains(register)
                && !each.contains(&Write::Tagged)
        })
        .map(|(register, _)| *register)
        .collect();
    // a write taking its value from a register that is neither a counter nor a bound is not
    // a machine write
    loop {
        let refusing: Vec<RegisterId> = members
            .iter()
            .filter(|register| {
                writes[register].iter().any(|write| {
                    let Write::From(sources) = write else {
                        return false;
                    };
                    sources.iter().flatten().any(|source| {
                        !members.contains(source)
                            && !is_fixed(function, &Value::Register(*source))
                            && !invariant(*source)
                    })
                })
            })
            .copied()
            .collect();
        if refusing.is_empty() {
            break;
        }
        for register in refusing {
            members.remove(&register);
        }
    }
    members.into_iter().collect()
}

/// what a write gives the register it writes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Write {
    /// a value a machine integer holds, whatever else is
    Machine,
    /// a value a machine integer holds if each of these registers holds one
    From([Option<RegisterId>; 2]),
    /// a value only the tagged representation holds
    Tagged,
}

fn machine_write(function: &Function, op: &Op) -> Write {
    // an operand of a machine write: a literal, or a register that has to hold a machine
    // integer too
    let operand = |value: &Value| match value {
        Value::Int(_) => Some(None),
        Value::Register(register) => Some(Some(*register)),
        _ => None,
    };
    match op {
        Op::Assign {
            src: Value::Int(_), ..
        }
        | Op::ArrayLen { .. } => Write::Machine,
        Op::Assign {
            src: Value::Register(source),
            ..
        } => Write::From([Some(*source), None]),
        Op::IntBinary {
            op: BinOp::Add | BinOp::Sub,
            lhs,
            rhs,
            ..
        } => match (operand(lhs), operand(rhs)) {
            (Some(lhs), Some(rhs)) => Write::From([lhs, rhs]),
            _ => Write::Tagged,
        },
        // a copy nested inside this loop writing its own machine counter back
        Op::Box {
            src: source @ Value::Register(_),
            ..
        } if is_fixed(function, source) => Write::Machine,
        _ => Write::Tagged,
    }
}

/// whether `register` is an `int` nothing in `body` writes, which a copy can narrow once on
/// the way in
fn is_invariant(function: &Function, body: &[BlockId], register: RegisterId) -> bool {
    function
        .register(register)
        .is_some_and(|decl| decl.ty == RType::INT && !decl.may_be_unassigned)
        && !body.iter().any(|id| {
            let block = &function.blocks[id.index()];
            block.terminator.dest() == Some(register)
                || block.ops.iter().any(|op| {
                    op.dest() == Some(register)
                        || op.unbinds() == Some(register)
                        || op.loop_cursor() == Some(register)
                })
        })
}

/// the tagged bounds that nothing in the loop writes and that its guards compare a counter
/// against or its counters are written from, each narrowed once on the way in
pub(crate) fn bounds(
    function: &Function,
    body: &[BlockId],
    counters: &[RegisterId],
) -> Vec<RegisterId> {
    let written: HashSet<RegisterId> = body
        .iter()
        .flat_map(|id| {
            let block = &function.blocks[id.index()];
            block
                .ops
                .iter()
                .flat_map(|op| {
                    op.dest()
                        .into_iter()
                        .chain(op.unbinds())
                        .chain(op.loop_cursor())
                })
                .chain(block.terminator.dest())
        })
        .collect();
    let mut out = Vec::new();
    for op in body.iter().flat_map(|id| &function.blocks[id.index()].ops) {
        // a sum a counter is written with reads its bounds as machine integers
        if op.dest().is_some_and(|dest| counters.contains(&dest))
            && let Write::From(sources) = machine_write(function, op)
        {
            for source in sources.into_iter().flatten() {
                if !counters.contains(&source)
                    && !is_fixed(function, &Value::Register(source))
                    && !written.contains(&source)
                    && !out.contains(&source)
                {
                    out.push(source);
                }
            }
            continue;
        }
        let Op::IntCompare {
            lhs,
            rhs: Value::Register(bound),
            ..
        } = op
        else {
            continue;
        };
        let counted = is_fixed(function, lhs)
            || matches!(lhs, Value::Register(counter) if counters.contains(counter));
        if counted
            && function
                .register(*bound)
                .is_some_and(|decl| decl.ty == RType::INT && !decl.may_be_unassigned)
            && !written.contains(bound)
            && !counters.contains(bound)
            && !out.contains(bound)
        {
            out.push(*bound);
        }
    }
    out
}

/// hold `counters` as machine integers in the copy of the loop `header` heads, and
/// `bounds` narrowed, answering what the copy narrows on the way in: each tagged register
/// it reads before writing, with the machine register that holds its value
///
/// `copies` names the copy of each block of `body`. a copied block may have been cut short
/// where the copy leaves for the loop as written, but what it keeps is that block's own
/// operations in their own places, which is what lets a step leave for the same point in
/// the original. the caller enters the copy through the narrowings this answers with, and
/// sends a value that does not narrow to the loop as written
pub(crate) fn unbox(
    function: &mut Function,
    header: BlockId,
    copies: &BTreeMap<BlockId, BlockId>,
    counters: &[RegisterId],
    bounds: &[RegisterId],
) -> Vec<(RegisterId, RegisterId)> {
    let live = live_in(function, &read_registers);
    let width = RType::fixed(IntWidth::I64);
    let fixed: BTreeMap<RegisterId, RegisterId> = counters
        .iter()
        .map(|counter| (*counter, fresh(function, width.clone())))
        .collect();
    let narrowed: BTreeMap<RegisterId, RegisterId> = bounds
        .iter()
        .map(|bound| (*bound, fresh(function, width.clone())))
        .collect();
    // every register a machine write reads its operands from
    let machine: BTreeMap<RegisterId, RegisterId> = fixed
        .iter()
        .chain(narrowed.iter())
        .map(|(tagged, machine)| (*tagged, *machine))
        .collect();
    // the pieces of the loop as written a step leaves for, and the blocks it leaves through
    let mut added: Vec<BlockId> = Vec::new();

    // every step of a counter, and the point of the loop as written it leaves for
    let mut steps: BTreeMap<BlockId, Vec<usize>> = BTreeMap::new();
    for (original, copy) in copies {
        let at: Vec<usize> = function.blocks[copy.index()]
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| is_step(op, &fixed))
            .map(|(index, _)| index)
            .collect();
        if !at.is_empty() {
            steps.insert(*original, at);
        }
    }
    // what is live at each step is asked of the loop as written before any of it is split
    let mut live_at_step: HashMap<(BlockId, usize), Vec<RegisterId>> = HashMap::new();
    for (original, at) in &steps {
        for index in at {
            let live_here = live_before(function, &live, *original, *index);
            live_at_step.insert(
                (*original, *index),
                counters
                    .iter()
                    .filter(|counter| live_here.contains(counter))
                    .copied()
                    .collect(),
            );
        }
    }
    let mut resumes: HashMap<(BlockId, usize), BlockId> = HashMap::new();
    for (original, at) in &steps {
        // latest first, so each split cuts what is left in front of the one after it
        for index in at.iter().rev() {
            let tail = split(function, *original, *index);
            resumes.insert((*original, *index), tail);
            added.push(tail);
        }
    }

    let mut region: BTreeSet<BlockId> = copies.values().copied().collect();
    let mut tagged: BTreeMap<RegisterId, RegisterId> = BTreeMap::new();
    for (original, copy) in copies {
        let block = &mut function.blocks[copy.index()];
        let ops = std::mem::take(&mut block.ops);
        let terminator = std::mem::replace(&mut block.terminator, Terminator::Unreachable);
        let (error_target, range, mut position) = (block.error_target, block.range, block.position);
        let mut current = *copy;
        let mut written: Vec<Op> = Vec::new();
        for (index, op) in ops.into_iter().enumerate() {
            if let Op::Line { position: line } = &op {
                position = Some(*line);
            }
            if is_step(&op, &fixed)
                && let Op::IntBinary {
                    dest,
                    op: step,
                    lhs,
                    rhs,
                } = &op
            {
                let resume = resumes[&(*original, index)];
                let landing = push_block(function, error_target, range, position);
                function.blocks[landing.index()].ops = live_at_step[&(*original, index)]
                    .iter()
                    .map(|counter| Op::Box {
                        dest: *counter,
                        src: Value::Register(fixed[counter]),
                    })
                    .collect();
                function.blocks[landing.index()].terminator = Terminator::Goto(resume);
                added.push(landing);
                let rest = push_block(function, error_target, range, position);
                region.insert(rest);
                let block = &mut function.blocks[current.index()];
                block.ops = std::mem::take(&mut written);
                block.terminator = Terminator::MachineStep {
                    dest: fixed[dest],
                    op: *step,
                    lhs: machine_operand(lhs, &machine),
                    rhs: machine_operand(rhs, &machine),
                    fits: rest,
                    overflows: landing,
                };
                current = rest;
                continue;
            }
            rewrite(function, op, &fixed, &machine, &mut tagged, &mut written);
        }
        // a terminator reads a counter tagged. that includes the narrowing of a copy nested
        // in this one: a machine integer here still has to be a short to take its edge,
        // which is what the nested copy's proofs rest on
        let mut terminator = terminator;
        for value in terminator.operands_mut() {
            if let Value::Register(read) = value
                && let Some(machine) = fixed.get(read)
            {
                let boxed = boxed_register(function, &mut tagged, *read);
                written.push(Op::Box {
                    dest: boxed,
                    src: Value::Register(*machine),
                });
                *value = Value::Register(boxed);
            }
        }
        let block = &mut function.blocks[current.index()];
        block.ops = written;
        block.terminator = terminator;
    }
    for id in &region {
        narrow_reads(function, *id, &narrowed);
    }

    // on the way out, whatever is read next is written back first
    let landings: BTreeSet<BlockId> = added.iter().copied().collect();
    let mut leaving: Vec<BlockId> = Vec::new();
    for id in region.iter().copied().collect::<Vec<_>>() {
        let targets = function.blocks[id.index()].terminator.successors();
        for target in targets {
            if region.contains(&target) || landings.contains(&target) {
                continue;
            }
            let read: Vec<RegisterId> = counters
                .iter()
                .filter(|counter| {
                    live.get(target.index())
                        .is_some_and(|set| set.contains(counter))
                })
                .copied()
                .collect();
            if read.is_empty() {
                continue;
            }
            let (error_target, range, position) = {
                let block = &function.blocks[id.index()];
                (block.error_target, block.range, block.position)
            };
            let landing = push_block(function, error_target, range, position);
            function.blocks[landing.index()].ops = read
                .iter()
                .map(|counter| Op::Box {
                    dest: *counter,
                    src: Value::Register(fixed[counter]),
                })
                .collect();
            function.blocks[landing.index()].terminator = Terminator::Goto(target);
            leaving.push(landing);
            for edge in function.blocks[id.index()].terminator.successors_mut() {
                if *edge == target {
                    *edge = landing;
                }
            }
        }
    }

    // the way out is where a counter is written back, and it is entered from a single edge
    // of the copy, with everything that edge establishes. a step leaving the machine word
    // is not: a counter's writes only keep it on one side of the short range while every
    // step fits
    if let Some(copy) = copies.get(&header) {
        let proven: Vec<BlockId> = region.into_iter().chain(leaving).collect();
        let short: Vec<RegisterId> = narrowed.values().copied().collect();
        tag_proven_shorts(function, &proven, *copy, &short);
    }

    let entered = live.get(header.index()).cloned().unwrap_or_default();
    fixed
        .iter()
        .filter(|(counter, _)| entered.contains(counter))
        .chain(narrowed.iter())
        .map(|(tagged, machine)| (*tagged, *machine))
        .collect()
}

/// a step of a counter by a literal, which becomes a [`Terminator::MachineStep`]
fn is_step(op: &Op, fixed: &BTreeMap<RegisterId, RegisterId>) -> bool {
    matches!(op, Op::IntBinary { dest, .. } if fixed.contains_key(dest))
}

/// an operand of a machine write, read from the machine register that holds it
fn machine_operand(value: &Value, machine: &BTreeMap<RegisterId, RegisterId>) -> Value {
    match value {
        Value::Register(register) => {
            Value::Register(machine.get(register).copied().unwrap_or(*register))
        }
        Value::Int(literal) => Value::Fixed(*literal),
        other => other.clone(),
    }
}

/// write `op` into the copy, reading and writing the machine registers
fn rewrite(
    function: &mut Function,
    mut op: Op,
    fixed: &BTreeMap<RegisterId, RegisterId>,
    machine: &BTreeMap<RegisterId, RegisterId>,
    tagged: &mut BTreeMap<RegisterId, RegisterId>,
    written: &mut Vec<Op>,
) {
    if let Some(dest) = op.dest()
        && let Some(written_into) = fixed.get(&dest)
    {
        let op = match op {
            Op::Assign { src, .. } => Op::Assign {
                dest: *written_into,
                src: machine_operand(&src, machine),
            },
            Op::Box { src, .. } => Op::Assign {
                dest: *written_into,
                src,
            },
            mut other => {
                if let Some(dest) = other.dest_mut() {
                    *dest = *written_into;
                }
                other
            }
        };
        written.push(op);
        return;
    }
    let served = served(function, &op, fixed);
    // each counter an operation reads tagged is boxed once, however many operands read it
    let mut boxed: BTreeMap<RegisterId, RegisterId> = BTreeMap::new();
    for value in op.operands_mut() {
        let Value::Register(read) = value else {
            continue;
        };
        let Some(machine) = fixed.get(read) else {
            continue;
        };
        if served.contains(read) {
            *value = Value::Register(*machine);
            continue;
        }
        let register = match boxed.get(read) {
            Some(register) => *register,
            None => {
                let register = boxed_register(function, tagged, *read);
                written.push(Op::Box {
                    dest: register,
                    src: Value::Register(*machine),
                });
                boxed.insert(*read, register);
                register
            }
        };
        *value = Value::Register(register);
    }
    written.push(op);
}

/// the blocks a copy is entered through, answering the first of them
///
/// each of `narrowings` is tested and narrowed in turn, and the first that does not fit
/// runs the loop as written from `header`. a counter every one of `entries` has just set to
/// a short literal needs no test, since the literal is what it holds, and is written
/// straight into its machine register: `i = 0` ahead of the loop is by far the usual way a
/// counter starts
pub(crate) fn enter(
    function: &mut Function,
    header: BlockId,
    entries: &[BlockId],
    narrowings: &[(RegisterId, RegisterId)],
    copy: BlockId,
) -> BlockId {
    let known = |register: RegisterId| {
        let mut literal = None;
        for entry in entries {
            let block = &function.blocks[entry.index()];
            if block.terminator.dest() == Some(register) {
                return None;
            }
            let written = block.ops.iter().rev().find(|op| {
                op.dest() == Some(register)
                    || op.unbinds() == Some(register)
                    || op.loop_cursor() == Some(register)
            });
            match written {
                Some(Op::Assign {
                    src: Value::Int(value),
                    ..
                }) if SHORT.contains(value) && literal.is_none_or(|seen| seen == *value) => {
                    literal = Some(*value);
                }
                _ => return None,
            }
        }
        literal
    };
    let mut assigned = Vec::new();
    let mut tested = Vec::new();
    for (tagged, machine) in narrowings {
        match known(*tagged) {
            Some(value) => assigned.push(Op::Assign {
                dest: *machine,
                src: Value::Fixed(value),
            }),
            None => tested.push((*tagged, *machine)),
        }
    }
    if assigned.is_empty() && tested.is_empty() {
        return copy;
    }
    let (range, position) = {
        let block = &function.blocks[header.index()];
        (block.range, block.position)
    };
    let first = BlockId(function.blocks.len());
    let links = tested.len().max(1);
    for index in 0..links {
        let next = if index + 1 < links {
            BlockId(first.index() + index + 1)
        } else {
            copy
        };
        let terminator = match tested.get(index) {
            Some((tagged, machine)) => Terminator::NarrowShort {
                dest: *machine,
                src: Value::Register(*tagged),
                fits: next,
                otherwise: header,
            },
            None => Terminator::Goto(next),
        };
        function.blocks.push(BasicBlock {
            ops: if index == 0 {
                std::mem::take(&mut assigned)
            } else {
                Vec::new()
            },
            terminator,
            owned_at_exit: None,
            range,
            position,
            error_target: None,
        });
    }
    first
}

/// the counters `op` reads that it can read as machine integers
///
/// codegen compares an unboxed left operand against either a tagged right one or an
/// unboxed one, and a right operand is only read unboxed when the left is too, since that
/// is the pair it reads as two machine integers. a character comparison, a subscript and a
/// packed buffer name their element by an offset, which is the number the register holds,
/// and a double or an object is built from the machine integer directly
fn served(
    function: &Function,
    op: &Op,
    fixed: &BTreeMap<RegisterId, RegisterId>,
) -> Vec<RegisterId> {
    let counter = |value: &Value| match value {
        Value::Register(register) if fixed.contains_key(register) => Some(*register),
        _ => None,
    };
    match op {
        Op::IntCompare { lhs, rhs, .. } => {
            let left = counter(lhs);
            let right = counter(rhs).filter(|_| left.is_some() || is_fixed(function, lhs));
            left.into_iter().chain(right).collect()
        }
        Op::StrItemCompare { index, .. }
        | Op::GetItem { index, .. }
        | Op::ArrayGet { index, .. }
        | Op::ArraySet { index, .. } => counter(index).into_iter().collect(),
        Op::IntToFloat { src, .. } => counter(src).into_iter().collect(),
        Op::Box { dest, src }
            if function
                .register(*dest)
                .is_some_and(|decl| decl.ty == RType::OBJECT) =>
        {
            counter(src).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// the one register each counter's tagged value is handed to readers in, inside a copy
fn boxed_register(
    function: &mut Function,
    tagged: &mut BTreeMap<RegisterId, RegisterId>,
    counter: RegisterId,
) -> RegisterId {
    if let Some(register) = tagged.get(&counter) {
        return *register;
    }
    let register = fresh(function, RType::INT);
    tagged.insert(counter, register);
    register
}

fn fresh(function: &mut Function, ty: RType) -> RegisterId {
    let id = RegisterId(function.registers.len());
    function.registers.push(RegisterDecl {
        name: None,
        ty,
        borrowed: false,
        may_be_unassigned: false,
    });
    id
}

fn push_block(
    function: &mut Function,
    error_target: Option<BlockId>,
    range: Option<(u32, u32)>,
    position: Option<by_ir::ops::Position>,
) -> BlockId {
    let id = BlockId(function.blocks.len());
    function.blocks.push(BasicBlock {
        ops: Vec::new(),
        terminator: Terminator::Unreachable,
        owned_at_exit: None,
        range,
        position,
        error_target,
    });
    id
}

/// the registers live just before the operation at `at` in `block`
fn live_before(
    function: &Function,
    live: &[HashSet<RegisterId>],
    block: BlockId,
    at: usize,
) -> HashSet<RegisterId> {
    let block = &function.blocks[block.index()];
    let mut out = live_out(block, live);
    let handler = error_live(block, live);
    for op in block.ops[at..].iter().rev() {
        out.extend(handler.iter().copied());
        if let Some(dest) = op.dest() {
            out.remove(&dest);
        }
        out.extend(read_registers(op));
    }
    out
}

pub(crate) fn is_fixed(function: &Function, value: &Value) -> bool {
    let ty = match value {
        Value::Register(id) => function.register(*id).map(|decl| decl.ty.clone()),
        other => other.immediate_type(),
    };
    matches!(ty, Some(RType::Primitive(Primitive::Fixed(_))))
}

/// read each bound out of its narrowed register instead of the tagged one, wherever the
/// narrowed one serves
///
/// a guard comparing a machine counter against the bound becomes a comparison of two
/// machine integers, and a bound widened to a double converts from the machine integer
/// with nothing to fail
pub(crate) fn narrow_reads(
    function: &mut Function,
    block: BlockId,
    narrowed: &BTreeMap<RegisterId, RegisterId>,
) {
    let fixed_lhs: Vec<bool> = function.blocks[block.index()]
        .ops
        .iter()
        .map(|op| matches!(op, Op::IntCompare { lhs, .. } if is_fixed(function, lhs)))
        .collect();
    let narrow = |value: &mut Value| {
        if let Value::Register(register) = value
            && let Some(machine) = narrowed.get(register)
        {
            *value = Value::Register(*machine);
        }
    };
    for (op, fixed_lhs) in function.blocks[block.index()].ops.iter_mut().zip(fixed_lhs) {
        match op {
            Op::IntCompare { rhs, .. } if fixed_lhs => narrow(rhs),
            Op::IntToFloat { src, .. } => narrow(src),
            _ => {}
        }
    }
}

/// the short range, in the machine integer a counter holds
const SHORT: std::ops::RangeInclusive<i64> = -(1 << 62)..=(1 << 62) - 1;

/// the side of the short range a guard that held keeps a counter on
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    /// no larger than the largest short
    Below,
    /// no smaller than the smallest short
    Above,
}

type Facts = BTreeSet<(RegisterId, Side)>;

/// give a counter its tagged value with no range test wherever the copy has proven it short
///
/// the proof is the copy's own guards. one comparing the counter against a narrowed bound,
/// or against a short immediate, keeps the counter on one side of the short range on the
/// edge it held on, for as long as nothing writes the counter again. the other side comes
/// from a second guard, or from the counter's writes: a counter only ever narrowed from a
/// short, set to one, or stepped towards the side a guard holds is on the other side for
/// its whole life, because a step that would leave the machine integer leaves the copy
///
/// ```python
/// i = 0
/// while i < n:        # below the largest short, on the edge into the body
///     split(i)        # and never below zero, since `i` only ever counts up from it
///     i = i + 1
/// ```
pub(crate) fn tag_proven_shorts(
    function: &mut Function,
    copy: &[BlockId],
    header: BlockId,
    narrowed: &[RegisterId],
) {
    let short = |value: &Value| match value {
        Value::Register(id) => narrowed.contains(id),
        Value::Fixed(literal) => SHORT.contains(literal),
        _ => false,
    };
    // a block an error edge reaches is entered part way through another, where nothing
    // was established
    let handlers: BTreeSet<BlockId> = copy
        .iter()
        .filter_map(|id| function.blocks[id.index()].error_target)
        .collect();
    let mut entry: BTreeMap<BlockId, Facts> = BTreeMap::from([(header, Facts::new())]);
    let mut queue = vec![header];
    while let Some(id) = queue.pop() {
        let Some(facts) = entry.get(&id).cloned() else {
            continue;
        };
        for (next, facts) in exits(function, &function.blocks[id.index()], facts, &short) {
            if !copy.contains(&next) || next == header || handlers.contains(&next) {
                continue;
            }
            let met = match entry.get(&next) {
                Some(known) => known.intersection(&facts).copied().collect(),
                None => facts,
            };
            if entry.get(&next) != Some(&met) {
                entry.insert(next, met);
                queue.push(next);
            }
        }
    }
    for handler in handlers {
        entry.insert(handler, Facts::new());
    }

    for id in copy {
        let Some(mut facts) = entry.get(id).cloned() else {
            continue;
        };
        for index in 0..function.blocks[id.index()].ops.len() {
            let op = &function.blocks[id.index()].ops[index];
            if let Op::Box {
                dest,
                src: Value::Register(counter),
            } = op
                && function
                    .register(*dest)
                    .is_some_and(|decl| decl.ty == RType::INT)
                && is_fixed(function, &Value::Register(*counter))
                && [Side::Below, Side::Above].into_iter().all(|side| {
                    facts.contains(&(*counter, side)) || never_past(function, *counter, side)
                })
            {
                let tagged = Op::TagShort {
                    dest: *dest,
                    src: Value::Register(*counter),
                };
                function.blocks[id.index()].ops[index] = tagged;
            }
            let op = &function.blocks[id.index()].ops[index];
            for written in op.dest().into_iter().chain(op.loop_cursor()) {
                facts.retain(|(register, _)| *register != written);
            }
        }
    }
}

/// the facts on each edge out of `block`, given those it was entered with
fn exits(
    function: &Function,
    block: &BasicBlock,
    mut facts: Facts,
    short: &impl Fn(&Value) -> bool,
) -> Vec<(BlockId, Facts)> {
    // what a comparison's bit says about its counter when it is true and when it is false
    let mut compared: BTreeMap<RegisterId, (RegisterId, Vec<Side>, Vec<Side>)> = BTreeMap::new();
    for op in &block.ops {
        for written in op.dest().into_iter().chain(op.loop_cursor()) {
            facts.retain(|(register, _)| *register != written);
            compared.retain(|bit, (counter, _, _)| *bit != written && *counter != written);
        }
        if let Op::IntCompare {
            dest,
            op: compare,
            lhs: lhs @ Value::Register(counter),
            rhs,
        } = op
            && is_fixed(function, lhs)
            && short(rhs)
        {
            let (held, failed) = match compare {
                CmpOp::Lt | CmpOp::Le => (vec![Side::Below], vec![Side::Above]),
                CmpOp::Gt | CmpOp::Ge => (vec![Side::Above], vec![Side::Below]),
                CmpOp::Eq => (vec![Side::Below, Side::Above], Vec::new()),
                CmpOp::Ne => (Vec::new(), vec![Side::Below, Side::Above]),
            };
            compared.insert(*dest, (*counter, held, failed));
        }
    }
    if let Some(written) = block.terminator.dest() {
        facts.retain(|(register, _)| *register != written);
    }
    let with = |sides: &[Side], counter: RegisterId| {
        let mut out = facts.clone();
        out.extend(sides.iter().map(|side| (counter, *side)));
        out
    };
    match &block.terminator {
        Terminator::Branch {
            cond: Value::Register(bit),
            then_block,
            else_block,
        } if let Some((counter, held, failed)) = compared.get(bit) => vec![
            (*then_block, with(held, *counter)),
            (*else_block, with(failed, *counter)),
        ],
        _ => block
            .successors()
            .into_iter()
            .map(|next| (next, facts.clone()))
            .collect(),
    }
}

/// whether every write to `counter` leaves it on `side` of the short range for good:
/// narrowed from a short, set to one, or stepped away from that edge by a literal
fn never_past(function: &Function, counter: RegisterId, side: Side) -> bool {
    let away = |op: BinOp, lhs: &Value, rhs: &Value| {
        let Value::Fixed(step) = rhs else {
            return false;
        };
        let away = match (op, side) {
            (BinOp::Add, Side::Above) | (BinOp::Sub, Side::Below) => *step >= 0,
            (BinOp::Add, Side::Below) | (BinOp::Sub, Side::Above) => *step <= 0,
            _ => false,
        };
        matches!(lhs, Value::Register(lhs) if *lhs == counter) && away
    };
    // how a register is written by a terminator: a narrowing is always short, and a step
    // has to be one away from the edge
    let terminator_writes = |register: RegisterId| {
        function
            .blocks
            .iter()
            .filter(|block| block.terminator.dest() == Some(register))
            .map(|block| match &block.terminator {
                Terminator::NarrowShort { .. } => true,
                Terminator::MachineStep { op, lhs, rhs, .. } => away(*op, lhs, rhs),
                _ => false,
            })
            .collect::<Vec<bool>>()
    };
    // a step written into a register of its own and then copied back
    let stepped = |source: RegisterId| {
        let by_terminator = terminator_writes(source);
        !by_terminator.is_empty()
            && by_terminator.iter().all(|away| *away)
            && !function
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .any(|op| op.dest() == Some(source))
    };
    // a parameter holds whatever the caller wrote
    counter.index() >= function.param_count
        && terminator_writes(counter).iter().all(|away| *away)
        && function
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter(|op| op.dest() == Some(counter))
            .all(|op| match op {
                Op::Assign {
                    src: Value::Fixed(literal),
                    ..
                } => SHORT.contains(literal),
                Op::Assign {
                    src: Value::Register(source),
                    ..
                } => stepped(*source),
                // a length is never negative, and never past the short range either — it
                // counts objects in memory
                Op::ArrayLen { .. } => side == Side::Above,
                _ => false,
            })
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::verify::verify;

    use super::*;
    use crate::unswitch::tests::{fast_ops, module_with};

    /// `i = 0; while i < len(s): <reader>; i = i + 1`, run through the pass that copies it
    fn counted(reader: impl FnOnce(&mut FunctionBuilder, RegisterId, RegisterId)) -> Function {
        let mut builder = FunctionBuilder::new("scan", RType::BOOL);
        let text = builder.param("s", RType::STR);
        let bound = builder.param("n", RType::INT);
        let counter = builder.local("i", RType::INT);
        let more = builder.temp(RType::BIT);
        builder.assign(counter, Value::Int(0));
        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.push(Op::IntCompare {
            dest: more,
            op: CmpOp::Lt,
            lhs: Value::Register(counter),
            rhs: Value::Register(bound),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });
        builder.switch_to(body);
        reader(&mut builder, text, counter);
        builder.push(Op::IntBinary {
            dest: counter,
            op: BinOp::Add,
            lhs: Value::Register(counter),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Bool(true)));
        let mut module = module_with(builder.finish());
        crate::unswitch::run(&mut module);
        let function = module.functions.remove(0);
        assert_eq!(verify(&function), Ok(()));
        function
    }

    fn boxes(function: &Function) -> usize {
        fast_ops(function)
            .iter()
            .filter(|op| matches!(op, Op::Box { .. } | Op::TagShort { .. }))
            .count()
    }

    #[test]
    fn a_counter_that_only_indexes_a_character_comparison_is_never_boxed() {
        let function = counted(|builder, text, counter| {
            let answer = builder.temp(RType::BIT);
            builder.push(Op::StrItemCompare {
                dest: answer,
                op: CmpOp::Eq,
                container: Value::Register(text),
                index: Value::Register(counter),
                character: ' ',
            });
        });
        assert_eq!(
            boxes(&function),
            0,
            "{}",
            by_ir::print::print_function(&function)
        );
    }

    /// a subscript names the element at an offset, and the offset is the number the
    /// register already holds — so the counter keeps its machine representation all
    /// the way into the read, and codegen reaches `By_GetItemI64` rather than boxing
    #[test]
    fn a_counter_that_indexes_a_container_is_never_boxed() {
        let function = counted(|builder, text, counter| {
            let element = builder.temp(RType::OBJECT);
            builder.push(Op::GetItem {
                dest: element,
                container: Value::Register(text),
                index: Value::Register(counter),
            });
        });
        assert_eq!(
            boxes(&function),
            0,
            "{}",
            by_ir::print::print_function(&function)
        );
    }

    /// an object is built from the machine integer directly, with no tagged value on the way
    #[test]
    fn a_counter_built_into_an_object_is_never_tagged_first() {
        let function = counted(|builder, _text, counter| {
            let object = builder.temp(RType::OBJECT);
            builder.push(Op::Box {
                dest: object,
                src: Value::Register(counter),
            });
        });
        let ops = fast_ops(&function);
        assert!(
            ops.iter()
                .any(|op| matches!(op, Op::Box { src, .. } if is_fixed(&function, src))),
            "{}",
            by_ir::print::print_function(&function)
        );
        assert_eq!(
            boxes(&function),
            1,
            "{}",
            by_ir::print::print_function(&function)
        );
    }

    /// the fused comparison is the only character reader that takes a machine index. a
    /// character *read* has to produce the character as an object, and the index it is
    /// asked for is the tagged one
    #[test]
    fn a_counter_that_reads_a_character_out_still_gets_its_tagged_value_back() {
        let function = counted(|builder, text, counter| {
            let character = builder.temp(RType::STR);
            builder.push(Op::StrGetItem {
                dest: character,
                container: Value::Register(text),
                index: Value::Register(counter),
            });
        });
        assert_eq!(
            boxes(&function),
            1,
            "{}",
            by_ir::print::print_function(&function)
        );
    }

    /// `m = m + n` with `n` a counter, `m = m + step` with `step` a bound nothing in the loop
    /// writes, and `m = m + product` with `product` written from a multiplication, whose
    /// value has no machine form
    fn summing(source: &str) -> (Function, RegisterId, BlockId, BlockId) {
        let mut builder = FunctionBuilder::new("sums", RType::INT);
        let step = builder.param("step", RType::INT);
        let n = builder.local("n", RType::INT);
        let m = builder.local("m", RType::INT);
        let product = builder.temp(RType::INT);
        let more = builder.temp(RType::BIT);
        builder.assign(n, Value::Int(1));
        builder.assign(m, Value::Int(0));
        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.push(Op::IntCompare {
            dest: more,
            op: CmpOp::Lt,
            lhs: Value::Register(m),
            rhs: Value::Int(100),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });
        builder.switch_to(body);
        builder.push(Op::IntBinary {
            dest: n,
            op: BinOp::Add,
            lhs: Value::Register(n),
            rhs: Value::Int(1),
        });
        builder.push(Op::IntBinary {
            dest: product,
            op: BinOp::Mul,
            lhs: Value::Register(n),
            rhs: Value::Int(2),
        });
        let added = match source {
            "counter" => n,
            "bound" => step,
            _ => product,
        };
        builder.push(Op::IntBinary {
            dest: m,
            op: BinOp::Add,
            lhs: Value::Register(m),
            rhs: Value::Register(added),
        });
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(m)));
        (builder.finish(), m, header, body)
    }

    #[test]
    fn a_counter_written_from_a_sum_of_counters_and_bounds_is_a_counter() {
        for (source, counted) in [("counter", true), ("bound", true), ("product", false)] {
            let (function, m, header, body) = summing(source);
            assert_eq!(
                counters(&function, &[header, body]).contains(&m),
                counted,
                "{source}"
            );
        }
        let (function, _, header, body) = summing("bound");
        let found = counters(&function, &[header, body]);
        assert_eq!(
            bounds(&function, &[header, body], &found),
            vec![RegisterId(0)]
        );
    }

    /// a handler outside the loop is reached from the middle of a trip with nothing on the
    /// way to write a counter back, so a counter it reads stays tagged
    #[test]
    fn a_counter_a_handler_outside_the_loop_reads_stays_tagged() {
        let mut builder = FunctionBuilder::new("scan", RType::INT);
        let bound = builder.param("n", RType::INT);
        let callee = builder.param("f", RType::OBJECT);
        let counter = builder.local("i", RType::INT);
        let more = builder.temp(RType::BIT);
        let called = builder.temp(RType::OBJECT);
        builder.assign(counter, Value::Int(0));
        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        let handler = builder.new_block();
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.push(Op::IntCompare {
            dest: more,
            op: CmpOp::Lt,
            lhs: Value::Register(counter),
            rhs: Value::Register(bound),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });
        builder.switch_to(body);
        builder.push(Op::CallValue {
            dest: called,
            callee: Value::Register(callee),
            args: Vec::new(),
        });
        builder.push(Op::IntBinary {
            dest: counter,
            op: BinOp::Add,
            lhs: Value::Register(counter),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Int(0)));
        builder.switch_to(handler);
        builder.terminate(Terminator::Return(Value::Register(counter)));
        let mut function = builder.finish();
        function.blocks[body.index()].error_target = Some(handler);
        assert_eq!(counters(&function, &[header, body]), Vec::new());
    }
}
