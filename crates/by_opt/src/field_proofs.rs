//! drop the test before a field read that an earlier write or test already answered
//!
//! an instance `__new__` made without running `__init__` has none of the fields
//! `__init__` gives every other instance, so a read of one is preceded by a test that
//! raises `AttributeError` where the field is absent. once a function has written a field
//! of an object, or tested it, a later read of that field of that same object finds it —
//! until python code runs, because python code can `del` it again:
//!
//! ```python
//! def step(state: State) -> None:
//!     state.a = state.b + state.c   # `b` and `c` are tested
//!     state.b = state.a - state.c   # `a` was written and `c` tested: nothing is
//! ```
//!
//! "that same object" is a register that has not been written in between. a register
//! written again may hold another object, and one written on some path into a block is
//! as good as written. a handler is entered from the middle of the blocks it covers, so
//! what it can rely on is only what held all through each of them. what python code can
//! run is [`Effects`]' answer, which is the one every other pass that holds a fact across
//! operations asks

use std::collections::{HashMap, HashSet};

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Terminator, Value};

use crate::runs_python::{Effects, Held};

/// a field of the object a register holds, which the function knows the object has
type Known = (RegisterId, String);

/// each class's fields in layout order, with `None` for one this pass says nothing about
type Layouts = HashMap<String, Vec<Option<String>>>;

pub(crate) fn run(module: &mut ModuleIr) {
    let effects = Effects::of(module);
    // only a field a read tests: an optional one's read asks for itself, and a field no
    // instance lacks has nothing to prove
    let layouts: HashMap<String, Vec<Option<String>>> = module
        .classes
        .iter()
        .map(|class| {
            (
                class.name.clone(),
                class
                    .fields
                    .iter()
                    .map(|field| {
                        (!field.optional && field.tracks_absence()).then(|| field.name.clone())
                    })
                    .collect(),
            )
        })
        .collect();
    for function in module.all_functions_mut() {
        let runs = runs_python(&effects, function);
        prove(function, &layouts, &runs);
    }
}

/// for each operation of each block, whether python code can run while it does — and so
/// take away a field the function had seen. a block the analysis never reached is taken to
/// run it everywhere
fn runs_python(effects: &Effects, function: &Function) -> Vec<Vec<bool>> {
    let entries = Held::solve(
        function,
        Function::entry(),
        effects.held_at_entry(function),
        |_| true,
    );
    function
        .blocks
        .iter()
        .enumerate()
        .map(|(index, block)| match entries.get(&BlockId(index)) {
            Some(entry) => {
                let mut held = entry.clone();
                block
                    .ops
                    .iter()
                    .map(|op| {
                        let runs = effects.op_runs_python(function, op, &held);
                        held.step(function, op);
                        runs
                    })
                    .collect()
            }
            None => vec![true; block.ops.len()],
        })
        .collect()
}

/// whether control leaves a block through `terminator` without running python code: a
/// jump, or a test of a value the block already holds
fn leaves_quietly(terminator: &Terminator) -> bool {
    matches!(
        terminator,
        Terminator::Goto(_)
            | Terminator::Branch { .. }
            | Terminator::NarrowShort { .. }
            | Terminator::MachineStep { .. }
    )
}

/// what an operation lets the function know about fields afterwards
fn learned(op: &Op, layouts: &Layouts) -> Vec<Known> {
    let permanent = |class: &str, field: &str| {
        layouts
            .get(class)
            .is_some_and(|names| names.iter().flatten().any(|name| name == field))
    };
    match op {
        Op::SetField {
            receiver: Value::Register(receiver),
            class,
            field,
            ..
        }
        | Op::RequireField {
            receiver: Value::Register(receiver),
            class,
            field,
        } if permanent(class, field) => vec![(*receiver, field.clone())],
        Op::NewInstance {
            dest,
            class,
            fields,
        } => layouts
            .get(class)
            .into_iter()
            .flat_map(|names| names.iter().zip(fields))
            .filter_map(|(name, value)| value.as_ref().and(name.as_ref()))
            .map(|name| (*dest, name.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// the register an operation leaves holding something else, if any
fn rebinds(op: &Op) -> Vec<RegisterId> {
    op.dest()
        .into_iter()
        .chain(op.unbinds())
        .chain(op.loop_cursor())
        .collect()
}

/// `known` after `op` has run, where `runs` says whether python code can run while it does
///
/// what the operation itself writes or tests is learned after what python code took away:
/// a store releases the value it replaces only once the new one is in place, so a
/// finalizer that release runs sees the field written
fn step(known: &mut HashSet<Known>, op: &Op, runs: bool, layouts: &Layouts) {
    if runs {
        known.clear();
    }
    for register in rebinds(op) {
        known.retain(|(held, _)| *held != register);
    }
    known.extend(learned(op, layouts));
}

fn prove(function: &mut Function, layouts: &Layouts, runs: &[Vec<bool>]) {
    let count = function.blocks.len();
    if count == 0 {
        return;
    }
    // `None` is "not reached yet", which is the whole set for the intersection below
    let mut entry: Vec<Option<HashSet<Known>>> = vec![None; count];
    entry[0] = Some(HashSet::new());
    let mut changed = true;
    while changed {
        changed = false;
        for index in 0..count {
            let Some(start) = entry[index].clone() else {
                continue;
            };
            let block = &function.blocks[index];
            let mut known = start.clone();
            // what held at every point of the block, for a handler it can raise into
            let mut throughout = start.clone();
            for (at, op) in block.ops.iter().enumerate() {
                step(&mut known, op, runs[index][at], layouts);
                throughout.retain(|fact| known.contains(fact));
            }
            if !leaves_quietly(&block.terminator) {
                known.clear();
            }
            // the exception edge is the last of the successors, and it carries only what
            // held all through the block
            let mut normal = block.successors();
            if block.error_target.is_some() {
                normal.pop();
            }
            let mut flows: Vec<(BlockId, HashSet<Known>)> = normal
                .into_iter()
                .map(|successor| (successor, known.clone()))
                .collect();
            if let Some(handler) = block.error_target {
                flows.push((handler, throughout));
            }
            for (successor, facts) in flows {
                let Some(slot) = entry.get_mut(successor.index()) else {
                    continue;
                };
                let merged = match slot {
                    None => facts,
                    Some(existing) => existing.intersection(&facts).cloned().collect(),
                };
                if slot.as_ref() != Some(&merged) {
                    *slot = Some(merged);
                    changed = true;
                }
            }
        }
    }

    for (index, block) in function.blocks.iter_mut().enumerate() {
        let Some(mut known) = entry[index].clone() else {
            continue;
        };
        let mut kept = Vec::with_capacity(block.ops.len());
        for (at, op) in block.ops.drain(..).enumerate() {
            let op = match op {
                Op::RequireField {
                    receiver: Value::Register(receiver),
                    ref field,
                    ..
                } if known.contains(&(receiver, field.clone())) => continue,
                Op::FieldIsSet {
                    dest,
                    receiver: Value::Register(receiver),
                    ref field,
                    ..
                } if known.contains(&(receiver, field.clone())) => Op::Assign {
                    dest,
                    src: Value::Bit(true),
                },
                Op::SetField {
                    receiver: receiver @ Value::Register(held),
                    class,
                    field,
                    value,
                    moves,
                    present: false,
                } if known.contains(&(held, field.clone())) => Op::SetField {
                    receiver,
                    class,
                    field,
                    value,
                    moves,
                    present: true,
                },
                other => other,
            };
            step(&mut known, &op, runs[index][at], layouts);
            kept.push(op);
        }
        block.ops = kept;
    }
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::{ClassIr, FieldDecl, ModuleIr};
    use by_ir::ops::{BinOp, Mutation, Op, Terminator, Value};
    use by_ir::rtype::RType;

    fn class(fields: &[(&str, bool)]) -> ClassIr {
        ClassIr {
            name: "State".to_string(),
            immutable: false,
            environment: false,
            mutable: false,
            unchecked_licences: false,
            exported: true,
            base: None,
            inherited_init: false,
            fields_are_parameters: false,
            dataclass: false,
            generic: false,
            slots: None,
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            fields: fields
                .iter()
                .map(|(name, optional)| FieldDecl {
                    cell: false,
                    name: (*name).to_string(),
                    ty: RType::INT,
                    default: None,
                    optional: *optional,
                    defaulted_by: None,
                })
                .collect(),
            decorators: Vec::new(),
            methods: Vec::new(),
            resume: None,
            keywords: Vec::new(),
        }
    }

    fn state() -> RType {
        RType::Instance {
            class: "State".to_string(),
            exact: false,
        }
    }

    fn require(receiver: by_ir::ops::RegisterId, field: &str) -> Op {
        Op::RequireField {
            receiver: Value::Register(receiver),
            class: "State".to_string(),
            field: field.to_string(),
        }
    }

    fn read(dest: by_ir::ops::RegisterId, receiver: by_ir::ops::RegisterId, field: &str) -> Op {
        Op::GetField {
            dest,
            receiver: Value::Register(receiver),
            class: "State".to_string(),
            field: field.to_string(),
        }
    }

    fn requires(module: &ModuleIr) -> usize {
        module.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter(|op| matches!(op, Op::RequireField { .. }))
            .count()
    }

    #[test]
    fn a_second_read_of_a_tested_field_is_not_tested_again() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let s = builder.param("s", state());
        let first = builder.temp(RType::INT);
        let second = builder.temp(RType::INT);
        let sum = builder.temp(RType::INT);
        builder.push(require(s, "b"));
        builder.push(read(first, s, "b"));
        builder.push(require(s, "b"));
        builder.push(read(second, s, "b"));
        builder.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(first),
            rhs: Value::Register(second),
            mutation: Mutation::Fresh,
        });
        builder.terminate(Terminator::Return(Value::Register(sum)));
        let mut module = ModuleIr::new("app");
        module.classes.push(class(&[("b", false)]));
        module.functions.push(builder.finish());

        super::run(&mut module);

        assert_eq!(requires(&module), 1);
    }

    #[test]
    fn a_written_field_is_not_tested_and_not_marked_again() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let s = builder.param("s", state());
        let value = builder.param("v", RType::INT);
        let out = builder.temp(RType::INT);
        let store = |present| Op::SetField {
            receiver: Value::Register(s),
            class: "State".to_string(),
            field: "a".to_string(),
            value: Value::Register(value),
            moves: false,
            present,
        };
        builder.push(store(false));
        builder.push(require(s, "a"));
        builder.push(read(out, s, "a"));
        builder.push(store(false));
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut module = ModuleIr::new("app");
        module.classes.push(class(&[("a", false)]));
        module.functions.push(builder.finish());

        super::run(&mut module);

        let ops = &module.functions[0].blocks[0].ops;
        assert_eq!(requires(&module), 0, "{ops:?}");
        assert!(
            matches!(ops[0], Op::SetField { present: false, .. }),
            "{ops:?}"
        );
        assert!(
            matches!(ops[2], Op::SetField { present: true, .. }),
            "{ops:?}"
        );
    }

    #[test]
    fn a_field_is_tested_again_once_python_code_can_have_run() {
        // python code can `del` a field the function had seen, so a call into it ends what
        // the function knows — while a store keeps what it wrote, which is in place before
        // the value it replaced is let go of
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let s = builder.param("s", state());
        let value = builder.param("v", RType::INT);
        let out = builder.temp(RType::INT);
        let called = builder.temp(RType::OBJECT);
        builder.push(require(s, "a"));
        builder.push(Op::CallPython {
            dest: called,
            callee: "hook".to_string(),
            args: vec![Value::Register(s)],
        });
        builder.push(require(s, "a"));
        builder.push(Op::SetField {
            receiver: Value::Register(s),
            class: "State".to_string(),
            field: "b".to_string(),
            value: Value::Register(value),
            moves: false,
            present: false,
        });
        builder.push(require(s, "a"));
        builder.push(require(s, "b"));
        builder.push(read(out, s, "a"));
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut module = ModuleIr::new("app");
        module.classes.push(class(&[("a", false), ("b", false)]));
        module.functions.push(builder.finish());

        super::run(&mut module);

        let ops = &module.functions[0].blocks[0].ops;
        assert_eq!(requires(&module), 3, "{ops:?}");
        assert!(
            matches!(&ops[4], Op::RequireField { field, .. } if field == "a"),
            "{ops:?}"
        );
    }

    #[test]
    fn a_rebound_receiver_or_an_optional_field_is_tested_again() {
        // the register may hold another object once written, and an optional field can be
        // deleted by anything in between
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let s = builder.param("s", state());
        let t = builder.param("t", state());
        let held = builder.local("held", state());
        let out = builder.temp(RType::INT);
        builder.assign(held, Value::Register(s));
        builder.push(require(held, "a"));
        builder.assign(held, Value::Register(t));
        builder.push(require(held, "a"));
        builder.push(require(held, "maybe"));
        builder.push(require(held, "maybe"));
        builder.push(read(out, held, "a"));
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut module = ModuleIr::new("app");
        module.classes.push(class(&[("a", false), ("maybe", true)]));
        module.functions.push(builder.finish());

        super::run(&mut module);

        assert_eq!(requires(&module), 4);
    }

    #[test]
    fn a_test_in_a_loop_body_is_not_answered_by_the_trip_before() {
        // the first trip reaches the test with nothing known, so it stays
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let s = builder.param("s", state());
        let more = builder.param("more", RType::BIT);
        let out = builder.temp(RType::INT);
        let body = builder.new_block();
        let done = builder.new_block();
        builder.terminate(Terminator::Goto(body));
        builder.switch_to(body);
        builder.push(require(s, "a"));
        builder.push(read(out, s, "a"));
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: done,
        });
        builder.switch_to(done);
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut module = ModuleIr::new("app");
        module.classes.push(class(&[("a", false)]));
        module.functions.push(builder.finish());

        super::run(&mut module);

        assert_eq!(requires(&module), 1);
    }
}
