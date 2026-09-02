//! the BIR verifier
//!
//! codegen is allowed to assume a verified function, so every assumption it makes
//! is checked here first: indices are in range, each operation's operands have
//! the types it requires, each write matches the destination register's declared
//! type, and no register is read on a path that has not written it.
//!
//! this is the guard on the representation invariant. a pass that produces
//! ill-typed BIR is a bug that would otherwise surface as miscompiled C.

use std::collections::{HashSet, VecDeque};
use std::fmt;

use crate::function::{Function, ModuleIr};
use crate::ops::{BlockId, Op, RegisterId, Terminator, UnaryOp, Value};
use crate::rtype::{Primitive, RType};

/// something wrong with a function's BIR
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyError {
    pub function: String,
    pub block: Option<BlockId>,
    pub message: String,
    /// whether this is a fact about the *program* rather than about the lowering
    ///
    /// almost everything the verifier finds is a compiler invariant, and saying so is
    /// the point. reading a local on a path that never assigned it is not: python
    /// answers that with `UnboundLocalError`, and the honest report says so rather
    /// than blaming the lowering
    pub about_the_source: bool,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.function)?;
        if let Some(block) = self.block {
            write!(f, ":b{}", block.0)?;
        }
        write!(f, ": {}", self.message)
    }
}

/// verify every function in a module
pub fn verify_module(module: &ModuleIr) -> Result<(), Vec<VerifyError>> {
    let errors: Vec<VerifyError> = module
        .all_functions()
        .filter_map(|function| verify_in(function, Some(module)).err())
        .flatten()
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// verify one function, on its own
///
/// a check that needs to look at another function — a call's arguments against the
/// callee's parameters — is skipped. [`verify_module`] is where those run
pub fn verify(function: &Function) -> Result<(), Vec<VerifyError>> {
    verify_in(function, None)
}

/// verify one function, against the module it belongs to where that is known
fn verify_in(function: &Function, module: Option<&ModuleIr>) -> Result<(), Vec<VerifyError>> {
    let mut verifier = Verifier {
        function,
        module,
        errors: Vec::new(),
    };
    verifier.run();
    if verifier.errors.is_empty() {
        Ok(())
    } else {
        Err(verifier.errors)
    }
}

/// whether `from` can be stored where `to` is expected with no conversion at all
///
/// exactly one pair qualifies: a `bit` is a `bool` whose error case has been ruled
/// out, and both are the same 0-or-1 byte. anything wider needs a real `Box`
fn free_widening(from: &RType, to: &RType) -> bool {
    match (from, to) {
        // a comparison result is already a valid bool byte
        (RType::Primitive(Primitive::Bit), RType::Primitive(Primitive::Bool)) => true,
        // `object` is the widest representation, and a boxed *primitive* is
        // already a `PyObject *` — so widening one to it moves a pointer and needs
        // no C cast anywhere it might be used. that is what lets the folder turn
        // `box` of a `str` into a copy.
        //
        // a native class is deliberately not here: its C type is a pointer to its
        // own struct, so widening it does need a cast, and `Box` is where that
        // cast is emitted
        (RType::Primitive(primitive), RType::Primitive(Primitive::Object)) => {
            !RType::Primitive(*primitive).is_unboxed()
        }
        _ => false,
    }
}

struct Verifier<'a> {
    function: &'a Function,
    /// the module this function belongs to, where the caller knows it
    ///
    /// only a cross-function check needs it: a call's arguments are checked against
    /// the callee's parameters, which live in another `Function`
    module: Option<&'a ModuleIr>,
    errors: Vec<VerifyError>,
}

impl Verifier<'_> {
    fn error(&mut self, block: Option<BlockId>, message: impl Into<String>) {
        self.push(block, message, false);
    }

    /// as [`Self::error`], for a finding that describes the source rather than the ir
    fn source_error(&mut self, block: Option<BlockId>, message: impl Into<String>) {
        self.push(block, message, true);
    }

    fn push(&mut self, block: Option<BlockId>, message: impl Into<String>, about_the_source: bool) {
        self.errors.push(VerifyError {
            function: self.function.name.clone(),
            block,
            message: message.into(),
            about_the_source,
        });
    }

    /// a call's arguments must have the representations the callee declares
    ///
    /// codegen emits the call as a plain C call, so a mismatch here is a mismatch the
    /// C compiler sees — and only *sometimes*: a tagged integer reaching a `PyObject *`
    /// is a diagnosable pointer/integer confusion, while the same mistake between two
    /// integer representations compiles quietly and is wrong at runtime. checking it
    /// here makes the whole class equally loud
    fn check_call(&mut self, block: BlockId, owner: Option<&str>, callee: &str, args: &[Value]) {
        let Some(module) = self.module else {
            return;
        };
        let Some(target) = module
            .all_functions()
            .find(|candidate| candidate.name == callee && candidate.owner.as_deref() == owner)
        else {
            return;
        };
        // arity is the frontend's to get right and it reports far better errors than
        // this could; checking only what lines up keeps the two from disagreeing
        if args.len() != target.param_count {
            return;
        }
        for (index, arg) in args.iter().enumerate() {
            let Some(declared) = target.registers.get(index).map(|decl| decl.ty.clone()) else {
                continue;
            };
            let Some(actual) = self.operand_type(block, arg) else {
                continue;
            };
            if actual != declared
                && !free_widening(&actual, &declared)
                && !self.upcasts(&actual, &declared)
            {
                let name = target
                    .registers
                    .get(index)
                    .and_then(|decl| decl.name.clone())
                    .unwrap_or_else(|| format!("r{index}"));
                self.error(
                    Some(block),
                    format!(
                        "`{callee}` declares `{name}` as {declared}, but the argument is {actual}"
                    ),
                );
            }
        }
    }

    /// whether an argument reaches a parameter by an upcast that costs nothing
    ///
    /// a subclass's struct begins with its base's, so a pointer to one already is a
    /// pointer to the other. `exact` is not consulted: it narrows what a value can be,
    /// and a narrower value is always acceptable where a wider one is declared
    fn upcasts(&self, from: &RType, to: &RType) -> bool {
        let (
            RType::Instance { class: from, .. },
            RType::Instance {
                class: declared, ..
            },
        ) = (from, to)
        else {
            return false;
        };
        let Some(module) = self.module else {
            return false;
        };
        let mut current = Some(from.clone());
        while let Some(name) = current {
            if name == *declared {
                return true;
            }
            // only an in-module base continues the chain: an external one has no struct
            // here, so nothing downstream of it is a free pointer cast
            current = module
                .classes
                .iter()
                .find(|class| class.name == name)
                .and_then(|class| class.base.as_ref())
                .and_then(|base| base.in_module())
                .map(str::to_owned);
        }
        false
    }

    fn run(&mut self) {
        if self.function.blocks.is_empty() {
            self.error(None, "a function needs at least an entry block");
            return;
        }
        if self.function.param_count > self.function.registers.len() {
            self.error(
                None,
                format!(
                    "param_count {} exceeds the {} declared registers",
                    self.function.param_count,
                    self.function.registers.len()
                ),
            );
            return;
        }

        for index in 0..self.function.blocks.len() {
            let id = BlockId(index);
            self.check_block(id);
        }

        self.check_definite_assignment();
        self.check_release_sets();
    }

    /// every refcounted register that could already hold a reference at an exit
    /// must be in that block's release set
    ///
    /// this is the invariant the refcount pass exists to *narrow*, and getting it
    /// wrong is a leak rather than a crash — so nothing else would notice. it is
    /// checked here from the opposite direction: which blocks can reach this one,
    /// rather than a forward dataflow fixed point, so a mistake in the pass cannot
    /// also hide itself
    fn check_release_sets(&mut self) {
        let count = self.function.blocks.len();
        // predecessors, from the successors each terminator names
        let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (index, block) in self.function.blocks.iter().enumerate() {
            for successor in block.successors() {
                if let Some(slot) = predecessors.get_mut(successor.index()) {
                    slot.push(index);
                }
            }
        }

        // a block nothing can reach never runs, so nothing is required of it. the
        // refcount pass reaches it the same way — by not reaching it — and demanding a
        // release set there would be a disagreement about dead code
        let mut reachable = HashSet::from([0usize]);
        let mut queue = VecDeque::from([Function::entry()]);
        while let Some(id) = queue.pop_front() {
            let Some(block) = self.function.block(id) else {
                continue;
            };
            for successor in block.successors() {
                if reachable.insert(successor.index()) {
                    queue.push_back(successor);
                }
            }
        }

        for index in 0..count {
            if !reachable.contains(&index) {
                continue;
            }
            let Some(owned) = self
                .function
                .blocks
                .get(index)
                .and_then(|block| block.owned_at_exit.as_ref())
            else {
                // the pass has not run, and codegen's conservative answer applies
                continue;
            };
            let owned: HashSet<RegisterId> = owned.iter().copied().collect();

            // every block that can reach this one, and this one
            let mut reaching = HashSet::from([index]);
            let mut queue = vec![index];
            while let Some(current) = queue.pop() {
                for &predecessor in &predecessors[current] {
                    if reaching.insert(predecessor) {
                        queue.push(predecessor);
                    }
                }
            }
            // a parameter holds a value from the first instruction
            let mut held: HashSet<RegisterId> =
                (0..self.function.param_count).map(RegisterId).collect();
            for block in reaching
                .iter()
                .filter_map(|at| self.function.blocks.get(*at))
            {
                held.extend(block.ops.iter().filter_map(Op::dest));
            }

            let mut missing: Vec<RegisterId> = held
                .into_iter()
                .filter(|register| {
                    !owned.contains(register)
                        && self
                            .function
                            .register(*register)
                            .is_some_and(|decl| decl.ty.is_refcounted() && !decl.borrowed)
                })
                .collect();
            missing.sort_unstable();
            for register in missing {
                self.error(
                    Some(BlockId(index)),
                    format!(
                        "r{} may hold a reference here but is not released",
                        register.0
                    ),
                );
            }
        }
    }

    fn check_block(&mut self, id: BlockId) {
        let Some(block) = self.function.block(id) else {
            return;
        };
        for op in &block.ops {
            self.check_op(id, op);
        }
        self.check_terminator(id, &block.terminator);
    }

    /// the type an operand carries, reporting a dangling register rather than
    /// silently succeeding
    fn operand_type(&mut self, block: BlockId, value: &Value) -> Option<RType> {
        match self.function.value_type(value) {
            Some(ty) => Some(ty),
            None => {
                if let Value::Register(id) = value {
                    self.error(Some(block), format!("r{} is not declared", id.0));
                }
                None
            }
        }
    }

    /// an operand's representation must be the expected one, or widen to it for free
    ///
    /// the free case is what lets a `str` be handed to something that takes an
    /// `object`: both are a `PyObject *`, so there is no conversion to emit and
    /// nothing for the frontend to have got wrong
    fn expect(&mut self, block: BlockId, value: &Value, expected: &RType, what: &str) {
        if let Some(actual) = self.operand_type(block, value)
            && actual != *expected
            && !free_widening(&actual, expected)
        {
            self.error(
                Some(block),
                format!("{what} expects {expected}, found {actual}"),
            );
        }
    }

    fn expect_dest(&mut self, block: BlockId, dest: RegisterId, expected: &RType, what: &str) {
        match self.function.register(dest) {
            None => self.error(Some(block), format!("r{} is not declared", dest.0)),
            Some(decl) if decl.ty != *expected => self.error(
                Some(block),
                format!(
                    "{what} produces {expected}, but r{} is declared {}",
                    dest.0, decl.ty
                ),
            ),
            Some(_) => {}
        }
    }

    /// the element type of an operand that must be an array
    fn array_element(&mut self, block: BlockId, array: &Value) -> Option<RType> {
        match self.operand_type(block, array) {
            Some(RType::Array(element)) => Some(*element),
            Some(other) => {
                self.error(Some(block), format!("expected an array, found {other}"));
                None
            }
            None => None,
        }
    }

    fn check_op(&mut self, block: BlockId, op: &Op) {
        match op {
            Op::Assign { dest, src } => {
                if let Some(src_ty) = self.operand_type(block, src) {
                    let expected = match self.function.register(*dest).map(|decl| &decl.ty) {
                        Some(declared) if free_widening(&src_ty, declared) => declared.clone(),
                        _ => src_ty,
                    };
                    self.expect_dest(block, *dest, &expected, "assign");
                }
            }
            Op::IntBinary { dest, lhs, rhs, op } => {
                // a *fixed* width is plain machine arithmetic: no tag, no shift, no
                // overflow branch. both operands have to agree on it, because the
                // width is the representation rather than a property of the value
                let width = match self.operand_type(block, lhs) {
                    Some(ty @ RType::Primitive(Primitive::Fixed(_))) => ty,
                    _ => RType::INT,
                };
                self.expect(block, lhs, &width, op.symbol());
                self.expect(block, rhs, &width, op.symbol());
                let result = if matches!(op, crate::ops::BinOp::TrueDiv) {
                    RType::FLOAT
                } else {
                    width
                };
                self.expect_dest(block, *dest, &result, op.symbol());
            }
            Op::FloatBinary { dest, lhs, rhs, op } => {
                self.expect(block, lhs, &RType::FLOAT, op.symbol());
                self.expect(block, rhs, &RType::FLOAT, op.symbol());
                self.expect_dest(block, *dest, &RType::FLOAT, op.symbol());
            }
            Op::IsInstance { dest, src, class } => {
                self.expect(block, src, &RType::OBJECT, "an isinstance test");
                self.expect(block, class, &RType::OBJECT, "an isinstance test");
                self.expect_dest(block, *dest, &RType::BIT, "an isinstance test");
            }
            Op::MatchKey { dest, map, key } => {
                self.expect(block, map, &RType::OBJECT, "a mapping pattern");
                self.expect(block, key, &RType::OBJECT, "a mapping pattern");
                self.expect_dest(block, *dest, &RType::OBJECT, "a mapping pattern");
            }
            Op::MatchRest { dest, map, keys } => {
                self.expect(block, map, &RType::OBJECT, "a mapping pattern");
                self.expect(block, keys, &RType::OBJECT, "a mapping pattern");
                self.expect_dest(block, *dest, &RType::OBJECT, "a mapping pattern");
            }
            Op::AsyncContext {
                dest,
                manager,
                exception,
            } => {
                self.expect(block, manager, &RType::OBJECT, "an async context manager");
                if let Some(exception) = exception {
                    self.expect(block, exception, &RType::OBJECT, "an async context manager");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "an async context manager");
            }
            Op::AsyncIter { dest, src, .. } => {
                self.expect(block, src, &RType::OBJECT, "an async iterator");
                self.expect_dest(block, *dest, &RType::OBJECT, "an async iterator");
            }
            Op::IsMapping { dest, src } => {
                self.expect(block, src, &RType::OBJECT, "a mapping-shape test");
                self.expect_dest(block, *dest, &RType::BIT, "a mapping-shape test");
            }
            Op::MatchAttr { dest, subject, .. } => {
                self.expect(block, subject, &RType::OBJECT, "a class pattern");
                self.expect_dest(block, *dest, &RType::OBJECT, "a class pattern");
            }
            Op::MethodStands { dest, src, .. } => {
                self.expect(block, src, &RType::OBJECT, "a dispatch test");
                self.expect_dest(block, *dest, &RType::BIT, "a dispatch test");
            }
            Op::DictShadows { dest, src, .. } => {
                // the test reads the instance's type and its dict slot and stores the
                // pointer nowhere, so it borrows — which is why an emitted class's own
                // pointer is taken as it stands rather than through a `box`, the one
                // widening that would otherwise cost a reference on the fast path
                if !matches!(
                    self.operand_type(block, src),
                    None | Some(RType::Instance { .. })
                ) {
                    self.expect(block, src, &RType::OBJECT, "an instance-dict test");
                }
                self.expect_dest(block, *dest, &RType::BIT, "an instance-dict test");
            }
            Op::IsMissing { dest, src } => {
                self.expect(block, src, &RType::OBJECT, "a class pattern");
                self.expect_dest(block, *dest, &RType::BIT, "a class pattern");
            }
            Op::MatchSlice { dest, sequence, .. } => {
                self.expect(block, sequence, &RType::OBJECT, "a sequence pattern");
                self.expect_dest(block, *dest, &RType::OBJECT, "a sequence pattern");
            }
            Op::IsSequence { dest, src } => {
                self.expect(block, src, &RType::OBJECT, "a sequence-shape test");
                self.expect_dest(block, *dest, &RType::BIT, "a sequence-shape test");
            }
            Op::Contains {
                dest,
                value,
                container,
                ..
            } => {
                self.expect(block, value, &RType::OBJECT, "a containment test");
                self.expect(block, container, &RType::OBJECT, "a containment test");
                self.expect_dest(block, *dest, &RType::BIT, "a containment test");
            }
            Op::Identity { dest, lhs, rhs, .. } => {
                self.expect(block, lhs, &RType::OBJECT, "an identity test");
                self.expect(block, rhs, &RType::OBJECT, "an identity test");
                self.expect_dest(block, *dest, &RType::BIT, "an identity test");
            }
            Op::FloatObjectCompare { dest, lhs, rhs, op } => {
                let sides = [self.operand_type(block, lhs), self.operand_type(block, rhs)];
                if sides != [Some(RType::FLOAT), Some(RType::OBJECT)]
                    && sides != [Some(RType::OBJECT), Some(RType::FLOAT)]
                {
                    self.error(
                        Some(block),
                        format!("{} tests one object against one float", op.symbol()),
                    );
                }
                self.expect_dest(block, *dest, &RType::BIT, op.symbol());
            }
            Op::FloatObjectBinary { dest, lhs, rhs, op } => {
                // exactly one side is the object being tested, and the other is
                // the double that stays in its register
                let sides = [self.operand_type(block, lhs), self.operand_type(block, rhs)];
                if sides != [Some(RType::FLOAT), Some(RType::OBJECT)]
                    && sides != [Some(RType::OBJECT), Some(RType::FLOAT)]
                {
                    let describe = |side: &Option<RType>| {
                        side.as_ref()
                            .map_or("nothing".to_string(), RType::to_string)
                    };
                    let message = format!(
                        "{} tests one object against one float, not {} and {}",
                        op.symbol(),
                        describe(&sides[0]),
                        describe(&sides[1]),
                    );
                    self.error(Some(block), message);
                }
                self.expect_dest(block, *dest, &RType::FLOAT, op.symbol());
            }
            Op::ObjectBinary {
                dest, lhs, rhs, op, ..
            } => {
                self.expect(block, lhs, &RType::OBJECT, op.symbol());
                self.expect(block, rhs, &RType::OBJECT, op.symbol());
                self.expect_dest(block, *dest, &RType::OBJECT, op.symbol());
            }
            Op::ObjectCompare { dest, lhs, rhs, op } => {
                self.expect(block, lhs, &RType::OBJECT, op.symbol());
                self.expect(block, rhs, &RType::OBJECT, op.symbol());
                self.expect_dest(block, *dest, &RType::BIT, op.symbol());
            }
            Op::StrCompare { dest, lhs, rhs, op } => {
                self.expect(block, lhs, &RType::STR, op.symbol());
                self.expect(block, rhs, &RType::STR, op.symbol());
                self.expect_dest(block, *dest, &RType::BIT, op.symbol());
            }
            Op::Truthy { dest, src } => {
                self.expect(block, src, &RType::OBJECT, "truthiness");
                self.expect_dest(block, *dest, &RType::BIT, "truthiness");
            }
            Op::IntCompare { dest, lhs, rhs, op } => {
                let width = match self.operand_type(block, lhs) {
                    Some(ty @ RType::Primitive(Primitive::Fixed(_))) => ty,
                    _ => RType::INT,
                };
                self.expect(block, lhs, &width, op.symbol());
                // an unboxed counter is compared against an ordinary `int` bound: the
                // bound is the loop's, and nothing proves it fits a register, so the
                // mixed pair is the *normal* shape rather than an exceptional one
                if matches!(width, RType::Primitive(Primitive::Fixed(_)))
                    && matches!(self.operand_type(block, rhs), Some(RType::INT))
                {
                    self.expect(block, rhs, &RType::INT, op.symbol());
                } else {
                    self.expect(block, rhs, &width, op.symbol());
                }
                self.expect_dest(block, *dest, &RType::BIT, op.symbol());
            }
            Op::FloatCompare { dest, lhs, rhs, op } => {
                self.expect(block, lhs, &RType::FLOAT, op.symbol());
                self.expect(block, rhs, &RType::FLOAT, op.symbol());
                self.expect_dest(block, *dest, &RType::BIT, op.symbol());
            }
            Op::Unary { dest, op, operand } => {
                let Some(operand_ty) = self.operand_type(block, operand) else {
                    return;
                };
                match op {
                    UnaryOp::Neg => {
                        if !matches!(
                            operand_ty,
                            RType::Primitive(Primitive::Int | Primitive::Float | Primitive::Object)
                        ) {
                            self.error(
                                Some(block),
                                format!(
                                    "unary `-` expects int, float or object, found {operand_ty}"
                                ),
                            );
                            return;
                        }
                        self.expect_dest(block, *dest, &operand_ty, "unary `-`");
                    }
                    UnaryOp::Invert => {
                        if !matches!(
                            operand_ty,
                            RType::Primitive(Primitive::Int | Primitive::Object)
                        ) {
                            self.error(
                                Some(block),
                                format!("unary `~` expects int or object, found {operand_ty}"),
                            );
                            return;
                        }
                        self.expect_dest(block, *dest, &operand_ty, "unary `~`");
                    }
                    UnaryOp::Not => {
                        // `bool` and `bit` are both a 0-or-1 byte, so `not`
                        // accepts either and always narrows to `bit`
                        if !matches!(
                            operand_ty,
                            RType::Primitive(Primitive::Bit | Primitive::Bool)
                        ) {
                            self.error(
                                Some(block),
                                format!("unary `not` expects bool or bit, found {operand_ty}"),
                            );
                            return;
                        }
                        self.expect_dest(block, *dest, &RType::BIT, "unary `not`");
                    }
                }
            }
            Op::CallNative {
                dest,
                owner,
                callee,
                args,
            } => {
                for arg in args {
                    self.operand_type(block, arg);
                }
                self.check_call(block, owner.as_deref(), callee, args);
                if let Some(dest) = dest
                    && self.function.register(*dest).is_none()
                {
                    self.error(Some(block), format!("r{} is not declared", dest.0));
                }
            }
            Op::IntToFloat { dest, src } => {
                if let Some(src_ty) = self.operand_type(block, src)
                    && src_ty != RType::INT
                {
                    self.error(
                        Some(block),
                        format!("int-to-float widens an int, but its operand is a {src_ty}"),
                    );
                }
                self.expect_dest(block, *dest, &RType::FLOAT, "int-to-float");
            }
            Op::Box { dest, src } => {
                // box *widens to* `object`, from either an unboxed value or a
                // known-class object. widening something already `object` is a
                // no-op the frontend should not have emitted
                match self.operand_type(block, src) {
                    Some(src_ty) if src_ty == RType::OBJECT => self.error(
                        Some(block),
                        "box widens to object, but its operand is already object",
                    ),
                    // an unboxed array has no `PyObject` to widen to. building a
                    // real `list` from one would be a *copy*, and a copy is a
                    // different list — so a value that escapes has to have been a
                    // real list all along
                    Some(RType::Array(_)) => self.error(
                        Some(block),
                        "an unboxed array cannot escape: building a list from it \
                         would be a copy, and a copy is a different list",
                    ),
                    _ => {}
                }
                // a machine integer's *object* representation is the tagged `int`:
                // that is the one an `int` has everywhere else in the IR, and going
                // straight to a `PyObject` would give the counter a representation no
                // consumer of an `int` accepts
                let widened = match self.operand_type(block, src) {
                    Some(RType::Primitive(Primitive::Fixed(_))) => RType::INT,
                    _ => RType::OBJECT,
                };
                self.expect_dest(block, *dest, &widened, "box");
            }
            Op::Unbox { dest, src, to } => {
                // unbox *narrows from* `object` — to an unboxed representation, or
                // to a boxed one whose class is known. narrowing to `object` would
                // be a no-op the frontend should not have emitted
                self.expect(block, src, &RType::OBJECT, "unbox");
                if *to == RType::OBJECT {
                    self.error(
                        Some(block),
                        "unbox narrows from object, but its target is object",
                    );
                }
                self.expect_dest(block, *dest, to, "unbox");
            }
            Op::TupleBuild { dest, items } => {
                let declared = self
                    .function
                    .register(*dest)
                    .map(|decl| decl.ty.clone())
                    .and_then(|ty| match ty {
                        RType::Tuple(slots) => Some(slots),
                        _ => None,
                    });
                let mut item_types = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    match self.operand_type(block, item) {
                        // a slot the item widens into *freely* takes it as it is —
                        // the same rule an assignment follows, and what lets a pass
                        // put an already-boxed value straight into an `object` slot
                        Some(ty) => item_types.push(
                            match declared.as_ref().and_then(|slots| slots.get(index)) {
                                Some(slot) if free_widening(&ty, slot) => slot.clone(),
                                _ => ty,
                            },
                        ),
                        None => return,
                    }
                }
                let built = RType::Tuple(item_types.into_boxed_slice());
                self.expect_dest(block, *dest, &built, "tuple build");
            }
            Op::Extend {
                dest,
                container,
                source,
                ..
            } => {
                self.expect(block, container, &RType::OBJECT, "extend");
                self.expect(block, source, &RType::OBJECT, "extend");
                self.expect_dest(block, *dest, &RType::BIT, "extend");
            }
            Op::CallUnpacked {
                dest,
                callee,
                args,
                kwargs,
            } => {
                self.expect(block, callee, &RType::OBJECT, "an unpacked call");
                self.expect(block, args, &RType::OBJECT, "an unpacked call");
                if let Some(kwargs) = kwargs {
                    self.expect(block, kwargs, &RType::OBJECT, "an unpacked call");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "an unpacked call");
            }
            Op::ArrayNew { dest, items } => {
                let Some(RType::Array(element)) =
                    self.function.register(*dest).map(|decl| decl.ty.clone())
                else {
                    self.error(Some(block), "an array build must land in an array");
                    return;
                };
                // the buffer is freed as a unit, so an element that owns a reference
                // would leak — the representation is for values that own nothing
                if element.is_refcounted() {
                    self.error(
                        Some(block),
                        format!("an array of {element} would leak: its elements are owned"),
                    );
                }
                for item in items {
                    self.expect(block, item, &element, "an array element");
                }
            }
            Op::ArrayGet { dest, array, index } => {
                let Some(element) = self.array_element(block, array) else {
                    return;
                };
                self.expect(block, index, &RType::INT, "an array index");
                self.expect_dest(block, *dest, &element, "an array read");
            }
            Op::ArraySet {
                dest,
                array,
                index,
                value,
            } => {
                let Some(element) = self.array_element(block, array) else {
                    return;
                };
                self.expect(block, index, &RType::INT, "an array index");
                self.expect(block, value, &element, "an array write");
                self.expect_dest(block, *dest, &RType::BIT, "an array write");
            }
            Op::ArrayLen { dest, array } => {
                if self.array_element(block, array).is_none() {
                    return;
                }
                // either representation: the destination says which
                let wanted = match self.function.register(*dest).map(|decl| decl.ty.clone()) {
                    Some(ty @ RType::Primitive(Primitive::Fixed(_))) => ty,
                    _ => RType::INT,
                };
                self.expect_dest(block, *dest, &wanted, "an array length");
            }
            Op::DeleteItem {
                dest,
                container,
                index,
            } => {
                self.expect(block, container, &RType::OBJECT, "del item");
                self.expect(block, index, &RType::OBJECT, "del item");
                self.expect_dest(block, *dest, &RType::BIT, "del item");
            }
            Op::DeleteAttr { dest, receiver, .. } => {
                self.expect(block, receiver, &RType::OBJECT, "del attribute");
                self.expect_dest(block, *dest, &RType::BIT, "del attribute");
            }
            Op::ArrayRead { dest, array, index } => {
                let Some(element) = self.array_element(block, array) else {
                    return;
                };
                // the lowering's own counter is a machine integer; an index a loop
                // guard *proved* is the tagged one that loop advances, and codegen
                // untags it rather than spending a register on the conversion
                if self.function.value_type(index) != Some(RType::INT) {
                    self.expect(
                        block,
                        index,
                        &RType::fixed(crate::rtype::IntWidth::I64),
                        "an unchecked array index",
                    );
                }
                self.expect_dest(block, *dest, &element, "an unchecked array read");
            }
            Op::ArrayPush { dest, array, value } => {
                let Some(element) = self.array_element(block, array) else {
                    return;
                };
                self.expect(block, value, &element, "an array append");
                self.expect_dest(block, *dest, &RType::BIT, "an array append");
            }
            Op::ToTuple { dest, src } => {
                self.expect(block, src, &RType::OBJECT, "tuple");
                self.expect_dest(block, *dest, &RType::OBJECT, "tuple");
            }
            Op::Unpack { dest, src, starred } => {
                self.expect(block, src, &RType::OBJECT, "unpack");
                let Some(ty) = self.function.register(*dest).map(|decl| decl.ty.clone()) else {
                    return;
                };
                let RType::Tuple(items) = &ty else {
                    self.error(
                        Some(block),
                        format!("unpack must land in a fixed-length tuple, found {ty}"),
                    );
                    return;
                };
                // every slot is filled by the runtime, which hands over objects
                if items.iter().any(|item| *item != RType::OBJECT) {
                    self.error(
                        Some(block),
                        format!("unpack lands in {ty}, not a tuple of objects"),
                    );
                }
                if starred.is_some_and(|index| index >= items.len()) {
                    self.error(Some(block), "the starred slot is past the end of the tuple");
                }
                self.expect_dest(block, *dest, &ty, "unpack");
            }
            Op::TupleGet { dest, src, index } => {
                let Some(src_ty) = self.operand_type(block, src) else {
                    return;
                };
                let RType::Tuple(items) = &src_ty else {
                    self.error(
                        Some(block),
                        format!("tuple get expects a fixed-length tuple, found {src_ty}"),
                    );
                    return;
                };
                let Some(element) = items.get(*index) else {
                    self.error(
                        Some(block),
                        format!("tuple get index {index} is past the end of {src_ty}"),
                    );
                    return;
                };
                let element = element.clone();
                self.expect_dest(block, *dest, &element, "tuple get");
            }
            Op::GetCell {
                dest,
                receiver,
                class,
                ..
            } => {
                self.expect(
                    block,
                    receiver,
                    &RType::Instance {
                        class: class.clone(),
                        exact: false,
                    },
                    "a cell read",
                );
                self.expect_dest(block, *dest, &RType::OBJECT, "a cell read");
            }
            Op::NewInstance {
                dest,
                class,
                fields,
            } => {
                for field in fields.iter().flatten() {
                    self.operand_type(block, field);
                }
                self.expect_dest(
                    block,
                    *dest,
                    &RType::Instance {
                        class: class.clone(),
                        exact: false,
                    },
                    "an allocation",
                );
            }
            Op::MakeClosure {
                dest, class, env, ..
            } => {
                self.expect(
                    block,
                    env,
                    &RType::Instance {
                        class: class.clone(),
                        exact: false,
                    },
                    "a closure's environment",
                );
                self.expect_dest(block, *dest, &RType::OBJECT, "a closure");
            }
            Op::LoadGlobal { dest, .. } => {
                self.expect_dest(block, *dest, &RType::OBJECT, "a global read");
            }
            Op::ModuleDict { dest } => {
                self.expect_dest(block, *dest, &RType::OBJECT, "the module namespace");
            }
            Op::StoreGlobal { dest, value, .. } => {
                // the namespace holds objects, so a write to one has to arrive boxed
                self.expect(block, value, &RType::OBJECT, "a global write");
                self.expect_dest(block, *dest, &RType::BIT, "a global write");
            }
            Op::DeleteGlobal { dest, .. } => {
                self.expect_dest(block, *dest, &RType::BIT, "a global delete");
            }
            // no type to check: the destination keeps whatever representation it was
            // declared with, and the deletion only puts it back to unbound
            Op::DeleteLocal { .. } => {}
            Op::LoadClass { dest, .. } => {
                self.expect_dest(block, *dest, &RType::OBJECT, "a class read");
            }
            Op::ImportModule { dest, .. } => {
                self.expect_dest(block, *dest, &RType::OBJECT, "an import");
            }
            Op::ImportFrom { dest, module, .. } => {
                self.expect(block, module, &RType::OBJECT, "an import");
                self.expect_dest(block, *dest, &RType::OBJECT, "an import");
            }
            Op::CallValue { dest, callee, args } => {
                self.expect(block, callee, &RType::OBJECT, "a call through a value");
                for arg in args {
                    self.expect(block, arg, &RType::OBJECT, "a call argument");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "a call through a value");
            }
            Op::CallPython { dest, args, .. } => {
                // the python convention is entirely boxed on both sides
                for arg in args {
                    self.expect(block, arg, &RType::OBJECT, "a python call argument");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "a python call");
            }
            Op::CallMethod {
                dest,
                receiver,
                args,
                ..
            } => {
                self.expect(block, receiver, &RType::OBJECT, "a method receiver");
                for arg in args {
                    self.expect(block, arg, &RType::OBJECT, "a method argument");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "a method call");
            }
            Op::GetField {
                dest,
                receiver,
                class,
                ..
            } => {
                // the receiver's representation has to *be* that class, or the
                // offset would be read out of the wrong struct
                if let Some(actual) = self.operand_type(block, receiver)
                    && !matches!(actual, RType::Instance { class: ref got, .. } if got == class)
                {
                    self.error(
                        Some(block),
                        format!("a field read on {class} has a {actual} receiver"),
                    );
                }
                if self.function.register(*dest).is_none() {
                    self.error(Some(block), format!("r{} is not declared", dest.0));
                }
            }
            Op::SetField {
                receiver,
                class,
                value,
                ..
            } => {
                if let Some(actual) = self.operand_type(block, receiver)
                    && !matches!(actual, RType::Instance { class: ref got, .. } if got == class)
                {
                    self.error(
                        Some(block),
                        format!("a field write on {class} has a {actual} receiver"),
                    );
                }
                self.operand_type(block, value);
            }
            Op::GetAttr { dest, receiver, .. } => {
                self.expect(block, receiver, &RType::OBJECT, "attribute access");
                self.expect_dest(block, *dest, &RType::OBJECT, "attribute access");
            }
            Op::SetAttr {
                dest,
                receiver,
                value,
                ..
            } => {
                self.expect(block, receiver, &RType::OBJECT, "attribute assignment");
                self.expect(block, value, &RType::OBJECT, "attribute assignment");
                self.expect_dest(block, *dest, &RType::BIT, "attribute assignment");
            }
            Op::BuildList { dest, items } => {
                for item in items {
                    self.expect(block, item, &RType::OBJECT, "a list element");
                }
                self.expect_dest(block, *dest, &RType::LIST, "a list display");
            }
            Op::BuildSet { dest, items } => {
                for item in items {
                    self.expect(block, item, &RType::OBJECT, "a set element");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "a set display");
            }
            Op::BuildTuple { dest, items } => {
                for item in items {
                    self.expect(block, item, &RType::OBJECT, "a tuple element");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "a tuple display");
            }
            Op::BuildDict { dest, pairs } => {
                if pairs.len() % 2 != 0 {
                    self.error(Some(block), "a dict display needs an even operand count");
                }
                for pair in pairs {
                    self.expect(block, pair, &RType::OBJECT, "a dict key or value");
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "a dict display");
            }
            Op::GetItem {
                dest,
                container,
                index,
            } => {
                self.expect(block, container, &RType::OBJECT, "a subscript container");
                // an integer index keeps its register, so the lookup never boxes it
                if let Some(ty) = self.operand_type(block, index)
                    && ty != RType::OBJECT
                    && ty != RType::INT
                {
                    self.error(
                        Some(block),
                        format!("a subscript index is an object or an int, not a {ty}"),
                    );
                }
                self.expect_dest(block, *dest, &RType::OBJECT, "a subscript");
            }
            Op::DictFind {
                dest,
                container,
                key,
            } => {
                self.expect(block, container, &RType::OBJECT, "a fused membership read");
                // the key is looked up through the protocol on the path that is not
                // an exact dict, so it has to be an object rather than a tagged int
                self.expect(block, key, &RType::OBJECT, "a fused membership read");
                self.expect_dest(block, *dest, &RType::OBJECT, "a fused membership read");
            }
            Op::StrGetItem {
                dest,
                container,
                index,
            } => {
                self.expect(block, container, &RType::STR, "a str subscript");
                if let Some(ty) = self.operand_type(block, index)
                    && ty != RType::INT
                {
                    self.error(
                        Some(block),
                        format!("a str subscript index is an int, not a {ty}"),
                    );
                }
                self.expect_dest(block, *dest, &RType::STR, "a str subscript");
            }
            Op::StrItemCompare {
                dest,
                container,
                index,
                ..
            } => {
                self.expect(block, container, &RType::STR, "a str subscript");
                if let Some(ty) = self.operand_type(block, index)
                    && ty != RType::INT
                {
                    self.error(
                        Some(block),
                        format!("a str subscript index is an int, not a {ty}"),
                    );
                }
                self.expect_dest(block, *dest, &RType::BIT, "a character comparison");
            }
            Op::SetItem {
                dest,
                container,
                index,
                value,
            } => {
                self.expect(block, container, &RType::OBJECT, "a subscript container");
                if let Some(ty) = self.operand_type(block, index)
                    && ty != RType::OBJECT
                    && ty != RType::INT
                {
                    self.error(
                        Some(block),
                        format!("a subscript index is an object or an int, not a {ty}"),
                    );
                }
                self.expect(block, value, &RType::OBJECT, "a subscript assignment");
                self.expect_dest(block, *dest, &RType::BIT, "a subscript assignment");
            }
            Op::Format {
                dest, value, spec, ..
            } => {
                self.expect(block, value, &RType::OBJECT, "an interpolation");
                if let Some(spec) = spec {
                    self.expect(block, spec, &RType::STR, "a format spec");
                }
                self.expect_dest(block, *dest, &RType::STR, "an interpolation");
            }
            Op::FetchException { dest } => {
                self.expect_dest(block, *dest, &RType::OBJECT, "fetching an exception");
            }
            Op::ExceptionMatches { dest, value, class } => {
                self.expect(block, value, &RType::OBJECT, "an exception match");
                self.expect(block, class, &RType::OBJECT, "an exception match");
                self.expect_dest(block, *dest, &RType::BIT, "an exception match");
            }
            Op::PushHandled { dest, value } => {
                self.expect(block, value, &RType::OBJECT, "entering a handler");
                self.expect_dest(block, *dest, &RType::OBJECT, "entering a handler");
            }
            Op::PopHandled { value } => {
                self.expect(block, value, &RType::OBJECT, "leaving a handler");
            }
            Op::RaiseObject { exception, cause } => {
                self.expect(block, exception, &RType::OBJECT, "a raise");
                if let Some(cause) = cause {
                    self.expect(block, cause, &RType::OBJECT, "a raise cause");
                }
            }
            Op::Reraise { value } => {
                self.expect(block, value, &RType::OBJECT, "a re-raise");
            }
            Op::GetIter { dest, src } => {
                self.expect(block, src, &RType::OBJECT, "iter");
                self.expect_dest(block, *dest, &RType::OBJECT, "iter");
            }
            Op::IterNext { dest, iter } => {
                self.expect(block, iter, &RType::OBJECT, "next");
                self.expect_dest(block, *dest, &RType::OBJECT, "next");
            }
            Op::IsNull { dest, src } => {
                self.expect(block, src, &RType::OBJECT, "a null test");
                self.expect_dest(block, *dest, &RType::BIT, "a null test");
            }
            Op::Len { dest, src } => {
                // a length is defined on anything with `__len__`, so the operand
                // is widened to `object` by the frontend before it gets here
                self.expect(block, src, &RType::OBJECT, "len");
                self.expect_dest(block, *dest, &RType::INT, "len");
            }
            Op::StrOfInt { dest, value } => {
                self.expect(block, value, &RType::INT, "str of an int");
                // the answer is only a `str` when the name resolved to the builtin,
                // and a module that rebound it may hand back anything at all
                self.expect_dest(block, *dest, &RType::OBJECT, "str of an int");
            }
            Op::StrConcat {
                dest,
                lhs,
                rhs,
                consumes_lhs,
            } => {
                self.expect(block, lhs, &RType::STR, "str concatenation");
                self.expect(block, rhs, &RType::STR, "str concatenation");
                self.expect_dest(block, *dest, &RType::STR, "str concatenation");
                // an immediate has no register to empty, and a string appended to
                // itself would have its buffer moved out from under the copy still
                // reading it — the runtime refuses that pair too, so this says the
                // ir should never have offered it
                if *consumes_lhs && (!matches!(lhs, Value::Register(_)) || lhs == rhs) {
                    self.error(
                        Some(block),
                        "a consuming concatenation needs a register of its own to take over",
                    );
                }
            }
            Op::RaiseStandard { .. } => {}
            Op::Enter { dest, manager } => {
                self.expect(block, manager, &RType::OBJECT, "a context manager");
                self.expect_dest(block, *dest, &RType::OBJECT, "`__enter__`");
            }
            Op::ExitContext {
                dest,
                manager,
                exception,
            } => {
                self.expect(block, manager, &RType::OBJECT, "a context manager");
                self.expect(block, exception, &RType::OBJECT, "a context exit");
                self.expect_dest(block, *dest, &RType::BIT, "`__exit__`");
            }
            Op::DelegateIter { dest, src, .. } => {
                self.expect(block, src, &RType::OBJECT, "a delegation source");
                self.expect_dest(block, *dest, &RType::OBJECT, "a delegation iterator");
            }
            Op::DelegateStep { dest, inner, sent } => {
                self.expect(block, inner, &RType::OBJECT, "a delegation step");
                self.expect(block, sent, &RType::OBJECT, "a delegation step");
                self.expect_dest(
                    block,
                    *dest,
                    &RType::Tuple(Box::from([RType::OBJECT, RType::BIT])),
                    "a delegation step",
                );
            }
            Op::RaiseWith { value, .. } => {
                self.expect(block, value, &RType::OBJECT, "a raise with a value");
            }
            // the value is what the frame handed back, and it is stored as an object
            // for whichever face asks for it — a finish never gets a chance to widen
            // it later
            Op::FinishFrame { value } => {
                self.expect(block, value, &RType::OBJECT, "a frame's return value");
            }
        }
    }

    fn check_terminator(&mut self, block: BlockId, terminator: &Terminator) {
        for target in terminator.successors() {
            if self.function.block(target).is_none() {
                self.error(Some(block), format!("b{} does not exist", target.0));
            }
        }
        match terminator {
            Terminator::Branch { cond, .. } => {
                self.expect(block, cond, &RType::BIT, "branch condition");
            }
            Terminator::Return(value) => {
                let declared = self.function.ret.clone();
                let expected = match self.operand_type(block, value) {
                    Some(actual) if free_widening(&actual, &declared) => actual,
                    _ => declared,
                };
                self.expect(block, value, &expected, "return");
            }
            Terminator::NarrowShort { dest, src, .. } => {
                self.expect(block, src, &RType::INT, "narrow-short source");
                let declared = self.function.register(*dest).map(|decl| decl.ty.clone());
                if !matches!(declared, Some(RType::Primitive(Primitive::Fixed(_)))) {
                    self.error(
                        Some(block),
                        "narrow-short must write a machine integer".to_string(),
                    );
                }
            }
            Terminator::Goto(_) | Terminator::Unreachable => {}
        }
    }

    /// forward dataflow: a register may only be read where every path from entry
    /// has written it. parameters start assigned
    fn check_definite_assignment(&mut self) {
        let block_count = self.function.blocks.len();
        let register_count = self.function.registers.len();

        let entry_assigned: Vec<bool> = (0..register_count)
            .map(|index| index < self.function.param_count)
            .collect();

        // `None` means the block has not been reached yet, which is how an
        // unreached block avoids constraining its successors
        let mut incoming: Vec<Option<Vec<bool>>> = vec![None; block_count];
        incoming[0] = Some(entry_assigned);

        let mut queue = VecDeque::from([Function::entry()]);
        while let Some(id) = queue.pop_front() {
            let Some(block) = self.function.block(id) else {
                continue;
            };
            let Some(mut state) = incoming[id.index()].clone() else {
                continue;
            };
            for op in &block.ops {
                if let Some(dest) = op.dest()
                    && dest.index() < state.len()
                {
                    state[dest.index()] = true;
                }
            }
            // a handler is entered from *before* any of this block's writes, because
            // the very first operation can be the one that failed
            let entry_state = incoming[id.index()].clone();
            // a narrowing terminator writes its destination on one edge only: the
            // other is the path where the value did not fit and there is nothing to
            // have written
            let narrowed = match &block.terminator {
                Terminator::NarrowShort { dest, fits, .. } => Some((*dest, *fits)),
                _ => None,
            };
            let edges = block
                .terminator
                .successors()
                .into_iter()
                .map(|target| {
                    let mut state = state.clone();
                    if let Some((dest, fits)) = narrowed
                        && target == fits
                        && dest.index() < state.len()
                    {
                        state[dest.index()] = true;
                    }
                    (target, state)
                })
                .chain(
                    block
                        .error_target
                        .into_iter()
                        .filter_map(|target| entry_state.clone().map(|state| (target, state))),
                );
            for (target, state) in edges {
                if target.index() >= block_count {
                    continue;
                }
                let merged = match &incoming[target.index()] {
                    // a register is assigned on entry to a block only when every
                    // predecessor assigned it
                    Some(existing) => {
                        let merged: Vec<bool> =
                            existing.iter().zip(&state).map(|(a, b)| *a && *b).collect();
                        if merged == *existing {
                            continue;
                        }
                        merged
                    }
                    None => state.clone(),
                };
                incoming[target.index()] = Some(merged);
                queue.push_back(target);
            }
        }

        for (index, entry) in incoming.iter().enumerate() {
            let id = BlockId(index);
            let Some(block) = self.function.block(id) else {
                continue;
            };
            let Some(mut state) = entry.clone() else {
                continue; // unreachable block: nothing to prove about it
            };
            for op in &block.ops {
                for operand in op.operands() {
                    self.check_read(id, operand, &state);
                }
                if let Some(dest) = op.dest()
                    && dest.index() < state.len()
                {
                    state[dest.index()] = true;
                }
            }
            for operand in block.terminator.operands() {
                self.check_read(id, operand, &state);
            }
        }
    }

    fn check_read(&mut self, block: BlockId, value: &Value, state: &[bool]) {
        let Value::Register(id) = value else {
            return;
        };
        // a register the unbound-locals pass flagged carries the answer to whether it
        // was written, and every read of it tests that byte — so the path this analysis
        // is worried about raises `UnboundLocalError` instead of reading a slot
        if self
            .function
            .register(*id)
            .is_some_and(|decl| decl.may_be_unassigned)
        {
            return;
        }
        match state.get(id.index()) {
            Some(true) => {}
            // a *named* register is a local somebody wrote, and this is the one
            // verifier finding a user sees — it reaches them as the reason a function
            // stayed interpreted. `r2` tells them nothing; `value` tells them what to
            // change
            Some(false) => match self
                .function
                .register(*id)
                .and_then(|decl| decl.name.clone())
            {
                Some(name) => self.source_error(
                    Some(block),
                    format!(
                        "`{name}` is read on a path that does not assign it, which python \
                         answers with `UnboundLocalError`"
                    ),
                ),
                None => self.error(
                    Some(block),
                    format!("r{} is read before it is assigned", id.0),
                ),
            },
            None => {} // already reported as undeclared
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::FunctionBuilder;
    use crate::function::{BasicBlock, CallConvention, RegisterDecl};
    use crate::ops::{BinOp, CmpOp};

    fn reg(name: &str, ty: RType) -> RegisterDecl {
        RegisterDecl {
            borrowed: false,
            name: Some(name.to_string()),
            ty,
            may_be_unassigned: false,
        }
    }

    fn temp(ty: RType) -> RegisterDecl {
        RegisterDecl {
            name: None,
            ty,
            borrowed: false,
            may_be_unassigned: false,
        }
    }

    /// `def add(a: int, b: int) -> int: return a + b`
    fn add() -> Function {
        let mut entry = BasicBlock::new(Terminator::Return(Value::Register(RegisterId(2))));
        entry.ops.push(Op::IntBinary {
            dest: RegisterId(2),
            op: BinOp::Add,
            lhs: Value::Register(RegisterId(0)),
            rhs: Value::Register(RegisterId(1)),
        });
        Function {
            posonly: 0,
            kwonly: 0,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            range: None,
            name: "add".to_string(),
            param_count: 2,
            ret: RType::INT,
            convention: CallConvention::NativeInfallible,
            registers: vec![reg("a", RType::INT), reg("b", RType::INT), temp(RType::INT)],
            blocks: vec![entry],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
        }
    }

    #[test]
    fn a_well_formed_function_verifies() {
        assert_eq!(verify(&add()), Ok(()));
    }

    #[test]
    fn a_function_with_no_blocks_is_rejected() {
        let mut f = add();
        f.blocks.clear();
        let errors = verify(&f).unwrap_err();
        assert!(errors[0].message.contains("entry block"));
    }

    #[test]
    fn an_undeclared_register_is_rejected() {
        let mut f = add();
        f.blocks[0].terminator = Terminator::Return(Value::Register(RegisterId(7)));
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("r7 is not declared"))
        );
    }

    #[test]
    fn a_missing_block_target_is_rejected() {
        let mut f = add();
        f.blocks[0].terminator = Terminator::Goto(BlockId(4));
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("b4 does not exist"))
        );
    }

    #[test]
    fn adding_floats_with_the_int_op_is_rejected() {
        let mut f = add();
        f.registers[1].ty = RType::FLOAT;
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("expects int, found float"))
        );
    }

    #[test]
    fn a_write_that_does_not_match_the_register_type_is_rejected() {
        let mut f = add();
        f.registers[2].ty = RType::FLOAT;
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("produces int, but r2 is declared float"))
        );
    }

    #[test]
    fn returning_the_wrong_type_is_rejected() {
        let mut f = add();
        f.ret = RType::FLOAT;
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("return expects float"))
        );
    }

    #[test]
    fn a_bit_may_be_stored_where_a_bool_is_expected() {
        // the one free widening: both are a 0-or-1 byte
        let mut f = add();
        f.ret = RType::BOOL;
        f.registers[2].ty = RType::BIT;
        f.blocks[0].ops[0] = Op::IntCompare {
            dest: RegisterId(2),
            op: CmpOp::Lt,
            lhs: Value::Register(RegisterId(0)),
            rhs: Value::Register(RegisterId(1)),
        };
        assert_eq!(verify(&f), Ok(()));
    }

    #[test]
    fn a_bool_may_not_be_stored_where_a_bit_is_expected() {
        // the widening is one-way: a `bool` register could hold an error value
        let mut f = add();
        f.ret = RType::BIT;
        f.registers[2].ty = RType::BOOL;
        f.blocks[0].ops.clear();
        f.blocks[0].ops.push(Op::Assign {
            dest: RegisterId(2),
            src: Value::Bool(true),
        });
        let errors = verify(&f).unwrap_err();
        assert!(
            errors.iter().any(|e| e.message.contains("return expects")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_comparison_must_produce_a_bit() {
        let mut f = add();
        f.blocks[0].ops[0] = Op::IntCompare {
            dest: RegisterId(2),
            op: CmpOp::Lt,
            lhs: Value::Register(RegisterId(0)),
            rhs: Value::Register(RegisterId(1)),
        };
        let errors = verify(&f).unwrap_err();
        assert!(errors.iter().any(|e| e.message.contains("produces bit")));
    }

    #[test]
    fn branching_on_a_non_bit_is_rejected() {
        let mut f = add();
        f.blocks[0].terminator = Terminator::Branch {
            cond: Value::Register(RegisterId(0)),
            then_block: BlockId(0),
            else_block: BlockId(0),
        };
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("branch condition expects bit"))
        );
    }

    #[test]
    fn reading_a_register_before_it_is_assigned_is_rejected() {
        let mut f = add();
        // drop the op that defines r2, leaving the return reading it
        f.blocks[0].ops.clear();
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("r2 is read before it is assigned"))
        );
    }

    #[test]
    fn a_register_assigned_on_only_one_branch_is_rejected() {
        // b0: branch -> b1 (assigns r2) / b2 (does not); both goto b3, which reads r2
        let entry = BasicBlock::new(Terminator::Branch {
            cond: Value::Register(RegisterId(1)),
            then_block: BlockId(1),
            else_block: BlockId(2),
        });
        let mut then_block = BasicBlock::new(Terminator::Goto(BlockId(3)));
        then_block.ops.push(Op::Assign {
            dest: RegisterId(2),
            src: Value::Int(1),
        });
        let else_block = BasicBlock::new(Terminator::Goto(BlockId(3)));
        let exit = BasicBlock::new(Terminator::Return(Value::Register(RegisterId(2))));

        let f = Function {
            posonly: 0,
            kwonly: 0,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            range: None,
            name: "cond".to_string(),
            param_count: 2,
            ret: RType::INT,
            convention: CallConvention::NativeInfallible,
            registers: vec![reg("a", RType::INT), reg("c", RType::BIT), temp(RType::INT)],
            blocks: vec![entry, then_block, else_block, exit],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
        };
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("r2 is read before it is assigned"))
        );
    }

    #[test]
    fn a_register_assigned_on_every_branch_is_accepted() {
        let entry = BasicBlock::new(Terminator::Branch {
            cond: Value::Register(RegisterId(1)),
            then_block: BlockId(1),
            else_block: BlockId(2),
        });
        let mut then_block = BasicBlock::new(Terminator::Goto(BlockId(3)));
        then_block.ops.push(Op::Assign {
            dest: RegisterId(2),
            src: Value::Int(1),
        });
        let mut else_block = BasicBlock::new(Terminator::Goto(BlockId(3)));
        else_block.ops.push(Op::Assign {
            dest: RegisterId(2),
            src: Value::Int(2),
        });
        let exit = BasicBlock::new(Terminator::Return(Value::Register(RegisterId(2))));

        let f = Function {
            posonly: 0,
            kwonly: 0,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            range: None,
            name: "cond".to_string(),
            param_count: 2,
            ret: RType::INT,
            convention: CallConvention::NativeInfallible,
            registers: vec![reg("a", RType::INT), reg("c", RType::BIT), temp(RType::INT)],
            blocks: vec![entry, then_block, else_block, exit],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
        };
        assert_eq!(verify(&f), Ok(()));
    }

    #[test]
    fn a_loop_back_edge_does_not_prove_assignment() {
        // r2 is assigned only in the loop body, and read in the header, which the
        // back edge reaches without going through the body on the first iteration
        let mut header = BasicBlock::new(Terminator::Branch {
            cond: Value::Register(RegisterId(1)),
            then_block: BlockId(1),
            else_block: BlockId(2),
        });
        header.ops.push(Op::Assign {
            dest: RegisterId(3),
            src: Value::Register(RegisterId(2)),
        });
        let mut body = BasicBlock::new(Terminator::Goto(BlockId(0)));
        body.ops.push(Op::Assign {
            dest: RegisterId(2),
            src: Value::Int(1),
        });
        let exit = BasicBlock::new(Terminator::Return(Value::Int(0)));

        let f = Function {
            posonly: 0,
            kwonly: 0,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            range: None,
            name: "loop".to_string(),
            param_count: 2,
            ret: RType::INT,
            convention: CallConvention::NativeInfallible,
            registers: vec![
                reg("a", RType::INT),
                reg("c", RType::BIT),
                temp(RType::INT),
                temp(RType::INT),
            ],
            blocks: vec![header, body, exit],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
        };
        let errors = verify(&f).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("r2 is read before it is assigned"))
        );
    }

    #[test]
    fn an_unreachable_block_does_not_constrain_anything() {
        let mut f = add();
        // an orphan block reading an unassigned register is not a program error
        f.blocks
            .push(BasicBlock::new(Terminator::Return(Value::Register(
                RegisterId(2),
            ))));
        assert_eq!(verify(&f), Ok(()));
    }

    #[test]
    fn widening_something_already_object_is_rejected() {
        // `box` widens *to* object; widening an object is a no-op the frontend
        // should never emit, and catching it keeps `to_object` honest
        let mut f = add();
        f.registers.push(temp(RType::OBJECT));
        f.registers.push(temp(RType::OBJECT));
        f.blocks[0].ops.push(Op::Box {
            dest: RegisterId(3),
            src: Value::Register(RegisterId(0)),
        });
        f.blocks[0].ops.push(Op::Box {
            dest: RegisterId(4),
            src: Value::Register(RegisterId(3)),
        });
        let errors = verify(&f).unwrap_err();
        assert!(
            errors.iter().any(|e| e.message.contains("already object")),
            "{errors:?}"
        );
    }

    #[test]
    fn widening_a_known_class_object_to_object_is_allowed() {
        // a `str` is already a PyObject, but the widening still takes a reference
        let mut f = add();
        f.registers.push(temp(RType::STR));
        f.registers.push(temp(RType::OBJECT));
        f.blocks[0].ops.push(Op::Assign {
            dest: RegisterId(3),
            src: Value::Str("x".to_string()),
        });
        f.blocks[0].ops.push(Op::Box {
            dest: RegisterId(4),
            src: Value::Register(RegisterId(3)),
        });
        assert_eq!(verify(&f), Ok(()));
    }

    #[test]
    fn a_tuple_get_past_the_end_is_rejected() {
        let tuple_ty = RType::Tuple(Box::new([RType::INT, RType::INT]));
        let mut entry = BasicBlock::new(Terminator::Return(Value::Register(RegisterId(3))));
        entry.ops.push(Op::TupleBuild {
            dest: RegisterId(2),
            items: vec![
                Value::Register(RegisterId(0)),
                Value::Register(RegisterId(1)),
            ],
        });
        entry.ops.push(Op::TupleGet {
            dest: RegisterId(3),
            src: Value::Register(RegisterId(2)),
            index: 5,
        });
        let f = Function {
            posonly: 0,
            kwonly: 0,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            range: None,
            name: "pick".to_string(),
            param_count: 2,
            ret: RType::INT,
            convention: CallConvention::NativeInfallible,
            registers: vec![
                reg("a", RType::INT),
                reg("b", RType::INT),
                temp(tuple_ty),
                temp(RType::INT),
            ],
            blocks: vec![entry],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
        };
        let errors = verify(&f).unwrap_err();
        assert!(errors.iter().any(|e| e.message.contains("past the end")));
    }

    #[test]
    fn verify_module_collects_errors_from_every_function() {
        let mut broken = add();
        broken.name = "broken".to_string();
        broken.ret = RType::FLOAT;
        let module = ModuleIr {
            name: crate::ModuleName::new("m"),
            functions: vec![add(), broken],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
        };
        let errors = verify_module(&module).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].function, "broken");
    }

    #[test]
    fn a_call_argument_must_have_the_representation_the_callee_declares() {
        // `add` takes two ints. handing it an object is a call the C compiler would
        // reject; handing it a *machine* integer is one it would accept and get wrong,
        // because both are integers as far as C is concerned
        let mut caller = add();
        caller.name = "caller".to_string();
        caller.registers.push(reg("wrong", RType::OBJECT));
        caller.blocks[0].ops.insert(
            0,
            Op::CallNative {
                dest: None,
                owner: None,
                callee: "add".to_string(),
                args: vec![
                    Value::Register(RegisterId(3)),
                    Value::Register(RegisterId(1)),
                ],
            },
        );
        let module = ModuleIr {
            name: crate::ModuleName::new("m"),
            functions: vec![add(), caller],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
        };
        let errors = verify_module(&module).unwrap_err();
        assert!(
            errors.iter().any(|e| e
                .message
                .contains("declares `a` as int, but the argument is object")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_release_set_that_misses_a_held_reference_is_rejected() {
        // exactly the bug the refcount pass nearly shipped: `scratch` is dead by
        // liveness at the early return and absolutely must still be released
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let cond = builder.param("c", RType::BIT);
        let scratch = builder.temp(RType::STR);
        let early = builder.new_block();
        let late = builder.new_block();
        builder.assign(scratch, Value::Str("x".to_string()));
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: early,
            else_block: late,
        });
        builder.switch_to(early);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(late);
        builder.terminate(Terminator::Return(Value::Int(0)));
        let mut function = builder.finish();

        // the correct answer verifies
        for block in &mut function.blocks {
            block.owned_at_exit = Some(vec![cond, scratch]);
        }
        assert_eq!(verify(&function), Ok(()));

        // the liveness answer does not
        function.blocks[1].owned_at_exit = Some(vec![cond]);
        let errors = verify(&function).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].block, Some(BlockId(1)));
        assert!(
            errors[0]
                .message
                .contains("may hold a reference here but is not released"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn a_register_first_written_after_an_exit_need_not_be_released_there() {
        // the reduction the pass exists for stays legal
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let cond = builder.param("c", RType::BIT);
        let later = builder.temp(RType::STR);
        let early = builder.new_block();
        let rest = builder.new_block();
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: early,
            else_block: rest,
        });
        builder.switch_to(early);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(rest);
        builder.assign(later, Value::Str("x".to_string()));
        builder.terminate(Terminator::Return(Value::Int(0)));
        let mut function = builder.finish();

        function.blocks[0].owned_at_exit = Some(vec![]);
        function.blocks[1].owned_at_exit = Some(vec![]);
        function.blocks[2].owned_at_exit = Some(vec![later]);
        assert_eq!(verify(&function), Ok(()));
    }

    #[test]
    fn a_loop_carried_reference_must_be_released_at_every_exit_in_the_loop() {
        // the back edge is what makes this different from the straight-line case:
        // the header can be reached *after* the body wrote the register
        let mut builder = FunctionBuilder::new("f", RType::NONE);
        let held = builder.local("held", RType::STR);
        let header = builder.new_block();
        let body = builder.new_block();
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.terminate(Terminator::Branch {
            cond: Value::Bit(true),
            then_block: body,
            else_block: header,
        });
        builder.switch_to(body);
        builder.assign(held, Value::Str("x".to_string()));
        builder.terminate(Terminator::Goto(header));
        let mut function = builder.finish();

        for block in &mut function.blocks {
            block.owned_at_exit = Some(vec![held]);
        }
        assert_eq!(verify(&function), Ok(()));

        function.blocks[1].owned_at_exit = Some(vec![]);
        let errors = verify(&function).unwrap_err();
        assert_eq!(errors[0].block, Some(BlockId(1)));
    }

    #[test]
    fn a_borrowed_register_is_not_held() {
        let mut builder = FunctionBuilder::new("f", RType::NONE);
        let held = builder.temp(RType::STR);
        builder.assign(held, Value::Str("x".to_string()));
        builder.terminate(Terminator::Return(Value::None));
        let mut function = builder.finish();
        function.blocks[0].owned_at_exit = Some(vec![]);
        assert!(verify(&function).is_err());

        function.registers[held.index()].borrowed = true;
        assert_eq!(verify(&function), Ok(()));
    }

    #[test]
    fn a_function_the_pass_has_not_run_on_is_not_checked() {
        // codegen's conservative discipline applies until the pass narrows it
        let mut builder = FunctionBuilder::new("f", RType::NONE);
        let held = builder.temp(RType::STR);
        builder.assign(held, Value::Str("x".to_string()));
        builder.terminate(Terminator::Return(Value::None));
        let function = builder.finish();
        assert!(function.blocks[0].owned_at_exit.is_none());
        assert_eq!(verify(&function), Ok(()));
    }
}
