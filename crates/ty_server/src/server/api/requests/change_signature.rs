//! `by/prepareChangeSignature` and `by/changeSignature`: a function's parameters, and changing
//! them with every call of the function in the project. see [`ty_ide::change_signature`] for how
//! calls are rewritten and what is refused

use std::borrow::Cow;

use lsp_types::{
    LspRequestMethod, MessageDirection, Position, Range, Request, TextDocumentIdentifier, Uri,
};
use ruff_db::files::FileRange;
use ty_ide::{
    AnnotationChange, NewParameter, ParameterKind, SignatureChange, change_signature,
    function_signature,
};
use ty_project::{ProjectDatabase, SemanticDb as _};

use super::move_symbol::{MoveSymbolAnswer, position};
use crate::document::FileRangeExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

/// how a parameter is passed
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Kind {
    /// before a `/`
    PositionalOnly,
    PositionalOrKeyword,
    /// `*args`
    Variadic,
    /// after a `*` or `*args`
    KeywordOnly,
    /// `**kwargs`
    VariadicKeyword,
}

impl From<ParameterKind> for Kind {
    fn from(kind: ParameterKind) -> Self {
        match kind {
            ParameterKind::PositionalOnly => Kind::PositionalOnly,
            ParameterKind::PositionalOrKeyword => Kind::PositionalOrKeyword,
            ParameterKind::Variadic => Kind::Variadic,
            ParameterKind::KeywordOnly => Kind::KeywordOnly,
            ParameterKind::VariadicKeyword => Kind::VariadicKeyword,
        }
    }
}

impl From<Kind> for ParameterKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::PositionalOnly => ParameterKind::PositionalOnly,
            Kind::PositionalOrKeyword => ParameterKind::PositionalOrKeyword,
            Kind::Variadic => ParameterKind::Variadic,
            Kind::KeywordOnly => ParameterKind::KeywordOnly,
            Kind::VariadicKeyword => ParameterKind::VariadicKeyword,
        }
    }
}

pub(crate) enum PrepareChangeSignatureRequest {}

impl Request for PrepareChangeSignatureRequest {
    type Params = PrepareChangeSignatureParams;
    type Result = Option<Signature>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/prepareChangeSignature");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// a position in a function's name or parameters where it is defined, or in a name that refers
/// to it
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareChangeSignatureParams {
    text_document: TextDocumentIdentifier,
    position: Position,
}

/// the function and its parameters
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Signature {
    name: String,
    /// the file it is defined in, which need not be the document asked about
    uri: Uri,
    /// its name, where it is defined
    range: Range,
    /// its parameters, without the `self` or `cls` of a method, which stays as it is
    parameters: Vec<SignatureParameter>,
    /// its return annotation, as the source writes it
    returns: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignatureParameter {
    name: String,
    kind: Kind,
    /// as the source writes it
    default: Option<String>,
    /// as the source writes it
    annotation: Option<String>,
}

pub(crate) struct PrepareChangeSignatureRequestHandler;

impl RequestHandler for PrepareChangeSignatureRequestHandler {
    type RequestType = PrepareChangeSignatureRequest;
}

impl BackgroundDocumentRequestHandler for PrepareChangeSignatureRequestHandler {
    fn document_uri(params: &PrepareChangeSignatureParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` where no function of the project is named, when language services are off and for
    /// a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: PrepareChangeSignatureParams,
    ) -> crate::server::Result<Option<Signature>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        let Some(signature) = function_signature(db, db.program_file(file), offset) else {
            return Ok(None);
        };
        let Some(location) = FileRange::new(signature.file, signature.name_range)
            .to_lsp_range(db, snapshot.encoding())
            .and_then(|range| range.to_location())
        else {
            return Ok(None);
        };
        Ok(Some(Signature {
            name: signature.name,
            uri: location.uri,
            range: location.range,
            parameters: signature
                .parameters
                .into_iter()
                .map(|parameter| SignatureParameter {
                    name: parameter.name,
                    kind: parameter.kind.into(),
                    default: parameter.default,
                    annotation: parameter.annotation,
                })
                .collect(),
            returns: signature.returns,
        }))
    }
}

impl RetriableRequestHandler for PrepareChangeSignatureRequestHandler {}

pub(crate) enum ChangeSignatureRequest {}

impl Request for ChangeSignatureRequest {
    type Params = ChangeSignatureParams;
    type Result = Option<MoveSymbolAnswer>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/changeSignature");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the function, as for `by/prepareChangeSignature`, and the whole of its new parameter list, in
/// order, without a method's `self` or `cls`
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ChangeSignatureParams {
    text_document: TextDocumentIdentifier,
    position: Position,
    /// its new name; absent keeps the one it has
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    parameters: Vec<ChangedParameter>,
    /// its return annotation
    #[serde(default, skip_serializing_if = "WrittenAnnotation::is_kept")]
    returns: WrittenAnnotation,
}

/// an annotation written as source, where `null` removes the one there is and a field left out
/// keeps it: the field's absence and its `null` say different things, so it is not an `Option`
#[derive(Debug, Default)]
pub(crate) struct WrittenAnnotation(AnnotationChange);

impl WrittenAnnotation {
    fn is_kept(&self) -> bool {
        self.0 == AnnotationChange::Keep
    }
}

impl<'de> serde::Deserialize<'de> for WrittenAnnotation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Option<String> as serde::Deserialize>::deserialize(deserializer)
            .map(|annotation| Self(AnnotationChange::Set(annotation)))
    }
}

impl serde::Serialize for WrittenAnnotation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            AnnotationChange::Keep => serializer.serialize_unit(),
            AnnotationChange::Set(annotation) => annotation.serialize(serializer),
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ChangedParameter {
    name: String,
    /// the parameter it was; absent for a new one
    #[serde(default)]
    old_name: Option<String>,
    kind: Kind,
    /// its default, written as source; absent for none
    #[serde(default)]
    default: Option<String>,
    /// its annotation; absent keeps the one it has, none for a new parameter
    #[serde(default, skip_serializing_if = "WrittenAnnotation::is_kept")]
    annotation: WrittenAnnotation,
    /// for a new parameter, what each call passes it, written as source; absent leaves it to its
    /// default
    #[serde(default)]
    value: Option<String>,
}

pub(crate) struct ChangeSignatureRequestHandler;

impl RequestHandler for ChangeSignatureRequestHandler {
    type RequestType = ChangeSignatureRequest;
}

impl BackgroundDocumentRequestHandler for ChangeSignatureRequestHandler {
    fn document_uri(params: &ChangeSignatureParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    /// `null` when language services are off and for a notebook or a template
    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: ChangeSignatureParams,
    ) -> crate::server::Result<Option<MoveSymbolAnswer>> {
        let Some((file, offset)) = position(db, snapshot, params.position) else {
            return Ok(None);
        };
        let change = SignatureChange {
            name: params.name,
            parameters: params
                .parameters
                .into_iter()
                .map(|parameter| NewParameter {
                    name: parameter.name,
                    old_name: parameter.old_name,
                    kind: parameter.kind.into(),
                    default: parameter.default,
                    annotation: parameter.annotation.0,
                    value: parameter.value,
                })
                .collect(),
            returns: params.returns.0,
        };
        Ok(Some(MoveSymbolAnswer::of(
            db,
            snapshot,
            change_signature(db, db.program_file(file), offset, &change),
        )))
    }
}

impl RetriableRequestHandler for ChangeSignatureRequestHandler {
    /// the user is waiting on the change they asked for
    const RETRY_ON_CANCELLATION: bool = true;
}
