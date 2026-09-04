//! what a django template gets wrong
//!
//! a template is compiled and rendered rather than type-checked, so the errors
//! worth reporting are the ones those two steps raise: a tag django cannot parse,
//! a name django cannot reverse, a file django cannot load. every check here is
//! one of those, and each says which of the project indexes it reads.
//!
//! the one rule the whole module is written around is that **a diagnostic must
//! never fire on correct code**. an index that cannot answer authoritatively —
//! because the settings module wasn't found, or because something it named
//! couldn't be worked out — silences every check that reads it, rather than
//! reporting against a partial answer. [`project::settings_are_authoritative`]
//! and [`project::routes_are_authoritative`] are where that is decided.

use compact_str::{CompactString, ToCompactString};
use ruff_db::diagnostic::{Annotation, Diagnostic, DiagnosticId, Span};
use ruff_db::files::File;
use ruff_db::system::SystemPathBuf;
use ruff_diagnostics::{Edit, Fix};
use ruff_source_file::OneIndexed;
use ruff_text_size::{TextRange, TextSize};
use rustc_hash::FxHashSet;
use ty_project::Db;
use ty_python_semantic::UNUSED_IGNORE_COMMENT;
use ty_python_semantic::django_template::{
    INVALID_ROUTE_ARGUMENTS, TEMPLATE_MEMBER_ALTERS_DATA, TEMPLATE_MEMBER_NEEDS_ARGUMENTS,
    UNCLOSED_TEMPLATE_BLOCK, UNKNOWN_TEMPLATE_BLOCK, UNKNOWN_TEMPLATE_FILTER,
    UNKNOWN_TEMPLATE_LIBRARY, UNKNOWN_TEMPLATE_TAG, UNLOADED_TEMPLATE_LIBRARY,
    UNMATCHED_TEMPLATE_CLOSE, UNRESOLVED_ROUTE, UNRESOLVED_STATIC_FILE, UNRESOLVED_TEMPLATE,
};
use ty_python_semantic::lint::{LintId, LintMetadata};
use ty_python_semantic::types::ide_support::{
    TemplateLookup, callable_needs_arguments, template_lookup,
};

use crate::code_action::{FileEdit, QuickFix};

use super::TEMPLATE_DIRECTORY;
use super::completion::{load_edit_for, loaded_libraries};
use super::index::{END_TAG_PREFIX, TemplateIndex};
use super::lexer::{Construct, ConstructKind, Token, TokenKind, string_contents};
use super::project::{self, Parameter, RegistrationKind, UrlName};
use super::resolve;
use super::uses::URL_TAG;
use super::{ancestors, builtins};
use ty_python_semantic::ProgramEnvironment;

/// the tag that names a static file
const STATIC_TAG: &str = "static";

/// what everything a template says is checked against
///
/// the indexes are read once here rather than once per check, since every one of
/// them is a project-wide query.
struct Checker<'a> {
    db: &'a dyn Db,
    file: File,
    index: &'a TemplateIndex,
    source: &'a str,
    /// the libraries this template has, its own `{% load %}`s and the ones the
    /// settings load into every template
    loaded: FxHashSet<CompactString>,
    /// whether the library index answers for the whole project
    ///
    /// a template that loads a library nothing here knows is a template whose
    /// tags and filters cannot be enumerated, whatever the project-wide index
    /// says, so this is per template rather than per project.
    libraries_known: bool,
    /// whether the settings index answers for the whole project
    ///
    /// every check reads something the settings decide, so nothing at all is
    /// decided about this file without them
    settings_known: bool,
    suppressions: Suppressions,
    /// the rules whose check reached a conclusion for this file
    ///
    /// see [`Checker::decides`]
    decided: Vec<LintId>,
    found: Vec<Diagnostic>,
}

/// every rule a template can report
///
/// a blanket `{# ty: ignore #}` covers all of them at once, so it is only ever
/// unused when every one of them was decided.
const TEMPLATE_LINTS: &[&LintMetadata] = &[
    &UNCLOSED_TEMPLATE_BLOCK,
    &UNMATCHED_TEMPLATE_CLOSE,
    &UNKNOWN_TEMPLATE_LIBRARY,
    &UNKNOWN_TEMPLATE_TAG,
    &UNKNOWN_TEMPLATE_FILTER,
    &UNLOADED_TEMPLATE_LIBRARY,
    &UNRESOLVED_TEMPLATE,
    &UNRESOLVED_STATIC_FILE,
    &UNRESOLVED_ROUTE,
    &INVALID_ROUTE_ARGUMENTS,
    &UNKNOWN_TEMPLATE_BLOCK,
    &TEMPLATE_MEMBER_NEEDS_ARGUMENTS,
    &TEMPLATE_MEMBER_ALTERS_DATA,
];

/// everything wrong with `file`, read as a django template
pub(crate) fn diagnostics(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
) -> Vec<Diagnostic> {
    let project = db.project();
    let loaded = loaded_libraries(db, index);

    let settings_known = project::settings_are_authoritative(db, project);
    let libraries_known = settings_known
        && index.loads().iter().all(|load| {
            project::tag_libraries(db, project)
                .iter()
                .any(|library| library.name == load.library)
        });

    let mut checker = Checker {
        db,
        file,
        index,
        source,
        loaded,
        libraries_known,
        settings_known,
        suppressions: Suppressions::of(index, source),
        decided: Vec::new(),
        found: Vec::new(),
    };

    checker.unclosed_blocks();
    checker.unmatched_closes();
    checker.unknown_libraries();
    checker.unknown_names();
    checker.unresolved_templates();
    checker.unresolved_static_files();
    checker.routes();
    checker.unknown_blocks();
    checker.members_needing_arguments();
    // last, since it answers for what every check above did or did not silence
    checker.unused_suppressions();

    checker.found.sort_by_key(|diagnostic| {
        diagnostic
            .primary_span()
            .and_then(|span| span.range())
            .map(TextRange::start)
            .unwrap_or_default()
    });
    checker.found
}

impl Checker<'_> {
    /// report `lint` over `range`, unless it is turned off or suppressed
    ///
    /// the returned diagnostic is the one just recorded, so that a caller with a
    /// fix or a hint to add can go on building it.
    fn report(
        &mut self,
        lint: &'static LintMetadata,
        range: TextRange,
        message: impl std::fmt::Display,
    ) -> Option<&mut Diagnostic> {
        let severity = self
            .db
            .rule_selection(self.file)
            .severity(LintId::of(lint))?;

        let line = line_of(self.source, range.start());
        if self.suppressions.silences(lint, line) {
            return None;
        }

        let mut diagnostic = Diagnostic::new(DiagnosticId::Lint(lint.name()), severity, message);
        diagnostic.annotate(Annotation::primary(Span::from(self.file).with_range(range)));

        self.found.push(diagnostic);
        self.found.last_mut()
    }

    /// record that the check for each of `lints` reached a conclusion
    ///
    /// this is what an unused suppression is answered from. a check that bailed —
    /// because the settings, the tag libraries or the url tree could not be read —
    /// has decided nothing about the file, and a comment silencing its rule is
    /// doing a job the moment those can be read, so it is not one to call unused.
    fn decides(&mut self, lints: &[&'static LintMetadata]) {
        if !self.settings_known {
            return;
        }

        self.decided.extend(lints.iter().copied().map(LintId::of));
    }

    fn text(&self, range: TextRange) -> &str {
        &self.source[range]
    }

    // ---- 1. a block tag that is never closed -------------------------------

    /// django raises `TemplateSyntaxError: Unclosed tag` for one of these
    ///
    /// only a tag *known* to open a block is reported. a tag the index took to
    /// open one because the file happens to hold a matching `{% end… %}`
    /// elsewhere is a guess, and a guess is not something to report.
    fn unclosed_blocks(&mut self) {
        self.decides(&[&UNCLOSED_TEMPLATE_BLOCK]);

        let unclosed: Vec<_> = self
            .index
            .spans()
            .iter()
            .filter(|block| !block.closed && opens_a_block(self.db, &block.name))
            .map(|block| (block.name.clone(), block.end_tag.clone(), block.open_range))
            .collect();

        for (name, end_tag, range) in unclosed {
            if let Some(diagnostic) = self.report(
                &UNCLOSED_TEMPLATE_BLOCK,
                range,
                format_args!("unclosed `{name}`"),
            ) {
                diagnostic.help(format_args!("close it with `{{% {end_tag} %}}`"));
            }
        }
    }

    // ---- 2. a closing tag that closes nothing ------------------------------

    /// django raises `TemplateSyntaxError: Invalid block tag` for one of these
    ///
    /// as above, only a name django itself would have been waiting for counts: a
    /// `{% endmodal %}` belonging to a project's own block tag is left alone,
    /// since nothing here knows whether `{% modal %}` opens a block at all.
    fn unmatched_closes(&mut self) {
        self.decides(&[&UNMATCHED_TEMPLATE_CLOSE]);

        let strays: Vec<_> = self
            .index
            .strays()
            .iter()
            .filter(|stray| closes_a_block(self.db, &stray.name))
            .map(|stray| (stray.name.clone(), stray.range))
            .collect();

        for (name, range) in strays {
            let opening = name
                .strip_prefix(END_TAG_PREFIX)
                .unwrap_or(&name)
                .to_compact_string();
            if let Some(diagnostic) = self.report(
                &UNMATCHED_TEMPLATE_CLOSE,
                range,
                format_args!("`{name}` closes nothing"),
            ) {
                diagnostic.help(format_args!("nothing above it opens a `{{% {opening} %}}`"));
            }
        }
    }

    // ---- 3. a `{% load %}` of a library that is not there -------------------

    fn unknown_libraries(&mut self) {
        if !project::settings_are_authoritative(self.db, self.db.project()) {
            return;
        }
        self.decides(&[&UNKNOWN_TEMPLATE_LIBRARY]);

        let unknown: Vec<_> = self
            .index
            .loads()
            .iter()
            .filter(|load| {
                !project::tag_libraries(self.db, self.db.project())
                    .iter()
                    .any(|library| library.name == load.library)
            })
            .map(|load| (load.library.clone(), load.range))
            .collect();

        for (library, range) in unknown {
            self.report(
                &UNKNOWN_TEMPLATE_LIBRARY,
                range,
                format_args!("no tag library named `{library}`"),
            );
        }
    }

    // ---- 4, 5, 6. a tag or filter that is unknown or not loaded -------------

    fn unknown_names(&mut self) {
        if !self.libraries_known {
            return;
        }
        self.decides(&[
            &UNKNOWN_TEMPLATE_TAG,
            &UNKNOWN_TEMPLATE_FILTER,
            &UNLOADED_TEMPLATE_LIBRARY,
        ]);

        let mut checked: Vec<(bool, CompactString, TextRange)> = Vec::new();

        for construct in self.index.lexed().constructs() {
            if construct.kind == ConstructKind::Comment {
                continue;
            }

            if let Some(range) = construct.name.filter(|_| self.tag_is_checkable(construct)) {
                checked.push((false, self.text(range).to_compact_string(), range));
            }

            checked.extend(
                self.index
                    .lexed()
                    .construct_tokens(construct)
                    .iter()
                    .filter(|token| token.kind == TokenKind::FilterName)
                    .map(|token| {
                        (
                            true,
                            self.text(token.range).to_compact_string(),
                            token.range,
                        )
                    }),
            );
        }

        for (filter, name, range) in checked {
            match self.provider(&name, filter) {
                Provider::Loaded => {}
                Provider::NotLoaded(library) => self.unloaded(&name, filter, &library, range),
                Provider::Unknown => {
                    let lint = if filter {
                        &UNKNOWN_TEMPLATE_FILTER
                    } else {
                        &UNKNOWN_TEMPLATE_TAG
                    };
                    let kind = if filter { "filter" } else { "tag" };
                    self.report(
                        lint,
                        range,
                        format_args!("no template {kind} named `{name}`"),
                    );
                }
            }
        }
    }

    /// report a tag or filter whose library the template hasn't loaded, with the
    /// `{% load %}` it needs as a fix
    fn unloaded(&mut self, name: &str, filter: bool, library: &str, range: TextRange) {
        let edit = load_edit_for(self.index, &self.loaded, Some(library))
            .map(|edit| Fix::safe_edit(Edit::insertion(edit.text, edit.range.start())));
        let kind = if filter { "filter" } else { "tag" };

        if let Some(diagnostic) = self.report(
            &UNLOADED_TEMPLATE_LIBRARY,
            range,
            format_args!("{kind} `{name}` needs `{{% load {library} %}}`"),
        ) {
            diagnostic.help(format_args!("add `{{% load {library} %}}`"));
            diagnostic.set_optional_fix(edit);
        }
    }

    /// whether a tag name is one this can decide anything about
    ///
    /// a tag that closed a block is a closing tag, and a closing tag is named by
    /// the tag it closes rather than registered under a name of its own. a branch
    /// is the same — `{% empty %}` is parsed by `{% for %}`, not registered — and
    /// since a project's own block tag parses branches nothing here can enumerate,
    /// every tag written directly inside one is left alone.
    fn tag_is_checkable(&self, construct: &Construct) -> bool {
        let Some(range) = construct.name else {
            return false;
        };
        let name = self.text(range);

        // a closing tag is named after the tag it closes rather than registered
        // under a name of its own, so it is never one to look up
        if name.starts_with(END_TAG_PREFIX) || BRANCH_TAGS.contains(&name) {
            return false;
        }

        // the block the tag is written in, if any, decides whether its branches
        // are knowable
        self.index
            .open_blocks_at(construct.range.start())
            .first()
            .is_none_or(|block| builtins::tag(&block.name).is_some())
    }

    /// where a tag or filter comes from, as far as this template is concerned
    fn provider(&self, name: &str, filter: bool) -> Provider {
        // what django provides is the installed django's to say where it can be
        // read, and the builtin table's only where it cannot
        if let Some(provided) = builtins::provided_by_django(self.db, name, filter) {
            return match provided.library() {
                None => Provider::Loaded,
                Some(library) if self.loaded.contains(&library.to_compact_string()) => {
                    Provider::Loaded
                }
                Some(library) => Provider::NotLoaded(library.to_compact_string()),
            };
        }

        let registrations = project::registrations(self.db, self.db.project());
        let mut unloaded = None;
        for registration in registrations.iter().filter(|registration| {
            registration.name == name && (registration.kind == RegistrationKind::Filter) == filter
        }) {
            if self.loaded.contains(&registration.library) {
                return Provider::Loaded;
            }
            unloaded.get_or_insert_with(|| registration.library.clone());
        }

        match unloaded {
            Some(library) => Provider::NotLoaded(library),
            None => Provider::Unknown,
        }
    }

    // ---- 7. a template that is not there -----------------------------------

    fn unresolved_templates(&mut self) {
        if !project::settings_are_authoritative(self.db, self.db.project()) {
            return;
        }
        self.decides(&[&UNRESOLVED_TEMPLATE]);

        let missing: Vec<_> = self
            .index
            .extends()
            .into_iter()
            .chain(self.index.includes())
            .filter(|reference| project::resolve_template(self.db, &reference.name).is_none())
            .map(|reference| (reference.name.clone(), reference.range))
            .collect();

        for (name, range) in missing {
            if let Some(diagnostic) = self.report(
                &UNRESOLVED_TEMPLATE,
                range,
                format_args!("no template named `{name}`"),
            ) {
                diagnostic.help(format_args!("create `{name}`"));
            }
        }
    }

    // ---- 8. a static file that is not there --------------------------------

    fn unresolved_static_files(&mut self) {
        if !project::settings_are_authoritative(self.db, self.db.project()) {
            return;
        }
        self.decides(&[&UNRESOLVED_STATIC_FILE]);

        let files = project::static_files(self.db, self.db.project());
        let named: Vec<_> = self
            .named_by(STATIC_TAG)
            .into_iter()
            .filter(|(name, _)| {
                if files.iter().any(|file| file.name == *name) {
                    return false;
                }

                // a name whose directory holds nothing at all is one this project
                // builds rather than commits, and a source tree cannot answer for
                // a file that isn't in it yet
                let Some((directory, _)) = name.rsplit_once('/') else {
                    return false;
                };
                files.iter().any(|file| {
                    file.name
                        .rsplit_once('/')
                        .is_some_and(|(candidate, _)| candidate == directory)
                })
            })
            .collect();

        for (name, range) in named {
            self.report(
                &UNRESOLVED_STATIC_FILE,
                range,
                format_args!("no static file named `{name}`"),
            );
        }
    }

    // ---- 9, 10. a route that cannot be reversed ----------------------------

    fn routes(&mut self) {
        if !project::routes_are_authoritative(self.db, self.db.project()) {
            return;
        }
        self.decides(&[&UNRESOLVED_ROUTE, &INVALID_ROUTE_ARGUMENTS]);

        let mut unresolved = Vec::new();
        let mut invalid = Vec::new();

        for construct in self.index.lexed().constructs() {
            if construct.name.map(|range| self.text(range)) != Some(URL_TAG) {
                continue;
            }

            let arguments = self.arguments(construct);
            let Some((name, range)) = arguments.first().and_then(|argument| {
                let literal = argument.literal()?;
                Some((
                    self.string_value(literal)?,
                    string_contents(self.source, literal.range),
                ))
            }) else {
                continue;
            };

            let candidates: Vec<&UrlName> = project::url_names(self.db, self.db.project())
                .iter()
                .filter(|url| url.name == name)
                .collect();

            if candidates.is_empty() {
                unresolved.push((name.to_compact_string(), range));
                continue;
            }

            if let Some(complaint) = self.mismatch(&candidates, &arguments[1..]) {
                invalid.push((name.to_compact_string(), complaint, construct.range));
            }
        }

        for (name, range) in unresolved {
            self.report(
                &UNRESOLVED_ROUTE,
                range,
                format_args!("no route named `{name}`"),
            );
        }
        for (name, complaint, range) in invalid {
            self.report(
                &INVALID_ROUTE_ARGUMENTS,
                range,
                format_args!("`{name}` {complaint}"),
            );
        }
    }

    /// why none of `candidates` takes `arguments`, when none of them does
    ///
    /// a route whose whole pattern isn't known takes anything, since what it
    /// takes is exactly what is unknown.
    fn mismatch(&self, candidates: &[&UrlName], arguments: &[Argument]) -> Option<String> {
        let mut complaint = None;

        for candidate in candidates {
            let parameters = candidate.parameters()?;

            match self.accepts(&parameters, arguments) {
                Ok(()) => return None,
                // the first candidate's complaint is the one reported: with one
                // route of the name, which is the usual case, it is the only one
                Err(reason) => complaint.get_or_insert(reason),
            };
        }

        complaint
    }

    /// whether a route taking `parameters` accepts `arguments`
    fn accepts(&self, parameters: &[Parameter], arguments: &[Argument]) -> Result<(), String> {
        let (keyword, positional): (Vec<&Argument>, Vec<&Argument>) = arguments
            .iter()
            .partition(|argument| argument.name.is_some());

        if !keyword.is_empty() && !positional.is_empty() {
            return Err("cannot take positional and keyword arguments at once".to_string());
        }

        if keyword.is_empty() {
            if positional.len() != parameters.len() {
                return Err(format!(
                    "takes {} argument{}, not {}",
                    parameters.len(),
                    if parameters.len() == 1 { "" } else { "s" },
                    positional.len()
                ));
            }

            for (parameter, argument) in parameters.iter().zip(positional) {
                self.converts(parameter, argument)?;
            }
            return Ok(());
        }

        for argument in &keyword {
            let name = argument.name.as_ref().expect("a keyword argument");
            let Some(parameter) = parameters
                .iter()
                .find(|parameter| parameter.name == *name.0)
            else {
                return Err(format!("takes no argument named `{}`", name.0));
            };
            self.converts(parameter, argument)?;
        }

        if let Some(missing) = parameters.iter().find(|parameter| {
            !keyword.iter().any(|argument| {
                argument
                    .name
                    .as_ref()
                    .is_some_and(|(name, _)| *name == parameter.name)
            })
        }) {
            return Err(format!("needs an argument named `{}`", missing.name));
        }

        Ok(())
    }

    /// whether a literal argument is one the parameter's converter would match
    ///
    /// only a literal is answered for: a variable's value is not known here, and
    /// django would only find out at render time either.
    fn converts(&self, parameter: &Parameter, argument: &Argument) -> Result<(), String> {
        let Some(token) = argument.literal() else {
            return Ok(());
        };
        let Some(converter) = parameter.converter else {
            return Ok(());
        };
        let value = match token.kind {
            TokenKind::String => self.string_value(token).unwrap_or_default(),
            TokenKind::Number => self.text(token.range),
            _ => return Ok(()),
        };

        if converter.matches(value) {
            return Ok(());
        }

        Err(format!(
            "takes `{}` through `{}`, which `{value}` is not",
            parameter.name,
            converter.name()
        ))
    }

    // ---- 11. a block no ancestor declares ----------------------------------

    fn unknown_blocks(&mut self) {
        if self.index.extends().is_none() {
            // a template that overrides nothing has no block that could be
            // unknown, which is a conclusion rather than a bail-out
            self.decides(&[&UNKNOWN_TEMPLATE_BLOCK]);
            return;
        }

        let chain = ancestors(self.db, self.file, self.index);
        // a chain that stops before a template with no parent of its own is one
        // whose blocks have not all been seen
        let complete = chain
            .last()
            .is_some_and(|(_, ancestor)| ancestor.extends().is_none());
        if !complete {
            return;
        }
        self.decides(&[&UNKNOWN_TEMPLATE_BLOCK]);

        let declared: FxHashSet<CompactString> = chain
            .iter()
            .flat_map(|(_, ancestor)| ancestor.blocks())
            .map(|block| block.name.clone())
            .collect();

        let unknown: Vec<_> = self
            .index
            .blocks()
            .iter()
            .filter(|block| !declared.contains(&block.name))
            .filter(|block| self.is_top_level(block.name_range))
            .map(|block| (block.name.clone(), block.name_range))
            .collect();

        for (name, range) in unknown {
            if let Some(diagnostic) = self.report(
                &UNKNOWN_TEMPLATE_BLOCK,
                range,
                format_args!("no ancestor template declares `{name}`"),
            ) {
                diagnostic.help("the block is never rendered");
            }
        }
    }

    /// whether `offset` is outside every `{% block %}` and `{% partialdef %}`
    ///
    /// a block nested inside one is rendered as part of it, so it overrides
    /// nothing and needs nothing above it to override.
    fn is_top_level(&self, range: TextRange) -> bool {
        !self.index.spans().iter().any(|span| {
            matches!(span.name.as_str(), "block" | "partialdef")
                && span.body_range.contains(range.start())
        })
    }

    // ---- 12. a lookup landing on a member django will not call -------------

    /// the two ways a `{{ }}` lookup can land on a method and render nothing:
    /// django refuses to call it, or django tries and cannot
    ///
    /// both are read off the same walk because both end the path — once a
    /// segment renders nothing, the segments after it have nothing to resolve
    /// against — and because they are checked in django's own order: it consults
    /// `alters_data` before it attempts the call, so a write method that also
    /// needs arguments is refused rather than uncallable.
    fn members_needing_arguments(&mut self) {
        let env = &ProgramEnvironment::from_file(self.db.program_file(self.file));
        self.decides(&[
            &TEMPLATE_MEMBER_NEEDS_ARGUMENTS,
            &TEMPLATE_MEMBER_ALTERS_DATA,
        ]);

        let mut found = Vec::new();
        let mut refused = Vec::new();

        for construct in self.index.lexed().constructs() {
            if construct.kind != ConstructKind::Variable {
                continue;
            }

            for path in self.paths(construct) {
                let segments: Vec<&str> = path.iter().map(|token| self.text(token.range)).collect();

                for length in 1..segments.len() {
                    let Some(ty) = resolve::path_type(
                        self.db,
                        env,
                        self.file,
                        self.index,
                        self.source,
                        construct.range.start(),
                        &segments[..length],
                    ) else {
                        break;
                    };
                    let name = segments[length];
                    let Some(member) = resolve::uncalled_member_type(self.db, env, ty, name) else {
                        break;
                    };

                    if template_lookup(self.db, env, ty, name, member) == TemplateLookup::Refuses {
                        refused.push((name.to_compact_string(), path[length].range));
                        break;
                    }

                    if callable_needs_arguments(self.db, env, member) {
                        found.push((name.to_compact_string(), path[length].range));
                        break;
                    }
                }
            }
        }

        for (name, range) in found {
            if let Some(diagnostic) = self.report(
                &TEMPLATE_MEMBER_NEEDS_ARGUMENTS,
                range,
                format_args!("`{name}` needs arguments"),
            ) {
                diagnostic.help("django renders nothing for a member it cannot call");
            }
        }

        for (name, range) in refused {
            if let Some(diagnostic) = self.report(
                &TEMPLATE_MEMBER_ALTERS_DATA,
                range,
                format_args!("django will not call `{name}` from a template"),
            ) {
                diagnostic.help("`alters_data` marks it, so django renders nothing instead");
            }
        }
    }

    // ---- 13. a suppression comment that silenced nothing --------------------

    /// a `{# ty: ignore #}` that turned out to be doing nothing
    ///
    /// this is `unused-ignore-comment` as a python file reports it — a comment
    /// that silenced nothing is a comment whose removal changes nothing, and
    /// leaving it there hides the day the rule starts firing again. the semantics
    /// are the python ones down to a rule that configuration has turned off: the
    /// check never runs, so the comment never silences anything, so it is unused.
    ///
    /// what a template adds is that a check can decline to answer at all — see
    /// [`Checker::decides`] — and a rule that was never decided is left alone.
    fn unused_suppressions(&mut self) {
        let Some(severity) = self
            .db
            .rule_selection(self.file)
            .severity(LintId::of(&UNUSED_IGNORE_COMMENT))
        else {
            return;
        };

        // the checks are all done with it, and reporting needs to mark codes used
        // as it goes
        let mut suppressions = std::mem::take(&mut self.suppressions);

        let blanket_answerable = TEMPLATE_LINTS
            .iter()
            .all(|lint| self.decided.contains(&LintId::of(lint)));

        // one candidate per code rather than per comment, so that a code can
        // silence a sibling's report the way it does in a python file
        let mut candidates = Vec::new();
        for (entry, suppression) in suppressions.entries.iter().enumerate() {
            if suppression.rules.is_empty() {
                if !suppression.used && blanket_answerable {
                    candidates.push(Reported { entry, code: None });
                }
                continue;
            }

            candidates.extend(
                suppression
                    .rules
                    .iter()
                    .enumerate()
                    .filter(|(_, rule)| !rule.used && self.answers_for(&rule.name))
                    .map(|(code, _)| Reported {
                        entry,
                        code: Some(code),
                    }),
            );
        }

        // a comment naming `unused-ignore-comment` silences these, and the code
        // being reported cannot silence itself
        let mut reported = Vec::new();
        for candidate in candidates {
            let line = suppressions.entries[candidate.entry].line;
            match suppressions.unused_silencer(line, candidate) {
                Some(silencer) => suppressions.mark_used(silencer),
                None => reported.push(candidate),
            }
        }

        // a code that went on to silence one of these is no longer unused
        reported.retain(|candidate| !suppressions.is_used(*candidate));

        // a comment every one of whose codes went unused is reported whole, since
        // the whole comment is then what there is to take out
        let whole_comments: Vec<usize> = suppressions
            .entries
            .iter()
            .enumerate()
            .filter(|(entry, suppression)| {
                !suppression.rules.is_empty()
                    && reported
                        .iter()
                        .filter(|candidate| candidate.entry == *entry)
                        .count()
                        == suppression.rules.len()
            })
            .map(|(entry, _)| entry)
            .collect();
        reported.retain(|candidate| {
            !whole_comments.contains(&candidate.entry) || candidate.code == Some(0)
        });

        for candidate in &reported {
            let suppression = &suppressions.entries[candidate.entry];
            let whole = whole_comments.contains(&candidate.entry);

            let (range, message) = match candidate.code.filter(|_| !whole) {
                Some(code) => (
                    suppression.rules[code].range,
                    format!(
                        "unused `ty: ignore` directive: `{}`",
                        suppression.rules[code].name
                    ),
                ),
                None if suppression.rules.is_empty() => (
                    suppression.range,
                    "unused blanket `ty: ignore` directive".to_string(),
                ),
                None => (
                    suppression.range,
                    "unused `ty: ignore` directive".to_string(),
                ),
            };

            let mut diagnostic = Diagnostic::new(
                DiagnosticId::Lint(UNUSED_IGNORE_COMMENT.name()),
                severity,
                message,
            );
            diagnostic.annotate(Annotation::primary(Span::from(self.file).with_range(range)));
            diagnostic.help("remove the unused suppression");
            diagnostic.set_fix(match candidate.code.filter(|_| !whole) {
                Some(code) => remove_code_fix(self.source, suppression, code),
                None => remove_comment_fix(self.source, suppression),
            });
            self.found.push(diagnostic);
        }
    }

    /// whether an unused suppression naming `rule` is one to report
    ///
    /// a code naming a rule no template check produces — a python one, or a
    /// misspelling — is left alone: nothing here has looked for it, so nothing
    /// here can say it went unused.
    fn answers_for(&self, rule: &str) -> bool {
        if rule == &*UNUSED_IGNORE_COMMENT.name() {
            return true;
        }

        TEMPLATE_LINTS
            .iter()
            .filter(|lint| rule == &*lint.name())
            .any(|lint| self.decided.contains(&LintId::of(lint)))
    }

    // ---- reading a construct's arguments -----------------------------------

    /// the first argument of every `{% tag %}` of this name that writes a string
    fn named_by(&self, tag: &str) -> Vec<(CompactString, TextRange)> {
        self.index
            .lexed()
            .constructs()
            .iter()
            .filter(|construct| construct.name.map(|range| self.text(range)) == Some(tag))
            .filter_map(|construct| {
                let literal = *self.arguments(construct).first()?.literal()?;
                Some((
                    self.string_value(&literal)?.to_compact_string(),
                    string_contents(self.source, literal.range),
                ))
            })
            .collect()
    }

    /// a tag's arguments, its name and its trailing `as …` dropped
    fn arguments(&self, construct: &Construct) -> Vec<Argument> {
        let tokens = self.index.lexed().inner_tokens(construct);
        let start = usize::from(
            tokens
                .first()
                .is_some_and(|token| token.kind == TokenKind::TagName),
        );

        let mut arguments = Vec::new();
        let mut index = start;

        while let Some(token) = tokens.get(index) {
            match token.kind {
                // everything after an `as` names the tag's result, not its input
                TokenKind::Keyword if self.text(token.range) == "as" => break,
                TokenKind::KeywordArgument => {
                    // the name, then the `=`, then the value
                    let value = tokens.get(index + 2).copied();
                    arguments.push(Argument {
                        name: Some((self.text(token.range).to_compact_string(), token.range)),
                        value,
                    });
                    index = end_of_value(self.source, tokens, index + 2);
                }
                TokenKind::String
                | TokenKind::Number
                | TokenKind::Variable
                | TokenKind::BuiltinConstant => {
                    arguments.push(Argument {
                        name: None,
                        value: Some(*token),
                    });
                    index = end_of_value(self.source, tokens, index);
                }
                _ => index += 1,
            }
        }

        arguments
    }

    /// every dotted path written in a construct, as its tokens
    fn paths(&self, construct: &Construct) -> Vec<Vec<Token>> {
        let tokens = self.index.lexed().inner_tokens(construct);
        let mut paths: Vec<Vec<Token>> = Vec::new();

        for (index, token) in tokens.iter().enumerate() {
            match token.kind {
                TokenKind::Variable => paths.push(vec![*token]),
                TokenKind::Attribute => {
                    // an attribute belongs to the path before it only if a `.`
                    // joins the two
                    let joined = tokens
                        .get(index - 1)
                        .is_some_and(|previous| self.text(previous.range) == ".");
                    if let Some(path) = paths.last_mut().filter(|_| joined) {
                        path.push(*token);
                    }
                }
                _ => {}
            }
        }

        paths
    }

    /// the contents of a string literal token
    fn string_value(&self, token: &Token) -> Option<&str> {
        (token.kind == TokenKind::String)
            .then(|| self.text(string_contents(self.source, token.range)))
    }
}

/// the quick fixes offered for a template diagnostic at `range`
///
/// the suppression is always offered — every rule has to be silenceable — and a
/// reference to a template that isn't there is offered the one action that fixes
/// it, since the template roots are known and so the destination is too.
pub(crate) fn code_actions(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    range: TextRange,
    lint: LintId,
) -> Vec<QuickFix> {
    let mut actions = Vec::new();

    if lint == LintId::of(&UNRESOLVED_TEMPLATE)
        && let Some(destination) = missing_template(db, file, index, source, range)
    {
        actions.push(QuickFix {
            title: format!("Create `{}`", &source[range]),
            edits: Vec::new(),
            preferred: true,
            create: Some(destination),
            add_dependency: None,
        });
    }

    actions.push(QuickFix {
        title: format!("Ignore '{}' for this line", lint.name()),
        edits: vec![FileEdit {
            file,
            edit: Edit::insertion(
                format!(" {{# ty: ignore[{}] #}}", lint.name()),
                line_end(source, range.start()),
            ),
        }],
        preferred: false,
        create: None,
        add_dependency: None,
    });

    actions
}

/// where the template a reference at `range` names ought to go
///
/// the app whose template writes the reference is the app the new template
/// belongs to, so it goes in that app's own `templates` directory — beside the
/// file that wanted it, under the name it wanted.
fn missing_template(
    db: &dyn Db,
    file: File,
    index: &TemplateIndex,
    source: &str,
    range: TextRange,
) -> Option<SystemPathBuf> {
    let name = index
        .extends()
        .into_iter()
        .chain(index.includes())
        .find(|reference| reference.range == range)?;

    let _ = source;
    let root = file
        .path(db)
        .as_system_path()?
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(TEMPLATE_DIRECTORY))?;

    Some(root.join(name.name.as_str()))
}

/// the end of the line `offset` is on, its terminator excluded
fn line_end(source: &str, offset: TextSize) -> TextSize {
    let offset = usize::from(offset);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    let end = if source[..end].ends_with('\r') {
        end - 1
    } else {
        end
    };

    TextSize::try_from(end).unwrap_or_default()
}

/// where a template can get a tag or a filter from
enum Provider {
    /// this template has it
    Loaded,
    /// the project has it, in a library this template hasn't loaded
    NotLoaded(CompactString),
    /// nothing the project can reach registers it
    Unknown,
}

/// one argument written in a tag
struct Argument {
    /// the `name=` a keyword argument is written with
    name: Option<(CompactString, TextRange)>,
    /// the first token of the argument's value
    value: Option<Token>,
}

impl Argument {
    /// the argument's value, when it is written out rather than computed
    fn literal(&self) -> Option<&Token> {
        self.value
            .as_ref()
            .filter(|token| matches!(token.kind, TokenKind::String | TokenKind::Number))
    }
}

/// the index just past the value beginning at `index`
///
/// a value is a dotted path with any number of filters applied to it, and each
/// filter may take an argument of its own — all of it one argument as far as the
/// tag is concerned.
fn end_of_value(source: &str, tokens: &[Token], index: usize) -> usize {
    let mut index = index + 1;

    while let (Some(joiner), Some(_)) = (tokens.get(index), tokens.get(index + 1)) {
        if joiner.kind != TokenKind::Operator || !matches!(&source[joiner.range], "." | "|" | ":") {
            break;
        }
        index += 2;
    }

    index
}

/// the names a builtin tag parses as part of its own block rather than
/// registering
///
/// `{% empty %}` is not a tag django could look up; it is a word `{% for %}`
/// reads. the union over every builtin is used rather than the enclosing block's
/// own list, so that a branch written under the wrong tag is left to django to
/// complain about rather than reported as a tag that doesn't exist.
const BRANCH_TAGS: &[&str] = &["elif", "else", "empty", "plural"];

/// whether `name` is a tag django knows opens a block
fn opens_a_block(db: &dyn Db, name: &str) -> bool {
    builtins::tag(name).is_some_and(|tag| tag.closed_by.is_some())
        || project::registrations(db, db.project())
            .iter()
            .any(|registration| {
                registration.name == name
                    && registration.kind == RegistrationKind::Tag { block: true }
            })
}

/// whether `name` is a tag django knows closes a block
fn closes_a_block(db: &dyn Db, name: &str) -> bool {
    name.strip_prefix(END_TAG_PREFIX)
        .is_some_and(|opening| opens_a_block(db, opening))
}

/// the `{# ty: ignore #}` comments a template writes
///
/// a template has no `# type: ignore` to borrow, and its comment syntax is the
/// natural place to put one: `{# ty: ignore #}` silences every diagnostic on its
/// line, `{# ty: ignore[unknown-template-tag] #}` only the rules it names, and a
/// comment written on a line of its own covers the line below it as well — which
/// is what a tag occupying its whole line needs.
#[derive(Debug, Default)]
struct Suppressions {
    entries: Vec<Suppression>,
}

#[derive(Debug)]
struct Suppression {
    /// the line the comment is on
    line: OneIndexed,
    /// whether the comment stands alone, and so covers the line below it too
    alone: bool,
    /// the whole comment, `{#` and `#}` included
    range: TextRange,
    /// the rules it names, or nothing for a blanket one
    rules: Vec<Rule>,
    /// whether a blanket comment silenced anything
    ///
    /// a comment naming rules answers for each of them separately, so this stays
    /// false there.
    used: bool,
}

/// one rule code written inside a suppression comment
#[derive(Debug)]
struct Rule {
    name: CompactString,
    /// the range of the code itself, brackets and commas excluded
    range: TextRange,
    /// whether this code silenced a diagnostic
    used: bool,
}

/// what a suppression comment is written with
const SUPPRESSION_PREFIX: &str = "ty: ignore";

impl Suppressions {
    fn of(index: &TemplateIndex, source: &str) -> Self {
        let mut entries = Vec::new();

        for construct in index.lexed().constructs() {
            if construct.kind != ConstructKind::Comment {
                continue;
            }

            let comment = &source[construct.range];
            let body = comment
                .trim_start_matches("{#")
                .trim_end_matches("#}")
                .trim();
            let Some(rules) = body.strip_prefix(SUPPRESSION_PREFIX) else {
                continue;
            };
            let rules = match rules.trim() {
                "" => Vec::new(),
                rules => {
                    if !rules.starts_with('[') || !rules.ends_with(']') {
                        continue;
                    }
                    // the codes are read off the comment rather than off the
                    // trimmed body, so that each one keeps the range it was
                    // written at — which is what a diagnostic about a single
                    // code has to point at
                    let (Some(open), Some(close)) = (comment.find('['), comment.rfind(']')) else {
                        continue;
                    };
                    codes(comment, open, close, construct.range.start())
                }
            };

            let line_start = source[..usize::from(construct.range.start())]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let line_end = source[usize::from(construct.range.end())..]
                .find('\n')
                .map_or(source.len(), |index| {
                    usize::from(construct.range.end()) + index
                });

            let alone = source[line_start..usize::from(construct.range.start())]
                .trim()
                .is_empty()
                && source[usize::from(construct.range.end())..line_end]
                    .trim()
                    .is_empty();

            entries.push(Suppression {
                line: line_of(source, construct.range.start()),
                alone,
                range: construct.range,
                rules,
                used: false,
            });
        }

        Self { entries }
    }

    /// whether a comment silences `lint` on `line`, counting every one that does
    /// as used
    ///
    /// every covering comment is counted rather than only the innermost: with two
    /// of them silencing the same rule on the same line there is no saying which
    /// one the writer meant to keep, and calling either unused would be a guess.
    fn silences(&mut self, lint: &'static LintMetadata, line: OneIndexed) -> bool {
        let mut silenced = false;

        for suppression in &mut self.entries {
            if !suppression.covers(line) {
                continue;
            }

            if suppression.rules.is_empty() {
                suppression.used = true;
                silenced = true;
                continue;
            }

            for rule in &mut suppression.rules {
                if rule.name.as_str() == &*lint.name() {
                    rule.used = true;
                    silenced = true;
                }
            }
        }

        silenced
    }

    /// the code that would silence an `unused-ignore-comment` reported on `line`
    ///
    /// only a code counts, never a blanket comment: a blanket `{# ty: ignore #}`
    /// silencing its own unused report would silence every one of these. `naming`
    /// is the code being reported, which cannot silence itself.
    fn unused_silencer(&self, line: OneIndexed, reported: Reported) -> Option<Reported> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, suppression)| suppression.covers(line))
            .flat_map(|(entry, suppression)| {
                suppression
                    .rules
                    .iter()
                    .enumerate()
                    .map(move |(code, rule)| {
                        (
                            Reported {
                                entry,
                                code: Some(code),
                            },
                            rule,
                        )
                    })
            })
            .find(|(candidate, rule)| {
                rule.name.as_str() == &*UNUSED_IGNORE_COMMENT.name() && *candidate != reported
            })
            .map(|(candidate, _)| candidate)
    }

    /// whether the code `reported` names was counted used
    fn is_used(&self, reported: Reported) -> bool {
        let suppression = &self.entries[reported.entry];
        match reported.code {
            Some(code) => suppression.rules[code].used,
            None => suppression.used,
        }
    }

    fn mark_used(&mut self, reported: Reported) {
        let suppression = &mut self.entries[reported.entry];
        match reported.code {
            Some(code) => suppression.rules[code].used = true,
            None => suppression.used = true,
        }
    }
}

impl Suppression {
    /// whether this comment applies to `line`
    fn covers(&self, line: OneIndexed) -> bool {
        self.line == line || (self.alone && self.line.saturating_add(1) == line)
    }
}

/// what an `unused-ignore-comment` diagnostic is about: a whole comment, or one
/// code inside it
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Reported {
    entry: usize,
    code: Option<usize>,
}

/// the codes written between `open` and `close` in `comment`, which starts at
/// `start` in the source
fn codes(comment: &str, open: usize, close: usize, start: TextSize) -> Vec<Rule> {
    let mut rules = Vec::new();
    let mut offset = open + 1;

    for piece in comment[open + 1..close].split(',') {
        let leading = piece.len() - piece.trim_start().len();
        let name = piece.trim();

        if !name.is_empty()
            && let Ok(at) = TextSize::try_from(offset + leading)
        {
            rules.push(Rule {
                name: name.to_compact_string(),
                range: TextRange::at(start + at, TextSize::of(name)),
                used: false,
            });
        }

        offset += piece.len() + 1;
    }

    rules
}

/// take a whole suppression comment out
///
/// a comment with nothing else on its line takes the line with it, so that
/// removing it doesn't leave a blank one behind.
fn remove_comment_fix(source: &str, suppression: &Suppression) -> Fix {
    let start = usize::from(suppression.range.start());
    let end = usize::from(suppression.range.end());

    let (start, end) = if suppression.alone {
        let start = source[..start].rfind('\n').map_or(0, |index| index + 1);
        let end = source[end..]
            .find('\n')
            .map_or(source.len(), |index| end + index + 1);
        (start, end)
    } else {
        // the whitespace that separated the comment from what precedes it goes
        // with it, so that no trailing space is left behind
        (source[..start].trim_end().len(), end)
    };

    Fix::safe_edit(Edit::range_deletion(range_of(start, end)))
}

/// take one code out of a suppression comment, with the comma that separated it
fn remove_code_fix(source: &str, suppression: &Suppression, code: usize) -> Fix {
    let range = suppression.rules[code].range;
    let limit = usize::from(suppression.range.end());
    let start = usize::from(range.start());
    let mut end = usize::from(range.end());

    let trailing = source[end..limit]
        .find(|character: char| !character.is_whitespace())
        .unwrap_or(0);
    if source[end + trailing..].starts_with(',') {
        // the comma and the space after it go with the code, so that the codes
        // left behind read as they were written
        end += trailing + 1;
        end += source[end..limit]
            .find(|character: char| !character.is_whitespace())
            .unwrap_or(0);
        return Fix::safe_edit(Edit::range_deletion(range_of(start, end)));
    }

    // the last code carries the comma before it instead
    let kept = source[..start].trim_end();
    let start = kept.strip_suffix(',').map_or(start, |before| {
        before
            .trim_end_matches(|character: char| character.is_whitespace())
            .len()
    });

    Fix::safe_edit(Edit::range_deletion(range_of(start, end)))
}

/// the range from `start` to `end`, which are byte offsets into a source
fn range_of(start: usize, end: usize) -> TextRange {
    let start = TextSize::try_from(start).unwrap_or_default();
    let end = TextSize::try_from(end).unwrap_or_default();

    TextRange::new(start.min(end), end)
}

/// the one-indexed line `offset` is on
fn line_of(source: &str, offset: TextSize) -> OneIndexed {
    OneIndexed::from_zero_indexed(source[..usize::from(offset)].matches('\n').count())
}

#[cfg(test)]
mod tests {
    use ruff_text_size::Ranged;

    use ty_python_semantic::lint::Level;

    use crate::django_template::tests::{DJANGO_BUILTINS, TemplateTest};

    /// a whole django project: a settings module the convention finds, an app
    /// with a tag library, models, a view, a url tree and a base template
    ///
    /// django is installed beside it rather than in it, since that is what makes
    /// `{% load static %}` and `{% load humanize %}` resolve — and what makes the
    /// indexes authoritative, which every check below depends on.
    fn project(template: &str) -> TemplateTest {
        project_with_rules(template, &[])
    }

    /// the same project, with some rules set to a level of their own
    fn project_with_rules(template: &str, rules: &[(&str, Level)]) -> TemplateTest {
        TemplateTest::with_rules(
            &[
                (
                    "manage.py",
                    "
                    import os

                    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
                    ",
                ),
                ("project/__init__.py", ""),
                (
                    "project/settings.py",
                    "
                    INSTALLED_APPS = ['django.contrib.humanize', 'blog']

                    TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]

                    ROOT_URLCONF = 'project.urls'
                    ",
                ),
                (
                    "project/urls.py",
                    "
                    from django.urls import include, path

                    urlpatterns = [path('blog/', include('blog.urls'))]
                    ",
                ),
                ("blog/__init__.py", ""),
                (
                    "blog/models.py",
                    "
                    from django.db.models import Model


                    class Book:
                        pk: int
                        title: str

                        def summary(self) -> str:
                            return self.title

                        def excerpt(self, length: int) -> str:
                            return self.title[:length]

                        def save(self) -> None:
                            return None


                    class Page(Model):
                        number: int

                        # a name of django's own, overridden. django's own
                        # `AltersData` copies the mark onto an override
                        def delete(self) -> None:
                            return None

                        # a name django marks on a queryset but not on a model
                        def update(self) -> str:
                            return ''
                    ",
                ),
                (
                    "blog/views.py",
                    "
                    from blog.models import Book, Page


                    def post(request):
                        return render(request, 'blog/post.html', {'book': Book(), 'page': Page()})
                    ",
                ),
                (
                    "blog/urls.py",
                    "
                    from django.urls import path

                    app_name = 'blog'

                    urlpatterns = [
                        path('', index, name='index'),
                        path('<int:pk>/', detail, name='detail'),
                        path('<slug:slug>/<int:page>/', paged, name='paged'),
                        path('<str:title>/', titled, name='titled'),
                        path('<uuid:key>/', keyed, name='keyed'),
                        path('<path:rest>/edit/', edit, name='edit'),
                        re_path(r'^archive/(?P<year>[0-9]{4})/$', archive, name='archive'),
                    ]
                    ",
                ),
                ("blog/templatetags/__init__.py", ""),
                (
                    "blog/templatetags/blog_extras.py",
                    "
                    from django import template

                    register = template.Library()

                    @register.filter
                    def shout(value):
                        return value

                    @register.simple_tag
                    def badge():
                        return ''

                    @register.simple_block_tag
                    def box(content):
                        return content
                    ",
                ),
                (
                    "blog/templates/blog/base.html",
                    "{% block content %}{% endblock %}{% block footer %}{% endblock %}",
                ),
                ("blog/static/blog/site.css", "body {}"),
                ("blog/templates/blog/post.html", template),
            ],
            &[
                ("django/__init__.py", ""),
                ("django/db/__init__.py", ""),
                (
                    "django/db/models/__init__.py",
                    "from django.db.models.base import Model",
                ),
                (
                    "django/db/models/utils.py",
                    "
                    class AltersData:
                        def __init_subclass__(cls, **kwargs) -> None: ...
                    ",
                ),
                (
                    "django/db/models/base.py",
                    "
                    from django.db.models.utils import AltersData


                    class Model(AltersData):
                        pk: int

                        def save(self) -> None: ...

                        def delete(self) -> None: ...

                        def refresh_from_db(self) -> None: ...
                    ",
                ),
                ("django/templatetags/__init__.py", ""),
                (
                    "django/templatetags/static.py",
                    "
                    from django.template import Library

                    register = Library()

                    @register.simple_tag
                    def static(path):
                        return path
                    ",
                ),
                ("django/contrib/__init__.py", ""),
                ("django/contrib/humanize/__init__.py", ""),
                ("django/contrib/humanize/templatetags/__init__.py", ""),
                (
                    "django/contrib/humanize/templatetags/humanize.py",
                    "
                    from django.template import Library

                    register = Library()

                    @register.filter
                    def intcomma(value):
                        return value
                    ",
                ),
            ],
            rules,
        )
    }

    /// the project's templates with nothing wrong with them
    #[test]
    fn a_correct_template_reports_nothing() {
        let test = project(
            "{% extends 'blog/base.html' %}\n\
             {% load blog_extras static %}\n\
             {% block content %}\n\
             <img src=\"{% static 'blog/site.css' %}\">\n\
             <a href=\"{% url 'blog:detail' pk=book.pk %}\">{{ book.title|shout|upper }}</a>\n\
             {% for word in book.summary %}{{ word }}{% empty %}none{% endfor %}\n\
             {% if book %}{{ book.summary }}{% else %}?{% endif %}\n\
             {% endblock %}\n",
        );

        assert_eq!(test.diagnostics(), Vec::<String>::new());
    }

    #[test]
    fn an_unclosed_block_tag_is_reported() {
        assert_eq!(
            project("{% if book %}\n<p>hi</p>\n").diagnostics(),
            ["unclosed-template-block Error: unclosed `if` [{% if book %}]"]
        );
    }

    #[test]
    fn a_closed_block_tag_is_not_reported() {
        assert!(
            project("{% if book %}<p>hi</p>{% endif %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn an_unclosed_block_tag_of_the_projects_own_is_reported() {
        assert_eq!(
            project("{% load blog_extras %}\n{% box %}a{% endbox %}\n{% box %}b\n").diagnostics(),
            ["unclosed-template-block Error: unclosed `box` [{% box %}]"]
        );
    }

    #[test]
    fn a_tag_nothing_says_opens_a_block_is_not_reported_unclosed() {
        // `{% modal %}` is nobody's registered block tag; that this file pairs one
        // of them with an `{% endmodal %}` is not enough to call the other wrong.
        // that the tag isn't registered at all is a different complaint
        assert_eq!(
            project("{% modal %}a{% endmodal %}\n{% modal %}b\n").diagnostics(),
            [
                "unknown-template-tag Error: no template tag named `modal` [modal]",
                "unknown-template-tag Error: no template tag named `modal` [modal]",
            ]
        );
    }

    #[test]
    fn a_closing_tag_that_closes_nothing_is_reported() {
        assert_eq!(
            project("{% for book in books %}a{% endwith %}{% endfor %}\n").diagnostics(),
            ["unmatched-template-close Error: `endwith` closes nothing [{% endwith %}]"]
        );
    }

    #[test]
    fn a_closing_tag_that_closes_something_is_not_reported() {
        assert!(
            project("{% for book in books %}a{% endfor %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_closing_tag_of_an_unknown_block_tag_is_not_reported() {
        assert!(project("{% endmodal %}\n").diagnostics().is_empty());
    }

    #[test]
    fn a_load_of_a_library_that_is_not_there_is_reported() {
        assert_eq!(
            project("{% load blog_xtras %}\n").diagnostics(),
            ["unknown-template-library Error: no tag library named `blog_xtras` [blog_xtras]"]
        );
    }

    #[test]
    fn a_load_of_a_library_that_is_there_is_not_reported() {
        assert!(
            project("{% load blog_extras static humanize %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_tag_nothing_registers_is_reported() {
        assert_eq!(
            project("{% iff book %}\n").diagnostics(),
            ["unknown-template-tag Error: no template tag named `iff` [iff]"]
        );
    }

    #[test]
    fn a_branch_of_a_builtin_block_is_not_reported_as_a_tag() {
        assert!(
            project(
                "{% if book %}a{% else %}b{% endif %}{% for x in y %}a{% empty %}b{% endfor %}"
            )
            .diagnostics()
            .is_empty()
        );
    }

    #[test]
    fn a_tag_written_inside_a_block_tag_nobody_knows_is_not_reported() {
        // whatever `{% modal %}` parses between its own tags is `{% modal %}`'s
        // business, and nothing here can enumerate it — only the opening tag,
        // which is written outside any such block, is answered for
        assert_eq!(
            project("{% modal %}{% modal_header %}{% endmodal %}").diagnostics(),
            ["unknown-template-tag Error: no template tag named `modal` [modal]"]
        );
    }

    #[test]
    fn a_filter_nothing_registers_is_reported() {
        assert_eq!(
            project("{{ book.title|uppercase }}\n").diagnostics(),
            ["unknown-template-filter Error: no template filter named `uppercase` [uppercase]"]
        );
    }

    #[test]
    fn a_builtin_filter_is_not_reported() {
        assert!(
            project("{{ book.title|upper|default:'x' }}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_tag_whose_library_is_not_loaded_is_reported() {
        assert_eq!(
            project("{% static 'blog/site.css' %}\n").diagnostics(),
            ["unloaded-template-library Error: tag `static` needs `{% load static %}` [static]"]
        );
    }

    #[test]
    fn a_filter_whose_library_is_not_loaded_is_reported() {
        assert_eq!(
            project("{{ book.title|shout }}\n").diagnostics(),
            [
                "unloaded-template-library Error: filter `shout` needs `{% load blog_extras %}` \
                 [shout]"
            ]
        );
    }

    #[test]
    fn an_unloaded_library_carries_the_load_it_needs_as_a_fix() {
        let test = project("{% extends 'blog/base.html' %}\n{{ book.title|shout }}\n");
        let diagnostics = crate::django_template::django_template_diagnostics(&test.db, test.file);

        let fix = diagnostics[0].fix().expect("a fix");
        let edit = fix.edits().first().expect("an edit");

        assert_eq!(edit.content(), Some("\n{% load blog_extras %}"));
        assert_eq!(
            u32::from(edit.start()),
            30,
            "the load goes below the `{{% extends %}}`, which has to stay first"
        );
    }

    #[test]
    fn a_loaded_library_is_not_reported() {
        assert!(
            project(
                "{% load blog_extras static %}{% static 'blog/site.css' %}{{ book.title|shout }}"
            )
            .diagnostics()
            .is_empty()
        );
    }

    #[test]
    fn a_library_the_settings_load_into_every_template_needs_no_load() {
        // this is the same template as above with the `{% load %}` taken away, and
        // it is correct because `OPTIONS['builtins']` says so
        let test = TemplateTest::with_site_packages(
            &[
                (
                    "manage.py",
                    "
                    import os

                    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
                    ",
                ),
                ("project/__init__.py", ""),
                (
                    "project/settings.py",
                    "
                    INSTALLED_APPS = []

                    TEMPLATES = [
                        {
                            'APP_DIRS': True,
                            'OPTIONS': {'builtins': ['django.templatetags.static']},
                        }
                    ]
                    ",
                ),
                ("blog/__init__.py", ""),
                (
                    "blog/templates/blog/post.html",
                    "{% static 'blog/site.css' %}",
                ),
            ],
            &[
                ("django/__init__.py", ""),
                ("django/templatetags/__init__.py", ""),
                (
                    "django/templatetags/static.py",
                    "
                    register = Library()

                    @register.simple_tag
                    def static(path):
                        return path
                    ",
                ),
            ],
        );

        assert!(test.diagnostics().is_empty());
    }

    #[test]
    fn an_extends_of_a_template_that_is_not_there_is_reported() {
        assert_eq!(
            project("{% extends 'blog/bass.html' %}\n").diagnostics(),
            ["unresolved-template Error: no template named `blog/bass.html` [blog/bass.html]"]
        );
    }

    #[test]
    fn an_include_of_a_template_that_is_not_there_is_reported() {
        assert_eq!(
            project("{% include 'blog/card.html' %}\n").diagnostics(),
            ["unresolved-template Error: no template named `blog/card.html` [blog/card.html]"]
        );
    }

    #[test]
    fn a_reference_to_a_template_that_is_there_is_not_reported() {
        assert!(
            project("{% extends 'blog/base.html' %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_static_file_that_is_not_there_is_reported() {
        assert_eq!(
            project("{% load static %}{% static 'blog/sight.css' %}\n").diagnostics(),
            [
                "unresolved-static-file Warning: no static file named `blog/sight.css` \
                 [blog/sight.css]"
            ]
        );
    }

    #[test]
    fn a_static_file_that_is_there_is_not_reported() {
        assert!(
            project("{% load static %}{% static 'blog/site.css' %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_static_file_in_a_directory_the_source_tree_has_nothing_in_is_not_reported() {
        // `bundles/` is what a build writes into `static/`, and a source tree
        // cannot answer for a file that isn't in it yet
        assert!(
            project("{% load static %}{% static 'bundles/app.js' %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_url_naming_a_route_that_is_not_there_is_reported() {
        assert_eq!(
            project("{% url 'blog:missing' %}\n").diagnostics(),
            ["unresolved-route Error: no route named `blog:missing` [blog:missing]"]
        );
    }

    #[test]
    fn a_url_naming_a_route_that_is_there_is_not_reported() {
        assert!(
            project("{% url 'blog:index' %}{% url 'blog:detail' 1 %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_url_missing_an_argument_is_reported() {
        assert_eq!(
            project("{% url 'blog:detail' %}\n").diagnostics(),
            [
                "invalid-route-arguments Error: `blog:detail` takes 1 argument, not 0 \
                 [{% url 'blog:detail' %}]"
            ]
        );
    }

    #[test]
    fn a_url_with_an_argument_the_route_does_not_name_is_reported() {
        assert_eq!(
            project("{% url 'blog:detail' slug='x' %}\n").diagnostics(),
            [
                "invalid-route-arguments Error: `blog:detail` takes no argument named `slug` \
                 [{% url 'blog:detail' slug='x' %}]"
            ]
        );
    }

    #[test]
    fn a_url_whose_literal_the_converter_would_reject_is_reported() {
        assert_eq!(
            project("{% url 'blog:detail' pk='abc' %}\n").diagnostics(),
            [
                "invalid-route-arguments Error: `blog:detail` takes `pk` through `int`, which \
                 `abc` is not [{% url 'blog:detail' pk='abc' %}]"
            ]
        );
    }

    #[test]
    fn a_url_whose_arguments_the_route_takes_is_not_reported() {
        assert!(
            project(
                "{% url 'blog:detail' pk=1 %}{% url 'blog:paged' slug='a' page=2 %}\
                 {% url 'blog:paged' 'a' 2 %}{% url 'blog:detail' book.pk %}\n"
            )
            .diagnostics()
            .is_empty()
        );
    }

    /// the message every converter gives a literal it would refuse
    ///
    /// django's converters are regexes, and `reverse()` raises `NoReverseMatch`
    /// for an argument that does not match one, so each of these is wrong before
    /// the template is ever rendered.
    #[test]
    fn a_literal_no_converter_would_match_is_reported() {
        let rejected = [
            (
                "blog:detail",
                "pk='abc'",
                "`pk` through `int`, which `abc` is not",
            ),
            (
                "blog:detail",
                "pk='-1'",
                "`pk` through `int`, which `-1` is not",
            ),
            (
                "blog:titled",
                "title='a/b'",
                "`title` through `str`, which `a/b` is not",
            ),
            (
                "blog:paged",
                "slug='a b' page=2",
                "`slug` through `slug`, which `a b` is not",
            ),
            (
                "blog:keyed",
                "key='nope'",
                "`key` through `uuid`, which `nope` is not",
            ),
            (
                "blog:keyed",
                "key='123E4567-E89B-12D3-A456-426614174000'",
                "`key` through `uuid`, which `123E4567-E89B-12D3-A456-426614174000` is not",
            ),
            (
                "blog:edit",
                "rest=''",
                "`rest` through `path`, which `` is not",
            ),
        ];

        for (route, arguments, complaint) in rejected {
            let template = format!("{{% url '{route}' {arguments} %}}\n");
            assert_eq!(
                project(&template).diagnostics(),
                [format!(
                    "invalid-route-arguments Error: `{route}` takes {complaint} \
                     [{}]",
                    template.trim_end()
                )],
                "{template}"
            );
        }
    }

    /// the same converters given a literal they do match
    #[test]
    fn a_literal_its_converter_matches_is_not_reported() {
        let accepted = [
            ("blog:detail", "pk=12"),
            ("blog:detail", "pk='12'"),
            ("blog:titled", "title='a b'"),
            ("blog:paged", "slug='a-b_1' page=2"),
            ("blog:keyed", "key='123e4567-e89b-12d3-a456-426614174000'"),
            ("blog:edit", "rest='a/b/c'"),
        ];

        for (route, arguments) in accepted {
            let template = format!("{{% url '{route}' {arguments} %}}\n");
            assert_eq!(
                project(&template).diagnostics(),
                Vec::<String>::new(),
                "{template}"
            );
        }
    }

    #[test]
    fn a_literal_written_against_a_regex_group_is_not_reported() {
        // a `re_path()` names its arguments but puts them through a pattern
        // rather than a converter, so there is nothing here to hold one against
        assert!(
            project("{% url 'blog:archive' year='not a year' %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_variable_argument_of_an_unknown_type_is_not_reported() {
        assert!(
            project("{% url 'blog:detail' pk=whatever %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_variable_argument_of_a_type_the_converter_takes_is_not_reported() {
        assert!(
            project("{% url 'blog:detail' pk=book.pk %}{% url 'blog:detail' book.pk %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_variable_argument_of_a_type_the_converter_would_not_take_is_not_reported() {
        // django stringifies before it matches, so a `str` may still be all
        // digits — a type alone is never enough to call an argument wrong
        assert!(
            project("{% url 'blog:detail' pk=book.title %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_block_no_ancestor_declares_is_reported() {
        assert_eq!(
            project("{% extends 'blog/base.html' %}{% block sidebar %}a{% endblock %}\n")
                .diagnostics(),
            ["unknown-template-block Warning: no ancestor template declares `sidebar` [sidebar]"]
        );
    }

    #[test]
    fn a_block_an_ancestor_declares_is_not_reported() {
        assert!(
            project("{% extends 'blog/base.html' %}{% block content %}a{% endblock %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_block_nested_inside_an_overriding_one_is_not_reported() {
        // django renders it as part of the block around it, so nothing above has
        // to declare it
        assert!(
            project(
                "{% extends 'blog/base.html' %}\
                 {% block content %}{% block inner %}a{% endblock %}{% endblock %}\n"
            )
            .diagnostics()
            .is_empty()
        );
    }

    #[test]
    fn a_block_in_a_template_that_extends_nothing_is_not_reported() {
        assert!(
            project("{% block anything %}a{% endblock %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_member_that_needs_arguments_is_reported() {
        assert_eq!(
            project("{{ book.excerpt }}\n").diagnostics(),
            ["template-member-needs-arguments Warning: `excerpt` needs arguments [excerpt]"]
        );
    }

    #[test]
    fn a_member_that_needs_no_arguments_is_not_reported() {
        assert!(
            project("{{ book.summary }}{{ book.title }}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_member_django_refuses_to_call_is_reported() {
        assert_eq!(
            project("{{ page.save }}\n").diagnostics(),
            [
                "template-member-alters-data Warning: django will not call `save` from a template [save]"
            ]
        );
    }

    #[test]
    fn an_override_of_a_member_django_refuses_to_call_is_reported() {
        assert_eq!(
            project("{{ page.delete }}\n").diagnostics(),
            [
                "template-member-alters-data Warning: django will not call `delete` from a template [delete]"
            ]
        );
    }

    #[test]
    fn a_member_of_the_projects_own_class_is_not_reported() {
        assert!(project("{{ book.save }}\n").diagnostics().is_empty());
    }

    #[test]
    fn a_model_method_django_has_no_equivalent_of_is_not_reported() {
        assert!(project("{{ page.update }}\n").diagnostics().is_empty());
    }

    #[test]
    fn a_model_member_django_calls_is_not_reported() {
        assert!(
            project("{{ page.number }}{{ page.refresh_from_db }}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_path_continuing_through_a_refused_member_is_reported_once() {
        assert_eq!(
            project("{{ page.save.anything }}\n").diagnostics(),
            [
                "template-member-alters-data Warning: django will not call `save` from a template [save]"
            ]
        );
    }

    #[test]
    fn a_suppression_on_the_same_line_silences_the_line() {
        assert!(
            project("{% iff book %} {# ty: ignore #}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_suppression_on_its_own_line_silences_the_line_below_it() {
        assert!(
            project("{# ty: ignore #}\n{% iff book %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_suppression_naming_rules_silences_only_those() {
        assert_eq!(
            project("{# ty: ignore[unknown-template-filter] #}\n{% iff book %}\n").diagnostics(),
            [
                "unused-ignore-comment Warning: unused `ty: ignore` directive [{# ty: ignore[unknown-template-filter] #}]",
                "unknown-template-tag Error: no template tag named `iff` [iff]"
            ],
            "a rule the comment doesn't name is still reported, and the one it does name went unused"
        );
        assert!(
            project("{# ty: ignore[unknown-template-tag] #}\n{% iff book %}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_suppression_with_something_beside_it_covers_only_its_own_line() {
        assert_eq!(
            project("a {# ty: ignore #}\n{% iff book %}\n").diagnostics(),
            [
                "unused-ignore-comment Warning: unused blanket `ty: ignore` directive [{# ty: ignore #}]",
                "unknown-template-tag Error: no template tag named `iff` [iff]"
            ]
        );
    }

    #[test]
    fn a_suppression_that_silences_nothing_is_reported() {
        assert_eq!(
            project("{# ty: ignore[unknown-template-tag] #}\n<p>hello</p>\n").diagnostics(),
            [
                "unused-ignore-comment Warning: unused `ty: ignore` directive [{# ty: ignore[unknown-template-tag] #}]"
            ]
        );
    }

    #[test]
    fn a_blanket_suppression_that_silences_nothing_is_reported() {
        assert_eq!(
            project("{# ty: ignore #}\n<p>hello</p>\n").diagnostics(),
            [
                "unused-ignore-comment Warning: unused blanket `ty: ignore` directive [{# ty: ignore #}]"
            ]
        );
    }

    #[test]
    fn a_suppression_that_silences_something_is_not_reported() {
        assert!(
            project("{% iff book %} {# ty: ignore[unknown-template-tag] #}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn only_the_codes_of_a_suppression_that_went_unused_are_reported() {
        assert_eq!(
            project("{% iff book %} {# ty: ignore[unknown-template-tag, unresolved-route] #}\n")
                .diagnostics(),
            [
                "unused-ignore-comment Warning: unused `ty: ignore` directive: `unresolved-route` [unresolved-route]"
            ],
            "the code that did the silencing is left alone"
        );
    }

    #[test]
    fn a_suppression_naming_a_rule_no_template_check_produces_is_left_alone() {
        // nothing here looked for `division-by-zero`, so nothing here can say the
        // comment went unused
        assert!(
            project("{# ty: ignore[division-by-zero] #}\n<p>hello</p>\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn an_unused_suppression_is_itself_suppressible() {
        assert!(
            project(
                "<p>hello</p> {# ty: ignore[unknown-template-tag] #} {# ty: ignore[unused-ignore-comment] #}\n"
            )
            .diagnostics()
            .is_empty()
        );
    }

    #[test]
    fn a_suppression_for_the_unused_rule_cannot_silence_its_own_report() {
        assert_eq!(
            project("<p>hello</p> {# ty: ignore[unused-ignore-comment] #}\n").diagnostics(),
            [
                "unused-ignore-comment Warning: unused `ty: ignore` directive [{# ty: ignore[unused-ignore-comment] #}]"
            ]
        );
    }

    #[test]
    fn a_blanket_suppression_does_not_silence_its_own_unused_report() {
        assert_eq!(
            project("<p>hello</p> {# ty: ignore #}\n").diagnostics(),
            [
                "unused-ignore-comment Warning: unused blanket `ty: ignore` directive [{# ty: ignore #}]"
            ]
        );
    }

    /// the template as every reported fix would leave it
    fn fixed(template: &str) -> String {
        let test = project(template);
        let source = ruff_db::source::source_text(&test.db, test.file);
        let mut fixed = source.to_string();

        let diagnostics = crate::django_template::django_template_diagnostics(&test.db, test.file);
        let mut edits: Vec<_> = diagnostics
            .iter()
            .filter_map(ruff_db::diagnostic::Diagnostic::fix)
            .flat_map(ruff_diagnostics::Fix::edits)
            .collect();
        // back to front, so that an earlier edit's offsets still hold
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.start()));

        for edit in edits {
            fixed.replace_range(
                usize::from(edit.start())..usize::from(edit.end()),
                edit.content().unwrap_or_default(),
            );
        }

        fixed
    }

    #[test]
    fn the_fix_takes_out_what_went_unused() {
        assert_eq!(
            fixed("{# ty: ignore[unknown-template-tag] #}\n<p>hello</p>\n"),
            "<p>hello</p>\n",
            "a comment with nothing else on its line takes the line with it"
        );
        assert_eq!(fixed("<p>hello</p> {# ty: ignore #}\n"), "<p>hello</p>\n");
        assert_eq!(
            fixed("{{ x|nope }} {# ty: ignore[unresolved-route, unknown-template-filter] #}\n"),
            "{{ x|nope }} {# ty: ignore[unknown-template-filter] #}\n",
            "only the code that went unused, and the comma after it"
        );
        assert_eq!(
            fixed("{{ x|nope }} {# ty: ignore[unknown-template-filter, unresolved-route] #}\n"),
            "{{ x|nope }} {# ty: ignore[unknown-template-filter] #}\n",
            "the last code carries the comma before it instead"
        );
    }

    #[test]
    fn a_code_beside_the_unused_rule_silences_the_whole_comment() {
        // what a python file does for `# ty: ignore[division-by-zero,
        // unused-ignore-comment]`: the second code silences the first's report,
        // and is used up doing it
        assert!(
            project("<p>hello</p> {# ty: ignore[unknown-template-tag, unused-ignore-comment] #}\n")
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn a_suppression_for_a_rule_configuration_turned_off_is_reported() {
        // the same as a python file: the check never runs, so the comment never
        // silences anything, so it is unused. see the `ty: ignore` suppression
        // tests under `crates/ty_python_semantic/resources/mdtest/suppressions`
        let test = project_with_rules(
            "{% iff book %} {# ty: ignore[unknown-template-tag] #}\n",
            &[("unknown-template-tag", Level::Ignore)],
        );

        assert_eq!(
            test.diagnostics(),
            [
                "unused-ignore-comment Warning: unused `ty: ignore` directive [{# ty: ignore[unknown-template-tag] #}]"
            ]
        );
    }

    #[test]
    fn nothing_is_reported_where_the_unused_rule_is_turned_off() {
        let test = project_with_rules(
            "<p>hello</p> {# ty: ignore[unknown-template-tag] #}\n",
            &[("unused-ignore-comment", Level::Ignore)],
        );

        assert!(test.diagnostics().is_empty());
    }

    #[test]
    fn a_suppression_is_not_reported_unused_where_a_check_could_not_answer() {
        // no `manage.py`, so no check decided anything about this file — see
        // `a_project_whose_settings_cannot_be_read_reports_nothing_it_would_have_to_guess`
        let test = TemplateTest::new(&[
            ("blog/__init__.py", ""),
            (
                "blog/templates/blog/post.html",
                "{# ty: ignore[unknown-template-tag] #}\n{# ty: ignore #}\n",
            ),
        ]);

        assert!(test.diagnostics().is_empty());
    }

    #[test]
    fn a_project_whose_settings_cannot_be_read_reports_nothing_it_would_have_to_guess() {
        // no `manage.py`, so no `INSTALLED_APPS` — and without those there is no
        // saying what a tag library, a template or a static file even is
        let test = TemplateTest::new(&[
            ("blog/__init__.py", ""),
            (
                "blog/templates/blog/post.html",
                "{% load nope %}{% extends 'blog/nope.html' %}{% iff x %}{{ x|nope }}\n",
            ),
        ]);

        assert!(test.diagnostics().is_empty());
    }

    #[test]
    fn a_project_whose_url_tree_cannot_be_walked_reports_no_route() {
        // `ROOT_URLCONF` names a module nothing can reach, so every name it holds
        // is a name that would be reported missing
        let test = TemplateTest::new(&[
            (
                "manage.py",
                "
                import os

                os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
                ",
            ),
            ("project/__init__.py", ""),
            (
                "project/settings.py",
                "
                INSTALLED_APPS = ['blog']

                TEMPLATES = [{'APP_DIRS': True}]

                ROOT_URLCONF = 'project.urls'
                ",
            ),
            (
                "project/urls.py",
                "
                from django.urls import include, path

                urlpatterns = [path('blog/', include('nowhere.urls'))]
                ",
            ),
            ("blog/__init__.py", ""),
            ("blog/templates/blog/post.html", "{% url 'anything' %}\n"),
        ]);

        assert!(test.diagnostics().is_empty());
    }

    /// a project whose django can be read all the way down to its implicit
    /// builtins
    ///
    /// what that django registers is deliberately not what the builtin table
    /// says — see [`DJANGO_BUILTINS`] — which is the whole point: the table is a
    /// fallback for a django nothing can read, not a claim about this one.
    fn with_djangos_own_builtins(template: &str) -> TemplateTest {
        TemplateTest::with_site_packages(
            &[
                (
                    "manage.py",
                    "
                    import os

                    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
                    ",
                ),
                ("project/__init__.py", ""),
                (
                    "project/settings.py",
                    "
                    INSTALLED_APPS = []

                    TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]
                    ",
                ),
                ("app/templates/app/page.html", template),
            ],
            DJANGO_BUILTINS,
        )
    }

    #[test]
    fn a_tag_this_django_registers_that_the_table_never_heard_of_is_not_reported() {
        assert_eq!(
            with_djangos_own_builtins("{% squish %}{{ x|shorten:2 }}").diagnostics(),
            Vec::<String>::new(),
            "django registers them, so they are there whatever the table knows"
        );
    }

    #[test]
    fn a_name_the_table_has_that_this_django_does_not_register_is_reported() {
        assert_eq!(
            with_djangos_own_builtins("{% lorem %}").diagnostics(),
            ["unknown-template-tag Error: no template tag named `lorem` [lorem]"]
        );
        assert_eq!(
            with_djangos_own_builtins("{{ x|slugify }}").diagnostics(),
            ["unknown-template-filter Error: no template filter named `slugify` [slugify]"]
        );
    }

    #[test]
    fn the_table_answers_in_full_where_django_cannot_be_read() {
        // this project's mock django stops at `django.templatetags`, so nothing of
        // what django itself registers was read and the table is all there is
        assert_eq!(
            project("{% lorem %}{{ book.title|slugify }}").diagnostics(),
            Vec::<String>::new()
        );
    }
}
