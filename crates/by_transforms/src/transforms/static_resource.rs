//! Lowers a static resource import into the python its document stands for.
//!
//! `import "data/config.yaml" as config` has no python spelling: python imports
//! modules, and the file is not one. What python does have is the document
//! itself, written out — a mapping as a class, a sequence as a tuple, a scalar
//! as a `Final` literal — and that is what the import becomes:
//!
//! ```python
//! class config:
//!     class a:
//!         b: Final = (1, 2)
//! ```
//!
//! The rendering is the one `by_resource` produces, which is also the one the
//! type checker infers, so what the program gets is what the checker described.
//!
//! # Why the document is written into the importing module
//!
//! Rather than into a module of its own that importers share. A resource is
//! read at build time and has no runtime existence, so a file that imports one
//! is complete on its own: `by transpile` on a single file emits python that
//! runs, with no second file to place beside it and no import edge that the
//! source did not have.
//!
//! Two modules importing one resource therefore get two objects rather than
//! one. Nothing about a value read through attributes notices, except identity:
//! `a.config.x is b.config.x` is false where a shared module would make it true.
//!
//! The class this emits is named after the binding rather than after the file,
//! so the reader of the output sees the name the import gave it. The checker
//! names the same class after the file — a resource file is rendered once, and
//! a name in it cannot depend on who imported it — which is visible only in
//! `__name__`.
//!
//! # Why there is no reverse transform
//!
//! Every other lowering here has one, so that python read back as basedpython
//! comes back as the idiom it started from. This one cannot: the python it
//! emits is a tree of classes holding constants, which is also what a
//! hand-written tree of classes holding constants looks like. Recognising one
//! as a document would mean claiming a file the source never mentions, and
//! guessing which of json, toml and yaml it had been. A resource import is
//! written by hand, and read back as the classes it became.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Alias, ModModule, Stmt, StmtImport};
use ruff_text_size::{Ranged, TextRange, TextSize};
use thin_vec::ThinVec;

use super::ast_driver::{AstPass, PassContext};
use crate::type_info::TypeInfo;

/// What each static resource import in a file lowers to.
#[derive(Default)]
pub(crate) struct ResourceLowerings {
    /// `(the import statement's range, the python that replaces it)`.
    ///
    /// The python is written at module level; the statement it replaces may be
    /// indented, and [`indented`] puts it where the statement was.
    replacements: Vec<(TextRange, String)>,
    /// documents that could not be read; the checker reports these too, and the
    /// transpile has nothing to emit for them
    errors: Vec<String>,
}

/// Lower every static resource import in `stmts`.
///
/// `stmts` must come from the same parse `types` answers for: the lowering is
/// keyed by source range, and a range from another parse names other text.
pub(crate) fn collect(stmts: &[Stmt], types: &dyn TypeInfo) -> ResourceLowerings {
    let mut collector = Collector {
        types,
        lowerings: ResourceLowerings::default(),
    };
    for stmt in stmts {
        collector.visit_stmt(stmt);
    }
    collector.lowerings
}

struct Collector<'a> {
    types: &'a dyn TypeInfo,
    lowerings: ResourceLowerings,
}

impl Visitor<'_> for Collector<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let Stmt::Import(import) = stmt
            && import.names.iter().any(|alias| alias.is_resource)
        {
            self.lower(import);
            return;
        }
        walk_stmt(self, stmt);
    }
}

impl Collector<'_> {
    fn lower(&mut self, import: &StmtImport) {
        // the parser rejects a statement that mixes a resource with a module, so
        // every alias here names a resource
        let mut lowered = String::new();

        for alias in &import.names {
            let Some(binding) = binding_of(alias) else {
                // no `as` clause: the parser has already said so, and there is
                // no name to bind the document to
                return;
            };
            match self.types.static_resource(&alias.name.id, binding) {
                Ok(rendered) => lowered.push_str(&rendered),
                Err(message) => {
                    self.lowerings.errors.push(format!(
                        "cannot read static resource `{path}`: {message}",
                        path = alias.name.id
                    ));
                    return;
                }
            }
        }

        self.lowerings.replacements.push((import.range(), lowered));
    }
}

/// The name an alias binds, which for a resource is always its `as` clause.
fn binding_of(alias: &Alias) -> Option<&str> {
    Some(alias.asname.as_ref()?.id.as_str())
}

/// The whitespace at the start of the line `offset` is on.
fn indentation_of(source: &str, offset: TextSize) -> String {
    let line_start = source[..usize::from(offset)]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    source[line_start..usize::from(offset)]
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect()
}

/// `source` with every non-empty line indented by `indent`.
///
/// The first line is left alone: it takes the place of the statement, which is
/// already at that indentation.
fn indented(source: &str, indent: &str) -> String {
    if indent.is_empty() {
        return source.to_string();
    }
    let mut indented = String::with_capacity(source.len());
    for (index, line) in source.lines().enumerate() {
        if index > 0 {
            indented.push_str(indent);
        }
        indented.push_str(line);
        indented.push('\n');
    }
    indented
}

/// Replaces each static resource import with the document it names.
///
/// The import goes from the source text *and* from the AST, because either one
/// alone leaves the other standing. A statement no pass mutated keeps its source
/// bytes, and ignores the AST; a statement some pass did mutate is re-rendered
/// from the AST, and the text edit inside it is dropped. An `import` statement
/// that survived to the output is not python at all — a path is not a module
/// name — so phase 3 rejects the whole file.
///
/// The AST rewrite deliberately does not declare the statement changed. Saying
/// so would re-render every enclosing `def` through the code generator, losing
/// the comments and the formatting of everything else in it; leaving it undeclared
/// means the rewrite matters only when something else already forced a re-render.
pub(crate) struct StaticResource {
    lowerings: ResourceLowerings,
    source: String,
}

impl StaticResource {
    pub(crate) fn new(lowerings: ResourceLowerings, source: &str) -> Self {
        Self {
            lowerings,
            source: source.to_string(),
        }
    }

    fn rendering_for(&self, range: TextRange) -> Option<&str> {
        self.lowerings
            .replacements
            .iter()
            .find_map(|(replaced, rendering)| (*replaced == range).then_some(rendering.as_str()))
    }

    /// Replace every lowered import in `body` with the statements it renders to.
    fn rewrite_body(&self, body: &mut ThinVec<Stmt>) {
        let mut index = 0;
        while index < body.len() {
            let rendered = match &body[index] {
                Stmt::Import(import) if import.names.iter().any(|alias| alias.is_resource) => {
                    self.rendering_for(import.range()).and_then(|rendering| {
                        let parsed = ruff_python_parser::parse_module(rendering).ok()?;
                        Some(parsed.into_syntax().body)
                    })
                }
                _ => None,
            };

            if let Some(rendered) = rendered {
                let count = rendered.len();
                body.splice(index..=index, rendered);
                index += count;
                continue;
            }

            self.rewrite_within(&mut body[index]);
            index += 1;
        }
    }

    /// Rewrite the bodies `stmt` holds, if it holds any.
    fn rewrite_within(&self, stmt: &mut Stmt) {
        match stmt {
            Stmt::FunctionDef(node) => self.rewrite_body(&mut node.body),
            Stmt::ClassDef(node) => self.rewrite_body(&mut node.body),
            Stmt::For(node) => {
                self.rewrite_body(&mut node.body);
                self.rewrite_body(&mut node.orelse);
            }
            Stmt::While(node) => {
                self.rewrite_body(&mut node.body);
                self.rewrite_body(&mut node.orelse);
            }
            Stmt::If(node) => {
                self.rewrite_body(&mut node.body);
                for clause in &mut node.elif_else_clauses {
                    self.rewrite_body(&mut clause.body);
                }
            }
            Stmt::With(node) => self.rewrite_body(&mut node.body),
            Stmt::Match(node) => {
                for case in &mut node.cases {
                    self.rewrite_body(&mut case.body);
                }
            }
            Stmt::Try(node) => {
                self.rewrite_body(&mut node.body);
                for handler in &mut node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    self.rewrite_body(&mut handler.body);
                }
                self.rewrite_body(&mut node.orelse);
                self.rewrite_body(&mut node.finalbody);
            }
            // every other statement holds expressions, and an import is not one
            _ => {}
        }
    }
}

impl AstPass for StaticResource {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        for error in &self.lowerings.errors {
            ctx.errors.push(error.clone());
        }
        if self.lowerings.replacements.is_empty() {
            return;
        }

        ctx.required_imports
            .push(by_resource::REQUIRED_IMPORT.to_string());
        for (range, rendering) in &self.lowerings.replacements {
            let indent = indentation_of(&self.source, range.start());
            // the rendering ends in a newline, and it replaces a statement the
            // splice will follow with one of its own
            let replacement = indented(rendering, &indent)
                .trim_end_matches('\n')
                .to_string();
            ctx.text_edits.push((*range, replacement));
        }

        self.rewrite_body(&mut module.body);
    }
}

#[cfg(test)]
mod tests {
    use ruff_db::files::system_path_to_file;
    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ty_project::{ProjectMetadata, TestDb};

    use super::*;
    use crate::{Config, transpile_typed};

    /// transpile `/proj/main.by`, with `files` written around it.
    fn transpile(files: &[(&str, &str)]) -> Result<String, String> {
        let mut db = TestDb::new(ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            SystemPathBuf::from("/proj"),
        ));
        for (path, source) in files {
            db.write_file(path, source).expect("write file failed");
        }
        db.init_program().expect("program init failed");
        let file = system_path_to_file(&db, "/proj/main.by").expect("file not in db");
        transpile_typed(&db, file, &Config::test_default(), None).map_err(|error| error.to_string())
    }

    #[test]
    fn a_document_is_written_into_the_module_that_imports_it() {
        let output = transpile(&[
            ("/proj/data/config.yaml", "a:\n  b:\n    - 1\n    - 2\n"),
            (
                "/proj/main.by",
                "import \"data/config.yaml\" as config\n\nprint(config.a.b[1])\n",
            ),
        ])
        .expect("transpile should succeed");

        assert!(output.contains("from typing import Final"), "{output}");
        assert!(
            output.contains("class config:\n    class a:\n        b: Final = (1, 2)"),
            "{output}"
        );
        assert!(output.contains("print(config.a.b[1])"), "{output}");
    }

    #[test]
    fn an_import_inside_a_function_is_indented_to_match() {
        let output = transpile(&[
            ("/proj/data/config.json", r#"{"a": 1}"#),
            (
                "/proj/main.by",
                "def f():\n    import \"data/config.json\" as config\n    return config.a\n",
            ),
        ])
        .expect("transpile should succeed");

        assert!(
            output.contains("    class config:\n        a: Final = 1"),
            "{output}"
        );
    }

    /// an ast-mutating pass re-renders the whole top-level statement it touched,
    /// through the code generator rather than from source. a resource import
    /// inside one is re-spelled by the generator, and a path written as a name
    /// is not python
    #[test]
    fn an_import_inside_a_statement_another_pass_rewrites_survives() {
        let output = transpile(&[
            ("/proj/data/config.json", r#"{"a": 1}"#),
            (
                "/proj/main.by",
                "def f(_, _):\n    import \"data/config.json\" as config\n    return config.a\n",
            ),
        ])
        .expect("transpile should succeed");

        assert!(output.contains("def f(_, _2):"), "{output}");
        assert!(output.contains("class config:"), "{output}");
        assert!(!output.contains("import data/config.json"), "{output}");
    }

    /// the statements spliced into the AST are parsed from the rendering and
    /// carry its ranges, which name nothing in this file. a pass reading one as
    /// a range in the source slices out of bounds
    #[test]
    fn a_rewritten_statement_does_not_poison_the_passes_around_it() {
        let output = transpile(&[
            ("/proj/data/config.json", r#"{"a": 1}"#),
            (
                "/proj/main.by",
                "let value = 1\n\n\ndef f(_, _):\n    import \"data/config.json\" as config\n    return config.a\n",
            ),
        ])
        .expect("transpile should succeed");

        assert!(output.contains("value: Final = 1"), "{output}");
        assert!(output.contains("class config:"), "{output}");
    }

    #[test]
    fn a_document_that_cannot_be_read_fails_the_transpile() {
        let error = transpile(&[
            ("/proj/data/broken.json", "{ \"a\": }"),
            ("/proj/main.by", "import \"data/broken.json\" as config\n"),
        ])
        .expect_err("a document that is not json has nothing to emit");

        assert!(error.contains("data/broken.json"), "{error}");
    }

    #[test]
    fn a_path_that_names_nothing_fails_the_transpile() {
        let error = transpile(&[("/proj/main.by", "import \"data/gone.json\" as config\n")])
            .expect_err("a path that names nothing has nothing to emit");

        assert!(error.contains("data/gone.json"), "{error}");
    }

    #[test]
    fn indentation() {
        assert_eq!(indentation_of("import x", 0.into()), "");
        assert_eq!(indentation_of("def f():\n    import x", 13.into()), "    ");
    }

    #[test]
    fn indenting_leaves_the_first_line_alone() {
        assert_eq!(
            indented("class a:\n    b: Final = 1\n", "    "),
            "class a:\n        b: Final = 1\n"
        );
    }
}
