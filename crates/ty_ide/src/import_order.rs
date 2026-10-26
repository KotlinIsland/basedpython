//! The isort configuration buff sorts a file's imports with.
//!
//! An import added to a file is placed where `buff check --select I001 --fix` would put it, so
//! adding one never leaves a block buff then reports as unsorted. buff reads its configuration
//! the way ruff does: from the closest `ruff.toml`, `.ruff.toml` or `pyproject.toml` with a
//! `[tool.ruff]` table above the file, then from the user's own configuration, and otherwise its
//! defaults with the `target-version` the closest `requires-python` implies. This resolves it
//! with the same `ruff_workspace` code, so the two agree on sections, known modules and order.
//!
//! The files are read from disk, not through the database: they are not Python sources and the
//! server does not watch them. A resolved configuration is cached against the revision (the
//! modification time) of the file it was read from, so an edit to it is seen by the next import
//! added.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use ruff_db::file_revision::FileRevision;
use ruff_db::parsed::ParsedModuleRef;
use ruff_db::source::source_text;
use ruff_db::system::{System, SystemPath};
use ruff_linter::package::PackageRoot;
use ruff_linter::packaging::detect_package_root;
use ruff_linter::rules::isort::placement::{ImportOrder, ImportPosition, sort_import_statements};
use ruff_python_ast::{PySourceType, PythonVersion};
use ruff_python_importer::{ImportOrdering, SortedImport};
use ruff_workspace::configuration::Configuration;
use ruff_workspace::pyproject::{
    find_fallback_target_version, find_settings_toml, find_user_settings_toml,
};
use ruff_workspace::resolver::{
    ConfigurationOrigin, ConfigurationTransformer, resolve_root_settings,
};
use rustc_hash::FxHashMap;
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::importer::Importer;

/// An importer for `file` that adds each import where buff's isort configuration sorts it, read
/// the first time an import is placed.
pub(crate) fn importer<'a>(
    db: &'a dyn Db,
    file: ProgramFile<'a>,
    parsed: &'a ParsedModuleRef,
) -> Importer<'a> {
    Importer::new(db, file, parsed).sorted_by(move || IsortConfig::for_program_file(db, file))
}

/// The isort settings that apply to one file, and what isort needs to know about the file to
/// categorize its imports.
#[derive(Debug)]
pub(crate) struct IsortConfig {
    settings: Arc<ruff_workspace::Settings>,
    package_root: Option<PathBuf>,
    target_version: PythonVersion,
}

impl IsortConfig {
    /// The configuration that applies to `file`, or `None` for a file that is not on disk or is
    /// a notebook.
    fn for_program_file(db: &dyn Db, file: ProgramFile<'_>) -> Option<Self> {
        if source_text(db, file.file(db)).is_notebook() {
            return None;
        }
        let path = file.file(db).path(db).as_system_path()?;
        Some(Self::for_file(
            db.system(),
            path.as_std_path(),
            db.project().root(db).as_std_path(),
        ))
    }

    /// The configuration that applies to the file at `path`. `project_root` stands in for the
    /// directory buff resolves `src` against when no configuration file is found.
    pub(crate) fn for_file(system: &dyn System, path: &Path, project_root: &Path) -> Self {
        Self::new(
            system,
            path.parent().unwrap_or(path),
            Some(path),
            project_root,
        )
    }

    /// the configuration that applies to a file in `directory`, without a per-file target
    /// version, which only a file's path can match
    pub(crate) fn for_directory(
        system: &dyn System,
        directory: &Path,
        project_root: &Path,
    ) -> Self {
        Self::new(system, directory, None, project_root)
    }

    fn new(
        system: &dyn System,
        directory: &Path,
        file: Option<&Path>,
        project_root: &Path,
    ) -> Self {
        let settings = settings_for(system, directory, project_root);
        let package_root = detect_package_root(directory, &settings.linter.namespace_packages)
            .map(Path::to_path_buf);
        let target_version = file
            .map_or(settings.linter.unresolved_target_version, |file| {
                settings.linter.resolve_target_version(file)
            })
            .linter_version();
        Self {
            settings,
            package_root,
            target_version,
        }
    }

    /// `source`, a run of import statements, as buff's `I001` fix sorts and writes them, or
    /// `None` for source with anything else in it, a comment among them.
    pub(crate) fn sort(&self, source: &str, source_type: PySourceType) -> Option<String> {
        sort_import_statements(
            source,
            &self.settings.linter,
            self.package_root.as_deref().map(PackageRoot::root),
            source_type,
            self.target_version,
        )
    }

    pub(crate) fn order(&self) -> ImportOrder<'_> {
        ImportOrder::new(
            &self.settings.linter.isort,
            &self.settings.linter.src,
            self.package_root.as_deref().map(PackageRoot::root),
            self.target_version,
        )
    }
}

impl ImportOrdering for IsortConfig {
    type Position = ImportPosition<'static>;

    fn position(&self, import: &SortedImport<'_>) -> Self::Position {
        self.order().position(import).into_owned()
    }

    fn section(&self, position: &Self::Position) -> usize {
        position.section()
    }

    fn blank_lines_between(&self, before: &Self::Position, after: &Self::Position) -> usize {
        self.order().blank_lines_between(before, after)
    }

    fn section_heading(&self, position: &Self::Position) -> Option<&str> {
        self.order().section_heading(position)
    }

    fn compare_members(&self, a: (&str, Option<&str>), b: (&str, Option<&str>)) -> Ordering {
        self.order().compare_members(a, b)
    }

    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool {
        self.order().imports_member_alone(module, asname)
    }
}

struct Unchanged;

impl ConfigurationTransformer for Unchanged {
    fn transform(&self, config: Configuration) -> Configuration {
        config
    }
}

/// A resolved configuration file and the revision it was resolved at.
struct Resolved {
    revision: Option<FileRevision>,
    settings: Arc<ruff_workspace::Settings>,
}

static RESOLVED: LazyLock<Mutex<FxHashMap<PathBuf, Resolved>>> = LazyLock::new(Mutex::default);

fn settings_for(
    system: &dyn System,
    directory: &Path,
    project_root: &Path,
) -> Arc<ruff_workspace::Settings> {
    let project = find_settings_toml(directory).ok().flatten();
    if let Some(path) = &project
        && let Some(settings) = resolved(system, path, ConfigurationOrigin::Ancestor)
    {
        return settings;
    }
    // the user's own configuration is left out of tests, which would otherwise depend on the
    // machine they run on
    if !cfg!(test)
        && let Some(path) = find_user_settings_toml()
        && let Some(settings) = resolved(system, &path, ConfigurationOrigin::UserSettings)
    {
        return settings;
    }
    let configuration = Configuration {
        target_version: find_fallback_target_version(directory).map(Into::into),
        ..Configuration::default()
    };
    Arc::new(
        configuration
            .into_settings(project_root)
            .unwrap_or_default(),
    )
}

/// The settings resolved from the configuration file at `path`, or `None` when it cannot be
/// read or is invalid, as buff then falls back on the next configuration too.
fn resolved(
    system: &dyn System,
    path: &Path,
    origin: ConfigurationOrigin,
) -> Option<Arc<ruff_workspace::Settings>> {
    let revision = SystemPath::from_std_path(path)
        .and_then(|path| system.path_metadata(path).ok())
        .map(|metadata| metadata.revision());
    let mut cache = RESOLVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(resolved) = cache.get(path)
        && resolved.revision == revision
    {
        return Some(resolved.settings.clone());
    }
    let settings = Arc::new(resolve_root_settings(path, &Unchanged, origin).ok()?);
    cache.insert(
        path.to_path_buf(),
        Resolved {
            revision,
            settings: settings.clone(),
        },
    );
    Some(settings)
}
