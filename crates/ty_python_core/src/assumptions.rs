//! What a debugger observed about a running program, as an input to analysis.
//!
//! Ordinary analysis knows only what the source says. While a program is stopped, something else
//! is knowable: what its names actually hold at one line. Feeding that back in is what lets an
//! editor say which of the branches below the stop line will be taken, rather than which of them
//! *could* be.
//!
//! ## Why this is part of the program identity
//!
//! [`Assumptions`] is a field of [`crate::Program`], and therefore part of what makes a
//! [`crate::ProgramFile`] the file it is. That is the same mechanism that keeps two Python
//! versions' interpretations of one file apart, used for the same reason: a seeded analysis and an
//! unseeded one are different readings of the same source, and neither may leak into the other.
//!
//! Concretely, it means the diagnostics an editor is already showing do not change when a debugger
//! stops, because they are answers to a different query. It also means Salsa caches the seeded
//! analysis, so stepping back to a line that was already asked about costs nothing.
//!
//! ## What an observation is, and is not
//!
//! An [`Observation`] is deliberately not a `Type`. Types live in `ty_python_semantic`, which
//! depends on this crate rather than the other way round — but the separation is worth having
//! anyway. What a debugger can see is an object's storage and its type's slots; what a type is, is
//! a lattice element. Keeping the vocabulary of the first out of the second means the thing being
//! recorded stays "what was observed" rather than becoming "what the checker made of it".
//!
//! Every form here is one a debugger can read **without running the program**. There is no
//! observation that a `__bool__`, a `__len__` or a property could have decided, because a debugger
//! that ran user code to answer a question about that code would be changing what it measured.
//!
//! ## Stability is resolved before it gets here
//!
//! A runtime reading has a shelf life: an `int` cannot change, a `list`'s length can, and an
//! instance's class can because `__class__` is assignable on a heap type. That judgement belongs to
//! whatever is holding the object, and it is made there — this crate receives only the
//! observations that survived it.

use ruff_python_ast::name::Name;

/// What a debugger observed at one line of one file.
#[salsa::interned(debug, heap_size = ruff_memory_usage::heap_size)]
pub struct Assumptions<'db> {
    /// The one-based line the program is stopped on.
    ///
    /// An observation describes the state *at* this line, so it applies to a use below it only
    /// while every binding that reaches that use is above it. A binding in between is the
    /// program's own and wins, which the use-def map already works out.
    pub line: u32,

    /// What was observed, at most one entry per name.
    #[returns(deref)]
    pub observations: Box<[Observation]>,
}

impl get_size2::GetSize for Assumptions<'_> {}

impl<'db> Assumptions<'db> {
    /// What was observed about `name`, if anything was.
    pub fn observed(self, db: &'db dyn crate::Db, name: &Name) -> Option<&'db Observed> {
        self.observations(db)
            .iter()
            .find(|observation| &observation.name == name)
            .map(|observation| &observation.observed)
    }
}

/// One thing that was true of one name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub struct Observation {
    /// The name it is about.
    ///
    /// A dotted path is spelled as it was written — `self.limit` — because that is what the source
    /// being analysed spells, and reassembling it here would be undoing work the caller did.
    pub name: Name,

    /// What was read.
    pub observed: Observed,
}

/// What was read off a value.
///
/// Closed on purpose. A consumer that swept an unrecognised form into a catch-all would be treating
/// a reading it does not understand as one it does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub enum Observed {
    /// The value is `None`.
    IsNone,

    /// The value is exactly this `bool`.
    IsBool(bool),

    /// The value is exactly this integer, in decimal, with a leading `-` when negative.
    ///
    /// Text rather than a number because a Python `int` has no width. A checker that cannot hold it
    /// as a literal falls back to `int`, which is still narrower than nothing.
    IsInt(String),

    /// The value is exactly this string.
    IsStr(String),

    /// The value is exactly these bytes.
    IsBytes(Box<[u8]>),

    /// `type(value)` is exactly this class, named so it can be resolved against the source.
    ///
    /// Exactly, not "an instance of": the type object itself rather than a base of it.
    IsExactly(ClassName),

    /// The value is this member of this enum.
    IsEnumMember {
        /// The enum class.
        class: ClassName,
        /// The member's name, as the source spells it after the dot.
        member: Name,
    },

    /// `len(value)` is this.
    HasLength(usize),

    /// `bool(value)` is this.
    ///
    /// Carried separately from the value because it is knowable for objects whose value is not: a
    /// container's truthiness follows a length that was readable even when nothing else was.
    IsTruthy(bool),
}

/// A class, named so that something reading source can resolve it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub struct ClassName {
    /// The module the class was defined in.
    ///
    /// `builtins` for a builtin, spelled out rather than left empty, so a consumer resolving names
    /// has one rule and not two.
    pub module: String,

    /// The class's name inside that module, qualified — so a class nested in another is
    /// distinguishable from a module-level one of the same name.
    pub qualname: String,
}
