//! asking what a character *is* without building it
//!
//! `s[i] == " "` reads a character of a `str` and asks one question of it. the
//! reading is where the cost is: a character of a `str` is a `str`, so the answer
//! goes through an object that exists for the length of one comparison and is
//! never looked at again. on a scan of a line that is two thirds of the running
//! time — more than the loop counter, the length and the indexing put together.
//!
//! the fusion rests on a fact about the *right-hand side* rather than about the
//! read: a `str` compares by code point, so a right-hand side of exactly one code
//! point turns the comparison into a question about a single code point — and an
//! exact `str` holds its code points directly. nothing about the character has to
//! be represented for the comparison to be answered.
//!
//! ## why the read cannot simply be given a code point instead
//!
//! the tempting rule is that a character of a `str` *is* a code point, and that
//! [`Op::StrGetItem`] should produce one, boxed back at whatever reads want an
//! object — which is what [`unbox_counters`](crate::unbox_counters) does for a
//! loop counter. it does not hold. `s` may be a subclass that has overridden
//! `__getitem__`, and what that hands back is only known to be a `str`: it may be
//! of no code points, or of several, and it may have overridden `__eq__` besides.
//! a character read has no code-point representation in general, so the code point
//! belongs to the *comparison*, which has an object path to fall back to, and not
//! to the read, which has nowhere to put one.
//!
//! ## why the two have to be adjacent
//!
//! the fused form does the reading, so the reading moves to where the comparison
//! is. `s[i]` raises `IndexError`, and moving that past anything that can raise or
//! be observed would move the exception with it. adjacency in a block is the
//! condition under which there is nothing to move it past — and it is also what
//! "the character's whole life is this comparison" means positionally. a character
//! read whose value outlives its comparison is left alone, because then the object
//! is wanted for its own sake.

use std::collections::HashMap;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{CmpOp, Op, RegisterId, Value};

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        fuse(function);
    }
}

fn fuse(function: &mut Function) {
    let reads = read_counts(function);
    for block in &mut function.blocks {
        let mut position = 0;
        while position + 1 < block.ops.len() {
            let Some(fused) = fusion(&block.ops[position], &block.ops[position + 1], &reads) else {
                position += 1;
                continue;
            };
            block.ops[position] = fused;
            block.ops.remove(position + 1);
            position += 1;
        }
    }
}

/// the fused operation for a character read immediately followed by its only
/// reader, when that reader compares it against a one-code-point literal
fn fusion(read: &Op, compare: &Op, reads: &HashMap<RegisterId, usize>) -> Option<Op> {
    let Op::StrGetItem {
        dest: character,
        container,
        index,
    } = read
    else {
        return None;
    };
    let Op::StrCompare { dest, op, lhs, rhs } = compare else {
        return None;
    };
    // the comparison is the character's only reader, so nothing else can want the
    // object — and the read itself must not be what the comparison writes back to
    if reads.get(character).copied().unwrap_or(0) != 1 || dest == character {
        return None;
    }
    // whichever side the read is on, the other has to be the literal — and reading
    // the literal on the left means asking the mirrored question
    let (op, literal) = match (lhs, rhs) {
        (Value::Register(read), literal) if read == character => (*op, literal),
        (literal, Value::Register(read)) if read == character => (mirrored(*op), literal),
        _ => return None,
    };
    let Value::Str(text) = literal else {
        return None;
    };
    Some(Op::StrItemCompare {
        dest: *dest,
        op,
        container: container.clone(),
        index: index.clone(),
        character: sole_character(text)?,
    })
}

/// the single code point of a text that is one code point long
fn sole_character(text: &str) -> Option<char> {
    let mut characters = text.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) => Some(character),
        _ => None,
    }
}

/// the same question with the operands the other way round
const fn mirrored(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

/// how many times each register is read, over the whole function
fn read_counts(function: &Function) -> HashMap<RegisterId, usize> {
    let mut counts = HashMap::new();
    let operands = function.blocks.iter().flat_map(|block| {
        block
            .ops
            .iter()
            .flat_map(Op::operands)
            .chain(block.terminator.operands())
    });
    for operand in operands {
        if let Value::Register(register) = operand {
            *counts.entry(*register).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::Terminator;
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
        }
    }

    /// `s[i] <op> <rhs>`, with the character read written to a register of its own
    ///
    /// `mirror` puts the read on the right of the comparison instead of the left
    fn compare(op: CmpOp, rhs: Value, mirror: bool) -> ModuleIr {
        let mut builder = FunctionBuilder::new("f", RType::BOOL);
        let text = builder.param("s", RType::STR);
        let index = builder.param("i", RType::INT);
        let character = builder.temp(RType::STR);
        let answer = builder.temp(RType::BIT);
        builder.push(Op::StrGetItem {
            dest: character,
            container: Value::Register(text),
            index: Value::Register(index),
        });
        let read = Value::Register(character);
        let (lhs, rhs) = if mirror { (rhs, read) } else { (read, rhs) };
        builder.push(Op::StrCompare {
            dest: answer,
            op,
            lhs,
            rhs,
        });
        builder.terminate(Terminator::Return(Value::Register(answer)));
        let mut module = module(builder.finish());
        run(&mut module);
        module
    }

    fn fused(module: &ModuleIr) -> Option<(CmpOp, char)> {
        module.functions[0].blocks[0]
            .ops
            .iter()
            .find_map(|op| match op {
                Op::StrItemCompare { op, character, .. } => Some((*op, *character)),
                _ => None,
            })
    }

    #[test]
    fn a_character_compared_against_a_one_character_literal_fuses() {
        let module = compare(CmpOp::Eq, Value::Str(" ".to_string()), false);
        assert_eq!(fused(&module), Some((CmpOp::Eq, ' ')));
        assert_eq!(module.functions[0].blocks[0].ops.len(), 1);
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_literal_of_one_astral_code_point_fuses() {
        // one code point, three `str` characters if anyone counted utf-8 bytes and
        // two if anyone counted utf-16 units — neither of which is what a `str` is
        let module = compare(CmpOp::Ne, Value::Str("🎉".to_string()), false);
        assert_eq!(fused(&module), Some((CmpOp::Ne, '🎉')));
    }

    #[test]
    fn a_combining_sequence_is_two_code_points_and_does_not_fuse() {
        let module = compare(CmpOp::Eq, Value::Str("e\u{301}".to_string()), false);
        assert_eq!(fused(&module), None);
    }

    #[test]
    fn an_empty_literal_does_not_fuse() {
        let module = compare(CmpOp::Eq, Value::Str(String::new()), false);
        assert_eq!(fused(&module), None);
    }

    #[test]
    fn a_literal_on_the_left_asks_the_mirrored_question() {
        let module = compare(CmpOp::Lt, Value::Str("m".to_string()), true);
        assert_eq!(fused(&module), Some((CmpOp::Gt, 'm')));
    }

    #[test]
    fn a_character_compared_against_another_register_does_not_fuse() {
        let module = compare(CmpOp::Eq, Value::Register(RegisterId(1)), false);
        assert_eq!(fused(&module), None);
    }

    #[test]
    fn a_character_read_by_anything_else_keeps_its_object() {
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let text = builder.param("s", RType::STR);
        let index = builder.param("i", RType::INT);
        let character = builder.temp(RType::STR);
        let answer = builder.temp(RType::BIT);
        builder.push(Op::StrGetItem {
            dest: character,
            container: Value::Register(text),
            index: Value::Register(index),
        });
        builder.push(Op::StrCompare {
            dest: answer,
            op: CmpOp::Eq,
            lhs: Value::Register(character),
            rhs: Value::Str(" ".to_string()),
        });
        // the character outlives the comparison, so the object is wanted
        builder.terminate(Terminator::Return(Value::Register(character)));

        let mut module = module(builder.finish());
        run(&mut module);
        assert_eq!(fused(&module), None);
    }

    #[test]
    fn a_comparison_that_is_not_next_to_its_read_keeps_its_object() {
        let mut builder = FunctionBuilder::new("f", RType::BOOL);
        let text = builder.param("s", RType::STR);
        let index = builder.param("i", RType::INT);
        let character = builder.temp(RType::STR);
        let length = builder.temp(RType::INT);
        let answer = builder.temp(RType::BIT);
        builder.push(Op::StrGetItem {
            dest: character,
            container: Value::Register(text),
            index: Value::Register(index),
        });
        // something that can raise stands between them, so fusing would move the
        // read's own `IndexError` past it
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(text),
        });
        builder.push(Op::StrCompare {
            dest: answer,
            op: CmpOp::Eq,
            lhs: Value::Register(character),
            rhs: Value::Str(" ".to_string()),
        });
        builder.terminate(Terminator::Return(Value::Register(answer)));

        let mut module = module(builder.finish());
        run(&mut module);
        assert_eq!(fused(&module), None);
    }
}
