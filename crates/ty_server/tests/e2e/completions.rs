use anyhow::Result;
use lsp_types::{Documentation, MarkupKind, Position};
use ruff_db::system::SystemPath;
use ty_server::{ClientOptions, ImportStyleOption};

use crate::TestServerBuilder;

/// Tests that auto-import is enabled by default.
#[test]
fn default_auto_import() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
walktr
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(0, 6));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "walktree (import inspect)",
        "kind": 3,
        "sortText": "0",
        "insertText": "walktree()",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 6
            }
          },
          "newText": "walktree()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from inspect import walktree\n\n"
          }
        ]
      }
    ]
    "#);

    Ok(())
}

/// Tests that disabling auto-import works.
#[test]
fn disable_auto_import() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
walktr
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default().with_auto_import(false))
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(0, 6));

    insta::assert_json_snapshot!(completions, @"[]");

    Ok(())
}

#[test]
fn external_file_uses_fallback_workspace_auto_import_setting() {
    for auto_import in [false, true] {
        let path = SystemPath::new("scratch.py");
        let source = "walktr";
        // Register tools before app/src to check that path order determines the fallback.
        // Leave initialization options unset so the workspaces retain distinct settings.
        let mut server = TestServerBuilder::new()
            .expect("create test server builder")
            .with_workspace(
                SystemPath::new("tools"),
                Some(ClientOptions::default().with_auto_import(!auto_import)),
            )
            .expect("register tools workspace")
            .with_workspace(
                SystemPath::new("app/src"),
                Some(ClientOptions::default().with_auto_import(auto_import)),
            )
            .expect("register app/src workspace")
            .with_files([(SystemPath::new("app/ty.toml"), ""), (path, source)])
            .expect("write project configuration and external file")
            .build()
            .wait_until_workspaces_are_initialized();

        server.open_text_document(path, source, 1);
        let completions = server.completion_request(&server.file_uri(path), Position::new(0, 6));
        assert_eq!(
            completions
                .iter()
                .any(|completion| completion.label == "walktree (import inspect)"),
            auto_import,
        );
    }
}

/// basedpython enables callable parentheses by default where upstream ty leaves
/// them off, so the no-options case is the *enabled* one — see
/// `CompletionSettings::default`.
#[test]
fn complete_function_parentheses_enabled_by_default() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def complete_parentheses(x: int) -> None: ...

complete_parenth
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .enable_completion_snippets(true)
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 16));
    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "complete_parentheses",
        "kind": 3,
        "detail": "def complete_parentheses(x: int)",
        "sortText": "0",
        "insertText": "complete_parentheses($0)",
        "insertTextFormat": 2,
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 16
            }
          },
          "newText": "complete_parentheses($0)"
        }
      }
    ]
    "#);

    Ok(())
}

#[test]
fn complete_function_parentheses() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def complete_parentheses(x: int) -> None: ...

complete_parenth
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(
            &ClientOptions::default().with_complete_function_parentheses(true),
        )
        .enable_completion_snippets(true)
        .with_trigger_parameter_hints_command()
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 16));
    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "complete_parentheses",
        "kind": 3,
        "detail": "def complete_parentheses(x: int)",
        "sortText": "0",
        "insertText": "complete_parentheses($0)",
        "insertTextFormat": 2,
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 16
            }
          },
          "newText": "complete_parentheses($0)"
        },
        "command": {
          "title": "Trigger parameter hints",
          "command": "ty.triggerParameterHints"
        }
      }
    ]
    "#);

    Ok(())
}

/// Tests that the signature-help command is omitted when the client has not
/// advertised support for it (for example, editors that would surface an
/// "unsupported command" error).
/// a function that takes no arguments is written whole, `()` included, with the caret after it:
/// there is nothing to type between the parentheses and no signature to show
#[test]
fn complete_function_parentheses_without_arguments() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def complete_parentheses() -> None: ...

complete_parenth
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .enable_completion_snippets(true)
        .with_trigger_parameter_hints_command()
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 16));
    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "complete_parentheses",
        "kind": 3,
        "detail": "def complete_parentheses()",
        "sortText": "0",
        "insertText": "complete_parentheses()",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 16
            }
          },
          "newText": "complete_parentheses()"
        }
      }
    ]
    "#);

    Ok(())
}

/// `completions.completeFunctionParentheses: false` in the initialization options turns the
/// parentheses off, for a client whose user does not want them
#[test]
fn complete_function_parentheses_disabled() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def complete_parentheses(x: int) -> None: ...

complete_parenth
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(
            &ClientOptions::default().with_complete_function_parentheses(false),
        )
        .enable_completion_snippets(true)
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 16));
    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "complete_parentheses",
        "kind": 3,
        "detail": "def complete_parentheses(x: int)",
        "sortText": "0",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 16
            }
          },
          "newText": "complete_parentheses"
        }
      }
    ]
    "#);

    Ok(())
}

#[test]
fn complete_function_parentheses_without_command_support() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def complete_parentheses(x: int) -> None: ...

complete_parenth
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(
            &ClientOptions::default().with_complete_function_parentheses(true),
        )
        .enable_completion_snippets(true)
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 16));
    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "complete_parentheses",
        "kind": 3,
        "detail": "def complete_parentheses(x: int)",
        "sortText": "0",
        "insertText": "complete_parentheses($0)",
        "insertTextFormat": 2,
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 16
            }
          },
          "newText": "complete_parentheses($0)"
        }
      }
    ]
    "#);

    Ok(())
}

#[test]
fn complete_function_parentheses_without_snippet_support() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
def complete_parentheses() -> None: ...

complete_parenth
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(
            &ClientOptions::default().with_complete_function_parentheses(true),
        )
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 16));
    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "complete_parentheses",
        "kind": 3,
        "detail": "def complete_parentheses()",
        "sortText": "0",
        "insertText": "complete_parentheses()",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 16
            }
          },
          "newText": "complete_parentheses()"
        }
      }
    ]
    "#);

    Ok(())
}

#[test]
fn complete_function_parentheses_preserves_qualified_label() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
import typing

is_typedd
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(
            &ClientOptions::default().with_complete_function_parentheses(true),
        )
        .enable_completion_snippets(true)
        .with_trigger_parameter_hints_command()
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 8));
    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "typing.is_typeddict",
        "kind": 3,
        "sortText": "0",
        "filterText": "is_typeddict",
        "insertText": "typing.is_typeddict($0)",
        "insertTextFormat": 2,
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 8
            }
          },
          "newText": "typing.is_typeddict($0)"
        },
        "command": {
          "title": "Trigger parameter hints",
          "command": "ty.triggerParameterHints"
        }
      },
      {
        "label": "is_typeddict (import typing_extensions)",
        "kind": 3,
        "sortText": "1",
        "insertText": "is_typeddict($0)",
        "insertTextFormat": 2,
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 8
            }
          },
          "newText": "is_typeddict($0)"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 1,
                "character": 0
              },
              "end": {
                "line": 1,
                "character": 0
              }
            },
            "newText": "\nfrom typing_extensions import is_typeddict\n"
          }
        ],
        "command": {
          "title": "Trigger parameter hints",
          "command": "ty.triggerParameterHints"
        }
      }
    ]
    "#);

    Ok(())
}

/// Tests that auto-import completions show the fully
/// qualified form when it will insert it for you. Also,
/// that an `import` won't be shown when it won't
/// actually be inserted.
#[test]
fn auto_import_shows_qualification() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
import typing

TypedDi<CURSOR>
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 7));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "typing.TypedDict",
        "kind": 6,
        "sortText": "0",
        "filterText": "TypedDict",
        "insertText": "typing.TypedDict",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "typing.TypedDict"
        }
      },
      {
        "label": "typing.is_typeddict",
        "kind": 3,
        "sortText": "1",
        "filterText": "is_typeddict",
        "insertText": "typing.is_typeddict()",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "typing.is_typeddict()"
        }
      },
      {
        "label": "TypedDict (import typing_extensions)",
        "kind": 6,
        "sortText": "2",
        "insertText": "TypedDict",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "TypedDict"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 1,
                "character": 0
              },
              "end": {
                "line": 1,
                "character": 0
              }
            },
            "newText": "\nfrom typing_extensions import TypedDict\n"
          }
        ]
      },
      {
        "label": "TypedDictFallback (import _typeshed._type_checker_internals)",
        "kind": 7,
        "sortText": "3",
        "insertText": "TypedDictFallback()",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "TypedDictFallback()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 1,
                "character": 0
              },
              "end": {
                "line": 1,
                "character": 0
              }
            },
            "newText": "\nfrom _typeshed._type_checker_internals import TypedDictFallback\n"
          }
        ]
      },
      {
        "label": "is_typeddict (import typing_extensions)",
        "kind": 3,
        "sortText": "4",
        "insertText": "is_typeddict()",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "is_typeddict()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 1,
                "character": 0
              },
              "end": {
                "line": 1,
                "character": 0
              }
            },
            "newText": "\nfrom typing_extensions import is_typeddict\n"
          }
        ]
      },
      {
        "label": "_FilterConfigurationTypedDict (import logging.config)",
        "kind": 7,
        "sortText": "5",
        "insertText": "_FilterConfigurationTypedDict()",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "_FilterConfigurationTypedDict()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 1,
                "character": 0
              },
              "end": {
                "line": 1,
                "character": 0
              }
            },
            "newText": "from logging.config import _FilterConfigurationTypedDict\n"
          }
        ]
      },
      {
        "label": "_FormatterConfigurationTypedDict (import logging.config)",
        "kind": 6,
        "sortText": "6",
        "insertText": "_FormatterConfigurationTypedDict",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "_FormatterConfigurationTypedDict"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 1,
                "character": 0
              },
              "end": {
                "line": 1,
                "character": 0
              }
            },
            "newText": "from logging.config import _FormatterConfigurationTypedDict\n"
          }
        ]
      },
      {
        "label": "_typeshed.dbapi (import _typeshed.dbapi)",
        "kind": 9,
        "sortText": "7",
        "insertText": "_typeshed.dbapi",
        "textEdit": {
          "range": {
            "start": {
              "line": 2,
              "character": 0
            },
            "end": {
              "line": 2,
              "character": 7
            }
          },
          "newText": "_typeshed.dbapi"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 1,
                "character": 0
              },
              "end": {
                "line": 1,
                "character": 0
              }
            },
            "newText": "\nimport _typeshed.dbapi\n"
          }
        ]
      }
    ]
    "#);

    Ok(())
}

/// Tests that auto-import completions show the fully
/// qualified form when it will insert it for you *and*
/// will also show the import when it will be inserted.
#[test]
fn auto_import_shows_qualification_and_import() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
TypedDi<CURSOR>
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(0, 7));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "TypedDict (import typing)",
        "kind": 6,
        "sortText": "0",
        "insertText": "TypedDict",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "TypedDict"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from typing import TypedDict\n\n"
          }
        ]
      },
      {
        "label": "is_typeddict (import typing)",
        "kind": 3,
        "sortText": "1",
        "insertText": "is_typeddict()",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "is_typeddict()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from typing import is_typeddict\n\n"
          }
        ]
      },
      {
        "label": "TypedDict (import typing_extensions)",
        "kind": 6,
        "sortText": "2",
        "insertText": "TypedDict",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "TypedDict"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from typing_extensions import TypedDict\n\n"
          }
        ]
      },
      {
        "label": "TypedDictFallback (import _typeshed._type_checker_internals)",
        "kind": 7,
        "sortText": "3",
        "insertText": "TypedDictFallback()",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "TypedDictFallback()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from _typeshed._type_checker_internals import TypedDictFallback\n\n"
          }
        ]
      },
      {
        "label": "is_typeddict (import typing_extensions)",
        "kind": 3,
        "sortText": "4",
        "insertText": "is_typeddict()",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "is_typeddict()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from typing_extensions import is_typeddict\n\n"
          }
        ]
      },
      {
        "label": "_FilterConfigurationTypedDict (import logging.config)",
        "kind": 7,
        "sortText": "5",
        "insertText": "_FilterConfigurationTypedDict()",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "_FilterConfigurationTypedDict()"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from logging.config import _FilterConfigurationTypedDict\n\n"
          }
        ]
      },
      {
        "label": "_FormatterConfigurationTypedDict (import logging.config)",
        "kind": 6,
        "sortText": "6",
        "insertText": "_FormatterConfigurationTypedDict",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "_FormatterConfigurationTypedDict"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "from logging.config import _FormatterConfigurationTypedDict\n\n"
          }
        ]
      },
      {
        "label": "_typeshed.dbapi (import _typeshed.dbapi)",
        "kind": 9,
        "sortText": "7",
        "insertText": "_typeshed.dbapi",
        "textEdit": {
          "range": {
            "start": {
              "line": 0,
              "character": 0
            },
            "end": {
              "line": 0,
              "character": 7
            }
          },
          "newText": "_typeshed.dbapi"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 0
              },
              "end": {
                "line": 0,
                "character": 0
              }
            },
            "newText": "import _typeshed.dbapi\n\n"
          }
        ]
      }
    ]
    "#);

    Ok(())
}

/// Tests that completions for function arguments will
/// show a `=` suffix.
#[test]
fn function_parameter_shows_equals_suffix() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
import re
re.match('', '', fla<CURSOR>
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default().with_auto_import(false))
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(1, 20));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "flags=",
        "kind": 6,
        "detail": "FlagsType",
        "sortText": "0",
        "filterText": "flags",
        "insertText": "flags=",
        "textEdit": {
          "range": {
            "start": {
              "line": 1,
              "character": 17
            },
            "end": {
              "line": 1,
              "character": 20
            }
          },
          "newText": "flags="
        }
      },
      {
        "label": "FloatingPointError",
        "kind": 7,
        "detail": "<class 'FloatingPointError'>",
        "documentation": {
          "kind": "plaintext",
          "value": "Floating-point operation failed.\n"
        },
        "sortText": "1",
        "insertText": "FloatingPointError()",
        "textEdit": {
          "range": {
            "start": {
              "line": 1,
              "character": 17
            },
            "end": {
              "line": 1,
              "character": 20
            }
          },
          "newText": "FloatingPointError()"
        }
      },
      {
        "label": "PythonFinalizationError",
        "kind": 7,
        "detail": "<class 'PythonFinalizationError'>",
        "documentation": {
          "kind": "plaintext",
          "value": "Operation blocked during Python finalization.\n"
        },
        "sortText": "2",
        "insertText": "PythonFinalizationError()",
        "textEdit": {
          "range": {
            "start": {
              "line": 1,
              "character": 17
            },
            "end": {
              "line": 1,
              "character": 20
            }
          },
          "newText": "PythonFinalizationError()"
        }
      },
      {
        "label": "float",
        "kind": 7,
        "detail": "<class 'float'>",
        "documentation": {
          "kind": "plaintext",
          "value": "Convert a string or number to a floating-point number, if possible.\n"
        },
        "sortText": "3",
        "insertText": "float()",
        "textEdit": {
          "range": {
            "start": {
              "line": 1,
              "character": 17
            },
            "end": {
              "line": 1,
              "character": 20
            }
          },
          "newText": "float()"
        }
      }
    ]
    "#);

    Ok(())
}

/// Tests the LSP-facing shape for string-literal completions with an already-typed prefix.
///
/// The server intentionally returns the full completion in `insertText`. Without an explicit
/// `textEdit`, LSP clients are allowed to interpret that insert text relative to the current
/// word; for example, VS Code applies `insertText: "apple"` at `app|` as the suffix `le`.
#[test]
fn string_literal_completion_uses_full_lsp_insert_text() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
from typing import Literal
x: Literal[\"apple\"] = \"app\"
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default().with_auto_import(false))
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(1, 26));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "apple",
        "kind": 12,
        "detail": "Literal[\"apple\"]",
        "sortText": "0",
        "insertText": "apple",
        "textEdit": {
          "range": {
            "start": {
              "line": 1,
              "character": 23
            },
            "end": {
              "line": 1,
              "character": 26
            }
          },
          "newText": "apple"
        }
      }
    ]
    "#);

    Ok(())
}

/// A name completed inside a plain string's braces reaches the client with the
/// `f` that makes the string an f-string as a further edit of its own.
#[test]
fn string_field_completion_carries_the_fstring_prefix() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
class Thing:
    def __str__(self) -> str: ...

zqzqzq = Thing()
\"hello {zqzq\"
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default().with_auto_import(false))
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(4, 12));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "zqzqzq",
        "kind": 22,
        "detail": "Thing",
        "sortText": "0",
        "insertText": "zqzqzq}",
        "textEdit": {
          "range": {
            "start": {
              "line": 4,
              "character": 8
            },
            "end": {
              "line": 4,
              "character": 12
            }
          },
          "newText": "zqzqzq}"
        },
        "additionalTextEdits": [
          {
            "range": {
              "start": {
                "line": 4,
                "character": 0
              },
              "end": {
                "line": 4,
                "character": 0
              }
            },
            "newText": "f"
          }
        ]
      }
    ]
    "#);

    Ok(())
}

/// The type shown beside a suggestion is read as source, so in a basedpython
/// file it is spelled the way it would be written there.
#[test]
fn completion_detail_uses_basedpython_display() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.by");
    let foo_content = "\
from typing import Literal
x: Literal[\"apple\"] = \"app\"
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default().with_auto_import(false))
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(1, 26));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "apple",
        "kind": 12,
        "detail": "\"apple\"",
        "sortText": "0",
        "insertText": "apple",
        "textEdit": {
          "range": {
            "start": {
              "line": 1,
              "character": 23
            },
            "end": {
              "line": 1,
              "character": 26
            }
          },
          "newText": "apple"
        }
      }
    ]
    "#);

    Ok(())
}

#[test]
fn typed_dict_literal_key_completion_before_colon() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
from typing import TypedDict

class Box(TypedDict):
    x: float
    y: float
    z: float

def take(box: Box): ...

take({\"\"})
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default().with_auto_import(false))
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(9, 7));

    insta::assert_json_snapshot!(completions, @r#"
    [
      {
        "label": "x",
        "kind": 12,
        "detail": "Literal[\"x\"]",
        "sortText": "0",
        "insertText": "x",
        "textEdit": {
          "range": {
            "start": {
              "line": 9,
              "character": 7
            },
            "end": {
              "line": 9,
              "character": 7
            }
          },
          "newText": "x"
        }
      },
      {
        "label": "y",
        "kind": 12,
        "detail": "Literal[\"y\"]",
        "sortText": "1",
        "insertText": "y",
        "textEdit": {
          "range": {
            "start": {
              "line": 9,
              "character": 7
            },
            "end": {
              "line": 9,
              "character": 7
            }
          },
          "newText": "y"
        }
      },
      {
        "label": "z",
        "kind": 12,
        "detail": "Literal[\"z\"]",
        "sortText": "2",
        "insertText": "z",
        "textEdit": {
          "range": {
            "start": {
              "line": 9,
              "character": 7
            },
            "end": {
              "line": 9,
              "character": 7
            }
          },
          "newText": "z"
        }
      }
    ]
    "#);

    Ok(())
}

#[test]
fn documentation_prefers_markdown_when_listed_first() -> Result<()> {
    assert_eq!(
        documentation_format(vec![MarkupKind::Markdown, MarkupKind::PlainText])?,
        MarkupKind::Markdown,
    );
    Ok(())
}

#[test]
fn documentation_prefers_plain_text_when_listed_first() -> Result<()> {
    assert_eq!(
        documentation_format(vec![MarkupKind::PlainText, MarkupKind::Markdown])?,
        MarkupKind::PlainText,
    );
    Ok(())
}

#[test]
fn documentation_supports_only_markdown() -> Result<()> {
    assert_eq!(
        documentation_format(vec![MarkupKind::Markdown])?,
        MarkupKind::Markdown
    );
    Ok(())
}

#[test]
fn documentation_supports_only_plain_text() -> Result<()> {
    assert_eq!(
        documentation_format(vec![MarkupKind::PlainText])?,
        MarkupKind::PlainText
    );
    Ok(())
}

fn documentation_format(formats: Vec<MarkupKind>) -> Result<MarkupKind> {
    let workspace_root = SystemPath::new("src");
    let document_path = SystemPath::new("src/foo.py");
    let document_content = r#"def foo_with_documentation() -> None:
    """
    Example doc comment
    """
    ...

foo_
"#;
    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(document_path, document_content)?
        .with_completion_documentation_format(formats)
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(document_path, document_content, 1);

    let completions =
        server.completion_request(&server.file_uri(document_path), Position::new(6, 4));

    let completion = completions
        .into_iter()
        .find(|completion| completion.label == "foo_with_documentation")
        .expect("Completion of function should exist");
    let documentation = completion
        .documentation
        .expect("Expected documentation in completion");

    let Documentation::MarkupContent(markup) = documentation else {
        panic!("Expected markup documentation");
    };

    Ok(markup.kind)
}

/// `completions.importStyle: "module"` imports the module an unimported name is in and writes
/// the name through it, where the default writes `from module import name`
#[test]
fn import_style_module_imports_the_module() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
import os

walktr
";

    let mut server = TestServerBuilder::new()?
        .with_initialization_options(
            &ClientOptions::default()
                .with_complete_function_parentheses(false)
                .with_import_style(ImportStyleOption::Module),
        )
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let completions = server.completion_request(&server.file_uri(foo), Position::new(2, 6));
    let offered: Vec<String> = completions
        .iter()
        .map(|item| {
            let imports: Vec<&str> = item
                .additional_text_edits
                .iter()
                .flatten()
                .map(|edit| edit.new_text.as_str())
                .collect();
            format!(
                "{}: {} with {imports:?}",
                item.label,
                item.insert_text.as_deref().unwrap_or(&item.label)
            )
        })
        .collect();
    insta::assert_debug_snapshot!(offered, @r#"
    [
        "inspect.walktree (import inspect): inspect.walktree with [\"import inspect/n\"]",
    ]
    "#);

    Ok(())
}

/// every completion says what it replaces, so that a client never falls back on its own idea of
/// the word before the cursor: inside an f-string's replacement field that is the whole string
/// token, against which no name matches, and inside a literal it is the last word of it
#[test]
fn every_completion_says_what_it_replaces() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
from typing import Literal

def pick(mode: Literal['slow-mode']) -> None: ...

cart = 1
label = f'{car}'
pick('slow-mo')
";
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(foo, foo_content, 1);
    let uri = server.file_uri(foo);

    let replaced = |completions: &[lsp_types::CompletionItem], label: &str| {
        assert!(
            completions.iter().all(|item| item.text_edit.is_some()),
            "{completions:#?}"
        );
        completions
            .iter()
            .find(|item| item.label == label)
            .and_then(|item| match &item.text_edit {
                Some(lsp_types::CompletionItemTextEdit::TextEdit(edit)) => {
                    Some((edit.range, edit.new_text.clone()))
                }
                _ => None,
            })
    };

    // `car` in the field, not `f'{car`
    let in_field = server.completion_request(&uri, Position::new(5, 14));
    assert_eq!(
        replaced(&in_field, "cart"),
        Some((
            lsp_types::Range::new(Position::new(5, 11), Position::new(5, 14)),
            "cart".to_string()
        ))
    );

    // all of the literal before the cursor, not its last word `mo`
    let in_literal = server.completion_request(&uri, Position::new(6, 13));
    assert_eq!(
        replaced(&in_literal, "slow-mode"),
        Some((
            lsp_types::Range::new(Position::new(6, 6), Position::new(6, 13)),
            "slow-mode".to_string()
        ))
    );
    Ok(())
}

/// `by/smartCompletion` as a client sends it, in json throughout
enum SmartCompletion {}

impl lsp_types::Request for SmartCompletion {
    type Params = serde_json::Value;
    type Result = Option<lsp_types::CompletionList>;
    const METHOD: lsp_types::LspRequestMethod<'static> =
        lsp_types::LspRequestMethod::Custom("by/smartCompletion");
    const MESSAGE_DIRECTION: lsp_types::MessageDirection =
        lsp_types::MessageDirection::ClientToServer;
}

/// smart completion answers, in the form `textDocument/completion` answers, only what fits the
/// type the position expects, and `null` where it expects none
#[test]
fn smart_completion_answers_what_fits_the_expected_type() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let foo_content = "\
class Shape: ...
class Label: ...

def draw(shape: Shape) -> None: ...

shape = Shape()
label = Label()
draw()
x = 
";
    let mut server = TestServerBuilder::new()?
        .with_initialization_options(&ClientOptions::default())
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .build()
        .wait_until_workspaces_are_initialized();
    server.open_text_document(foo, foo_content, 1);
    let uri = server.file_uri(foo);
    let ask = |server: &mut crate::TestServer, position: Position| {
        server.send_request_await::<SmartCompletion>(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": position,
        }))
    };

    let in_argument = ask(&mut server, Position::new(7, 5)).expect("an argument expects a type");
    let labels: Vec<_> = in_argument
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(labels, ["Shape", "shape"]);
    assert!(
        in_argument
            .items
            .iter()
            .all(|item| item.text_edit.is_some())
    );

    assert_eq!(ask(&mut server, Position::new(8, 4)), None);
    Ok(())
}
