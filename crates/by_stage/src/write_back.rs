//! what a program run out of a staged tree wrote there, carried back to the project
//!
//! `by run` and `by repl` run the program out of a temporary tree, which is deleted when the
//! program ends. a program that writes beside its own modules writes into that tree: django's
//! `makemigrations` writes into the directory its app package's `__path__` names, which is the
//! staged one. without this, what it wrote went with the tree
//!
//! the tree is inventoried just before the program starts, every file by its content and its
//! permissions. when the program has ended, what it wrote is carried back to the project:
//!
//! - a new file goes to the project directory its staged directory was staged from. a directory
//!   the program made goes where its parent's would, an empty one included
//! - a changed copy of a project file is written over that file, its permissions with it
//! - a file `by` wrote — a transpiled module, the runtime, the sourcemap, a file a re-stage added
//!   — has no project file to go back to, so what the program wrote over it is not carried back,
//!   and said not to be
//!
//! carrying back only ever adds to the project: nothing in it is deleted or displaced. a copy the
//! program deleted, renamed away or replaced with a directory leaves its project file as it is,
//! which is said. a new file is written only where the project has nothing, and a changed copy
//! only over a project file still holding what was staged; anything else there is left, and the
//! file not written. a project path already holding what the program left needs nothing, as the
//! old name of a file renamed only in case does on a file system that folds case
//!
//! a staged directory that files from more than one project directory were staged into, as the
//! root of a src-layout project's tree is (its `src/` and the project root both land there),
//! says nothing about where a new file in it belongs, so the file is reported rather than
//! written, unless the program takes the root of the tree for the project root, as it does when
//! it runs there
//!
//! what the program wrote that is not carried back, an [`Outcome`] that
//! [keeps the tree](Outcome::keeps_tree), has the tree as the one place holding it, and the
//! caller keeps the tree rather than deleting it
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
use std::sync::atomic::{AtomicU32, Ordering};

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
        let project_permissions = self
            .copies
            .iter()
            .filter_map(|(relative, project)| {
                let metadata = fs::symlink_metadata(project).ok()?;
                metadata
                    .is_file()
                    .then(|| (relative.clone(), metadata.permissions()))
            })
            .collect();
        Ok(Inventory {
            tree: tree.to_path_buf(),
            mirror: self,
            files,
            project_permissions,
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
    /// the permissions of the project file each copy was made from, as they were when the tree
    /// was inventoried: a copy does not keep every one of them, as macOS's drops the setuid bit.
    /// none for a project file that was no longer a regular file by then
    project_permissions: BTreeMap<PathBuf, fs::Permissions>,
    /// what was in the tree and was neither a file nor a directory
    others: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
}

impl Inventory {
    /// carry what the program wrote in the tree back to the project, and say what was done and
    /// what was not
    ///
    /// a file the program left as it was staged says nothing, and nor does one whose project
    /// file already is what the program left
    pub fn write_back(&self) -> Vec<Outcome> {
        let plan = self.plan();
        let mut outcomes = plan.outcomes;
        for change in &plan.changes {
            outcomes.extend(self.apply(change));
        }
        outcomes
    }

    /// what the program wrote that goes to the project, and what is said of the rest
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
                    plan.outcomes.push(Outcome::Unreadable { staged, error });
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
                        plan.outcomes.push(Outcome::NotAFile(relative));
                    }
                }
                Err(error) => plan.outcomes.push(Outcome::Unreadable {
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
            plan.outcomes.push(Outcome::RestagesUnknown(error));
            RestageRecord::default()
        });

        // a deletion is never carried back, so a copy the program deleted, renamed away or put
        // something else in the place of leaves its project file as it is
        for relative in self.files.keys() {
            if present.contains(relative) || !is_readable(relative) {
                continue;
            }
            match self.mirror.copies.get(relative) {
                Some(project) if is_absent(project) => {}
                Some(project) => plan.outcomes.push(Outcome::NotDeleted(project.clone())),
                None => plan.outcomes.push(self.lost(relative, true)),
            }
        }
        for relative in record.added(&self.files) {
            if !present.contains(&relative) && is_readable(&relative) {
                plan.outcomes.push(self.lost(&relative, true));
            }
        }

        for (relative, now) in differing {
            if record.wrote(&relative, now.digest) {
                continue;
            }
            let (project, known, expected, now) = match self.files.get(&relative) {
                Some(before) => match self.mirror.copies.get(&relative) {
                    Some(project) => {
                        let Some(permissions) = self.project_permissions.get(&relative) else {
                            plan.outcomes.push(Outcome::ChangedMeanwhile {
                                project: project.clone(),
                                deleted: is_absent(project),
                            });
                            continue;
                        };
                        let expected = Held {
                            digest: record.expected(&relative, before.digest),
                            permissions: permissions.clone(),
                        };
                        // permissions the program left as they were staged stay as the project
                        // file has them, whatever the copy was given
                        let now = if now.permissions == before.permissions {
                            Held {
                                digest: now.digest,
                                permissions: permissions.clone(),
                            }
                        } else {
                            now
                        };
                        (
                            project.clone(),
                            project.parent().unwrap_or(project).to_path_buf(),
                            Some(expected),
                            now,
                        )
                    }
                    None => {
                        plan.outcomes.push(self.lost(&relative, false));
                        continue;
                    }
                },
                // a file a re-stage added to the tree is `by`'s, as a transpiled module is
                None if record.listed(&relative).is_some() => {
                    plan.outcomes.push(self.lost(&relative, false));
                    continue;
                }
                None => match self.destination(&relative) {
                    Ok((known, project)) => (project, known, None, now),
                    Err(outcome) => {
                        plan.outcomes.push(outcome);
                        continue;
                    }
                },
            };
            plan.changes.push(Change {
                project,
                known,
                expected,
                wanted: Wanted::File(relative, now),
            });
        }
        // a directory the program made with something in it is made as that is written; one
        // with nothing in it is made on its own
        for relative in made {
            if holding.contains(&relative) || !is_readable(&relative) {
                continue;
            }
            match self.destination(&relative) {
                Ok((known, project)) => plan.changes.push(Change {
                    project,
                    known,
                    expected: None,
                    wanted: Wanted::Directory,
                }),
                Err(outcome) => plan.outcomes.push(outcome),
            }
        }
        plan
    }

    /// make `change` in the project, as long as the project holds what it did when the program
    /// started there
    fn apply(&self, change: &Change) -> Option<Outcome> {
        let project = &change.project;
        let found = match judge_now(
            project,
            &change.known,
            change.expected.as_ref(),
            &change.wanted,
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
            Wanted::File(relative, now) => write(
                &self.tree.join(relative),
                project,
                &now.permissions,
                change.expected.as_ref(),
            )
            .map(|placed| match placed {
                Placement::Done => Outcome::Wrote(project.clone()),
                Placement::Occupied => Outcome::Occupied(project.clone()),
                Placement::Changed { deleted } => Outcome::ChangedMeanwhile {
                    project: project.clone(),
                    deleted,
                },
            }),
            Wanted::Directory => {
                fs::create_dir_all(project).map(|()| Outcome::MadeDirectory(project.clone()))
            }
        };
        Some(done.unwrap_or_else(|error| Outcome::Failed {
            project: project.clone(),
            error,
        }))
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

/// what carrying a tree back comes to
#[derive(Default)]
struct Plan {
    /// what is said of what the program did that is not made in the project
    outcomes: Vec<Outcome>,
    /// what the program wrote that goes to the project
    changes: Vec<Change>,
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
    /// anything else: a named pipe, a socket, a device, a symbolic link, or, for a path below a
    /// file, the file
    Other,
}

impl Found {
    fn at(path: &Path) -> io::Result<Self> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            // nothing is there, or a file is above it: unix tells the two apart, but windows
            // answers a path below a file as not found, so what is above it is asked
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(if is_below_a_file(path) {
                    Self::Other
                } else {
                    Self::Absent
                });
            }
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

/// what to do about `project`, reached from `known`, which held `expected` as it was staged,
/// `None` for nothing, to make it hold `wanted`
fn judge_now(project: &Path, known: &Path, expected: Option<&Held>, wanted: &Wanted) -> Verdict {
    if through_link(project, known) {
        return Verdict::Refused(Outcome::Linked(project.to_path_buf()));
    }
    let found = match Found::at(project) {
        Ok(found) => found,
        Err(error) => {
            return Verdict::Refused(Outcome::Failed {
                project: project.to_path_buf(),
                error,
            });
        }
    };
    let holds_wanted = match (wanted, &found) {
        (Wanted::Directory, Found::Directory) => true,
        (Wanted::File(_, wanted), Found::File(held)) => held == wanted,
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

/// whether the nearest path above `path` that anything is at is not a directory, so that
/// nothing can be made at `path`
fn is_below_a_file(path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .find_map(|above| fs::metadata(above).ok())
        .is_some_and(|metadata| !metadata.is_dir())
}

/// whether nothing is at `path`, a broken symbolic link being something
fn is_absent(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
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

    /// the digest of what the file at `relative`, inventoried holding what digests to `staged`,
    /// holds as far as the program is concerned: what a re-stage last wrote there, which held
    /// what the project file did then, or else what was staged
    fn expected(&self, relative: &Path, staged: ContentDigest) -> ContentDigest {
        self.listed(relative).unwrap_or(staged)
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
    /// the program deleted the copy of the project file, or renamed it away or put something
    /// else in its place, and the project file is kept, as a deletion is never carried back
    NotDeleted(PathBuf),
    /// the program changed or deleted a file `by` wrote, which has no project file to go back to
    Lost {
        staged: PathBuf,
        /// the `.by` it was transpiled from, for a transpiled module
        source: Option<PathBuf>,
        deleted: bool,
    },
    /// the project file changed while the program ran, so what the program wrote over its copy
    /// is not carried over it
    ChangedMeanwhile { project: PathBuf, deleted: bool },
    /// the program made a new file or directory where the project already holds something else
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
    /// the project file could not be read or written
    Failed { project: PathBuf, error: io::Error },
}

impl Outcome {
    /// whether this leaves something the program wrote in the staged tree alone, which the
    /// caller then keeps rather than deletes
    pub fn keeps_tree(&self) -> bool {
        match self {
            Self::Wrote(_)
            | Self::Permissions(_)
            | Self::MadeDirectory(_)
            | Self::NotDeleted(_)
            | Self::Lost { deleted: true, .. } => false,
            Self::Lost { deleted: false, .. }
            | Self::ChangedMeanwhile { .. }
            | Self::Occupied(_)
            | Self::Ambiguous { .. }
            | Self::Unmapped(_)
            | Self::NotAFile(_)
            | Self::Linked(_)
            | Self::RestagesUnknown(_)
            | Self::Unreadable { .. }
            | Self::Failed { .. } => true,
        }
    }

    /// whether this did what the program asked, did not, or could not
    fn level(&self) -> Level {
        match self {
            Self::Wrote(_)
            | Self::Permissions(_)
            | Self::MadeDirectory(_)
            | Self::NotDeleted(_)
            | Self::Lost { deleted: true, .. } => Level::Note,
            Self::RestagesUnknown(_) | Self::Unreadable { .. } | Self::Failed { .. } => {
                Level::Error
            }
            Self::Lost { deleted: false, .. }
            | Self::ChangedMeanwhile { .. }
            | Self::Occupied(_)
            | Self::Ambiguous { .. }
            | Self::Unmapped(_)
            | Self::NotAFile(_)
            | Self::Linked(_) => Level::Warning,
        }
    }

    /// one line saying what happened, naming project files relative to `cwd` when they are
    /// below it and files of the tree by their place in it. `cwd` is canonical, as the project's
    /// paths are, or none of them is found below it
    pub fn note(&self, cwd: &Path) -> Note {
        let shown = |path: &Path| path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        let message = match self {
            Self::Wrote(project) => format!("wrote back {}", shown(project)),
            Self::Permissions(project) => {
                format!("wrote back the permissions of {}", shown(project))
            }
            Self::MadeDirectory(project) => format!("made the directory {}", shown(project)),
            Self::NotDeleted(project) => format!(
                "not deleted in the project: {}, which the program deleted (what a program \
                 writes is carried back, not what it deletes)",
                shown(project)
            ),
            Self::Lost {
                staged,
                source,
                deleted,
            } => {
                let made = match source {
                    Some(source) => format!("transpiled from {}", shown(source)),
                    None => "written by `by`".to_owned(),
                };
                if *deleted {
                    format!(
                        "the program deleted {}, which is {made}",
                        shown_in_tree(staged)
                    )
                } else {
                    format!(
                        "not written back: the program changed {}, which is {made}",
                        shown_in_tree(staged)
                    )
                }
            }
            Self::ChangedMeanwhile { project, deleted } => format!(
                "not written back: {} was {} in the project while the program ran",
                shown(project),
                if *deleted { "deleted" } else { "changed" }
            ),
            Self::Occupied(project) => format!(
                "not written back: {}, where the project already holds something else",
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

/// what came of putting a copy of a file the program wrote in its place in the project
enum Placement {
    Done,
    /// something was made where the project had nothing
    Occupied,
    /// the project file no longer held what was staged
    Changed {
        deleted: bool,
    },
}

/// copy `from` to `to` with `permissions`, making the directories `to` needs, as long as `to`
/// holds `expected` at the moment the copy takes its place, or nothing for `None`
///
/// through a file beside `to`, filled and on disk before it takes the place of what `to` holds,
/// so that `to` is never left half written, a crash included. what `to` holds is judged in the
/// same step as the copy takes its place, so that a file an editor saves there while a large
/// copy is made is never written over:
///
/// - a new file is renamed into place with a rename that refuses to replace anything, which is
///   `renamex_np` with `RENAME_EXCL` on macOS and `renameat2` with `RENAME_NOREPLACE` on linux,
///   or else hard linked into place, which refuses the same
/// - a changed copy is exchanged with the project file, by `renamex_np` with `RENAME_SWAP` on
///   macOS and `renameat2` with `RENAME_EXCHANGE` on linux, and what it was exchanged with is
///   judged once it is out of the way: anything but what was staged is exchanged back. without
///   an exchange the project file is renamed aside to be judged, and the copy is then linked
///   into place where nothing is, so that for that moment `to` names nothing
///
/// a file system offering neither such a rename nor hard links is not written to at all
fn write(
    from: &Path,
    to: &Path,
    permissions: &fs::Permissions,
    expected: Option<&Held>,
) -> io::Result<Placement> {
    let Some(parent) = to.parent() else {
        return Err(io::Error::other("not a path a file can be written to"));
    };
    fs::create_dir_all(parent)?;
    let (partial, file) = partial_file(parent)?;
    if let Err(error) = fill(file, from, permissions) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    #[cfg(test)]
    tests::before_publishing(to);
    match expected {
        None => place_new(&partial, to),
        Some(expected) => place_over(parent, &partial, to, expected),
    }
}

/// put the file at `partial` at `to` where nothing is, leaving nothing at `partial`
fn place_new(partial: &Path, to: &Path) -> io::Result<Placement> {
    match move_without_replacing(partial, to) {
        Ok(()) => Ok(Placement::Done),
        Err(error) => {
            let _ = fs::remove_file(partial);
            if error.kind() == io::ErrorKind::AlreadyExists {
                Ok(Placement::Occupied)
            } else {
                Err(error)
            }
        }
    }
}

/// put the file at `partial`, in `directory`, at `to` in place of the project file there, as
/// long as that holds `expected`, leaving nothing at `partial`
fn place_over(
    directory: &Path,
    partial: &Path,
    to: &Path,
    expected: &Held,
) -> io::Result<Placement> {
    #[cfg(unix)]
    match place_over_by_exchanging(partial, to, expected) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
        placed => return placed,
    }
    place_over_by_moving_aside(directory, partial, to, expected)
}

/// [`place_over`] through an exchange of the two files, `Unsupported` with `partial` left as it
/// was where the platform or the file system has no exchange
#[cfg(unix)]
fn place_over_by_exchanging(partial: &Path, to: &Path, expected: &Held) -> io::Result<Placement> {
    use std::os::unix::fs::MetadataExt as _;
    let identity =
        |path: &Path| fs::symlink_metadata(path).map(|metadata| (metadata.dev(), metadata.ino()));
    let discard = |error: io::Error| {
        let _ = fs::remove_file(partial);
        Err(error)
    };
    let copy = match identity(partial) {
        Ok(copy) => copy,
        Err(error) => return discard(error),
    };
    match flagged_rename::exchange(partial, to) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::Unsupported => return Err(error),
        Err(_) if is_absent(to) => {
            let _ = fs::remove_file(partial);
            return Ok(Placement::Changed { deleted: true });
        }
        Err(error) => return discard(error),
    }
    // `to` holds the copy now, and `partial` what `to` held
    if holds(partial, expected) {
        let _ = fs::remove_file(partial);
        return Ok(Placement::Done);
    }
    flagged_rename::exchange(partial, to).map_err(|error| displaced(partial, &error))?;
    // a file put at `to` between the two exchanges took the copy's place, and is newer still
    if identity(partial).is_ok_and(|found| found == copy) {
        let _ = fs::remove_file(partial);
        Ok(Placement::Changed { deleted: false })
    } else {
        Err(displaced(
            partial,
            &io::Error::other("it changed again while it was being put back"),
        ))
    }
}

/// [`place_over`] without an exchange: the project file is renamed aside, to a name of this
/// process's in `directory`, judged there, and the copy put at `to` only where nothing is, so
/// that a file put there meanwhile is never written over
fn place_over_by_moving_aside(
    directory: &Path,
    partial: &Path,
    to: &Path,
    expected: &Held,
) -> io::Result<Placement> {
    // a second name for the copy, which the project file is renamed over, so the file system is
    // known to make hard links before anything of the project is moved
    let aside = match beside(directory, |path| fs::hard_link(partial, path)) {
        Ok((aside, ())) => aside,
        Err(error) => {
            let _ = fs::remove_file(partial);
            return Err(error);
        }
    };
    if let Err(error) = fs::rename(to, &aside) {
        let _ = fs::remove_file(&aside);
        let _ = fs::remove_file(partial);
        return if is_absent(to) {
            Ok(Placement::Changed { deleted: true })
        } else {
            Err(error)
        };
    }
    if !holds(&aside, expected) {
        let _ = fs::remove_file(partial);
        put_back(&aside, to)?;
        return Ok(Placement::Changed { deleted: false });
    }
    match move_without_replacing(partial, to) {
        Ok(()) => {
            let _ = fs::remove_file(&aside);
            Ok(Placement::Done)
        }
        // what was put at `to` meanwhile is newer than what was staged, which is all `aside` holds
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(partial);
            let _ = fs::remove_file(&aside);
            Ok(Placement::Changed { deleted: false })
        }
        Err(error) => {
            let _ = fs::remove_file(partial);
            put_back(&aside, to)?;
            Err(error)
        }
    }
}

/// whether `path` is a regular file holding `expected`
fn holds(path: &Path, expected: &Held) -> bool {
    matches!(Found::at(path), Ok(Found::File(held)) if held == *expected)
}

/// the project file moved aside to `aside` moved back to `to`, where nothing is
fn put_back(aside: &Path, to: &Path) -> io::Result<()> {
    move_without_replacing(aside, to).map_err(|error| displaced(aside, &error))
}

/// `error` from putting back a project file that is now at `at`
fn displaced(at: &Path, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "{error}, so what the project held there is now {}",
            at.display()
        ),
    )
}

/// rename `from` to `to`, failing with `AlreadyExists` where anything is at `to`, a broken
/// symbolic link included
fn move_without_replacing(from: &Path, to: &Path) -> io::Result<()> {
    match flagged_rename::without_replacing(from, to) {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            fs::hard_link(from, to)?;
            let _ = fs::remove_file(from);
            Ok(())
        }
        renamed => renamed,
    }
}

/// the renames [`write`] puts a file in place with, where the platform has them: each is
/// `Unsupported` where the platform or the file system has not
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
mod flagged_rename {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    /// rename `from` to `to`, failing where anything is at `to`
    pub(super) fn without_replacing(from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(target_vendor = "apple")]
        let flags = libc::RENAME_EXCL;
        #[cfg(not(target_vendor = "apple"))]
        let flags = libc::RENAME_NOREPLACE;
        rename(from, to, flags)
    }

    /// exchange `a` and `b`, both of which must be there
    pub(super) fn exchange(a: &Path, b: &Path) -> io::Result<()> {
        #[cfg(target_vendor = "apple")]
        let flags = libc::RENAME_SWAP;
        #[cfg(not(target_vendor = "apple"))]
        let flags = libc::RENAME_EXCHANGE;
        rename(a, b, flags)
    }

    fn rename(from: &Path, to: &Path, flags: libc::c_uint) -> io::Result<()> {
        #[cfg(test)]
        if super::tests::without_flagged_renames() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        if call(&from, &to, flags) == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // `ENOSYS` from a kernel without the call, and `EINVAL` or `ENOTSUP` from a file system
        // without the flag: neither file is touched
        match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EINVAL | libc::ENOTSUP) => {
                Err(io::Error::new(io::ErrorKind::Unsupported, error))
            }
            _ => Err(error),
        }
    }

    #[cfg(target_vendor = "apple")]
    fn call(from: &CString, to: &CString, flags: libc::c_uint) -> libc::c_int {
        // SAFETY: both are nul-terminated paths, which outlive the call
        #[expect(unsafe_code)]
        unsafe {
            libc::renamex_np(from.as_ptr(), to.as_ptr(), flags)
        }
    }

    /// through the system call rather than glibc's wrapper of it, which glibc before 2.28 lacks
    #[cfg(not(target_vendor = "apple"))]
    fn call(from: &CString, to: &CString, flags: libc::c_uint) -> libc::c_long {
        // SAFETY: both are nul-terminated paths, which outlive the call
        #[expect(unsafe_code)]
        unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                flags,
            )
        }
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
mod flagged_rename {
    use std::io;
    use std::path::Path;

    pub(super) fn without_replacing(_from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    #[cfg(unix)]
    pub(super) fn exchange(_a: &Path, _b: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// how many paths [`beside`] has named in this process
static PARTIAL_FILES: AtomicU32 = AtomicU32::new(0);

/// what `make` makes at a new path in `directory` for [`write`] to go through, and that path
///
/// named for this process and a count rather than for the file written, so that the name is a
/// short one whatever that file's is. `make` fails with `AlreadyExists` where anything is, so
/// that it never takes the place of a file of the project
fn beside<T>(directory: &Path, make: impl Fn(&Path) -> io::Result<T>) -> io::Result<(PathBuf, T)> {
    let mut taken = 0;
    loop {
        let count = PARTIAL_FILES.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".by-write-back.{}.{count}", std::process::id()));
        match make(&path) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && taken < 64 => {
                taken += 1;
            }
            made => return made.map(|made| (path, made)),
        }
    }
}

/// a new, empty file in `directory` for [`write`] to fill, and its path
fn partial_file(directory: &Path) -> io::Result<(PathBuf, fs::File)> {
    beside(directory, |path| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // readable by its owner alone until it is whole and given its own permissions, as what
        // the program wrote may be for its owner alone
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(path)
    })
}

/// `file` filled with what the regular file at `from` holds, given `permissions`, and on disk
fn fill(mut file: fs::File, from: &Path, permissions: &fs::Permissions) -> io::Result<()> {
    let Some(mut source) = open_regular(from)? else {
        return Err(io::Error::other(
            "it is no longer a regular file in the staged tree",
        ));
    };
    io::copy(&mut source, &mut file)?;
    file.set_permissions(permissions.clone())?;
    file.sync_all()
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
    use std::cell::{Cell, RefCell};

    use super::*;

    type Hook = Box<dyn Fn(&Path)>;

    thread_local! {
        /// what a test does to the project once a write has filled its file and before that
        /// file is put in place, given the project path it is written to
        static BEFORE_PUBLISHING: RefCell<Option<Hook>> = const { RefCell::new(None) };
        /// whether a write goes the way it goes where the platform has no rename that refuses
        /// to replace and no exchange
        static WITHOUT_FLAGGED_RENAMES: Cell<bool> = const { Cell::new(false) };
    }

    #[cfg_attr(
        not(any(target_vendor = "apple", target_os = "linux")),
        expect(
            dead_code,
            reason = "only the flagged renames of macOS and linux consult it"
        )
    )]
    pub(super) fn without_flagged_renames() -> bool {
        WITHOUT_FLAGGED_RENAMES.get()
    }

    pub(super) fn before_publishing(to: &Path) {
        BEFORE_PUBLISHING.with_borrow(|hook| {
            if let Some(hook) = hook {
                hook(to);
            }
        });
    }

    /// `inventory` written back with `meanwhile` done to the project at the moment each file is
    /// about to be put in place, with the platform's flagged renames or without them
    fn write_back_racing(
        inventory: &Inventory,
        flagged_renames: bool,
        meanwhile: impl Fn(&Path) + 'static,
    ) -> Vec<Outcome> {
        BEFORE_PUBLISHING.set(Some(Box::new(meanwhile)));
        WITHOUT_FLAGGED_RENAMES.set(!flagged_renames);
        let outcomes = inventory.write_back();
        WITHOUT_FLAGGED_RENAMES.set(false);
        BEFORE_PUBLISHING.set(None);
        outcomes
    }

    /// a project holding `files` (relative path and content), each staged as a copy into a tree
    /// beside a file of `by`'s own in a directory of its own, and the tree inventoried
    fn staged(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Inventory) {
        staged_with(files, |_| {})
    }

    /// [`staged`], with `prepare` done to each project file before it is staged
    fn staged_with(
        files: &[(&str, &str)],
        prepare: impl Fn(&Path),
    ) -> (tempfile::TempDir, tempfile::TempDir, Inventory) {
        let project = tempfile::tempdir().expect("tempdir");
        let tree = tempfile::tempdir().expect("tempdir");
        let mut staging = Staging::new(tree.path());
        for (relative, content) in files {
            let path = project.path().join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            fs::write(&path, content).expect("write");
            prepare(&path);
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

    /// whether the file system holding `directory` folds case, reaching `name` as `NAME`
    fn folds_case(directory: &Path) -> bool {
        fs::write(directory.join("case-probe"), "").expect("write");
        let folds = directory.join("CASE-PROBE").exists();
        fs::remove_file(directory.join("case-probe")).expect("remove");
        folds
    }

    /// a deletion is never carried back: the project keeps the file, and the note says so
    #[test]
    fn a_file_the_program_deleted_is_kept_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/r.txt", "kept\n")]);
        fs::remove_file(tree.path().join("pkg/r.txt")).expect("remove");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/r.txt"))
                .ok()
                .as_deref(),
            Some("kept\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes.as_slice(), [Outcome::NotDeleted(kept)] if kept.ends_with("pkg/r.txt")),
            "{outcomes:?}"
        );
        assert!(!outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
    }

    /// a file renamed only in case is, on a file system that folds case, already in the project
    /// under its new name, holding what the program left, so nothing is written, and the old name,
    /// which is the same file, is not deleted
    #[test]
    fn a_case_only_rename_leaves_the_project_file() {
        let (project, tree, inventory) = staged(&[("pkg/Data.txt", "staged\n")]);
        fs::rename(
            tree.path().join("pkg/Data.txt"),
            tree.path().join("pkg/data.txt"),
        )
        .expect("rename");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/Data.txt"))
                .ok()
                .as_deref(),
            Some("staged\n"),
            "{outcomes:?}"
        );
        assert!(!outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
        if folds_case(project.path()) {
            assert_eq!(names(&project.path().join("pkg")), ["Data.txt"]);
            assert!(
                matches!(outcomes.as_slice(), [Outcome::NotDeleted(kept)] if kept.ends_with("pkg/Data.txt")),
                "{outcomes:?}"
            );
        }
    }

    /// a directory renamed only in case leaves every file in it in the project, as a file renamed
    /// only in case does
    #[test]
    fn a_case_only_rename_of_a_directory_leaves_its_files_in_the_project() {
        let (project, tree, inventory) =
            staged(&[("pkg/Sub/m.txt", "m\n"), ("pkg/Sub/n.txt", "n\n")]);
        fs::rename(tree.path().join("pkg/Sub"), tree.path().join("pkg/sub")).expect("rename");
        let outcomes = inventory.write_back();
        let read = |relative: &str| fs::read_to_string(project.path().join(relative)).ok();
        assert_eq!(
            read("pkg/Sub/m.txt").as_deref(),
            Some("m\n"),
            "{outcomes:?}"
        );
        assert_eq!(
            read("pkg/Sub/n.txt").as_deref(),
            Some("n\n"),
            "{outcomes:?}"
        );
        assert!(!outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
        if folds_case(project.path()) {
            assert_eq!(names(&project.path().join("pkg")), ["Sub"]);
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, Outcome::NotDeleted(_)))
                    .count(),
                2,
                "{outcomes:?}"
            );
        }
    }

    /// a case-only rename whose content the program also changed is, on a file system that folds
    /// case, a new file where the project already holds something else
    #[test]
    fn a_case_only_rename_with_a_change_is_not_written_over_the_project_file() {
        let (project, tree, inventory) = staged(&[("pkg/Data.txt", "staged\n")]);
        fs::rename(
            tree.path().join("pkg/Data.txt"),
            tree.path().join("pkg/data.txt"),
        )
        .expect("rename");
        fs::write(tree.path().join("pkg/data.txt"), "changed\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/Data.txt"))
                .ok()
                .as_deref(),
            Some("staged\n"),
            "{outcomes:?}"
        );
        if folds_case(project.path()) {
            assert!(
                outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, Outcome::Occupied(path) if path.ends_with("pkg/data.txt"))),
                "{outcomes:?}"
            );
            assert!(outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
        }
    }

    /// a name of 255 bytes, the most most file systems allow, is written back: the temporary
    /// file it is written through has a short name of its own
    #[test]
    fn a_file_with_the_longest_name_a_file_system_allows_is_written_back() {
        let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
        let name = "a".repeat(255);
        fs::write(tree.path().join("pkg").join(&name), "new\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg").join(&name))
                .ok()
                .as_deref(),
            Some("new\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Wrote(_)]),
            "{outcomes:?}"
        );
        assert_eq!(names(&project.path().join("pkg")), ["a.py", name.as_str()]);
    }

    /// the temporary file a write goes through is made only where nothing is, so a project file
    /// with the name it would have had is left as it is
    #[test]
    fn the_temporary_file_of_a_write_never_replaces_a_project_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let start = PARTIAL_FILES.load(Ordering::Relaxed);
        let taken: Vec<PathBuf> = (start..start + 8)
            .map(|count| {
                directory
                    .path()
                    .join(format!(".by-write-back.{}.{count}", std::process::id()))
            })
            .collect();
        for path in &taken {
            fs::write(path, "the project's\n").expect("write");
        }
        let (path, _file) = partial_file(directory.path()).expect("a partial file");
        assert!(!taken.contains(&path), "{path:?}");
        for path in &taken {
            assert_eq!(
                fs::read_to_string(path).ok().as_deref(),
                Some("the project's\n")
            );
        }
    }

    /// a file the program moved into a `__pycache__` directory is not carried back, as nothing
    /// there is, and its project file is not deleted
    #[test]
    fn a_file_the_program_moved_into_pycache_is_kept_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/data.txt", "precious\n")]);
        fs::create_dir(tree.path().join("pkg/__pycache__")).expect("mkdir");
        fs::rename(
            tree.path().join("pkg/data.txt"),
            tree.path().join("pkg/__pycache__/data.txt"),
        )
        .expect("rename");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/data.txt"))
                .ok()
                .as_deref(),
            Some("precious\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes.as_slice(), [Outcome::NotDeleted(_)]),
            "{outcomes:?}"
        );
    }

    /// a file the program replaced with a directory stays a file in the project, which leaves
    /// nowhere for what the program put in the directory to go
    #[test]
    fn a_file_the_program_replaced_with_a_directory_is_kept_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/data.json", "{}\n")]);
        fs::remove_file(tree.path().join("pkg/data.json")).expect("remove");
        fs::create_dir(tree.path().join("pkg/data.json")).expect("mkdir");
        fs::write(tree.path().join("pkg/data.json/x.txt"), "x\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/data.json"))
                .ok()
                .as_deref(),
            Some("{}\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::NotDeleted(kept), Outcome::Occupied(occupied)]
                    if kept.ends_with("pkg/data.json") && occupied.ends_with("pkg/data.json/x.txt")
            ),
            "{outcomes:?}"
        );
    }

    /// a directory the program replaced with a file stays a directory in the project, with its
    /// files, so the file is not written
    #[test]
    fn a_directory_the_program_replaced_with_a_file_is_kept_in_the_project() {
        let (project, tree, inventory) = staged(&[("pkg/sub/x.txt", "x\n")]);
        fs::remove_dir_all(tree.path().join("pkg/sub")).expect("remove");
        fs::write(tree.path().join("pkg/sub"), "now a file\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/sub/x.txt"))
                .ok()
                .as_deref(),
            Some("x\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::NotDeleted(kept), Outcome::Occupied(occupied)]
                    if kept.ends_with("pkg/sub/x.txt") && occupied.ends_with("pkg/sub")
            ),
            "{outcomes:?}"
        );
    }

    #[test]
    fn a_directory_the_program_deleted_is_kept_in_the_project() {
        let (project, tree, inventory) =
            staged(&[("pkg/sub/deep/x.txt", "x\n"), ("pkg/kept.txt", "kept\n")]);
        fs::remove_dir_all(tree.path().join("pkg/sub")).expect("remove");
        let outcomes = inventory.write_back();
        assert!(
            project.path().join("pkg/sub/deep/x.txt").is_file(),
            "{outcomes:?}"
        );
        assert!(!outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
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

    /// the program renamed a file to where the project already has another: the new name is
    /// refused, the old one is not deleted, and the tree holding the file under its new name is
    /// kept
    #[test]
    fn a_file_moved_where_it_cannot_be_written_is_kept_under_its_old_name() {
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
        assert_eq!(
            fs::read_to_string(project.path().join("pkg/b.txt"))
                .ok()
                .as_deref(),
            Some("theirs\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(
                outcomes.as_slice(),
                [Outcome::NotDeleted(kept), Outcome::Occupied(occupied)]
                    if occupied.ends_with("pkg/b.txt") && kept.ends_with("pkg/a.txt")
            ),
            "{outcomes:?}"
        );
        assert!(outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
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
        assert!(!outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
        let read = |relative: &str| fs::read_to_string(project.path().join(relative)).ok();
        assert_eq!(read("report.xml").as_deref(), Some("<report/>\n"));
        assert_eq!(read(".pytest_cache/v/lastfailed").as_deref(), Some("{}\n"));
        assert_eq!(read("src/app/new.txt").as_deref(), Some("new\n"));
    }

    /// a hidden directory is not staged, so a test runner's cache the project already has is not
    /// in the tree, and the program writes a new one there: one holding something else is not
    /// written over the project's, and the tree holding it is kept
    #[test]
    fn a_new_file_where_the_project_has_one_staging_left_out_is_left() {
        let (project, tree, inventory) = staged_src_layout(true);
        let nodeids = ".pytest_cache/v/cache/nodeids";
        fs::create_dir_all(project.path().join(".pytest_cache/v/cache")).expect("mkdir");
        fs::write(project.path().join(nodeids), "[\"a\"]\n").expect("write");
        fs::create_dir_all(tree.path().join(".pytest_cache/v/cache")).expect("mkdir");
        fs::write(tree.path().join(nodeids), "[\"a\", \"b\"]\n").expect("write");
        let outcomes = inventory.write_back();
        assert_eq!(
            fs::read_to_string(project.path().join(nodeids))
                .ok()
                .as_deref(),
            Some("[\"a\"]\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Occupied(path)] if path.ends_with(nodeids)),
            "{outcomes:?}"
        );
        assert!(outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
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

    /// a file the project gains while the program's new file of that name is being written, as
    /// an editor saving a file does while a large one is copied, is left as it is, and the
    /// program's is not written
    #[test]
    fn a_file_made_in_the_project_while_a_new_one_is_written_is_left() {
        for flagged_renames in [true, false] {
            let (project, tree, inventory) = staged(&[("pkg/a.py", "staged\n")]);
            fs::write(tree.path().join("pkg/new.txt"), "the program's\n").expect("write");
            let theirs = project.path().join("pkg/new.txt");
            let outcomes = write_back_racing(&inventory, flagged_renames, move |_| {
                fs::write(&theirs, "theirs\n").expect("write");
            });
            assert_eq!(
                fs::read_to_string(project.path().join("pkg/new.txt"))
                    .ok()
                    .as_deref(),
                Some("theirs\n"),
                "flagged renames: {flagged_renames}, {outcomes:?}"
            );
            assert!(
                matches!(outcomes.as_slice(), [Outcome::Occupied(path)] if path.ends_with("pkg/new.txt")),
                "flagged renames: {flagged_renames}, {outcomes:?}"
            );
            assert!(outcomes.iter().any(Outcome::keeps_tree), "{outcomes:?}");
            assert_eq!(names(&project.path().join("pkg")), ["a.py", "new.txt"]);
        }
    }

    /// a project file changed while the program's copy of it is being written back, whether
    /// written in place or saved over through a file renamed over it, as editors do, is left as
    /// it is, and the program's copy is not written
    #[test]
    fn a_project_file_changed_while_its_copy_is_written_is_left() {
        for (flagged_renames, renamed_over) in
            [(true, false), (true, true), (false, false), (false, true)]
        {
            let (project, tree, inventory) = staged(&[("pkg/a.txt", "staged\n")]);
            fs::write(tree.path().join("pkg/a.txt"), "the program's\n").expect("write");
            let theirs = project.path().join("pkg/a.txt");
            let outcomes = write_back_racing(&inventory, flagged_renames, move |_| {
                if renamed_over {
                    let saved = theirs.with_file_name("a.txt.saving");
                    fs::write(&saved, "theirs\n").expect("write");
                    fs::rename(&saved, &theirs).expect("rename");
                } else {
                    fs::write(&theirs, "theirs\n").expect("write");
                }
            });
            assert_eq!(
                fs::read_to_string(project.path().join("pkg/a.txt"))
                    .ok()
                    .as_deref(),
                Some("theirs\n"),
                "flagged renames: {flagged_renames}, renamed over: {renamed_over}, {outcomes:?}"
            );
            assert!(
                matches!(
                    outcomes.as_slice(),
                    [Outcome::ChangedMeanwhile { project, deleted: false }] if project.ends_with("pkg/a.txt")
                ),
                "flagged renames: {flagged_renames}, renamed over: {renamed_over}, {outcomes:?}"
            );
            assert_eq!(names(&project.path().join("pkg")), ["a.txt"]);
        }
    }

    /// without a rename that refuses to replace or an exchange, a new file is linked into place
    /// and a project file still holding what was staged is moved aside for the copy
    #[test]
    fn files_are_written_back_without_flagged_renames() {
        let (project, tree, inventory) = staged(&[("pkg/a.txt", "staged\n")]);
        fs::write(tree.path().join("pkg/a.txt"), "changed\n").expect("write");
        fs::write(tree.path().join("pkg/new.txt"), "new\n").expect("write");
        let outcomes = write_back_racing(&inventory, false, |_| {});
        let read = |name: &str| fs::read_to_string(project.path().join("pkg").join(name)).ok();
        assert_eq!(read("a.txt").as_deref(), Some("changed\n"), "{outcomes:?}");
        assert_eq!(read("new.txt").as_deref(), Some("new\n"), "{outcomes:?}");
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Wrote(_), Outcome::Wrote(_)]),
            "{outcomes:?}"
        );
        assert_eq!(names(&project.path().join("pkg")), ["a.txt", "new.txt"]);
    }

    /// what the program wrote is in the temporary file before it is put in place, so the file is
    /// made readable only by its owner, whatever it is given once it is whole
    #[cfg(unix)]
    #[test]
    fn the_temporary_file_of_a_write_is_made_readable_only_by_its_owner() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (path, _file) = partial_file(directory.path()).expect("a partial file");
        assert_eq!(mode(&path) & 0o077, 0, "{:o}", mode(&path));
    }

    /// a copy does not keep every permission of the file it copies: macOS's drops the setuid
    /// bit. what a project file is expected to hold is what it held when it was staged, not
    /// what its copy was given, so the program's change to it is written back rather than taken
    /// for a change to the project, and a permission the program did not change is kept
    #[cfg(unix)]
    #[test]
    fn a_setuid_file_the_program_changed_is_written_back() {
        let (project, tree, inventory) = staged_with(&[("pkg/run.sh", "echo hi\n")], |path| {
            set_mode(path, 0o4755);
        });
        let copy = tree.path().join("pkg/run.sh");
        let staged_mode = mode(&copy);
        fs::write(&copy, "echo changed\n").expect("write");
        let changed_mode = mode(&copy);
        let outcomes = inventory.write_back();
        let project_file = project.path().join("pkg/run.sh");
        assert_eq!(
            fs::read_to_string(&project_file).ok().as_deref(),
            Some("echo changed\n"),
            "{outcomes:?}"
        );
        assert!(
            matches!(outcomes.as_slice(), [Outcome::Wrote(path)] if path.ends_with("pkg/run.sh")),
            "{outcomes:?}"
        );
        // a write clears the setuid bit on some systems, which is then the program's change
        let kept = if changed_mode == staged_mode {
            0o4755
        } else {
            changed_mode
        };
        assert_eq!(mode(&project_file), kept);
    }
}
