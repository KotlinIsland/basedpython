//! the base a `protocol` lowers onto
//!
//! a protocol is a base made for the checker: a value is one by its members, which no runtime
//! test sees. so by default it lowers onto the runtime's own `_ByProtocol`, which marks the
//! class a protocol and records its members, has no metaclass to conflict with any other
//! base's, and does nothing else. another checker reading the python doesn't know that base, so
//! `lowering.compatibility` lowers onto `typing.Protocol`, which every checker reads. a stub is
//! only ever read by a checker, so it always does. the checker models the class from the same
//! setting, `AnalysisSettings::compatibility`, which `transpile_typed` reads for each file

use super::repeated_underscore::WrittenNames;
use crate::config::Config;
use crate::runtime::{self, Helper};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolBase {
    /// `typing.Protocol`
    Typing,
    /// the runtime's `_ByProtocol`
    Runtime,
}

impl ProtocolBase {
    pub(crate) fn of(config: &Config) -> Self {
        if config.compatibility || config.is_stub {
            Self::Typing
        } else {
            Self::Runtime
        }
    }

    /// how the emitted python names the base
    pub(crate) fn name(self, written: WrittenNames) -> String {
        match self {
            Self::Typing => written.imported("typing", "Protocol"),
            Self::Runtime => runtime::PROTOCOL.name().to_owned(),
        }
    }

    /// the runtime helper the base is, if it is one. `typing.Protocol` is imported instead
    pub(crate) fn helper(self) -> Option<Helper> {
        match self {
            Self::Typing => None,
            Self::Runtime => Some(runtime::PROTOCOL),
        }
    }
}
