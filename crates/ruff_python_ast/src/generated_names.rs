//! basedpython: the name nodes the parser writes rather than the author
//!
//! several constructs stand for code nobody typed. `init(let x: int)` stands for
//! a `self.x = x`, a property accessor's `field` stands for a read of the
//! property's backing storage, and an enum variant's anonymous field is declared
//! under a name the parser counts out (`_0`, `_1`, …). the nodes built for those
//! carry a source range all the same — a diagnostic about one has to point
//! somewhere the reader can see — so a range is not enough to tell them apart
//! from what the author wrote
//!
//! the parser records each one here instead, and a consumer that cares whether a
//! name in the tree is the author's asks [`GeneratedNames`] rather than reading
//! the source back. `used-underscore-name` is the consumer this exists for: it
//! reports a use of a name the author spelled with a leading underscore, and both
//! halves of that question — is this use one the author wrote, and is the
//! declaration it resolves to spelled by the author — are answered here

use ruff_text_size::TextRange;
use rustc_hash::FxHashMap;

use crate::name::Name;

/// whose name a generated node carries
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "get-size", derive(get_size2::GetSize))]
pub enum GeneratedName {
    /// the parser wrote the node, under a name the author wrote elsewhere: the
    /// `self.x = x` of `init(let x: int)` names the parameter `x`
    AuthorName,
    /// the parser wrote the node *and* the name it carries: a property's backing
    /// storage `__age`, an enum variant's anonymous field `_0`
    ParserName,
}

/// every name node the parser wrote in one file, by the range and name it carries
///
/// two generated nodes can share a range — an `init` parameter's `self.x` target
/// and the `x` it reads both sit on the parameter — so the name is part of the
/// key. a node the author wrote is never recorded, and looking one up answers
/// `None`
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeneratedNames(FxHashMap<(TextRange, Name), GeneratedName>);

impl GeneratedNames {
    pub fn insert(&mut self, range: TextRange, name: Name, generated: GeneratedName) {
        self.0.insert((range, name), generated);
    }

    pub fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// what wrote the name `name` at `range`, or `None` where the author did
    pub fn get(&self, range: TextRange, name: Name) -> Option<GeneratedName> {
        self.0.get(&(range, name)).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn shrink_to_fit(&mut self) {
        self.0.shrink_to_fit();
    }
}

#[cfg(feature = "get-size")]
impl get_size2::GetSize for GeneratedNames {
    fn get_heap_size(&self) -> usize {
        let entry = size_of::<((TextRange, Name), GeneratedName)>();
        self.0.capacity() * entry
            + self
                .0
                .keys()
                .map(|(_, name)| get_size2::GetSize::get_heap_size(name))
                .sum::<usize>()
    }
}
