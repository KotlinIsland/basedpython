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
mod code_lens;
mod completion;
mod diagnostics;
mod folding;
mod goto;
mod hover;
mod index;
mod inlay_hints;
mod lexer;
mod project;
mod python;
mod references;
mod rename;
mod resolve;
mod routes;
mod semantic_tokens;
mod signature_help;
mod symbols;
mod uses;

pub use code_lens::{DjangoCodeLens, DjangoLensAction, DjangoLensTarget};
pub use completion::{TemplateCompletion, TemplateEdit};
pub use hover::{DisplayTemplateHover, TemplateHover};
pub use inlay_hints::{TemplateInlayHint, TemplateInlayHintKind};
pub(crate) use python::{
    string_completions as django_string_completions, string_definition as django_string_definition,
};
pub use rename::{PreparedTemplateRename, TemplateRename, TemplateRenameOutcome};
pub use signature_help::TemplateSignature;
pub use symbols::{DjangoSymbol, TemplateSymbol};

use ruff_db::diagnostic::Diagnostic;
use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::SystemPath;
use ruff_text_size::{TextRange, TextSize};
use ty_project::Db;
use ty_project::glob::IncludeResult;

use ty_python_semantic::lint::LintId;

use crate::code_action::QuickFix;
use crate::semantic_tokens::SemanticTokens;
use crate::{FoldingRange, InlayHintSettings, NavigationTargets, RangedValue, ReferenceTarget};

use index::TemplateIndex;

/// how many `{% extends %}` hops a parent chain is followed
const MAX_INHERITANCE_DEPTH: usize = 16;

/// the file extensions a django template is conventionally written with
///
/// django itself puts no constraint on the extension — the template loader takes
/// whatever path it is given — so this list only decides what the server will
/// *offer* template support for when the editor hasn't already told it the file
/// is a template.
///
/// `.jinja` is deliberately not here. jinja is a different language that reads
/// alike, and everything in this module answers as django: a `{% set %}`, a
/// `{% macro %}` or a `|default("x")` is correct jinja that django's tag and
/// filter tables know nothing about, so claiming a jinja file means reporting
/// correct code as wrong. we do not support jinja, so we do not claim its files.
const TEMPLATE_EXTENSIONS: &[&str] = &["html", "htm", "txt", "xml", "django", "dj"];

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

/// every django thing of the project whose name matches `query`
pub(crate) fn django_workspace_symbols(
    db: &dyn Db,
    query: &crate::symbols::QueryPattern,
) -> Vec<DjangoSymbol> {
    symbols::workspace_symbols(db, query)
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
    if !project::has_django(db, db.project()) {
        return Vec::new();
    }

    let source = source_text(db, file);
    diagnostics::diagnostics(db, file, template_index(db, file), source.as_str())
}

/// everything wrong with the django a python file writes
///
/// this is what the type checker's own pass over the file cannot say: whether a
/// route's view can take the arguments the route gives it is a question about the
/// project's whole url tree, which is read here rather than there.
///
/// the file's suppression comments are deliberately *not* applied: these are
/// folded into the type checker's own diagnostics, which is where a `ty: ignore`
/// is honoured and counted used — see [`ty_python_semantic::check_file_with`].
pub fn django_python_diagnostics(db: &dyn Db, file: File) -> Vec<Diagnostic> {
    routes::diagnostics(db, file)
}

/// django's checks, as something [`ty_project::Project::check`] can run
///
/// registering this is what makes the command line and an editor report the same
/// rules in the same places: both reach the two functions above, and both reach
/// them through the same suppression and configuration.
#[derive(Debug, Default, Clone, Copy)]
pub struct DjangoChecker;

impl ty_project::ProjectChecker for DjangoChecker {
    fn owns(&self, db: &dyn Db, path: &SystemPath) -> bool {
        is_django_template_path(path) && project::has_django(db, db.project())
    }

    fn files(&self, db: &dyn Db) -> Vec<File> {
        let project = db.project();
        if !project::has_django(db, project) {
            return Vec::new();
        }

        project::template_files(db, project)
            .iter()
            // an installed app's templates are django's or a dependency's, and a
            // project is no more answerable for those than for the python beside them
            .filter(|discovered| discovered.own)
            // the same include, exclude and `ty check <paths>` filtering the python
            // files of the project go through
            .filter(|discovered| {
                matches!(
                    project.is_file_included(db, &discovered.path),
                    IncludeResult::Included { .. }
                )
            })
            .filter_map(|discovered| system_path_to_file(db, &discovered.path).ok())
            .collect()
    }

    fn check_file(&self, db: &dyn Db, file: File) -> Vec<Diagnostic> {
        django_template_diagnostics(db, file)
    }

    fn check_python_file(&self, db: &dyn Db, file: File) -> Vec<Diagnostic> {
        django_python_diagnostics(db, file)
    }

    fn django_settings_file(&self, db: &dyn Db) -> Option<File> {
        *project::settings_file(db, db.project())
    }
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

/// what the filter argument at `offset` of `file` takes, read as a django
/// template
pub fn django_template_signature_help(
    db: &dyn Db,
    file: File,
    offset: TextSize,
) -> Option<TemplateSignature> {
    let source = source_text(db, file);
    signature_help::signature_help(db, template_index(db, file), source.as_str(), offset)
}

/// the hints `range` of `file` shows, read as a django template
pub fn django_template_inlay_hints(
    db: &dyn Db,
    file: File,
    range: TextRange,
    settings: &InlayHintSettings,
) -> Vec<TemplateInlayHint> {
    let source = source_text(db, file);
    inlay_hints::inlay_hints(
        db,
        file,
        template_index(db, file),
        source.as_str(),
        range,
        settings,
    )
}

/// the project's `manage.py`, django's own entry point
///
/// the lenses above say what to run through it; this is what a caller that has to
/// actually run one needs, and a project without one is a project none of them
/// apply to.
pub fn django_manage_script(db: &dyn Db) -> Option<File> {
    *project::manage_file(db, db.project())
}

/// the lenses `file` shows, read as a django template
///
/// this is the view side of the join: a template is told what renders it, which
/// is the one thing about itself it cannot say.
pub fn django_template_code_lenses(db: &dyn Db, file: File) -> Vec<DjangoCodeLens> {
    code_lens::template_code_lenses(db, file)
}

/// the lenses `file` shows, read as one of the project's python modules
///
/// these are the `manage.py` invocations that apply to the file, and a module
/// django gives no role to has none of them.
pub fn django_python_code_lenses(db: &dyn Db, file: File) -> Vec<DjangoCodeLens> {
    code_lens::python_code_lenses(db, file)
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
    use ruff_ranged_value::RangedValue;
    use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};
    use std::ops::Range;
    use ty_module_resolver::SearchPathSettings;
    use ty_project::metadata::options::{Options, Rules};
    use ty_project::{ProjectMetadata, TestDb};
    use ty_python_core::platform::PythonPlatform;
    use ty_python_core::program::{FallibleStrategy, Program, ProgramSettings};
    use ty_python_semantic::PythonVersionWithSource;
    use ty_python_semantic::lint::Level;

    use crate::MarkupKind;

    use crate::InlayHintSettings;

    use super::{
        DjangoLensAction, PreparedTemplateRename, TemplateRenameOutcome, TemplateSymbol,
        django_prepare_rename, django_python_code_lenses, django_python_diagnostics,
        django_references, django_rename, django_template_code_lenses, django_template_completions,
        django_template_diagnostics, django_template_document_symbols,
        django_template_folding_ranges, django_template_goto_definition, django_template_hover,
        django_template_inlay_hints, django_template_signature_help, is_django_template_path,
    };

    /// a mock django whose implicit builtins are there to be read
    ///
    /// what it registers differs from the builtin table deliberately, and in both
    /// directions: it has a `{% squish %}` and a `|shorten` the table has never
    /// heard of, and it has no `{% lorem %}` or `|slugify` though the table does.
    /// that is what makes it stand in for a django the table has drifted from.
    pub(crate) const DJANGO_BUILTINS: &[(&str, &str)] = &[
        ("django/__init__.py", ""),
        ("django/template/__init__.py", ""),
        (
            "django/template/defaulttags.py",
            "
            from django.template import Library

            register = Library()

            @register.tag('for')
            def do_for(parser, token): ...

            @register.tag('if')
            def do_if(parser, token): ...

            @register.tag
            def squish(parser, token):
                '''squishes its body.'''
            ",
        ),
        (
            "django/template/defaultfilters.py",
            "
            from django.template import Library

            register = Library()

            @register.filter(is_safe=True)
            def upper(value): ...

            @register.filter
            def shorten(value, arg): ...
            ",
        ),
        (
            "django/template/loader_tags.py",
            "
            from django.template import Library

            register = Library()

            @register.tag('block')
            def do_block(parser, token): ...

            @register.tag('extends')
            def do_extends(parser, token): ...

            @register.tag('include')
            def do_include(parser, token): ...
            ",
        ),
    ];

    /// a mock django whose model base and admin machinery are there to be
    /// resolved
    ///
    /// what matters about it is where the classes are *declared*: a `Model` or a
    /// `ModelAdmin` counts as django's own because the module it comes from is
    /// django's, never because of the name written at the point of use.
    pub(crate) const DJANGO_ADMIN: &[(&str, &str)] = &[
        ("django/__init__.py", ""),
        ("django/db/__init__.py", ""),
        ("django/db/models/__init__.py", "class Model: ...\n"),
        ("django/contrib/__init__.py", ""),
        (
            "django/contrib/admin/__init__.py",
            "
            class AdminSite:
                def register(self, model_or_iterable, admin_class=None, **options): ...

            class ModelAdmin: ...

            class InlineModelAdmin: ...

            class TabularInline(InlineModelAdmin): ...

            site = AdminSite()

            def register(*models, site=None): ...
            ",
        ),
    ];

    /// `text` with the host's path separator written back as a forward slash
    ///
    /// the memory file system normalizes what a test writes to the separator of
    /// the machine running it, so every path that reaches an assertion — and
    /// every message quoting one — reads as `\src\...` on windows where the
    /// test says `/src/...`. it turns *every* backslash, so keep it off any
    /// rendering that can carry one of its own: a route pattern's `\d`, say.
    pub(crate) fn with_forward_slashes(text: impl std::fmt::Display) -> String {
        text.to_string().replace('\\', "/")
    }

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
        /// the project root the sources were written under, for a test that
        /// names a second file of its own
        root: SystemPathBuf,
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
            Self::with_rules(sources, installed, &[])
        }

        /// the same again, with some rules set to a level of their own
        ///
        /// the rule selection is part of the project's metadata, so a test that
        /// needs a rule turned off has to say so before the project is built.
        pub(crate) fn with_rules(
            sources: &[(&str, &str)],
            installed: &[(&str, &str)],
            rules: &[(&str, Level)],
        ) -> Self {
            let root = SystemPathBuf::from("/src");
            let site_packages = SystemPathBuf::from("/site-packages");

            let mut metadata = ProjectMetadata::new("test", root.clone());
            if !rules.is_empty() {
                metadata.apply_override_options(Options {
                    rules: Some(
                        rules
                            .iter()
                            .map(|(rule, level)| {
                                (
                                    RangedValue::cli((*rule).to_string()),
                                    RangedValue::cli(*level),
                                )
                            })
                            .collect::<Rules>(),
                    ),
                    ..Options::default()
                });
            }

            let mut db = TestDb::new(metadata);

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
            Self {
                db,
                file,
                offset,
                root: root.to_path_buf(),
            }
        }

        /// rewrite one of the project's files
        ///
        /// this is for a test that varies a source the fixture already wrote —
        /// the url configuration a route is mounted from, most often.
        pub(crate) fn rewrite(&mut self, path: &str, contents: &str) {
            self.db
                .write_file(self.root.join(path), dedent(contents).as_ref())
                .unwrap();
        }

        /// every django diagnostic of the python file at `path`, rendered as
        /// `rule severity: message [text]`
        ///
        /// what the type checker itself reports about the file is deliberately
        /// left out: this is the django join, and mixing the two would make every
        /// test below depend on how a mock django happens to be annotated.
        pub(crate) fn python_diagnostics(&self, path: &str) -> Vec<String> {
            let file = system_path_to_file(&self.db, self.root.join(path))
                .expect("the file to have been written");
            let source = ruff_db::source::source_text(&self.db, file);

            django_python_diagnostics(&self.db, file)
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

        /// the same, as the project check reports it
        ///
        /// [`python_diagnostics`](Self::python_diagnostics) is the raw scan, which
        /// answers before any `ty: ignore` is read. this is that scan folded into
        /// the type checker's own pass, which is where a suppression comment is
        /// honoured and counted used — so it is also what says whether
        /// `unused-ignore-comment` then fires on the comment that did the silencing.
        pub(crate) fn checked_python_diagnostics(&self, path: &str) -> Vec<String> {
            const REPORTED: &[&str] = &[
                "invalid-route-handler",
                "invalid-route-parameter-type",
                "unused-ignore-comment",
            ];

            let file = system_path_to_file(&self.db, self.root.join(path))
                .expect("the file to have been written");
            let external = django_python_diagnostics(&self.db, file);

            ty_python_semantic::check_file_with(&self.db, file, external)
                .expect("the file to be readable")
                .iter()
                // the mock django the fixtures install is annotated no further than
                // each test needs, so what the type checker itself says about a view
                // is no business of a test about the django join
                .filter(|diagnostic| {
                    diagnostic
                        .id()
                        .as_lint()
                        .is_some_and(|name| REPORTED.contains(&name.as_str()))
                })
                .map(|diagnostic| format!("{}: {}", diagnostic.id(), diagnostic.primary_message()))
                .collect()
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

        /// the workspace symbols matching `query`, as `container name [path]`
        ///
        /// what python contributes is rendered under `python`, so that a test
        /// says both what django added and what it left alone. the order is the
        /// order the files were walked in, which is no order at all, so it is
        /// sorted here rather than asserted on.
        pub(crate) fn workspace_symbols(&self, query: &str) -> Vec<String> {
            let mut found: Vec<String> = crate::workspace_symbols(&self.db, query)
                .into_iter()
                .map(|found| {
                    format!(
                        "{} {} [{}]",
                        found.container.unwrap_or("python"),
                        found.symbol.name,
                        with_forward_slashes(found.file.path(&self.db)),
                    )
                })
                .collect();
            found.sort();
            found
        }

        /// the labels of the completions at the cursor, in the order offered
        pub(crate) fn completions(&self) -> Vec<String> {
            django_template_completions(&self.db, self.file, self.offset)
                .into_iter()
                .map(|completion| completion.label)
                .collect()
        }

        /// the labels of the completions at the cursor django will not render
        pub(crate) fn unusable(&self) -> Vec<String> {
            django_template_completions(&self.db, self.file, self.offset)
                .into_iter()
                .filter(|completion| completion.unusable)
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
                        with_forward_slashes(target.file().path(&self.db)),
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

        /// the signature help at the cursor, as
        /// `label [parameter] — documentation`
        pub(crate) fn signature(&self) -> String {
            let Some(signature) = django_template_signature_help(&self.db, self.file, self.offset)
            else {
                return "no signature".to_string();
            };

            let parameter = signature
                .parameter
                .map(|parameter| format!(" [{parameter}]"))
                .unwrap_or_default();
            let documentation = signature
                .documentation
                .map(|documentation| format!(" — {documentation}"))
                .unwrap_or_default();

            format!("{}{parameter}{documentation}", signature.label)
        }

        /// every hint of the whole template, as ``kind at `text`: `label` ``
        pub(crate) fn hints(&self) -> Vec<String> {
            let source = ruff_db::source::source_text(&self.db, self.file);
            self.rendered_hints(
                TextRange::up_to(source.text_len()),
                &InlayHintSettings::default(),
            )
        }

        /// the same, restricted to a byte range of the template
        pub(crate) fn hints_in(&self, range: Range<u32>) -> Vec<String> {
            self.rendered_hints(
                TextRange::new(range.start.into(), range.end.into()),
                &InlayHintSettings::default(),
            )
        }

        /// the same, with only the settings `enable` turns on
        pub(crate) fn hints_with(
            &self,
            enable: impl FnOnce(&mut InlayHintSettings),
        ) -> Vec<String> {
            let mut settings = InlayHintSettings::none();
            enable(&mut settings);

            let source = ruff_db::source::source_text(&self.db, self.file);
            self.rendered_hints(TextRange::up_to(source.text_len()), &settings)
        }

        fn rendered_hints(&self, range: TextRange, settings: &InlayHintSettings) -> Vec<String> {
            let source = ruff_db::source::source_text(&self.db, self.file);

            django_template_inlay_hints(&self.db, self.file, range, settings)
                .into_iter()
                .map(|hint| {
                    let line_start = source.as_str()[..usize::from(hint.position)]
                        .rfind('\n')
                        .map_or(0, |index| index + 1);
                    let anchored = source.as_str()[line_start..usize::from(hint.position)]
                        .rsplit([' ', '{', '%'])
                        .next()
                        .unwrap_or_default();

                    with_forward_slashes(format_args!(
                        "{:?} at `{anchored}`: `{}`",
                        hint.kind, hint.label
                    ))
                })
                .collect()
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
                Some(PreparedTemplateRename::Refused(why)) => {
                    with_forward_slashes(format_args!("refused: {why}"))
                }
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
                    return vec![with_forward_slashes(format_args!("refused: {why}"))];
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
                        with_forward_slashes(edit.file().path(&self.db)),
                        &source[edit.range()]
                    )
                })
                .collect();

            if let Some((from, to)) = rename.file_rename {
                lines.push(format!(
                    "move {} -> {}",
                    with_forward_slashes(from),
                    with_forward_slashes(to)
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
                        with_forward_slashes(target.file().path(&self.db)),
                        &source[target.range()],
                    )
                })
                .collect()
        }

        /// every lens of the file under test, as `title -> what it does`
        ///
        /// which of the two implementations answers is the same question the
        /// server asks, so the harness asks it the same way.
        pub(crate) fn lenses(&self) -> Vec<String> {
            let lenses = if self.is_template() {
                django_template_code_lenses(&self.db, self.file)
            } else {
                django_python_code_lenses(&self.db, self.file)
            };

            lenses
                .into_iter()
                .map(|lens| {
                    let action = match lens.action {
                        DjangoLensAction::Run(arguments) => {
                            format!("manage.py {}", arguments.join(" "))
                        }
                        DjangoLensAction::Navigate(targets) => targets
                            .iter()
                            .map(|target| {
                                let source = ruff_db::source::source_text(&self.db, target.file);
                                format!(
                                    "{}:{}",
                                    with_forward_slashes(target.file.path(&self.db)),
                                    &source[target.range],
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                    };

                    format!("{} -> {action}", lens.title)
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

    #[test]
    fn a_jinja_file_is_not_claimed_as_a_django_template() {
        // see `TEMPLATE_EXTENSIONS`: answering a jinja file as django reports its
        // correct code as wrong
        assert!(!is_django_template_path(SystemPath::new(
            "/app/templates/page.jinja"
        )));
        assert!(!is_django_template_path(SystemPath::new(
            "/app/templates/page.html.jinja"
        )));
    }
}
