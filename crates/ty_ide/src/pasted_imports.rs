//! The imports copied code depends on, and the ones it is missing where it is pasted.
//!
//! Code copied out of one module and pasted into another loses the imports it was
//! written against: a function that calls `Path(...)` under `from pathlib import Path`
//! is a function with an unresolved `Path` once it lands in a module without that
//! import. So an editor asks two questions. [`copied_imports`] answers which of the
//! names the copied text uses are bound by the global scope of the file it came from,
//! where it came from, and how to bind each of them again elsewhere. [`pasted_imports`]
//! answers which of those the file the text landed in does not bind where it landed,
//! and the edits that import them.
//!
//! The first is asked about the copied text, with the place it was copied from, rather
//! than about a range of the file: cutting a function is an edit that takes the text
//! out of the file, and the question is only worth asking once the paste has happened.
//! Its answer describes each import rather than pointing into the source file, so the
//! second question can be asked about another file.

use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, ExprContext, Stmt};
use ruff_python_parser::{ParseOptions, parse_unchecked};
use ruff_python_trivia::textwrap::dedent;
use ruff_text_size::{TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_module_resolver::{ImportingFile, ModuleName, file_to_module};
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_semantic::SemanticModel;

use ty_python_semantic::importer::ImportRequest;

use crate::import_order;

/// A name copied code uses, and the import that binds it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CopiedImport {
    /// The name the copied code uses.
    pub name: String,
    /// The module to import, by its absolute name: a relative import in the file the
    /// code was copied from means nothing from anywhere else.
    pub module: String,
    /// What to import from [`Self::module`], when the name is bound to a member of it
    /// rather than to the module itself. A member imported under another name keeps
    /// its own name here, and [`Self::name`] is the alias.
    pub member: Option<String>,
}

impl CopiedImport {
    /// How [`Importer::import`] is asked for this import.
    fn request(&self) -> ImportRequest<'_> {
        match &self.member {
            Some(member) if *member == self.name => {
                ImportRequest::import_from(&self.module, member).force()
            }
            Some(member) => ImportRequest::import_from_as(&self.module, member, &self.name).force(),
            // `import a.b` binds `a`, so the import that binds `a` again is `import a.b`,
            // which keeps `a.b` an attribute the copied code can read
            None if self.module.split('.').next() == Some(self.name.as_str()) => {
                ImportRequest::module(&self.module)
            }
            None => ImportRequest::module_as(&self.module, &self.name),
        }
    }
}

/// Code copied out of a file: its text, and the offset in the file it was copied from.
#[derive(Debug, Clone, Copy)]
pub struct CopiedCode<'a> {
    pub text: &'a str,
    pub at: TextSize,
}

/// The names `copies` use that `file`'s global scope binds where each was copied from,
/// and the import that binds each of them again in another module.
///
/// The copied text is read on its own, not out of `file`: what a cut takes is no
/// longer in the file by the time anything is asked about it, and what a copy took
/// may have been edited since. It is the scope at [`CopiedCode::at`] that says what
/// each name meant, and a cut leaves that scope's bindings where they were.
///
/// A name bound by an import is imported the same way, from the module that import
/// names. A name `file` defines itself — a function, a class, a module-level
/// variable — is imported from `file`'s own module, when `file` is a module anything
/// can import. Names the copied code binds itself travel with it, and names of any
/// scope but the global one, or the builtins, cannot be imported, so neither is
/// answered. Each name is answered once, in the order the code first uses it.
pub fn copied_imports(
    db: &dyn Db,
    file: ProgramFile<'_>,
    copies: &[CopiedCode<'_>],
) -> Vec<CopiedImport> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let source = source_text(db, file.file(db));
    let model = SemanticModel::new(db, file);
    let importing_file = ImportingFile::File(file.file(db), file.resolver_environment(db));
    let own_module = file_to_module(db, file.resolver_file(db))
        .map(|module| module.name(db).as_str().to_string());
    let options = ParseOptions::from(file.file(db).source_type(db)).with_basedpython(matches!(
        file.file(db).path(db).extension(),
        Some("by" | "byi")
    ));

    let mut seen = FxHashSet::default();
    let mut imports = Vec::new();
    for copy in copies {
        let Some(at) = (copy.at <= TextSize::of(source.as_str())).then_some(copy.at) else {
            continue;
        };
        let snippet = snippet_source(source.as_str(), at, copy.text);
        let snippet = parse_unchecked(&snippet, options.clone());
        let mut names = SnippetNames::default();
        names.visit_mod(snippet.syntax());

        // what each name meant where the code was copied from: the bindings of every scope
        // around that place, innermost first, so that a local shadows the global it names
        let scope = covering_node(parsed.syntax().into(), TextRange::empty(at)).node();
        let members = model.members_in_scope_at(scope);

        for name in names.read {
            if names.bound.contains(&name) || !seen.insert(name.clone()) {
                continue;
            }
            let Some(member) = members.get(name.as_str()) else {
                continue;
            };
            let definition = member.first_reachable_definition;
            if definition.file(db) != file.file(db) || !definition.file_scope(db).is_global() {
                continue;
            }
            imports.extend(import_of(
                db,
                importing_file,
                &parsed,
                own_module.as_deref(),
                &name,
                definition,
            ));
        }
    }
    imports
}

/// The import that binds `name` as `definition`, a global binding of the file `parsed` is,
/// binds it.
fn import_of(
    db: &dyn Db,
    importing_file: ImportingFile<'_>,
    parsed: &ParsedModuleRef,
    own_module: Option<&str>,
    name: &str,
    definition: Definition<'_>,
) -> Option<CopiedImport> {
    match definition.kind(db) {
        DefinitionKind::Import(import) => {
            let alias = import.alias(parsed);
            Some(CopiedImport {
                name: name.to_string(),
                module: alias.name.to_string(),
                member: None,
            })
        }
        DefinitionKind::ImportFrom(import) => {
            absolute_module(db, importing_file, import.import(parsed)).map(|module| CopiedImport {
                name: name.to_string(),
                module,
                member: Some(import.alias(parsed).name.to_string()),
            })
        }
        DefinitionKind::StarImport(import) => {
            absolute_module(db, importing_file, import.import(parsed)).map(|module| CopiedImport {
                name: name.to_string(),
                module,
                member: Some(name.to_string()),
            })
        }
        // `from a.b import c` also binds `a` in `a/__init__`, which is not a name code
        // outside that package reads
        DefinitionKind::ImportFromSubmodule(_) => None,
        _ => own_module.map(|module| CopiedImport {
            name: name.to_string(),
            module: module.to_string(),
            member: Some(name.to_string()),
        }),
    }
}

/// `text`, copied from `at` in `source`, as source that parses on its own.
///
/// A copy usually starts at the first character of a line's code, after the
/// indentation, and takes whole lines after that, each indented as it was: `return
/// x\n        y = 1`. Read alone that is an unexpected indent, so the copy is given back
/// the indentation its first line had, as spaces, and then the whole is dedented.
fn snippet_source(source: &str, at: TextSize, text: &str) -> String {
    let line_start = source[..at.to_usize()]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let before = &source[line_start..at.to_usize()];
    let indent = if before.chars().all(char::is_whitespace) {
        before.to_string()
    } else {
        " ".repeat(before.chars().count())
    };
    dedent(&format!("{indent}{text}")).into_owned()
}

/// The names copied code reads, in the order it reads them, and the names it binds.
#[derive(Default)]
struct SnippetNames {
    read: Vec<String>,
    bound: FxHashSet<String>,
}

impl<'a> SourceOrderVisitor<'a> for SnippetNames {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr {
            match name.ctx {
                ExprContext::Load => self.read.push(name.id.to_string()),
                ExprContext::Store | ExprContext::Del | ExprContext::Invalid => {
                    self.bound.insert(name.id.to_string());
                }
            }
        }
        source_order::walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                self.bound.insert(function.name.to_string());
            }
            Stmt::ClassDef(class) => {
                self.bound.insert(class.name.to_string());
            }
            _ => {}
        }
        source_order::walk_stmt(self, stmt);
    }

    fn visit_parameter(&mut self, parameter: &'a ast::Parameter) {
        self.bound.insert(parameter.name.to_string());
        source_order::walk_parameter(self, parameter);
    }

    fn visit_alias(&mut self, alias: &'a ast::Alias) {
        let bound = alias.asname.as_ref().unwrap_or(&alias.name);
        let bound = bound.as_str().split('.').next().unwrap_or_default();
        self.bound.insert(bound.to_string());
    }

    fn visit_except_handler(&mut self, handler: &'a ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(handler_inner) = handler;
        if let Some(name) = &handler_inner.name {
            self.bound.insert(name.to_string());
        }
        source_order::walk_except_handler(self, handler);
    }

    fn visit_pattern(&mut self, pattern: &'a ast::Pattern) {
        match pattern {
            ast::Pattern::MatchAs(ast::PatternMatchAs {
                name: Some(name), ..
            })
            | ast::Pattern::MatchStar(ast::PatternMatchStar {
                name: Some(name), ..
            })
            | ast::Pattern::MatchMapping(ast::PatternMatchMapping {
                rest: Some(name), ..
            }) => {
                self.bound.insert(name.to_string());
            }
            _ => {}
        }
        source_order::walk_pattern(self, pattern);
    }

    fn visit_type_param(&mut self, type_param: &'a ast::TypeParam) {
        self.bound.insert(type_param.name().to_string());
        source_order::walk_type_param(self, type_param);
    }
}

/// The absolute name of the module `import` imports from.
fn absolute_module(
    db: &dyn Db,
    importing_file: ImportingFile<'_>,
    import: &ast::StmtImportFrom,
) -> Option<String> {
    ModuleName::from_import_statement(db, importing_file, import)
        .ok()
        .map(|module| module.as_str().to_string())
}

/// What [`pasted_imports`] answers: the imports the pasted code is missing, and the
/// edits that add them.
#[derive(Debug, Default)]
pub struct PastedImports {
    /// The imports asked about that the pasted code is missing, in the order asked.
    pub missing: Vec<CopiedImport>,
    /// The edits that add every import of [`Self::missing`], all to the file the code
    /// was pasted into and all computed against its text as it is.
    pub edits: Vec<Edit>,
}

/// Which of `imports` the code pasted at `range` of `file` is missing, and the edits
/// that add them.
///
/// An import is missing when its name is not bound in the scope the pasted code
/// landed in. A name that is bound there is left alone even when it is bound to
/// something else: an import would shadow that, or be shadowed by it, and either
/// changes code the paste did not write. An import of a module the file cannot
/// resolve is not offered, nor one of `file`'s own members — those are the names the
/// paste can only have been cut from here, and are gone.
pub fn pasted_imports(
    db: &dyn Db,
    file: ProgramFile<'_>,
    range: TextRange,
    imports: &[CopiedImport],
) -> PastedImports {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let importer = import_order::importer(db, file, &parsed);
    let node = landing_node(&parsed, range);
    let members = importer.members_in_scope_at(node, range.start());
    let model = SemanticModel::new(db, file);
    let own_module = file_to_module(db, file.resolver_file(db))
        .map(|module| module.name(db).as_str().to_string());

    let mut answer = PastedImports::default();
    let mut actions = Vec::new();
    let mut seen = FxHashSet::default();
    for import in imports {
        if !seen.insert(import.name.as_str())
            || members.find_member(&import.name).is_some()
            || own_module.as_deref() == Some(import.module.as_str())
        {
            continue;
        }
        let resolves = ModuleName::new(&import.module)
            .is_some_and(|module| model.resolve_module_name(&module).is_some());
        if !resolves {
            continue;
        }
        let action = importer.import(import.request(), &members);
        // an import the file can only satisfy by spelling the name another way — as
        // `pathlib.Path` under an `import pathlib` — would leave the pasted name as
        // unresolved as it was
        if action.import().is_some() && action.symbol_text() == import.name {
            answer.missing.push(import.clone());
            actions.push(action);
        }
    }
    // placed together, so imports added at one place come out in isort's order
    answer.edits = importer.combined_edits(&actions);
    answer
}

/// The node whose scope the code pasted at `range` landed in.
fn landing_node(parsed: &ParsedModuleRef, range: TextRange) -> AnyNodeRef<'_> {
    covering_node(parsed.syntax().into(), range).node()
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_db::files::{File, system_path_to_file};
    use ruff_db::source::source_text;
    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ruff_python_trivia::textwrap::dedent;
    use ruff_text_size::{Ranged, TextRange, TextSize};
    use ty_project::{ProjectMetadata, SemanticDb as _};

    use super::{CopiedCode, CopiedImport, copied_imports, pasted_imports};

    /// A project of files, each written with its `<START>`/`<END>` markers taken out and kept as
    /// that file's ranges.
    struct PasteTest {
        db: ty_project::TestDb,
    }

    impl PasteTest {
        fn new(files: &[(&str, &str)]) -> (Self, Vec<(File, Vec<TextRange>)>) {
            let mut db =
                ty_project::TestDb::new(ProjectMetadata::new("test", SystemPathBuf::from("/")));
            let mut written = Vec::new();
            for (path, source) in files {
                let mut text = dedent(source).into_owned();
                let mut ranges = Vec::new();
                while let Some(start) = text.find("<START>") {
                    text.replace_range(start..start + "<START>".len(), "");
                    let end = text.find("<END>").expect("an `<END>` for every `<START>`");
                    text.replace_range(end..end + "<END>".len(), "");
                    ranges.push(TextRange::new(
                        TextSize::try_from(start).unwrap(),
                        TextSize::try_from(end).unwrap(),
                    ));
                }
                db.write_file(path, text)
                    .expect("write to memory file system to be successful");
                let file = system_path_to_file(&db, path).expect("newly written file to exist");
                written.push((file, ranges));
            }
            (Self { db }, written)
        }

        /// One line per import of the text in `ranges`, copied from where it is: `name <-
        /// import statement`.
        fn copied(&self, file: File, ranges: &[TextRange]) -> String {
            let source = source_text(&self.db, file);
            let copies: Vec<_> = ranges
                .iter()
                .map(|range| CopiedCode {
                    text: &source[*range],
                    at: range.start(),
                })
                .collect();
            self.copied_code(file, &copies)
        }

        /// One line per import of `copies`, copied from `file`.
        fn copied_code(&self, file: File, copies: &[CopiedCode<'_>]) -> String {
            copied_imports(&self.db, self.db.program_file(file), copies)
                .iter()
                .map(|import| format!("{} <- {}", import.name, statement(import)))
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// The imports missing at `range` of `file`, then the file with the edits applied.
        fn pasted(&self, file: File, range: TextRange, imports: &[CopiedImport]) -> String {
            let pasted = pasted_imports(&self.db, self.db.program_file(file), range, imports);
            let mut text = source_text(&self.db, file).as_str().to_string();
            let mut edits = pasted.edits;
            // edits at one position appear in the order given, so the later one goes in first
            edits.sort_by_key(Ranged::start);
            for edit in edits.iter().rev() {
                text.replace_range(
                    edit.start().to_usize()..edit.end().to_usize(),
                    edit.content().unwrap_or_default(),
                );
            }
            let missing = pasted
                .missing
                .iter()
                .map(statement)
                .collect::<Vec<_>>()
                .join("\n");
            format!("{missing}\n---\n{text}")
        }
    }

    fn statement(import: &CopiedImport) -> String {
        match &import.member {
            Some(member) if *member == import.name => {
                format!("from {} import {member}", import.module)
            }
            Some(member) => format!("from {} import {member} as {}", import.module, import.name),
            None if import.module.split('.').next() == Some(import.name.as_str()) => {
                format!("import {}", import.module)
            }
            None => format!("import {} as {}", import.module, import.name),
        }
    }

    fn import(name: &str, module: &str, member: Option<&str>) -> CopiedImport {
        CopiedImport {
            name: name.to_string(),
            module: module.to_string(),
            member: member.map(str::to_string),
        }
    }

    /// Every name the copied code reads from the module's global scope is answered with the import
    /// that binds it again: the imports the module has, under the names it has them, and the
    /// module's own definitions, from the module. A parameter, a name the copied code binds itself
    /// and a builtin are not imports at all.
    #[test]
    fn copied_code_needs_the_imports_and_definitions_of_its_module() {
        let (test, files) = PasteTest::new(&[(
            "shapes.py",
            r#"
            import math
            import os.path
            import numbers as num
            from pathlib import Path as P
            from collections import *

            SCALE = 2

            def helper() -> int:
                return 1

            def area(r: float) -> float:
                <START>local = math.pi * r * SCALE
                print(P(os.path.join("a")), helper(), local, deque, num.Number)
                return local<END>
            "#,
        )]);
        let (file, ranges) = &files[0];

        assert_snapshot!(test.copied(*file, ranges), @"
        math <- import math
        SCALE <- from shapes import SCALE
        P <- from pathlib import Path as P
        os <- import os.path
        helper <- from shapes import helper
        deque <- from collections import deque
        num <- import numbers as num
        ");
    }

    /// A definition the copied ranges include travels with them, and needs no import.
    #[test]
    fn a_definition_copied_with_its_uses_needs_no_import() {
        let (test, files) = PasteTest::new(&[(
            "shapes.py",
            r#"
            <START>def helper() -> int:
                return 1

            def area() -> int:
                return helper()<END>
            "#,
        )]);
        let (file, ranges) = &files[0];

        assert_snapshot!(test.copied(*file, ranges), @"");
    }

    /// Cut code is no longer in the file it came from. What its names meant is what the scope it
    /// was cut from still binds, and the imports at the top of the file outlive the cut.
    #[test]
    fn cut_code_needs_what_the_place_it_was_cut_from_binds() {
        let (test, files) = PasteTest::new(&[(
            "shapes.py",
            r#"
            import math

            def area(r: float) -> float:
                <START><END>

            def where(): ...
            "#,
        )]);
        let (file, ranges) = &files[0];
        let cut = CopiedCode {
            text: "return math.pi * r * r * where()\n    x = 1",
            at: ranges[0].start(),
        };

        assert_snapshot!(test.copied_code(*file, &[cut]), @"
        math <- import math
        where <- from shapes import where
        ");
    }

    /// A relative import is answered by the absolute module it names, which is what the code needs
    /// from anywhere else.
    #[test]
    fn a_relative_import_is_answered_by_its_absolute_module() {
        let (test, files) = PasteTest::new(&[
            ("pkg/__init__.py", ""),
            ("pkg/sizes.py", "BIG = 10\n"),
            (
                "pkg/shapes.py",
                r#"
                from .sizes import BIG

                <START>x = BIG<END>
                "#,
            ),
        ]);
        let (file, ranges) = &files[2];

        assert_snapshot!(test.copied(*file, ranges), @"BIG <- from pkg.sizes import BIG");
    }

    /// Several carets copy several ranges, and the answer covers all of them, once per name.
    #[test]
    fn every_range_copied_is_answered_once_per_name() {
        let (test, files) = PasteTest::new(&[(
            "main.py",
            r#"
            import math
            import json

            <START>a = math.pi<END>
            b = json.dumps(1)
            <START>c = math.tau<END>
            "#,
        )]);
        let (file, ranges) = &files[0];

        assert_snapshot!(test.copied(*file, ranges), @"math <- import math");
    }

    /// Pasted into a module that has some of the imports already, the paste is missing the rest: a
    /// member of a module the file imports from goes into that import, and the others are new.
    #[test]
    fn a_paste_is_missing_the_imports_its_module_does_not_have() {
        let (test, files) = PasteTest::new(&[
            ("shapes.py", "SCALE = 2\n"),
            (
                "main.py",
                r#"
                import math
                from pathlib import PurePath

                def f():
                    <START>return math.pi * SCALE, Path(), P(), np<END>
                "#,
            ),
        ]);
        let (file, ranges) = &files[1];
        let imports = [
            import("math", "math", None),
            import("SCALE", "shapes", Some("SCALE")),
            import("Path", "pathlib", Some("Path")),
            import("P", "pathlib", Some("PurePosixPath")),
            import("np", "numbers", None),
        ];

        assert_snapshot!(test.pasted(*file, ranges[0], &imports), @"
        from shapes import SCALE
        from pathlib import Path
        from pathlib import PurePosixPath as P
        import numbers as np
        ---

        import math
        import numbers as np
        from pathlib import Path, PurePath
        from pathlib import PurePosixPath as P

        from shapes import SCALE

        def f():
            return math.pi * SCALE, Path(), P(), np
        ");
    }

    /// A name bound where the code lands is left alone, whatever it is bound to: an import would
    /// change what the code around the paste reads.
    #[test]
    fn a_name_bound_where_the_paste_lands_is_not_imported() {
        let (test, files) = PasteTest::new(&[(
            "main.py",
            r#"
            def Path():
                ...

            <START>x = Path()<END>
            "#,
        )]);
        let (file, ranges) = &files[0];
        let imports = [import("Path", "pathlib", Some("Path"))];

        assert_snapshot!(test.pasted(*file, ranges[0], &imports), @"

        ---

        def Path():
            ...

        x = Path()
        ");
    }

    /// Code cut from a module and pasted back into it needs nothing from itself: what it named in
    /// the module is either still there, and bound, or went with the cut.
    #[test]
    fn a_paste_into_its_own_module_imports_nothing_from_it() {
        let (test, files) = PasteTest::new(&[(
            "shapes.py",
            r#"
            <START>x = helper()<END>
            "#,
        )]);
        let (file, ranges) = &files[0];
        let imports = [import("helper", "shapes", Some("helper"))];

        assert_snapshot!(test.pasted(*file, ranges[0], &imports), @"

        ---

        x = helper()
        ");
    }

    /// A module the file it lands in cannot resolve is not imported: the import would be as
    /// unresolved as the name.
    #[test]
    fn a_module_that_does_not_resolve_is_not_imported() {
        let (test, files) = PasteTest::new(&[(
            "main.py",
            r#"
            <START>x = thing<END>
            "#,
        )]);
        let (file, ranges) = &files[0];
        let imports = [import("thing", "elsewhere", Some("thing"))];

        assert_snapshot!(test.pasted(*file, ranges[0], &imports), @"

        ---

        x = thing
        ");
    }
}
