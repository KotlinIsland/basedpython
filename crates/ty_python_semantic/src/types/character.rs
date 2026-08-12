//! `Character`-type queries for the basedpython transpiler
//!
//! the grapheme string surface lowers an annotated assignment `x: Character = …`
//! into a real `Character(…)` construction so the runtime value's class is
//! `Character`, not a plain `str`. that lowering asks two questions, answered
//! here so the transpiler need not reach into ty's `pub(crate)` internals:
//!
//! - does an annotation denote exactly the `Character` type? ([`denotes_character`])
//! - is a value already a `Character` instance (so wrapping it again would be
//!   redundant)? ([`is_character_instance`])

use unicode_segmentation::UnicodeSegmentation;

use crate::Db;
use crate::types::ProgramEnvironment;
use crate::types::{KnownClass, Type};

/// whether `value` is a single `Character` — exactly one extended grapheme
/// cluster (UAX #29). counts graphemes, not code points, so a multi-scalar
/// cluster like the US flag `"\u{1F1FA}\u{1F1F8}"` (two code points) is one
/// `Character`
pub(crate) fn is_single_grapheme(value: &str) -> bool {
    let mut graphemes = value.graphemes(true);
    graphemes.next().is_some() && graphemes.next().is_none()
}

/// whether `ty` denotes the `Character` type — its instance type (the meaning
/// of a bare `Character` in an annotation position) or the class literal
/// `type[Character]` (its meaning in a value position). a union, optional, or
/// `str` does not qualify
pub fn denotes_character<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    is_character_instance(db, env, ty)
        || ty
            .to_class_type(db)
            .is_some_and(|class| class.is_known(db, KnownClass::Character))
}

/// whether `ty` is a `Character` instance — its class is exactly `Character`
pub fn is_character_instance<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    matches!(
        ty,
        Type::NominalInstance(instance) if instance.class(db, env).is_known(db, KnownClass::Character)
    )
}
