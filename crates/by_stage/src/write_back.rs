//! what a program run out of a staged tree left in it, carried back to the project
//!
//! `by run` and `by repl` run the program out of a temporary tree, which is deleted when the
//! program ends. a program that writes beside its own modules writes into that tree: django's
//! `makemigrations` writes into the directory its app package's `__path__` names, which is the
//! staged one. without this, what it wrote went with the tree
//!
//! the tree is inventoried just before the program starts, every file by its content and its
//! permissions. when the program has ended, what differs is carried back to the project:
//!
//! - a new file goes to the project directory its staged directory was staged from. a directory
//!   the program made goes where its parent's would, an empty one included
//! - a changed copy of a project file is written over that file, its permissions with it, and a
//!   deleted one deletes it
//! - a file `by` wrote — a transpiled module, the runtime, the sourcemap, a file a re-stage added
//!   — has no project file to go back to, so what the program did to it is not carried back, and
//!   said not to be
//!
//! a project file is written or deleted only while it is still what was staged, or, for a new
//! file, still absent; one that changed while the program ran is left as it is. a staged
//! directory that files from more than one project directory were staged into, as the root of a
//! src-layout project's tree is (its `src/` and the project root both land there), says nothing
//! about where a new file in it belongs, so the file is reported rather than written, unless the
//! program takes the root of the tree for the project root, as it does when it runs there
//!
//! nothing the project or the program held is lost on the way. what to do is decided for every
//! file before anything is done, and a project file the program deleted is deleted only once
//! everything else the program changed has been written: a file it renamed has its only other
//! copy in the tree. so anything that is not carried back, an [`Outcome`] that is not
//! [carried](Outcome::is_carried), leaves the tree as the one place holding it, and the caller
//! keeps the tree rather than deleting it
//!
//! a debugger that replaces code in the running program writes the new bytes into the tree too,
//! through a re-stage ([`crate::restage`]), which lists each file it wrote in the tree's
//! sourcemap with the digest of what it wrote. a file still holding what the re-stage listed is
//! `by`'s, not the program's, and is neither carried back nor reported
//!
//! the only other thing in the tree that is never carried back is what is in a `__pycache__`
//! directory, which PEP 3147 reserves for the caches of the import system: the bytecode python
//! writes beside the modules it imports, the temporary file it writes that through, and what a
//! library such as numba caches there. it describes the tree's modules rather than the
//! project's, and the build never stages from such a directory

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use walkdir::WalkDir;

use crate::run_notes::{Level, Note};
use crate::sourcemap::{BY_SOURCEMAP_FILENAME, map_digest, parse_digest, restaged_files, tree_key};
use crate::staging::{Staging, placement_base};

type ContentDigest = [u8; 32];

/// where the files and directories of a staged tree came from in the project
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
    /// the project root
    root: PathBuf,
    /// whether the root of the tree stands for the project root: see
    /// [`Mirror::with_the_tree_root_as_the_project_root`]
    tree_root_is_project_root: bool,
}

impl Mirror {
    /// where everything `staging` placed came from, for a project rooted at `root` with the
    /// module roots `roots`, which staging placed it by
    pub fn of(staging: &Staging, roots: &[PathBuf], root: &Path) -> Self {
        let mut mirror = Self {
            root: root.to_path_buf(),
            ..Self::default()
        };
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

    /// the mirror of a tree whose root the program takes for the project root, as it
    /// does when it runs there: `by run --in-build` runs it with the root of the tree for its
    /// working directory. a new file or directory there, or below a directory the program made
    /// there, then goes to the project root, rather than being refused for the root of a
    /// src-layout project's tree holding what was staged from `src/` too. one below a directory
    /// that was staged goes where that directory was staged from, as always
    #[must_use]
    pub fn with_the_tree_root_as_the_project_root(mut self) -> Self {
        self.tree_root_is_project_root = true;
        self
    }

    /// every file in `tree` by its content and permissions, as it is before the program starts
    ///
    /// taken once everything `by` writes into the tree is there, so that anything not a copy of
    /// a project file is known to be `by`'s
    pub fn inventory(self, tree: &Path) -> io::Result<Inventory> {
        let mut files = BTreeMap::new();
        let mut others = BTreeSet::new();
        let mut directories = BTreeSet::new();
        for entry in walk(tree) {
            let entry = entry.map_err(io::Error::other)?;
            let relative = relative_to(tree, entry.path());
            if entry.file_type().is_dir() {
                directories.insert(relative);
            } else if entry.file_type().is_file()
                && let Some(held) = Held::read(entry.path())?
            {
                files.insert(relative, held);
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

/// a staged tree as it was before the program started
#[derive(Debug)]
pub struct Inventory {
    tree: PathBuf,
    mirror: Mirror,
    files: BTreeMap<PathBuf, Held>,
    /// what was in the tree and was neither a file nor a directory
    others: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
}

impl Inventory {
    /// carry what the program changed in the tree back to the project, and say what was done
    /// and what was not
    ///
    /// a file the program left as it was staged says nothing, and nor does one whose project
    /// file already is what the program left
    pub fn write_back(&self) -> Vec<Outcome> {
        let plan = self.plan();
        let mut outcomes = plan.refused;
        for change in &plan.changes {
            outcomes.extend(self.apply(change));
        }
        if outcomes.iter().all(Outcome::is_carried) {
            // a file the program renamed to a name its project's file system cannot tell from the
            // old one, as a case-only rename is on a case-insensitive one, is deleted under its
            // old name before it is written under its new one, rather than the other way round,
            // which deletes what was just written. so is a file the program replaced with a
            // directory of the same name, which can then be made
            for deletion in &plan.deletions {
                outcomes.extend(self.delete(deletion));
            }
            for change in &plan.displacing {
                outcomes.extend(self.apply(change));
            }
        } else {
            outcomes.extend(
                plan.deletions
                    .iter()
                    .map(|deletion| Outcome::NotDeleted(deletion.project.clone())),
            );
            outcomes.extend(
                plan.displacing
                    .iter()
                    .map(|change| Outcome::Displacing(change.project.clone())),
            );
        }
        outcomes
    }

    /// what carrying the tree back comes to, decided for every file before anything is done
    fn plan(&self) -> Plan {
        let mut plan = Plan::default();
        // everything at a path the walk saw, a file or not: a copy whose path holds something
        // else now was replaced rather than deleted
        let mut present = BTreeSet::new();
        // what the walk could not read, whose files are neither present nor gone
        let mut unreadable = Vec::new();
        let mut differing = Vec::new();
        // the directories the program made, and the directories anything is in
        let mut made = Vec::new();
        let mut holding = BTreeSet::new();
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
                    plan.refused.push(Outcome::Unreadable { staged, error });
                    continue;
                }
            };
            let relative = relative_to(&self.tree, entry.path());
            if entry.depth() > 0
                && let Some(parent) = relative.parent()
            {
                holding.insert(parent.to_path_buf());
            }
            if entry.file_type().is_dir() {
                if !self.directories.contains(&relative) {
                    made.push(relative);
                }
                continue;
            }
            present.insert(relative.clone());
            let held = if entry.file_type().is_file() {
                Held::read(entry.path())
            } else {
                Ok(None)
            };
            match held {
                Ok(Some(now)) if self.files.get(&relative) == Some(&now) => {}
                Ok(Some(now)) => differing.push((relative, now)),
                Ok(None) => {
                    if !self.others.contains(&relative) {
                        plan.refused.push(Outcome::NotAFile(relative));
                    }
                }
                Err(error) => plan.refused.push(Outcome::Unreadable {
                    staged: relative,
                    error,
                }),
            }
        }
        let is_readable = |relative: &Path| {
            !unreadable
                .iter()
                .any(|directory| relative.starts_with(directory))
        };
        let record = RestageRecord::read(&self.tree).unwrap_or_else(|error| {
            plan.refused.push(Outcome::RestagesUnknown(error));
            RestageRecord::default()
        });

        for (relative, before) in &self.files {
            if present.contains(relative) || !is_readable(relative) {
                continue;
            }
            let Some(project) = self.mirror.copies.get(relative) else {
                plan.refused.push(self.lost(relative, true));
                continue;
            };
            let expected = record.expected(relative, before);
            let known = project.parent().unwrap_or(project);
            match judge_now(project, known, Some(&expected), None) {
                Verdict::Done => {}
                Verdict::Proceed(_) => plan.deletions.push(Deletion {
                    staged: relative.clone(),
                    project: project.clone(),
                    expected,
                }),
                Verdict::Refused(outcome) => plan.refused.push(outcome),
            }
        }
        // a file a re-stage added to the tree is `by`'s, as a transpiled module is: it has no
        // project file of its own to delete
        for relative in record.added(&self.files) {
            if !present.contains(&relative) && is_readable(&relative) {
                plan.refused.push(self.lost(&relative, true));
            }
        }

        let deleted: BTreeSet<PathBuf> = plan
            .deletions
            .iter()
            .map(|deletion| deletion.project.clone())
            .collect();
        for (relative, now) in differing {
            if record.wrote(&relative, now.digest) {
                continue;
            }
            let (project, known, expected) = match self.files.get(&relative) {
                Some(before) => match self.mirror.copies.get(&relative) {
                    Some(project) => (
                        project.clone(),
                        project.parent().unwrap_or(project).to_path_buf(),
                        Some(record.expected(&relative, before)),
                    ),
                    None => {
                        plan.refused.push(self.lost(&relative, false));
                        continue;
                    }
                },
                None if record.listed(&relative).is_some() => {
                    plan.refused.push(self.lost(&relative, false));
                    continue;
                }
                None => match self.destination(&relative) {
                    Ok((known, project)) => (project, known, None),
                    Err(outcome) => {
                        plan.refused.push(outcome);
                        continue;
                    }
                },
            };
            plan.decide(
                Change {
                    project,
                    known,
                    expected,
                    wanted: Wanted::File(relative, now),
                },
                &deleted,
            );
        }
        // a directory the program made with something in it is made as that is written; one
        // with nothing in it is made on its own
        for relative in made {
            if holding.contains(&relative) || !is_readable(&relative) {
                continue;
            }
            match self.destination(&relative) {
                Ok((known, project)) => plan.decide(
                    Change {
                        project,
                        known,
                        expected: None,
                        wanted: Wanted::Directory,
                    },
                    &deleted,
                ),
                Err(outcome) => plan.refused.push(outcome),
            }
        }
        plan
    }

    /// make `change` in the project, looking at the project again first: it may have changed
    /// since the plan was made
    fn apply(&self, change: &Change) -> Option<Outcome> {
        let project = &change.project;
        let found = match judge_now(
            project,
            &change.known,
            change.expected.as_ref(),
            Some(&change.wanted),
        ) {
            Verdict::Done => return None,
            Verdict::Refused(outcome) => return Some(outcome),
            Verdict::Proceed(found) => found,
        };
        let done = match &change.wanted {
            // the content already there, so only the permissions differ
            Wanted::File(_, now) if matches!(&found, Found::File(held) if held.digest == now.digest) => {
                fs::set_permissions(project, now.permissions.clone())
                    .map(|()| Outcome::Permissions(project.clone()))
            }
            Wanted::File(relative, _) => {
                write(&self.tree.join(relative), project).map(|()| Outcome::Wrote(project.clone()))
            }
            Wanted::Directory => {
                fs::create_dir_all(project).map(|()| Outcome::MadeDirectory(project.clone()))
            }
        };
        Some(done.unwrap_or_else(|error| Outcome::Failed {
            project: project.clone(),
            error,
        }))
    }

    fn delete(&self, deletion: &Deletion) -> Option<Outcome> {
        let project = &deletion.project;
        let known = project.parent().unwrap_or(project);
        match judge_now(project, known, Some(&deletion.expected), None) {
            Verdict::Done => return None,
            Verdict::Refused(outcome) => return Some(outcome),
            Verdict::Proceed(_) => {}
        }
        Some(match fs::remove_file(project) {
            Ok(()) => {
                self.remove_emptied_directories(&deletion.staged, project);
                Outcome::Deleted(project.clone())
            }
            Err(error) => Outcome::Failed {
                project: project.clone(),
                error,
            },
        })
    }

    /// remove the project directories above `project` that the program removed from the tree,
    /// once deleting `project` has left them empty, so that a package the program deleted is
    /// deleted rather than left as empty directories, and a file the program put in place of
    /// one can be written
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

    fn lost(&self, relative: &Path, deleted: bool) -> Outcome {
        Outcome::Lost {
            staged: relative.to_path_buf(),
            source: self.mirror.transpiled.get(relative).cloned(),
            deleted,
        }
    }

    /// where in the project a file or directory the program made at `relative` goes: below the
    /// project directory its nearest directory that was there before the program started was
    /// staged from, which is returned with it
    fn destination(&self, relative: &Path) -> Result<(PathBuf, PathBuf), Outcome> {
        let parent = relative.parent().unwrap_or(Path::new(""));
        let unmapped = || Outcome::Unmapped(relative.to_path_buf());
        let existing = parent
            .ancestors()
            .find(|directory| self.directories.contains(*directory))
            .ok_or_else(unmapped)?;
        let below = relative.strip_prefix(existing).map_err(|_| unmapped())?;
        if existing.as_os_str().is_empty() && self.mirror.tree_root_is_project_root {
            let root = &self.mirror.root;
            return Ok((root.clone(), root.join(below)));
        }
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
}

/// what carrying a tree back comes to, decided before anything is done
#[derive(Default)]
struct Plan {
    /// what is not carried back, and why
    refused: Vec<Outcome>,
    /// what lands in the project with nothing deleted first
    changes: Vec<Change>,
    /// the project files to delete, once everything else has landed
    deletions: Vec<Deletion>,
    /// what lands only where a deletion makes room for it, after the deletions
    displacing: Vec<Change>,
}

impl Plan {
    /// put `change` where it goes in the plan, the project files in `deleted` to be deleted
    fn decide(&mut self, change: Change, deleted: &BTreeSet<PathBuf>) {
        if through_link(&change.project, &change.known) {
            self.refused.push(Outcome::Linked(change.project));
            return;
        }
        let (found, displacing) = match found_after(&change.project, deleted) {
            Ok(found) => found,
            Err(error) => {
                self.refused.push(Outcome::Failed {
                    project: change.project,
                    error,
                });
                return;
            }
        };
        match judge(
            &change.project,
            found,
            change.expected.as_ref(),
            Some(&change.wanted),
        ) {
            Verdict::Done => {}
            Verdict::Proceed(_) if displacing => self.displacing.push(change),
            Verdict::Proceed(_) => self.changes.push(change),
            Verdict::Refused(outcome) => self.refused.push(outcome),
        }
    }
}

/// a project file to delete, as the program deleted its copy at `staged`
struct Deletion {
    staged: PathBuf,
    project: PathBuf,
    /// what it holds as it was staged
    expected: Held,
}

/// something to make in the project
struct Change {
    project: PathBuf,
    /// the directory of the project `project` is known to be below, a real one the build read
    /// files from: nothing between it and `project` may be a symbolic link, which would take the
    /// write somewhere the build never staged from, outside the project even
    known: PathBuf,
    /// what the project holds there as it was staged, `None` for nothing
    expected: Option<Held>,
    wanted: Wanted,
}

/// what a [`Change`] leaves in the project
enum Wanted {
    /// what the file at this path of the tree holds
    File(PathBuf, Held),
    /// a directory
    Directory,
}

/// a regular file as it is: what is in it, and who may read, write and run it
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    digest: ContentDigest,
    permissions: fs::Permissions,
}

impl Held {
    /// what is at `path`, `None` for something that is not a regular file
    fn read(path: &Path) -> io::Result<Option<Self>> {
        let Some(mut file) = open_regular(path)? else {
            return Ok(None);
        };
        let permissions = file.metadata()?.permissions();
        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher)?;
        Ok(Some(Self {
            digest: hasher.finalize().into(),
            permissions,
        }))
    }
}

/// what is at a path of the project
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Absent,
    File(Held),
    Directory,
    /// anything else: a named pipe, a socket, a device, a symbolic link
    Other,
}

impl Found {
    fn at(path: &Path) -> io::Result<Self> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Absent),
            Err(error) => return Err(error),
        };
        if metadata.is_dir() {
            return Ok(Self::Directory);
        }
        if !metadata.is_file() {
            return Ok(Self::Other);
        }
        Ok(Held::read(path)?.map_or(Self::Other, Self::File))
    }
}

/// what to do about one project path
enum Verdict {
    /// the project already holds what it would come to
    Done,
    /// the project holds what was staged, which is found there
    Proceed(Found),
    /// it is left, for the reason given
    Refused(Outcome),
}

/// what to do about `project`, which holds `found` and held `expected` as it was staged, to make
/// it hold `wanted`, `None` for nothing
fn judge(
    project: &Path,
    found: Found,
    expected: Option<&Held>,
    wanted: Option<&Wanted>,
) -> Verdict {
    let holds_wanted = match (wanted, &found) {
        (None, Found::Absent) | (Some(Wanted::Directory), Found::Directory) => true,
        (Some(Wanted::File(_, wanted)), Found::File(held)) => held == wanted,
        _ => false,
    };
    if holds_wanted {
        return Verdict::Done;
    }
    let holds_expected = match (expected, &found) {
        (None, Found::Absent) => true,
        (Some(expected), Found::File(held)) => held == expected,
        _ => false,
    };
    if holds_expected {
        return Verdict::Proceed(found);
    }
    Verdict::Refused(match expected {
        None => Outcome::Occupied(project.to_path_buf()),
        Some(_) => Outcome::ChangedMeanwhile {
            project: project.to_path_buf(),
            deleted: found == Found::Absent,
        },
    })
}

/// [`judge`] what is at `project` now, reached from `known`
fn judge_now(
    project: &Path,
    known: &Path,
    expected: Option<&Held>,
    wanted: Option<&Wanted>,
) -> Verdict {
    if through_link(project, known) {
        return Verdict::Refused(Outcome::Linked(project.to_path_buf()));
    }
    match Found::at(project) {
        Ok(found) => judge(project, found, expected, wanted),
        Err(error) => Verdict::Refused(Outcome::Failed {
            project: project.to_path_buf(),
            error,
        }),
    }
}

/// what is at `path` once the project files in `deleted` are deleted, and whether it is absent
/// only because they are
fn found_after(path: &Path, deleted: &BTreeSet<PathBuf>) -> io::Result<(Found, bool)> {
    // a file the program replaced with a directory, which is made where the file was
    if path.ancestors().any(|path| deleted.contains(path)) {
        return Ok((Found::Absent, true));
    }
    let found = Found::at(path)?;
    let displaced = match &found {
        Found::Directory => is_emptied(path, deleted)?,
        Found::File(_) => is_another_name_of_deleted(path, deleted)?,
        Found::Absent | Found::Other => false,
    };
    Ok(if displaced {
        (Found::Absent, true)
    } else {
        (found, false)
    })
}

/// whether deleting the files in `deleted` leaves `directory` with nothing in it, so that it is
/// removed with them: every file below it is deleted, and every directory below it holds one
fn is_emptied(directory: &Path, deleted: &BTreeSet<PathBuf>) -> io::Result<bool> {
    if deleted.is_empty() {
        return Ok(false);
    }
    let mut directories = Vec::new();
    let mut holding = BTreeSet::new();
    for entry in WalkDir::new(directory).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        if entry.depth() > 0
            && let Some(parent) = entry.path().parent()
        {
            holding.insert(parent.to_path_buf());
        }
        if entry.file_type().is_dir() {
            directories.push(entry.into_path());
        } else if !deleted.contains(entry.path()) {
            return Ok(false);
        }
    }
    Ok(directories
        .iter()
        .all(|directory| holding.contains(directory)))
}

/// whether `path` reaches an entry its directory lists under another name, as a file system that
/// folds names together, a case-insensitive one, reaches `data.txt` as `Data.txt`, and every
/// entry it reaches is among the files in `deleted`
fn is_another_name_of_deleted(path: &Path, deleted: &BTreeSet<PathBuf>) -> io::Result<bool> {
    let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(false);
    };
    if deleted.is_empty() {
        return Ok(false);
    }
    let mut reached = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() == name {
            return Ok(false);
        }
        let listed = entry.path();
        if same_entry(&listed, path)? {
            if !deleted.contains(&listed) {
                return Ok(false);
            }
            reached = true;
        }
    }
    Ok(reached)
}

/// whether two paths reach the same entry of the file system, found without opening either: a
/// named pipe would block the open until something wrote to it
#[cfg(unix)]
fn same_entry(left: &Path, right: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let (left, right) = (fs::symlink_metadata(left)?, fs::symlink_metadata(right)?);
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(not(unix))]
fn same_entry(left: &Path, right: &Path) -> io::Result<bool> {
    same_file::is_same_file(left, right)
}

/// what a re-stage wrote into the tree while the program ran from it, as the tree's sourcemap
/// lists it (see [`crate::restage`]): what the debugger put there, not the program
#[derive(Debug, Default)]
struct RestageRecord {
    /// each file by its [`tree_key`], and the digest of what a re-stage last wrote there
    files: BTreeMap<String, ContentDigest>,
    /// the digest the map is listed at while it is as a re-stage wrote it
    map: Option<ContentDigest>,
}

impl RestageRecord {
    /// the record the tree's map holds. an error for a map whose table cannot be read, after
    /// which nothing tells what a re-stage wrote from what the program wrote
    fn read(tree: &Path) -> io::Result<Self> {
        let map = match fs::read_to_string(tree.join(BY_SOURCEMAP_FILENAME)) {
            Ok(map) => map,
            // a map the program deleted is reported as such, and no re-stage can have written
            // into a tree without one
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let unreadable = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "it is not a table of files and their digests",
            )
        };
        let files = restaged_files(&map)
            .ok_or_else(unreadable)?
            .into_iter()
            .map(|(key, digest)| Some((key, parse_digest(&digest)?)))
            .collect::<Option<_>>()
            .ok_or_else(unreadable)?;
        Ok(Self {
            files,
            map: map_digest(&map).as_deref().and_then(parse_digest),
        })
    }

    /// the digest of what a re-stage last wrote at `relative`, if one wrote there
    fn listed(&self, relative: &Path) -> Option<ContentDigest> {
        self.files.get(&tree_key(relative)?).copied()
    }

    /// what the file at `relative`, inventoried as `before`, holds as far as the program is
    /// concerned: what a re-stage last wrote there, which held what the project file did then,
    /// or else what was staged
    fn expected(&self, relative: &Path, before: &Held) -> Held {
        Held {
            digest: self.listed(relative).unwrap_or(before.digest),
            permissions: before.permissions.clone(),
        }
    }

    /// the files a re-stage wrote that were not in the tree when it was inventoried
    fn added(&self, inventoried: &BTreeMap<PathBuf, Held>) -> Vec<PathBuf> {
        self.files
            .keys()
            .map(|key| key.split('/').collect::<PathBuf>())
            .filter(|relative| !inventoried.contains_key(relative))
            .collect()
    }

    /// whether the file at `relative`, holding what digests to `now`, is what a re-stage wrote
    fn wrote(&self, relative: &Path, now: ContentDigest) -> bool {
        let Some(listed) = self.listed(relative) else {
            return false;
        };
        if relative == Path::new(BY_SOURCEMAP_FILENAME) {
            self.map == Some(listed)
        } else {
            listed == now
        }
    }
}

/// what carrying one file of the tree back to the project came to
#[derive(Debug)]
pub enum Outcome {
    /// the project file was written with what the program left in the tree
    Wrote(PathBuf),
    /// the project file's permissions were changed, as the program changed its copy's
    Permissions(PathBuf),
    /// the project directory was made, as the program made an empty directory in the tree
    MadeDirectory(PathBuf),
    /// the project file was deleted, as the program deleted its copy
    Deleted(PathBuf),
    /// the program deleted the copy of the project file, which is not deleted, since something
    /// else the program changed was not carried back
    NotDeleted(PathBuf),
    /// the program made the file or directory where a project file it deleted the copy of is,
    /// which is not deleted, since something else the program changed was not carried back
    Displacing(PathBuf),
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
    /// the program made a new file where the project already has something else
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
    /// the table of what a re-stage wrote into the tree could not be read, so a file a re-stage
    /// wrote reads as one the program wrote
    RestagesUnknown(io::Error),
    /// a file in the tree could not be read
    Unreadable { staged: PathBuf, error: io::Error },
    /// the project file could not be read, written or deleted
    Failed { project: PathBuf, error: io::Error },
}

impl Outcome {
    /// whether this did what the program asked. anything else leaves the staged tree the only
    /// place holding what the program left, which the caller keeps
    pub fn is_carried(&self) -> bool {
        self.level() == Level::Note
    }

    /// whether this did what the program asked, did not, or could not
    fn level(&self) -> Level {
        match self {
            Self::Wrote(_) | Self::Permissions(_) | Self::MadeDirectory(_) | Self::Deleted(_) => {
                Level::Note
            }
            Self::RestagesUnknown(_) | Self::Unreadable { .. } | Self::Failed { .. } => {
                Level::Error
            }
            Self::NotDeleted(_)
            | Self::Displacing(_)
            | Self::Lost { .. }
            | Self::ChangedMeanwhile { .. }
            | Self::Occupied(_)
            | Self::Ambiguous { .. }
            | Self::Unmapped(_)
            | Self::NotAFile(_)
            | Self::Linked(_) => Level::Warning,
        }
    }

    /// one line saying what happened, naming project files relative to `cwd` when they are
    /// below it and files of the tree by their place in it
    pub fn note(&self, cwd: &Path) -> Note {
        let shown = |path: &Path| path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        let message = match self {
            Self::Wrote(project) => format!("wrote back {}", shown(project)),
            Self::Permissions(project) => {
                format!("wrote back the permissions of {}", shown(project))
            }
            Self::MadeDirectory(project) => format!("made the directory {}", shown(project)),
            Self::Deleted(project) => format!(
                "deleted {}, as the program deleted its copy",
                shown(project)
            ),
            Self::NotDeleted(project) => format!(
                "not deleted: {}, whose copy the program deleted, since not everything else it \
                 changed was written back",
                shown(project)
            ),
            Self::Displacing(project) => format!(
                "not written back: {}, which takes the place of a project file that is not \
                 deleted, since not everything else the program changed was written back",
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
                    "not written back: the program {did} {}, which is {made}",
                    shown_in_tree(staged)
                )
            }
            Self::ChangedMeanwhile { project, deleted } => format!(
                "not written back: {} was {} in the project while the program ran",
                shown(project),
                if *deleted { "deleted" } else { "changed" }
            ),
            Self::Occupied(project) => format!(
                "not written back: {} is already in the project, holding something else",
                shown(project)
            ),
            Self::Ambiguous {
                staged,
                directories,
            } => format!(
                "not written back: {}, whose directory in the staged tree was staged \
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
                "not written back: {}, whose directory in the staged tree no project \
                 directory was staged into",
                shown_in_tree(staged)
            ),
            Self::NotAFile(staged) => format!(
                "not written back: {}, which is not a regular file",
                shown_in_tree(staged)
            ),
            Self::Linked(project) => format!(
                "not written back: {} is reached through a symbolic link in the project",
                shown(project)
            ),
            Self::RestagesUnknown(error) => format!(
                "could not read the table of what a re-stage wrote in {BY_SOURCEMAP_FILENAME}, \
                 so what a debugger wrote into the staged tree is taken for the program's: \
                 {error}"
            ),
            Self::Unreadable { staged, error } => format!(
                "could not read {} in the staged tree to write it back: {error}",
                shown_in_tree(staged)
            ),
            Self::Failed { project, error } => {
                format!("could not write back {}: {error}", shown(project))
            }
        };
        Note::new(self.level(), message)
    }
}

/// the tree, without its `__pycache__` directories
fn walk(tree: &Path) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
    WalkDir::new(tree)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| !is_import_cache(entry))
}

/// whether `entry` is a `__pycache__` directory below the root of the tree
fn is_import_cache(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0 && entry.file_type().is_dir() && entry.file_name() == "__pycache__"
}

fn relative_to(tree: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(tree).unwrap_or(path).to_path_buf()
}

/// `path` opened for reading, `None` when it is not a regular file. opened without waiting for
/// a writer, so that a named pipe found there does not block the open until one comes
fn open_regular(path: &Path) -> io::Result<Option<fs::File>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = options.open(path)?;
    Ok(file.metadata()?.is_file().then_some(file))
}

/// whether anything from below `known` down to `project` itself is a symbolic link
fn through_link(project: &Path, known: &Path) -> bool {
    project
        .ancestors()
        .take_while(|path| *path != known && path.starts_with(known))
        .any(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()))
}

/// copy `from` over `to`, its permissions with it, making the directories `to` needs
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
        staging
            .write(Path::new(BY_SOURCEMAP_FILENAME), None, "RESTAGED = {}\n")
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

    /// write `content` to `relative` in the tree as a re-stage does, listing it in the map
    fn restage(tree: &Path, relative: &str, content: &str) {
        fs::write(tree.join(relative), content).expect("write");
        let map_path = tree.join(BY_SOURCEMAP_FILENAME);
        let map = fs::read_to_string(&map_path).expect("the map");
        let mut files = restaged_files(&map).expect("a table");
        files.insert(
            relative.to_owned(),
            crate::sourcemap::content_digest(content.as_bytes()),
        );
        files.insert(
            BY_SOURCEMAP_FILENAME.to_owned(),
            map_digest(&map).expect("a digest"),
        );
        let map = crate::sourcemap::with_restaged_files(&map, &files).expect("a table");
        fs::write(map_path, map).expect("write");
    }

    /// the debugger put the user's edit into the tree, and the user then took it back: the
    /// edit is not the program's, and must not undo theirs
    #[test]
    fn a_file_still_holding_what_a_restage_wrote_is_left() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        restage(tree.path(), "pkg/a.py", "edited\n");
        let outcomes = inventory.write_back();
        assert!(outcomes.is_empty(), "{outcomes:?}");
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/a.py")).expect("read"),
            "staged\n"
        );
    }

    /// a re-staged copy held what the project file did, so the program's change to it is
    /// carried back over that, not refused as a change made in the project
    #[test]
    fn what_the_program_writes_over_a_restaged_file_is_carried_back() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        fs::write(project.path().join("pkg/a.py"), "edited\n").expect("write");
        restage(tree.path(), "pkg/a.py", "edited\n");
        fs::write(tree.path().join("pkg/a.py"), "the program's\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/a.py")).expect("read"),
            "the program's\n",
            "{outcomes:?}"
        );
    }

    /// without the table nothing tells a re-stage's writes from the program's, which is said
    #[test]
    fn a_table_of_restaged_files_that_cannot_be_read_is_reported() {
        let (_project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        fs::write(tree.path().join(BY_SOURCEMAP_FILENAME), "RESTAGED = {\"a\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, Outcome::RestagesUnknown(_))),
            "{outcomes:?}"
        );
    }

    #[test]
    fn a_project_file_already_holding_what_the_program_left_is_not_mentioned() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        fs::write(tree.path().join("pkg/a.py"), "changed\n").expect("write");
        fs::write(project.path().join("pkg/a.py"), "changed\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(outcomes.is_empty(), "{outcomes:?}");
    }

    /// a `__pycache__` directory is the import system's, whatever is in it: python's bytecode,
    /// the temporary file python writes it through, a library's own cache. a `.pyc` anywhere
    /// else is the program's
    #[test]
    fn nothing_in_a_pycache_directory_is_carried_back() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        let cache = tree.path().join("pkg/__pycache__");
        fs::create_dir_all(cache.join("nested")).expect("mkdir");
        for name in [
            "a.cpython-313.pyc",
            "a.cpython-313.pyc.4402818880",
            "a.f-1.py313.nbi",
            "a.f-1.py313.1.nbc",
            "nested/deeper",
        ] {
            fs::write(cache.join(name), "cached\n").expect("write");
        }
        fs::write(tree.path().join("pkg/b.pyc"), "the program's\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Wrote(path)] if path.ends_with("pkg/b.pyc")),
            "{outcomes:?}"
        );
        assert_eq!(names(&project.path().join("pkg")), ["a.py", "b.pyc"]);
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

    /// the program renamed a file to where the project already has another: the rename's new
    /// half is refused, so its old half is not carried back either, and the file's content is in
    /// the project still rather than only in the tree
    #[test]
    fn a_file_moved_where_it_cannot_be_written_is_not_deleted() {
        let (project, tree, inventory) = staged(&[("pkg/a.txt", "precious\n")]);
        fs::write(project.path().join("pkg/b.txt"), "theirs\n").expect("write");
        fs::rename(tree.path().join("pkg/a.txt"), tree.path().join("pkg/b.txt")).expect("rename");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/a.txt"))
                .ok()
                .as_deref(),
            Some("precious\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::Occupied(occupied), Outcome::NotDeleted(kept)]
                    if occupied.ends_with("pkg/b.txt") && kept.ends_with("pkg/a.txt")
            ),
            "{outcomes:?}"
        );
    }

    /// a deletion waits for every write: one refused anywhere keeps every file the program
    /// deleted, however unrelated
    #[test]
    fn no_file_is_deleted_while_another_is_not_written_back() {
        let (project, tree, inventory) =
            staged(&[("pkg/gone.txt", "gone\n"), ("pkg/a.py", "staged\n")]);
        fs::remove_file(tree.path().join("pkg/gone.txt")).expect("remove");
        fs::write(tree.path().join("_by_own/new"), "new\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(project.path().join("pkg/gone.txt").exists(), "{outcomes:?}");
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, Outcome::NotDeleted(_))),
            "{outcomes:?}"
        );
    }

    #[test]
    fn an_empty_directory_the_program_made_is_made_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        fs::create_dir_all(tree.path().join("pkg/empty/inner")).expect("mkdir");
        let outcomes = inventory.write_back();
        assert!(
            project.path().join("pkg/empty/inner").is_dir(),
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes.as_slice(), [Outcome::MadeDirectory(made)] if made.ends_with("pkg/empty/inner")),
            "{outcomes:?}"
        );
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).expect("metadata").permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    #[cfg(unix)]
    #[test]
    fn a_change_to_only_the_permissions_of_a_copy_is_carried_back() {
        let (project, tree, inventory) = staged(&[("pkg/run.sh", "echo hi\n")]);
        set_mode(&tree.path().join("pkg/run.sh"), 0o755);
        let outcomes = inventory.write_back();
        assert_eq!(
            mode(&project.path().join("pkg/run.sh")),
            0o755,
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Permissions(path)] if path.ends_with("pkg/run.sh")),
            "{outcomes:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_is_written_back_with_the_permissions_the_program_gave_it() {
        let (project, tree, inventory) = staged(&[("pkg/run.sh", "echo hi\n")]);
        fs::write(tree.path().join("pkg/run.sh"), "echo changed\n").expect("write");
        set_mode(&tree.path().join("pkg/run.sh"), 0o700);
        fs::write(tree.path().join("pkg/new.sh"), "echo new\n").expect("write");
        set_mode(&tree.path().join("pkg/new.sh"), 0o750);
        let outcomes = inventory.write_back();
        assert_eq!(
            mode(&project.path().join("pkg/run.sh")),
            0o700,
            "{outcomes:?}"
        );
        assert_eq!(
            mode(&project.path().join("pkg/new.sh")),
            0o750,
            "{outcomes:?}"
        );
    }

    /// a named pipe is never opened: opening one to read it waits for something to write to it
    #[cfg(unix)]
    #[test]
    fn a_named_pipe_in_the_project_is_not_opened() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        let pipe = project.path().join("pkg/b.txt");
        let made = std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        fs::write(tree.path().join("pkg/b.txt"), "mine\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Occupied(path)] if path.ends_with("pkg/b.txt")),
            "{outcomes:?}"
        );
    }

    /// a file a re-stage put in the tree that was not there before is `by`'s, as a transpiled
    /// module is: what the program does to it has no project file to go to, and is said to be
    /// lost rather than written into the project as a file the program made, or passed over
    #[test]
    fn a_file_a_restage_added_is_not_the_programs() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        restage(tree.path(), "pkg/deleted.py", "added\n");
        restage(tree.path(), "pkg/changed.py", "added\n");
        fs::remove_file(tree.path().join("pkg/deleted.py")).expect("remove");
        fs::write(tree.path().join("pkg/changed.py"), "the program's\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(names(&project.path().join("pkg")), ["a.py"], "{outcomes:?}");
        let lost = |name: &str, gone: bool| {
            outcomes.iter().any(|outcome| {
                matches!(
                    outcome,
                    Outcome::Lost { staged, deleted, .. } if staged.ends_with(name) && *deleted == gone
                )
            })
        };
        assert!(lost("pkg/deleted.py", true), "{outcomes:?}");
        assert!(lost("pkg/changed.py", false), "{outcomes:?}");
    }

    /// a src-layout project, `src/app/a.py` beside `pyproject.toml`, staged by the roots `src/`
    /// and the project root, so that both land at the root of the tree, and the tree inventoried
    /// with its root standing for the project root or not
    fn staged_src_layout(
        tree_root_is_project_root: bool,
    ) -> (tempfile::TempDir, tempfile::TempDir, Inventory) {
        let project = tempfile::tempdir().expect("tempdir");
        let tree = tempfile::tempdir().expect("tempdir");
        let mut staging = Staging::new(tree.path());
        for (relative, staged) in [
            ("src/app/a.py", "app/a.py"),
            ("pyproject.toml", "pyproject.toml"),
        ] {
            let path = project.path().join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            fs::write(&path, "staged\n").expect("write");
            staging.copy(Path::new(staged), &path).expect("copy");
        }
        let roots = [project.path().join("src"), project.path().to_path_buf()];
        let mirror = Mirror::of(&staging, &roots, project.path());
        let mirror = if tree_root_is_project_root {
            mirror.with_the_tree_root_as_the_project_root()
        } else {
            mirror
        };
        staging.finish().expect("finish");
        let inventory = mirror.inventory(tree.path()).expect("inventory");
        (project, tree, inventory)
    }

    /// what a program writes at the root of a tree it takes for the project root, as a test
    /// runner run in the tree writes its report and its cache, goes to the project root, while a
    /// file below a staged directory still goes where that directory came from
    #[test]
    fn a_new_file_at_a_tree_root_standing_for_the_project_root_goes_to_the_project_root() {
        let (project, tree, inventory) = staged_src_layout(true);
        fs::write(tree.path().join("report.xml"), "<report/>\n").expect("write");
        fs::create_dir_all(tree.path().join(".pytest_cache/v")).expect("mkdir");
        fs::write(tree.path().join(".pytest_cache/v/lastfailed"), "{}\n").expect("write");
        fs::write(tree.path().join("app/new.txt"), "new\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(outcomes.iter().all(Outcome::is_carried), "{outcomes:?}");
        let read = |relative: &str| fs::read_to_string(project.path().join(relative)).ok();
        assert_eq!(read("report.xml").as_deref(), Some("<report/>\n"));
        assert_eq!(read(".pytest_cache/v/lastfailed").as_deref(), Some("{}\n"));
        assert_eq!(read("src/app/new.txt").as_deref(), Some("new\n"));
    }

    /// a tree root that does not stand for the project root says nothing about which of the
    /// directories staged into it a new file there belongs in
    #[test]
    fn a_new_file_at_a_tree_root_staged_from_two_directories_is_refused() {
        let (project, tree, inventory) = staged_src_layout(false);
        fs::write(tree.path().join("report.xml"), "<report/>\n").expect("write");
        let outcomes = inventory.write_back();
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Ambiguous { .. }]),
            "{outcomes:?}"
        );
        assert!(!project.path().join("report.xml").exists());
    }
}
