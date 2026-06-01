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
//! - declare pre-source text deletions for pure-deletion rewrites that
//!   would otherwise leak when an outer transform copies operand source
//!   verbatim (variance keyword stripping)
//!
//! AST passes always engage — there is no gate. The text-edit pipeline
//! is intentionally not invoked for any construct an AST pass handles.

use std::borrow::Cow;
use std::cell::RefCell;

use ruff_python_ast::visitor::transformer::Transformer;
use ruff_python_ast::{Expr, ModModule, PySourceType, Stmt};
use ruff_python_codegen::{Generator, Indentation, Mode};
use ruff_python_parser::parse_unchecked_source;
use ruff_source_file::LineEnding;
use ruff_text_size::{Ranged, TextRange};

use super::{
    annotation, anon_named_tuple, auto_quote, callable, cast, coalesce, coalesce_chain, compat,
    decl_site_variance, decorator_keyword, dedent_string, empty_declarations, float_const,
    generic_call, generics, identity_swap, implicit_typing, init_method, intersection, just_float,
    kw_subscript,
    literal_types, main_function, modifiers, mutable_defaults, none_chain, not_type, overload, postfix_await,
    repeated_underscore, sentinel, super_keyword, top_star, tuple_index, type_is,
    typed_dict_literal, typed_lambda, typeof_keyword, unpack, use_site_variance,
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
    /// Hard transpile errors a pass surfaced — abort the pipeline rather
    /// than emit partial / invalid output. Each entry is a human-readable
    /// message suitable for showing the user
    pub(crate) errors: Vec<String>,
    /// Lines to append AFTER the spliced body (e.g. modifiers' auto-
    /// generated `__all__ = [...]`). Driver emits each as its own line
    pub(crate) epilogue: Vec<String>,
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
/// `generic_call`). It is used only when use-site variance stripping is a
/// no-op — a variance rewrite shifts byte positions, so the project parse
/// would no longer align with `source_ref` and we fall back to a single-file
/// db. The chosen db owns the parse the type-aware passes query: `inferred_type`
/// does AST node-identity lookups, so the model and the walked suite must come
/// from one db
pub(crate) fn run_against_source<'a>(
    source: &'a str,
    config: &Config,
    project: Option<(&dyn ty_python_semantic::Db, ruff_db::files::File)>,
) -> (Cow<'a, str>, Vec<String>, Vec<Option<u32>>) {
    // strip use-site variance up front; downstream passes (callable,
    // intersection) copy operand source verbatim and would leak `out`/`in`
    // keywords otherwise
    let stripped = use_site_variance::strip(source);
    let source_ref: &str = stripped.as_ref();

    // pick the db backing the type-aware passes: the project db when present
    // and the source is byte-identical after stripping (so cross-module
    // imports resolve), otherwise a single-file in-memory db
    let sem = match project {
        Some((pdb, pfile)) if matches!(stripped, Cow::Borrowed(_)) => SemDb::Project(pdb, pfile),
        _ => {
            let (db, file) = crate::make_in_memory_db(source_ref);
            SemDb::Local(db, file)
        }
    };
    let (sem_db, sem_file): (&dyn ty_python_semantic::Db, ruff_db::files::File) = match &sem {
        SemDb::Project(db, f) => (*db, *f),
        SemDb::Local(db, f) => (db, *f),
    };
    let parsed_handle = ruff_db::parsed::parsed_module(sem_db, sem_file).load(sem_db);
    let semantic_model = ty_python_semantic::SemanticModel::new(sem_db, sem_file);

    // identity line table for the no-change early returns: stripping variance
    // is within-line, so every line still maps to itself
    if !parsed_handle.errors().is_empty() {
        let cow = match stripped {
            Cow::Borrowed(_) => Cow::Borrowed(source),
            Cow::Owned(s) => Cow::Owned(s),
        };
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, Vec::new(), table);
    }
    let parsed = parse_unchecked_source(source_ref, PySourceType::BasedPython);
    if !parsed.errors().is_empty() {
        let cow = match stripped {
            Cow::Borrowed(_) => Cow::Borrowed(source),
            Cow::Owned(s) => Cow::Owned(s),
        };
        let table = crate::source_map::line_table(cow.as_ref(), &[]);
        return (cow, Vec::new(), table);
    }
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

    let cast_inner = cast::CastFold::new();
    let cast_pass = VisitorPass {
        inner: &cast_inner,
        changed_cell: cast_inner.changed_cell(),
        imports: vec![], // declared after run; see post-pass merge below
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    let typeof_inner = typeof_keyword::TypeofFold::new();
    let typeof_pass = VisitorPass {
        inner: &typeof_inner,
        changed_cell: typeof_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    let tuple_index_inner = tuple_index::TupleIndex::new();
    let tuple_index_pass = VisitorPass {
        inner: &tuple_index_inner,
        changed_cell: tuple_index_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

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

    let typed_lambda_inner = typed_lambda::TypedLambda::new();
    let typed_lambda_pass = VisitorPass {
        inner: &typed_lambda_inner,
        changed_cell: typed_lambda_inner.changed_cell(),
        imports: vec![],
        hoist: RefCell::new(vec![]),
        text_edits: RefCell::new(vec![]),
    };

    let not_type_pass = not_type::NotType::new();
    let intersection_pass = intersection::IntersectionType::new();
    let type_is_pass = type_is::TypeIs::new();
    let top_star_pass = top_star::TopStar::new();
    let identity_swap_pass = identity_swap::IdentitySwap::new(source_ref);
    let compat_pass = compat::CompatRewrite::new(source_ref, config.clone());
    let dedent_string_pass = dedent_string::DedentString::new(source_ref);
    let super_keyword_pass = super_keyword::SuperKeyword::new();
    let postfix_await_pass = postfix_await::PostfixAwait::new(source_ref);
    let mutable_defaults_pass = mutable_defaults::MutableDefaults::new();
    let auto_quote_pass = auto_quote::AutoQuote::new(
        source_ref,
        config.min_version,
        config.inject_future_annotations,
    );
    let init_method_pass = init_method::InitMethod::new(source_ref);
    let modifiers_pass = modifiers::ModifiersPass::new(source_ref);
    let main_function_pass = main_function::MainFunction::new(source_ref, config.is_stub);
    let empty_declarations_pass = empty_declarations::EmptyDeclarations::new();
    let overload_pass = overload::Overload::new(source_ref, config.is_stub);
    let decorator_keyword_pass = decorator_keyword::DecoratorKeyword::new(source_ref);
    let unpack_pass = unpack::UnpackSyntax::new(config.clone());
    let typed_dict_literal_pass = typed_dict_literal::TypedDictLiteralPass::new(source_ref);
    let just_float_pass = just_float::JustFloatPass::new();
    let float_const_pass = float_const::FloatConstPass::new();
    let kw_subscript_pass = kw_subscript::KwSubscriptPass::new(source_ref);
    let generic_call_pass = generic_call::GenericCallStripPass::new(source_ref);
    let implicit_typing_pass = implicit_typing::ImplicitTypingPass::new();
    let tuple_types_pass = annotation::TupleLiteralTypePass::new(source_ref);
    let literal_types_pass = literal_types::LiteralTypePass::new(source_ref);
    let callable_pass = callable::CallableSyntaxPass::new(source_ref);
    let coalesce_text_pass = coalesce::NoneCoalescePass::new(source_ref);
    let none_chain_pass = none_chain::NoneChainPass::new(source_ref);
    let generics_pass = generics::GenericPolyfillPass::new(source_ref, config.clone());
    let variance_pass = decl_site_variance::VarianceStripPass::new();
    let anon_named_tuple_pass =
        anon_named_tuple::AnonNamedTuplePass::new(source_ref, config.clone());

    // Order matters: passes that read source ranges via `text_edits` mode
    // must run BEFORE passes that mutate the AST (which zero source ranges
    // on synthesised nodes). All text-edit-emitting passes here read AST
    // node ranges to compute their edits; once another pass replaces an
    // Expr wholesale, its range is `TextRange::default()` and source lookups
    // are invalid.
    let passes: &[&dyn AstPass] = &[
        // text-edit-emitting passes first (read source ranges).
        // type_is must run before identity_swap so type-position `a is T`
        // wins the first-wins overlap dedup over identity_swap's
        // value-context `isinstance(a, T)` rewrite
        &type_is_pass,
        &top_star_pass,
        &identity_swap_pass,
        &compat_pass,
        &dedent_string_pass,
        &super_keyword_pass,
        &postfix_await_pass,
        &auto_quote_pass,
        &init_method_pass,
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
        // AST-mutation passes second (may zero node ranges)
        &coalesce_pass,
        &cast_pass,
        &typeof_pass,
        &tuple_index_pass,
        &sentinel_pass,
        &repeated_underscore_pass,
        &typed_lambda_pass,
        &mutable_defaults_pass,
    ];
    for pass in passes {
        pass.run(&mut module, &mut ctx);
    }

    // type-aware passes: operate on the salsa-owned parsed module (so
    // semantic queries hit the right AST nodes), emit text_edits / imports
    let type_aware: &[&dyn TypeAwarePass] = &[
        &not_type_pass,
        &intersection_pass,
        &just_float_pass,
        &float_const_pass,
        &kw_subscript_pass,
        &generic_call_pass,
        &implicit_typing_pass,
        &tuple_types_pass,
        &literal_types_pass,
        &callable_pass,
        // coalesce sees `?.` LHS via source ranges; must run BEFORE
        // none_chain so its wider `??` edit wins over none_chain's narrow
        // `?.` edit when both target the same span
        &coalesce_text_pass,
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
    if cast_inner.ever_changed() {
        ctx.required_imports
            .push("from typing import cast".to_owned());
    }
    if typeof_inner.ever_changed() {
        ctx.required_imports
            .push("from ty_extensions import TypeOf".to_owned());
    }
    if sentinel_inner.ever_changed() {
        ctx.required_imports
            .push("from typing_extensions import Sentinel".to_owned());
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
        && ctx.epilogue.is_empty()
    {
        let cow = match stripped {
            Cow::Borrowed(_) => Cow::Borrowed(source),
            Cow::Owned(s) => Cow::Owned(s),
        };
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

    let occupied_ranges: Vec<(usize, usize)> =
        all_idx.iter().map(|&i| original_ranges[i]).collect();
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

        let stmt_text = if ctx.changed.binary_search(&idx).is_ok() {
            let rendered = render_stmt(&original_body[idx]);
            // render_stmt emits a trailing newline. drop it when the source
            // already has one immediately after the stmt (avoids `\n\n`); keep
            // it when the stmt is at end-of-file with no trailing newline so
            // we don't lose multi-line structure
            let source_has_trailing_newline = source_ref.as_bytes().get(end) == Some(&b'\n');
            if source_has_trailing_newline {
                rendered.trim_end_matches('\n').to_owned()
            } else {
                rendered
            }
        } else {
            source_ref[start..end].to_owned()
        };

        let mut block = String::new();
        if let Some(hoists) = hoisted_by_idx.remove(&idx) {
            for h in hoists {
                let rendered = render_stmt(&h).trim_end_matches('\n').to_owned();
                block.push_str(&rendered);
                block.push('\n');
                block.push_str(&line_indent);
            }
        }
        block.push_str(&stmt_text);
        edits.push((start, end, block));
    }
    // ruff-style first-wins dedup for text_edits. sort by start; skip any
    // edit whose start is before the running cursor (overlaps a prior edit)
    // or which collides with a whole-statement splice. zero-width insertions
    // (start == end) at the cursor are allowed — they consume no source bytes
    // so multiple insertions + a deletion at the same position can compose
    let mut sub_edits: Vec<(usize, usize, String)> = ctx
        .text_edits
        .into_iter()
        .map(|(r, s)| (usize::from(r.start()), usize::from(r.end()), s))
        .collect();
    // start asc. tie-break by edit shape:
    //   1. zero-width insertions first — they don't consume bytes, so any
    //      following deletion/replacement at the same start can still apply
    //   2. then wider replacements before narrower ones — so a wider edit
    //      wins over a narrow one nested inside it (ruff-style first-wins
    //      overlap skip preserves the wider edit's intent)
    sub_edits.sort_by(|a, b| {
        let priority = |e: &(usize, usize, String)| {
            // (start, is_replacement_not_insertion, neg_end-for-wider-first)
            if e.1 == e.0 {
                (e.0, 0i64, 0i64) // insertion
            } else {
                #[allow(clippy::cast_possible_wrap)]
                let neg_end = -(e.1 as i64);
                (e.0, 1i64, neg_end)
            }
        };
        priority(a).cmp(&priority(b))
    });
    let mut cursor = 0usize;
    let mut i = 0;
    while i < sub_edits.len() {
        let (start, end, repl) = sub_edits[i].clone();
        if start < cursor || overlaps(start, end) {
            i += 1;
            continue;
        }
        // coalesce all zero-width insertions sharing this start into a
        // single combined insertion (text concatenated in push order). this
        // sidesteps the replace_range-at-same-position ordering issue: each
        // pass pushes its slice in left-to-right intent order, and we
        // splice them as one contiguous string
        if end == start {
            let mut combined = repl;
            let mut j = i + 1;
            while j < sub_edits.len() && sub_edits[j].0 == start && sub_edits[j].1 == start {
                combined.push_str(&sub_edits[j].2);
                j += 1;
            }
            edits.push((start, start, combined));
            i = j;
            continue;
        }
        if end > start {
            cursor = end;
        }
        edits.push((start, end, repl));
        i += 1;
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
    let mut table: Vec<Option<u32>> = Vec::with_capacity(body_table.len());
    table.extend(std::iter::repeat_n(None, ctx.required_imports.len()));
    table.extend(body_table);
    if !ctx.required_imports.is_empty() {
        let mut prefix = String::new();
        for imp in &ctx.required_imports {
            prefix.push_str(imp);
            prefix.push('\n');
        }
        out.insert_str(0, &prefix);
    }
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
}
