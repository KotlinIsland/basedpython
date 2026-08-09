use lsp_types::{
    LanguageKind, TextDocumentContentChangeEvent, TextDocumentContentChangePartial,
    TextDocumentContentChangeWholeDocument, Uri,
};
use ruff_python_ast::PySourceType;
use ruff_source_file::LineIndex;

use crate::PositionEncoding;
use crate::document::DocumentKey;
use crate::document::range::lsp_range_to_text_range;
use crate::system::AnySystemPath;

pub(crate) type DocumentVersion = i32;

/// A regular text file or the content of a notebook cell.
///
/// The state of an individual document in the server. Stays up-to-date
/// with changes made by the user, including unsaved changes.
#[derive(Debug, Clone)]
pub struct TextDocument {
    /// The URI as sent by the client
    uri: Uri,

    /// The string contents of the document.
    contents: String,

    /// The latest version of the document, set by the LSP client. The server will panic in
    /// debug mode if we attempt to update the document with an 'older' version.
    version: DocumentVersion,

    /// The language ID of the document as provided by the client.
    language_id: LanguageId,

    /// For cells, the path to the notebook document.
    notebook: Option<AnySystemPath>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LanguageId {
    Python,
    /// A django template. Not python, but the server still has language services
    /// for it — see [`ty_ide::django_template_completions`].
    DjangoTemplate,
    Other,
}

impl LanguageId {
    /// The language of a document the client has opened at `path`.
    ///
    /// Editors disagree about what to call a django template: the vs code django
    /// extensions use `django-html`, vim uses `htmldjango`, and a great many
    /// setups just call it `html`. That last case is why the path is consulted
    /// at all — a `.html` file inside a `templates` directory is a django
    /// template whatever the client chose to call it.
    ///
    /// A jinja id is deliberately turned away rather than taken as django. The
    /// two languages read alike, but every template service here answers as
    /// django, so a jinja document routed to them has its correct `{% set %}`,
    /// `{% macro %}` or `|default("x")` reported as an error — a false positive
    /// on correct code. We have no jinja support to offer, and [`Self::Other`] —
    /// no template services at all — is the honest answer. It is answered
    /// outright, ahead of the path fallback, because a client that says `jinja`
    /// has settled the question that fallback exists to guess at.
    ///
    /// Editors disagree about what to call python too — `python3`, `py` and
    /// `basedpython` are all in use — and [`Self::Other`] is a document the
    /// server never adds to the project, so an id we do not know would leave a
    /// `.py` file with no diagnostics and every service reading the file as last
    /// saved rather than as the client has it. So a path python owns is python,
    /// exactly as a path django owns is a template.
    pub(crate) fn new(language_id: &LanguageKind, path: &AnySystemPath) -> Self {
        match language_id.as_str() {
            "python" | "by" | "basedpython" => Self::Python,
            "django-html" | "django-txt" | "htmldjango" | "django" => Self::DjangoTemplate,
            "jinja" | "jinja-html" | "jinja2" => Self::Other,
            _ => match path.as_system() {
                Some(path) if ty_ide::is_django_template_path(path) => Self::DjangoTemplate,
                Some(path) if PySourceType::try_from_path(path.as_std_path()).is_some() => {
                    Self::Python
                }
                _ => Self::Other,
            },
        }
    }

    pub(crate) const fn is_django_template(self) -> bool {
        matches!(self, Self::DjangoTemplate)
    }
}

impl TextDocument {
    pub fn new(
        uri: Uri,
        contents: String,
        version: DocumentVersion,
        language_id: &LanguageKind,
    ) -> Self {
        let language_id =
            LanguageId::new(language_id, &DocumentKey::from_uri(&uri).into_file_path());

        Self {
            uri,
            contents,
            version,
            language_id,
            notebook: None,
        }
    }

    #[must_use]
    pub(crate) fn with_notebook(mut self, notebook: AnySystemPath) -> Self {
        self.notebook = Some(notebook);
        self
    }

    pub fn into_contents(self) -> String {
        self.contents
    }

    pub(crate) fn uri(&self) -> &Uri {
        &self.uri
    }

    pub fn contents(&self) -> &str {
        &self.contents
    }

    pub fn version(&self) -> DocumentVersion {
        self.version
    }

    pub fn language_id(&self) -> LanguageId {
        self.language_id
    }

    pub(crate) fn notebook(&self) -> Option<&AnySystemPath> {
        self.notebook.as_ref()
    }

    pub fn apply_changes(
        &mut self,
        changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
        new_version: DocumentVersion,
        encoding: PositionEncoding,
    ) {
        if let [
            lsp_types::TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                TextDocumentContentChangeWholeDocument { text },
            ),
        ] = changes.as_slice()
        {
            tracing::debug!("Fast path - replacing entire document");
            self.modify(|contents, version| {
                contents.clone_from(text);
                *version = new_version;
            });
            return;
        }

        let mut new_contents = self.contents().to_string();
        let mut active_index = LineIndex::from_source_text(&new_contents);

        for change in changes {
            match change {
                TextDocumentContentChangeEvent::TextDocumentContentChangePartial(
                    TextDocumentContentChangePartial { range, text, .. },
                ) => {
                    let range =
                        lsp_range_to_text_range(range, &new_contents, &active_index, encoding);

                    new_contents
                        .replace_range(usize::from(range.start())..usize::from(range.end()), &text);
                }
                TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                    TextDocumentContentChangeWholeDocument { text },
                ) => {
                    new_contents = text;
                }
            }

            active_index = LineIndex::from_source_text(&new_contents);
        }

        self.modify(|contents, version| {
            *contents = new_contents;
            *version = new_version;
        });
    }

    pub fn update_version(&mut self, new_version: DocumentVersion) {
        self.modify(|_, version| {
            *version = new_version;
        });
    }

    // A private function for overriding how we update the line index by default.
    fn modify(&mut self, func: impl FnOnce(&mut String, &mut DocumentVersion)) {
        let old_version = self.version;
        func(&mut self.contents, &mut self.version);
        debug_assert!(self.version >= old_version);
    }
}

#[cfg(test)]
mod tests {
    use crate::document::text_document::LanguageId;
    use crate::{PositionEncoding, TextDocument};
    use lsp_types::{
        LanguageKind, Position, TextDocumentContentChangeEvent, TextDocumentContentChangePartial,
        Uri,
    };

    /// the language a client opening `path` with `language_id` gets
    ///
    /// `path` is absolute and written the posix way. windows has no path
    /// without a drive and `Uri::to_file_path` rejects one, so a uri the tests
    /// wrote out by hand would be no path at all there — and every case that
    /// leans on the path would answer [`LanguageId::Other`] for that reason
    /// rather than the one under test.
    fn opened(path: &str, language_id: &str) -> LanguageId {
        let uri = if cfg!(windows) {
            format!("file:///C:{path}")
        } else {
            format!("file://{path}")
        };

        TextDocument::new(
            Uri::parse(&uri).unwrap(),
            String::new(),
            0,
            &LanguageKind::from(language_id.to_string()),
        )
        .language_id()
    }

    #[test]
    fn the_editors_django_language_ids_are_django_templates() {
        for id in ["django", "django-html", "django-txt", "htmldjango"] {
            assert_eq!(
                opened("/app/page.html", id),
                LanguageId::DjangoTemplate,
                "`{id}` is a django template"
            );
        }
    }

    #[test]
    fn a_jinja_document_gets_no_template_services() {
        // jinja is a different language that reads alike, and everything the
        // template services answer with is django's — see `LanguageId::new`
        for id in ["jinja", "jinja-html", "jinja2"] {
            assert_eq!(
                opened("/app/templates/page.html", id),
                LanguageId::Other,
                "`{id}` is turned away even where the path says django"
            );
        }

        assert_eq!(
            opened("/app/templates/page.jinja", "plaintext"),
            LanguageId::Other
        );
    }

    #[test]
    fn the_editors_python_language_ids_are_python() {
        for id in ["python", "by", "basedpython"] {
            assert_eq!(
                opened("/app/mod.by", id),
                LanguageId::Python,
                "`{id}` is python"
            );
        }
    }

    #[test]
    fn a_python_path_is_python_whatever_the_client_calls_it() {
        // an id we do not know would otherwise leave the file out of the project
        // entirely: no diagnostics, and every service reading it as last saved
        for (path, id) in [
            ("/app/mod.py", "python3"),
            ("/app/mod.py", "py"),
            ("/app/mod.by", "basedpython-lang"),
            ("/app/mod.pyi", ""),
            ("/app/mod.byi", "plaintext"),
        ] {
            assert_eq!(
                opened(path, id),
                LanguageId::Python,
                "`{path}` called `{id}` is python"
            );
        }

        assert_eq!(opened("/app/notes.txt", "plaintext"), LanguageId::Other);
    }

    #[test]
    fn an_html_file_under_templates_is_a_django_template_whatever_it_is_called() {
        assert_eq!(
            opened("/app/templates/page.html", "html"),
            LanguageId::DjangoTemplate
        );
        assert_eq!(opened("/app/page.html", "html"), LanguageId::Other);
    }

    #[test]
    fn redo_edit() {
        let mut document = TextDocument::new(
            Uri::parse("file:///test").unwrap(),
            r#""""
测试comment
一些测试内容
"""
import click


@click.group()
def interface():
    pas
"#
            .to_string(),
            0,
            &LanguageKind::Python,
        );

        // Add an `s`, remove it again (back to the original code), and then re-add the `s`
        document.apply_changes(
            vec![
                TextDocumentContentChangeEvent::TextDocumentContentChangePartial(
                    TextDocumentContentChangePartial {
                        range: lsp_types::Range::new(Position::new(9, 7), Position::new(9, 7)),
                        text: "s".to_string(),
                        ..Default::default()
                    },
                ),
                TextDocumentContentChangeEvent::TextDocumentContentChangePartial(
                    TextDocumentContentChangePartial {
                        range: lsp_types::Range::new(Position::new(9, 7), Position::new(9, 8)),
                        text: String::new(),
                        ..Default::default()
                    },
                ),
                TextDocumentContentChangeEvent::TextDocumentContentChangePartial(
                    TextDocumentContentChangePartial {
                        range: lsp_types::Range::new(Position::new(9, 7), Position::new(9, 7)),
                        text: "s".to_string(),
                        ..Default::default()
                    },
                ),
            ],
            1,
            PositionEncoding::UTF16,
        );

        assert_eq!(
            &document.contents,
            r#""""
测试comment
一些测试内容
"""
import click


@click.group()
def interface():
    pass
"#
        );
    }
}
