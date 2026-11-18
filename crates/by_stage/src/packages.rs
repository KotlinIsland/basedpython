//! the initializers of the packages basedpython has without one
//!
//! basedpython does not need an `__init__.by`: a directory below a package root that holds a
//! basedpython module is a regular package, and python needs an `__init__.py` to agree. the
//! checker decides which directories those are (see
//! [`ty_module_resolver::is_directory_package_at`]) and the build writes each one it lays a file
//! out in an empty initializer, so the tree python imports has the packages the checker read

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ruff_db::Db as _;
use ruff_db::system::SystemPath;
use ty_module_resolver::PACKAGE_INITIALIZERS;
use ty_project::{Db as _, ProjectDatabase};

use crate::staging::Staging;

/// whether the project directory `directory` is a package once built: it has an initializer of
/// its own, or it is a directory package the build writes one for
pub(crate) fn is_package(db: &ProjectDatabase, directory: &Path) -> bool {
    let Some(directory) = SystemPath::from_std_path(directory) else {
        return false;
    };
    let system = db.system();
    PACKAGE_INITIALIZERS
        .iter()
        .any(|initializer| system.is_file(&directory.join(initializer)))
        || is_directory_package(db, directory)
}

fn is_directory_package(db: &ProjectDatabase, directory: &SystemPath) -> bool {
    ty_module_resolver::is_directory_package_at(
        db,
        db.project().program(db).resolver_environment(db),
        directory,
    )
}

/// write the empty initializer of every directory package that `staging` laid a file out in
///
/// every file the staging laid out from a source lands at the source's path below its module
/// root, so each directory between the file and the top of the tree is the build's copy of the
/// matching directory of the project. two module roots holding a directory of the same name lay
/// both out into one, which is a package when either is, and already has an initializer when
/// either brought one
pub fn stage_package_initializers(
    db: &ProjectDatabase,
    staging: &mut Staging,
) -> anyhow::Result<()> {
    let staged: BTreeSet<&Path> = staging
        .entries()
        .map(|(destination, _)| destination)
        .collect();
    let mut directories: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    for (destination, source) in staging.laid_out() {
        for (built, project) in destination
            .ancestors()
            .skip(1)
            .zip(source.ancestors().skip(1))
        {
            if built.as_os_str().is_empty() {
                break;
            }
            directories
                .entry(built.to_path_buf())
                .or_default()
                .insert(project.to_path_buf());
        }
    }
    let packages: Vec<(PathBuf, PathBuf)> = directories
        .into_iter()
        .filter(|(built, _)| !staged.contains(built.join("__init__.py").as_path()))
        .filter_map(|(built, projects)| {
            let package = projects.into_iter().find(|project| {
                SystemPath::from_std_path(project)
                    .is_some_and(|project| is_directory_package(db, project))
            })?;
            Some((built, package))
        })
        .collect();

    for (built, project) in packages {
        staging.write_initializer(&built.join("__init__.py"), &project)?;
    }
    Ok(())
}

/// the directory packages that `files`, modules of the project, are in, at any depth below the
/// module root holding each: the directories python needs an `__init__.py` in to read them as the
/// packages the checker did
pub fn directory_packages<'a>(
    db: &ProjectDatabase,
    roots: &[PathBuf],
    files: impl IntoIterator<Item = &'a Path>,
) -> BTreeSet<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut packages = BTreeSet::new();
    for file in files {
        let Some(root) = roots.iter().find(|root| file.starts_with(root)) else {
            continue;
        };
        for directory in file.ancestors().skip(1) {
            if directory == root.as_path() || !seen.insert(directory.to_path_buf()) {
                break;
            }
            if SystemPath::from_std_path(directory)
                .is_some_and(|directory| is_directory_package(db, directory))
            {
                packages.insert(directory.to_path_buf());
            }
        }
    }
    packages
}
