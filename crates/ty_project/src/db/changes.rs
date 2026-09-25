use crate::build_output::{is_build_manifest, is_within_build_output};
use crate::db::{Db, ProjectDatabase};
use crate::metadata::CONFIG_FILE_NAMES;
use crate::script::script_tag;
use crate::watch::{ChangeEvent, CreatedKind, DeletedKind};
use crate::{ProjectMetadata, ProjectReloadResult};
use std::collections::BTreeSet;

use crate::walk::{ProjectFilesWalker, create_walker_builder};
use ruff_db::Db as _;
use ruff_db::files::{File, Files, system_path_to_file};
use ruff_db::system::{SystemPath, SystemPathBuf};
use rustc_hash::FxHashSet;
use ty_python_core::program::FallibleStrategy;

/// Represents the result of applying changes to the project database.
pub struct ChangeResult {
    project_changed: bool,
    project_sync_path: Option<SystemPathBuf>,
    custom_stdlib_changed: bool,
    changed_files: ChangedFiles,
    database_changed: bool,
}

impl ChangeResult {
    /// Returns `true` if the project structure has changed.
    pub fn project_changed(&self) -> bool {
        self.project_changed
    }

    /// The directory whose uv project metadata needs refreshing, if any.
    ///
    /// This may be an ancestor of the previous project root if that directory was deleted.
    pub fn project_sync_path(&self) -> Option<&SystemPath> {
        self.project_sync_path.as_deref()
    }

    /// whether applying the changes changed anything in the database
    ///
    /// when it did not, every query answers as it did before the changes, so nothing a client
    /// has been shown can be out of date. most file system events are like that: a write to a
    /// file nothing has read, to a directory nothing has listed, to a build's output or to an
    /// ignored directory
    pub fn database_changed(&self) -> bool {
        self.database_changed
    }

    /// Returns `true` if the custom stdlib's VERSIONS file has changed.
    pub fn custom_stdlib_changed(&self) -> bool {
        self.custom_stdlib_changed
    }

    /// Returns the scripts whose environments may need synchronization after these file events.
    ///
    /// Returns no scripts if the project was unindexed when the changes were applied.
    /// Otherwise, only includes scripts in [`crate::Project::files`], reindexing if needed.
    ///
    /// The result may include scripts with unsaved changes to their PEP 723 metadata.
    /// Callers must defer environment synchronization until those changes are saved:
    /// uv reads the file from disk, not the editor buffer.
    pub fn scripts_to_synchronize(&self, db: &dyn Db) -> Vec<File> {
        match &self.changed_files {
            ChangedFiles::Unindexed => Vec::new(),
            ChangedFiles::Known(changed_files) => {
                if changed_files.is_empty() {
                    return Vec::new();
                }

                let indexed = db.project().files(db);
                changed_files
                    .intersection(indexed.scripts())
                    .copied()
                    .collect()
            }
            ChangedFiles::Unknown => db.project().files(db).scripts().iter().copied().collect(),
        }
    }
}

enum ChangedFiles {
    /// The project was unindexed when the changes were applied.
    Unindexed,
    /// The set of files that were created, opened, or modified. This set may be empty.
    ///
    /// For example, editing `main.py` includes that file. Files excluded by path or ignore rules
    /// are not listed.
    ///
    /// The project's files are indexed and reflect these changes when
    /// [`ProjectDatabase::apply_changes`] returns.
    Known(FxHashSet<File>),
    /// The project was indexed, but the set of changed files is unknown.
    ///
    /// For example, a directory event may represent many new files, or editing `.gitignore`
    /// or `src.exclude` may change which files belong to the project.
    Unknown,
}

impl ChangedFiles {
    fn mark_unknown(&mut self) {
        if matches!(self, Self::Known(_)) {
            *self = Self::Unknown;
        }
    }
}

impl ProjectDatabase {
    /// Applies file changes to the database.
    ///
    /// Any required uv synchronization is returned in [`ChangeResult`] for the caller to schedule.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn apply_changes(&mut self, changes: &[ChangeEvent]) -> ChangeResult {
        let project = self.project();
        let project_root = project.root(self).to_path_buf();
        let configuration_paths = ConfigurationPaths::from_metadata(project.metadata(self));
        let program = self.project().program(self);
        let custom_stdlib_versions_path = program
            .custom_stdlib_search_path(self)
            .map(|path| path.join("VERSIONS"));

        let mut result = ChangeResult {
            project_changed: false,
            project_sync_path: None,
            custom_stdlib_changed: false,
            changed_files: if project.file_set(self).is_lazy() {
                ChangedFiles::Unindexed
            } else {
                ChangedFiles::Known(FxHashSet::default())
            },
            // every write below reports whether it set anything, and each one that did sets this
            database_changed: false,
        };
        // Paths whose project files should be discovered incrementally.
        let mut added_paths = BTreeSet::default();

        // Deduplicate the `sync` calls. Many file watchers emit multiple events for the same path.
        let mut synced_files = FxHashSet::default();
        let mut sync_recursively = BTreeSet::default();
        // A non-file delete may be a deleted directory or an ambiguous LSP delete for a path
        // that no longer exists. Handle it recursively to keep Salsa's file state in sync.
        let mut removed_paths = BTreeSet::default();
        let mut reload_project = false;
        let mut reload_project_files = false;
        // TODO: This should be removed once the incremental checker is ported
        // over to the `ignore` crate, since the `ignore` crate will respect
        // the settings provided in `create_walker`. ---AG
        let respect_ignore_files = project.settings(self).src().respect_ignore_files;
        // also what decides what the walk would skip as a build's output, for one changed path at
        // a time. a build writes a copy of every source it read, and none of it is the project's:
        // the writes are synced like any file's, but they neither join the project nor set off a
        // walk
        let walk_roots = project.included_paths_or_root(self).to_vec();
        let ignore_walk_roots = respect_ignore_files.then_some(walk_roots.as_slice());
        let mut ignore_files = ignore_walk_roots.and_then(|walk_roots| {
            Some(create_walker_builder(self, walk_roots)?.incremental_matcher())
        });

        for change in changes {
            tracing::debug!("Handling file watcher change event: {:?}", change);

            if let Some(path) = change.system_path() {
                if configuration_paths.is_configuration(path, &project_root) {
                    result.database_changed |= File::sync_path(self, path);
                    reload_project = true;

                    continue;
                }

                // a build's manifest appearing or going away turns the directory into an output or
                // back into part of the project, as an ignore file saying `*` would. walking it
                // again settles which: a walk skips an output. a manifest a build only rewrites
                // changes nothing
                if is_build_manifest(path, &walk_roots)
                    && matches!(
                        change,
                        ChangeEvent::Created { .. } | ChangeEvent::Deleted { .. }
                    )
                {
                    result.database_changed |= File::sync_path(self, path);
                    if let Some(directory) = path.parent()
                        && !is_within_build_output(self.system(), directory, &walk_roots)
                        && project.is_directory_included(self, directory)
                    {
                        tracing::debug!(
                            manifest = %path,
                            directory = %directory,
                            "Queueing project-file reindex for a build output's manifest"
                        );
                        removed_paths.insert(directory.to_path_buf());
                        result.changed_files.mark_unknown();
                        if self.system().is_directory(directory) {
                            added_paths.insert(directory.to_path_buf());
                        }
                    }

                    continue;
                }

                if is_ignore_file(path) && project.settings(self).src().respect_ignore_files {
                    result.database_changed |= File::sync_path(self, path);
                    if let Some(directory) = path.parent() {
                        if project
                            .included_paths_or_root(self)
                            .iter()
                            .any(|included_path| included_path.starts_with(directory))
                        {
                            tracing::debug!(
                                ignore_file = %path,
                                directory = %directory,
                                "Reloading project files for changed ignore file at or above included path"
                            );
                            reload_project_files = true;
                        } else if project.is_directory_included(self, directory)
                            && !is_within_build_output(self.system(), path, &walk_roots)
                            && ignore_files.as_mut().is_none_or(|ignore_files| {
                                !ignore_files.is_ignored(directory, true)
                            })
                        {
                            tracing::debug!(
                                ignore_file = %path,
                                directory = %directory,
                                "Queueing project-file reindex for changed ignore file"
                            );

                            removed_paths.insert(directory.to_path_buf());
                            result.changed_files.mark_unknown();

                            if self.system().path_exists(directory) {
                                added_paths.insert(directory.to_path_buf());
                            }
                        } else {
                            tracing::debug!(
                                ignore_file = %path,
                                directory = %directory,
                                "Ignoring changed ignore file because it doesn't affect indexed project paths"
                            );
                        }
                    }

                    continue;
                }

                if Some(path) == custom_stdlib_versions_path.as_deref() {
                    result.custom_stdlib_changed = true;
                }
            }

            match change {
                ChangeEvent::Changed { path, .. }
                | ChangeEvent::Opened(path)
                | ChangeEvent::Created { path, .. } => {
                    match change {
                        ChangeEvent::Changed { .. } => {
                            if synced_files.insert(path.to_path_buf()) {
                                result.database_changed |= File::sync_path_only(self, path);
                            }
                        }
                        ChangeEvent::Opened(_)
                        | ChangeEvent::Created {
                            kind: CreatedKind::File,
                            ..
                        } => {
                            if synced_files.insert(path.to_path_buf()) {
                                result.database_changed |= File::sync_path(self, path);
                            }
                        }
                        _ => {
                            sync_recursively.insert(path.clone());
                        }
                    }

                    if !project.file_set(self).is_lazy() {
                        // A `Changed` event only updates known files. Opening or creating a file can
                        // introduce a new one, but only after it passes the filters below.
                        let is_file = if change.is_changed() {
                            self.files()
                                .try_system(self, path)
                                .is_some_and(|file| file.exists(self))
                        } else {
                            self.system().is_file(path)
                        };

                        if is_file {
                            if !project
                                .is_file_included(self, path)
                                .should_index_file(self.system(), path)
                                || is_within_build_output(self.system(), path, &walk_roots)
                            {
                                continue;
                            }

                            if ignore_files
                                .as_mut()
                                .is_none_or(|ignore_files| !ignore_files.is_ignored(path, false))
                                && let Ok(file) = system_path_to_file(self, path)
                            {
                                let is_script = script_tag(self, file).is_some();
                                // Explicitly included files are checked even when scripts are otherwise excluded.
                                let exclude_script = is_script
                                    && project.settings(self).src().exclude_scripts
                                    && !project.is_file_explicitly_included(self, file);

                                if exclude_script {
                                    result.database_changed |= project.remove_file(self, file);
                                } else {
                                    result.database_changed |=
                                        project.add_file(self, file, is_script);
                                }

                                if let ChangedFiles::Known(changed_files) =
                                    &mut result.changed_files
                                {
                                    changed_files.insert(file);
                                }
                            }
                        } else if change.is_created()
                            && project.is_directory_included(self, path)
                            && !is_within_build_output(self.system(), path, &walk_roots)
                            && ignore_files
                                .as_mut()
                                .is_none_or(|ignore_files| !ignore_files.is_ignored(path, true))
                        {
                            // Unlike a new file, a new directory needs walking to discover
                            // project files that exist below it.
                            added_paths.insert(path.clone());
                            result.changed_files.mark_unknown();
                        }
                    }
                }

                ChangeEvent::Deleted { kind, path } => {
                    let is_file = match kind {
                        DeletedKind::File => true,
                        DeletedKind::Directory => false,
                        DeletedKind::Any => self
                            .files
                            .try_system(self, path)
                            .is_some_and(|file| file.exists(self)),
                    };

                    if is_file {
                        if synced_files.insert(path.to_path_buf()) {
                            result.database_changed |= File::sync_path(self, path);
                        }

                        if let Some(file) = self.files().try_system(self, path) {
                            result.database_changed |= project.remove_file(self, file);
                        }
                    } else {
                        sync_recursively.insert(path.clone());
                        removed_paths.insert(path.clone());

                        if custom_stdlib_versions_path
                            .as_ref()
                            .is_some_and(|versions_path| versions_path.starts_with(path))
                        {
                            result.custom_stdlib_changed = true;
                        }

                        if configuration_paths.may_contain_configuration(path, &project_root) {
                            tracing::debug!(
                                "Reload project because a configuration file \
                                may have been deleted."
                            );
                            reload_project = true;
                        }
                    }
                }

                ChangeEvent::CreatedVirtual(path) | ChangeEvent::ChangedVirtual(path) => {
                    result.database_changed |= File::sync_virtual_path(self, path);
                }

                ChangeEvent::DeletedVirtual(path) => {
                    if let Some(virtual_file) = self.files().try_virtual_file(path) {
                        virtual_file.close(self);
                        result.database_changed = true;
                    }
                }

                ChangeEvent::Rescan => {
                    reload_project = true;
                    reload_project_files = true;
                    result.database_changed |= Files::sync_all(self);
                    sync_recursively.clear();
                    removed_paths.clear();
                    break;
                }
            }
        }

        result.database_changed |= Files::sync_all_recursive(self, sync_recursively);

        if reload_project {
            // The active project root may have been deleted. Start rediscovery from the closest
            // existing ancestor so ty can fall back to an enclosing project.
            let path = project_root
                .ancestors()
                .find(|path| self.system().is_directory(path))
                .unwrap_or(&project_root);
            let metadata = project.metadata(self);
            if metadata.use_uv().workspace_discovery_enabled()
                && metadata.config_file_override().is_none()
            {
                result.project_sync_path = Some(path.to_path_buf());
            } else {
                // We're not refreshing uv metadata, so use the existing environment.
                let environment = metadata.environment().clone();
                match project.rediscover(self, path, environment) {
                    // a rediscovery that leaves the metadata and settings as they were can still
                    // replace the program settings or the settings diagnostics, and does not say
                    // whether it did, so it is taken to have
                    Ok(ProjectReloadResult::Unchanged) => result.database_changed = true,
                    Ok(ProjectReloadResult::Changed { files_changed }) => {
                        result.database_changed = true;
                        result.project_changed = true;
                        result.changed_files.mark_unknown();
                        if files_changed {
                            // The project file set has been invalidated; continuing would
                            // run incremental discovery from paths collected before the reload.
                            return result;
                        }
                    }
                    Err(error) => {
                        let error = anyhow::Error::new(error);
                        tracing::error!(
                            "Failed to load project, keeping old project configuration: {error:#}"
                        );
                        if reload_project_files {
                            result.database_changed |= project.reload_files(self);
                            result.changed_files.mark_unknown();
                            return result;
                        }
                    }
                }
            }
        }

        if reload_project_files {
            result.database_changed |= project.reload_files(self);
            result.changed_files.mark_unknown();
            // A full project-file reload supersedes incremental project-file updates.
            added_paths.clear();
            removed_paths.clear();
        }

        if result.custom_stdlib_changed {
            let metadata = project.metadata(self);
            let merged_options = metadata.to_merged_options();
            match merged_options.to_program_settings(
                self.system(),
                self.vendored(),
                &FallibleStrategy,
            ) {
                Ok((program_settings, program_settings_diagnostics)) => {
                    let mut settings_diagnostics =
                        match merged_options.to_settings(self, &FallibleStrategy) {
                            Ok((_, diagnostics)) => diagnostics,
                            Err(error) => vec![error.into_diagnostic()],
                        };
                    result.database_changed |= project.update_program(self, program_settings);
                    settings_diagnostics.extend(
                        program_settings_diagnostics
                            .into_iter()
                            .map(|diagnostic| diagnostic.into_diagnostic(self)),
                    );
                    result.database_changed |=
                        project.update_settings_diagnostics(self, settings_diagnostics);
                }
                Err(error) => {
                    tracing::error!("Failed to resolve program settings: {error}");
                }
            }
        }

        result.database_changed |= project.remove_files_under(self, removed_paths);

        let diagnostics = if !project.file_set(self).is_lazy() {
            // Use directory walking to discover newly added files.
            let walker = ProjectFilesWalker::incremental(added_paths);
            let (files, diagnostics) = walker.collect_vec(self);

            for file in files {
                result.database_changed |= project.add_file(self, file.file, file.is_script);
            }

            diagnostics
        } else {
            Vec::new()
        };

        // Note: We simply replace all IO related diagnostics here. This isn't ideal, because
        // it removes IO errors that may still be relevant. However, tracking IO errors correctly
        // across revisions doesn't feel essential, considering that they're rare. However, we could
        // implement a `BTreeMap` or similar and only prune the diagnostics from paths that we've
        // re-scanned (or that were removed etc).
        result.database_changed |= project.replace_index_diagnostics(self, diagnostics);

        result
    }
}

struct ConfigurationPaths {
    normal_discovery: bool,
    extra: Box<[SystemPathBuf]>,
}

impl ConfigurationPaths {
    fn from_metadata(metadata: &ProjectMetadata) -> Self {
        Self {
            normal_discovery: metadata.config_file_override().is_none(),
            extra: metadata
                .extra_configuration_paths()
                .map(SystemPath::to_path_buf)
                .collect(),
        }
    }

    fn is_configuration(&self, path: &SystemPath, project_root: &SystemPath) -> bool {
        if self
            .extra
            .iter()
            .any(|config_path| config_path.as_path() == path)
        {
            return true;
        }

        self.normal_discovery
            && path
                .parent()
                .is_some_and(|parent| project_root.starts_with(parent))
            && path
                .file_name()
                .is_some_and(|name| CONFIG_FILE_NAMES.contains(&name) || name == "pyproject.toml")
    }

    fn may_contain_configuration(&self, directory: &SystemPath, project_root: &SystemPath) -> bool {
        if self
            .extra
            .iter()
            .any(|config_path| config_path.starts_with(directory))
        {
            return true;
        }

        // Deleting the project root or one of its ancestors can change rediscovery:
        // ty may need to fall back to an enclosing configuration.
        self.normal_discovery && project_root.starts_with(directory)
    }
}

fn is_ignore_file(path: &SystemPath) -> bool {
    matches!(path.file_name(), Some(".gitignore" | ".ignore"))
}
