//! `by/documentInheritedMembers`: the members each class of a document inherits
//!
//! what an editor's structure view lists under a class when it shows inherited members: every
//! member a class of its method resolution order declares, methods, properties, nested classes and
//! attributes alike, from the first class to declare the name, less the names the class declares
//! itself. each member is as that superclass's outline (`textDocument/documentSymbol`) has it, so
//! that a client draws it as it draws the class's own. see [`ty_ide::document_inherited_members`]
//!
//! one document at a time, whole, like the other document requests: a client asks once for all the
//! classes it shows. the answer depends on every file the classes inherit from, and the server
//! announces no change to it: a client asks again after any document of the workspace changes

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Range, Request, SymbolKind, TextDocumentIdentifier, Uri,
};
use ruff_db::files::FileRange;
use ty_ide::document_inherited_members;
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{FileRangeExt, ToRangeExt};
use crate::server::api::symbols::convert_symbol_kind;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum DocumentInheritedMembersRequest {}

impl Request for DocumentInheritedMembersRequest {
    type Params = DocumentInheritedMembersParams;
    type Result = Option<Vec<ClassInheritedMembers>>;
    const METHOD: LspRequestMethod<'static> =
        LspRequestMethod::Custom("by/documentInheritedMembers");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DocumentInheritedMembersParams {
    text_document: TextDocumentIdentifier,
}

/// a class of the document, and what it inherits
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClassInheritedMembers {
    /// the class's name where the document declares it: the `selectionRange` of its document
    /// symbol
    selection_range: Range,
    /// in the class's method resolution order, then in each superclass's source order
    members: Vec<InheritedMember>,
}

/// a member a class inherits, as the outline of the superclass that declares it has it
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InheritedMember {
    name: String,
    /// as `textDocument/documentSymbol` gives it: method, constructor, property, field, class
    kind: SymbolKind,
    /// as `textDocument/documentSymbol` gives it: a method's parameters, a class's bases
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// the superclass that declares it
    container_name: String,
    /// the module that superclass is declared in, `shapes.base`, `builtins` for `object`. absent
    /// for a superclass in a file no module is imported from
    #[serde(skip_serializing_if = "Option::is_none")]
    container_module: Option<String>,
    /// the file the superclass declares it in
    uri: Uri,
    /// the whole declaration
    range: Range,
    /// its name, which is where to go
    selection_range: Range,
}

pub(crate) struct DocumentInheritedMembersRequestHandler;

impl RequestHandler for DocumentInheritedMembersRequestHandler {
    type RequestType = DocumentInheritedMembersRequest;
}

impl BackgroundDocumentRequestHandler for DocumentInheritedMembersRequestHandler {
    // what the project's files say, which is the same whether this one is a buffer or the file on
    // disk; the answer reads no version
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &DocumentInheritedMembersParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off, or for a notebook, whose ranges are per cell, or a
    /// template. otherwise every class of the document's outline, nested ones among them, in
    /// source order, each with the members it inherits, none for a class that inherits nothing
    /// it does not declare itself
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: DocumentInheritedMembersParams,
    ) -> crate::server::Result<Option<Vec<ClassInheritedMembers>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        if snapshot.document().is_cell_or_notebook() || snapshot.is_django_template() {
            return Ok(None);
        }
        let Some(file) = snapshot.document().to_notebook_or_file(db) else {
            return Ok(None);
        };
        let encoding = snapshot.encoding();

        Ok(Some(
            document_inherited_members(db, db.program_file(file))
                .into_iter()
                .filter_map(|class| {
                    let selection_range = class
                        .name_range
                        .to_lsp_range(db, file, encoding)?
                        .local_range();
                    let members = class
                        .members
                        .into_iter()
                        .filter_map(|member| {
                            let location = FileRange::new(member.file, member.full_range)
                                .to_lsp_range(db, encoding)?
                                .into_location()?;
                            let selection_range = FileRange::new(member.file, member.name_range)
                                .to_lsp_range(db, encoding)?
                                .local_range();
                            Some(InheritedMember {
                                name: member.name,
                                kind: convert_symbol_kind(member.kind),
                                detail: member.detail,
                                container_name: member.superclass.to_string(),
                                container_module: member.superclass_module,
                                uri: location.uri,
                                range: location.range,
                                selection_range,
                            })
                        })
                        .collect();
                    Some(ClassInheritedMembers {
                        selection_range,
                        members,
                    })
                })
                .collect(),
        ))
    }
}

impl RetriableRequestHandler for DocumentInheritedMembersRequestHandler {}
