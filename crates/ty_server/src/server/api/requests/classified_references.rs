//! `by/references`: `textDocument/references`, each reference with what it does with the name
//!
//! what an editor's *find usages* groups usages by: `import`, `isinstance`, `typeHint`,
//! `superclass`, `write`, `read` and `declaration`, the groups PyCharm's Python sorts into. a
//! `Location` has nowhere to say it, so this is a request of its own, answering exactly the
//! references `textDocument/references` does, in the same order. see [`ty_ide::usage_kinds`] for
//! what each kind is

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Range, ReferenceParams, Request, Uri};
use ty_ide::{UsageKind, usage_kinds};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::ToLink;
use crate::server::api::requests::references::references_at;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum ClassifiedReferencesRequest {}

impl Request for ClassifiedReferencesRequest {
    type Params = ReferenceParams;
    type Result = Option<Vec<ClassifiedReference>>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/references");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// one reference, where `textDocument/references` would put it, and what it does with the name
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClassifiedReference {
    uri: Uri,
    range: Range,
    kind: Kind,
}

/// what a reference does with the name, the first of these that holds
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Kind {
    /// in an `import` or `from ... import` statement
    Import,
    /// among the classes an `isinstance` call tests against
    Isinstance,
    /// in a type expression: an annotation, a type alias's value, a type parameter's bound, or
    /// an argument a call takes as a type, `cast`'s first
    TypeHint,
    /// in the parentheses of a class's header
    Superclass,
    /// binding the name: an assignment, a `del`, a `for` target
    Write,
    /// reading the name, calling it among the ways
    Read,
    /// the name where it is declared, with `includeDeclaration`
    Declaration,
}

impl From<UsageKind> for Kind {
    fn from(kind: UsageKind) -> Self {
        match kind {
            UsageKind::Import => Kind::Import,
            UsageKind::Isinstance => Kind::Isinstance,
            UsageKind::TypeHint => Kind::TypeHint,
            UsageKind::Superclass => Kind::Superclass,
            UsageKind::Write => Kind::Write,
            UsageKind::Read => Kind::Read,
            UsageKind::Declaration => Kind::Declaration,
        }
    }
}

pub(crate) struct ClassifiedReferencesRequestHandler;

impl RequestHandler for ClassifiedReferencesRequestHandler {
    type RequestType = ClassifiedReferencesRequest;
}

impl BackgroundDocumentRequestHandler for ClassifiedReferencesRequestHandler {
    fn document_uri(params: &ReferenceParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document_position_params.text_document.uri)
    }

    /// `null` where `textDocument/references` answers `null`: when language services are off,
    /// and where there is no name to search for
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: ReferenceParams,
    ) -> crate::server::Result<Option<Vec<ClassifiedReference>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let Some(targets) = references_at(
            db,
            snapshot,
            params.text_document_position_params.position,
            params.context.include_declaration,
        ) else {
            return Ok(None);
        };
        let kinds = usage_kinds(db, db.program_file(file), &targets);

        Ok(Some(
            targets
                .into_iter()
                .zip(kinds)
                .filter_map(|(target, kind)| {
                    let location = target.to_location(db, snapshot.encoding())?;
                    Some(ClassifiedReference {
                        uri: location.uri,
                        range: location.range,
                        kind: kind.into(),
                    })
                })
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for ClassifiedReferencesRequestHandler {}
