//! What a distribution says about itself in its `by.typed` marker.
//!
//! A project only ever sees what the wheel installed. Its dependencies'
//! `pyproject.toml` files were never installed, and `METADATA` has a closed set
//! of fields with nowhere to put anything of ours, so anything a library wants to
//! tell the projects that depend on it has to travel inside the package itself.
//!
//! `by.typed` is the file that is already there. It marks a package as
//! basedpython's, and what it says beyond that is written here.
//!
//! It says which of a distribution's own dependencies are part of its public
//! interface. A project that depends on `pandas` may import `numpy` if — and only
//! if — `pandas` says that handing out numpy arrays is something it does on
//! purpose.
//!
//! It also says how the distribution's `.by` sources were lowered into the python
//! it ships, which the `.by` a project reads can't say: `lowering.compatibility`
//! decides whether a `protocol` is a `typing.Protocol`, and the shipped python was
//! fixed when the distribution was built, whatever the project reading it sets.

use ruff_db::files::{directory_listing, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::SystemPath;
use rustc_hash::FxHashMap;

use crate::db::Db;
use crate::distributions::{DistributionName, distribution_name_from_dist_info, record_path};
use crate::environment::{ResolverEnvironment, ResolverFile};
use crate::path::directory_contains_file;
use crate::resolve::{ModuleResolveMode, search_paths};

/// The name of the marker file a basedpython distribution ships.
pub const BY_TYPED: &str = "by.typed";

/// What a distribution's `by.typed` declares.
///
/// An empty marker is the ordinary case — the file is a marker first — and it
/// declares nothing, which is also what an unreadable or ill-formed one declares.
/// A package saying something ty cannot understand is not a package saying "yes".
#[derive(Clone, Debug, Default, Eq, PartialEq, get_size2::GetSize)]
pub struct Marker {
    exported_dependencies: Box<[DistributionName]>,
    /// whether the distribution was built with `lowering.compatibility`. a marker that doesn't
    /// say was built without it, which is the default
    lowering_compatibility: bool,
}

impl Marker {
    /// Reads what a `by.typed` says.
    fn parse(text: &str) -> Self {
        let Ok(table) = text.parse::<toml::Table>() else {
            return Self::default();
        };

        let exported = table
            .get("exported-dependencies")
            .and_then(toml::Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(DistributionName::new)
                    .collect()
            })
            .unwrap_or_default();

        let lowering_compatibility = table
            .get("lowering")
            .and_then(toml::Value::as_table)
            .and_then(|lowering| lowering.get("compatibility"))
            .and_then(toml::Value::as_bool)
            .unwrap_or_default();

        Self {
            exported_dependencies: exported,
            lowering_compatibility,
        }
    }

    /// The text of a marker that declares `exported`, and the build's
    /// `lowering.compatibility` where it was set.
    ///
    /// This is the other end of `parse`, and the only thing that writes a
    /// `by.typed`'s contents: what a build stages is what a consumer reads back.
    pub fn render(exported: &[impl AsRef<str>], lowering_compatibility: bool) -> String {
        let mut table = toml::Table::new();
        if !exported.is_empty() {
            let names = exported
                .iter()
                .map(|name| toml::Value::String(name.as_ref().to_string()))
                .collect();
            table.insert(
                "exported-dependencies".to_string(),
                toml::Value::Array(names),
            );
        }
        if lowering_compatibility {
            let mut lowering = toml::Table::new();
            lowering.insert("compatibility".to_string(), toml::Value::Boolean(true));
            table.insert("lowering".to_string(), toml::Value::Table(lowering));
        }

        table.to_string()
    }

    /// Whether the distribution's `.by` sources were lowered with
    /// `lowering.compatibility`.
    pub fn lowering_compatibility(&self) -> bool {
        self.lowering_compatibility
    }

    /// The dependencies this distribution declares part of its interface.
    ///
    /// Resolution asks [`Self::exports`] about one name at a time; the whole list is
    /// what the tests below check `parse` and `render` against.
    #[cfg(test)]
    fn exported_dependencies(&self) -> &[DistributionName] {
        &self.exported_dependencies
    }

    /// Whether this distribution hands `distribution` out on purpose.
    fn exports(&self, distribution: &DistributionName) -> bool {
        self.exported_dependencies.contains(distribution)
    }

    fn is_empty(&self) -> bool {
        self.exported_dependencies.is_empty() && !self.lowering_compatibility
    }
}

/// What the `by.typed` of the package a file was built into declares.
///
/// A build writes a marker into each package it ships, so a file under a marked
/// package is one a build produced, and the nearest marker above it is the one that
/// build wrote. Only the search paths that hold code from elsewhere are asked: an
/// installed distribution, an editable install, or an extra path. The same file can
/// also be under the project's own root — a workspace member's build is — and is
/// still that build's. A file only the project's own search paths reach is lowered as
/// the project says, wherever a marker sits among its sources. `None` for a file no
/// marked package holds.
pub fn built_marker(db: &dyn Db, file: ResolverFile<'_>) -> Option<Marker> {
    let path = file.file(db).path(db).as_system_path()?;
    search_paths(db, file.environment(db), ModuleResolveMode::Runtime)
        .filter(|search_path| search_path.can_contain_third_party_code())
        .find_map(|search_path| {
            let root = search_path.as_system_path()?;
            let packages = path.strip_prefix(root).ok()?.parent()?;
            let mut directory = root.to_path_buf();
            let mut nearest = None;
            for package in packages.components() {
                directory.push(package.as_str());
                // a marker only counts inside a package, as resolution has it
                if directory_contains_file(db, &directory, &[BY_TYPED])
                    && directory_contains_file(
                        db,
                        &directory,
                        &["__init__.py", "__init__.pyi", "__init__.by", "__init__.byi"],
                    )
                {
                    nearest = Some(directory.join(BY_TYPED));
                }
            }
            let marker = system_path_to_file(db, nearest?).ok()?;
            Some(Marker::parse(&source_text(db, marker)))
        })
}

/// What every installed distribution's `by.typed` declares.
#[derive(Debug, Default, Eq, PartialEq, get_size2::GetSize)]
pub struct ExportIndex {
    /// Only distributions whose marker declares something are kept: an empty
    /// marker says the same as no marker at all.
    markers: FxHashMap<DistributionName, Marker>,
}

impl ExportIndex {
    /// Whether `distribution` declares `exported` part of its public interface.
    pub fn exports(&self, distribution: &DistributionName, exported: &DistributionName) -> bool {
        self.markers
            .get(distribution)
            .is_some_and(|marker| marker.exports(exported))
    }

    /// What `distribution` declares, if it declares anything.
    pub fn marker_of(&self, distribution: &DistributionName) -> Option<&Marker> {
        self.markers.get(distribution)
    }
}

/// The `by.typed` of every installed distribution that ships one.
///
/// Only asked for when an import would otherwise be reported, so a project whose
/// dependencies are all declared never reads a marker at all.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn export_index<'db>(
    db: &'db dyn Db,
    resolver_environment: ResolverEnvironment<'db>,
) -> ExportIndex {
    let _span = tracing::debug_span!("export_index").entered();

    let mut markers = FxHashMap::default();

    for site_packages in resolver_environment.search_paths(db).site_packages_paths() {
        index_markers(db, site_packages, &mut markers);
    }

    markers.shrink_to_fit();
    ExportIndex { markers }
}

fn index_markers(
    db: &dyn Db,
    site_packages: &SystemPath,
    markers: &mut FxHashMap<DistributionName, Marker>,
) {
    let Ok(listing) = directory_listing(db, site_packages) else {
        tracing::debug!("Failed to list `{site_packages}` when indexing markers");
        return;
    };

    for (name, file_type) in listing.iter() {
        if !file_type.is_directory() {
            continue;
        }
        let Some(distribution) = distribution_name_from_dist_info(name) else {
            continue;
        };

        // where the marker is, is what the distribution's own `RECORD` says: a
        // distribution can install more than one package, and only its record of
        // what it installed ties any of them back to it
        let record = site_packages.join(name).join("RECORD");
        let Ok(record) = system_path_to_file(db, &record) else {
            continue;
        };

        for path in marker_paths(&source_text(db, record)) {
            let Ok(marker) = system_path_to_file(db, site_packages.join(path)) else {
                continue;
            };
            let marker = Marker::parse(&source_text(db, marker));
            if marker.is_empty() {
                continue;
            }

            markers.insert(distribution.clone(), marker);
            break;
        }
    }
}

/// The `by.typed` files a `RECORD` says its distribution installed.
///
/// A marker only counts inside a package. One directly in `site-packages` marks
/// nothing, and a `RECORD` reaches outside the install directory with `..`.
fn marker_paths(record: &str) -> impl Iterator<Item = &str> {
    record.lines().filter_map(|line| {
        let path = record_path(line)?;
        let (directory, file) = path.rsplit_once('/')?;

        (file == BY_TYPED
            && !directory.is_empty()
            && !directory.starts_with("..")
            && !directory.starts_with('/'))
        .then_some(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_declares_what_it_lists() {
        let marker = Marker::parse("exported-dependencies = [\"numpy\", \"PyJWT\"]\n");

        assert!(marker.exports(&DistributionName::new("numpy")));
        // an export is a distribution, so it is matched however it is spelled
        assert!(marker.exports(&DistributionName::new("pyjwt")));
        assert!(!marker.exports(&DistributionName::new("pandas")));
    }

    #[test]
    fn a_marker_that_says_nothing_exports_nothing() {
        // the empty file is the ordinary marker, and the others are a marker
        // written by something that is not this
        for text in ["", "\n", "not toml at all [", "exported-dependencies = 12"] {
            assert!(
                Marker::parse(text).exported_dependencies().is_empty(),
                "`{text}` should declare nothing",
            );
        }
    }

    #[test]
    fn what_is_rendered_is_what_is_read_back() {
        let rendered = Marker::render(&["numpy", "python-dateutil"], true);
        let marker = Marker::parse(&rendered);

        assert_eq!(
            marker.exported_dependencies(),
            [
                DistributionName::new("numpy"),
                DistributionName::new("python-dateutil")
            ]
        );
        assert!(marker.lowering_compatibility());
    }

    #[test]
    fn a_marker_says_how_its_distribution_was_lowered() {
        let nothing: [&str; 0] = [];
        assert!(Marker::parse(&Marker::render(&nothing, true)).lowering_compatibility());
        // a marker that doesn't say was built with the default lowering
        for text in ["", "exported-dependencies = [\"numpy\"]\n", "lowering = 1"] {
            assert!(
                !Marker::parse(text).lowering_compatibility(),
                "`{text}` should declare the default lowering",
            );
        }
    }

    #[test]
    fn a_marker_with_nothing_to_declare_is_still_a_marker() {
        // the file's presence is what marks the package, so it is written even
        // when there is nothing to put in it
        let nothing: [&str; 0] = [];
        assert_eq!(Marker::render(&nothing, false), "");
    }

    #[test]
    fn only_a_marker_inside_a_package_is_found() {
        let record = "\
my_lib/__init__.py,sha256=a,1
my_lib/by.typed,sha256=b,2
by.typed,sha256=c,3
../../../etc/by.typed,sha256=d,4
my_lib-1.0.dist-info/RECORD,,
";

        assert_eq!(
            marker_paths(record).collect::<Vec<_>>(),
            ["my_lib/by.typed"]
        );
    }
}
