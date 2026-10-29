//! `by`'s command line as data, described from the parser `by` parses its arguments with
//!
//! the language server answers `by/commandLine` with it, so that an editor completing `by` in a
//! terminal has `by`'s commands, options and the kind of each value without a copy of its own.
//! nothing here lists a command or an option: every one comes from [`Cli`], and the kind of a
//! value from the type it parses to — a module name is a [`ModuleName`], a rule a [`RuleName`],
//! a path a `PathBuf` — or from the values or the hint the parser gives it

use std::any::TypeId;
use std::path::PathBuf;

use clap::builder::PossibleValue;
use clap::{Arg, ArgAction, Command, CommandFactory, ValueHint};
use ruff_db::system::SystemPathBuf;
use ty_server::{
    Choice, CommandLine, CommandOption, CommandValue, PathHint, Positional, ValueKind,
};

use crate::args::{Cli, ModuleName, RuleName};

/// `by`'s command line, as an editor completes it: every command and argument a user can see in
/// `--help`, `--help` itself among them
pub(crate) fn command_line() -> CommandLine {
    let mut cli = Cli::command();
    // the arguments clap adds itself, `--help` and `--version`, exist once it is built
    cli.build();
    describe(&cli)
}

fn describe(command: &Command) -> CommandLine {
    let arguments: Vec<&Arg> = command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .collect();
    CommandLine {
        name: command.get_name().to_owned(),
        aliases: command.get_visible_aliases().map(str::to_owned).collect(),
        help: command.get_about().map(ToString::to_string),
        options: arguments
            .iter()
            .filter(|argument| !argument.is_positional())
            .map(|argument| option(argument))
            .collect(),
        positionals: arguments
            .iter()
            .filter(|argument| argument.is_positional())
            .map(|argument| positional(argument))
            .collect(),
        subcommands: command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(describe)
            .collect(),
    }
}

fn option(argument: &Arg) -> CommandOption {
    CommandOption {
        long: argument.get_long().map(str::to_owned),
        short: argument.get_short(),
        aliases: argument
            .get_visible_aliases()
            .unwrap_or_default()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        help: argument.get_help().map(ToString::to_string),
        value: argument
            .get_action()
            .takes_values()
            .then(|| value(argument)),
        repeatable: matches!(argument.get_action(), ArgAction::Append | ArgAction::Count),
    }
}

fn positional(argument: &Arg) -> Positional {
    Positional {
        help: argument.get_help().map(ToString::to_string),
        value: value(argument),
        required: argument.is_required_set(),
        repeatable: argument
            .get_num_args()
            .is_some_and(|count| count.max_values() > 1),
        trailing: argument.is_trailing_var_arg_set(),
    }
}

fn value(argument: &Arg) -> CommandValue {
    let name = argument
        .get_value_names()
        .and_then(|names| names.first())
        .map_or_else(
            || argument.get_id().as_str().to_uppercase(),
            ToString::to_string,
        );
    CommandValue {
        name,
        kind: kind(argument),
    }
}

fn kind(argument: &Arg) -> ValueKind {
    let parsed = argument.get_value_parser().type_id();
    if parsed == TypeId::of::<ModuleName>() {
        return ValueKind::Module;
    }
    if parsed == TypeId::of::<RuleName>() {
        return ValueKind::Rule;
    }
    let choices: Vec<Choice> = argument
        .get_possible_values()
        .iter()
        .filter(|choice| !choice.is_hide_set())
        .map(choice)
        .collect();
    if !choices.is_empty() {
        return ValueKind::Choice { choices };
    }
    match argument.get_value_hint() {
        ValueHint::FilePath => ValueKind::Path {
            hint: PathHint::File,
        },
        ValueHint::DirPath => ValueKind::Path {
            hint: PathHint::Directory,
        },
        ValueHint::ExecutablePath => ValueKind::Path {
            hint: PathHint::Executable,
        },
        ValueHint::AnyPath => ValueKind::Path {
            hint: PathHint::Any,
        },
        // a path the parser gives no hint for is any path, as clap takes a `PathBuf` to be
        _ if parsed == TypeId::of::<PathBuf>() || parsed == TypeId::of::<SystemPathBuf>() => {
            ValueKind::Path {
                hint: PathHint::Any,
            }
        }
        _ => ValueKind::Text,
    }
}

fn choice(value: &PossibleValue) -> Choice {
    Choice {
        name: value.get_name().to_owned(),
        help: value.get_help().map(ToString::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subcommand<'a>(command: &'a CommandLine, name: &str) -> &'a CommandLine {
        command
            .subcommands
            .iter()
            .find(|subcommand| subcommand.name == name)
            .unwrap_or_else(|| panic!("no `{name}` in {:?}", command.name))
    }

    fn long<'a>(command: &'a CommandLine, name: &str) -> &'a CommandOption {
        command
            .options
            .iter()
            .find(|option| option.long.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("`{}` has no `--{name}`", command.name))
    }

    fn value_kind(option: &CommandOption) -> &ValueKind {
        &option.value.as_ref().expect("an option with a value").kind
    }

    /// the module `by run` runs is a module, and what follows it is the program's
    #[test]
    fn run_takes_a_module_then_the_program_s_arguments() {
        let by = command_line();
        let run = subcommand(&by, "run");
        let [module, arguments] = run.positionals.as_slice() else {
            panic!("{:?}", run.positionals);
        };
        assert_eq!(module.value.kind, ValueKind::Module);
        assert!(!module.required && !module.trailing);
        assert_eq!(arguments.value.kind, ValueKind::Text);
        assert!(arguments.trailing && arguments.repeatable);
        assert_eq!(
            value_kind(long(subcommand(&by, "repl"), "module")),
            &ValueKind::Module
        );
    }

    #[test]
    fn a_path_is_a_path() {
        let by = command_line();
        let run = subcommand(&by, "run");
        let python = long(run, "python");
        assert_eq!(
            value_kind(python),
            &ValueKind::Path {
                hint: PathHint::Any
            }
        );
        assert_eq!(python.aliases, Vec::<String>::new(), "`--venv` is hidden");
        let check = subcommand(&by, "check");
        assert!(matches!(
            check.positionals[0].value.kind,
            ValueKind::Path { .. }
        ));
        assert!(check.positionals[0].repeatable);
    }

    #[test]
    fn a_rule_is_a_rule() {
        let by = command_line();
        let check = subcommand(&by, "check");
        for flag in ["error", "warn", "ignore"] {
            let option = long(check, flag);
            assert_eq!(value_kind(option), &ValueKind::Rule);
            assert!(option.repeatable);
        }
        let explain = subcommand(subcommand(&by, "explain"), "rule");
        assert_eq!(explain.positionals[0].value.kind, ValueKind::Rule);
    }

    /// a value of a fixed set lists the set, and a flag takes no value
    #[test]
    fn a_choice_lists_its_values_and_a_flag_takes_none() {
        let by = command_line();
        let ValueKind::Choice { choices } =
            value_kind(long(subcommand(&by, "version"), "output-format"))
        else {
            panic!("`by version --output-format` is a choice");
        };
        assert!(choices.iter().any(|choice| choice.name == "json"));
        let compiled = long(subcommand(&by, "run"), "compiled");
        assert_eq!(compiled.value, None);
        assert!(!compiled.repeatable);
    }

    /// what `--help` does not show is not offered: the hidden shell-completion command
    #[test]
    fn a_hidden_command_is_left_out() {
        let by = command_line();
        assert!(
            by.subcommands
                .iter()
                .all(|subcommand| subcommand.name != "generate-shell-completion")
        );
        assert!(
            by.options
                .iter()
                .any(|option| option.long.as_deref() == Some("help"))
        );
    }

    /// the description is what the server sends, so it has to be what `serde` writes
    #[test]
    fn the_description_serializes() {
        let json = serde_json::to_value(command_line()).expect("plain data");
        assert_eq!(json["name"], "by");
        assert!(
            json["subcommands"]
                .as_array()
                .is_some_and(|commands| !commands.is_empty())
        );
    }
}
