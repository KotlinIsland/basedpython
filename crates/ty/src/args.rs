use std::path::PathBuf;

use crate::logging::Verbosity;
use crate::python_version::PythonVersion;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::error::ErrorKind;
use clap::{ArgAction, ArgMatches, Error, Parser};
use ruff_db::system::SystemPathBuf;
use ruff_ranged_value::{RangedValue, ValueSource};
use ty_combine::Combine;
use ty_project::metadata::options::{EnvironmentOptions, Options, SrcOptions, TerminalOptions};
use ty_project::metadata::value::{RelativeGlobPattern, RelativePathBuf};
use ty_python_semantic::lint;
use ty_static::EnvVars;

// Configures Clap v3-style help menu colors
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

#[derive(Debug, Parser)]
#[command(
    author,
    name = "by",
    about = "an extremely fast Python type checker, with basedpython support"
)]
#[command(long_version = crate::version::version())]
#[command(styles = STYLES)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[expect(clippy::large_enum_variant)]
#[derive(Debug, clap::Subcommand)]
pub(crate) enum Command {
    /// Check a project for type errors.
    Check(CheckCommand),

    /// Start the language server
    Server,

    /// Display ty's version
    Version {
        #[arg(
            long,
            value_enum,
            default_value = "text",
            help = "The format in which to display the version information"
        )]
        output_format: HelpFormat,
    },

    /// Generate shell completion
    #[clap(hide = true)]
    GenerateShellCompletion { shell: clap_complete_command::Shell },

    /// Explain rules and other parts of ty
    Explain {
        #[command(subcommand)]
        command: ExplainCommand,
    },

    // ── basedpython commands ─────────────────────────────────────────────────
    /// Transpile and run a module with `python -m <module>`.
    Run {
        /// module to run (e.g. `by run main` looks for main.by)
        /// [default: the `run.main` entry point configured for the project]
        module: Option<String>,
        /// arguments forwarded to the program, as `sys.argv[1:]`
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
        /// minimum Python version the output must run on
        /// [default: the version of the interpreter that will run it]
        #[arg(long, value_name = "VERSION")]
        min_version: Option<String>,
        /// The interpreter to run on, or the environment holding it.
        ///
        /// Defaults to the project environment — the same one `by check`
        /// resolves imports against — then `$PYTHON`, then `python3` on `PATH`.
        #[arg(long, value_name = "PATH", alias = "venv")]
        python: Option<PathBuf>,
        #[command(flatten)]
        lowering: LoweringArgs,
        /// Compile every imported module to a native extension first.
        ///
        /// A function the compiler declines still runs — from the interpreted
        /// source embedded in the extension — so this changes speed, not
        /// behaviour. Needs a C toolchain and python development headers.
        ///
        /// The entry module itself stays interpreted: running something as
        /// `__main__` needs a code object, and an extension module has none.
        #[arg(long)]
        compiled: bool,
    },

    /// Start a new project.
    ///
    /// Writes a `pyproject.toml` that names the basedpython build backend, a
    /// `src` layout, and a python version the checker, the transpiler and the
    /// interpreter all agree on — so the project is installable, runnable and
    /// publishable from the moment it exists.
    Init {
        /// Where to create the project [default: the current directory]
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
        /// The project's name [default: the directory's name]
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Create a library: no entry point, the same packaging.
        #[arg(long, conflicts_with = "app")]
        lib: bool,
        /// Create an application, with an entry point `by run` uses. The default.
        #[arg(long)]
        app: bool,
        /// The python version to target
        /// [default: the version of the project environment's interpreter]
        #[arg(long, value_name = "VERSION")]
        python_version: Option<String>,
    },

    /// Build the project as python.
    ///
    /// The output is the whole project, not only the transpiled half: every `.by`
    /// file becomes a `.py`, and every other file — a hand-written `.py`, a
    /// `py.typed`, a template, a data file — is carried over to the same place.
    /// What the previous build wrote and this one did not is deleted.
    Build {
        /// minimum Python version the output must run on
        /// [default: the project's configured python version]
        #[arg(long, value_name = "VERSION")]
        min_version: Option<String>,
        /// Build one publishable wheel per python version, and a source
        /// distribution, into `dist/`.
        ///
        /// Each wheel is lowered to the version it is tagged for, so an
        /// installer hands every interpreter the best wheel it can use rather
        /// than one lowered to the oldest python the project supports. Needs
        /// `uv`, which does the packaging.
        #[arg(long, conflicts_with_all = ["min_version", "print_manifest"])]
        wheels: bool,
        /// Where to write the output [default: `out`, or `dist` with `--wheels`]
        #[arg(short = 'o', long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Report what the build read and produced, as `<kind> <value>` lines.
        ///
        /// `input <path>` for every file the project is made of — what a source
        /// distribution has to carry to rebuild into the same thing — and
        /// `package <name>` for every top-level package that came out.
        #[arg(long)]
        print_manifest: bool,
        #[command(flatten)]
        lowering: LoweringArgs,
    },

    /// Compile .by and .py files to native CPython extension modules.
    ///
    /// A function the compiler cannot lower natively is left to its interpreted
    /// definition rather than failing the build, so compilation is always
    /// partial-credit. `--verbose` reports each one and why.
    Compile {
        /// Files to compile. Defaults to every `.by` and `.py` file under the
        /// project root.
        #[arg(value_name = "FILE")]
        files: Vec<PathBuf>,
        /// Where to write the generated C and the extension modules.
        #[arg(short = 'o', long, value_name = "DIR", default_value = "out")]
        output: PathBuf,
        /// Report every function that was not lowered natively, with the reason.
        #[arg(long)]
        verbose: bool,
        /// Emit the generated C without invoking the C compiler.
        #[arg(long)]
        emit_c_only: bool,
        /// Fail the build when a function cannot be compiled because a type is
        /// gradual, instead of leaving it to its interpreted definition.
        ///
        /// A contract about predictability rather than a speed switch: a
        /// gradual type is the commonest reason a function silently stays
        /// interpreted.
        #[arg(long)]
        no_any: bool,
        /// Fail the build if *any* function is left to its interpreted
        /// definition, whatever the reason.
        ///
        /// Stricter than `--no-any`, and a different question: `--no-any` asks
        /// whether the module is fully typed, this asks whether it compiles
        /// entirely.
        #[arg(long)]
        require_native: bool,
        /// Write a `<module>.annotated` report next to the generated C: each
        /// function's BIR, whether it is infallible, and for every function left
        /// interpreted, the reason why.
        #[arg(long)]
        annotate: bool,
        /// The lowering options a declined function's interpreted definition is
        /// transpiled with.
        ///
        /// A declined function *runs* from that source, so these have to be the
        /// same ones a `transpile` of the module would use or the two halves
        /// disagree about what they check. `.py` sources are their own fallback
        /// and are unaffected.
        #[command(flatten)]
        lowering: LoweringArgs,
    },

    /// Generate an api lockfile (`api.lock`) summarising the public type-level
    /// surface of the project.
    ///
    /// The file is one record per public symbol in a terse, line-oriented
    /// format. It is not meant to be parsed back into types — the goal is that
    /// any meaningful change to the public api shows up as a diff.
    GenerateApiFile {
        /// Where to write the lockfile. Defaults to `api.lock` in the project root.
        #[arg(short = 'o', long, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Write the lockfile to stdout instead of a file.
        #[arg(long, conflicts_with = "output")]
        stdout: bool,
        /// Run the command within the given project directory.
        #[arg(long, value_name = "PROJECT")]
        project: Option<SystemPathBuf>,
        /// Path to your project's Python environment or interpreter.
        #[arg(long, value_name = "PATH", alias = "venv")]
        python: Option<SystemPathBuf>,
        /// Python version to assume when resolving types.
        #[arg(long, value_name = "VERSION", alias = "target-version", value_enum)]
        python_version: Option<PythonVersion>,
    },

    /// Transpile a file to stdout, or a whole directory in place (reads stdin if no path given).
    Transpile {
        /// file to transpile to stdout, or a directory to transpile in place
        /// (every `.by` → `.py`, or with `--reverse` every `.py` → `.by`)
        file: Option<PathBuf>,
        /// convert Python source into basedpython idioms (instead of the default by → py direction)
        #[arg(long)]
        reverse: bool,
        /// minimum Python version the output must run on
        /// [default: the project's configured python version]
        #[arg(long, value_name = "VERSION")]
        min_version: Option<String>,
        #[command(flatten)]
        lowering: LoweringArgs,
    },
}

/// How the transpiler lowers a program, shared by `run`, `build` and
/// `transpile`. Every option here changes the emitted python, never the
/// checker's verdict.
#[derive(Clone, Debug, Default, Parser)]
pub(crate) struct LoweringArgs {
    /// which runtime type-soundness checks to insert: `default`, `all`
    /// (adds the opt-in `parameters` entry checks), `none`, or a
    /// comma-separated subset of `generic-calls`, `projections`,
    /// `iterations`, `assignments`, `returns`, `arguments`, `parameters`
    #[arg(long, value_name = "SPEC", default_value = "default")]
    pub(crate) soundness: String,
    /// wrap every function with a `raises` clause in a runtime guard that
    /// fails when it raises something the clause does not include
    #[arg(long)]
    pub(crate) runtime_raises_checks: bool,
    /// leave a closure made inside a loop sharing the loop's one binding,
    /// as python does, instead of binding the values of the iteration it
    /// was made in
    #[arg(long)]
    pub(crate) no_unique_loop_bindings: bool,
}

#[derive(Debug, Parser)]
#[expect(clippy::struct_excessive_bools)]
pub(crate) struct CheckCommand {
    /// List of files or directories to check.
    #[clap(
        help = "List of files or directories to check [default: the project root]",
        value_name = "PATH"
    )]
    pub paths: Vec<SystemPathBuf>,

    /// Apply fixes to resolve errors.
    #[arg(long)]
    pub(crate) fix: bool,

    /// Adds `ty: ignore` comments to suppress all rule diagnostics.
    #[arg(long, conflicts_with("fix"))]
    pub(crate) add_ignore: bool,

    /// Run the command within the given project directory.
    ///
    /// All `pyproject.toml` files will be discovered by walking up the directory tree from the given project directory,
    /// as will the project's virtual environment (`.venv`) unless the `venv-path` option is set.
    ///
    /// Other command-line arguments (such as relative paths) will be resolved relative to the current working directory.
    #[arg(long, value_name = "PROJECT")]
    pub(crate) project: Option<SystemPathBuf>,

    /// Path to your project's Python environment or interpreter.
    ///
    /// ty uses your Python environment to resolve third-party imports in your code.
    ///
    /// This can be a path to:
    ///
    /// - A Python interpreter, e.g. `.venv/bin/python3`
    /// - A virtual environment directory, e.g. `.venv`
    /// - A system Python [`sys.prefix`] directory, e.g. `/usr`
    ///
    /// If you're using a project management tool such as uv or you have an activated Conda or virtual
    /// environment, you should not generally need to specify this option.
    ///
    /// [`sys.prefix`]: https://docs.python.org/3/library/sys.html#sys.prefix
    #[arg(long, value_name = "PATH", alias = "venv")]
    python: Option<SystemPathBuf>,

    /// Custom directory to use for stdlib typeshed stubs.
    #[arg(long, value_name = "PATH", alias = "custom-typeshed-dir")]
    typeshed: Option<SystemPathBuf>,

    /// Additional path to use as a module-resolution source (can be passed multiple times).
    ///
    /// This is an advanced option that should usually only be used for first-party or third-party
    /// modules that are not installed into your Python environment in a conventional way.
    /// Use `--python` to point ty to your Python environment if it is in an unusual location.
    #[arg(long, value_name = "PATH")]
    extra_search_path: Option<Vec<SystemPathBuf>>,

    /// Python version to assume when resolving types.
    ///
    /// The Python version affects allowed syntax, type definitions of the standard library, and
    /// type definitions of first- and third-party modules that are conditional on the Python version.
    ///
    /// If a version is not specified on the command line or in a configuration file,
    /// ty will try the following techniques in order of preference to determine a value:
    /// 1. Check for the `project.requires-python` setting in a `pyproject.toml` file
    ///    and use the minimum version from the specified range
    /// 2. Check for an activated or configured Python environment
    ///    and attempt to infer the Python version of that environment
    /// 3. Fall back to the latest stable Python version supported by ty (see `ty check --help` output)
    #[arg(long, value_name = "VERSION", alias = "target-version", value_enum)]
    python_version: Option<PythonVersion>,

    /// Target platform to assume when resolving types.
    ///
    /// This is used to specialize the type of `sys.platform` and will affect the visibility
    /// of platform-specific functions and attributes. If the value is set to `all`, no
    /// assumptions are made about the target platform. If unspecified, the current system's
    /// platform will be used.
    #[arg(long, value_name = "PLATFORM", alias = "platform")]
    python_platform: Option<String>,

    /// The defaults that rules and analysis settings start from.
    ///
    /// `strict` enables every diagnostic and every analysis option that buys soundness.
    /// `ty-compatible` uses ty's own defaults instead, leaving basedpython's diagnostics and
    /// analysis options off, so that a project reports what ty itself would report.
    #[arg(long, value_name = "PRESET", value_enum)]
    pub(crate) type_checking_preset: Option<TypeCheckingPreset>,

    #[clap(flatten)]
    pub(crate) verbosity: Verbosity,

    #[clap(flatten)]
    rules: RulesArg,

    #[clap(flatten)]
    config: ConfigsArg,

    /// The path to a `basedpython.toml` or `ty.toml` file to use for configuration.
    ///
    /// While ty configuration can be included in a `pyproject.toml` file, it is not allowed in this context.
    #[arg(long, env = EnvVars::TY_CONFIG_FILE, value_name = "PATH")]
    pub(crate) config_file: Option<SystemPathBuf>,

    /// The format to use for printing diagnostic messages.
    #[arg(long, env = EnvVars::TY_OUTPUT_FORMAT)]
    output_format: Option<OutputFormat>,

    /// Use exit code 1 if there are any warning-level diagnostics.
    ///
    /// Cannot be used in combination with `--exit-zero` or `--exit-zero-on-warning`.
    #[arg(long, conflicts_with = "exit_zero", default_missing_value = "true", num_args=0..1)]
    error_on_warning: Option<bool>,

    /// Always use exit code 0, even when there are error-level diagnostics.
    ///
    /// Cannot be used in combination with `--error-on-warning`.
    #[arg(long)]
    pub(crate) exit_zero: bool,

    /// Use exit code 0 if there are no error-level diagnostics.
    ///
    /// Cannot be used in combination with `--error-on-warning`.
    #[arg(long, conflicts_with = "error_on_warning")]
    exit_zero_on_warning: bool,

    /// Watch files for changes and recheck files related to the changed files.
    #[arg(long, short = 'W')]
    pub(crate) watch: bool,

    /// Respect file exclusions via `.gitignore` and other standard ignore files.
    /// Use `--no-respect-ignore-files` to disable.
    #[arg(
        long,
        overrides_with("no_respect_ignore_files"),
        help_heading = "File selection",
        default_missing_value = "true",
        num_args = 0..1
    )]
    respect_ignore_files: Option<bool>,
    #[clap(long, overrides_with("respect_ignore_files"), hide = true)]
    no_respect_ignore_files: bool,

    /// Enforce exclusions, even for paths passed to ty directly on the command-line.
    /// Use `--no-force-exclude` to disable.
    #[arg(
        long,
        overrides_with("no_force_exclude"),
        help_heading = "File selection"
    )]
    force_exclude: bool,
    #[clap(long, overrides_with("force_exclude"), hide = true)]
    no_force_exclude: bool,

    /// Exclude files containing PEP 723 inline script metadata unless passed explicitly.
    /// Use `--include-scripts` to disable.
    #[arg(
        long,
        overrides_with("include_scripts"),
        help_heading = "File selection",
        default_missing_value = "true",
        num_args = 0..1
    )]
    exclude_scripts: Option<bool>,
    #[clap(long, overrides_with("exclude_scripts"), hide = true)]
    include_scripts: bool,

    /// Glob patterns for files to exclude from type checking.
    ///
    /// Uses gitignore-style syntax to exclude files and directories from type checking.
    /// Supports patterns like `tests/`, `*.tmp`, `**/__pycache__/**`.
    #[arg(long, help_heading = "File selection")]
    exclude: Option<Vec<String>>,

    /// Control when colored output is used.
    #[arg(
        long,
        value_name = "WHEN",
        help_heading = "Global options",
        display_order = 1000
    )]
    pub(crate) color: Option<TerminalColor>,

    /// Hide all progress outputs.
    ///
    /// For example, spinners or progress bars.
    #[arg(global = true, long, value_parser = clap::builder::BoolishValueParser::new(), help_heading = "Global options")]
    pub no_progress: bool,
}

impl CheckCommand {
    pub(crate) fn force_exclude(&self) -> bool {
        resolve_bool_arg(self.force_exclude, self.no_force_exclude).unwrap_or_default()
    }

    pub(crate) fn into_options(self) -> Options {
        let rules = if self.rules.is_empty() {
            None
        } else {
            Some(
                self.rules
                    .into_iter()
                    .map(|(rule, level)| (RangedValue::cli(rule), RangedValue::cli(level)))
                    .collect(),
            )
        };

        // --no-respect-gitignore defaults to false and is set true by CLI flag. If passed, override config file
        // Otherwise, only pass this through if explicitly set (don't default to anything here to
        // make sure that doesn't take precedence over an explicitly-set config file value)
        let respect_ignore_files = self
            .no_respect_ignore_files
            .then_some(false)
            .or(self.respect_ignore_files);
        let exclude_scripts = self
            .include_scripts
            .then_some(false)
            .or(self.exclude_scripts);
        let error_on_warning = self
            .exit_zero_on_warning
            .then_some(false)
            .or(self.error_on_warning);
        let options = Options {
            type_checking_preset: self
                .type_checking_preset
                .map(|preset| RangedValue::cli(preset.into())),
            environment: Some(EnvironmentOptions {
                python_version: self.python_version.map(Into::into).map(RangedValue::cli),
                python_platform: self
                    .python_platform
                    .map(|platform| RangedValue::cli(platform.into())),
                python: self.python.map(RelativePathBuf::cli),
                typeshed: self.typeshed.map(RelativePathBuf::cli),
                extra_paths: self.extra_search_path.map(|extra_search_paths| {
                    extra_search_paths
                        .into_iter()
                        .map(RelativePathBuf::cli)
                        .collect()
                }),
                ..EnvironmentOptions::default()
            }),
            terminal: Some(TerminalOptions {
                output_format: self
                    .output_format
                    .map(|output_format| RangedValue::cli(output_format.into())),
                error_on_warning,
            }),
            src: Some(SrcOptions {
                respect_ignore_files,
                exclude_scripts,
                exclude: self.exclude.map(|excludes| {
                    RangedValue::cli(excludes.iter().map(RelativeGlobPattern::cli).collect())
                }),
                ..SrcOptions::default()
            }),
            rules,
            ..Options::default()
        };
        // Merge with options passed in via --config
        options.combine(self.config.into_options().unwrap_or_default())
    }
}

/// A list of rules to enable or disable with a given severity.
///
/// This type is used to parse the `--error`, `--warn`, and `--ignore` arguments
/// while preserving the order in which they were specified (arguments last override previous severities).
#[derive(Debug)]
pub(crate) struct RulesArg(Vec<(String, lint::Level)>);

impl RulesArg {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn into_iter(self) -> impl Iterator<Item = (String, lint::Level)> {
        self.0.into_iter()
    }
}

impl clap::FromArgMatches for RulesArg {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        let mut rules = Vec::new();

        for (level, arg_id) in [
            (lint::Level::Ignore, "ignore"),
            (lint::Level::Warn, "warn"),
            (lint::Level::Error, "error"),
        ] {
            let indices = matches.indices_of(arg_id).into_iter().flatten();
            let levels = matches.get_many::<String>(arg_id).into_iter().flatten();
            rules.extend(
                indices
                    .zip(levels)
                    .map(|(index, rule)| (index, rule, level)),
            );
        }

        // Sort by their index so that values specified later override earlier ones.
        rules.sort_by_key(|(index, _, _)| *index);

        Ok(Self(
            rules
                .into_iter()
                .map(|(_, rule, level)| (rule.to_owned(), level))
                .collect(),
        ))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        self.0 = Self::from_arg_matches(matches)?.0;
        Ok(())
    }
}

impl clap::Args for RulesArg {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        const HELP_HEADING: &str = "Enabling / disabling rules";

        cmd.arg(
            clap::Arg::new("error")
                .long("error")
                .action(ArgAction::Append)
                .help(
                    "Treat the given rule as having severity 'error'. \
                    Can be specified multiple times. \
                    Use 'all' to apply to all rules.",
                )
                .value_name("RULE")
                .help_heading(HELP_HEADING),
        )
        .arg(
            clap::Arg::new("warn")
                .long("warn")
                .action(ArgAction::Append)
                .help(
                    "Treat the given rule as having severity 'warn'. \
                    Can be specified multiple times. \
                    Use 'all' to apply to all rules.",
                )
                .value_name("RULE")
                .help_heading(HELP_HEADING),
        )
        .arg(
            clap::Arg::new("ignore")
                .long("ignore")
                .action(ArgAction::Append)
                .help(
                    "Disables the rule. \
                    Can be specified multiple times. \
                    Use 'all' to apply to all rules.",
                )
                .value_name("RULE")
                .help_heading(HELP_HEADING),
        )
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Self::augment_args(cmd)
    }
}

/// The defaults that `rules` and `analysis` start from.
#[derive(Copy, Clone, Hash, Debug, PartialEq, Eq, PartialOrd, Ord, Default, clap::ValueEnum)]
pub enum TypeCheckingPreset {
    /// Enable every diagnostic, and every analysis option that buys soundness (default).
    #[default]
    #[value(name = "strict")]
    Strict,

    /// Use ty's own defaults, leaving basedpython's diagnostics and analysis options off.
    #[value(name = "ty-compatible")]
    TyCompatible,
}

impl From<TypeCheckingPreset> for ty_python_semantic::TypeCheckingPreset {
    fn from(preset: TypeCheckingPreset) -> ty_python_semantic::TypeCheckingPreset {
        match preset {
            TypeCheckingPreset::Strict => Self::Strict,
            TypeCheckingPreset::TyCompatible => Self::TyCompatible,
        }
    }
}

/// The diagnostic output format.
#[derive(Copy, Clone, Hash, Debug, PartialEq, Eq, PartialOrd, Ord, Default, clap::ValueEnum)]
pub enum OutputFormat {
    /// Print diagnostics verbosely, with context and helpful hints (default).
    ///
    /// Diagnostic messages may include additional context and
    /// annotations on the input to help understand the message.
    #[default]
    #[value(name = "full")]
    Full,
    /// Print diagnostics concisely, one per line.
    ///
    /// This will guarantee that each diagnostic is printed on
    /// a single line. Only the most important or primary aspects
    /// of the diagnostic are included. Contextual information is
    /// dropped.
    #[value(name = "concise")]
    Concise,
    /// Print diagnostics in the JSON format expected by GitLab Code Quality reports.
    #[value(name = "gitlab")]
    Gitlab,
    /// Print diagnostics in the format used by GitHub Actions workflow error annotations.
    #[value(name = "github")]
    Github,
    /// Print diagnostics as a JUnit-style XML report.
    #[value(name = "junit")]
    Junit,
}

impl From<OutputFormat> for ty_project::metadata::options::OutputFormat {
    fn from(format: OutputFormat) -> ty_project::metadata::options::OutputFormat {
        match format {
            OutputFormat::Full => Self::Full,
            OutputFormat::Concise => Self::Concise,
            OutputFormat::Gitlab => Self::Gitlab,
            OutputFormat::Github => Self::Github,
            OutputFormat::Junit => Self::Junit,
        }
    }
}

/// Control when colored output is used.
#[derive(Copy, Clone, Hash, Debug, PartialEq, Eq, PartialOrd, Ord, Default, clap::ValueEnum)]
pub(crate) enum TerminalColor {
    /// Display colors if the output goes to an interactive terminal.
    #[default]
    Auto,

    /// Always display colors.
    Always,

    /// Never display colors.
    Never,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum HelpFormat {
    Text,
    Json,
}

/// A TOML `<KEY> = <VALUE>` pair
/// (such as you might find in a `ty.toml` configuration file)
/// overriding a specific configuration option.
///
/// Overrides of individual settings using this option always take precedence
/// over all configuration files.
#[derive(Debug, Clone)]
pub(crate) struct ConfigsArg(Option<Options>);

impl clap::FromArgMatches for ConfigsArg {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        let combined = matches
            .get_many::<String>("config")
            .into_iter()
            .flatten()
            .map(|s| {
                Options::from_toml_str(s, ValueSource::Cli)
                    .map_err(|err| Error::raw(ErrorKind::InvalidValue, err.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .reduce(|acc, item| item.combine(acc));
        Ok(Self(combined))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        self.0 = Self::from_arg_matches(matches)?.0;
        Ok(())
    }
}

impl clap::Args for ConfigsArg {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        cmd.arg(
            clap::Arg::new("config")
                .short('c')
                .long("config")
                .value_name("CONFIG_OPTION")
                .help("A TOML `<KEY> = <VALUE>` pair overriding a specific configuration option.")
                .long_help(
                    "
A TOML `<KEY> = <VALUE>` pair (such as you might find in a `ty.toml` configuration file)
overriding a specific configuration option.

Overrides of individual settings using this option always take precedence
over all configuration files.",
                )
                .action(ArgAction::Append),
        )
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Self::augment_args(cmd)
    }
}

impl ConfigsArg {
    fn into_options(self) -> Option<Options> {
        self.0
    }
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum ExplainCommand {
    /// Explain a rule (or all rules).
    Rule {
        /// Rule to explain
        ///
        /// Defaults to all rules if omitted.
        #[arg(hide_possible_values = true)]
        rule: Option<String>,

        /// Output format
        #[arg(long, value_enum, default_value = "text")]
        output_format: HelpFormat,
    },
}

fn resolve_bool_arg(yes: bool, no: bool) -> Option<bool> {
    match (yes, no) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
        (..) => unreachable!("Clap should make this impossible"),
    }
}
