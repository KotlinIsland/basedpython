use std::hash::BuildHasherDefault;
use std::iter::FusedIterator;

use ruff_db::system::SystemPath;
use rustc_hash::FxHasher;

pub use db::Db;
pub use environment::{ResolverEnvironment, ResolverFile};
pub use module::KnownModule;
pub use module::Module;
pub use module_name::{ImportingFile, ModuleName, ModuleNameResolutionError};
pub use path::{SearchPath, SearchPathError};
pub use resolve::{
    SearchPaths, file_to_module, resolve_module, resolve_module_confident, resolve_real_module,
    resolve_real_module_confident, resolve_real_shadowable_module,
};
pub use settings::{SearchPathSettings, SearchPathSettingsError};
pub use strategy::{FallibleStrategy, MisconfigurationStrategy, UseDefaultStrategy};
pub use typeshed::{PyVersionRange, TypeshedVersions, TypeshedVersionsParseError};

pub use by_typed::{BY_TYPED, ExportIndex, Marker, export_index};
pub use distributions::{
    DistributionIndex, DistributionName, RequirementIndex, distribution_index, requirement_index,
};
pub use list::{all_modules, list_modules};
pub use module_glob::{ModuleGlobError, ModuleGlobSet, ModuleGlobSetBuilder, ModuleNameMatch};
pub use resolve::{ModuleResolveMode, SearchPathIterator, search_paths};

mod by_typed;
mod db;
mod distributions;
mod environment;
mod list;
mod module;
mod module_glob;
mod module_name;
mod path;
mod resolve;
mod settings;
mod strategy;
mod typeshed;

type FxOrderMap<K, V> = ordermap::map::OrderMap<K, V, BuildHasherDefault<FxHasher>>;

#[cfg(test)]
mod testing;

/// The name a module at `path` would have, whether or not anything is there yet.
///
/// [`file_to_module`] answers this for a file the system already knows about, and it does more than
/// convert a path: it resolves the name it derived back to a file and checks that the answer is the
/// same file, so that a `src/foo.py` sitting beside a `src/foo/__init__.py` is correctly reported as
/// *not* being the module `foo`.
///
/// That check is exactly what cannot be done for a path nothing is at. This answers the narrower,
/// purely path-shaped question — which search path covers it, and what does the rest of the path
/// spell — which is what an editor asks when it is about to *move* a file: at that moment the old
/// path still holds the file and the new path holds nothing, and both names are needed to work out
/// what the move costs.
///
/// Works for a directory as well as a file, because a package is a directory: `src/foo/bar` and
/// `src/foo/bar.py` both give `foo.bar`.
///
/// # Which search path names it
///
/// The **deepest** one that contains it, not the first one consulted. Search paths nest all the
/// time — a project root with a `src/` layout under it, and, in a uv workspace, one editable entry
/// per member pointing at that member's own `src` — so a member's package is inside two of them at
/// once. Taking the first would name `packages/alpha/src/alpha` after the project root, as
/// `packages.alpha.src.alpha`, which is not a module anything can import and not the name any
/// `import alpha` in the project resolves to. The deepest entry is the one whose name resolves back
/// to that path, which is what [`file_to_module`] verifies for a file that exists.
pub fn path_to_module_name<'db>(
    db: &'db dyn Db,
    resolver_environment: ResolverEnvironment<'db>,
    path: &SystemPath,
) -> Option<ModuleName> {
    // `Typing` mode for the reason `system_module_search_paths` gives below: the question is which
    // paths belong to the project at all, not which of two stdlib variants a name resolves to.
    search_paths(db, resolver_environment, ModuleResolveMode::Typing)
        .filter_map(|search_path| {
            let name = search_path.relativize_system_path(path)?.to_module_name()?;
            // How much of `path` this search path accounts for. A vendored path accounts for none of
            // it and sorts last, which is right: nothing under the project is named by the stdlib.
            let depth = search_path
                .as_system_path()
                .map_or(0, |root| root.as_str().len());
            Some((depth, name))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, name)| name)
}

/// Returns an iterator over all search paths pointing to a system path
pub fn system_module_search_paths<'db>(
    db: &'db dyn Db,
    resolver_environment: ResolverEnvironment<'db>,
) -> SystemModuleSearchPathsIter<'db> {
    SystemModuleSearchPathsIter {
        // Always run in `Typing` mode because we want to include as much as possible
        // and we don't care about the "real" stdlib
        inner: search_paths(db, resolver_environment, ModuleResolveMode::Typing),
    }
}

pub struct SystemModuleSearchPathsIter<'db> {
    inner: SearchPathIterator<'db>,
}

impl<'db> Iterator for SystemModuleSearchPathsIter<'db> {
    type Item = &'db SystemPath;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let next = self.inner.next()?;

            if let Some(system_path) = next.as_system_path() {
                return Some(system_path);
            }
        }
    }
}

impl FusedIterator for SystemModuleSearchPathsIter<'_> {}

#[cfg(test)]
mod tests {
    use ruff_db::Db as _;
    use ruff_db::system::{DbWithWritableSystem as _, SystemPathBuf};

    use crate::db::tests::TestDb;
    use crate::settings::SearchPathSettings;
    use crate::strategy::FallibleStrategy;

    use super::*;

    /// A project whose search paths nest, which is what a uv workspace's editable installs produce:
    /// the project root, and one entry per member pointing at that member's own `src`.
    fn workspace() -> TestDb {
        let project = SystemPathBuf::from("/project");
        let member_src = project.join("packages/alpha/src");

        let mut db = TestDb::new();
        db.write_file(member_src.join("alpha/__init__.py"), "")
            .unwrap();

        let search_paths = SearchPathSettings {
            src_roots: vec![project],
            extra_paths: vec![member_src],
            ..SearchPathSettings::empty()
        }
        .to_search_paths(db.system(), db.vendored(), &FallibleStrategy)
        .expect("valid search path settings");
        db.set_search_paths(search_paths);
        db
    }

    /// The bug this rule exists for: the project root also contains the member's package, and naming
    /// it from there gives `packages.alpha.src.alpha` — a name nothing imports and nothing resolves.
    #[test]
    fn a_path_is_named_by_the_deepest_search_path_that_contains_it() {
        let db = workspace();
        assert_eq!(
            path_to_module_name(
                &db,
                db.resolver_environment(),
                SystemPath::new("/project/packages/alpha/src/alpha"),
            ),
            ModuleName::new("alpha"),
        );
    }

    #[test]
    fn a_module_inside_a_package_is_named_under_it() {
        let db = workspace();
        assert_eq!(
            path_to_module_name(
                &db,
                db.resolver_environment(),
                SystemPath::new("/project/packages/alpha/src/alpha/util.py"),
            ),
            ModuleName::new("alpha.util"),
        );
    }

    /// The point of asking about a path rather than a file: at the moment an editor asks, the answer
    /// is about somewhere nothing has been written yet.
    #[test]
    fn a_path_nothing_is_at_still_has_a_name() {
        let db = workspace();
        assert_eq!(
            path_to_module_name(
                &db,
                db.resolver_environment(),
                SystemPath::new("/project/packages/alpha/src/gamma"),
            ),
            ModuleName::new("gamma"),
        );
    }

    #[test]
    fn a_path_no_search_path_covers_is_not_a_module() {
        let db = workspace();
        assert_eq!(
            path_to_module_name(
                &db,
                db.resolver_environment(),
                SystemPath::new("/elsewhere/thing.py")
            ),
            None,
        );
    }
}
