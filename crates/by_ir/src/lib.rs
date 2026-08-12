//! BIR — the basedpython intermediate representation
//!
//! a typed, block-structured register machine that sits between `.by` source and
//! emitted C. the design is in `docs/basedpython/development/compilation/ir.md`
//!
//! this crate deliberately depends on nothing from the checker: once a fact is
//! not in BIR, it does not exist, so anything a later pass needs has to be
//! recorded here rather than re-derived by reaching back into ty.

pub mod builder;
pub mod function;
pub mod ops;
pub mod print;
pub mod rtype;
pub mod unbound_locals;
pub mod verify;

pub use builder::FunctionBuilder;
pub use function::{
    BasicBlock, CallConvention, ClassIr, Declined, FieldDecl, Function, GradualUse, ModuleIr,
    RegisterDecl,
};
pub use ops::{BinOp, BlockId, CmpOp, Op, RegisterId, StandardError, Terminator, UnaryOp, Value};
pub use print::{print_function, print_module};
pub use rtype::{IntWidth, Primitive, RType};
pub use verify::{VerifyError, verify, verify_module};
