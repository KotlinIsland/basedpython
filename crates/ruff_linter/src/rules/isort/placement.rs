//! Where isort puts one import among the imports of a block it has already sorted.
//!
//! An editor that adds an import (an auto-import completion, a quick fix for an unresolved
//! name) inserts a single statement, or a single member into an existing `from` import, and
//! leaves every other line alone. To leave the block as `I001` would write it, the statement
//! has to land in its section, in the order isort sorts that section, separated from its
//! neighbours by the blank lines isort puts there. [`ImportOrder`] answers those questions
//! with the same keys, sections and settings [`super::format_imports`] sorts with, so the two
//! cannot disagree.

use std::cmp::Ordering;
use std::path::PathBuf;

use ruff_python_ast::PythonVersion;
pub use ruff_python_importer::SortedImport;

use super::categorize::{ImportSection, ImportType, categorize};
use super::settings::Settings;
use super::sorting::{ImportStyle, MemberKey, ModuleKey};
use crate::package::PackageRoot;

/// The import style isort groups `import` by within a section.
fn style(import: &SortedImport) -> ImportStyle {
    match import {
        SortedImport::Straight { .. } => ImportStyle::Straight,
        SortedImport::From { .. } => ImportStyle::From,
    }
}

/// Where an import statement sorts in its block: its section, its group within the section
/// (plain or `from`, eager or lazy) and its key within the group. Positions compare as isort
/// orders the statements.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImportPosition<'a> {
    section: usize,
    group: u8,
    key: ModuleKey<'a>,
    style: ImportStyle,
}

impl ImportPosition<'_> {
    /// The index of the statement's section in the configured section order.
    pub fn section(&self) -> usize {
        self.section
    }

    /// The position, owning the names it compares.
    pub fn into_owned(self) -> ImportPosition<'static> {
        ImportPosition {
            section: self.section,
            group: self.group,
            key: self.key.into_owned(),
            style: self.style,
        }
    }
}

/// The ordering isort applies to the imports of one file.
#[derive(Debug, Clone, Copy)]
pub struct ImportOrder<'a> {
    settings: &'a Settings,
    src: &'a [PathBuf],
    package: Option<PackageRoot<'a>>,
    target_version: PythonVersion,
}

impl<'a> ImportOrder<'a> {
    /// The ordering for a file in `package` (as `detect_package_root` finds it), under the
    /// project's isort `settings`, its `src` roots and the Python version it targets.
    pub fn new(
        settings: &'a Settings,
        src: &'a [PathBuf],
        package: Option<PackageRoot<'a>>,
        target_version: PythonVersion,
    ) -> Self {
        Self {
            settings,
            src,
            package,
            target_version,
        }
    }

    /// Where `import` sorts.
    pub fn position<'b>(&self, import: &SortedImport<'b>) -> ImportPosition<'b> {
        let (module_name, is_relative) = match import {
            SortedImport::Straight { name, .. } => ((*name).to_string(), false),
            SortedImport::From { module, level, .. } => (
                ruff_python_ast::helpers::format_import_from(*level, *module).into_owned(),
                *level > 0,
            ),
        };
        let section = categorize(
            &module_name,
            is_relative,
            self.src,
            self.package,
            self.settings.detect_same_package,
            &self.settings.known_modules,
            self.target_version,
            self.settings.no_sections,
            &self.settings.section_order,
            &self.settings.default_section,
        );
        let is_future = matches!(section, ImportSection::Known(ImportType::Future));
        let section = self
            .settings
            .section_order
            .iter()
            .position(|candidate| candidate == section)
            .unwrap_or(self.settings.section_order.len());

        let style = style(import);
        let key = match import {
            SortedImport::Straight { name, asname, .. } => {
                ModuleKey::from_module(Some(name), *asname, 0, None, style, self.settings)
            }
            SortedImport::From {
                module,
                level,
                first_member,
                ..
            } => ModuleKey::from_module(*module, None, *level, *first_member, style, self.settings),
        };

        // Mirrors `order_imports`: the `__future__` section always puts its `from` imports
        // first, `force-sort-within-sections` mixes the two kinds, and otherwise each kind is
        // its own run, plain imports first unless `from-first`. Lazy imports follow the eager
        // ones in every case.
        let from_first = is_future || self.settings.from_first;
        let kind = if !is_future && self.settings.force_sort_within_sections {
            0
        } else {
            match (style, from_first) {
                (ImportStyle::Straight, false) | (ImportStyle::From, true) => 0,
                (ImportStyle::From, false) | (ImportStyle::Straight, true) => 1,
            }
        };
        let group = u8::from(import.is_lazy()) * 2 + kind;

        ImportPosition {
            section,
            group,
            key,
            style,
        }
    }

    /// How many blank lines isort writes between a statement at `before` and the statement at
    /// `after` that follows it.
    pub fn blank_lines_between(&self, before: &ImportPosition, after: &ImportPosition) -> usize {
        if before.section != after.section {
            // `format_import_block` separates two sections with a blank line unless every
            // section from the one after `before` up to `after`'s is listed in
            // `no-lines-before`.
            let separated = self
                .settings
                .section_order
                .iter()
                .take(after.section + 1)
                .skip(before.section + 1)
                .any(|section| !self.settings.no_lines_before.contains(section));
            return usize::from(separated);
        }
        if self.settings.force_sort_within_sections || self.settings.lines_between_types == 0 {
            return 0;
        }
        let first_kind = if self.settings.from_first {
            ImportStyle::From
        } else {
            ImportStyle::Straight
        };
        if before.style == first_kind && after.style != first_kind {
            self.settings.lines_between_types
        } else {
            0
        }
    }

    /// The comment isort writes above the first import of the section at `position`, if one is
    /// configured.
    pub fn section_heading(&self, position: &ImportPosition) -> Option<&'a str> {
        let section = self.settings.section_order.get(position.section)?;
        self.settings
            .import_headings
            .get(section)
            .map(String::as_str)
    }

    /// How isort orders two members of one `from` import.
    pub fn compare_members(&self, a: (&str, Option<&str>), b: (&str, Option<&str>)) -> Ordering {
        MemberKey::from_member(a.0, a.1, self.settings).cmp(&MemberKey::from_member(
            b.0,
            b.1,
            self.settings,
        ))
    }

    /// Whether isort writes `member` (imported `as asname`, if given) from `module` in a `from`
    /// import of its own rather than among the members of an existing one, under
    /// `force-single-line` or, for an aliased member, without `combine-as-imports`.
    pub fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool {
        (self.settings.force_single_line
            && module.is_none_or(|module| !self.settings.single_line_exclusions.contains(module)))
            || (asname.is_some() && !self.settings.combine_as_imports)
    }
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::PythonVersion;
    use rustc_hash::{FxHashMap, FxHashSet};

    use super::{ImportOrder, SortedImport};
    use crate::rules::isort::categorize::{ImportSection, ImportType, KnownModules};
    use crate::rules::isort::settings::Settings;
    use crate::settings::types::IdentifierPattern;

    fn straight(name: &str) -> SortedImport<'_> {
        SortedImport::Straight {
            name,
            asname: None,
            is_lazy: false,
        }
    }

    fn from<'a>(module: &'a str, member: &'a str) -> SortedImport<'a> {
        SortedImport::From {
            module: Some(module),
            level: 0,
            first_member: Some((member, None)),
            is_lazy: false,
        }
    }

    /// Sorts `imports` by position and returns them as statements.
    fn sorted(order: &ImportOrder, imports: &[SortedImport]) -> Vec<String> {
        let mut imports: Vec<_> = imports
            .iter()
            .map(|import| (order.position(import), import))
            .collect();
        imports.sort_by(|a, b| a.0.cmp(&b.0));
        imports
            .into_iter()
            .map(|(_, import)| match import {
                SortedImport::Straight { name, .. } => format!("import {name}"),
                SortedImport::From {
                    module,
                    first_member,
                    ..
                } => format!(
                    "from {} import {}",
                    module.unwrap_or_default(),
                    first_member.map(|(name, _)| name).unwrap_or_default()
                ),
            })
            .collect()
    }

    #[test]
    fn sections_then_plain_before_from() {
        let settings = Settings::default();
        let order = ImportOrder::new(&settings, &[], None, PythonVersion::PY313);
        assert_eq!(
            sorted(
                &order,
                &[
                    from("attrs", "define"),
                    straight("sys"),
                    from("collections", "OrderedDict"),
                    straight("attr"),
                    from("__future__", "annotations"),
                    straight("json"),
                ]
            ),
            [
                "from __future__ import annotations",
                "import json",
                "import sys",
                "from collections import OrderedDict",
                "import attr",
                "from attrs import define",
            ]
        );
    }

    #[test]
    fn force_sort_within_sections_mixes_kinds() {
        let settings = Settings {
            force_sort_within_sections: true,
            ..Settings::default()
        };
        let order = ImportOrder::new(&settings, &[], None, PythonVersion::PY313);
        assert_eq!(
            sorted(
                &order,
                &[
                    straight("sys"),
                    from("collections", "OrderedDict"),
                    straight("json")
                ]
            ),
            [
                "from collections import OrderedDict",
                "import json",
                "import sys"
            ]
        );
    }

    #[test]
    fn blank_lines() {
        let settings = Settings {
            lines_between_types: 1,
            known_modules: KnownModules::new(
                vec![IdentifierPattern::new("pkg").unwrap()],
                vec![],
                vec![],
                vec![],
                FxHashMap::default(),
            ),
            no_lines_before: FxHashSet::from_iter([ImportSection::Known(ImportType::FirstParty)]),
            ..Settings::default()
        };
        let order = ImportOrder::new(&settings, &[], None, PythonVersion::PY313);
        let json = order.position(&straight("json"));
        let collections = order.position(&from("collections", "OrderedDict"));
        let attr = order.position(&straight("attr"));
        let pkg = order.position(&straight("pkg"));
        // a plain import and a `from` import of one section, under `lines-between-types`
        assert_eq!(order.blank_lines_between(&json, &collections), 1);
        // two sections
        assert_eq!(order.blank_lines_between(&collections, &attr), 1);
        // a section listed in `no-lines-before`
        assert_eq!(order.blank_lines_between(&attr, &pkg), 0);
        // past a section that is absent but separated
        assert_eq!(order.blank_lines_between(&json, &pkg), 1);
    }

    #[test]
    fn members_by_type() {
        let settings = Settings::default();
        let order = ImportOrder::new(&settings, &[], None, PythonVersion::PY313);
        assert!(
            order
                .compare_members(("Counter", None), ("OrderedDict", None))
                .is_lt()
        );
        assert!(
            order
                .compare_members(("OrderedDict", None), ("defaultdict", None))
                .is_lt()
        );
        assert!(
            order
                .compare_members(("CONST", None), ("Class", None))
                .is_lt()
        );
    }
}
