pub mod config;
mod reverse_transforms;
pub mod source_map;
mod transforms;
pub(crate) mod type_info;

pub use config::{Config, PythonVersion, SoundnessPositions};

use std::collections::{BTreeSet, HashSet};

use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::{DbWithWritableSystem as _, SystemPathBuf};
use ruff_diagnostics::{Edit, Fix, IsolationLevel};
use ruff_python_ast::Stmt;
use ruff_python_ast::visitor::Visitor;
use ruff_text_size::{Ranged, TextSize};
use ty_project::{ProjectMetadata, TestDb};

use type_info::TypeInfo;

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
    let parsed = ruff_db::parsed::parsed_module(db, file).load(db);
    if !parsed.errors().is_empty() {
        return std::borrow::Cow::Borrowed(source);
    }
    let model = ty_python_semantic::SemanticModel::new(db, file);
    transforms::context_sensitive::qualify(source, parsed.suite(), &model)
}

/// Transpile `.by` source text to python without a project db (single-file:
/// type-aware passes see only this file). Used for stdin input and tests; the
/// file-backed [`transpile_typed`] resolves cross-module types.
pub fn transpile(source: &str, config: &Config) -> Result<String, String> {
    if config.is_python {
        return Ok(source.to_owned());
    }

    // one db over the original source, shared by the qualification phase below
    // and — as long as nothing rewrites the source — by phase 0's type-aware
    // passes, which would otherwise build an identical one of their own
    let (local_db, local_file) = make_in_memory_db(source);

    // --- Context-sensitive resolution: qualify names resolved against their
    // expected type (`a: Color = Red` → `Color.Red`) while the source is still
    // the one ty checks ---
    let qualified = run_context_sensitive_phase(source, &local_db, local_file);
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
    let unchanged =
        !qualified_changed && matches!(enum_lowered.output, std::borrow::Cow::Borrowed(_));
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
    let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
    let model = ty_python_semantic::SemanticModel::new(&db, file);
    if let Some(err) = module.errors().iter().find(|e| e.is_basedpython_only()) {
        return Err(err.to_string());
    }
    let LoweringResult { output, errors } = run_lowering_phase(src, module.suite(), config, &model);
    if let Some(first) = errors.first() {
        return Err(first.clone());
    }

    // --- Phase 2: import-redirect, surface-syntax cleanup, lazy-import marking ---
    let final_output = run_import_redirect_phase(output, config);
    let final_output = run_anon_named_tuple_cleanup(final_output, config)?;
    let final_output = run_lazy_import_phase(final_output, config);

    // --- Phase 3: syntax verification ---
    verify_syntax(&final_output).map_err(|e| e.message)?;

    Ok(final_output)
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
) -> Result<String, TranspileError> {
    transpile_typed_with_map(db, file, config).map(|(out, _)| out)
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
) -> Result<(String, Vec<Option<u32>>), TranspileError> {
    let source_ref = ruff_db::source::source_text(db, file);
    let original_source = source_ref.as_str();

    if config.is_python {
        let out = original_source.to_owned();
        return Ok((out, source_map::line_table(original_source, &[])));
    }

    // context-sensitive resolution: qualify names resolved against their expected
    // type (`a: Color = Red` → `Color.Red`) against the source ty checks, before
    // the enum lowering rewrites it. a within-line rewrite, so line
    // correspondence is unaffected
    let qualified = run_context_sensitive_phase(original_source, db, file);
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
    let source_changed = qualified_changed || enum_changed;

    // phase 0: AST passes. with the project db (no enums) type-aware passes
    // resolve cross-module imports; `phase0_map` maps spliced lines → working
    // (post-enum) lines
    let project = if source_changed {
        None
    } else {
        Some((db, file))
    };
    let (spliced, ast_errors, phase0_map) =
        transforms::ast_driver::run_against_source(working_source, config, project);
    if let Some(first) = ast_errors.first() {
        return Err(first.clone().into());
    }
    let spliced_lines = newline_count(spliced.as_ref());
    let (output, errors) = if let std::borrow::Cow::Owned(modified) = spliced {
        let (local_db, local_file) = make_in_memory_db(&modified);
        let local_source_ref = ruff_db::source::source_text(&local_db, local_file);
        let src = local_source_ref.as_str();
        let module = ruff_db::parsed::parsed_module(&local_db, local_file).load(&local_db);
        let model = ty_python_semantic::SemanticModel::new(&local_db, local_file);
        let LoweringResult { output, errors } =
            run_lowering_phase(src, module.suite(), config, &model);
        (output, errors)
    } else if source_changed {
        // ast_driver made no further changes, but the working source differs
        // from the project file — parse it in a single-file db
        let (local_db, local_file) = make_in_memory_db(working_source);
        let local_source_ref = ruff_db::source::source_text(&local_db, local_file);
        let src = local_source_ref.as_str();
        let module = ruff_db::parsed::parsed_module(&local_db, local_file).load(&local_db);
        let model = ty_python_semantic::SemanticModel::new(&local_db, local_file);
        let LoweringResult { output, errors } =
            run_lowering_phase(src, module.suite(), config, &model);
        (output, errors)
    } else {
        let module = ruff_db::parsed::parsed_module(db, file).load(db);
        let model = ty_python_semantic::SemanticModel::new(db, file);
        let LoweringResult { output, errors } =
            run_lowering_phase(original_source, module.suite(), config, &model);
        (output, errors)
    };
    if let Some(first) = errors.first() {
        return Err(first.clone().into());
    }

    let final_output = run_import_redirect_phase(output, config);
    let final_output = run_anon_named_tuple_cleanup(final_output, config)?;
    let final_output = run_lazy_import_phase(final_output, config);

    // phases 1-2c only prepend preambles at the top and edit within lines, so
    // the spliced body keeps its line correspondence: prepend one `None` per
    // generated leading line to lift `phase0_map` into final-output coordinates.
    // when the enum phase fired, also compose `working → original .by` lines
    let prepended = newline_count(&final_output).saturating_sub(spliced_lines);
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
    let mut line_map: Vec<Option<u32>> = Vec::with_capacity(prepended + composed.len());
    line_map.extend(std::iter::repeat_n(None, prepended));
    line_map.extend(composed);

    // verify last: on failure, map the generated span back to a `.by` range
    if let Err(mut err) = verify_syntax(&final_output) {
        err.by_range = err.output_range.and_then(|r| {
            output_offset_to_by_range(&line_map, &final_output, original_source, r.start())
        });
        return Err(err);
    }

    Ok((final_output, line_map))
}

fn newline_count(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
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
        let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
        let model = ty_python_semantic::SemanticModel::new(&db, file);

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
            preamble.push_str("from typing import NamedTuple\n");
            preamble.push_str(&anon.class_defs());
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
            format!("{preamble}{body}")
        };

        let _ = config;
    }
    Ok(source)
}

/// Rewrite stdlib imports to `typing_extensions` where the imported name is
/// not yet available at the configured min version
fn run_import_redirect_phase(source: String, config: &Config) -> String {
    let (db, file) = make_in_memory_db(&source);
    let source_ref = ruff_db::source::source_text(&db, file);
    let src = source_ref.as_str();
    let module = ruff_db::parsed::parsed_module(&db, file).load(&db);

    let mut typing_redirect = transforms::typing_redirect::TypingRedirect::new(src, config.clone());
    for stmt in module.suite() {
        typing_redirect.visit_stmt(stmt);
    }

    if typing_redirect.edits.is_empty() {
        return source;
    }

    let (output, _) = apply_transforms_once(src, typing_redirect.edits);
    output
}

/// Lazy-import marking phase: walks the post-typing-redirect output and
/// prepends `lazy ` (PEP 810, Python 3.15+) to every `import` and
/// `from import` statement. Skips `from __future__` and star imports.
///
/// Gated on `min_version >= 3.15`: PEP 810 syntax doesn't parse on older
/// Python, so we leave imports eager when the target version can't handle
/// the keyword. A redundant `lazy` keyword written in source is stripped
/// in that case so the output stays valid
fn run_lazy_import_phase(source: String, config: &Config) -> String {
    if !config.lazy_imports {
        return source;
    }

    let (db, file) = make_in_memory_db(&source);
    let source_ref = ruff_db::source::source_text(&db, file);
    let src = source_ref.as_str();
    let module = ruff_db::parsed::parsed_module(&db, file).load(&db);

    let keyword_supported = config.min_version >= ruff_python_ast::PythonVersion::from((3, 15));
    let mut lazy = transforms::lazy_import::LazyImport::new(src, keyword_supported);
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
    } else if let Some(rest) = body.strip_prefix("from __future__ import annotations\n") {
        // a `from __future__` line MUST come first in the file. main lowering
        // emits it; splice the polyfill preamble in *after* so both stay valid
        format!("from __future__ import annotations\n{preamble}{rest}")
    } else {
        format!("{preamble}{body}")
    }
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

    Ok(())
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

    let mut start = 0usize;
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
fn run_lowering_phase(
    source: &str,
    stmts: &[Stmt],
    config: &Config,
    _types: &dyn TypeInfo,
) -> LoweringResult {
    let mut output = String::new();
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
    let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
    let model = ty_python_semantic::SemanticModel::new(&db, file);

    let mut super_kw_rev = reverse_transforms::super_keyword::SuperKeywordReverse::new(src);
    let mut anon_named_tuple_rev =
        reverse_transforms::anon_named_tuple::AnonNamedTupleReverse::new(src, module.suite());
    let mut empty_decls =
        reverse_transforms::empty_declarations::EmptyDeclarations::new(config.is_stub);
    let mut literal_types = reverse_transforms::literal_types::LiteralReverse::new(src, &model);
    let mut subscript = reverse_transforms::subscript::SubscriptReverse::new(src, &model);
    let mut indent_string = reverse_transforms::dedent_string::IndentString::new(src);
    let mut constraints = reverse_transforms::constraints::ConstraintsReverse::new();
    let mut callable = {
        let c = reverse_transforms::callable::CallableReverse::new(src, &model);
        if config.is_stub { c.stub() } else { c }
    };
    let mut intersection = reverse_transforms::intersection::IntersectionReverse::new(src, &model);
    let mut not_rev = reverse_transforms::not_type::NotTypeReverse::new(src, &model);
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
    let mut implementation_rev =
        reverse_transforms::implementation::ImplementationReverse::new(src, module.suite());
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
        constraints.visit_stmt(stmt);
        intersection.visit_stmt(stmt);
        not_rev.visit_stmt(stmt);
        dynamic_keyword_rev.visit_stmt(stmt);
        literal_string_rev.visit_stmt(stmt);
        type_is_rev.visit_stmt(stmt);
        identity_rev.visit_stmt(stmt);
        tuple_type.visit_stmt(stmt);
        unpack.visit_stmt(stmt);
        modifiers_rev.visit_stmt(stmt);
        enums_rev.visit_stmt(stmt);
        extension_rev.visit_stmt(stmt);
        implementation_rev.visit_stmt(stmt);
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
    fixes.extend(constraints.edits);
    fixes.extend(callable.edits);
    fixes.extend(intersection.edits);
    fixes.extend(not_rev.edits);
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
    fixes.extend(implementation_rev.edits);
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
        let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
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
        let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
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
        let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
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
        let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
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
        let module = ruff_db::parsed::parsed_module(&db, file).load(&db);
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
}

/// Transpilation that depends on type information resolved across module
/// boundaries. These exercise `transpile_typed` with a real multi-file db so
/// type-aware passes (`generic_call`, `literal_types`, …) see imported types.
#[cfg(test)]
mod cross_file {
    use super::*;
    use ruff_db::files::system_path_to_file;
    use ty_project::{ProjectMetadata, TestDb};

    fn project_db(files: &[(&str, &str)]) -> TestDb {
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

    fn transpile_file(db: &TestDb, path: &str, config: &Config) -> String {
        let file = system_path_to_file(db, path).expect("file not in db");
        transpile_typed(db, file, config).expect("transpile failed")
    }

    /// `f[int](1)` must lower to `f(1)` only because ty resolves the imported
    /// `f` to a generic *function* (constructor calls like `Foo[int](1)` keep
    /// their args). that resolution requires cross-module type info — the
    /// single-file path can't see `f` and would leave the broken `f[int](1)`.
    #[test]
    fn generic_call_stripped_via_imported_function() {
        let db = project_db(&[
            ("/mod_a.by", "def f[T](t: T) -> T: ...\n"),
            ("/mod_b.by", "from mod_a import f\nresult = f[int](1)\n"),
        ]);
        let out = transpile_file(&db, "/mod_b.by", &Config::test_default());
        assert!(
            out.contains("result = f(1)"),
            "imported generic function should strip type args, got:\n{out}"
        );
        assert!(
            !out.contains("f[int]"),
            "type args should be gone, got:\n{out}"
        );
    }

    /// an imported *reified* generic function keeps its `[int]` specialization
    /// — the `@generic` wrapper routes it through `__getitem__`. only the
    /// cross-module type tells us `f` reifies `T` (value-position use), so the
    /// single-file path can't make this call
    #[test]
    fn imported_reified_function_call_site_preserved() {
        let db = project_db(&[
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
        let out = transpile_file(&db, "/mod_b.by", &config);
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
        let db = project_db(&[
            (
                "/mod_a.by",
                "class Box[T]:\n    def __init__(self, t: T): ...\n",
            ),
            ("/mod_b.by", "from mod_a import Box\nb = Box[int](1)\n"),
        ]);
        let out = transpile_file(&db, "/mod_b.by", &Config::test_default());
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
        let db = project_db(&[
            (
                "/ext.by",
                "extension list:\n    def second(self) -> Element:\n        return self[1]\n",
            ),
            (
                "/main.by",
                "import ext\n\nxs = [1, 2, 3]\nprint(xs.second())\n",
            ),
        ]);
        let out = transpile_file(&db, "/main.by", &Config::test_default());
        assert!(
            out.contains("from ext import _by_ext__list__second"),
            "backing-function import should be emitted, got:\n{out}"
        );
        assert!(
            out.contains("print(_by_ext__list__second(xs))"),
            "call should be rewritten, got:\n{out}"
        );
        // the defining module lowers the block itself
        let ext_out = transpile_file(&db, "/ext.by", &Config::test_default());
        assert!(
            ext_out.contains("def _by_ext__list__second(self):"),
            "defining module should lower the block, got:\n{ext_out}"
        );
    }

    /// a conversion site whose witness class lives in another module must both
    /// wrap the argument and import the class. the mangled name is computed from
    /// the checker's answer on both sides — the defining module emits the class,
    /// the using module constructs it — so the two files must agree exactly
    #[test]
    fn imported_implementation_wraps_argument_and_adds_import() {
        let db = project_db(&[
            (
                "/iface.by",
                "abstract class A:\n    abstract def f(self) -> int: ...\n\nclass B:\n    a: int = 3\n",
            ),
            (
                "/adapters.by",
                "from iface import A, B\n\nimplementation A for B:\n    override def f(self) -> int:\n        return self.a\n",
            ),
            (
                "/main.by",
                "import adapters\nfrom iface import A, B\n\ndef takes_a(a: A) -> int:\n    return a.f()\n\nb = B()\ntakes_a(b)\n",
            ),
        ]);
        let out = transpile_file(&db, "/main.by", &Config::test_default());
        assert!(
            out.contains("from adapters import _by_impl__A__B"),
            "witness import should be emitted, got:\n{out}"
        );
        assert!(
            out.contains("takes_a(_by_impl__A__B(b))"),
            "argument should be wrapped, got:\n{out}"
        );
        // the defining module emits the class under the same name
        let adapters_out = transpile_file(&db, "/adapters.by", &Config::test_default());
        assert!(
            adapters_out.contains("class _by_impl__A__B(_by_Implementation, A):"),
            "defining module should emit the witness class, got:\n{adapters_out}"
        );
    }

    /// a conversion dunder travels with the type rather than with imports, so a
    /// `__from__` on an imported class converts here even though this file never
    /// mentions the target by name. the lowering then has to bring the class in
    /// itself — that import is the whole difference from the single-file case
    #[test]
    fn imported_from_converts_and_imports_the_target() {
        let db = project_db(&[
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
        let out = transpile_file(&db, "/main.by", &Config::test_default());
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
    /// when the file already binds its own name. importing the bare name would
    /// rebind whatever this file already means by it
    #[test]
    fn an_already_imported_target_still_uses_its_alias() {
        let db = project_db(&[
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
        let out = transpile_file(&db, "/main.by", &Config::test_default());
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
        let db = project_db(&[
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
        let out = transpile_file(&db, "/main.by", &Config::test_default());
        assert!(
            out.contains("from temps import Fahrenheit as _by_conv__Fahrenheit"),
            "the alias keeps the local class intact, got:\n{out}"
        );
        assert!(
            out.contains("report(_by_conv__Fahrenheit.__from__(Celsius()))"),
            "the conversion must not go through the local class, got:\n{out}"
        );
    }

    /// the synthesized witness import must address the module the way the importing
    /// file does. ty's absolute module name can be one the interpreter cannot
    /// resolve — a file under a directory that is not an importable package
    /// (`target/mod.by` → `target.mod` for the checker, `mod` at runtime)
    #[test]
    fn an_imported_witness_is_spelled_as_the_file_imports_it() {
        let db = project_db(&[
            (
                "/nested/iface.by",
                "abstract class A:\n    abstract def f(self) -> int: ...\n\nclass B:\n    a: int = 3\n",
            ),
            (
                "/nested/adapters.by",
                "from iface import A, B\n\nimplementation A for B:\n    override def f(self) -> int:\n        return self.a\n",
            ),
            (
                "/nested/main.by",
                "import adapters\nfrom iface import A, B\n\ndef takes_a(x: A) -> int:\n    return x.f()\n\nb = B()\ntakes_a(b)\n",
            ),
        ]);
        let out = transpile_file(&db, "/nested/main.by", &Config::test_default());
        assert!(
            out.contains("from adapters import _by_impl__A__B"),
            "the import must use the file's own spelling, got:\n{out}"
        );
        assert!(
            !out.contains("nested.adapters"),
            "an absolute name the interpreter cannot resolve leaked in:\n{out}"
        );
    }

    /// a relative import has no absolute spelling at all, so the synthesized one
    /// must keep the dots
    #[test]
    fn a_relatively_imported_witness_keeps_its_dots() {
        let db = project_db(&[
            ("/pkg/__init__.by", "\n"),
            (
                "/pkg/iface.by",
                "abstract class A:\n    abstract def f(self) -> int: ...\n\nclass B:\n    a: int = 3\n",
            ),
            (
                "/pkg/adapters.by",
                "from .iface import A, B\n\nimplementation A for B:\n    override def f(self) -> int:\n        return self.a\n",
            ),
            (
                "/pkg/main.by",
                "from .adapters import A\nfrom .iface import B\n\ndef takes_a(x: A) -> int:\n    return x.f()\n\ndef main():\n    takes_a(B())\n",
            ),
        ]);
        let out = transpile_file(&db, "/pkg/main.by", &Config::test_default());
        assert!(
            out.contains("from .adapters import _by_impl__A__B"),
            "the relative spelling must be kept, got:\n{out}"
        );
    }

    /// a `from mod import X` establishes the dependency just as `import mod` does,
    /// so it must bring the module's implementations into scope
    #[test]
    fn a_from_import_makes_an_implementation_applicable() {
        let db = project_db(&[
            (
                "/iface.by",
                "abstract class A:\n    abstract def f(self) -> int: ...\n\nclass B:\n    a: int = 3\n",
            ),
            (
                "/adapters.by",
                "from iface import A, B\n\nimplementation A for B:\n    override def f(self) -> int:\n        return self.a\n",
            ),
            (
                "/main.by",
                "from adapters import A\nfrom iface import B\n\ndef takes_a(x: A) -> int:\n    return x.f()\n\nb = B()\ntakes_a(b)\n",
            ),
        ]);
        let out = transpile_file(&db, "/main.by", &Config::test_default());
        assert!(
            out.contains("from adapters import _by_impl__A__B"),
            "witness import should be emitted, got:\n{out}"
        );
        assert!(
            out.contains("takes_a(_by_impl__A__B(b))"),
            "argument should be wrapped, got:\n{out}"
        );
    }

    /// without the import the implementation is not applicable, so there is
    /// nothing to wrap — the checker reports the assignment instead
    #[test]
    fn implementation_without_the_import_wraps_nothing() {
        let db = project_db(&[
            (
                "/iface.by",
                "abstract class A:\n    abstract def f(self) -> int: ...\n\nclass B:\n    a: int = 3\n",
            ),
            (
                "/adapters.by",
                "from iface import A, B\n\nimplementation A for B:\n    override def f(self) -> int:\n        return self.a\n",
            ),
            (
                "/main.by",
                "from iface import A, B\n\ndef takes_a(a: A) -> int:\n    return a.f()\n\nb = B()\ntakes_a(b)\n",
            ),
        ]);
        let out = transpile_file(&db, "/main.by", &Config::test_default());
        assert!(out.contains("takes_a(b)"), "got:\n{out}");
        assert!(!out.contains("__by_impl__"), "got:\n{out}");
    }

    /// the line map must point a runtime statement in the generated python back
    /// to the line it came from in the `.by` source — the basis of `by run`'s
    /// traceback rewriting. exercised through a lazy import (large generated
    /// preamble) and an intersection annotation (within-line rewrite)
    #[test]
    fn line_map_points_runtime_line_to_by_source() {
        let src = "from collections.abc import Iterator\n\nx: int & str\n\ndef boom() -> int:\n    return 1 // 0\n";
        let db = project_db(&[("/m.by", src)]);
        let file = system_path_to_file(&db, "/m.by").expect("file not in db");
        let (out, map) =
            transpile_typed_with_map(&db, file, &Config::test_default()).expect("transpile failed");

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
}
