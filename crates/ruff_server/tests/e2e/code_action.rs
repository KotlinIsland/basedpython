use anyhow::Result;
use insta::assert_json_snapshot;
use lsp_types::{
    Code, CodeAction, CodeActionKind, CodeActionResolveRequest, CodeActionResponse,
    DocumentDiagnosticReport,
};

use crate::TestServerBuilder;

fn assert_code_action_resolve_unchanged(server: &mut crate::TestServer, action: &CodeAction) {
    let request_id = server.send_request::<CodeActionResolveRequest>(action.clone());
    assert_eq!(
        &server.await_response::<CodeActionResolveRequest>(&request_id),
        action
    );
}

#[test]
fn no_code_actions_for_markdown() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    server.open_text_document_with_language_id("test.md", "markdown", "# Hello", 1);

    let actions = server
        .code_action_request("test.md", vec![])
        .expect("Expected Some response");

    assert_json_snapshot!(actions, @"[]");

    Ok(())
}

#[test]
fn code_actions_for_python() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    server.open_text_document("test.py", "import os\n", 1);

    let actions = server
        .code_action_request("test.py", vec![])
        .expect("Expected Some response");

    assert_json_snapshot!(
        actions,
        @r#"
    [
      {
        "title": "Ruff: Fix all auto-fixable problems",
        "kind": "source.fixAll.ruff",
        "edit": {
          "changes": {
            "file://<temp_dir>/test.py": [
              {
                "range": {
                  "start": {
                    "line": 0,
                    "character": 0
                  },
                  "end": {
                    "line": 1,
                    "character": 0
                  }
                },
                "newText": ""
              }
            ]
          }
        }
      },
      {
        "title": "Ruff: Organize imports",
        "kind": "source.organizeImports.ruff",
        "edit": {
          "changes": {}
        }
      }
    ]
    "#
    );

    Ok(())
}

#[test]
fn code_actions_for_toml() -> Result<()> {
    let source = r#"
[lint]
preview = true
select = ["rule-codes-in-selectors"]
extend-select = ["F401"]
"#;
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .with_file("ruff.toml", source)?
        .build();

    server.open_text_document_with_language_id("ruff.toml", "toml", source, 1);

    let diagnostics = match server.document_diagnostic_request("ruff.toml", None) {
        DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) => {
            report.full_document_diagnostic_report.items
        }
        DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(_) => {
            panic!("Expected a full diagnostic report");
        }
    };
    let actions = server
        .code_action_request("ruff.toml", diagnostics)
        .expect("Expected code actions");

    assert_json_snapshot!(actions, @r#"
    [
      {
        "title": "Ruff (rule-codes-in-selectors): Replace rule code with `unused-import`",
        "kind": "quickfix",
        "diagnostics": [
          {
            "range": {
              "start": {
                "line": 4,
                "character": 18
              },
              "end": {
                "line": 4,
                "character": 22
              }
            },
            "severity": 2,
            "code": "rule-codes-in-selectors",
            "codeDescription": {
              "href": "https://kotlinisland.github.io/basedpython/rules/rule-codes-in-selectors"
            },
            "source": "Ruff",
            "message": "Rule code used instead of name in `lint.extend-select`\n\nhelp: Replace rule code with `unused-import`",
            "tags": []
          }
        ],
        "edit": {
          "changes": {
            "file://<temp_dir>/ruff.toml": [
              {
                "range": {
                  "start": {
                    "line": 4,
                    "character": 18
                  },
                  "end": {
                    "line": 4,
                    "character": 22
                  }
                },
                "newText": "unused-import"
              }
            ]
          }
        },
        "data": "file://<temp_dir>/ruff.toml"
      },
      {
        "title": "Ruff: Fix all auto-fixable problems",
        "kind": "source.fixAll.ruff",
        "edit": {
          "changes": {
            "file://<temp_dir>/ruff.toml": [
              {
                "range": {
                  "start": {
                    "line": 4,
                    "character": 0
                  },
                  "end": {
                    "line": 5,
                    "character": 0
                  }
                },
                "newText": "extend-select = [\"unused-import\"]\n"
              }
            ]
          }
        }
      }
    ]
    "#);

    Ok(())
}

#[test]
fn human_readable_rule_names() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .with_file(
            "pyproject.toml",
            r#"
[tool.ruff]
preview = true
"#,
        )?
        .build();

    server.open_text_document("test.py", "import os\n", 1);

    let diagnostics = match server.document_diagnostic_request("test.py", None) {
        DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) => {
            report.full_document_diagnostic_report.items
        }
        DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(_) => {
            panic!("Expected a full diagnostic report");
        }
    };
    assert_eq!(
        diagnostics[0].code,
        Some(Code::String("unused-import".to_string()))
    );

    let actions = server
        .code_action_request("test.py", diagnostics)
        .expect("Expected code actions");
    let titles: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            CodeActionResponse::CodeAction(action) => Some(action.title.as_str()),
            CodeActionResponse::Command(_) => None,
        })
        .collect();

    assert!(titles.contains(&"Ruff (unused-import): Remove unused import: `os`"));
    assert!(titles.contains(&"Ruff (unused-import): Disable for this line"));

    Ok(())
}

#[test]
fn code_action_without_valid_url_returns_unchanged_action() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    let action = CodeAction {
        title: "Some other code action".to_string(),
        kind: Some(CodeActionKind::QuickFix),
        ..Default::default()
    };

    assert_code_action_resolve_unchanged(&mut server, &action);

    Ok(())
}

#[test]
fn invalid_code_action_resolve_data_returns_unchanged_action() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    let action = CodeAction {
        title: "Ruff: Fix all auto-fixable problems".to_string(),
        kind: Some(CodeActionKind::from("source.fixAll.ruff")),
        data: Some(serde_json::json!("not-a-uri")),
        ..Default::default()
    };

    assert_code_action_resolve_unchanged(&mut server, &action);

    Ok(())
}

/// A module whose imports are both out of order and partly unused, and whose body needs
/// reformatting — so an action that does only part of the job is visibly distinguishable.
const NEEDS_EVERYTHING: &str =
    "import sys\nimport os\nimport abc\n\nx = {  'a' : 1 }\nprint(sys.argv, abc.ABC)\n";

/// The two save-time source actions are not offered to a request that just asks for everything.
///
/// The lightbulb menu already has *Organize imports* and *Fix all*; a near-identically named entry
/// beside each would be a puzzle rather than a choice, so they are answered only when named.
#[test]
fn save_time_actions_are_not_offered_unasked() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    server.open_text_document("test.py", NEEDS_EVERYTHING, 1);

    let actions = server
        .code_action_request("test.py", vec![])
        .expect("Expected Some response");

    let kinds: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            CodeActionResponse::CodeAction(action) => action.kind.as_ref(),
            CodeActionResponse::Command(_) => None,
        })
        .map(CodeActionKind::as_str)
        .collect();

    assert!(
        !kinds.contains(&"source.optimizeImports.ruff")
            && !kinds.contains(&"source.formatAndOrganizeImports.ruff")
            && !kinds.contains(&"source.formatAndOptimizeImports.ruff"),
        "Unasked-for source actions leaked into the menu: {kinds:?}"
    );

    Ok(())
}

/// Asked for by name, `optimizeImports` resolves to an edit that both sorts and drops the unused
/// import — the half `source.organizeImports` leaves behind.
#[test]
fn optimize_imports_is_answered_when_named() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    server.open_text_document("test.py", NEEDS_EVERYTHING, 1);

    let actions = server
        .code_action_request_only(
            "test.py",
            vec![CodeActionKind::new("source.optimizeImports.ruff")],
        )
        .expect("Expected Some response");

    assert_json_snapshot!(actions);

    Ok(())
}

/// The *Reformat Code* composite: sorts and formats, and leaves the unused `import os` alone.
///
/// This is the whole difference from `formatAndOptimizeImports`, and it is the point of having
/// both — laying a file out is not licence to delete anything from it.
#[test]
fn format_and_organize_imports_is_answered_when_named() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    server.open_text_document("test.py", NEEDS_EVERYTHING, 1);

    let actions = server
        .code_action_request_only(
            "test.py",
            vec![CodeActionKind::new("source.formatAndOrganizeImports.ruff")],
        )
        .expect("Expected Some response");

    assert_json_snapshot!(actions);

    Ok(())
}

/// The deferred round trip for the *Reformat Code* composite, which is the path the plugin takes.
#[test]
fn format_and_organize_imports_resolves_deferred() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_code_action_edit_resolution(true)
        .build();

    server.open_text_document("test.py", NEEDS_EVERYTHING, 1);

    let actions = server
        .code_action_request_only(
            "test.py",
            vec![CodeActionKind::new("source.formatAndOrganizeImports.ruff")],
        )
        .expect("Expected Some response");

    let [CodeActionResponse::CodeAction(action)] = actions.as_slice() else {
        panic!("Expected exactly one code action, got {actions:?}");
    };
    assert!(
        action.edit.is_none(),
        "A deferred action should carry no edit until it is resolved"
    );

    let request_id = server.send_request::<CodeActionResolveRequest>(action.clone());
    let resolved = server.await_response::<CodeActionResolveRequest>(&request_id);

    assert_json_snapshot!(resolved.edit);

    Ok(())
}

/// The composite resolves to one edit covering both the import pass and the formatter.
#[test]
fn format_and_optimize_imports_is_answered_when_named() -> Result<()> {
    let mut server = TestServerBuilder::new()?.with_workspace(".")?.build();

    server.open_text_document("test.py", NEEDS_EVERYTHING, 1);

    let actions = server
        .code_action_request_only(
            "test.py",
            vec![CodeActionKind::new("source.formatAndOptimizeImports.ruff")],
        )
        .expect("Expected Some response");

    assert_json_snapshot!(actions);

    Ok(())
}

/// The path a real editor takes: the action comes back without an edit, and the edit arrives from a
/// follow-up `codeAction/resolve`. This is what the PyCharm plugin uses, so the composite has to
/// survive the round trip rather than only working when the edit is inlined.
#[test]
fn format_and_optimize_imports_resolves_deferred() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(".")?
        .enable_code_action_edit_resolution(true)
        .build();

    server.open_text_document("test.py", NEEDS_EVERYTHING, 1);

    let actions = server
        .code_action_request_only(
            "test.py",
            vec![CodeActionKind::new("source.formatAndOptimizeImports.ruff")],
        )
        .expect("Expected Some response");

    let [CodeActionResponse::CodeAction(action)] = actions.as_slice() else {
        panic!("Expected exactly one code action, got {actions:?}");
    };
    assert!(
        action.edit.is_none(),
        "A deferred action should carry no edit until it is resolved"
    );

    let request_id = server.send_request::<CodeActionResolveRequest>(action.clone());
    let resolved = server.await_response::<CodeActionResolveRequest>(&request_id);

    assert_json_snapshot!(resolved.edit);

    Ok(())
}
