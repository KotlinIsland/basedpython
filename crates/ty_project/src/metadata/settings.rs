use std::sync::Arc;

use ruff_db::files::File;
use ty_combine::Combine;
use ty_python_semantic::AnalysisSettings;
use ty_python_semantic::lint::RuleSelection;

use crate::metadata::options::{FileOptions, InnerOverrideOptions, Options, OutputFormat};
use crate::metadata::script::script_metadata;
use crate::{Db, glob::IncludeExcludeFilter};

/// The resolved [`super::Options`] for the project.
///
/// Unlike [`super::Options`], the struct has default values filled in and
/// uses representations that are optimized for reads (instead of preserving the source representation).
/// It's also not required that this structure precisely resembles the TOML schema, although
/// it's encouraged to use a similar structure.
///
/// It's worth considering to adding a salsa query for specific settings to
/// limit the blast radius when only some settings change. For example,
/// changing the terminal settings shouldn't invalidate any core type-checking queries.
/// This can be achieved by adding a salsa query for the type checking specific settings.
///
/// Settings that are part of [`ty_python_core::program::ProgramSettings`] are not included here.
#[derive(Clone, Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct Settings {
    pub(super) rules: Arc<RuleSelection>,
    pub(super) terminal: TerminalSettings,
    pub(super) src: SrcSettings,
    pub(super) analysis: AnalysisSettings,
    pub(super) editor: EditorSettings,

    /// Settings for configuration overrides that apply to specific file patterns.
    ///
    /// Each override can specify include/exclude patterns and rule configurations
    /// that apply to matching files. Multiple overrides can match the same file,
    /// with later overrides taking precedence.
    pub(super) overrides: Vec<Override>,
}

impl Settings {
    fn rules(&self) -> &RuleSelection {
        &self.rules
    }

    pub fn src(&self) -> &SrcSettings {
        &self.src
    }

    pub(crate) fn to_rules(&self) -> Arc<RuleSelection> {
        self.rules.clone()
    }

    pub fn terminal(&self) -> &TerminalSettings {
        &self.terminal
    }

    fn overrides(&self) -> &[Override] {
        &self.overrides
    }

    pub(crate) fn analysis(&self) -> &AnalysisSettings {
        &self.analysis
    }

    pub fn editor(&self) -> &EditorSettings {
        &self.editor
    }
}

/// The resolved `[tool.ty.editor]` options.
#[derive(Debug, Default, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct EditorSettings {
    /// The modules a name is a common alias of, paired with the alias and sorted by it.
    ///
    /// These are only the aliases the project configured. The ones the editor knows on its own
    /// live with the feature that offers them, in `ty_ide`.
    common_aliases: Box<[(Box<str>, Box<str>)]>,
}

impl EditorSettings {
    pub(super) fn new<'a>(common_aliases: impl Iterator<Item = (&'a str, &'a str)>) -> Self {
        let mut common_aliases: Box<[(Box<str>, Box<str>)]> = common_aliases
            .map(|(alias, module)| (Box::from(alias), Box::from(module)))
            .collect();
        // `common_alias` looks these up by binary search
        common_aliases.sort_by(|(left, _), (right, _)| left.cmp(right));
        Self { common_aliases }
    }

    /// The module the project configured `alias` to name, if it configured one.
    pub fn common_alias(&self, alias: &str) -> Option<&str> {
        self.common_aliases
            .binary_search_by(|(configured, _)| (**configured).cmp(alias))
            .ok()
            .and_then(|found| self.common_aliases.get(found))
            .map(|(_, module)| &**module)
    }

    /// Every alias the project configured, paired with the module it names, in alias order.
    pub fn common_aliases(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.common_aliases
            .iter()
            .map(|(alias, module)| (&**alias, &**module))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct TerminalSettings {
    pub output_format: OutputFormat,
    pub error_on_warning: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            output_format: OutputFormat::default(),
            error_on_warning: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct SrcSettings {
    pub respect_ignore_files: bool,
    pub(crate) exclude_scripts: bool,
    pub(crate) files: IncludeExcludeFilter,
}
impl SrcSettings {
    pub(crate) fn default() -> Self {
        Self {
            respect_ignore_files: true,
            exclude_scripts: false,
            files: IncludeExcludeFilter::default(),
        }
    }
}

/// A single configuration override that applies to files matching specific patterns.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct Override {
    /// File pattern filter to determine which files this override applies to.
    pub(super) files: IncludeExcludeFilter,

    /// The raw options as specified in the configuration (minus `include` and `exclude`.
    /// Necessary to merge multiple overrides if necessary.
    pub(super) options: Arc<InnerOverrideOptions>,

    /// Pre-resolved rule selection for this override alone.
    /// Used for efficient lookup when only this override matches a file.
    pub(super) settings: Arc<OverrideSettings>,
}

impl Override {
    /// Returns whether this override applies to the given file path.
    fn matches_file(&self, path: &ruff_db::system::SystemPath) -> bool {
        use crate::glob::{GlobFilterCheckMode, IncludeResult};

        matches!(
            self.files
                .is_file_included(path, GlobFilterCheckMode::Adhoc),
            IncludeResult::Included { .. }
        )
    }
}

/// Resolves the settings for a given file.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn file_settings(db: &dyn Db, file: File) -> FileSettings {
    let project = db.project();

    // a PEP 723 script's own `[tool.ty]` block is one more configuration layer for
    // that single file — the highest-precedence one — and not a replacement for the
    // project's configuration. a script that says nothing about a rule is held to
    // whatever the project it sits in says about that rule, the same as any other
    // file, so a project can relax a rule for a vendored script or tighten one
    // without every site needing its own suppression comment.
    //
    // an explicit `--config` is the one thing that outranks the block: it is a
    // deliberate instruction from the command line about this very run, so the
    // block is dropped rather than layered.
    //
    // ignore script settings for files that aren't checked as part of the project.
    // check for metadata first so files without metadata don't depend on the
    // low-durability open-file set.
    let script_layer = if let Some(script) = script_metadata(db, file)
        && crate::should_check_file(db, file)
        && project.metadata(db).config_file_override().is_none()
    {
        script
            .options()
            .map(|options| {
                let FileOptions { rules, analysis } = options.file_options();
                Arc::new(InnerOverrideOptions { rules, analysis })
            })
            .filter(|layer| layer.rules.is_some() || layer.analysis.is_some())
    } else {
        None
    };

    let settings = project.settings(db);

    let path = match file.path(db) {
        ruff_db::files::FilePath::System(path) => path,
        ruff_db::files::FilePath::SystemVirtual(_) | ruff_db::files::FilePath::Vendored(_) => {
            // a file with no system path matches no `include`/`exclude` glob, but a
            // script carries its configuration in its own text, so that still applies
            return script_settings(db, script_layer);
        }
    };

    let mut matching_overrides = settings
        .overrides()
        .iter()
        .filter(|over| over.matches_file(path));

    let Some(first) = matching_overrides.next() else {
        // If the file matches no override, it uses the global settings.
        return script_settings(db, script_layer);
    };

    let Some(second) = matching_overrides.next() else {
        tracing::debug!("Applying override for file `{path}`: {}", first.files);
        // If the file matches only one override, return that override's settings.
        return match script_layer {
            Some(layer) => merge_overrides(db, vec![Arc::clone(&first.options)], Some(layer)),
            None => FileSettings::File(Arc::clone(&first.settings)),
        };
    };

    let mut filters = tracing::enabled!(tracing::Level::DEBUG)
        .then(|| format!("({}), ({})", first.files, second.files));

    let mut overrides = vec![Arc::clone(&first.options), Arc::clone(&second.options)];

    for over in matching_overrides {
        use std::fmt::Write;

        if let Some(filters) = &mut filters {
            let _ = write!(filters, ", ({})", over.files);
        }

        overrides.push(Arc::clone(&over.options));
    }

    if let Some(filters) = &filters {
        tracing::debug!("Applying multiple overrides for file `{path}`: {filters}");
    }

    merge_overrides(db, overrides, None)
}

/// The settings for a file that matches no override, which for a PEP 723 script
/// still has to account for the script's own `[tool.ty]` block.
fn script_settings(db: &dyn Db, script: Option<Arc<InnerOverrideOptions>>) -> FileSettings {
    match script {
        Some(script) => merge_overrides(db, Vec::new(), Some(script)),
        None => FileSettings::Global,
    }
}

/// Merges multiple override options, caching the result.
///
/// Overrides often apply to multiple files. This query ensures that we avoid
/// resolving the same override combinations multiple times.
///
/// `script` is a PEP 723 script's own `[tool.ty]` block. It applies to exactly one
/// file, so it does not share the caching benefit the override list has, but it
/// takes part in the same merge because it is just one more layer.
#[salsa::tracked(returns(clone), heap_size=ruff_memory_usage::heap_size)]
fn merge_overrides(
    db: &dyn Db,
    overrides: Vec<Arc<InnerOverrideOptions>>,
    script: Option<Arc<InnerOverrideOptions>>,
) -> FileSettings {
    let mut overrides = overrides.into_iter().rev();
    let mut merged = overrides.next().map_or(
        InnerOverrideOptions {
            rules: None,
            analysis: None,
        },
        |first| (*first).clone(),
    );

    for option in overrides {
        merged.combine_with((*option).clone());
    }

    let metadata = db.project().metadata(db);
    let script = script.map(|script| Options {
        rules: script.rules.clone(),
        analysis: script.analysis.clone(),
        ..Options::default()
    });

    // Merge with the project level options by replaying the individual options
    // in the correct precedence order.
    for options in
        metadata.options_in_precedence_order_with_script(metadata.options(), script.as_ref())
    {
        merged.rules.combine_with(options.rules.clone());
        merged.analysis.combine_with(options.analysis.clone());
    }

    if merged.rules.is_none() && merged.analysis.is_none() {
        return FileSettings::Global;
    }

    let rules = merged.rules.unwrap_or_default();
    let analysis = merged.analysis.unwrap_or_default();

    // It's okay to ignore the errors here because the rules are eagerly validated
    // during `overrides.to_settings()`.
    let rules = rules.to_rule_selection(db, &mut Vec::new());
    let analysis = analysis.to_settings(db, &mut Vec::new());

    FileSettings::File(Arc::new(OverrideSettings { rules, analysis }))
}

/// The resolved settings for a file.
#[derive(Debug, Eq, PartialEq, Clone, get_size2::GetSize)]
pub enum FileSettings {
    /// The file uses the global settings.
    Global,

    /// The file has specific override settings.
    File(Arc<OverrideSettings>),
}

impl FileSettings {
    pub(crate) fn rules<'a>(&'a self, db: &'a dyn Db) -> &'a RuleSelection {
        match self {
            FileSettings::Global => db.project().settings(db).rules(),
            FileSettings::File(override_settings) => &override_settings.rules,
        }
    }

    pub(crate) fn analysis<'a>(&'a self, db: &'a dyn Db) -> &'a AnalysisSettings {
        match self {
            FileSettings::Global => db.project().settings(db).analysis(),
            FileSettings::File(override_settings) => &override_settings.analysis,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, get_size2::GetSize)]
pub struct OverrideSettings {
    pub(super) rules: RuleSelection,
    pub(super) analysis: AnalysisSettings,
}
