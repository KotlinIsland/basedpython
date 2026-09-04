//! the one shape every supported format is read into.

/// a value read out of a static resource.
///
/// json, toml and yaml disagree about a great many things, but they agree about
/// this much: a document is a tree of scalars, sequences and string-keyed
/// mappings. everything a format offers beyond that (a toml datetime, a yaml
/// anchor) is resolved or rejected while parsing, so the renderer never has to
/// know which format it came from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// an integer too large for [`Value::Int`], kept as the digits it was
    /// written with so nothing is lost to a round trip through `f64`
    BigInt(String),
    Float(f64),
    Str(String),
    Seq(Vec<Value>),
    /// insertion-ordered, because a reader of the generated python should find
    /// the keys in the order the document lists them
    Map(Vec<(String, Value)>),
}

impl Value {
    /// how many nodes the tree holds, counting itself.
    pub(crate) fn size(&self) -> usize {
        match self {
            Value::Seq(items) => 1 + items.iter().map(Value::size).sum::<usize>(),
            Value::Map(entries) => 1 + entries.iter().map(|(_, v)| v.size()).sum::<usize>(),
            _ => 1,
        }
    }

    /// how deeply the tree nests.
    pub(crate) fn depth(&self) -> usize {
        match self {
            Value::Seq(items) => 1 + items.iter().map(Value::depth).max().unwrap_or(0),
            Value::Map(entries) => 1 + entries.iter().map(|(_, v)| v.depth()).max().unwrap_or(0),
            _ => 1,
        }
    }
}
