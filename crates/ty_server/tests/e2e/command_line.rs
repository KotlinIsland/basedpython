//! `by/commandLine`: `by`'s command line as data, for an editor to complete it in a terminal

use anyhow::Result;
use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_db::system::SystemPath;
use serde_json::{Value, json};
use ty_server::{CommandLine, CommandOption, CommandValue, Positional, ValueKind};

use crate::TestServerBuilder;

enum CommandLineRequest {}

impl Request for CommandLineRequest {
    type Params = Value;
    // read as json rather than as the server's own type, so the shape a client depends on is
    // what is asserted
    type Result = Value;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/commandLine");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the server answers with the command line of the `by` that started it, whatever the project
#[test]
fn the_command_line_is_the_one_the_server_was_started_with() -> Result<()> {
    let command_line = CommandLine {
        name: "by".to_owned(),
        subcommands: vec![CommandLine {
            name: "run".to_owned(),
            help: Some("Transpile and run a module".to_owned()),
            options: vec![CommandOption {
                long: Some("python".to_owned()),
                short: None,
                aliases: vec!["venv".to_owned()],
                help: None,
                value: Some(CommandValue {
                    name: "PATH".to_owned(),
                    kind: ValueKind::Path {
                        hint: ty_server::PathHint::Any,
                    },
                }),
                repeatable: false,
            }],
            positionals: vec![Positional {
                help: None,
                value: CommandValue {
                    name: "MODULE".to_owned(),
                    kind: ValueKind::Module,
                },
                required: false,
                repeatable: false,
                trailing: false,
            }],
            ..CommandLine::default()
        }],
        ..CommandLine::default()
    };
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("project"), None)?
        .with_command_line(command_line)
        .build()
        .wait_until_workspaces_are_initialized();

    for params in [Value::Null, json!({})] {
        let answer = server.send_request_await::<CommandLineRequest>(params);
        assert_eq!(
            answer,
            json!({
                "name": "by",
                "aliases": [],
                "help": null,
                "options": [],
                "positionals": [],
                "subcommands": [{
                    "name": "run",
                    "aliases": [],
                    "help": "Transpile and run a module",
                    "options": [{
                        "long": "python",
                        "short": null,
                        "aliases": ["venv"],
                        "help": null,
                        "value": {"name": "PATH", "kind": "path", "hint": "any"},
                        "repeatable": false
                    }],
                    "positionals": [{
                        "help": null,
                        "value": {"name": "MODULE", "kind": "module"},
                        "required": false,
                        "repeatable": false,
                        "trailing": false
                    }],
                    "subcommands": []
                }]
            })
        );
    }
    Ok(())
}
