use anyhow::Result;
use ruff_db::system::SystemPath;

use crate::TestServerBuilder;

#[test]
fn script_metadata_is_highlighted_as_toml() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let script = SystemPath::new("src/script.py");
    let source = r#"#!/usr/bin/env python3
# /// script
# dependencies = ["httpx"]
# requires-python = ">=3.12"
# ///
value = 1
"#;

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(script, source)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(script, source, 1);

    let tokens = server
        .semantic_tokens_full_request(&server.file_uri(script))
        .ok_or_else(|| anyhow::anyhow!("expected semantic tokens for the script"))?;

    let actual: Vec<_> = tokens
        .data
        .into_iter()
        .map(|token| {
            (
                token.delta_line,
                token.delta_start,
                token.length,
                token.token_type,
            )
        })
        .collect();

    assert_eq!(
        actual,
        vec![
            (2, 2, 12, ty_ide::SemanticTokenType::Variable as u32),
            (0, 13, 1, ty_ide::SemanticTokenType::Operator as u32),
            (0, 2, 1, ty_ide::SemanticTokenType::Operator as u32),
            (0, 1, 7, ty_ide::SemanticTokenType::String as u32),
            (0, 7, 1, ty_ide::SemanticTokenType::Operator as u32),
            (1, 2, 15, ty_ide::SemanticTokenType::Variable as u32),
            (0, 16, 1, ty_ide::SemanticTokenType::Operator as u32),
            (0, 2, 8, ty_ide::SemanticTokenType::String as u32),
            (2, 0, 5, ty_ide::SemanticTokenType::Variable as u32),
            (0, 8, 1, ty_ide::SemanticTokenType::Number as u32),
        ]
    );

    Ok(())
}

#[test]
fn multiline_token_client_not_supporting_multiline_tokens() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = r#"def my_function(param1: int, param2: str) -> bool:
    """Example function with PEP 484 type annotations.

    Args:
        param1: The first parameter.
        param2: The second parameter.

    Returns:
        The return value. True for success, False otherwise.

    """
"#;

    let mut server = TestServerBuilder::new()?
        .enable_multiline_token_support(false)
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let tokens = server.semantic_tokens_full_request(&server.file_uri(foo));

    insta::assert_json_snapshot!(tokens);

    Ok(())
}

#[test]
fn multiline_token_client_supporting_multiline_tokens() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = r#"def my_function(param1: int, param2: str) -> bool:
    """Example function with PEP 484 type annotations.

    Args:
        param1: The first parameter.
        param2: The second parameter.

    Returns:
        The return value. True for success, False otherwise.

    """
"#;

    let mut server = TestServerBuilder::new()?
        .enable_multiline_token_support(true)
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let tokens = server.semantic_tokens_full_request(&server.file_uri(foo));

    insta::assert_json_snapshot!(tokens);

    Ok(())
}

// Regression test for https://github.com/astral-sh/ty/issues/2346
#[test]
fn no_stale_tokens_after_opening_the_same_file_with_new_content() -> Result<()> {
    let file_name = "src/foo";
    let initial_content =
        "def calculate_sum(a):\n # Version A: Basic math\n return a\n\nresult = calculate_sum(5)\n";
    let mut server = TestServerBuilder::new()?
        .enable_multiline_token_support(true)
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(file_name, initial_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(file_name, initial_content, 0);

    let initial_tokens = server
        .semantic_tokens_full_request(&server.file_uri(file_name))
        .unwrap();

    server.close_text_document(file_name);

    server.open_text_document(
        file_name,
        "# Version B: Basic greeting\ndef say_hello():\n print(\"Hello, World!\")\n\nsay_hello()\n",
        0,
    );

    let new_tokens = server
        .semantic_tokens_full_request(&server.file_uri(file_name))
        .unwrap();

    assert_ne!(initial_tokens, new_tokens);

    Ok(())
}

const UNREACHABLE: &str = "\
def f():
    print(exit())
    return
    print(1)
";

/// what a client asks for code reported as unreachable
#[derive(Clone, Copy)]
struct Asks {
    without: bool,
    modifier: bool,
}

const NOTHING: Asks = Asks {
    without: false,
    modifier: false,
};

/// the tokens of `UNREACHABLE` as `(line, character, text, unreachable)`, from a full request or,
/// given one, a request for `range`
fn unreachable_tokens(
    asks: Asks,
    range: Option<lsp_types::Range>,
) -> Result<Vec<(u32, u32, String, bool)>> {
    let workspace_root = SystemPath::new("src");
    let path = SystemPath::new("src/foo.py");
    let mut builder = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(path, UNREACHABLE)?;
    if asks.without {
        builder = builder.with_unreachable_without_semantic_tokens();
    }
    if asks.modifier {
        builder = builder.with_unreachable_semantic_token_modifier();
    }
    let mut server = builder.build().wait_until_workspaces_are_initialized();
    server.open_text_document(path, UNREACHABLE, 1);
    let uri = server.file_uri(path);

    let tokens = match range {
        None => server.semantic_tokens_full_request(&uri),
        Some(range) => server.send_request_await::<lsp_types::SemanticTokensRangeRequest>(
            lsp_types::SemanticTokensRangeParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                range,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
        ),
    }
    .ok_or_else(|| anyhow::anyhow!("expected semantic tokens"))?
    .data;

    let unreachable = 1
        << ty_ide::SemanticTokenModifier::UNREACHABLE
            .bits()
            .trailing_zeros();
    let lines: Vec<&str> = UNREACHABLE.lines().collect();
    let (mut line, mut character) = (0, 0);
    Ok(tokens
        .into_iter()
        .map(|token| {
            if token.delta_line == 0 {
                character += token.delta_start;
            } else {
                line += token.delta_line;
                character = token.delta_start;
            }
            let text = &lines[line as usize][character as usize..][..token.length as usize];
            (
                line,
                character,
                text.to_string(),
                token.token_modifiers_bitset & unreachable != 0,
            )
        })
        .collect())
}

fn token(line: u32, character: u32, text: &str) -> (u32, u32, String, bool) {
    (line, character, text.to_string(), false)
}

fn faded(line: u32, character: u32, text: &str) -> (u32, u32, String, bool) {
    (line, character, text.to_string(), true)
}

/// an editor that greys unreachable code with a colour of its own can ask for no token inside
/// it, so that the token's colour does not win over the grey
#[test]
fn unreachable_code_has_no_tokens_when_the_client_asks() -> Result<()> {
    let asks = Asks {
        without: true,
        modifier: false,
    };
    assert_eq!(
        unreachable_tokens(asks, None)?,
        [token(0, 4, "f"), token(1, 4, "print"), token(1, 10, "exit")]
    );
    Ok(())
}

#[test]
fn unreachable_code_has_no_tokens_in_a_range_when_the_client_asks() -> Result<()> {
    let asks = Asks {
        without: true,
        modifier: false,
    };
    let range = lsp_types::Range::new(
        lsp_types::Position::new(1, 0),
        lsp_types::Position::new(3, 12),
    );
    assert_eq!(
        unreachable_tokens(asks, Some(range))?,
        [token(1, 4, "print"), token(1, 10, "exit")]
    );
    Ok(())
}

/// an editor that fades unreachable code in its own colours asks for every character of it to
/// carry a token that says so: the parentheses of the call `exit()` keeps from being made, and
/// every token of the statements after it, `return` and the punctuation included
#[test]
fn every_character_of_unreachable_code_has_a_marked_token_when_the_client_asks() -> Result<()> {
    let marked = [
        token(0, 4, "f"),
        token(1, 4, "print"),
        faded(1, 9, "("),
        token(1, 10, "exit"),
        faded(1, 16, ")"),
        faded(2, 4, "return"),
        faded(3, 4, "print"),
        faded(3, 9, "("),
        faded(3, 10, "1"),
        faded(3, 11, ")"),
    ];
    let asks = Asks {
        without: false,
        modifier: true,
    };
    assert_eq!(unreachable_tokens(asks, None)?, marked);
    // asked for both, the marks win
    let both = Asks {
        without: true,
        modifier: true,
    };
    assert_eq!(unreachable_tokens(both, None)?, marked);
    Ok(())
}

#[test]
fn unreachable_code_in_a_range_has_marked_tokens_when_the_client_asks() -> Result<()> {
    let asks = Asks {
        without: false,
        modifier: true,
    };
    let range = lsp_types::Range::new(
        lsp_types::Position::new(3, 0),
        lsp_types::Position::new(3, 10),
    );
    assert_eq!(
        unreachable_tokens(asks, Some(range))?,
        [faded(3, 4, "print"), faded(3, 9, "(")]
    );
    // a token the range ends inside is answered whole, and is no less unreachable
    let range = lsp_types::Range::new(
        lsp_types::Position::new(3, 0),
        lsp_types::Position::new(3, 6),
    );
    assert_eq!(
        unreachable_tokens(asks, Some(range))?,
        [faded(3, 4, "print")]
    );
    Ok(())
}

/// an editor that fades unreachable code over the tokens it already has asks for neither
#[test]
fn unreachable_code_keeps_its_tokens_otherwise() -> Result<()> {
    assert_eq!(
        unreachable_tokens(NOTHING, None)?,
        [
            token(0, 4, "f"),
            token(1, 4, "print"),
            token(1, 10, "exit"),
            token(3, 4, "print"),
            token(3, 10, "1"),
        ]
    );
    Ok(())
}
