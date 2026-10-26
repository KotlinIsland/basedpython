//! `by/endpoints` — the HTTP routes a project serves, for an editor's endpoints view
//!
//! each route with the methods it serves, the whole path it is served at, its handler, where the
//! route is declared and the application that serves it, read through what the code means: see
//! [`ty_ide::endpoints`]. asked of every project in the session, as `workspace/symbol` is

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, Uri};
use ty_ide::{EndpointTarget, endpoints};

use crate::Db;
use crate::PositionEncoding;
use crate::document::{FileRangeExt, ToLink};
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum EndpointsRequest {}

impl Request for EndpointsRequest {
    type Params = Option<EndpointsParams>;
    type Result = Vec<Endpoint>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/endpoints");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// nothing: the answer is every project's
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the brackets make it the empty object a client sends, which a unit struct is not"
)]
pub(crate) struct EndpointsParams {}

/// one route
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Endpoint {
    /// `fastapi` or `django`
    framework: String,
    /// the HTTP methods, upper case. empty for a route that serves every method, as django's do
    methods: Vec<String>,
    /// the path the route is served at, every prefix it is mounted under included: `/v1/items/{item_id}`
    path: String,
    /// what handles a request to it
    handler: Target,
    /// where the route is declared: the decorator, `add_api_route(...)` or `path(...)`
    declaration: Location,
    /// the FastAPI application, or the urlconf module django starts at. absent for a route on a
    /// router no application includes
    #[serde(skip_serializing_if = "Option::is_none")]
    application: Option<Target>,
}

/// a declaration an endpoint names
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Target {
    /// the dotted name it is reached by, `app.items.read_item`; absent where it has none
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    uri: Uri,
    /// the whole declaration
    range: Range,
    /// its name
    selection_range: Range,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct Location {
    uri: Uri,
    range: Range,
}

pub(crate) struct EndpointsHandler;

impl RequestHandler for EndpointsHandler {
    type RequestType = EndpointsRequest;
}

impl BackgroundRequestHandler for EndpointsHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        _params: Option<EndpointsParams>,
    ) -> crate::server::Result<Vec<Endpoint>> {
        let encoding = snapshot.position_encoding();
        let mut found = Vec::new();
        for db in snapshot.projects() {
            for endpoint in endpoints(db) {
                // an application that cannot be linked is left out with the rest of the route,
                // as a handler is, rather than sent as absent, which says no application serves it
                let application = match &endpoint.application {
                    Some(application) => match target(db, application, encoding) {
                        Some(application) => Some(application),
                        None => continue,
                    },
                    None => None,
                };
                let (Some(handler), Some(declaration)) = (
                    target(db, &endpoint.handler, encoding),
                    endpoint
                        .declaration
                        .to_lsp_range(db, encoding)
                        .and_then(|range| range.to_location()),
                ) else {
                    continue;
                };
                found.push(Endpoint {
                    framework: endpoint.framework.as_str().to_string(),
                    methods: endpoint.methods,
                    path: endpoint.path,
                    handler,
                    declaration: Location {
                        uri: declaration.uri,
                        range: declaration.range,
                    },
                    application,
                });
            }
        }
        Ok(found)
    }
}

fn target(db: &dyn Db, target: &EndpointTarget, encoding: PositionEncoding) -> Option<Target> {
    let link = target.target.to_link(db, None, encoding)?;
    Some(Target {
        name: target.name.clone(),
        uri: link.target_uri,
        range: link.target_range,
        selection_range: link.target_selection_range,
    })
}

impl RetriableRequestHandler for EndpointsHandler {}
