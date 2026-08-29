//! The build identity that goes into `_by_build.json`.
//!
//! A build tree records which `by` wrote it, and a later re-stage of one file into
//! that tree has to be able to say whether *this* `by` would have written the same
//! bytes. Nothing about a tree on disk answers that: the transpiler is the answer,
//! and the only handle on the transpiler is which build of it is running.
//!
//! It is computed here rather than read out of `crates/ty/build.rs` because two
//! programs need the same answer. `by run` writes the record and `by server`
//! checks it, and they are separate binaries that a user may well have built at
//! different times. A constant compiled into the crate they share is the only
//! spelling where they cannot drift: whatever produced one produced the other.
//!
//! The identity is the package version, the commit, and whether the worktree was
//! dirty when the crate was compiled. The dirty flag is not decoration — during
//! development every interesting change to the transpiler is an uncommitted one,
//! and an identity that ignored it would let a build tree written before the
//! change be re-staged by the binary written after it, which is exactly the
//! "different code in the case that matters" this whole record exists to prevent.
//! Two binaries built from one dirty tree in one `cargo build` still agree, which
//! is the case that has to keep working.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let crate_root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let workspace_root = crate_root.join("..").join("..");

    let mut identity = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    if let Some(commit) = commit(&workspace_root) {
        identity.push('+');
        identity.push_str(&commit);
    }
    println!("cargo::rustc-env=BY_BUILD_IDENTITY={identity}");
}

/// The commit the worktree is on, with `.dirty` appended when it has
/// uncommitted changes. `None` outside a git checkout, or when git cannot be
/// run — a released binary built from a tarball has no commit, and its package
/// version is the whole of its identity.
fn commit(workspace_root: &Path) -> Option<String> {
    // rebuild when HEAD moves. a dirty worktree cannot be watched this way at
    // all — there is no one file whose change means "the sources moved" — so the
    // dirty flag is only as fresh as the last time cargo decided to rebuild this
    // crate. that is the right trade: touching any source in the workspace
    // rebuilds `by_transforms` and everything above it, this crate included
    let git_dir = watch_git_head(workspace_root);
    git_dir?;

    let output = Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut commit = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if commit.is_empty() {
        return None;
    }

    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(workspace_root)
        .output();
    if let Ok(status) = status
        && status.status.success()
        && !String::from_utf8_lossy(&status.stdout).trim().is_empty()
    {
        commit.push_str(".dirty");
    }
    Some(commit)
}

/// Ask cargo to rerun this script when `HEAD` (and the ref it points at) moves,
/// and report whether there was a git directory to watch at all.
fn watch_git_head(workspace_root: &Path) -> Option<()> {
    let git_dir = workspace_root.join(".git");
    // a worktree's `.git` is a file naming the real directory; the standard case
    // is a directory holding `HEAD` directly
    let head = if git_dir.is_dir() {
        git_dir.join("HEAD")
    } else {
        let contents = std::fs::read_to_string(&git_dir).ok()?;
        let (label, path) = contents.split_once(':')?;
        if label != "gitdir" {
            return None;
        }
        PathBuf::from(path.trim()).join("HEAD")
    };
    if !head.exists() {
        return None;
    }
    println!("cargo:rerun-if-changed={}", head.display());

    // on a branch, `HEAD` names a ref whose own file is what a commit moves
    if let Ok(contents) = std::fs::read_to_string(&head)
        && let Some(reference) = contents.split_whitespace().nth(1)
        && let Some(parent) = head.parent()
    {
        println!(
            "cargo:rerun-if-changed={}",
            parent.join(reference).display()
        );
    }
    Some(())
}
