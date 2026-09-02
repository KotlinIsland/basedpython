//! the operations BIR is built from
//!
//! BIR is a three-address register machine: every operation reads [`Value`]s and
//! writes at most one [`RegisterId`]. blocks carry a list of [`Op`]s and end in
//! exactly one [`Terminator`] — keeping the terminator out of the op list means
//! "every block is terminated" is true by construction rather than by check.

use crate::rtype::RType;

/// index of a register within a [`Function`](crate::function::Function)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegisterId(pub usize);

/// index of a block within a [`Function`](crate::function::Function)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub usize);

impl RegisterId {
    pub fn index(self) -> usize {
        self.0
    }
}

impl BlockId {
    pub fn index(self) -> usize {
        self.0
    }
}

/// an operand: either a register or an immediate
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Register(RegisterId),
    /// an integer immediate, small enough for the tagged representation
    Int(i64),
    /// an integer immediate at machine width — a literal has no representation of
    /// its own, and this is the one a `fixed` register wants
    Fixed(i64),
    Float(f64),
    Bool(bool),
    /// a comparison result written directly, which a `bool` cannot stand in for
    /// because the two are different representations
    Bit(bool),
    None,
    /// a string literal, interned into a module static by the emitter
    Str(String),
    /// a bytes literal, built once into a module static by the emitter
    ///
    /// `bytes` has no representation of its own, so this is an `object` — what it
    /// buys is that the constant exists at all, not that operations on it are
    /// cheaper than the protocol's
    Bytes(Box<[u8]>),
}

impl Value {
    pub fn register(id: RegisterId) -> Self {
        Self::Register(id)
    }

    /// the type of an immediate, or `None` for a register (whose type comes from
    /// the function's register table)
    pub fn immediate_type(&self) -> Option<RType> {
        match self {
            Self::Register(_) => None,
            Self::Int(_) => Some(RType::INT),
            Self::Fixed(_) => Some(RType::fixed(crate::rtype::IntWidth::I64)),
            Self::Float(_) => Some(RType::FLOAT),
            Self::Bool(_) => Some(RType::BOOL),
            Self::Bit(_) => Some(RType::BIT),
            Self::None => Some(RType::NONE),
            Self::Str(_) => Some(RType::STR),
            Self::Bytes(_) => Some(RType::OBJECT),
        }
    }
}

/// whether an operation is the augmented form
///
/// python offers the left operand of `a += b` a chance to perform the operation *on
/// itself* before falling back to the binary one, and what it hands back is what `a`
/// is rebound to — so returning `self` and returning a fresh object are both correct
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mutation {
    Fresh,
    InPlace,
}

/// arithmetic operators, over both tagged integers and doubles
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    FloorDiv,
    Mod,
    TrueDiv,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl BinOp {
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::FloorDiv => "//",
            Self::Mod => "%",
            Self::TrueDiv => "/",
            Self::Pow => "**",
            Self::BitAnd => "&",
            Self::BitOr => "|",
            Self::BitXor => "^",
            Self::Shl => "<<",
            Self::Shr => ">>",
        }
    }

    /// whether the operation can fail for otherwise-valid operands, which is what
    /// decides if the caller needs an error check after it
    pub const fn can_fail(self) -> bool {
        // `**` can raise on a negative exponent of an int, and a shift on a
        // negative count
        matches!(
            self,
            Self::FloorDiv | Self::Mod | Self::TrueDiv | Self::Pow | Self::Shl | Self::Shr
        )
    }
}

/// comparison operators, producing a [`Bit`](crate::rtype::Primitive::Bit)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// the `!s` / `!r` / `!a` conversion on an f-string interpolation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Conversion {
    None,
    Str,
    Repr,
    Ascii,
}

impl Conversion {
    /// the `BY_CONV_*` constant the runtime switches on
    pub const fn c_name(self) -> &'static str {
        match self {
            Self::None => "BY_CONV_NONE",
            Self::Str => "BY_CONV_STR",
            Self::Repr => "BY_CONV_REPR",
            Self::Ascii => "BY_CONV_ASCII",
        }
    }
}

/// unary operators
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Neg,
    Not,
    Invert,
}

/// which error class a [`Op::RaiseStandard`] raises
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardError {
    TypeError,
    ValueError,
    ZeroDivisionError,
    AssertionError,
    RuntimeError,
    IndexError,
    KeyError,
    NotImplementedError,
    StopIteration,
    OverflowError,
    AttributeError,
}

impl StandardError {
    pub const fn c_name(self) -> &'static str {
        match self {
            Self::TypeError => "PyExc_TypeError",
            Self::ValueError => "PyExc_ValueError",
            Self::ZeroDivisionError => "PyExc_ZeroDivisionError",
            Self::AssertionError => "PyExc_AssertionError",
            Self::RuntimeError => "PyExc_RuntimeError",
            Self::IndexError => "PyExc_IndexError",
            Self::KeyError => "PyExc_KeyError",
            Self::NotImplementedError => "PyExc_NotImplementedError",
            Self::StopIteration => "PyExc_StopIteration",
            Self::OverflowError => "PyExc_OverflowError",
            Self::AttributeError => "PyExc_AttributeError",
        }
    }

    /// the builtin name a user writes, for matching a `raise` target
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "TypeError" => Self::TypeError,
            "ValueError" => Self::ValueError,
            "ZeroDivisionError" => Self::ZeroDivisionError,
            "AssertionError" => Self::AssertionError,
            "RuntimeError" => Self::RuntimeError,
            "IndexError" => Self::IndexError,
            "KeyError" => Self::KeyError,
            "NotImplementedError" => Self::NotImplementedError,
            "StopIteration" => Self::StopIteration,
            "OverflowError" => Self::OverflowError,
            "AttributeError" => Self::AttributeError,
            _ => return None,
        })
    }
}

/// a straight-line operation
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// `dest = src`, with no representation change
    Assign { dest: RegisterId, src: Value },
    /// arithmetic on tagged integers
    IntBinary {
        dest: RegisterId,
        op: BinOp,
        lhs: Value,
        rhs: Value,
    },
    /// arithmetic on unboxed doubles
    FloatBinary {
        dest: RegisterId,
        op: BinOp,
        lhs: Value,
        rhs: Value,
    },
    /// comparison of tagged integers
    IntCompare {
        dest: RegisterId,
        op: CmpOp,
        lhs: Value,
        rhs: Value,
    },
    /// arithmetic through the abstract object protocol, for `object` operands
    ObjectBinary {
        dest: RegisterId,
        op: BinOp,
        lhs: Value,
        rhs: Value,
        mutation: Mutation,
    },
    /// comparison through the abstract object protocol
    ObjectCompare {
        dest: RegisterId,
        op: CmpOp,
        lhs: Value,
        rhs: Value,
    },
    /// comparison of two `str`s, which the type of the operands settles
    ///
    /// the abstract protocol exists to find *whose* comparison to run, and a pair
    /// of exact `str`s answers that without looking: it is `str`'s own. a subclass
    /// may have said otherwise, so the emitted form still checks the exact type and
    /// falls back to [`Self::ObjectCompare`] when it does not hold
    StrCompare {
        dest: RegisterId,
        op: CmpOp,
        lhs: Value,
        rhs: Value,
    },
    /// python truthiness of an object, which a user `__bool__` can make fail
    Truthy { dest: RegisterId, src: Value },
    /// comparison of unboxed doubles
    FloatCompare {
        dest: RegisterId,
        op: CmpOp,
        lhs: Value,
        rhs: Value,
    },
    /// negation or logical not
    Unary {
        dest: RegisterId,
        op: UnaryOp,
        operand: Value,
    },
    /// a call to another compiled function, by name, using the native convention
    CallNative {
        dest: Option<RegisterId>,
        /// the class the callee is a method of, when it is one.
        ///
        /// a method of an emitted class is final by construction — the static type
        /// object does not set `Py_TPFLAGS_BASETYPE`, and a base class declines —
        /// so naming it here is enough to reach it directly, with no vtable
        owner: Option<String>,
        callee: String,
        args: Vec<Value>,
    },
    /// give an unboxed value an object representation
    Box { dest: RegisterId, src: Value },
    /// take an object apart into an unboxed representation. this is a *narrowing*
    /// and so is always checked — it raises `TypeError` if the object does not
    /// have the expected type
    Unbox {
        dest: RegisterId,
        src: Value,
        /// the type being unboxed to, which is the destination register's type
        to: RType,
    },
    /// `lhs <op> rhs` where `lhs` is a double and `rhs` is any object, for the
    /// case where the checker has said the result is a `float`
    ///
    /// the object is *tested* rather than the double boxed to meet it: an exact
    /// float is the only type whose value a double already holds, and anything
    /// else goes through the object protocol exactly as it would have. what that
    /// saves is the `PyFloatObject` allocated to hold a value already in a
    /// register — the allocation that made a running sum over a `list[float]`
    /// slower than the interpreter
    FloatObjectBinary {
        dest: RegisterId,
        op: BinOp,
        lhs: Value,
        rhs: Value,
    },
    /// `isinstance(src, class)`
    IsInstance {
        dest: RegisterId,
        src: Value,
        class: Value,
    },
    /// the attribute a class pattern names, or the *missing* answer
    ///
    /// a missing attribute is no match rather than an error, so this has a third
    /// answer beyond a value and a failure — see [`Op::IsMissing`]
    MatchAttr {
        dest: RegisterId,
        subject: Value,
        /// the attribute by name, for a keyword sub-pattern
        name: Option<String>,
        /// the class and position, for a positional one — which names its
        /// attribute through `__match_args__` at runtime
        class: Option<Value>,
        index: i64,
        count: i64,
    },
    /// whether a call on `src` may go straight to the body this module emitted for
    /// `class`'s `method`, rather than through the object protocol
    ///
    /// the two questions are one test because a call site is licensed by both and by
    /// neither alone: `src` has to be *exactly* a `class` — a subclass written in the
    /// interpreter has a body of its own — and `class` has to still answer `method`
    /// with what was compiled, which a rebinding after import undoes. what each
    /// question costs at runtime is in `By_MethodStands`
    MethodStands {
        dest: RegisterId,
        src: Value,
        class: String,
        method: String,
    },
    /// whether `src`'s own dict holds `method`, so that the body this module emitted
    /// for `class` is *not* what an attribute lookup would answer with
    ///
    /// a method is a non-data descriptor, so a value stored on the instance under the
    /// same name wins over the class's entry: `a.double = f` makes `a.double()` call
    /// `f`. that is the one thing a call on a class python can neither subclass nor
    /// rebind still has to ask, and the answer cannot be settled at import — nothing
    /// about a class says whether one of its instances has been written to
    ///
    /// where `class`'s instances keep no dict of their own there is nowhere to write
    /// such a value, and the question is answered `false` when the C is written
    DictShadows {
        dest: RegisterId,
        src: Value,
        class: String,
        method: String,
    },
    /// whether a class pattern's lookup found nothing
    IsMissing { dest: RegisterId, src: Value },
    /// an element or a slice of a sequence a `match` case is taking apart
    ///
    /// counted from the *end* where a star pattern has made the front index
    /// unknowable, and producing a list for the star itself
    MatchSlice {
        dest: RegisterId,
        sequence: Value,
        /// where the star's own elements begin
        start: i64,
        /// how many fixed elements follow it
        after: i64,
        /// whether this is the star's list, or the single element at `after`
        rest: bool,
    },
    /// `lhs <op> rhs` with one side a proven double and the other an object that
    /// can only be an int or a float
    ///
    /// the object is *tested* rather than the double boxed to reach it, and a
    /// value that fails the test takes the object protocol — which is what keeps
    /// `1.5 < 10**400` exact, since python compares an int against a float without
    /// converting either
    FloatObjectCompare {
        dest: RegisterId,
        op: CmpOp,
        lhs: Value,
        rhs: Value,
    },
    /// `map[key]` for a mapping pattern, or the *missing* answer
    MatchKey {
        dest: RegisterId,
        map: Value,
        key: Value,
    },
    /// the dict a mapping pattern's `**rest` binds
    MatchRest {
        dest: RegisterId,
        map: Value,
        /// the keys the pattern named, as a tuple
        keys: Value,
    },
    /// `__aenter__` on `manager`, or `__aexit__` when `exception` is present
    ///
    /// both hand back an *awaitable* rather than an answer, so the caller awaits
    /// what comes back — which is the whole difference from the synchronous pair
    AsyncContext {
        dest: RegisterId,
        manager: Value,
        exception: Option<Value>,
    },
    /// `__aiter__` on `src`, or `__anext__` when `next`
    ///
    /// separate from an ordinary method call because `async for` reports an object
    /// it cannot iterate, where an attribute lookup reports a missing attribute
    AsyncIter {
        dest: RegisterId,
        src: Value,
        next: bool,
    },
    /// whether `src` has the shape a mapping pattern matches
    IsMapping { dest: RegisterId, src: Value },
    /// whether `src` has the shape a sequence pattern matches
    ///
    /// the interpreter's own test: a type flagged as a sequence, which `str`,
    /// `bytes` and `bytearray` are deliberately not — so `case [a, b]:` never
    /// takes a two-character string apart
    IsSequence { dest: RegisterId, src: Value },
    /// `value in container`, or `not in` when negated
    ///
    /// the container protocol, which is `__contains__` where a type has one and a
    /// scan otherwise — so it is not expressible as a comparison
    Contains {
        dest: RegisterId,
        value: Value,
        container: Value,
        negated: bool,
    },
    /// `lhs is rhs`, or `is not` when negated
    ///
    /// identity, not equality: it compares the pointers and asks nothing of the
    /// objects, which is what makes it the test `case None:` uses and what keeps a
    /// type with its own `__eq__` from answering for it
    Identity {
        dest: RegisterId,
        lhs: Value,
        rhs: Value,
        negated: bool,
    },
    /// widen an integer to a float, the way python's numeric tower does
    ///
    /// this is the conversion `float.__add__` performs on an `int` operand before
    /// adding, so a mixed pair lowered as a double operation *is* the operation
    /// rather than an approximation of it. an integer too large to be a float
    /// raises `OverflowError`, exactly as python does
    IntToFloat { dest: RegisterId, src: Value },
    /// build a fixed-length tuple from its elements
    TupleBuild { dest: RegisterId, items: Vec<Value> },
    /// unpack a value into a fixed-length tuple, the way an assignment target list
    /// does. `starred` is the slot that collects the surplus into a list
    ///
    /// the destination is a tuple rather than one register per target because an op
    /// has exactly one destination — a second would be invisible to liveness
    Unpack {
        dest: RegisterId,
        src: Value,
        starred: Option<usize>,
    },
    /// a call whose arguments are a tuple and a dict rather than a fixed list —
    /// what `f(*args, **kwargs)` needs, because the binding happens at runtime
    CallUnpacked {
        dest: RegisterId,
        callee: Value,
        args: Value,
        kwargs: Option<Value>,
    },
    /// merge everything `source` holds into a display under construction — what a
    /// `*` or a `**` in one does. `mapping` is the `**` form
    Extend {
        dest: RegisterId,
        container: Value,
        source: Value,
        mapping: bool,
    },
    /// build an unboxed array from its elements — a `list` display whose elements
    /// are stored in a buffer of their own rather than as a `PyObject *` each
    ArrayNew { dest: RegisterId, items: Vec<Value> },
    /// read one element, bounds-checked the way a `list` index is
    ArrayGet {
        dest: RegisterId,
        array: Value,
        index: Value,
    },
    /// write one element. `dest` is the status bit, as for every checked write
    ArraySet {
        dest: RegisterId,
        array: Value,
        index: Value,
        value: Value,
    },
    /// how many elements it holds, which is a field read and cannot fail.
    ///
    /// the destination decides the representation: a tagged `int` where the length
    /// reaches source code, a fixed width where the lowering made the counter itself
    ArrayLen { dest: RegisterId, array: Value },
    /// read one element at an index already known to be in range, so it needs no
    /// check.
    ///
    /// either the compiler owns the counter — a `for` over an array, where it is a
    /// machine integer — or a loop guard *proved* a source counter, where it is the
    /// tagged one that loop advances. an index neither covers takes [`Self::ArrayGet`]
    ArrayRead {
        dest: RegisterId,
        array: Value,
        index: Value,
    },
    /// append one, growing the buffer when it is full
    ArrayPush {
        dest: RegisterId,
        array: Value,
        value: Value,
    },
    /// `del container[index]`
    DeleteItem {
        dest: RegisterId,
        container: Value,
        index: Value,
    },
    /// `del receiver.name`
    DeleteAttr {
        dest: RegisterId,
        receiver: Value,
        name: String,
    },
    /// a `tuple` from anything iterable, which is how a starred tuple display is
    /// built: the pieces go into a list, and the list becomes the tuple
    ToTuple { dest: RegisterId, src: Value },
    /// read one element of a fixed-length tuple
    TupleGet {
        dest: RegisterId,
        src: Value,
        index: usize,
    },
    /// a call to a name the compilation unit does not own: resolved through the
    /// module namespace then builtins, and invoked with the python convention
    CallPython {
        dest: RegisterId,
        callee: String,
        args: Vec<Value>,
    },
    /// the module a `from` statement reads names off
    ///
    /// `__import__(name, globals, NULL, fromlist, level)`. the fromlist is what makes
    /// the importer resolve a *submodule* of that name, and the globals are what a
    /// relative import resolves its package from — the module dict has the
    /// `__name__` and `__package__` the interpreter would have read off the frame
    ImportModule {
        dest: RegisterId,
        name: String,
        fromlist: Box<[String]>,
        level: u32,
    },
    /// one name off an imported module
    ///
    /// not a plain attribute read: a name the module does not have is an
    /// `ImportError`, which is what a guarded lazy import catches
    ImportFrom {
        dest: RegisterId,
        module: Value,
        name: String,
    },
    /// `with EXPR`: the manager's `__enter__`
    Enter { dest: RegisterId, manager: Value },
    /// `__exit__`, on the normal path or with a live exception.
    ///
    /// `dest` is a bit: set when the exception was *suppressed*, so the caller
    /// re-raises when it is clear
    ExitContext {
        dest: RegisterId,
        manager: Value,
        /// the live exception, or `None` on the normal path
        exception: Value,
    },
    /// the iterator a delegation drives: `iter(x)` or `x.__await__()`
    DelegateIter {
        dest: RegisterId,
        src: Value,
        /// `await` uses `__await__`; `yield from` uses the iteration protocol
        awaitable: bool,
    },
    /// one step of a delegation: send a value in, and report which of the three
    /// outcomes happened.
    ///
    /// the destination is a fixed `(object, bit)` tuple rather than two registers,
    /// because [`Self::dest`] reports *one* — a second destination would be invisible
    /// to liveness, and a register that is never killed is never released either
    DelegateStep {
        dest: RegisterId,
        inner: Value,
        sent: Value,
    },
    /// raise a standard error carrying a value.
    ///
    /// `assert cond, msg` is the shape that needs it: the message is a value rather
    /// than a literal, so the instance has to be built from it
    RaiseWith { error: StandardError, value: Value },
    /// a resumable frame has run to its end, handing back `value`.
    ///
    /// this is the *finish* of a generator or coroutine frame, and it is deliberately
    /// not a raise. python reports a finish as `StopIteration(value)`, but building
    /// that exception is only one of the two faces a finish has: the send slot
    /// (`am_send`) answers `PYGEN_RETURN` with the value itself and no exception at
    /// all. leaving the choice to codegen means the frame stores the value and the
    /// consumer decides, instead of every finish paying for an exception that is
    /// usually taken apart again immediately.
    ///
    /// keeping it apart from [`Self::RaiseStandard`] is the *correctness* half. a
    /// body that writes `raise StopIteration` has raised an exception, which python
    /// eventually converts to `RuntimeError` (pep 479) — it has not finished the
    /// frame with a value. the two lower to different operations here so that no
    /// later stage has to guess which was meant from the error class
    FinishFrame { value: Value },
    /// read a shared closure cell: a field that starts unset.
    ///
    /// unlike [`Self::GetField`] this can fail — reading a cell before anything
    /// wrote it is `UnboundLocalError`, which is what python does and what keeps a
    /// zeroed field from reading back as a valid value
    GetCell {
        dest: RegisterId,
        receiver: Value,
        class: String,
        field: String,
        /// whether the reading frame *closes over* the name rather than owning it,
        /// which is the difference between `NameError` and `UnboundLocalError`
        free: bool,
    },
    /// allocate an instance of an emitted class from its field values
    ///
    /// used for a closure environment: the values are copied in where the `def`
    /// runs, which is why only captures that cannot change afterwards are accepted
    NewInstance {
        dest: RegisterId,
        class: String,
        /// one entry per field, in layout order. `None` leaves the field *unset*,
        /// which is how a shared closure cell starts
        fields: Vec<Option<Value>>,
    },
    /// bind a method of an emitted class to a receiver, giving a callable
    ///
    /// this is what a nested function's name is bound to: `PyCFunction_NewEx` with
    /// the environment as `self`
    MakeClosure {
        dest: RegisterId,
        class: String,
        method: String,
        env: Value,
    },
    /// read a name this frame does not bind: the module namespace, then builtins.
    ///
    /// resolved on every read for the same reason a call is — a module global may
    /// be rebound, and python would see it
    LoadGlobal { dest: RegisterId, name: String },
    /// the module namespace itself, which is what `globals()` answers with
    ///
    /// the builtin cannot be called for it. `globals()` reads the *calling* frame's
    /// namespace, and a compiled function pushes no frame — so it would answer with
    /// whatever frame happens to be underneath, which is the caller's, in another
    /// module. reading one gave `None` for a name the module plainly binds, and
    /// `globals()["x"] = 1` bound `x` in the caller instead.
    ///
    /// the dict this reaches is the same one [`Self::LoadGlobal`] and
    /// [`Self::StoreGlobal`] reach, so a write through it is visible at once to the
    /// rest of the module and to the interpreted twin, exactly as python's is
    ModuleDict { dest: RegisterId },
    /// bind a name in the module namespace: what an assignment under a `global`
    /// declaration does
    ///
    /// the other half of [`Self::LoadGlobal`], and it has to reach the same place. a
    /// register write is private to the frame, where python's binding is the module's
    /// — visible at once to every other reader, the interpreted twin included
    StoreGlobal {
        /// the status of the store, zero on success
        dest: RegisterId,
        name: String,
        value: Value,
    },
    /// unbind a name in the module namespace: `del x` under a `global x`
    ///
    /// a name that is not bound is a `NameError`, which is not what deleting from a
    /// dict raises — see `By_DeleteGlobal`
    DeleteGlobal { dest: RegisterId, name: String },
    /// unbind a local: `del x` where `x` is a register
    ///
    /// the unbound state is the byte the unbound-locals pass gives a register that
    /// some path may read before writing — set by every write, tested by every read.
    /// `del` puts it back to zero, so afterwards a read raises `UnboundLocalError`
    /// with python's own wording, a second `del` raises the same, and a later write
    /// binds the name again.
    ///
    /// the target is a `RegisterId` rather than a `Value` on purpose: this is a
    /// *place* and not a value, and a pass that substitutes an immediate into an
    /// operand would otherwise quietly retarget the unbinding. it is `dest` so that
    /// everything renumbering registers reaches it, and [`Self::unbinds`] is what
    /// tells the analyses that it is read as well as written
    DeleteLocal { dest: RegisterId },
    /// the type object of a class this module emits, by identity rather than by name
    ///
    /// a class decorator replaces the *namespace* entry, and a module may rebind the
    /// name outright — so a namespace read answers with whatever is there now. this
    /// answers with the class the `class` statement made, which is what python's own
    /// `__class__` cell holds
    LoadClass { dest: RegisterId, class: String },
    /// a call to a callable held in a register — a parameter, a local, anything
    /// that is a *value* rather than a name to resolve
    CallValue {
        dest: RegisterId,
        callee: Value,
        args: Vec<Value>,
    },
    /// `receiver.name(args)` through the object protocol
    CallMethod {
        dest: RegisterId,
        receiver: Value,
        name: String,
        args: Vec<Value>,
    },
    /// `receiver.field` as a direct struct read, for a receiver whose class the
    /// compiler emitted. no hash lookup and no descriptor
    GetField {
        dest: RegisterId,
        receiver: Value,
        class: String,
        field: String,
    },
    /// `receiver.field = value` as a direct struct write
    SetField {
        receiver: Value,
        class: String,
        field: String,
        value: Value,
    },
    /// `receiver.name`
    GetAttr {
        dest: RegisterId,
        receiver: Value,
        name: String,
    },
    /// `receiver.name = value`
    SetAttr {
        /// a bit register that is 2 on failure, so the error check has somewhere
        /// to look
        dest: RegisterId,
        receiver: Value,
        name: String,
        value: Value,
    },
    /// a list display, from already-boxed elements
    BuildList { dest: RegisterId, items: Vec<Value> },
    /// a set display
    BuildSet { dest: RegisterId, items: Vec<Value> },
    /// a tuple display
    BuildTuple { dest: RegisterId, items: Vec<Value> },
    /// a dict display, as alternating keys and values
    BuildDict { dest: RegisterId, pairs: Vec<Value> },
    /// `container[index]`
    GetItem {
        dest: RegisterId,
        container: Value,
        /// an object, or an integer register — an integer index stays in its
        /// register on the fast path rather than being boxed for the lookup
        index: Value,
    },
    /// `k in d` answered by the very lookup `d[k]` would go on to make
    ///
    /// a membership test and the read that follows it hash the same key and walk
    /// the same table, and on a histogram loop that pair is most of the running
    /// time. this asks the table once and reports both answers in one register: a
    /// new reference to the value where the key is there, and null where it is
    /// not. failure is null with an exception set, the same three outcomes
    /// [`Self::IterNext`] has and tested the same way
    ///
    /// asking once is a *runtime*-checked fact rather than a static one, because
    /// both things it would take to make asking twice observable are ordinary
    /// python. a dict subclass may have overridden `__contains__` or
    /// `__getitem__`, and a key may have a `__hash__` that counts how often it is
    /// called — so the single probe is taken only for an exact dict keyed by an
    /// exact `str`, and everything else goes through the protocol twice, in the
    /// order it would have: `__contains__`, and then `__getitem__` only where that
    /// said yes
    DictFind {
        dest: RegisterId,
        container: Value,
        key: Value,
    },
    /// `s[i]` where the container is a `str` and the index an integer
    ///
    /// a character of a `str` is a `str`, so this writes its own representation and
    /// the check the general form needs on the way back is not needed here. a
    /// subclass may have overridden `__getitem__` and may hand back anything at all,
    /// so the emitted form checks the exact type and falls back to the protocol —
    /// and to that check — when it does not hold
    StrGetItem {
        dest: RegisterId,
        container: Value,
        index: Value,
    },
    /// `s[i] <op> c`, where `s` is a `str` and `c` a `str` of exactly one code point
    ///
    /// a `str` compares by code point, so a right-hand side of one code point makes
    /// the whole comparison a question about the character's code point — and an
    /// exact `str` holds its code points directly. the character never has to become
    /// a `str` of its own, which is the entire cost of a scan that only ever asks
    /// what a character *is*
    ///
    /// the emitted form still reads an object where it has to: a subclass may have
    /// overridden `__getitem__` and may hand back any `str` at all, and an index out
    /// of range still has to raise. both take the same route [`Self::StrGetItem`]
    /// would, so neither the answer nor the `IndexError` moves
    StrItemCompare {
        dest: RegisterId,
        op: CmpOp,
        container: Value,
        index: Value,
        /// the one-character right-hand side, which is exactly one code point by
        /// being a `char` at all
        character: char,
    },
    /// `container[index] = value`, with a bit result so failure has a home
    SetItem {
        dest: RegisterId,
        container: Value,
        index: Value,
        value: Value,
    },
    /// one f-string interpolation: a conversion, then a format spec
    Format {
        dest: RegisterId,
        value: Value,
        /// `None` means no spec at all
        spec: Option<Value>,
        conversion: Conversion,
    },
    /// take the pending exception out of the thread state. a null result means
    /// nothing was set, which the handler block tests with `IsNull`
    FetchException { dest: RegisterId },
    /// whether a fetched exception matches a class, or any class in a tuple of them
    ///
    /// the class is an ordinary operand rather than a builtin name, so a user-defined
    /// exception, a tuple handler and a *shadowed* builtin all take the same path
    ExceptionMatches {
        dest: RegisterId,
        value: Value,
        class: Value,
    },
    /// enter an `except` block: mark the caught exception as the one being handled,
    /// and hand back whatever was being handled before
    PushHandled { dest: RegisterId, value: Value },
    /// leave one, putting back what [`Self::PushHandled`] handed over
    PopHandled { value: Value },
    /// `raise <exception>`, optionally `from <cause>`
    ///
    /// the general form. a class is instantiated, an instance raised as it is
    RaiseObject {
        exception: Value,
        cause: Option<Value>,
    },
    /// put a fetched exception back, for an unmatched handler or a bare re-raise
    Reraise { value: Value },
    /// `iter(o)`
    GetIter { dest: RegisterId, src: Value },
    /// one step of an iterator. a null result means exhausted *or* failed, which
    /// the emitted error check distinguishes with `PyErr_Occurred`
    IterNext { dest: RegisterId, iter: Value },
    /// whether an object register is null, as a bit. this is how an exhausted
    /// iterator is tested, so it must not itself be treated as an error
    IsNull { dest: RegisterId, src: Value },
    /// the length of a sized object, as a tagged int
    Len { dest: RegisterId, src: Value },
    /// concatenate two strings
    StrConcat {
        dest: RegisterId,
        lhs: Value,
        rhs: Value,
        /// whether the operation takes over the left operand's register, leaving it
        /// holding nothing.
        ///
        /// a `str` grows in place only for its *sole* owner, so a concatenation that
        /// leaves the register owning one too can never be anything but a copy —
        /// which is what makes a chain of them quadratic. handing the reference over
        /// is what puts the count at one.
        ///
        /// only `by_opt`'s `str_append` pass sets this, and only for a register
        /// nothing reads again — the error edge included, so a failure that empties
        /// the register cannot be observed either
        consumes_lhs: bool,
    },
    /// `str(n)` where `n` is a tagged integer
    ///
    /// this is a [`Self::CallPython`] of the name `str` and nothing more: the name is
    /// resolved through the module namespace on every trip, so a module that rebinds
    /// `str` — in its own source or through a write to `globals()` — is obeyed exactly
    /// as it was before. what is different is only what happens once the resolution
    /// has answered with the builtin.
    ///
    /// the general form has to give the integer an object representation to pass it,
    /// and `str` of that object then formats it through a writer — two allocations to
    /// produce a handful of ascii digits that were already in a register. the digits
    /// are written straight into one string instead.
    ///
    /// only `by_opt`'s `str_of_int` pass produces this
    StrOfInt { dest: RegisterId, value: Value },
    /// raise a standard error with a fixed message
    RaiseStandard {
        error: StandardError,
        message: String,
    },
}

impl Op {
    /// every class of this module's own that the operation reaches into
    ///
    /// a class whose storage is appended past a base's instance can only be reached
    /// through the type the emitter builds for it, at an offset no other type has —
    /// so before a module may leave such a class unbuilt and keep the rest of itself
    /// compiled, it has to know that nothing compiled would have read one. these are
    /// the operations that would: the six that name a class outright, the direct call
    /// to one of its methods, and the narrowing whose destination type is an instance.
    ///
    /// deliberately written without a catch-all arm. an operation added later with a
    /// class in it is then a compile error here rather than an operation that quietly
    /// answers "none" and lets a class be left out from under it
    pub fn named_classes(&self) -> Vec<&str> {
        match self {
            Self::GetCell { class, .. }
            | Self::NewInstance { class, .. }
            | Self::MakeClosure { class, .. }
            | Self::LoadClass { class, .. }
            | Self::GetField { class, .. }
            | Self::SetField { class, .. } => vec![class.as_str()],
            // a method reached directly rather than through the type, which only a
            // class the emitter laid out has
            Self::CallNative { owner, .. } => owner.as_deref().into_iter().collect(),
            Self::Unbox { to, .. } => to.instance_classes(),
            Self::Assign { .. }
            | Self::IntBinary { .. }
            | Self::FloatBinary { .. }
            | Self::IntCompare { .. }
            | Self::ObjectBinary { .. }
            | Self::ObjectCompare { .. }
            | Self::StrCompare { .. }
            | Self::Truthy { .. }
            | Self::FloatCompare { .. }
            | Self::Unary { .. }
            | Self::Box { .. }
            | Self::FloatObjectBinary { .. }
            | Self::IsInstance { .. }
            | Self::MatchAttr { .. }
            | Self::MethodStands { .. }
            | Self::DictShadows { .. }
            | Self::IsMissing { .. }
            | Self::MatchSlice { .. }
            | Self::FloatObjectCompare { .. }
            | Self::MatchKey { .. }
            | Self::MatchRest { .. }
            | Self::AsyncContext { .. }
            | Self::AsyncIter { .. }
            | Self::IsMapping { .. }
            | Self::IsSequence { .. }
            | Self::Contains { .. }
            | Self::Identity { .. }
            | Self::IntToFloat { .. }
            | Self::TupleBuild { .. }
            | Self::Unpack { .. }
            | Self::CallUnpacked { .. }
            | Self::Extend { .. }
            | Self::ArrayNew { .. }
            | Self::ArrayGet { .. }
            | Self::ArraySet { .. }
            | Self::ArrayLen { .. }
            | Self::ArrayRead { .. }
            | Self::ArrayPush { .. }
            | Self::DeleteItem { .. }
            | Self::DeleteAttr { .. }
            | Self::ToTuple { .. }
            | Self::TupleGet { .. }
            | Self::CallPython { .. }
            | Self::ImportModule { .. }
            | Self::ImportFrom { .. }
            | Self::Enter { .. }
            | Self::ExitContext { .. }
            | Self::DelegateIter { .. }
            | Self::DelegateStep { .. }
            | Self::RaiseWith { .. }
            | Self::FinishFrame { .. }
            | Self::LoadGlobal { .. }
            | Self::ModuleDict { .. }
            | Self::StoreGlobal { .. }
            | Self::DeleteGlobal { .. }
            | Self::DeleteLocal { .. }
            | Self::CallValue { .. }
            | Self::CallMethod { .. }
            | Self::GetAttr { .. }
            | Self::SetAttr { .. }
            | Self::BuildList { .. }
            | Self::BuildSet { .. }
            | Self::BuildTuple { .. }
            | Self::BuildDict { .. }
            | Self::GetItem { .. }
            | Self::DictFind { .. }
            | Self::StrGetItem { .. }
            | Self::StrItemCompare { .. }
            | Self::SetItem { .. }
            | Self::Format { .. }
            | Self::FetchException { .. }
            | Self::ExceptionMatches { .. }
            | Self::PushHandled { .. }
            | Self::PopHandled { .. }
            | Self::RaiseObject { .. }
            | Self::Reraise { .. }
            | Self::GetIter { .. }
            | Self::IterNext { .. }
            | Self::IsNull { .. }
            | Self::Len { .. }
            | Self::StrConcat { .. }
            | Self::StrOfInt { .. }
            | Self::RaiseStandard { .. } => Vec::new(),
        }
    }

    /// the register this operation *unbinds*, if any
    ///
    /// [`Self::dest`] answers "which register does this leave a new value in", and
    /// `del x` is the one operation for which that is the wrong question: it leaves
    /// its destination holding nothing, and it *reads* it on the way — both to answer
    /// whether the name was bound and to release what it held. so every analysis that
    /// asks what an operation reads has to ask this too, or it concludes the value was
    /// dead at the `del` and hands the reference away underneath it
    pub fn unbinds(&self) -> Option<RegisterId> {
        match self {
            Self::DeleteLocal { dest } => Some(*dest),
            _ => None,
        }
    }

    /// the register this operation writes, if any
    pub fn dest(&self) -> Option<RegisterId> {
        match self {
            Self::Assign { dest, .. }
            | Self::Contains { dest, .. }
            | Self::AsyncContext { dest, .. }
            | Self::AsyncIter { dest, .. }
            | Self::FloatObjectCompare { dest, .. }
            | Self::MatchKey { dest, .. }
            | Self::MatchRest { dest, .. }
            | Self::IsMapping { dest, .. }
            | Self::MatchAttr { dest, .. }
            | Self::MethodStands { dest, .. }
            | Self::DictShadows { dest, .. }
            | Self::IsMissing { dest, .. }
            | Self::MatchSlice { dest, .. }
            | Self::IsInstance { dest, .. }
            | Self::IsSequence { dest, .. }
            | Self::Identity { dest, .. }
            | Self::FloatObjectBinary { dest, .. }
            | Self::IntToFloat { dest, .. }
            | Self::IntBinary { dest, .. }
            | Self::FloatBinary { dest, .. }
            | Self::IntCompare { dest, .. }
            | Self::ObjectBinary { dest, .. }
            | Self::ObjectCompare { dest, .. }
            | Self::StrCompare { dest, .. }
            | Self::Truthy { dest, .. }
            | Self::FloatCompare { dest, .. }
            | Self::Unary { dest, .. }
            | Self::Box { dest, .. }
            | Self::Unbox { dest, .. }
            | Self::TupleBuild { dest, .. }
            | Self::Len { dest, .. }
            | Self::StrOfInt { dest, .. }
            | Self::CallPython { dest, .. }
            | Self::ImportModule { dest, .. }
            | Self::ImportFrom { dest, .. }
            | Self::CallValue { dest, .. }
            | Self::LoadGlobal { dest, .. }
            | Self::ModuleDict { dest }
            | Self::StoreGlobal { dest, .. }
            | Self::DeleteGlobal { dest, .. }
            | Self::DeleteLocal { dest }
            | Self::LoadClass { dest, .. }
            | Self::NewInstance { dest, .. }
            | Self::GetCell { dest, .. }
            | Self::Enter { dest, .. }
            | Self::ExitContext { dest, .. }
            | Self::DelegateIter { dest, .. }
            | Self::DelegateStep { dest, .. }
            | Self::MakeClosure { dest, .. }
            | Self::CallMethod { dest, .. }
            | Self::GetField { dest, .. }
            | Self::GetAttr { dest, .. }
            | Self::SetAttr { dest, .. }
            | Self::BuildList { dest, .. }
            | Self::BuildSet { dest, .. }
            | Self::BuildTuple { dest, .. }
            | Self::BuildDict { dest, .. }
            | Self::GetItem { dest, .. }
            | Self::DictFind { dest, .. }
            | Self::StrGetItem { dest, .. }
            | Self::StrItemCompare { dest, .. }
            | Self::SetItem { dest, .. }
            | Self::Format { dest, .. }
            | Self::FetchException { dest, .. }
            | Self::ExceptionMatches { dest, .. }
            | Self::GetIter { dest, .. }
            | Self::IterNext { dest, .. }
            | Self::IsNull { dest, .. }
            | Self::StrConcat { dest, .. }
            | Self::TupleGet { dest, .. }
            | Self::Unpack { dest, .. }
            | Self::ToTuple { dest, .. }
            | Self::ArrayNew { dest, .. }
            | Self::ArrayGet { dest, .. }
            | Self::ArraySet { dest, .. }
            | Self::ArrayLen { dest, .. }
            | Self::ArrayRead { dest, .. }
            | Self::DeleteItem { dest, .. }
            | Self::DeleteAttr { dest, .. }
            | Self::ArrayPush { dest, .. }
            | Self::Extend { dest, .. }
            | Self::CallUnpacked { dest, .. }
            | Self::PushHandled { dest, .. } => Some(*dest),
            Self::CallNative { dest, .. } => *dest,
            Self::RaiseStandard { .. }
            | Self::RaiseWith { .. }
            | Self::FinishFrame { .. }
            | Self::RaiseObject { .. }
            | Self::PopHandled { .. }
            | Self::Reraise { .. }
            | Self::SetField { .. } => None,
        }
    }

    /// the register this operation writes, mutably
    ///
    /// the mirror of [`Self::dest`], arm for arm. a pass that redirects a write
    /// asks this one and asks [`Self::dest`] to decide whether it may — the two
    /// covering different operations would let a redirect silently not happen
    pub fn dest_mut(&mut self) -> Option<&mut RegisterId> {
        match self {
            Self::Assign { dest, .. }
            | Self::Contains { dest, .. }
            | Self::AsyncContext { dest, .. }
            | Self::AsyncIter { dest, .. }
            | Self::FloatObjectCompare { dest, .. }
            | Self::MatchKey { dest, .. }
            | Self::MatchRest { dest, .. }
            | Self::IsMapping { dest, .. }
            | Self::MatchAttr { dest, .. }
            | Self::MethodStands { dest, .. }
            | Self::DictShadows { dest, .. }
            | Self::IsMissing { dest, .. }
            | Self::MatchSlice { dest, .. }
            | Self::IsInstance { dest, .. }
            | Self::IsSequence { dest, .. }
            | Self::Identity { dest, .. }
            | Self::FloatObjectBinary { dest, .. }
            | Self::IntToFloat { dest, .. }
            | Self::IntBinary { dest, .. }
            | Self::FloatBinary { dest, .. }
            | Self::IntCompare { dest, .. }
            | Self::ObjectBinary { dest, .. }
            | Self::ObjectCompare { dest, .. }
            | Self::StrCompare { dest, .. }
            | Self::Truthy { dest, .. }
            | Self::FloatCompare { dest, .. }
            | Self::Unary { dest, .. }
            | Self::Box { dest, .. }
            | Self::Unbox { dest, .. }
            | Self::TupleBuild { dest, .. }
            | Self::Len { dest, .. }
            | Self::StrOfInt { dest, .. }
            | Self::CallPython { dest, .. }
            | Self::ImportModule { dest, .. }
            | Self::ImportFrom { dest, .. }
            | Self::CallValue { dest, .. }
            | Self::LoadGlobal { dest, .. }
            | Self::ModuleDict { dest }
            | Self::StoreGlobal { dest, .. }
            | Self::DeleteGlobal { dest, .. }
            | Self::DeleteLocal { dest }
            | Self::LoadClass { dest, .. }
            | Self::NewInstance { dest, .. }
            | Self::GetCell { dest, .. }
            | Self::Enter { dest, .. }
            | Self::ExitContext { dest, .. }
            | Self::DelegateIter { dest, .. }
            | Self::DelegateStep { dest, .. }
            | Self::MakeClosure { dest, .. }
            | Self::CallMethod { dest, .. }
            | Self::GetField { dest, .. }
            | Self::GetAttr { dest, .. }
            | Self::SetAttr { dest, .. }
            | Self::BuildList { dest, .. }
            | Self::BuildSet { dest, .. }
            | Self::BuildTuple { dest, .. }
            | Self::BuildDict { dest, .. }
            | Self::GetItem { dest, .. }
            | Self::DictFind { dest, .. }
            | Self::StrGetItem { dest, .. }
            | Self::StrItemCompare { dest, .. }
            | Self::SetItem { dest, .. }
            | Self::Format { dest, .. }
            | Self::FetchException { dest, .. }
            | Self::ExceptionMatches { dest, .. }
            | Self::GetIter { dest, .. }
            | Self::IterNext { dest, .. }
            | Self::IsNull { dest, .. }
            | Self::StrConcat { dest, .. }
            | Self::TupleGet { dest, .. }
            | Self::Unpack { dest, .. }
            | Self::ToTuple { dest, .. }
            | Self::ArrayNew { dest, .. }
            | Self::ArrayGet { dest, .. }
            | Self::ArraySet { dest, .. }
            | Self::ArrayLen { dest, .. }
            | Self::ArrayRead { dest, .. }
            | Self::DeleteItem { dest, .. }
            | Self::DeleteAttr { dest, .. }
            | Self::ArrayPush { dest, .. }
            | Self::Extend { dest, .. }
            | Self::CallUnpacked { dest, .. }
            | Self::PushHandled { dest, .. } => Some(dest),
            Self::CallNative { dest, .. } => dest.as_mut(),
            Self::RaiseStandard { .. }
            | Self::RaiseWith { .. }
            | Self::FinishFrame { .. }
            | Self::RaiseObject { .. }
            | Self::PopHandled { .. }
            | Self::Reraise { .. }
            | Self::SetField { .. } => None,
        }
    }

    /// every operand this operation reads
    pub fn operands(&self) -> Vec<&Value> {
        match self {
            Self::AsyncContext {
                manager,
                exception: Some(exception),
                ..
            } => vec![manager, exception],
            Self::Assign { src, .. }
            | Self::Box { src, .. }
            | Self::MethodStands { src, .. }
            | Self::DictShadows { src, .. }
            | Self::IsMissing { src, .. }
            | Self::IsMapping { src, .. }
            | Self::AsyncIter { src, .. }
            | Self::AsyncContext {
                manager: src,
                exception: None,
                ..
            }
            | Self::MatchAttr {
                subject: src,
                class: None,
                ..
            }
            | Self::MatchSlice { sequence: src, .. }
            | Self::IsSequence { src, .. }
            | Self::IntToFloat { src, .. }
            | Self::Unbox { src, .. }
            | Self::Truthy { src, .. }
            | Self::Len { src, .. }
            | Self::StrOfInt { value: src, .. }
            | Self::GetIter { src, .. }
            | Self::IsNull { src, .. }
            | Self::TupleGet { src, .. }
            | Self::Unpack { src, .. }
            | Self::ToTuple { src, .. } => vec![src],
            Self::MatchKey {
                map: lhs, key: rhs, ..
            }
            | Self::MatchRest {
                map: lhs,
                keys: rhs,
                ..
            }
            | Self::MatchAttr {
                subject: lhs,
                class: Some(rhs),
                ..
            }
            | Self::IsInstance {
                src: lhs,
                class: rhs,
                ..
            }
            | Self::Identity { lhs, rhs, .. }
            | Self::Contains {
                value: lhs,
                container: rhs,
                ..
            }
            | Self::FloatObjectBinary { lhs, rhs, .. }
            | Self::FloatObjectCompare { lhs, rhs, .. }
            | Self::IntBinary { lhs, rhs, .. }
            | Self::FloatBinary { lhs, rhs, .. }
            | Self::IntCompare { lhs, rhs, .. }
            | Self::ObjectBinary { lhs, rhs, .. }
            | Self::ObjectCompare { lhs, rhs, .. }
            | Self::StrCompare { lhs, rhs, .. }
            | Self::StrConcat { lhs, rhs, .. }
            | Self::FloatCompare { lhs, rhs, .. } => vec![lhs, rhs],
            Self::Unary { operand, .. } => vec![operand],
            Self::CallNative { args, .. } | Self::CallPython { args, .. } => args.iter().collect(),
            Self::CallValue { callee, args, .. } => {
                let mut all = vec![callee];
                all.extend(args.iter());
                all
            }
            Self::NewInstance { fields, .. } => fields.iter().flatten().collect(),
            Self::MakeClosure { env, .. } => vec![env],
            Self::GetCell { receiver, .. } => vec![receiver],
            Self::IterNext { iter, .. } => vec![iter],
            Self::CallMethod { receiver, args, .. } => {
                let mut all = vec![receiver];
                all.extend(args.iter());
                all
            }
            Self::GetAttr { receiver, .. } | Self::GetField { receiver, .. } => vec![receiver],
            Self::ImportFrom { module, .. } => vec![module],
            Self::SetField {
                receiver, value, ..
            } => vec![receiver, value],
            Self::SetAttr {
                receiver, value, ..
            } => vec![receiver, value],
            Self::StoreGlobal { value, .. } => vec![value],
            Self::TupleBuild { items, .. }
            | Self::BuildList { items, .. }
            | Self::BuildSet { items, .. }
            | Self::BuildTuple { items, .. } => items.iter().collect(),
            Self::BuildDict { pairs, .. } => pairs.iter().collect(),
            Self::DictFind {
                container,
                key: index,
                ..
            }
            | Self::GetItem {
                container, index, ..
            }
            | Self::StrGetItem {
                container, index, ..
            }
            | Self::StrItemCompare {
                container, index, ..
            } => vec![container, index],
            Self::SetItem {
                container,
                index,
                value,
                ..
            } => vec![container, index, value],
            Self::Format { value, spec, .. } => match spec {
                Some(spec) => vec![value, spec],
                None => vec![value],
            },
            Self::RaiseStandard { .. }
            | Self::FetchException { .. }
            | Self::LoadGlobal { .. }
            | Self::ModuleDict { .. }
            | Self::DeleteGlobal { .. }
            | Self::DeleteLocal { .. }
            | Self::LoadClass { .. }
            | Self::ImportModule { .. } => Vec::new(),
            Self::Enter { manager, .. } => vec![manager],
            Self::ExitContext {
                manager, exception, ..
            } => vec![manager, exception],
            Self::DelegateIter { src, .. } => vec![src],
            Self::DelegateStep { inner, sent, .. } => vec![inner, sent],
            Self::ArrayNew { items, .. } => items.iter().collect(),
            Self::ArrayGet { array, index, .. }
            | Self::ArrayRead { array, index, .. }
            | Self::ArraySet { array, index, .. } => vec![array, index],
            Self::ArrayLen { array, .. } => vec![array],
            Self::DeleteItem {
                container, index, ..
            } => vec![container, index],
            Self::DeleteAttr { receiver, .. } => vec![receiver],
            Self::ArrayPush { array, value, .. } => vec![array, value],
            Self::Extend {
                container, source, ..
            } => vec![container, source],
            Self::CallUnpacked {
                callee,
                args,
                kwargs,
                ..
            } => match kwargs {
                Some(kwargs) => vec![callee, args, kwargs],
                None => vec![callee, args],
            },
            Self::ExceptionMatches { value, class, .. } => vec![value, class],
            Self::RaiseObject { exception, cause } => match cause {
                Some(cause) => vec![exception, cause],
                None => vec![exception],
            },
            Self::Reraise { value }
            | Self::PushHandled { value, .. }
            | Self::PopHandled { value }
            | Self::FinishFrame { value }
            | Self::RaiseWith { value, .. } => vec![value],
        }
    }

    /// every operand this operation reads, mutably
    ///
    /// the mirror of [`Self::operands`]. a pass that substitutes one register for
    /// another needs it, and doing it by hand per op kind is how a new op gets
    /// silently skipped
    pub fn operands_mut(&mut self) -> Vec<&mut Value> {
        match self {
            Self::AsyncContext {
                manager,
                exception: Some(exception),
                ..
            } => vec![manager, exception],
            Self::Assign { src, .. }
            | Self::Box { src, .. }
            | Self::MethodStands { src, .. }
            | Self::DictShadows { src, .. }
            | Self::IsMissing { src, .. }
            | Self::IsMapping { src, .. }
            | Self::AsyncIter { src, .. }
            | Self::AsyncContext {
                manager: src,
                exception: None,
                ..
            }
            | Self::MatchAttr {
                subject: src,
                class: None,
                ..
            }
            | Self::MatchSlice { sequence: src, .. }
            | Self::IsSequence { src, .. }
            | Self::IntToFloat { src, .. }
            | Self::Unbox { src, .. }
            | Self::Truthy { src, .. }
            | Self::Len { src, .. }
            | Self::StrOfInt { value: src, .. }
            | Self::GetIter { src, .. }
            | Self::IsNull { src, .. }
            | Self::TupleGet { src, .. }
            | Self::Unpack { src, .. }
            | Self::ToTuple { src, .. } => vec![src],
            Self::MatchKey {
                map: lhs, key: rhs, ..
            }
            | Self::MatchRest {
                map: lhs,
                keys: rhs,
                ..
            }
            | Self::MatchAttr {
                subject: lhs,
                class: Some(rhs),
                ..
            }
            | Self::IsInstance {
                src: lhs,
                class: rhs,
                ..
            }
            | Self::Identity { lhs, rhs, .. }
            | Self::Contains {
                value: lhs,
                container: rhs,
                ..
            }
            | Self::FloatObjectBinary { lhs, rhs, .. }
            | Self::FloatObjectCompare { lhs, rhs, .. }
            | Self::IntBinary { lhs, rhs, .. }
            | Self::FloatBinary { lhs, rhs, .. }
            | Self::IntCompare { lhs, rhs, .. }
            | Self::ObjectBinary { lhs, rhs, .. }
            | Self::ObjectCompare { lhs, rhs, .. }
            | Self::StrCompare { lhs, rhs, .. }
            | Self::StrConcat { lhs, rhs, .. }
            | Self::FloatCompare { lhs, rhs, .. } => vec![lhs, rhs],
            Self::Unary { operand, .. } => vec![operand],
            Self::CallNative { args, .. } | Self::CallPython { args, .. } => {
                args.iter_mut().collect()
            }
            Self::CallValue { callee, args, .. } => {
                let mut all = vec![callee];
                all.extend(args.iter_mut());
                all
            }
            Self::NewInstance { fields, .. } => fields.iter_mut().flatten().collect(),
            Self::MakeClosure { env, .. } => vec![env],
            Self::GetCell { receiver, .. } => vec![receiver],
            Self::IterNext { iter, .. } => vec![iter],
            Self::CallMethod { receiver, args, .. } => {
                let mut all = vec![receiver];
                all.extend(args.iter_mut());
                all
            }
            Self::GetAttr { receiver, .. } | Self::GetField { receiver, .. } => vec![receiver],
            Self::ImportFrom { module, .. } => vec![module],
            Self::SetField {
                receiver, value, ..
            } => vec![receiver, value],
            Self::SetAttr {
                receiver, value, ..
            } => vec![receiver, value],
            Self::StoreGlobal { value, .. } => vec![value],
            Self::TupleBuild { items, .. }
            | Self::BuildList { items, .. }
            | Self::BuildSet { items, .. }
            | Self::BuildTuple { items, .. } => items.iter_mut().collect(),
            Self::BuildDict { pairs, .. } => pairs.iter_mut().collect(),
            Self::DictFind {
                container,
                key: index,
                ..
            }
            | Self::GetItem {
                container, index, ..
            }
            | Self::StrGetItem {
                container, index, ..
            }
            | Self::StrItemCompare {
                container, index, ..
            } => vec![container, index],
            Self::SetItem {
                container,
                index,
                value,
                ..
            } => vec![container, index, value],
            Self::Format { value, spec, .. } => match spec {
                Some(spec) => vec![value, spec],
                None => vec![value],
            },
            Self::RaiseStandard { .. }
            | Self::FetchException { .. }
            | Self::LoadGlobal { .. }
            | Self::ModuleDict { .. }
            | Self::DeleteGlobal { .. }
            | Self::DeleteLocal { .. }
            | Self::LoadClass { .. }
            | Self::ImportModule { .. } => Vec::new(),
            Self::Enter { manager, .. } => vec![manager],
            Self::ExitContext {
                manager, exception, ..
            } => vec![manager, exception],
            Self::DelegateIter { src, .. } => vec![src],
            Self::DelegateStep { inner, sent, .. } => vec![inner, sent],
            Self::ArrayNew { items, .. } => items.iter_mut().collect(),
            Self::ArrayGet { array, index, .. }
            | Self::ArrayRead { array, index, .. }
            | Self::ArraySet { array, index, .. } => vec![array, index],
            Self::DeleteItem {
                container, index, ..
            } => vec![container, index],
            Self::DeleteAttr { receiver, .. } => vec![receiver],
            Self::ArrayLen { array, .. } => vec![array],
            Self::ArrayPush { array, value, .. } => vec![array, value],
            Self::Extend {
                container, source, ..
            } => vec![container, source],
            Self::CallUnpacked {
                callee,
                args,
                kwargs,
                ..
            } => match kwargs {
                Some(kwargs) => vec![callee, args, kwargs],
                None => vec![callee, args],
            },
            Self::ExceptionMatches { value, class, .. } => vec![value, class],
            Self::RaiseObject { exception, cause } => match cause {
                Some(cause) => vec![exception, cause],
                None => vec![exception],
            },
            Self::Reraise { value }
            | Self::PushHandled { value, .. }
            | Self::PopHandled { value }
            | Self::FinishFrame { value }
            | Self::RaiseWith { value, .. } => vec![value],
        }
    }
}

/// how a block ends. a block has exactly one, so control flow is total
#[derive(Debug, Clone, PartialEq)]
pub enum Terminator {
    Goto(BlockId),
    Branch {
        cond: Value,
        then_block: BlockId,
        else_block: BlockId,
    },
    Return(Value),
    /// narrow a tagged `int` to a machine integer, taking `otherwise` when it does
    /// not fit
    ///
    /// the test and the narrowing are one terminator because they cannot be told
    /// apart: `dest` holds a value only on the `fits` edge, and there is no failure
    /// to report on the other — a bound too large for a machine integer is an
    /// ordinary `int` that the general path handles. writing it as an operation
    /// would mean an unchecked narrowing whose safety lived in a dominating test the
    /// verifier cannot see
    NarrowShort {
        dest: RegisterId,
        src: Value,
        fits: BlockId,
        otherwise: BlockId,
    },
    /// control cannot reach here. emitted after a raise, and after a body the
    /// checker proved diverges
    Unreachable,
}

impl Terminator {
    /// the blocks control can transfer to
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Self::Goto(target) => vec![*target],
            Self::Branch {
                then_block,
                else_block,
                ..
            } => vec![*then_block, *else_block],
            Self::NarrowShort {
                fits, otherwise, ..
            } => vec![*fits, *otherwise],
            Self::Return(_) | Self::Unreachable => Vec::new(),
        }
    }

    /// the register this terminator writes, on the edge that writes one
    pub fn dest(&self) -> Option<RegisterId> {
        match self {
            Self::NarrowShort { dest, .. } => Some(*dest),
            Self::Goto(_) | Self::Branch { .. } | Self::Return(_) | Self::Unreachable => None,
        }
    }

    /// every operand this terminator reads
    pub fn operands(&self) -> Vec<&Value> {
        match self {
            Self::Branch { cond, .. } => vec![cond],
            Self::Return(value) | Self::NarrowShort { src: value, .. } => vec![value],
            Self::Goto(_) | Self::Unreachable => Vec::new(),
        }
    }

    /// every operand this terminator reads, mutably
    pub fn operands_mut(&mut self) -> Vec<&mut Value> {
        match self {
            Self::Branch { cond, .. } => vec![cond],
            Self::Return(value) | Self::NarrowShort { src: value, .. } => vec![value],
            Self::Goto(_) | Self::Unreachable => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_and_operands_agree_with_the_op_shape() {
        let op = Op::IntBinary {
            dest: RegisterId(2),
            op: BinOp::Add,
            lhs: Value::Register(RegisterId(0)),
            rhs: Value::Int(1),
        };
        assert_eq!(op.dest(), Some(RegisterId(2)));
        assert_eq!(op.operands().len(), 2);
    }

    #[test]
    fn a_raise_writes_nothing_and_reads_nothing() {
        let op = Op::RaiseStandard {
            error: StandardError::ValueError,
            message: "bad".to_string(),
        };
        assert_eq!(op.dest(), None);
        assert!(op.operands().is_empty());
    }

    /// every shape that reaches into a class of this module's own, asked one at a time
    ///
    /// the six that carry a class name, the direct call to one of its methods, and the
    /// narrowing whose destination is an instance. a shape missing from here is a class
    /// that could be left unbuilt from under a live reader — see `declines_on_its_own`
    /// in `by_codegen_c`
    #[test]
    fn every_shape_that_reaches_into_a_class_names_it() {
        let held = Value::Register(RegisterId(0));
        let reaching = [
            Op::GetCell {
                dest: RegisterId(1),
                receiver: held.clone(),
                class: "Held".to_string(),
                field: "cell".to_string(),
                free: false,
            },
            Op::NewInstance {
                dest: RegisterId(1),
                class: "Held".to_string(),
                fields: Vec::new(),
            },
            Op::MakeClosure {
                dest: RegisterId(1),
                class: "Held".to_string(),
                method: "step".to_string(),
                env: held.clone(),
            },
            Op::LoadClass {
                dest: RegisterId(1),
                class: "Held".to_string(),
            },
            Op::GetField {
                dest: RegisterId(1),
                receiver: held.clone(),
                class: "Held".to_string(),
                field: "tag".to_string(),
            },
            Op::SetField {
                receiver: held.clone(),
                class: "Held".to_string(),
                field: "tag".to_string(),
                value: Value::Int(1),
            },
            Op::CallNative {
                owner: Some("Held".to_string()),
                dest: None,
                callee: "step".to_string(),
                args: Vec::new(),
            },
            Op::Unbox {
                dest: RegisterId(1),
                src: held,
                to: RType::Instance {
                    class: "Held".to_string(),
                    exact: true,
                },
            },
        ];
        for op in reaching {
            assert_eq!(op.named_classes(), vec!["Held"], "{op:?}");
        }
    }

    /// and a shape that only ever reaches a class through a name the namespace resolves
    /// names none of them. that is what lets a module go on calling a class it left
    /// unbuilt: the lookup answers with the interpreted definition, which is what the
    /// name means from then on
    #[test]
    fn a_name_resolved_through_the_namespace_reaches_into_no_class() {
        let op = Op::LoadGlobal {
            dest: RegisterId(1),
            name: "Held".to_string(),
        };
        assert!(op.named_classes().is_empty());
        let op = Op::CallMethod {
            dest: RegisterId(1),
            receiver: Value::Register(RegisterId(0)),
            name: "step".to_string(),
            args: Vec::new(),
        };
        assert!(op.named_classes().is_empty());
    }

    #[test]
    fn a_void_call_has_no_destination() {
        let op = Op::CallNative {
            owner: None,
            dest: None,
            callee: "f".to_string(),
            args: vec![Value::Int(1)],
        };
        assert_eq!(op.dest(), None);
        assert_eq!(op.operands().len(), 1);
    }

    #[test]
    fn only_the_branching_terminators_have_successors() {
        assert_eq!(Terminator::Goto(BlockId(1)).successors(), vec![BlockId(1)]);
        assert_eq!(
            Terminator::Branch {
                cond: Value::Bool(true),
                then_block: BlockId(1),
                else_block: BlockId(2),
            }
            .successors(),
            vec![BlockId(1), BlockId(2)]
        );
        assert!(Terminator::Return(Value::None).successors().is_empty());
        assert!(Terminator::Unreachable.successors().is_empty());
    }

    #[test]
    fn division_can_fail_and_addition_cannot() {
        assert!(BinOp::FloorDiv.can_fail());
        assert!(BinOp::Mod.can_fail());
        assert!(!BinOp::Add.can_fail());
        assert!(!BinOp::Mul.can_fail());
    }
}
