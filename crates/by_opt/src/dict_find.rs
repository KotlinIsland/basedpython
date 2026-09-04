//! asking the table once for a membership test and the read it guards
//!
//! ```python
//! if word in seen:
//!     seen[word] = seen[word] + 1
//! ```
//!
//! `word in seen` and `seen[word]` hash the same key and walk the same table, and
//! the second is only reached because the first said yes. on a histogram loop that
//! pair, together with the write, is most of the running time — the membership test
//! alone is about a fifth of it. so the read is done where the test is, once, and
//! the test becomes a null check on what it found.
//!
//! ## the fact is checked at runtime, not inferred from a type
//!
//! two ordinary things make asking twice observable. a dict *subclass* may have
//! overridden `__contains__` or `__getitem__`, and then how many times each is
//! called is the program's own business. and a key may have a `__hash__` that
//! counts its calls, in which case hashing once where the source hashes twice is a
//! different program. neither is a question this pass answers: the emitted helper
//! takes its single probe only for an exact dict keyed by an exact `str`, and
//! everything else goes through the protocol twice over in the order it would
//! have. so the pass needs no static type at all — it is a *shape* rewrite, and it
//! is correct over a list, a subclass or anything else that reaches it.
//!
//! ## why the branch has to be part of the shape
//!
//! the read is moved *backwards*, to before the branch that guarded it. that is
//! only invisible when nothing can tell: the block holding the read must be
//! reachable only through the edge the test's own answer selects, so that arriving
//! there means the key was there; nothing but register copies may stand between
//! the test and the read, so there is nothing for the read to move past; and the
//! two blocks must be under the same handler, or a `__getitem__` that raises would
//! be caught by an `except` the source put around only one of them.
//!
//! the one thing that does move is the *line* an exception from a slow-path
//! `__getitem__` reports, because a line is a property of a block here and the read
//! has changed blocks. that costs a subclass whose `__getitem__` raises the
//! subscript's line in favour of the `if`'s. on the fused path there is nothing to
//! report: a single probe that found the value cannot then fail to read it.

use std::collections::HashMap;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Terminator, Value};

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        fuse(function);
    }
}

/// where a membership test and the read it guards were found, and what to do
struct Fusion {
    test_block: BlockId,
    /// the `Contains` within `test_block`
    test_at: usize,
    /// the block the "it is there" edge leads to, and the `GetItem` within it
    found_block: BlockId,
    read_at: usize,
    /// the block the other edge leads to
    absent_block: BlockId,
    /// the register the read wrote, which the fused lookup now writes instead
    value: RegisterId,
    /// the register the test wrote, which now says the key was *absent*
    answer: RegisterId,
    container: Value,
    key: Value,
}

fn fuse(function: &mut Function) {
    // every application removes one `Contains`, so the number of them bounds the
    // number of rounds — and a round has to start over because an application can
    // shift the positions a later one was found at
    let rounds = function
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter(|op| matches!(op, Op::Contains { .. }))
        .count();
    for _ in 0..rounds {
        let Some(fusion) = find(function) else { return };
        apply(function, &fusion);
    }
}

fn find(function: &Function) -> Option<Fusion> {
    let reads = read_counts(function);
    let writes = write_counts(function);
    let predecessors = predecessor_counts(function);
    (0..function.blocks.len())
        .find_map(|index| plan(function, BlockId(index), &reads, &writes, &predecessors))
}

fn plan(
    function: &Function,
    test_block: BlockId,
    reads: &HashMap<RegisterId, usize>,
    writes: &HashMap<RegisterId, usize>,
    predecessors: &HashMap<BlockId, usize>,
) -> Option<Fusion> {
    let block = function.block(test_block)?;
    let Terminator::Branch {
        cond: Value::Register(answer),
        then_block,
        else_block,
    } = &block.terminator
    else {
        return None;
    };
    // a branch whose edges lead to the same block does not tell the two answers
    // apart, so arriving there says nothing about the key
    if then_block == else_block {
        return None;
    }
    // the test's answer feeds this branch and nothing else, so the pass is free to
    // give the register the opposite meaning
    if reads.get(answer).copied().unwrap_or(0) != 1 || writes.get(answer).copied() != Some(1) {
        return None;
    }

    let test_at = block.ops.iter().position(|op| op.dest() == Some(*answer))?;
    let Op::Contains {
        value: key,
        container,
        negated,
        ..
    } = &block.ops[test_at]
    else {
        return None;
    };

    // `not in` answers the mirror image, so the read is guarded by the other edge
    let (found_block, absent_block) = if *negated {
        (*else_block, *then_block)
    } else {
        (*then_block, *else_block)
    };
    if found_block == test_block {
        return None;
    }
    // arriving in the read's block must *mean* the key was there, so nothing else
    // may reach it — an exception edge included, which is why this counts every
    // successor rather than only a terminator's
    if predecessors.get(&found_block).copied() != Some(1) {
        return None;
    }
    let found = function.block(found_block)?;
    // a `__getitem__` that raises has to reach the same handler it would have
    if found.error_target != block.error_target {
        return None;
    }

    let read_at = found
        .ops
        .iter()
        .position(|op| !matches!(op, Op::Assign { .. }))?;
    let Op::GetItem {
        dest: value,
        container: read_container,
        index,
    } = &found.ops[read_at]
    else {
        return None;
    };

    // the two sides rarely name the same register: a value read out of a container
    // is spilled into a fresh alias for each use of it, so the test holds one copy
    // of the key and the read holds another. what has to match is what they are
    // copies *of*
    let copies = sole_copies(function, writes);
    let (container_from, mut roots) = origin(container, &copies);
    let (key_from, key_roots) = origin(key, &copies);
    roots.extend(key_roots);
    let (read_container_from, _) = origin(read_container, &copies);
    let (read_key_from, _) = origin(index, &copies);
    if container_from != read_container_from || key_from != read_key_from {
        return None;
    }

    // nothing may stand between the test and the read but copies, and none of them
    // may rewrite what the two sides are copies of — the aliases they make *from*
    // it are exactly what the copies in between are for, so those are left alone
    let untouched = |ops: &[Op]| {
        ops.iter().all(|op| {
            matches!(op, Op::Assign { .. }) && op.dest().is_none_or(|dest| !roots.contains(&dest))
        })
    };
    if !untouched(&block.ops[test_at + 1..]) || !untouched(&found.ops[..read_at]) {
        return None;
    }

    // the read's destination is written where the test is now, which is a place the
    // absent path reaches too — so it must be a temporary of this read alone, and
    // every reader of it must sit in the block the read was in
    let declaration = function.register(*value)?;
    if declaration.name.is_some()
        || declaration.may_be_unassigned
        || writes.get(value).copied() != Some(1)
        || value == answer
        || Value::Register(*value) == *container
        || Value::Register(*value) == *key
    {
        return None;
    }
    let read_elsewhere = function.blocks.iter().enumerate().any(|(index, other)| {
        let mut operands = other
            .ops
            .iter()
            .flat_map(Op::operands)
            .chain(other.terminator.operands());
        BlockId(index) != found_block && operands.any(|operand| operand == &Value::Register(*value))
    });
    if read_elsewhere {
        return None;
    }

    Some(Fusion {
        test_block,
        test_at,
        found_block,
        read_at,
        absent_block,
        value: *value,
        answer: *answer,
        container: container.clone(),
        key: key.clone(),
    })
}

fn apply(function: &mut Function, fusion: &Fusion) {
    if let Some(block) = function.blocks.get_mut(fusion.test_block.index()) {
        block.ops[fusion.test_at] = Op::DictFind {
            dest: fusion.value,
            container: fusion.container.clone(),
            key: fusion.key.clone(),
        };
        block.ops.insert(
            fusion.test_at + 1,
            Op::IsNull {
                dest: fusion.answer,
                src: Value::Register(fusion.value),
            },
        );
        // the register now says *absent* where it said present, so the edges trade
        // places — and `not in` had already traded them, which is why this is the
        // same assignment either way
        block.terminator = Terminator::Branch {
            cond: Value::Register(fusion.answer),
            then_block: fusion.absent_block,
            else_block: fusion.found_block,
        };
    }
    if let Some(found) = function.blocks.get_mut(fusion.found_block.index()) {
        found.ops.remove(fusion.read_at);
    }

    // the alias the read held of the key was made for the read, so it goes with it
    // — otherwise it stays as a retain and a release of a value nothing looks at.
    // only a copy is dropped, and only where the register it wrote is now read
    // nowhere: the write was its only one, so the register simply never comes to
    // hold anything and the exit release it still gets is of nothing
    let reads = read_counts(function);
    let orphaned: Vec<usize> = function
        .block(fusion.found_block)
        .into_iter()
        .flat_map(|found| {
            found.ops[..fusion.read_at]
                .iter()
                .enumerate()
                .filter(|(_, op)| match op {
                    Op::Assign { dest, .. } => {
                        !reads.contains_key(dest)
                            && function.register(*dest).is_some_and(|it| it.name.is_none())
                    }
                    _ => false,
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        })
        .collect();
    if let Some(found) = function.blocks.get_mut(fusion.found_block.index()) {
        for index in orphaned.into_iter().rev() {
            found.ops.remove(index);
        }
    }
}

/// for each register written exactly once and written by a copy, what it copies
///
/// a register written more than once has no single answer, and one written by
/// anything else is where a chain of copies ends
fn sole_copies(
    function: &Function,
    writes: &HashMap<RegisterId, usize>,
) -> HashMap<RegisterId, Value> {
    let mut copies = HashMap::new();
    for block in &function.blocks {
        for op in &block.ops {
            if let Op::Assign { dest, src } = op
                && writes.get(dest).copied() == Some(1)
            {
                copies.insert(*dest, src.clone());
            }
        }
    }
    copies
}

/// what a value is ultimately a copy of, and every register the chain passed
/// through on the way — the value itself included, where it is a register
fn origin(value: &Value, copies: &HashMap<RegisterId, Value>) -> (Value, Vec<RegisterId>) {
    let mut value = value.clone();
    let mut visited = Vec::new();
    while let Value::Register(register) = value {
        if visited.contains(&register) {
            break;
        }
        visited.push(register);
        let Some(source) = copies.get(&register) else {
            break;
        };
        value = source.clone();
    }
    (value, visited)
}

/// how many times each register is read, over the whole function
fn read_counts(function: &Function) -> HashMap<RegisterId, usize> {
    let mut counts = HashMap::new();
    for block in &function.blocks {
        let operands = block
            .ops
            .iter()
            .flat_map(Op::operands)
            .chain(block.terminator.operands());
        for operand in operands {
            if let Value::Register(register) = operand {
                *counts.entry(*register).or_insert(0) += 1;
            }
        }
        // a `del` reads its register on the way to emptying it
        for register in block.ops.iter().filter_map(Op::unbinds) {
            *counts.entry(register).or_insert(0) += 1;
        }
    }
    counts
}

/// how many times each register is written, over the whole function
fn write_counts(function: &Function) -> HashMap<RegisterId, usize> {
    let mut counts = HashMap::new();
    // a parameter arrives written
    for index in 0..function.param_count {
        *counts.entry(RegisterId(index)).or_insert(0) += 1;
    }
    for block in &function.blocks {
        let written = block
            .ops
            .iter()
            .filter_map(Op::dest)
            .chain(block.ops.iter().filter_map(Op::unbinds))
            .chain(block.terminator.dest());
        for register in written {
            *counts.entry(register).or_insert(0) += 1;
        }
    }
    counts
}

/// how many edges arrive at each block, the exception edges included
fn predecessor_counts(function: &Function) -> HashMap<BlockId, usize> {
    let mut counts = HashMap::new();
    for block in &function.blocks {
        for successor in block.successors() {
            *counts.entry(successor).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::BinOp;
    use by_ir::rtype::RType;
    use by_ir::verify::verify;

    fn module(function: Function) -> ModuleIr {
        ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions: vec![function],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
            shims: None,
        }
    }

    /// `if k in d: return d[k]` else `return d`, as three blocks
    ///
    /// `negated` writes `not in` and swaps which edge the read is on, so the two
    /// spellings describe the same program
    fn guarded_read(negated: bool) -> Function {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let container = builder.param("d", RType::OBJECT);
        let key = builder.param("k", RType::OBJECT);
        let answer = builder.temp(RType::BIT);
        let value = builder.temp(RType::OBJECT);
        let found = builder.new_block();
        let absent = builder.new_block();

        builder.push(Op::Contains {
            dest: answer,
            value: Value::Register(key),
            container: Value::Register(container),
            negated,
        });
        let (then_block, else_block) = if negated {
            (absent, found)
        } else {
            (found, absent)
        };
        builder.terminate(Terminator::Branch {
            cond: Value::Register(answer),
            then_block,
            else_block,
        });

        builder.switch_to(found);
        builder.push(Op::GetItem {
            dest: value,
            container: Value::Register(container),
            index: Value::Register(key),
        });
        builder.terminate(Terminator::Return(Value::Register(value)));

        builder.switch_to(absent);
        builder.terminate(Terminator::Return(Value::Register(container)));
        builder.finish()
    }

    /// one more unnamed temporary on a function the builder has already finished
    fn extra_temp(function: &mut Function, ty: RType) -> RegisterId {
        let id = RegisterId(function.registers.len());
        function.registers.push(by_ir::function::RegisterDecl {
            name: None,
            ty,
            borrowed: false,
            may_be_unassigned: false,
        });
        id
    }

    fn fused(function: &Function) -> Option<&Op> {
        function
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .find(|op| matches!(op, Op::DictFind { .. }))
    }

    #[test]
    fn a_read_guarded_by_the_test_that_precedes_it_becomes_one_lookup() {
        let mut module = module(guarded_read(false));
        run(&mut module);
        let function = &module.functions[0];
        assert!(fused(function).is_some());
        // the test's own call is gone, and the answer is now a null check
        assert!(
            !function.blocks[0]
                .ops
                .iter()
                .any(|op| matches!(op, Op::Contains { .. }))
        );
        assert!(matches!(function.blocks[0].ops[1], Op::IsNull { .. }));
        // and the read is gone from the block it guarded
        assert!(function.blocks[1].ops.is_empty());
        assert_eq!(verify(function), Ok(()));
    }

    #[test]
    fn the_edges_trade_places_so_the_null_answer_leads_where_the_miss_did() {
        let mut module = module(guarded_read(false));
        run(&mut module);
        assert_eq!(
            module.functions[0].blocks[0].terminator,
            Terminator::Branch {
                cond: Value::Register(RegisterId(2)),
                then_block: BlockId(2),
                else_block: BlockId(1),
            }
        );
    }

    #[test]
    fn a_not_in_test_guards_the_other_edge_and_keeps_it() {
        let mut module = module(guarded_read(true));
        run(&mut module);
        let function = &module.functions[0];
        assert!(fused(function).is_some());
        // `not in` already sent the miss down the `then` edge, so nothing swaps
        assert_eq!(
            function.blocks[0].terminator,
            Terminator::Branch {
                cond: Value::Register(RegisterId(2)),
                then_block: BlockId(2),
                else_block: BlockId(1),
            }
        );
        assert_eq!(verify(function), Ok(()));
    }

    #[test]
    fn a_read_of_some_other_key_is_left_alone() {
        let mut function = guarded_read(false);
        let other = extra_temp(&mut function, RType::OBJECT);
        function.blocks[1].ops[0] = Op::GetItem {
            dest: RegisterId(3),
            container: Value::Register(RegisterId(0)),
            index: Value::Register(other),
        };
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn a_block_something_else_can_reach_is_left_alone() {
        let mut function = guarded_read(false);
        // the miss falls through into the read's block, so arriving there no longer
        // says the key was found
        function.blocks[2].terminator = Terminator::Goto(BlockId(1));
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn a_read_under_a_handler_of_its_own_is_left_alone() {
        let mut function = guarded_read(false);
        // an `except` around the subscript alone: moving the read out of it would
        // hand its exception to whatever encloses the `if` instead
        function.blocks[1].error_target = Some(BlockId(2));
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn an_answer_that_is_wanted_for_itself_is_left_alone() {
        let mut function = guarded_read(false);
        // the test's answer is returned as well as branched on, so it cannot be
        // given the opposite meaning
        function.blocks[2].terminator = Terminator::Return(Value::Register(RegisterId(2)));
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn an_operation_between_the_test_and_the_read_is_left_alone() {
        let mut function = guarded_read(false);
        let sum = extra_temp(&mut function, RType::OBJECT);
        // something that can raise stands in front of the read, so moving the read
        // in front of the branch would move it in front of this too
        function.blocks[1].ops.insert(
            0,
            Op::ObjectBinary {
                dest: sum,
                op: BinOp::Add,
                lhs: Value::Register(RegisterId(0)),
                rhs: Value::Register(RegisterId(1)),
                mutation: by_ir::ops::Mutation::Fresh,
            },
        );
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn a_copy_between_the_test_and_the_read_still_fuses() {
        let mut function = guarded_read(false);
        let copy = extra_temp(&mut function, RType::OBJECT);
        function.blocks[1].ops.insert(
            0,
            Op::Assign {
                dest: copy,
                src: Value::Register(RegisterId(0)),
            },
        );
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_some());
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn the_two_sides_holding_their_own_copy_of_one_key_still_fuse() {
        // the shape the frontend actually emits: a key read out of a container is
        // spilled into a fresh alias for each use, so the test and the read never
        // name the same register
        let mut function = guarded_read(false);
        let held = extra_temp(&mut function, RType::OBJECT);
        let again = extra_temp(&mut function, RType::OBJECT);
        function.blocks[0].ops.insert(
            0,
            Op::Assign {
                dest: held,
                src: Value::Register(RegisterId(1)),
            },
        );
        function.blocks[0].ops[1] = Op::Contains {
            dest: RegisterId(2),
            value: Value::Register(held),
            container: Value::Register(RegisterId(0)),
            negated: false,
        };
        function.blocks[1].ops.insert(
            0,
            Op::Assign {
                dest: again,
                src: Value::Register(RegisterId(1)),
            },
        );
        function.blocks[1].ops[1] = Op::GetItem {
            dest: RegisterId(3),
            container: Value::Register(RegisterId(0)),
            index: Value::Register(again),
        };
        let mut module = module(function);
        run(&mut module);
        let fused = fused(&module.functions[0]).cloned();
        // the lookup takes the copy that is live where the test was
        assert_eq!(
            fused,
            Some(Op::DictFind {
                dest: RegisterId(3),
                container: Value::Register(RegisterId(0)),
                key: Value::Register(held),
            })
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn two_copies_of_different_keys_are_left_alone() {
        let mut function = guarded_read(false);
        let held = extra_temp(&mut function, RType::OBJECT);
        let other = extra_temp(&mut function, RType::OBJECT);
        function.blocks[0].ops.insert(
            0,
            Op::Assign {
                dest: held,
                src: Value::Register(RegisterId(1)),
            },
        );
        function.blocks[0].ops[1] = Op::Contains {
            dest: RegisterId(2),
            value: Value::Register(held),
            container: Value::Register(RegisterId(0)),
            negated: false,
        };
        // a copy of the *container* is not a copy of the key, however alike the
        // two chains look
        function.blocks[1].ops.insert(
            0,
            Op::Assign {
                dest: other,
                src: Value::Register(RegisterId(0)),
            },
        );
        function.blocks[1].ops[1] = Op::GetItem {
            dest: RegisterId(3),
            container: Value::Register(RegisterId(0)),
            index: Value::Register(other),
        };
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn the_histogram_loop_fuses() {
        // the shape the frontend emits for `if word in seen: seen[word] = seen[word] + 1`,
        // down to the alias per use and the named `word` the two aliases copy
        let mut builder = FunctionBuilder::new("counted", RType::OBJECT);
        let seen = builder.local("seen", RType::OBJECT);
        let word = builder.local("word", RType::STR);
        let held = builder.temp(RType::OBJECT);
        let answer = builder.temp(RType::BIT);
        let again = builder.temp(RType::OBJECT);
        let value = builder.temp(RType::OBJECT);
        let found = builder.new_block();
        let absent = builder.new_block();
        let join = builder.new_block();

        builder.push(Op::BuildDict {
            dest: seen,
            pairs: Vec::new(),
        });
        builder.push(Op::Assign {
            dest: word,
            src: Value::Str("w".to_string()),
        });
        builder.push(Op::Assign {
            dest: held,
            src: Value::Register(word),
        });
        builder.push(Op::Contains {
            dest: answer,
            value: Value::Register(held),
            container: Value::Register(seen),
            negated: false,
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(answer),
            then_block: found,
            else_block: absent,
        });

        builder.switch_to(found);
        builder.push(Op::Assign {
            dest: again,
            src: Value::Register(word),
        });
        builder.push(Op::GetItem {
            dest: value,
            container: Value::Register(seen),
            index: Value::Register(again),
        });
        builder.terminate(Terminator::Goto(join));

        builder.switch_to(absent);
        builder.terminate(Terminator::Goto(join));

        builder.switch_to(join);
        builder.terminate(Terminator::Return(Value::Register(seen)));

        let mut module = module(builder.finish());
        run(&mut module);
        assert!(fused(&module.functions[0]).is_some());
        // the alias the read held of the key went with the read
        assert!(module.functions[0].blocks[1].ops.iter().all(|op| !matches!(
            op,
            Op::Assign {
                src: Value::Register(_),
                ..
            }
        )));
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_copy_that_rebinds_the_container_is_left_alone() {
        let mut function = guarded_read(false);
        // the name the read looks in is rebound before the read, so the lookup the
        // test would make is not the lookup the read makes
        function.blocks[1].ops.insert(
            0,
            Op::Assign {
                dest: RegisterId(0),
                src: Value::Register(RegisterId(1)),
            },
        );
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn a_value_wanted_after_the_join_is_left_alone() {
        let mut function = guarded_read(false);
        // the read's register is live past the block the read was in, where the
        // fused lookup would leave it holding nothing on the miss
        function.blocks[1].terminator = Terminator::Goto(BlockId(2));
        function.blocks[2].terminator = Terminator::Return(Value::Register(RegisterId(3)));
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }

    #[test]
    fn a_named_local_is_left_alone() {
        let mut function = guarded_read(false);
        // `v = d[k]` writes a name the source can read again, and a name python
        // leaves bound is not a place to put "nothing was found"
        if let Some(declaration) = function.registers.get_mut(3) {
            declaration.name = Some("v".to_string());
        }
        let mut module = module(function);
        run(&mut module);
        assert!(fused(&module.functions[0]).is_none());
    }
}
