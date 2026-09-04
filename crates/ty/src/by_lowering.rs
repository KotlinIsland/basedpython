//! The lowering options a release settles for the builds inside it.
//!
//! `by build --wheels` is not one build. It calls the packaging frontend once
//! for the source distribution and once per python version, and each of those
//! reaches a fresh `by build` through the PEP 517 backend. Those inner builds
//! are where the transpiling actually happens, so a lowering option that stops
//! at the outer command changes nothing at all — `--soundness none` would be
//! accepted and then quietly produce wheels full of soundness checks.
//!
//! So the outer command settles the options once and hands them down, the same
//! way and for much the same reason as the stamps in [`crate::by_stamps`]: the
//! wheels of one release have to be one artifact set, and wheels lowered under
//! different options are not that.
//!
//! Only `by build` reads this, because `by build` is the only command the
//! backend runs. These options change what the emitted python *does*, so a
//! variable left in the environment must not quietly re-lower an unrelated
//! `by transpile` that was given options of its own.
//!
//! The two `by` executables involved are not necessarily the same build. A
//! project's `requires` names `basedpython` with a lower bound, so the frontend
//! resolves whatever is newest into the build environment, and the backend
//! prefers that one to whatever is on `PATH`. The outer `by` a user typed can
//! therefore be older than the inner one it hands these to — which is why every
//! field defaults rather than being required. A build that receives options
//! written by a `by` that had not heard of one of them takes the rest and
//! defaults that one, instead of failing to read the message and silently
//! lowering as though a release had settled nothing.
//!
//! The soundness spec crosses as the string the flag takes, so a *newer* outer
//! `by` can name a position an older inner one cannot parse. That fails the
//! build loudly, from inside the frontend, which is worse to read than it needs
//! to be but does not ship a wheel lowered other than as asked.

use serde::{Deserialize, Serialize};

use crate::args::LoweringArgs;

/// How one release hands its settled lowering options to the builds inside it.
pub(crate) const SETTLED_LOWERING: &str = "BY_BUILD_LOWERING";

/// The lowering options themselves, in the shape the command line spells them.
///
/// Deliberately the same field names and senses as [`LoweringArgs`], including
/// the negated `no_unique_loop_bindings`: this is copied from one and applied as
/// the other, and a field that changed sense on the way through would invert a
/// lowering option with nothing to catch it.
///
/// That is also why this is not [`by_stage::record::ConfigRecord`], which
/// carries the same three options for the build record. It spells the loop
/// option positively, renames its fields for its own file format, and refuses
/// unknown ones — all right for a record read back by the same `by` that wrote
/// it, and all wrong for a message crossing between two versions of `by`.
///
/// [`by_stage::record::ConfigRecord`]: by_stage::record::ConfigRecord
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SettledLowering {
    /// Defaulted to the spelling the flag itself defaults to. An empty spec is
    /// a valid one meaning *no checks at all*, so a missing field must not fall
    /// back to `String::default`.
    #[serde(default = "unset_soundness")]
    pub(crate) soundness: String,
    #[serde(default)]
    pub(crate) runtime_raises_checks: bool,
    #[serde(default)]
    pub(crate) no_unique_loop_bindings: bool,
}

/// What `--soundness` means when nothing asked for anything, matching the
/// flag's own `default_value`.
fn unset_soundness() -> String {
    "default".to_owned()
}

impl From<&LoweringArgs> for SettledLowering {
    fn from(arguments: &LoweringArgs) -> Self {
        // destructured rather than read field by field. a lowering option added
        // to the arguments and forgotten here would be accepted by a `--wheels`
        // release and then ignored by every build inside it — which is the bug
        // this whole mechanism exists to close, so it should not be possible to
        // reopen it quietly. written this way, it does not compile
        let LoweringArgs {
            soundness,
            runtime_raises_checks,
            no_unique_loop_bindings,
            // not one of these: a stamp's value is discovered as well as given,
            // so the release settles them itself rather than copying them across
            stamps: _,
        } = arguments;
        Self {
            soundness: soundness.clone(),
            runtime_raises_checks: *runtime_raises_checks,
            no_unique_loop_bindings: *no_unique_loop_bindings,
        }
    }
}

/// The options an enclosing release settled, when this build is one of several
/// inside one.
pub(crate) fn settled_by_the_release() -> Option<SettledLowering> {
    settled_from(std::env::var(SETTLED_LOWERING).ok().as_deref())
}

/// [`settled_by_the_release`] against a given value, so the reading of it can be
/// tested without writing to the process environment.
///
/// Unreadable content is ignored rather than refused, for the same reason the
/// stamps are: the variable is only ever written by the outer command, so
/// anything else in it belongs to whatever else set it, and a build that
/// stopped over it would be refusing to run for a reason nobody could act on.
fn settled_from(raw: Option<&str>) -> Option<SettledLowering> {
    serde_json::from_str(raw?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What one release writes, a build inside it has to be able to read.
    #[test]
    fn settled_options_round_trip_through_the_variable() {
        let settled = SettledLowering {
            soundness: "generic-calls,returns".to_owned(),
            runtime_raises_checks: true,
            no_unique_loop_bindings: true,
        };
        let read = settled_from(Some(&serde_json::to_string(&settled).unwrap()))
            .expect("a release's own message reads back");
        assert_eq!(read.soundness, "generic-calls,returns");
        assert!(read.runtime_raises_checks);
        assert!(read.no_unique_loop_bindings);
    }

    /// The two `by` executables in a release need not be the same build — the
    /// outer one can be older than the inner one it hands these to. An option
    /// the writer had never heard of must cost only itself, because the
    /// alternative is reading nothing and lowering as though no release had
    /// settled anything, which is the bug this mechanism exists to close.
    #[test]
    fn options_written_by_an_older_by_still_read() {
        let read = settled_from(Some(r#"{"soundness":"none"}"#)).expect("the rest still reads");
        assert_eq!(read.soundness, "none");
        assert!(!read.runtime_raises_checks);
        assert!(!read.no_unique_loop_bindings);
    }

    /// An empty spec is a valid one meaning *no checks*, so the absent case has
    /// to land on the flag's own default rather than on `String::default`.
    #[test]
    fn an_unstated_soundness_is_the_default_not_the_empty_spec() {
        let read = settled_from(Some(r#"{"runtime_raises_checks":true}"#)).expect("reads");
        assert_eq!(read.soundness, "default");
        assert!(read.runtime_raises_checks);
    }

    /// Whatever else set the variable, this is not a reason to refuse to build.
    #[test]
    fn content_that_is_not_settled_options_is_ignored() {
        assert!(settled_from(None).is_none());
        assert!(settled_from(Some("not the outer command's")).is_none());
        assert!(settled_from(Some("[1, 2]")).is_none());
    }

    /// The arguments are carried across whole, and the negated one is the one
    /// worth pinning: carried with its sense flipped it would turn the option
    /// into its opposite in every wheel of the release.
    #[test]
    fn the_arguments_are_carried_across_unchanged() {
        let arguments = LoweringArgs {
            soundness: "none".to_owned(),
            runtime_raises_checks: true,
            no_unique_loop_bindings: true,
            ..LoweringArgs::default()
        };
        let settled = SettledLowering::from(&arguments);
        assert_eq!(settled.soundness, "none");
        assert!(settled.runtime_raises_checks);
        assert!(settled.no_unique_loop_bindings);

        let defaults = LoweringArgs::default();
        let settled = SettledLowering::from(&defaults);
        assert!(!settled.runtime_raises_checks);
        assert!(!settled.no_unique_loop_bindings);
    }
}
