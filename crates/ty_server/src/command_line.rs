//! `by`'s command line as data: its commands, the options and positional arguments each
//! takes, and what kind of value each of those is
//!
//! the server does not know the command line itself: `by` describes it from the parser it parses
//! its own arguments with, and hands the description to the server it starts. an editor that
//! completes `by` in a terminal asks for it with `by/commandLine` rather than keeping a copy of
//! its own

use serde::{Deserialize, Serialize};

/// a command: `by` itself, or one of its subcommands at any depth
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandLine {
    /// the name the command is invoked by, `run` for `by run`
    pub name: String,
    /// other names that invoke it
    pub aliases: Vec<String>,
    /// the one-line description `--help` gives it
    pub help: Option<String>,
    /// the options it takes, in the order `--help` lists them
    pub options: Vec<CommandOption>,
    /// the positional arguments it takes, in order
    pub positionals: Vec<Positional>,
    /// its subcommands
    pub subcommands: Vec<CommandLine>,
}

/// an option: `--python PATH`, or a flag such as `--compiled`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOption {
    /// the long name without its dashes, `python` for `--python`
    pub long: Option<String>,
    /// the short name, `o` for `-o`
    pub short: Option<char>,
    /// other long names it takes, `venv` for `--python`
    pub aliases: Vec<String>,
    pub help: Option<String>,
    /// the value it takes, or `None` for a flag
    pub value: Option<CommandValue>,
    /// whether it may be given more than once
    pub repeatable: bool,
}

/// a positional argument
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Positional {
    pub help: Option<String>,
    pub value: CommandValue,
    pub required: bool,
    /// whether it takes more than one value
    pub repeatable: bool,
    /// whether it takes everything after it on the command line, options included: the
    /// arguments `by run` hands the program it runs, which are the program's to interpret
    pub trailing: bool,
}

/// the value an option or a positional argument takes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandValue {
    /// the placeholder `--help` shows for it, `PATH` in `--python PATH`
    pub name: String,
    #[serde(flatten)]
    pub kind: ValueKind,
}

/// what a value is, which is what completing it takes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ValueKind {
    /// a module of the project, by its dotted name: one `by/runModules` lists
    Module,
    /// a rule, by its name: one `by/rules` lists, or `all`
    Rule,
    /// a path on the file system
    Path { hint: PathHint },
    /// one of a fixed set of values
    Choice { choices: Vec<Choice> },
    /// anything else, which nothing completes
    Text,
}

/// what a path names
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PathHint {
    /// a file or a directory
    Any,
    File,
    Directory,
    /// a program to run
    Executable,
}

/// one of a choice's values
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    pub name: String,
    pub help: Option<String>,
}
