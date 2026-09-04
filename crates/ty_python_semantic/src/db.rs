use crate::dependencies::DependencyManifest;
use crate::lint::{LintRegistry, RuleSelection};
use crate::{AnalysisSettings, ExperimentalSettings, PythonVersionWithSource};
use ruff_db::diagnostic::Diagnostic;
use ruff_db::files::File;
use ty_python_core::{Db as PythonCoreDb, ProgramFile};

/// Database giving access to semantic information about a Python program.
#[salsa::db]
pub trait Db: PythonCoreDb {
    fn check_file(&self, file: File) -> Vec<Diagnostic>;

    /// Returns the program file for `file`.
    fn program_file(&self, file: File) -> ProgramFile<'_>;

    /// Returns the Python version and its configuration source for `file`.
    fn python_version_with_source(&self, file: File) -> &PythonVersionWithSource;

    /// Resolves the rule selection for a given file.
    fn rule_selection(&self, file: File) -> &RuleSelection;

    fn lint_registry(&self) -> &LintRegistry;

    fn analysis_settings(&self, file: File) -> &AnalysisSettings;

    /// The experimental features the project has opted in to.
    ///
    /// Project-wide rather than per-file: an experimental feature is a language
    /// feature, and a module's meaning cannot depend on which file is asking.
    fn experimental_settings(&self) -> &ExperimentalSettings;

    /// Whether ty is running with logging verbosity INFO or higher (`-v` or more).
    fn verbose(&self) -> bool;

    /// Returns `true` if `file` is open in the editor.
    ///
    /// Expected types for string-literal completions are only collected for open files.
    fn is_open_file(&self, file: File) -> bool;

    /// The module the project points `DJANGO_SETTINGS_MODULE` at, if it names one.
    ///
    /// Which module that is comes out of the project's own files, and enumerating
    /// them belongs to a crate above this one, so the answer is handed down rather
    /// than worked out here. What is left to do with it — reading `settings.NAME`
    /// off the module — is type inference and lives here. See
    /// [`crate::django_settings`].
    ///
    /// The implementation is expected to be a Salsa query, so that a change to the
    /// script that names the module is seen by everything that read this.
    fn django_settings_file(&self) -> Option<File> {
        None
    }

    /// basedpython: does any file in the project declare a protocol conformance
    /// (`extension str(A):`)?
    ///
    /// Whether a requirement read off an interface-typed receiver has to go
    /// through the witness table is a *whole-program* question: a conformance is
    /// written in the module that imports the interface, so the module declaring
    /// the function that uses it can never see one. Enumerating the project's
    /// files belongs to a crate above this one, so the answer is handed down —
    /// the same shape as [`Db::django_settings_file`].
    ///
    /// Defaults to `true`. A database that cannot answer must keep dispatching:
    /// answering `false` wrongly is a miscompile, answering `true` wrongly only
    /// costs a lookup in an empty registry.
    ///
    /// The implementation is expected to be a Salsa query, so that adding the
    /// first conformance to a project invalidates everything that read this.
    fn project_declares_conformances(&self) -> bool {
        true
    }

    /// What the project `file` belongs to declares it depends on.
    ///
    /// Reading `pyproject.toml` — or the PEP 723 block of a script, which is why
    /// this is asked per file — belongs to a crate above this one, so the answer
    /// is handed down. The shape follows [`Db::django_settings_file`].
    ///
    /// `None` means nothing is known, which every caller has to read as "impose
    /// no restriction": a project with no manifest must behave exactly as one
    /// whose every import is declared.
    ///
    /// The implementation is expected to be a Salsa query, so that editing the
    /// dependency list is seen by everything that read this.
    fn dependency_manifest(&self, file: File) -> Option<&DependencyManifest> {
        let _ = file;
        None
    }

    fn dyn_clone(&self) -> Box<dyn Db>;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use anyhow::Context;
    use ty_python_core::platform::PythonPlatform;

    use crate::{ProgramEnvironment, TypeCheckingPreset, check_file_unwrap, default_lint_registry};
    use ruff_db::Db as SourceDb;
    use ruff_db::files::Files;
    use ruff_db::system::{
        DbWithTestSystem, DbWithWritableSystem as _, System, SystemPath, SystemPathBuf, TestSystem,
    };
    use ruff_db::vendored::VendoredFileSystem;
    use ruff_python_ast::PythonVersion;
    use ty_module_resolver::{Db as ModuleResolverDb, SearchPathSettings};
    use ty_python_core::TestProgramDb;
    use ty_python_core::program::{FallibleStrategy, ProgramSettings};
    use ty_site_packages::{PythonVersionSource, PythonVersionWithSource};

    type Events = Arc<Mutex<Vec<salsa::Event>>>;

    #[salsa::db]
    #[derive(Clone)]
    pub(crate) struct TestDb {
        storage: salsa::Storage<Self>,
        files: Files,
        system: TestSystem,
        vendored: VendoredFileSystem,
        events: Events,
        rule_selection: Arc<RuleSelection>,
        analysis_settings: Arc<AnalysisSettings>,
        experimental_settings: ExperimentalSettings,
        open_files: rustc_hash::FxHashSet<File>,
        program_settings: ProgramSettings,
    }

    impl TestDb {
        fn new() -> Self {
            let events = Events::default();
            let vendored = ty_vendored::file_system().clone();
            let program_settings = ProgramSettings::empty(&vendored);
            Self {
                storage: salsa::Storage::new(Some(Box::new({
                    let events = events.clone();
                    move |event| {
                        tracing::trace!("event: {event:?}");
                        let mut events = events.lock().unwrap();
                        events.push(event);
                    }
                }))),
                system: TestSystem::default(),
                vendored,
                events,
                files: Files::default(),
                rule_selection: Arc::new(RuleSelection::from_preset(
                    default_lint_registry(),
                    TypeCheckingPreset::default(),
                )),
                analysis_settings: AnalysisSettings::default().into(),
                // the in-crate test db is used for unit tests of the type system
                // itself, where an experimental feature is what is under test
                experimental_settings: ExperimentalSettings { module_api: true },
                open_files: rustc_hash::FxHashSet::default(),
                program_settings,
            }
        }

        pub(crate) fn python_version(&self) -> PythonVersion {
            self.program().python_version(self)
        }

        pub(crate) fn program_environment(&self) -> ProgramEnvironment<'_> {
            ProgramEnvironment::from_program(self.program())
        }

        /// Marks `file` as open in the editor.
        ///
        /// This is untracked state: open a file before running any queries.
        pub(crate) fn open_file(&mut self, file: File) {
            self.open_files.insert(file);
        }

        /// basedpython: read a missing annotation gradually, the way python does.
        ///
        /// For a test about the *shape* a signature is parsed into rather than about what is
        /// recovered for the parts nobody wrote.
        /// The gradual baseline: nothing is recovered that an annotation did not write down.
        ///
        /// `sound-types` reaches the same positions from the other side, so a test that wants a
        /// signature to say only what the source says has to start from the preset that leaves
        /// both off rather than from the default one.
        pub(crate) fn without_inferred_signatures(mut self) -> Self {
            self.analysis_settings = AnalysisSettings {
                infer_unannotated_signatures: false,
                ..AnalysisSettings::from_preset(TypeCheckingPreset::TyCompatible)
            }
            .into();
            self
        }

        /// Takes the salsa events.
        pub(crate) fn take_salsa_events(&mut self) -> Vec<salsa::Event> {
            let mut events = self.events.lock().unwrap();

            std::mem::take(&mut *events)
        }

        /// Clears the salsa events.
        ///
        /// ## Panics
        /// If there are any pending salsa snapshots.
        pub(crate) fn clear_salsa_events(&mut self) {
            self.take_salsa_events();
        }
    }

    impl DbWithTestSystem for TestDb {
        fn test_system(&self) -> &TestSystem {
            &self.system
        }

        fn test_system_mut(&mut self) -> &mut TestSystem {
            &mut self.system
        }
    }

    #[salsa::db]
    impl SourceDb for TestDb {
        fn vendored(&self) -> &VendoredFileSystem {
            &self.vendored
        }

        fn system(&self) -> &dyn System {
            &self.system
        }

        fn files(&self) -> &Files {
            &self.files
        }
    }

    #[salsa::db]
    impl ty_python_core::Db for TestDb {
        fn should_check_file(&self, file: File) -> bool {
            !file.path(self).is_vendored_path()
        }
    }

    #[salsa::db]
    impl TestProgramDb for TestDb {
        fn program_settings(&self) -> &ProgramSettings {
            &self.program_settings
        }
    }

    #[salsa::db]
    impl Db for TestDb {
        fn check_file(&self, file: File) -> Vec<Diagnostic> {
            if !self.should_check_file(file) {
                return Vec::new();
            }

            check_file_unwrap(self, self.program_file(file))
        }

        fn program_file(&self, file: File) -> ProgramFile<'_> {
            self.program().program_file(self, file)
        }

        fn python_version_with_source(&self, _file: File) -> &PythonVersionWithSource {
            &self.program_settings.python_version
        }

        fn rule_selection(&self, _file: File) -> &RuleSelection {
            &self.rule_selection
        }

        fn lint_registry(&self) -> &LintRegistry {
            default_lint_registry()
        }

        fn analysis_settings(&self, _file: File) -> &AnalysisSettings {
            &self.analysis_settings
        }

        fn experimental_settings(&self) -> &ExperimentalSettings {
            &self.experimental_settings
        }

        fn verbose(&self) -> bool {
            false
        }

        fn is_open_file(&self, file: File) -> bool {
            self.open_files.contains(&file)
        }

        fn dyn_clone(&self) -> Box<dyn crate::Db> {
            Box::new(self.clone())
        }
    }

    #[salsa::db]
    impl ModuleResolverDb for TestDb {}

    #[salsa::db]
    impl salsa::Database for TestDb {}

    pub(crate) struct TestDbBuilder<'a> {
        /// Target Python version
        python_version: PythonVersion,
        /// Target Python platform
        python_platform: PythonPlatform,
        /// Path and content pairs for files that should be present
        files: Vec<(&'a str, &'a str)>,
        /// Directories resolved as site-packages (third-party) search paths
        site_packages: Vec<SystemPathBuf>,
    }

    impl<'a> TestDbBuilder<'a> {
        pub(crate) fn new() -> Self {
            Self {
                python_version: PythonVersion::default(),
                python_platform: PythonPlatform::default(),
                files: vec![],
                site_packages: vec![],
            }
        }

        /// Resolve `path` as a site-packages directory, so files written
        /// under it model an installed third-party package (`KnownModule`
        /// third-party gating requires this — a first-party file can never
        /// be recognized as e.g. `pydantic.main`).
        pub(crate) fn with_site_packages(
            mut self,
            path: &(impl AsRef<SystemPath> + ?Sized),
        ) -> Self {
            self.site_packages.push(path.as_ref().to_path_buf());
            self
        }

        pub(crate) fn with_python_version(mut self, version: PythonVersion) -> Self {
            self.python_version = version;
            self
        }

        pub(crate) fn with_python_platform(mut self, platform: PythonPlatform) -> Self {
            self.python_platform = platform;
            self
        }

        pub(crate) fn with_file(
            mut self,
            path: &'a (impl AsRef<SystemPath> + ?Sized),
            content: &'a str,
        ) -> Self {
            self.files.push((path.as_ref().as_str(), content));
            self
        }

        pub(crate) fn build(self) -> anyhow::Result<TestDb> {
            let mut db = TestDb::new();

            let src_root = SystemPathBuf::from("/src");
            db.memory_file_system().create_directory_all(&src_root)?;
            for site_packages in &self.site_packages {
                db.memory_file_system()
                    .create_directory_all(site_packages)?;
            }

            db.write_files(self.files)
                .context("Failed to write test files")?;

            let search_paths = SearchPathSettings {
                site_packages_paths: self.site_packages,
                ..SearchPathSettings::new(vec![src_root])
            };

            let program_settings = ProgramSettings {
                python_version: PythonVersionWithSource {
                    version: self.python_version,
                    source: PythonVersionSource::default(),
                },
                python_platform: self.python_platform,
                search_paths: search_paths
                    .to_search_paths(db.system(), db.vendored(), &FallibleStrategy)
                    .context("Invalid search path settings")?,
            };
            program_settings.search_paths.try_register_static_roots(&db);
            db.program_settings = program_settings;

            Ok(db)
        }
    }

    pub(crate) fn setup_db() -> TestDb {
        TestDbBuilder::new().build().expect("valid TestDb setup")
    }
}
