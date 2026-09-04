use crate::Db;
use crate::GlobFilterCheckMode;
use crate::glob::{
    AbsolutePortableGlobPattern, ExcludeFilter, IncludeExcludeFilter, IncludeFilter,
    PortableGlobKind,
};
use crate::metadata::pyproject::{ResolveRequiresPythonError, resolve_requires_python_lower_bound};
use crate::metadata::python_version::SupportedPythonVersion;
use crate::metadata::settings::{BuildSettings, OverrideSettings, SrcSettings};

use super::settings::{EditorSettings, Override, Settings, TerminalSettings};
use crate::metadata::value::{RelativeGlobPattern, RelativePathBuf};
use ordermap::OrderMap;
use pep440_rs::VersionSpecifiers;
use ruff_db::RustDoc;
use ruff_db::diagnostic::{
    Annotation, Diagnostic, DiagnosticFormat, DiagnosticId, DisplayDiagnosticConfig, Severity,
    Span, SubDiagnostic, SubDiagnosticSeverity,
};
use ruff_db::system::{System, SystemPath, SystemPathBuf};
use ruff_db::vendored::VendoredFileSystem;
use ruff_macros::{Combine, OptionsMetadata, RustDoc};
use ruff_options_metadata::{OptionSet, OptionsMetadata, Visit};
use ruff_python_ast::PythonVersion;
use ruff_ranged_value::{RangedValue, ValueSource, ValueSourceGuard};
use ruff_text_size::TextRange;
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::{self, Debug, Display};
use std::hash::BuildHasherDefault;
use std::ops::Deref;
use std::sync::Arc;
use strum::IntoEnumIterator;
use thiserror::Error;
use ty_combine::Combine;
use ty_module_resolver::{
    ModuleGlobSet, ModuleGlobSetBuilder, SearchPathSettings, SearchPathSettingsError, SearchPaths,
};
use ty_python_core::platform::PythonPlatform;
use ty_python_core::program::{MisconfigurationStrategy, ProgramSettings};
use ty_python_semantic::lint::{Level, LintSource, RuleSelection};
use ty_python_semantic::{
    AnalysisSettings, ExperimentalSettings, PythonEnvironment, PythonVersionFileSource,
    PythonVersionSource, PythonVersionWithSource, SitePackagesDiscoveryError, SitePackagesPaths,
    SysPrefixPathOrigin, TypeCheckingPreset, inferred_python_version_source_annotation,
};
use ty_static::EnvVars;

#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Options {
    /// The defaults that `rules` and `analysis` start from.
    ///
    /// A preset decides which diagnostics exist and which of them are enabled, and it supplies
    /// the default for every `analysis` option. Both tables are still read, and both still win
    /// over the preset, so a preset is a starting point rather than a straitjacket.
    ///
    /// * `strict`: every diagnostic is enabled, and every analysis option that buys soundness
    ///   is on. This is the default.
    /// * `ty-compatible`: the defaults of [ty](https://github.com/astral-sh/ty), which
    ///   basedpython is built on. basedpython's own diagnostics and analysis options are off,
    ///   so that a project reports what ty itself would report. A diagnostic that doesn't exist
    ///   in ty can't be enabled under this preset, not even with `rules = { all = "error" }`.
    ///
    /// Defaults to `strict`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"strict"#,
        value_type = r#""strict" | "ty-compatible""#,
        example = r#"
            type-checking-preset = "ty-compatible"
        "#
    )]
    pub type_checking_preset: Option<RangedValue<TypeCheckingPreset>>,

    /// Configures the type checking environment.
    #[option_group]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentOptions>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub src: Option<SrcOptions>,

    /// Configures the enabled rules and their severity.
    ///
    /// The keys are either rule names or `all` to set a default severity for all rules.
    /// See [the rules documentation](https://ty.dev/rules) for a list of all available rules.
    ///
    /// Valid severities are:
    ///
    /// * `ignore`: Disable the rule.
    /// * `warn`: Enable the rule and create a warning diagnostic.
    /// * `error`: Enable the rule and create an error diagnostic.
    ///
    /// By default, ty exits with code 1 if it emits any warning or error diagnostics.
    /// Set `terminal.error-on-warning` to `false` to exit with code 0 if all diagnostics have `warning` severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"{...}"#,
        value_type = r#"dict[RuleName | "all", "ignore" | "warn" | "error"]"#,
        example = r#"
            [tool.ty.rules]
            possibly-unresolved-reference = "warn"
            division-by-zero = "ignore"
        "#
    )]
    pub rules: Option<Rules>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub terminal: Option<TerminalOptions>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub analysis: Option<AnalysisOptions>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub experimental: Option<ExperimentalOptions>,

    /// Configures how `by run` executes the project.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub run: Option<RunOptions>,

    /// Configures what `by build` writes, and what a wheel of this project carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub build: Option<BuildOptions>,

    /// Configures how basedpython spells constructs python has no spelling of its own for.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub lowering: Option<LoweringOptions>,

    /// Configures the parts of the editor experience that type checking does not decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub editor: Option<EditorOptions>,

    /// Override configurations for specific file patterns.
    ///
    /// Each override specifies include/exclude patterns and rule configurations
    /// that apply to matching files. Multiple overrides can match the same file,
    /// with later overrides taking precedence.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    pub overrides: Option<OverridesOptions>,
}

impl Options {
    pub fn from_toml_str(content: &str, source: ValueSource) -> Result<Self, TyTomlError> {
        let _guard = ValueSourceGuard::new(source, true);
        let mut options: Self = toml::from_str(content)?;
        options.prioritize_all_selectors();
        Ok(options)
    }

    /// Infers the Python version from `requires-python` unless it was configured explicitly.
    pub(crate) fn apply_requires_python(
        &mut self,
        requires_python: Option<&RangedValue<VersionSpecifiers>>,
    ) -> Result<(), ResolveRequiresPythonError> {
        if self
            .environment
            .as_ref()
            .is_some_and(|environment| environment.python_version.is_some())
        {
            return Ok(());
        }

        if let Some(requires_python) = requires_python
            && let Some(python_version) = resolve_requires_python_lower_bound(requires_python)?
        {
            self.environment.get_or_insert_default().python_version = Some(python_version);
        }

        Ok(())
    }

    /// Ensures that the `all` selector is applied before per-rule selectors
    /// in all rule tables (top-level and overrides).
    ///
    /// This must be called after deserializing from TOML and before any
    /// [`Combine::combine`] calls, because TOML tables are unordered and the
    /// `toml` crate sorts keys lexicographically.
    pub(crate) fn prioritize_all_selectors(&mut self) {
        // Stable sort that moves all `all` selectors before non-`all` selectors
        // while preserving relative order among non-`all` entries.
        let sort = |rules: &mut Rules| {
            rules.inner.sort_by(
                |key_a, _, key_b, _| match (**key_a == "all", **key_b == "all") {
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    _ => Ordering::Equal,
                },
            );
        };

        if let Some(rules) = &mut self.rules {
            sort(rules);
        }
        if let Some(overrides) = &mut self.overrides {
            for override_option in &mut overrides.0 {
                if let Some(rules) = &mut override_option.rules {
                    sort(rules);
                }
            }
        }
    }

    pub fn deserialize_with<'de, D>(source: ValueSource, deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let _guard = ValueSourceGuard::new(source, false);
        Self::deserialize(deserializer)
    }

    /// Resolve configured paths and discover defaults according to the project or script context.
    pub(crate) fn to_program_settings<Strategy: MisconfigurationStrategy>(
        &self,
        context: OptionsContext<'_>,
        project_name: &str,
        system: &dyn System,
        vendored: &VendoredFileSystem,
        strategy: &Strategy,
    ) -> Result<
        (ProgramSettings, Vec<ProgramSettingsDiagnostic>),
        Strategy::Error<ToProgramSettingsError>,
    > {
        let mut diagnostics = Vec::new();
        let environment = self.environment.or_default();

        let configured_python_version = environment
            .python_version
            .as_ref()
            .map(python_version_from_config);
        let python_platform = environment
            .python_platform
            .as_deref()
            .cloned()
            .unwrap_or_else(|| {
                let default = PythonPlatform::default();
                tracing::info!("Defaulting to python-platform `{default}`");
                default
            });

        let python_environment = match self.python_environment(context.configuration_root(), system)
        {
            Ok(None) => PythonEnvironment::discover(context.project_root(), system)
                .map_err(ToProgramSettingsError::PythonEnvironmentDiscovery),
            configured => configured.map_err(ToProgramSettingsError::PythonEnvironment),
        };

        // If in safe-mode, fallback to None if this fails instead of erroring.
        let python_environment = strategy
            .fallback_opt(python_environment, |_| {
                tracing::debug!("Default settings failed to discover local Python environment");
            })?
            .flatten();

        let self_environment = self_environment_search_paths(
            python_environment
                .as_ref()
                .map(ty_python_semantic::PythonEnvironment::origin)
                .cloned(),
            system,
        );

        let site_packages_paths = if let Some(python_environment) = python_environment.as_ref() {
            let site_packages_paths = python_environment
                .site_packages_paths(system)
                .map_err(ToProgramSettingsError::SitePackagesDiscovery);
            let site_packages_paths = strategy.fallback(site_packages_paths, |_| {
                tracing::debug!("Default settings failed to discover site-packages directory");
                SitePackagesPaths::default()
            })?;
            match self_environment {
                // When ty is installed in a virtual environment (e.g., `uvx --with ...`),
                // the self-environment takes priority over the discovered environment.
                Some((self_site_packages, true)) => {
                    self_site_packages.concatenate(site_packages_paths)
                }
                // When ty is installed in a system Python, do not include the system
                // Python's site-packages if there's a discovered project environment.
                Some((_, false)) | None => site_packages_paths,
            }
        } else {
            tracing::debug!("No virtual environment found");
            self_environment.map(|(paths, _)| paths).unwrap_or_default()
        };

        let real_stdlib_path = python_environment.as_ref().and_then(|python_environment| {
            // For now this is considered non-fatal, we don't Need this for anything.
            python_environment
                .real_stdlib_path(system)
                .map_err(|err| {
                    tracing::info!(
                        "No real stdlib found, stdlib goto-definition \
                        may have degraded quality: {err}"
                    );
                })
                .ok()
        });

        let python_version = configured_python_version
            .map(PythonVersionResolution::Configured)
            .or_else(|| {
                let inferred_python_version = python_environment
                    .as_ref()
                    .and_then(|python_environment| {
                        python_environment.python_version_from_metadata()
                    })
                    .cloned()
                    .or_else(|| site_packages_paths.python_version_from_layout());

                inferred_python_version.map(PythonVersionResolution::Inferred)
            })
            .and_then(|resolution| resolution.into_program_version(&mut diagnostics))
            .unwrap_or_default();

        let search_paths = strategy.map_err(
            self.to_search_paths(
                context,
                project_name,
                site_packages_paths,
                real_stdlib_path,
                system,
                vendored,
                strategy,
            ),
            ToProgramSettingsError::SearchPaths,
        )?;

        tracing::info!(
            "Python version: Python {python_version}, platform: {python_platform}",
            python_version = python_version.version
        );

        Ok((
            ProgramSettings {
                python_version,
                python_platform,
                search_paths,
            },
            diagnostics,
        ))
    }

    /// Resolve the configured Python environment. Return `None` if no path was configured.
    pub(crate) fn python_environment(
        &self,
        configuration_root: &SystemPath,
        system: &dyn System,
    ) -> Result<Option<PythonEnvironment>, SitePackagesDiscoveryError> {
        let environment = self.environment.or_default();
        let Some(python_path) = environment.python.as_ref() else {
            return Ok(None);
        };

        let origin = match python_path.source() {
            ValueSource::Cli => SysPrefixPathOrigin::PythonCliFlag,
            ValueSource::File(path) => {
                SysPrefixPathOrigin::ConfigFileSetting(path.clone(), python_path.range())
            }
            ValueSource::ScriptMetadata(_) => SysPrefixPathOrigin::ScriptMetadataSetting,
            ValueSource::Editor => SysPrefixPathOrigin::Editor,
            ValueSource::UvMetadata => SysPrefixPathOrigin::UvMetadata,
        };

        PythonEnvironment::new(
            python_path.absolute(configuration_root, system),
            origin,
            system,
        )
        .map(Some)
    }

    #[expect(clippy::too_many_arguments)]
    fn to_search_paths<Strategy: MisconfigurationStrategy>(
        &self,
        context: OptionsContext<'_>,
        project_name: &str,
        site_packages_paths: SitePackagesPaths,
        real_stdlib_path: Option<SystemPathBuf>,
        system: &dyn System,
        vendored: &VendoredFileSystem,
        strategy: &Strategy,
    ) -> Result<SearchPaths, Strategy::Error<SearchPathSettingsError>> {
        let environment = self.environment.or_default();

        let environment_roots = if let Some(roots) = environment.root.as_deref() {
            roots
                .iter()
                .map(|root| root.absolute(context.configuration_root(), system))
                .collect()
        } else {
            let project_root = context.configuration_root();
            let mut roots = vec![];
            let is_package = |dir: &SystemPath| {
                system.is_file(&dir.join("__init__.py"))
                    || system.is_file(&dir.join("__init__.pyi"))
                    || system.is_file(&dir.join("__init__.by"))
                    || system.is_file(&dir.join("__init__.byi"))
            };

            // Check for `./src` directory (src-layout)
            let src = project_root.join("src");
            if system.is_directory(&src) && !is_package(&src) {
                tracing::debug!(
                    "Including `./src` in `environment.root` \
                    because a `./src` directory exists and is not a package"
                );
                roots.push(src);
            }

            // Check for `./<project-name>/<project-name>` directory (src-layout with project-named folder)
            // For example, the "src" folder for `psycopg` is called `psycopg` and the python files are in `psycopg/psycopg/_adapters_map.py`
            let project_name_dir = project_root.join(project_name);
            if system.is_directory(&project_name_dir.join(project_name))
                && !is_package(&project_name_dir)
                && !roots.contains(&project_name_dir)
            {
                tracing::debug!(
                    "Including `./{project_name}` in `environment.root` because a \
                     `./{project_name}/{project_name}` directory exists \
                     and `./{project_name}` is not a package"
                );
                roots.push(project_name_dir);
            }

            // Check for `./python` directory (maturin-based rust/python projects)
            // https://github.com/PyO3/maturin/blob/979fe1db42bb9e58bc150fa6fc45360b377288bf/README.md?plain=1#L88-L99
            let python = project_root.join("python");
            if system.is_directory(&python) && !is_package(&python) && !roots.contains(&python) {
                tracing::debug!(
                    "Including `./python` in `environment.root` \
                    because a `./python` directory exists and is not a package"
                );
                roots.push(python);
            }

            // The project root is always included, and should always come last
            // (after any subdirectories such as `./src`, `./<project-name>`, and/or `./python`).
            roots.push(project_root.to_path_buf());

            roots
        };

        // collect the existing site packages
        let mut extra_paths: Vec<SystemPathBuf> = environment
            .extra_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|path| path.absolute(context.configuration_root(), system))
            .collect();

        // read all the paths off the PYTHONPATH environment variable, check
        // they exist as a directory, and add them to the vec of extra_paths
        // as they should be checked before site-packages just like python
        // interpreter does
        if let Ok(python_path) = system.env_var(EnvVars::PYTHONPATH) {
            for path in std::env::split_paths(python_path.as_str()) {
                let path = match SystemPathBuf::from_path_buf(path) {
                    Ok(path) => path,
                    Err(path) => {
                        tracing::debug!(
                            "Skipping `{path}` listed in `PYTHONPATH` \
                            because the path is not valid UTF-8",
                            path = path.display()
                        );
                        continue;
                    }
                };

                let abspath = SystemPath::absolute(path, system.current_directory());

                if !system.is_directory(&abspath) {
                    tracing::debug!(
                        "Skipping `{abspath}` listed in `PYTHONPATH` \
                        because the path doesn't exist or isn't a directory"
                    );
                    continue;
                }

                tracing::debug!(
                    "Adding `{abspath}` from the `PYTHONPATH` environment variable \
                    to `extra_paths`"
                );

                extra_paths.push(abspath);
            }
        }

        let settings = SearchPathSettings {
            extra_paths,
            src_roots: environment_roots,
            custom_typeshed: environment
                .typeshed
                .as_ref()
                .map(|path| path.absolute(context.configuration_root(), system)),
            site_packages_paths: site_packages_paths.into_vec(),
            real_stdlib_path,
        };

        settings.to_search_paths(system, vendored, strategy)
    }

    pub(crate) fn to_settings<Strategy: MisconfigurationStrategy>(
        &self,
        db: &dyn Db,
        context: OptionsContext<'_>,
        strategy: &Strategy,
    ) -> Result<(Settings, Vec<OptionDiagnostic>), Strategy::Error<ToSettingsError>> {
        let mut diagnostics = Vec::new();
        let preset = self.type_checking_preset();
        let rules = self.to_rule_selection(db, preset, &mut diagnostics);

        let terminal_options = self.terminal.or_default();
        let terminal = TerminalSettings {
            output_format: terminal_options
                .output_format
                .as_deref()
                .copied()
                .unwrap_or_default(),
            error_on_warning: terminal_options.error_on_warning.unwrap_or(true),
        };

        let src_options = self.src.or_default();

        let src = src_options
            .to_settings(db, context.configuration_root(), &mut diagnostics)
            .map_err(|err| ToSettingsError {
                diagnostic: err,
                output_format: terminal.output_format,
                color: colored::control::SHOULD_COLORIZE.should_colorize(),
            });
        let src = strategy.fallback(src, |_| SrcSettings::default())?;

        let build = self
            .build
            .or_default()
            .to_settings(db, context.configuration_root(), &mut diagnostics)
            .map_err(|err| ToSettingsError {
                diagnostic: err,
                output_format: terminal.output_format,
                color: colored::control::SHOULD_COLORIZE.should_colorize(),
            });
        let build = strategy.fallback(build, |_| BuildSettings::default())?;

        let mut analysis_diagnostics = Vec::new();
        let analysis =
            self.analysis
                .or_default()
                .to_settings(db, preset, &mut analysis_diagnostics);

        let analysis_result: Result<_, ToSettingsError> =
            if let Some(diagnostic) = analysis_diagnostics.into_iter().next() {
                Err(ToSettingsError {
                    diagnostic: Box::new(diagnostic),
                    output_format: terminal.output_format,
                    color: colored::control::SHOULD_COLORIZE.should_colorize(),
                })
            } else {
                Ok(analysis)
            };
        let analysis =
            strategy.fallback(analysis_result, |_| AnalysisSettings::from_preset(preset))?;

        let experimental = self.experimental.or_default().to_settings();

        let overrides = self
            .to_overrides_settings(db, context.configuration_root(), preset, &mut diagnostics)
            .map_err(|err| ToSettingsError {
                diagnostic: err,
                output_format: terminal.output_format,
                color: colored::control::SHOULD_COLORIZE.should_colorize(),
            });
        let overrides = strategy.fallback(overrides, |_| Vec::new())?;

        let editor = EditorSettings::new(
            self.editor
                .as_ref()
                .and_then(|editor| editor.common_aliases.as_ref())
                .into_iter()
                .flat_map(CommonAliases::iter),
        );

        let settings = Settings {
            rules: Arc::new(rules),
            terminal,
            src,
            build,
            analysis,
            experimental,
            editor,
            overrides,
        };

        Ok((settings, diagnostics))
    }

    /// The preset the project's other settings start from.
    fn type_checking_preset(&self) -> TypeCheckingPreset {
        self.configured_type_checking_preset().unwrap_or_default()
    }

    /// The preset this layer of configuration sets, if it sets one.
    pub(crate) fn configured_type_checking_preset(&self) -> Option<TypeCheckingPreset> {
        self.type_checking_preset.as_deref().copied()
    }

    #[must_use]
    fn to_rule_selection(
        &self,
        db: &dyn Db,
        preset: TypeCheckingPreset,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> RuleSelection {
        self.rules
            .or_default()
            .to_rule_selection(db, preset, diagnostics)
    }

    fn to_overrides_settings(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        preset: TypeCheckingPreset,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<Vec<Override>, Box<OptionDiagnostic>> {
        let override_options = &**self.overrides.or_default();

        let mut overrides = Vec::with_capacity(override_options.len());

        for override_option in override_options {
            let override_instance = override_option.to_override(
                db,
                project_root,
                preset,
                self.rules.as_ref(),
                self.analysis.as_ref(),
                diagnostics,
            )?;

            if let Some(value) = override_instance {
                overrides.push(value);
            }
        }

        Ok(overrides)
    }
}

/// The project or standalone script whose options are being resolved.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OptionsContext<'a> {
    Project(&'a SystemPath),
    /// The directory containing a standalone script, or the working directory for a virtual script.
    Script(&'a SystemPath),
}

impl<'a> OptionsContext<'a> {
    fn configuration_root(self) -> &'a SystemPath {
        match self {
            Self::Project(root) | Self::Script(root) => root,
        }
    }

    fn project_root(self) -> Option<&'a SystemPath> {
        match self {
            Self::Project(root) => Some(root),
            Self::Script(_) => None,
        }
    }
}

fn python_version_from_config(
    ranged_version: &RangedValue<SupportedPythonVersion>,
) -> PythonVersionWithSource {
    PythonVersionWithSource {
        version: PythonVersion::from(**ranged_version),
        source: match ranged_version.source() {
            ValueSource::Cli => PythonVersionSource::Cli,
            ValueSource::File(path) => PythonVersionSource::ConfigFile(
                PythonVersionFileSource::new(path.clone(), ranged_version.range()),
            ),
            ValueSource::ScriptMetadata(file) => PythonVersionSource::ScriptMetadata(
                Span::from(*file).with_optional_range(ranged_version.range()),
            ),
            ValueSource::Editor => PythonVersionSource::Editor,
            ValueSource::UvMetadata => PythonVersionSource::UvMetadata,
        },
    }
}

/// A Python version before unsupported inferred versions are filtered.
#[derive(Eq, PartialEq, Debug, Clone)]
enum PythonVersionResolution {
    /// The Python version was configured directly by the user.
    Configured(PythonVersionWithSource),
    /// The Python version was inferred from the environment.
    Inferred(PythonVersionWithSource),
}

impl PythonVersionResolution {
    fn into_program_version(
        self,
        diagnostics: &mut Vec<ProgramSettingsDiagnostic>,
    ) -> Option<PythonVersionWithSource> {
        match self {
            Self::Configured(python_version) => Some(python_version),
            Self::Inferred(python_version) => {
                if SupportedPythonVersion::try_from(python_version.version).is_ok() {
                    Some(python_version)
                } else {
                    diagnostics.push(ProgramSettingsDiagnostic::UnsupportedInferredPythonVersion(
                        python_version,
                    ));
                    None
                }
            }
        }
    }
}

/// A diagnostic produced while resolving [`ProgramSettings`].
///
/// These diagnostics are kept separate from [`OptionDiagnostic`] while program settings are
/// resolved so that this step does not need access to the database.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ProgramSettingsDiagnostic {
    /// The Python version inferred from the environment is newer than ty supports.
    UnsupportedInferredPythonVersion(PythonVersionWithSource),
}

impl ProgramSettingsDiagnostic {
    /// Convert this program-settings diagnostic into a diagnostic that can be stored on a project.
    pub fn into_diagnostic(self, db: &dyn Db) -> OptionDiagnostic {
        match self {
            Self::UnsupportedInferredPythonVersion(python_version) => {
                unsupported_inferred_python_version_diagnostic(db, &python_version)
            }
        }
    }
}

/// Construct an [`OptionDiagnostic`] to indicate that the inferred Python version is unsupported.
fn unsupported_inferred_python_version_diagnostic(
    db: &dyn Db,
    python_version: &PythonVersionWithSource,
) -> OptionDiagnostic {
    let expected = SupportedPythonVersion::iter()
        .map(|version| format!("`{version}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let fallback = PythonVersion::latest_ty();

    let mut diagnostic = OptionDiagnostic::new(
        DiagnosticId::UnsupportedPythonVersion,
        format!(
            "Ignoring unsupported inferred Python version `{}`; \
            ty will use Python {fallback} instead.",
            python_version.version
        ),
        Severity::Warning,
    )
    .sub(SubDiagnostic::new(
        SubDiagnosticSeverity::Info,
        format!("Expected one of {expected}."),
    ))
    .sub(SubDiagnostic::new(
        SubDiagnosticSeverity::Info,
        "Set `environment.python-version` explicitly to override the inferred version.",
    ));

    diagnostic = match &python_version.source {
        source @ PythonVersionSource::ConfigFile(_) => diagnostic
            .with_annotation(inferred_python_version_source_annotation(db, source))
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The version was inferred from a configuration file.",
            )),
        source @ PythonVersionSource::ScriptMetadata(_) => diagnostic
            .with_annotation(inferred_python_version_source_annotation(db, source))
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The version was inferred from script metadata.",
            )),
        source @ PythonVersionSource::PyvenvCfgFile(_) => diagnostic
            .with_annotation(inferred_python_version_source_annotation(db, source))
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The version was inferred from your virtual environment metadata.",
            )),
        PythonVersionSource::InstallationDirectoryLayout {
            site_packages_parent_dir,
            source,
        } => diagnostic
            .with_annotation(inferred_python_version_source_annotation(
                db,
                &PythonVersionSource::InstallationDirectoryLayout {
                    site_packages_parent_dir: site_packages_parent_dir.clone(),
                    source: source.clone(),
                },
            ))
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                format!(
                    "The version was inferred from the \
                    `lib/{site_packages_parent_dir}/site-packages` directory layout.",
                ),
            )),
        PythonVersionSource::Cli => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "The version was inferred from the command line.",
        )),
        PythonVersionSource::Editor => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "The version was inferred from your editor.",
        )),
        PythonVersionSource::UvMetadata => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "The version was provided by uv metadata.",
        )),
        PythonVersionSource::Default => diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "ty fell back to its default Python version.",
        )),
    };

    diagnostic
}

/// Return the site-packages from the environment ty is installed in, as derived from ty's
/// executable.
///
/// If there's an existing environment with an origin that does not allow including site-packages
/// from ty's environment, discovery of ty's environment is skipped and [`None`] is returned.
///
/// Since ty may be executed from an arbitrary non-Python location, errors during discovery of ty's
/// environment are not raised, instead [`None`] is returned.
///
/// Returns a tuple of (`site_packages`, `is_virtual_env`). When the self-environment is a virtual
/// environment (e.g., `uvx --with ...`), it takes priority over other environments.
/// When it's a system Python and there's a project environment (like `.venv`), the system
/// Python's site-packages are excluded entirely.
fn self_environment_search_paths(
    existing_origin: Option<SysPrefixPathOrigin>,
    system: &dyn System,
) -> Option<(SitePackagesPaths, bool)> {
    if existing_origin.is_some_and(|origin| !origin.allows_concatenation_with_self_environment()) {
        return None;
    }

    let Ok(exe_path) = std::env::current_exe() else {
        return None;
    };
    let ty_path = SystemPath::from_std_path(exe_path.as_path())?;

    let environment = PythonEnvironment::new(ty_path, SysPrefixPathOrigin::SelfEnvironment, system)
        .inspect_err(|err| tracing::debug!("Failed to discover ty's environment: {err}"))
        .ok()?;

    let is_virtual_env = environment.is_virtual();

    let search_paths = environment
        .site_packages_paths(system)
        .inspect_err(|err| {
            tracing::debug!("Failed to discover site-packages in ty's environment: {err}");
        })
        .ok()?;

    tracing::debug!("Using site-packages from ty's environment");
    Some((search_paths, is_virtual_env))
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct EnvironmentOptions {
    /// The root paths of the project, used for finding first-party modules.
    ///
    /// Accepts a list of directory paths searched in priority order (first has highest priority).
    ///
    /// If left unspecified, ty will try to detect common project layouts and initialize `root` accordingly.
    /// The project root (`.`) is always included. Additionally, the following directories are included
    /// if they exist and are not packages (i.e. they do not contain `__init__.py` or `__init__.pyi` files):
    ///
    /// * `./src`
    /// * `./<project-name>` (if a `./<project-name>/<project-name>` directory exists)
    /// * `./python`
    ///
    /// Scripts with inline metadata have no first-party roots by default because they are
    /// single-file programs. Set `root = ["."]` to allow importing local modules.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "list[str]",
        example = r#"
            # Multiple directories (priority order)
            root = ["./src", "./lib", "./vendor"]
        "#
    )]
    pub root: Option<Vec<RelativePathBuf>>,

    /// Specifies the version of Python that will be used to analyze the source code.
    /// The version should be specified as a string in the format `M.m` where `M` is the major version
    /// and `m` is the minor (e.g. `"3.7"` or `"3.12"`).
    /// If a version is provided, ty will generate errors if the source code makes use of language features
    /// that are not supported in that version.
    ///
    /// ty officially supports type checking code that targets Python 3.10 and later. Python 3.7
    /// through 3.9 can still be selected, but ty may produce false positives or false negatives for
    /// standard-library APIs because its bundled stubs do not fully describe those versions.
    ///
    /// If a version is not specified, ty will try the following techniques in order of preference
    /// to determine a value:
    /// 1. Check for the `project.requires-python` setting in a `pyproject.toml` file
    ///    and use the minimum version from the specified range
    /// 2. Check for an activated or configured Python environment
    ///    and attempt to infer the Python version of that environment
    /// 3. Fall back to the default value (see below)
    ///
    /// Scripts with inline metadata use their `requires-python` field instead of
    /// `project.requires-python`. They do not inherit the Python version of the enclosing project.
    ///
    /// For some language features, ty can also understand conditionals based on comparisons
    /// with `sys.version_info`. These are commonly found in typeshed, for example,
    /// to reflect the differing contents of the standard library across Python versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#""3.14""#,
        value_type = r#""3.7" | "3.8" | "3.9" | "3.10" | "3.11" | "3.12" | "3.13" | "3.14" | "3.15""#,
        example = r#"
            python-version = "3.12"
        "#
    )]
    pub python_version: Option<RangedValue<SupportedPythonVersion>>,

    /// Specifies the target platform that will be used to analyze the source code.
    /// If specified, ty will understand conditions based on comparisons with `sys.platform`, such
    /// as are commonly found in typeshed to reflect the differing contents of the standard library across platforms.
    /// If `all` is specified, ty will assume that the source code can run on any platform.
    ///
    /// If no platform is specified, ty will use the current platform:
    /// - `win32` for Windows
    /// - `darwin` for macOS
    /// - `android` for Android
    /// - `ios` for iOS
    /// - `linux` for everything else
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"<current-platform>"#,
        value_type = r#""win32" | "darwin" | "android" | "ios" | "linux" | "all" | str"#,
        example = r#"
        # Tailor type stubs and conditionalized type definitions to windows.
        python-platform = "win32"
        "#
    )]
    pub python_platform: Option<RangedValue<PythonPlatform>>,

    /// User-provided paths that should take first priority in module resolution.
    ///
    /// This is an advanced option that should usually only be used for first-party or third-party
    /// modules that are not installed into your Python environment in a conventional way.
    /// Use the `python` option to specify the location of your Python environment.
    ///
    /// This option is similar to mypy's `MYPYPATH` environment variable and pyright's `stubPath`
    /// configuration setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            extra-paths = ["./shared/my-search-path"]
        "#
    )]
    pub extra_paths: Option<Vec<RelativePathBuf>>,

    /// Optional path to a "typeshed" directory on disk for us to use for standard-library types.
    /// If this is not provided, we will fallback to our vendored typeshed stubs for the stdlib,
    /// bundled as a zip file in the binary
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            typeshed = "/path/to/custom/typeshed"
        "#
    )]
    pub typeshed: Option<RelativePathBuf>,

    /// Path to your project's Python environment or interpreter.
    ///
    /// ty uses the `site-packages` directory of your project's Python environment
    /// to resolve third-party (and, in some cases, first-party) imports in your code.
    ///
    /// This can be a path to:
    ///
    /// - A Python interpreter, e.g. `.venv/bin/python3`
    /// - A virtual environment directory, e.g. `.venv`
    /// - A system Python [`sys.prefix`] directory, e.g. `/usr`
    ///
    /// If you're using a project management tool such as uv, you should not generally need to
    /// specify this option, as commands such as `uv run` will set the `VIRTUAL_ENV` environment
    /// variable to point to your project's virtual environment. ty can also infer the location of
    /// your environment from an activated Conda environment, and will look for a `.venv` directory
    /// in the project root if none of the above apply. Failing that, ty will look for a `python3`
    /// or `python` binary available in `PATH`.
    ///
    /// Scripts with inline metadata use their own Python environment. They can use an explicitly
    /// configured environment, an activated environment, or an environment selected by the editor.
    /// Unlike projects, they do not automatically use a `.venv` directory.
    ///
    /// [`sys.prefix`]: https://docs.python.org/3/library/sys.html#sys.prefix
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            python = "./custom-venv-location/.venv"
        "#
    )]
    pub python: Option<RelativePathBuf>,
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SrcOptions {
    /// Whether to automatically exclude files that are ignored by `.ignore`,
    /// `.gitignore`, `.git/info/exclude`, and global `gitignore` files.
    /// Enabled by default.
    #[option(
        default = r#"true"#,
        value_type = r#"bool"#,
        example = r#"
            respect-ignore-files = false
        "#
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respect_ignore_files: Option<bool>,

    /// Whether to exclude files containing PEP 723 inline script metadata unless they are
    /// explicitly passed on the command line.
    #[option(
        default = r#"false"#,
        value_type = r#"bool"#,
        example = r#"
            exclude-scripts = true
        "#
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_scripts: Option<bool>,

    /// A list of files and directories to check. The `include` option
    /// follows a similar syntax to `.gitignore` but reversed:
    /// Including a file or directory will make it so that it (and its contents)
    /// are type checked.
    ///
    /// - `./src/` matches only a directory
    /// - `./src` matches both files and directories
    /// - `src` matches a file or directory named `src`
    /// - `*` matches any (possibly empty) sequence of characters (except `/`).
    /// - `**` matches zero or more path components.
    ///   This sequence **must** form a single path component, so both `**a` and `b**` are invalid and will result in an error.
    ///   A sequence of more than two consecutive `*` characters is also invalid.
    /// - `?` matches any single character except `/`
    /// - `[abc]` matches any character inside the brackets. Character sequences can also specify ranges of characters, as ordered by Unicode,
    ///   so e.g. `[0-9]` specifies any character between `0` and `9` inclusive. An unclosed bracket is invalid.
    ///
    /// All paths are anchored relative to the project root (`src` only
    /// matches `<project_root>/src` and not `<project_root>/test/src`).
    ///
    /// `exclude` takes precedence over `include`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            include = [
                "src",
                "tests",
            ]
        "#
    )]
    pub include: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// A list of file and directory patterns to exclude from type checking.
    ///
    /// Patterns follow a syntax similar to `.gitignore`:
    ///
    /// - `./src/` matches only a directory
    /// - `./src` matches both files and directories
    /// - `src` matches files or directories named `src`
    /// - `*` matches any (possibly empty) sequence of characters (except `/`).
    /// - `**` matches zero or more path components.
    ///   This sequence **must** form a single path component, so both `**a` and `b**` are invalid and will result in an error.
    ///   A sequence of more than two consecutive `*` characters is also invalid.
    /// - `?` matches any single character except `/`
    /// - `[abc]` matches any character inside the brackets. Character sequences can also specify ranges of characters, as ordered by Unicode,
    ///   so e.g. `[0-9]` specifies any character between `0` and `9` inclusive. An unclosed bracket is invalid.
    /// - `!pattern` negates a pattern (undoes the exclusion of files that would otherwise be excluded)
    ///
    /// All paths are anchored relative to the project root (`src` only
    /// matches `<project_root>/src` and not `<project_root>/test/src`).
    /// To exclude any directory or file named `src`, use `**/src` instead.
    ///
    /// By default, ty excludes commonly ignored directories:
    ///
    /// - `**/.bzr/`
    /// - `**/.direnv/`
    /// - `**/.eggs/`
    /// - `**/.git/`
    /// - `**/.git-rewrite/`
    /// - `**/.hg/`
    /// - `**/.mypy_cache/`
    /// - `**/.nox/`
    /// - `**/.pants.d/`
    /// - `**/.pytype/`
    /// - `**/.ruff_cache/`
    /// - `**/.svn/`
    /// - `**/.tox/`
    /// - `**/.venv/`
    /// - `**/__pypackages__/`
    /// - `**/_build/`
    /// - `**/buck-out/`
    /// - `**/dist/`
    /// - `**/node_modules/`
    /// - `**/venv/`
    ///
    /// You can override any default exclude by using a negated pattern. For example,
    /// to re-include `dist` use `exclude = ["!dist"]`, or `exclude = ["!**/dist/"]` to
    /// re-include every `dist` directory rather than only the one at the project root.
    ///
    /// A negated pattern can only re-include something that is still walked, so it cannot
    /// reach into a directory that is itself excluded. `exclude = ["!dist/generated.py"]`
    /// re-includes nothing, because the walk stops at `dist`. Re-include the directory
    /// first: `exclude = ["!**/dist/", "**/dist/**", "!**/dist/generated.py"]`
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            exclude = [
                "generated",
                "*.proto",
                "tests/fixtures/**",
                "!tests/fixtures/important.py"  # Include this one file
            ]
        "#
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<RangedValue<Vec<RelativeGlobPattern>>>,
}

impl SrcOptions {
    fn to_settings(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<SrcSettings, Box<OptionDiagnostic>> {
        let include = build_include_filter(
            db,
            project_root,
            self.include.as_ref(),
            GlobFilterContext::SrcRoot,
            diagnostics,
        )?;
        let exclude = build_exclude_filter(
            db,
            project_root,
            self.exclude.as_ref(),
            DEFAULT_SRC_EXCLUDES,
            GlobFilterContext::SrcRoot,
            diagnostics,
        )?;
        let files = IncludeExcludeFilter::new(include, exclude);

        Ok(SrcSettings {
            respect_ignore_files: self.respect_ignore_files.unwrap_or(true),
            exclude_scripts: self.exclude_scripts.unwrap_or(false),
            files,
        })
    }
}

#[derive(
    Debug, Default, Clone, Eq, PartialEq, Combine, Serialize, Deserialize, Hash, get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", transparent)]
pub struct Rules {
    /// The rules with their severity. Entries coming later in the map take precedence over
    /// earlier entries (e.g. a `all` selector earlier in the hash map will be overridden
    /// by a specific rule selector coming after it but if `all` is the last selector, then it
    /// overrides even specific rule codes).
    inner: OrderMap<RangedValue<String>, RangedValue<Level>, BuildHasherDefault<FxHasher>>,
}

impl FromIterator<(RangedValue<String>, RangedValue<Level>)> for Rules {
    fn from_iter<T: IntoIterator<Item = (RangedValue<String>, RangedValue<Level>)>>(
        iter: T,
    ) -> Self {
        Self {
            inner: iter.into_iter().collect(),
        }
    }
}

impl Rules {
    /// Convert the rules to a `RuleSelection` with diagnostics.
    pub(crate) fn to_rule_selection(
        &self,
        db: &dyn Db,
        preset: TypeCheckingPreset,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> RuleSelection {
        let registry = db.lint_registry();

        // Initialize the selection with the preset's defaults
        let mut selection = RuleSelection::from_preset(registry, preset);

        for (rule_name, level) in &self.inner {
            let source = rule_name.source();
            let lint_source = match source {
                ValueSource::File(_) => LintSource::File,
                ValueSource::ScriptMetadata(_) => LintSource::ScriptMetadata,
                ValueSource::Cli => LintSource::Cli,
                ValueSource::Editor => LintSource::Editor,
                ValueSource::UvMetadata => LintSource::UvMetadata,
            };

            let mut set_lint_level = |lint| {
                if let Ok(severity) = Severity::try_from(**level) {
                    selection.enable(lint, severity, lint_source);
                } else {
                    selection.disable(lint);
                }
            };

            // Handle "all" as a special case - apply the level to all rules
            if rule_name.as_str() == "all" {
                for lint in registry.lints().iter().filter(|lint| preset.includes(lint)) {
                    set_lint_level(*lint);
                }
                continue;
            }

            let unknown = match registry.get(rule_name) {
                // a rule the preset leaves out doesn't exist as far as the project is concerned,
                // so naming it is the same mistake as naming one that was never declared
                Ok(lint) if !preset.includes(&lint) => Some(format!(
                    "Rule `{rule_name}` is a basedpython rule, which the `{preset}` type checking preset does not include"
                )),
                Ok(lint) => {
                    set_lint_level(lint);
                    None
                }
                Err(error) => Some(error.to_string()),
            };

            if let Some(message) = unknown {
                // The file may have been deleted since its configuration was read. In that
                // case, report the diagnostic without a configuration-file annotation.
                let file = source.file(db);

                // TODO: Add a note if the value was configured on the CLI
                let diagnostic =
                    OptionDiagnostic::new(DiagnosticId::UnknownRule, message, Severity::Warning);

                let annotation = file
                    .map(Span::from)
                    .map(|span| Annotation::primary(span.with_optional_range(rule_name.range())));
                diagnostics.push(diagnostic.with_annotation(annotation));
            }
        }

        selection
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Default exclude patterns for src options.
pub(crate) const DEFAULT_SRC_EXCLUDES: &[&str] = &[
    "**/.bzr/",
    "**/.direnv/",
    "**/.eggs/",
    "**/.git/",
    "**/.git-rewrite/",
    "**/.hg/",
    "**/.mypy_cache/",
    "**/.nox/",
    "**/.pants.d/",
    "**/.pytype/",
    "**/.ruff_cache/",
    "**/.svn/",
    "**/.tox/",
    "**/.venv/",
    "**/__pypackages__/",
    "**/_build/",
    "**/buck-out/",
    "**/dist/",
    "**/node_modules/",
    "**/venv/",
];

/// Helper function to build an include filter from patterns with proper error handling.
fn build_include_filter(
    db: &dyn Db,
    project_root: &SystemPath,
    include_patterns: Option<&RangedValue<Vec<RelativeGlobPattern>>>,
    context: GlobFilterContext,
    diagnostics: &mut Vec<OptionDiagnostic>,
) -> Result<IncludeFilter, Box<OptionDiagnostic>> {
    use crate::glob::{IncludeFilterBuilder, PortableGlobPattern};

    let system = db.system();
    let mut includes = IncludeFilterBuilder::new();

    if let Some(include_patterns) = include_patterns {
        if include_patterns.is_empty() {
            // An override with an empty include `[]` won't match any files.
            let mut diagnostic = OptionDiagnostic::new(
                DiagnosticId::EmptyInclude,
                "Empty include matches no files".to_string(),
                Severity::Warning,
            )
            .sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "Remove the `include` option to match all files \
                or add a pattern to match specific files",
            ));

            // Add source annotation if we have source information
            if let Some(file) = include_patterns.source().file(db) {
                let annotation = Annotation::primary(
                    Span::from(file).with_optional_range(include_patterns.range()),
                )
                .message("This `include` list is empty");
                diagnostic = diagnostic.with_annotation(Some(annotation));
            }

            diagnostics.push(diagnostic);
        }

        for pattern in include_patterns {
            pattern
                .absolute(project_root, system, PortableGlobKind::Include)
                .and_then(|include| Ok(includes.add(&include)?))
                .map_err(|err| {
                    let diagnostic = OptionDiagnostic::new(
                        DiagnosticId::InvalidGlob,
                        format!("Invalid include pattern `{pattern}`: {err}"),
                        Severity::Error,
                    );

                    diagnostic.with_source_sub(
                        db,
                        pattern.value(),
                        "pattern",
                        context.include_name(),
                        err,
                    )
                })?;
        }
    } else {
        includes
            .add(
                &PortableGlobPattern::parse("**", PortableGlobKind::Include)
                    .unwrap()
                    .into_absolute(""),
            )
            .unwrap();
    }

    includes.build().map_err(|_| {
        let diagnostic = OptionDiagnostic::new(
            DiagnosticId::InvalidGlob,
            format!(
                "The `{}` patterns resulted in a regex that is too large",
                context.include_name()
            ),
            Severity::Error,
        );
        Box::new(diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "Please open an issue on the ty repository \
            and share the patterns that caused the error.",
        )))
    })
}

/// Helper function to build an exclude filter from patterns with proper error handling.
fn build_exclude_filter(
    db: &dyn Db,
    project_root: &SystemPath,
    exclude_patterns: Option<&RangedValue<Vec<RelativeGlobPattern>>>,
    default_patterns: &[&str],
    context: GlobFilterContext,
    diagnostics: &mut Vec<OptionDiagnostic>,
) -> Result<ExcludeFilter, Box<OptionDiagnostic>> {
    use crate::glob::{ExcludeFilterBuilder, PortableGlobPattern};

    let system = db.system();
    let mut excludes = ExcludeFilterBuilder::new();

    for pattern in default_patterns {
        PortableGlobPattern::parse(pattern, PortableGlobKind::Exclude)
            .and_then(|exclude| Ok(excludes.add(&exclude.into_absolute(""))?))
            .unwrap_or_else(|err| {
                panic!("Expected default exclude to be valid glob but adding it failed with: {err}")
            });
    }

    // Held on to so that, once the filter is built, every negation can be checked against the
    // whole pattern set — including the negations that come after it.
    let mut negations = Vec::new();

    // Add user-specified excludes
    if let Some(exclude_patterns) = exclude_patterns {
        for exclude in exclude_patterns {
            let pattern = exclude
                .absolute(project_root, system, PortableGlobKind::Exclude)
                .and_then(|pattern| {
                    excludes.add(&pattern)?;
                    Ok(pattern)
                })
                .map_err(|err| {
                    let diagnostic = OptionDiagnostic::new(
                        DiagnosticId::InvalidGlob,
                        format!("Invalid exclude pattern `{exclude}`: {err}"),
                        Severity::Error,
                    );

                    diagnostic.with_source_sub(
                        db,
                        exclude.value(),
                        "pattern",
                        context.exclude_name(),
                        err,
                    )
                })?;

            if pattern.is_negated() {
                negations.push((exclude, pattern));
            }
        }
    }

    let filter = excludes.build().map_err(|_| {
        let diagnostic = OptionDiagnostic::new(
            DiagnosticId::InvalidGlob,
            format!(
                "The `{}` patterns resulted in a regex that is too large",
                context.exclude_name()
            ),
            Severity::Error,
        );
        Box::new(diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "Please open an issue on the ty repository \
            and share the patterns that caused the error.",
        )))
    })?;

    for (exclude, pattern) in negations {
        let Some(blocked_by) = unreachable_negation_cause(&filter, &pattern) else {
            continue;
        };

        // Paths in a configuration file read better relative to the project they configure.
        let blocked_by_display = blocked_by.strip_prefix(project_root).unwrap_or(blocked_by);

        let mut diagnostic = OptionDiagnostic::new(
            DiagnosticId::UnreachableExcludeNegation,
            format!("Negated pattern `{exclude}` has no effect"),
            Severity::Warning,
        )
        .sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            format!("`{blocked_by_display}` is excluded, so nothing inside it is ever reached"),
        ));

        if let Some(name) = blocked_by.file_name() {
            diagnostic = diagnostic.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                format!("Re-include the directory first by adding `!**/{name}/`"),
            ));
        }

        if let Some(file) = exclude.value().source().file(db) {
            diagnostic = diagnostic.with_annotation(Some(
                Annotation::primary(Span::from(file).with_optional_range(exclude.value().range()))
                    .message("This pattern can never match"),
            ));
        }

        diagnostics.push(diagnostic);
    }

    Ok(filter)
}

/// Returns the excluded directory that stops `negation` from ever re-including anything.
///
/// A negation only takes effect if the walk actually reaches the paths it matches, and the walk
/// stops at the first excluded directory. So the pattern is dead if any directory it has to be
/// reached through is excluded — the shallowest such directory is the one reported, because that
/// is where the walk really stops.
fn unreachable_negation_cause<'a>(
    filter: &ExcludeFilter,
    negation: &'a AbsolutePortableGlobPattern,
) -> Option<&'a SystemPath> {
    negation
        .required_directory()?
        .ancestors()
        .filter(|directory| filter.match_directory(directory, GlobFilterCheckMode::TopDown))
        .last()
}

/// Context for filter operations, used in error messages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobFilterContext {
    /// Source root configuration context
    SrcRoot,
    /// Override configuration context
    Overrides,
    /// Build output configuration context
    Build,
}

impl GlobFilterContext {
    fn include_name(self) -> &'static str {
        match self {
            Self::SrcRoot => "src.include",
            Self::Overrides => "overrides.include",
            Self::Build => "build.include",
        }
    }

    fn exclude_name(self) -> &'static str {
        match self {
            Self::SrcRoot => "src.exclude",
            Self::Overrides => "overrides.exclude",
            Self::Build => "build.exclude",
        }
    }
}

/// The diagnostic output format.
#[derive(
    Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum OutputFormat {
    /// The default full mode will print "pretty" diagnostics.
    ///
    /// That is, color will be used when printing to a `tty`.
    /// Moreover, diagnostic messages may include additional
    /// context and annotations on the input to help understand
    /// the message.
    #[default]
    Full,
    /// Print diagnostics in a concise mode.
    ///
    /// This will guarantee that each diagnostic is printed on
    /// a single line. Only the most important or primary aspects
    /// of the diagnostic are included. Contextual information is
    /// dropped.
    ///
    /// This may use color when printing to a `tty`.
    Concise,
    /// Print diagnostics in the JSON format expected by GitLab [Code Quality] reports.
    ///
    /// [Code Quality]: https://docs.gitlab.com/ci/testing/code_quality/#code-quality-report-format
    Gitlab,
    /// Print diagnostics in the format used by [GitHub Actions] workflow error annotations.
    ///
    /// [GitHub Actions]: https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#setting-an-error-message
    Github,
    /// Print diagnostics as a JUnit-style XML report.
    #[cfg(feature = "junit")]
    Junit,
}

impl OutputFormat {
    /// Returns `true` if this format is intended for users to read directly, in contrast to
    /// machine-readable or structured formats.
    ///
    /// This can be used to check whether information beyond the diagnostics, such as a header or
    /// `Found N diagnostics` footer, should be included.
    pub const fn is_human_readable(&self) -> bool {
        matches!(self, OutputFormat::Full | OutputFormat::Concise)
    }
}

impl From<OutputFormat> for DiagnosticFormat {
    fn from(value: OutputFormat) -> Self {
        match value {
            OutputFormat::Full => Self::Full,
            OutputFormat::Concise => Self::Concise,
            OutputFormat::Gitlab => Self::Gitlab,
            OutputFormat::Github => Self::Github,
            #[cfg(feature = "junit")]
            OutputFormat::Junit => Self::Junit,
        }
    }
}

impl Combine for OutputFormat {
    #[inline(always)]
    fn combine_with(&mut self, _other: Self) {}

    #[inline]
    fn combine(self, _other: Self) -> Self {
        self
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct TerminalOptions {
    /// The format to use for printing diagnostic messages.
    ///
    /// Defaults to `full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"full"#,
        value_type = "full | concise | github | gitlab | junit",
        example = r#"
            output-format = "concise"
        "#
    )]
    pub output_format: Option<RangedValue<OutputFormat>>,
    /// Use exit code 1, even if all diagnostics only had `warning` severity.
    ///
    /// Defaults to `true`.
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Exit with code 0 if all diagnostics had `warning` severity.
        error-on-warning = false
        "#
    )]
    pub error_on_warning: Option<bool>,
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Hash,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RunOptions {
    /// The module `by run` executes when no module is given on the command line.
    ///
    /// This is the project's entry point: with it set, `by run` alone transpiles the project and
    /// runs `python -m <main>`, exactly as if the module had been named on the command line. A
    /// module named explicitly always wins.
    ///
    /// The value is a module path, not a file path — `app.cli`, not `app/cli.by`.
    ///
    /// Defaults to `null`, in which case `by run` requires a module argument.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            main = "app.cli"
        "#
    )]
    pub main: Option<RangedValue<String>>,
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Hash,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct LoweringOptions {
    /// How a float or complex literal type is spelled in the transpiled python.
    ///
    /// basedpython reads `a: 1.5` as a literal type, and python has no spelling for one:
    /// PEP 586 admits only `None`, `int`, `bool`, `str`, `bytes` and enum members into
    /// `Literal[...]`.
    ///
    /// * `nominal` (the default) writes the type the literal is one of — `a: 1.5` becomes
    ///   `a: float`, `a: 2j` becomes `a: complex`. The precision is lost, and every checker
    ///   that reads the output accepts it.
    /// * `literal` keeps the literal, writing `a: Literal[1.5]`. The precision survives and
    ///   the output still runs, because `typing` does not check what it is handed — but a
    ///   checker reading it reports the argument as invalid.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#""nominal""#,
        value_type = r#""nominal" | "literal""#,
        example = r#"
            float-literals = "literal"
        "#
    )]
    pub float_literals: Option<RangedValue<FloatLiteralLowering>>,
}

/// How a float or complex literal type reaches the transpiled python.
#[derive(
    Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize, get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum FloatLiteralLowering {
    /// # Nominal
    ///
    /// The type the literal is one of: `1.5` becomes `float`, `2j` becomes `complex`.
    #[default]
    Nominal,

    /// # Literal
    ///
    /// The literal itself, inside `Literal[...]`. Runs, but no checker accepts it.
    Literal,
}

impl Combine for FloatLiteralLowering {
    fn combine_with(&mut self, _other: Self) {}

    fn combine(self, _other: Self) -> Self {
        self
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Hash,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct BuildOptions {
    /// Files to carry into the build output verbatim, in addition to the ones
    /// that are there by default.
    ///
    /// `by build` mirrors the whole module tree: a `.by` file is transpiled, and
    /// every other file — a hand-written `.py`, a `py.typed` marker, a template,
    /// a data file — is copied to the same place in the output. `include` is for
    /// the files that sit *outside* a module root and still belong in the build,
    /// such as a data directory next to `src`.
    ///
    /// The syntax is the same as `src.include`, and paths are anchored to the
    /// project root. `exclude` takes precedence over `include`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            include = [
                "assets",
            ]
        "#
    )]
    pub include: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// Files to keep out of the build output.
    ///
    /// The syntax is the same as `src.exclude`, and paths are anchored to the
    /// project root. Excluding a `.by` file keeps its transpiled output out of
    /// the build as well.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            exclude = [
                "tests",
                "**/*.snapshot",
            ]
        "#
    )]
    pub exclude: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// Whether the build output carries the `.by` sources alongside the python
    /// they were transpiled into, with a `by.typed` marker naming them as the
    /// authoritative surface.
    ///
    /// This is what lets one basedpython project depend on another: a downstream
    /// python project reads the transpiled `.py` and is served perfectly, while a
    /// downstream basedpython project reads the `.by` and keeps the declarations
    /// that have no python spelling — `extension` blocks, `raises` clauses,
    /// read-only `let`, sum types.
    ///
    /// Enabled by default. Turn it off to ship python only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
            sources = false
        "#
    )]
    pub sources: Option<bool>,

    /// The python versions to build a wheel for, one wheel each.
    ///
    /// `by build --wheels` builds one wheel per version listed and tags each for
    /// the python it was lowered to, so an installer hands every interpreter the
    /// best wheel it can use. A python with no wheel of its own takes the newest
    /// one below it.
    ///
    /// Defaults to every version from the one the project targets up to the
    /// newest this release knows about — which is what `requires-python` already
    /// says the project supports, so most projects need not set this. List them
    /// explicitly to ship fewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            wheel-versions = ["3.9", "3.13"]
        "#
    )]
    pub wheel_versions: Option<RangedValue<Vec<RangedValue<String>>>>,

    /// The module to read `__version__` from, when `[project]` declares
    /// `dynamic = ["version"]`.
    ///
    /// This is read when a wheel or a source distribution is built, not by the
    /// checker: a version has to be settled before the packaging backend sees the
    /// project, and the place it lives is a `.by` module that backend cannot
    /// read.
    ///
    /// The value is a path relative to the project root.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "str",
        example = r#"
            version-from = "src/app/__init__.by"
        "#
    )]
    pub version_from: Option<RangedValue<String>>,
}

impl BuildOptions {
    fn to_settings(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<BuildSettings, Box<OptionDiagnostic>> {
        let include = build_include_filter(
            db,
            project_root,
            self.include.as_ref(),
            GlobFilterContext::Build,
            diagnostics,
        )?;
        // no default patterns of its own: the build is already bounded by
        // `src.exclude`, defaults and all, and applying them a second time here
        // would re-drop whatever a negation there deliberately took back
        let exclude = build_exclude_filter(
            db,
            project_root,
            self.exclude.as_ref(),
            &[],
            GlobFilterContext::Build,
            diagnostics,
        )?;

        Ok(BuildSettings {
            files: IncludeExcludeFilter::new(include, exclude),
            sources: self.sources.unwrap_or(true),
        })
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct EditorOptions {
    /// The modules a name is a common alias of, keyed by the alias.
    ///
    /// A file that writes `np.` before importing anything almost always means numpy, because `np`
    /// is what numpy is conventionally imported as. The editor completes such a name as the module
    /// it names, and accepting one of those completions writes the `import numpy as np` that makes
    /// the name real.
    ///
    /// This adds aliases of your own to the ones ty already knows; an entry whose alias ty knows
    /// replaces it. An alias for a module the project does not have is never offered, so an entry
    /// for a module nobody installed costs nothing.
    ///
    /// Defaults to `{}`, which leaves ty's own aliases as they are.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"{}"#,
        value_type = "dict[str, str]",
        example = r#"
            [tool.ty.editor.common-aliases]
            npt = "numpy.typing"
        "#
    )]
    pub common_aliases: Option<CommonAliases>,
}

/// The modules that names are common aliases of, keyed by the alias.
///
/// A `BTreeMap` rather than a hash map because the order these are offered in should not depend on
/// a hash seed.
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize, get_size2::GetSize)]
#[serde(transparent)]
pub struct CommonAliases {
    inner: BTreeMap<String, String>,
}

impl CommonAliases {
    /// The configured aliases, each paired with the module it names.
    fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.inner
            .iter()
            .map(|(alias, module)| (alias.as_str(), module.as_str()))
    }
}

impl Combine for CommonAliases {
    fn combine_with(&mut self, mut other: Self) {
        // `self` takes precedence over `other`, and `extend` overwrites what it lands on, so the
        // lower-precedence map is the one that gets extended
        std::mem::swap(&mut self.inner, &mut other.inner);
        self.inner.extend(other.inner);
    }
}

impl FromIterator<(String, String)> for CommonAliases {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self {
            inner: iter.into_iter().collect(),
        }
    }
}

/// Features that are still being designed, and are off unless the project asks
/// for them by name.
///
/// An experimental feature may change or be withdrawn without the deprecation
/// period a stable one gets. Opting in says you would rather have it than that
/// guarantee.
#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Hash,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ExperimentalOptions {
    /// Whether an `implements` declaration is enforced.
    ///
    /// `implements Backend` obliges the module that writes it to answer the
    /// protocol, and a `for` clause in a package's `__init__` imposes the same
    /// obligation on the modules its patterns name. With this off the declaration
    /// still parses and still lowers, but nothing is checked against it — and a
    /// declaration written anyway is reported, rather than quietly doing nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
            # hold every module in `backends` to the `Backend` protocol
            module-api = true
        "#
    )]
    module_api: Option<bool>,

    /// Whether a `build:` block declares build stamps.
    ///
    /// `build:` declares the values a build settles when it produces the artifact
    /// — the commit it was built from, the time it was built at — and each is read
    /// as `build.NAME` at the type it declares. With this off the block still
    /// parses and still lowers, so a program that reads a stamp keeps working, but
    /// writing one is reported: nothing settles a stamp the project has not asked
    /// for, so it would silently stand for its default, or for nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
            # let the program read the commit it was built from
            build-stamps = true
        "#
    )]
    build_stamps: Option<bool>,
}

impl ExperimentalOptions {
    fn to_settings(&self) -> ExperimentalSettings {
        ExperimentalSettings {
            module_api: self.module_api.unwrap_or_default(),
            build_stamps: self.build_stamps.unwrap_or_default(),
        }
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Hash,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct AnalysisOptions {
    /// Whether ty should use strict narrowing for unspecialized generic classes in
    /// `isinstance()` and `issubclass()` checks, as well as `match` class patterns.
    ///
    /// When enabled, ty narrows to the top materialization of the class. For example,
    /// `isinstance(value, list)` narrows a value of type `object` to `Top[list[Unknown]]`,
    /// representing the (infinite) union of all possible `list` specializations. Iterating
    /// over the list would yield values of type `object`.
    ///
    /// When disabled, ty uses gradual generic narrowing, preserving compatible type
    /// arguments from the original type where possible. For example,
    /// `isinstance(value, list)` narrows a value of type `Sequence[int]` to `list[int]`.
    /// If no specialization is available, the same check narrows a value of type `object`
    /// to `list[Unknown]`; items of any type can then be appended to the list. Class
    /// patterns such as `case list():` follow the same behavior.
    ///
    /// Defaults to `false`.
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
            # Use the top materialization when narrowing to an unspecialized generic class
            strict-generic-narrowing = true
        "#
    )]
    pub strict_generic_narrowing: Option<bool>,

    /// Configure ty's behavior regarding type inference and narrowing of equality
    /// checks.
    ///
    /// Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.
    ///
    /// With this option disabled, ty makes various assumptions about equality checks that
    /// match the intuitions of most Python programmers, but may not be fully sound in all
    /// situations. Leaving it enabled makes ty conservative about those assumptions, making it
    /// less likely to infer `Literal[True]` or `Literal[False]` as the result of an
    /// equality check. This has various effects on type checking, including fewer type
    /// narrowing opportunities and more conservative assumptions regarding control flow.
    ///
    /// One such unsound assumption is narrowing an object `x` of type `str` to `Literal["a"]`
    /// after an `if x == "a"` check. This is unsound because a subclass of `str` with value
    /// `"a"` will (by default) compare equal to `"a"`, but will not be of type `Literal["a"]`:
    ///
    /// ```pycon
    /// >>> # `Literal["a"]` can only be inhabited by instances of exactly `str`, not
    /// >>> # subclasses, but str subclasses compare equal by default:
    /// >>> class StringSubclass(str): ...
    /// ...
    /// >>> StringSubclass("a") == "a"
    /// True
    /// >>>
    /// >>> # This also applies to `StrEnum`s:
    /// >>> from enum import StrEnum
    /// >>> class MyEnum(StrEnum):
    /// ...     A = "a"
    /// ...
    /// >>> MyEnum.A == "a"
    /// True
    /// ```
    ///
    /// This option prevents the unsound narrowing of `x` to `Literal["a"]`, and instead keeps
    /// it as `str`:
    ///
    /// ```python
    /// from typing import Literal
    ///
    /// def parse(value: str) -> Literal["a"] | None:
    ///     # with `strict-equality-semantics` enabled, no narrowing will occur here,
    ///     # and an error will be emitted on the `return` statement.
    ///     if value == "a":
    ///         return value
    ///     return None
    /// ```
    ///
    /// Another assumption ty makes by default is that subclasses will never override `__eq__` or
    /// `__ne__`. This allows ty to narrow the following union based on an equality check, despite
    /// the fact that an instance of a subclass of `Foo` could compare equal to `None`, and it's
    /// perfectly valid to pass an instance of a subclass into the `x` parameter of this function:
    ///
    /// ```python
    /// def narrow(x: Foo | None, other: Foo) -> None:
    ///     if x == other:
    ///         # with this option enabled, `x` still has type `Foo | None` here,
    ///         # since it is legal to subclass `Foo` and override its `__eq__` method.
    ///         reveal_type(x)
    /// ```
    ///
    /// Many operations in Python implicitly call `__eq__` under the hood, and this option
    /// impacts those too. For example, it also impacts narrowing from `in` checks, and narrowing
    /// in `match` statements that use value patterns:
    ///
    /// ```python
    /// def narrow_in(x: Foo | None, other: list[Foo]) -> None:
    ///     if x in other:
    ///         # with this option enabled, `x` still has type `Foo | None` here,
    ///         # since the `in` operator implicitly calls `__eq__` on each element of `other`.
    ///         reveal_type(x)
    ///
    ///
    /// def narrow_match(x: str) -> None:
    ///     match x:
    ///         case "a":
    ///             # with this option enabled, `x` still has type `str` here,
    ///             # since this `case` branch will be taken by any object that compares
    ///             # equal to `"a"`, including subclasses of `str`.
    ///             reveal_type(x)
    /// ```
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
        # Preserve broad builtin types instead of narrowing them to literals
        strict-equality-semantics = true
        "#
    )]
    #[serde(alias = "strict-literal-narrowing")]
    pub strict_equality_semantics: Option<bool>,

    /// Whether ty should respect `type: ignore` comments.
    ///
    /// When set to `false`, `type: ignore` comments are treated like any other normal
    /// comment and can't be used to suppress ty errors (you have to use `ty: ignore` instead).
    ///
    /// Setting this option can be useful when using ty alongside other type checkers or when
    /// you prefer using `ty: ignore` over `type: ignore`.
    ///
    /// Defaults to `true`.
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Disable support for `type: ignore` comments
        respect-type-ignore-comments = false
        "#
    )]
    pub respect_type_ignore_comments: Option<bool>,

    /// Whether to disable "fluid specializations", a basedpython feature that widens the
    /// inferred generic specialization of an unannotated binding flow-sensitively based on
    /// its later uses in the same scope.
    ///
    /// When set to `true`, each unannotated binding keeps the specialization it was inferred
    /// with at its creation site; later uses no longer widen or lock it.
    ///
    /// Defaults to `false`, and to `true` under the `ty-compatible` type checking preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
        # Turn off fluid specializations
        disable-fluid-specializations = true
        "#
    )]
    pub disable_fluid_specializations: Option<bool>,

    /// Whether a `let` or `var` declaration written inside a block binds its name for
    /// that block only. This is a basedpython feature.
    ///
    /// Python has no block scopes: a name bound anywhere in a function is a local of
    /// that whole function, and the python a `.by` file lowers to keeps it that way. So
    /// this is a rule the checker enforces rather than something the emitted code does:
    ///
    /// ```by
    /// if flag:
    ///     let a = 1
    ///
    /// print(a)  # error: `a` is not in scope here
    /// ```
    ///
    /// Only the binding keyword scopes a name to its block. A plain `a = 1` binds for
    /// the whole enclosing function or module, as it does in python.
    ///
    /// Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Let a `let` or `var` in a block be visible for the rest of the scope
        block-scoped-declarations = false
        "#
    )]
    pub block_scoped_declarations: Option<bool>,

    /// Whether `float` and `complex` annotations mean *only* themselves. This is a
    /// basedpython feature.
    ///
    /// The typing spec's special case says an `int` is acceptable wherever a `float` is
    /// asked for, so `x: float` really declares `int | float`. A `.by` file opts out of
    /// that already; this makes the same model available to a `.py` one, per module.
    ///
    /// It is not only a checking question. The wider annotation is why a `.py`
    /// `list[float]` cannot be laid out as an unboxed buffer and a `.py` class cannot
    /// have `double` fields, so `by compile` reads this to choose a representation.
    ///
    /// Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
        # `float` means float, so a numeric module compiles to unboxed doubles
        strict-float = true
        "#
    )]
    pub strict_float: Option<bool>,

    /// Whether to infer sound (non-gradual) types wherever a precise type is available. This is a
    /// basedpython feature.
    ///
    /// Python's gradual guarantee requires a type checker to fall back to a gradual type whenever
    /// an annotation is missing, even when a precise type could be inferred. In a fully typed
    /// project that is pure boilerplate: it forces an annotation to be written for something the
    /// checker already knows. When set to `true`, this option deliberately breaks the gradual
    /// guarantee and uses the precise type instead. It affects:
    ///
    /// - **Unannotated parameters**: each one opens an anonymous type parameter named after it,
    ///   bounded by everything the function requires of it — the promoted type of its default, the
    ///   members its body reads and calls, the parameters it is forwarded into, and any `assert` at
    ///   the top of the body. So `def f(a=1)` rejects a `str` at a call site, and
    ///   `def ident(x): return x` is inferred as the identity function. A lambda parameter with a
    ///   default takes that default's promoted type directly.
    /// - **Unannotated return types**: the union of what the body returns, plus `None` when control
    ///   can fall off the end. An empty body returns `None` and a body that always raises returns
    ///   `Never`; a generator returns a generator.
    /// - **Unannotated methods that override a base method**: the parameter and return types are
    ///   inherited from the overridden method, including from `Protocol` members and
    ///   `abstractmethod` declarations.
    /// - **Bare `ClassVar` annotations**: `x: ClassVar = 1` declares `int` rather than the union of
    ///   `Unknown` and the inferred type.
    /// - **Empty collection literals**: `[]` has element type `Never`, so passing one to a generic
    ///   call solves from it precisely instead of leaking `Unknown`.
    ///
    /// An explicit annotation always takes priority over any of the above.
    ///
    /// Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Fall back to a gradual type wherever an annotation is missing
        sound-types = false
        "#
    )]
    pub sound_types: Option<bool>,

    /// Whether a function with no annotations is given the signature its body determines. This is
    /// a basedpython feature.
    ///
    /// Python's gradual guarantee makes an unannotated `def` say nothing: its parameters accept
    /// anything and it returns `Unknown`. That is the largest remaining source of `Unknown` in an
    /// otherwise typed project, and it silently swallows real mistakes. With this enabled, the
    /// missing half of the signature is recovered from what the function itself already determines:
    ///
    /// - **Each unannotated parameter** opens an anonymous type parameter named after it — the same
    ///   hole `some` spells by hand — bounded by everything the function requires of it: the
    ///   promoted type of its default, the members its body reads and calls, the parameters it is
    ///   forwarded into, and any `assert` at the top of the body. Naming the hole is what keeps
    ///   what a call passes in connected to what it gets back, so `def ident(x): return x` is
    ///   inferred as the identity function.
    /// - **A missing return type** is the union of what the body returns, plus `None` when control
    ///   can also fall off the end. An empty body returns `None`, a body that always raises returns
    ///   `Never`, and a generator returns a generator.
    ///
    /// Nothing is invented from a use this analysis cannot read, so such a parameter stays gradual
    /// and its body keeps type-checking exactly as it did. An explicit annotation always wins, and
    /// so does anything an overload group or an overridden base method already supplies.
    ///
    /// Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Leave an unannotated function gradual
        infer-unannotated-signatures = false
        "#
    )]
    pub infer_unannotated_signatures: Option<bool>,

    /// Whether a private attribute leaves an inferred type parameter bivariant. This is a
    /// basedpython feature.
    ///
    /// A private (single-underscore or name-mangled) member is invisible to external observers, so
    /// it cannot be used to distinguish two specializations of its class, and therefore cannot
    /// constrain the class's variance:
    ///
    /// ```python
    /// class A[T]:
    ///     _t: T
    /// ```
    ///
    /// With this option enabled, `T` is inferred bivariant: nothing on `A`'s public surface
    /// mentions `T`, so `A[int]` and `A[object]` are mutually assignable. As soon as a public
    /// member mentions `T`, that member drives the inference as usual.
    ///
    /// When set to `false`, a private attribute is instead treated as immutable-but-readable,
    /// which constrains the type parameter to covariance.
    ///
    /// Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Let private attributes constrain inferred variance to covariance
        bivariant-private-attributes = false
        "#
    )]
    pub bivariant_private_attributes: Option<bool>,

    /// Whether a type variable that a call leaves unsolved is solved to `Never`. This is a
    /// basedpython feature.
    ///
    /// A call can leave a type variable entirely unsolved, because no argument mentions it:
    ///
    /// ```python
    /// def f[T]() -> T: ...
    ///
    /// a = f()
    /// ```
    ///
    /// `Never` is the precise answer here: no value ever reaches that position, so nothing the
    /// call returns can be observed at type `T`. When set to `false`, the type variable falls back
    /// to the gradual `Unknown` instead, which silences any error that would follow from the call
    /// site.
    ///
    /// This applies where the type variable is an output. Where it is instead written through or
    /// passed back in — the element of an invariant `list[T]`, the parameter of a returned
    /// `Callable[[T], R]` — `Never` would say that nothing can ever be put there, so an invariant
    /// or contravariant occurrence keeps the gradual `Unknown`.
    ///
    /// A PEP 696 default (`def f[T = str]()`) always takes priority, and a `ParamSpec`,
    /// `TypeVarTuple` or keyword-variadic pack is unaffected because `Never` is not a valid
    /// solution for one.
    ///
    /// Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"true"#,
        value_type = "bool",
        example = r#"
        # Solve an unsolved type variable to `Unknown` rather than `Never`
        precise-unsolved-typevars = false
        "#
    )]
    pub precise_unsolved_typevars: Option<bool>,

    /// A list of classes whose values do not count as a distinct member of an
    /// [`overlapping-condition`](rules.md#overlapping-condition).
    ///
    /// `if not x` over an `int | None` selects both a falsy `int` and `None`, and is reported
    /// because the branch cannot tell them apart. Listing `int` here says that conflating a falsy
    /// `int` with anything else is fine, so only `None` is left and the condition is accepted.
    ///
    /// Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
    /// spelled bare (`int`), and `None` stands for the type of `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            # Accept a falsy `int` or `str` sharing a branch with another member
            overlapping-condition-exempt-types = ["int", "str"]
        "#
    )]
    pub overlapping_condition_exempt_types: Option<Vec<RangedValue<String>>>,

    /// A list of classes never reported as an
    /// [`implicit-object-repr`](rules.md#implicit-object-repr).
    ///
    /// A class deriving from one of these is exempt too, so listing a base opts out a whole
    /// hierarchy.
    ///
    /// Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
    /// spelled bare (`int`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            # Never report a bare `Thread` or `Lock`
            implicit-object-repr-exempt-types = ["threading.Thread", "threading.Lock"]
        "#
    )]
    pub implicit_object_repr_exempt_types: Option<Vec<RangedValue<String>>>,

    /// A list of classes whose stub is taken at its word when looking for an
    /// [`implicit-object-repr`](rules.md#implicit-object-repr).
    ///
    /// A stub normally settles nothing, because it omits `__str__` and `__repr__` whether or not
    /// the runtime class has them — `int` declares neither and still prints as a number. For a
    /// class listed here the omission counts as real, the same way it would for a class written
    /// in source, so a value of that class is reported unless the stub does declare one.
    ///
    /// Defaults to the two whose bare repr is seen most often: `types.FunctionType`, which prints
    /// `<function f at 0x...>`, and `builtins.type`, which prints `<class 'C'>`.
    ///
    /// Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
    /// spelled bare (`int`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"["types.FunctionType", "builtins.type"]"#,
        value_type = "list[str]",
        example = r#"
            # Also report a bare module object
            implicit-object-repr-report-types = ["types.FunctionType", "type", "types.ModuleType"]
        "#
    )]
    pub implicit_object_repr_report_types: Option<Vec<RangedValue<String>>>,

    /// Whether an instance with no `__bool__` and no `__len__` counts as always truthy when
    /// looking for an [`overlapping-condition`](rules.md#overlapping-condition).
    ///
    /// Such an instance is only *ambiguously* truthy — a subclass may define `__bool__` — so by
    /// default it is a falsy member of `if not x` just as `None` is. Enabling this assumes the
    /// class means what it looks like it means, which drops the reports for the very common
    /// `if not x` over an optional instance.
    ///
    /// Defaults to `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"false"#,
        value_type = "bool",
        example = r#"
            # `if not x` over a `Foo | None` only selects `None`
            overlapping-condition-assume-truthy-instances = true
        "#
    )]
    pub overlapping_condition_assume_truthy_instances: Option<bool>,

    /// A list of module glob patterns for which `unresolved-import` diagnostics should be suppressed.
    ///
    /// Details on supported glob patterns:
    /// - `*` matches zero or more characters except `.`. For example, `foo.*` matches `foo.bar` but
    ///   not `foo.bar.baz`; `foo*` matches `foo` and `foobar` but not `foo.bar` or `barfoo`; and `*foo`
    ///   matches `foo` and `barfoo` but not `foo.bar` or `foobar`.
    /// - `**` matches any number of module components (e.g., `foo.**` matches `foo`, `foo.bar`, etc.)
    /// - Prefix a pattern with `!` to exclude matching modules
    ///
    /// When multiple patterns match, later entries take precedence.
    ///
    /// Glob patterns can be used in combinations with each other. For example, to suppress errors for
    /// any module where the first component contains the substring `test`, use `*test*.**`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            # Suppress errors for all `test` modules except `test.foo`
            allowed-unresolved-imports = ["test.**", "!test.foo"]
        "#
    )]
    pub allowed_unresolved_imports: Option<Vec<RangedValue<String>>>,

    /// A list of module glob patterns whose imports should be replaced with `typing.Any`.
    ///
    /// Unlike `allowed-unresolved-imports`, this setting replaces the module's type information
    /// with `typing.Any` even if the module can be resolved. Import diagnostics are
    /// unconditionally suppressed for matching modules.
    ///
    /// - Prefix a pattern with `!` to exclude matching modules
    ///
    /// When multiple patterns match, later entries take precedence.
    ///
    /// Glob patterns can be used in combinations with each other. For example, to suppress errors for
    /// any module where the first component contains the substring `test`, use `*test*.**`.
    ///
    /// When multiple patterns match, later entries take precedence.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"[]"#,
        value_type = "list[str]",
        example = r#"
            # Replace all pandas and numpy imports with Any
            replace-imports-with-any = ["pandas.**", "numpy.**"]
        "#
    )]
    pub replace_imports_with_any: Option<Vec<RangedValue<String>>>,

    /// The requirement groups the matching files may import from.
    ///
    /// `project` names `[project].dependencies`, an extra or a PEP 735 dependency group
    /// is named by its own name, and `*` names every group.
    ///
    /// When this is unset, a file may import from every group unless it is part of what
    /// the project ships — the modules named by `shipped-modules` — in which case it may
    /// import only `project` and the extras. Nothing the project ships can import a
    /// dependency group, because nothing installs one alongside the project.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "list[str]",
        example = r#"
            [[tool.ty.overrides]]
            include = ["tests/**"]

            [tool.ty.overrides.analysis]
            dependency-groups = ["project", "dev", "test"]
        "#
    )]
    pub dependency_groups: Option<Vec<RangedValue<String>>>,

    /// The top-level modules the project ships.
    ///
    /// Defaults to the module named after `[project].name`: a project named `my-lib`
    /// ships `my_lib`. Only a project that ships several unrelated modules, or one whose
    /// module is not named after it, needs to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "list[str]",
        example = r#"
            shipped-modules = ["foo", "foo_plugins"]
        "#
    )]
    pub shipped_modules: Option<Vec<RangedValue<String>>>,

    /// The dependencies this project hands to its own users.
    ///
    /// A library whose interface is partly made of another distribution's types — one that
    /// returns numpy arrays, or takes a pydantic model — can say so, and then a project
    /// that depends on this one may import those distributions without declaring them
    /// itself.
    ///
    /// Only what the project already depends on can be exported, and the claim only
    /// travels one link: exporting a distribution does not export whatever *it* depends
    /// on, unless that distribution exports it in turn.
    ///
    /// This is written into the `by.typed` marker when the project is built, because that
    /// is what its users have — a `pyproject.toml` is not installed with the package.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = "list[str]",
        example = r#"
            exported-dependencies = ["numpy"]
        "#
    )]
    pub exported_dependencies: Option<Vec<RangedValue<String>>>,
}

impl AnalysisOptions {
    pub(super) fn to_settings(
        &self,
        db: &dyn Db,
        preset: TypeCheckingPreset,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> AnalysisSettings {
        let Self {
            strict_generic_narrowing,
            strict_equality_semantics,
            respect_type_ignore_comments,
            allowed_unresolved_imports,
            replace_imports_with_any,
            block_scoped_declarations,
            strict_float,
            disable_fluid_specializations,
            sound_types,
            infer_unannotated_signatures,
            bivariant_private_attributes,
            precise_unsolved_typevars,
            overlapping_condition_exempt_types,
            overlapping_condition_assume_truthy_instances,
            implicit_object_repr_exempt_types,
            implicit_object_repr_report_types,
            dependency_groups,
            shipped_modules,
            exported_dependencies,
        } = self;

        let AnalysisSettings {
            strict_generic_narrowing: strict_generic_narrowing_default,
            strict_equality_semantics: strict_equality_semantics_default,
            respect_type_ignore_comments: respect_type_ignore_default,
            allowed_unresolved_imports: allowed_unresolved_imports_default,
            replace_imports_with_any: replace_imports_with_any_default,
            block_scoped_declarations: block_scoped_declarations_default,
            strict_float: strict_float_default,
            disable_fluid_specializations: disable_fluid_specializations_default,
            sound_types: sound_types_default,
            infer_unannotated_signatures: infer_unannotated_signatures_default,
            bivariant_private_attributes: bivariant_private_attributes_default,
            precise_unsolved_typevars: precise_unsolved_typevars_default,
            overlapping_condition_exempt_types: overlapping_condition_exempt_types_default,
            overlapping_condition_assume_truthy_instances:
                overlapping_condition_assume_truthy_instances_default,
            implicit_object_repr_exempt_types: implicit_object_repr_exempt_types_default,
            implicit_object_repr_report_types: implicit_object_repr_report_types_default,
            dependency_groups: dependency_groups_default,
            shipped_modules: shipped_modules_default,
            exported_dependencies: exported_dependencies_default,
        } = AnalysisSettings::from_preset(preset);

        let allowed_unresolved_imports =
            if let Some(allowed_unresolved_imports) = allowed_unresolved_imports {
                build_module_glob_set(db, allowed_unresolved_imports, "allowed_unresolved_imports")
                    .unwrap_or_else(|error| {
                        diagnostics.push(*error);
                        ModuleGlobSet::empty()
                    })
            } else {
                allowed_unresolved_imports_default
            };

        let replace_imports_with_any =
            if let Some(replace_imports_with_any) = replace_imports_with_any {
                build_module_glob_set(db, replace_imports_with_any, "replace_imports_with_any")
                    .unwrap_or_else(|error| {
                        diagnostics.push(*error);
                        ModuleGlobSet::empty()
                    })
            } else {
                replace_imports_with_any_default
            };

        AnalysisSettings {
            strict_generic_narrowing: strict_generic_narrowing
                .unwrap_or(strict_generic_narrowing_default),
            strict_equality_semantics: strict_equality_semantics
                .unwrap_or(strict_equality_semantics_default),
            respect_type_ignore_comments: respect_type_ignore_comments
                .unwrap_or(respect_type_ignore_default),
            allowed_unresolved_imports,
            replace_imports_with_any,
            block_scoped_declarations: block_scoped_declarations
                .unwrap_or(block_scoped_declarations_default),
            strict_float: strict_float.unwrap_or(strict_float_default),
            disable_fluid_specializations: disable_fluid_specializations
                .unwrap_or(disable_fluid_specializations_default),
            sound_types: sound_types.unwrap_or(sound_types_default),
            infer_unannotated_signatures: infer_unannotated_signatures
                .unwrap_or(infer_unannotated_signatures_default),
            bivariant_private_attributes: bivariant_private_attributes
                .unwrap_or(bivariant_private_attributes_default),
            precise_unsolved_typevars: precise_unsolved_typevars
                .unwrap_or(precise_unsolved_typevars_default),
            overlapping_condition_exempt_types: overlapping_condition_exempt_types
                .as_ref()
                .map(|types| {
                    build_class_name_list(
                        db,
                        types,
                        "overlapping-condition-exempt-types",
                        diagnostics,
                    )
                })
                .unwrap_or(overlapping_condition_exempt_types_default),
            overlapping_condition_assume_truthy_instances:
                overlapping_condition_assume_truthy_instances
                    .unwrap_or(overlapping_condition_assume_truthy_instances_default),
            implicit_object_repr_exempt_types: implicit_object_repr_exempt_types
                .as_ref()
                .map(|types| {
                    build_class_name_list(
                        db,
                        types,
                        "implicit-object-repr-exempt-types",
                        diagnostics,
                    )
                })
                .unwrap_or(implicit_object_repr_exempt_types_default),
            implicit_object_repr_report_types: implicit_object_repr_report_types
                .as_ref()
                .map(|types| {
                    build_class_name_list(
                        db,
                        types,
                        "implicit-object-repr-report-types",
                        diagnostics,
                    )
                })
                .unwrap_or(implicit_object_repr_report_types_default),
            dependency_groups: dependency_groups
                .as_ref()
                .map(|groups| groups.iter().map(|group| Box::from(&***group)).collect())
                .or(dependency_groups_default),
            shipped_modules: shipped_modules
                .as_ref()
                .map(|modules| {
                    modules
                        .iter()
                        .map(|module| Box::from(&***module))
                        .collect::<Box<[Box<str>]>>()
                })
                .or(shipped_modules_default),
            exported_dependencies: exported_dependencies
                .as_ref()
                .map(|exported| {
                    exported
                        .iter()
                        .map(|name| Box::from(&***name))
                        .collect::<Box<[Box<str>]>>()
                })
                .or(exported_dependencies_default),
        }
    }
}

/// Collect a list of configured class names, rejecting entries that are not spelled like one.
///
/// Only the spelling is checked. Whether a well-formed name resolves to a class that exists is
/// not — an entry that resolves to nothing simply never matches, the same way a well-formed glob
/// that matches no module is not an error.
fn build_class_name_list(
    db: &dyn Db,
    names: &[RangedValue<String>],
    option_name: &'static str,
    diagnostics: &mut Vec<OptionDiagnostic>,
) -> Box<[Box<str>]> {
    let is_identifier = |segment: &str| {
        let mut chars = segment.chars();
        chars
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_')
            && chars.all(|char| char.is_alphanumeric() || char == '_')
    };

    let mut collected = Vec::with_capacity(names.len());
    for name in names {
        if name.split('.').all(is_identifier) {
            collected.push(Box::from(&***name));
        } else {
            // Fatal, like every other diagnostic in this vec — `Options::to_settings` turns the
            // first analysis diagnostic into a `ToSettingsError` regardless of its severity, and
            // that is the same treatment a malformed `allowed-unresolved-imports` glob gets.
            diagnostics.push(
                OptionDiagnostic::new(
                    DiagnosticId::InvalidClassName,
                    format!("`{}` is not a class name", &***name),
                    Severity::Error,
                )
                .with_source_sub(
                    db,
                    name,
                    "class name",
                    option_name,
                    "Expected a bare or qualified class name, such as `int` or `decimal.Decimal`",
                ),
            );
        }
    }
    collected.into_boxed_slice()
}

fn build_module_glob_set(
    db: &dyn Db,
    patterns: &[RangedValue<String>],
    option_name: &str,
) -> Result<ModuleGlobSet, Box<OptionDiagnostic>> {
    let mut builder = ModuleGlobSetBuilder::new();

    for glob in patterns {
        if let Err(error) = builder.add(glob) {
            let diagnostic = OptionDiagnostic::new(
                DiagnosticId::InvalidGlob,
                format!("Invalid glob pattern `{error}`"),
                Severity::Error,
            );

            return Err(diagnostic
                .with_source_sub(db, glob, "glob", option_name, error)
                .into());
        }
    }

    builder.build().map_err(|_| {
        let diagnostic = OptionDiagnostic::new(
            DiagnosticId::InvalidGlob,
            "The `{option_name}` patterns resulted in a regex that is too large".to_string(),
            Severity::Error,
        );

        Box::new(diagnostic.sub(SubDiagnostic::new(
            SubDiagnosticSeverity::Info,
            "Please open an issue on the ty repository \
            and share the patterns that caused the error.",
        )))
    })
}

/// Configuration override that applies to specific files based on glob patterns.
///
/// An override allows you to apply different rule configurations to specific
/// files or directories. Multiple overrides can match the same file, with
/// later overrides take precedence. Override rules take precedence over global
/// rules for matching files.
///
/// For example, to relax enforcement of rules in test files:
///
/// ```toml
/// [[tool.ty.overrides]]
/// include = ["tests/**", "**/test_*.py"]
///
/// [tool.ty.overrides.rules]
/// possibly-unresolved-reference = "warn"
/// ```
///
/// Or, to ignore a rule in generated files but retain enforcement in an important file:
///
/// ```toml
/// [[tool.ty.overrides]]
/// include = ["generated/**"]
/// exclude = ["generated/important.py"]
///
/// [tool.ty.overrides.rules]
/// possibly-unresolved-reference = "ignore"
/// ```
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    Combine,
    Serialize,
    Deserialize,
    RustDoc,
    get_size2::GetSize,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct OverridesOptions(Vec<RangedValue<OverrideOptions>>);

impl OptionsMetadata for OverridesOptions {
    fn documentation() -> Option<&'static str> {
        Some(<Self as RustDoc>::rust_doc())
    }

    fn record(visit: &mut dyn Visit) {
        OptionSet::of::<OverrideOptions>().record(visit);
    }
}

impl Deref for OverridesOptions {
    type Target = [RangedValue<OverrideOptions>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    Eq,
    PartialEq,
    Combine,
    Serialize,
    Deserialize,
    OptionsMetadata,
    get_size2::GetSize,
)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OverrideOptions {
    /// A list of file and directory patterns to include for this override.
    ///
    /// The `include` option follows a similar syntax to `.gitignore` but reversed:
    /// Including a file or directory will make it so that it (and its contents)
    /// are affected by this override.
    ///
    /// If not specified, defaults to `["**"]` (matches all files).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            [[tool.ty.overrides]]
            include = [
                "src",
                "tests",
            ]
        "#
    )]
    include: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// A list of file and directory patterns to exclude from this override.
    ///
    /// Patterns follow a syntax similar to `.gitignore`.
    /// Exclude patterns take precedence over include patterns within the same override.
    ///
    /// If not specified, defaults to `[]` (excludes no files).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"null"#,
        value_type = r#"list[str]"#,
        example = r#"
            [[tool.ty.overrides]]
            exclude = [
                "generated",
                "*.proto",
                "tests/fixtures/**",
                "!tests/fixtures/important.py"  # Include this one file
            ]
        "#
    )]
    exclude: Option<RangedValue<Vec<RelativeGlobPattern>>>,

    /// Rule overrides for files matching the include/exclude patterns.
    ///
    /// These rules will be merged with the global rules, with override rules
    /// taking precedence for matching files. You can set rules to different
    /// severity levels or disable them entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[option(
        default = r#"{...}"#,
        value_type = r#"dict[RuleName | "all", "ignore" | "warn" | "error"]"#,
        example = r#"
            [[tool.ty.overrides]]
            include = ["src"]

            [tool.ty.overrides.rules]
            possibly-unresolved-reference = "ignore"
        "#
    )]
    rules: Option<Rules>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[option_group]
    analysis: Option<AnalysisOptions>,
}

trait ToOverride {
    fn to_override(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        preset: TypeCheckingPreset,
        global_rules: Option<&Rules>,
        global_analysis: Option<&AnalysisOptions>,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<Option<Override>, Box<OptionDiagnostic>>;
}

impl ToOverride for RangedValue<OverrideOptions> {
    fn to_override(
        &self,
        db: &dyn Db,
        project_root: &SystemPath,
        preset: TypeCheckingPreset,
        global_rules: Option<&Rules>,
        global_analysis: Option<&AnalysisOptions>,
        diagnostics: &mut Vec<OptionDiagnostic>,
    ) -> Result<Option<Override>, Box<OptionDiagnostic>> {
        let rules = self.rules.or_default();
        let analysis = self.analysis.or_default();

        // First, warn about incorrect or useless overrides.
        if rules.is_empty() && *analysis == AnalysisOptions::default() {
            let mut diagnostic = OptionDiagnostic::new(
                DiagnosticId::UselessOverridesSection,
                "Useless `overrides` section".to_string(),
                Severity::Warning,
            );

            diagnostic = if self.rules.is_none() && self.analysis.is_none() {
                diagnostic = diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "It has no `rules` or `analysis` table",
                ));
                diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "Add a `[overrides.rules]` or `[overrides.analysis]` table...",
                ))
            } else {
                if self.rules.is_some() && rules.is_empty() {
                    diagnostic = diagnostic.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "The `rules` table is empty",
                    ));
                    diagnostic = diagnostic.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "Add a rule to `[overrides.rules]` to override specific rules...",
                    ));
                }

                if self.analysis.is_some() && *analysis == AnalysisOptions::default() {
                    diagnostic = diagnostic.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        "The `analysis` table is empty",
                    ));
                }

                diagnostic
            };

            diagnostic = diagnostic.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "or remove the `[[overrides]]` section if there's nothing to override",
            ));

            // Add source annotation if we have source information
            if let Some(file) = self.source().file(db) {
                let annotation =
                    Annotation::primary(Span::from(file).with_optional_range(self.range()))
                        .message("This overrides section overrides no settings");
                diagnostic = diagnostic.with_annotation(Some(annotation));
            }

            diagnostics.push(diagnostic);
            // Return `None`, because this override doesn't override anything
            return Ok(None);
        }

        let include_missing = self.include.is_none();
        let exclude_empty = self
            .exclude
            .as_ref()
            .is_none_or(|exclude| exclude.is_empty());

        if include_missing && exclude_empty {
            // Neither include nor exclude specified - applies to all files
            let mut diagnostic = OptionDiagnostic::new(
                DiagnosticId::UnnecessaryOverridesSection,
                "Unnecessary `overrides` section".to_string(),
                Severity::Warning,
            );

            diagnostic = if self.exclude.is_none() {
                diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "It has no `include` or `exclude` option restricting the files",
                ))
            } else {
                diagnostic.sub(SubDiagnostic::new(
                    SubDiagnosticSeverity::Info,
                    "It has no `include` option and `exclude` is empty",
                ))
            };

            diagnostic = diagnostic.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "Restrict the files by adding a pattern to `include` or `exclude`...",
            ));

            diagnostic = diagnostic.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "or remove the `[[overrides]]` section \
                and merge the configuration into the root `[rules]` table \
                if the configuration should apply to all files",
            ));

            // Add source annotation if we have source information
            if let Some(file) = self.source().file(db) {
                let annotation =
                    Annotation::primary(Span::from(file).with_optional_range(self.range()))
                        .message("This overrides section applies to all files");
                diagnostic = diagnostic.with_annotation(Some(annotation));
            }

            diagnostics.push(diagnostic);
        }

        // The override is at least (partially) valid.
        // Construct the matcher and resolve the settings.
        let include = build_include_filter(
            db,
            project_root,
            self.include.as_ref(),
            GlobFilterContext::Overrides,
            diagnostics,
        )?;

        let exclude = build_exclude_filter(
            db,
            project_root,
            self.exclude.as_ref(),
            &[],
            GlobFilterContext::Overrides,
            diagnostics,
        )?;

        let files = IncludeExcludeFilter::new(include, exclude);

        // Merge global rules with override rules, with override rules taking precedence
        let mut merged_rules = rules.into_owned();

        if let Some(global_rules) = global_rules {
            merged_rules = merged_rules.combine(global_rules.clone());
        }

        // Convert merged rules to rule selection
        let rule_selection = merged_rules.to_rule_selection(db, preset, diagnostics);

        let mut merged_analysis = analysis.into_owned();

        if let Some(global_analysis) = global_analysis {
            merged_analysis = merged_analysis.combine(global_analysis.clone());
        }

        let analysis = merged_analysis.to_settings(db, preset, diagnostics);

        let override_instance = Override {
            files,
            options: Arc::new(InnerOverrideOptions {
                rules: self.rules.clone(),
                analysis: self.analysis.clone(),
            }),
            settings: Arc::new(OverrideSettings {
                rules: rule_selection,
                analysis,
            }),
        };

        Ok(Some(override_instance))
    }
}

/// The options for an override but without the include/exclude patterns.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Combine, get_size2::GetSize)]
pub(crate) struct InnerOverrideOptions {
    /// Raw rule options as specified in the configuration.
    /// Used when multiple overrides match a file and need to be merged.
    pub(crate) rules: Option<Rules>,

    pub(crate) analysis: Option<AnalysisOptions>,
}

/// A failure to resolve a project's or standalone script's program settings.
#[derive(Debug, Error)]
pub enum ToProgramSettingsError {
    /// The explicitly configured Python environment could not be resolved.
    #[error(transparent)]
    PythonEnvironment(SitePackagesDiscoveryError),

    /// No explicitly configured Python environment was available, and discovery failed.
    #[error("Failed to discover local Python environment")]
    PythonEnvironmentDiscovery(#[source] SitePackagesDiscoveryError),

    /// The resolved Python environment did not contain usable site-packages directories.
    #[error("Failed to discover the site-packages directory")]
    SitePackagesDiscovery(#[source] SitePackagesDiscoveryError),

    /// One of the configured Python module search paths could not be resolved.
    #[error(transparent)]
    SearchPaths(#[from] SearchPathSettingsError),
}

impl ToProgramSettingsError {
    /// Returns the program-settings error without its optional diagnostic detail.
    pub(crate) fn message(&self) -> String {
        self.to_string()
    }

    /// Returns details for failures whose message only identifies the failed operation.
    pub(crate) fn hint(&self) -> Option<String> {
        match self {
            Self::PythonEnvironmentDiscovery(error) | Self::SitePackagesDiscovery(error) => {
                Some(error.to_string())
            }
            Self::PythonEnvironment(_) | Self::SearchPaths(_) => None,
        }
    }

    pub(crate) fn setting_source<'a>(
        &self,
        options: &'a Options,
    ) -> Option<(&'a ValueSource, Option<TextRange>)> {
        let environment = options.environment.as_ref()?;

        match self {
            Self::PythonEnvironment(_) | Self::SitePackagesDiscovery(_) => environment
                .python
                .as_ref()
                .map(|setting| (setting.source(), setting.range())),
            Self::SearchPaths(
                SearchPathSettingsError::FailedToReadVersionsFile { .. }
                | SearchPathSettingsError::VersionsParseError(_),
            ) => environment
                .typeshed
                .as_ref()
                .map(|setting| (setting.source(), setting.range())),
            Self::PythonEnvironmentDiscovery(_)
            | Self::SearchPaths(SearchPathSettingsError::InvalidSearchPath(_)) => None,
        }
    }
}

/// Error returned when the settings can't be resolved because of a hard error.
#[derive(Debug)]
pub struct ToSettingsError {
    diagnostic: Box<OptionDiagnostic>,
    output_format: OutputFormat,
    color: bool,
}

impl ToSettingsError {
    pub(crate) fn pretty<'a>(&'a self, db: &'a dyn Db) -> impl fmt::Display + use<'a> {
        let db: &dyn ruff_db::Db = db;

        fmt::from_fn(move |f| {
            let display_config = DisplayDiagnosticConfig::new("ty")
                .format(self.output_format.into())
                .color(self.color);

            write!(
                f,
                "{}",
                self.diagnostic
                    .to_diagnostic()
                    .display(&db, &display_config)
            )
        })
    }

    pub(crate) fn into_diagnostic(self) -> OptionDiagnostic {
        *self.diagnostic
    }
}

impl Display for ToSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diagnostic.message)
    }
}

impl std::error::Error for ToSettingsError {}

#[cfg(feature = "schemars")]
mod schema {
    impl schemars::JsonSchema for super::Rules {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("Rules")
        }

        fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
            use serde_json::{Map, Value};

            let registry = ty_python_semantic::default_lint_registry();
            let level_schema = generator.subschema_for::<super::Level>();

            let mut properties: Map<String, Value> = registry
                .lints()
                .iter()
                .map(|lint| {
                    let mut schema = schemars::Schema::default();
                    let object = schema.ensure_object();
                    object.insert(
                        "title".to_string(),
                        Value::String(lint.summary().to_string()),
                    );
                    object.insert(
                        "description".to_string(),
                        Value::String(lint.documentation()),
                    );
                    if lint.status.is_deprecated() {
                        object.insert("deprecated".to_string(), Value::Bool(true));
                    }
                    object.insert(
                        "default".to_string(),
                        Value::String(lint.default_level.to_string()),
                    );
                    object.insert(
                        "oneOf".to_string(),
                        Value::Array(vec![level_schema.clone().into()]),
                    );

                    (lint.name().to_string(), schema.into())
                })
                .collect();

            let mut all_schema = schemars::Schema::default();
            let all = all_schema.ensure_object();
            all.insert(
                "title".to_string(),
                Value::String("set the default severity level for all rules".to_string()),
            );
            all.insert(
                "description".to_string(),
                Value::String(
                    "Configure a default severity level for all rules. \
                        Individual rule settings override this default."
                        .to_string(),
                ),
            );
            all.insert(
                "oneOf".to_string(),
                Value::Array(vec![level_schema.clone().into()]),
            );

            properties.insert("all".to_string(), all_schema.into());

            let mut schema = schemars::json_schema!({ "type": "object" });
            let object = schema.ensure_object();
            object.insert("properties".to_string(), Value::Object(properties));
            // Allow unknown rules: ty will warn about them. It gives a better experience when using an older
            // ty version because the schema will not deny rules that have been removed in newer versions.
            object.insert("additionalProperties".to_string(), level_schema.into());

            schema
        }
    }

    impl schemars::JsonSchema for super::CommonAliases {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("CommonAliases")
        }

        fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
            schemars::json_schema!({
                "type": "object",
                "additionalProperties": { "type": "string" },
            })
        }
    }
}

#[derive(Error, Debug)]
pub enum TyTomlError {
    #[error(transparent)]
    TomlSyntax(#[from] toml::de::Error),
}

#[derive(Debug, PartialEq, Eq, Clone, get_size2::GetSize)]
pub struct OptionDiagnostic {
    id: DiagnosticId,
    message: String,
    concise_message: Option<String>,
    severity: Severity,
    annotation: Option<Annotation>,
    sub: Vec<SubDiagnostic>,
}

impl OptionDiagnostic {
    fn new(id: DiagnosticId, message: String, severity: Severity) -> Self {
        Self {
            id,
            message,
            concise_message: None,
            severity,
            annotation: None,
            sub: Vec::new(),
        }
    }

    #[must_use]
    fn with_message(self, message: impl Display) -> Self {
        OptionDiagnostic {
            message: message.to_string(),
            ..self
        }
    }

    #[must_use]
    fn with_concise_message(self, message: impl Display) -> Self {
        OptionDiagnostic {
            concise_message: Some(message.to_string()),
            ..self
        }
    }

    #[must_use]
    fn with_annotation(self, annotation: Option<Annotation>) -> Self {
        OptionDiagnostic { annotation, ..self }
    }

    fn with_source_sub<T>(
        mut self,
        db: &dyn Db,
        value: &RangedValue<T>,
        value_label: &str,
        option_name: &str,
        err: impl Display,
    ) -> Self {
        match value.source() {
            ValueSource::File(_) | ValueSource::ScriptMetadata(_) => {
                if let Some(file) = value.source().file(db) {
                    let concise_message = std::mem::take(&mut self.message);
                    self.with_concise_message(concise_message)
                        .with_message(format_args!("Invalid {value_label}"))
                        .with_annotation(Some(
                            Annotation::primary(
                                Span::from(file).with_optional_range(value.range()),
                            )
                            .message(err.to_string()),
                        ))
                } else {
                    self.sub(SubDiagnostic::new(
                        SubDiagnosticSeverity::Info,
                        format!(
                            "The {value_label} is defined in the `{option_name}` option \
                            in your configuration file"
                        ),
                    ))
                }
            }
            ValueSource::Cli => self.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The {value_label} was specified on the CLI",
            )),
            ValueSource::Editor => self.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                "The {value_label} was specified in the editor settings.",
            )),
            ValueSource::UvMetadata => self.sub(SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                format!("The {value_label} was provided by uv metadata."),
            )),
        }
    }

    #[must_use]
    fn sub(mut self, sub: SubDiagnostic) -> Self {
        self.sub.push(sub);
        self
    }

    pub(crate) fn to_diagnostic(&self) -> Diagnostic {
        let mut diag = Diagnostic::new(self.id, self.severity, &self.message);

        if let Some(concise_message) = &self.concise_message {
            diag.set_concise_message(concise_message);
        }

        if let Some(annotation) = self.annotation.clone() {
            diag.annotate(annotation);
        }

        for sub in &self.sub {
            diag.sub(sub.clone());
        }

        diag
    }
}

trait OrDefault {
    type Target: ToOwned;

    fn or_default(&self) -> Cow<'_, Self::Target>;
}

impl<T> OrDefault for Option<T>
where
    T: Default + ToOwned<Owned = T>,
{
    type Target = T;

    fn or_default(&self) -> Cow<'_, Self::Target> {
        match self {
            Some(value) => Cow::Borrowed(value),
            None => Cow::Owned(T::default()),
        }
    }
}
