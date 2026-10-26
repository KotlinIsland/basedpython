//! `buff/formatSettings` — the formatter settings `buff` lays a document out with
//!
//! what an editor shows as the document's code style, and where it draws its right margin, has to
//! be what the formatter will do: `buff` resolves them from the project's configuration, its
//! `pyproject.toml` or `ruff.toml` and whatever the editor's settings override in it, and that
//! resolution is the server's alone. an editor reading `line-length` out of a file itself would
//! pick the wrong file in a nested project, and miss the defaults and the editor's own overrides

use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier, Uri};
use ruff_formatter::IndentStyle;
use ruff_python_formatter::QuoteStyle;
use ruff_workspace::LineEnding;

use crate::server::Result;
use crate::session::{Client, DocumentSnapshot};

pub(crate) enum FormatSettingsRequest {}

impl Request for FormatSettingsRequest {
    type Params = FormatSettingsParams;
    type Result = Option<FormatSettings>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("buff/formatSettings");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the document to answer for. a field it does not name, a `workDoneToken` among them, is
/// ignored, as LSP's own requests ignore one
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FormatSettingsParams {
    text_document: TextDocumentIdentifier,
}

/// the settings, as the configuration spells them
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FormatSettings {
    /// `line-length`: the column a line is wrapped before
    line_length: u16,
    /// `indent-width`: the columns one level of indentation takes
    indent_width: u32,
    /// `indent-style`: `space` or `tab`
    indent_style: String,
    /// `quote-style`: `double`, `single` or `preserve`
    quote_style: String,
    /// `line-ending`: `auto`, `lf`, `cr-lf` or `native`
    line_ending: String,
    /// the configuration file the settings were read from, `null` when no file configures the
    /// document and the settings are the defaults with the editor's overrides
    configuration_file: Option<Uri>,
}

pub(crate) struct FormatSettingsHandler;

impl super::RequestHandler for FormatSettingsHandler {
    type RequestType = FormatSettingsRequest;
}

impl super::BackgroundDocumentRequestHandler for FormatSettingsHandler {
    super::define_document_uri!(params: &FormatSettingsParams);

    /// `null` for a document the editor has not opened
    fn run_with_snapshot(
        snapshot: Self::Snapshot,
        _client: &Client,
        _params: FormatSettingsParams,
    ) -> Result<Option<FormatSettings>> {
        let snapshot: DocumentSnapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(uri) => {
                tracing::warn!("Returning no format settings because document `{uri}` isn't open.");
                return Ok(None);
            }
        };
        let settings = snapshot.query().settings();
        let formatter = &settings.formatter;
        Ok(Some(FormatSettings {
            line_length: formatter.line_width.value(),
            indent_width: formatter.indent_width.value(),
            indent_style: match formatter.indent_style {
                IndentStyle::Space => "space",
                IndentStyle::Tab => "tab",
            }
            .to_string(),
            quote_style: match formatter.quote_style {
                QuoteStyle::Double => "double",
                QuoteStyle::Single => "single",
                QuoteStyle::Preserve => "preserve",
            }
            .to_string(),
            line_ending: match formatter.line_ending {
                LineEnding::Auto => "auto",
                LineEnding::Lf => "lf",
                LineEnding::CrLf => "cr-lf",
                LineEnding::Native => "native",
            }
            .to_string(),
            configuration_file: settings
                .path()
                .and_then(|path| Uri::from_file_path(path).ok()),
        }))
    }
}
