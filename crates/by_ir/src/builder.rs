//! a builder for well-formed functions
//!
//! the builder exists so that constructing BIR does not require getting index
//! bookkeeping right by hand. it hands out typed [`RegisterId`]s and [`BlockId`]s
//! and refuses to finish a block twice, which removes the two mistakes that would
//! otherwise account for most verifier failures.

use crate::function::{BasicBlock, CallConvention, Decorator, Function, RegisterDecl};
use crate::ops::{BlockId, Op, RegisterId, Terminator, Value};
use crate::rtype::RType;

/// builds one function
pub struct FunctionBuilder {
    name: String,
    param_count: usize,
    ret: RType,
    convention: CallConvention,
    exported: bool,
    owner: Option<String>,
    decorators: Vec<Decorator>,
    registers: Vec<RegisterDecl>,
    /// `None` until the block is sealed with a terminator
    blocks: Vec<Option<BasicBlock>>,
    pending: Vec<Vec<Op>>,
    current: BlockId,
    /// where a failing operation in a block sealed from now on should jump
    error_target: Option<BlockId>,
    /// the definition's own `.by` span
    range: Option<(u32, u32)>,
    /// the default for each parameter, `None` where it has none
    defaults: Vec<Option<Value>>,
    vararg: bool,
    kwarg: bool,
    deferring: Vec<usize>,
    computed_defaults: Vec<usize>,
    posonly: usize,
    kwonly: usize,
    /// per-block `.by` spans, parallel to `blocks`
    block_ranges: Vec<Option<(u32, u32)>>,
}

impl FunctionBuilder {
    /// start a function. the entry block is created and made current
    pub fn new(name: impl Into<String>, ret: RType) -> Self {
        Self {
            name: name.into(),
            param_count: 0,
            ret,
            convention: CallConvention::Native,
            exported: true,
            owner: None,
            decorators: Vec::new(),
            registers: Vec::new(),
            blocks: vec![None],
            pending: vec![Vec::new()],
            current: BlockId(0),
            error_target: None,
            range: None,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            posonly: 0,
            kwonly: 0,
            block_ranges: vec![None],
        }
    }

    pub fn convention(&mut self, convention: CallConvention) -> &mut Self {
        self.convention = convention;
        self
    }

    pub fn exported(&mut self, exported: bool) -> &mut Self {
        self.exported = exported;
        self
    }

    /// decorators to apply at module init, outermost first
    pub fn decorators(&mut self, decorators: Vec<Decorator>) -> &mut Self {
        self.decorators = decorators;
        self
    }

    /// declare a parameter. every parameter must be added before any local, since
    /// parameters are the leading registers
    pub fn param(&mut self, name: impl Into<String>, ty: RType) -> RegisterId {
        debug_assert_eq!(
            self.param_count,
            self.registers.len(),
            "parameters must be declared before locals"
        );
        self.param_count += 1;
        self.declare(Some(name.into()), ty)
    }

    /// declare a named local
    pub fn local(&mut self, name: impl Into<String>, ty: RType) -> RegisterId {
        self.declare(Some(name.into()), ty)
    }

    /// declare an unnamed temporary
    pub fn temp(&mut self, ty: RType) -> RegisterId {
        self.declare(None, ty)
    }

    fn declare(&mut self, name: Option<String>, ty: RType) -> RegisterId {
        let id = RegisterId(self.registers.len());
        self.registers.push(RegisterDecl {
            name,
            ty,
            borrowed: false,
            may_be_unassigned: false,
        });
        id
    }

    /// the type a register was declared with
    pub fn register_type(&self, id: RegisterId) -> Option<&RType> {
        self.registers.get(id.index()).map(|decl| &decl.ty)
    }

    /// reserve a block to be filled in later
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len());
        self.blocks.push(None);
        self.pending.push(Vec::new());
        self.block_ranges.push(None);
        id
    }

    /// make `block` current, so subsequent ops append to it
    pub fn switch_to(&mut self, block: BlockId) {
        self.current = block;
    }

    /// record where the definition came from, for `#line`
    pub fn at(&mut self, range: (u32, u32)) {
        self.range = Some(range);
    }

    /// the defaults, one entry per parameter
    pub fn defaults(&mut self, defaults: Vec<Option<Value>>) {
        self.defaults = defaults;
    }

    /// whether the trailing parameters are `*args` and `**kwargs`
    pub fn variadic(&mut self, vararg: bool, kwarg: bool) {
        self.vararg = vararg;
        self.kwarg = kwarg;
    }

    /// which parameters the boundary can only sometimes establish
    ///
    /// see [`Function::deferring`]
    pub fn deferring(&mut self, deferring: Vec<usize>) {
        self.deferring = deferring;
    }

    /// which parameters have a default only the interpreted definition holds
    ///
    /// see [`Function::computed_defaults`]
    pub fn computed_defaults(&mut self, computed: Vec<usize>) {
        self.computed_defaults = computed;
    }

    /// how many named parameters are positional-only, and how many keyword-only
    pub fn binding_kinds(&mut self, posonly: usize, kwonly: usize) {
        self.posonly = posonly;
        self.kwonly = kwonly;
    }

    /// record where the *current block's* code came from
    ///
    /// the first statement lowered into a block wins: a block is a run of
    /// straight-line code, and pointing at where it starts is what a `#line` is for
    pub fn block_at(&mut self, range: (u32, u32)) {
        if let Some(slot) = self.block_ranges.get_mut(self.current.index())
            && slot.is_none()
        {
            *slot = Some(range);
        }
    }

    pub fn current_block(&self) -> BlockId {
        self.current
    }

    /// whether the current block already has a terminator, which is how a builder
    /// caller knows a `return` made the rest of a statement list dead
    pub fn is_sealed(&self, block: BlockId) -> bool {
        self.blocks.get(block.index()).is_some_and(Option::is_some)
    }

    /// append an operation to the current block. ops added after the block is
    /// sealed are dropped, which is what makes emitting dead code harmless
    pub fn push(&mut self, op: Op) {
        if self.is_sealed(self.current) {
            return;
        }
        if let Some(ops) = self.pending.get_mut(self.current.index()) {
            ops.push(op);
        }
    }

    /// finish the current block. the first terminator wins, so a `return` inside
    /// an `if` arm is not overwritten by the arm's implicit jump
    pub fn terminate(&mut self, terminator: Terminator) {
        if self.is_sealed(self.current) {
            return;
        }
        let ops = self
            .pending
            .get_mut(self.current.index())
            .map(std::mem::take)
            .unwrap_or_default();
        if let Some(slot) = self.blocks.get_mut(self.current.index()) {
            *slot = Some(BasicBlock {
                range: None,
                ops,
                terminator,
                owned_at_exit: None,
                error_target: self.error_target,
            });
        }
    }

    /// route a failing operation to `target` instead of the function's error exit
    ///
    /// returns the previous target, so a caller can restore it
    pub fn set_error_target(&mut self, target: Option<BlockId>) -> Option<BlockId> {
        std::mem::replace(&mut self.error_target, target)
    }

    /// convenience: emit `dest = value`
    pub fn assign(&mut self, dest: RegisterId, src: Value) {
        self.push(Op::Assign { dest, src });
    }

    /// finish the function. any block left without a terminator is closed as
    /// unreachable rather than dropped, so block indices stay stable
    pub fn finish(self) -> Function {
        let blocks = self
            .blocks
            .into_iter()
            .zip(self.pending)
            .zip(self.block_ranges)
            .map(|((block, ops), range)| {
                let mut block = block.unwrap_or(BasicBlock {
                    ops,
                    terminator: Terminator::Unreachable,
                    owned_at_exit: None,
                    error_target: None,
                    range: None,
                });
                block.range = range;
                block
            })
            .collect();
        Function {
            posonly: self.posonly,
            kwonly: self.kwonly,
            name: self.name,
            param_count: self.param_count,
            ret: self.ret,
            convention: self.convention,
            registers: self.registers,
            blocks,
            exported: self.exported,
            owner: self.owner,
            decorators: self.decorators,
            defaults: self.defaults,
            vararg: self.vararg,
            kwarg: self.kwarg,
            range: self.range,
            deferring: self.deferring,
            computed_defaults: self.computed_defaults,
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::BinOp;
    use crate::verify::verify;

    #[test]
    fn a_built_function_verifies() {
        let mut builder = FunctionBuilder::new("add", RType::INT);
        let a = builder.param("a", RType::INT);
        let b = builder.param("b", RType::INT);
        let sum = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(sum)));
        assert_eq!(verify(&builder.finish()), Ok(()));
    }

    #[test]
    fn the_first_terminator_wins() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.terminate(Terminator::Return(Value::Int(2)));
        let function = builder.finish();
        assert_eq!(
            function.blocks[0].terminator,
            Terminator::Return(Value::Int(1))
        );
    }

    #[test]
    fn ops_after_a_terminator_are_dropped() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let dead = builder.temp(RType::INT);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.assign(dead, Value::Int(7));
        let function = builder.finish();
        assert!(function.blocks[0].ops.is_empty());
    }

    #[test]
    fn an_unterminated_block_becomes_unreachable_and_keeps_its_index() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let orphan = builder.new_block();
        builder.terminate(Terminator::Return(Value::Int(1)));
        let function = builder.finish();
        assert_eq!(function.blocks.len(), 2);
        assert_eq!(orphan, BlockId(1));
        assert_eq!(function.blocks[1].terminator, Terminator::Unreachable);
        assert_eq!(verify(&function), Ok(()));
    }

    #[test]
    fn blocks_can_be_filled_out_of_order() {
        let mut builder = FunctionBuilder::new("pick", RType::INT);
        let c = builder.param("c", RType::BIT);
        let then_block = builder.new_block();
        let else_block = builder.new_block();
        builder.terminate(Terminator::Branch {
            cond: Value::Register(c),
            then_block,
            else_block,
        });
        // fill the second target first
        builder.switch_to(else_block);
        builder.terminate(Terminator::Return(Value::Int(0)));
        builder.switch_to(then_block);
        builder.terminate(Terminator::Return(Value::Int(1)));
        assert_eq!(verify(&builder.finish()), Ok(()));
    }

    #[test]
    fn register_types_are_readable_back() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::FLOAT);
        assert_eq!(builder.register_type(a), Some(&RType::FLOAT));
        assert_eq!(builder.register_type(RegisterId(9)), None);
    }
}
