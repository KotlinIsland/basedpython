//! TOML-deserializable ty configuration, similar to `ty.toml`, to be able to
//! control some configuration options from Markdown files. For now, this supports the
//! following limited structure:
//!
//! ```toml
//! log = true # or log = "ty=WARN"
//!
//! [rules]
//! possibly-unresolved-reference = "warn"
//!
//! [environment]
//! python-version = "3.10"
//!
//! [project]
//! dependencies = ["pydantic==2.12.2"]
//! ```

use std::collections::BTreeMap;

use compact_str::CompactString;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_python_ast::PythonVersion;
use ruff_python_ast::script::ScriptTag;
use serde::{Deserialize, Serialize};
use ty_module_resolver::{DistributionName, ModuleName};
use ty_python_core::platform::PythonPlatform;
use ty_python_semantic::TypeCheckingPreset;
use ty_python_semantic::dependencies::{DependencyGroup, DependencyManifest, GroupName};
use ty_python_semantic::dependency::{
    DependencyDistribution, DependencyMetadata, DependencyProject, DependencyProjectKind,
};
use ty_python_semantic::lint::Level;

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct MarkdownTestConfig {
    pub(crate) type_checking_preset: Option<TypeCheckingPreset>,

    pub(crate) environment: Option<Environment>,

    pub(crate) log: Option<Log>,

    pub(crate) rules: Option<Rules>,

    pub(crate) analysis: Option<Analysis>,

    /// The experimental features the test opts in to, as `[tool.ty.experimental]`
    /// does for a project.
    pub(crate) experimental: Option<Experimental>,

    /// The [`ruff_db::system::System`] to use for tests.
    ///
    /// Defaults to the case-sensitive [`ruff_db::system::InMemorySystem`].
    pub(crate) system: Option<SystemKind>,

    /// Project configuration for installing external dependencies.
    pub(crate) project: Option<Project>,

    /// What the project under test declares it depends on.
    ///
    /// This stands in for a `pyproject.toml`, which mdtest has no way to hand to
    /// the type checker. Entries are bare distribution names: reading PEP 508
    /// requirement strings is `ty_project`'s job and is tested there.
    pub(crate) dependencies: Option<Dependencies>,

    /// Dependency declarations and module ownership without installing packages.
    pub(crate) dependency_metadata: Option<DependencyMetadataFixture>,

    /// Simulate the use passing `-v` on the command line,
    /// which can be used to show more information in test diagnostics.
    pub(crate) verbose: Option<bool>,
}

impl MarkdownTestConfig {
    pub(crate) fn type_checking_preset(&self) -> TypeCheckingPreset {
        self.type_checking_preset.unwrap_or_default()
    }

    pub(crate) fn python_version(&self) -> Option<PythonVersion> {
        self.environment.as_ref()?.python_version
    }

    pub(crate) fn python_platform(&self) -> Option<PythonPlatform> {
        self.environment.as_ref()?.python_platform.clone()
    }

    pub(crate) fn typeshed(&self) -> Option<&SystemPath> {
        self.environment.as_ref()?.typeshed.as_deref()
    }

    pub(crate) fn extra_paths(&self) -> Option<&[SystemPathBuf]> {
        self.environment.as_ref()?.extra_paths.as_deref()
    }

    pub(crate) fn python(&self) -> Option<&SystemPath> {
        self.environment.as_ref()?.python.as_deref()
    }

    pub(crate) fn dependencies(&self) -> Option<&[String]> {
        self.project.as_ref()?.dependencies.as_deref()
    }

    pub(crate) fn verbose(&self) -> bool {
        self.verbose.unwrap_or_default()
    }

    pub(crate) fn dependency_manifest(&self) -> Option<DependencyManifest> {
        let declared = self.dependencies.as_ref()?;

        let names = |requirements: &[String]| {
            requirements
                .iter()
                .map(|name| DistributionName::new(name))
                .collect()
        };

        let mut groups = vec![DependencyGroup {
            name: GroupName::Project,
            requirements: names(declared.project.as_deref().unwrap_or(&[])),
        }];

        for (extra, requirements) in &declared.extras {
            groups.push(DependencyGroup {
                name: GroupName::Extra(Box::from(&**extra)),
                requirements: names(requirements),
            });
        }

        for (group, requirements) in &declared.groups {
            groups.push(DependencyGroup {
                name: GroupName::Group(Box::from(&**group)),
                requirements: names(requirements),
            });
        }

        Some(DependencyManifest::new(
            declared.name.as_deref().map(DistributionName::new),
            groups,
        ))
    }
}

/// What the project under test declares it depends on.
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct Dependencies {
    /// `[project].name`, which is what decides the modules the project ships.
    pub(crate) name: Option<String>,

    /// `[project].dependencies`.
    pub(crate) project: Option<Vec<String>>,

    /// `[project.optional-dependencies]`.
    #[serde(default)]
    pub(crate) extras: BTreeMap<String, Vec<String>>,

    /// `[dependency-groups]`.
    #[serde(default)]
    pub(crate) groups: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct ScriptOptions {
    pub(crate) type_checking_preset: Option<TypeCheckingPreset>,
    pub(crate) rules: Option<Rules>,
    pub(crate) analysis: Option<Analysis>,
    pub(crate) dependency_metadata: Option<DependencyMetadataFixture>,
}

impl ScriptOptions {
    pub(crate) fn from_source(source: &str) -> Option<Self> {
        let tag = ScriptTag::parse(source.as_bytes())?;
        let metadata: ScriptMetadata = toml::from_str(tag.metadata()).ok()?;

        let mut options = metadata.tool.and_then(|tool| tool.ty).unwrap_or_default();
        if let Some(fixture) = &mut options.dependency_metadata {
            for project in &mut fixture.metadata.projects {
                project.kind = DependencyProjectKind::Script;
            }
        }

        Some(options)
    }
}

#[derive(Deserialize)]
struct ScriptMetadata {
    tool: Option<ScriptTool>,
}

#[derive(Deserialize)]
struct ScriptTool {
    ty: Option<ScriptOptions>,
}

pub(crate) type Rules = BTreeMap<String, Level>;

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct Environment {
    /// Target Python version to assume when resolving types.
    ///
    /// The Python version affects allowed syntax, type definitions of the standard library, and
    /// type definitions of first- and third-party modules that are conditional on the Python version.
    ///
    /// By default, the Python version is inferred as the lower bound of the project's
    /// `requires-python` field from the `pyproject.toml`, if available. Otherwise, the latest
    /// stable version supported by ty is used (see `ty check --help` output).
    ///
    /// ty will not infer the Python version from the Python environment at this time.
    python_version: Option<PythonVersion>,

    /// Target platform to assume when resolving types.
    python_platform: Option<PythonPlatform>,

    /// Path to a custom typeshed directory.
    typeshed: Option<SystemPathBuf>,

    /// Additional search paths to consider when resolving modules.
    extra_paths: Option<Vec<SystemPathBuf>>,

    /// Path to the Python environment.
    ///
    /// ty uses the Python environment to resolve type information and third-party dependencies.
    ///
    /// If a path to a Python interpreter is provided, e.g., `.venv/bin/python3`, ty will attempt to
    /// find an environment two directories up from the interpreter's path, e.g., `.venv`. At this
    /// time, ty does not invoke the interpreter to determine the location of the environment. This
    /// means that ty will not resolve dynamic executables such as a shim.
    ///
    /// ty will search in the resolved environment's `site-packages` directories for type
    /// information and third-party imports.
    #[serde(skip_serializing_if = "Option::is_none")]
    python: Option<SystemPathBuf>,
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct Experimental {
    /// see [`ty_python_semantic::ExperimentalSettings::module_api`]
    pub(crate) module_api: Option<bool>,

    /// see [`ty_python_semantic::ExperimentalSettings::build_stamps`]
    pub(crate) build_stamps: Option<bool>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct Analysis {
    /// Whether narrowing with generic classes uses the top materialization.
    pub(crate) strict_generic_narrowing: Option<bool>,

    /// Whether equality-based checks should preserve possible subclass behavior.
    #[serde(alias = "strict-literal-narrowing")]
    pub(crate) strict_equality_semantics: Option<bool>,

    /// Whether ty should support `type: ignore` comments.
    pub(crate) respect_type_ignore_comments: Option<bool>,

    pub(crate) allowed_unresolved_imports: Option<Vec<String>>,

    pub(crate) replace_imports_with_any: Option<Vec<String>>,

    /// see [`ty_python_semantic::AnalysisSettings::block_scoped_declarations`]
    pub(crate) block_scoped_declarations: Option<bool>,

    /// see [`ty_python_semantic::AnalysisSettings::strict_float`]
    pub(crate) strict_float: Option<bool>,

    pub(crate) disable_fluid_specializations: Option<bool>,

    pub(crate) sound_types: Option<bool>,

    pub(crate) infer_unannotated_signatures: Option<bool>,

    pub(crate) bivariant_private_attributes: Option<bool>,

    pub(crate) precise_unsolved_typevars: Option<bool>,

    pub(crate) overlapping_condition_exempt_types: Option<Vec<String>>,

    pub(crate) overlapping_condition_assume_truthy_instances: Option<bool>,

    pub(crate) implicit_object_repr_exempt_types: Option<Vec<String>>,

    pub(crate) implicit_object_repr_report_types: Option<Vec<String>>,

    pub(crate) dependency_groups: Option<Vec<String>>,

    pub(crate) shipped_modules: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub(crate) enum Log {
    /// Enable logging with tracing when `true`.
    Bool(bool),
    /// Enable logging and only show filters that match the given [env-filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
    Filter(String),
}

/// The system to use for tests.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SystemKind {
    /// Use an in-memory system with a case-sensitive file system.
    ///
    /// This is recommended for all tests because it's fast.
    #[default]
    InMemory,

    /// Use the os system.
    ///
    /// This system should only be used when testing system or OS specific behavior.
    Os,
}

/// Project configuration for tests that need external dependencies.
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct Project {
    /// List of Python package dependencies in `pyproject.toml` format.
    ///
    /// These will be installed using `uv sync` into a temporary virtual environment.
    /// The site-packages directory will then be copied into the test's filesystem.
    ///
    /// Example: `dependencies = ["pydantic==2.12.2"]`
    dependencies: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(try_from = "DependencyMetadataOptions")]
pub(crate) struct DependencyMetadataFixture {
    pub(crate) metadata: DependencyMetadata,
}

impl TryFrom<DependencyMetadataOptions> for DependencyMetadataFixture {
    type Error = String;

    fn try_from(options: DependencyMetadataOptions) -> Result<Self, Self::Error> {
        let module_owners = options
            .module_owners
            .into_iter()
            .map(|(module, owners)| {
                let module_name = ModuleName::new(&module)
                    .ok_or_else(|| format!("Invalid dependency module name `{module}`"))?;
                Ok((module_name, owners.into_boxed_slice()))
            })
            .collect::<Result<_, Self::Error>>()?;

        Ok(Self {
            metadata: DependencyMetadata {
                projects: options
                    .projects
                    .into_iter()
                    .map(|project| DependencyProject {
                        path: project.path,
                        kind: DependencyProjectKind::Project,
                        distribution: project.distribution,
                        dependencies: project.dependencies.into_iter().collect(),
                        group_dependencies: project.group_dependencies.into_iter().collect(),
                    })
                    .collect(),
                distributions: options
                    .distributions
                    .into_iter()
                    .map(|(id, distribution)| {
                        (
                            id,
                            DependencyDistribution {
                                name: distribution.name,
                                editable_path: distribution.editable_path,
                            },
                        )
                    })
                    .collect(),
                module_owners,
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct DependencyMetadataOptions {
    #[serde(default)]
    projects: Vec<DependencyProjectOptions>,
    #[serde(default)]
    distributions: BTreeMap<CompactString, DependencyDistributionOptions>,
    #[serde(default)]
    module_owners: BTreeMap<CompactString, Vec<CompactString>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct DependencyProjectOptions {
    path: SystemPathBuf,
    distribution: Option<CompactString>,
    #[serde(default)]
    dependencies: Vec<CompactString>,
    #[serde(default)]
    group_dependencies: Vec<CompactString>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct DependencyDistributionOptions {
    name: CompactString,
    editable_path: Option<SystemPathBuf>,
}
