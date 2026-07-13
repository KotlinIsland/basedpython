//! ast patches applied to the basedpython typeshed (`.byi`) after fresh
//! reverse-transpile from upstream `.pyi`
//!
//! each patch declares a set of target symbols and emits text edits over a
//! parsed module. patches run as phase 2 of the sync — after reverse-transpile
//! but before the pep 695 `ruff-fix` — so they operate on the legacy
//! `TypeVar(...)` + `Generic[...]` form, where typevars appear as plain name
//! references in class bases and method signatures
//!
//! see `docs/basedpython/development/typeshed-patches.md` for the full design
//! and the ongoing typeshed sync workflow

pub mod patches;
pub mod pep695;

use std::path::Path;

use ruff_python_ast::ModModule;
use ruff_python_parser::Parsed;

/// a single semantic adjustment applied to one or more typeshed modules
pub trait Patch {
    /// stable identifier used in logs and drift alerts
    fn name(&self) -> &'static str;

    /// qualified symbols this patch touches, e.g. `["typing.Mapping"]`. used
    /// for drift detection: if any of these symbols changed in an upstream
    /// sync, the patch is flagged for review
    fn target_symbols(&self) -> &'static [&'static str];

    /// return text edits over `parsed` if this patch applies to the module at
    /// `module_path` (relative to the typeshed `stdlib/` root). empty vec
    /// means no-op for this file
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit>;
}

/// minimal text edit. (start, end, replacement). end is exclusive
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// registry of every legacy-form patch the sync pipeline must apply, in
/// declared order. these run before the pep 695 conversion and see the legacy
/// `TypeVar` + `Generic[...]` form
pub fn all_patches() -> Vec<Box<dyn Patch>> {
    // patches are added here as upstream syncs surface concrete drift. each
    // entry must have a corresponding module in `src/patches/` with tests
    vec![Box::new(patches::mapping::MappingKeyCovariance)]
}

/// registry of patches that run *after* the pep 695 conversion, over the final
/// form with explicit variance keywords. a patch belongs here when it needs the
/// resolved variance (`in out` vs `out`) that only the converted form exposes
pub fn all_post_patches() -> Vec<Box<dyn Patch>> {
    vec![Box::new(patches::output_widening::OutputWidening)]
}

/// dotted module name for a typeshed file path relative to `stdlib/`, e.g.
/// `typing.byi` -> `typing`, `os/path.byi` -> `os.path`,
/// `asyncio/__init__.byi` -> `asyncio`
pub(crate) fn module_qualname(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let mut parts: Vec<&str> = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if stem != "__init__" {
        parts.push(stem);
    }
    Some(parts.join("."))
}

/// apply `edits` to `source`, returning the new text. edits must be disjoint;
/// applied in reverse start order so earlier offsets remain valid
pub fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = source.to_string();
    let mut last_start = usize::MAX;
    for edit in edits {
        assert!(
            edit.end <= last_start,
            "overlapping edits: {edit:?} overlaps prior at {last_start}"
        );
        last_start = edit.start;
        out.replace_range(edit.start..edit.end, &edit.replacement);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edits_disjoint() {
        let src = "hello world";
        let edits = vec![
            Edit {
                start: 0,
                end: 5,
                replacement: "HI".into(),
            },
            Edit {
                start: 6,
                end: 11,
                replacement: "THERE".into(),
            },
        ];
        assert_eq!(apply_edits(src, edits), "HI THERE");
    }

    #[test]
    fn apply_edits_empty() {
        assert_eq!(apply_edits("unchanged", vec![]), "unchanged");
    }
}
