use anyhow::Result;
use insta::assert_json_snapshot;
use lsp_types::Request as _;
use lsp_types::{
    DocumentFormattingRequest, DocumentRangeFormattingRequest, Position, Range, RegistrationRequest,
};

use crate::TestServerBuilder;

/// the server names itself as buff, the name it shows in everything else
#[test]
fn server_is_named_buff() -> Result<()> {
    let server = TestServerBuilder::new()?.build();
    let server_info = server
        .initialization_result()
        .expect("Server should return initialization capabilities")
        .server_info
        .as_ref()
        .expect("the server names itself");

    assert_eq!(server_info.name, "buff");

    Ok(())
}

#[test]
fn statically_registers_formatting_when_dynamic_registration_is_unsupported() -> Result<()> {
    let server = TestServerBuilder::new()?.build();
    let capabilities = &server
        .initialization_result()
        .expect("Server should return initialization capabilities")
        .capabilities;

    assert_eq!(capabilities.document_formatting_provider, Some(true.into()));
    assert_eq!(
        capabilities.document_range_formatting_provider,
        Some(true.into())
    );

    Ok(())
}

#[test]
fn dynamically_registers_formatting_and_range_formatting_for_python_basedpython_and_markdown()
-> Result<()> {
    let mut server = TestServerBuilder::new()?
        .enable_formatting_dynamic_registration(true)
        .enable_range_formatting_dynamic_registration(true)
        .build();
    let capabilities = &server
        .initialization_result()
        .expect("Server should return initialization capabilities")
        .capabilities;

    assert_eq!(capabilities.document_formatting_provider, None);
    assert_eq!(capabilities.document_range_formatting_provider, None);

    let (_, params) = server.await_request::<RegistrationRequest>();
    let [formatting, range_formatting] = params.registrations.as_slice() else {
        panic!("Expected both dynamic formatting registrations");
    };

    assert_eq!(
        formatting.method,
        DocumentFormattingRequest::METHOD.as_str()
    );
    assert_eq!(
        range_formatting.method,
        DocumentRangeFormattingRequest::METHOD.as_str()
    );
    assert_json_snapshot!(
        formatting.register_options,
    @r#"
    {
      "documentSelector": [
        {
          "language": "python"
        },
        {
          "language": "basedpython"
        },
        {
          "language": "markdown"
        },
        {
          "language": "python",
          "notebook": "*"
        }
      ]
    }
    "#
    );
    assert_eq!(
        range_formatting.register_options,
        formatting.register_options
    );

    Ok(())
}

/// a `.by` document is range-formatted as python is: only the statements the range covers are laid
/// out again
#[test]
fn range_formats_a_basedpython_document() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();
    server.open_text_document_with_language_id(
        "test.by",
        "basedpython",
        "x=[1,2]\ny   =   {'a':1}\n",
        1,
    );

    let edits = server.format_range_request(
        "test.by",
        Range {
            start: Position {
                line: 1,
                character: 0,
            },
            end: Position {
                line: 1,
                character: 15,
            },
        },
    );

    assert_json_snapshot!(edits, @r#"
    [
      {
        "range": {
          "start": {
            "line": 1,
            "character": 0
          },
          "end": {
            "line": 1,
            "character": 15
          }
        },
        "newText": "y = {\"a\": 1}"
      }
    ]
    "#);

    Ok(())
}
