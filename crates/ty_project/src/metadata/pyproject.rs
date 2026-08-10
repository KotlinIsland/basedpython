use crate::metadata::options::Options;
use crate::metadata::python_version::SupportedPythonVersion;
use pep440_rs::{Version, VersionSpecifiers, release_specifiers_to_ranges};
use pep508_rs::{Requirement, VerbatimUrl};
use ruff_python_ast::PythonVersion;
use ruff_ranged_value::{RangedValue, ValueSource, ValueSourceGuard};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, Bound};
use std::ops::Deref;
use std::str::FromStr;
use strum::IntoEnumIterator;
use thiserror::Error;
use ty_combine::Combine;
use ty_module_resolver::DistributionName;
use ty_python_semantic::dependencies::{DependencyGroup, DependencyManifest, GroupName};

/// A `pyproject.toml` as specified in PEP 517.
#[derive(Deserialize, Serialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct PyProject {
    /// PEP 621-compliant project metadata.
    pub project: Option<Project>,
    /// Tool-specific metadata.
    pub tool: Option<Tool>,
    /// PEP 735 dependency groups.
    pub dependency_groups: Option<BTreeMap<String, Vec<DependencyGroupEntry>>>,
    /// A PEP 723 script's requirements.
    ///
    /// A `pyproject.toml` has no top-level `dependencies` key. This is only ever
    /// set when the file being read is a script's inline metadata block, which
    /// this type doubles as — see [`crate::metadata::script::script_metadata`].
    pub dependencies: Option<Vec<String>>,
}

impl PyProject {
    /// The options configured in `[tool.basedpython]` and `[tool.ty]`.
    ///
    /// Both sections are honored; where they set the same option, `[tool.basedpython]` wins.
    pub(crate) fn options(&self) -> Option<Options> {
        let tool = self.tool.as_ref()?;
        tool.basedpython.clone().combine(tool.ty.clone())
    }

    /// Whether the file configures ty at all, through either of its sections.
    pub(crate) fn has_options(&self) -> bool {
        self.tool
            .as_ref()
            .is_some_and(|tool| tool.basedpython.is_some() || tool.ty.is_some())
    }

    /// What this file declares the project depends on, or `None` if it declares
    /// nothing at all.
    ///
    /// A file with neither a `[project]` table nor a `[dependency-groups]` table
    /// is not a manifest: it says nothing about dependencies, which is different
    /// from saying there are none.
    pub(crate) fn dependency_manifest(&self) -> Option<DependencyManifest> {
        let uv_dev = self
            .tool
            .as_ref()
            .and_then(|tool| tool.uv.as_ref())
            .and_then(|uv| uv.dev_dependencies.as_deref());

        if self.project.is_none()
            && self.dependency_groups.is_none()
            && self.dependencies.is_none()
            && uv_dev.is_none()
        {
            return None;
        }

        let mut groups = Vec::new();

        // a script's own requirements and `[project].dependencies` are the same
        // thing — what has to be installed for the code to run — so they are one
        // group, and only one of the two is ever present
        let runtime = self
            .project
            .as_ref()
            .and_then(|project| project.dependencies.as_deref())
            .or(self.dependencies.as_deref());

        if self.project.is_some() || runtime.is_some() {
            groups.push(DependencyGroup {
                name: GroupName::Project,
                requirements: distribution_names(runtime.unwrap_or(&[])),
            });
        }

        if let Some(project) = &self.project {

            for (extra, requirements) in project.optional_dependencies.iter().flatten() {
                groups.push(DependencyGroup {
                    name: GroupName::Extra(Box::from(&**extra)),
                    requirements: distribution_names(requirements),
                });
            }
        }

        if let Some(declared) = &self.dependency_groups {
            for name in declared.keys() {
                groups.push(DependencyGroup {
                    name: GroupName::Group(Box::from(&**name)),
                    requirements: resolve_group(declared, name),
                });
            }
        }

        if let Some(dev) = uv_dev {
            // uv merges `[tool.uv].dev-dependencies` into the `dev` group rather
            // than keeping a second group beside it
            let requirements = distribution_names(dev);
            match groups
                .iter_mut()
                .find(|group| group.name == GroupName::Group(Box::from("dev")))
            {
                Some(group) => {
                    let mut merged = group.requirements.to_vec();
                    merged.extend(requirements);
                    merged.sort_unstable();
                    merged.dedup();
                    group.requirements = merged.into_boxed_slice();
                }
                None => groups.push(DependencyGroup {
                    name: GroupName::Group(Box::from("dev")),
                    requirements,
                }),
            }
        }

        let project_name = self
            .project
            .as_ref()
            .and_then(|project| project.name.as_ref())
            .map(|name| DistributionName::new(name.as_str()));

        Some(DependencyManifest::new(project_name, groups))
    }

    /// The names of the configured sections, for diagnostics. Empty if there are none.
    pub(crate) fn section_names(&self) -> String {
        let Some(tool) = self.tool.as_ref() else {
            return String::new();
        };

        match (&tool.basedpython, &tool.ty) {
            (Some(_), Some(_)) => "`tool.basedpython` and `tool.ty` sections".to_string(),
            (Some(_), None) => "`tool.basedpython` section".to_string(),
            (None, Some(_)) => "`tool.ty` section".to_string(),
            (None, None) => String::new(),
        }
    }
}

/// The distributions a list of PEP 508 requirement strings names.
///
/// An entry that does not parse is dropped rather than guessed at: a requirement
/// ty cannot read is one it knows nothing about, and inventing a name for it
/// would make an import look declared when it is not.
fn distribution_names(requirements: &[String]) -> Box<[DistributionName]> {
    let mut names: Vec<_> = requirements
        .iter()
        .filter_map(|requirement| {
            Requirement::<VerbatimUrl>::from_str(requirement)
                .inspect_err(|error| {
                    tracing::debug!("Ignoring unreadable requirement `{requirement}`: {error}");
                })
                .ok()
        })
        .map(|requirement| DistributionName::new(requirement.name.as_ref()))
        .collect();

    names.sort_unstable();
    names.dedup();
    names.into_boxed_slice()
}

/// The requirements of `group`, with every `include-group` followed.
///
/// PEP 735 forbids a cycle between groups, but a `pyproject.toml` is not
/// validated before it gets here, so the walk tracks what it has seen instead of
/// trusting that.
fn resolve_group(
    groups: &BTreeMap<String, Vec<DependencyGroupEntry>>,
    group: &str,
) -> Box<[DistributionName]> {
    let mut requirements = Vec::new();
    let mut seen = FxHashSet::from_iter([group]);
    let mut queue = vec![group];

    while let Some(name) = queue.pop() {
        let Some(entries) = groups.get(name) else {
            continue;
        };

        for entry in entries {
            match entry {
                DependencyGroupEntry::Requirement(requirement) => {
                    requirements.push(requirement.clone());
                }
                DependencyGroupEntry::Include { include_group } => {
                    if seen.insert(include_group) {
                        queue.push(include_group);
                    }
                }
            }
        }
    }

    distribution_names(&requirements)
}

#[derive(Error, Debug)]
pub enum PyProjectError {
    #[error(transparent)]
    TomlSyntax(#[from] toml::de::Error),
}

impl PyProject {
    pub(crate) fn from_toml_str(
        content: &str,
        source: ValueSource,
    ) -> Result<Self, PyProjectError> {
        let _guard = ValueSourceGuard::new(source, true);
        Self::deserialize_toml(content)
    }

    pub(crate) fn from_toml_str_without_spans(
        content: &str,
        source: ValueSource,
    ) -> Result<Self, PyProjectError> {
        let _guard = ValueSourceGuard::new(source, false);
        Self::deserialize_toml(content)
    }

    fn deserialize_toml(content: &str) -> Result<Self, PyProjectError> {
        let mut pyproject: Self = toml::from_str(content).map_err(PyProjectError::TomlSyntax)?;
        // TOML tables are unordered and the `toml` crate sorts keys
        // lexicographically. Normalize rule order so that the `all` selector
        // is applied before per-rule selectors.
        if let Some(tool) = &mut pyproject.tool {
            for options in [tool.basedpython.as_mut(), tool.ty.as_mut()]
                .into_iter()
                .flatten()
            {
                options.prioritize_all_selectors();
            }
        }
        Ok(pyproject)
    }
}

/// PEP 621 project metadata (`project`).
///
/// See <https://packaging.python.org/en/latest/specifications/pyproject-toml>.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct Project {
    /// The name of the project
    ///
    /// Note: Intentionally option to be more permissive during deserialization.
    /// `PackageMetadata::from_pyproject` reports missing names.
    pub name: Option<RangedValue<PackageName>>,
    /// The version of the project
    pub version: Option<RangedValue<Version>>,
    /// The Python versions this project is compatible with.
    pub requires_python: Option<RangedValue<VersionSpecifiers>>,
    /// The requirements installed alongside the project.
    ///
    /// Kept as written rather than as parsed requirements: one entry ty cannot
    /// make sense of must not cost it the whole file, and a `[project]` table it
    /// fails to deserialize is a project it fails to load.
    pub dependencies: Option<Vec<String>>,
    /// The requirements of each extra, installed only when the extra is asked for.
    pub optional_dependencies: Option<BTreeMap<String, Vec<String>>>,
}

impl Project {
    pub(super) fn resolve_requires_python_lower_bound(
        &self,
    ) -> Result<Option<RangedValue<SupportedPythonVersion>>, ResolveRequiresPythonError> {
        let Some(requires_python) = self.requires_python.as_ref() else {
            return Ok(None);
        };

        tracing::debug!("Resolving requires-python constraint: `{requires_python}`");

        let ranges = release_specifiers_to_ranges((**requires_python).clone());
        let Some((lower, _)) = ranges.bounding_range() else {
            return Ok(None);
        };

        let version = match lower {
            // Ex) `>=3.10.1` -> `>=3.10`
            Bound::Included(version) => version,

            // Ex) `>3.10.1` -> `>=3.10` or `>3.10` -> `>=3.10`
            // The second example looks obscure at first but it is required because
            // `3.10.1 > 3.10` is true but we only have two digits here. So including 3.10 is the
            // right move. Overall, using `>` without a patch release is most likely bogus.
            Bound::Excluded(version) => version,

            // Ex) `<3.10` or ``
            Bound::Unbounded => {
                return Err(ResolveRequiresPythonError::NoLowerBound(
                    requires_python.to_string(),
                ));
            }
        };

        // Take the major and minor version
        let mut versions = version.release().iter().take(2);

        let Some(major) = versions.next().copied() else {
            return Ok(None);
        };

        let minor = versions.next().copied().unwrap_or_default();

        tracing::debug!("Resolved requires-python constraint to: {major}.{minor}");

        let major =
            u8::try_from(major).map_err(|_| ResolveRequiresPythonError::TooLargeMajor(major))?;
        let minor =
            u8::try_from(minor).map_err(|_| ResolveRequiresPythonError::TooLargeMinor(minor))?;

        let lower_bound = PythonVersion::from((major, minor));
        let supported_version = SupportedPythonVersion::iter()
            .find(|supported_version| supported_version.to_python_version() >= lower_bound);

        let Some(supported_version) = supported_version else {
            return Err(ResolveRequiresPythonError::NoSupportedVersion(
                requires_python.to_string(),
            ));
        };

        Ok(Some(
            requires_python.clone().map_value(|_| supported_version),
        ))
    }
}

#[derive(Debug, Error)]
pub enum ResolveRequiresPythonError {
    #[error("The major version `{0}` is larger than the maximum supported value 255")]
    TooLargeMajor(u64),
    #[error("The minor version `{0}` is larger than the maximum supported value 255")]
    TooLargeMinor(u64),
    #[error(
        "value `{0}` does not contain a lower bound. Add a lower bound to indicate the minimum compatible Python version (e.g., `>=3.13`) or specify a version in `environment.python-version`."
    )]
    NoLowerBound(String),
    #[error(
        "value `{0}` does not include any Python version supported by ty. Adjust `requires-python` to include a supported Python 3 version or specify `environment.python-version` explicitly."
    )]
    NoSupportedVersion(String),
}

#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct Tool {
    pub ty: Option<Options>,
    pub basedpython: Option<Options>,
    pub uv: Option<Uv>,
}

/// The parts of `[tool.uv]` that say what the project depends on.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub struct Uv {
    /// uv's own development dependencies, which predate PEP 735 and are still
    /// widely written. uv treats them as the `dev` group, and so does this.
    pub dev_dependencies: Option<Vec<String>>,
}

/// One entry of a PEP 735 dependency group: a requirement, or another group.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum DependencyGroupEntry {
    /// A PEP 508 requirement string.
    Requirement(String),
    /// `{include-group = "..."}`, which pulls in another group's requirements.
    #[serde(rename_all = "kebab-case")]
    Include { include_group: String },
}

/// The normalized name of a package.
///
/// Converts the name to lowercase and collapses runs of `-`, `_`, and `.` down to a single `-`.
/// For example, `---`, `.`, and `__` are all converted to a single `-`.
///
/// See: <https://packaging.python.org/en/latest/specifications/name-normalization/>
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct PackageName(String);

impl PackageName {
    /// Create a validated, normalized package name.
    pub(crate) fn new(name: String) -> Result<Self, InvalidPackageNameError> {
        if name.is_empty() {
            return Err(InvalidPackageNameError::Empty);
        }

        if name.starts_with(['-', '_', '.']) {
            return Err(InvalidPackageNameError::NonAlphanumericStart(
                name.chars().next().unwrap(),
            ));
        }

        if name.ends_with(['-', '_', '.']) {
            return Err(InvalidPackageNameError::NonAlphanumericEnd(
                name.chars().last().unwrap(),
            ));
        }

        let Some(start) = name.find(|c: char| {
            !c.is_ascii() || c.is_ascii_uppercase() || matches!(c, '-' | '_' | '.')
        }) else {
            return Ok(Self(name));
        };

        let (already_normalized, maybe_normalized) = name.split_at(start);

        let mut normalized = String::with_capacity(name.len());
        normalized.push_str(already_normalized);
        let mut last = None;

        for c in maybe_normalized.chars() {
            if !c.is_ascii() {
                return Err(InvalidPackageNameError::InvalidCharacter(c));
            }

            if c.is_ascii_uppercase() {
                normalized.push(c.to_ascii_lowercase());
            } else if matches!(c, '-' | '_' | '.') {
                if matches!(last, Some('-' | '_' | '.')) {
                    // Only keep a single instance of `-`, `_` and `.`
                } else {
                    normalized.push('-');
                }
            } else {
                normalized.push(c);
            }

            last = Some(c);
        }

        Ok(Self(normalized))
    }

    /// Returns the underlying package name.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<PackageName> for String {
    fn from(value: PackageName) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for PackageName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for PackageName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Deref for PackageName {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

#[derive(Error, Debug)]
pub(crate) enum InvalidPackageNameError {
    #[error("name must start with letter or number but it starts with '{0}'")]
    NonAlphanumericStart(char),
    #[error("name must end with letter or number but it ends with '{0}'")]
    NonAlphanumericEnd(char),
    #[error(
        "valid name consists only of ASCII letters and numbers, period, underscore and hyphen but name contains '{0}'"
    )]
    InvalidCharacter(char),
    #[error("name must not be empty")]
    Empty,
}

#[cfg(test)]
mod manifest_tests {
    use super::*;
    use ruff_ranged_value::ValueSource;

    fn manifest(source: &str) -> Option<DependencyManifest> {
        PyProject::from_toml_str_without_spans(source, ValueSource::Cli)
            .expect("should parse")
            .dependency_manifest()
    }

    /// The groups a manifest declares, as `(group, [distribution])` pairs.
    fn groups(source: &str) -> Vec<(String, Vec<String>)> {
        manifest(source)
            .expect("should be a manifest")
            .groups()
            .iter()
            .map(|group| {
                (
                    group.name.as_str().to_string(),
                    group
                        .requirements
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    #[test]
    fn a_file_that_says_nothing_about_dependencies_is_not_a_manifest() {
        assert!(manifest("").is_none());
        assert!(manifest("[tool.ty]\n").is_none());
    }

    #[test]
    fn a_project_that_depends_on_nothing_is_still_a_manifest() {
        assert_eq!(
            groups("[project]\nname = \"mine\"\n"),
            [("project".to_string(), vec![])]
        );
    }

    #[test]
    fn requirement_strings_are_read_down_to_their_names() {
        assert_eq!(
            groups(
                r#"
[project]
name = "mine"
dependencies = [
    "numpy",
    "requests>=2.31",
    "httpx[http2] == 0.27.*",
    "typing-extensions; python_version < '3.12'",
    "mine @ git+https://example.invalid/mine.git",
]
"#,
            ),
            [(
                "project".to_string(),
                vec![
                    "httpx".to_string(),
                    "mine".to_string(),
                    "numpy".to_string(),
                    "requests".to_string(),
                    "typing-extensions".to_string(),
                ],
            )]
        );
    }

    #[test]
    fn an_unreadable_requirement_is_dropped_rather_than_guessed_at() {
        assert_eq!(
            groups(
                r#"
[project]
name = "mine"
dependencies = ["numpy", "!!not a requirement!!"]
"#,
            ),
            [("project".to_string(), vec!["numpy".to_string()])]
        );
    }

    #[test]
    fn extras_and_dependency_groups_are_their_own_groups() {
        assert_eq!(
            groups(
                r#"
[project]
name = "mine"
dependencies = ["numpy"]

[project.optional-dependencies]
cli = ["click"]

[dependency-groups]
dev = ["pytest"]
"#,
            ),
            [
                ("project".to_string(), vec!["numpy".to_string()]),
                ("cli".to_string(), vec!["click".to_string()]),
                ("dev".to_string(), vec!["pytest".to_string()]),
            ]
        );
    }

    #[test]
    fn include_group_is_followed() {
        assert_eq!(
            groups(
                r#"
[dependency-groups]
test = ["pytest"]
lint = ["ruff"]
dev = [{ include-group = "test" }, { include-group = "lint" }, "ipython"]
"#,
            ),
            [
                (
                    "dev".to_string(),
                    vec![
                        "ipython".to_string(),
                        "pytest".to_string(),
                        "ruff".to_string()
                    ]
                ),
                ("lint".to_string(), vec!["ruff".to_string()]),
                ("test".to_string(), vec!["pytest".to_string()]),
            ]
        );
    }

    #[test]
    fn a_cycle_between_groups_terminates() {
        assert_eq!(
            groups(
                r#"
[dependency-groups]
a = ["one", { include-group = "b" }]
b = ["two", { include-group = "a" }]
"#,
            ),
            [
                ("a".to_string(), vec!["one".to_string(), "two".to_string()]),
                ("b".to_string(), vec!["one".to_string(), "two".to_string()]),
            ]
        );
    }

    #[test]
    fn uv_dev_dependencies_are_the_dev_group() {
        assert_eq!(
            groups("[tool.uv]\ndev-dependencies = [\"pytest\"]\n"),
            [("dev".to_string(), vec!["pytest".to_string()])]
        );
    }

    #[test]
    fn uv_dev_dependencies_merge_into_a_declared_dev_group() {
        assert_eq!(
            groups(
                r#"
[dependency-groups]
dev = ["pytest"]

[tool.uv]
dev-dependencies = ["ruff"]
"#,
            ),
            [(
                "dev".to_string(),
                vec!["pytest".to_string(), "ruff".to_string()]
            )]
        );
    }

    #[test]
    fn a_script_declares_its_requirements_as_the_project_group() {
        // PEP 723 writes `dependencies` at the top level, not under `[project]`
        assert_eq!(
            groups("dependencies = [\"numpy\"]\nrequires-python = \">=3.11\"\n"),
            [("project".to_string(), vec!["numpy".to_string()])]
        );
    }

    #[test]
    fn the_project_name_identifies_what_it_ships() {
        let manifest = manifest("[project]\nname = \"My.Lib\"\n").unwrap();
        assert_eq!(
            manifest.project_name().map(ToString::to_string).as_deref(),
            Some("my-lib")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::PackageName;

    #[test]
    fn normalize() {
        let inputs = [
            "friendly-bard",
            "Friendly-Bard",
            "FRIENDLY-BARD",
            "friendly.bard",
            "friendly_bard",
            "friendly--bard",
            "friendly-.bard",
            "FrIeNdLy-._.-bArD",
        ];

        for input in inputs {
            assert_eq!(
                PackageName::new(input.to_string()).unwrap(),
                PackageName::new("friendly-bard".to_string()).unwrap(),
            );
        }
    }
}
