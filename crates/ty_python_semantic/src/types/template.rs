//! basedpython template literal types — an f-string written in a type position.
//!
//! ```by
//! path: f"/{str}"
//! version: f"v{int}.{int}"
//! ```
//!
//! a template type is the set of strings its pattern can produce: the fixed text
//! spelled between the holes, with each hole standing for `str(x)` over the
//! values of the hole's own type. `Literal["/home"]` inhabits `f"/{str}"`;
//! `Literal["home"]` does not.
//!
//! the constructor normalizes, so the variant only ever exists in a form that
//! nothing else can already spell:
//!
//! - a hole spelled as a type alias is the type that alias stands for
//! - a hole whose type renders to one string is folded into the text, so a
//!   template with no holes left is a plain string literal
//! - a union hole distributes, so `f"{Literal[1, 2]}"` is `Literal["1", "2"]`
//! - a lone hole that is itself a set of strings is that type, so `f"{str}"` is
//!   `str`
//!
//! a pattern written in a type expression is a declared type and never widens.
//! one inferred from an f-string *value* is promotable, exactly as a string
//! literal inferred from `"abc"` is: it keeps its precision where the precision
//! is readable and widens to `str` where a `str` is what the context can hold —
//! an element of a mutable list, say, which is invariant in it.
//!
//! two questions are asked of a template, and both are answered structurally:
//! whether a concrete string inhabits it ([`TemplateLiteralType::matches_str`])
//! and whether one template's strings are all contained in another's
//! ([`TemplateLiteralType::contains`]). containment is a sufficient test, not a
//! decision procedure — a template pair it cannot align is reported unrelated
//! rather than guessed at.

use compact_str::CompactString;
use rustc_hash::FxHashSet;

use crate::Db;
use crate::types::character::is_single_grapheme;
use crate::types::{KnownClass, ProgramEnvironment, Type, UnionType};

/// how many templates one pattern may distribute into before the union holes
/// are left standing instead. a hole left standing is read as "any string",
/// which is wider than the union it came from — the alternative is a type
/// expression that quietly builds tens of thousands of arms
const DISTRIBUTION_LIMIT: usize = 256;

/// how long a string may be before [`TemplateLiteralType::matches_str`] stops
/// looking. the matcher is quadratic in the string length, and a literal longer
/// than this is not something a pattern was written to describe
const MATCH_LENGTH_LIMIT: usize = 4096;

/// whether a pattern widens to `str` where the context cannot hold the pattern.
///
/// a pattern written in a type expression is declared and never widens; one
/// inferred from an f-string value widens exactly where a string literal would
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Promotable {
    /// inferred from a value
    Yes,
    /// written in a type expression
    No,
}

impl Promotable {
    const fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// one piece of a template pattern.
#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum TemplatePart<'db> {
    /// fixed text, spelled literally in the pattern
    Text(CompactString),
    /// a hole, standing for `str(x)` over the values of this type
    Hole(Type<'db>),
}

/// what strings a hole can stand for. derived from the hole's type; a type the
/// reading does not model is [`HoleShape::Anything`], which never rejects
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum HoleShape {
    /// any string at all
    Anything,
    /// the decimal rendering of an `int` — what `str(int)` actually produces,
    /// so no leading zeros and no leading `+`
    Int,
    /// exactly one extended grapheme cluster (`Character`)
    Grapheme,
}

impl HoleShape {
    fn of<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, hole: Type<'db>) -> Self {
        let Some(class) = hole.nominal_class(db, env) else {
            return Self::Anything;
        };
        if class.is_known(db, KnownClass::Int) {
            Self::Int
        } else if class.is_known(db, KnownClass::Character) {
            Self::Grapheme
        } else {
            Self::Anything
        }
    }

    /// whether `value` is a string this shape can stand for
    fn admits(self, value: &str) -> bool {
        match self {
            Self::Anything => true,
            Self::Int => is_int_rendering(value),
            Self::Grapheme => is_single_grapheme(value),
        }
    }

    /// whether every string `other` stands for is also one `self` stands for
    fn contains(self, other: Self) -> bool {
        self == other || self == Self::Anything
    }

    /// whether the empty string is one of this shape's strings
    fn admits_empty(self) -> bool {
        self == Self::Anything
    }
}

/// whether `value` is what `str()` produces for some `int`
fn is_int_rendering(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    match digits.as_bytes() {
        [] => false,
        // `str(0)` is the only rendering with a leading zero, and `str(-0)` is `"0"`
        [b'0'] => value.as_bytes()[0] != b'-',
        [b'0', ..] => false,
        _ => digits.bytes().all(|byte| byte.is_ascii_digit()),
    }
}

#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub(crate) struct TemplateLiteralType<'db> {
    /// the pattern, alternating however the source spelled it. the constructor
    /// guarantees at least one [`TemplatePart::Hole`], no empty and no adjacent
    /// [`TemplatePart::Text`]
    #[returns(deref)]
    pub(crate) parts: Box<[TemplatePart<'db>]>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for TemplateLiteralType<'_> {}

impl<'db> TemplateLiteralType<'db> {
    /// build the type a pattern denotes, normalizing it first. the result is a
    /// template only when nothing simpler says the same thing
    pub(crate) fn from_parts(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        parts: Vec<TemplatePart<'db>>,
        promotable: Promotable,
    ) -> Type<'db> {
        // normalization reads a hole's own shape — whether it is a union to
        // distribute, whether it renders to one string — and a type alias
        // answers none of those questions for the type it stands for
        let parts: Vec<TemplatePart<'db>> = parts
            .into_iter()
            .map(|part| match part {
                TemplatePart::Hole(hole) => TemplatePart::Hole(resolved_hole(db, env, hole)),
                text @ TemplatePart::Text(_) => text,
            })
            .collect();

        if parts
            .iter()
            .any(|part| matches!(part, TemplatePart::Hole(Type::Never)))
        {
            return Type::Never;
        }

        let alternatives = distribute(db, env, &parts);
        let arms: Vec<Type<'db>> = alternatives
            .into_iter()
            .map(|alternative| Self::from_folded_parts(db, env, alternative, promotable))
            .collect();

        UnionType::from_elements(db, env, arms)
    }

    /// build the type one distribution branch denotes. every hole here is
    /// already a single type rather than a union
    fn from_folded_parts(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        parts: Vec<TemplatePart<'db>>,
        promotable: Promotable,
    ) -> Type<'db> {
        // a hole that is itself a pattern is that pattern spliced in: the string
        // it stands for is built the same way, one level down. this waits until
        // after distribution, because a hole is only ever a single pattern once
        // the union it may have been written as has been split into branches
        let parts: Vec<TemplatePart<'db>> = parts
            .into_iter()
            .flat_map(|part| match part {
                TemplatePart::Hole(Type::LiteralValue(literal))
                    if let Some(nested) = literal.as_template() =>
                {
                    nested.parts(db).to_vec()
                }
                other => vec![other],
            })
            .collect();

        let mut folded: Vec<TemplatePart<'db>> = Vec::with_capacity(parts.len());
        for part in parts {
            let part = match part {
                TemplatePart::Text(text) => TemplatePart::Text(text),
                TemplatePart::Hole(hole) => match rendered_text(db, env, hole) {
                    Some(text) => TemplatePart::Text(text),
                    None => TemplatePart::Hole(hole),
                },
            };
            match (&part, folded.last_mut()) {
                (TemplatePart::Text(text), Some(TemplatePart::Text(previous))) => {
                    previous.push_str(text);
                }
                (TemplatePart::Text(text), _) if text.is_empty() => {}
                _ => folded.push(part),
            }
        }

        match folded.as_slice() {
            [] => Type::string_literal(db, ""),
            [TemplatePart::Text(text)] => Type::string_literal(db, text.as_str()),
            // a lone hole that is already a set of strings says nothing the hole
            // type does not say by itself
            [TemplatePart::Hole(hole)] if is_string_set(db, env, *hole) => *hole,
            _ => Type::LiteralValue(crate::types::LiteralValueType::new(
                crate::types::LiteralValueTypeKind::Template(Self::new(
                    db,
                    folded.into_boxed_slice(),
                )),
                promotable.is_yes(),
            )),
        }
    }

    /// the type of every hole in this pattern, in source order
    pub(crate) fn holes(self, db: &'db dyn Db) -> impl Iterator<Item = Type<'db>> {
        self.parts(db).iter().filter_map(|part| match part {
            TemplatePart::Hole(hole) => Some(*hole),
            TemplatePart::Text(_) => None,
        })
    }

    /// rebuild this pattern with `map` applied to each hole, renormalizing —
    /// specializing `f"a{T}"` with `T = Literal[1]` is `Literal["a1"]`
    pub(crate) fn map_holes(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        promotable: Promotable,
        mut map: impl FnMut(Type<'db>) -> Type<'db>,
    ) -> Type<'db> {
        let mut mapped = Vec::with_capacity(self.parts(db).len());
        let mut changed = false;
        for part in self.parts(db) {
            mapped.push(match part {
                TemplatePart::Text(text) => TemplatePart::Text(text.clone()),
                TemplatePart::Hole(hole) => {
                    let new = map(*hole);
                    changed |= new != *hole;
                    TemplatePart::Hole(new)
                }
            });
        }
        if changed {
            Self::from_parts(db, env, mapped, promotable)
        } else {
            Type::LiteralValue(crate::types::LiteralValueType::new(
                crate::types::LiteralValueTypeKind::Template(self),
                promotable.is_yes(),
            ))
        }
    }

    /// whether every string this pattern produces is non-empty, which decides
    /// the truthiness of a value it types
    pub(crate) fn is_always_non_empty(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
    ) -> bool {
        self.parts(db).iter().any(|part| match part {
            TemplatePart::Text(text) => !text.is_empty(),
            TemplatePart::Hole(hole) => !HoleShape::of(db, env, *hole).admits_empty(),
        })
    }

    /// whether `value` is one of the strings this pattern produces
    pub(crate) fn matches_str(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        value: &str,
    ) -> bool {
        if value.len() > MATCH_LENGTH_LIMIT {
            return true;
        }
        let parts = self.parts(db);
        let shapes: Vec<HoleShape> = parts
            .iter()
            .map(|part| match part {
                TemplatePart::Text(_) => HoleShape::Anything,
                TemplatePart::Hole(hole) => HoleShape::of(db, env, *hole),
            })
            .collect();
        let mut failed = FxHashSet::default();
        match_from(parts, &shapes, 0, value, 0, &mut failed)
    }

    /// whether every string `other` produces is one `self` produces.
    ///
    /// this is a sufficient test, not a decision procedure: it aligns the two
    /// patterns piece by piece, and a pair it cannot align is reported as
    /// unrelated even where the languages happen to nest
    pub(crate) fn contains(
        self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        other: Self,
    ) -> bool {
        if self == other {
            return true;
        }
        let pattern = atoms(db, env, self);
        let candidate = atoms(db, env, other);
        let mut failed = FxHashSet::default();
        contains_from(&pattern, &candidate, 0, 0, &mut failed)
    }

    /// the fixed text this pattern must start with, if it starts with any
    pub(crate) fn fixed_prefix(self, db: &'db dyn Db) -> Option<&'db str> {
        match self.parts(db).first() {
            Some(TemplatePart::Text(text)) => Some(text.as_str()),
            _ => None,
        }
    }

    /// the fixed text this pattern must end with, if it ends with any
    pub(crate) fn fixed_suffix(self, db: &'db dyn Db) -> Option<&'db str> {
        match self.parts(db).last() {
            Some(TemplatePart::Text(text)) => Some(text.as_str()),
            _ => None,
        }
    }
}

/// the type a hole really stands for.
///
/// a hole may be written as a type alias, and a union written in a type
/// expression keeps its arms' aliases — [`UnionType::from_elements_leave_aliases`]
/// builds it that way so a union displays the names it was written with. so
/// reaching the union that [`distribute`] has to see, or the pattern that gets
/// spliced in, can take more than the one step [`Type::resolve_type_alias`] takes
fn resolved_hole<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    hole: Type<'db>,
) -> Type<'db> {
    resolve_hole_arms(db, env, hole, &mut FxHashSet::default())
}

/// [`resolved_hole`], carrying the unions already being expanded further up.
///
/// `type Cyc = Cyc | "q"` gives a union holding the very alias that produced it,
/// so following the arms of a union is not on its own a descent — without
/// remembering what is already open, that alias expands forever. the unions are
/// dropped again on the way out, so an arm that appears twice in different
/// branches is still expanded in both
fn resolve_hole_arms<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    hole: Type<'db>,
    open: &mut FxHashSet<Type<'db>>,
) -> Type<'db> {
    let resolved = hole.resolve_type_alias(db);
    let Type::Union(union) = resolved else {
        return resolved;
    };
    if !open.insert(resolved) {
        return resolved;
    }
    let arms: Vec<Type<'db>> = union
        .elements(db)
        .iter()
        .map(|arm| resolve_hole_arms(db, env, *arm, open))
        .collect();
    open.remove(&resolved);
    UnionType::from_elements(db, env, arms)
}

/// whether a hole type is itself a set of strings, so a pattern that is nothing
/// but that hole is that type
fn is_string_set<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, hole: Type<'db>) -> bool {
    if hole.is_literal_string() {
        return true;
    }
    hole.nominal_class(db, env).is_some_and(|class| {
        class.is_known(db, KnownClass::Str) || class.is_known(db, KnownClass::Character)
    })
}

/// the one string a hole type renders to, when it renders to exactly one.
///
/// `Type::str` already answers this for every literal value python can spell —
/// `Literal[5]` renders to `Literal["5"]` — so only `None`, whose `str` falls
/// back to the `str` instance, is read here directly
fn rendered_text<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    hole: Type<'db>,
) -> Option<CompactString> {
    if hole.is_none(db) {
        return Some(CompactString::const_new("None"));
    }
    hole.str(db, env)
        .as_string_literal()
        .map(|literal| CompactString::new(literal.value(db)))
}

/// expand every union hole into its members, one branch per combination.
///
/// returns the pattern unexpanded when the combinations would exceed
/// [`DISTRIBUTION_LIMIT`]
fn distribute<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    parts: &[TemplatePart<'db>],
) -> Vec<Vec<TemplatePart<'db>>> {
    let combinations = parts
        .iter()
        .map(|part| match part {
            TemplatePart::Hole(Type::Union(union)) => union.elements(db).len(),
            _ => 1,
        })
        .try_fold(1usize, usize::checked_mul)
        .unwrap_or(usize::MAX);
    if combinations > DISTRIBUTION_LIMIT {
        return vec![parts.to_vec()];
    }

    let mut branches: Vec<Vec<TemplatePart<'db>>> = vec![Vec::with_capacity(parts.len())];
    for part in parts {
        let arms: Vec<TemplatePart<'db>> = match part {
            TemplatePart::Hole(Type::Union(union)) => union
                .elements(db)
                .iter()
                .map(|element| TemplatePart::Hole(*element))
                .collect(),
            // `bool` is two strings, and its instance type is the only way to
            // spell them without spelling the literals
            TemplatePart::Hole(hole)
                if hole
                    .nominal_class(db, env)
                    .is_some_and(|class| class.is_known(db, KnownClass::Bool)) =>
            {
                vec![
                    TemplatePart::Hole(Type::bool_literal(true)),
                    TemplatePart::Hole(Type::bool_literal(false)),
                ]
            }
            other => vec![other.clone()],
        };
        branches = branches
            .into_iter()
            .flat_map(|branch| {
                arms.iter().map(move |arm| {
                    let mut extended = branch.clone();
                    extended.push(arm.clone());
                    extended
                })
            })
            .collect();
    }
    branches
}

/// whether `value[offset..]` is produced by `parts[part..]`.
///
/// backtracking with a memo of the (part, offset) states already known to fail,
/// which bounds the search at one visit per state. offsets are absolute so that
/// two paths reaching the same state share the memo
fn match_from(
    parts: &[TemplatePart<'_>],
    shapes: &[HoleShape],
    part: usize,
    value: &str,
    offset: usize,
    failed: &mut FxHashSet<(usize, usize)>,
) -> bool {
    let rest = &value[offset..];
    let Some(current) = parts.get(part) else {
        return rest.is_empty();
    };
    if failed.contains(&(part, offset)) {
        return false;
    }

    let matched = match current {
        TemplatePart::Text(text) => {
            rest.starts_with(text.as_str())
                && match_from(parts, shapes, part + 1, value, offset + text.len(), failed)
        }
        TemplatePart::Hole(_) => {
            let shape = shapes[part];
            // the last part of a pattern has nothing left to split against
            if part + 1 == parts.len() {
                shape.admits(rest)
            } else {
                rest.char_indices()
                    .map(|(index, _)| index)
                    .chain(std::iter::once(rest.len()))
                    .any(|split| {
                        shape.admits(&rest[..split])
                            && match_from(parts, shapes, part + 1, value, offset + split, failed)
                    })
            }
        }
    };

    if !matched {
        failed.insert((part, offset));
    }
    matched
}

/// one indivisible piece of a pattern, for aligning two patterns against each
/// other: the fixed text is spread into characters so a hole on the other side
/// can absorb part of it
#[derive(Copy, Clone, PartialEq, Eq)]
enum Atom {
    Char(char),
    Hole(HoleShape),
}

fn atoms<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    template: TemplateLiteralType<'db>,
) -> Vec<Atom> {
    template
        .parts(db)
        .iter()
        .flat_map(|part| match part {
            TemplatePart::Text(text) => text.chars().map(Atom::Char).collect::<Vec<_>>(),
            TemplatePart::Hole(hole) => vec![Atom::Hole(HoleShape::of(db, env, *hole))],
        })
        .collect()
}

/// whether every string `candidate[c..]` produces is one `pattern[p..]` produces.
///
/// each step either matches one atom against one atom, or lets a `Anything` hole
/// in `pattern` absorb one more atom of `candidate`. a narrower hole in
/// `pattern` may take a run of `candidate` characters only when that whole run
/// is a string the hole stands for, so a run that would need the hole to also
/// swallow a `candidate` hole is left unaligned
fn contains_from(
    pattern: &[Atom],
    candidate: &[Atom],
    p: usize,
    c: usize,
    failed: &mut FxHashSet<(usize, usize)>,
) -> bool {
    if c == candidate.len() {
        return pattern[p..]
            .iter()
            .all(|atom| matches!(atom, Atom::Hole(shape) if shape.admits_empty()));
    }
    if p == pattern.len() || failed.contains(&(p, c)) {
        return false;
    }

    let matched = match (pattern[p], candidate[c]) {
        (Atom::Char(left), Atom::Char(right)) => {
            left == right && contains_from(pattern, candidate, p + 1, c + 1, failed)
        }
        (Atom::Hole(HoleShape::Anything), _) => {
            // absorb this atom, or close the hole here and try the next one
            contains_from(pattern, candidate, p, c + 1, failed)
                || contains_from(pattern, candidate, p + 1, c, failed)
        }
        (Atom::Hole(shape), Atom::Hole(other)) => {
            shape.contains(other) && contains_from(pattern, candidate, p + 1, c + 1, failed)
        }
        (Atom::Hole(shape), Atom::Char(_)) => {
            // the hole must take a complete rendering, and can only verify one
            // built entirely out of characters
            let mut run = String::new();
            candidate[c..]
                .iter()
                .take_while(|atom| matches!(atom, Atom::Char(_)))
                .enumerate()
                .any(|(offset, atom)| {
                    let Atom::Char(character) = atom else {
                        return false;
                    };
                    run.push(*character);
                    shape.admits(&run)
                        && contains_from(pattern, candidate, p + 1, c + offset + 1, failed)
                })
        }
        (Atom::Char(_), Atom::Hole(_)) => false,
    };

    if !matched {
        failed.insert((p, c));
    }
    matched
}

/// basedpython: the finite set of strings `ty` denotes, when it denotes one.
///
/// the transpiler asks this of an f-string in a type position. a pattern that
/// folded to literal strings keeps that precision in the emitted python; a
/// pattern that is still a pattern has no python spelling and widens to `str`
pub fn finite_string_set<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Vec<String>> {
    let arms: Vec<Type<'db>> = match ty {
        Type::Union(union) => union.elements(db).to_vec(),
        other => vec![other],
    };
    arms.into_iter()
        .map(|arm| {
            arm.as_string_literal()
                .map(|literal| literal.value(db).to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{HoleShape, is_int_rendering};

    #[test]
    fn int_renderings() {
        for value in ["0", "5", "-5", "1234567890123456789012345"] {
            assert!(is_int_rendering(value), "{value} should be an int");
        }
        for value in ["", "-", "-0", "05", "+5", "5.0", "5a", " 5", "1_0"] {
            assert!(!is_int_rendering(value), "{value} should not be an int");
        }
    }

    #[test]
    fn shape_containment() {
        assert!(HoleShape::Anything.contains(HoleShape::Int));
        assert!(HoleShape::Anything.contains(HoleShape::Grapheme));
        assert!(!HoleShape::Int.contains(HoleShape::Anything));
        assert!(!HoleShape::Int.contains(HoleShape::Grapheme));
        assert!(HoleShape::Int.contains(HoleShape::Int));
    }
}
