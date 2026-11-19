//! `by/inlayHintCatalog` — every kind of inlay hint, for an editor to list them
//!
//! an editor that lets its user switch each kind of hint, or draw it its own way, lists the kinds
//! before any hint arrives: each hint names its kind as `data.kind`, the `inlayHints` option that
//! switches it, and this is the list those names come from, each with a title, a family to group it
//! under, a line saying what it shows, which side of the code it belongs with, and an example
//! program with the hints drawn on it.
//!
//! what the kinds are is `ty_ide`'s, beside the kinds themselves, so this only puts it on the wire.
//! an example's hints are not written down anywhere: they are computed for each request, by the
//! same code and in the same shape as `textDocument/inlayHint`'s, so a preview is what this server
//! draws in a file

use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ty_ide::{
    AnyInlayHintKind, INLAY_HINT_CATALOG, InlayHintAttachment, InlayHintExamples, InlayHintFamily,
    InlayHintFileKind,
};

use super::inlay_hints::lsp_inlay_hint;
use crate::PositionEncoding;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum InlayHintCatalogRequest {}

impl Request for InlayHintCatalogRequest {
    type Params = Option<InlayHintCatalogParams>;
    type Result = InlayHintCatalog;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/inlayHintCatalog");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// nothing, an empty object or no `params` at all: the kinds are the server's, whatever the
/// project
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the brackets make it the empty object a client sends, which a unit struct is not"
)]
pub(crate) struct InlayHintCatalogParams {}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayHintCatalog {
    /// the families the kinds are grouped under, in the order they are shown
    groups: Vec<InlayHintGroup>,
    /// every kind, in the order it is shown: family by family
    kinds: Vec<InlayHintCatalogKind>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayHintGroup {
    id: String,
    title: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayHintCatalogKind {
    /// the `inlayHints` option that switches the kind, and what its hints carry as `data.kind`
    setting: String,
    title: String,
    /// the `id` of its group
    group: String,
    /// one or two plain sentences, no markup
    description: String,
    attaches_to: Attachment,
    file_kind: FileKind,
    /// `null` for a kind no standalone program shows
    example: Option<InlayHintCatalogExample>,
}

/// which code a hint belongs with: the side of it a caret typed at its offset lands on
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Attachment {
    /// it completes the code before it: `: int`, `-> int`, `raises E`
    Preceding,
    /// it introduces the code after it: `x=`, `override`
    Following,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FileKind {
    /// python and basedpython files
    Source,
    /// django templates
    Template,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayHintCatalogExample {
    source: String,
    /// every hint this server draws on `source` with every kind enabled, of whichever kind, as
    /// `textDocument/inlayHint` returns them, without what only means something in a file of a
    /// project: where a label part leads, and the edits that would write a hint into the source
    hints: Vec<lsp_types::InlayHint>,
}

pub(crate) struct InlayHintCatalogHandler;

impl RequestHandler for InlayHintCatalogHandler {
    type RequestType = InlayHintCatalogRequest;
}

impl BackgroundRequestHandler for InlayHintCatalogHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        _params: Option<InlayHintCatalogParams>,
    ) -> crate::server::Result<InlayHintCatalog> {
        // the examples are a database of their own, which reading them attaches. that is legal
        // here because a handler that is not a document's runs with no database attached, unlike
        // one that is, which runs inside the session's (see `detached_transpile`)
        Ok(inlay_hint_catalog(snapshot.position_encoding()))
    }
}

impl RetriableRequestHandler for InlayHintCatalogHandler {}

/// every kind of hint, with its example drawn
fn inlay_hint_catalog(encoding: PositionEncoding) -> InlayHintCatalog {
    let start = std::time::Instant::now();
    let examples = InlayHintExamples::new();

    let groups = <InlayHintFamily as strum::IntoEnumIterator>::iter()
        .map(|family| InlayHintGroup {
            id: family.id().to_string(),
            title: family.title().to_string(),
        })
        .collect();

    let kinds = INLAY_HINT_CATALOG
        .iter()
        .map(|&kind| {
            let about = kind.about();
            let example = match (kind, &examples) {
                (AnyInlayHintKind::Source(kind), Some(examples)) => {
                    examples.hints(kind).map(|(file, hints)| {
                        let db = examples.db();
                        InlayHintCatalogExample {
                            source: about.example.unwrap_or_default().to_string(),
                            hints: hints
                                .into_iter()
                                .filter_map(|hint| lsp_inlay_hint(hint, db, file, encoding))
                                .map(scratch_hint)
                                .collect(),
                        }
                    })
                }
                _ => None,
            };
            InlayHintCatalogKind {
                setting: kind.setting().to_string(),
                title: about.title.to_string(),
                group: about.family.id().to_string(),
                description: about.description.to_string(),
                attaches_to: match about.attaches_to {
                    InlayHintAttachment::Preceding => Attachment::Preceding,
                    InlayHintAttachment::Following => Attachment::Following,
                },
                file_kind: match kind.file_kind() {
                    InlayHintFileKind::Source => FileKind::Source,
                    InlayHintFileKind::Template => FileKind::Template,
                },
                example,
            }
        })
        .collect();

    tracing::debug!("drew the inlay hint examples in {:?}", start.elapsed());
    InlayHintCatalog { groups, kinds }
}

/// `hint` as a preview draws it: an example is no file of a project, so a label part leads nowhere
/// and there is no source to write the hint into
fn scratch_hint(mut hint: lsp_types::InlayHint) -> lsp_types::InlayHint {
    if let lsp_types::Label::InlayHintLabelPartList(parts) = &mut hint.label {
        for part in parts {
            part.location = None;
        }
    }
    hint.text_edits = None;
    hint
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the wire form, exactly as a client sends it: no parameters at all, or an empty object
    #[test]
    fn the_params_a_client_sends_parse() {
        let empty: Option<InlayHintCatalogParams> =
            serde_json::from_str("{}").expect("an empty object");
        assert!(empty.is_some());
        let absent: Option<InlayHintCatalogParams> =
            serde_json::from_value(serde_json::Value::Null).expect("no params");
        assert!(absent.is_none());
        assert!(serde_json::from_str::<Option<InlayHintCatalogParams>>(r#"{"x": 1}"#).is_err());
    }

    /// every source kind has an example, drawn with a hint of its own kind, tagged as a file's
    /// hints are; the template kinds have none
    #[test]
    fn every_source_kind_has_an_example_with_its_own_hint() {
        let catalog = inlay_hint_catalog(PositionEncoding::UTF16);
        assert_eq!(catalog.kinds.len(), INLAY_HINT_CATALOG.len());
        for kind in &catalog.kinds {
            match kind.file_kind {
                FileKind::Source => {
                    let example = kind
                        .example
                        .as_ref()
                        .unwrap_or_else(|| panic!("{} has no example", kind.setting));
                    assert!(
                        example.hints.iter().any(|hint| {
                            hint.data.as_ref().and_then(|data| data["kind"].as_str())
                                == Some(kind.setting.as_str())
                        }),
                        "{}'s example shows no hint of its own",
                        kind.setting
                    );
                    assert!(example.hints.iter().all(|hint| hint.text_edits.is_none()));
                }
                FileKind::Template => assert!(kind.example.is_none()),
            }
        }
    }

    #[test]
    fn the_catalog_serializes_as_a_client_reads_it() {
        let json = serde_json::to_value(inlay_hint_catalog(PositionEncoding::UTF16)).expect("json");
        assert_eq!(json["groups"][0]["id"], "types");
        let returns = json["kinds"]
            .as_array()
            .expect("kinds")
            .iter()
            .find(|kind| kind["setting"] == "returnTypes")
            .expect("returnTypes");
        assert_eq!(returns["attachesTo"], "preceding");
        assert_eq!(returns["fileKind"], "source");
        assert_eq!(returns["group"], "types");
        let template = json["kinds"]
            .as_array()
            .expect("kinds")
            .iter()
            .find(|kind| kind["setting"] == "resolvedTemplates")
            .expect("resolvedTemplates");
        assert_eq!(template["fileKind"], "template");
        assert!(template["example"].is_null());
    }
}
