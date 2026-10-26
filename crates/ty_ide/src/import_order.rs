//! the isort configuration buff sorts a file's imports with
//!
//! an import added to a file is placed where `buff check --select I001 --fix` would put it, so
//! adding one never leaves a block buff then reports as unsorted. buff reads its configuration
//! the way ruff does: from the closest `ruff.toml`, `.ruff.toml` or `pyproject.toml` with a
//! `[tool.ruff]` table above the file, then from the user's own configuration, and otherwise its
//! defaults with the `target-version` the closest `requires-python` implies. this resolves it
//! with the same `ruff_workspace` code, so the two agree on sections, known modules and order
//!
//! the configuration files are read through the database's [`System`], as everything else the
//! project reads is, but not tracked by it: they need not be in a directory the server watches,
//! as the user's own configuration is not. a resolved configuration is cached instead with the
//! revision of every file resolving it looked at, including the ones it found missing, and used
//! again only while each is unchanged, so an edit to a configuration file, to a file it
//! `extend`s, or a configuration file created closer to the file is seen by the next import
//! added
//!
//! whether a module is the project's own is decided, as buff decides it, by looking for it in
//! the configured `src` directories. that lookup goes through the database's files, which keep
//! what they found until the server reports a change, so a completion offering hundreds of
//! imports doesn't look at the file system for each

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use ruff_db::file_revision::FileRevision;
use ruff_db::files::{FileError, system_path_to_file};
use ruff_db::parsed::ParsedModuleRef;
use ruff_db::system::{System, SystemPath, SystemPathBuf};
use ruff_linter::package::PackageRoot;
use ruff_linter::packaging::{PackageFiles, detect_package_root_in};
use ruff_linter::rules::isort::placement::{ImportOrder, sort_import_statements_in};
use ruff_linter::settings::LinterSettings;
use ruff_python_ast::{PySourceType, PythonVersion};
use ruff_workspace::configuration::Configuration;
use ruff_workspace::files::{ConfigurationFiles, GlobResult, glob_by_listing};
use ruff_workspace::pyproject::{
    find_fallback_target_version_in, find_settings_toml_in, user_settings_toml_in,
};
use ruff_workspace::resolver::{
    ConfigurationOrigin, ConfigurationTransformer, resolve_root_settings_in,
};
use rustc_hash::FxHashMap;
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::importer::Importer;

/// an importer for `file` that adds each import where buff's isort configuration sorts it, read
/// the first time an import is placed
pub(crate) fn importer<'a>(
    db: &'a dyn Db,
    file: ProgramFile<'a>,
    parsed: &'a ParsedModuleRef,
) -> Importer<'a> {
    Importer::new(db, file, parsed).sorted_by(move || order(db, file))
}

/// the order buff's isort configuration sorts the imports of `file` in, or `None` for a file
/// that is not on disk. a notebook's cells are each sorted in this order on their own
fn order<'a>(db: &'a dyn Db, file: ProgramFile<'a>) -> Option<ImportOrder<ProjectFiles<'a>>> {
    let path = file.file(db).path(db).as_system_path()?.as_std_path();
    let files = ProjectFiles(db);
    let config = IsortConfig::new(
        db.system(),
        &files,
        path.parent().unwrap_or(path),
        Some(path),
        db.project().root(db).as_std_path(),
    );
    Some(ImportOrder::new(
        config.settings,
        config.package_root,
        config.target_version,
        files,
    ))
}

/// the isort settings that apply to one file, and what isort needs to know about the file to
/// categorize its imports
pub(crate) struct IsortConfig<'a> {
    settings: Arc<LinterSettings>,
    package_root: Option<PathBuf>,
    target_version: PythonVersion,
    system: &'a dyn System,
}

impl<'a> IsortConfig<'a> {
    /// the configuration that applies to the file at `path`. `project_root` stands in for the
    /// directory buff resolves `src` against when no configuration file is found
    pub(crate) fn for_file(system: &'a dyn System, path: &Path, project_root: &Path) -> Self {
        Self::new(
            system,
            &SystemFiles(system),
            path.parent().unwrap_or(path),
            Some(path),
            project_root,
        )
    }

    /// the configuration that applies to a file in `directory`, without a per-file target
    /// version, which only a file's path can match
    pub(crate) fn for_directory(
        system: &'a dyn System,
        directory: &Path,
        project_root: &Path,
    ) -> Self {
        Self::new(system, &SystemFiles(system), directory, None, project_root)
    }

    /// the configuration of a file in `directory`, its package found in `files`
    fn new(
        system: &'a dyn System,
        files: &dyn PackageFiles,
        directory: &Path,
        file: Option<&Path>,
        project_root: &Path,
    ) -> Self {
        let settings = settings_for(system, directory, project_root);
        let package_root = detect_package_root_in(files, directory, &settings.namespace_packages)
            .map(Path::to_path_buf);
        let target_version = file
            .map_or(settings.unresolved_target_version, |file| {
                settings.resolve_target_version(file)
            })
            .linter_version();
        Self {
            settings,
            package_root,
            target_version,
            system,
        }
    }

    /// the linter settings buff reads, and the package root of the file they are for
    #[cfg(test)]
    pub(crate) fn settings(&self) -> (&LinterSettings, Option<&Path>) {
        (&self.settings, self.package_root.as_deref())
    }

    /// `source`, a run of import statements, as buff's `I001` fix sorts and writes them, or
    /// `None` for source with anything else in it, a comment among them
    pub(crate) fn sort(&self, source: &str, source_type: PySourceType) -> Option<String> {
        sort_import_statements_in(
            source,
            &self.settings,
            &SystemFiles(self.system),
            self.package_root.as_deref().map(PackageRoot::root),
            source_type,
            self.target_version,
        )
    }
}

/// the files of the database, which remember what they found at a path until the server reports
/// a change to it
pub(crate) struct ProjectFiles<'db>(&'db dyn Db);

impl PackageFiles for ProjectFiles<'_> {
    fn is_file(&self, path: &Path) -> bool {
        SystemPath::from_std_path(path)
            .is_some_and(|path| system_path_to_file(self.0, path).is_ok())
    }

    fn is_directory(&self, path: &Path) -> bool {
        SystemPath::from_std_path(path).is_some_and(|path| {
            matches!(
                system_path_to_file(self.0, path),
                Err(FileError::IsADirectory)
            )
        })
    }
}

/// the files of a [`System`], looked at each time they are asked about
struct SystemFiles<'a>(&'a dyn System);

impl PackageFiles for SystemFiles<'_> {
    fn is_file(&self, path: &Path) -> bool {
        SystemPath::from_std_path(path).is_some_and(|path| self.0.is_file(path))
    }

    fn is_directory(&self, path: &Path) -> bool {
        SystemPath::from_std_path(path).is_some_and(|path| self.0.is_directory(path))
    }
}

/// the configuration files of a [`System`], recording the revision of every file looked at:
/// `None` for one that is missing
struct RecordedFiles<'a> {
    system: &'a dyn System,
    revisions: RefCell<Vec<(PathBuf, Option<FileRevision>)>>,
}

impl RecordedFiles<'_> {
    fn revision(&self, path: &Path) -> Option<FileRevision> {
        let revision = file_revision(self.system, path);
        self.revisions
            .borrow_mut()
            .push((path.to_path_buf(), revision));
        revision
    }
}

impl ConfigurationFiles for RecordedFiles<'_> {
    fn is_file(&self, path: &Path) -> bool {
        self.revision(path).is_some()
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.revision(path);
        let path = SystemPath::from_std_path(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a UTF-8 path"))?;
        self.system.read_to_string(path)
    }

    fn glob(&self, pattern: &str) -> GlobResult {
        let list = |directory: &Path| {
            SystemPath::from_std_path(directory)
                .and_then(|directory| self.system.read_directory(directory).ok())
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| {
                    let is_directory = entry.file_type().is_directory();
                    (entry.into_path().into_std_path_buf(), is_directory)
                })
                .collect()
        };
        let exists = |path: &Path| {
            SystemPath::from_std_path(path).is_some_and(|path| self.system.path_exists(path))
        };
        glob_by_listing(pattern, &list, &exists)
    }
}

/// the revision of the file at `path`, or `None` when there is none
fn file_revision(system: &dyn System, path: &Path) -> Option<FileRevision> {
    let metadata = system
        .path_metadata(SystemPath::from_std_path(path)?)
        .ok()?;
    metadata.file_type().is_file().then(|| metadata.revision())
}

/// sets the target version the closest `requires-python` implies, which takes precedence over
/// the user's own configuration, as buff gives it
struct FallbackTargetVersion(Option<ruff_linter::settings::types::PythonVersion>);

impl ConfigurationTransformer for FallbackTargetVersion {
    fn transform(&self, mut configuration: Configuration) -> Configuration {
        if let Some(fallback) = self.0 {
            configuration.target_version = Some(fallback.into());
        }
        configuration
    }
}

struct Unchanged;

impl ConfigurationTransformer for Unchanged {
    fn transform(&self, config: Configuration) -> Configuration {
        config
    }
}

/// the settings resolved for a directory, and the revision of every file resolving them looked
/// at
struct Resolved {
    revisions: Vec<(PathBuf, Option<FileRevision>)>,
    settings: Arc<LinterSettings>,
}

/// the resolved settings, by the directory, project root and user configuration directory they
/// were resolved for
static RESOLVED: LazyLock<Mutex<FxHashMap<ResolvedKey, Resolved>>> = LazyLock::new(Mutex::default);

type ResolvedKey = (PathBuf, PathBuf, Option<SystemPathBuf>);

/// the linter settings buff reads for a file in `directory`, from the cache while no file
/// resolving them looked at has changed
fn settings_for(system: &dyn System, directory: &Path, project_root: &Path) -> Arc<LinterSettings> {
    let user_config_directory = system.user_config_directory();
    let key = (
        directory.to_path_buf(),
        project_root.to_path_buf(),
        user_config_directory.clone(),
    );
    {
        let cache = RESOLVED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(resolved) = cache.get(&key)
            && resolved
                .revisions
                .iter()
                .all(|(path, revision)| file_revision(system, path) == *revision)
        {
            return resolved.settings.clone();
        }
    }
    let files = RecordedFiles {
        system,
        revisions: RefCell::default(),
    };
    let settings = Arc::new(resolve(
        &files,
        directory,
        project_root,
        user_config_directory.as_deref(),
    ));
    RESOLVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key,
            Resolved {
                revisions: files.revisions.into_inner(),
                settings: settings.clone(),
            },
        );
    settings
}

/// the linter settings buff reads for a file in `directory`, from the configuration files of
/// `files`. a configuration file that can't be read or is invalid is passed over for the next,
/// as buff falls back on the next configuration too
fn resolve(
    files: &RecordedFiles,
    directory: &Path,
    project_root: &Path,
    user_config_directory: Option<&SystemPath>,
) -> LinterSettings {
    if let Some(path) = find_settings_toml_in(files, directory).ok().flatten()
        && let Ok(settings) = resolve_root_settings_in(
            files,
            project_root,
            &path,
            &Unchanged,
            ConfigurationOrigin::Ancestor,
        )
    {
        return settings.linter;
    }
    // the user's own configuration resolves its paths against the current directory buff runs
    // in, which the project root stands in for
    if let Some(config_directory) = user_config_directory
        && let Some(path) = user_settings_toml_in(files, config_directory.as_std_path())
        && let Ok(settings) = resolve_root_settings_in(
            files,
            project_root,
            &path,
            &FallbackTargetVersion(find_fallback_target_version_in(files, project_root)),
            ConfigurationOrigin::UserSettings,
        )
    {
        return settings.linter;
    }
    let configuration = Configuration {
        target_version: find_fallback_target_version_in(files, directory).map(Into::into),
        ..Configuration::default()
    };
    configuration
        .into_settings(project_root)
        .map(|settings| settings.linter)
        .unwrap_or_else(|_| LinterSettings::new(project_root))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_db::system::{SystemPathBuf, TestSystem};

    use super::IsortConfig;

    /// the line length buff reads for `/project/main.py` of `system`
    fn line_length(system: &TestSystem) -> usize {
        IsortConfig::for_file(system, Path::new("/project/main.py"), Path::new("/project"))
            .settings
            .line_length
            .value()
            .into()
    }

    fn system(files: &[(&str, &str)]) -> TestSystem {
        let system = TestSystem::default();
        system
            .memory_file_system()
            .write_files_all(files.iter().copied())
            .expect("files to be written");
        system
    }

    /// a configuration file `extend`s another: an edit to the other is seen
    #[test]
    fn edit_to_an_extended_file_is_seen() {
        let system = system(&[
            ("/project/main.py", ""),
            ("/project/ruff.toml", "extend = \"../base.toml\"\n"),
            ("/base.toml", "line-length = 40\n"),
        ]);
        assert_eq!(line_length(&system), 40);
        system
            .memory_file_system()
            .write_file("/base.toml", "line-length = 50\n")
            .expect("file to be written");
        assert_eq!(line_length(&system), 50);
    }

    /// a configuration file created where none was found is read
    #[test]
    fn configuration_created_later_is_read() {
        let system = system(&[("/project/main.py", "")]);
        assert_eq!(line_length(&system), 88);
        system
            .memory_file_system()
            .write_file("/project/ruff.toml", "line-length = 40\n")
            .expect("file to be written");
        assert_eq!(line_length(&system), 40);
    }

    /// the user's own configuration is read from the system's configuration directory
    #[test]
    fn user_configuration_from_the_system() {
        let system = system(&[
            ("/project/main.py", ""),
            ("/config/ruff/ruff.toml", "line-length = 60\n"),
        ]);
        assert_eq!(line_length(&system), 88);
        system
            .in_memory()
            .set_user_configuration_directory(Some(SystemPathBuf::from("/config")));
        assert_eq!(line_length(&system), 60);
    }

    /// `src` globs are matched against the system's files: a module in a matched directory is
    /// the project's own
    #[test]
    fn src_globs_match_the_systems_files() {
        let system = system(&[
            ("/project/main.py", ""),
            ("/project/ruff.toml", "src = [\"libs/*\"]\n"),
            ("/project/libs/one/shapes.py", ""),
            ("/project/libs/two/colours.py", ""),
        ]);
        let config = IsortConfig::for_file(
            &system,
            Path::new("/project/main.py"),
            Path::new("/project"),
        );
        assert_eq!(
            config.settings.src,
            [
                Path::new("/project/libs/one").to_path_buf(),
                Path::new("/project/libs/two").to_path_buf(),
            ]
        );
    }
}
