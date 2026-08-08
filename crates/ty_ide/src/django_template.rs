//! ide support for django template files
//!
//! django templates are not python, so none of the machinery the rest of this
//! crate is built on — the parser, the semantic index, type inference — applies
//! to them. what *is* shared is the project: a template's variables come from
//! the view that renders it, its `{% url %}` names come from the project's url
//! configuration, and its custom tags and filters come from the project's
//! `templatetags` modules. so this module owns a small template front end of its
//! own ([`lexer`], [`index`]) and spends the rest of its effort joining it to the
//! python side ([`project`]).
//!
//! the join runs both ways. [`python`] is the other direction: the template name
//! a view renders and the route name a view reverses are plain strings to python,
//! and it is what lets the python services see them for what they are.

mod builtins;
mod completion;
mod diagnostics;
mod folding;
mod goto;
mod hover;
mod index;
mod lexer;
mod project;
mod python;
mod references;
mod rename;
mod resolve;
mod semantic_tokens;
mod symbols;
mod uses;

pub use completion::{TemplateCompletion, TemplateEdit};
pub use hover::{DisplayTemplateHover, TemplateHover};
pub(crate) use python::{
    string_completions as django_string_completions, string_definition as django_string_definition,
};
pub use rename::{PreparedTemplateRename, TemplateRename, TemplateRenameOutcome};
pub use symbols::TemplateSymbol;

use ruff_db::diagnostic::Diagnostic;
use ruff_db::files::File;
use ruff_db::source::source_text;
use ruff_db::system::SystemPath;
use ruff_text_size::{TextRange, TextSize};
use ty_project::Db;

use ty_python_semantic::lint::LintId;

use crate::code_action::QuickFix;
use crate::semantic_tokens::SemanticTokens;
use crate::{FoldingRange, NavigationTargets, RangedValue, ReferenceTarget};

use index::TemplateIndex;

/// how many `{% extends %}` hops a parent chain is followed
const MAX_INHERITANCE_DEPTH: usize = 16;

/// the file extensions a django template is conventionally written with
///
/// django itself puts no constraint on the extension — the template loader takes
/// whatever path it is given — so this list only decides what the server will
/// *offer* template support for when the editor hasn't already told it the file
/// is a template.
const TEMPLATE_EXTENSIONS: &[&str] = &["html", "htm", "txt", "xml", "django", "dj", "jinja"];

/// the directory name django's app-directories loader looks in
const TEMPLATE_DIRECTORY: &str = "templates";

/// whether `path` looks like a django template
///
/// an editor that knows the file's language (vs code's `django-html`, vim's
/// `htmldjango`) tells the server directly and this is never consulted. it is
/// the fallback for the much more common case of a `.html` file that the editor
/// reports as plain html, and it deliberately requires the file to be *inside* a
/// `templates` directory so that ordinary html in a project is left alone.
pub fn is_django_template_path(path: &SystemPath) -> bool {
    let has_template_extension = path
        .extension()
        .is_some_and(|extension| TEMPLATE_EXTENSIONS.contains(&extension));

    has_template_extension
        && path
            .ancestors()
            .any(|ancestor| ancestor.file_name() == Some(TEMPLATE_DIRECTORY))
}

/// the index of `file`, parsed as a django template
///
/// the query is tracked so that the several ide features that need it — the
/// completions, the semantic tokens, goto — parse each template once per edit
/// rather than once per request.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn template_index(db: &dyn Db, file: File) -> TemplateIndex {
    TemplateIndex::from_source(source_text(db, file).as_str())
}

/// the semantic tokens of `file`, read as a django template
///
/// `range` restricts the result to the tokens it touches, for the ranged
/// semantic-tokens request.
pub fn django_template_semantic_tokens(
    db: &dyn Db,
    file: File,
    range: Option<TextRange>,
) -> SemanticTokens {
    let source = source_text(db, file);
    SemanticTokens::new(semantic_tokens::semantic_tokens(
        db,
        template_index(db, file),
        source.as_str(),
        range,
    ))
}

/// where the name at `offset` of `file` is defined, read as a django template
pub fn django_template_goto_definition(
    db: &dyn Db,
    file: File,
    offset: TextSize,
) -> Option<RangedValue<NavigationTargets>> {
    let source = source_text(db, file);
    goto::goto_definition(db, file, template_index(db, file), source.as_str(), offset)
}

/// what the thing at `offset` of `file` is, read as a django template
pub fn django_template_hover(
    db: &dyn Db,
    file: File,
    offset: TextSize,
) -> Option<RangedValue<TemplateHover>> {
    let source = source_text(db, file);
    hover::hover(db, file, template_index(db, file), source.as_str(), offset)
}

/// the outline of `file`, read as a django template
pub fn django_template_document_symbols(db: &dyn Db, file: File) -> Vec<TemplateSymbol> {
    symbols::document_symbols(template_index(db, file))
}

/// the foldable ranges of `file`, read as a django template
pub fn django_template_folding_ranges(db: &dyn Db, file: File) -> Vec<FoldingRange> {
    let source = source_text(db, file);
    folding::folding_ranges(template_index(db, file), source.as_str())
}

/// the templates `index` extends, nearest ancestor first
///
/// a template whose parent chain has a cycle in it would not render, but the
/// editor must not hang on one, so the walk stops at the first template it has
/// already been through — and at [`MAX_INHERITANCE_DEPTH`] regardless.
fn ancestors<'db>(
    db: &'db dyn Db,
    file: File,
    index: &TemplateIndex,
) -> Vec<(File, &'db TemplateIndex)> {
    let mut collected = Vec::new();
    let mut seen = vec![file];
    let mut parent = index.extends().map(|reference| reference.name.clone());

    while collected.len() < MAX_INHERITANCE_DEPTH {
        let Some(name) = parent.take() else { break };
        let Some(parent_file) = project::resolve_template(db, &name) else {
            break;
        };
        if seen.contains(&parent_file) {
            break;
        }
        seen.push(parent_file);

        let parent_index = template_index(db, parent_file);
        collected.push((parent_file, parent_index));
        parent = parent_index
            .extends()
            .map(|reference| reference.name.clone());
    }

    collected
}

/// everything wrong with `file`, read as a django template
///
/// the type checker never sees a template — it is not python — so this is the
/// whole of what a template document can report.
pub fn django_template_diagnostics(db: &dyn Db, file: File) -> Vec<Diagnostic> {
    let source = source_text(db, file);
    diagnostics::diagnostics(db, file, template_index(db, file), source.as_str())
}

/// the quick fixes offered for a template diagnostic at `range`
pub(crate) fn django_template_code_actions(
    db: &dyn Db,
    file: File,
    range: TextRange,
    lint: LintId,
) -> Vec<QuickFix> {
    let source = source_text(db, file);
    diagnostics::code_actions(
        db,
        file,
        template_index(db, file),
        source.as_str(),
        range,
        lint,
    )
}

/// whether the django name at `offset` of `file` can be renamed
///
/// `template` says the file is a django template rather than python, which the
/// caller knows and this cannot. either language may write one of these names,
/// and `None` — a position that names nothing django knows — is what leaves a
/// python file's position to the python services.
pub fn django_prepare_rename(
    db: &dyn Db,
    file: File,
    offset: TextSize,
    template: bool,
) -> Option<PreparedTemplateRename> {
    rename::prepare(db, file, offset, template)
}

/// every edit renaming the django name at `offset` of `file` to `new_name` makes
pub fn django_rename(
    db: &dyn Db,
    file: File,
    offset: TextSize,
    new_name: &str,
    template: bool,
) -> Option<TemplateRenameOutcome> {
    rename::rename(db, file, offset, new_name, template)
}

/// every place the django name at `offset` of `file` is written
///
/// `template` says the file is a django template rather than python, as for the
/// rename above. `include_declaration` is the client's, as LSP specifies: with it
/// off, the block the base declares, the file a template name loads and the
/// `path(…, name=…)` a route is declared by are all left out of the answer.
pub fn django_references(
    db: &dyn Db,
    file: File,
    offset: TextSize,
    include_declaration: bool,
    template: bool,
) -> Option<Vec<ReferenceTarget>> {
    references::references(db, file, offset, include_declaration, template)
}

/// the completions for `offset` in `file`, read as a django template
pub fn django_template_completions(
    db: &dyn Db,
    file: File,
    offset: TextSize,
) -> Vec<TemplateCompletion> {
    let source = source_text(db, file);
    completion::completions(db, file, template_index(db, file), source.as_str(), offset)
}

#[cfg(test)]
pub(crate) mod tests {
    use ruff_db::Db as _;
    use ruff_db::files::{File, FileRootKind, system_path_to_file};
    use ruff_db::system::{DbWithTestSystem, DbWithWritableSystem, SystemPath, SystemPathBuf};
    use ruff_python_ast::PythonVersion;
    use ruff_python_trivia::textwrap::dedent;
    use ruff_text_size::{Ranged, TextSize};
    use ty_module_resolver::SearchPathSettings;
    use ty_project::{ProjectMetadata, TestDb};
    use ty_python_core::platform::PythonPlatform;
    use ty_python_core::program::{FallibleStrategy, Program, ProgramSettings};
    use ty_python_semantic::PythonVersionWithSource;

    use crate::MarkupKind;

    use super::{
        PreparedTemplateRename, TemplateRenameOutcome, TemplateSymbol, django_prepare_rename,
        django_references, django_rename, django_template_completions, django_template_diagnostics,
        django_template_document_symbols, django_template_folding_ranges,
        django_template_goto_definition, django_template_hover, is_django_template_path,
    };

    /// a project whose files are written out, with the cursor marked by
    /// `<CURSOR>` in at most one of them
    ///
    /// a test that has no cursor to place — a diagnostic's, which reads a whole
    /// file rather than a position in one — leaves the marker out, and the file
    /// under test is then the last one written.
    pub(crate) struct TemplateTest {
        pub(crate) db: TestDb,
        pub(crate) file: File,
        pub(crate) offset: TextSize,
    }

    impl TemplateTest {
        /// build a project from `(path, contents)` pairs
        pub(crate) fn new(sources: &[(&str, &str)]) -> Self {
            let mut db = TestDb::new(ProjectMetadata::new("test", SystemPathBuf::from("/")));
            db.init_program_with_python_version(PythonVersion::latest_ty())
                .unwrap();

            Self::write(db, SystemPath::new("/"), sources)
        }

        /// the same, with `installed` written to a site-packages directory
        ///
        /// site-packages sits *outside* the project root, which is what makes
        /// what is written there third-party rather than the project's own —
        /// under the root it would be found by the first-party scans instead.
        pub(crate) fn with_site_packages(
            sources: &[(&str, &str)],
            installed: &[(&str, &str)],
        ) -> Self {
            let root = SystemPathBuf::from("/src");
            let site_packages = SystemPathBuf::from("/site-packages");

            let mut db = TestDb::new(ProjectMetadata::new("test", root.clone()));

            for (path, contents) in installed {
                db.write_file(site_packages.join(path), dedent(contents).as_ref())
                    .unwrap();
            }
            db.memory_file_system().create_directory_all(&root).unwrap();
            db.memory_file_system()
                .create_directory_all(&site_packages)
                .unwrap();

            let search_paths = SearchPathSettings {
                src_roots: vec![root.clone()],
                site_packages_paths: vec![site_packages.clone()],
                ..SearchPathSettings::empty()
            }
            .to_search_paths(db.system(), db.vendored(), &FallibleStrategy)
            .expect("valid search paths");

            Program::from_settings(
                &db,
                ProgramSettings {
                    python_version: PythonVersionWithSource::default(),
                    python_platform: PythonPlatform::default(),
                    search_paths,
                },
            );

            db.files().try_add_root(&db, &root, FileRootKind::Project);
            db.files()
                .try_add_root(&db, &site_packages, FileRootKind::SearchPath);

            Self::write(db, &root, sources)
        }

        /// write every source under `root`, taking the one `<CURSOR>` marker out
        fn write(mut db: TestDb, root: &SystemPath, sources: &[(&str, &str)]) -> Self {
            const MARKER: &str = "<CURSOR>";

            let mut cursor = None;
            let mut last = None;

            for (path, contents) in sources {
                let contents = dedent(contents).into_owned();

                let (contents, offset) = match contents.find(MARKER) {
                    Some(index) => {
                        let mut without = contents[..index].to_string();
                        without.push_str(&contents[index + MARKER.len()..]);
                        (without, Some(TextSize::try_from(index).unwrap()))
                    }
                    None => (contents, None),
                };

                let path = root.join(path);
                db.write_file(&path, &contents).unwrap();
                let file = system_path_to_file(&db, &path).unwrap();
                last = Some(file);

                if let Some(offset) = offset {
                    assert!(cursor.is_none(), "more than one `<CURSOR>` marker");
                    cursor = Some((file, offset));
                }
            }

            let (file, offset) = cursor
                .unwrap_or_else(|| (last.expect("a source to be written"), TextSize::default()));
            Self { db, file, offset }
        }

        /// every diagnostic of the file under test, rendered as
        /// `rule severity: message [text]`
        pub(crate) fn diagnostics(&self) -> Vec<String> {
            let source = ruff_db::source::source_text(&self.db, self.file);

            django_template_diagnostics(&self.db, self.file)
                .into_iter()
                .map(|diagnostic| {
                    let range = diagnostic
                        .primary_span()
                        .and_then(|span| span.range())
                        .unwrap_or_default();

                    format!(
                        "{} {:?}: {} [{}]",
                        diagnostic.id(),
                        diagnostic.severity(),
                        diagnostic.primary_message(),
                        &source[range]
                    )
                })
                .collect()
        }

        /// the labels of the completions at the cursor, in the order offered
        pub(crate) fn completions(&self) -> Vec<String> {
            django_template_completions(&self.db, self.file, self.offset)
                .into_iter()
                .map(|completion| completion.label)
                .collect()
        }

        /// the completions at the cursor, rendered as `label — detail`
        pub(crate) fn detailed(&self) -> Vec<String> {
            django_template_completions(&self.db, self.file, self.offset)
                .into_iter()
                .map(|completion| match completion.detail {
                    Some(detail) => format!("{} — {detail}", completion.label),
                    None => completion.label,
                })
                .collect()
        }

        /// where goto-definition at the cursor lands, as `path:text`
        pub(crate) fn definitions(&self) -> Vec<String> {
            let Some(targets) = django_template_goto_definition(&self.db, self.file, self.offset)
            else {
                return Vec::new();
            };

            targets
                .into_iter()
                .map(|target| {
                    let source = ruff_db::source::source_text(&self.db, target.file());
                    format!(
                        "{}:{}",
                        // the memory file system reports the host's separator
                        target.file().path(&self.db).to_string().replace('\\', "/"),
                        &source[target.focus_range()]
                    )
                })
                .collect()
        }

        /// the hover at the cursor, as markdown, or `""` where there is none
        pub(crate) fn hover(&self) -> String {
            django_template_hover(&self.db, self.file, self.offset)
                .map(|hover| hover.display(MarkupKind::Markdown).to_string())
                .unwrap_or_default()
        }

        /// the template's outline, one line per symbol, nesting indented
        pub(crate) fn symbols(&self) -> Vec<String> {
            fn render(symbols: &[TemplateSymbol], depth: usize, lines: &mut Vec<String>) {
                for symbol in symbols {
                    lines.push(format!(
                        "{:indent$}{:?} {}",
                        "",
                        symbol.kind,
                        symbol.name,
                        indent = depth * 2
                    ));
                    render(&symbol.children, depth + 1, lines);
                }
            }

            let mut lines = Vec::new();
            render(
                &django_template_document_symbols(&self.db, self.file),
                0,
                &mut lines,
            );
            lines
        }

        /// whether the file under test is read as a template rather than python
        ///
        /// this is the same question the server answers from what the editor told
        /// it, and falls back to the same path check when it wasn't told.
        fn is_template(&self) -> bool {
            match self.file.path(&self.db) {
                ruff_db::files::FilePath::System(path) => is_django_template_path(path),
                _ => false,
            }
        }

        /// what the editor is offered for a rename at the cursor
        pub(crate) fn prepare_rename(&self) -> String {
            match django_prepare_rename(&self.db, self.file, self.offset, self.is_template()) {
                None => "no rename".to_string(),
                Some(PreparedTemplateRename::Refused(why)) => format!("refused: {why}"),
                Some(PreparedTemplateRename::Ready { range, placeholder }) => {
                    let source = ruff_db::source::source_text(&self.db, self.file);
                    format!("rename `{placeholder}`, replacing `{}`", &source[range])
                }
            }
        }

        /// every edit a rename at the cursor would make, as `path:line old -> new`
        pub(crate) fn rename(&self, new_name: &str) -> Vec<String> {
            let renamed = django_rename(
                &self.db,
                self.file,
                self.offset,
                new_name,
                self.is_template(),
            );

            let rename = match renamed {
                None => return vec!["no rename".to_string()],
                Some(TemplateRenameOutcome::Refused(why)) => {
                    return vec![format!("refused: {why}")];
                }
                Some(TemplateRenameOutcome::Edits(rename)) => rename,
            };

            let mut lines: Vec<String> = rename
                .edits
                .iter()
                .map(|edit| {
                    let source = ruff_db::source::source_text(&self.db, edit.file());
                    let line = source.as_str()[..usize::from(edit.range().start())]
                        .matches('\n')
                        .count()
                        + 1;

                    format!(
                        "{}:{line} {} -> {new_name}",
                        // the memory file system reports the host's separator
                        edit.file().path(&self.db).to_string().replace('\\', "/"),
                        &source[edit.range()]
                    )
                })
                .collect();

            if let Some((from, to)) = rename.file_rename {
                lines.push(format!(
                    "move {} -> {}",
                    from.as_str().replace('\\', "/"),
                    to.as_str().replace('\\', "/")
                ));
            }

            lines
        }

        /// every reference at the cursor, as `path:line text`
        ///
        /// a declaration is marked, since which occurrences are declarations is
        /// what `includeDeclaration` turns on and off.
        pub(crate) fn references(&self) -> Vec<String> {
            self.references_with_declaration(true)
        }

        /// the same, as a client that asked for the uses alone gets them
        pub(crate) fn references_without_declaration(&self) -> Vec<String> {
            self.references_with_declaration(false)
        }

        fn references_with_declaration(&self, include_declaration: bool) -> Vec<String> {
            let found = django_references(
                &self.db,
                self.file,
                self.offset,
                include_declaration,
                self.is_template(),
            );

            found
                .unwrap_or_default()
                .into_iter()
                .map(|target| {
                    let source = ruff_db::source::source_text(&self.db, target.file());
                    let line = source.as_str()[..usize::from(target.range().start())]
                        .matches('\n')
                        .count()
                        + 1;

                    format!(
                        "{}{}:{line} {}",
                        match target.kind() {
                            crate::ReferenceKind::Other => "declaration ",
                            _ => "",
                        },
                        // the memory file system reports the host's separator
                        target.file().path(&self.db).to_string().replace('\\', "/"),
                        &source[target.range()],
                    )
                })
                .collect()
        }

        /// each foldable range, as the tags that open and close it
        pub(crate) fn folds(&self) -> Vec<String> {
            let source = ruff_db::source::source_text(&self.db, self.file);

            django_template_folding_ranges(&self.db, self.file)
                .into_iter()
                .map(|fold| {
                    let text = &source[fold.range];
                    let first = text.lines().next().unwrap_or_default().trim();
                    let last = text.lines().next_back().unwrap_or_default().trim();
                    format!("{first} … {last}")
                })
                .collect()
        }
    }

    #[test]
    fn a_template_is_an_html_file_under_a_templates_directory() {
        assert!(is_django_template_path(SystemPath::new(
            "/app/templates/blog/post.html"
        )));
        assert!(is_django_template_path(SystemPath::new(
            "/templates/base.txt"
        )));
    }

    #[test]
    fn ordinary_html_outside_a_templates_directory_is_not_a_template() {
        assert!(!is_django_template_path(SystemPath::new(
            "/app/static/index.html"
        )));
        assert!(!is_django_template_path(SystemPath::new("/README.html")));
    }

    #[test]
    fn a_python_file_is_never_a_template() {
        assert!(!is_django_template_path(SystemPath::new(
            "/app/templates/views.py"
        )));
    }
}
