//! The values a `build:` block's stamps take.
//!
//! Discovery lives here, at the command layer, and never in the transpiler. The
//! emitted python has to be a function of the source and the transpile config;
//! a pipeline that asked git for the commit itself would answer differently on
//! every run, and a re-stage — which re-transpiles one file into a tree an
//! earlier build wrote — would put a module claiming one commit beside modules
//! claiming another.
//!
//! So a command settles the stamps once, hands them to the transpiler as
//! config, and the build record writes them down so a later re-stage reproduces
//! them exactly.
//!
//! Nothing here guesses. A project outside a git checkout, or on a machine with
//! no `git`, gets no git stamps at all — and a block that declared one with no
//! default then fails to transpile, which is the promise the declaration made.
//! Inventing `"unknown"` would let a build claim a commit it does not have.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::Context;

/// `SOURCE_DATE_EPOCH` — the ecosystem's agreement on what "now" is for a build
/// that has to come out the same twice.
///
/// Honouring it is what lets a project with a `BUILT_AT` stamp still produce a
/// reproducible wheel; without it the artifact differs on every build and
/// defeats both caching and anyone trying to verify it was built from the
/// source it claims.
const SOURCE_DATE_EPOCH: &str = "SOURCE_DATE_EPOCH";

/// How one release hands its settled stamps to the builds inside it.
///
/// `by build --wheels` is not one build: it calls the packaging frontend once
/// for the source distribution and once per python version, and each of those
/// reaches a fresh `by build` that would settle its own stamps. Two wheels of
/// one release stamped a second apart — or, if something landed mid-release,
/// two different commits — are not one artifact set, so the outer command
/// settles the stamps once and passes them down through here.
pub(crate) const SETTLED_STAMPS: &str = "BY_BUILD_STAMPS";

/// Parse the `--stamp NAME=VALUE` arguments into the map the transpiler takes.
///
/// A value may contain `=`; only the first splits the pair, so
/// `--stamp DESCRIBE=v1.2-3-gabc=def` is one stamp.
pub(crate) fn parse_explicit(arguments: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut stamps = BTreeMap::new();
    for argument in arguments {
        let (name, value) = argument.split_once('=').with_context(|| {
            format!("`--stamp {argument}` is not a `NAME=VALUE` pair — a stamp needs both")
        })?;
        if name.is_empty() {
            anyhow::bail!("`--stamp {argument}` names no stamp");
        }
        if let Some(replaced) = stamps.insert(name.to_owned(), value.to_owned()) {
            anyhow::bail!(
                "`--stamp {name}` was given twice, as {replaced:?} and {value:?} — which one the \
                 build meant is not something to guess at"
            );
        }
    }
    Ok(stamps)
}

/// Fill in the stamps this build can work out for itself, leaving anything
/// already in `stamps` alone.
///
/// An explicit `--stamp` wins, because a CI job that knows the commit it was
/// dispatched for knows it better than a checkout that may be a detached head
/// or a shallow clone.
///
/// `target_version` is the python the output is being lowered to, and `None`
/// says this command is not lowering for one — `by build --wheels` settles the
/// stamps its wheels share, and each wheel is lowered to a different python, so
/// `PYTHON_VERSION` is the one stamp the release must leave to them.
pub(crate) fn fill_discovered(
    stamps: &mut BTreeMap<String, String>,
    project_root: &Path,
    target_version: Option<&str>,
) {
    let mut supply = |name: &str, value: Option<String>| {
        if let Some(value) = value
            && !stamps.contains_key(name)
        {
            stamps.insert(name.to_owned(), value);
        }
    };

    // a release that already settled its stamps says so, and this build is one
    // of the several inside it rather than one of its own
    if let Some(settled) = settled_by_the_release() {
        for (name, value) in settled {
            supply(&name, Some(value));
        }
    }

    let head = git(project_root, &["rev-parse", "HEAD"]);
    let head_found = head.is_some();
    supply("GIT_SHA_SHORT", head.as_ref().map(|sha| short(sha)));
    supply("GIT_SHA", head);
    supply(
        "GIT_BRANCH",
        git(project_root, &["rev-parse", "--abbrev-ref", "HEAD"]),
    );
    // `--exact-match` so this is the tag *of this commit* and not the nearest
    // one behind it: a stamp saying `v1.2.0` on three commits past the tag is a
    // release claim nobody made.
    //
    // an untagged commit is the ordinary case rather than a failure to discover
    // anything, so inside a repository the answer is the empty string. `describe`
    // exits non-zero either way, which is why this asks whether there was a
    // repository at all rather than reading its status
    if head_found {
        supply(
            "GIT_TAG",
            Some(git(project_root, &["describe", "--tags", "--exact-match"]).unwrap_or_default()),
        );
    }
    // asked separately from the sha, and reported rather than folded into it: a
    // build from a tree with uncommitted changes is not the commit it names, and
    // a program that cannot say so will eventually be asked to explain a stack
    // trace that does not match its source
    supply(
        "GIT_DIRTY",
        git(project_root, &["status", "--porcelain"])
            .map(|changes| if changes.is_empty() { "false" } else { "true" }.to_owned()),
    );
    supply("BUILT_AT", Some(built_at()));
    supply("PYTHON_VERSION", target_version.map(str::to_owned));
}

/// The stamps an enclosing release settled, when this build is one of several
/// inside one.
///
/// Unreadable content is ignored rather than refused: the variable is only ever
/// written by the outer command, so anything else in it belongs to whatever else
/// set it, and a build that stopped over it would be refusing to run for a reason
/// nobody could act on.
fn settled_by_the_release() -> Option<BTreeMap<String, String>> {
    serde_json::from_str(&std::env::var(SETTLED_STAMPS).ok()?).ok()
}

/// The first twelve characters of a commit hash — long enough to be unambiguous
/// in any real repository, short enough to read in a `--version` line.
fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// The moment the build happened, as an RFC 3339 timestamp in UTC.
///
/// `SOURCE_DATE_EPOCH` replaces it when set, so a reproducible build stays
/// reproducible.
fn built_at() -> String {
    built_at_from(std::env::var(SOURCE_DATE_EPOCH).ok().as_deref())
}

/// [`built_at`] against a given `SOURCE_DATE_EPOCH`, so the reading of it can be
/// tested without reaching into the process environment.
///
/// A value that is not a number of seconds is ignored rather than refused: the
/// variable belongs to whatever invoked the build, and a build that stopped
/// because something upstream set it oddly would be harder to explain than one
/// that stamped the clock.
fn built_at_from(epoch: Option<&str>) -> String {
    if let Some(epoch) = epoch
        && let Ok(seconds) = epoch.trim().parse::<i64>()
        && let Ok(timestamp) = jiff::Timestamp::from_second(seconds)
    {
        return timestamp.to_string();
    }
    jiff::Timestamp::now().to_string()
}

/// Run `git` in the project and return its trimmed output, or `None` for any
/// reason at all it did not answer: no git on the machine, no repository, a
/// repository with no commits, a `describe` that found no tag.
///
/// Each of those is a real situation a build runs in — a source distribution
/// unpacked from a package index has no `.git` at all — and none of them is an error here.
/// The declaration is what decides whether a missing stamp is fatal.
fn git(project_root: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_stamps_round_trip_through_the_variable() {
        // what one release writes, a build inside it has to be able to read —
        // values with `=`, quotes and newlines in them included
        let mut settled = BTreeMap::new();
        settled.insert("GIT_SHA".to_owned(), "abc123".to_owned());
        settled.insert("SUBJECT".to_owned(), "fix \"one\"=two\nand more".to_owned());
        let rendered = serde_json::to_string(&settled).unwrap();
        let read: BTreeMap<String, String> = serde_json::from_str(&rendered).unwrap();
        assert_eq!(read, settled);
    }

    #[test]
    fn a_pair_splits_on_its_first_equals() {
        let stamps = parse_explicit(&["DESCRIBE=v1.2-3-gabc=def".to_owned()]).unwrap();
        assert_eq!(stamps["DESCRIBE"], "v1.2-3-gabc=def");
    }

    #[test]
    fn a_stamp_without_a_value_is_refused() {
        let error = parse_explicit(&["GIT_SHA".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("is not a `NAME=VALUE` pair"));
    }

    #[test]
    fn the_same_stamp_twice_is_refused() {
        let error = parse_explicit(&["A=1".to_owned(), "A=2".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("was given twice"));
    }

    #[test]
    fn an_explicit_stamp_is_not_overwritten_by_discovery() {
        let mut stamps = parse_explicit(&["GIT_SHA=deadbeef".to_owned()]).unwrap();
        fill_discovered(&mut stamps, Path::new("."), Some("3.13"));
        assert_eq!(stamps["GIT_SHA"], "deadbeef");
    }

    #[test]
    fn discovery_supplies_the_target_python() {
        let mut stamps = BTreeMap::new();
        fill_discovered(&mut stamps, Path::new("."), Some("3.13"));
        assert_eq!(stamps["PYTHON_VERSION"], "3.13");
    }

    /// A release settles what its wheels share. Each wheel is lowered to a
    /// different python, so that one stamp is not the release's to settle.
    #[test]
    fn a_release_leaves_the_target_python_to_its_wheels() {
        let mut stamps = BTreeMap::new();
        fill_discovered(&mut stamps, Path::new("."), None);
        assert!(!stamps.contains_key("PYTHON_VERSION"));
        assert!(stamps.contains_key("BUILT_AT"));
    }

    #[test]
    fn source_date_epoch_replaces_the_clock() {
        // the epoch itself, and a moment well after it
        assert_eq!(built_at_from(Some("0")), "1970-01-01T00:00:00Z");
        assert_eq!(built_at_from(Some(" 1750000000 ")), "2025-06-15T15:06:40Z");
    }

    #[test]
    fn a_source_date_epoch_that_is_not_a_time_is_ignored() {
        // the variable belongs to whatever ran the build; a build that stopped
        // over it would be harder to explain than one that stamped the clock
        for odd in ["", "yesterday", "1e9", "999999999999999999999"] {
            assert_ne!(built_at_from(Some(odd)), "1970-01-01T00:00:00Z", "{odd:?}");
        }
    }

    /// Ruff's own checkout is a repository with no tag on `HEAD` most of the
    /// time, and an untagged commit is an ordinary state rather than a discovery
    /// that failed — so the stamp is there and empty, not absent.
    #[test]
    fn an_untagged_commit_in_a_repository_still_has_a_tag_stamp() {
        let mut stamps = BTreeMap::new();
        fill_discovered(&mut stamps, Path::new("."), Some("3.13"));
        // only meaningful where there *is* a repository to be in
        if stamps.contains_key("GIT_SHA") {
            assert!(stamps.contains_key("GIT_TAG"));
        }
    }

    #[test]
    fn a_directory_that_is_no_repository_gets_no_git_stamps() {
        let outside = tempfile::TempDir::new().unwrap();
        let mut stamps = BTreeMap::new();
        fill_discovered(&mut stamps, outside.path(), Some("3.13"));
        // `git` walks upward, so the only thing that reliably holds here is that
        // nothing was invented: whatever it did or did not find, no stamp says
        // "unknown"
        assert!(stamps.values().all(|value| value != "unknown"));
        assert!(stamps.contains_key("BUILT_AT"));
    }
}
