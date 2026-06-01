//! Reverse transforms: rewrite standard Python source into idiomatic basedpython.
//!
//! Each forward transform in `crate::transforms` polyfills a basedpython idiom
//! to standard Python; the corresponding reverse transform here detects that
//! polyfill output and rewrites it back. Together they enable round-tripping
//! for ecosystem testing — a Python file run through `reverse_transpile` and
//! then `transpile` should produce code with the same AST as the original.
//!
//! Reverse transforms are intentionally conservative: when in doubt, leave
//! the source unchanged. A false negative (missed rewrite) is a no-op; a
//! false positive can change semantics.

pub(crate) mod anon_named_tuple;
pub(crate) mod auto_quote;
pub(crate) mod callable;
pub(crate) mod coalesce;
pub(crate) mod compat;
pub(crate) mod constraints;
pub(crate) mod dedent_string;
pub(crate) mod dynamic_keyword;
pub(crate) mod empty_declarations;
pub(crate) mod enums;
pub(crate) mod generics;
pub(crate) mod identity_swap;
pub(crate) mod intersection;
pub(crate) mod literal_types;
pub(crate) mod modifiers;
pub(crate) mod none_chain;
pub(crate) mod not_type;
pub(crate) mod overload;
pub(crate) mod prune_imports;
pub(crate) mod subscript;
pub(crate) mod super_keyword;
pub(crate) mod tuple_type;
pub(crate) mod type_is;
pub(crate) mod typing_redirect;
pub(crate) mod unpack;
