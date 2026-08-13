use std::borrow::Cow;

use crate::document::{PositionEncoding, PositionExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;
use lsp_types::{ActiveParameter, SignatureHelpRequest};
use lsp_types::{
    Documentation, ParameterInformation, ParameterInformationLabel, SignatureHelp,
    SignatureHelpParams, SignatureInformation, Uri,
};
use ty_ide::{TemplateSignature, django_template_signature_help, signature_help};
use ty_project::{ProjectDatabase, SemanticDb as _};
use ty_python_semantic::ProgramEnvironment;

pub(crate) struct SignatureHelpRequestHandler;

impl RequestHandler for SignatureHelpRequestHandler {
    type RequestType = SignatureHelpRequest;
}

impl BackgroundDocumentRequestHandler for SignatureHelpRequestHandler {
    fn document_uri(params: &SignatureHelpParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: SignatureHelpParams,
    ) -> crate::server::Result<Option<SignatureHelp>> {
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
            return Ok(django_template_signature_help(
                db,
                &ProgramEnvironment::from_file(db.program_file(file)),
                file,
                offset,
            )
            .map(template_signature));
        }

        if triggered_by_a_template_character(&params) {
            // `:` is registered as a trigger character for the sake of django
            // templates, and LSP has no way to register it for one language
            // only. python gets the requests too, and a popup the moment the
            // user annotates a parameter or opens a slice is not what they
            // asked for.
            return Ok(None);
        }

        // Extract signature help capabilities from the client
        let resolved_capabilities = snapshot.resolved_client_capabilities();

        let Some(signature_help_info) = signature_help(db, db.program_file(file), offset) else {
            return Ok(None);
        };

        // Compute active parameter from the active signature
        let active_parameter = signature_help_info
            .active_signature
            .and_then(|s| signature_help_info.signatures.get(s))
            .and_then(|sig| sig.active_parameter)
            .and_then(|p| u32::try_from(p).ok())
            .map(ActiveParameter::Int);

        // Convert from IDE types to LSP types
        let signatures = signature_help_info
            .signatures
            .into_iter()
            .map(|sig| {
                let parameters = sig
                    .parameters
                    .into_iter()
                    .map(|param| {
                        let label = if resolved_capabilities.supports_signature_label_offset() {
                            // Find the parameter's offset in the signature label
                            if let Some(start) = sig.label.find(&param.label) {
                                let encoding = snapshot.encoding();

                                // Convert byte offsets to character offsets based on negotiated encoding
                                let start_char_offset = match encoding {
                                    PositionEncoding::UTF8 => start,
                                    PositionEncoding::UTF16 => {
                                        sig.label[..start].encode_utf16().count()
                                    }
                                    PositionEncoding::UTF32 => sig.label[..start].chars().count(),
                                };

                                let end_char_offset = match encoding {
                                    PositionEncoding::UTF8 => start + param.label.len(),
                                    PositionEncoding::UTF16 => sig.label
                                        [..start + param.label.len()]
                                        .encode_utf16()
                                        .count(),
                                    PositionEncoding::UTF32 => {
                                        sig.label[..start + param.label.len()].chars().count()
                                    }
                                };

                                let start_u32 =
                                    u32::try_from(start_char_offset).unwrap_or(u32::MAX);
                                let end_u32 = u32::try_from(end_char_offset).unwrap_or(u32::MAX);
                                ParameterInformationLabel::Tuple((start_u32, end_u32))
                            } else {
                                ParameterInformationLabel::String(param.label)
                            }
                        } else {
                            ParameterInformationLabel::String(param.label)
                        };

                        ParameterInformation {
                            label,
                            documentation: param.documentation.map(Documentation::String),
                        }
                    })
                    .collect();

                let active_parameter =
                    if resolved_capabilities.supports_signature_active_parameter() {
                        sig.active_parameter
                            .and_then(|p| u32::try_from(p).ok())
                            .map(ActiveParameter::Int)
                    } else {
                        None
                    };

                SignatureInformation {
                    label: sig.label,
                    documentation: sig
                        .documentation
                        .map(|docstring| Documentation::String(docstring.render_plaintext())),
                    parameters: Some(parameters),
                    active_parameter,
                }
            })
            .collect();

        let signature_help = SignatureHelp {
            signatures,
            active_signature: signature_help_info
                .active_signature
                .and_then(|s| u32::try_from(s).ok()),
            active_parameter,
        };

        Ok(Some(signature_help))
    }
}

impl RetriableRequestHandler for SignatureHelpRequestHandler {}

/// The trigger character registered for django templates alone, listed in
/// `capabilities`, where the reason it is registered at all is spelled out.
const TEMPLATE_TRIGGER_CHARACTER: &str = ":";

/// Whether this request was triggered by typing a character that opens no
/// argument outside a django template.
fn triggered_by_a_template_character(params: &SignatureHelpParams) -> bool {
    params.context.as_ref().is_some_and(|context| {
        context.trigger_kind == lsp_types::SignatureHelpTriggerKind::TriggerCharacter
            && context.trigger_character.as_deref() == Some(TEMPLATE_TRIGGER_CHARACTER)
    })
}

/// a django filter's argument, as the one signature the client is offered
///
/// the parameter is written as a plain string rather than as an offset into the
/// label: a template's signature is short enough that a client can find it, and a
/// label offset would have to be re-encoded for the client's position encoding.
fn template_signature(signature: TemplateSignature) -> SignatureHelp {
    let parameters: Vec<_> = signature
        .parameter
        .into_iter()
        .map(|parameter| ParameterInformation {
            label: ParameterInformationLabel::String(parameter),
            documentation: None,
        })
        .collect();
    let active_parameter = (!parameters.is_empty()).then_some(ActiveParameter::Int(0));

    SignatureHelp {
        signatures: vec![SignatureInformation {
            label: signature.label,
            documentation: signature.documentation.map(Documentation::String),
            parameters: Some(parameters),
            active_parameter,
        }],
        active_signature: Some(0),
        active_parameter,
    }
}
