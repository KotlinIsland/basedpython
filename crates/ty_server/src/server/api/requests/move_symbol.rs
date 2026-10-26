//! `by/prepareMoveSymbol` and `by/moveSymbol`: moving a module's top-level function, class or
//! variable to another module, with every reference to it in the project following it. LSP has no
//! request for a move whose target the user picks: a code action carries no argument, and
//! `workspace/willRenameFiles` moves files. see [`ty_ide::move_symbol`] for what is moved and what
//! is refused

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, TextEdit,
    Uri, WorkspaceEdit,
};
use ruff_db::files::{File, FileRange, system_path_to_file};
use ruff_db::system::SystemPathBuf;
use ruff_text_size::{Ranged, TextSize};
use rustc_hash::FxHashMap;
use ty_ide::{MovableKind, SymbolMove, movable_symbol, move_symbol};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::document::{FileRangeExt, PositionExt};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum PrepareMoveSymbolRequest {}

impl Request for PrepareMoveSymbolRequest {
    type Params = PrepareMoveSymbolParams;
    type Result = Option<MovableSymbol>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/prepareMoveSymbol");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// a position in a top-level definition's name, or in a reference to it
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareMoveSymbolParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// the definition a move would move
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MovableSymbol {
    name: String,
    /// `function`, `class` or `variable`
    kind: String,
    /// the dotted name of the module it is defined in
    module: String,
    /// the file it is defined in, which need not be the document asked about
    uri: Uri,
    /// its name, where it is defined
    range: Range,
}

pub(crate) struct PrepareMoveSymbolRequestHandler;

impl RequestHandler for PrepareMoveSymbolRequestHandler {
    type RequestType = PrepareMoveSymbolRequest;
}

impl BackgroundDocumentRequestHandler for PrepareMoveSymbolRequestHandler {
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &PrepareMoveSymbolParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` where there is no top-level function, class or variable of a project module to
    /// move, when language services are off and for a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: PrepareMoveSymbolParams,
    ) -> crate::server::Result<Option<MovableSymbol>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        let Some(symbol) = movable_symbol(db, db.program_file(file), offset) else {
            return Ok(None);
        };
        let Some(location) = FileRange::new(symbol.file, symbol.name_range)
            .to_lsp_range(db, snapshot.encoding())
            .and_then(|range| range.to_location())
        else {
            return Ok(None);
        };
        Ok(Some(MovableSymbol {
            name: symbol.name,
            kind: match symbol.kind {
                MovableKind::Function => "function",
                MovableKind::Class => "class",
                MovableKind::Variable => "variable",
            }
            .to_string(),
            module: symbol.module,
            uri: location.uri,
            range: location.range,
        }))
    }
}

impl RetriableRequestHandler for PrepareMoveSymbolRequestHandler {}

pub(crate) enum MoveSymbolRequest {}

impl Request for MoveSymbolRequest {
    type Params = MoveSymbolParams;
    type Result = Option<MoveSymbolAnswer>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/moveSymbol");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the definition to move, named as for `by/prepareMoveSymbol`, and the module to move it to
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MoveSymbolParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    /// an existing `.py` or `.by` file of the project, of the same kind as the definition's
    target: Uri,
}

/// either the edit that moves it, or why it is not moved
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MoveSymbolAnswer {
    /// the edits to every file the move changes; absent when refused
    #[serde(skip_serializing_if = "Option::is_none")]
    edit: Option<WorkspaceEdit>,
    /// why the definition is not moved; absent when it is
    #[serde(skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
    /// references the edit leaves as they are, which may no longer reach the definition, each
    /// with the file and line it is on
    warnings: Vec<String>,
}

pub(crate) struct MoveSymbolRequestHandler;

impl RequestHandler for MoveSymbolRequestHandler {
    type RequestType = MoveSymbolRequest;
}

impl BackgroundDocumentRequestHandler for MoveSymbolRequestHandler {
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &MoveSymbolParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off and for a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: MoveSymbolParams,
    ) -> crate::server::Result<Option<MoveSymbolAnswer>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        let Some(target) = params
            .target
            .to_file_path()
            .ok()
            .and_then(|path| SystemPathBuf::from_path_buf(path).ok())
            .and_then(|path| system_path_to_file(db, &path).ok())
        else {
            return Ok(Some(MoveSymbolAnswer::of(
                db,
                snapshot,
                Err("the target is not a file that exists".to_string()),
            )));
        };

        Ok(Some(MoveSymbolAnswer::of(
            db,
            snapshot,
            move_symbol(db, db.program_file(file), offset, target),
        )))
    }
}

impl MoveSymbolAnswer {
    /// the answer to a move: its edits as a workspace edit and its warnings, or why it refused
    pub(super) fn of(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        moved: Result<SymbolMove, String>,
    ) -> Self {
        let refused = |reason: String| MoveSymbolAnswer {
            refused: Some(reason),
            ..MoveSymbolAnswer::default()
        };
        let moved = match moved {
            Ok(moved) => moved,
            Err(reason) => return refused(reason),
        };
        let mut changes: FxHashMap<Uri, Vec<TextEdit>> = FxHashMap::default();
        for file_edit in moved.edits {
            let Some(location) = FileRange::new(file_edit.file, file_edit.edit.range())
                .to_lsp_range(db, snapshot.encoding())
                .and_then(|range| range.to_location())
            else {
                return refused("a file the move changes cannot be edited".to_string());
            };
            changes.entry(location.uri).or_default().push(TextEdit {
                range: location.range,
                new_text: file_edit.edit.content().unwrap_or_default().to_string(),
            });
        }
        MoveSymbolAnswer {
            edit: Some(WorkspaceEdit {
                changes: Some(changes.into_iter().collect()),
                document_changes: None,
                change_annotations: None,
            }),
            refused: None,
            warnings: moved.warnings,
        }
    }
}

impl RetriableRequestHandler for MoveSymbolRequestHandler {
    /// the user is waiting on the move they asked for
    const RETRY_ON_CANCELLATION: bool = true;
}

/// the file and offset of a position in the snapshot's document, `None` where a refactoring of a
/// definition is not answered: when language services are off, and for a template, a notebook or
/// a file that is not source code
pub(super) fn position(
    db: &ProjectDatabase,
    snapshot: &DocumentSnapshot,
    position: Position,
) -> Option<(File, TextSize)> {
    if snapshot
        .workspace_settings()
        .is_language_services_disabled()
        || snapshot.is_django_template()
    {
        return None;
    }
    let file = snapshot.document().to_notebook_or_file(db)?;
    if !file.source_type(db).is_py_file() {
        return None;
    }
    let offset = position.to_text_size(db, file, snapshot.document().uri(), snapshot.encoding())?;
    Some((file, offset))
}
