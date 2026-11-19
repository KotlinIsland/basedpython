//! What a program run out of a staged tree left in it, carried back to the project.
//!
//! `by run` and `by repl` run the program out of a temporary tree, which is deleted when the
//! program ends. A program that writes beside its own modules writes into that tree: django's
//! `makemigrations` writes into the directory its app package's `__path__` names, which is the
//! staged one. Without this, what it wrote went with the tree.
//!
//! The tree is inventoried just before the program starts, every file by its content. When the
//! program has ended, what differs is carried back to the project:
//!
//! - a new file goes to the project directory its staged directory was staged from. a directory
//!   the program made goes where its parent's would
//! - a changed copy of a project file is written over that file, and a deleted one deletes it
//! - a file `by` wrote — a transpiled module, the runtime, the sourcemap — has no project file to
//!   go back to, so what the program did to it is lost, and said to be
//!
//! A project file is written or deleted only while it is still what was staged, or, for a new
//! file, still absent; one that changed while the program ran is left as it is. A staged
//! directory that files from more than one project directory were staged into, as the root of a
//! src-layout project's tree is (its `src/` and the project root both land there), says nothing
//! about where a new file in it belongs, so the file is reported rather than written.
//!
//! Directories the build never descends into — hidden ones, `__pycache__`, `build` and the rest
//! `may_hold_build_content` turns away — are not part of the mirror in either
//! direction: nothing in them is staged, and nothing a program writes into them is carried back.
//! That is also what keeps the bytecode python caches in `__pycache__` out of the project.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use walkdir::WalkDir;

use crate::project::may_hold_build_content_named;
use crate::staging::Staging;

type ContentDigest = [u8; 32];

/// Where the files and directories of a staged tree came from in the project.
#[derive(Debug, Default)]
pub struct Mirror {
    /// each copy of a project file, relative to the tree, and that file
    copies: BTreeMap<PathBuf, PathBuf>,
    /// each file written from a source rather than copied from it, relative to the tree, and
    /// that source
    transpiled: BTreeMap<PathBuf, PathBuf>,
    /// each staged directory, relative to the tree, and the project directories files were
    /// staged into it from
    directories: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

impl Mirror {
    /// Where everything `staging` placed came from.
    pub fn of(staging: &Staging) -> Self {
        let mut mirror = Self::default();
        for (relative, source) in staging.entries() {
            let Some(source) = source else {
                continue;
            };
            if staging.is_copy(relative) {
                mirror
                    .copies
                    .insert(relative.to_path_buf(), source.to_path_buf());
            } else {
                mirror
                    .transpiled
                    .insert(relative.to_path_buf(), source.to_path_buf());
            }
            mirror.record_directories(relative, source);
        }
        mirror
    }

    /// Record the directories a file staged at `relative` from `source` lands in.
    ///
    /// Staging keeps a source's path below the module root it is in (see
    /// [`crate::staging::relative_destination`]), so the staged directory `n` levels above
    /// `relative` is the project directory `n` levels above `source`, up to the tree's root.
    fn record_directories(&mut self, relative: &Path, source: &Path) {
        let (Some(staged), Some(project)) = (relative.parent(), source.parent()) else {
            return;
        };
        if !project.ends_with(staged) {
            return;
        }
        let Some(base) = project.ancestors().nth(staged.components().count()) else {
            return;
        };
        for directory in staged.ancestors() {
            self.directories
                .entry(directory.to_path_buf())
                .or_default()
                .insert(base.join(directory));
        }
    }

    /// Every file in `tree` by its content, as it is before the program starts.
    ///
    /// Taken once everything `by` writes into the tree is there, so that anything not a copy of
    /// a project file is known to be `by`'s.
    pub fn inventory(self, tree: &Path) -> io::Result<Inventory> {
        let mut files = BTreeMap::new();
        let mut others = BTreeSet::new();
        let mut directories = BTreeSet::new();
        for entry in walk(tree) {
            let entry = entry.map_err(io::Error::other)?;
            let relative = relative_to(tree, entry.path());
            if entry.file_type().is_dir() {
                directories.insert(relative);
            } else if entry.file_type().is_file() {
                files.insert(relative, digest(entry.path())?);
            } else {
                others.insert(relative);
            }
        }
        Ok(Inventory {
            tree: tree.to_path_buf(),
            mirror: self,
            files,
            others,
            directories,
        })
    }
}

/// A staged tree as it was before the program started.
#[derive(Debug)]
pub struct Inventory {
    tree: PathBuf,
    mirror: Mirror,
    files: BTreeMap<PathBuf, ContentDigest>,
    /// what was in the tree and was neither a file nor a directory
    others: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
}

impl Inventory {
    /// Carry what the program changed in the tree back to the project, and say what was done
    /// and what was not.
    ///
    /// A file the program left as it was staged says nothing, and nor does one whose project
    /// file already is what the program left.
    pub fn write_back(&self) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        let mut present = BTreeSet::new();
        for entry in walk(&self.tree) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let path = error
                        .path()
                        .map_or_else(PathBuf::new, |path| relative_to(&self.tree, path));
                    outcomes.push(Outcome::Unreadable {
                        staged: path,
                        error: io::Error::other(error),
                    });
                    continue;
                }
            };
            let relative = relative_to(&self.tree, entry.path());
            if entry.file_type().is_dir() {
                continue;
            }
            if !entry.file_type().is_file() {
                if !self.others.contains(&relative) {
                    outcomes.push(Outcome::NotAFile(relative));
                }
                continue;
            }
            present.insert(relative.clone());
            let now = match digest(entry.path()) {
                Ok(now) => now,
                Err(error) => {
                    outcomes.push(Outcome::Unreadable {
                        staged: relative,
                        error,
                    });
                    continue;
                }
            };
            let outcome = match self.files.get(&relative) {
                Some(before) if *before == now => None,
                Some(before) => self.changed(&relative, *before, now),
                None => self.created(&relative, now),
            };
            outcomes.extend(outcome);
        }
        for (relative, before) in &self.files {
            if !present.contains(relative) {
                outcomes.extend(self.deleted(relative, *before));
            }
        }
        outcomes
    }

    fn changed(
        &self,
        relative: &Path,
        before: ContentDigest,
        now: ContentDigest,
    ) -> Option<Outcome> {
        match self.mirror.copies.get(relative) {
            Some(project) => self.settle(project, Some(before), Some((relative, now))),
            None => Some(self.lost(relative, false)),
        }
    }

    fn deleted(&self, relative: &Path, before: ContentDigest) -> Option<Outcome> {
        match self.mirror.copies.get(relative) {
            Some(project) => self.settle(project, Some(before), None),
            None => Some(self.lost(relative, true)),
        }
    }

    fn created(&self, relative: &Path, now: ContentDigest) -> Option<Outcome> {
        match self.destination(relative) {
            Ok(project) => self.settle(&project, None, Some((relative, now))),
            Err(outcome) => Some(outcome),
        }
    }

    fn lost(&self, relative: &Path, deleted: bool) -> Outcome {
        Outcome::Lost {
            staged: relative.to_path_buf(),
            source: self.mirror.transpiled.get(relative).cloned(),
            deleted,
        }
    }

    /// Where in the project a file the program made at `relative` goes: below the project
    /// directory its nearest directory that was there before the program started was staged
    /// from.
    fn destination(&self, relative: &Path) -> Result<PathBuf, Outcome> {
        let parent = relative.parent().unwrap_or(Path::new(""));
        let unmapped = || Outcome::Unmapped(relative.to_path_buf());
        let existing = parent
            .ancestors()
            .find(|directory| self.directories.contains(*directory))
            .ok_or_else(unmapped)?;
        let below = relative.strip_prefix(existing).map_err(|_| unmapped())?;
        let sources = self.mirror.directories.get(existing).ok_or_else(unmapped)?;
        match sources.iter().collect::<Vec<_>>().as_slice() {
            [source] => Ok(source.join(below)),
            _ => Err(Outcome::Ambiguous {
                staged: relative.to_path_buf(),
                directories: sources.iter().cloned().collect(),
            }),
        }
    }

    /// Make `project` hold what the program left at `wanted` (its path in the tree and its
    /// digest), or make it absent when `wanted` is `None` — provided it still holds `staged`,
    /// what it held when the tree was staged, `None` for a file that was not there.
    fn settle(
        &self,
        project: &Path,
        staged: Option<ContentDigest>,
        wanted: Option<(&Path, ContentDigest)>,
    ) -> Option<Outcome> {
        let current = match project_digest(project) {
            Ok(current) => current,
            Err(error) => {
                return Some(Outcome::Failed {
                    project: project.to_path_buf(),
                    error,
                });
            }
        };
        if current == wanted.map(|(_, digest)| digest) {
            return None;
        }
        if current != staged {
            return Some(match staged {
                None => Outcome::Occupied(project.to_path_buf()),
                Some(_) => Outcome::ChangedMeanwhile {
                    project: project.to_path_buf(),
                    deleted: current.is_none(),
                },
            });
        }
        let done = match wanted {
            Some((relative, _)) => write(&self.tree.join(relative), project)
                .map(|()| Outcome::Wrote(project.to_path_buf())),
            None => fs::remove_file(project).map(|()| Outcome::Deleted(project.to_path_buf())),
        };
        Some(done.unwrap_or_else(|error| Outcome::Failed {
            project: project.to_path_buf(),
            error,
        }))
    }
}

/// What carrying one file of the tree back to the project came to.
#[derive(Debug)]
pub enum Outcome {
    /// the project file was written with what the program left in the tree
    Wrote(PathBuf),
    /// the project file was deleted, as the program deleted its copy
    Deleted(PathBuf),
    /// the program changed or deleted a file `by` wrote, which has no project file to go back to
    Lost {
        staged: PathBuf,
        /// the `.by` it was transpiled from, for a transpiled module
        source: Option<PathBuf>,
        deleted: bool,
    },
    /// the project file changed while the program ran, so what the program did to its copy is
    /// not carried over it
    ChangedMeanwhile { project: PathBuf, deleted: bool },
    /// the program made a new file where the project already has a different one
    Occupied(PathBuf),
    /// the program made a new file in a staged directory more than one project directory was
    /// staged into
    Ambiguous {
        staged: PathBuf,
        directories: Vec<PathBuf>,
    },
    /// the program made a new file in a staged directory no project directory was staged into
    Unmapped(PathBuf),
    /// the program left something that is neither a file nor a directory
    NotAFile(PathBuf),
    /// a file in the tree could not be read
    Unreadable { staged: PathBuf, error: io::Error },
    /// the project file could not be read, written or deleted
    Failed { project: PathBuf, error: io::Error },
}

impl Outcome {
    /// Whether this is an I/O error rather than a decision.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Unreadable { .. } | Self::Failed { .. })
    }

    /// One line saying what happened, naming project files relative to `cwd` when they are
    /// below it and files of the tree by their place in it.
    pub fn describe(&self, cwd: &Path) -> String {
        let shown = |path: &Path| path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        match self {
            Self::Wrote(project) => format!("note: wrote back {}", shown(project)),
            Self::Deleted(project) => format!(
                "note: deleted {}, as the program deleted its copy",
                shown(project)
            ),
            Self::Lost {
                staged,
                source,
                deleted,
            } => {
                let did = if *deleted { "deleted" } else { "changed" };
                let made = match source {
                    Some(source) => format!("transpiled from {}", shown(source)),
                    None => "written by `by`".to_owned(),
                };
                format!(
                    "warning: not written back: the program {did} {}, which is {made}",
                    portable(staged)
                )
            }
            Self::ChangedMeanwhile { project, deleted } => format!(
                "warning: not written back: {} was {} in the project while the program ran",
                shown(project),
                if *deleted { "deleted" } else { "changed" }
            ),
            Self::Occupied(project) => format!(
                "warning: not written back: {} is already in the project, holding something else",
                shown(project)
            ),
            Self::Ambiguous {
                staged,
                directories,
            } => format!(
                "warning: not written back: {}, whose directory in the staged tree was staged \
                 from more than one in the project: {}",
                portable(staged),
                directories
                    .iter()
                    .map(|directory| shown(directory))
                    .map(|shown| if shown.is_empty() {
                        ".".to_owned()
                    } else {
                        shown
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Unmapped(staged) => format!(
                "warning: not written back: {}, whose directory in the staged tree no project \
                 directory was staged into",
                portable(staged)
            ),
            Self::NotAFile(staged) => format!(
                "warning: not written back: {}, which is not a regular file",
                portable(staged)
            ),
            Self::Unreadable { staged, error } => format!(
                "error: could not read {} in the staged tree to write it back: {error}",
                portable(staged)
            ),
            Self::Failed { project, error } => {
                format!("error: could not write back {}: {error}", shown(project))
            }
        }
    }
}

/// The tree, without the directories the build never descends into.
fn walk(tree: &Path) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
    WalkDir::new(tree)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || entry
                    .file_name()
                    .to_str()
                    .is_some_and(may_hold_build_content_named)
        })
}

fn relative_to(tree: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(tree).unwrap_or(path).to_path_buf()
}

fn digest(path: &Path) -> io::Result<ContentDigest> {
    let mut hasher = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().into())
}

/// the digest of `path` in the project, `None` when there is no file there
fn project_digest(path: &Path) -> io::Result<Option<ContentDigest>> {
    match digest(path) {
        Ok(digest) => Ok(Some(digest)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// copy `from` over `to`, making the directories `to` needs
fn write(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to).map(drop)
}

/// a path of the tree with `/` separators, whatever the platform
fn portable(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a project with `pkg/a.py`, staged into a tree beside a file of `by`'s own in a directory
    /// of its own, and the tree inventoried
    fn staged() -> (tempfile::TempDir, tempfile::TempDir, Inventory) {
        let project = tempfile::tempdir().expect("tempdir");
        let tree = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(project.path().join("pkg")).expect("mkdir");
        fs::write(project.path().join("pkg/a.py"), "staged\n").expect("write");
        let mut staging = Staging::new(tree.path());
        staging
            .copy(Path::new("pkg/a.py"), &project.path().join("pkg/a.py"))
            .expect("copy");
        staging
            .write(Path::new("_by_own/marker"), None, "by's\n")
            .expect("write");
        let mirror = Mirror::of(&staging);
        staging.finish().expect("finish");
        let inventory = mirror.inventory(tree.path()).expect("inventory");
        (project, tree, inventory)
    }

    #[test]
    fn a_project_file_already_holding_what_the_program_left_is_not_mentioned() {
        let (project, tree, inventory) = staged();
        fs::write(tree.path().join("pkg/a.py"), "changed\n").expect("write");
        fs::write(project.path().join("pkg/a.py"), "changed\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(outcomes.is_empty(), "{outcomes:?}");
    }

    /// a file the project has and the build did not stage is not the program's to replace
    #[test]
    fn a_new_file_where_the_project_has_another_is_left() {
        let (project, tree, inventory) = staged();
        fs::write(project.path().join("pkg/b.txt"), "theirs\n").expect("write");
        fs::write(tree.path().join("pkg/b.txt"), "mine\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Occupied(path)] if path.ends_with("pkg/b.txt")),
            "{outcomes:?}"
        );
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/b.txt")).expect("read"),
            "theirs\n"
        );
    }

    #[test]
    fn a_new_file_in_a_directory_nothing_was_staged_from_is_reported() {
        let (_project, tree, inventory) = staged();
        fs::write(tree.path().join("_by_own/new"), "new\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Unmapped(path)] if path == Path::new("_by_own/new")),
            "{outcomes:?}"
        );
    }
}
