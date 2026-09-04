//! the preset that supplies the defaults every other type checking setting layers onto

use crate::lint::{Level, LintMetadata, TyCompat};
use std::fmt::{self, Display, Formatter};
use ty_combine::Combine;

/// the set of defaults a project's rules and analysis settings start from
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, get_size2::GetSize)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "kebab-case")
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum TypeCheckingPreset {
    /// # Strict
    ///
    /// Every diagnostic is enabled, and every analysis option that buys soundness is on.
    #[default]
    Strict,

    /// # Ty compatible
    ///
    /// The defaults of [ty](https://github.com/astral-sh/ty), which basedpython is built on.
    /// basedpython's own diagnostics and analysis options are off, so that a project reports
    /// what ty itself would report.
    TyCompatible,
}

impl TypeCheckingPreset {
    pub(crate) const fn is_strict(self) -> bool {
        matches!(self, Self::Strict)
    }

    pub const fn is_ty_compatible(self) -> bool {
        matches!(self, Self::TyCompatible)
    }

    /// whether `lint` exists at all under this preset
    ///
    /// a lint that doesn't exist can't be enabled, not even by `rules = { all = "error" }`
    pub const fn includes(self, lint: &LintMetadata) -> bool {
        match self {
            Self::Strict => true,
            Self::TyCompatible => !matches!(lint.ty_compat, TyCompat::BasedPython),
        }
    }

    /// the level `lint` runs at under this preset, before any `rules` configuration
    pub(crate) const fn level(self, lint: &LintMetadata) -> Level {
        match self {
            Self::Strict => lint.default_level,
            Self::TyCompatible => match lint.ty_compat {
                TyCompat::Same => lint.default_level,
                TyCompat::Level(level) => level,
                TyCompat::BasedPython => Level::Ignore,
            },
        }
    }
}

impl Display for TypeCheckingPreset {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Strict => f.write_str("strict"),
            Self::TyCompatible => f.write_str("ty-compatible"),
        }
    }
}

impl Combine for TypeCheckingPreset {
    fn combine_with(&mut self, _other: Self) {}

    fn combine(self, _other: Self) -> Self {
        self
    }
}
