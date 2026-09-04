//! write a result into its destination rather than into a temporary
//!
//! the lowering emits an expression's value into a fresh temporary and then
//! stores that temporary into the place the statement names. the two are
//! separate because an expression does not know what will be done with it — but
//! when the store is the temporary's only use and comes straight after, the
//! operation can name the destination itself.
//!
//! what that saves is a reference: a store *copies*, so it retains the new value
//! and releases the old, while an operation writing its own result only releases
//! the old. one retain per statement, in every loop in the program.

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{Op, RegisterId, Value};

pub(crate) fn run(module: &mut ModuleIr) {
    for function in &mut module.functions {
        coalesce(function);
    }
    for class in &mut module.classes {
        for method in &mut class.methods {
            coalesce(method);
        }
    }
}

fn coalesce(function: &mut Function) {
    let uses = use_counts(function);
    let param_count = function.param_count;
    for block in 0..function.blocks.len() {
        let mut index = 0;
        while index + 1 < function.blocks[block].ops.len() {
            let Some(target) = pairing(function, block, index, &uses, param_count) else {
                index += 1;
                continue;
            };
            let Some(dest) = function.blocks[block].ops[index].dest_mut() else {
                index += 1;
                continue;
            };
            *dest = target;
            function.blocks[block].ops.remove(index + 1);
        }
    }
}

/// the register the operation at `index` may write directly, if any
fn pairing(
    function: &Function,
    block: usize,
    index: usize,
    uses: &[usize],
    param_count: usize,
) -> Option<RegisterId> {
    let ops = &function.blocks[block].ops;
    // `del x` leaves its destination unbound rather than holding a result, so there is
    // nothing for a following store to take over. the named-local test below already
    // excludes it; this is the reason
    if ops[index].unbinds().is_some() {
        return None;
    }
    let temp = ops[index].dest()?;
    let Op::Assign {
        dest,
        src: Value::Register(src),
    } = ops[index + 1]
    else {
        return None;
    };
    if src != temp || dest == temp {
        return None;
    }
    // the store is the temporary's only use, so nothing else can observe the
    // register disappearing. a *named* register is a source local, which the
    // debugger and the closure environment both expect to keep existing
    if uses.get(temp.index()).copied() != Some(1)
        || temp.index() < param_count
        || function.register(temp)?.name.is_some()
    {
        return None;
    }
    // an operation writes its declared type, so the two have to be the same one
    if function.register(temp)?.ty != function.register(dest)?.ty {
        return None;
    }
    Some(dest)
}

/// how many times each register is read
fn use_counts(function: &Function) -> Vec<usize> {
    let mut counts = vec![0; function.registers.len()];
    let mut count = |value: &Value| {
        if let Value::Register(id) = value
            && let Some(slot) = counts.get_mut(id.index())
        {
            *slot += 1;
        }
    };
    for block in &function.blocks {
        for op in &block.ops {
            for operand in op.operands() {
                count(operand);
            }
        }
        for operand in block.terminator.operands() {
            count(operand);
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::ModuleIr;
    use by_ir::ops::{BinOp, Op, Terminator, Value};
    use by_ir::rtype::RType;

    fn module(function: by_ir::function::Function) -> ModuleIr {
        ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions: vec![function],
            classes: Vec::new(),
            declined: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
            shims: None,
        }
    }

    #[test]
    fn an_increment_writes_its_own_counter() {
        let mut builder = FunctionBuilder::new("count", RType::INT);
        let k = builder.local("k".to_string(), RType::INT);
        let temp = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: temp,
            op: BinOp::Add,
            lhs: Value::Register(k),
            rhs: Value::Int(1),
        });
        builder.assign(k, Value::Register(temp));
        builder.terminate(Terminator::Return(Value::Register(k)));
        let mut module = module(builder.finish());

        super::run(&mut module);

        let ops = &module.functions[0].blocks[0].ops;
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], Op::IntBinary { dest, .. } if dest == k));
    }

    #[test]
    fn a_temporary_read_twice_is_left_alone() {
        let mut builder = FunctionBuilder::new("twice", RType::INT);
        let k = builder.local("k".to_string(), RType::INT);
        let temp = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: temp,
            op: BinOp::Add,
            lhs: Value::Register(k),
            rhs: Value::Int(1),
        });
        builder.assign(k, Value::Register(temp));
        builder.terminate(Terminator::Return(Value::Register(temp)));
        let mut module = module(builder.finish());

        super::run(&mut module);

        assert_eq!(module.functions[0].blocks[0].ops.len(), 2);
    }

    #[test]
    fn a_named_register_keeps_its_own_store() {
        // a source local is what a closure environment and a debugger both expect
        // to find, so it is never the one that disappears
        let mut builder = FunctionBuilder::new("named", RType::INT);
        let k = builder.local("k".to_string(), RType::INT);
        let named = builder.local("t".to_string(), RType::INT);
        builder.push(Op::IntBinary {
            dest: named,
            op: BinOp::Add,
            lhs: Value::Register(k),
            rhs: Value::Int(1),
        });
        builder.assign(k, Value::Register(named));
        builder.terminate(Terminator::Return(Value::Register(k)));
        let mut module = module(builder.finish());

        super::run(&mut module);

        assert_eq!(module.functions[0].blocks[0].ops.len(), 2);
    }
}
