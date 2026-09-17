//! narrowings a test standing over them has already made
//!
//! two lowerings ask a question and then, on the arm that got a yes, ask it again. a
//! class pattern tests the subject's layout before reading a field at its offset, and
//! then narrows the subject to that layout — which checks the type a second time. a call
//! through a base-typed name tests whether the method still stands, which compares the
//! receiver's type exactly, and then narrows the receiver to that class — again a second
//! test.
//!
//! neither second test can answer differently from the first, so what is left of the
//! narrowing is the static type. the check goes and the value keeps its representation.
//!
//! the proof is given up at the first operation that could run python. python code can
//! reassign an object's `__class__`, and a release is python code: a finalizer runs
//! wherever the last reference to something goes. so the window the proof covers is the
//! one in which nothing at all can have looked at the object.

use std::collections::HashMap;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Terminator, Value};
use by_ir::rtype::RType;

use crate::runs_python::{Effects, Held};

/// what a test standing over a block proved about the object it asked about
#[derive(Clone, Copy, PartialEq, Eq)]
enum Proof {
    /// the object's type *is* the class: what `By_MethodStands` compares
    Exact,
    /// the object is an instance of the class, a subclass of it included: what
    /// `PyObject_TypeCheck` answers, and so what `Op::HoldsLayout` proves
    Instance,
}

impl Proof {
    /// whether it licenses a narrowing to this class
    fn licenses(self, exact: bool) -> bool {
        self == Self::Exact || !exact
    }
}

pub(crate) fn run(module: &mut ModuleIr) {
    let effects = Effects::of(module);
    for function in module.all_functions_mut() {
        prove(&effects, function);
    }
}

fn prove(effects: &Effects, function: &mut Function) {
    let entries = Held::solve(
        function,
        Function::entry(),
        effects.held_at_entry(function),
        |_| true,
    );
    let proofs = proved_on_entry(effects, function, &entries);
    let mut rewrites: Vec<(usize, usize)> = Vec::new();
    for (index, block) in function.blocks.iter().enumerate() {
        let (Some((object, class, proof)), Some(entry)) =
            (proofs.get(&index), entries.get(&BlockId(index)))
        else {
            continue;
        };
        let mut known = entry.clone();
        for (at, op) in block.ops.iter().enumerate() {
            if let Op::Unbox {
                src: Value::Register(source),
                to: RType::Instance { class: to, exact },
                proved: false,
                ..
            } = op
                && source == object
                && to == class
                && proof.licenses(*exact)
            {
                rewrites.push((index, at));
            }
            // the operation's own use of the object is the one the proof covers; what it
            // can do afterwards is what ends the proof for everything behind it
            if effects.op_runs_python(function, op, &known) || writes(op, *object) {
                break;
            }
            known.step(function, op);
        }
    }
    for (block, at) in rewrites {
        if let Some(Op::Unbox { proved, .. }) = function.blocks[block].ops.get_mut(at) {
            *proved = true;
        }
    }
}

/// what holds on entry to each block that is reached only by a test saying yes
///
/// only a block with that one way in: a block some other edge also reaches is entered
/// without the test having been asked
fn proved_on_entry(
    effects: &Effects,
    function: &Function,
    entries: &HashMap<BlockId, Held>,
) -> HashMap<usize, (RegisterId, String, Proof)> {
    let mut ways_in: Vec<usize> = vec![0; function.blocks.len()];
    for block in &function.blocks {
        for successor in block.successors() {
            if let Some(count) = ways_in.get_mut(successor.index()) {
                *count += 1;
            }
        }
    }
    let mut proofs = HashMap::new();
    for (index, block) in function.blocks.iter().enumerate() {
        let Terminator::Branch {
            cond: Value::Register(cond),
            then_block,
            ..
        } = &block.terminator
        else {
            continue;
        };
        // the entry is entered by the call as well as by any edge, so a count of one
        // does not mean one way in
        if *then_block == Function::entry()
            || then_block.index() == index
            || ways_in.get(then_block.index()).copied() != Some(1)
        {
            continue;
        }
        let Some((at, proof)) = block.ops.iter().enumerate().rev().find_map(|(at, op)| {
            let proof = match op {
                Op::HoldsLayout {
                    dest,
                    src: Value::Register(src),
                    class,
                } if dest == cond => (*src, class.clone(), Proof::Instance),
                Op::MethodStands {
                    dest,
                    src: Value::Register(src),
                    class,
                    ..
                } if dest == cond => (*src, class.clone(), Proof::Exact),
                _ => return None,
            };
            Some((at, proof))
        }) else {
            continue;
        };
        // whatever the block does after asking can have run python, and then the answer
        // is no longer the answer
        let Some(entry) = entries.get(&BlockId(index)) else {
            continue;
        };
        let mut known = entry.clone();
        let mut stale = false;
        for (position, op) in block.ops.iter().enumerate() {
            if position > at
                && (effects.op_runs_python(function, op, &known) || writes(op, proof.0))
            {
                stale = true;
                break;
            }
            known.step(function, op);
        }
        if !stale {
            proofs.insert(then_block.index(), proof);
        }
    }
    proofs
}

/// whether `op` leaves `register` holding something other than what it held
fn writes(op: &Op, register: RegisterId) -> bool {
    op.dest()
        .into_iter()
        .chain(op.unbinds())
        .chain(op.loop_cursor())
        .any(|written| written == register)
        || matches!(op, Op::Move { src: Value::Register(moved), .. } if *moved == register)
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::{Function, ModuleIr};
    use by_ir::ops::{Op, Terminator, Value};
    use by_ir::rtype::RType;

    /// `if the subject is laid out as P: read it as a P`, with `extra` in front of the
    /// narrowing and `another_way_in` deciding whether some other edge also reaches the
    /// block the test says yes to
    fn tested(extra: Option<Op>, another_way_in: bool) -> Function {
        let laid_out = RType::Instance {
            class: "P".to_string(),
            exact: false,
        };
        let mut builder = FunctionBuilder::new("pick", RType::OBJECT);
        let subject = builder.param("subject", RType::OBJECT);
        let holds = builder.temp(RType::BIT);
        let narrowed = builder.temp(laid_out.clone());
        let out = builder.local("out", RType::OBJECT);
        let native = builder.new_block();
        let generic = builder.new_block();
        let join = builder.new_block();
        builder.push(Op::HoldsLayout {
            dest: holds,
            src: Value::Register(subject),
            class: "P".to_string(),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(holds),
            then_block: native,
            else_block: generic,
        });

        builder.switch_to(native);
        if let Some(extra) = extra {
            builder.push(extra);
        }
        builder.push(Op::Unbox {
            dest: narrowed,
            src: Value::Register(subject),
            to: laid_out,
            proved: false,
        });
        builder.assign(out, Value::Register(narrowed));
        builder.terminate(Terminator::Goto(join));

        builder.switch_to(generic);
        builder.assign(out, Value::Register(subject));
        builder.terminate(if another_way_in {
            Terminator::Goto(native)
        } else {
            Terminator::Goto(join)
        });

        builder.switch_to(join);
        builder.terminate(Terminator::Return(Value::Register(out)));
        builder.finish()
    }

    fn run_on(function: Function) -> Function {
        let mut module = ModuleIr::new("app");
        module.functions = vec![function];
        super::run(&mut module);
        module.functions.remove(0)
    }

    fn narrowings_proved(function: &Function) -> usize {
        function
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter(|op| matches!(op, Op::Unbox { proved: true, .. }))
            .count()
    }

    #[test]
    fn a_narrowing_the_test_above_it_already_made_is_proved() {
        assert_eq!(narrowings_proved(&run_on(tested(None, false))), 1);
    }

    #[test]
    fn a_block_some_other_edge_reaches_proves_nothing() {
        // the other edge is entered without the test having been asked, so nothing on
        // the block holds because the test said yes
        assert_eq!(narrowings_proved(&run_on(tested(None, true))), 0);
    }

    #[test]
    fn an_operation_that_can_run_python_gives_the_proof_up() {
        // python code can reassign an object's `__class__`, so a namespace lookup
        // between the test and the narrowing leaves the narrowing to check for itself
        let lookup = Op::LoadGlobal {
            dest: by_ir::ops::RegisterId(0),
            name: "anything".to_string(),
        };
        assert_eq!(narrowings_proved(&run_on(tested(Some(lookup), false))), 0);
    }
}
