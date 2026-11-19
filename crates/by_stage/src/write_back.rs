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
//! A debugger that replaces code in the running program writes the new bytes into the tree too,
//! through a re-stage ([`crate::restage`]), which lists each file it wrote in the tree's
//! sourcemap with the digest of what it wrote. A file still holding what the re-stage listed is
//! `by`'s, not the program's, and is neither carried back nor reported.
//!
//! The one thing in the tree that is never carried back is the bytecode python caches beside
//! the modules it imports: a `.pyc` in a `__pycache__` directory, the place
//! `importlib.util.cache_from_source` names (PEP 3147). The interpreter writes it, not the program,
//! and it describes the tree's modules rather than the project's.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use walkdir::WalkDir;

use crate::sourcemap::{BY_SOURCEMAP_FILENAME, map_digest, restaged_files};
use crate::staging::{Staging, placement_base, portable};

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
    /// Where everything `staging` placed came from, for a project rooted at `root` with the
    /// module roots `roots`, which staging placed it by.
    pub fn of(staging: &Staging, roots: &[PathBuf], root: &Path) -> Self {
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
            // a source below no root keeps its whole path in the tree, so its directories there
            // were staged from no directory of the project
            if let Some(base) = placement_base(roots, root, source)
                && let Some(staged) = relative.parent()
            {
                for directory in staged.ancestors() {
                    mirror
                        .directories
                        .entry(directory.to_path_buf())
                        .or_default()
                        .insert(base.join(directory));
                }
            }
        }
        mirror
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
        // everything at a path the walk saw, a file or not: a copy whose path holds something
        // else now was replaced rather than deleted
        let mut present = BTreeSet::new();
        // what the walk could not read, whose files are neither present nor gone
        let mut unreadable = Vec::new();
        let mut differing = Vec::new();
        for entry in walk(&self.tree) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let staged = error
                        .path()
                        .map_or_else(PathBuf::new, |path| relative_to(&self.tree, path));
                    unreadable.push(staged.clone());
                    let error = error.into_io_error().unwrap_or_else(|| {
                        io::Error::other("a directory of the tree loops back on itself")
                    });
                    outcomes.push(Outcome::Unreadable { staged, error });
                    continue;
                }
            };
            let relative = relative_to(&self.tree, entry.path());
            if entry.file_type().is_dir() {
                continue;
            }
            present.insert(relative.clone());
            if !entry.file_type().is_file() {
                if !self.others.contains(&relative) {
                    outcomes.push(Outcome::NotAFile(relative));
                }
                continue;
            }
            match digest(entry.path()) {
                Ok(now) if self.files.get(&relative) == Some(&now) => {}
                Ok(now) => differing.push((relative, now)),
                Err(error) => outcomes.push(Outcome::Unreadable {
                    staged: relative,
                    error,
                }),
            }
        }
        // deletions first: a file the program renamed to a name its project's file system cannot
        // tell from the old one, as a case-only rename is on a case-insensitive one, is then
        // deleted under its old name before it is written under its new one, rather than the
        // other way round, which deletes what was just written. so is a file the program
        // replaced with a directory of the same name, which can then be made
        for (relative, before) in &self.files {
            if !present.contains(relative)
                && !unreadable
                    .iter()
                    .any(|directory| relative.starts_with(directory))
            {
                outcomes.extend(self.deleted(relative, *before));
            }
        }
        let restaged = Restaged::read(&self.tree);
        for (relative, now) in differing {
            if restaged.wrote(&relative, now) {
                continue;
            }
            let outcome = match self.files.get(&relative) {
                Some(before) => self.changed(&relative, *before, now),
                None => self.created(&relative, now),
            };
            outcomes.extend(outcome);
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
            Some(project) => self.settle(
                project,
                project.parent().unwrap_or(project),
                Some(before),
                Some((relative, now)),
            ),
            None => Some(self.lost(relative, false)),
        }
    }

    fn deleted(&self, relative: &Path, before: ContentDigest) -> Option<Outcome> {
        let Some(project) = self.mirror.copies.get(relative) else {
            return Some(self.lost(relative, true));
        };
        let outcome = self.settle(
            project,
            project.parent().unwrap_or(project),
            Some(before),
            None,
        );
        if matches!(outcome, Some(Outcome::Deleted(_))) {
            self.remove_emptied_directories(relative, project);
        }
        outcome
    }

    /// Remove the project directories above `project` that the program removed from the tree,
    /// once deleting `project` has left them empty, so that a package the program deleted is
    /// deleted rather than left as empty directories, and a file the program put in place of
    /// one can be written.
    fn remove_emptied_directories(&self, relative: &Path, project: &Path) {
        for (staged, directory) in relative.ancestors().zip(project.ancestors()).skip(1) {
            if staged.as_os_str().is_empty()
                || self.tree.join(staged).is_dir()
                || fs::remove_dir(directory).is_err()
            {
                break;
            }
        }
    }

    fn created(&self, relative: &Path, now: ContentDigest) -> Option<Outcome> {
        match self.destination(relative) {
            Ok((directory, project)) => {
                self.settle(&project, &directory, None, Some((relative, now)))
            }
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
    /// from, which is returned with it.
    fn destination(&self, relative: &Path) -> Result<(PathBuf, PathBuf), Outcome> {
        let parent = relative.parent().unwrap_or(Path::new(""));
        let unmapped = || Outcome::Unmapped(relative.to_path_buf());
        let existing = parent
            .ancestors()
            .find(|directory| self.directories.contains(*directory))
            .ok_or_else(unmapped)?;
        let below = relative.strip_prefix(existing).map_err(|_| unmapped())?;
        let sources = self.mirror.directories.get(existing).ok_or_else(unmapped)?;
        let mut sources_iter = sources.iter();
        match (sources_iter.next(), sources_iter.next()) {
            (Some(source), None) => Ok((source.clone(), source.join(below))),
            _ => Err(Outcome::Ambiguous {
                staged: relative.to_path_buf(),
                directories: sources.iter().cloned().collect(),
            }),
        }
    }

    /// Make `project` hold what the program left at `wanted` (its path in the tree and its
    /// digest), or make it absent when `wanted` is `None` — provided it still holds `staged`,
    /// what it held when the tree was staged, `None` for a file that was not there.
    ///
    /// `known` is the directory of the project the path is known to be below, a real one the
    /// build read files from: nothing between it and `project` may be a symbolic link, which
    /// would take the write somewhere the build never staged from, outside the project even
    fn settle(
        &self,
        project: &Path,
        known: &Path,
        staged: Option<ContentDigest>,
        wanted: Option<(&Path, ContentDigest)>,
    ) -> Option<Outcome> {
        if through_link(project, known) {
            return Some(Outcome::Linked(project.to_path_buf()));
        }
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

/// What a re-stage wrote into the tree while the program ran from it, as the tree's sourcemap
/// lists it (see [`crate::restage`]): what the debugger put there, not the program.
#[derive(Debug, Default)]
struct Restaged {
    /// each file, relative to the tree with `/` separators, and the digest of what was written
    files: BTreeMap<String, String>,
    /// the digest the map is listed at when it is what a re-stage wrote
    map: Option<String>,
}

impl Restaged {
    fn read(tree: &Path) -> Self {
        let Ok(map) = fs::read_to_string(tree.join(BY_SOURCEMAP_FILENAME)) else {
            return Self::default();
        };
        Self {
            files: restaged_files(&map).unwrap_or_default(),
            map: map_digest(&map),
        }
    }

    /// whether the file at `relative`, holding what digests to `now`, is what a re-stage wrote
    fn wrote(&self, relative: &Path, now: ContentDigest) -> bool {
        let Some(listed) = self.files.get(&portable(relative)) else {
            return false;
        };
        if relative == Path::new(BY_SOURCEMAP_FILENAME) {
            self.map.as_ref() == Some(listed)
        } else {
            *listed == spelled(&now)
        }
    }
}

/// a digest as the sourcemap spells one
fn spelled(digest: &ContentDigest) -> String {
    let mut spelled = String::from("sha256:");
    for byte in digest {
        let _ = write!(spelled, "{byte:02x}");
    }
    spelled
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
    /// the project file is reached through a symbolic link below the directory it was staged
    /// from or would be written to
    Linked(PathBuf),
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
                    shown_in_tree(staged)
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
                shown_in_tree(staged),
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
                shown_in_tree(staged)
            ),
            Self::NotAFile(staged) => format!(
                "warning: not written back: {}, which is not a regular file",
                shown_in_tree(staged)
            ),
            Self::Linked(project) => format!(
                "warning: not written back: {} is reached through a symbolic link in the project",
                shown(project)
            ),
            Self::Unreadable { staged, error } => format!(
                "error: could not read {} in the staged tree to write it back: {error}",
                shown_in_tree(staged)
            ),
            Self::Failed { project, error } => {
                format!("error: could not write back {}: {error}", shown(project))
            }
        }
    }
}

/// The tree, without the bytecode python caches in it.
fn walk(tree: &Path) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
    WalkDir::new(tree)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter(|entry| !entry.as_ref().is_ok_and(is_bytecode_cache))
}

/// whether `entry` is a file python wrote as the bytecode cache of a module: a `.pyc` in a
/// `__pycache__` directory
fn is_bytecode_cache(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_file()
        && entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "pyc")
        && entry
            .path()
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|directory| directory == "__pycache__")
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

/// whether anything from below `known` down to `project` itself is a symbolic link
fn through_link(project: &Path, known: &Path) -> bool {
    project
        .ancestors()
        .take_while(|path| *path != known && path.starts_with(known))
        .any(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()))
}

/// copy `from` over `to`, making the directories `to` needs
///
/// through a file beside `to` that is then renamed over it, so that `to` is never left half
/// written: the old content stays until the new is whole
fn write(from: &Path, to: &Path) -> io::Result<()> {
    let (Some(parent), Some(name)) = (to.parent(), to.file_name()) else {
        return Err(io::Error::other("not a path a file can be written to"));
    };
    fs::create_dir_all(parent)?;
    let partial = parent.join(format!(
        ".{}.{}.by-write-back",
        name.to_string_lossy(),
        std::process::id()
    ));
    let written = fs::copy(from, &partial).and_then(|_| fs::rename(&partial, to));
    if written.is_err() {
        let _ = fs::remove_file(&partial);
    }
    written
}

/// a path of the tree with `/` separators, whatever the platform, to show
fn shown_in_tree(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a project holding `files` (relative path and content), each staged as a copy into a tree
    /// beside a file of `by`'s own in a directory of its own, and the tree inventoried
    fn staged(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Inventory) {
        let project = tempfile::tempdir().expect("tempdir");
        let tree = tempfile::tempdir().expect("tempdir");
        let mut staging = Staging::new(tree.path());
        for (relative, content) in files {
            let path = project.path().join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            fs::write(&path, content).expect("write");
            staging.copy(Path::new(relative), &path).expect("copy");
        }
        staging
            .write(Path::new("_by_own/marker"), None, "by's\n")
            .expect("write");
        let mirror = Mirror::of(&staging, &[project.path().to_path_buf()], project.path());
        staging.finish().expect("finish");
        let inventory = mirror.inventory(tree.path()).expect("inventory");
        (project, tree, inventory)
    }

    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("read_dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_project_file_already_holding_what_the_program_left_is_not_mentioned() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        fs::write(tree.path().join("pkg/a.py"), "changed\n").expect("write");
        fs::write(project.path().join("pkg/a.py"), "changed\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(outcomes.is_empty(), "{outcomes:?}");
    }

    /// a file the project has and the build did not stage is not the program's to replace
    #[test]
    fn a_new_file_where_the_project_has_another_is_left() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
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
        let (_project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        fs::write(tree.path().join("_by_own/new"), "new\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Unmapped(path)] if path == Path::new("_by_own/new")),
            "{outcomes:?}"
        );
    }

    /// on a case-insensitive file system the new name is the old file, so writing it before
    /// deleting the old name deleted what was written
    #[test]
    fn a_case_only_rename_renames_the_project_file() {
        let (project, tree, inventory) = staged(&[("pkg/Data.txt", "staged\n")]);
        fs::rename(
            tree.path().join("pkg/Data.txt"),
            tree.path().join("pkg/data.txt"),
        )
        .expect("rename");
        let outcomes = inventory.write_back();
        assert_eq!(
            names(&project.path().join("pkg")),
            ["data.txt"],
            "{outcomes:?}"
        );
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/data.txt")).expect("read"),
            "staged\n"
        );
    }

    #[test]
    fn a_file_the_program_replaced_with_a_directory_is_replaced_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/data.json", "{}\n")]);
        fs::remove_file(tree.path().join("pkg/data.json")).expect("remove");
        fs::create_dir(tree.path().join("pkg/data.json")).expect("mkdir");
        fs::write(tree.path().join("pkg/data.json/x.txt"), "x\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/data.json/x.txt"))
                .ok()
                .as_deref(),
            Some("x\n"),
            "{outcomes:?}"
        );
    }

    #[test]
    fn a_directory_the_program_replaced_with_a_file_is_replaced_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/sub/x.txt", "x\n")]);
        fs::remove_dir_all(tree.path().join("pkg/sub")).expect("remove");
        fs::write(tree.path().join("pkg/sub"), "now a file\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/sub"))
                .ok()
                .as_deref(),
            Some("now a file\n"),
            "{outcomes:?}"
        );
    }

    /// a package the program deleted goes from the project whole, not as empty directories
    #[test]
    fn a_directory_the_program_deleted_is_deleted_from_the_project() {
        let (project, tree, inventory) =
            staged(&[("pkg/sub/deep/x.txt", "x\n"), ("pkg/kept.txt", "kept\n")]);
        fs::remove_dir_all(tree.path().join("pkg/sub")).expect("remove");
        let outcomes = inventory.write_back();
        assert_eq!(
            names(&project.path().join("pkg")),
            ["kept.txt"],
            "{outcomes:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_the_program_replaced_with_a_symlink_is_kept() {
        let (project, tree, inventory) =
            staged(&[("pkg/notes.txt", "notes\n"), ("pkg/other.txt", "other\n")]);
        fs::remove_file(tree.path().join("pkg/notes.txt")).expect("remove");
        std::os::unix::fs::symlink("other.txt", tree.path().join("pkg/notes.txt"))
            .expect("symlink");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::NotAFile(_)]),
            "{outcomes:?}"
        );
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/notes.txt")).expect("read"),
            "notes\n"
        );
    }

    /// a directory the program made unreadable says nothing about whether its files are there
    #[cfg(unix)]
    #[test]
    fn the_files_of_a_directory_that_cannot_be_read_are_not_deleted() {
        use std::os::unix::fs::PermissionsExt as _;
        let (project, tree, inventory) = staged(&[("pkg/sub/x.txt", "x\n")]);
        let sub = tree.path().join("pkg/sub");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o300)).expect("chmod");
        let outcomes = inventory.write_back();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).expect("chmod");
        assert!(
            project.path().join("pkg/sub/x.txt").exists(),
            "{outcomes:?}"
        );
    }

    /// the build never stages through a symbolic link, so a write through one would land
    /// wherever it points, outside the project even
    #[cfg(unix)]
    #[test]
    fn a_new_file_is_not_written_through_a_symlink_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        let outside = tempfile::tempdir().expect("tempdir");
        let target = outside.path().join("target.txt");
        std::os::unix::fs::symlink(&target, project.path().join("pkg/link.txt")).expect("symlink");
        fs::write(tree.path().join("pkg/link.txt"), "through the link\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Linked(_)]),
            "{outcomes:?}"
        );
        assert!(!target.exists());
    }
}
