//! Declaring a dependency that an import needs but the project does not have.
//!
//! This is offered as a code action rather than as a fix on the diagnostic
//! because the edit lands in `pyproject.toml` — a different file to the one the
//! diagnostic is in, and not a Python file at all, which is what every fix
//! applier in the tree assumes it is rewriting.

use ruff_db::files::{File, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast as ast;
use ruff_python_ast::find_node::covering_node;
use ruff_text_size::{Ranged, TextRange, TextSize};
use toml_edit::{Array, DocumentMut, Item, Table, Value};
use ty_module_resolver::{DistributionName, ModuleName, resolve_module};
use ty_project::Db;
use ty_python_semantic::dependencies::{GroupName, available_groups};

use crate::code_action::{FileEdit, QuickFix};

/// The actions that declare the distribution an import at `range` needs.
///
/// Empty unless the project has a `pyproject.toml` to declare it in and the
/// import names something ty can attribute to a distribution.
pub(crate) fn code_actions(db: &dyn Db, file: File, range: TextRange) -> Vec<QuickFix> {
    let Some(module_name) = imported_module_name(db, file, range) else {
        return Vec::new();
    };
    let Some(distribution) = distribution_to_declare(db, file, &module_name) else {
        return Vec::new();
    };

    let manifest_path = db.project().root(db).join("pyproject.toml");
    let Ok(manifest) = system_path_to_file(db, &manifest_path) else {
        return Vec::new();
    };
    let source = source_text(db, manifest);

    // the main dependency list first: it is what an import from shipped code
    // needs, and the answer most of the time
    let mut targets = vec![Target::Project];
    if let Some(declared) = db.dependency_manifest(file) {
        for group in declared.groups() {
            match &group.name {
                GroupName::Project => {}
                GroupName::Extra(name) => targets.push(Target::Extra(name.to_string())),
                GroupName::Group(name) => targets.push(Target::Group(name.to_string())),
            }
        }
    }

    targets
        .into_iter()
        .filter_map(|target| {
            let updated = declare(&source, &target, distribution.as_str())?;
            let edit = minimal_edit(&source, &updated)?;
            Some(QuickFix {
                title: format!("Declare `{distribution}` in {}", target.describe()),
                edits: vec![FileEdit {
                    file: manifest,
                    edit,
                }],
                preferred: matches!(target, Target::Project),
                create: None,
            })
        })
        .collect()
}

/// The module an import statement covering `range` names.
fn imported_module_name(db: &dyn Db, file: File, range: TextRange) -> Option<ModuleName> {
    let parsed = parsed_module(db, file).load(db);
    let covering = covering_node(parsed.syntax().into(), range);

    // `import a.b` anchors the diagnostic on the alias, `from a.b import c` on
    // the module identifier
    let node = covering
        .find_first(|node| node.is_alias() || node.is_stmt_import_from())
        .ok()?;

    match node.node() {
        ast::AnyNodeRef::Alias(alias) => ModuleName::new(&alias.name),
        ast::AnyNodeRef::StmtImportFrom(import) => {
            // a relative import names nothing installed
            if import.level > 0 {
                return None;
            }
            ModuleName::new(import.module.as_ref()?)
        }
        _ => None,
    }
}

/// The distribution an import of `module_name` needs the project to declare.
///
/// `None` when there is nothing to declare: the import resolves to something the
/// project may already use, or to code no distribution installed.
fn distribution_to_declare(
    db: &dyn Db,
    file: File,
    module_name: &ModuleName,
) -> Option<DistributionName> {
    let root = ModuleName::new(module_name.first_component())?;

    match resolve_module(db, file, module_name) {
        Some(module) => {
            let owners = ty_module_resolver::distribution_index(db).owners_of(db, module);
            let available = available_groups(db, file);
            owners
                .iter()
                .find(|owner| !available.allows_distribution(owner))
                .cloned()
        }
        // nothing is installed under that name, so nothing can say what
        // distribution installs it. the module's own name is the best guess
        // there is, and it is right far more often than not
        None => Some(DistributionName::new(root.as_str())),
    }
}

/// A place in a `pyproject.toml` that a requirement can be declared.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Project,
    Extra(String),
    Group(String),
}

impl Target {
    fn describe(&self) -> String {
        match self {
            Target::Project => "the project's dependencies".to_string(),
            Target::Extra(name) => format!("extra `{name}`"),
            Target::Group(name) => format!("dependency group `{name}`"),
        }
    }
}

/// `source` with `name` declared in `target`, or `None` if it cannot be.
///
/// Format-preserving: everything but the inserted entry comes back byte for
/// byte, which is what lets the caller reduce it to an edit of the entry alone.
fn declare(source: &str, target: &Target, name: &str) -> Option<String> {
    let mut document: DocumentMut = source.parse().ok()?;
    let array = requirements_array(&mut document, target)?;

    if array
        .iter()
        .filter_map(Value::as_str)
        .any(|existing| declares(existing, name))
    {
        return None;
    }

    // an array written over several lines keeps being written that way
    let indent = multiline_indent(array);
    array.push(name);
    if let Some(indent) = indent
        && let Some(added) = array.iter_mut().last()
    {
        added.decor_mut().set_prefix(indent);
    }

    Some(document.to_string())
}

/// The array `target` names, adding the tables on the way to it if they are not
/// already there.
fn requirements_array<'a>(document: &'a mut DocumentMut, target: &Target) -> Option<&'a mut Array> {
    let table = |item: &'a mut Item| item.as_table_mut();
    let new_table = || Item::Table(Table::new());
    let new_array = || Item::Value(Value::Array(Array::new()));

    let item = match target {
        Target::Project => table(document.entry("project").or_insert_with(new_table))?
            .entry("dependencies")
            .or_insert_with(new_array),
        Target::Extra(extra) => {
            let project = table(document.entry("project").or_insert_with(new_table))?;
            // `[project]` is only on the way to the extras here. an empty one
            // that is implicit is not written out; one with keys of its own is
            project.set_implicit(true);
            table(
                project
                    .entry("optional-dependencies")
                    .or_insert_with(new_table),
            )?
            .entry(extra)
            .or_insert_with(new_array)
        }
        Target::Group(group) => table(
            document
                .entry("dependency-groups")
                .or_insert_with(new_table),
        )?
        .entry(group)
        .or_insert_with(new_array),
    };

    item.as_array_mut()
}

/// Whether the requirement string `requirement` is about the distribution `name`.
fn declares(requirement: &str, name: &str) -> bool {
    let declared = requirement
        .trim_start()
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .next()
        .unwrap_or_default();

    !declared.is_empty() && DistributionName::new(declared) == DistributionName::new(name)
}

/// The whitespace an array's entries are prefixed with, when it is written over
/// several lines.
fn multiline_indent(array: &Array) -> Option<String> {
    let last = array.iter().last()?;
    let prefix = last.decor().prefix()?.as_str()?;
    prefix.contains('\n').then(|| prefix.to_string())
}

/// The one edit that turns `before` into `after`.
///
/// `after` came out of a format-preserving rewrite, so it differs from `before`
/// only where the entry was added. Trimming the shared head and tail turns a
/// whole rewritten file into an edit an editor shows as the one-line change it
/// is.
fn minimal_edit(before: &str, after: &str) -> Option<Edit> {
    if before == after {
        return None;
    }

    let prefix = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(before, after)| before == after)
        .count();

    let shortest = before.len().min(after.len()) - prefix;
    let suffix = before
        .bytes()
        .rev()
        .zip(after.bytes().rev())
        .take_while(|(before, after)| before == after)
        .count()
        .min(shortest);

    // a byte offset inside a character would cut the string apart mid-encoding
    let prefix = floor_char_boundary(before, prefix);
    let suffix = before.len() - floor_char_boundary(before, before.len() - suffix);

    let replaced = TextRange::new(
        TextSize::try_from(prefix).ok()?,
        TextSize::try_from(before.len() - suffix).ok()?,
    );
    let inserted = &after[prefix..after.len() - suffix];

    Some(if inserted.is_empty() {
        Edit::range_deletion(replaced)
    } else if replaced.is_empty() {
        Edit::insertion(inserted.to_string(), replaced.start())
    } else {
        Edit::range_replacement(inserted.to_string(), replaced)
    })
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(source: &str, target: &Target, name: &str) -> Option<String> {
        declare(source, target, name)
    }

    #[test]
    fn adds_to_an_empty_project_table() {
        assert_eq!(
            declared("[project]\nname = \"mine\"\n", &Target::Project, "numpy").as_deref(),
            Some("[project]\nname = \"mine\"\ndependencies = [\"numpy\"]\n")
        );
    }

    #[test]
    fn adds_to_an_inline_array() {
        assert_eq!(
            declared(
                "[project]\ndependencies = [\"requests\"]\n",
                &Target::Project,
                "numpy"
            )
            .as_deref(),
            Some("[project]\ndependencies = [\"requests\", \"numpy\"]\n")
        );
    }

    #[test]
    fn a_multiline_array_stays_multiline() {
        assert_eq!(
            declared(
                "[project]\ndependencies = [\n    \"requests\",\n]\n",
                &Target::Project,
                "numpy"
            )
            .as_deref(),
            Some("[project]\ndependencies = [\n    \"requests\",\n    \"numpy\",\n]\n")
        );
    }

    #[test]
    fn comments_and_formatting_elsewhere_survive() {
        let source = "\
# a project
[project]
name = \"mine\"       # its name
dependencies = [\"requests\"]

[tool.ty]
respect-ignore-files = true
";
        let updated = declared(source, &Target::Project, "numpy").unwrap();
        assert!(updated.contains("# a project"));
        assert!(updated.contains("name = \"mine\"       # its name"));
        assert!(updated.contains("respect-ignore-files = true"));
        assert!(updated.contains("[\"requests\", \"numpy\"]"));
    }

    #[test]
    fn a_missing_table_is_created() {
        assert_eq!(
            declared("", &Target::Group("dev".to_string()), "pytest").as_deref(),
            Some("[dependency-groups]\ndev = [\"pytest\"]\n")
        );
        // `[project]` is only on the way to the extras, so it is not written out
        assert_eq!(
            declared("", &Target::Extra("cli".to_string()), "click").as_deref(),
            Some("[project.optional-dependencies]\ncli = [\"click\"]\n")
        );
    }

    #[test]
    fn an_existing_project_table_still_prints_when_an_extra_is_added() {
        assert_eq!(
            declared(
                "[project]\nname = \"mine\"\n",
                &Target::Extra("cli".to_string()),
                "click"
            )
            .as_deref(),
            Some("[project]\nname = \"mine\"\n\n[project.optional-dependencies]\ncli = [\"click\"]\n")
        );
    }

    #[test]
    fn an_existing_group_is_added_to() {
        assert_eq!(
            declared(
                "[dependency-groups]\ndev = [\"pytest\"]\n",
                &Target::Group("dev".to_string()),
                "ruff"
            )
            .as_deref(),
            Some("[dependency-groups]\ndev = [\"pytest\", \"ruff\"]\n")
        );
    }

    #[test]
    fn a_requirement_already_there_is_not_added_again() {
        for existing in ["numpy", "numpy>=2", "NumPy [extra] == 2.*", " numpy ; sys_platform"] {
            let source = format!("[project]\ndependencies = [\"{existing}\"]\n");
            assert_eq!(
                declared(&source, &Target::Project, "numpy"),
                None,
                "`{existing}` already declares numpy",
            );
        }
    }

    #[test]
    fn a_different_requirement_is_not_mistaken_for_it() {
        assert!(declared(
            "[project]\ndependencies = [\"numpy-financial\"]\n",
            &Target::Project,
            "numpy"
        )
        .is_some());
    }

    #[test]
    fn invalid_toml_declares_nothing() {
        assert_eq!(declared("[project", &Target::Project, "numpy"), None);
    }

    #[test]
    fn the_edit_covers_only_what_changed() {
        let before = "[project]\ndependencies = [\"requests\"]\n";
        let after = "[project]\ndependencies = [\"requests\", \"numpy\"]\n";
        let edit = minimal_edit(before, after).unwrap();

        assert_eq!(edit.content(), Some(", \"numpy\""));
        assert_eq!(
            &before[usize::from(edit.start())..usize::from(edit.end())],
            ""
        );
    }

    #[test]
    fn the_edit_is_none_when_nothing_changed() {
        assert!(minimal_edit("same", "same").is_none());
    }

    /// The actions offered for `reported` in `source`, as `(title, the
    /// `pyproject.toml` the action produces)` pairs.
    ///
    /// `reported` stands in for what the diagnostic is anchored on, which is the
    /// module name for both import forms.
    fn offered(manifest: &str, source: &str, reported: &str) -> Vec<(String, String)> {
        use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
        use ty_project::ProjectMetadata;

        let mut db = ty_project::TestDb::new(ProjectMetadata::new("test", SystemPathBuf::from("/")));
        db.init_program().unwrap();
        db.write_file("pyproject.toml", manifest).unwrap();
        db.write_file("main.py", source).unwrap();

        let file = system_path_to_file(&db, "main.py").unwrap();
        let start = source.find(reported).expect("`reported` should be in source");
        let range = TextRange::at(
            TextSize::try_from(start).unwrap(),
            TextSize::try_from(reported.len()).unwrap(),
        );

        code_actions(&db, file, range)
            .into_iter()
            .map(|action| {
                let edit = &action.edits[0];
                let mut applied = manifest.to_string();
                applied.replace_range(
                    usize::from(edit.edit.start())..usize::from(edit.edit.end()),
                    edit.edit.content().unwrap_or_default(),
                );
                (action.title, applied)
            })
            .collect()
    }

    #[test]
    fn an_unresolved_import_is_offered_its_guessed_name() {
        let manifest = "[project]\nname = \"mine\"\ndependencies = []\n";
        let offered = offered(manifest, "import numpy\n", "numpy");

        assert_eq!(
            offered,
            [(
                "Declare `numpy` in the project's dependencies".to_string(),
                "[project]\nname = \"mine\"\ndependencies = [\"numpy\"]\n".to_string()
            )]
        );
    }

    #[test]
    fn every_declared_group_is_offered() {
        let manifest = "\
[project]
name = \"mine\"
dependencies = []

[dependency-groups]
dev = []
";
        let titles: Vec<_> = offered(manifest, "import pytest\n", "pytest")
            .into_iter()
            .map(|(title, _)| title)
            .collect();

        assert_eq!(
            titles,
            [
                "Declare `pytest` in the project's dependencies",
                "Declare `pytest` in dependency group `dev`",
            ]
        );
    }

    #[test]
    fn a_relative_import_is_offered_nothing() {
        let manifest = "[project]\nname = \"mine\"\n";
        assert!(offered(manifest, "from . import sibling\n", "from . import sibling").is_empty());
    }

    #[test]
    fn nothing_is_offered_without_a_manifest_file() {
        use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
        use ty_project::ProjectMetadata;

        let mut db =
            ty_project::TestDb::new(ProjectMetadata::new("test", SystemPathBuf::from("/")));
        db.init_program().unwrap();
        db.write_file("main.py", "import numpy\n").unwrap();

        let file = system_path_to_file(&db, "main.py").unwrap();
        assert!(code_actions(&db, file, TextRange::new(0.into(), 12.into())).is_empty());
    }

    #[test]
    fn the_edit_never_splits_a_character() {
        let before = "[project]\nname = \"é\"\ndependencies = [\"é\"]\n";
        let after = "[project]\nname = \"é\"\ndependencies = [\"é\", \"numpy\"]\n";
        let edit = minimal_edit(before, after).unwrap();

        let mut applied = before.to_string();
        applied.replace_range(
            usize::from(edit.start())..usize::from(edit.end()),
            edit.content().unwrap_or_default(),
        );
        assert_eq!(applied, after);
    }
}
