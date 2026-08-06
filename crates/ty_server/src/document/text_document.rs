use lsp_types::{
    LanguageKind, TextDocumentContentChangeEvent, TextDocumentContentChangePartial,
    TextDocumentContentChangeWholeDocument, Uri,
};
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
    pub(crate) fn new(language_id: &LanguageKind, path: &AnySystemPath) -> Self {
        match language_id.as_str() {
            "python" | "by" => Self::Python,
            "django-html" | "django-txt" | "htmldjango" | "jinja-html" | "django" => {
                Self::DjangoTemplate
            }
            _ => match path.as_system() {
                Some(path) if ty_ide::is_django_template_path(path) => Self::DjangoTemplate,
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
    use crate::{PositionEncoding, TextDocument};
    use lsp_types::{
        LanguageKind, Position, TextDocumentContentChangeEvent, TextDocumentContentChangePartial,
        Uri,
    };

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
