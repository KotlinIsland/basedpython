//! drop the test before a field read that an earlier write or test already answered
//!
//! an instance `__new__` made without running `__init__` has none of the fields
//! `__init__` gives every other instance, so a read of one is preceded by a test that
//! raises `AttributeError` where the field is absent. but a field that is always
//! assigned cannot be taken away again once it has been — `del` of one is refused, and
//! only a field `__init__` might leave out can be deleted, whose read tests for itself.
//! so once a function has written a field of an object, or tested it, every later read
//! of that field of that same object finds it:
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
//! what it can rely on is only what held all through each of them

use std::collections::{HashMap, HashSet};

use by_ir::function::{BasicBlock, Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Value};

/// a field of the object a register holds, which the function knows the object has
type Known = (RegisterId, String);

/// each class's fields in layout order, with `None` for one this pass says nothing about
type Layouts = HashMap<String, Vec<Option<String>>>;

pub(crate) fn run(module: &mut ModuleIr) {
    // only a field nothing can take away again: an optional one can be deleted by
    // anything the function calls, and a field no instance lacks has nothing to prove
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
        prove(function, &layouts);
    }
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

/// `known` after `op` has run
fn step(known: &mut HashSet<Known>, op: &Op, layouts: &Layouts) {
    for register in rebinds(op) {
        known.retain(|(held, _)| *held != register);
    }
    known.extend(learned(op, layouts));
}

/// what holds at every point of a block, for a handler it can raise into
fn throughout(block: &BasicBlock, entry: &HashSet<Known>) -> HashSet<Known> {
    let rebound: HashSet<RegisterId> = block.ops.iter().flat_map(rebinds).collect();
    entry
        .iter()
        .filter(|(held, _)| !rebound.contains(held))
        .cloned()
        .collect()
}

fn prove(function: &mut Function, layouts: &Layouts) {
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
            for op in &block.ops {
                step(&mut known, op, layouts);
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
                flows.push((handler, throughout(block, &start)));
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
        for op in block.ops.drain(..) {
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
            step(&mut known, &op, layouts);
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
