pub mod config;
mod reverse_transforms;
pub mod source_map;
mod transforms;
pub(crate) mod type_info;

pub use config::{Config, PythonVersion, SoundnessPositions};

use std::collections::{BTreeSet, HashSet};

use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::{DbWithWritableSystem as _, SystemPath, SystemPathBuf};
use ruff_diagnostics::{Edit, Fix, IsolationLevel, SourceMap};
use ruff_python_ast::Stmt;
use ruff_python_ast::visitor::Visitor;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextSize};
use salsa::Setter as _;
use ty_project::{ProjectMetadata, TestDb};

/// Creates a single-file in-memory database for transpilation.
///
/// The source is registered at `/input.by`.
pub(crate) fn make_in_memory_db(source: &str) -> (TestDb, File) {
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    db.write_file("/input.by", source)
        .expect("write file failed");
    let file = system_path_to_file(&db, "/input.by").expect("file not in db");
    (db, file)
}

/// A caller-supplied capability to build a *second* database over the same
/// project as the one it hands to [`transpile_typed`].
///
/// A pre-pass — erased-union reification, name qualification, enum lowering —
/// can rewrite the source before phase 0 runs, and phase 0's type-aware passes
/// must query a db whose contents are the source they walk. Given this
/// capability the transpiler keeps the real project (its metadata, search paths
/// and sibling files) and serves only the rewritten file from memory; without
/// it, phase 0 falls back to a single-file db that resolves nothing outside the
/// file — correct, but blind to imports.
///
/// The database must be a *new* one, not a clone: salsa handles cloned from one
/// database share storage, so the source override would be visible through the
/// caller's own db.
pub type RebuildProject<'a> = &'a dyn Fn() -> Option<Box<dyn ty_python_semantic::Db>>;

/// A rebuilt project db in which one file reads as rewritten source.
struct Overlaid {
    db: Box<dyn ty_python_semantic::Db>,
    file: File,
}

/// Rebuild the project db and serve `source` as the contents of `path`, so
/// type-aware passes see the rewritten source with the project still around it.
///
/// Returns `None` when the caller supplied no [`RebuildProject`], the rebuild
/// failed, or the rebuilt db doesn't hold `path` — every one of which leaves the
/// single-file fallback in place.
fn overlay_rewritten_source(
    rebuild: Option<RebuildProject<'_>>,
    path: &SystemPath,
    source: &str,
) -> Option<Overlaid> {
    let mut db = rebuild?()?;
    let file = system_path_to_file(&*db, path).ok()?;
    let text = ruff_db::source::source_text(&*db, file)
        .with_text(source.to_owned(), &SourceMap::default());
    file.set_source_text_override(&mut *db).to(Some(text));
    Some(Overlaid { db, file })
}

/// Qualify every context-sensitively resolved name (`a: Color = Red` →
/// `Color.Red`) against the *original* source — before the enum lowering
/// rewrites `enum class` to python, so ty resolves the same source it checks.
///
/// `project`, when `Some`, supplies the real project db + file so an enum
/// imported from another module resolves. Returns a borrowed `Cow` when nothing
/// was qualified, which is also the signal that the caller's db still matches
/// the source.
fn run_context_sensitive_phase<'a>(
    source: &'a str,
    db: &dyn ty_python_semantic::Db,
    file: File,
) -> std::borrow::Cow<'a, str> {
    let parsed = ruff_db::parsed::parsed_module(db, db.program_file(file).python_file(db)).load(db);
    if !parsed.errors().is_empty() {
        return std::borrow::Cow::Borrowed(source);
    }
    let model = ty_python_semantic::SemanticModel::new(db, db.program_file(file));
    transforms::context_sensitive::qualify(source, parsed.suite(), &model)
}

/// Give every erased-union parameter (`list[int] | list[str]`) a reified type
/// parameter, so the specialization travels with the call instead of being
/// asked of a value that erased it.
///
/// Runs *before* the phase-0 AST passes, like the qualification phase above and
/// for the same reason: phase 0 re-parses and re-infers its input, so the
/// passes that act on the rewrite see it as ordinary source. Edits stay within
/// the def header and the annotations, so line numbering is unaffected.
fn run_erased_union_phase<'a>(
    source: &'a str,
    db: &dyn ty_python_semantic::Db,
    file: File,
    config: &Config,
) -> std::borrow::Cow<'a, str> {
    let parsed = ruff_db::parsed::parsed_module(db, db.program_file(file).python_file(db)).load(db);
    if !parsed.errors().is_empty() {
        return std::borrow::Cow::Borrowed(source);
    }
    let model = ty_python_semantic::SemanticModel::new(db, db.program_file(file));
    transforms::erased_union::reify(source, parsed.suite(), &model, config.min_version)
}

/// Transpile `.by` source text to python without a project db (single-file:
/// type-aware passes see only this file). Used for stdin input and tests; the
/// file-backed [`transpile_typed`] resolves cross-module types.
pub fn transpile(source: &str, config: &Config) -> Result<String, String> {
    transpile_with_report(source, config).map(|(output, _)| output)
}

/// Like [`transpile`], and also reports what the emitted python needs installed
/// to run.
pub fn transpile_with_report(
    source: &str,
    config: &Config,
) -> Result<(String, RuntimeRequirements), String> {
    if config.is_python {
        return Ok((source.to_owned(), RuntimeRequirements::default()));
    }

    // one db over the original source, shared by the qualification phase below
    // and — as long as nothing rewrites the source — by phase 0's type-aware
    // passes, which would otherwise build an identical one of their own
    let (local_db, local_file) = make_in_memory_db(source);

    // --- Erased-union reification: give a `list[int] | list[str]` parameter a
    // reified type parameter, while the source is still the one ty checks ---
    let reified = run_erased_union_phase(source, &local_db, local_file, config);
    let reified_changed = matches!(reified, std::borrow::Cow::Owned(_));
    let source = reified.as_ref();

    // that rewrite edits signatures, so the qualification phase below needs a
    // db over what it is actually editing rather than the pre-rewrite source
    let rebuilt = reified_changed.then(|| make_in_memory_db(source));
    let (qualify_db, qualify_file) = match &rebuilt {
        Some((db, file)) => (db as &dyn ty_python_semantic::Db, *file),
        None => (&local_db as &dyn ty_python_semantic::Db, local_file),
    };

    // --- Context-sensitive resolution: qualify names resolved against their
    // expected type (`a: Color = Red` → `Color.Red`) while the source is still
    // the one ty checks ---
    let qualified = run_context_sensitive_phase(source, qualify_db, qualify_file);
    let qualified_changed = matches!(qualified, std::borrow::Cow::Owned(_));
    let source = qualified.as_ref();

    // --- Enum lowering: rewrite `enum` sum types to Python before the main
    // pipeline, so member bodies (copied verbatim) are lowered downstream ---
    let enum_lowered = transforms::enums::lower(source, config.min_version);
    if let Some(first) = enum_lowered.errors.first() {
        return Err(first.clone());
    }
    let source = enum_lowered.output.as_ref();

    // --- Phase 0: AST rewrite passes ---
    let unchanged = !reified_changed
        && !qualified_changed
        && matches!(enum_lowered.output, std::borrow::Cow::Borrowed(_));
    let (source, ast_errors, _phase0_map) = transforms::ast_driver::run_against_source(
        source,
        config,
        unchanged.then_some((&local_db as &dyn ty_python_semantic::Db, local_file)),
    );
    if let Some(first) = ast_errors.first() {
        return Err(first.clone());
    }
    let source = source.as_ref();

    // --- Phase 1: basedpython lowering ---
    let (db, file) = make_in_memory_db(source);
    let source_ref = ruff_db::source::source_text(&db, file);
    let src = source_ref.as_str();
    let module = ruff_db::parsed::parsed_module(
        &db,
        ty_python_semantic::Db::program_file(&db, file).python_file(&db),
    )
    .load(&db);
    let model = ty_python_semantic::SemanticModel::new(
        &db,
        ty_python_semantic::Db::program_file(&db, file),
    );
    if let Some(err) = module.errors().iter().find(|e| e.is_basedpython_only()) {
        return Err(err.to_string());
    }
    let LoweringResult { output, errors } = run_lowering_phase(src, module.suite(), config);
    if let Some(first) = errors.first() {
        return Err(first.clone());
    }

    // --- Phase 2: import-redirect, surface-syntax cleanup, lazy-import marking ---
    let (final_output, requirements) = run_import_redirect_phase(output, config);
    let final_output = run_anon_named_tuple_cleanup(final_output, config)?;
    let final_output = run_lazy_import_phase(
        final_output,
        config,
        &model.eagerly_imported_modules(),
        &model.eagerly_imported_names(),
    );
    let final_output = run_version_polyfill_phase(final_output, config);

    // --- Phase 3: syntax verification ---
    verify_syntax(&final_output).map_err(|e| e.message)?;
    verify_target_syntax(&final_output, config).map_err(|e| e.message)?;

    Ok((final_output, requirements))
}

/// Transpile using ty's full type inference. `db` and `file` must already
/// have semantic analysis available (i.e. the file is indexed in the project).
///
/// Pipeline:
/// 1. **Lowering phase** — uses the supplied db (with salsa cache). Runs all
///    basedpython→python transforms; produces python source plus the preamble.
/// 2. **Import-redirect phase** — fresh in-memory db against the lowering
///    output. Rewrites `from typing import X` to `from typing_extensions import X`
///    where X is not yet in stdlib at the configured min version.
/// 3. **Syntax verification** — final parse to catch structural errors.
pub fn transpile_typed(
    db: &dyn ty_python_semantic::Db,
    file: File,
    config: &Config,
    rebuild: Option<RebuildProject<'_>>,
) -> Result<String, TranspileError> {
    transpile_typed_with_map(db, file, config, rebuild).map(|(out, _)| out)
}

/// Like [`transpile_typed`] but also returns a line table mapping each output
/// line (0-indexed) back to the originating `.by` line, or `None` for generated
/// lines (preambles, synthesized classes). Used by `by run` to rewrite runtime
/// tracebacks into `.by` coordinates.
///
/// On failure, [`TranspileError::output_range`] (a span in the generated python)
/// is mapped back to the originating `.by` range here, so the caller can render
/// a source-annotated diagnostic.
pub fn transpile_typed_with_map(
    db: &dyn ty_python_semantic::Db,
    file: File,
    config: &Config,
    rebuild: Option<RebuildProject<'_>>,
) -> Result<(String, Vec<Option<u32>>), TranspileError> {
    transpile_typed_with_report(db, file, config, rebuild)
        .map(|(output, line_map, _)| (output, line_map))
}

/// What the emitted python needs at run time that the standard library does not
/// provide.
///
/// Lowering for an older python can put a name in the output that only
/// `typing_extensions` has there — `Self` on 3.9, say. That is a real dependency
/// of the built artifact, and nothing in the source says so, which is why the
/// transpile is what reports it: a wheel that shipped without it would install
/// cleanly and fail on the first import.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRequirements {
    typing_extensions: bool,
}

/// What the emitted python needs when lowering reached for `typing_extensions`.
///
/// One floor for every name rather than one per name: which release of
/// `typing_extensions` first carried each backport is not something the emitted
/// code records, and a floor too low is a wheel that installs and then fails on
/// an import. It has to cover every name
/// [`ty_python_semantic::basedpython_typing_added_in`] can redirect, which is why
/// it lives beside the pass that does the redirecting rather than beside the
/// command that reports it.
const TYPING_EXTENSIONS_REQUIREMENT: &str = "typing_extensions>=4.12";

impl RuntimeRequirements {
    /// Fold in what another module needed.
    pub fn merge(&mut self, other: Self) {
        self.typing_extensions |= other.typing_extensions;
    }

    /// The requirements, spelled the way a `[project] dependencies` entry is.
    ///
    /// A list rather than a set of flags, so that a second requirement is a line
    /// here instead of an edit at every place a caller asks what is needed.
    pub fn specifiers(self) -> Vec<&'static str> {
        let mut specifiers = Vec::new();
        if self.typing_extensions {
            specifiers.push(TYPING_EXTENSIONS_REQUIREMENT);
        }
        specifiers
    }
}

/// Like [`transpile_typed_with_map`], and also reports what the emitted python
/// needs installed to run.
pub fn transpile_typed_with_report(
    db: &dyn ty_python_semantic::Db,
    file: File,
    config: &Config,
    rebuild: Option<RebuildProject<'_>>,
) -> Result<(String, Vec<Option<u32>>, RuntimeRequirements), TranspileError> {
    let source_ref = ruff_db::source::source_text(db, file);
    let original_source = source_ref.as_str();

    if config.is_python {
        let out = original_source.to_owned();
        return Ok((
            out,
            source_map::line_table(original_source, &[]),
            RuntimeRequirements::default(),
        ));
    }

    // erased-union reification: give a `list[int] | list[str]` parameter a
    // reified type parameter, against the source ty checks. edits stay inside
    // the def header and the annotations, so line correspondence is unaffected
    let reified = run_erased_union_phase(original_source, db, file, config);
    let reified_changed = matches!(reified, std::borrow::Cow::Owned(_));

    // that rewrite edits signatures, so qualification needs a db over what it
    // is actually editing rather than the project file
    let rebuilt = reified_changed.then(|| make_in_memory_db(reified.as_ref()));
    let (qualify_db, qualify_file) = match &rebuilt {
        Some((rebuilt_db, rebuilt_file)) => {
            (rebuilt_db as &dyn ty_python_semantic::Db, *rebuilt_file)
        }
        None => (db, file),
    };

    // context-sensitive resolution: qualify names resolved against their expected
    // type (`a: Color = Red` → `Color.Red`) against the source ty checks, before
    // the enum lowering rewrites it. a within-line rewrite, so line
    // correspondence is unaffected
    let qualified = run_context_sensitive_phase(reified.as_ref(), qualify_db, qualify_file);
    let qualified_changed = matches!(qualified, std::borrow::Cow::Owned(_));

    // enum lowering: rewrite `enum` sum types to Python first. when it fires,
    // the working source differs from the project file, so type-aware passes
    // and the final lowering run against a single-file db built from it
    let enum_lowered = transforms::enums::lower(qualified.as_ref(), config.min_version);
    if let Some(first) = enum_lowered.errors.first() {
        return Err(first.clone().into());
    }
    let working_source = enum_lowered.output.as_ref();
    // two independent facts, deliberately kept apart: `enum_changed` says the
    // enum phase *renumbered lines*, so the final map must compose through its
    // line map (which is empty when it didn't fire, and would map every line to
    // nothing). `source_changed` says the working source no longer matches the
    // project file, so a single-file db is needed. qualification changes the
    // second without changing the first — it only ever edits within a line
    let enum_changed = matches!(enum_lowered.output, std::borrow::Cow::Owned(_));
    let source_changed = reified_changed || qualified_changed || enum_changed;

    // phase 0: AST passes. type-aware passes resolve cross-module imports from
    // the project db; when a pre-pass rewrote the source, that db is rebuilt
    // over the rewritten text rather than given up, so an enum or a qualified
    // name elsewhere in the file doesn't blind them. `phase0_map` maps spliced
    // lines → working (post-enum) lines
    let overlaid = if source_changed {
        file.path(db)
            .as_system_path()
            .and_then(|path| overlay_rewritten_source(rebuild, path, working_source))
    } else {
        None
    };
    let project = match &overlaid {
        Some(overlaid) => Some((&*overlaid.db, overlaid.file)),
        None if source_changed => None,
        None => Some((db, file)),
    };
    // which imports must stay eager, computed against the *project* db: a
    // single-file db cannot resolve the modules that declare the conformances
    let eager_model = ty_python_semantic::SemanticModel::new(db, db.program_file(file));
    let eager_imports = eager_model.eagerly_imported_modules();
    let eager_names = eager_model.eagerly_imported_names();
    let (spliced, ast_errors, phase0_map) =
        transforms::ast_driver::run_against_source(working_source, config, project);
    if let Some(first) = ast_errors.first() {
        return Err(first.clone().into());
    }
    let spliced_lines = line_count(spliced.as_ref());
    let (output, errors) = if let std::borrow::Cow::Owned(modified) = spliced {
        let (local_db, local_file) = make_in_memory_db(&modified);
        let local_source_ref = ruff_db::source::source_text(&local_db, local_file);
        let src = local_source_ref.as_str();
        let module = ruff_db::parsed::parsed_module(
            &local_db,
            ty_python_semantic::Db::program_file(&local_db, local_file).python_file(&local_db),
        )
        .load(&local_db);
        let LoweringResult { output, errors } = run_lowering_phase(src, module.suite(), config);
        (output, errors)
    } else if source_changed {
        // ast_driver made no further changes, but the working source differs
        // from the project file — parse it in a single-file db
        let (local_db, local_file) = make_in_memory_db(working_source);
        let local_source_ref = ruff_db::source::source_text(&local_db, local_file);
        let src = local_source_ref.as_str();
        let module = ruff_db::parsed::parsed_module(
            &local_db,
            ty_python_semantic::Db::program_file(&local_db, local_file).python_file(&local_db),
        )
        .load(&local_db);
        let LoweringResult { output, errors } = run_lowering_phase(src, module.suite(), config);
        (output, errors)
    } else {
        let module =
            ruff_db::parsed::parsed_module(db, db.program_file(file).python_file(db)).load(db);
        let LoweringResult { output, errors } =
            run_lowering_phase(original_source, module.suite(), config);
        (output, errors)
    };
    if let Some(first) = errors.first() {
        return Err(first.clone().into());
    }

    let (final_output, requirements) = run_import_redirect_phase(output, config);
    let final_output = run_anon_named_tuple_cleanup(final_output, config)?;
    let final_output = run_lazy_import_phase(final_output, config, &eager_imports, &eager_names);
    let final_output = run_version_polyfill_phase(final_output, config);

    // phases 1-2c only prepend preambles at the top and edit within lines, so
    // the spliced body keeps its line correspondence: prepend one `None` per
    // generated leading line to lift `phase0_map` into final-output coordinates.
    // when the enum phase fired, also compose `working → original .by` lines.
    //
    // both sides are counted in lines rather than `\n`s, and deliberately: a
    // phase between the two that only supplied a missing final terminator has
    // added no line, and counting terminators would read that as one more
    // prepended line and shift every entry after the preamble by one
    let prepended = line_count(&final_output).saturating_sub(spliced_lines);
    let composed: Vec<Option<u32>> = if enum_changed {
        phase0_map
            .into_iter()
            .map(|m| {
                m.and_then(|working_line| {
                    enum_lowered
                        .line_map
                        .get(working_line as usize)
                        .copied()
                        .flatten()
                })
            })
            .collect()
    } else {
        phase0_map
    };
    // the module docstring stays first, so the generated lines land after it —
    // its lines keep their own mapping ahead of the `None`s
    let kept =
        newline_count(&final_output[..transforms::source_util::docstring_end(&final_output)])
            .min(composed.len());
    let mut line_map: Vec<Option<u32>> = Vec::with_capacity(prepended + composed.len());
    line_map.extend(composed[..kept].iter().copied());
    line_map.extend(std::iter::repeat_n(None, prepended));
    line_map.extend(composed[kept..].iter().copied());

    // verify last: on failure, map the generated span back to a `.by` range
    let verified =
        verify_syntax(&final_output).and_then(|()| verify_target_syntax(&final_output, config));
    if let Err(mut err) = verified {
        err.by_range = err.output_range.and_then(|r| {
            output_offset_to_by_range(&line_map, &final_output, original_source, r.start())
        });
        return Err(err);
    }

    Ok((final_output, line_map, requirements))
}

fn newline_count(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
}

/// How many lines `s` has, which is how many entries a line table for it holds.
///
/// One more than [`newline_count`] when the last line has no terminator, because
/// that line exists all the same — python numbers it and a traceback names it.
/// Empty text has no lines at all, as distinct from one blank one.
fn line_count(s: &str) -> usize {
    newline_count(s) + usize::from(!(s.is_empty() || s.ends_with('\n')))
}

/// Re-runs the anon-named-tuple lowering on post-transform output to catch
/// expressions that other transforms (e.g. the PEP-695 polyfill) copied
/// verbatim from the source after the original pass ran
fn run_anon_named_tuple_cleanup(mut source: String, config: &Config) -> Result<String, String> {
    use ruff_python_ast::visitor::Visitor;

    for _ in 0..4 {
        let (db, file) = make_in_memory_db(&source);
        let source_ref = ruff_db::source::source_text(&db, file);
        let src = source_ref.as_str();
        let module = ruff_db::parsed::parsed_module(
            &db,
            ty_python_semantic::Db::program_file(&db, file).python_file(&db),
        )
        .load(&db);
        let model = ty_python_semantic::SemanticModel::new(
            &db,
            ty_python_semantic::Db::program_file(&db, file),
        );

        let mut anon =
            transforms::anon_named_tuple::AnonNamedTuple::new(src, &model, config.clone());
        for stmt in module.suite() {
            anon.visit_stmt(stmt);
        }
        if let Some(err) = anon.errors.first() {
            return Err(err.clone());
        }
        let protocol = transforms::protocol_type::cleanup(src, &model, module.suite(), config)?;

        if anon.edits.is_empty() && !anon.needs_import && protocol.is_none() {
            return Ok(source);
        }
        let mut preamble = String::new();
        if anon.needs_import {
            // an earlier run over the pre-splice source already emitted a class
            // (and its imports) for an anon-NT whose edit an AST pass then
            // re-rendered away, so only what the spliced output is actually
            // missing gets prepended. matched as a run of whole lines, the way
            // the inline-protocol cleanup does
            let source_lines: Vec<&str> = source.lines().collect();
            let push_missing = |preamble: &mut String, entry: &str| {
                let entry_lines: Vec<&str> = entry.lines().collect();
                let present = !entry_lines.is_empty()
                    && source_lines
                        .windows(entry_lines.len())
                        .any(|window| window == entry_lines.as_slice());
                if !present {
                    preamble.push_str(entry);
                    preamble.push('\n');
                }
            };
            push_missing(&mut preamble, "from typing import NamedTuple");
            for line in anon.callable.take_import_lines() {
                push_missing(&mut preamble, &line);
            }
            for defs in [anon.callable.class_defs().to_owned(), anon.class_defs()] {
                for class_def in defs.split_inclusive("\n\n") {
                    push_missing(&mut preamble, class_def.trim_end_matches('\n'));
                }
            }
        }
        let mut edits = anon.edits;
        if let Some((protocol_edits, protocol_preamble)) = protocol {
            preamble.push_str(&protocol_preamble);
            edits.extend(protocol_edits);
        }

        let (body, _) = apply_transforms_once(src, edits);
        source = if preamble.is_empty() {
            body
        } else {
            splice_preamble(&body, &preamble)
        };

        let _ = config;
    }
    Ok(source)
}

/// Rewrite stdlib imports to `typing_extensions` where the imported name is
/// not yet available at the configured min version
fn run_import_redirect_phase(source: String, config: &Config) -> (String, RuntimeRequirements) {
    let (db, file) = make_in_memory_db(&source);
    let source_ref = ruff_db::source::source_text(&db, file);
    let src = source_ref.as_str();
    let module = ruff_db::parsed::parsed_module(
        &db,
        ty_python_semantic::Db::program_file(&db, file).python_file(&db),
    )
    .load(&db);

    let mut typing_redirect = transforms::typing_redirect::TypingRedirect::new(src, config.clone());
    for stmt in module.suite() {
        typing_redirect.visit_stmt(stmt);
    }

    if typing_redirect.edits.is_empty() {
        return (source, RuntimeRequirements::default());
    }

    let (output, _) = apply_transforms_once(src, typing_redirect.edits);
    (
        output,
        RuntimeRequirements {
            typing_extensions: true,
        },
    )
}

/// Lazy-import marking phase: walks the post-typing-redirect output and
/// prepends `lazy ` (PEP 810, Python 3.15+) to every `import` and
/// `from import` statement. Skips `from __future__` and star imports.
///
/// Gated on `min_version >= 3.15`: PEP 810 syntax doesn't parse on older
/// Python, so we leave imports eager when the target version can't handle
/// the keyword. A redundant `lazy` keyword written in source is stripped
/// in that case so the output stays valid
///
/// `eager` names the modules that must not be deferred whatever the target: a
/// module declaring a conformance exists at runtime only because its
/// registration ran, so deferring it defers the conformance out of existence
///
/// `eager_names` names the *bindings* that must be bound to the real object: a
/// lazy proxy cannot stand where cpython checks for a real class, which is what
/// `except` does
fn run_lazy_import_phase(
    source: String,
    config: &Config,
    eager: &[String],
    eager_names: &[String],
) -> String {
    if !config.lazy_imports {
        return source;
    }

    let (db, file) = make_in_memory_db(&source);
    let source_ref = ruff_db::source::source_text(&db, file);
    let src = source_ref.as_str();
    let module = ruff_db::parsed::parsed_module(
        &db,
        ty_python_semantic::Db::program_file(&db, file).python_file(&db),
    )
    .load(&db);

    let keyword_supported = config.min_version >= ruff_python_ast::PythonVersion::from((3, 15));
    let mut lazy =
        transforms::lazy_import::LazyImport::new(src, keyword_supported, eager, eager_names);
    for stmt in module.suite() {
        lazy.visit_stmt(stmt);
    }
    let needs_module = lazy.needs_module_helper;
    let needs_attr = lazy.needs_attr_helper;
    let needs_ty_ext = lazy.needs_ty_ext_marker;
    let needs_character_class = lazy.needs_character_class;

    let preamble = transforms::lazy_import::polyfill_preamble(
        needs_module,
        needs_attr,
        needs_ty_ext,
        needs_character_class,
    );
    if lazy.edits.is_empty() && preamble.is_empty() {
        return source;
    }

    let (body, _) = apply_transforms_once(src, lazy.edits);
    if preamble.is_empty() {
        body
    } else {
        splice_preamble(&body, &preamble)
    }
}

/// Version polyfill phase: rewrite syntax the target python cannot parse into
/// syntax it can. Runs over the finished python rather than over `.by`, so a
/// `match` an earlier lowering *generated* — for a `let` destructuring, an
/// `if let`, a statement expression — is lowered by the same code as one the
/// author wrote.
///
/// Nothing here changes how many lines the file has: the polyfill rewrites
/// headers in place and pads them back to their original height, so the only
/// lines it adds are the runtime preamble's, at the top, where the line map
/// already accounts for generated leading lines.
fn run_version_polyfill_phase(source: String, config: &Config) -> String {
    transforms::match_polyfill::lower(source, config.min_version)
}

/// Re-parse the transpiled output *as the target python version* and report any
/// construct that version cannot parse.
///
/// [`verify_syntax`] asks whether the output is python at all; this asks whether
/// it is python the file's declared floor can run. Without it, syntax no
/// polyfill covers — `except*`, t-strings, a PEP 701 f-string — is emitted
/// verbatim and fails at import time in generated code the author never wrote.
fn verify_target_syntax(source: &str, config: &Config) -> Result<(), TranspileError> {
    let options = ruff_python_parser::ParseOptions::from(ruff_python_ast::PySourceType::Python)
        .with_target_version(config.min_version);
    let parsed = ruff_python_parser::parse_unchecked(source, options);
    let Some(first) = parsed.unsupported_syntax_errors().first() else {
        return Ok(());
    };
    // the parser's own wording already names both versions: "Cannot use
    // `except*` on Python 3.9 (syntax was added in Python 3.11)"
    Err(TranspileError {
        message: first.to_string(),
        output_range: Some(first.range),
        by_range: None,
    })
}

/// Splice `preamble` into `body` where generated lines belong: after the module
/// docstring and any `from __future__ import`, each of which is only valid
/// where it already is.
fn splice_preamble(body: &str, preamble: &str) -> String {
    let at = transforms::source_util::preamble_offset(body);
    format!("{}{preamble}{}", &body[..at], &body[at..])
}

/// Re-parse the transpiled source as **python** (`.py`) and surface any parse
/// errors as a transpile failure. We never want to emit invalid Python: this
/// guards against both structural malformations from buggy edits (unbalanced
/// brackets, truncated strings) and any leftover basedpython surface syntax
/// that some transform forgot to lower.
///
/// In addition to parse errors, this walks the resulting AST for any
/// basedpython-only flags (`is_anon_named_tuple` / `is_anon_named_tuple_value`)
/// since the unified parser accepts those — a leftover flag in the output
/// means a transform passed responsibility for invalid Python down the chain
/// and we abort here.
/// True when the user source already imports `annotations` from
/// `__future__`, so the lowering doesn't emit a duplicate
fn has_future_annotations(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| {
        let Stmt::ImportFrom(node) = s else {
            return false;
        };
        node.module.as_deref() == Some("__future__")
            && node
                .names
                .iter()
                .any(|alias| alias.name.as_str() == "annotations")
    })
}

/// A transpile failure. `message` is human-facing and free of internal
/// artifacts (no "byte range"). `output_range` is the span in the *generated*
/// python that triggered the failure; `by_range` is that span mapped back to
/// the originating `.by` source, which callers use to render a source-annotated
/// diagnostic.
#[derive(Debug, Clone)]
pub struct TranspileError {
    pub message: String,
    pub output_range: Option<ruff_text_size::TextRange>,
    pub by_range: Option<ruff_text_size::TextRange>,
}

impl std::fmt::Display for TranspileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for TranspileError {
    fn from(message: String) -> Self {
        Self {
            message,
            output_range: None,
            by_range: None,
        }
    }
}

fn verify_syntax(source: &str) -> Result<(), TranspileError> {
    use ruff_python_ast::{PySourceType, visitor::Visitor};

    let parsed = ruff_python_parser::parse_unchecked_source(source, PySourceType::Python);
    let parse_errors = parsed.errors();
    if let Some(first) = parse_errors.first() {
        if std::env::var_os("BY_TRANSPILE_DEBUG").is_some() {
            #[expect(clippy::print_stderr, reason = "opt-in debug dump behind an env var")]
            {
                eprintln!("=== INVALID TRANSPILED OUTPUT ===\n{source}\n=== END ===");
            }
        }
        // `first.error` is the clean message; the full `Display` would append
        // "at byte range …" which is meaningless to the user
        return Err(TranspileError {
            message: format!("transpiler produced invalid Python: {}", first.error),
            output_range: Some(first.location),
            by_range: None,
        });
    }

    #[expect(clippy::items_after_statements, reason = "scanner colocated with use")]
    struct AnonNamedTupleScanner {
        leftover_range: Option<ruff_text_size::TextRange>,
        leftover_typeof: Option<ruff_text_size::TextRange>,
    }
    #[expect(clippy::items_after_statements, reason = "scanner colocated with use")]
    impl<'ast> Visitor<'ast> for AnonNamedTupleScanner {
        fn visit_expr(&mut self, expr: &'ast ruff_python_ast::Expr) {
            if self.leftover_range.is_some() || self.leftover_typeof.is_some() {
                return;
            }
            if let ruff_python_ast::Expr::Tuple(t) = expr {
                if t.is_anon_named_tuple || t.is_anon_named_tuple_value {
                    self.leftover_range = Some(<ruff_python_ast::ExprTuple as Ranged>::range(t));
                    return;
                }
            }
            if let ruff_python_ast::Expr::Subscript(s) = expr {
                if s.is_typeof {
                    self.leftover_typeof =
                        Some(<ruff_python_ast::ExprSubscript as Ranged>::range(s));
                    return;
                }
            }
            ruff_python_ast::visitor::walk_expr(self, expr);
        }
    }

    let mut scanner = AnonNamedTupleScanner {
        leftover_range: None,
        leftover_typeof: None,
    };
    for stmt in parsed.suite() {
        scanner.visit_stmt(stmt);
        if scanner.leftover_range.is_some() || scanner.leftover_typeof.is_some() {
            break;
        }
    }
    if let Some(range) = scanner.leftover_range {
        let snippet = &source[usize::from(range.start())..usize::from(range.end())];
        return Err(TranspileError {
            message: format!("transpiler failed to lower anonymous named tuple syntax `{snippet}`"),
            output_range: Some(range),
            by_range: None,
        });
    }
    if let Some(range) = scanner.leftover_typeof {
        let snippet = &source[usize::from(range.start())..usize::from(range.end())];
        return Err(TranspileError {
            message: format!("transpiler failed to lower `typeof` syntax `{snippet}`"),
            output_range: Some(range),
            by_range: None,
        });
    }

    // a `.by` source holds back python's two `match` checks, because a bare
    // `case A:` name may be an enum member of the subject rather than the
    // capture it looks like. by here every name is spelled out, so the checks
    // apply again — and a `match` that fails them is python that will not parse
    if let Some(error) = first_invalid_match_statement(parsed.suite()) {
        return Err(TranspileError {
            message: format!("transpiler produced invalid Python: {error}"),
            output_range: Some(error.range),
            by_range: None,
        });
    }

    Ok(())
}

/// The first `match` in `suite` that python's own parse-time checks reject.
fn first_invalid_match_statement(
    suite: &[ruff_python_ast::Stmt],
) -> Option<ruff_python_parser::semantic_errors::SemanticSyntaxError> {
    use ruff_python_ast::visitor::{Visitor, walk_stmt};
    use ruff_python_parser::semantic_errors::{SemanticSyntaxChecker, SemanticSyntaxError};

    struct Scanner {
        error: Option<SemanticSyntaxError>,
    }
    impl<'ast> Visitor<'ast> for Scanner {
        fn visit_stmt(&mut self, stmt: &'ast ruff_python_ast::Stmt) {
            if self.error.is_some() {
                return;
            }
            if let ruff_python_ast::Stmt::Match(match_stmt) = stmt {
                self.error = SemanticSyntaxChecker::python_match_statement_errors(
                    match_stmt,
                    // the checks are version-independent; `match` itself needs
                    // 3.10, which the parse above has already settled
                    ruff_python_ast::PythonVersion::latest(),
                )
                .into_iter()
                .next();
                if self.error.is_some() {
                    return;
                }
            }
            walk_stmt(self, stmt);
        }
    }

    let mut scanner = Scanner { error: None };
    for stmt in suite {
        scanner.visit_stmt(stmt);
        if scanner.error.is_some() {
            break;
        }
    }
    scanner.error
}

/// Map a byte offset in the generated python to a byte range in the original
/// `.by` source, via the line table. Returns the full `.by` line's range (line
/// granularity is all the table provides today; see the sourcemap plan).
fn output_offset_to_by_range(
    line_map: &[Option<u32>],
    final_output: &str,
    by_source: &str,
    output_offset: ruff_text_size::TextSize,
) -> Option<ruff_text_size::TextRange> {
    use ruff_text_size::{TextRange, TextSize};

    let output_line =
        newline_count(&final_output[..usize::from(output_offset).min(final_output.len())]);
    let by_line = (*line_map.get(output_line)?)? as usize;

    // the first line starts after any BOM: the diagnostic renderer strips one
    // from the snippet it prints, so an annotation that covered it would run
    // past the end of that buffer
    let mut start = usize::from(by_source.bom_start_offset());
    for _ in 0..by_line {
        start = by_source[start..].find('\n').map(|i| start + i + 1)?;
    }
    let end = by_source[start..]
        .find('\n')
        .map_or(by_source.len(), |i| start + i);
    Some(TextRange::new(
        TextSize::try_from(start).ok()?,
        TextSize::try_from(end).ok()?,
    ))
}

/// Result of phase 1 (basedpython lowering)
pub(crate) struct LoweringResult {
    /// The full transformed source: preamble + body
    pub(crate) output: String,
    /// Hard transpile errors collected from individual transforms — abort the
    /// pipeline rather than emit partial / invalid output
    pub(crate) errors: Vec<String>,
}

/// Every basedpython transform now runs in `ast_driver`; this phase only
/// prepends the opt-in `from __future__ import annotations` preamble when
/// `inject_future_annotations` is set (off by default — forward references
/// are quoted surgically by `auto_quote` instead)
fn run_lowering_phase(source: &str, stmts: &[Stmt], config: &Config) -> LoweringResult {
    let mut output = String::new();
    // a BOM stays at offset 0 — moved off it by the preamble it is no longer a
    // BOM, just a character python refuses to tokenize
    let (bom, source) = source.split_at(usize::from(source.bom_start_offset()));
    output.push_str(bom);
    // below 3.10 the runtime cannot evaluate pep 604 `X | Y` annotations —
    // which the optional lowering itself produces — so annotation evaluation
    // must always be deferred on those targets
    let needs_lazy_annotations =
        config.inject_future_annotations || config.min_version < PythonVersion::PY310;
    if needs_lazy_annotations && !config.is_stub && !has_future_annotations(stmts) {
        output.push_str("from __future__ import annotations\n");
    }
    output.push_str(source);

    LoweringResult {
        output,
        errors: Vec::new(),
    }
}

/// Rewrite standard Python source into idiomatic basedpython.
///
/// Counterpart to [`transpile`]: detects polyfill output patterns and
/// rewrites them to the basedpython surface form. Used for ecosystem
/// round-trip testing — `transpile(reverse_transpile(py))` should produce
/// AST-equivalent code to `transpile(py)`.
pub fn reverse_transpile(source: &str, config: &Config) -> Result<String, String> {
    let (db, file) = make_in_memory_db(source);
    let source_ref = ruff_db::source::source_text(&db, file);
    let src = source_ref.as_str();
    let module = ruff_db::parsed::parsed_module(
        &db,
        ty_python_semantic::Db::program_file(&db, file).python_file(&db),
    )
    .load(&db);
    let model = ty_python_semantic::SemanticModel::new(
        &db,
        ty_python_semantic::Db::program_file(&db, file),
    );

    let mut super_kw_rev = reverse_transforms::super_keyword::SuperKeywordReverse::new(src);
    let mut anon_named_tuple_rev =
        reverse_transforms::anon_named_tuple::AnonNamedTupleReverse::new(src, module.suite());
    let mut empty_decls =
        reverse_transforms::empty_declarations::EmptyDeclarations::new(config.is_stub);
    let mut literal_types = reverse_transforms::literal_types::LiteralReverse::new(src, &model);
    let mut subscript = reverse_transforms::subscript::SubscriptReverse::new(src, &model);
    let mut indent_string = reverse_transforms::dedent_string::IndentString::new(src);
    let mut type_mapping = reverse_transforms::type_mapping::TypeMappingReverse::new();
    let mut callable = {
        let c = reverse_transforms::callable::CallableReverse::new(src, &model);
        if config.is_stub { c.stub() } else { c }
    };
    let mut intersection = reverse_transforms::intersection::IntersectionReverse::new(src, &model);
    let mut not_rev = reverse_transforms::not_type::NotTypeReverse::new(src, &model);
    let mut decorated_type_rev =
        reverse_transforms::decorated_type::DecoratedTypeReverse::new(src, &model);
    let mut dynamic_keyword_rev =
        reverse_transforms::dynamic_keyword::DynamicKeywordReverse::new(&model);
    let mut literal_string_rev =
        reverse_transforms::literal_string::LiteralStringReverse::new(&model);
    let mut type_is_rev = reverse_transforms::type_is::TypeIsReverse::new(src, &model);
    let mut identity_rev = reverse_transforms::identity_swap::IdentitySwapReverse::new(src);
    let mut tuple_type = reverse_transforms::tuple_type::TupleTypeReverse::new(src, &model);
    let mut unpack = reverse_transforms::unpack::UnpackReverse::new(src, &model);
    let mut overload = reverse_transforms::overload::OverloadReverse::new(src);
    let mut modifiers_rev = reverse_transforms::modifiers::ModifiersReverse::new(src);
    let mut enums_rev = reverse_transforms::enums::EnumsReverse::new();
    let mut extension_rev =
        reverse_transforms::extension::ExtensionReverse::new(src, module.suite());
    let mut coalesce_rev = reverse_transforms::coalesce::CoalesceReverse::new(src);
    let mut generics_rev = reverse_transforms::generics::GenericsReverse::new(src);
    let mut auto_quote_rev = reverse_transforms::auto_quote::AutoQuoteReverse::new(src);
    let mut compat_rev = reverse_transforms::compat::CompatReverse::new();
    let mut none_chain_rev = reverse_transforms::none_chain::NoneChainReverse::new(src);
    let mut reified_generic_rev =
        reverse_transforms::reified_generic::ReifiedGenericReverse::new(src);
    let mut string_tag_rev = reverse_transforms::string_tag::StringTagReverse::new(src);
    let mut unique_loop_bindings_rev =
        reverse_transforms::unique_loop_bindings::UniqueLoopBindingsReverse::new(src);
    let mut typing_redirect_rev = reverse_transforms::typing_redirect::TypingRedirectReverse::new();
    let mut export_import_rev = reverse_transforms::export_import::ExportImportReverse::new(src);

    for stmt in module.suite() {
        super_kw_rev.visit_stmt(stmt);
        anon_named_tuple_rev.visit_stmt(stmt);
        subscript.visit_stmt(stmt);
        indent_string.visit_stmt(stmt);
        type_mapping.visit_stmt(stmt);
        intersection.visit_stmt(stmt);
        not_rev.visit_stmt(stmt);
        decorated_type_rev.visit_stmt(stmt);
        dynamic_keyword_rev.visit_stmt(stmt);
        literal_string_rev.visit_stmt(stmt);
        type_is_rev.visit_stmt(stmt);
        identity_rev.visit_stmt(stmt);
        tuple_type.visit_stmt(stmt);
        unpack.visit_stmt(stmt);
        modifiers_rev.visit_stmt(stmt);
        enums_rev.visit_stmt(stmt);
        extension_rev.visit_stmt(stmt);
        coalesce_rev.visit_stmt(stmt);
        auto_quote_rev.visit_stmt(stmt);
        compat_rev.visit_stmt(stmt);
        none_chain_rev.visit_stmt(stmt);
        reified_generic_rev.visit_stmt(stmt);
        string_tag_rev.visit_stmt(stmt);
        export_import_rev.visit_stmt(stmt);
        unique_loop_bindings_rev.visit_stmt(stmt);
        // `callable` rewrites callable annotations to the arrow form. it runs
        // for stubs too, but in a restricted "stub" mode (set above) that only
        // touches the gradual `Callable[..., R]` form — the `Callable[[A, B],
        // R]` list form is left intact, since ty's native basedpython parser
        // can't carry `Unpack[Ts]`/`*Ts` through the arrow and stubs would
        // lose generic callable info
        callable.visit_stmt(stmt);
        // skip transforms that change runtime/display semantics when
        // rewriting stubs:
        //  - `literal_types` strips `Literal[...]` to bare literals, but a
        //    bare `1 | 2` in a `TypeAlias = ...` RHS evaluates at runtime
        //    as integer OR (= `3`) rather than `Literal[1, 2]`
        //  - `typing_redirect` rewrites `typing_extensions` imports, but
        //    stubs use them deliberately for version-aware re-exports
        //  - `generics` turns `X: TypeAlias = T` into PEP 695 `type X = T`,
        //    which resolves lazily and changes alias display in diagnostics
        if !config.is_stub {
            literal_types.visit_stmt(stmt);
            typing_redirect_rev.visit_stmt(stmt);
            generics_rev.visit_stmt(stmt);
        }
    }
    empty_decls.visit_body(module.suite());
    overload.visit_body(module.suite());

    let mut fixes: Vec<Fix> = Vec::new();
    fixes.extend(super_kw_rev.edits);
    fixes.extend(anon_named_tuple_rev.edits);
    fixes.extend(empty_decls.edits);
    fixes.extend(literal_types.edits);
    fixes.extend(subscript.edits);
    fixes.extend(indent_string.edits);
    fixes.extend(type_mapping.edits);
    fixes.extend(callable.edits);
    fixes.extend(intersection.edits);
    fixes.extend(not_rev.edits);
    fixes.extend(decorated_type_rev.edits);
    fixes.extend(dynamic_keyword_rev.edits);
    fixes.extend(literal_string_rev.edits);
    fixes.extend(type_is_rev.edits);
    fixes.extend(identity_rev.edits);
    fixes.extend(unpack.edits);
    fixes.extend(tuple_type.edits);
    fixes.extend(overload.edits);
    fixes.extend(modifiers_rev.edits);
    fixes.extend(enums_rev.edits);
    fixes.extend(extension_rev.edits);
    fixes.extend(coalesce_rev.edits);
    fixes.extend(generics_rev.edits);
    fixes.extend(auto_quote_rev.edits);
    fixes.extend(compat_rev.edits);
    fixes.extend(none_chain_rev.edits);
    fixes.extend(reified_generic_rev.edits);
    fixes.extend(string_tag_rev.edits);
    fixes.extend(unique_loop_bindings_rev.edits);
    fixes.extend(typing_redirect_rev.edits);
    fixes.extend(export_import_rev.edits);

    let body = apply_transforms_once(src, fixes).0;
    // most reverse transforms swap an import-backed feature (`@dataclass`,
    // `@final`, `NamedTuple` subclassing, `Callable[...]` annotations) for
    // a basedpython keyword form. the original `from typing import ...`
    // lines become dead. strip them so the produced `.by` source is clean
    if config.prune_unused_imports_after_reverse {
        Ok(reverse_transforms::prune_imports::prune_unused_imports(
            &body,
        ))
    } else {
        Ok(body)
    }
}

/// Apply fixes to source in a single forward pass, mirroring ruff's
/// `apply_fixes` algorithm. Fixes are sorted by start position; overlapping
/// fixes and isolation-group conflicts are skipped (first wins)
fn apply_transforms_once(source: &str, mut fixes: Vec<Fix>) -> (String, Vec<Edit>) {
    fixes.sort_by_key(Fix::min_start);

    let mut output = String::with_capacity(source.len());
    let mut last_pos = TextSize::default();
    let mut applied: BTreeSet<Edit> = BTreeSet::default();
    let mut isolated: HashSet<u32> = HashSet::default();
    let mut kept: Vec<Edit> = Vec::new();

    for fix in &fixes {
        let new_edits: Vec<&Edit> = fix
            .edits()
            .iter()
            .filter(|e| !applied.contains(*e))
            .collect();

        let Some(first) = new_edits.first() else {
            continue;
        };

        if let IsolationLevel::Group(id) = fix.isolation()
            && !isolated.insert(id)
        {
            continue;
        }

        if first.start() < last_pos {
            continue;
        }

        for edit in new_edits {
            output.push_str(&source[usize::from(last_pos)..usize::from(edit.start())]);
            let content = edit.content().unwrap_or_default();
            output.push_str(content);
            last_pos = edit.end();
            applied.insert(edit.clone());
            kept.push(edit.clone());
        }
    }
    output.push_str(&source[usize::from(last_pos)..]);
    (output, kept)
}

#[cfg(test)]
pub mod python_passthrough {
    use super::*;

    pub fn py(source: &str) -> String {
        transpile(
            source,
            &Config {
                is_python: true,
                ..Config::default()
            },
        )
        .unwrap()
    }

    pub fn unchanged(source: &str) {
        assert_eq!(py(source), source);
    }

    /// No-op identity helper retained for backwards compatibility with test
    /// `check` functions. The lazy-import transform only fires when
    /// `min_version >= 3.15`; tests that use `Config::default()` (3.10) get
    /// plain imports, so no adjustment is needed
    pub fn lazify_expected(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn normal_class_unchanged() {
        unchanged("class A: ...\n");
    }

    #[test]
    fn decorated_class_unchanged() {
        unchanged("@dataclass\nclass A:\n    x: int\n");
    }
}

#[cfg(test)]
mod python_parse_errors {
    use super::*;

    /// parse `source` as a `.py` file and return the parse error messages
    fn parse_errors_in_py(source: &str) -> Vec<String> {
        let mut db = ty_project::TestDb::new(ty_project::ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            ruff_db::system::SystemPathBuf::from("/"),
        ));
        db.init_program().expect("program init failed");
        db.write_file("/input.py", source)
            .expect("write file failed");
        let file = ruff_db::files::system_path_to_file(&db, "/input.py").expect("file not in db");
        let module = ruff_db::parsed::parsed_module(
            &db,
            ty_python_semantic::Db::program_file(&db, file).python_file(&db),
        )
        .load(&db);
        module.errors().iter().map(ToString::to_string).collect()
    }

    #[test]
    fn abstract_class_in_py_errors() {
        let errs = parse_errors_in_py("abstract class A: ...\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `abstract class` in .py file"
        );
        assert!(
            errs[0].contains("abstract"),
            "expected error mentioning `abstract`, got: {errs:?}"
        );
    }

    #[test]
    fn final_class_in_py_errors() {
        let errs = parse_errors_in_py("final class A: ...\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `final class` in .py file"
        );
        assert!(
            errs[0].contains("final"),
            "expected error mentioning `final`, got: {errs:?}"
        );
    }

    #[test]
    fn abstract_method_in_py_errors() {
        let errs = parse_errors_in_py("class A:\n    abstract def f(self): ...\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `abstract def` in .py file"
        );
        assert!(
            errs[0].contains("abstract"),
            "expected error mentioning `abstract`, got: {errs:?}"
        );
    }

    #[test]
    fn bare_class_in_py_errors() {
        let errs = parse_errors_in_py("class A\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for body-less class in .py file"
        );
    }

    #[test]
    fn normal_class_in_py_no_errors() {
        let errs = parse_errors_in_py("class A: ...\n");
        assert!(errs.is_empty(), "unexpected parse errors: {errs:?}");
    }

    #[test]
    fn abstract_class_in_by_no_errors() {
        // .by files: basedpython syntax is valid — no parse errors
        let (db, file) = make_in_memory_db("abstract class A: ...\n");
        let module = ruff_db::parsed::parsed_module(
            &db,
            ty_python_semantic::Db::program_file(&db, file).python_file(&db),
        )
        .load(&db);
        assert!(
            module.errors().is_empty(),
            "unexpected parse errors in .by file: {:?}",
            module.errors()
        );
    }

    #[test]
    fn optional_type_in_py_errors() {
        let errs = parse_errors_in_py("x: int?\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `T?` optional type in .py file"
        );
        assert!(
            errs[0].contains("optional/result type"),
            "expected error mentioning optional/result type, got: {errs:?}"
        );
    }

    #[test]
    fn result_type_in_py_errors() {
        let errs = parse_errors_in_py("x: int ? ValueError\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `T ? E` result type in .py file"
        );
    }

    #[test]
    fn propagate_operator_in_py_errors() {
        let errs = parse_errors_in_py("x = foo()^\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `^` propagate in .py file"
        );
        assert!(
            errs[0].contains("propagate"),
            "expected error mentioning propagate, got: {errs:?}"
        );
    }

    #[test]
    fn force_unwrap_in_py_errors() {
        let errs = parse_errors_in_py("x = foo()!\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `!` force-unwrap in .py file"
        );
        assert!(
            errs[0].contains("force-unwrap"),
            "expected error mentioning force-unwrap, got: {errs:?}"
        );
    }

    #[test]
    fn xor_still_valid_in_py() {
        // a real bitwise-xor expression must NOT be flagged as basedpython-only
        let errs = parse_errors_in_py("x = a ^ b\n");
        assert!(errs.is_empty(), "unexpected parse errors: {errs:?}");
    }

    #[test]
    fn checked_cast_in_py_errors() {
        let errs = parse_errors_in_py("b = a cast? int\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `cast?` in .py file"
        );
        assert!(
            errs.iter().any(|e| e.contains("cast?")),
            "expected error mentioning `cast?`, got: {errs:?}"
        );
    }

    #[test]
    fn bang_cast_in_py_errors() {
        let errs = parse_errors_in_py("b = a cast! int\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `cast!` in .py file"
        );
        assert!(
            errs.iter().any(|e| e.contains("cast!")),
            "expected error mentioning `cast!`, got: {errs:?}"
        );
    }

    #[test]
    fn plain_cast_still_valid_in_py_after_checked() {
        // `cast` (the identifier / regular call) must not be mistaken for `cast?`
        let errs = parse_errors_in_py("cast = 5\nb = cast(int, a)\n");
        assert!(errs.is_empty(), "unexpected parse errors: {errs:?}");
    }

    #[test]
    fn sentinel_in_py_errors() {
        let errs = parse_errors_in_py("sentinel A\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for `sentinel` declaration in .py file"
        );
        assert!(
            errs[0].contains("sentinel"),
            "expected error mentioning `sentinel`, got: {errs:?}"
        );
    }

    #[test]
    fn enum_sumtype_in_py_errors() {
        let errs = parse_errors_in_py("enum class Color:\n    case Red\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for sum-type `enum` in .py file"
        );
        assert!(
            errs[0].contains("enum"),
            "expected error mentioning `enum`, got: {errs:?}"
        );
    }

    #[test]
    fn enum_sumtype_in_by_no_errors() {
        let (db, file) = make_in_memory_db("enum class Color:\n    case Red, Green\n");
        let module = ruff_db::parsed::parsed_module(
            &db,
            ty_python_semantic::Db::program_file(&db, file).python_file(&db),
        )
        .load(&db);
        assert!(
            module.errors().is_empty(),
            "unexpected parse errors in .by file: {:?}",
            module.errors()
        );
    }

    #[test]
    fn enum_module_attr_access_unaffected_in_py() {
        // `enum.Enum` / `enum = …` are ordinary Python and must not be mistaken
        // for a based-enum declaration
        let errs = parse_errors_in_py("import enum\nx = enum.Enum\nenum = 5\n");
        assert!(
            errs.is_empty(),
            "unexpected parse errors in .py file: {errs:?}"
        );
    }

    #[test]
    fn bare_class_in_by_no_errors() {
        let (db, file) = make_in_memory_db("class A\n");
        let module = ruff_db::parsed::parsed_module(
            &db,
            ty_python_semantic::Db::program_file(&db, file).python_file(&db),
        )
        .load(&db);
        assert!(
            module.errors().is_empty(),
            "unexpected parse errors in .by file: {:?}",
            module.errors()
        );
    }

    #[test]
    fn init_call_inside_method_parses_in_py() {
        // regression: a plain `init(...)` call inside a method of a class is
        // valid python (cpython's `mimetypes.py` does exactly this). it must not
        // be mistaken for the basedpython init-method shorthand, which would
        // raise "`init(...)` method shorthand is not valid in .py files"
        let errs = parse_errors_in_py("class C:\n    def __init__(self):\n        init()\n");
        assert!(
            errs.is_empty(),
            "unexpected parse errors in .py file: {errs:?}"
        );
    }

    #[test]
    fn postfix_await_in_py_errors() {
        let errs = parse_errors_in_py("async def f():\n    g().await\n");
        assert!(
            !errs.is_empty(),
            "expected parse error for postfix `.await` in .py file"
        );
        assert!(
            errs[0].contains("await"),
            "expected error mentioning `await`, got: {errs:?}"
        );
    }

    #[test]
    fn postfix_await_in_by_no_errors() {
        let (db, file) = make_in_memory_db("async def f():\n    g().await\n");
        let module = ruff_db::parsed::parsed_module(
            &db,
            ty_python_semantic::Db::program_file(&db, file).python_file(&db),
        )
        .load(&db);
        assert!(
            module.errors().is_empty(),
            "unexpected parse errors in .by file: {:?}",
            module.errors()
        );
    }
}

#[cfg(test)]
mod transpile_error {
    use super::*;
    use ruff_text_size::TextSize;

    /// basedpython holds back python's two `match` checks for a `.by` source,
    /// where a bare `case A:` name may be an enum member rather than a capture.
    /// The emitted python has every name spelled out, so a `match` that is
    /// invalid there has to be caught rather than written out.
    #[test]
    fn verify_syntax_rejects_an_invalid_match() {
        for source in [
            // alternatives that bind different names
            "match x:\n    case a | b:\n        pass\n",
            // a capture that makes the remaining cases unreachable
            "match x:\n    case a:\n        pass\n    case 2:\n        pass\n",
            // nested in a function, which the scan has to reach
            "def f(x):\n    match x:\n        case a | b:\n            pass\n",
        ] {
            let err = verify_syntax(source).unwrap_err();
            assert!(
                err.message
                    .starts_with("transpiler produced invalid Python:"),
                "got: {}",
                err.message
            );
        }
    }

    /// The qualified spelling a resolved case name lowers to binds nothing, so
    /// the alternatives agree and the check has nothing to say.
    #[test]
    fn verify_syntax_accepts_a_qualified_match() {
        verify_syntax(
            "match x:\n    case Color.Red | Color.Green:\n        pass\n    case Color.Blue:\n        pass\n",
        )
        .unwrap();
    }

    #[test]
    fn verify_syntax_message_has_no_byte_range() {
        let err = verify_syntax("def f(:\n    pass\n").unwrap_err();
        assert!(
            !err.message.contains("byte range"),
            "message must not leak internal byte ranges: {}",
            err.message
        );
        assert!(
            err.message
                .starts_with("transpiler produced invalid Python:"),
            "got: {}",
            err.message
        );
        assert!(
            err.output_range.is_some(),
            "a parse error should carry its span"
        );
    }

    #[test]
    fn maps_output_offset_to_by_line_range() {
        // two generated preamble lines, then the body maps 1:1 to source
        let by_source = "a = 1\nb = 2\nc = 3\n";
        let final_output = "PREAMBLE\nPREAMBLE\na = 1\nb = 2\nc = 3\n";
        let line_map = [None, None, Some(0), Some(1), Some(2)];

        let offset = TextSize::try_from(final_output.find("c = 3").unwrap()).unwrap();
        let range = output_offset_to_by_range(&line_map, final_output, by_source, offset)
            .expect("offset should map to a .by line");
        assert_eq!(&by_source[range], "c = 3");
    }

    #[test]
    fn first_line_range_excludes_a_bom() {
        let by_source = "\u{feff}a = 1\nb = 2\n";
        let final_output = "a = 1\nb = 2\n";
        let line_map = [Some(0), Some(1)];

        let range = output_offset_to_by_range(&line_map, final_output, by_source, TextSize::new(0))
            .expect("offset should map to a .by line");
        assert_eq!(&by_source[range], "a = 1");
    }
}

/// Transpilation that depends on type information resolved across module
/// boundaries. These exercise `transpile_typed` with a real multi-file db so
/// type-aware passes (`generic_call`, `literal_types`, …) see imported types.
#[cfg(test)]
mod cross_file {
    use super::*;
    use ruff_db::files::system_path_to_file;
    use ty_project::{ProjectMetadata, TestDb};

    /// a project db over `files`, keeping the file list so the project can be
    /// built a second time — the [`RebuildProject`] capability a real caller
    /// supplies, for when a pre-pass rewrites the source
    struct Project {
        files: Vec<(String, String)>,
        db: TestDb,
    }

    impl Project {
        fn db(&self) -> &TestDb {
            &self.db
        }

        fn rebuild(&self) -> Box<dyn ty_python_semantic::Db> {
            Box::new(build_db(&self.files))
        }
    }

    fn build_db(files: &[(String, String)]) -> TestDb {
        let mut db = TestDb::new(ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            SystemPathBuf::from("/"),
        ));
        db.init_program().expect("program init failed");
        for (path, src) in files {
            db.write_file(path, src).expect("write file failed");
        }
        db
    }

    fn project_db(files: &[(&str, &str)]) -> Project {
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(path, src)| ((*path).to_owned(), (*src).to_owned()))
            .collect();
        let db = build_db(&files);
        Project { files, db }
    }

    fn transpile_file(project: &Project, path: &str, config: &Config) -> String {
        let file = system_path_to_file(project.db(), path).expect("file not in db");
        let rebuild = || Some(project.rebuild());
        transpile_typed(project.db(), file, config, Some(&rebuild)).expect("transpile failed")
    }

    /// `f[int](1)` must lower to `f(1)` only because ty resolves the imported
    /// `f` to a generic *function* (constructor calls like `Foo[int](1)` keep
    /// their args). that resolution requires cross-module type info — the
    /// single-file path can't see `f` and would leave the broken `f[int](1)`.
    #[test]
    fn generic_call_stripped_via_imported_function() {
        let project = project_db(&[
            ("/mod_a.by", "def f[T](t: T) -> T: ...\n"),
            ("/mod_b.by", "from mod_a import f\nresult = f[int](1)\n"),
        ]);
        let out = transpile_file(&project, "/mod_b.by", &Config::test_default());
        assert!(
            out.contains("result = f(1)"),
            "imported generic function should strip type args, got:\n{out}"
        );
        assert!(
            !out.contains("f[int]"),
            "type args should be gone, got:\n{out}"
        );
    }

    /// the same call, in a file that also declares an `enum class`. the enum
    /// lowering rewrites the source before phase 0, and phase 0 then drops the
    /// project db because the working source no longer matches the file
    /// (`transpile_typed_with_map`'s `source_changed` → `project = None`) — so
    /// every type-aware pass loses cross-module *and* project resolution, and
    /// this call is left as the broken `f[int](1)`. an unrelated declaration
    /// elsewhere in the file must not change what a call lowers to.
    ///
    /// reordering cannot fix it: phase 0 depends on running against the
    /// enum-lowered source (`inferred_annotation` skips the `__slots__ = ()`
    /// the enum lowering re-feeds through the pipeline). the fix is to give
    /// phase 0 a db that keeps the project's metadata and system while serving
    /// the rewritten source for this one file — a `System` wrapper passed to
    /// `ProjectDatabase::fallible`, which needs a capability threaded in from
    /// the caller that owns the real db
    #[test]
    fn cross_module_resolution_survives_an_enum_in_the_same_file() {
        let project = project_db(&[
            ("/mod_a.by", "def f[T](t: T) -> T: ...\n"),
            (
                "/mod_b.by",
                "from mod_a import f\n\nenum class Colour:\n    case Red\n\nresult = f[int](1)\n",
            ),
        ]);
        let out = transpile_file(&project, "/mod_b.by", &Config::test_default());
        assert!(
            out.contains("result = f(1)"),
            "an enum elsewhere in the file must not blind cross-module resolution, got:\n{out}"
        );
    }

    /// the same defect reached by *qualification* rather than by an enum
    /// declaration, which is the likelier trigger and the wider one: one
    /// unqualified member of an enum imported from elsewhere — `return Red`,
    /// the headline example of context-sensitive resolution — is enough, since
    /// `qualified_changed` sets `source_changed` on its own
    ///
    /// so composing two shipped basedpython features in one file silently
    /// breaks the second, with a clean `by check` and valid emitted python
    #[test]
    fn cross_module_resolution_survives_a_qualified_name_in_the_same_file() {
        let project = project_db(&[
            ("/mod_a.by", "def f[T](t: T) -> T: ...\n"),
            ("/colours.by", "enum class Colour:\n    case Red, Green\n"),
            (
                "/mod_b.by",
                "from mod_a import f\nfrom colours import Colour\n\ndef pick() -> Colour:\n    return Red\n\nresult = f[int](1)\n",
            ),
        ]);
        let out = transpile_file(&project, "/mod_b.by", &Config::test_default());
        assert!(
            out.contains("Colour.Red"),
            "the qualification itself should still happen, got:\n{out}"
        );
        assert!(
            out.contains("result = f(1)"),
            "a qualified name elsewhere in the file must not blind cross-module \
             resolution, got:\n{out}"
        );
    }

    /// an imported *reified* generic function keeps its `[int]` specialization
    /// — the `@generic` wrapper routes it through `__getitem__`. only the
    /// cross-module type tells us `f` reifies `T` (value-position use), so the
    /// single-file path can't make this call
    #[test]
    fn imported_reified_function_call_site_preserved() {
        let project = project_db(&[
            (
                "/mod_a.by",
                "def f[T](t: object) -> bool:\n    return isinstance(t, T)\n",
            ),
            ("/mod_b.by", "from mod_a import f\nresult = f[int](1)\n"),
        ]);
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        let out = transpile_file(&project, "/mod_b.by", &config);
        assert!(
            out.contains("f[int](1)"),
            "reified call site must keep its type args, got:\n{out}"
        );
    }

    /// an imported plain class subscript-call (`Box[int](1)`) is a real generic
    /// constructor and must be preserved — the cross-module type tells us it's
    /// a class, not a function.
    #[test]
    fn imported_class_constructor_preserved() {
        let project = project_db(&[
            (
                "/mod_a.by",
                "class Box[T]:\n    def __init__(self, t: T): ...\n",
            ),
            ("/mod_b.by", "from mod_a import Box\nb = Box[int](1)\n"),
        ]);
        let out = transpile_file(&project, "/mod_b.by", &Config::test_default());
        assert!(
            out.contains("Box[int](1)"),
            "imported generic constructor must keep its type args, got:\n{out}"
        );
    }

    /// an extension declared in an imported module resolves at call sites in
    /// the importing module: the lowering rewrites the call to the backing
    /// function and emits its precise import. the surface stays `import ext`
    #[test]
    fn imported_extension_rewrites_call_and_adds_import() {
        let project = project_db(&[
            (
                "/ext.by",
                "extension list:\n    def second(self) -> Element:\n        return self[1]\n",
            ),
            (
                "/main.by",
                "import ext\n\nxs = [1, 2, 3]\nprint(xs.second())\n",
            ),
        ]);
        let out = transpile_file(&project, "/main.by", &Config::test_default());
        assert!(
            out.contains("from ext import _by_ext__list__second"),
            "backing-function import should be emitted, got:\n{out}"
        );
        assert!(
            out.contains("print(_by_ext__list__second(xs))"),
            "call should be rewritten, got:\n{out}"
        );
        // the defining module lowers the block itself
        let ext_out = transpile_file(&project, "/ext.by", &Config::test_default());
        assert!(
            ext_out.contains("def _by_ext__list__second(self):"),
            "defining module should lower the block, got:\n{ext_out}"
        );
    }

    /// Locate an interpreter for the cross-module runtime checks.
    fn python() -> Option<String> {
        let mut candidates = Vec::new();
        if let Ok(p) = std::env::var("PYTHON") {
            candidates.push(p);
        }
        candidates.extend(["python3.13", "python3", "python"].map(String::from));
        candidates.into_iter().find(|py| {
            std::process::Command::new(py)
                .args(["-c", ""])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    /// a target new enough that `override` resolves in the stdlib rather than
    /// through `typing_extensions`, which the test environment need not have
    fn runtime_config() -> Config {
        Config {
            min_version: ruff_python_ast::PythonVersion::PY313,
            ..Config::test_default()
        }
    }

    /// Transpile every module of `project` into a temp directory and run `main`.
    ///
    /// Asserting the emitted *text* of each file in isolation is not enough for
    /// anything cross-module: a conformance registers in one module and is read
    /// in another, so only executing both together proves the two halves meet.
    /// Two text-asserting tests once passed while the program they described
    /// raised `AttributeError`.
    fn run_project(project: &Project, config: &Config) -> Result<String, String> {
        let Some(python) = python() else {
            return Ok("SKIPPED".to_owned());
        };
        let dir = tempfile::tempdir().expect("temp dir");
        for (path, _) in &project.files {
            let out = transpile_file(project, path, config);
            let name = path.trim_start_matches('/').replace(".by", ".py");
            let target = dir.path().join(&name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&target, out).expect("write");
        }
        let output = std::process::Command::new(&python)
            .arg("main.py")
            .current_dir(dir.path())
            .output()
            .expect("spawn python");
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }

    /// the feature's headline case: the interface comes from one module, the
    /// conformance from another, and the consumer sees both only through its
    /// imports. this must *run*, not merely lower
    #[test]
    fn an_imported_conformance_dispatches_at_runtime() {
        let project = project_db(&[
            ("/iface.by", "protocol Show:\n    def show(self) -> str\n"),
            (
                "/adapters.by",
                "from iface import Show\n\nextension str(Show):\n    override def show(self) -> str:\n        return \"OWN:\" + self\n",
            ),
            (
                "/main.by",
                "import adapters\nfrom iface import Show\n\ndef render(value: Show) -> str:\n    return value.show()\n\nprint(render(\"hi\"))\n",
            ),
        ]);
        match run_project(&project, &runtime_config()) {
            Ok(out) if out == "SKIPPED" => {}
            Ok(out) => assert_eq!(out, "OWN:hi"),
            Err(err) => panic!("the imported conformance did not dispatch:\n{err}"),
        }
    }

    /// the module that declares the protocol-typed function is *upstream* of the
    /// conformance by construction, so it can never see one. it still has to
    /// emit a dispatch
    #[test]
    fn a_conformance_declared_downstream_of_its_use_still_dispatches() {
        let project = project_db(&[
            (
                "/lib.by",
                "protocol Show:\n    def show(self) -> str\n\ndef render(value: Show) -> str:\n    return value.show()\n",
            ),
            (
                "/main.by",
                "from lib import Show, render\n\nextension str(Show):\n    override def show(self) -> str:\n        return \"OWN:\" + self\n\nprint(render(\"hi\"))\n",
            ),
        ]);
        match run_project(&project, &runtime_config()) {
            Ok(out) if out == "SKIPPED" => {}
            Ok(out) => assert_eq!(out, "OWN:hi"),
            Err(err) => panic!("a downstream conformance did not dispatch:\n{err}"),
        }
    }

    /// a project with no conformance anywhere is byte-for-byte unaffected.
    ///
    /// whether a requirement dispatches is a whole-program question — a
    /// conformance is written downstream of the interface, so no per-file gate is
    /// sound — but "does this project contain one at all" *is* answerable, and
    /// without it every protocol member access in every python project would be
    /// rewritten, naming the protocol at runtime where a `TYPE_CHECKING`-only
    /// import could not survive it
    #[test]
    fn a_project_without_conformances_is_untouched() {
        let project = project_db(&[(
            "/main.by",
            "from typing import Protocol\n\nclass Shape(Protocol):\n    def area(self) -> int: ...\n\ndef total(s: Shape) -> int:\n    return s.area()\n",
        )]);
        let out = transpile_file(&project, "/main.by", &Config::test_default());
        assert!(out.contains("return s.area()"), "got:\n{out}");
        assert!(!out.contains("_by_witness"), "got:\n{out}");
        assert!(!out.contains("_by_conformances"), "got:\n{out}");
    }

    /// laziness is only given up where it has to be: for a module whose
    /// *execution* is the point of the import.
    ///
    /// a conformance registers itself when its module runs, so importing one
    /// lazily would defer the conformance out of existence. nothing else earns
    /// that: an ordinary extension is resolved at transpile time, and a module
    /// that merely imports a conforming one is not where any conformance is
    /// applicable — ty grants visibility one level, so this mirrors it
    #[test]
    fn only_a_direct_conformance_import_gives_up_laziness() {
        let project = project_db(&[
            (
                "/plainext.by",
                "extension list:\n    def second(self) -> Element:\n        return self[1]\n",
            ),
            ("/iface.by", "protocol Show:\n    def show(self) -> str\n"),
            (
                "/adapters.by",
                "from iface import Show\n\nextension str(Show):\n    override def show(self) -> str:\n        return self\n",
            ),
            ("/mid.by", "import adapters\n\nVALUE = 1\n"),
            (
                "/main.by",
                "import plainext\nimport mid\nimport adapters\n\nxs = [1, 2, 3]\nprint(xs.second())\n",
            ),
        ]);
        // `test_default` turns laziness off, and the whole point here is what it
        // does when it is on
        let config = Config {
            lazy_imports: true,
            ..Config::test_default()
        };
        let out = transpile_file(&project, "/main.by", &config);
        assert!(
            out.contains("plainext = _lazy_module(\"plainext\")"),
            "an ordinary extension needs nothing to have run, got:\n{out}"
        );
        assert!(
            out.contains("mid = _lazy_module(\"mid\")"),
            "merely reaching a conformance is not declaring one, got:\n{out}"
        );
        assert!(
            out.contains("import adapters"),
            "the conformance's own module must be imported eagerly, got:\n{out}"
        );
        assert!(
            !out.contains("adapters = _lazy_module"),
            "the registration would never run, got:\n{out}"
        );
    }

    /// a conformance declared in another module dispatches from here: the
    /// registration lives with the conformance, and this file only needs to spell
    /// the interface. the whole point of the feature is invisible in one file
    #[test]
    fn an_imported_conformance_dispatches_here() {
        let project = project_db(&[
            ("/iface.by", "protocol Show:\n    def show(self) -> str\n"),
            (
                "/adapters.by",
                "from iface import Show\n\nextension str(Show):\n    override def show(self) -> str:\n        return self\n",
            ),
            (
                "/main.by",
                "import adapters\nfrom iface import Show\n\ndef render(value: Show) -> str:\n    return value.show()\n\nprint(render(\"hi\"))\n",
            ),
        ]);
        let out = transpile_file(&project, "/main.by", &Config::test_default());
        assert!(
            out.contains("return _by_witness(value, _by_conv__Show, \"show\")()"),
            "the requirement should dispatch through the table, got:\n{out}"
        );
        // the conformance is not this file's to register
        assert!(!out.contains("_by_conform(_by_conv__Show,"), "got:\n{out}");

        // the declaring module emits both halves, naming the backing function it
        // lowered itself
        let adapters_out = transpile_file(&project, "/adapters.by", &Config::test_default());
        assert!(
            adapters_out
                .contains("_by_conform(_by_conv__Show, str, {\"show\": _by_ext__str__show})"),
            "the declaring module should register the table, got:\n{adapters_out}"
        );
    }

    /// a conformance whose requirement is answered by a protocol extension in a
    /// third module has to import that module's backing function into the
    /// registration — the table names a function, not a member
    #[test]
    fn a_registration_imports_a_default_from_another_module() {
        let project = project_db(&[
            (
                "/iface.by",
                "protocol Show:\n    def show(self) -> str\n\nextension Show:\n    def show(self) -> str:\n        return \"?\"\n",
            ),
            (
                "/adapters.by",
                "from iface import Show\n\nextension str(Show): ...\n",
            ),
        ]);
        let out = transpile_file(&project, "/adapters.by", &Config::test_default());
        assert!(
            out.contains("_by_ext__Show__show") && out.contains("from iface import"),
            "the default's backing function must be imported, got:\n{out}"
        );
        assert!(
            out.contains("_by_conform(_by_conv__Show, str, {\"show\": _by_ext__Show__show})"),
            "got:\n{out}"
        );
    }

    /// a conversion dunder travels with the type rather than with imports, so a
    /// `__from__` on an imported class converts here even though this file never
    /// mentions the target by name. the lowering then has to bring the class in
    /// itself — that import is the whole difference from the single-file case
    #[test]
    fn imported_from_converts_and_imports_the_target() {
        let project = project_db(&[
            (
                "/temps.by",
                "class Celsius:\n    degrees: float = 0.0\n\n\
                 class Fahrenheit:\n    degrees: float = 0.0\n\n    \
                 @classmethod\n    def __from__(cls, value: Celsius) -> Self:\n        return cls()\n\n\
                 def report(t: Fahrenheit) -> None: ...\n",
            ),
            (
                "/main.by",
                "from temps import Celsius, report\n\nreport(Celsius())\n",
            ),
        ]);
        let out = transpile_file(&project, "/main.by", &Config::test_default());
        assert!(
            out.contains("from temps import Fahrenheit as _by_conv__Fahrenheit"),
            "the target class should be imported under its alias, got:\n{out}"
        );
        assert!(
            out.contains("report(_by_conv__Fahrenheit.__from__(Celsius()))"),
            "argument should be converted, got:\n{out}"
        );
    }

    /// a cross-module target is *always* imported under a mangled alias, even
    /// when the file already binds its own name. reusing that binding is not
    /// safe: it may be conditional (`if TYPE_CHECKING:`) or come after the site,
    /// and the end-of-scope type a symbol lookup reports says nothing about either
    #[test]
    fn an_already_imported_target_still_uses_its_alias() {
        let project = project_db(&[
            (
                "/temps.by",
                "class Celsius:\n    degrees: float = 0.0\n\n\
                 class Fahrenheit:\n    degrees: float = 0.0\n\n    \
                 @classmethod\n    def __from__(cls, value: Celsius) -> Self:\n        return cls()\n",
            ),
            (
                "/main.by",
                "from temps import Celsius, Fahrenheit\n\n\
                 def report(t: Fahrenheit) -> None: ...\n\nreport(Celsius())\n",
            ),
        ]);
        let out = transpile_file(&project, "/main.by", &Config::test_default());
        assert!(
            out.contains("report(_by_conv__Fahrenheit.__from__(Celsius()))"),
            "argument should be converted through the alias, got:\n{out}"
        );
        assert!(
            !out.contains("from temps import Fahrenheit\n"),
            "the bare name must never be rebound, got:\n{out}"
        );
    }

    /// a file with its own, unrelated class of the target's name must still
    /// convert correctly: the bare import would be shadowed by that class, and
    /// the call would reach the wrong object at runtime
    #[test]
    fn a_local_class_of_the_same_name_does_not_capture_the_conversion() {
        let project = project_db(&[
            (
                "/temps.by",
                "class Celsius:\n    degrees: float = 0.0\n\n\
                 class Fahrenheit:\n    degrees: float = 0.0\n\n    \
                 @classmethod\n    def __from__(cls, value: Celsius) -> Self:\n        return cls()\n\n\
                 def report(t: Fahrenheit) -> None: ...\n",
            ),
            (
                "/main.by",
                "from temps import Celsius, report\n\n\
                 class Fahrenheit:\n    unrelated: int = 0\n\nreport(Celsius())\n",
            ),
        ]);
        let out = transpile_file(&project, "/main.by", &Config::test_default());
        assert!(
            out.contains("from temps import Fahrenheit as _by_conv__Fahrenheit"),
            "the alias keeps the local class intact, got:\n{out}"
        );
        assert!(
            out.contains("report(_by_conv__Fahrenheit.__from__(Celsius()))"),
            "the conversion must not go through the local class, got:\n{out}"
        );
    }

    /// the line map must point a runtime statement in the generated python back
    /// to the line it came from in the `.by` source — the basis of `by run`'s
    /// traceback rewriting. exercised through a lazy import (large generated
    /// preamble) and an intersection annotation (within-line rewrite)
    #[test]
    fn line_map_points_runtime_line_to_by_source() {
        let src = "from collections.abc import Iterator\n\nx: int & str\n\ndef boom() -> int:\n    return 1 // 0\n";
        let project = project_db(&[("/m.by", src)]);
        let file = system_path_to_file(project.db(), "/m.by").expect("file not in db");
        let (out, map) =
            transpile_typed_with_map(project.db(), file, &Config::test_default(), None)
                .expect("transpile failed");

        let out_idx = out
            .lines()
            .position(|l| l.contains("return 1 // 0"))
            .expect("statement present in output");
        let by_line = map[out_idx].expect("output line should map to source") as usize;
        let by_src: Vec<&str> = src.lines().collect();
        assert_eq!(
            by_src[by_line],
            "    return 1 // 0",
            "line map should point at the originating .by line, got line {by_line}: {:?}",
            by_src.get(by_line)
        );
    }

    /// The map for one source, spelled with and without a final newline.
    fn map_both_ways(body: &str) -> (Vec<Option<u32>>, Vec<Option<u32>>) {
        let mut maps = Vec::new();
        for src in [body.to_owned(), format!("{body}\n")] {
            let project = project_db(&[("/m.by", &src)]);
            let file = system_path_to_file(project.db(), "/m.by").expect("file not in db");
            let (_, map) =
                transpile_typed_with_map(project.db(), file, &Config::test_default(), None)
                    .expect("transpile failed");
            maps.push(map);
        }
        let with_newline = maps.pop().expect("two maps");
        let without = maps.pop().expect("two maps");
        (without, with_newline)
    }

    /// A trailing newline terminates the last line; it does not add one. So the
    /// two spellings of the same program are the same program, and the debugger
    /// must be told the same thing about both.
    ///
    /// It was not: the table was built per `\n`, so a file that ended without one
    /// lost the entry for its own last line and every entry the entry-point
    /// epilogue appended slid up over it. A breakpoint on that line was then
    /// refused as generated prelude — a claim about a line the user wrote
    /// themselves.
    #[test]
    fn a_missing_final_newline_does_not_cost_the_last_line_its_mapping() {
        let body = "def g():\n    print(2)\n\ndef main():\n    g()";
        let (without, with_newline) = map_both_ways(body);

        assert_eq!(
            without, with_newline,
            "the two spellings of one program must map alike"
        );
        // the last line the user wrote is the last line that maps to source, and
        // it maps to itself: `    g()` is `.by` line 4
        let last_mapped = without
            .iter()
            .rposition(Option::is_some)
            .expect("some line maps to source");
        assert_eq!(without[last_mapped], Some(4), "map:\n{without:?}");
    }

    /// the same, through the enum lowering — a second table producer, which
    /// counted completed lines the same way and dropped the last one alike
    #[test]
    fn a_missing_final_newline_maps_the_last_line_through_the_enum_lowering() {
        let body = "enum class Colour:\n    case Red\n    case Green\n\ndef main():\n    print(Colour.Red)";
        let (without, with_newline) = map_both_ways(body);

        assert_eq!(
            without, with_newline,
            "the two spellings of one program must map alike"
        );
        let last_mapped = without
            .iter()
            .rposition(Option::is_some)
            .expect("some line maps to source");
        assert_eq!(
            without[last_mapped],
            Some(5),
            "`    print(Colour.Red)` is .by line 5, map:\n{without:?}"
        );
    }

    /// the map is indexed by generated line, so it has to be exactly as long as
    /// the generated python has lines — an entry short and every lookup past the
    /// gap answers for its neighbour
    #[test]
    fn the_map_has_one_entry_per_generated_line() {
        for body in [
            "print(1)",
            "print(1)\n",
            "def main():\n    print(1)",
            "x: int & str\nprint(1)",
            "enum class E:\n    case A\nprint(E.A)",
        ] {
            let project = project_db(&[("/m.by", body)]);
            let file = system_path_to_file(project.db(), "/m.by").expect("file not in db");
            let (out, map) =
                transpile_typed_with_map(project.db(), file, &Config::test_default(), None)
                    .expect("transpile failed");
            assert_eq!(
                map.len(),
                super::line_count(&out),
                "one entry per generated line for {body:?}"
            );
        }
    }
}
