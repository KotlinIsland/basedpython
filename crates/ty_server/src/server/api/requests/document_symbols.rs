use std::borrow::Cow;

use lsp_types::DocumentSymbolRequest;
use lsp_types::{DocumentSymbol, DocumentSymbolParams, Uri};
use ruff_db::files::File;
use ty_ide::{
    HierarchicalSymbols, SymbolId, SymbolInfo, TemplateSymbol, django_template_document_symbols,
    document_symbols,
};
use ty_project::ProjectDatabase;

use crate::Db;
use crate::document::{PositionEncoding, ToRangeExt};
use crate::server::api::symbols::{convert_symbol_kind, convert_to_lsp_symbol_information};
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) struct DocumentSymbolRequestHandler;

impl RequestHandler for DocumentSymbolRequestHandler {
    type RequestType = DocumentSymbolRequest;
}

impl BackgroundDocumentRequestHandler for DocumentSymbolRequestHandler {
    fn document_uri(params: &DocumentSymbolParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: DocumentSymbolParams,
    ) -> crate::server::Result<Option<lsp_types::DocumentSymbolResponse>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }

        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        // Check if the client supports hierarchical document symbols
        let supports_hierarchical = snapshot
            .resolved_client_capabilities()
            .supports_hierarchical_document_symbols();

        if snapshot.is_django_template() {
            return Ok(template_symbols(
                db,
                file,
                supports_hierarchical,
                snapshot.encoding(),
            ));
        }

        let symbols = document_symbols(db, file);
        if symbols.is_empty() {
            return Ok(None);
        }

        if supports_hierarchical {
            let symbols = symbols.to_hierarchical();
            let lsp_symbols = symbols
                .iter()
                .filter_map(|(id, symbol)| {
                    convert_to_lsp_document_symbol(
                        db,
                        file,
                        &symbols,
                        id,
                        symbol,
                        snapshot.encoding(),
                    )
                })
                .collect();

            Ok(Some(lsp_types::DocumentSymbolResponse::DocumentSymbolList(
                lsp_symbols,
            )))
        } else {
            // Return flattened symbols as SymbolInformation
            let lsp_symbols = symbols
                .iter()
                .filter_map(|(_, symbol)| {
                    convert_to_lsp_symbol_information(db, file, symbol, None, snapshot.encoding())
                })
                .collect();

            Ok(Some(
                lsp_types::DocumentSymbolResponse::SymbolInformationList(lsp_symbols),
            ))
        }
    }
}

impl RetriableRequestHandler for DocumentSymbolRequestHandler {}

/// the outline of a django template, in whichever shape the client understands
fn template_symbols(
    db: &ProjectDatabase,
    file: File,
    supports_hierarchical: bool,
    encoding: PositionEncoding,
) -> Option<lsp_types::DocumentSymbolResponse> {
    let symbols = django_template_document_symbols(db, file);
    if symbols.is_empty() {
        return None;
    }

    if supports_hierarchical {
        let lsp_symbols = symbols
            .iter()
            .filter_map(|symbol| convert_template_symbol(db, file, symbol, encoding))
            .collect();

        return Some(lsp_types::DocumentSymbolResponse::DocumentSymbolList(
            lsp_symbols,
        ));
    }

    let mut flattened = Vec::new();
    flatten_template_symbols(&symbols, &mut flattened);

    let lsp_symbols = flattened
        .into_iter()
        .filter_map(|symbol| convert_to_lsp_symbol_information(db, file, symbol, None, encoding))
        .collect();

    Some(lsp_types::DocumentSymbolResponse::SymbolInformationList(
        lsp_symbols,
    ))
}

fn convert_template_symbol(
    db: &dyn Db,
    file: File,
    symbol: &TemplateSymbol,
    encoding: PositionEncoding,
) -> Option<DocumentSymbol> {
    Some(DocumentSymbol {
        name: symbol.name.clone(),
        detail: None,
        kind: convert_symbol_kind(symbol.kind),
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: symbol
            .full_range
            .to_lsp_range(db, file, encoding)?
            .local_range(),
        selection_range: symbol
            .name_range
            .to_lsp_range(db, file, encoding)?
            .local_range(),
        children: Some(
            symbol
                .children
                .iter()
                .filter_map(|child| convert_template_symbol(db, file, child, encoding))
                .collect(),
        ),
    })
}

/// every symbol of the outline, each parent before what it encloses
fn flatten_template_symbols<'a>(
    symbols: &'a [TemplateSymbol],
    flattened: &mut Vec<SymbolInfo<'a>>,
) {
    for symbol in symbols {
        flattened.push(symbol.symbol_info());
        flatten_template_symbols(&symbol.children, flattened);
    }
}

fn convert_to_lsp_document_symbol(
    db: &dyn Db,
    file: File,
    symbols: &HierarchicalSymbols,
    id: SymbolId,
    symbol: SymbolInfo<'_>,
    encoding: PositionEncoding,
) -> Option<DocumentSymbol> {
    let symbol_kind = convert_symbol_kind(symbol.kind);

    Some(DocumentSymbol {
        name: symbol.name.into_owned(),
        detail: None,
        kind: symbol_kind,
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: symbol
            .full_range
            .to_lsp_range(db, file, encoding)?
            .local_range(),
        selection_range: symbol
            .name_range
            .to_lsp_range(db, file, encoding)?
            .local_range(),
        children: Some(
            symbols
                .children(id)
                .filter_map(|(child_id, child)| {
                    convert_to_lsp_document_symbol(db, file, symbols, child_id, child, encoding)
                })
                .collect(),
        ),
    })
}
