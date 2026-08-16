//! The names that are conventionally an alias of a module.
//!
//! A file that writes `np.` before importing anything almost always means numpy, because `np` is
//! what numpy is written as everywhere numpy is written. Completions read that: `np.` offers
//! numpy's members, and accepting one writes the `import numpy as np` the file was missing.
//!
//! A project adds aliases of its own through `[tool.ty.editor] common-aliases`, and an alias it
//! spells replaces the one here.

use ty_module_resolver::ModuleName;
use ty_project::Db;

/// The aliases that are conventions of the wider python ecosystem, sorted by alias.
///
/// Nothing is offered for a module the project does not have, so an entry here only ever reaches
/// somebody who installed the library it names.
static KNOWN_ALIASES: &[(&str, &str)] = &[
    ("ET", "xml.etree.ElementTree"),
    ("F", "torch.nn.functional"),
    ("da", "dask.array"),
    ("dd", "dask.dataframe"),
    ("dt", "datetime"),
    ("go", "plotly.graph_objects"),
    ("gpd", "geopandas"),
    ("jnp", "jax.numpy"),
    ("mpl", "matplotlib"),
    ("nn", "torch.nn"),
    ("np", "numpy"),
    ("npt", "numpy.typing"),
    ("nx", "networkx"),
    ("pa", "pyarrow"),
    ("pd", "pandas"),
    ("pl", "polars"),
    ("plt", "matplotlib.pyplot"),
    ("px", "plotly.express"),
    ("sa", "sqlalchemy"),
    ("sm", "statsmodels.api"),
    ("sns", "seaborn"),
    ("sp", "scipy"),
    ("st", "streamlit"),
    ("tf", "tensorflow"),
    ("tk", "tkinter"),
    ("ttk", "tkinter.ttk"),
    ("xr", "xarray"),
];

/// The module `alias` names, when it is one of the names an alias is.
///
/// This does not say whether the project has that module — resolve the name to find that out.
pub(crate) fn module_of(db: &dyn Db, alias: &str) -> Option<ModuleName> {
    let configured = db.project().settings(db).editor().common_alias(alias);
    let module = configured.or_else(|| {
        KNOWN_ALIASES
            .binary_search_by_key(&alias, |&(known, _)| known)
            .ok()
            .and_then(|found| KNOWN_ALIASES.get(found))
            .map(|&(_, module)| module)
    })?;
    ModuleName::new(module)
}

/// Every alias, paired with the module it names.
///
/// A configured alias comes first, and replaces the known alias it spells.
pub(crate) fn all(db: &dyn Db) -> impl Iterator<Item = (&str, &str)> {
    let editor = db.project().settings(db).editor();
    let known = KNOWN_ALIASES
        .iter()
        .filter(|(known, _)| editor.common_alias(known).is_none())
        .map(|&(alias, module)| (alias, module));
    editor.common_aliases().chain(known)
}

#[cfg(test)]
mod tests {
    use super::KNOWN_ALIASES;

    /// `module_of` finds an alias by binary search.
    #[test]
    fn known_aliases_are_sorted() {
        let mut sorted: Vec<_> = KNOWN_ALIASES.iter().map(|&(alias, _)| alias).collect();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            KNOWN_ALIASES
                .iter()
                .map(|&(alias, _)| alias)
                .collect::<Vec<_>>()
        );
    }
}
