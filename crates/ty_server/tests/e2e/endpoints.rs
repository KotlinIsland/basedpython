//! `by/endpoints`: the HTTP routes a project serves, for an editor's endpoints view

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};

use crate::TestServerBuilder;

enum Endpoints {}

impl Request for Endpoints {
    type Params = Value;
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/endpoints");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[test]
fn a_route_is_listed_with_its_whole_path_handler_and_application() -> Result<()> {
    let main = SystemPath::new("project/app/main.by");
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_file(
            SystemPath::new("project/fastapi/__init__.py"),
            "from .applications import FastAPI as FastAPI\nfrom .routing import APIRouter as APIRouter\n",
        )?
        .with_file(
            SystemPath::new("project/fastapi/routing.py"),
            "from typing import Any, Callable\n\nclass APIRouter:\n    def __init__(self, *, prefix: str = '') -> None: ...\n    def get(self, path: str) -> Callable[[Callable[..., Any]], Callable[..., Any]]: ...\n",
        )?
        .with_file(
            SystemPath::new("project/fastapi/applications.py"),
            "from fastapi.routing import APIRouter\n\nclass FastAPI:\n    def include_router(self, router: APIRouter, *, prefix: str = '') -> None: ...\n",
        )?
        .with_file(SystemPath::new("project/app/__init__.by"), "")?
        .with_file(
            main,
            "from fastapi import APIRouter, FastAPI\n\nrouter = APIRouter(prefix='/items')\n\n@router.get('/{item_id}')\ndef read_item(item_id: int) -> int:\n    return item_id\n\napp = FastAPI()\napp.include_router(router, prefix='/v1')\n",
        )?
        .build()
        .wait_until_workspaces_are_initialized();

    let response = server.send_request_await::<Endpoints>(json!({}));
    let uri = server.file_uri(main);
    assert_eq!(
        response,
        json!([{
            "framework": "fastapi",
            "methods": ["GET"],
            "path": "/v1/items/{item_id}",
            "handler": {
                "name": "app.main.read_item",
                "uri": uri,
                "range": {"start": {"line": 4, "character": 0}, "end": {"line": 6, "character": 18}},
                "selectionRange": {"start": {"line": 5, "character": 4}, "end": {"line": 5, "character": 13}},
            },
            "declaration": {
                "uri": uri,
                "range": {"start": {"line": 4, "character": 1}, "end": {"line": 4, "character": 25}},
            },
            "application": {
                "name": "app.main.app",
                "uri": uri,
                "range": {"start": {"line": 8, "character": 0}, "end": {"line": 8, "character": 15}},
                "selectionRange": {"start": {"line": 8, "character": 0}, "end": {"line": 8, "character": 3}},
            },
        }])
    );
    Ok(())
}
