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

mod builtins;
mod completion;
mod goto;
mod index;
mod lexer;
mod project;
mod resolve;
mod semantic_tokens;

pub use completion::{TemplateCompletion, TemplateEdit};

use ruff_db::files::File;
use ruff_db::source::source_text;
use ruff_db::system::SystemPath;
use ruff_text_size::{TextRange, TextSize};
use ty_project::Db;

use crate::semantic_tokens::SemanticTokens;
use crate::{NavigationTargets, RangedValue};

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
    use ruff_db::files::{File, system_path_to_file};
    use ruff_db::system::{DbWithWritableSystem, SystemPath, SystemPathBuf};
    use ruff_python_ast::PythonVersion;
    use ruff_python_trivia::textwrap::dedent;
    use ruff_text_size::TextSize;
    use ty_project::{ProjectMetadata, TestDb};

    use super::{
        django_template_completions, django_template_goto_definition, is_django_template_path,
    };

    /// a project whose files are written out, with the cursor marked by
    /// `<CURSOR>` in exactly one of them
    pub(crate) struct TemplateTest {
        pub(crate) db: TestDb,
        pub(crate) file: File,
        pub(crate) offset: TextSize,
    }

    impl TemplateTest {
        /// build a project from `(path, contents)` pairs
        pub(crate) fn new(sources: &[(&str, &str)]) -> Self {
            const MARKER: &str = "<CURSOR>";

            let mut db = TestDb::new(ProjectMetadata::new("test", SystemPathBuf::from("/")));
            db.init_program_with_python_version(PythonVersion::latest_ty())
                .unwrap();

            let mut cursor = None;

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

                db.write_file(path, &contents).unwrap();
                let file = system_path_to_file(&db, path).unwrap();

                if let Some(offset) = offset {
                    assert!(cursor.is_none(), "more than one `<CURSOR>` marker");
                    cursor = Some((file, offset));
                }
            }

            let (file, offset) = cursor.expect("a source to contain `<CURSOR>`");
            Self { db, file, offset }
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
