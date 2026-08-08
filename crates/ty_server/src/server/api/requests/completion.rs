use std::borrow::Cow;
use std::time::Instant;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList,
    CompletionParams, CompletionRequest, CompletionResponse, Documentation, InsertTextFormat,
    TextEdit, Uri,
};
use ruff_source_file::OneIndexed;
use ruff_text_size::Ranged;
use ty_ide::{CompletionCapabilities, CompletionInsertTextFormat, CompletionKind, completion};
use ty_project::ProjectDatabase;

use crate::document::{PositionExt, ToRangeExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct CompletionRequestHandler;

impl RequestHandler for CompletionRequestHandler {
    type RequestType = CompletionRequest;
}

impl BackgroundDocumentRequestHandler for CompletionRequestHandler {
    fn document_uri(params: &CompletionParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: CompletionParams,
    ) -> crate::server::Result<Option<CompletionResponse>> {
        let start = Instant::now();

        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let Some(offset) = params.text_document_position_params.position.to_text_size(
            db,
            file,
            snapshot.uri(),
            snapshot.encoding(),
        ) else {
            return Ok(None);
        };
        if snapshot.is_django_template() {
            return Ok(django_template_completions(db, snapshot, file, offset));
        }
        if triggered_by_a_template_character(&params) {
            // `{`, `%` and `|` are registered as trigger characters for the sake
            // of django templates, and LSP has no way to register them for one
            // language only. python gets the requests too, and a popup listing
            // every name in scope the moment the user opens a dict literal or
            // writes a union is not what they asked for.
            return Ok(None);
        }

        let client_capabilities = snapshot.resolved_client_capabilities();
        let completions = completion(
            db,
            snapshot.workspace_settings().completions(),
            CompletionCapabilities::default()
                .snippets(client_capabilities.supports_completion_item_snippets()),
            file,
            offset,
        );
        if completions.is_empty() {
            return Ok(None);
        }

        // Safety: we just checked that completions is not empty.
        let max_index_len = OneIndexed::new(completions.len()).unwrap().digits().get();
        let items: Vec<CompletionItem> = completions
            .into_iter()
            .enumerate()
            .map(|(i, comp)| {
                let kind = comp.kind.map(ty_kind_to_lsp_kind);
                // a format spec clause has no type to show, so it carries its
                // own few words instead
                let type_display = comp
                    .ty
                    .map(|ty| ty.display(db).to_string())
                    .or_else(|| comp.detail.as_ref().map(ToString::to_string));
                let import_edit = comp.import.as_ref().and_then(|edit| {
                    let range = edit
                        .range()
                        .to_lsp_range(db, file, snapshot.encoding())?
                        .local_range();
                    Some(TextEdit {
                        range,
                        new_text: edit.content().map(ToString::to_string).unwrap_or_default(),
                    })
                });

                let label = comp.label().to_string();
                let import_suffix = comp
                    .module_name
                    .and_then(|name| import_edit.is_some().then(|| format!(" (import {name})")));
                let (label, label_details) = if snapshot
                    .resolved_client_capabilities()
                    .supports_completion_item_label_details()
                {
                    let label_details = CompletionItemLabelDetails {
                        detail: import_suffix,
                        description: type_display.clone(),
                    };
                    (label, Some(label_details))
                } else {
                    let label = import_suffix
                        .map(|suffix| format!("{label}{suffix}"))
                        .unwrap_or(label);
                    (label, None)
                };

                let documentation = comp.documentation.map(|docstring| {
                    let (kind, value) = if snapshot
                        .resolved_client_capabilities()
                        .prefers_markdown_in_completion()
                    {
                        (lsp_types::MarkupKind::Markdown, docstring.render_markdown())
                    } else {
                        (
                            lsp_types::MarkupKind::PlainText,
                            docstring.render_plaintext(),
                        )
                    };

                    Documentation::MarkupContent(lsp_types::MarkupContent { kind, value })
                });
                let insert_text = comp.insert.map(String::from);
                let insert_text_format = match comp.insert_text_format {
                    CompletionInsertTextFormat::PlainText => None,
                    CompletionInsertTextFormat::Snippet => Some(InsertTextFormat::Snippet),
                };
                // A completion that says what it replaces is one the client's own
                // idea of the word under the cursor would get wrong, so the range
                // is spelled out as an edit rather than left to be guessed.
                let text_edit = comp.replace.and_then(|replace| {
                    let range = replace
                        .to_lsp_range(db, file, snapshot.encoding())?
                        .local_range();
                    Some(lsp_types::CompletionItemTextEdit::TextEdit(TextEdit {
                        range,
                        new_text: insert_text.clone().unwrap_or_else(|| label.clone()),
                    }))
                });

                CompletionItem {
                    label,
                    kind,
                    sort_text: Some(format!("{i:-max_index_len$}")),
                    detail: type_display,
                    label_details,
                    insert_text,
                    insert_text_format,
                    text_edit,
                    additional_text_edits: import_edit.map(|edit| vec![edit]),
                    documentation,
                    ..Default::default()
                }
            })
            .collect();
        let len = items.len();
        let response = CompletionResponse::CompletionList(CompletionList {
            is_incomplete: true,
            items,
            item_defaults: None,
            apply_kind: None,
        });
        tracing::debug!(
            "Completions request returned {len} suggestions in {elapsed:?}",
            elapsed = Instant::now().duration_since(start)
        );
        Ok(Some(response))
    }
}

impl RetriableRequestHandler for CompletionRequestHandler {
    const RETRY_ON_CANCELLATION: bool = true;
}

/// The characters that only ever open a django template construct.
///
/// Kept in step with the trigger characters the server registers in
/// `capabilities`, where the reason they are registered at all is spelled out.
const TEMPLATE_TRIGGER_CHARACTERS: &[&str] = &["{", "%", "|"];

/// Whether this request was triggered by typing a character that means nothing
/// outside a django template.
fn triggered_by_a_template_character(params: &CompletionParams) -> bool {
    params.context.as_ref().is_some_and(|context| {
        context.trigger_kind == lsp_types::CompletionTriggerKind::TriggerCharacter
            && context
                .trigger_character
                .as_deref()
                .is_some_and(|character| TEMPLATE_TRIGGER_CHARACTERS.contains(&character))
    })
}

/// The completion response for a django template document.
///
/// Unlike the python ones, every template completion carries the range it
/// replaces: a template path or a namespaced url name is not a word by any
/// client's definition of one, so leaving the replacement to the client would
/// mangle them.
fn django_template_completions(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    file: ruff_db::files::File,
    offset: ruff_text_size::TextSize,
) -> Option<CompletionResponse> {
    let completions = ty_ide::django_template_completions(db, file, offset);
    if completions.is_empty() {
        return None;
    }

    // Safety: we just checked that completions is not empty.
    let max_index_len = OneIndexed::new(completions.len()).unwrap().digits().get();
    let to_edit = |edit: &ty_ide::TemplateEdit| {
        Some(TextEdit {
            range: edit
                .range
                .to_lsp_range(db, file, snapshot.encoding())?
                .local_range(),
            new_text: edit.text.clone(),
        })
    };

    let items: Vec<CompletionItem> = completions
        .into_iter()
        .enumerate()
        .map(|(index, completion)| {
            let text_edit = completion
                .range
                .to_lsp_range(db, file, snapshot.encoding())
                .map(|range| {
                    lsp_types::CompletionItemTextEdit::TextEdit(TextEdit {
                        range: range.local_range(),
                        new_text: completion
                            .insert
                            .clone()
                            .unwrap_or_else(|| completion.label.clone()),
                    })
                });

            let documentation = completion.documentation.map(|value| {
                let kind = if snapshot
                    .resolved_client_capabilities()
                    .prefers_markdown_in_completion()
                {
                    lsp_types::MarkupKind::Markdown
                } else {
                    lsp_types::MarkupKind::PlainText
                };

                Documentation::MarkupContent(lsp_types::MarkupContent { kind, value })
            });

            CompletionItem {
                label: completion.label,
                kind: Some(ty_kind_to_lsp_kind(completion.kind)),
                // the order the suggestions arrive in is the order they are
                // meant to be shown in
                sort_text: Some(format!("{index:-max_index_len$}")),
                detail: completion.detail,
                documentation,
                text_edit,
                additional_text_edits: completion
                    .additional_edit
                    .as_ref()
                    .and_then(to_edit)
                    .map(|edit| vec![edit]),
                ..Default::default()
            }
        })
        .collect();

    Some(CompletionResponse::CompletionList(CompletionList {
        is_incomplete: true,
        items,
        item_defaults: None,
        apply_kind: None,
    }))
}

fn ty_kind_to_lsp_kind(kind: CompletionKind) -> CompletionItemKind {
    // Gimme my dang globs in tight scopes!
    #[allow(clippy::enum_glob_use)]
    use self::CompletionKind::*;

    // ref https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#completionItemKind
    match kind {
        Text => CompletionItemKind::Text,
        Method => CompletionItemKind::Method,
        Function => CompletionItemKind::Function,
        Constructor => CompletionItemKind::Constructor,
        Field => CompletionItemKind::Field,
        Variable => CompletionItemKind::Variable,
        Class => CompletionItemKind::Class,
        Interface => CompletionItemKind::Interface,
        Module => CompletionItemKind::Module,
        Property => CompletionItemKind::Property,
        Unit => CompletionItemKind::Unit,
        Value => CompletionItemKind::Value,
        Enum => CompletionItemKind::Enum,
        Keyword => CompletionItemKind::Keyword,
        Snippet => CompletionItemKind::Snippet,
        Color => CompletionItemKind::Color,
        File => CompletionItemKind::File,
        Reference => CompletionItemKind::Reference,
        Folder => CompletionItemKind::Folder,
        EnumMember => CompletionItemKind::EnumMember,
        Constant => CompletionItemKind::Constant,
        Struct => CompletionItemKind::Struct,
        Event => CompletionItemKind::Event,
        Operator => CompletionItemKind::Operator,
        TypeParameter => CompletionItemKind::TypeParameter,
    }
}
