//! `by/typeDeclaration` — the class a dotted type name names, for a debugger's *jump to type
//! source*.
//!
//! A debugger knows a value's type by its module and qualified name, `shapes.base.Circle`, and
//! has no position in a document to ask `by/declaration` about; `workspace/symbol` matches a
//! query loosely and only among the project's own files, where `builtins.list` or a package's
//! class is not. The name is resolved here as an import of it would be: see
//! [`ty_ide::type_declaration`].

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, Uri};
use ty_ide::type_declaration;
use ty_project::{Db as _, SemanticDb as _};

use crate::document::ToLink;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum TypeDeclarationRequest {}

impl Request for TypeDeclarationRequest {
    type Params = TypeDeclarationParams;
    type Result = Option<TypeDeclaration>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/typeDeclaration");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TypeDeclarationParams {
    /// The type's module and qualified name, dot-separated: `shapes.base.Shape.Meta`.
    type_name: String,
}

/// The class, as `by/declaration` describes a declaration.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TypeDeclaration {
    /// Always `class`.
    kind: String,
    name: String,
    /// The class's own module and qualified name, which differs from the name asked about for a
    /// class reached through a re-export.
    qualified_name: Option<String>,
    uri: Uri,
    /// The whole class.
    range: Range,
    /// Its name.
    selection_range: Range,
}

pub(crate) struct TypeDeclarationHandler;

impl RequestHandler for TypeDeclarationHandler {
    type RequestType = TypeDeclarationRequest;
}

impl BackgroundRequestHandler for TypeDeclarationHandler {
    /// The first project in the session whose environment the name resolves in; `null` for a name
    /// that names no class.
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: TypeDeclarationParams,
    ) -> crate::server::Result<Option<TypeDeclaration>> {
        let encoding = snapshot.position_encoding();
        for db in snapshot.projects() {
            // the project's environment is what resolves a module, and any file of it stands in
            // for the module an import of the name would be written in
            let Some(context) = db.project().files(db).iter().next() else {
                continue;
            };
            let Some(found) = type_declaration(db, db.program_file(context), &params.type_name)
            else {
                continue;
            };
            let Some(link) = found.target.to_link(db, None, encoding) else {
                continue;
            };
            return Ok(Some(TypeDeclaration {
                kind: found.declaration.kind.as_str().to_string(),
                name: found.declaration.name,
                qualified_name: found.declaration.qualified_name,
                uri: link.target_uri,
                range: link.target_range,
                selection_range: link.target_selection_range,
            }));
        }
        Ok(None)
    }
}

impl RetriableRequestHandler for TypeDeclarationHandler {}
