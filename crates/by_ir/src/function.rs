//! functions and modules
//!
//! a [`Function`] owns a flat register table and a list of blocks. registers are
//! declared once with a type, which is what makes the representation invariant
//! checkable: every write to a register must produce that register's type.

use std::path::PathBuf;

use crate::ops::{BlockId, Op, RegisterId, Terminator, Value};
use crate::rtype::RType;

/// a declared register
#[derive(Debug, Clone, PartialEq)]
pub struct RegisterDecl {
    /// the source name, where the register came from one. generated temporaries
    /// carry `None` and print as `r<N>`
    pub name: Option<String>,
    pub ty: RType,
    /// whether this register only *borrows* its value, so the frame neither
    /// retains on the write nor releases at an exit.
    ///
    /// set by the borrow pass, which proves the value outlives every use. a
    /// borrowed register is not "held" for the purposes of the release sets
    pub borrowed: bool,
    /// whether some path reaches a read of this register without having written it.
    ///
    /// python answers that with `UnboundLocalError` rather than a value, so the
    /// register carries a byte saying whether it has been written and every read of it
    /// tests that byte first. set by the unbound-locals pass
    pub may_be_unassigned: bool,
}

impl RegisterDecl {
    /// the C variable holding whether this register has been written
    ///
    /// only a [maybe-unassigned](Self::may_be_unassigned) register has one
    pub fn presence(id: crate::ops::RegisterId) -> String {
        format!("by_u{}", id.0)
    }
}

/// a basic block: straight-line ops, then exactly one terminator
#[derive(Debug, Clone, PartialEq)]
pub struct BasicBlock {
    pub ops: Vec<Op>,
    pub terminator: Terminator,
    /// which registers an exit in this block must release, from the refcount
    /// pass.
    ///
    /// `None` means the analysis has not run, and the emitter falls back to
    /// releasing every refcounted register — the conservative answer. so a bug in
    /// the analysis degrades to extra work rather than to a leak
    pub owned_at_exit: Option<Vec<RegisterId>>,
    /// the `.by` byte offsets the block's code came from, when the frontend set
    /// them.
    ///
    /// a block is the finest granularity that survives the passes untouched — none
    /// of them merge, split or reorder blocks — which is why the span lives here
    /// and not on each op. codegen turns it into a `#line`
    ///
    /// `unswitch` *appends* blocks, which is a different thing: existing ids keep
    /// their meaning, and a copied block reports the source it was copied from,
    /// which is the source it still is
    pub range: Option<(u32, u32)>,
    /// where a *failing* operation in this block jumps.
    ///
    /// `None` is the function's own error exit. inside a `try` it is the handler,
    /// which is what makes exception edges real CFG edges rather than an implicit
    /// jump to one place
    pub error_target: Option<BlockId>,
}

impl BasicBlock {
    /// every block control can reach from here, the exception edge included
    ///
    /// an exception edge *is* a CFG edge, and a dataflow analysis that only follows
    /// terminators simply never visits a handler — which is a wrong answer, not a
    /// missing one
    pub fn successors(&self) -> Vec<BlockId> {
        let mut out = self.terminator.successors();
        out.extend(self.error_target);
        out
    }

    pub fn new(terminator: Terminator) -> Self {
        Self {
            ops: Vec::new(),
            terminator,
            owned_at_exit: None,
            error_target: None,
            range: None,
        }
    }
}

/// how a function may be called
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallConvention {
    /// unboxed arguments, positional only, errors via a sentinel return
    Native,
    /// unboxed arguments, and cannot fail — no error path is emitted at all.
    /// selected when the checker proved the function's exception set is empty
    NativeInfallible,
}

impl CallConvention {
    pub fn can_fail(self) -> bool {
        matches!(self, Self::Native)
    }
}

/// a decorator expression, as much of one as module init can evaluate
///
/// python evaluates the expression where the `def` or the `class` stands, in the
/// enclosing frame. module-level code is not compiled — it runs from the interpreted
/// twin — so there is no native frame standing there, and the only moment left is
/// module init, once that twin has run the whole body and the native definition is in
/// the namespace.
///
/// that is faithful for an expression which is nothing but *reads*, and only for one.
/// a call would be made a second time, at the end of the module rather than where it
/// was written, and whatever it did on the way would happen twice — so `@lru_cache(512)`
/// has no variant here and declines instead
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decorator {
    /// a name, or a chain of attributes read off one: `functools.cache`
    ///
    /// the root resolves the way `LOAD_GLOBAL` resolves it, so a decorator defined in
    /// the module, imported into it, or a builtin all work; each attribute is then a
    /// plain `getattr`
    Path {
        root: String,
        attributes: Vec<String>,
    },
}

impl Decorator {
    /// a decorator written as a bare name — which is what every language *modifier*
    /// translates to
    pub fn name(name: impl Into<String>) -> Self {
        Self::Path {
            root: name.into(),
            attributes: Vec::new(),
        }
    }

    /// the bare name this is, where it is one
    pub fn as_name(&self) -> Option<&str> {
        match self {
            Self::Path { root, attributes } if attributes.is_empty() => Some(root),
            Self::Path { .. } => None,
        }
    }

    /// the name the resolution starts from, whatever shape the rest is
    pub fn root(&self) -> &str {
        match self {
            Self::Path { root, .. } => root,
        }
    }

    /// the expression as written, which is also how codegen spells it: a python
    /// identifier holds no `.`, so the join is unambiguous to split again
    pub fn dotted(&self) -> String {
        match self {
            Self::Path { root, attributes } if attributes.is_empty() => root.clone(),
            Self::Path { root, attributes } => format!("{root}.{}", attributes.join(".")),
        }
    }
}

/// what python leaves in slot zero when a method is called through the class
///
/// the three conventions `type_add_methods` distinguishes, and the reason they cannot
/// be inferred from the parameter list: `def make(x)` under `@staticmethod` takes an
/// `x`, and the same source without it takes a receiver written `x`. a module-level
/// function is [`Instance`](Self::Instance) and nothing reads it there — the field
/// only means anything about a method
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Binding {
    /// slot zero holds the receiver: the ordinary method
    #[default]
    Instance,
    /// there is no receiver, and slot zero is the first written parameter
    Static,
    /// slot zero holds the *class* rather than an instance of it
    Class,
}

impl Binding {
    /// whether the boundary is handed slot zero outside the argument vector
    ///
    /// true for a class method as much as an instance one: `METH_CLASS` puts the class
    /// in `self`, which is the same slot in the same place
    pub fn takes_slot_zero_from_self(self) -> bool {
        matches!(self, Self::Instance | Self::Class)
    }

    /// the `METH_` flag this convention adds to a method table entry
    pub fn method_flag(self) -> Option<&'static str> {
        match self {
            Self::Instance => None,
            Self::Static => Some("METH_STATIC"),
            Self::Class => Some("METH_CLASS"),
        }
    }
}

/// a compiled function
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// the name as written in source, used for the python-visible surface
    pub name: String,
    /// the number of leading registers that are parameters
    pub param_count: usize,
    pub ret: RType,
    pub convention: CallConvention,
    /// declarations for every register, parameters first
    pub registers: Vec<RegisterDecl>,
    pub blocks: Vec<BasicBlock>,
    /// whether the module's python namespace exposes this function
    pub exported: bool,
    /// the class this function is a method of, when it is one.
    ///
    /// it only affects the emitted symbol names: a method and a module-level
    /// function may share a python-visible name without colliding in C
    pub owner: Option<String>,
    /// whether the parameter after the named ones is `*args`, and whether the one
    /// after *that* is `**kwargs`.
    ///
    /// both hold a real python object the wrapper builds, so the body sees an ordinary
    /// `tuple` and `dict` — the packing is the boundary's job, not the body's
    pub vararg: bool,
    pub kwarg: bool,
    /// how many of the named parameters are *positional-only*, and how many of the
    /// trailing ones are *keyword-only*
    ///
    /// the two together say which run a caller may fill positionally, which is what
    /// the arity errors are phrased in terms of.
    ///
    /// both are counted from [`Self::params`]'s first entry, which includes a receiver
    /// — a boundary handed that receiver separately shifts them into the index space of
    /// the names it binds
    pub posonly: usize,
    pub kwonly: usize,
    /// the default for each parameter, `None` where it has none.
    ///
    /// only an immediate: a literal default is evaluated once in python and cannot
    /// change, so inlining it in the wrapper is exactly the same thing. a *computed*
    /// default would need a module-level slot evaluated at definition time
    pub defaults: Vec<Option<Value>>,
    /// the `.by` byte offsets of the definition, for `#line`
    pub range: Option<(u32, u32)>,
    /// the indices of parameters the boundary can only *sometimes* establish.
    ///
    /// python's `float` annotation admits an `int`, so a `double` parameter is not
    /// a promise the caller has to keep. rejecting the call would be wrong — it is
    /// legal python — and converting the argument would be a *different* program,
    /// so the wrapper hands the whole call to the interpreted definition, which is
    /// exactly the code the annotation describes.
    ///
    /// `.by` opts out of the promotion, so this is empty for every `.by` function
    pub deferring: Vec<usize>,
    /// the indices of parameters whose default is not an immediate.
    ///
    /// python evaluates a default once, at definition time — which is what makes a
    /// mutable one shared by every call. the interpreted definition already did
    /// that and holds the object, so a call that omits one of these parameters is
    /// handed to it rather than given a second object that no other call sees.
    ///
    /// the parameter is still *optional*: it is the value that is unavailable here,
    /// not the arity
    pub computed_defaults: Vec<usize>,
    /// decorators to apply, outermost first, after the native function is
    /// installed in the module namespace
    pub decorators: Vec<Decorator>,
    /// what python puts in slot zero when this method is reached through the class.
    ///
    /// `staticmethod` and `classmethod` are honoured by the method table entry rather
    /// than by [applying the decorator](Self::decorators) at init — the decorator is
    /// dropped where this says anything other than [`Binding::Instance`]
    pub binding: Binding,
}

impl Function {
    /// a name unique within the module
    ///
    /// a method and a module-level function may share a python-visible name, so
    /// anything keyed per function — the call graph, say — has to qualify it
    pub fn qualified_name(&self) -> String {
        qualify(self.owner.as_deref(), &self.name)
    }

    /// the C identifier for the native entry point
    pub fn native_symbol(&self, module: &str) -> String {
        format!(
            "by_{}_{}{}",
            mangle(module),
            self.owner
                .as_deref()
                .map(|owner| format!("{}_", mangle(owner)))
                .unwrap_or_default(),
            mangle(&self.name)
        )
    }

    /// the C identifier for the python-facing wrapper
    pub fn wrapper_symbol(&self, module: &str) -> String {
        format!(
            "byw_{}_{}{}",
            mangle(module),
            self.owner
                .as_deref()
                .map(|owner| format!("{}_", mangle(owner)))
                .unwrap_or_default(),
            mangle(&self.name)
        )
    }

    /// whether any call can be handed to the interpreted definition
    pub fn defers(&self) -> bool {
        !self.deferring.is_empty() || !self.computed_defaults.is_empty()
    }

    /// the C identifier for the handle to this function's interpreted definition,
    /// which a deferring boundary calls instead of the native entry
    pub fn interpreted_symbol(&self, module: &str) -> String {
        // qualified by the class, for the same reason the compiled symbol is: a method
        // and a module-level function may share a python-visible name
        match &self.owner {
            Some(owner) => format!(
                "byi_{}_{}_{}",
                mangle(module),
                mangle(owner),
                mangle(&self.name)
            ),
            None => format!("byi_{}_{}", mangle(module), mangle(&self.name)),
        }
    }

    pub fn params(&self) -> &[RegisterDecl] {
        &self.registers[..self.param_count]
    }

    pub fn register(&self, id: RegisterId) -> Option<&RegisterDecl> {
        self.registers.get(id.index())
    }

    pub fn block(&self, id: BlockId) -> Option<&BasicBlock> {
        self.blocks.get(id.index())
    }

    /// the type of an operand: an immediate's own type, or the declared type of
    /// the register it names
    pub fn value_type(&self, value: &Value) -> Option<RType> {
        match value {
            Value::Register(id) => self.register(*id).map(|decl| decl.ty.clone()),
            other => other.immediate_type(),
        }
    }

    /// whether this function reaches into a class of this module's own, by name
    ///
    /// three places say so, and all three are needed. an operation may name a class
    /// outright — see [`Op::named_classes`]. a register may be *typed* as an instance
    /// of one, which is what licenses every direct field read the body then makes of
    /// it. and the return type is a register's type one frame along: a caller that
    /// takes the answer into an instance-typed register reads it as that struct.
    ///
    /// a tuple or an array of instances counts too, which is why the types are asked
    /// rather than matched — see [`RType::instance_classes`]
    pub fn names_class(&self, class: &str) -> bool {
        self.ret.instance_classes().contains(&class)
            || self
                .registers
                .iter()
                .any(|decl| decl.ty.instance_classes().contains(&class))
            || self.blocks.iter().any(|block| {
                block
                    .ops
                    .iter()
                    .any(|op| op.named_classes().contains(&class))
            })
    }

    /// the entry block, which is always block 0
    pub const fn entry() -> BlockId {
        BlockId(0)
    }
}

/// one attribute of a native class
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    pub name: String,
    pub ty: crate::rtype::RType,
    /// the constructor's default for it, `None` where it has none
    ///
    /// only an immediate, for the same reason a parameter default is: it is
    /// evaluated once in python and cannot change, so inlining it is the same thing
    pub default: Option<crate::ops::Value>,
    /// whether an instance may not have this attribute at all.
    ///
    /// `__init__` assigns it on some paths and not others, which python answers with
    /// `AttributeError` on the paths that skipped it. the field is still laid out — it
    /// costs a byte beside it saying whether it was ever written
    pub optional: bool,
}

impl FieldDecl {
    /// the C member name
    ///
    /// a generated field is named with a `$` so it cannot collide with a source
    /// name, and `$` is not a portable C identifier character — msvc rejects it
    /// the C struct member this field is stored in
    ///
    /// prefixed, and unconditionally: a python attribute may be called `int`, `const`
    /// or `default`, and a struct member of that name is not C at all. a list of the
    /// reserved words would have to be kept correct against every C version and
    /// compiler extension forever; a prefix cannot collide with any of them
    pub fn member(&self) -> String {
        format!("by_f_{}", mangle(&self.name))
    }

    /// the C struct member holding whether this field has been written
    ///
    /// only an [optional](Self::optional) field has one. zero means absent, and
    /// `tp_alloc` zeroes the instance, so "never written" needs no constructor work
    pub fn presence(&self) -> String {
        format!("by_p_{}", mangle(&self.name))
    }
}

/// what makes a class a generator or a coroutine
///
/// the two differ only in how python asks for the next step — `__await__` against
/// `__iter__` — so `coroutine` says nothing on its own and lives here rather than
/// beside it
#[derive(Debug, Clone, PartialEq)]
pub struct Resumption {
    /// the method that runs the next step
    pub method: String,
    /// which surface the state object presents to python
    pub surface: Surface,
}

/// what a resumable frame looks like from python
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `__iter__`/`__next__`
    Generator,
    /// `__await__`, and deliberately *not* iterable
    Coroutine,
    /// `__aiter__`/`__anext__`, where `__anext__` hands back an awaitable — the one
    /// surface whose two kinds of suspension mean different things
    AsyncGenerator,
}

/// what a class extends
///
/// the two cases differ in everything that matters to codegen — the layout, who builds
/// and frees the instance, and what `self` is — so they are one type rather than two
/// fields, and every consumer has to say which it means
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassBase {
    /// a class this module also emits.
    ///
    /// the subclass's struct begins with the base's fields, so a pointer to one is a
    /// valid pointer to the other, and the methods read fields at fixed offsets
    InModule(String),
    /// names resolved at module init — imports in the module dict, or builtins like
    /// `Exception`. more than one where the class has more than one base.
    ///
    /// nothing is known about their layout, so the subclass declares **none of its
    /// own**: `basicsize` is 0, the bases build and free the instance, and python works
    /// out the MRO and which of them owns the layout.
    ///
    /// a class this module emits may stand among them, so long as it lays nothing out.
    /// it brings no layout, so it changes none of the above — and it is in the module
    /// namespace by the time this class is built, so it resolves like any other name
    External(Vec<String>),
}

impl ClassBase {
    /// the in-module class whose *layout* this one extends, where it has one
    ///
    /// only the single-base form: a class standing beside names from outside takes its
    /// layout from outside too, so nothing here continues a struct
    pub fn in_module(&self) -> Option<&str> {
        match self {
            Self::InModule(name) => Some(name),
            Self::External(_) => None,
        }
    }

    /// the names to resolve at module init, where the bases are not ours
    pub fn external(&self) -> Option<&[String]> {
        match self {
            Self::External(names) => Some(names),
            Self::InModule(_) => None,
        }
    }

    /// every base written as a bare name, which is what a class this module emits looks
    /// like from here
    ///
    /// a dotted path is never one of ours, so only a bare name can be — and which of
    /// them the module actually emits is the caller's to decide, because only it knows
    /// what got a layout. it is what says a class is open: one another class here is
    /// built on has to be a heap type python can derive from, and it gives up the
    /// direct method call, because that other class may override
    pub fn plain_names(&self) -> impl Iterator<Item = &str> {
        let (single, many) = match self {
            Self::InModule(name) => (Some(name.as_str()), [].as_slice()),
            Self::External(paths) => (None, paths.as_slice()),
        };
        single.into_iter().chain(
            many.iter()
                .map(String::as_str)
                .filter(|path| !path.contains('.')),
        )
    }
}

/// a class compiled to a C extension type
///
/// the instance layout is a C struct, so an attribute is a field read at a
/// compile-time offset rather than two hash lookups. every field is assigned by
/// the generated `__init__`, which is what makes them *always defined* — no
/// bitfield and no per-read check
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the lint guards against confusable positional arguments, and every one of \
              these is written by name"
)]
pub struct ClassIr {
    pub name: String,
    /// whether a field, once the constructor has written it, cannot change again.
    ///
    /// only a `frozen data class`, and two things follow from it: no setters, the way
    /// `@dataclass(frozen=True)` has none, and — the part a type system is needed for
    /// — two reads of one field are a *single* read even across an arbitrary call
    pub immutable: bool,
    /// whether the module's python namespace exposes this class.
    ///
    /// a closure environment is a real type with a real layout, but it is an
    /// implementation detail — nothing should be able to name it
    pub exported: bool,
    /// the class this one extends, where it has one
    pub base: Option<ClassBase>,
    /// whether the class has no `__init__` of its own — neither written nor generated
    ///
    /// then `object.__init__` is what rejects a call with arguments, and python names
    /// the *class* in that message rather than a method the class does not have
    pub inherited_init: bool,
    /// whether the class body declares `__slots__`.
    ///
    /// python reads that as "this instance's attributes are exactly these names", and
    /// gives such an instance no `__dict__` — so a class that declares it is asking for
    /// the layout an emitted class has anyway, and one that does not is asking for a
    /// name it never mentioned to still have somewhere to go
    pub declares_slots: bool,
    /// whether the class declares type parameters.
    ///
    /// they are erased in the *layout* — every `T` field is an object, whatever
    /// `T` turns out to be — but not in the namespace: `Box[int]` has to keep
    /// working, and a type built in C answers that through `__class_getitem__`
    pub generic: bool,
    /// class-level constants, by name.
    ///
    /// the *values* are not here: whatever the expression was, the interpreted
    /// definition already evaluated it at class-definition time, so module init
    /// copies each one across rather than computing it a second time. that also
    /// keeps the object identical between the two, which is what python's
    /// evaluate-once rule means
    pub constants: Vec<String>,
    /// the constants that are dunders filling a type slot, and so need one emitted.
    ///
    /// a name in `tp_dict` does not fill a slot: python reads `tp_repr` for `repr(x)`
    /// and never consults the name. so `__repr__ = _repr` needs a slot of its own that
    /// reaches the assigned value, or the class answers twice — the inherited slot for
    /// `repr(x)` and the assignment for `x.__repr__()`
    pub slot_aliases: Vec<SlotAlias>,
    pub fields: Vec<FieldDecl>,
    /// decorators to apply, outermost first, after the type is in the namespace.
    ///
    /// a construction resolves the class through that namespace, so it gets whatever
    /// the decorator produced — which is what makes decorating the *class* sound
    /// where decorating a construction site would not be
    pub decorators: Vec<Decorator>,
    /// methods, whose first parameter is the receiver
    pub methods: Vec<Function>,
    /// how the class resumes, when it is a generator or a coroutine
    pub resume: Option<Resumption>,
    /// the keyword arguments the class header carries, in source order
    ///
    /// python passes these to the metaclass — `metaclass` itself picks the metaclass,
    /// and every other one reaches `__init_subclass__`. a type spec has nowhere to put
    /// them, so a class with any is built through its metaclass instead
    pub keywords: Vec<ClassKeyword>,
}

/// a dunder the class body *assigned* rather than defined, which fills a type slot
///
/// the value is not here for the same reason a constant's is not: the interpreted
/// definition evaluated it once, and module init copies that same object across. the
/// slot reaches it back out of the type's dict, so both `repr(x)` and `x.__repr__()`
/// answer with the one object the assignment named
#[derive(Debug, Clone, PartialEq)]
pub struct SlotAlias {
    /// the dunder written on the left of the assignment
    pub name: String,
    /// whether the body assigned `None`, which is how python says a type does not
    /// support the operation at all rather than naming something to call.
    ///
    /// only `__hash__` reaches this: it is the one slot with a standing value for
    /// "unsupported" (`PyObject_HashNotImplemented`), and every other dunder assigned
    /// `None` is declined
    pub unsupported: bool,
}

/// one `name=value` in a class header
#[derive(Debug, Clone, PartialEq)]
pub struct ClassKeyword {
    pub name: String,
    pub value: KeywordValue,
}

/// the value side of a class keyword
///
/// evaluated at class-definition time in the module scope, which at import is
/// exactly what the module namespace holds — so a name is resolved the way a base
/// is, and a literal is emitted where it stands
#[derive(Debug, Clone, PartialEq)]
pub enum KeywordValue {
    /// a name, or a chain of attributes on one
    Path(String),
    Bool(bool),
    None,
    Int(i64),
    Str(String),
}

impl ClassIr {
    /// whether python may assign to a field of this class
    ///
    /// an immutable one has no setters by definition, and a class nothing can name —
    /// a generator's state object, a closure environment — has no surface to put them
    /// on. those set neither flag for the same reason: they are not the *language's*
    /// classes
    pub fn writable(&self) -> bool {
        !self.immutable && self.exported
    }

    /// the C identifier for the instance struct
    pub fn struct_name(&self, module: &str) -> String {
        format!("By_{}_{}", mangle(module), mangle(&self.name))
    }

    /// the C identifier for the type object — a static struct, or a pointer to a
    /// heap type when the class carries decorators
    pub fn type_name(&self, module: &str) -> String {
        format!("By_{}_{}_Type", mangle(module), mangle(&self.name))
    }
}

/// where a module's `.by` source lives, and where its lines start
///
/// enough for codegen to turn a byte offset into a `#line`, without carrying the
/// source text around
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineTable {
    pub path: String,
    /// the byte offset of each line's first character
    pub starts: Box<[u32]>,
}

impl LineTable {
    pub fn new(path: impl Into<String>, source: &str) -> Self {
        // a `.by` file past 4GiB would need a wider offset everywhere, starting
        // with the ast's own `TextSize`
        let mut starts = vec![0u32];
        starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .filter_map(|(offset, _)| u32::try_from(offset).ok()?.checked_add(1)),
        );
        Self {
            path: path.into(),
            starts: starts.into_boxed_slice(),
        }
    }

    /// the 1-based line a byte offset falls on
    pub fn line(&self, offset: u32) -> usize {
        self.starts.partition_point(|start| *start <= offset)
    }
}

/// a module's worth of compiled functions
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleIr {
    /// the module's name — see [`ModuleName`] for why that is not one string
    pub name: ModuleName,
    pub functions: Vec<Function>,
    pub classes: Vec<ClassIr>,
    /// functions the compiler declined, with the reason. these fall back to the
    /// interpreted definition and are reported by `by compile --verbose`
    pub declined: Vec<Declined>,
    /// places where a gradual type entered a compiled signature.
    ///
    /// a gradual type no longer stops a function compiling — it lands on the
    /// widest representation — so `--no-any` cannot be answered by looking at
    /// what declined. it is answered from here
    pub gradual: Vec<GradualUse>,
    /// places that would have had an unboxed representation but for python's
    /// numeric promotion.
    ///
    /// nothing here stops a function compiling — it compiles to boxed arithmetic —
    /// so a decline message has nowhere to hang. this is the honest signal, and the
    /// moment someone wants to know `strict-float` exists
    pub promoted: Vec<PromotedPlace>,
    /// the `.by` source's line table, when the build knew it. codegen emits
    /// `#line` from it, so a compiler warning or a debugger points at the `.by`
    pub lines: Option<LineTable>,
    /// the module's transpiled python, executed in the extension's own namespace
    /// at import time.
    ///
    /// this is what makes coverage of the language total: module-level code runs,
    /// declined functions exist, and the natively compiled functions are then
    /// installed over the top of their interpreted definitions
    pub fallback_source: Option<String>,
    /// the same program, already compiled by the interpreter this artefact is
    /// being built for, when the build had one to ask.
    ///
    /// parsing a module the size of `argparse` costs milliseconds and reading a
    /// marshalled code object costs tens of microseconds, and every import pays
    /// the difference. it is a *cache* of [`Self::fallback_source`] and never a
    /// replacement for it — see [`FallbackCode`] for what an interpreter has to
    /// match before it may use one
    pub fallback_code: Option<FallbackCode>,
}

/// a module body compiled to a code object, and what an interpreter has to match
/// before it may run that code object rather than the source
///
/// both conditions are about the code object being *the same program* the source
/// would have compiled to on this interpreter. neither is a guess: they are the two
/// things cpython itself keys a `.pyc` on
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackCode {
    /// `marshal.dumps` of the module body's code object
    pub marshalled: Box<[u8]>,
    /// the bytecode magic number of the interpreter that wrote it.
    ///
    /// cpython bumps this whenever a code object stops meaning what it did, which
    /// is why an upgraded interpreter regenerates `.pyc` files instead of misreading
    /// them. an interpreter whose own magic differs falls back to the source
    pub magic: i64,
    /// the optimization level it was compiled at.
    ///
    /// `-O` takes `assert` out of the bytecode and `-OO` takes docstrings too, so
    /// source compiled at one level is a different program from the same source
    /// compiled at another. an interpreter running at a different level falls back
    /// to the source, which is what makes `python -O` still mean `-O` here
    pub optimize: i32,
}

/// a gradual type in a compiled signature
#[derive(Debug, Clone, PartialEq)]
pub struct GradualUse {
    pub function: String,
    /// the place, as a user would name it: a parameter name, or `return`
    pub place: String,
}

/// a representation python's numeric promotion cost a place
#[derive(Debug, Clone, PartialEq)]
pub struct PromotedPlace {
    pub function: String,
    /// the place, as a user would name it: a parameter name, a local, or `return`
    pub place: String,
    /// what it would have been, spelled the way the annotated report spells a type
    pub missed: RType,
}

/// a function the compiler could not lower natively
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declined {
    pub name: String,
    pub reason: String,
    /// the byte offsets of the definition in the `.by` source, when the frontend
    /// knew them.
    ///
    /// a decline is the compiler's main output for the code it did *not* take, so
    /// it should point at that code the way any other diagnostic does
    pub range: Option<(u32, u32)>,
}

/// the qualified name a `CallNative` target resolves to
pub fn qualify(owner: Option<&str>, name: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}.{name}"),
        None => name.to_string(),
    }
}

/// a module-level definition whose decorators module init applies to the native one
///
/// python evaluates a decorator where the `def` or the `class` stands. the interpreted
/// twin is what stands there, so the twin evaluates it — and module init then evaluates
/// it a second time, over the native definition that replaces the twin's. the binding
/// the namespace ends up with is right either way, so what shows is not a wrong value
/// but a side effect that happened twice: a registry with two entries for one function.
///
/// the decorator is therefore taken out of the source the twin runs, and this is the one
/// description of which ones: it drives both that removal and the C that applies them, so
/// the two cannot come to disagree.
///
/// a module-level *function* and a module-level *class* are both here. a class the
/// module body still reaches before init has run does not get this far: it declines
/// instead, because between the twin's `class` statement and init the name holds an
/// undecorated definition, and whatever read it in that window keeps what it read
///
/// a *method* is deliberately not here, and its decorator still runs twice. it is not
/// only a side effect that would move: the class *construction* reads what a method
/// decorator wrote, and `ABCMeta` is the case — it computes `__abstractmethods__` from
/// the namespace the body left, so taking `@abstractmethod` out of the twin empties that
/// set on every class whose construction falls back to the interpreted definition. a
/// method's decorator has to be applied where the `def` stands, which means the answer
/// is to carry the *result* across rather than to move the decorator; see the module docs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitDecoration<'a> {
    /// the name as written, which is the name the twin's source binds it under
    pub name: &'a str,
    /// outermost first, the order they are written in
    pub decorators: &'a [Decorator],
}

impl ModuleIr {
    /// every module-level definition init decorates — see [`InitDecoration`]
    pub fn decorated_at_init(&self) -> impl Iterator<Item = InitDecoration<'_>> {
        // a closure environment is not in the namespace under any name, so there is no
        // entry to apply anything to — and it carries no decorator anyway
        let classes = self
            .classes
            .iter()
            .filter(|class| class.exported && !class.decorators.is_empty())
            .map(|class| InitDecoration {
                name: class.name.as_str(),
                decorators: &class.decorators,
            });
        // an unboxed edition is a second `Function` under a mangled name the namespace
        // never holds, and it carries the decorators of the function it is an edition
        // of. reaching for that name here would fail the import with a `NameError`
        let functions = self
            .functions
            .iter()
            .filter(|function| function.exported && !function.decorators.is_empty())
            .map(|function| InitDecoration {
                name: function.name.as_str(),
                decorators: &function.decorators,
            });
        classes.chain(functions)
    }

    /// every compiled function, methods included
    ///
    /// a pass that iterates `functions` alone silently skips every method, which
    /// is most of the code in a class-heavy module
    pub fn all_functions(&self) -> impl Iterator<Item = &Function> {
        self.functions
            .iter()
            .chain(self.classes.iter().flat_map(|class| class.methods.iter()))
    }

    pub fn all_functions_mut(&mut self) -> impl Iterator<Item = &mut Function> {
        self.functions.iter_mut().chain(
            self.classes
                .iter_mut()
                .flat_map(|class| class.methods.iter_mut()),
        )
    }

    pub fn new(name: impl Into<ModuleName>) -> Self {
        Self {
            name: name.into(),
            functions: Vec::new(),
            classes: Vec::new(),
            declined: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
        }
    }

    /// the C identifier for the module's init function, which cpython looks up by
    /// name when loading the extension
    pub fn init_symbol(&self) -> String {
        format!("PyInit_{}", mangle(self.name.last_component()))
    }
}

/// a module's name, in the forms a build needs
///
/// they are different things, and conflating them is what made a class in
/// `tkinter/m.py` answer `__module__ == "m"`. only the dotted name is stored, so
/// they cannot drift apart: the shorter ones are derived from it on demand
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ModuleName {
    dotted: String,
    /// whether the name belongs to a package's `__init__` rather than to a plain
    /// module
    ///
    /// the dotted name cannot say, and the difference decides which file the
    /// artefact is written to: `a.b` is `a/b.py` for one and `a/b/__init__.py`
    /// for the other
    is_package: bool,
}

impl ModuleName {
    /// a plain module — one written in a file named after its own last component
    pub fn new(dotted: impl Into<String>) -> Self {
        Self {
            dotted: dotted.into(),
            is_package: false,
        }
    }

    /// a package, written in the `__init__` inside the directory the name spells
    ///
    /// `dotted` is the package's own name, not its `__init__`'s: `a/b/__init__.py`
    /// is the module `a.b`, because that is what python imports it as and what a
    /// class written in it reports as its `__module__`
    pub fn package(dotted: impl Into<String>) -> Self {
        Self {
            dotted: dotted.into(),
            is_package: true,
        }
    }

    /// the name python imports the module as, dots and all
    ///
    /// this is what a type's `tp_name` carries: cpython splits `tp_name` at its
    /// last dot to answer `__module__` and `__name__`, so a class in the module
    /// `tkinter.m` needs the whole of it to say where it came from.
    ///
    /// it is also what every emitted C identifier is prefixed with, once the
    /// mangling has turned the dots into underscores — two modules of the same
    /// name in different packages then get different symbols
    pub fn dotted(&self) -> &str {
        &self.dotted
    }

    /// the last dotted component — the module's own name inside its package
    ///
    /// this is cpython's rule rather than ours: cpython derives the init function
    /// it looks up from the last component of the *spec* name, so an extension
    /// imported as `tkinter.m` has to export `PyInit_m` however deep the package
    /// goes, and a package imported as `tkinter.sub` exports `PyInit_sub` even
    /// though its file is called `__init__`
    ///
    /// it is emphatically **not** the file name — see [`Self::relative_path`]
    pub fn last_component(&self) -> &str {
        self.dotted.rsplit('.').next().unwrap_or(&self.dotted)
    }

    /// where the module's file belongs inside an output tree, given the suffix the
    /// file carries (`".c"`, or the interpreter's extension suffix)
    ///
    /// the tree has to mirror the module tree, because that is the only layout
    /// python will import these back under: `a.b.c` is only reachable as
    /// `a/b/c<suffix>`. a flat directory can offer nothing but the last component,
    /// which is both unimportable as a package member and a collision — two
    /// members of one package ending in the same component overwrite each other.
    ///
    /// a package is the case the dotted name alone gets wrong: `a.b` as a package
    /// is the *directory* `a/b`, and its file is the `__init__` inside it
    pub fn relative_path(&self, suffix: &str) -> PathBuf {
        let mut directories: Vec<&str> = self
            .dotted
            .split('.')
            // an empty component would contribute nothing to the path but would
            // shift which component names the file, so it is dropped rather than
            // joined
            .filter(|component| !component.is_empty())
            .collect();
        // a package's last component names the directory holding its `__init__`; a
        // plain module's names the file itself
        let stem = if self.is_package {
            "__init__"
        } else {
            directories.pop().unwrap_or_default()
        };
        let mut path: PathBuf = directories.into_iter().collect();
        path.push(format!("{stem}{suffix}"));
        path
    }
}

impl From<&str> for ModuleName {
    fn from(dotted: &str) -> Self {
        Self::new(dotted)
    }
}

impl From<String> for ModuleName {
    fn from(dotted: String) -> Self {
        Self::new(dotted)
    }
}

/// map a dotted or otherwise punctuated name onto a C identifier
fn mangle(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ops::BlockId;

    fn function() -> Function {
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
            registers: vec![
                RegisterDecl {
                    borrowed: false,
                    name: Some("a".to_string()),
                    ty: RType::INT,
                    may_be_unassigned: false,
                },
                RegisterDecl {
                    borrowed: false,
                    name: Some("b".to_string()),
                    ty: RType::INT,
                    may_be_unassigned: false,
                },
            ],
            blocks: vec![BasicBlock::new(Terminator::Return(Value::Register(
                RegisterId(0),
            )))],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            binding: Binding::Instance,
        }
    }

    /// an unboxed edition is a second `Function` under a mangled name, carrying the
    /// decorators of the function it is an edition of. the namespace never binds that
    /// name, so a decorator applied to it at init would raise `NameError` and take the
    /// whole import with it
    #[test]
    fn a_definition_the_namespace_does_not_bind_is_not_decorated_at_init() {
        let mut module = ModuleIr::new("m");
        let mut exported = function();
        exported.decorators = vec![Decorator::name("mark")];
        let mut edition = exported.clone();
        edition.name = "add$arr0l".to_string();
        edition.exported = false;
        module.functions = vec![exported, edition];
        let named: Vec<&str> = module
            .decorated_at_init()
            .map(|decoration| decoration.name)
            .collect();
        assert_eq!(named, ["add"]);
    }

    #[test]
    fn symbols_are_c_identifiers() {
        let f = function();
        assert_eq!(f.native_symbol("pkg.mod"), "by_pkg_mod_add");
        assert_eq!(f.wrapper_symbol("pkg.mod"), "byw_pkg_mod_add");
    }

    #[test]
    fn the_init_symbol_uses_only_the_last_component() {
        // cpython looks up PyInit_<last component of the spec name>
        let module = ModuleIr::new("pkg.sub.mod");
        assert_eq!(module.init_symbol(), "PyInit_mod");
        assert_eq!(ModuleIr::new("mod").init_symbol(), "PyInit_mod");
    }

    /// a package's file is called `__init__`, but cpython still derives the init
    /// symbol from the last component of the name it is imported under — so the
    /// symbol and the file name disagree, and only for a package
    #[test]
    fn a_packages_init_symbol_is_named_after_the_package_not_its_file() {
        let module = ModuleIr::new(ModuleName::package("pkg.sub"));
        assert_eq!(module.init_symbol(), "PyInit_sub");
        assert_eq!(
            module.name.relative_path(".so"),
            Path::new("pkg/sub/__init__.so")
        );
    }

    /// the two halves a module name has to keep apart: what python imports the
    /// module as, and the shorter name cpython's loader uses
    #[test]
    fn a_module_name_keeps_its_dotted_form_and_its_last_component() {
        let name = ModuleName::new("pkg.sub.mod");
        assert_eq!(name.dotted(), "pkg.sub.mod");
        assert_eq!(name.last_component(), "mod");

        // a top-level module is the same string twice, which is why the two were
        // conflated for so long
        let alone = ModuleName::new("mod");
        assert_eq!(alone.dotted(), "mod");
        assert_eq!(alone.last_component(), "mod");
    }

    /// the artefact tree is the module tree: a name is only importable back from
    /// the path its dots spell out
    #[test]
    fn a_module_name_spells_out_the_path_its_artefact_is_written_to() {
        assert_eq!(
            ModuleName::new("pkg.sub.mod").relative_path(".c"),
            Path::new("pkg/sub/mod.c")
        );
        assert_eq!(
            ModuleName::package("pkg.sub").relative_path(".c"),
            Path::new("pkg/sub/__init__.c")
        );
        assert_eq!(
            ModuleName::new("mod").relative_path(".c"),
            Path::new("mod.c")
        );
        assert_eq!(
            ModuleName::package("pkg").relative_path(".cpython-313-darwin.so"),
            Path::new("pkg/__init__.cpython-313-darwin.so")
        );
    }

    /// two members of one package can share a last component, and a flat output
    /// directory would give them the same file — this is the collision the tree
    /// layout removes
    #[test]
    fn two_package_members_sharing_a_last_component_take_different_paths() {
        assert_ne!(
            ModuleName::new("pkg.dup").relative_path(".so"),
            ModuleName::new("pkg.sub.dup").relative_path(".so")
        );
    }

    #[test]
    fn params_are_the_leading_registers() {
        let f = function();
        assert_eq!(f.params().len(), 2);
        assert_eq!(f.params()[1].name.as_deref(), Some("b"));
    }

    #[test]
    fn value_type_reads_registers_and_immediates() {
        let f = function();
        assert_eq!(
            f.value_type(&Value::Register(RegisterId(1))),
            Some(RType::INT)
        );
        assert_eq!(f.value_type(&Value::Float(1.0)), Some(RType::FLOAT));
        // an out-of-range register has no type rather than panicking
        assert_eq!(f.value_type(&Value::Register(RegisterId(9))), None);
    }

    #[test]
    fn out_of_range_lookups_return_none() {
        let f = function();
        assert!(f.register(RegisterId(9)).is_none());
        assert!(f.block(BlockId(9)).is_none());
        assert!(f.block(Function::entry()).is_some());
    }

    #[test]
    fn only_the_fallible_convention_can_fail() {
        assert!(CallConvention::Native.can_fail());
        assert!(!CallConvention::NativeInfallible.can_fail());
    }
}

#[cfg(test)]
mod line_table_tests {
    use super::LineTable;

    #[test]
    fn an_offset_maps_to_its_one_based_line() {
        let source = "a
bb

ccc";
        let table = LineTable::new("m.by", source);
        assert_eq!(table.line(0), 1);
        assert_eq!(table.line(2), 2);
        // the blank line, and the line after it
        assert_eq!(table.line(5), 3);
        assert_eq!(table.line(6), 4);
        // one past the end still answers, rather than panicking
        assert_eq!(table.line(u32::try_from(source.len()).unwrap_or(0)), 4);
    }

    #[test]
    fn a_source_with_no_newline_is_one_line() {
        let table = LineTable::new("m.by", "just this");
        assert_eq!(table.line(0), 1);
        assert_eq!(table.line(8), 1);
    }

    #[test]
    fn an_empty_source_still_answers() {
        let table = LineTable::new("m.by", "");
        assert_eq!(table.line(0), 1);
    }
}
