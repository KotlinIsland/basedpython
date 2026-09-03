//! AST → AST rewrite passes for basedpython lowering.
//!
//! Each pass receives the parsed module AST and mutates it. After every
//! pass runs, the driver re-renders each touched top-level statement
//! through [`ruff_python_codegen::Generator`] (basedpython mode) and
//! splices the result back into the source string. The output is then
//! handed to the post-codegen text phases (import-redirect, lazy-import,
//! compat, verify).
//!
//! Capabilities a pass may use:
//!
//! - mutate any expression / statement in place via the
//!   [`Transformer`](ruff_python_ast::visitor::transformer::Transformer)
//!   protocol
//! - declare hoisted statements (new top-level lines that must precede a
//!   particular original statement — e.g. anon-NT class synthesis)
//! - declare required imports (full `import …` / `from … import …` lines
//!   that the driver prepends to the source)
//! - declare sub-statement text edits, for rewrites that would otherwise leak
//!   when an outer transform copies operand source verbatim (variance keyword
//!   blanking)
//!
//! AST passes always engage — there is no gate. The text-edit pipeline
//! is intentionally not invoked for any construct an AST pass handles.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;

use ruff_python_ast::visitor::transformer::Transformer;
use ruff_python_ast::{Expr, ModModule, PySourceType, Stmt};
use ruff_python_codegen::{Generator, Indentation, Mode};
use ruff_python_parser::parse_unchecked_source;
use ruff_source_file::LineEnding;
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::source_util::preamble_offset;
use super::{
    annotation, anon_named_tuple, auto_quote, callable, character_type, checked_cast, coalesce,
    coalesce_chain, compat, conformance, context_params, conversion, decl_site_variance,
    decorator_keyword, dedent_string, destructure, django_lookup, dynamic_keyword,
    empty_declarations, export_import, extension, float_const, force_unwrap, frameworks,
    generic_call, generics, grapheme_string, identity_swap, if_let, implicit_receiver,
    implicit_typing, inferred_annotation, init_method, just_float, kw_subscript, literal_string,
    literal_types, local_once, main_function, match_type, modifiers, mutable_defaults, none_chain,
    optional_type, overload, parametric_is, postfix_await, private_method, propagate, properties,
    protocol_type, raises_clause, reified_generic, repeated_underscore, return_value_use,
    runtime_union, sentinel, some_ctor, soundness, statement_expression, string_tag, super_keyword,
    symbolic_type_op, template_type, top_star, trailing_lambda, tuple_index, type_fn, type_is,
    type_reification, typed_dict_literal, typed_lambda, typeof_keyword, unique_loop_bindings,
    unpack, use_site_variance,
};
use crate::Config;
use crate::type_info::TypeInfo;

/// Holds the db backing the type-aware passes. `Local` owns a single-file
/// in-memory db; `Project` borrows the caller's project db (cross-module
/// imports resolve). Either way the parse + [`SemanticModel`] the passes use
/// come from this one db, preserving `inferred_type` node-identity lookups.
enum SemDb<'p> {
    Project(&'p dyn ty_python_semantic::Db, ruff_db::files::File),
    Local(ty_project::TestDb, ruff_db::files::File),
}

/// One fragment of a [`PassContext::template_edits`] replacement: literal text,
/// or a passthrough span of original source. Passthrough spans are materialized
/// with any sibling edits inside them applied, so a wide rewrite (e.g.
/// `a ?? b` → `a if a is not None else b`) composes with lowerings inside its
/// operands instead of clobbering them via first-wins overlap dedup
#[derive(Clone)]
pub(crate) enum Fragment {
    Lit(String),
    Src(TextRange),
}

/// Mutable state shared across every pass during a single transpile.
#[derive(Default)]
pub(crate) struct PassContext {
    /// Top-level statements that any pass inserted. Each entry is
    /// `(insert_before_idx, stmt)` where `insert_before_idx` is the
    /// 0-based index in the **original** module body before which the
    /// new statement should appear. Multiple inserts at the same idx
    /// preserve declared order.
    pub(crate) hoisted: Vec<(usize, Stmt)>,
    /// Full source lines to prepend to the file (e.g. `from typing import cast`).
    /// Deduped before emission.
    pub(crate) required_imports: Vec<String>,
    /// Indices into the *original* module body of statements any pass
    /// mutated (so the driver knows to re-render them). Indices may
    /// repeat — the driver dedupes.
    pub(crate) changed: Vec<usize>,
    /// Sub-statement text edits: `(range_in_source, replacement)`. Used
    /// by passes that rewrite a single sub-expression (e.g. an annotation
    /// inside a `final def` signature) without disturbing the rest of the
    /// statement. Avoids whole-statement codegen for cases where the
    /// surrounding context contains basedpython markers a sibling pass
    /// hasn't lowered yet
    pub(crate) text_edits: Vec<(TextRange, String)>,
    /// Structured sub-statement edits whose replacement is a [`Fragment`] list.
    /// Unlike `text_edits` (whose plain string wins over anything nested inside
    /// it), the `Src` passthrough spans of a template are materialized with the
    /// sibling edits they contain applied — use this for any rewrite that
    /// re-emits operand source
    pub(crate) template_edits: Vec<(TextRange, Vec<Fragment>)>,
    /// Templates inserted at a *statement* boundary — a guard a pass injects
    /// ahead of the statement starting at that offset. Identical to a
    /// zero-width [`template_edits`](Self::template_edits) entry except that a
    /// rewrite of the statement's own first expression cannot absorb it: a
    /// statement materialized inside an expression is a syntax error, not a
    /// composition
    pub(crate) statement_inserts: Vec<(TextSize, Vec<Fragment>)>,
    /// Hard transpile errors a pass surfaced — abort the pipeline rather
    /// than emit partial / invalid output. Each entry is a human-readable
    /// message suitable for showing the user
    pub(crate) errors: Vec<String>,
    /// Lines to append AFTER the spliced body (e.g. modifiers' auto-
    /// generated `__all__ = [...]`). Driver emits each as its own line
    pub(crate) epilogue: Vec<String>,
    /// Import lines a *synthesized* type expression needs that the source never
    /// wrote (`import decimal` for an inferred `decimal.Decimal` annotation).
    /// The driver emits these under `if TYPE_CHECKING:` — the output always
    /// carries `from __future__ import annotations`, so the name is only ever
    /// read by a checker, and a runtime import here would add an import edge the
    /// source does not have
    pub(crate) type_only_imports: BTreeSet<String>,
    /// Source ranges of operations that `symbolic_type_op` resolved up front
    /// (e.g. `1 + 1` → `Literal[2]`). Type-aware passes skip these via
    /// [`walk_type_positions_skipping`](super::type_expr_walker::walk_type_positions_skipping)
    /// so they don't re-process an operation that no longer appears in the output
    pub(crate) claimed_type_op_ranges: Vec<TextRange>,
    /// The same operations as `(range, rendered)` pairs. A pass that replaces a
    /// whole statement subsumes any fold inside it — skipping is not enough, the
    /// rendered text has to be spliced into the replacement or the operation is
    /// re-emitted from source and reaches the runtime
    pub(crate) symbolic_substitutions: Vec<(TextRange, String)>,
}

/// A single AST-level rewrite pass.
pub(crate) trait AstPass {
    /// Run the pass against the entire parsed module. The pass is free
    /// to mutate any statement in place, declare hoisted statements,
    /// and request runtime imports via [`PassContext`].
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext);
}

/// Type-aware pass that reads semantic info from the salsa-owned parsed
/// module + [`SemanticModel`]. Operates strictly via [`PassContext`]
/// `text_edits` / `required_imports`; the input AST is shared & immutable
/// because `inferred_type` queries bind to its exact node identities
pub(crate) trait TypeAwarePass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext);
}

/// Adapter: lift a [`Transformer`] (visitor that mutates AST in place)
/// into an [`AstPass`] that auto-tracks which top-level statements
/// changed. The transformer must record its mutation status into the
/// supplied `Cell<bool>`; the adapter resets the cell per statement.
pub(crate) struct VisitorPass<'a, T: Transformer> {
    pub(crate) inner: &'a T,
    pub(crate) changed_cell: &'a std::cell::Cell<bool>,
    pub(crate) imports: Vec<String>,
    pub(crate) hoist: RefCell<Vec<(usize, Stmt)>>,
    /// Sub-statement text edits the pass wants the driver to apply. Pass
    /// computes the new sub-AST, renders it via [`render_expr`], and pushes
    /// `(original_range, replacement)` here
    pub(crate) text_edits: RefCell<Vec<(TextRange, String)>>,
}

impl<T: Transformer> AstPass for VisitorPass<'_, T> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        for (idx, stmt) in module.body.iter_mut().enumerate() {
            self.changed_cell.set(false);
            self.inner.visit_stmt(stmt);
            if self.changed_cell.get() {
                ctx.changed.push(idx);
            }
        }
        ctx.required_imports.extend(self.imports.iter().cloned());
        ctx.hoisted.extend(self.hoist.borrow_mut().drain(..));
        ctx.text_edits
            .extend(self.text_edits.borrow_mut().drain(..));
    }
}

/// Render a statement back to python source using ruff's [`Generator`].
/// Basedpython mode handles surviving basedpython-only AST nodes.
pub(crate) fn render_stmt(stmt: &Stmt) -> String {
    let indent = Indentation::default();
    Generator::new(&indent, LineEnding::Lf)
        .with_mode(Mode::BasedPython)
        .stmt(stmt)
}

/// Render an expression back to python source using ruff's [`Generator`].
/// Used by passes that emit sub-statement text edits.
pub(crate) fn render_expr(expr: &Expr) -> String {
    let indent = Indentation::default();
    Generator::new(&indent, LineEnding::Lf)
        .with_mode(Mode::BasedPython)
        .expr(expr)
}

/// A sub-statement edit's replacement: plain text, or a template whose `Src`
/// passthrough spans compose with nested edits.
enum SubPatch {
    Text(String),
    Template(Vec<Fragment>),
    /// a template anchored at a statement boundary — see
    /// [`PassContext::statement_inserts`]. Materializes exactly like
    /// [`SubPatch::Template`]; the difference is only in what may claim it
    Statement(Vec<Fragment>),
}

/// The sub-edits a template materializes, in position order: those nested in
/// its own range, plus those its `Src` passthrough spans contain.
///
/// The two normally coincide, because a template re-emits the span it replaces.
/// A *relocating* template passes source through from somewhere else — the
/// default re-evaluation guard re-emits a parameter default at the body start —
/// and the lowerings inside that span have to materialize where the source
/// lands, not where it was written.
fn template_claimees(
    frags: &[Fragment],
    all: &[(usize, usize, SubPatch)],
    claimed: &[bool],
    self_idx: usize,
    own: Option<(usize, usize)>,
) -> Vec<usize> {
    (0..all.len())
        .filter(|&m| m != self_idx && claimed[m])
        .filter(|&m| {
            let (s, e) = (all[m].0, all[m].1);
            let in_own = own.is_some_and(|(start, end)| s >= start && e <= end && s != end);
            // a boundary insertion at a passthrough's end is left to
            // `apply_within`'s `include_end`, which knows whether an adjacent
            // span will re-emit it
            let in_src = frags.iter().any(|frag| match frag {
                Fragment::Lit(_) => false,
                Fragment::Src(span) => {
                    s >= usize::from(span.start()) && e <= usize::from(span.end())
                }
            });
            in_own || in_src
        })
        .collect()
}

/// Materialize a template's fragments into `out`. `Src` passthrough spans are
/// emitted from original source with the contained sub-edits (indices into
/// `all`) applied.
fn materialize_fragments(
    out: &mut String,
    frags: &[Fragment],
    source: &str,
    all: &[(usize, usize, SubPatch)],
    contained: &[usize],
) {
    for (i, frag) in frags.iter().enumerate() {
        match frag {
            Fragment::Lit(s) => out.push_str(s),
            Fragment::Src(span) => {
                // a zero-width insertion at this span's end is normally deferred
                // to the *next* `Src` span (which re-emits it at its start), so
                // two adjacent passthroughs don't both emit it. but when no
                // adjacent `Src` follows (the next fragment is literal text, or
                // this is the last fragment) there is nothing to defer it to, so
                // this span must emit it — otherwise a wrap whose closing token
                // sits at the span boundary (a reified `[1]` → `list[int]([1])`)
                // loses that token
                let include_end = !matches!(
                    frags.get(i + 1),
                    Some(Fragment::Src(next)) if next.start() == span.end()
                );
                apply_within(
                    out,
                    source,
                    usize::from(span.start()),
                    usize::from(span.end()),
                    all,
                    contained,
                    include_end,
                );
            }
        }
    }
}

/// Emit `source[s0..e0]` with the edits from `contained` (indices into `all`,
/// in position order) that fall inside the span applied, first-wins on
/// overlap. Nested templates recurse; a same-start insertion at depth ≥ 2 is
/// emitted ahead of its nested template rather than absorbed into it (only the
/// top-level claim pass implements absorption). `statement_expression` nests
/// templates — one statement expression inside another's suite, and any pass
/// whose template lands inside the suite it passes through. `include_end` controls
/// whether a zero-width insertion exactly at `e0` is emitted here (see
/// [`materialize_fragments`]).
fn apply_within(
    out: &mut String,
    source: &str,
    s0: usize,
    e0: usize,
    all: &[(usize, usize, SubPatch)],
    contained: &[usize],
    include_end: bool,
) {
    let mut cursor = s0;
    let mut k = 0;
    while k < contained.len() {
        let idx = contained[k];
        let (s, e) = (all[idx].0, all[idx].1);
        // outside this span, a deferred boundary insertion at its end, or
        // overlapping an already-applied edit — skip
        if s < cursor || s < s0 || e > e0 || (!include_end && s == e && s == e0) {
            k += 1;
            continue;
        }
        out.push_str(&source[cursor..s]);
        match &all[idx].2 {
            SubPatch::Text(t) => out.push_str(t),
            SubPatch::Template(frags) | SubPatch::Statement(frags) => {
                let inner: Vec<usize> = contained[k + 1..]
                    .iter()
                    .copied()
                    .filter(|&m| all[m].0 >= s && all[m].1 <= e && all[m].0 != e)
                    .collect();
                materialize_fragments(out, frags, source, all, &inner);
            }
        }
        cursor = cursor.max(e);
        k += 1;
    }
    out.push_str(&source[cursor..e0]);
}

/// Coalesce repeated `from <module> import X` lines into a single
/// `from <module> import X, Y, ...` line. Preserves any non-matching
/// lines (e.g. `import foo`, `_MISSING = object()`) in their original
/// order. Names within a merged line are sorted and deduped
fn merge_from_imports(lines: Vec<String>) -> Vec<String> {
    // preserve first-seen module order so tests that depend on specific
    // import sequence (e.g. `from typing import TypeVar, Generic` before
    // `from typing import Final`) stay stable. names within a module
    // also keep first-seen order (deduped)
    let mut groups: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
    let mut other: Vec<String> = Vec::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("from ")
            && let Some((module, names)) = rest.split_once(" import ")
        {
            let entry = groups.entry(module.trim().to_owned()).or_default();
            for name in names.split(',') {
                let name = name.trim().to_owned();
                if !name.is_empty() && !entry.contains(&name) {
                    entry.push(name);
                }
            }
            continue;
        }
        other.push(line);
    }
    // `from` imports first (first-seen module order), then raw lines
    // (synthesized class defs etc.) so any class body referencing imported
    // names sees them already in scope
    let mut from_lines: Vec<String> = groups
        .into_iter()
        .map(|(module, names)| format!("from {module} import {}", names.join(", ")))
        .collect();
    from_lines.extend(other);
    from_lines
}

/// Run every registered AST pass against `source` and splice the rewritten
/// statements back into the source text. Returns a borrowed `Cow` when
/// nothing changed.
///
/// `project`, when `Some`, supplies the real project db + file so type-aware
/// passes resolve cross-module imports (e.g. an imported generic function for
/// `generic_call`). The chosen db owns the parse the type-aware passes query:
/// `inferred_type` does AST node-identity lookups, so the model and the walked
/// suite must come from one db
pub(crate) fn run_against_source<'a>(
    source: &'a str,
    config: &Config,
    project: Option<(&dyn ty_python_semantic::Db, ruff_db::files::File)>,
) -> (Cow<'a, str>, Vec<String>, Vec<Option<u32>>) {
    // blank the keyword-prefix type markers — use-site variance and the
    // `literal`/`final` type modifiers — out up front; downstream passes
    // (callable, intersection) copy operand source verbatim and would leak the
    // keywords otherwise. blanking is length-preserving, so every range below
    // is valid in both the original and the blanked source
    let blanked = use_site_variance::blank(source);
    let source_ref: &str = blanked.source.as_ref();

    // the db keeps the *original* source, markers and all, so ty can answer
    // questions that depend on a use-site projection (`x is A[out int]`).
    // that's sound precisely because blanking preserves byte positions: the
    // db's parse and the blanked parse below agree on every node's range
    let sem = match project {
        Some((pdb, pfile)) => SemDb::Project(pdb, pfile),
        None => {
            let (db, file) = crate::make_in_memory_db(source);
            SemDb::Local(db, file)
        }
    };
    let (sem_db, sem_file): (&dyn ty_python_semantic::Db, ruff_db::files::File) = match &sem {
        SemDb::Project(db, f) => (*db, *f),
        SemDb::Local(db, f) => (db, *f),
    };
    let parsed_handle =
        ruff_db::parsed::parsed_module(sem_db, sem_db.program_file(sem_file).python_file(sem_db))
            .load(sem_db);
    let semantic_model =
        ty_python_semantic::SemanticModel::new(sem_db, sem_db.program_file(sem_file));

    // identity line table for the no-change early returns: stripping variance
    // is within-line, so every line still maps to itself
    if !parsed_handle.errors().is_empty() {
        let cow = blanked.stripped(source);
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, Vec::new(), table);
    }
    let parsed = parse_unchecked_source(source_ref, PySourceType::BasedPython);
    if !parsed.errors().is_empty() {
        let cow = blanked.stripped(source);
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, Vec::new(), table);
    }
    // the parentheses grouping an expression are visible in the tokens and
    // nowhere in the AST, so anything that needs them has to measure now — the
    // tokens go when the syntax tree is taken out of the parse
    let accessor_value_ranges =
        properties::collect_value_ranges(&parsed.syntax().body, parsed.tokens());
    let mut module = parsed.into_syntax();
    // capture each top-level statement's original source range before any
    // pass mutates the AST. AST mutations replace nodes with synthesised
    // ones whose ranges are zeroed (default `TextRange`), so the splice
    // driver can't rely on `stmt.range()` after the passes run
    let original_ranges: Vec<(usize, usize)> = module
        .body
        .iter()
        .map(|s| (usize::from(s.range().start()), usize::from(s.range().end())))
        .collect();
    let mut ctx = PassContext::default();

    let coalesce_inner = coalesce_chain::CoalesceFold::new();
    let coalesce_pass = VisitorPass {
        inner: &coalesce_inner,
        changed_cell: coalesce_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    // resolve symbolic operations in type positions (`1 + 1` → `Literal[2]`)
    // up front, from the original parse where `typeof` operands are still
    // intact for ty to read. the pass replaces each operation node and must run
    // before `typeof` lowering so a `typeof` operand is consumed here
    // `literal str` is the one use-site modifier python can spell, so it lowers
    // to `LiteralString` rather than being erased with the other markers. it has
    // to be collected from the db's own (marker-bearing) parse, since the
    // blanked copy the passes walk no longer has a `literal` keyword in it
    let literal_string_rewrites = literal_string::collect(parsed_handle.suite(), &semantic_model);

    let symbolic_folds =
        symbolic_type_op::collect_symbolic_folds(parsed_handle.suite(), &semantic_model);
    let symbolic_needs_literal_import = symbolic_folds.needs_literal_import;
    let symbolic_needs_any_import = symbolic_folds.needs_any_import;
    ctx.claimed_type_op_ranges = symbolic_folds.claimed_ranges();
    ctx.symbolic_substitutions = symbolic_folds.substitutions();
    let symbolic_pass = symbolic_type_op::SymbolicTypeOp::new(symbolic_folds);

    // a `typeof` nested under a structural type-form (`&` / `or` / `not` / an
    // arrow) belongs to the type-expression lowerer's wide edit; the fold
    // skips those so its statement re-render doesn't drop that edit
    let typeof_skip = typeof_keyword::collect_structural_typeof_ranges(
        parsed_handle.suite(),
        &semantic_model,
        &ctx.claimed_type_op_ranges,
    );
    let typeof_inner = typeof_keyword::TypeofFold::new(typeof_skip);
    let typeof_pass = VisitorPass {
        inner: &typeof_inner,
        changed_cell: typeof_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    let tuple_index_pass = tuple_index::TupleIndexPass::new();

    let sentinel_inner = sentinel::Sentinel::new();
    let sentinel_pass = VisitorPass {
        inner: &sentinel_inner,
        changed_cell: sentinel_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    let repeated_underscore_inner = repeated_underscore::RepeatedUnderscore::new();
    let repeated_underscore_pass = VisitorPass {
        inner: &repeated_underscore_inner,
        changed_cell: repeated_underscore_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    let typed_lambda_inner = typed_lambda::TypedLambda::new(source_ref);
    let typed_lambda_pass = VisitorPass {
        inner: &typed_lambda_inner,
        changed_cell: typed_lambda_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    let export_import_pass = export_import::ExportImport::new(source_ref);
    let dynamic_keyword_pass = dynamic_keyword::DynamicKeywordPass::new();
    let character_type_pass = character_type::CharacterTypePass::new();
    let grapheme_string_pass = grapheme_string::GraphemeStringPass::new();
    let type_is_pass = type_is::TypeIs::new(source_ref);
    let top_star_pass = top_star::TopStar::new();
    let identity_swap_pass = identity_swap::IdentitySwap::new(source_ref);
    let compat_pass = compat::CompatRewrite::new(source_ref, config.clone());
    let string_tag_pass = string_tag::StringTagPass::new(source_ref, config.clone());
    let dedent_string_pass = dedent_string::DedentString::new(source_ref);
    let super_keyword_pass = super_keyword::SuperKeyword::new();
    let postfix_await_pass = postfix_await::PostfixAwait::new(source_ref);
    let mutable_defaults_pass = mutable_defaults::MutableDefaultsPass::new(source_ref);
    let unique_loop_bindings_pass =
        unique_loop_bindings::UniqueLoopBindingsPass::new(source_ref, config.unique_loop_bindings);
    let auto_quote_pass = auto_quote::AutoQuote::new(
        source_ref,
        config.min_version,
        config.inject_future_annotations,
    );
    let init_method_pass = init_method::InitMethod::new(source_ref);
    let properties_pass = properties::PropertiesPass::new(source_ref, accessor_value_ranges);
    let local_once_pass = local_once::LocalOncePass::new(source_ref);
    let raises_strip_pass = raises_clause::RaisesStripPass::new(source_ref);
    let return_value_use_pass = return_value_use::ReturnValueUsePass::new(
        source_ref,
        return_value_use::collect(parsed_handle.suite(), &semantic_model),
    );
    let raises_guard_pass =
        raises_clause::RaisesGuardPass::new(source_ref, config.runtime_raises_checks);
    let type_fn_pass = type_fn::TypeFnPass::new(source_ref);
    let match_type_pass = match_type::MatchTypePass::new(source_ref);
    let modifiers_pass = modifiers::ModifiersPass::new(source_ref);
    let main_function_pass = main_function::MainFunction::new(source_ref, config.is_stub);
    let empty_declarations_pass = empty_declarations::EmptyDeclarations::new();
    let overload_pass = overload::Overload::new(source_ref, config.is_stub);
    let decorator_keyword_pass = decorator_keyword::DecoratorKeyword::new(source_ref);
    let unpack_pass = unpack::UnpackSyntax::new(config.clone());
    let typed_dict_literal_pass = typed_dict_literal::TypedDictLiteralPass::new(source_ref);
    let just_float_pass = just_float::JustFloatPass::new();
    let float_const_pass = float_const::FloatConstPass::new();
    let kw_subscript_pass = kw_subscript::KwSubscriptPass::new(source_ref, config.min_version);
    let generic_call_pass = generic_call::GenericCallStripPass::new(source_ref);
    let reified_generic_pass =
        reified_generic::ReifiedGenericPass::new(source_ref, config.min_version);
    let type_reification_pass =
        type_reification::TypeReificationPass::new(config.min_version, config.is_stub);
    let private_method_pass = private_method::PrivateMethodPass;
    let parametric_is_pass = parametric_is::ParametricIsPass::new(source_ref);
    let implicit_typing_pass = implicit_typing::ImplicitTypingPass::new();
    let inferred_annotation_pass = inferred_annotation::InferredAnnotationPass::new();
    let template_type_pass = template_type::TemplateTypePass;
    let tuple_types_pass = annotation::TupleLiteralTypePass::new(source_ref, config.clone());
    let literal_types_pass = literal_types::LiteralTypePass::new(source_ref);
    let callable_pass = callable::CallableSyntaxPass::new(source_ref);
    let protocol_type_pass = protocol_type::ProtocolTypePass::new(source_ref, config.clone());
    let coalesce_text_pass = coalesce::NoneCoalescePass::new(source_ref);
    let force_unwrap_pass = force_unwrap::ForceUnwrapPass::new(source_ref);
    let some_ctor_pass = some_ctor::SomeCtorPass::new();
    let propagate_pass = propagate::PropagatePass::new(source_ref);
    let none_chain_pass = none_chain::NoneChainPass::new(source_ref);
    let optional_type_pass = optional_type::OptionalTypePass::new(source_ref, config.min_version);
    let runtime_union_pass = runtime_union::RuntimeUnionPass::new(config.min_version);
    let generics_pass = generics::GenericPolyfillPass::new(source_ref, config.clone());
    let soundness_pass = soundness::SoundnessPass::new(source_ref, config);
    let checked_cast_pass = checked_cast::CheckedCastPass;
    let trailing_lambda_pass = trailing_lambda::TrailingLambdaPass::new(source_ref);
    let if_let_pass = if_let::IfLetPass::new(source_ref);
    let destructure_pass = destructure::DestructurePass::new(source_ref);
    let statement_expression_pass = statement_expression::StatementExpressionPass::new(source_ref);
    let context_params_pass = context_params::ContextParamsPass::new(source_ref);
    let extension_block_pass = extension::ExtensionBlockPass::new(source_ref);
    let extension_call_pass = extension::ExtensionCallPass;
    let witness_dispatch_pass = conformance::WitnessDispatchPass;
    let conversion_pass = conversion::ConversionPass::new(source_ref);
    let implicit_receiver_pass = implicit_receiver::ImplicitReceiverPass;
    let django_lookup_pass = django_lookup::DjangoLookupPass;
    let frameworks_pass = frameworks::FrameworksPass::new(source_ref);
    let variance_pass = decl_site_variance::VarianceStripPass::new(source_ref);
    let anon_named_tuple_pass =
        anon_named_tuple::AnonNamedTuplePass::new(source_ref, config.clone());

    // Order matters: passes that read source ranges via `text_edits` mode
    // must run BEFORE passes that mutate the AST (which zero source ranges
    // on synthesised nodes). All text-edit-emitting passes here read AST
    // node ranges to compute their edits; once another pass replaces an
    // Expr wholesale, its range is `TextRange::default()` and source lookups
    // are invalid.
    let passes: &[&dyn AstPass] = &[
        // a statement expression moves its enclosing statement's assignment
        // below a suite; every other lowering inside that suite composes through
        // the passthrough spans it emits, so it goes first
        &statement_expression_pass,
        // destructuring: the `let` statement, patterns in binding positions, and
        // the `and` pattern. Like `if let` it replaces headers only, so bodies
        // keep their source bytes and the lowerings inside them compose
        &destructure_pass,
        // text-edit-emitting passes first (read source ranges).
        // type_is must run before identity_swap so type-position `a is T`
        // wins the first-wins overlap dedup over identity_swap's
        // value-context `isinstance(a, T)` rewrite
        &type_is_pass,
        // `from x export y` → `from x import y as y`: two source edits inside
        // an import statement, independent of every other pass
        &export_import_pass,
        &top_star_pass,
        &identity_swap_pass,
        &compat_pass,
        // a custom string tag wraps a template literal whose interpolations may
        // themselves lower; its template-edit passes interpolation source
        // through as `Src` fragments so those inner edits still compose
        &string_tag_pass,
        &dedent_string_pass,
        &super_keyword_pass,
        &postfix_await_pass,
        &auto_quote_pass,
        // strip `local` / `once` parameter modifiers (source-span deletions,
        // like init_method's `let` handling — must read ranges before any
        // AST-mutation pass zeroes them)
        &local_once_pass,
        // delete `raises` clauses (a source-span deletion, like `local` / `once`
        // — must read ranges before any AST-mutation pass zeroes them)
        &raises_strip_pass,
        // delete the return-value markers. it also strips them from the AST, so
        // it has to run before any pass re-renders a statement one sits on
        &return_value_use_pass,
        // erase `type def` declarations; their applications were already folded to
        // the resolved type by the symbolic pass above
        &type_fn_pass,
        // replace a match type's `case` blocks with a runtime value, and strip
        // `TypeVarTuple` bounds wherever they appear
        &match_type_pass,
        &modifiers_pass,
        // after modifiers so the entry-point guard follows any `__all__` it
        // emits, and before the AST-mutation passes so `main`'s decorator
        // ranges are still valid for the `private` check
        &main_function_pass,
        &empty_declarations_pass,
        &overload_pass,
        &decorator_keyword_pass,
        &unpack_pass,
        &typed_dict_literal_pass,
        // AST-mutation passes second (may zero node ranges).
        // symbolic_type_op replaces whole operation nodes (consuming any
        // `typeof` operand) and reads original source ranges, so it must run
        // first among the mutation passes — before `typeof` and before any
        // pass that zeroes ranges
        &symbolic_pass,
        &coalesce_pass,
        &typeof_pass,
        &sentinel_pass,
        &repeated_underscore_pass,
        &typed_lambda_pass,
    ];
    for pass in passes {
        pass.run(&mut module, &mut ctx);
    }

    // type-aware passes: operate on the salsa-owned parsed module (so
    // semantic queries hit the right AST nodes), emit text_edits / imports
    let type_aware: &[&dyn TypeAwarePass] = &[
        // framework gates only push errors (no edits), so their position is
        // order-independent; first, so a hard incompatibility surfaces
        // before any edit-conflict noise
        &frameworks_pass,
        // the `raises` runtime guard is a decorator inserted at the start of the
        // `def` line, so it composes with every edit inside the signature and
        // body (the clause deletion among them)
        &raises_guard_pass,
        // `init(...)` shorthand: rewrite to `def __init__`, strip `let`, and
        // synthesize `self.<name>: <ann> = <name>`. type-aware because the
        // synthesized annotation is fresh output that must reproduce whatever
        // lowering the parameter's own annotation gets (a callable arrow, a
        // `T?`, a bare `float`); the imports / hoisted classes those need are
        // requested by the sibling passes' visit of the same parameter
        &init_method_pass,
        // property accessor blocks: replace the whole `var`/`let` + `get`/`set`
        // construct with the python `@property` members the parser already
        // synthesized. it claims the construct span as one template, so it runs
        // among the early template-claiming passes; type positions and the
        // backing initialiser pass through as `Src` and still compose
        &properties_pass,
        // soundness wraps whole gated expressions in `_soundness_check(...)`
        // template edits; it runs first so an equal-span template from a
        // later pass (e.g. coalesce on a wrapped iterable) is claimed and
        // materialized inside the check rather than dropping it
        &soundness_pass,
        // a `private` method is reached by its mangled name; the edit replaces
        // the attribute identifier alone, so it composes with any rewrite of the
        // receiver it is read from
        &private_method_pass,
        // checked cast wraps `<value> cast? <type>` in `_checked_cast(...)`; its
        // template passes value + type through as `Src`, so lowerings inside
        // them (a `??` value, a `T?` type) still compose
        &checked_cast_pass,
        // trailing lambda blocks re-emit a whole statement as a def + call
        // template; the suite and the called expression pass through as `Src`,
        // so lowerings inside them (including nested trailing lambdas) are
        // claimed and materialized in place
        &trailing_lambda_pass,
        // `if let <pattern> := <subject>:` chains flatten onto a selector
        // variable. only the clause headers are replaced, so every body keeps
        // its source bytes and the lowerings inside them compose
        &if_let_pass,
        // context parameters: strip `context` prefixes, lower `context NAME =`
        // declarations, and append the resolved implicit arguments before each
        // call's closing paren. single insertions, so they compose inside any
        // wrapping template's `Src` spans (including the trailing-lambda one)
        &context_params_pass,
        // `extension` blocks lower to module-level backing functions; member
        // bodies pass through as `Src` spans so lowerings inside them (and
        // the call rewrites below) still compose
        &extension_block_pass,
        // type-directed rewrite of attribute accesses ty resolved to
        // extension members (`xs.second()` → `_by_ext__list__second(xs)`)
        &extension_call_pass,
        // a protocol *requirement* read off an interface-typed receiver goes
        // through the witness table its conformance registered, since the value
        // may carry no such attribute of its own. disjoint from the extension
        // rewrite above, which only fires where normal member lookup found
        // nothing — a requirement is a member of the interface
        &witness_dispatch_pass,
        // every conversion site the checker accepted wraps its value. reads
        // source ranges, so it runs alongside the extension passes and before
        // the AST-mutating ones
        &conversion_pass,
        // implicit receivers: `x.fn()` → `fn(x)` for a receiver callable in
        // scope, and a trailing lambda block's `self` / unqualified receiver
        // members → its receiver parameter. same shape as the extension rewrite
        // above, which wins when both could apply
        &implicit_receiver_pass,
        // django lookups written as expressions (`filter(author.name == "x")`)
        // become keyword arguments. the argument is replaced whole, with the
        // value passing through as `Src`, so lowerings inside it still compose;
        // disjoint from the call rewrites above, which target the callee
        &django_lookup_pass,
        &dynamic_keyword_pass,
        // import-only companion to the ty-side implicit `Character` resolution;
        // emits no text edits, so ordering among the type passes is free
        &character_type_pass,
        &just_float_pass,
        &float_const_pass,
        &kw_subscript_pass,
        // reified generics wrap `def f[T]` (value-position `T`) in `@generic`;
        // must precede generic_call so the call-site strip skips the wrapped
        // function's specialized calls (they route through `generic.__getitem__`)
        &reified_generic_pass,
        &generic_call_pass,
        // type reification: bare generic constructor calls get their solved
        // specialization (`A(1)` → `A[int](1)`) and collection literals their
        // inferred element types (`[1, 2]` → `list[int]([1, 2])`). disjoint
        // from the two passes above — they handle *function* callees, this one
        // class callees and displays
        &type_reification_pass,
        // parametric type tests (`x is list[int]`): identity_swap leaves
        // keyword-form `is` pairs with a subscripted rhs for this pass, which
        // resolves them rust-style from static types (fold / reified-cell
        // token equality / witness probe / unchecked runtime probe)
        &parametric_is_pass,
        &implicit_typing_pass,
        // synthesize declared types for bare class-body assignments
        // (`class A: a = 1` → `a: int = 1`); a zero-width insertion at the
        // target name, disjoint from the value-position lowerings above
        &inferred_annotation_pass,
        // an f-string in a type position replaces its whole span with the
        // python spelling of the type ty resolved it to. it runs before the
        // type-position passes below so its wide edit claims the holes, whose
        // own lowerings would otherwise be emitted for source that is gone
        &template_type_pass,
        &tuple_types_pass,
        &literal_types_pass,
        &callable_pass,
        // inline protocols hoist to a synthesized `Protocol` class; the wide
        // replacement covers the whole `protocol(...)` span, so it must follow
        // `callable` — whose visit of the same member types emits the imports
        // and `_Callable_*` classes our re-rendered copies name
        &protocol_type_pass,
        // `T?` → `T | None`; a type-position edit, disjoint from the
        // value-position `??` / `?.` lowerings below
        &optional_type_pass,
        // a PEP 604 union the runtime will evaluate (`isinstance(x, int | str)`)
        // is spelled the way the target can. its template covers the whole
        // union, and the sort puts a wider replacement first, so the lowerings
        // inside each arm are materialized rather than dropped
        &runtime_union_pass,
        // coalesce sees `?.` LHS via source ranges; must run BEFORE
        // none_chain so its wider `??` edit wins over none_chain's narrow
        // `?.` edit when both target the same span
        &coalesce_text_pass,
        // `expr!` → `_force_unwrap(expr)`; narrow insert/replace edits that compose
        // with sibling operator lowerings inside the operand
        &force_unwrap_pass,
        // `expr.N` → `expr[N]`; a narrow replacement of the `.N` bytes only
        &tuple_index_pass,
        // grapheme string surface (`s.character_count` → `len(_by_graphemes(s))`,
        // …); receiver spans pass through as `Src` fragments, so sibling
        // lowerings inside compose
        &grapheme_string_pass,
        // mutable defaults → `_MISSING` sentinel swap + body-prologue guard;
        // narrow edits, so the function body's own lowerings still apply
        &mutable_defaults_pass,
        // per-iteration loop bindings: a closure made in a loop body is applied
        // to the loop's values through a wrapper (or, for a `def`, gets a
        // closure-rebinding decorator). the closure passes through as `Src`, so
        // every lowering inside it still composes. after reified generics, so
        // the `@generic` line stays above the rebind and receives the rebuilt
        // function
        &unique_loop_bindings_pass,
        // `Some(x)` → `Optional(x)`; a narrow identifier rename
        &some_ctor_pass,
        // `expr^` → guard hoisted before the enclosing statement + unwrapped value
        &propagate_pass,
        &none_chain_pass,
        // generics emits wide replacements covering whole type-params
        // headers; variance's narrow def-site deletion gets dropped by
        // first-wins dedup when generics fires (3.10), survives when
        // generics doesn't (3.12+ native PEP 695)
        &generics_pass,
        &variance_pass,
        // anon_named_tuple must run BEFORE tuple_types so its outer-region
        // edits win when isolation conflicts arise — but tuple_types is
        // already earlier in this list. The cleanup-loop in lib.rs catches
        // anon-NT spans generics polyfill leaked verbatim into class headers
        &anon_named_tuple_pass,
    ];
    for pass in type_aware {
        pass.run(parsed_handle.suite(), &semantic_model, &mut ctx);
    }

    // collect import requests the inner passes raised at the end of their run
    if typeof_inner.ever_changed() {
        ctx.required_imports
            .push("from ty_extensions import TypeOf".to_owned());
    }
    // symbolic folds that produced a `Literal[..]` need the import, unless the
    // source already binds `Literal`
    if symbolic_needs_literal_import && !literal_types::literal_already_imported(&semantic_model) {
        ctx.required_imports
            .push("from typing import Literal".to_owned());
    }
    // symbolic folds that produced `Any` (e.g. `dynamic + 1`) need the import,
    // unless the source already binds `Any`
    if symbolic_needs_any_import && !semantic_model.is_bound_globally("Any") {
        ctx.required_imports
            .push("from typing import Any".to_owned());
    }
    // typed lambdas are removed as source deletions so the statement around
    // them is never re-rendered (see `typed_lambda`); collect them here
    ctx.text_edits.extend(typed_lambda_inner.take_edits());
    if sentinel_inner.ever_changed() {
        ctx.required_imports
            .push("from typing_extensions import Sentinel".to_owned());
    }

    // collapse the padding `blank` left behind. these are ordinary edits, so a
    // statement an AST pass re-rendered (which never had the marker in its
    // AST) and a wider template edit (which materializes them inside its `Src`
    // spans) both come out clean; only a wider *plain* text edit keeps the
    // padding, which is valid Python either way
    for range in &blanked.ranges {
        // a marker rewritten to `LiteralString` has its whole range replaced
        // below; emitting the keyword collapse too would leave two plain text
        // edits racing for the same start offset
        if literal_string_rewrites.covers(*range) {
            continue;
        }
        ctx.text_edits
            .push((*range, use_site_variance::collapsed_to(source_ref, *range)));
    }
    for (range, replacement) in literal_string_rewrites.edits() {
        ctx.text_edits.push((range, replacement));
    }
    if literal_string_rewrites.needs_import {
        ctx.required_imports
            .push("from typing import LiteralString".to_owned());
    }

    // a synthesized annotation may name a class the source never imported. the
    // import goes under `if TYPE_CHECKING:` as one block: every such annotation
    // is a string at runtime (the lowering always emits the `__future__` import),
    // so only a checker ever reads the name, and a real import would give the
    // output an import edge — and a possible cycle — the source never had
    if !ctx.type_only_imports.is_empty() {
        let mut block = String::from("from typing import TYPE_CHECKING\nif TYPE_CHECKING:");
        for line in std::mem::take(&mut ctx.type_only_imports) {
            block.push_str("\n    ");
            block.push_str(&line);
        }
        ctx.required_imports.push(block);
    }

    ctx.required_imports.sort();
    ctx.required_imports.dedup();
    ctx.required_imports = merge_from_imports(std::mem::take(&mut ctx.required_imports));
    ctx.changed.sort_unstable();
    ctx.changed.dedup();

    if ctx.changed.is_empty()
        && ctx.required_imports.is_empty()
        && ctx.hoisted.is_empty()
        && ctx.text_edits.is_empty()
        && ctx.template_edits.is_empty()
        && ctx.statement_inserts.is_empty()
        && ctx.epilogue.is_empty()
    {
        let cow = blanked.stripped(source);
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, ctx.errors, table);
    }

    // splice changed statements back into the source string. process highest
    // index first so byte offsets in unmodified prefixes stay valid through
    // the loop. hoisted statements are emitted as text just before the
    // splice for their target idx
    let mut hoisted_by_idx: std::collections::BTreeMap<usize, Vec<Stmt>> =
        std::collections::BTreeMap::new();
    for (idx, stmt) in ctx.hoisted {
        hoisted_by_idx.entry(idx).or_default().push(stmt);
    }

    let original_body = &module.body;
    let mut all_idx: std::collections::BTreeSet<usize> = ctx.changed.iter().copied().collect();
    for k in hoisted_by_idx.keys() {
        all_idx.insert(*k);
    }

    // only statements an AST pass actually re-rendered occupy their range —
    // a hoist-only target keeps its original text (the hoists are emitted as a
    // zero-width insertion before it), so sub-statement edits inside it still
    // apply
    let occupied_ranges: Vec<(usize, usize)> =
        ctx.changed.iter().map(|&i| original_ranges[i]).collect();
    let overlaps = |start: usize, end: usize| -> bool {
        occupied_ranges.iter().any(|(s, e)| start < *e && *s < end)
    };

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for idx in all_idx.iter().copied() {
        let (start, end) = original_ranges[idx];
        let line_indent = {
            let prefix = &source_ref[..start];
            let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
            &source_ref[line_start..start]
        }
        .to_owned();

        let mut block = String::new();
        if let Some(hoists) = hoisted_by_idx.remove(&idx) {
            for h in hoists {
                let rendered = render_stmt(&h).trim_end_matches('\n').to_owned();
                block.push_str(&rendered);
                block.push('\n');
                block.push_str(&line_indent);
            }
        }

        if ctx.changed.binary_search(&idx).is_ok() {
            let rendered = render_stmt(&original_body[idx]);
            // render_stmt emits a trailing newline. drop it when the source
            // already has one immediately after the stmt (avoids `\n\n`); keep
            // it when the stmt is at end-of-file with no trailing newline so
            // we don't lose multi-line structure
            let source_has_trailing_newline = source_ref.as_bytes().get(end) == Some(&b'\n');
            if source_has_trailing_newline {
                block.push_str(rendered.trim_end_matches('\n'));
            } else {
                block.push_str(&rendered);
            }
            edits.push((start, end, block));
        } else if !block.is_empty() {
            // hoist-only: insert the hoisted lines before the statement and
            // leave its source bytes (and any edits inside them) in place
            edits.push((start, start, block));
        }
    }
    // ruff-style first-wins dedup for sub-statement edits. sort by start; skip
    // any edit whose start is before the running cursor (overlaps a prior
    // edit) or which collides with a whole-statement splice. zero-width
    // insertions (start == end) at the cursor are allowed — they consume no
    // source bytes so multiple insertions + a deletion at the same position
    // can compose. a plain-text edit wins over anything nested inside it; a
    // template edit instead *materializes* nested edits within its `Src`
    // passthrough spans, so wide rewrites compose with inner lowerings
    let mut sub_edits: Vec<(usize, usize, SubPatch)> = ctx
        .text_edits
        .into_iter()
        .map(|(r, s)| {
            (
                usize::from(r.start()),
                usize::from(r.end()),
                SubPatch::Text(s),
            )
        })
        .chain(ctx.template_edits.into_iter().map(|(r, frags)| {
            (
                usize::from(r.start()),
                usize::from(r.end()),
                SubPatch::Template(frags),
            )
        }))
        .chain(ctx.statement_inserts.into_iter().map(|(at, frags)| {
            let at = usize::from(at);
            (at, at, SubPatch::Statement(frags))
        }))
        .collect();
    // start asc. tie-break by edit shape:
    //   1. zero-width insertions first — they don't consume bytes, so any
    //      following deletion/replacement at the same start can still apply.
    //      a statement-anchored insertion leads them: it emits whole statements
    //      that must precede everything the statement itself lowers to
    //   2. then wider replacements before narrower ones — so a wider edit
    //      wins over (or, for templates, absorbs) a narrow one nested inside
    //      it
    //   3. at one identical span, a *substitution* — plain text, or a template
    //      with no `Src` passthrough — ahead of a *rewrite*, a template that
    //      re-emits part of the span. a substitution says the construct does not
    //      appear here at all, which a rewrite of it cannot outrank: the pass
    //      that substitutes may be relocating the construct (default
    //      re-evaluation moves a parameter default into the body), and the
    //      rewrite still materializes wherever the passthrough re-emits it
    sub_edits.sort_by(|a, b| {
        let priority = |e: &(usize, usize, SubPatch)| {
            let rewrites = i64::from(match &e.2 {
                SubPatch::Text(_) => false,
                SubPatch::Template(frags) | SubPatch::Statement(frags) => {
                    frags.iter().any(|frag| matches!(frag, Fragment::Src(_)))
                }
            });
            let statement = i64::from(!matches!(e.2, SubPatch::Statement(_)));
            // (start, is_replacement_not_insertion, statement-insert-first,
            //  neg_end-for-wider-first, substitution-before-rewrite)
            if e.1 == e.0 {
                (e.0, 0i64, statement, 0i64, rewrites) // insertion
            } else {
                #[allow(clippy::cast_possible_wrap)]
                let neg_end = -(e.1 as i64);
                (e.0, 1i64, statement, neg_end, rewrites)
            }
        };
        priority(a).cmp(&priority(b))
    });
    // claim pre-pass: each replacement, outermost-first (the sort guarantees an
    // enclosing edit precedes anything inside it), claims the edits nested in
    // its span. a template *materializes* its claimees inside its `Src` spans;
    // a plain-text replacement drops them (first-wins). same-start zero-width
    // insertions are absorbed by a template (they target the construct's first
    // token, e.g. `_force_unwrap(` ahead of a coalesce operand) but stay
    // independent ahead of a plain-text replacement, preserving the documented
    // insertion + deletion compose behaviour. the one exception is a
    // statement-anchored insertion sharing a boundary: it emits statements, so
    // absorbing it into an expression rewrite of the statement it precedes
    // would splice a suite into the middle of an expression
    let mut claimed = vec![false; sub_edits.len()];
    for i in 0..sub_edits.len() {
        let (s_i, e_i) = (sub_edits[i].0, sub_edits[i].1);
        if e_i == s_i || claimed[i] {
            continue;
        }
        let is_template = matches!(
            sub_edits[i].2,
            SubPatch::Template(_) | SubPatch::Statement(_)
        );
        for (m, edit) in sub_edits.iter().enumerate() {
            if m == i || claimed[m] {
                continue;
            }
            let (s_m, e_m) = (edit.0, edit.1);
            let inside = s_m >= s_i && e_m <= e_i && s_m != e_i;
            let boundary_insertion = s_m == e_m && (s_m == s_i || s_m == e_i);
            let anchored = matches!(edit.2, SubPatch::Statement(_));
            if inside && (is_template || !boundary_insertion) && !(boundary_insertion && anchored) {
                claimed[m] = true;
            }
        }
    }

    let mut dropped_by_splice: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    let mut i = 0;
    while i < sub_edits.len() {
        if claimed[i] {
            i += 1;
            continue;
        }
        let (start, end) = (sub_edits[i].0, sub_edits[i].1);
        if start < cursor {
            i += 1;
            continue;
        }
        if overlaps(start, end) {
            if end > start {
                dropped_by_splice.push((start, end));
            }
            i += 1;
            continue;
        }
        // coalesce all unclaimed zero-width insertions sharing this start into
        // a single combined insertion (text concatenated in push order). this
        // sidesteps the replace_range-at-same-position ordering issue: each
        // pass pushes its slice in left-to-right intent order, and we
        // splice them as one contiguous string
        if end == start {
            let mut combined = String::new();
            let mut j = i;
            while j < sub_edits.len() && sub_edits[j].0 == start && sub_edits[j].1 == start {
                if !claimed[j] {
                    match &sub_edits[j].2 {
                        SubPatch::Text(t) => combined.push_str(t),
                        SubPatch::Template(frags) | SubPatch::Statement(frags) => {
                            let contained = template_claimees(frags, &sub_edits, &claimed, j, None);
                            materialize_fragments(
                                &mut combined,
                                frags,
                                source_ref,
                                &sub_edits,
                                &contained,
                            );
                        }
                    }
                }
                j += 1;
            }
            edits.push((start, start, combined));
            i = j;
            continue;
        }
        let repl = match &sub_edits[i].2 {
            // a plain-text replacement wins over anything inside it
            SubPatch::Text(t) => t.clone(),
            SubPatch::Template(frags) | SubPatch::Statement(frags) => {
                // the claimees nested in this span materialize inside the
                // template's `Src` passthrough fragments
                let contained =
                    template_claimees(frags, &sub_edits, &claimed, i, Some((start, end)));
                let mut out = String::new();
                materialize_fragments(&mut out, frags, source_ref, &sub_edits, &contained);
                out
            }
        };
        edits.push((start, end, repl));
        cursor = end;
        i += 1;
    }

    // composition invariant: an edit dropped because an AST pass re-rendered
    // its enclosing statement is fine when the pass consumed the construct,
    // but a leak when the re-render reprinted it. detect the leak precisely
    // rather than letting it surface as a confusing syntax error downstream
    for (start, end) in dropped_by_splice {
        let construct = &source_ref[start..end];
        let leaked = edits.iter().any(|(bs, be, btext)| {
            *be > *bs && *bs <= start && end <= *be && btext.contains(construct)
        });
        if leaked {
            let preview: String = construct.chars().take(40).collect();
            ctx.errors.push(format!(
                "transform conflict: `{preview}` was lowered by a sub-statement edit, but an AST pass re-rendered its enclosing statement and the construct leaked into the output"
            ));
        }
    }
    // line table for the spliced body, built from the ascending edit list
    // before the descending application sort consumes it. generated lines from
    // the import prefix (top) and epilogue (bottom) have no source origin
    let mut body_edits = edits.clone();
    body_edits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let body_table = crate::source_map::line_table(source_ref, &body_edits);

    // sort by start descending so prefix offsets stay valid through replace_range.
    // tie-break by end descending so wider edits (deletions) are applied before
    // zero-width insertions sharing the same start — otherwise the insertion's
    // text would land inside the deletion's range and be wiped on the next pass
    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut out = source_ref.to_owned();
    for (start, end, repl) in edits {
        out.replace_range(start..end, &repl);
    }
    // an entry may be multi-line (runtime helper defs), so the table prefix
    // counts the lines each entry emits, not the entries themselves
    let prefix_lines: usize = ctx
        .required_imports
        .iter()
        .map(|imp| crate::newline_count(imp) + 1)
        .sum();
    let mut preamble_end = 0usize;
    // lines of `out` the preamble is spliced *after*, which therefore keep their
    // own source mapping ahead of the preamble's `None`s
    let mut kept_before_preamble = 0usize;
    if !ctx.required_imports.is_empty() {
        let mut prefix = String::new();
        for imp in &ctx.required_imports {
            prefix.push_str(imp);
            prefix.push('\n');
        }
        // a BOM, the module docstring and a `from __future__ import …` line all
        // have to stay first — the BOM is only a BOM at offset 0, a docstring
        // pushed down by the preamble stops being one (leaving the built
        // module's `__doc__` empty), and the future import is a syntax error
        // anywhere else. so splice required imports in after all three
        let at = preamble_offset(&out);
        kept_before_preamble = crate::newline_count(&out[..at]);
        out.insert_str(at, &prefix);
        preamble_end = at + prefix.len();
    }
    let mut table: Vec<Option<u32>> = Vec::with_capacity(prefix_lines + body_table.len());
    let kept = kept_before_preamble.min(body_table.len());
    table.extend(body_table[..kept].iter().copied());
    table.extend(std::iter::repeat_n(None, prefix_lines));
    table.extend(body_table[kept..].iter().copied());
    if !ctx.epilogue.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        for line in &ctx.epilogue {
            out.push_str(line);
            out.push('\n');
        }
        table.extend(std::iter::repeat_n(None, ctx.epilogue.len()));
    }
    // normalise trailing newline. AST-mutation passes splice rendered
    // multi-line statements that may bring their own internal newlines;
    // EOF without `\n` after such a splice looks awkward. for pure
    // sub-statement text-edit changes we preserve the source's exact
    // end-of-file shape (matters for tests like `final a = 1` with no
    // trailing newline)
    let did_render_stmt = !ctx.changed.is_empty();
    let needs_trailing_nl = source_ref.ends_with('\n') || did_render_stmt;
    if needs_trailing_nl && !out.ends_with('\n') {
        out.push('\n');
    }
    // extension backing functions are lowered in place (so their bodies keep
    // their source ranges for sibling-pass composition); hoist them to the
    // module top now that lowering is done, so a member called before its
    // block's position still resolves
    let (out, table) = extension::hoist_backing_functions(out, table, preamble_end);
    (Cow::Owned(out), ctx.errors, table)
}

#[cfg(test)]
mod driver_tests {
    use super::*;
    use crate::Config;

    #[test]
    fn double_coalesce_spliced() {
        let src = "x = None\na = x ?? x ?? \"fallback\"\n";
        let (out, _, _) = run_against_source(src, &Config::test_default(), None);
        assert!(!out.contains("??"), "still has ??: {out}");
    }

    /// a docstring is only a docstring while it is the module's first statement,
    /// so anything generated ahead of one empties the built module's `__doc__`
    #[test]
    fn generated_lines_follow_the_module_docstring() {
        let src = "\"\"\"a module docstring.\"\"\"\n\nlet LIMIT = 10\n";
        let out = crate::transpile(src, &Config::test_default()).unwrap();
        assert!(
            out.starts_with("\"\"\"a module docstring.\"\"\"\n"),
            "got:\n{out}"
        );
        assert!(out.contains("from typing import Final"), "got:\n{out}");
    }

    #[test]
    fn generated_lines_follow_a_future_import_under_the_docstring() {
        let src = "\"\"\"doc.\"\"\"\nfrom __future__ import annotations\n\nlet LIMIT = 10\n";
        let out = crate::transpile(src, &Config::test_default()).unwrap();
        assert!(
            out.starts_with("\"\"\"doc.\"\"\"\nfrom __future__ import annotations\n"),
            "got:\n{out}"
        );
    }

    /// a zero-width insertion exactly at a `Src` span's end must be emitted
    /// when the following fragment is literal text — nothing else can emit it,
    /// so deferring it (the rule for adjacent passthroughs) would lose it
    #[test]
    fn end_boundary_insertion_emitted_before_literal() {
        use ruff_text_size::TextSize;
        let source = "[1]";
        let all = vec![(3usize, 3usize, SubPatch::Text(")".to_owned()))];
        let frags = vec![
            Fragment::Src(TextRange::new(TextSize::from(0u32), TextSize::from(3u32))),
            Fragment::Lit("Y".to_owned()),
        ];
        let mut out = String::new();
        materialize_fragments(&mut out, &frags, source, &all, &[0]);
        assert_eq!(out, "[1])Y");
    }

    /// between two adjacent `Src` spans the shared boundary insertion is
    /// emitted only by the second (at its start), never twice
    #[test]
    fn shared_boundary_insertion_emitted_once() {
        use ruff_text_size::TextSize;
        let source = "[1]W";
        let all = vec![(3usize, 3usize, SubPatch::Text(")".to_owned()))];
        let frags = vec![
            Fragment::Src(TextRange::new(TextSize::from(0u32), TextSize::from(3u32))),
            Fragment::Src(TextRange::new(TextSize::from(3u32), TextSize::from(4u32))),
        ];
        let mut out = String::new();
        materialize_fragments(&mut out, &frags, source, &all, &[0]);
        assert_eq!(out, "[1])W");
    }
}
