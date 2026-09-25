//! `by/syntaxOutline` — a document's block structure and string literals, as the parser sees them.
//!
//! What an editor needs to type in an indentation-delimited language and to draw what is inside a
//! string: which line opens a suite and where a compound statement ends, which clause keywords
//! belong to one statement, what a call statement calls, and each string literal's escapes,
//! interpolations and the indentation basedpython strips from it.
//!
//! None of it has a standard request. `textDocument/foldingRange` and
//! `textDocument/selectionRange` carry ranges without saying which is a header, a clause or a
//! suite; semantic tokens cannot overlap, and an interpolation is a span with tokens inside it.
//! The keyword pairs themselves are answered by `textDocument/documentHighlight`, which is
//! exactly that question; this reports the clause keywords too, because an editor also navigates
//! between them without a position to ask about.
//!
//! One document at a time, whole: a client keeps the answer against the revision it asked about
//! and serves every feature on that revision from it.

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, TextDocumentIdentifier, Uri};
use ruff_db::files::File;
use ruff_text_size::TextRange;
use ty_ide::{OutlineStatement, OutlineString, syntax_outline};
use ty_project::{ProjectDatabase, SemanticDb as _};

use crate::PositionEncoding;
use crate::document::ToRangeExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

pub(crate) enum SyntaxOutlineRequest {}

impl Request for SyntaxOutlineRequest {
    type Params = SyntaxOutlineParams;
    type Result = Option<SyntaxOutlineResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/syntaxOutline");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Which document to outline.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SyntaxOutlineParams {
    /// The document — its buffer, as the client last synchronised it.
    text_document: TextDocumentIdentifier,
}

/// The outline of one document.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyntaxOutlineResponse {
    /// The module's statements, in source order.
    statements: Vec<Statement>,
    /// Every string literal part, in source order.
    strings: Vec<StringPart>,
}

/// One statement.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Statement {
    /// From the statement's first token — a decorator, on a decorated definition — to the end of
    /// its last clause's body.
    range: Range,
    /// A compound statement's clauses, in source order. Absent on a simple statement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clauses: Vec<Clause>,
    /// The basedpython modifier keywords a definition was declared with, in source order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    modifiers: Vec<Modifier>,
    /// On an expression statement that is a call: the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call: Option<Call>,
}

/// One clause of a compound statement.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Clause {
    /// The clause keyword: `if`, `elif`, `case`, the `def` of a function. Absent on a clause that
    /// has none, which is a trailing lambda's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keyword: Option<Range>,
    /// The `:` that opens the suite. Absent when the header has none to find — a header the parser
    /// recovered from, or the `let` half of a `let ... := ... else:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    colon: Option<Range>,
    /// From the start of the header to the end of the body, or of the header when the body is
    /// empty.
    range: Range,
    /// The suite's statements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    body: Vec<Statement>,
}

/// A basedpython modifier keyword.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Modifier {
    /// The keyword or keywords (`frozen data`), without trailing whitespace.
    range: Range,
    /// What the parser read it as: `data_class`, `frozen_data_class`, `enum_def`, `classmethod`,
    /// `protocol_class`, `final`, ...
    name: String,
}

/// A call made as a statement of its own.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Call {
    /// The expression called.
    callee: Range,
    /// From the start of the first argument to the end of the last. Absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    arguments: Option<Range>,
    /// Whether every argument is positional and none is unpacked.
    positional_only: bool,
}

/// One string literal part.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StringPart {
    /// The part, prefix and quotes included.
    range: Range,
    /// How many leading characters basedpython strips from every line of the literal. Absent when
    /// it strips nothing, which includes every python file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stripped_indent: Option<u32>,
    /// An f-string's or t-string's `{...}` interpolations, braces included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    interpolations: Vec<Range>,
    /// The escape sequences in the literal text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    escapes: Vec<Range>,
}

pub(crate) struct SyntaxOutlineRequestHandler;

impl RequestHandler for SyntaxOutlineRequestHandler {
    type RequestType = SyntaxOutlineRequest;
}

impl BackgroundDocumentRequestHandler for SyntaxOutlineRequestHandler {
    // a parse and what is read off it: the same for a file read from disk as for a buffer
    const ANSWERS_CLOSED_DOCUMENTS: bool = true;

    fn document_uri(params: &SyntaxOutlineParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        _params: SyntaxOutlineParams,
    ) -> crate::server::Result<Option<SyntaxOutlineResponse>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        // A notebook's ranges are per cell, and nothing here is asked about cells.
        if snapshot.document().is_cell_or_notebook() || snapshot.is_django_template() {
            return Ok(None);
        }
        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };

        let outline = syntax_outline(db, db.program_file(file).python_file(db));
        let converter = Converter {
            db,
            file,
            encoding: snapshot.encoding(),
        };

        // A part that does not map is dropped rather than the whole answer: every range here comes
        // from the same parse of the same text, so one that fails to map is not something a client
        // could have used anyway.
        Ok(Some(SyntaxOutlineResponse {
            statements: converter.statements(&outline.statements),
            strings: outline
                .strings
                .iter()
                .filter_map(|string| converter.string(string))
                .collect(),
        }))
    }
}

impl RetriableRequestHandler for SyntaxOutlineRequestHandler {}

struct Converter<'a> {
    db: &'a ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
}

impl Converter<'_> {
    fn range(&self, range: TextRange) -> Option<Range> {
        range
            .to_lsp_range(self.db, self.file, self.encoding)
            .map(|range| range.local_range())
    }

    fn statements(&self, statements: &[OutlineStatement]) -> Vec<Statement> {
        statements
            .iter()
            .filter_map(|statement| {
                Some(Statement {
                    range: self.range(statement.range)?,
                    clauses: statement
                        .clauses
                        .iter()
                        .filter_map(|clause| {
                            Some(Clause {
                                keyword: clause.keyword.and_then(|range| self.range(range)),
                                colon: clause.colon.and_then(|range| self.range(range)),
                                range: self.range(clause.range)?,
                                body: self.statements(&clause.body),
                            })
                        })
                        .collect(),
                    modifiers: statement
                        .modifiers
                        .iter()
                        .filter_map(|modifier| {
                            Some(Modifier {
                                range: self.range(modifier.range)?,
                                name: modifier.name.clone(),
                            })
                        })
                        .collect(),
                    call: statement.call.as_ref().and_then(|call| {
                        Some(Call {
                            callee: self.range(call.callee)?,
                            arguments: call.arguments.and_then(|range| self.range(range)),
                            positional_only: call.positional_only,
                        })
                    }),
                })
            })
            .collect()
    }

    fn string(&self, string: &OutlineString) -> Option<StringPart> {
        Some(StringPart {
            range: self.range(string.range)?,
            stripped_indent: string.stripped_indent.map(u32::from),
            interpolations: string
                .interpolations
                .iter()
                .filter_map(|range| self.range(*range))
                .collect(),
            escapes: string
                .escapes
                .iter()
                .filter_map(|range| self.range(*range))
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form, exactly as a client sends it.
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: SyntaxOutlineParams =
            serde_json::from_str(r#"{"textDocument":{"uri":"file:///main.by"}}"#)
                .expect("a client sends a document");
        assert_eq!(parsed.text_document.uri.as_str(), "file:///main.by");
    }
}
