//! Which installed distribution owns which top-level name.
//!
//! A module tells you nothing about the distribution it came from: `jwt` is
//! installed by `PyJWT`, `google` by any of a dozen distributions. The link is
//! only recorded in the install metadata, so it has to be read from there.
//!
//! The source is `RECORD`, the manifest of installed files every wheel install
//! leaves behind in its `.dist-info` directory. `top_level.txt` records the same
//! thing more directly but is a setuptools artifact rather than a specified one:
//! backends such as hatchling, flit, pdm and maturin do not write it, so a
//! reader that trusts it misses a large fraction of a real environment.

use ruff_db::files::{directory_listing, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::SystemPath;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

use crate::db::Db;
use crate::environment::ResolverEnvironment;
use crate::module::Module;

/// The name of a distribution, as `pyproject.toml` and `site-packages` spell it.
///
/// Two spellings name the same distribution when their [PEP 503] normalized forms
/// are equal — `PyJWT`, `pyjwt` and `py_jwt` are one distribution — so equality and
/// hashing go through the normalized form and the original spelling is kept only
/// to display.
///
/// [PEP 503]: https://peps.python.org/pep-0503/#normalized-names
#[derive(Clone, Debug, Eq, get_size2::GetSize)]
pub struct DistributionName {
    /// The spelling this name was read with, for display.
    display: Box<str>,
    /// The normalized form, which is this name's identity.
    normalized: Box<str>,
}

impl DistributionName {
    pub fn new(name: &str) -> Self {
        Self {
            display: Box::from(name),
            normalized: normalize(name).into_boxed_str(),
        }
    }

    /// The spelling this name was read with.
    pub fn as_str(&self) -> &str {
        &self.display
    }

    /// The [PEP 503] normalized form, which is what two names are compared by.
    ///
    /// [PEP 503]: https://peps.python.org/pep-0503/#normalized-names
    pub fn normalized(&self) -> &str {
        &self.normalized
    }
}

/// Lowercase, with every run of `-`, `_` and `.` collapsed to a single `-`.
///
/// This is [PEP 503] name normalization, the rule that decides when two spellings
/// name one distribution.
///
/// [PEP 503]: https://peps.python.org/pep-0503/#normalized-names
fn normalize(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut last_was_separator = false;

    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !last_was_separator {
                normalized.push('-');
            }
            last_was_separator = true;
        } else {
            normalized.extend(c.to_lowercase());
            last_was_separator = false;
        }
    }

    normalized
}

impl PartialEq for DistributionName {
    fn eq(&self, other: &Self) -> bool {
        self.normalized == other.normalized
    }
}

impl std::hash::Hash for DistributionName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.normalized.hash(state);
    }
}

impl PartialOrd for DistributionName {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DistributionName {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.normalized.cmp(&other.normalized)
    }
}

impl std::fmt::Display for DistributionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.display.fmt(f)
    }
}

/// Every top-level name installed into the environment's `site-packages`
/// directories, and the distributions that installed it.
#[derive(Debug, Default, Eq, PartialEq, get_size2::GetSize)]
pub struct DistributionIndex {
    /// Top-level name (`numpy`, `jwt`, `requests-stubs`) to its owners.
    ///
    /// A namespace package such as `google` has several; the common case has one.
    owners: FxHashMap<Box<str>, Box<[DistributionName]>>,
}

impl DistributionIndex {
    /// The distributions that install `top_level`, which is a directory name or
    /// a module file's stem directly inside `site-packages`.
    fn owners_of_top_level(&self, top_level: &str) -> &[DistributionName] {
        self.owners.get(top_level).map_or(&[], |owners| owners)
    }

    /// The distributions that could have installed `module`.
    ///
    /// Empty when `module` is not installed third-party code — first-party code,
    /// the standard library, an editable install and a namespace package all
    /// answer with nothing.
    ///
    /// The lookup goes through the file's path rather than its module name so
    /// that a stub-only distribution answers for the module it describes:
    /// `types-requests` installs `requests-stubs`, and it is `requests-stubs`
    /// that the index knows about.
    pub fn owners_of<'db>(&self, db: &'db dyn Db, module: Module<'db>) -> &[DistributionName] {
        let Some(top_level) = top_level_of(db, module) else {
            return &[];
        };
        self.owners_of_top_level(top_level)
    }

    /// Whether the index knows of any installed distribution at all.
    ///
    /// An environment with no readable install metadata cannot answer any
    /// question this index exists to answer, and a caller has to tell that apart
    /// from "the answer is no".
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

/// The name directly inside `site-packages` that `module` was resolved from.
///
/// `None` unless the module came from a `site-packages` search path: nothing else
/// is installed by a distribution.
fn top_level_of<'db>(db: &'db dyn Db, module: Module<'db>) -> Option<&'db str> {
    let search_path = module.search_path(db)?;
    if !search_path.is_site_packages() {
        return None;
    }

    let file = module.file(db)?;
    let path = file.path(db).as_system_path()?;
    let relative = search_path.as_system_path()?;
    let relative = path.strip_prefix(relative).ok()?;

    let first = relative.components().next()?.as_str();
    Some(module_stem(first))
}

/// The distributions installed into every `site-packages` directory ty resolved.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn distribution_index<'db>(
    db: &'db dyn Db,
    resolver_environment: ResolverEnvironment<'db>,
) -> DistributionIndex {
    let _span = tracing::debug_span!("distribution_index").entered();

    let mut owners: FxHashMap<Box<str>, Vec<DistributionName>> = FxHashMap::default();

    for site_packages in resolver_environment.search_paths(db).site_packages_paths() {
        index_site_packages(db, site_packages, &mut owners);
    }

    DistributionIndex {
        owners: owners
            .into_iter()
            .map(|(top_level, mut distributions)| {
                distributions.sort_unstable();
                distributions.dedup();
                (top_level, distributions.into_boxed_slice())
            })
            .collect(),
    }
}

/// What each installed distribution says it requires.
///
/// This answers "why is this here?" about something the project never asked for:
/// the requirement edges lead back from it to the dependency that did ask.
#[derive(Debug, Default, Eq, PartialEq, get_size2::GetSize)]
pub struct RequirementIndex {
    /// A distribution to the installed distributions it requires, sorted.
    ///
    /// Only edges to something installed are kept. An edge to a distribution
    /// that is not in the environment leads nowhere, and this is only ever
    /// walked towards something that is.
    requirements: FxHashMap<DistributionName, Box<[DistributionName]>>,
}

impl RequirementIndex {
    /// The installed distributions `distribution` requires.
    fn requirements_of(&self, distribution: &DistributionName) -> &[DistributionName] {
        self.requirements
            .get(distribution)
            .map_or(&[], |requirements| requirements)
    }

    /// The shortest way `target` is reached from one of `roots`.
    ///
    /// The answer is the path up to and including whatever requires `target`
    /// directly, so its first element is the root that explains the install and
    /// its last is the distribution that names `target` outright. `None` when no
    /// root reaches it, which is what a missing or unreadable `METADATA` looks
    /// like from here.
    ///
    /// Ties are broken by name so that the same environment always gives the
    /// same answer.
    ///
    /// Only requirements `follow` accepts are walked, which is what lets the same
    /// search answer a narrower question than "what pulled this in" — the caller
    /// asking which requirements are *exported* walks the same graph with fewer
    /// edges.
    pub fn path_from<'a>(
        &'a self,
        roots: &[&DistributionName],
        target: &DistributionName,
        follow: impl Fn(&DistributionName, &DistributionName) -> bool,
    ) -> Option<Vec<&'a DistributionName>> {
        let mut roots = roots.to_vec();
        roots.sort_unstable();
        roots.dedup();

        // the distribution each one was first reached from, which is both the
        // visited set and what the path is rebuilt from
        let mut reached_from: FxHashMap<&DistributionName, Option<&DistributionName>> =
            FxHashMap::default();
        let mut frontier: VecDeque<&DistributionName> = VecDeque::new();

        for root in roots {
            let Some((installed, _)) = self.requirements.get_key_value(root) else {
                continue;
            };
            if reached_from.insert(installed, None).is_none() {
                frontier.push_back(installed);
            }
        }

        while let Some(distribution) = frontier.pop_front() {
            for required in self.requirements_of(distribution) {
                if !follow(distribution, required) {
                    continue;
                }

                if required == target {
                    let mut path = vec![distribution];
                    let mut step = distribution;
                    while let Some(Some(previous)) = reached_from.get(step) {
                        path.push(previous);
                        step = previous;
                    }
                    path.reverse();
                    return Some(path);
                }

                if reached_from.insert(required, Some(distribution)).is_none() {
                    frontier.push_back(required);
                }
            }
        }

        None
    }
}

/// What every installed distribution requires, read from its `METADATA`.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn requirement_index<'db>(
    db: &'db dyn Db,
    resolver_environment: ResolverEnvironment<'db>,
) -> RequirementIndex {
    let _span = tracing::debug_span!("requirement_index").entered();

    let mut requirements: FxHashMap<DistributionName, Vec<DistributionName>> = FxHashMap::default();

    for site_packages in resolver_environment.search_paths(db).site_packages_paths() {
        index_requirements(db, site_packages, &mut requirements);
    }

    let installed: FxHashSet<&DistributionName> = requirements.keys().collect();
    let kept: Vec<_> = requirements
        .iter()
        .map(|(distribution, required)| {
            let mut required: Vec<_> = required
                .iter()
                .filter(|name| installed.contains(name))
                .cloned()
                .collect();
            required.sort_unstable();
            required.dedup();
            (distribution.clone(), required.into_boxed_slice())
        })
        .collect();

    RequirementIndex {
        requirements: kept.into_iter().collect(),
    }
}

fn index_requirements(
    db: &dyn Db,
    site_packages: &SystemPath,
    requirements: &mut FxHashMap<DistributionName, Vec<DistributionName>>,
) {
    let Ok(listing) = directory_listing(db, site_packages) else {
        tracing::debug!("Failed to list `{site_packages}` when indexing requirements");
        return;
    };

    for (name, file_type) in listing.iter() {
        if !file_type.is_directory() {
            continue;
        }
        let Some(distribution) = distribution_name_from_dist_info(name) else {
            continue;
        };

        // a distribution with no readable `METADATA` is still installed, and so
        // still something another distribution can require: it is entered with
        // no requirements of its own rather than left out
        let entry = requirements.entry(distribution).or_default();

        let metadata = site_packages.join(name).join("METADATA");
        let Ok(metadata) = system_path_to_file(db, &metadata) else {
            tracing::debug!("No `METADATA` for `{name}` in `{site_packages}`");
            continue;
        };

        entry.extend(required_distributions(&source_text(db, metadata)));
    }
}

/// The distributions a `METADATA` file's `Requires-Dist` fields name.
///
/// Only what is installed alongside the distribution itself counts, so a
/// requirement an extra brings in — `Requires-Dist: pytest; extra == "dev"` — is
/// left out: whether that extra was asked for is not something the installed
/// metadata records, and claiming it explains an install would be a guess.
///
/// The fields stop at the first empty line, after which `METADATA` carries the
/// project's own description and any line in it could look like a field. Empty
/// means empty: a field's value can run over several lines, and every line after
/// the first is indented — including the ones separating the paragraphs of a
/// license, which is what a long `License` field is made of.
fn required_distributions(metadata: &str) -> impl Iterator<Item = DistributionName> {
    metadata
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let requirement = line.strip_prefix("Requires-Dist:")?.trim();
            let (requirement, marker) = match requirement.split_once(';') {
                Some((requirement, marker)) => (requirement, marker),
                None => (requirement, ""),
            };

            if marker.contains("extra") {
                return None;
            }

            let name = requirement
                .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
                .next()?;

            (!name.is_empty()).then(|| DistributionName::new(name))
        })
}

fn index_site_packages(
    db: &dyn Db,
    site_packages: &SystemPath,
    owners: &mut FxHashMap<Box<str>, Vec<DistributionName>>,
) {
    let Ok(listing) = directory_listing(db, site_packages) else {
        tracing::debug!("Failed to list `{site_packages}` when indexing distributions");
        return;
    };

    for (name, file_type) in listing.iter() {
        if !file_type.is_directory() {
            continue;
        }
        let Some(distribution) = distribution_name_from_dist_info(name) else {
            continue;
        };

        let record = site_packages.join(name).join("RECORD");
        let Ok(record) = system_path_to_file(db, &record) else {
            // a distribution installed by something other than a wheel install
            // has no `RECORD`; there is nothing to read and nothing to report
            tracing::debug!("No `RECORD` for `{name}` in `{site_packages}`");
            continue;
        };

        for top_level in top_level_names(&source_text(db, record)) {
            owners
                .entry(Box::from(top_level))
                .or_default()
                .push(distribution.clone());
        }
    }
}

/// The distribution a `<name>-<version>.dist-info` directory belongs to.
///
/// The escaping a wheel applies to the name (`absl-py` becomes `absl_py`) is not
/// undone here: [`DistributionName`] compares normalized, and both spellings
/// normalize alike.
pub(crate) fn distribution_name_from_dist_info(directory: &str) -> Option<DistributionName> {
    let stem = directory.strip_suffix(".dist-info")?;
    // the version never contains `-`: PEP 440 spells a local version with `+`,
    // and the escaped name never contains one either
    let (name, _version) = stem.rsplit_once('-')?;
    (!name.is_empty()).then(|| DistributionName::new(name))
}

/// The top-level names a `RECORD` says its distribution installed.
///
/// Every line is `<path>,<hash>,<size>`, with the path relative to the directory
/// the distribution was installed into. What is wanted is the first path segment
/// of each, minus the entries that are not importable: the install metadata
/// itself, and anything installed outside `site-packages` (a console script,
/// which `RECORD` reaches through `..`).
fn top_level_names(record: &str) -> impl Iterator<Item = &str> {
    record.lines().filter_map(|line| {
        let path = record_path(line)?;
        let first = path.split('/').next()?;

        if first.is_empty()
            || first == ".."
            || first == "."
            || METADATA_DIRECTORIES
                .iter()
                .any(|suffix| first.ends_with(suffix))
        {
            return None;
        }

        // a segment with a path after it is a directory, and so a package
        if first.len() < path.len() {
            return Some(first);
        }

        // otherwise it is a file installed directly into `site-packages`, which
        // is importable only if it is a module. the suffix is matched rather than
        // the whole extension because an extension module carries its ABI tag,
        // as in `_speedups.cpython-313-darwin.so`
        let stem = module_stem(first);
        let suffix = first.get(stem.len()..)?;
        MODULE_SUFFIXES
            .iter()
            .any(|extension| suffix.ends_with(extension))
            .then_some(stem)
    })
}

/// What a file directly inside `site-packages` has to end with to be importable.
const MODULE_SUFFIXES: &[&str] = &[".py", ".pyi", ".by", ".byi", ".so", ".pyd"];

/// What an install leaves beside the code it installed, which is not importable.
const METADATA_DIRECTORIES: &[&str] = &[".dist-info", ".data", ".egg-info"];

/// The first field of a `RECORD` line, which is a CSV record.
pub(crate) fn record_path(line: &str) -> Option<&str> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return None;
    }

    // a path containing a comma is quoted, and a literal quote inside it is
    // doubled. only the field's extent matters here, so the doubling is left as
    // it is: no importable name contains a quote
    if let Some(quoted) = line.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some(&quoted[..end]);
    }

    Some(line.split(',').next().unwrap_or(line))
}

/// The importable name of a file, which stops at the first `.`.
///
/// `foo.py` and `foo.cpython-313-darwin.so` are both the module `foo`.
fn module_stem(file_name: &str) -> &str {
    file_name.split('.').next().unwrap_or(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization() {
        for spelling in ["PyJWT", "pyjwt", "PYJWT"] {
            assert_eq!(
                DistributionName::new(spelling),
                DistributionName::new("pyjwt"),
                "`{spelling}` should name the same distribution",
            );
        }

        // a run of any of `-`, `_` and `.` is one separator, but a name without
        // a separator is a different name: `PyJWT` is not `py-jwt`
        for spelling in ["absl_py", "absl.py", "absl--py", "Absl_Py"] {
            assert_eq!(
                DistributionName::new(spelling),
                DistributionName::new("absl-py"),
                "`{spelling}` should name the same distribution",
            );
        }

        assert_ne!(
            DistributionName::new("pyjwt"),
            DistributionName::new("py-jwt")
        );
        assert_ne!(
            DistributionName::new("types-requests"),
            DistributionName::new("requests")
        );
    }

    #[test]
    fn display_keeps_the_original_spelling() {
        assert_eq!(DistributionName::new("PyJWT").as_str(), "PyJWT");
        assert_eq!(DistributionName::new("PyJWT").normalized(), "pyjwt");
    }

    #[test]
    fn distribution_name_from_directory() {
        let name = |directory| {
            distribution_name_from_dist_info(directory).map(|name| name.as_str().to_string())
        };

        assert_eq!(name("PyJWT-2.10.1.dist-info").as_deref(), Some("PyJWT"));
        assert_eq!(name("absl_py-2.3.1.dist-info").as_deref(), Some("absl_py"));
        assert_eq!(
            name("dm_env_rpc-1.1.6.dist-info").as_deref(),
            Some("dm_env_rpc")
        );
        assert_eq!(
            name("dprint_py-0.50.2.0.dist-info").as_deref(),
            Some("dprint_py")
        );
        assert_eq!(name("numpy").as_deref(), None);
        assert_eq!(name("numpy-2.2.6.data").as_deref(), None);
        assert_eq!(name("-1.0.dist-info").as_deref(), None);
    }

    fn top_levels(record: &str) -> Vec<&str> {
        let mut names: Vec<_> = top_level_names(record).collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    #[test]
    fn package_record() {
        let record = "\
numpy/__init__.py,sha256=abc,100
numpy/core/_methods.py,sha256=def,200
numpy-2.2.6.dist-info/METADATA,sha256=ghi,300
numpy-2.2.6.dist-info/RECORD,,
../../../bin/f2py,sha256=jkl,400
";
        assert_eq!(top_levels(record), ["numpy"]);
    }

    #[test]
    fn single_module_record() {
        let record = "\
typing_extensions.py,sha256=abc,100
typing_extensions-4.15.0.dist-info/RECORD,,
";
        assert_eq!(top_levels(record), ["typing_extensions"]);
    }

    #[test]
    fn extension_module_record() {
        let record = "\
_speedups.cpython-313-darwin.so,sha256=abc,100
_speedups.cp313-win_amd64.pyd,sha256=def,200
";
        assert_eq!(top_levels(record), ["_speedups"]);
    }

    #[test]
    fn non_importable_entries_are_skipped() {
        let record = "\
LICENSE,sha256=abc,100
distutils-precedence.pth,sha256=def,200
foo-1.0.data/scripts/foo,sha256=ghi,300
./relative,sha256=jkl,400
,,
";
        assert!(top_levels(record).is_empty());
    }

    #[test]
    fn quoted_record_path() {
        let record = "\"weird,name/__init__.py\",sha256=abc,100\n";
        assert_eq!(top_levels(record), ["weird,name"]);
    }

    #[test]
    fn stub_package_record() {
        let record = "\
requests-stubs/__init__.pyi,sha256=abc,100
types_requests-2.32.0.dist-info/RECORD,,
";
        assert_eq!(top_levels(record), ["requests-stubs"]);
    }

    mod index {
        use ruff_db::system::{DbWithWritableSystem as _, SystemPathBuf};

        use super::super::*;
        use crate::db::tests::TestDb;
        use crate::list::all_modules;
        use crate::module_name::ModuleName;
        use crate::testing::{FileSpec, TestCase, TestCaseBuilder};

        /// Builds a test case whose `site-packages` has the given files, and
        /// whose distributions are named by `dist_info`, a
        /// `(dist-info directory, RECORD contents)` list.
        fn case(files: &[FileSpec], dist_info: &[FileSpec]) -> (TestDb, SystemPathBuf) {
            let TestCase {
                mut db,
                site_packages,
                ..
            } = TestCaseBuilder::new()
                .with_site_packages_files(files)
                .build();

            for (directory, record) in dist_info {
                db.write_file(site_packages.join(directory).join("RECORD"), *record)
                    .unwrap();
            }

            (db, site_packages)
        }

        fn owners(db: &TestDb, module: &str) -> Vec<String> {
            let module_name = ModuleName::new(module).unwrap();
            let module = all_modules(db, db.resolver_environment())
                .into_iter()
                .find(|listed| listed.name(db) == &module_name)
                .unwrap_or_else(|| panic!("`{module}` should resolve"));
            distribution_index(db, db.resolver_environment())
                .owners_of(db, module)
                .iter()
                .map(ToString::to_string)
                .collect()
        }

        #[test]
        fn maps_a_module_to_the_distribution_that_installed_it() {
            let (db, _) = case(
                &[("jwt/__init__.py", "")],
                &[("PyJWT-2.10.1.dist-info", "jwt/__init__.py,sha256=a,1\n")],
            );

            assert_eq!(owners(&db, "jwt"), ["PyJWT"]);
        }

        #[test]
        fn a_submodule_answers_with_its_root_distribution() {
            let (db, _) = case(
                &[("jwt/__init__.py", ""), ("jwt/algorithms.py", "")],
                &[(
                    "PyJWT-2.10.1.dist-info",
                    "jwt/__init__.py,sha256=a,1\njwt/algorithms.py,sha256=b,2\n",
                )],
            );

            assert_eq!(owners(&db, "jwt.algorithms"), ["PyJWT"]);
        }

        #[test]
        fn a_namespace_package_has_every_owner() {
            let (db, _) = case(
                &[
                    ("google/protobuf/__init__.py", ""),
                    ("google/cloud/__init__.py", ""),
                ],
                &[
                    (
                        "protobuf-6.33.1.dist-info",
                        "google/protobuf/__init__.py,sha256=a,1\n",
                    ),
                    (
                        "google_cloud_core-2.4.1.dist-info",
                        "google/cloud/__init__.py,sha256=b,2\n",
                    ),
                ],
            );

            let index = distribution_index(&db, db.resolver_environment());
            let mut owners: Vec<_> = index
                .owners_of_top_level("google")
                .iter()
                .map(ToString::to_string)
                .collect();
            owners.sort();
            assert_eq!(owners, ["google_cloud_core", "protobuf"]);
        }

        #[test]
        fn first_party_code_has_no_owner() {
            let TestCase { db, .. } = TestCaseBuilder::new()
                .with_src_files(&[("mine.py", "")])
                .build();

            assert!(owners(&db, "mine").is_empty());
        }

        #[test]
        fn a_distribution_without_a_record_is_skipped() {
            let (db, _) = case(&[("orphan/__init__.py", "")], &[]);

            assert!(distribution_index(&db, db.resolver_environment()).is_empty());
            assert!(owners(&db, "orphan").is_empty());
        }

        #[test]
        fn installing_a_distribution_invalidates_the_index() {
            let (mut db, site_packages) = case(
                &[("jwt/__init__.py", "")],
                &[("PyJWT-2.10.1.dist-info", "jwt/__init__.py,sha256=a,1\n")],
            );

            assert!(
                distribution_index(&db, db.resolver_environment())
                    .owners_of_top_level("numpy")
                    .is_empty()
            );

            db.write_file(site_packages.join("numpy/__init__.py"), "")
                .unwrap();
            db.write_file(
                site_packages.join("numpy-2.2.6.dist-info/RECORD"),
                "numpy/__init__.py,sha256=a,1\n",
            )
            .unwrap();

            assert_eq!(owners(&db, "numpy"), ["numpy"]);
        }

        #[test]
        fn rewriting_a_record_invalidates_the_index() {
            let (mut db, site_packages) = case(
                &[("jwt/__init__.py", ""), ("extra/__init__.py", "")],
                &[("PyJWT-2.10.1.dist-info", "jwt/__init__.py,sha256=a,1\n")],
            );

            assert!(
                distribution_index(&db, db.resolver_environment())
                    .owners_of_top_level("extra")
                    .is_empty()
            );

            db.write_file(
                site_packages.join("PyJWT-2.10.1.dist-info/RECORD"),
                "jwt/__init__.py,sha256=a,1\nextra/__init__.py,sha256=b,2\n",
            )
            .unwrap();

            assert_eq!(owners(&db, "extra"), ["PyJWT"]);
        }
    }
    mod requirements {
        use super::*;

        fn required(metadata: &str) -> Vec<String> {
            required_distributions(metadata)
                .map(|name| name.to_string())
                .collect()
        }

        #[test]
        fn a_field_that_runs_over_several_lines_does_not_end_the_fields() {
            // a real `METADATA` carries the whole license text in one field, and
            // the empty-looking lines between its paragraphs are the field's, not
            // the end of the block that `Requires-Dist` is in
            let metadata = "\
Metadata-Version: 2.1
Name: pandas
License: BSD 3-Clause License
        
         Redistribution is permitted.
Requires-Dist: numpy>=1.26.0; python_version < \"3.14\"
Requires-Dist: python-dateutil>=2.8.2

pandas is a data analysis library.

Requires-Dist: not-a-field-down-here
";

            assert_eq!(required(metadata), ["numpy", "python-dateutil"]);
        }

        #[test]
        fn a_requirement_an_extra_brings_in_is_left_out() {
            let metadata = "\
Name: pandas
Requires-Dist: numpy>=1.26.0
Requires-Dist: pytest>=7.3.2; extra == \"test\"
";

            assert_eq!(required(metadata), ["numpy"]);
        }

        #[test]
        fn the_version_and_the_extras_of_a_requirement_are_not_part_of_its_name() {
            let metadata = "\
Name: mine
Requires-Dist: uvicorn[standard]>=0.30
Requires-Dist: httpx (>=0.27)
Requires-Dist: attrs
";

            assert_eq!(required(metadata), ["uvicorn", "httpx", "attrs"]);
        }
    }
}
