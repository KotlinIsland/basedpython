//! What a project declares it depends on, and what that lets a file import.
//!
//! Reading the declaration out of `pyproject.toml` belongs to a crate above this
//! one; the manifest arrives through [`crate::Db::dependency_manifest`] already
//! read. What is left to do with it — deciding whether a given import is one the
//! file is entitled to make — is here.
//!
//! Nothing in this module ever answers "not allowed" from missing information. A
//! project with no manifest, an environment with no install metadata, and an
//! import ty could not attribute to any distribution all reach the same place:
//! no restriction at all.

use ruff_db::files::File;
use ty_module_resolver::{DistributionName, Module, distribution_index};

use crate::Db;

/// A group of requirements a project declares.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, get_size2::GetSize)]
pub enum GroupName {
    /// `[project].dependencies` — what the project needs to run.
    Project,
    /// `[project.optional-dependencies].<name>` — an extra, installed on request.
    Extra(Box<str>),
    /// `[dependency-groups].<name>` (PEP 735), or `[tool.uv].dev-dependencies`,
    /// which is spelled here as the group `dev`.
    ///
    /// These are never installed by anything that depends on the project, so
    /// nothing the project ships may import them.
    Group(Box<str>),
}

impl GroupName {
    /// How this group is written in configuration that selects groups by name.
    pub fn as_str(&self) -> &str {
        match self {
            GroupName::Project => "project",
            GroupName::Extra(name) | GroupName::Group(name) => name,
        }
    }

    /// Whether a distribution in this group is installed for everyone who
    /// installs the project.
    ///
    /// Only [`GroupName::Project`] is: an extra is installed on request, and a
    /// dependency group is never installed by a dependant at all.
    pub fn is_always_installed(&self) -> bool {
        matches!(self, GroupName::Project)
    }
}

impl std::fmt::Display for GroupName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupName::Project => f.write_str("`[project].dependencies`"),
            GroupName::Extra(name) => write!(f, "extra `{name}`"),
            GroupName::Group(name) => write!(f, "dependency group `{name}`"),
        }
    }
}

/// One declared group and the distributions it declares.
#[derive(Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct DependencyGroup {
    pub name: GroupName,
    pub requirements: Box<[DistributionName]>,
}

/// Everything a project declares about what it depends on.
#[derive(Debug, Default, Eq, PartialEq, get_size2::GetSize)]
pub struct DependencyManifest {
    /// The project's own name, when it states one.
    ///
    /// This is what identifies the modules the project ships, which is what
    /// decides where a dependency group may not be imported from.
    project_name: Option<DistributionName>,
    groups: Box<[DependencyGroup]>,
}

impl DependencyManifest {
    pub fn new(project_name: Option<DistributionName>, groups: Vec<DependencyGroup>) -> Self {
        Self {
            project_name,
            groups: groups.into_boxed_slice(),
        }
    }

    pub fn project_name(&self) -> Option<&DistributionName> {
        self.project_name.as_ref()
    }

    pub fn groups(&self) -> &[DependencyGroup] {
        &self.groups
    }

    /// The groups that declare `distribution`.
    ///
    /// A distribution can be in more than one — a test dependency that is also
    /// an extra, say — and then any one of them being available is enough.
    pub fn groups_declaring<'a>(
        &'a self,
        distribution: &'a DistributionName,
    ) -> impl Iterator<Item = &'a GroupName> {
        self.groups
            .iter()
            .filter(move |group| group.requirements.contains(distribution))
            .map(|group| &group.name)
    }

    /// Whether any group declares `distribution`.
    pub fn declares(&self, distribution: &DistributionName) -> bool {
        self.groups_declaring(distribution).next().is_some()
    }

    /// Whether the manifest says nothing at all.
    ///
    /// A `pyproject.toml` with a `[project]` table but no dependencies is a
    /// project that genuinely depends on nothing, and an import of a third-party
    /// module in it really is undeclared. A file with no `[project]` table at all
    /// is not a manifest and never reaches here.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// How an import of a third-party module stands against what the file may import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportStanding<'a> {
    /// Nothing is known that could make this import wrong: there is no manifest,
    /// the environment has no install metadata, or the module is not installed
    /// third-party code at all.
    Unknown,
    /// The module comes from a distribution this file may import.
    Available,
    /// The module comes from a distribution the project declares, but only in
    /// groups that are not available to this file.
    Misplaced {
        distribution: &'a DistributionName,
        declared_in: Vec<&'a GroupName>,
    },
    /// The module is installed, but the project declares no dependency on the
    /// distribution that installed it: it is only there because something else
    /// needed it.
    Undeclared { distribution: &'a DistributionName },
}

/// The groups `file` may import from.
///
/// A group is available to a file unless the file is part of what the project
/// ships. Shipped code is installed for everyone who installs the project, and
/// nothing installs the project's dependency groups along with it, so an import
/// of one from shipped code is broken for every user of the project.
///
/// Which modules the project ships is derived from the name it gives itself:
/// `[project] name = "my-lib"` ships `my_lib`. A project that says nothing about
/// its name ships nothing that can be identified, and then every group is
/// available everywhere — under-eager, but never wrong.
pub fn available_groups(db: &dyn Db, file: File) -> AvailableGroups<'_> {
    let Some(manifest) = db.dependency_manifest(file) else {
        return AvailableGroups::Unknown;
    };

    let settings = db.analysis_settings(file);
    let allowed = if let Some(selected) = settings.dependency_groups.as_deref() {
        AllowedGroups::Named(selected.to_vec())
    } else if is_shipped(db, file, manifest, settings.shipped_modules.as_deref()) {
        AllowedGroups::Shipped
    } else {
        AllowedGroups::All
    };

    AvailableGroups::Known { manifest, allowed }
}

/// The groups a file may import from, as [`available_groups`] worked them out.
#[derive(Clone, Debug)]
pub enum AvailableGroups<'db> {
    /// The project declares nothing, so nothing can be out of place.
    ///
    /// This is distinct from a manifest that allows every group: there, a
    /// distribution declared in no group at all is still undeclared.
    Unknown,
    Known {
        manifest: &'db DependencyManifest,
        allowed: AllowedGroups,
    },
}

/// Which of a manifest's groups a file may import from.
#[derive(Clone, Debug)]
pub enum AllowedGroups {
    /// Every group the project declares.
    All,
    /// Only what is installed for everyone who installs the project: the main
    /// dependency list and the extras.
    Shipped,
    /// Exactly the groups named, or every group if one of them is `*`.
    Named(Vec<Box<str>>),
}

impl AllowedGroups {
    fn allows(&self, group: &GroupName) -> bool {
        match self {
            AllowedGroups::All => true,
            // an extra is not installed unless it is asked for, but the code
            // that guards an optional import is shipped code too. treating an
            // extra as unavailable would report every one of those, so what an
            // extra needs is a check that the import is guarded — a separate
            // question from this one
            AllowedGroups::Shipped => matches!(group, GroupName::Project | GroupName::Extra(_)),
            AllowedGroups::Named(selected) => selected
                .iter()
                .any(|name| &**name == group.as_str() || &**name == "*"),
        }
    }
}

impl<'db> AvailableGroups<'db> {
    pub fn manifest(&self) -> Option<&'db DependencyManifest> {
        match self {
            AvailableGroups::Unknown => None,
            AvailableGroups::Known { manifest, .. } => Some(manifest),
        }
    }

    /// Whether `distribution` may be imported here.
    ///
    /// A distribution the manifest does not declare at all is never allowed,
    /// however permissive the group selection: importing something only because
    /// a dependency happens to install it is wrong everywhere, not only in
    /// shipped code.
    pub fn allows_distribution(&self, distribution: &DistributionName) -> bool {
        match self {
            AvailableGroups::Unknown => true,
            AvailableGroups::Known { manifest, allowed } => manifest
                .groups_declaring(distribution)
                .any(|group| allowed.allows(group)),
        }
    }
}

/// Where `module`, imported from `file`, stands against what `file` may import.
pub fn import_standing<'db>(
    db: &'db dyn Db,
    file: File,
    module: Module<'db>,
) -> ImportStanding<'db> {
    let available = available_groups(db, file);
    let Some(manifest) = available.manifest() else {
        return ImportStanding::Unknown;
    };

    let index = distribution_index(db, db.program_file(file).resolver_environment(db));
    if index.is_empty() {
        // nothing in the environment could be attributed to a distribution, so
        // an import being unattributable says nothing about the import
        return ImportStanding::Unknown;
    }

    let owners = index.owners_of(db, module);
    let [first, ..] = owners else {
        return ImportStanding::Unknown;
    };

    if owners
        .iter()
        .any(|owner| available.allows_distribution(owner))
    {
        return ImportStanding::Available;
    }

    // a namespace package has several owners; report against the one that is at
    // least declared, since that is the one the project meant
    let declared = owners.iter().find(|owner| manifest.declares(owner));

    match declared {
        Some(distribution) => ImportStanding::Misplaced {
            distribution,
            declared_in: manifest.groups_declaring(distribution).collect(),
        },
        None => ImportStanding::Undeclared {
            distribution: first,
        },
    }
}

/// Whether `file` is part of what the project ships.
fn is_shipped(
    db: &dyn Db,
    file: File,
    manifest: &DependencyManifest,
    shipped_modules: Option<&[Box<str>]>,
) -> bool {
    let Some(module) =
        ty_module_resolver::file_to_module(db, db.program_file(file).resolver_file(db))
    else {
        return false;
    };
    let Some(search_path) = module.search_path(db) else {
        return false;
    };
    if !search_path.is_first_party() {
        return false;
    }

    let root = module
        .name(db)
        .components()
        .next()
        .map(ToString::to_string)
        .unwrap_or_default();

    match shipped_modules {
        Some(shipped) => shipped.iter().any(|name| **name == *root),
        // `[project] name = "my-lib"` ships the module `my_lib`: the distribution
        // name with its separators written the way an identifier has to be
        None => manifest
            .project_name()
            .is_some_and(|name| name.normalized().replace('-', "_") == root),
    }
}
