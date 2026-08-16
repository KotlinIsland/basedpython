//! what a debugger observed about a running program, as an input to analysis
//!
//! ordinary analysis knows only what the source says. while a program is stopped, something else
//! is knowable: what its names actually hold at one line. feeding that back in is what lets an
//! editor say which of the branches below the stop line will be taken, rather than which of them
//! *could* be
//!
//! ## why this is part of the program identity
//!
//! [`Assumptions`] is a field of [`crate::Program`], and therefore part of what makes a
//! [`crate::ProgramFile`] the file it is. that is the same mechanism that keeps two python
//! versions' interpretations of one file apart, used for the same reason: a seeded analysis and an
//! unseeded one are different readings of the same source, and neither may leak into the other
//!
//! concretely, it means the diagnostics an editor is already showing do not change when a debugger
//! stops, because they are answers to a different query. it also means salsa caches the seeded
//! analysis, so stepping back to a line that was already asked about costs nothing
//!
//! ## what an observation is, and is not
//!
//! an [`Observation`] is deliberately not a `Type`. types live in `ty_python_semantic`, which
//! depends on this crate rather than the other way round — but the separation is worth having
//! anyway. what a debugger can see is an object's storage and its type's slots; what a type is, is
//! a lattice element. keeping the vocabulary of the first out of the second means the thing being
//! recorded stays "what was observed" rather than becoming "what the checker made of it"
//!
//! every form here is one a debugger can read **without running the program**. there is no
//! observation that a `__bool__`, a `__len__` or a property could have decided, because a debugger
//! that ran user code to answer a question about that code would be changing what it measured
//!
//! ## which frame an observation belongs to
//!
//! an observation carries a name and no scope, so [`Assumptions::line`] is what says where it was
//! read: the observations are the ones visible in the frame stopped on that line, and the
//! consumer applies them to the innermost scope that line falls in and to nothing else. a name
//! that scope does not bind is a free variable whose value nothing here can police, and it is
//! refused rather than guessed at
//!
//! ## stability is resolved before it gets here
//!
//! a runtime reading has a shelf life: an `int` cannot change, a `list`'s length can, and an
//! instance's class can because `__class__` is assignable on a heap type. that judgement belongs to
//! whatever is holding the object, and it is made there — this crate receives only the
//! observations that survived it

use ruff_db::files::File;
use ruff_python_ast::name::Name;

/// what a debugger observed at one line of one file
#[salsa::interned(debug, heap_size = ruff_memory_usage::heap_size)]
pub struct Assumptions<'db> {
    /// the file the program is stopped in
    ///
    /// every file resolved while analysing that one is resolved under the same seeded program, so
    /// without this a line number would be read against files the debugger was never in — a
    /// typeshed stub has a line 5 too, and it may well bind a name an observation is about
    #[returns(copy)]
    pub file: File,

    /// the one-based line the program is stopped on
    ///
    /// an observation describes the state *at* this line, so it applies to a use below it only
    /// while every binding that reaches that use is above it. a binding in between is the
    /// program's own and wins, which the use-def map already works out
    #[returns(copy)]
    pub line: u32,

    /// what was observed, at most one entry per name
    #[returns(deref)]
    pub observations: Box<[Observation]>,
}

impl get_size2::GetSize for Assumptions<'_> {}

/// one thing that was true of one name
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub struct Observation {
    /// the name it is about
    ///
    /// a dotted path is spelled as it was written — `self.limit` — because that is what the source
    /// being analysed spells, and reassembling it here would be undoing work the caller did
    pub name: Name,

    /// what was read
    pub observed: Observed,
}

/// what was read off a value
///
/// closed on purpose. a consumer that swept an unrecognised form into a catch-all would be treating
/// a reading it does not understand as one it does. for the same reason there is no form here that
/// nothing downstream can turn into a type: a variant that is only ever recorded would be a promise
/// to a client that the analysis does not keep
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub enum Observed {
    /// the value is `None`
    IsNone,

    /// the value is exactly this `bool`
    IsBool(bool),

    /// the value is exactly this integer, in decimal, with a leading `-` when negative
    ///
    /// text rather than a number because a python `int` has no width. a checker that cannot hold it
    /// as a literal falls back to `int`, which is still narrower than nothing
    IsInt(String),

    /// the value is exactly this string
    IsStr(String),

    /// the value is exactly these bytes
    IsBytes(Box<[u8]>),

    /// `type(value)` is exactly this class, named so it can be resolved against the source
    ///
    /// exactly, not "an instance of": the type object itself rather than a base of it
    IsExactly(ClassName),

    /// the value is this member of this enum
    IsEnumMember {
        /// the enum class
        class: ClassName,
        /// the member's name, as the source spells it after the dot
        member: Name,
    },
}

/// a class, named so that something reading source can resolve it
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub struct ClassName {
    /// the module the class was defined in
    ///
    /// `builtins` for a builtin, spelled out rather than left empty, so a consumer resolving names
    /// has one rule and not two
    pub module: String,

    /// the class's name inside that module, qualified — so a class nested in another is
    /// distinguishable from a module-level one of the same name
    pub qualname: String,
}
