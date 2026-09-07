//! How a command line finds a server that is already holding this project.
//!
//! A server is started by an editor, over stdin and stdout, and nothing about that says
//! where it is or that it exists. So a listening server leaves a small record of itself in
//! a per-user directory, and a command line reads the directory.
//!
//! The record is the capability: it carries the port *and* the token, so being able to
//! reach a server means being able to read a file that only this user can read.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use std::io::Write;
use std::path::{Path, PathBuf};

use ruff_db::system::{System, SystemPath, SystemPathBuf};

use super::protocol::Build;

/// What one listening server publishes about itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    pub protocol: u32,
    pub build: Build,
    pub port: u16,
    pub token: String,

    /// The workspace roots this server was started for.
    ///
    /// A caller keeps a record whose roots contain the project it resolved. That is a
    /// filter, not a promise — whether a database is actually rooted at that project is
    /// something only the server can answer, and it does.
    pub roots: Vec<SystemPathBuf>,
}

/// Where records live for this user, or `None` where this platform has no home to put them.
///
/// Under ty's own cache directory rather than a directory of this module's choosing, so that
/// there is one answer to "where does ty keep per-user state" and it is the same answer on
/// every platform.
///
/// Callers pass the directory in rather than reaching for this, so that a server started for
/// a test publishes somewhere a real `by check` will never look.
pub fn default_directory(system: &dyn System) -> Option<SystemPathBuf> {
    Some(system.cache_dir()?.join("project-servers"))
}

/// A record on disk, removed when the server that wrote it lets go of this.
#[derive(Debug)]
pub(crate) struct Registration {
    path: PathBuf,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::debug!(
                "Failed to remove the project server record `{}`: {error}",
                self.path.display()
            );
        }
    }
}

/// Publishes `record`, replacing whatever this process published before.
///
/// Keyed by process id, so a server that died without cleaning up is overwritten by
/// whatever the operating system next gives that number to, and in the meantime is
/// recognised as dead by [`live_records`] failing to connect to it.
pub(crate) fn publish(directory: &SystemPath, record: &Record) -> std::io::Result<Registration> {
    let directory = directory.as_std_path();
    std::fs::create_dir_all(directory)?;

    let path = directory.join(format!("{}.json", std::process::id()));
    let mut file = std::fs::File::create(&path)?;
    restrict_to_owner(&file)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.flush()?;

    Ok(Registration { path })
}

/// The token is the whole of the access control, so nobody else may read the file.
#[cfg(unix)]
fn restrict_to_owner(file: &std::fs::File) -> std::io::Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

/// Windows has no mode bits; the containing directory is already per-user.
#[cfg(not(unix))]
fn restrict_to_owner(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// A published record, and the file it was published in.
///
/// The path travels with it so that a caller who finds nothing listening can take the record
/// away — see `forget`.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub record: Record,
    path: PathBuf,
}

/// Removes a record whose server is not there any more.
///
/// A server takes its own record away as it exits, but a server that was killed never gets
/// to. Nothing else would ever remove those, and each one costs every later `by check` a
/// connection attempt.
///
/// The file is re-read first. Records are named by process id, so between reading one and
/// failing to reach it a new server can have been given that id and published over it —
/// deleting that one would leave a live server undiscoverable for the rest of its life.
pub(super) fn forget(candidate: &Candidate) {
    match read_record(&candidate.path) {
        Some(current) if current.port != candidate.record.port => {
            tracing::debug!(
                "Leaving `{}` alone: another server has published over it",
                candidate.path.display()
            );
            return;
        }
        _ => {}
    }

    if let Err(error) = std::fs::remove_file(&candidate.path) {
        tracing::debug!(
            "Failed to remove the stale project server record `{}`: {error}",
            candidate.path.display()
        );
    }
}

/// Every published record whose roots contain `project_root`, nearest root first.
///
/// Both `project_root` and each record's roots go through `super::canonical` before they are
/// compared: the two reach the same tree from different processes, and neither necessarily by
/// the same name.
///
/// Nearest first because a workspace opened at a repository root and one opened at the
/// package inside it are both candidates, and the inner one is the one whose settings were
/// resolved for this project.
pub fn candidates(directory: &SystemPath, project_root: &SystemPath) -> Vec<Candidate> {
    let Ok(entries) = std::fs::read_dir(directory.as_std_path()) else {
        return Vec::new();
    };

    let project_root = super::canonical(project_root);
    let mut candidates: Vec<(usize, Candidate)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter_map(|path| Some((read_record(&path)?, path)))
        .filter_map(|(record, path)| {
            let depth = record
                .roots
                .iter()
                // both sides, because a root reaches this file from whichever process wrote
                // it and a project root reaches this function from whichever asked — see
                // [`super::canonical`]
                .map(|root| super::canonical(root))
                .filter(|root| project_root.starts_with(root))
                .map(|root| root.components().count())
                .max()?;
            Some((depth, Candidate { record, path }))
        })
        .collect();

    candidates.sort_by(|(left, _), (right, _)| right.cmp(left));
    candidates
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect()
}

fn read_record(path: &Path) -> Option<Record> {
    if path.extension()? != "json" {
        return None;
    }
    let contents = std::fs::read(path).ok()?;
    match serde_json::from_slice(&contents) {
        Ok(record) => Some(record),
        // a record written by a build that has since changed the shape. it is not this
        // process's to delete — the server that wrote it still owns the file, and will
        // remove it when it exits
        Err(error) => {
            tracing::debug!(
                "Ignoring unreadable project server record `{}`: {error}",
                path.display()
            );
            None
        }
    }
}
