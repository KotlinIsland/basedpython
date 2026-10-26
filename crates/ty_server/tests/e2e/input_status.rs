use crate::TestServerBuilder;

/// the request as a client sends it, in json throughout
enum InputStatus {}

impl lsp_types::Request for InputStatus {
    type Params = serde_json::Value;
    type Result = serde_json::Value;
    const METHOD: lsp_types::LspRequestMethod<'static> =
        lsp_types::LspRequestMethod::Custom("by/inputStatus");
    const MESSAGE_DIRECTION: lsp_types::MessageDirection =
        lsp_types::MessageDirection::ClientToServer;
}

/// `by/inputStatus` says whether a `by repl` console's text runs at enter, with no document
/// open: a block runs at the blank line after it, a header, a decorator, an open bracket, a
/// continued line or an open triple-quoted string waits for the next line, and input no line
/// can fix runs at once
#[test]
fn input_status_says_whether_enter_runs_the_input() -> anyhow::Result<()> {
    let mut server = TestServerBuilder::new()?
        .build()
        .wait_until_workspaces_are_initialized();

    for (text, status) in [
        ("let rate = 0.2\n", "complete"),
        ("def taxed(amount: float) -> float:\n", "incomplete"),
        (
            "def taxed(amount: float) -> float:\n    return amount * (1 + rate)\n",
            "incomplete",
        ),
        (
            "def taxed(amount: float) -> float:\n    return amount * (1 + rate)\n\n",
            "complete",
        ),
        ("total = (1 +\n", "incomplete"),
        ("x = = 1\n", "invalid"),
        ("try:\n", "incomplete"),
        ("try:\n    risky()\n", "incomplete"),
        ("try:\n    risky()\nexcept ValueError:\n", "incomplete"),
        ("try:\n    risky()\nfinally:\n", "incomplete"),
        ("match command:\n", "incomplete"),
        ("match command:\n    case \"go\":\n", "incomplete"),
        ("with open(path) as f:\n", "incomplete"),
        ("class Shape:\n", "incomplete"),
        ("async def fetch():\n", "incomplete"),
        ("@cache\n", "incomplete"),
        ("if ready:\n    go()\nelif waiting:\n", "incomplete"),
        ("if ready:\n    go()\nelse:\n", "incomplete"),
        ("total = 1 + \\\n", "incomplete"),
        ("doc = \"\"\"one\n", "incomplete"),
        ("try:\n\n", "invalid"),
        ("lambda:\n", "invalid"),
    ] {
        let answer = server.send_request_await::<InputStatus>(serde_json::json!({ "text": text }));
        assert_eq!(answer, serde_json::json!({ "status": status }), "{text:?}");
    }
    Ok(())
}
