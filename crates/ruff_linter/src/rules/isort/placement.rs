//! where isort puts one import among the imports of a block it has already sorted
//!
//! an editor that adds an import (an auto-import completion, a quick fix for an unresolved
//! name) inserts a single statement, or a single member into an existing `from` import, and
//! leaves every other line alone. to leave the block as `I001` would write it, the statement
//! has to land in its section, in the order isort sorts that section, separated from its
//! neighbours by the blank lines isort puts there. [`ImportOrder`] answers those questions
//! with the same keys, sections and settings [`super::format_imports`] sorts with, so the two
//! cannot disagree

use std::cell::RefCell;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;

use ruff_python_ast::{PySourceType, PythonVersion};
use ruff_python_codegen::Stylist;
use ruff_python_importer::ImportOrdering;
pub use ruff_python_importer::SortedImport;
use rustc_hash::FxHashMap;

use super::categorize::{ImportSection, ImportType, SourceRoots, categorize_in};
use super::settings::Settings;
use super::sorting::{ImportStyle, MemberKey, ModuleKey};
use crate::package::PackageRoot;
use crate::packaging::{HostPackageFiles, PackageFiles};
use crate::settings::LinterSettings;

/// the import style isort groups `import` by within a section
fn style(import: &SortedImport) -> ImportStyle {
    match import {
        SortedImport::Straight { .. } => ImportStyle::Straight,
        SortedImport::From { .. } => ImportStyle::From,
    }
}

/// where an import statement sorts in its block: its section, its group within the section
/// (plain or `from`, eager or lazy) and its key within the group. positions compare as isort
/// orders the statements
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImportPosition<'a> {
    section: usize,
    group: u8,
    key: ModuleKey<'a>,
    style: ImportStyle,
}

impl ImportPosition<'_> {
    /// the index of the statement's section in the configured section order
    pub fn section(&self) -> usize {
        self.section
    }

    /// the position, owning the names it compares
    pub fn into_owned(self) -> ImportPosition<'static> {
        ImportPosition {
            section: self.section,
            group: self.group,
            key: self.key.into_owned(),
            style: self.style,
        }
    }
}

/// The ordering isort applies to the imports of one file, looking modules up in `files` to tell
/// whether they are first-party.
pub struct ImportOrder<F> {
    settings: Arc<LinterSettings>,
    package: Option<PathBuf>,
    target_version: PythonVersion,
    files: F,
    /// the index of the section of each module categorized so far, by the name `categorize`
    /// reads, which has the leading dots of a relative import
    sections: RefCell<FxHashMap<String, usize>>,
}

impl<F: PackageFiles> ImportOrder<F> {
    /// the ordering for a file in `package` (as `detect_package_root` finds it), under the
    /// project's linter `settings` and the Python version it targets.
    pub fn new(
        settings: Arc<LinterSettings>,
        package: Option<PathBuf>,
        target_version: PythonVersion,
        files: F,
    ) -> Self {
        Self {
            settings,
            package,
            target_version,
            files,
            sections: RefCell::default(),
        }
    }

    fn isort(&self) -> &Settings {
        &self.settings.isort
    }

    /// The index of the section `module_name` sorts in, in the configured section order.
    fn section_index(&self, module_name: String, is_relative: bool) -> usize {
        if let Some(section) = self.sections.borrow().get(&module_name) {
            return *section;
        }
        let settings = self.isort();
        let section = categorize_in(
            &module_name,
            is_relative,
            SourceRoots::new(&self.settings.src, &self.files),
            self.package.as_deref().map(PackageRoot::root),
            settings.detect_same_package,
            &settings.known_modules,
            self.target_version,
            settings.no_sections,
            &settings.section_order,
            &settings.default_section,
        );
        let index = settings
            .section_order
            .iter()
            .position(|candidate| candidate == section)
            .unwrap_or(settings.section_order.len());
        self.sections.borrow_mut().insert(module_name, index);
        index
    }

    /// where `import` sorts
    pub fn position<'b>(&self, import: &SortedImport<'b>) -> ImportPosition<'b> {
        let settings = self.isort();
        let (module_name, is_relative) = match import {
            SortedImport::Straight { name, .. } => ((*name).to_string(), false),
            SortedImport::From { module, level, .. } => (
                ruff_python_ast::helpers::format_import_from(*level, *module).into_owned(),
                *level > 0,
            ),
        };
        let section = self.section_index(module_name, is_relative);
        let is_future =
            settings.section_order.get(section) == Some(&ImportSection::Known(ImportType::Future));

        let style = style(import);
        let key = match import {
            SortedImport::Straight { name, asname, .. } => {
                ModuleKey::from_module(Some(name), *asname, 0, None, style, settings)
            }
            SortedImport::From {
                module,
                level,
                first_member,
                ..
            } => ModuleKey::from_module(*module, None, *level, *first_member, style, settings),
        };

        // mirrors `order_imports`: the `__future__` section always puts its `from` imports
        // first, `force-sort-within-sections` mixes the two kinds, and otherwise each kind is
        // its own run, plain imports first unless `from-first`. lazy imports follow the eager
        // ones in every case
        let from_first = is_future || settings.from_first;
        let kind = if !is_future && settings.force_sort_within_sections {
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

    /// how many blank lines isort writes between a statement at `before` and the statement at
    /// `after` that follows it
    pub fn blank_lines_between(&self, before: &ImportPosition, after: &ImportPosition) -> usize {
        let settings = self.isort();
        if before.section != after.section {
            // `format_import_block` separates two sections with a blank line unless every
            // section from the one after `before` up to `after`'s is listed in
            // `no-lines-before`
            let separated = settings
                .section_order
                .iter()
                .take(after.section + 1)
                .skip(before.section + 1)
                .any(|section| !settings.no_lines_before.contains(section));
            return usize::from(separated);
        }
        if settings.force_sort_within_sections || settings.lines_between_types == 0 {
            return 0;
        }
        let first_kind = if settings.from_first {
            ImportStyle::From
        } else {
            ImportStyle::Straight
        };
        if before.style == first_kind && after.style != first_kind {
            settings.lines_between_types
        } else {
            0
        }
    }

    /// the comment isort writes above the first import of the section at `position`, if one is
    /// configured
    pub fn section_heading(&self, position: &ImportPosition) -> Option<&str> {
        let settings = self.isort();
        let section = settings.section_order.get(position.section)?;
        settings.import_headings.get(section).map(String::as_str)
    }

    /// Whether `comment` is the heading isort writes above a section, which it takes into the
    /// block when it is directly above the block's first import.
    pub fn is_heading(&self, comment: &str) -> bool {
        self.isort()
            .import_headings
            .values()
            .any(|heading| heading == comment)
    }

    /// how isort orders two members of one `from` import
    pub fn compare_members(&self, a: (&str, Option<&str>), b: (&str, Option<&str>)) -> Ordering {
        let settings = self.isort();
        MemberKey::from_member(a.0, a.1, settings).cmp(&MemberKey::from_member(b.0, b.1, settings))
    }

    /// whether isort writes `member` (imported `as asname`, if given) from `module` in a `from`
    /// import of its own rather than among the members of an existing one, under
    /// `force-single-line` or, for an aliased member, without `combine-as-imports`
    pub fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool {
        let settings = self.isort();
        (settings.force_single_line
            && module.is_none_or(|module| !settings.single_line_exclusions.contains(module)))
            || (asname.is_some() && !settings.combine_as_imports)
    }

    /// How many blank lines isort writes between the last import of a block and the statement
    /// after it, as `format_imports` writes them after a block in a source file.
    pub fn blank_lines_after_block(&self, before_definition: bool) -> usize {
        match usize::try_from(self.isort().lines_after_imports) {
            Ok(lines) => lines,
            Err(_) if before_definition => 2,
            Err(_) => 1,
        }
    }

    /// `source`, a run of import statements, as isort writes them in `stylist`'s style, without
    /// the headings of their sections; see [`sort_import_statements`].
    pub fn write_statements(&self, source: &str, stylist: &Stylist) -> Option<String> {
        let written = write_import_statements(
            source,
            &self.settings,
            SourceRoots::new(&self.settings.src, &self.files),
            self.package.as_deref().map(PackageRoot::root),
            PySourceType::Python,
            self.target_version,
            Some(stylist),
        )?;
        // `source` has no comments, so the only ones written are headings
        Some(
            written
                .split_inclusive('\n')
                .filter(|line| !self.is_heading(line.trim()))
                .collect(),
        )
    }
}

impl<F: PackageFiles> ImportOrdering for ImportOrder<F> {
    type Position = ImportPosition<'static>;

    fn position(&self, import: &SortedImport<'_>) -> Self::Position {
        ImportOrder::position(self, import).into_owned()
    }

    fn section(&self, position: &Self::Position) -> usize {
        position.section()
    }

    fn blank_lines_between(&self, before: &Self::Position, after: &Self::Position) -> usize {
        ImportOrder::blank_lines_between(self, before, after)
    }

    fn section_heading(&self, position: &Self::Position) -> Option<&str> {
        ImportOrder::section_heading(self, position)
    }

    fn is_heading(&self, comment: &str) -> bool {
        ImportOrder::is_heading(self, comment)
    }

    fn compare_members(&self, a: (&str, Option<&str>), b: (&str, Option<&str>)) -> Ordering {
        ImportOrder::compare_members(self, a, b)
    }

    fn imports_member_alone(&self, module: Option<&str>, asname: Option<&str>) -> bool {
        ImportOrder::imports_member_alone(self, module, asname)
    }

    fn blank_lines_after_block(&self, before_definition: bool) -> usize {
        ImportOrder::blank_lines_after_block(self, before_definition)
    }

    fn write_statements(&self, source: &str, stylist: &Stylist) -> Option<String> {
        ImportOrder::write_statements(self, source, stylist)
    }
}

/// `source`, a run of import statements and nothing else, sorted and written as `I001`'s fix
/// writes an import block with the same settings: one block, deduplicated, `from` imports of one
/// module combined, in sections, each wrapped to the line length. `None` for source with
/// anything other than import statements in it, or a comment, which isort keeps beside the import
/// it is on and this cannot place.
pub fn sort_import_statements(
    source: &str,
    settings: &LinterSettings,
    package: Option<PackageRoot<'_>>,
    source_type: PySourceType,
    target_version: PythonVersion,
) -> Option<String> {
    sort_import_statements_in(
        source,
        settings,
        &HostPackageFiles,
        package,
        source_type,
        target_version,
    )
}

/// [`sort_import_statements`], looking modules up in `files` to tell whether they are
/// first-party
pub fn sort_import_statements_in(
    source: &str,
    settings: &LinterSettings,
    files: &dyn PackageFiles,
    package: Option<PackageRoot<'_>>,
    source_type: PySourceType,
    target_version: PythonVersion,
) -> Option<String> {
    write_import_statements(
        source,
        settings,
        SourceRoots::new(&settings.src, files),
        package,
        source_type,
        target_version,
        None,
    )
}

/// [`sort_import_statements`], in `stylist`'s style rather than the style of `source` itself
fn write_import_statements(
    source: &str,
    settings: &LinterSettings,
    src: SourceRoots<'_>,
    package: Option<PackageRoot<'_>>,
    source_type: PySourceType,
    target_version: PythonVersion,
    stylist: Option<&Stylist>,
) -> Option<String> {
    use ruff_python_index::Indexer;

    let parsed = ruff_python_parser::parse_unchecked_source(source, source_type);
    if parsed.has_syntax_errors() {
        return None;
    }
    let indexer = Indexer::from_tokens(parsed.tokens(), source);
    if !indexer.comment_ranges().is_empty() {
        return None;
    }
    let imports: Vec<&ruff_python_ast::Stmt> = parsed.suite().iter().collect();
    if imports
        .iter()
        .any(|statement| !(statement.is_import_stmt() || statement.is_import_from_stmt()))
    {
        return None;
    }
    let block = super::block::Block {
        nested: false,
        imports,
        trailer: None,
    };
    let locator = crate::Locator::new(source);
    let own_style;
    let stylist = match stylist {
        Some(stylist) => stylist,
        None => {
            own_style = Stylist::from_tokens(parsed.tokens(), source);
            &own_style
        }
    };
    Some(super::format_imports(
        &block,
        Vec::new(),
        &locator,
        settings.line_length,
        crate::line_width::LineWidthBuilder::new(settings.tab_size),
        stylist,
        src,
        package,
        source_type,
        target_version,
        &settings.isort,
        parsed.tokens(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ruff_python_ast::PythonVersion;
    use rustc_hash::{FxHashMap, FxHashSet};

    use super::{ImportOrder, SortedImport};
    use crate::packaging::HostPackageFiles;
    use crate::rules::isort::categorize::{ImportSection, ImportType, KnownModules};
    use crate::rules::isort::settings::Settings;
    use crate::settings::LinterSettings;
    use crate::settings::types::IdentifierPattern;

    /// the order `settings` sort with, with no `src` roots for a module to be found in
    fn order(settings: Settings) -> ImportOrder<HostPackageFiles> {
        ImportOrder::new(
            Arc::new(LinterSettings {
                isort: settings,
                src: Vec::new(),
                ..LinterSettings::default()
            }),
            None,
            PythonVersion::PY313,
            HostPackageFiles,
        )
    }

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

    /// sorts `imports` by position and returns them as statements
    fn sorted(order: &ImportOrder<HostPackageFiles>, imports: &[SortedImport]) -> Vec<String> {
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
        let order = order(Settings::default());
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
        let order = order(Settings {
            force_sort_within_sections: true,
            ..Settings::default()
        });
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
        let order = order(Settings {
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
        });
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
        let order = order(Settings::default());
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
