//! Keeping a project's imports pointing at a module that is about to move.
//!
//! An editor that renames `src/alpha/util.py` to `src/alpha/helpers.py` has renamed the module
//! `alpha.util`, and every `from alpha.util import thing` in the project now names a module that
//! is not there. Finding those is not something an editor can do for itself — it would have to
//! resolve every import in the project against the same search paths the type checker uses — so
//! LSP has the client ask before it moves anything (`workspace/willRenameFiles`) and the server
//! answers with the edits that keep the project working.
//!
//! # What is edited
//!
//! The module paths written in import statements, in every file of the project:
//!
//! ```py
//! import alpha.util              # the dotted name
//! import alpha.util as util      # ... with an alias, which is unaffected
//! from alpha.util import thing   # the module a symbol is imported from
//! from alpha import util         # the module imported as a name
//! from .util import thing        # a relative import, when the new name can still be written
//!                                # relative to the importing file's own package
//! ```
//!
//! and, when one of those imports *binds* a name that changes — `import alpha.util` binds `alpha`,
//! and `from alpha import util` binds `util` — the uses of that name in the same file. Those are
//! found by asking the type checker what each expression is rather than by matching text: `util` in
//! one file may be the module and in another a local variable that happens to share its name, and
//! only one of them is a reference to what moved.
//!
//! # What is not
//!
//! - **Module names written as strings** — `importlib.import_module("alpha.util")`, a Django
//!   `INSTALLED_APPS` entry, a `pyproject.toml` entry point. The Django ones have their own answer
//!   in [`crate::django_template`]; the rest are not distinguishable from any other string.
//! - **An import that would have to change shape.** `from alpha import util` can be rewritten while
//!   `util` is still a submodule of `alpha`; a move that puts it under a different package needs a
//!   different statement, and rewriting one import statement into another is a refactor rather than
//!   a repair. Those are left alone rather than half-done — see [`ModuleRenameEdits::skipped`].
//! - **Relative imports whose target leaves the importing file's package.** Same reason: the dots
//!   no longer reach it, and only a different statement would.
//!
//! Everything left alone is reported, so the client can tell the user which files it could not fix
//! rather than leaving them to find out from a stack trace.

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::system::SystemPathBuf;
use ruff_diagnostics::Edit;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_stmt};
use ruff_python_ast::{self as ast};
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::{ImportingFile, ModuleName, path_to_module_name};
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::types::Type;
use ty_python_semantic::{HasType, SemanticModel};

use crate::code_action::FileEdit;

/// A file or directory the client is about to move, and where it is going.
///
/// Both paths are absolute. The old one is where the thing still is when this is asked — the
/// request arrives *before* the move — and the new one is where nothing is yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMove {
    pub old_path: SystemPathBuf,
    pub new_path: SystemPathBuf,
}

/// What a set of moves costs the project.
#[derive(Debug, Default)]
pub struct ModuleRenameEdits {
    /// The edits to apply, at most one per range, grouped by nothing in particular.
    pub edits: Vec<FileEdit>,

    /// Imports that name something being moved and that this could not rewrite.
    ///
    /// Reported rather than dropped: an import left pointing at a module that has gone is a broken
    /// file, and the difference between "the rename fixed everything" and "the rename fixed
    /// everything except these two lines" is the difference between a working project and half an
    /// hour of confusion.
    pub skipped: Vec<SkippedImport>,
}

/// An import naming something that moved, and why it was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedImport {
    pub file: File,
    pub range: TextRange,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The module ends up under a different parent, so `from <parent> import <name>` no longer
    /// reaches it and only a different statement would.
    NeedsDifferentStatement,
    /// A relative import whose target is no longer reachable from the importing file's package.
    NoLongerRelative,
}

/// The edits that keep the project's imports working when `moves` happen.
///
/// Returns nothing when no move changes a module's name — moving a file the search paths do not
/// cover, or renaming a directory that is not a package, is not a rename of anything importable.
pub fn module_rename_edits(db: &dyn Db, moves: &[FileMove]) -> ModuleRenameEdits {
    let environment = db.project().program(db).resolver_environment(db);

    let renamings: Vec<Renaming> = moves
        .iter()
        .filter_map(|file_move| {
            let old = path_to_module_name(db, environment, &file_move.old_path)?;
            let new = path_to_module_name(db, environment, &file_move.new_path)?;
            (old != new).then_some(Renaming { old, new })
        })
        .collect();

    if renamings.is_empty() {
        return ModuleRenameEdits::default();
    }

    let mut result = ModuleRenameEdits::default();
    // Serial rather than the parallel walk `workspace_symbols` uses: this runs once, from a
    // deliberate gesture, and it has to produce a stable order — the client applies these as one
    // edit and a set of edits that arrives in a different order on every run is one nobody can
    // review or test.
    let mut files: Vec<File> = db.project().files(db).iter().copied().collect();
    files.sort_by_key(|file| file.path(db).as_str().to_string());

    for file in files {
        collect_edits_for_file(db, file, &renamings, &mut result);
    }

    result
}

/// One module moving to another name, and everything under it moving with it.
struct Renaming {
    old: ModuleName,
    new: ModuleName,
}

impl Renaming {
    /// What `name` becomes, or `None` when this renaming does not cover it.
    ///
    /// A package takes its submodules with it: renaming `alpha` to `beta` renames `alpha.util` to
    /// `beta.util` without anybody saying so, which is why this is a prefix rewrite rather than an
    /// equality check.
    fn apply(&self, name: &ModuleName) -> Option<ModuleName> {
        if name == &self.old {
            return Some(self.new.clone());
        }
        let rest = name.relative_to(&self.old)?;
        let mut renamed = self.new.clone();
        renamed.extend(&rest);
        Some(renamed)
    }
}

/// The new name for `name` under any of `renamings`.
fn renamed(renamings: &[Renaming], name: &ModuleName) -> Option<ModuleName> {
    renamings.iter().find_map(|renaming| renaming.apply(name))
}

fn collect_edits_for_file(
    db: &dyn Db,
    file: File,
    renamings: &[Renaming],
    result: &mut ModuleRenameEdits,
) {
    let program_file = db.program_file(file);
    let parsed = parsed_module(db, program_file.python_file(db));
    let module = parsed.load(db);

    let mut imports = Imports::default();
    imports.visit_body(&module.syntax().body);
    if imports.is_empty() {
        return;
    }

    let importing_file =
        ImportingFile::File(file, db.project().program(db).resolver_environment(db));

    // The names this file binds to a module whose name changes. Collected while the import
    // statements are rewritten and used afterwards, because a use of `alpha` in the body can only
    // be judged once it is known that this file's `import alpha` was one of the imports rewritten.
    let mut rebound: Vec<Rebinding> = Vec::new();

    for import in &imports.plain {
        for alias in &import.names {
            let Some(name) = ModuleName::new(alias.name.as_str()) else {
                continue;
            };
            let Some(new_name) = renamed(renamings, &name) else {
                continue;
            };
            result.edits.push(FileEdit {
                file,
                edit: Edit::range_replacement(new_name.as_str().to_string(), alias.name.range()),
            });
            // `import alpha.util` binds `alpha`, and every use of it in the body is written
            // `alpha.util.thing`; with an `as` the binding is the alias and nothing else changes.
            if alias.asname.is_none() {
                rebound.push(Rebinding {
                    spelling: name.clone(),
                    module: name,
                    replacement: new_name.as_str().to_string(),
                });
            }
        }
    }

    for import in &imports.from {
        collect_edits_for_import_from(
            db,
            file,
            importing_file,
            renamings,
            import,
            result,
            &mut rebound,
        );
    }

    if !rebound.is_empty() {
        collect_edits_for_uses(db, file, program_file, &module, &rebound, result);
    }
}

/// A name this file binds that is about to mean something else.
///
/// Three separate facts, because the name as *written* and the module it *is* are not the same
/// string: `from alpha import util` writes `util` and means `alpha.util`. The written form is what
/// the body spells and what has to be matched there; the module is what the type checker will say
/// the expression is; and the replacement is the written form's new spelling, which for that
/// statement is one component and for `import alpha.util` is the whole dotted name.
struct Rebinding {
    /// How the body spells it — `util`, or `alpha.util`.
    spelling: ModuleName,
    /// The module it refers to, absolute, which is what its type will name.
    module: ModuleName,
    /// What the body should spell instead.
    replacement: String,
}

fn collect_edits_for_import_from(
    db: &dyn Db,
    file: File,
    importing_file: ImportingFile<'_>,
    renamings: &[Renaming],
    import: &ast::StmtImportFrom,
    result: &mut ModuleRenameEdits,
    rebound: &mut Vec<Rebinding>,
) {
    // The module the statement imports *from*, as an absolute name. Relative imports are resolved
    // against the importing file, which is the whole reason this needs the resolver rather than the
    // text of the statement.
    let Ok(from) = ModuleName::from_import_statement(db, importing_file, import) else {
        return;
    };

    if let Some(new_from) = renamed(renamings, &from) {
        match rewritten_module_reference(db, importing_file, renamings, import, &new_from) {
            Ok(Some(edit)) => result.edits.push(edit),
            // Nothing to rewrite: either the statement has no module text at all (`from . import x`)
            // or the text it has still spells the right thing, which is the ordinary outcome for a
            // relative import inside a package that is moving as a whole.
            Ok(None) => {}
            Err(reason) => result.skipped.push(SkippedImport {
                file,
                range: import.range(),
                reason,
            }),
        }
    }

    // `from alpha import util` names a module in its *alias* rather than in its module path, and
    // that is the form the rename of a leaf module usually meets.
    for alias in &import.names {
        let Some(alias_name) = ModuleName::new(alias.name.as_str()) else {
            continue;
        };
        let mut imported = from.clone();
        imported.extend(&alias_name);
        let Some(new_imported) = renamed(renamings, &imported) else {
            continue;
        };
        // Only the last component may change here: the rest of the name is written in the `from`,
        // which the loop above has already dealt with if it moved too.
        let Some(new_parent) = new_imported.parent() else {
            continue;
        };
        let new_from = renamed(renamings, &from).unwrap_or_else(|| from.clone());
        if new_parent != new_from {
            result.skipped.push(SkippedImport {
                file,
                range: alias.range(),
                reason: SkipReason::NeedsDifferentStatement,
            });
            continue;
        }
        // The last component is often untouched — renaming the package `alpha` to `beta` leaves
        // `from beta import util` spelling `util` exactly as it did — and an edit that replaces a
        // name with itself is noise in a diff the user is about to be shown.
        if new_imported.last_component() == alias.name.as_str() {
            continue;
        }
        result.edits.push(FileEdit {
            file,
            edit: Edit::range_replacement(
                new_imported.last_component().to_string(),
                alias.name.range(),
            ),
        });
        if alias.asname.is_none() {
            rebound.push(Rebinding {
                spelling: alias_name,
                module: imported,
                replacement: new_imported.last_component().to_string(),
            });
        }
    }
}

/// The edit that makes `import` name `new_from`, or the reason it cannot.
///
/// Absolute imports are a straight replacement of the module text. A relative one is only
/// rewritable while the new name is still under the package the dots reach: `from .util import x`
/// in `alpha/main.py` can become `from .helpers import x`, but nothing that starts with a dot can
/// name a module that has left `alpha`.
fn rewritten_module_reference(
    db: &dyn Db,
    importing_file: ImportingFile<'_>,
    renamings: &[Renaming],
    import: &ast::StmtImportFrom,
    new_from: &ModuleName,
) -> Result<Option<FileEdit>, SkipReason> {
    let file = importing_file.file(db);
    let Some(module) = import.module.as_ref() else {
        // `from . import x` / `from .. import x`: the dots are the whole reference.
        return Ok(None);
    };

    if import.level == 0 {
        return Ok(Some(FileEdit {
            file,
            edit: Edit::range_replacement(new_from.as_str().to_string(), module.range()),
        }));
    }

    // What the dots resolve to, which is what the text after them is relative to.
    let Ok(base) = ModuleName::from_identifier_parts(db, importing_file, None, import.level) else {
        return Err(SkipReason::NoLongerRelative);
    };
    // The dots go on meaning "this file's own package", and that package moves when the file moves
    // with it. So a relative import inside a package that is being renamed as a whole is measured
    // against where the package is going, and comes out unchanged — which is the truth: nothing
    // about `from .util import thing` stops working because its package was renamed around it.
    let new_base = renamed(renamings, &base).unwrap_or(base);
    match new_from.relative_to(&new_base) {
        Some(tail) if tail.as_str() == module.as_str() => Ok(None),
        Some(tail) => Ok(Some(FileEdit {
            file,
            edit: Edit::range_replacement(tail.as_str().to_string(), module.range()),
        })),
        None => Err(SkipReason::NoLongerRelative),
    }
}

/// The uses, in this file, of a name that an import statement bound to a module that moved.
///
/// `import alpha.util` binds `alpha`, so `alpha.util.thing()` in the body has to become
/// `beta.util.thing()` when `alpha` moves. The candidates are found by their text and confirmed by
/// their type: an expression is only rewritten when the type checker says it *is* the module that
/// moved, which is what keeps a local variable called `util` from being rewritten in a file that
/// also imports a module of that name.
fn collect_edits_for_uses(
    db: &dyn Db,
    file: File,
    program_file: ProgramFile<'_>,
    module: &ruff_db::parsed::ParsedModuleRef,
    rebound: &[Rebinding],
    result: &mut ModuleRenameEdits,
) {
    let model = SemanticModel::new(db, program_file);

    let mut uses = ModuleUses {
        rebound,
        candidates: Vec::new(),
    };
    uses.visit_body(&module.syntax().body);

    for (expression, rebinding) in uses.candidates {
        let Some(Type::ModuleLiteral(literal)) = expression.inferred_type(&model) else {
            continue;
        };
        // The expression's own text says which module it *reads* as; the type says which module it
        // is. Both have to agree, or this is a use of something else that happens to be spelled the
        // same — a package whose `__init__` re-exports a submodule of another name, most obviously.
        if literal.module(db).name(db) != &rebinding.module {
            continue;
        }
        result.edits.push(FileEdit {
            file,
            edit: Edit::range_replacement(rebinding.replacement.clone(), expression.range()),
        });
    }
}

/// Every import statement in a file, at any depth.
///
/// Imports are not only at the top of a file: they sit inside `if TYPE_CHECKING:`, inside `try:`
/// blocks that fall back to another package, and inside functions that defer an expensive one. A
/// rename that only rewrote the top-level ones would leave exactly the imports that were written
/// carefully.
#[derive(Default)]
struct Imports<'a> {
    plain: Vec<&'a ast::StmtImport>,
    from: Vec<&'a ast::StmtImportFrom>,
}

impl Imports<'_> {
    fn is_empty(&self) -> bool {
        self.plain.is_empty() && self.from.is_empty()
    }
}

impl<'a> SourceOrderVisitor<'a> for Imports<'a> {
    fn visit_stmt(&mut self, stmt: &'a ast::Stmt) {
        match stmt {
            ast::Stmt::Import(import) => self.plain.push(import),
            ast::Stmt::ImportFrom(import) => self.from.push(import),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

/// The expressions that spell one of the rebound module names.
///
/// Text first, type second: asking the type checker about every expression in a file would type the
/// whole file to answer a question about the handful of expressions that could possibly be affected.
struct ModuleUses<'a, 'r> {
    rebound: &'r [Rebinding],
    candidates: Vec<(ModuleUse<'a>, &'r Rebinding)>,
}

/// An expression that reads as a dotted module name.
#[derive(Debug, Clone, Copy)]
enum ModuleUse<'a> {
    Name(&'a ast::ExprName),
    Attribute(&'a ast::ExprAttribute),
}

impl ModuleUse<'_> {
    fn range(self) -> TextRange {
        match self {
            ModuleUse::Name(name) => name.range(),
            ModuleUse::Attribute(attribute) => attribute.range(),
        }
    }

    fn inferred_type<'db>(self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
        match self {
            ModuleUse::Name(name) => name.inferred_type(model),
            ModuleUse::Attribute(attribute) => attribute.inferred_type(model),
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for ModuleUses<'a, '_> {
    fn visit_expr(&mut self, expr: &'a ast::Expr) {
        // The longest dotted prefix wins, and its subexpressions are not visited: rewriting both
        // `alpha` and `alpha.util` inside `alpha.util.thing` would produce two overlapping edits of
        // the same text.
        if let Some((candidate, rebinding)) = self.candidate(expr) {
            self.candidates.push((candidate, rebinding));
            return;
        }
        ruff_python_ast::visitor::source_order::walk_expr(self, expr);
    }
}

impl<'a, 'r> ModuleUses<'a, 'r> {
    fn candidate(&self, expr: &'a ast::Expr) -> Option<(ModuleUse<'a>, &'r Rebinding)> {
        let (use_, spelling) = match expr {
            ast::Expr::Name(name) => (ModuleUse::Name(name), name.id.to_string()),
            ast::Expr::Attribute(attribute) => (ModuleUse::Attribute(attribute), dotted(expr)?),
            _ => return None,
        };
        let spelled = ModuleName::new(&spelling)?;
        let rebinding = self
            .rebound
            .iter()
            .find(|rebinding| rebinding.spelling == spelled)?;
        Some((use_, rebinding))
    }
}

/// The dotted name an expression spells, when it is one: `alpha.util` from `alpha.util`, and
/// nothing at all from `f().util` or `alpha[0].util`.
fn dotted(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Name(name) => Some(name.id.to_string()),
        ast::Expr::Attribute(attribute) => {
            let mut base = dotted(&attribute.value)?;
            base.push('.');
            base.push_str(attribute.attr.as_str());
            Some(base)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{CursorTest, cursor_test};
    use insta::assert_snapshot;
    use ruff_db::source::source_text;
    use std::collections::BTreeMap;
    use std::fmt::Write;

    impl CursorTest {
        /// The project after `old` is renamed to `new`, showing only the files that changed.
        ///
        /// The edits are applied rather than listed, because what matters about a rename is the
        /// import line it leaves behind — a list of ranges and replacement strings is a puzzle the
        /// reader has to solve before they can see whether the answer is right.
        fn rename_module(&self, old: &str, new: &str) -> String {
            let result = salsa::attach(&self.db, || {
                module_rename_edits(
                    &self.db,
                    &[FileMove {
                        old_path: SystemPathBuf::from(old),
                        new_path: SystemPathBuf::from(new),
                    }],
                )
            });

            let mut by_file: BTreeMap<String, (File, Vec<&Edit>)> = BTreeMap::new();
            for file_edit in &result.edits {
                by_file
                    .entry(file_edit.file.path(&self.db).as_str().to_string())
                    .or_insert_with(|| (file_edit.file, Vec::new()))
                    .1
                    .push(&file_edit.edit);
            }

            let mut rendered = String::new();
            for (path, (file, mut edits)) in by_file {
                let mut text = source_text(&self.db, file).as_str().to_string();
                // Back to front, so an earlier edit's replacement cannot move a later one's range.
                edits.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
                for edit in edits {
                    text.replace_range(
                        usize::from(edit.start())..usize::from(edit.end()),
                        edit.content().unwrap_or_default(),
                    );
                }
                let _ = writeln!(rendered, "--- {path}\n{}", text.trim_end());
            }

            for skipped in &result.skipped {
                let _ = writeln!(
                    rendered,
                    "!!! {} {:?} {:?}",
                    skipped.file.path(&self.db),
                    skipped.range,
                    skipped.reason
                );
            }

            if rendered.is_empty() {
                "no edits".to_string()
            } else {
                rendered
            }
        }
    }

    /// The commonest form by far, and the one with nothing else to think about: the module is named
    /// in the `from`, and the names the statement binds are symbols rather than the module.
    #[test]
    fn from_import_follows_a_renamed_module() {
        let test = CursorTest::builder()
            .source("alpha/__init__.py", "")
            .source("alpha/util.py", "def thing(): ...")
            .source(
                "main.py",
                "\
from alpha.util import thing

thing()<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.rename_module("/alpha/util.py", "/alpha/helpers.py"), @"
        --- /main.py
        from alpha.helpers import thing

        thing()
        ");
    }

    /// `import alpha.util` binds `alpha`, not `alpha.util`, so the statement is only half the job:
    /// every use in the body spells the module out again and has to move with it.
    #[test]
    fn plain_import_and_its_uses_follow_a_renamed_module() {
        let test = CursorTest::builder()
            .source("alpha/__init__.py", "")
            .source("alpha/util.py", "def thing(): ...")
            .source(
                "main.py",
                "\
import alpha.util

alpha.util.thing()
print(alpha.util)<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.rename_module("/alpha/util.py", "/alpha/helpers.py"), @"
        --- /main.py
        import alpha.helpers

        alpha.helpers.thing()
        print(alpha.helpers)
        ");
    }

    /// A renamed package takes everything under it, without any of those modules being named.
    #[test]
    fn renaming_a_package_renames_the_modules_inside_it() {
        let test = CursorTest::builder()
            .source("alpha/__init__.py", "")
            .source("alpha/util.py", "def thing(): ...")
            .source("alpha/deep/__init__.py", "")
            .source("alpha/deep/inner.py", "value = 1")
            .source(
                "main.py",
                "\
from alpha.util import thing
from alpha.deep.inner import value
from alpha import util
import alpha.util

thing()
print(value, util, alpha.util)<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.rename_module("/alpha", "/beta"), @"
        --- /main.py
        from beta.util import thing
        from beta.deep.inner import value
        from beta import util
        import beta.util

        thing()
        print(value, util, beta.util)
        ");
    }

    /// With an alias, the name the body uses is the alias, which the move does not touch.
    #[test]
    fn an_alias_is_left_alone() {
        let test = CursorTest::builder()
            .source("alpha/__init__.py", "")
            .source("alpha/util.py", "def thing(): ...")
            .source(
                "main.py",
                "\
import alpha.util as u

u.thing()<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.rename_module("/alpha/util.py", "/alpha/helpers.py"), @"
        --- /main.py
        import alpha.helpers as u

        u.thing()
        ");
    }

    /// A relative import keeps its dots and changes only the part after them, so long as the module
    /// is still inside the package they reach.
    #[test]
    fn a_relative_import_is_rewritten_after_the_dots() {
        let test = CursorTest::builder()
            .source("alpha/__init__.py", "")
            .source("alpha/util.py", "def thing(): ...")
            .source(
                "alpha/main.py",
                "\
from .util import thing
from . import util

thing()
util.thing()<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.rename_module("/alpha/util.py", "/alpha/helpers.py"), @"
        --- /alpha/main.py
        from .helpers import thing
        from . import helpers

        thing()
        helpers.thing()
        ");
    }

    /// The check that stops this being a search and replace: the local `util` is a string that
    /// happens to be spelled like the module, and renaming it would change what the function means.
    #[test]
    fn a_local_that_shadows_the_module_is_not_touched() {
        let test = CursorTest::builder()
            .source("alpha/__init__.py", "")
            .source("alpha/util.py", "def thing(): ...")
            .source(
                "main.py",
                "\
from alpha import util

def shadowed():
    util = \"not the module\"
    return util.upper()

util.thing()<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.rename_module("/alpha/util.py", "/alpha/helpers.py"), @r#"
        --- /main.py
        from alpha import helpers

        def shadowed():
            util = "not the module"
            return util.upper()

        helpers.thing()
        "#);
    }

    /// Moving a module to another package is not a rename any single import statement can express:
    /// `from alpha import util` would have to become a different statement. Reported rather than
    /// rewritten into something that does not mean the same thing.
    #[test]
    fn an_import_that_would_need_a_different_statement_is_reported() {
        let test = CursorTest::builder()
            .source("alpha/__init__.py", "")
            .source("alpha/util.py", "def thing(): ...")
            .source("beta/__init__.py", "")
            .source(
                "main.py",
                "\
from alpha import util

util.thing()<CURSOR>
",
            )
            .build();

        assert_snapshot!(test.rename_module("/alpha/util.py", "/beta/util.py"), @"!!! /main.py 18..22 NeedsDifferentStatement");
    }

    /// Nothing to do for a file the search paths do not cover: it is not a module, so no import can
    /// be naming it.
    #[test]
    fn moving_something_that_is_not_a_module_costs_nothing() {
        let test = cursor_test(
            "\
x = 1<CURSOR>
",
        );

        assert_snapshot!(test.rename_module("/notes.md", "/notes-old.md"), @"no edits");
    }

    /// A move that leaves the name alone — a directory renamed to itself, a file moved between two
    /// paths that spell the same module — is not a rename of anything.
    #[test]
    fn a_move_that_does_not_change_the_name_costs_nothing() {
        let test = cursor_test(
            "\
x = 1<CURSOR>
",
        );

        assert_snapshot!(test.rename_module("/alpha/util.py", "/alpha/util.py"), @"no edits");
    }
}
