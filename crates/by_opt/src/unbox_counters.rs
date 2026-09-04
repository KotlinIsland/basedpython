//! giving a loop counter a machine-integer representation
//!
//! a tagged `int` pays, on every trip of a counting loop, a shortness test on each
//! operand of the comparison, another pair on the step, the step's overflow
//! computation, and two tests of the results. an `int64_t` pays a compare and a
//! checked add. on a scalar float loop that is 19% of the whole running time — the
//! largest single cost left in a loop whose arithmetic already matches what a C
//! compiler would emit for the same program written in C.
//!
//! what makes it sound is not the guard, which compares against an ordinary `int`
//! that may be any size, but the *step*: `__builtin_add_overflow` costs one
//! instruction and a branch that is never taken, and it is emitted unconditionally,
//! so the representation carries no obligation the frontend has to discharge.

use std::collections::HashMap;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BinOp, Op, RegisterId, Value};
use by_ir::rtype::{IntWidth, Primitive, RType};

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        let picks: Vec<(RegisterId, Vec<RegisterId>)> = candidates(function)
            .into_iter()
            // an earlier pick may have claimed this register as one of its steps, and a
            // register already unboxed must not be unboxed again
            .filter(|(register, _)| function.registers[register.0].ty == RType::INT)
            .collect();
        // every representation is settled before any boxing is decided: whether a read
        // needs its tagged value back depends on what the *other* operand ended up as,
        // and doing both in one pass would make that depend on register order
        for (register, steps) in &picks {
            retype(function, *register, steps);
        }
        for (register, steps) in &picks {
            box_remaining_reads(function, *register, steps);
        }
    }
}

/// how a register is written
enum Definition {
    /// `r = <literal>`
    Literal,
    /// `r = t`, where `t` came from stepping `r` by a literal
    Step(RegisterId),
}

/// the registers whose every definition admits the unboxed form, each with the
/// steps that feed it
///
/// no *use* can disqualify one. the counter was an `int` before the pass, so every
/// consumer of it already accepts an `int` — and boxing hands back exactly that.
/// only the writes decide, because only they bound the value
///
/// a *parameter* never qualifies: its representation is the calling convention's,
/// and the caller wrote it
fn candidates(function: &Function) -> Vec<(RegisterId, Vec<RegisterId>)> {
    let mut out = Vec::new();
    for index in function.param_count..function.registers.len() {
        let register = RegisterId(index);
        if function.registers[index].ty != RType::INT {
            continue;
        }
        if let Some(steps) = definitions(function, register) {
            out.push((register, steps));
        }
    }
    out
}

/// every write to `register`, when every one of them is a literal or a step
///
/// anything else — a call's result, a read of a field, a value from another
/// register — leaves the tagged representation as the only one that fits
fn definitions(function: &Function, register: RegisterId) -> Option<Vec<RegisterId>> {
    let mut out = Vec::new();
    for block in &function.blocks {
        for op in &block.ops {
            if op.dest() != Some(register) {
                continue;
            }
            // a buffer's length is a `Py_ssize_t` — it is already a machine integer, and
            // tagging it only to compare it against a counter is the tag going round in
            // a circle. `while i < len(a)` is where that shows
            if matches!(op, Op::ArrayLen { .. }) {
                out.push(Definition::Literal);
                continue;
            }
            let Op::Assign { src, .. } = op else {
                return None;
            };
            match src {
                Value::Int(_) => out.push(Definition::Literal),
                Value::Register(source) => out.push(Definition::Step(*source)),
                _ => return None,
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    // a step's source has to be a step *of this register*, and one nothing else
    // reads — otherwise unboxing it would change what that other reader sees
    let steps = out
        .iter()
        .filter_map(|definition| match definition {
            Definition::Step(source) => Some(*source),
            Definition::Literal => None,
        })
        .collect::<Vec<_>>();
    let counts = read_counts(function);
    for source in &steps {
        let stepped = step_operands(function, *source)?;
        if stepped != register || counts.get(source).copied().unwrap_or(0) != 1 {
            return None;
        }
    }
    Some(steps)
}

/// the register a step reads, when `source` is `lhs + <literal>` and nothing else
fn step_operands(function: &Function, source: RegisterId) -> Option<RegisterId> {
    let mut found = None;
    for block in &function.blocks {
        for op in &block.ops {
            if op.dest() != Some(source) {
                continue;
            }
            let Op::IntBinary {
                op: BinOp::Add | BinOp::Sub,
                lhs: Value::Register(lhs),
                rhs: Value::Int(_),
                ..
            } = op
            else {
                return None;
            };
            if found.is_some() {
                return None;
            }
            found = Some(*lhs);
        }
    }
    found
}

fn reads(value: &Value, register: RegisterId) -> bool {
    matches!(value, Value::Register(id) if *id == register)
}

/// how many ops read each register
fn read_counts(function: &Function) -> HashMap<RegisterId, usize> {
    let mut counts = HashMap::new();
    for block in &function.blocks {
        for op in &block.ops {
            for value in op.operands() {
                if let Value::Register(id) = value {
                    *counts.entry(*id).or_insert(0) += 1;
                }
            }
        }
        for value in block.terminator.operands() {
            if let Value::Register(id) = value {
                *counts.entry(*id).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// rewrite `register`, and the steps that feed it, to the machine representation
fn retype(function: &mut Function, register: RegisterId, steps: &[RegisterId]) {
    let width = RType::fixed(IntWidth::I64);
    function.registers[register.0].ty = width.clone();
    for step in steps {
        function.registers[step.0].ty = width.clone();
    }

    // the literals feeding the counter and its steps are immediates, and an
    // immediate carries its own representation
    for block in &mut function.blocks {
        for op in &mut block.ops {
            let touches = op
                .dest()
                .is_some_and(|dest| dest == register || steps.contains(&dest));
            if !touches {
                continue;
            }
            for value in op.operands_mut() {
                if let Value::Int(literal) = value {
                    *value = Value::Fixed(*literal);
                }
            }
        }
    }
}

/// give every read that is not the guard or the step its tagged value back
fn box_remaining_reads(function: &mut Function, register: RegisterId, steps: &[RegisterId]) {
    let mut boxed: Option<RegisterId> = None;
    for index in 0..function.blocks.len() {
        let mut rewritten = Vec::new();
        for (position, op) in function.blocks[index].ops.iter().enumerate() {
            // the guard is served: codegen compares an unboxed left operand against
            // either a tagged right one or an unboxed one. a *right* operand is served
            // only when the left is unboxed too, since that is the pair codegen reads as
            // two machine integers.
            //
            // anything else is served only if its *destination* was unboxed too, which
            // is what `i * i` is not — it reads the counter but produces an ordinary
            // `int`
            let served = match op {
                Op::IntCompare {
                    lhs: Value::Register(lhs),
                    ..
                } if *lhs == register => true,
                Op::IntCompare {
                    lhs: Value::Register(lhs),
                    rhs: Value::Register(rhs),
                    ..
                } if *rhs == register => function
                    .register(*lhs)
                    .is_some_and(|decl| matches!(decl.ty, RType::Primitive(Primitive::Fixed(_)))),
                // a scan reads the character at its counter and asks one question of
                // it. the fused comparison reads a machine index as readily as a
                // tagged one, so handing the counter back its tagged value here would
                // be a shift out and a shift straight back in, once per character
                Op::StrItemCompare {
                    index: Value::Register(index),
                    ..
                } if *index == register => true,
                // and a subscript names its element by an offset, which is the
                // number already in the register. handing the counter back its
                // tagged value here would be a shift out and a shift straight
                // back in, once per element
                Op::GetItem {
                    index: Value::Register(index),
                    ..
                } if *index == register => true,
                _ => false,
            } || op
                .dest()
                .is_some_and(|dest| dest == register || steps.contains(&dest));
            if served {
                continue;
            }
            if op.operands().iter().any(|value| reads(value, register)) {
                rewritten.push(position);
            }
        }
        let terminator_reads = function.blocks[index]
            .terminator
            .operands()
            .iter()
            .any(|value| reads(value, register));
        if rewritten.is_empty() && !terminator_reads {
            continue;
        }
        // one boxed copy serves the whole function: the counter's tagged value is
        // wanted at an exit, and an exit is reached once
        let target = match boxed {
            Some(id) => id,
            None => {
                let id = RegisterId(function.registers.len());
                function.registers.push(by_ir::function::RegisterDecl {
                    name: None,
                    ty: RType::INT,
                    borrowed: false,
                    may_be_unassigned: false,
                });
                boxed = Some(id);
                id
            }
        };
        for position in rewritten.iter().rev() {
            for value in function.blocks[index].ops[*position].operands_mut() {
                if let Value::Register(id) = value
                    && *id == register
                {
                    *value = Value::Register(target);
                }
            }
            function.blocks[index].ops.insert(
                *position,
                Op::Box {
                    dest: target,
                    src: Value::Register(register),
                },
            );
        }
        if terminator_reads {
            for value in function.blocks[index].terminator.operands_mut() {
                if let Value::Register(id) = value
                    && *id == register
                {
                    *value = Value::Register(target);
                }
            }
            function.blocks[index].ops.push(Op::Box {
                dest: target,
                src: Value::Register(register),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{CmpOp, Terminator};
    use by_ir::verify::verify;

    use super::*;

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

    /// a counter set to zero and then read once, by whatever `reader` pushes
    fn counted(reader: impl FnOnce(&mut FunctionBuilder, RegisterId, RegisterId)) -> ModuleIr {
        let mut builder = FunctionBuilder::new("scan", RType::BOOL);
        let text = builder.param("s", RType::STR);
        let counter = builder.local("i", RType::INT);
        builder.push(Op::Assign {
            dest: counter,
            src: Value::Int(0),
        });
        reader(&mut builder, text, counter);
        builder.terminate(Terminator::Return(Value::Bool(true)));
        let mut module = module(builder.finish());
        run(&mut module);
        module
    }

    fn boxes(module: &ModuleIr) -> usize {
        module.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter(|op| matches!(op, Op::Box { .. }))
            .count()
    }

    #[test]
    fn a_counter_that_only_indexes_a_character_comparison_is_never_boxed() {
        let module = counted(|builder, text, counter| {
            let answer = builder.temp(RType::BIT);
            builder.push(Op::StrItemCompare {
                dest: answer,
                op: CmpOp::Eq,
                container: Value::Register(text),
                index: Value::Register(counter),
                character: ' ',
            });
        });
        assert_eq!(boxes(&module), 0, "{}", by_ir::print::print_module(&module));
        assert_eq!(
            module.functions[0].registers[1].ty,
            RType::fixed(IntWidth::I64)
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    /// a subscript names the element at an offset, and the offset is the number the
    /// register already holds — so the counter keeps its machine representation all
    /// the way into the read, and codegen reaches `By_GetItemI64` rather than boxing
    #[test]
    fn a_counter_that_indexes_a_container_is_never_boxed() {
        let module = counted(|builder, text, counter| {
            let element = builder.temp(RType::OBJECT);
            builder.push(Op::GetItem {
                dest: element,
                container: Value::Register(text),
                index: Value::Register(counter),
            });
        });
        assert_eq!(boxes(&module), 0, "{}", by_ir::print::print_module(&module));
        assert_eq!(
            module.functions[0].registers[1].ty,
            RType::fixed(IntWidth::I64)
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    /// a *container* that happens to be the counter is not an index, so it is boxed
    /// as any other read would be
    #[test]
    fn a_counter_subscripted_as_the_container_still_gets_its_tagged_value_back() {
        let module = counted(|builder, _text, counter| {
            let element = builder.temp(RType::OBJECT);
            builder.push(Op::GetItem {
                dest: element,
                container: Value::Register(counter),
                index: Value::Int(0),
            });
        });
        assert_eq!(boxes(&module), 1, "{}", by_ir::print::print_module(&module));
    }

    /// the fused comparison is the only reader that takes a machine index. a
    /// character *read* has to produce the character as an object, and the index it
    /// is asked for is the tagged one
    #[test]
    fn a_counter_that_reads_a_character_out_still_gets_its_tagged_value_back() {
        let module = counted(|builder, text, counter| {
            let character = builder.temp(RType::STR);
            builder.push(Op::StrGetItem {
                dest: character,
                container: Value::Register(text),
                index: Value::Register(counter),
            });
        });
        assert_eq!(boxes(&module), 1, "{}", by_ir::print::print_module(&module));
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }
}
