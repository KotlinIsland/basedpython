//! Answering `by` command lines out of a server that is already holding the project.
//!
//! A `by check` builds a project database, resolves an environment, parses every file and
//! infers every one of them, and then exits and throws all of it away. An editor with the
//! language server running has that same work already done and kept up to date. This is the
//! way across: a loopback socket the server announces in a per-user
//! [record](discovery::Record), a [request](protocol::Request) that names the project and
//! the configuration the caller resolved, and an answer rendered out of the warm database.
//!
//! Two rules shape everything here.
//!
//! The hot answer has to be the cold answer. A server checks what the editor is holding,
//! under settings the editor contributed to, in a build that may be months older than the
//! `by` on the `PATH` — so the answer only crosses when none of that is true, and every
//! other case is a [refusal](protocol::Refusal) the caller silently checks past. A refusal
//! costs the time it would have cost anyway. A wrong answer costs trust in the command.
//!
//! And it is never the only way to get an answer. Nothing here is required to work: no
//! server, a stale record, a refused connection and a garbled reply all lead to the same
//! place, which is the check the caller would have run if none of this existed.

use std::fmt::Write;

use rand::TryRng;
use ruff_db::system::{System, SystemPath, SystemPathBuf};
use ty_static::EnvVars;

mod check;
pub mod client;
pub mod discovery;
mod listener;
pub mod protocol;

pub use check::environment;
pub(crate) use listener::{Incoming, Listener, respond};

/// Whether [`EnvVars::BY_NO_PROJECT_SERVER`] has switched the project server off.
///
/// One switch for both halves, because a user who does not want a command line reaching into
/// their editor's process does not want the port open either.
///
/// Read through `system` rather than the process environment, which is what the rest of the
/// server does: tests run concurrently in one process, so a test that wanted to exercise this
/// could not set the variable without setting it for every other test at the same time.
pub fn disabled(system: &dyn System) -> bool {
    system
        .env_var(EnvVars::BY_NO_PROJECT_SERVER)
        .is_ok_and(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "n" | "off"
            )
        })
}

/// `path` with every symlink resolved, or `path` itself where it cannot be.
///
/// The two sides reach the same project by different names. A `by check` takes its project
/// root from the working directory, which the operating system hands over already resolved;
/// an editor sends whatever it was opened with. On macOS that alone is enough to disagree —
/// a temporary directory is `/var/…` to one and `/private/var/…` to the other — and a user
/// whose project is behind a symlink disagrees everywhere.
///
/// So both roots are put through this before either is compared to the other, and a path
/// that cannot be resolved at all is left alone rather than dropped: it still compares equal
/// to itself, which is the case where both sides spell it the same way.
fn canonical(path: &SystemPath) -> SystemPathBuf {
    // Two things make a resolved path stop comparing against an unresolved one. Resolving
    // only succeeds for a path that exists, so a directory and a file under it that has not
    // been written yet would come back spelled differently; and on Windows the answer is a
    // verbatim `\\?\C:\…` path, which no path the editor or the caller sends is spelled
    // like. So the longest ancestor that does exist is resolved, the rest is put back on,
    // and the verbatim prefix is dropped the way `System::canonicalize_path` drops it —
    // everything under one root then comes back in one spelling, created or not.
    let mut unresolved = Vec::new();
    let mut current = path;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(current.as_std_path())
            && let Ok(resolved) = SystemPathBuf::from_path_buf(resolved)
        {
            let mut resolved = resolved.simplified().to_path_buf();
            for component in unresolved.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
            return path.to_path_buf();
        };
        unresolved.push(name);
        current = parent;
    }
}

/// A fresh secret for one server's lifetime.
///
/// The listener is on loopback, which every user on this machine can reach; the token is
/// what makes reaching it depend on being able to read a file that only this user can.
fn token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| anyhow::anyhow!("failed to read random bytes: {error}"))?;

    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(token, "{byte:02x}")?;
    }
    Ok(token)
}

/// What a pass over a request produced.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Nothing stands in the way, and the file system has not been re-read yet.
    ///
    /// The re-read has to happen on the main loop, because it mutates the session — so a
    /// request that gets this far goes back there and comes round once more.
    NeedsRescan,

    Answered(protocol::Response),
}

/// Runs a request against `snapshot`.
///
/// `rescanned` says whether the file system has already been re-read for this request. Until
/// it has, the most this can do is agree that the request is answerable.
///
/// A panic in the checker is caught here rather than left to the request-handling hook,
/// because the caller on the other end of the socket is waiting for one of two shapes and a
/// server that says nothing at all leaves it there until its own timeout.
pub(crate) fn run(
    snapshot: &crate::session::SessionSnapshot,
    payload: &protocol::Payload,
    rescanned: bool,
) -> Outcome {
    // nothing borrowed here is read again after an unwind: the outcome is built from scratch
    // on either path, so asserting unwind safety costs nothing
    let checked = ruff_db::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match payload {
        protocol::Payload::Check(request) => check::run(snapshot, request, rescanned),
    }));

    match checked {
        Ok(outcome) => outcome,
        Err(panic) => {
            tracing::error!("A command-line request panicked: {panic}");
            Outcome::Answered(protocol::Response::Refused {
                reason: protocol::Refusal::Panicked,
            })
        }
    }
}

/// Whether a fresh snapshot could turn `response` into an answer.
///
/// A check runs against a snapshot, and typing in the editor cancels it. Retrying means
/// going back through the main loop for a snapshot of what the session is now.
pub(crate) fn is_retryable(response: &protocol::Response) -> bool {
    matches!(
        response,
        protocol::Response::Refused {
            reason: protocol::Refusal::Cancelled
        }
    )
}

#[cfg(test)]
mod tests {
    use super::canonical;
    use ruff_db::system::SystemPath;
    use tempfile::TempDir;

    /// The two sides compare a root against a path under it, and a path is only ever
    /// compared to another that went through [`canonical`] too. What breaks that is one
    /// spelling for a path that exists and another for one that does not: a temporary
    /// directory reached through a symlink resolves elsewhere, and on Windows a resolved
    /// path carries a verbatim prefix an unresolved one never has.
    #[test]
    fn a_path_that_does_not_exist_yet_is_spelled_like_the_root_it_is_under() {
        let directory = TempDir::new().expect("a temporary directory");
        let root = SystemPath::from_std_path(directory.path()).expect("a UTF-8 path");

        let canonical_root = canonical(root);
        let nested = canonical(&root.join("not-written-yet"));

        assert!(
            nested.starts_with(&canonical_root),
            "`{nested}` should be inside `{canonical_root}`"
        );
        assert_eq!(
            nested.strip_prefix(&canonical_root).map(SystemPath::as_str),
            Ok("not-written-yet")
        );
    }
}
