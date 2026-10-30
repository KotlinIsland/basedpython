//! `by/validateRenameName` and `by/renameNameSuggestions`: what a rename's new name may be, asked
//! while the user types it
//!
//! a rename that is given a name it cannot use is refused when it is made, with the reason as the
//! error. an editor that shows a name field wants to say so before then, as the user types, and
//! that has to be the rename's own rule rather than the editor's idea of one: the rule is `by`'s,
//! and it differs by what is renamed — a python symbol takes an identifier that is not a keyword,
//! while a django template's name may be any text. so the editor asks
//!
//! and it wants names to offer in the field, which only something that knows what the symbol
//! holds can suggest

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Request, TextDocumentIdentifier, Uri,
};
use ty_ide::{
    PreparedRename, TemplateRenameOutcome, django_rename, invalid_new_name, name_suggestions,
    prepare_rename,
};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::PositionExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum ValidateRenameNameRequest {}

impl Request for ValidateRenameNameRequest {
    type Params = ValidateRenameNameParams;
    type Result = Option<RenameNameVerdict>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/validateRenameName");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the symbol at a position, as `textDocument/rename` names it, and the name it would be given
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ValidateRenameNameParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    new_name: String,
}

/// whether `textDocument/rename` would take the name, and if not, the reason it would give
#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameNameVerdict {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

pub(crate) struct ValidateRenameNameHandler;

impl RequestHandler for ValidateRenameNameHandler {
    type RequestType = ValidateRenameNameRequest;
}

impl BackgroundDocumentRequestHandler for ValidateRenameNameHandler {
    fn document_uri(params: &ValidateRenameNameParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` where there is nothing to rename, as `textDocument/prepareRename` answers there
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: ValidateRenameNameParams,
    ) -> crate::server::Result<Option<RenameNameVerdict>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(None);
        };
        let template = snapshot.is_django_template();
        let refused = |message: String| {
            Some(RenameNameVerdict {
                valid: false,
                message: Some(message),
            })
        };
        let valid = Some(RenameNameVerdict {
            valid: true,
            message: None,
        });

        // the rename's own order: a python symbol, and else the django name a module writes as a
        // string or a template names, whose rule the django rename itself applies
        let python = (!template).then(|| prepare_rename(db, db.program_file(file), offset));
        if let Some(PreparedRename::Ready { .. }) = python {
            return Ok(match invalid_new_name(&params.new_name) {
                Some(why) => refused(why),
                None => valid,
            });
        }
        Ok(
            match django_rename(db, file, offset, &params.new_name, template) {
                Some(TemplateRenameOutcome::Refused(why)) => refused(why),
                Some(TemplateRenameOutcome::Edits(_)) => valid,
                None => match python {
                    Some(PreparedRename::Refused(why)) => refused(why),
                    Some(PreparedRename::Ready { .. } | PreparedRename::NoSymbol) | None => None,
                },
            },
        )
    }
}

impl RetriableRequestHandler for ValidateRenameNameHandler {}

pub(crate) enum RenameNameSuggestionsRequest {}

impl Request for RenameNameSuggestionsRequest {
    type Params = RenameNameSuggestionsParams;
    type Result = Vec<String>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/renameNameSuggestions");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the symbol at a position, as `textDocument/rename` names it
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RenameNameSuggestionsParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

pub(crate) struct RenameNameSuggestionsHandler;

impl RequestHandler for RenameNameSuggestionsHandler {
    type RequestType = RenameNameSuggestionsRequest;
}

impl BackgroundDocumentRequestHandler for RenameNameSuggestionsHandler {
    fn document_uri(params: &RenameNameSuggestionsParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// best first, and empty where there is no python symbol to rename or nothing to say
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: RenameNameSuggestionsParams,
    ) -> crate::server::Result<Vec<String>> {
        if snapshot.is_django_template()
            || snapshot
                .workspace_settings()
                .is_language_services_disabled()
        {
            return Ok(Vec::new());
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(Vec::new());
        };
        let Some(offset) =
            params
                .position
                .to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())
        else {
            return Ok(Vec::new());
        };
        Ok(name_suggestions(db, db.program_file(file), offset))
    }
}

impl RetriableRequestHandler for RenameNameSuggestionsHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// the wire form, exactly as a client sends and reads it
    #[test]
    fn the_wire_forms() {
        let params: ValidateRenameNameParams = serde_json::from_str(
            r#"{"textDocument":{"uri":"file:///a.by"},"position":{"line":0,"character":4},"newName":"class"}"#,
        )
        .expect("the validate params parse");
        assert_eq!(params.new_name, "class");
        let _: RenameNameSuggestionsParams = serde_json::from_str(
            r#"{"textDocument":{"uri":"file:///a.by"},"position":{"line":0,"character":4}}"#,
        )
        .expect("the suggestions params parse");
        assert_eq!(
            serde_json::to_string(&RenameNameVerdict {
                valid: false,
                message: Some("`class` is a keyword".to_owned()),
            })
            .unwrap(),
            r#"{"valid":false,"message":"`class` is a keyword"}"#
        );
        assert_eq!(
            serde_json::to_string(&RenameNameVerdict {
                valid: true,
                message: None,
            })
            .unwrap(),
            r#"{"valid":true}"#
        );
    }
}
